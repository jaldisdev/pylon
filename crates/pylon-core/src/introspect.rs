//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! PostgreSQL pg_catalog introspection for schema diffing — a direct Rust
//! port of `pylon.schema._introspect.introspect_db_state`. Queries the live
//! database and returns a `DbState` describing the current user-managed
//! schema structure. System schemas (`_pylon`, `pg_*`, `information_schema`)
//! are excluded automatically; the `public` Postgres schema is treated as
//! the `default` Pylon module.
//!
//! Every query here projects its SELECT list as `(col1, col2, ...) AS
//! result` (a single-column composite) or a bare `col AS result`, matching
//! `pylon-pgcon`'s `decode_result_column`, which only ever decodes column
//! 0 — the same convention `pylon-core`'s own SQL emission uses, reused
//! here rather than teaching `pylon-pgcon` a second, more general
//! "decode every column" path for this one caller.

use crate::diff::{
    DbColumn, DbDomain, DbEnum, DbForeignKey, DbFunction, DbIndex, DbSequence, DbState, DbTable, DbView,
};
use pylon_pgcon::PgPool;
use pylon_value::DecodedValue;

pub type Result<T> = std::result::Result<T, pylon_pgcon::Error>;

// Excluded from the module-listing query only — public is handled as "default".
const SCHEMA_LIST_EXCLUDES: [&str; 5] = ["_pylon", "public", "pg_catalog", "information_schema", "pg_toast"];

// Excluded from all object queries (tables, enums, …) — public is included
// here so we pick up default-module objects.
const OBJECT_EXCLUDES: [&str; 4] = ["_pylon", "pg_catalog", "information_schema", "pg_toast"];

fn pg_to_module(schema: &str) -> String {
    if schema == "public" {
        "default".to_string()
    } else {
        schema.to_string()
    }
}

fn exclude_param(excludes: &[&str]) -> DecodedValue {
    DecodedValue::Array(excludes.iter().map(|s| DecodedValue::Str((*s).to_string())).collect())
}

fn ddl_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(text.as_bytes())[..8])
}

async fn query(pool: &PgPool, sql: &str, params: &[DecodedValue]) -> Result<Vec<DecodedValue>> {
    pool.query_typed(sql, params, pool.types()).await
}

/// Destructures a `DecodedValue::Composite`'s fields into a fixed-size
/// array, or `None` if the row's shape doesn't match — defensive, should
/// never actually happen for these hand-written queries.
fn fields<const N: usize>(row: DecodedValue) -> Option<[DecodedValue; N]> {
    match row {
        DecodedValue::Composite(fields) => fields.try_into().ok(),
        _ => None,
    }
}

fn as_str(v: DecodedValue) -> Option<String> {
    match v {
        DecodedValue::Str(s) => Some(s),
        _ => None,
    }
}

fn as_bool(v: DecodedValue) -> Option<bool> {
    match v {
        DecodedValue::Bool(b) => Some(b),
        _ => None,
    }
}

fn as_opt_str(v: DecodedValue) -> Option<String> {
    match v {
        DecodedValue::Null => None,
        DecodedValue::Str(s) => Some(s),
        _ => None,
    }
}

const SCHEMAS_SQL: &str = r#"
    SELECT nspname AS result
    FROM pg_namespace
    WHERE nspname NOT LIKE 'pg_%'
      AND nspname <> ALL($1::text[])
    ORDER BY nspname
"#;

const ENUMS_SQL: &str = r#"
    SELECT (n.nspname, t.typname, e.enumlabel) AS result
    FROM pg_type t
    JOIN pg_namespace n ON n.oid = t.typnamespace
    JOIN pg_enum e ON e.enumtypid = t.oid
    WHERE t.typtype = 'e'
      AND n.nspname NOT LIKE 'pg_%'
      AND n.nspname <> ALL($1::text[])
    ORDER BY n.nspname, t.typname, e.enumsortorder
"#;

const DOMAINS_SQL: &str = r#"
    SELECT (n.nspname, t.typname) AS result
    FROM pg_type t
    JOIN pg_namespace n ON n.oid = t.typnamespace
    WHERE t.typtype = 'd'
      AND n.nspname NOT LIKE 'pg_%'
      AND n.nspname <> ALL($1::text[])
    ORDER BY n.nspname, t.typname
"#;

const TABLES_SQL: &str = r#"
    SELECT (n.nspname, c.relname) AS result
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind = 'r'
      AND n.nspname NOT LIKE 'pg_%'
      AND n.nspname <> ALL($1::text[])
    ORDER BY n.nspname, c.relname
"#;

const COLUMNS_SQL: &str = r#"
    SELECT (
        a.attname,
        pg_catalog.format_type(a.atttypid, a.atttypmod),
        NOT a.attnotnull,
        a.attgenerated = 's',
        CASE WHEN a.atthasdef AND a.attgenerated = '' THEN
            pg_catalog.pg_get_expr(d.adbin, d.adrelid)
        END
    ) AS result
    FROM pg_attribute a
    JOIN pg_class c ON c.oid = a.attrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE n.nspname = $1
      AND c.relname = $2
      AND a.attnum > 0
      AND NOT a.attisdropped
    ORDER BY a.attnum
"#;

const FKS_SQL: &str = r#"
    SELECT (con.conname, a.attname, n2.nspname, c2.relname) AS result
    FROM pg_constraint con
    JOIN pg_class c ON c.oid = con.conrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[1]
    JOIN pg_class c2 ON c2.oid = con.confrelid
    JOIN pg_namespace n2 ON n2.oid = c2.relnamespace
    WHERE con.contype = 'f'
      AND n.nspname = $1
      AND c.relname = $2
"#;

const INDEXES_SQL: &str = r#"
    SELECT (i.relname, ix.indisunique, am.amname) AS result
    FROM pg_index ix
    JOIN pg_class i ON i.oid = ix.indexrelid
    JOIN pg_am am ON am.oid = i.relam
    JOIN pg_class t ON t.oid = ix.indrelid
    JOIN pg_namespace n ON n.oid = t.relnamespace
    WHERE ix.indisprimary = false
      AND n.nspname = $1
      AND t.relname = $2
"#;

const SEQUENCES_SQL: &str = r#"
    SELECT (schemaname, sequencename) AS result
    FROM pg_sequences
    WHERE schemaname NOT LIKE 'pg_%'
      AND schemaname <> ALL($1::text[])
    ORDER BY schemaname, sequencename
"#;

const VIEWS_SQL: &str = r#"
    SELECT (table_schema, table_name, view_definition) AS result
    FROM information_schema.views
    WHERE table_schema NOT LIKE 'pg_%'
      AND table_schema <> ALL($1::text[])
    ORDER BY table_schema, table_name
"#;

const FUNCTIONS_SQL: &str = r#"
    SELECT (n.nspname, p.proname, pg_get_functiondef(p.oid)) AS result
    FROM pg_proc p
    JOIN pg_namespace n ON n.oid = p.pronamespace
    WHERE p.prokind = 'f'
      AND n.nspname NOT LIKE 'pg_%'
      AND n.nspname <> ALL($1::text[])
    ORDER BY n.nspname, p.proname
"#;

// Every non-internal trigger, not just constraint triggers (`tgconstraint
// != 0`) — that used to exclude `pylon_cache_invalidate` and `@pylon.signal`
// capture triggers (both plain `CREATE TRIGGER`s, not `CREATE CONSTRAINT
// TRIGGER`s) from ever being recognized as "already present", so the diff
// engine proposed recreating every one of them on every single
// `migration create`/`watch` run, forever, even with zero schema changes
// (confirmed live against the demo project).
const TRIGGERS_SQL: &str = r#"
    SELECT (t.tgname, n.nspname, c.relname) AS result
    FROM pg_trigger t
    JOIN pg_class c ON c.oid = t.tgrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE NOT t.tgisinternal
      AND n.nspname NOT LIKE 'pg_%'
      AND n.nspname <> ALL($1::text[])
    ORDER BY n.nspname, c.relname, t.tgname
"#;

const EXTENSIONS_SQL: &str = r#"
    SELECT extname AS result
    FROM pg_extension
    ORDER BY extname
"#;

/// Query pg_catalog and return a `DbState` describing the live database.
pub async fn introspect_db_state(pool: &PgPool) -> Result<DbState> {
    let mut state = DbState::default();
    let schema_excludes = exclude_param(&SCHEMA_LIST_EXCLUDES);
    let object_excludes = exclude_param(&OBJECT_EXCLUDES);

    // The default module always exists (mapped to public).
    state.schemas.push("default".to_string());

    // User-defined modules (non-default).
    for row in query(pool, SCHEMAS_SQL, std::slice::from_ref(&schema_excludes)).await? {
        if let Some(nspname) = as_str(row) {
            state.schemas.push(nspname);
        }
    }

    // Enums (collect members per enum — consecutive rows share a key,
    // thanks to `ORDER BY n.nspname, t.typname, e.enumsortorder`).
    let mut current_enum: Option<(String, String)> = None;
    let mut members: Vec<String> = Vec::new();
    for row in query(pool, ENUMS_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([schema, name, member]) = fields::<3>(row) else {
            continue;
        };
        let (Some(schema), Some(name), Some(member)) = (as_str(schema), as_str(name), as_str(member)) else {
            continue;
        };
        let key = (schema, name);
        if current_enum.as_ref() != Some(&key) {
            if let Some((cs, cn)) = current_enum.take() {
                state.enums.push(DbEnum {
                    schema: pg_to_module(&cs),
                    name: cn,
                    members: std::mem::take(&mut members),
                });
            }
            current_enum = Some(key);
        }
        members.push(member);
    }
    if let Some((cs, cn)) = current_enum {
        state.enums.push(DbEnum {
            schema: pg_to_module(&cs),
            name: cn,
            members,
        });
    }

    // Domains
    for row in query(pool, DOMAINS_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([schema, name]) = fields::<2>(row) else {
            continue;
        };
        let (Some(schema), Some(name)) = (as_str(schema), as_str(name)) else {
            continue;
        };
        state.domains.push(DbDomain {
            schema: pg_to_module(&schema),
            name,
        });
    }

    // Tables + columns + FKs + indexes
    for row in query(pool, TABLES_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([pg_schema_v, name_v]) = fields::<2>(row) else {
            continue;
        };
        let (Some(pg_schema), Some(name)) = (as_str(pg_schema_v), as_str(name_v)) else {
            continue;
        };
        let module = pg_to_module(&pg_schema);

        let mut columns = Vec::new();
        for col in query(
            pool,
            COLUMNS_SQL,
            &[DecodedValue::Str(pg_schema.clone()), DecodedValue::Str(name.clone())],
        )
        .await?
        {
            let Some([attname, pg_type, nullable, is_generated, column_default]) = fields::<5>(col) else {
                continue;
            };
            let (Some(attname), Some(pg_type), Some(nullable), Some(is_generated)) = (
                as_str(attname),
                as_str(pg_type),
                as_bool(nullable),
                as_bool(is_generated),
            ) else {
                continue;
            };
            columns.push(DbColumn {
                name: attname,
                pg_type,
                nullable,
                is_generated,
                column_default: as_opt_str(column_default),
            });
        }

        let mut foreign_keys = Vec::new();
        for fk in query(
            pool,
            FKS_SQL,
            &[DecodedValue::Str(pg_schema.clone()), DecodedValue::Str(name.clone())],
        )
        .await?
        {
            let Some([conname, attname, ref_schema, ref_table]) = fields::<4>(fk) else {
                continue;
            };
            let (Some(conname), Some(attname), Some(ref_schema), Some(ref_table)) =
                (as_str(conname), as_str(attname), as_str(ref_schema), as_str(ref_table))
            else {
                continue;
            };
            foreign_keys.push(DbForeignKey {
                constraint_name: conname,
                local_column: attname,
                ref_schema: pg_to_module(&ref_schema),
                ref_table,
            });
        }

        let mut indexes = Vec::new();
        for idx in query(
            pool,
            INDEXES_SQL,
            &[DecodedValue::Str(pg_schema.clone()), DecodedValue::Str(name.clone())],
        )
        .await?
        {
            let Some([relname, is_unique, amname]) = fields::<3>(idx) else {
                continue;
            };
            let (Some(relname), Some(is_unique), Some(amname)) = (as_str(relname), as_bool(is_unique), as_str(amname))
            else {
                continue;
            };
            indexes.push(DbIndex {
                name: relname,
                is_unique,
                method: amname,
            });
        }

        state.tables.push(DbTable {
            schema: module,
            name,
            columns,
            foreign_keys,
            indexes,
            checks: Vec::new(),
            triggers: Vec::new(),
        });
    }

    // Sequences
    for row in query(pool, SEQUENCES_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([schema, name]) = fields::<2>(row) else {
            continue;
        };
        let (Some(schema), Some(name)) = (as_str(schema), as_str(name)) else {
            continue;
        };
        state.sequences.push(DbSequence {
            schema: pg_to_module(&schema),
            name,
        });
    }

    // Views
    for row in query(pool, VIEWS_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([schema, name, definition]) = fields::<3>(row) else {
            continue;
        };
        let (Some(schema), Some(name), Some(definition)) = (as_str(schema), as_str(name), as_str(definition)) else {
            continue;
        };
        state.views.push(DbView {
            schema: pg_to_module(&schema),
            name,
            body_hash: ddl_hash(&definition),
        });
    }

    // Functions
    for row in query(pool, FUNCTIONS_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([schema, name, definition]) = fields::<3>(row) else {
            continue;
        };
        let (Some(schema), Some(name), Some(definition)) = (as_str(schema), as_str(name), as_str(definition)) else {
            continue;
        };
        state.functions.push(DbFunction {
            schema: pg_to_module(&schema),
            name,
            body_hash: ddl_hash(&definition),
        });
    }

    // Constraint triggers (cross-table exclusive enforcement)
    for row in query(pool, TRIGGERS_SQL, std::slice::from_ref(&object_excludes)).await? {
        let Some([tgname, pg_schema, table_name]) = fields::<3>(row) else {
            continue;
        };
        let (Some(tgname), Some(pg_schema), Some(table_name)) = (as_str(tgname), as_str(pg_schema), as_str(table_name))
        else {
            continue;
        };
        state.add_trigger(&pg_to_module(&pg_schema), &table_name, &tgname);
    }

    // Installed extensions (no excludes — this list is small and deliberate).
    for row in query(pool, EXTENSIONS_SQL, &[]).await? {
        if let Some(extname) = as_str(row) {
            state.extensions.push(extname);
        }
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").expect("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
    }

    fn unique_name(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{prefix}_{nanos}")
    }

    #[tokio::test]
    #[ignore]
    async fn finds_a_table_with_columns_fk_and_index() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let parent = unique_name("introspect_parent");
        let child = unique_name("introspect_child");
        pool.batch_execute(&format!(
            "CREATE TABLE {parent} (id int8 PRIMARY KEY);
             CREATE TABLE {child} (
                 id int8 PRIMARY KEY,
                 parent_id int8 NOT NULL REFERENCES {parent}(id),
                 label text DEFAULT 'x'
             );
             CREATE INDEX {child}_label_idx ON {child} (label);"
        ))
        .await
        .unwrap();

        let state = introspect_db_state(&pool).await.unwrap();

        let child_table = state
            .tables
            .iter()
            .find(|t| t.name == child)
            .expect("child table found");
        assert_eq!(child_table.schema, "default"); // public -> default

        let parent_id_col = child_table.columns.iter().find(|c| c.name == "parent_id").unwrap();
        assert!(!parent_id_col.nullable);
        assert!(!parent_id_col.is_generated);

        let label_col = child_table.columns.iter().find(|c| c.name == "label").unwrap();
        assert!(label_col.nullable);
        assert_eq!(label_col.column_default.as_deref(), Some("'x'::text"));

        let fk = child_table.foreign_keys.first().expect("fk found");
        assert_eq!(fk.local_column, "parent_id");
        assert_eq!(fk.ref_table, parent);
        assert_eq!(fk.ref_schema, "default");

        let idx = child_table
            .indexes
            .iter()
            .find(|i| i.name == format!("{child}_label_idx"))
            .expect("index found");
        assert!(!idx.is_unique);
        assert_eq!(idx.method, "btree");
    }

    #[tokio::test]
    #[ignore]
    async fn finds_an_enum_with_all_members_in_order() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let enum_name = unique_name("introspect_enum");
        pool.batch_execute(&format!("CREATE TYPE {enum_name} AS ENUM ('a', 'b', 'c');"))
            .await
            .unwrap();

        let state = introspect_db_state(&pool).await.unwrap();

        let e = state.enums.iter().find(|e| e.name == enum_name).expect("enum found");
        assert_eq!(e.schema, "default");
        assert_eq!(e.members, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    #[ignore]
    async fn finds_a_domain_a_sequence_and_a_view() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let domain_name = unique_name("introspect_domain");
        let seq_name = unique_name("introspect_seq");
        let view_name = unique_name("introspect_view");
        pool.batch_execute(&format!(
            "CREATE DOMAIN {domain_name} AS text;
             CREATE SEQUENCE {seq_name};
             CREATE VIEW {view_name} AS SELECT 1 AS x;"
        ))
        .await
        .unwrap();

        let state = introspect_db_state(&pool).await.unwrap();

        assert!(
            state
                .domains
                .iter()
                .any(|d| d.name == domain_name && d.schema == "default")
        );
        assert!(
            state
                .sequences
                .iter()
                .any(|s| s.name == seq_name && s.schema == "default")
        );
        assert!(state.views.iter().any(|v| v.name == view_name && v.schema == "default"));
    }

    #[tokio::test]
    #[ignore]
    async fn finds_a_function_and_hashes_its_definition_stably() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let fn_name = unique_name("introspect_fn");
        pool.batch_execute(&format!(
            "CREATE FUNCTION {fn_name}() RETURNS int8 AS $$ SELECT 1::int8 $$ LANGUAGE sql;"
        ))
        .await
        .unwrap();

        let state1 = introspect_db_state(&pool).await.unwrap();
        let state2 = introspect_db_state(&pool).await.unwrap();

        let f1 = state1
            .functions
            .iter()
            .find(|f| f.name == fn_name)
            .expect("function found");
        let f2 = state2.functions.iter().find(|f| f.name == fn_name).unwrap();
        assert_eq!(f1.schema, "default");
        assert_eq!(f1.body_hash, f2.body_hash, "hash must be stable across introspections");
        assert_eq!(f1.body_hash.len(), 16);
    }

    #[tokio::test]
    #[ignore]
    async fn excludes_pylon_and_system_schema_objects() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
        let internal_table = unique_name("introspect_internal");
        pool.batch_execute(&format!("CREATE TABLE _pylon.{internal_table} (id int8);"))
            .await
            .unwrap();

        let state = introspect_db_state(&pool).await.unwrap();

        assert!(
            !state.tables.iter().any(|t| t.name == internal_table),
            "_pylon-schema tables must be excluded"
        );
        assert!(
            !state.schemas.iter().any(|s| s == "_pylon"),
            "_pylon must never appear in the schema list"
        );
        assert!(
            !state
                .schemas
                .iter()
                .any(|s| s == "pg_catalog" || s == "information_schema")
        );
    }

    #[tokio::test]
    #[ignore]
    async fn user_defined_module_schema_is_not_renamed() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let module = unique_name("introspect_module");
        pool.batch_execute(&format!("CREATE SCHEMA {module};")).await.unwrap();

        let state = introspect_db_state(&pool).await.unwrap();

        assert!(state.schemas.iter().any(|s| s == &module));
        assert!(state.schemas.iter().any(|s| s == "default")); // always present
    }
}
