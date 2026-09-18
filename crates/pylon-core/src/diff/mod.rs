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

//! Schema diff engine: given a target SchemaDescriptor and the live database
//! state (introspected via pg_catalog), produce ordered DDL SQL statements
//! to bring the database in sync with the target.
//!
//! Used by `pylon migration watch` (apply directly) and `pylon migration create`
//! (render as a migration file body). Pure computation — no I/O.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::schema::{SchemaDescriptor, SearchBackend, TypeDescriptor};

// ── Live database state ────────────────────────────────────────────────────────

/// Snapshot of the live PostgreSQL database, built by Python from pg_catalog queries.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DbState {
    /// User-managed schema names (excludes _pylon, public, pg_* etc.).
    #[serde(default)]
    pub schemas: Vec<String>,
    #[serde(default)]
    pub tables: Vec<DbTable>,
    #[serde(default)]
    pub enums: Vec<DbEnum>,
    #[serde(default)]
    pub domains: Vec<DbDomain>,
    #[serde(default)]
    pub sequences: Vec<DbSequence>,
    #[serde(default)]
    pub views: Vec<DbView>,
    #[serde(default)]
    pub functions: Vec<DbFunction>,
    /// Installed Postgres extension names (e.g. `vector`) — used to decide
    /// whether a `CREATE EXTENSION` needs to be added to a migration; see
    /// `required_extensions`/`missing_extension_ddl`.
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<DbColumn>,
    pub foreign_keys: Vec<DbForeignKey>,
    pub indexes: Vec<DbIndex>,
    pub checks: Vec<DbCheck>,
    #[serde(default)]
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbColumn {
    pub name: String,
    pub pg_type: String,
    pub nullable: bool,
    pub is_generated: bool,
    #[serde(default)]
    pub column_default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbForeignKey {
    pub constraint_name: String,
    pub local_column: String,
    pub ref_schema: String,
    pub ref_table: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbIndex {
    pub name: String,
    pub is_unique: bool,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCheck {
    pub constraint_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbEnum {
    pub schema: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbDomain {
    pub schema: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbSequence {
    pub schema: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbView {
    pub schema: String,
    pub name: String,
    /// SHA-256 (first 16 hex chars) of the rendered DDL — detects definition changes.
    pub body_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbFunction {
    pub schema: String,
    pub name: String,
    /// SHA-256 (first 16 hex chars) of the rendered DDL — detects body changes.
    pub body_hash: String,
}

// ── Schema → DbState projection ────────────────────────────────────────────────

/// Convert a compiled `SchemaDescriptor` into the equivalent `DbState` snapshot.
///
/// This produces exactly what `introspect_db_state` would return after the schema
/// is fully applied — used to persist a baseline alongside each migration record
/// so that subsequent `pylon migration create` calls can diff without introspecting
/// the live database.
pub fn schema_to_db_state(schema: &SchemaDescriptor) -> DbState {
    use std::collections::BTreeSet;

    let type_map: HashMap<String, (&str, &str)> = schema
        .types
        .iter()
        .map(|t| {
            (
                format!("{}::{}", t.module, t.name),
                (t.module.as_str(), t.table.as_str()),
            )
        })
        .collect();

    // Collect all module names that contribute a Postgres schema.
    let mut schema_set: BTreeSet<String> = BTreeSet::new();
    for t in &schema.types {
        schema_set.insert(t.module.clone());
    }
    for e in &schema.enums {
        schema_set.insert(e.module.clone());
    }
    for s in &schema.scalars {
        schema_set.insert(s.module.clone());
    }

    let schemas: Vec<String> = schema_set.into_iter().collect();

    // Enums
    let enums: Vec<DbEnum> = schema
        .enums
        .iter()
        .map(|e| DbEnum {
            schema: e.module.clone(),
            name: e.name.clone(),
            members: e.members.clone(),
        })
        .collect();

    // Domains (custom scalars)
    let domains: Vec<DbDomain> = schema
        .scalars
        .iter()
        .map(|s| DbDomain {
            schema: s.module.clone(),
            name: s.name.clone(),
        })
        .collect();

    // Sequences (sequence scalars only)
    let sequences: Vec<DbSequence> = schema
        .scalars
        .iter()
        .filter(|s| s.is_sequence)
        .map(|s| DbSequence {
            schema: s.module.clone(),
            name: format!("{}_seq", s.name),
        })
        .collect();

    let expected_trigger_names = expected_triggers(schema, &type_map);
    let mut tables: Vec<DbTable> = Vec::new();

    for td in &schema.types {
        if td.abstract_ || td.junction {
            continue;
        }

        // Columns: properties + link FK stubs + vector/search generated columns
        let mut columns: Vec<DbColumn> = Vec::new();
        for p in &td.properties {
            columns.push(DbColumn {
                name: p.name.clone(),
                pg_type: col_type_str(p).to_string(),
                nullable: p.nullable,
                is_generated: false,
                column_default: resolve_default(p, schema),
            });
        }
        for l in &td.links {
            // A junction-backed link has no `{name}_id` column — it's
            // stored via a junction table instead (below), same as a
            // multi-link.
            if l.is_junction_backed() {
                continue;
            }
            columns.push(DbColumn {
                name: format!("{}_id", l.name),
                pg_type: "uuid".to_string(),
                nullable: l.nullable,
                is_generated: false,
                column_default: resolve_link_default(l, schema),
            });
        }
        // Generated columns for vector indexes
        for vi in &td.vector_indexes {
            let col = vi.column_name();
            if !columns.iter().any(|c| c.name == col) {
                columns.push(DbColumn {
                    name: col,
                    pg_type: format!("vector({})", vi.dimensions),
                    nullable: true,
                    is_generated: false,
                    column_default: None,
                });
            }
        }
        // Generated columns for search indexes (tsvector)
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }
            let col = si.column_name();
            if !columns.iter().any(|c| c.name == col) {
                columns.push(DbColumn {
                    name: col,
                    pg_type: "tsvector".to_string(),
                    nullable: true,
                    is_generated: true,
                    column_default: None,
                });
            }
        }

        // FK constraints: one per single link
        let mut foreign_keys: Vec<DbForeignKey> = Vec::new();
        for l in &td.links {
            if l.is_junction_backed() {
                continue;
            }
            let cname = format!("{}_{}_fkey", td.table, l.name);
            if let Some((tgt_schema, tgt_table)) = type_map.get(&l.target) {
                foreign_keys.push(DbForeignKey {
                    constraint_name: cname,
                    local_column: format!("{}_id", l.name),
                    ref_schema: tgt_schema.to_string(),
                    ref_table: tgt_table.to_string(),
                });
            }
        }

        // Unique indexes from exclusive properties and links (Postgres auto-names them).
        let mut indexes: Vec<DbIndex> = Vec::new();
        for p in &td.properties {
            if p.is_exclusive && !p.is_pk {
                indexes.push(DbIndex {
                    name: format!("{}_{}_key", td.table, p.name),
                    is_unique: true,
                    method: "btree".to_string(),
                });
            }
        }
        for l in &td.links {
            // A junction-backed exclusive link's uniqueness is a
            // `UNIQUE (target)` table constraint on its junction table
            // (below), not a separate index on this table.
            if l.is_exclusive && !l.is_junction_backed() {
                indexes.push(DbIndex {
                    name: format!("{}_{}_id_key", td.table, l.name),
                    is_unique: true,
                    method: "btree".to_string(),
                });
            }
        }
        for (i, constraint) in td.constraints.iter().enumerate() {
            use crate::schema::TypeConstraint;
            if let TypeConstraint::Exclusive { pointers: fields, .. } = constraint {
                // Postgres auto-names these; mirror the convention.
                let idx_name = format!("{}_{}_{}_key", td.table, fields.join("_"), i);
                indexes.push(DbIndex {
                    name: idx_name,
                    is_unique: true,
                    method: "btree".to_string(),
                });
            }
        }
        // Plain indexes (Postgres auto-names these too).
        for (i, idx) in td.indexes.iter().enumerate() {
            let name = if idx.expression.is_some() {
                format!("{}__expr{}_idx", td.table, i)
            } else {
                format!("{}__{}_idx", td.table, idx.pointers.join("_"))
            };
            indexes.push(DbIndex {
                name,
                is_unique: idx.unique,
                method: "btree".to_string(),
            });
        }
        // Vector HNSW indexes
        for vi in &td.vector_indexes {
            let idx_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(n) => format!("{}__vector_{}__", td.table, n),
            };
            indexes.push(DbIndex {
                name: idx_name,
                is_unique: false,
                method: "hnsw".to_string(),
            });
        }
        // Search GIN indexes
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }
            let idx_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(n) => format!("{}__search_{}__", td.table, n),
            };
            indexes.push(DbIndex {
                name: idx_name,
                is_unique: false,
                method: "gin".to_string(),
            });
        }

        // CHECK constraints — names are hash-based in the real schema; approximate here.
        let mut checks: Vec<DbCheck> = Vec::new();
        for p in &td.properties {
            for (i, _) in p.check_constraints.iter().enumerate() {
                checks.push(DbCheck {
                    constraint_name: format!("{}_{}_check_{}", td.table, p.name, i),
                });
            }
        }
        for (i, constraint) in td.constraints.iter().enumerate() {
            use crate::schema::TypeConstraint;
            if let TypeConstraint::Expression { .. } = constraint {
                checks.push(DbCheck {
                    constraint_name: format!("{}_expr_check_{}", td.table, i),
                });
            }
        }

        let triggers: Vec<String> = expected_trigger_names
            .get(&(td.module.clone(), td.table.clone()))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        tables.push(DbTable {
            schema: td.module.clone(),
            name: td.table.clone(),
            columns,
            foreign_keys,
            indexes,
            checks,
            triggers,
        });

        // Junction tables for multi-links
        for ml in &td.multilinks {
            tables.push(build_junction_db_table(
                schema,
                &type_map,
                td,
                &ml.name,
                &ml.target,
                ml.through.as_deref(),
                &expected_trigger_names,
            ));
        }
        // Junction tables for junction-backed single links — same shape
        // (source/target + through columns); cardinality is enforced via
        // an inline PRIMARY KEY/UNIQUE table constraint the diff engine
        // doesn't track as a separate index (mirroring how a plain
        // multi-link's own inline `PRIMARY KEY (source, target)` isn't
        // tracked as an index here either).
        for l in &td.links {
            if !l.is_junction_backed() {
                continue;
            }
            tables.push(build_junction_db_table(
                schema,
                &type_map,
                td,
                &l.name,
                &l.target,
                l.through.as_deref(),
                &expected_trigger_names,
            ));
        }
    }

    // Views (interface types)
    let views: Vec<DbView> = crate::export::interface_view_ddl_with_names(schema)
        .into_iter()
        .map(|(module, name, ddl)| DbView {
            schema: module,
            name,
            body_hash: ddl_hash(&ddl),
        })
        .collect();

    // User-defined functions
    let functions: Vec<DbFunction> = crate::export::function_ddl_with_names(schema)
        .unwrap_or_default()
        .into_iter()
        .map(|(module, name, ddl)| DbFunction {
            schema: module,
            name,
            body_hash: ddl_hash(&ddl),
        })
        .collect();

    let extensions: Vec<String> = required_extensions(schema).iter().map(|s| s.to_string()).collect();

    DbState {
        schemas,
        tables,
        enums,
        domains,
        sequences,
        views,
        functions,
        extensions,
    }
}

/// Postgres extensions `target` needs in order for its own DDL to apply
/// cleanly — currently just `vector` (pgvector), needed the moment any
/// type declares a vector index. Extending this to a future
/// extension-dependent feature is just adding another check here; the
/// caller-facing surface (`missing_extension_ddl`) doesn't change.
pub fn required_extensions(target: &SchemaDescriptor) -> Vec<&'static str> {
    let mut out = Vec::new();
    if target.types.iter().any(|t| !t.vector_indexes.is_empty()) {
        out.push("vector");
    }
    if target.types.iter().any(|t| t.partition.is_some()) {
        out.push("pg_partman");
    }
    out
}

/// `CREATE EXTENSION IF NOT EXISTS` statements for every extension
/// `target` requires that isn't already present in `current` — meant to be
/// prepended to an assembled migration/`watch` sync unconditionally,
/// outside the interactive per-step confirmation flow: enabling a
/// required extension isn't a design decision to confirm or reject, it's
/// a hard prerequisite the rest of the DDL can't succeed without.
pub fn missing_extension_ddl(target: &SchemaDescriptor, current: &DbState) -> Vec<String> {
    required_extensions(target)
        .into_iter()
        .filter(|ext| !current.extensions.iter().any(|e| e == ext))
        .map(|ext| format!("CREATE EXTENSION IF NOT EXISTS \"{ext}\";"))
        .collect()
}

/// Builds the expected `DbTable` for one junction table — shared by a
/// multi-link and a junction-backed single link, since both store
/// (source, target, through-type properties) identically; only the
/// caller-supplied cardinality constraint (a `PRIMARY KEY`/`UNIQUE` table
/// constraint, not tracked here as a separate index — see
/// `schema_to_db_state`) differs between the two.
fn build_junction_db_table(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    td: &TypeDescriptor,
    name: &str,
    target: &str,
    through: Option<&str>,
    expected_trigger_names: &HashMap<(String, String), HashSet<String>>,
) -> DbTable {
    let jt_name = format!("{}.{}", td.table, name);
    let mut jt_columns = vec![
        DbColumn {
            name: "source".to_string(),
            pg_type: "uuid".to_string(),
            nullable: false,
            is_generated: false,
            column_default: None,
        },
        DbColumn {
            name: "target".to_string(),
            pg_type: "uuid".to_string(),
            nullable: false,
            is_generated: false,
            column_default: None,
        },
    ];

    // Extra columns from the through junction type
    if let Some(through_qname) = through
        && let Some(through_td) = schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == *through_qname && t.junction)
    {
        for p in &through_td.properties {
            if p.name == "id" {
                continue;
            }
            let pg_type = col_type_str(p).to_string();
            jt_columns.push(DbColumn {
                name: p.name.clone(),
                pg_type,
                nullable: p.nullable,
                is_generated: false,
                column_default: p.default_sql.clone(),
            });
        }
    }

    let mut jt_fks = Vec::new();
    let src_fk_name = format!("{}_{}_source_fkey", td.table, name);
    jt_fks.push(DbForeignKey {
        constraint_name: src_fk_name,
        local_column: "source".to_string(),
        ref_schema: td.module.clone(),
        ref_table: td.table.clone(),
    });
    if let Some((tgt_schema, tgt_table)) = type_map.get(target) {
        let tgt_fk_name = format!("{}_{}_target_fkey", td.table, name);
        jt_fks.push(DbForeignKey {
            constraint_name: tgt_fk_name,
            local_column: "target".to_string(),
            ref_schema: tgt_schema.to_string(),
            ref_table: tgt_table.to_string(),
        });
    }

    let triggers: Vec<String> = expected_trigger_names
        .get(&(td.module.clone(), jt_name.clone()))
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();

    DbTable {
        schema: td.module.clone(),
        name: jt_name,
        columns: jt_columns,
        foreign_keys: jt_fks,
        indexes: vec![],
        checks: vec![],
        triggers,
    }
}

fn ddl_hash(ddl: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(ddl.as_bytes());
    hex::encode(&digest[..8])
}

/// Serialize a `DbState` to a JSON string for storage in `_pylon."Migrations".db_state`.
pub fn db_state_to_json(state: &DbState) -> String {
    serde_json::to_string(state).expect("DbState serialization is infallible")
}

/// Deserialize a `DbState` from the JSON stored in `_pylon."Migrations".db_state`.
pub fn db_state_from_json(json: &str) -> Result<DbState, String> {
    serde_json::from_str(json).map_err(|e| e.to_string())
}

impl DbState {
    /// Add a constraint trigger name to an existing table entry.
    pub fn add_trigger(&mut self, module: &str, table: &str, trigger_name: &str) {
        if let Some(t) = self.tables.iter_mut().find(|t| t.schema == module && t.name == table) {
            t.triggers.push(trigger_name.to_string());
        }
    }
}

/// Every trigger name a table (or its junction tables) should have once
/// `schema` is fully applied — constraint triggers for interface-exclusive
/// enforcement, deletion-policy triggers, `@pylon.signal` capture triggers,
/// user-declared schema `Trigger`s, and the unconditional cache-invalidation
/// trigger.
///
/// Shared by the diff engine's own add/drop decisions (`diff_inner`'s
/// trigger phase) and `schema_to_db_state`'s baseline snapshot — both need
/// the exact same "what should be there" answer. Having two separate,
/// independently-maintained copies of this logic is exactly how they
/// drifted apart before: cache-invalidate and `@pylon.signal` triggers were
/// only ever recognized by the diff engine's own live-comparison path, so
/// `schema_to_db_state`'s snapshot never listed them as already
/// present — making `migration create` propose recreating every single
/// one of them, forever, even with zero schema changes (confirmed live
/// against the demo project).
pub fn expected_triggers(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
) -> HashMap<(String, String), HashSet<String>> {
    let mut expected: HashMap<(String, String), HashSet<String>> = HashMap::new();

    for info in crate::export::interface_exclusive_trigger_infos(schema) {
        expected
            .entry((info.impl_module.clone(), info.impl_table.clone()))
            .or_default()
            .extend([info.ins_trigger_name, info.upd_trigger_name]);
    }
    for info in crate::export::deletion_policy_trigger_infos(schema, type_map) {
        expected
            .entry((info.table_module.clone(), info.table_name.clone()))
            .or_default()
            .insert(info.trigger_name);
    }
    for info in crate::export::signal_trigger_infos(schema) {
        expected
            .entry((info.table_module.clone(), info.table_name.clone()))
            .or_default()
            .insert(info.trigger_name);
    }
    for (module, table, name) in crate::export::user_trigger_names(schema) {
        expected.entry((module, table)).or_default().insert(name);
    }

    // Cache-invalidation trigger — every concrete table and every
    // multi-link junction table (or junction-backed single link's own
    // physical table), unconditionally (not gated by `[cache].enabled`;
    // see the cache layer plan's design decision).
    let mut cache_trigger_tables: HashSet<(String, String)> = HashSet::new();
    for td in &schema.types {
        if td.abstract_ {
            continue;
        }
        cache_trigger_tables.insert((td.module.clone(), td.table.clone()));
        if !td.junction {
            for ml in &td.multilinks {
                cache_trigger_tables.insert((td.module.clone(), format!("{}.{}", td.table, ml.name)));
            }
            for l in &td.links {
                if !l.is_junction_backed() {
                    continue;
                }
                cache_trigger_tables.insert((td.module.clone(), format!("{}.{}", td.table, l.name)));
            }
        }
    }
    for key in cache_trigger_tables {
        expected
            .entry(key)
            .or_default()
            .insert("pylon_cache_invalidate".to_string());
    }

    expected
}

// ── Rename candidates ─────────────────────────────────────────────────────────

/// A detected potential type (table) rename: a dropped table whose column
/// structure closely matches a newly-created type.
#[derive(Debug)]
pub struct TypeRenameCandidate {
    pub old_module: String,
    pub old_table: String,
    pub new_module: String,
    pub new_table: String,
    /// Python-level type name for the new type (used in the prompt).
    pub new_type_name: String,
    /// Jaccard similarity of column sets: 0.0–1.0.
    pub confidence: f64,
}

/// A detected potential column rename within an existing table: a dropped
/// column and an added column with the same Postgres type.
#[derive(Debug)]
pub struct ColRenameCandidate {
    pub module: String,
    pub table: String,
    pub old_col: String,
    pub new_col: String,
    pub pg_type: String,
}

/// A column that is being made NOT NULL but currently contains (or could
/// contain) NULL rows, requiring a fill expression to backfill existing rows
/// before the constraint can be added.
#[derive(Debug)]
pub struct FillRequired {
    /// Schema / Postgres schema name of the table.
    pub module: String,
    pub table: String,
    pub column: String,
    pub pg_type: String,
    /// Python-level type name — used in the prompt ("property 'x' of 'Post'").
    pub type_name: String,
    /// True when the column doesn't exist yet (ADD COLUMN); false when it
    /// already exists as nullable (nullability change).
    pub is_new_column: bool,
    /// The schema-level `default=` SQL, if one is declared.  When present it
    /// can be used as an automatic fill expression without prompting the user.
    pub default_sql: Option<String>,
}

// ── Diff operation ─────────────────────────────────────────────────────────────

/// A single DDL operation produced by the diff engine.
#[derive(Debug, Clone)]
pub struct DiffOp {
    pub sql: String,
    /// When true the statement must run outside any transaction wrapper —
    /// i.e. it uses `CONCURRENTLY`. `pylon migration create` will insert a
    /// `-- pylon:step non-transactional` marker before these ops.
    pub non_transactional: bool,
}

// ── Migration steps ─────────────────────────────────────────────────────────
//
// One logical schema-level question ("did you create scalar type 'X'?"),
// bundling every DDL statement that answers it — so an interactive caller
// can confirm/reject one object at a time instead of a flat DDL dump.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Create,
    Alter,
    Drop,
    Rename,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Create => "create",
            Verb::Alter => "alter",
            Verb::Drop => "drop",
            Verb::Rename => "rename",
        }
    }
}

/// Stable identity an emitted step is grouped by. Every DDL statement that
/// answers the same logical question (e.g. every column/FK/trigger change
/// to one table) shares one key and therefore one step.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpKey {
    Module(String),
    /// Covers enums, custom scalar domains, and sequence scalars — all
    /// three share one object identity (a scalar can only be one of them).
    Scalar(String, String),
    /// Covers a concrete type's own table plus everything folded into it:
    /// column/FK diffs, junction tables for its multi-/through-links,
    /// vector/search index columns, and constraint/deletion/signal/cache
    /// triggers — a user thinks of all of these as "altering type X", not
    /// as separate objects.
    Table(String, String),
    Function(String, String),
    View(String, String),
    /// A table's foreign keys, deliberately *not* folded into `Table`.
    ///
    /// Steps render in insertion order and same-key ops merge into the step
    /// that already exists, so an FK keyed by its own table would be emitted
    /// inside that table's `CREATE TABLE` step — before any table created
    /// later. That made correctness depend on `topo_sort_types` finding a
    /// perfect order, which it cannot when links form a cycle (a
    /// self-referencing optional link is enough). Giving foreign keys their
    /// own key puts every one of them after every table, so the order of the
    /// table steps stops mattering.
    ForeignKey(String, String),
}

#[derive(Debug)]
pub struct MigrationStep {
    /// `"did you {verb} {object_desc}?"` — matches the phrasing convention
    /// this feature is modeled on.
    pub prompt: String,
    pub verb: Verb,
    /// e.g. `"scalar type 'perspective::DomainStatus'"`.
    pub object_desc: String,
    /// Every statement that answers this one question, in emission order.
    /// A statement may contain a `\(placeholder)` token — see
    /// `required_input` — that the caller must substitute before executing it.
    pub ddl: Vec<DiffOp>,
    pub op_key: OpKey,
    /// Free-form expressions the caller must supply before this step's `ddl`
    /// is usable — e.g. a conversion expression for a property's type
    /// change. Empty for the common case.
    pub required_input: Vec<RequiredInput>,
}

impl MigrationStep {
    /// This step's DDL with every `\(placeholder)` token substituted —
    /// `overrides.get(placeholder)` if present, else that input's own
    /// `default_expr`. Statements naming no placeholder pass through as-is.
    pub fn resolved_ddl(&self, overrides: &HashMap<String, String>) -> Vec<DiffOp> {
        self.ddl
            .iter()
            .map(|op| {
                let mut sql = op.sql.clone();
                for input in &self.required_input {
                    let value = overrides.get(&input.placeholder).unwrap_or(&input.default_expr);
                    sql = sql.replace(&format!("\\({})", input.placeholder), value);
                }
                DiffOp {
                    sql,
                    non_transactional: op.non_transactional,
                }
            })
            .collect()
    }
}

/// One `\(placeholder)` token embedded in a step's DDL that the caller must
/// resolve to a PyQL expression (compiled via `query::compile_fill_expr`,
/// evaluated against `type_name`) before executing that statement — mirrors
/// how a fill expression is resolved, just for a type-change's conversion
/// expression instead of a backfill.
#[derive(Debug, Clone)]
pub struct RequiredInput {
    /// The `\(name)` token to substitute in the owning step's DDL text.
    pub placeholder: String,
    /// Prompt text for the caller to show the user.
    pub prompt: String,
    /// A reasonable default expression — the caller may offer this and
    /// accept it on an empty response, matching how a fill's declared
    /// default is offered.
    pub default_expr: String,
    /// Qualified type name to compile the user's PyQL expression against.
    pub type_name: String,
}

fn verbosename_module(name: &str) -> String {
    format!("module '{name}'")
}

/// Covers both enums and custom scalar domains — Pylon describes both as
/// "scalar type" in migration prompts, matching how neither reads naturally
/// as its own separate noun to someone reviewing a schema change.
fn verbosename_scalar(module: &str, name: &str) -> String {
    format!("scalar type '{module}::{name}'")
}

fn verbosename_type(module: &str, name: &str) -> String {
    format!("object type '{module}::{name}'")
}

fn verbosename_interface(module: &str, name: &str) -> String {
    format!("interface type '{module}::{name}'")
}

fn verbosename_function(module: &str, name: &str) -> String {
    format!("function '{module}::{name}'")
}

/// Accumulates `DiffOp`s into `MigrationStep`s keyed by `OpKey`, preserving
/// first-insertion order. A key's verb/description are fixed by whichever
/// call inserts it first; later calls under the same key just append DDL
/// (and any required-input entries).
#[derive(Default)]
struct StepBuilder {
    order: Vec<OpKey>,
    drafts: HashMap<OpKey, (Verb, String, Vec<DiffOp>, Vec<RequiredInput>)>,
}

impl StepBuilder {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, key: OpKey, verb: Verb, object_desc: impl Into<String>, op: DiffOp) {
        self.extend(key, verb, object_desc, vec![op]);
    }

    fn extend(&mut self, key: OpKey, verb: Verb, object_desc: impl Into<String>, ops: Vec<DiffOp>) {
        self.extend_with_input(key, verb, object_desc, ops, vec![]);
    }

    fn extend_with_input(
        &mut self,
        key: OpKey,
        verb: Verb,
        object_desc: impl Into<String>,
        ops: Vec<DiffOp>,
        inputs: Vec<RequiredInput>,
    ) {
        if ops.is_empty() && inputs.is_empty() {
            return;
        }
        use std::collections::hash_map::Entry;
        match self.drafts.entry(key.clone()) {
            Entry::Occupied(mut e) => {
                e.get_mut().2.extend(ops);
                e.get_mut().3.extend(inputs);
            }
            Entry::Vacant(e) => {
                e.insert((verb, object_desc.into(), ops, inputs));
                self.order.push(key);
            }
        }
    }

    fn finish(self) -> Vec<MigrationStep> {
        let Self { order, mut drafts } = self;
        order
            .into_iter()
            .map(|key| {
                let (verb, object_desc, ddl, required_input) = drafts.remove(&key).unwrap();
                let prompt = format!("did you {} {}?", verb.as_str(), object_desc);
                MigrationStep {
                    prompt,
                    verb,
                    object_desc,
                    ddl,
                    op_key: key,
                    required_input,
                }
            })
            .collect()
    }
}

/// Guidance for re-diffing after a rejected rename candidate — mirrors the
/// two ambiguity detectors below; a plain create/alter/drop has no
/// alternative reading to search for, so rejecting one of those just
/// excludes it (handled by the interactive caller, not here).
#[derive(Debug, Default, Clone)]
pub struct Guidance {
    /// `(old_module, old_table, new_module, new_table)` tuples the caller
    /// has rejected as a rename — never propose these again.
    pub banned_type_renames: HashSet<(String, String, String, String)>,
    /// `(module, table, old_col, new_col)` tuples the caller has rejected
    /// as a rename.
    pub banned_col_renames: HashSet<(String, String, String, String)>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute ordered DDL SQL strings to bring a database in sync with `target`.
/// All statements use plain (non-CONCURRENTLY) index creation — suitable for
/// `watch` mode where everything runs inside a single transaction.
pub fn diff_schema(target: &SchemaDescriptor, current: &DbState) -> Result<Vec<String>, String> {
    Ok(flatten_ops(diff_inner(target, current, false, &HashMap::new())?)
        .into_iter()
        .map(|op| op.sql)
        .collect())
}

/// Compute ordered `DiffOp`s suitable for a migration file body.
/// Index creation on pre-existing tables uses `CONCURRENTLY` and is marked
/// `non_transactional = true` so `create` can insert step-boundary markers.
pub fn diff_schema_ops(target: &SchemaDescriptor, current: &DbState) -> Result<Vec<DiffOp>, String> {
    Ok(flatten_ops(diff_inner(target, current, true, &HashMap::new())?))
}

/// True when `target` differs from `previous` in ANY way at all — not just
/// the DDL-visible parts `diff_schema_steps`/`diff_schema_ops` can see.
///
/// Some schema semantics have zero physical Postgres footprint: a
/// property/link's `is_readonly` flag, mutation `rewrites`, computed
/// pointers, session/computed globals, and (once added) pub/sub `Channel`
/// declarations are all enforced purely by `pylon-core`'s own compiler
/// consulting `SchemaDescriptor` — there is no column, constraint, or
/// catalog object for live-database introspection (`DbState`) to ever see,
/// no matter how they change. Before this function existed, a schema edit
/// confined to one of these was **structurally invisible** to `migration
/// create`/`watch`: `diff_schema_steps`/`diff_schema_ops` would report zero
/// DDL, the CLI would print "No schema changes detected" and exit, and
/// `_pylon."Schema"` (what every connecting client actually compiles
/// against — see `pylon.client._install_migrated_schema`) would never be
/// updated. That's not "changing it doesn't require a migration" (the
/// intended, documented behavior for e.g. `readonly`) — it's "changing it
/// can *never* be migrated at all," permanently, until some unrelated
/// DDL-visible change happens to piggyback one through.
///
/// Deliberately does **not** enumerate which fields are DDL-invisible —
/// that list has already grown twice (`readonly`/rewrites, then `Channel`)
/// and would silently miss the next one. Instead this compares the two
/// schemas' full JSON content (the same `serde_json` round-trip already
/// used for `_pylon."Schema"` storage — see `SchemaDescriptor::to_json`/
/// `from_json` in `pylon-py`), so *any* field on *any* descriptor —
/// present now or added later — is covered automatically. Callers combine
/// this with `diff_schema_steps`/`diff_schema_ops`'s own result: if there
/// are DDL steps, this check is redundant (a migration is already
/// happening); it only changes behavior in the previously-broken case,
/// zero DDL steps but real content drift.
///
/// `previous = None` (no migration has ever been applied to this database
/// yet) compares against an empty `SchemaDescriptor` — matches `target`
/// only when `target` itself is completely empty.
pub fn schema_content_changed(target: &SchemaDescriptor, previous: Option<&SchemaDescriptor>) -> bool {
    let previous = previous.cloned().unwrap_or_default();
    // `.ok()` + compare-as-Option rather than `.expect()`: a serialization
    // failure here would be a `serde` bug, not a real difference in schema
    // content — but crashing `migration create` over it would be worse
    // than just treating that (should-never-happen) case as "unchanged".
    serde_json::to_value(target).ok() != serde_json::to_value(&previous).ok()
}

/// Compute the diff as one `MigrationStep` per logical schema-level question
/// — for an interactive caller that confirms/rejects one object at a time
/// instead of a flat DDL dump. `fill_index` marks columns whose NOT NULL
/// constraint is deferred to a caller-supplied backfill (see
/// `diff_schema_ops_with_renames_and_fills`'s doc comment).
pub fn diff_schema_steps(
    target: &SchemaDescriptor,
    current: &DbState,
    fill_index: &HashMap<(String, String), HashSet<String>>,
) -> Result<Vec<MigrationStep>, String> {
    diff_inner(target, current, true, fill_index)
}

/// Flattens steps to a plain `DiffOp` list for every non-interactive caller
/// (`watch`, `diff_schema_ops`, squash) — resolving any `\(placeholder)`
/// token to its `RequiredInput::default_expr` along the way, since these
/// callers have no interactive loop to ask the user for an override. The
/// interactive path (`diff_schema_steps`) returns `MigrationStep`s
/// untouched instead, placeholders and all, for the caller to resolve itself.
fn flatten_ops(steps: Vec<MigrationStep>) -> Vec<DiffOp> {
    let no_overrides = HashMap::new();
    steps.iter().flat_map(|s| s.resolved_ddl(&no_overrides)).collect()
}

/// Diff two live-database snapshots (used by squash to capture the net effect
/// of a range of migrations applied to an ephemeral shadow database).
/// "before" = state at the start of the squashed range, "after" = state at
/// the end. Returns DiffOps suitable for a migration file body.
pub fn diff_states(before: &DbState, after: &DbState) -> Vec<DiffOp> {
    diff_states_inner(before, after)
}

/// Detect potential type (table) renames: tables that exist in `current` but
/// not in `target`, paired with types that exist in `target` but not in
/// `current`, where the column-set Jaccard similarity meets a threshold.
/// Candidates already rejected once (`guidance.banned_type_renames`) are
/// never proposed again — a re-diff after "no" should offer something else.
pub fn detect_type_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    guidance: &Guidance,
) -> Vec<TypeRenameCandidate> {
    let target_keys: HashSet<(&str, &str)> = target
        .types
        .iter()
        .filter(|t| !t.abstract_ && !t.junction)
        .map(|t| (t.module.as_str(), t.table.as_str()))
        .collect();
    let current_keys: HashSet<(&str, &str)> = current
        .tables
        .iter()
        .map(|t| (t.schema.as_str(), t.name.as_str()))
        .collect();

    let dropped: Vec<&DbTable> = current
        .tables
        .iter()
        .filter(|t| !target_keys.contains(&(t.schema.as_str(), t.name.as_str())))
        .collect();
    let created: Vec<&TypeDescriptor> = target
        .types
        .iter()
        .filter(|t| !t.abstract_ && !t.junction)
        .filter(|t| !current_keys.contains(&(t.module.as_str(), t.table.as_str())))
        .collect();

    if dropped.is_empty() || created.is_empty() {
        return vec![];
    }

    let mut candidates: Vec<TypeRenameCandidate> = Vec::new();
    for dropped_t in &dropped {
        let old_cols: HashSet<&str> = dropped_t
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| !n.starts_with("__"))
            .collect();
        for new_type in &created {
            let new_cols: HashSet<&str> = new_type.properties.iter().map(|p| p.name.as_str()).collect();
            let intersection = old_cols.intersection(&new_cols).count();
            let union_size = old_cols.union(&new_cols).count();
            if union_size == 0 {
                continue;
            }
            let confidence = intersection as f64 / union_size as f64;
            let banned = guidance.banned_type_renames.contains(&(
                dropped_t.schema.clone(),
                dropped_t.name.clone(),
                new_type.module.clone(),
                new_type.table.clone(),
            ));
            if confidence >= 0.4 && !banned {
                candidates.push(TypeRenameCandidate {
                    old_module: dropped_t.schema.clone(),
                    old_table: dropped_t.name.clone(),
                    new_module: new_type.module.clone(),
                    new_table: new_type.table.clone(),
                    new_type_name: new_type.name.clone(),
                    confidence,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

/// Detect potential column renames within tables that exist in both `current`
/// and `target`. A candidate is a (dropped_col, added_col) pair in the same
/// table with the same Postgres type. Candidates already rejected once
/// (`guidance.banned_col_renames`) are never proposed again.
pub fn detect_col_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    guidance: &Guidance,
) -> Vec<ColRenameCandidate> {
    let cur_tables: HashMap<(&str, &str), &DbTable> = current
        .tables
        .iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();

    let mut candidates: Vec<ColRenameCandidate> = Vec::new();
    for td in &target.types {
        if td.abstract_ || td.junction {
            continue;
        }
        let Some(cur) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) else {
            continue;
        };

        // Target columns as owned Vec to avoid temporary String lifetime issues.
        let target_cols: Vec<(String, String)> = td
            .properties
            .iter()
            .map(|p| (p.name.clone(), col_type_str(p).to_string()))
            .chain(
                td.links
                    .iter()
                    .filter(|l| !l.is_junction_backed())
                    .map(|l| (format!("{}_id", l.name), "uuid".to_string())),
            )
            .collect();

        // Current columns (skip internal __*__ columns).
        let cur_cols: Vec<(&str, &str)> = cur
            .columns
            .iter()
            .filter(|c| !c.name.starts_with("__"))
            .map(|c| (c.name.as_str(), c.pg_type.as_str()))
            .collect();

        // Dropped: in current but not in target.
        let dropped: Vec<(&str, &str)> = cur_cols
            .iter()
            .copied()
            .filter(|(name, _)| !target_cols.iter().any(|(t, _)| t.as_str() == *name))
            .collect();
        // Added: in target but not in current.
        let added: Vec<(&str, &str)> = target_cols
            .iter()
            .filter(|(name, _)| !cur_cols.iter().any(|&(c, _)| c == name.as_str()))
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();

        if dropped.is_empty() || added.is_empty() {
            continue;
        }

        // Match dropped↔added pairs by Postgres type.
        // Only propose when unambiguous: exactly one dropped and one added per type.
        let mut dropped_by_type: HashMap<&str, Vec<&str>> = HashMap::new();
        for (name, pg_type) in &dropped {
            dropped_by_type.entry(pg_type).or_default().push(name);
        }
        let mut added_by_type: HashMap<&str, Vec<&str>> = HashMap::new();
        for (name, pg_type) in &added {
            added_by_type.entry(pg_type).or_default().push(name);
        }

        for (pg_type, dropped_names) in &dropped_by_type {
            if let Some(added_names) = added_by_type.get(pg_type)
                && dropped_names.len() == 1
                && added_names.len() == 1
            {
                let banned = guidance.banned_col_renames.contains(&(
                    td.module.clone(),
                    td.table.clone(),
                    dropped_names[0].to_string(),
                    added_names[0].to_string(),
                ));
                if !banned {
                    candidates.push(ColRenameCandidate {
                        module: td.module.clone(),
                        table: td.table.clone(),
                        old_col: dropped_names[0].to_string(),
                        new_col: added_names[0].to_string(),
                        pg_type: pg_type.to_string(),
                    });
                }
            }
        }
    }
    candidates
}

/// Detect all properties that are being made NOT NULL but whose existing rows
/// may contain NULL values and therefore require a fill expression.
///
/// Returns a `FillRequired` for:
/// - New NOT NULL columns without a schema-level `default=` on existing tables.
/// - Existing nullable columns that the target marks as NOT NULL (with or without
///   a default — the default is surfaced as `default_sql` for auto-fill).
///
/// New tables are excluded (no rows yet).  PK columns are excluded (always NOT
/// NULL by definition).
pub fn detect_fill_required(target: &SchemaDescriptor, current: &DbState) -> Vec<FillRequired> {
    let cur_tables: HashMap<(&str, &str), &DbTable> = current
        .tables
        .iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();

    let mut result: Vec<FillRequired> = Vec::new();

    for td in &target.types {
        if td.abstract_ || td.junction {
            continue;
        }
        let Some(cur) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) else {
            continue;
        };

        let cur_col_map: HashMap<&str, &DbColumn> = cur.columns.iter().map(|c| (c.name.as_str(), c)).collect();

        for p in &td.properties {
            if p.nullable || p.is_pk {
                continue;
            }
            match cur_col_map.get(p.name.as_str()) {
                None => {
                    // New required column — only needs a fill when there's no default.
                    if p.default_sql.is_none() {
                        result.push(FillRequired {
                            module: td.module.clone(),
                            table: td.table.clone(),
                            column: p.name.clone(),
                            pg_type: col_type_str(p).to_string(),
                            type_name: td.name.clone(),
                            is_new_column: true,
                            default_sql: None,
                        });
                    }
                }
                Some(cur_col) if cur_col.nullable => {
                    // Existing nullable column becoming NOT NULL.
                    result.push(FillRequired {
                        module: td.module.clone(),
                        table: td.table.clone(),
                        column: p.name.clone(),
                        pg_type: col_type_str(p).to_string(),
                        type_name: td.name.clone(),
                        is_new_column: false,
                        default_sql: p.default_sql.clone(),
                    });
                }
                _ => {}
            }
        }

        for l in &td.links {
            if l.nullable || l.is_junction_backed() {
                continue;
            }
            let col = format!("{}_id", l.name);
            match cur_col_map.get(col.as_str()) {
                None => {
                    result.push(FillRequired {
                        module: td.module.clone(),
                        table: td.table.clone(),
                        column: col,
                        pg_type: "uuid".to_string(),
                        type_name: td.name.clone(),
                        is_new_column: true,
                        default_sql: None,
                    });
                }
                Some(cur_col) if cur_col.nullable => {
                    result.push(FillRequired {
                        module: td.module.clone(),
                        table: td.table.clone(),
                        column: col,
                        pg_type: "uuid".to_string(),
                        type_name: td.name.clone(),
                        is_new_column: false,
                        default_sql: None,
                    });
                }
                _ => {}
            }
        }
    }
    result
}

/// Rewrite `state` as though the confirmed renames had already happened, so
/// the diff that follows sees the new names as present and reports only the
/// differences that aren't the rename.
///
/// The rename DDL itself is *not* emitted here, because its two callers
/// disagree about who owns it: the flat-`DiffOp` form emits it alongside
/// this, while the `MigrationStep` form leaves it to whoever resolved the
/// renames in the first place. What they can't disagree about is this edit
/// — it has to describe exactly what the rename DDL does, or the diff
/// re-derives as a DROP + CREATE the very rename it was told about.
fn apply_renames(
    state: &mut DbState,
    type_renames: &[(String, String, String, String)],
    col_renames: &[(String, String, String, String)],
) {
    for (old_mod, old_table, new_mod, new_table) in type_renames {
        if let Some(t) = state
            .tables
            .iter_mut()
            .find(|t| &t.schema == old_mod && &t.name == old_table)
        {
            t.schema = new_mod.clone();
            t.name = new_table.clone();
        }
    }
    for (module, table, old_col, new_col) in col_renames {
        if let Some(t) = state
            .tables
            .iter_mut()
            .find(|t| &t.schema == module && &t.name == table)
            && let Some(col) = t.columns.iter_mut().find(|c| &c.name == old_col)
        {
            col.name = new_col.clone();
        }
    }
}

/// Full diff with confirmed renames and fill expressions applied.
///
/// `type_renames`: `(old_module, old_table, new_module, new_table)`.
/// `col_renames`:  `(module, table, old_col, new_col)`.
/// `fills`:        `(module, table, column, sql_expr)` — one entry per column
///                 that needs a backfill before its NOT NULL constraint is set.
///
/// For each fill the function emits:
///   1. `UPDATE … SET col = expr WHERE col IS NULL;`
///   2. `ALTER TABLE … ALTER COLUMN col SET NOT NULL;`
///
/// For newly-added NOT NULL columns that are in the fills list the ADD COLUMN
/// is emitted as nullable so the fill can succeed before the constraint is set.
pub fn diff_schema_ops_with_renames_and_fills(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: &[(String, String, String, String)],
    col_renames: &[(String, String, String, String)],
    fills: &[(String, String, String, String)],
) -> Result<Vec<DiffOp>, String> {
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut modified = current.clone();

    // ── Emit the rename DDL ───────────────────────────────────────────────────
    for (old_mod, old_table, new_mod, new_table) in type_renames {
        if old_mod == new_mod {
            push_tx(
                &mut ops,
                format!("ALTER TABLE {} RENAME TO {};", qn(old_mod, old_table), qi(new_table)),
            );
        } else {
            push_tx(
                &mut ops,
                format!("ALTER TABLE {} SET SCHEMA {};", qn(old_mod, old_table), qi(new_mod)),
            );
            push_tx(
                &mut ops,
                format!("ALTER TABLE {} RENAME TO {};", qn(new_mod, old_table), qi(new_table)),
            );
        }
    }
    for (module, table, old_col, new_col) in col_renames {
        push_tx(
            &mut ops,
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                qn(module, table),
                qi(old_col),
                qi(new_col)
            ),
        );
    }

    // ── ...and make the baseline match what that DDL will have done ───────────
    apply_renames(&mut modified, type_renames, col_renames);

    // ── Build fill index so emit_column_diff can defer NOT NULL for fills ─────
    let mut fill_index: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for (module, table, col, _) in fills {
        fill_index
            .entry((module.clone(), table.clone()))
            .or_default()
            .insert(col.clone());
    }

    // ── Standard diff (renames already resolved in `modified`) ───────────────
    let mut diff_ops = flatten_ops(diff_inner(target, &modified, true, &fill_index)?);
    ops.append(&mut diff_ops);

    // ── Fill DDL: UPDATE backfill + SET NOT NULL ──────────────────────────────
    for (module, table, col, fill_expr) in fills {
        push_tx(
            &mut ops,
            format!(
                "UPDATE {} SET {} = {} WHERE {} IS NULL;",
                qn(module, table),
                qi(col),
                fill_expr,
                qi(col)
            ),
        );
        push_tx(
            &mut ops,
            format!(
                "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                qn(module, table),
                qi(col)
            ),
        );
    }

    Ok(ops)
}

/// Like `diff_schema_ops_with_renames_and_fills` but returns `MigrationStep`s
/// grouped by object instead of a flat `DiffOp` list — for the interactive
/// confirmation loop. Renames themselves aren't represented as steps here;
/// the caller resolves those first (see `detect_type_renames`/
/// `detect_col_renames` + `Guidance`) and passes the confirmed set in, same
/// as the flat-`DiffOp` version. A fill's `UPDATE` + `SET NOT NULL` folds
/// into its own table's step (falling back to a standalone step in the rare
/// case that table has no other change in this diff).
pub fn diff_schema_steps_with_renames_and_fills(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: &[(String, String, String, String)],
    col_renames: &[(String, String, String, String)],
    fills: &[(String, String, String, String)],
) -> Result<Vec<MigrationStep>, String> {
    let mut modified = current.clone();
    apply_renames(&mut modified, type_renames, col_renames);

    let mut fill_index: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for (module, table, col, _) in fills {
        fill_index
            .entry((module.clone(), table.clone()))
            .or_default()
            .insert(col.clone());
    }

    let mut steps = diff_inner(target, &modified, true, &fill_index)?;

    for (module, table, col, fill_expr) in fills {
        let fill_ops = vec![
            DiffOp {
                sql: format!(
                    "UPDATE {} SET {} = {} WHERE {} IS NULL;",
                    qn(module, table),
                    qi(col),
                    fill_expr,
                    qi(col)
                ),
                non_transactional: false,
            },
            DiffOp {
                sql: format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                    qn(module, table),
                    qi(col)
                ),
                non_transactional: false,
            },
        ];
        match steps
            .iter_mut()
            .find(|s| matches!(&s.op_key, OpKey::Table(m, t) if m == module && t == table))
        {
            Some(step) => step.ddl.extend(fill_ops),
            None => steps.push(MigrationStep {
                prompt: format!("did you {} {}?", Verb::Alter.as_str(), verbosename_type(module, table)),
                verb: Verb::Alter,
                object_desc: verbosename_type(module, table),
                ddl: fill_ops,
                required_input: vec![],
                op_key: OpKey::Table(module.clone(), table.clone()),
            }),
        }
    }

    Ok(steps)
}

// ── Identifier helpers ────────────────────────────────────────────────────────

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn pg_schema(module: &str) -> String {
    if module == "default" {
        "\"public\"".into()
    } else {
        qi(module)
    }
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", pg_schema(schema), qi(name))
}

// ── Topological sort (referenced types before referencing) ────────────────────

/// Referenced types before referencing ones, best-effort.
///
/// **A cycle here is not an error.** `emit_create_table` emits plain `uuid`
/// link columns with no `REFERENCES` clause — every FK is added afterwards, in
/// Phase 6, once all the tables exist — so no link actually constrains
/// creation order, and a cycle of them constrains nothing either. This used to
/// reject any back-edge, which failed on perfectly ordinary shapes: a
/// self-referencing optional link (`WorkflowAction.parent -> WorkflowAction`,
/// `QuestionnaireChapter.parent -> QuestionnaireChapter`) is a tree, not an
/// impossibility. The cycle that *is* impossible — one made of *required*
/// links, which no INSERT could ever satisfy — is rejected by the schema
/// walker at `pylon.finalize()` time, with a message naming the cycle and how
/// to break it. Back-edges are simply skipped, leaving the ordering a hint.
fn topo_sort_types(types: &[TypeDescriptor], polymorphic: &HashSet<String>) -> Vec<usize> {
    let idx_of: HashMap<String, usize> = types
        .iter()
        .enumerate()
        .map(|(i, t)| (format!("{}::{}", t.module, t.name), i))
        .collect();

    // Three-colour DFS: 0=unvisited, 1=in current path (gray), 2=done (black).
    let mut colour = vec![0u8; types.len()];
    let mut order: Vec<usize> = Vec::with_capacity(types.len());

    fn visit(
        i: usize,
        types: &[TypeDescriptor],
        idx_of: &HashMap<String, usize>,
        polymorphic: &HashSet<String>,
        colour: &mut Vec<u8>,
        order: &mut Vec<usize>,
    ) {
        if colour[i] != 0 {
            return; // already done, or a back-edge we are deliberately ignoring
        }
        colour[i] = 1;
        // Multilinks — and junction-backed single links, stored the same way —
        // keep their columns on the junction table, and a link to an interface
        // gets no FK at all, so none of those order anything either.
        for l in &types[i].links {
            if l.is_junction_backed() || polymorphic.contains(&l.target) {
                continue;
            }
            if let Some(&dep) = idx_of.get(&l.target) {
                visit(dep, types, idx_of, polymorphic, colour, order);
            }
        }
        colour[i] = 2;
        order.push(i);
    }

    for i in 0..types.len() {
        visit(i, types, &idx_of, polymorphic, &mut colour, &mut order);
    }
    order
}

/// A property's actual DDL column type: its own registered-scalar DOMAIN
/// name when it has one (`column_type`), else its plain `pg_type` (with the
/// `__nt__:` nominal-tuple marker resolved to `jsonb`).
fn col_type_str(p: &crate::schema::PropertyDescriptor) -> &str {
    p.column_type
        .as_deref()
        .unwrap_or_else(|| p.pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(&p.pg_type))
}

/// Maps a Pylon-internal base `pg_type` spelling to PostgreSQL's own
/// canonical `format_type()` display name (`int8` -> `bigint`, etc.), so a
/// freshly-introspected `DbColumn.pg_type` can be compared against a
/// target schema's type without every alias spelling looking like drift.
/// Recurses into array element types (`int8[]` -> `bigint[]`).
fn canonical_pg_type(pg_type: &str) -> String {
    if let Some(elem) = pg_type.strip_suffix("[]") {
        return format!("{}[]", canonical_pg_type(elem));
    }
    match pg_type {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "timestamptz" => "timestamp with time zone",
        "timestamp" => "timestamp without time zone",
        "time" => "time without time zone",
        other => other,
    }
    .to_string()
}

/// The bare (unqualified, unquoted) identifier at the end of a possibly
/// schema-qualified, possibly-quoted PostgreSQL type reference — e.g.
/// `"public"."Gender"`, `public."Gender"`, and `"Gender"` all yield
/// `Gender`. `format_type()` omits the schema qualifier whenever it's on
/// `search_path` (which a live column's introspected type always is, but
/// Pylon's own qualified references never bother checking), so comparing
/// only this trailing segment is what lets a domain/enum-typed column's
/// *type itself* changing be distinguished from a same-domain column that
/// merely looks unqualified.
fn bare_type_name(pg_type: &str) -> &str {
    pg_type.rsplit('.').next().unwrap_or(pg_type).trim_matches('"')
}

/// Whether a property's own DDL column type (`col_type_str`'s output —
/// either a registered scalar's schema-qualified DOMAIN reference or a
/// plain base type) actually differs from a live column's introspected
/// `format_type()` string, warranting `ALTER COLUMN ... TYPE`.
fn pg_type_changed(target: &str, current: &str) -> bool {
    if target.starts_with('"') {
        bare_type_name(target) != bare_type_name(current)
    } else {
        canonical_pg_type(target) != canonical_pg_type(current)
    }
}

// ── Core diff implementation ──────────────────────────────────────────────────

fn diff_inner(
    target: &SchemaDescriptor,
    current: &DbState,
    for_migration: bool,
    fill_index: &HashMap<(String, String), HashSet<String>>,
) -> Result<Vec<MigrationStep>, String> {
    let mut steps = StepBuilder::new();

    let cur_schemas: HashSet<&str> = current.schemas.iter().map(|s| s.as_str()).collect();
    let cur_tables: HashMap<(&str, &str), &DbTable> = current
        .tables
        .iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();
    let cur_enums: HashMap<(&str, &str), &DbEnum> = current
        .enums
        .iter()
        .map(|e| ((e.schema.as_str(), e.name.as_str()), e))
        .collect();
    let cur_domains: HashSet<(&str, &str)> = current
        .domains
        .iter()
        .map(|d| (d.schema.as_str(), d.name.as_str()))
        .collect();
    let cur_sequences: HashSet<(&str, &str)> = current
        .sequences
        .iter()
        .map(|s| (s.schema.as_str(), s.name.as_str()))
        .collect();
    let cur_views: HashMap<(&str, &str), &str> = current
        .views
        .iter()
        .map(|v| ((v.schema.as_str(), v.name.as_str()), v.body_hash.as_str()))
        .collect();
    let cur_functions: HashMap<(&str, &str), &str> = current
        .functions
        .iter()
        .map(|f| ((f.schema.as_str(), f.name.as_str()), f.body_hash.as_str()))
        .collect();

    let type_map: HashMap<String, (&str, &str)> = target
        .types
        .iter()
        .map(|t| {
            (
                format!("{}::{}", t.module, t.name),
                (t.module.as_str(), t.table.as_str()),
            )
        })
        .collect();
    let polymorphic = crate::export::polymorphic_types(target);

    // Every kind of top-level declaration can live in a module of its own,
    // including one with no types/scalars/enums at all (see the matching
    // comment on `export::emit_schemas`, which had the same gap — a
    // function/global/alias-only module's own CREATE SCHEMA step was never
    // generated, so its first CREATE FUNCTION/etc. failed outright).
    let mut target_schemas: HashSet<String> = HashSet::new();
    for t in &target.types {
        target_schemas.insert(t.module.clone());
    }
    for e in &target.enums {
        target_schemas.insert(e.module.clone());
    }
    for s in &target.scalars {
        target_schemas.insert(s.module.clone());
    }
    for f in &target.functions {
        target_schemas.insert(f.module.clone());
    }
    for g in &target.globals {
        target_schemas.insert(g.module.clone());
    }
    for a in &target.aliases {
        target_schemas.insert(a.module.clone());
    }

    // ── Phase 1: modules ─────────────────────────────────────────────────────
    for module in &target_schemas {
        if module == "default" {
            continue;
        } // public always exists
        if !cur_schemas.contains(module.as_str()) {
            steps.push(
                OpKey::Module(module.clone()),
                Verb::Create,
                verbosename_module(module),
                DiffOp {
                    sql: format!("CREATE SCHEMA IF NOT EXISTS {};", pg_schema(module)),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 2: enums (described as "scalar type", matching how a custom
    // scalar domain is described — see `verbosename_scalar`) ─────────────────
    for e in &target.enums {
        match cur_enums.get(&(e.module.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e
                    .members
                    .iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                steps.push(
                    OpKey::Scalar(e.module.clone(), e.name.clone()),
                    Verb::Create,
                    verbosename_scalar(&e.module, &e.name),
                    DiffOp {
                        sql: format!(
                            "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                         EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                            pg_schema(&e.module),
                            qi(&e.name),
                            members.join(", ")
                        ),
                        non_transactional: false,
                    },
                );
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        steps.push(
                            OpKey::Scalar(e.module.clone(), e.name.clone()),
                            Verb::Alter,
                            verbosename_scalar(&e.module, &e.name),
                            DiffOp {
                                sql: format!(
                                    "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                                    pg_schema(&e.module),
                                    qi(&e.name),
                                    member.replace('\'', "''")
                                ),
                                non_transactional: false,
                            },
                        );
                    }
                }
            }
        }
    }

    // ── Phase 2.5: sequences (for sequence scalars — folded into the same
    // scalar's step, see `OpKey::Scalar`'s doc comment) ──────────────────────
    for s in &target.scalars {
        if s.is_sequence {
            let seq_name = format!("{}_seq", s.name);
            if !cur_sequences.contains(&(s.module.as_str(), seq_name.as_str())) {
                let verb = if cur_domains.contains(&(s.module.as_str(), s.name.as_str())) {
                    Verb::Alter
                } else {
                    Verb::Create
                };
                steps.push(
                    OpKey::Scalar(s.module.clone(), s.name.clone()),
                    verb,
                    verbosename_scalar(&s.module, &s.name),
                    DiffOp {
                        sql: format!(
                            "CREATE SEQUENCE IF NOT EXISTS {}.{};",
                            pg_schema(&s.module),
                            qi(&seq_name)
                        ),
                        non_transactional: false,
                    },
                );
            }
        }
    }

    // ── Phase 3: custom scalar domains ───────────────────────────────────────
    for s in &target.scalars {
        if !cur_domains.contains(&(s.module.as_str(), s.name.as_str())) {
            let checks: Vec<String> = s
                .check_constraints
                .iter()
                .map(|c| format!("    CHECK ({})", c))
                .collect();
            let check_clause = if checks.is_empty() {
                String::new()
            } else {
                format!("\n{}", checks.join("\n"))
            };
            steps.push(
                OpKey::Scalar(s.module.clone(), s.name.clone()),
                Verb::Create,
                verbosename_scalar(&s.module, &s.name),
                DiffOp {
                    sql: format!(
                        "DO $do$ BEGIN CREATE DOMAIN {}.{} AS {}{}; \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $do$;",
                        pg_schema(&s.module),
                        qi(&s.name),
                        s.pg_type,
                        check_clause
                    ),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 3.5: scalar functions (before tables — table DEFAULTs may call them) ──
    let scalar_fn_ddls = crate::export::scalar_function_ddl_with_names(target).map_err(|e| e.to_string())?;
    for (module, name, ddl) in scalar_fn_ddls {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_functions
                .get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit {
            let verb = if cur_functions.contains_key(&(module.as_str(), name.as_str())) {
                Verb::Alter
            } else {
                Verb::Create
            };
            steps.push(
                OpKey::Function(module.clone(), name.clone()),
                verb,
                verbosename_function(&module, &name),
                DiffOp {
                    sql: ddl,
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 4 & 5: tables (create new or alter existing) ───────────────────
    let sort_order = topo_sort_types(&target.types, &polymorphic);

    // Track which tables are created in this diff (needed for CONCURRENTLY decision).
    let mut new_tables: HashSet<(String, String)> = HashSet::new();

    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        let key = (td.module.as_str(), td.table.as_str());
        match cur_tables.get(&key) {
            None => {
                let mut local: Vec<DiffOp> = Vec::new();
                emit_create_table(td, target, &mut local);
                steps.extend(
                    OpKey::Table(td.module.clone(), td.table.clone()),
                    Verb::Create,
                    verbosename_type(&td.module, &td.name),
                    local,
                );
                new_tables.insert((td.module.clone(), td.table.clone()));
            }
            Some(existing) => {
                let fill_cols = fill_index
                    .get(&(td.module.clone(), td.table.clone()))
                    .cloned()
                    .unwrap_or_default();
                let mut local: Vec<DiffOp> = Vec::new();
                let mut inputs: Vec<RequiredInput> = Vec::new();
                emit_column_diff(td, existing, &mut local, for_migration, &fill_cols, target, &mut inputs);
                steps.extend_with_input(
                    OpKey::Table(td.module.clone(), td.table.clone()),
                    Verb::Alter,
                    verbosename_type(&td.module, &td.name),
                    local,
                    inputs,
                );
            }
        }
    }

    // ── Phase 6: FK constraints for new and existing tables (folded into the
    // same table step) ────────────────────────────────────────────────────────
    // Must run for brand-new tables too (`emit_create_table` only emits plain
    // `uuid` link columns, never a `REFERENCES` clause) — a table absent from
    // `cur_tables` still needs every one of its FKs added, just against an
    // empty "already has" set instead of an introspected one.
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str())).copied();
        let verb = if new_tables.contains(&(td.module.clone(), td.table.clone())) {
            Verb::Create
        } else {
            Verb::Alter
        };
        let mut local: Vec<DiffOp> = Vec::new();
        emit_fk_diff(td, existing, &type_map, &polymorphic, &mut local);
        steps.extend(
            OpKey::ForeignKey(td.module.clone(), td.table.clone()),
            verb,
            verbosename_type(&td.module, &td.name),
            local,
        );
    }

    // ── Phase 6b: junction target FKs ─────────────────────────────────────────
    // Keyed by the junction table rather than its owner, so these land in a
    // step of their own after every `CREATE TABLE` — the target of a junction
    // is an unrelated type whose table the owner's own step cannot depend on.
    for (jt_module, jt_name, cname, ddl) in crate::export::junction_fk_constraints(target, &type_map) {
        let already_there = cur_tables
            .get(&(jt_module.as_str(), jt_name.as_str()))
            .map(|t| t.foreign_keys.iter().any(|fk| fk.constraint_name == cname))
            .unwrap_or(false);
        if already_there {
            continue;
        }
        let verb = if cur_tables.contains_key(&(jt_module.as_str(), jt_name.as_str())) {
            Verb::Alter
        } else {
            Verb::Create
        };
        let mut local: Vec<DiffOp> = Vec::new();
        push_tx(&mut local, ddl);
        steps.extend(
            OpKey::ForeignKey(jt_module.clone(), jt_name.clone()),
            verb,
            format!("link table '{}.{}'", jt_module, jt_name),
            local,
        );
    }

    // ── Phase 7: junction tables for new multi-links (and junction-backed
    // single links, which share the exact same junction-table machinery,
    // just capped to one row per source) — folded into the owning type's
    // step, since a user thinks of this as "altering/creating the type to
    // add a multi-link", not as a separate table ────────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        let owner_key = OpKey::Table(td.module.clone(), td.table.clone());
        let owner_verb = if new_tables.contains(&(td.module.clone(), td.table.clone())) {
            Verb::Create
        } else {
            Verb::Alter
        };
        let owner_desc = verbosename_type(&td.module, &td.name);
        for ml in &td.multilinks {
            let jt = format!("{}.{}", td.table, ml.name);
            if !cur_tables.contains_key(&(td.module.as_str(), jt.as_str())) {
                let mut local: Vec<DiffOp> = Vec::new();
                emit_junction_table(
                    td,
                    &ml.name,
                    ml.through.as_deref(),
                    target,
                    false,
                    false,
                    &mut local,
                );
                steps.extend(owner_key.clone(), owner_verb, owner_desc.clone(), local);
                new_tables.insert((td.module.clone(), jt));
            }
        }
        for l in &td.links {
            if !l.is_junction_backed() {
                continue;
            }
            let jt = format!("{}.{}", td.table, l.name);
            if !cur_tables.contains_key(&(td.module.as_str(), jt.as_str())) {
                let mut local: Vec<DiffOp> = Vec::new();
                emit_junction_table(
                    td,
                    &l.name,
                    l.through.as_deref(),
                    target,
                    true,
                    l.is_exclusive,
                    &mut local,
                );
                steps.extend(owner_key.clone(), owner_verb, owner_desc.clone(), local);
                new_tables.insert((td.module.clone(), jt));
            }
        }
    }

    // ── Phase 8: vector columns + indexes (folded into the owning type's step) ──
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.vector_indexes.is_empty() {
            continue;
        }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        let table_is_new = new_tables.contains(&(td.module.clone(), td.table.clone()));
        let owner_key = OpKey::Table(td.module.clone(), td.table.clone());
        let owner_verb = if table_is_new { Verb::Create } else { Verb::Alter };
        let owner_desc = verbosename_type(&td.module, &td.name);

        for vi in &td.vector_indexes {
            let col = vi.column_name();
            if existing
                .map(|t| t.columns.iter().any(|c| c.name == col))
                .unwrap_or(false)
            {
                continue;
            }
            let mut local: Vec<DiffOp> = Vec::new();
            push_tx(
                &mut local,
                format!(
                    "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} vector({});",
                    qn(&td.module, &td.table),
                    qi(&col),
                    vi.dimensions
                ),
            );
            let idx_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(n) => format!("{}__vector_{}__", td.table, n),
            };
            // Pre-existing tables: CONCURRENTLY (non-transactional step in migration).
            // New tables: plain CREATE INDEX (no rows, no locking concern).
            let use_concurrently = for_migration && !table_is_new;
            let idx_sql = if use_concurrently {
                format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING hnsw ({} {});",
                    qi(&idx_name),
                    qn(&td.module, &td.table),
                    qi(&col),
                    vi.ops_class()
                )
            } else {
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} USING hnsw ({} {});",
                    qi(&idx_name),
                    qn(&td.module, &td.table),
                    qi(&col),
                    vi.ops_class()
                )
            };
            local.push(DiffOp {
                sql: idx_sql,
                non_transactional: use_concurrently,
            });
            steps.extend(owner_key.clone(), owner_verb, owner_desc.clone(), local);
        }
    }

    // ── Phase 9: search tsvector columns + GIN indexes (folded likewise) ─────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.search_indexes.is_empty() {
            continue;
        }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        let table_is_new = new_tables.contains(&(td.module.clone(), td.table.clone()));
        let owner_key = OpKey::Table(td.module.clone(), td.table.clone());
        let owner_verb = if table_is_new { Verb::Create } else { Verb::Alter };
        let owner_desc = verbosename_type(&td.module, &td.name);

        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }
            let col = si.column_name();
            if existing
                .map(|t| t.columns.iter().any(|c| c.name == col))
                .unwrap_or(false)
            {
                continue;
            }
            let mut local: Vec<DiffOp> = Vec::new();
            let parts: Vec<String> = si
                .pointers
                .iter()
                .map(|sf| {
                    format!(
                        "setweight(to_tsvector('english', coalesce({}, '')), '{}')",
                        qi(&sf.name),
                        sf.weight.as_str()
                    )
                })
                .collect();
            let expr = if parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                parts.join(" || ")
            };
            push_tx(
                &mut local,
                format!(
                    "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED;",
                    qn(&td.module, &td.table),
                    qi(&col),
                    expr
                ),
            );
            let idx_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(n) => format!("{}__search_{}__", td.table, n),
            };
            let use_concurrently = for_migration && !table_is_new;
            let idx_sql = if use_concurrently {
                format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING gin ({});",
                    qi(&idx_name),
                    qn(&td.module, &td.table),
                    qi(&col)
                )
            } else {
                format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {} USING gin ({});",
                    qi(&idx_name),
                    qn(&td.module, &td.table),
                    qi(&col)
                )
            };
            local.push(DiffOp {
                sql: idx_sql,
                non_transactional: use_concurrently,
            });
            steps.extend(owner_key.clone(), owner_verb, owner_desc.clone(), local);
        }
    }

    // ── Phase 10: interface views (after all tables exist) ───────────────────
    // In watch mode always re-emit (CREATE OR REPLACE VIEW is idempotent).
    // In migration mode only emit new or changed views.
    for (module, name, ddl) in crate::export::interface_view_ddl_with_names(target) {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_views
                .get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit {
            let verb = if cur_views.contains_key(&(module.as_str(), name.as_str())) {
                Verb::Alter
            } else {
                Verb::Create
            };
            steps.push(
                OpKey::View(module.clone(), name.clone()),
                verb,
                verbosename_interface(&module, &name),
                DiffOp {
                    sql: ddl,
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 10.5: junction-backed exclusive link cross-implementor views ───
    // Same add/hash-diff treatment as Phase 10's interface object views —
    // see `export::interface_junction_view_ddl_with_names`'s own doc comment for
    // why a junction-backed exclusive link needs its own helper view at
    // all (its value lives in each implementor's own separate junction
    // table, never on the owner row). Must run before Phase 11.5, which
    // queries these views from the trigger functions it emits.
    for (module, name, ddl) in crate::export::interface_junction_view_ddl_with_names(target) {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_views
                .get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit {
            let verb = if cur_views.contains_key(&(module.as_str(), name.as_str())) {
                Verb::Alter
            } else {
                Verb::Create
            };
            steps.push(
                OpKey::View(module.clone(), name.clone()),
                verb,
                verbosename_interface(&module, &name),
                DiffOp {
                    sql: ddl,
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 11: object-returning functions (after tables and views exist) ──
    let obj_fn_ddls = crate::export::object_function_ddl_with_names(target).map_err(|e| e.to_string())?;
    for (module, name, ddl) in obj_fn_ddls {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_functions
                .get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit {
            let verb = if cur_functions.contains_key(&(module.as_str(), name.as_str())) {
                Verb::Alter
            } else {
                Verb::Create
            };
            steps.push(
                OpKey::Function(module.clone(), name.clone()),
                verb,
                verbosename_function(&module, &name),
                DiffOp {
                    sql: ddl,
                    non_transactional: false,
                },
            );
        }
    }

    // Every physical table (including multilink/junction-backed-link tables)
    // `target` still wants — computed here, ahead of Phase 11.5, so that
    // phase can tell a table being *fully dropped* (Phase 12) apart from one
    // that's merely losing a trigger. Reused as-is by Phase 12 itself below.
    let mut target_tables: HashSet<(String, String)> = HashSet::new();
    for td in &target.types {
        if !td.abstract_ {
            target_tables.insert((td.module.clone(), td.table.clone()));
        }
        if !td.abstract_ && !td.junction {
            for ml in &td.multilinks {
                target_tables.insert((td.module.clone(), format!("{}.{}", td.table, ml.name)));
            }
            for l in &td.links {
                if !l.is_junction_backed() {
                    continue;
                }
                target_tables.insert((td.module.clone(), format!("{}.{}", td.table, l.name)));
            }
        }
    }

    // ── Phase 11.5: interface exclusive constraint triggers (folded into the
    // owning concrete table's step — a user thinks of these as part of
    // "altering type X", not as separate objects) ────────────────────────────
    {
        let infos = crate::export::interface_exclusive_trigger_infos(target);
        let cur_trigger_map: HashMap<(&str, &str), HashSet<&str>> = current
            .tables
            .iter()
            .map(|t| {
                (
                    (t.schema.as_str(), t.name.as_str()),
                    t.triggers.iter().map(|n| n.as_str()).collect::<HashSet<_>>(),
                )
            })
            .collect();

        // What every table's trigger set *should* be once `target` is fully
        // applied — the single source of truth both the add-decisions below
        // and the final drop phase compare `current` against (see
        // `expected_triggers`'s own doc comment for why this must be one
        // shared computation, not two).
        let expected_trigger_map = expected_triggers(target, &type_map);
        // Track which trigger functions have been emitted in this diff pass.
        let mut fn_emitted: HashSet<String> = HashSet::new();

        // A physical table's (module, name) -> the type whose step its
        // trigger changes should fold into. Concrete tables own themselves;
        // junction tables (a multi-link or junction-backed single link's own
        // physical table) fold into the type that declared that link.
        let owner_of = |module: &str, table: &str| -> (OpKey, Verb, String) {
            for &i in &sort_order {
                let td = &target.types[i];
                if td.abstract_ || td.junction {
                    continue;
                }
                let is_owner = td.module == module
                    && (td.table == table
                        || td
                            .multilinks
                            .iter()
                            .any(|ml| format!("{}.{}", td.table, ml.name) == table)
                        || td
                            .links
                            .iter()
                            .any(|l| l.is_junction_backed() && format!("{}.{}", td.table, l.name) == table));
                if is_owner {
                    let verb = if new_tables.contains(&(td.module.clone(), td.table.clone())) {
                        Verb::Create
                    } else {
                        Verb::Alter
                    };
                    return (
                        OpKey::Table(td.module.clone(), td.table.clone()),
                        verb,
                        verbosename_type(&td.module, &td.name),
                    );
                }
            }
            (
                OpKey::Table(module.to_string(), table.to_string()),
                Verb::Alter,
                verbosename_type(module, table),
            )
        };

        for info in &infos {
            let cur = cur_trigger_map
                .get(&(info.impl_module.as_str(), info.impl_table.as_str()))
                .cloned()
                .unwrap_or_default();
            let need_ins = !cur.contains(info.ins_trigger_name.as_str());
            let need_upd = !cur.contains(info.upd_trigger_name.as_str());
            if need_ins || need_upd {
                let mut local: Vec<DiffOp> = Vec::new();
                if fn_emitted.insert(info.fn_name.clone()) {
                    push_tx(&mut local, info.fn_ddl.clone());
                }
                if need_ins {
                    push_tx(&mut local, info.ins_ddl.clone());
                }
                if need_upd {
                    push_tx(&mut local, info.upd_ddl.clone());
                }
                let (key, verb, desc) = owner_of(&info.impl_module, &info.impl_table);
                steps.extend(key, verb, desc, local);
            }
        }

        // Deletion-policy triggers (single-link Source-side DeleteTarget/
        // DeleteTargetIfOrphan, multilink Source-side same, multilink
        // Target-side DeleteSource) — previously only `export_schema`'s
        // fresh-install path emitted these; the incremental migration path
        // silently never did, so these `on_delete` policies never took
        // effect for anything added via a real migration (confirmed live,
        // `tests/live_execution_on_delete.rs`, before this fix).
        for info in crate::export::deletion_policy_trigger_infos(target, &type_map) {
            let cur = cur_trigger_map
                .get(&(info.table_module.as_str(), info.table_name.as_str()))
                .cloned()
                .unwrap_or_default();
            if !cur.contains(info.trigger_name.as_str()) {
                let (key, verb, desc) = owner_of(&info.table_module, &info.table_name);
                steps.extend(
                    key,
                    verb,
                    desc,
                    vec![DiffOp {
                        sql: info.ddl.clone(),
                        non_transactional: false,
                    }],
                );
            }
        }

        // `@pylon.signal` capture triggers — only for types with a
        // non-empty `signals` list (see `export::signal_trigger_infos`).
        // Adding/removing a signal handler changes the combined `on=`
        // bitmask the walker attaches to the schema, so this naturally
        // adds/drops the trigger on the next migration, same as any other
        // schema change — no separate sync step needed.
        for info in crate::export::signal_trigger_infos(target) {
            let cur = cur_trigger_map
                .get(&(info.table_module.as_str(), info.table_name.as_str()))
                .cloned()
                .unwrap_or_default();
            if !cur.contains(info.trigger_name.as_str()) {
                let (key, verb, desc) = owner_of(&info.table_module, &info.table_name);
                steps.extend(
                    key,
                    verb,
                    desc,
                    vec![DiffOp {
                        sql: info.ddl.clone(),
                        non_transactional: false,
                    }],
                );
            }
        }

        // User-declared schema `Trigger`s — same previously-missing-parity
        // bug as deletion-policy triggers above: only `export_schema`'s
        // fresh-install path used to emit these at all (`emit_triggers` was
        // never called from the incremental migration path), so adding or
        // changing a `Trigger(...)` on an existing type via `migration
        // create` silently did nothing. A changed handler changes this
        // trigger's hash-derived name (`export::trigger_ddl_name`), so an
        // in-place edit surfaces here as "add the new name" — the stale
        // old-named trigger is caught by the generic "drop what's no longer
        // expected" sweep below, using the same `expected_trigger_map`.
        for info in crate::export::user_trigger_infos(target).map_err(|e| e.to_string())? {
            let cur = cur_trigger_map
                .get(&(info.table_module.as_str(), info.table_name.as_str()))
                .cloned()
                .unwrap_or_default();
            if !cur.contains(info.trigger_name.as_str()) {
                let (key, verb, desc) = owner_of(&info.table_module, &info.table_name);
                steps.extend(
                    key,
                    verb,
                    desc,
                    vec![DiffOp {
                        sql: info.ddl.clone(),
                        non_transactional: false,
                    }],
                );
            }
        }

        // Cache-invalidation trigger — every concrete table and every
        // multi-link junction table, unconditionally (not gated by
        // `[cache].enabled`; see the cache layer plan's design decision).
        // Only the per-table attachment is migration content — the trigger
        // *function* itself ships via bootstrap DDL
        // (`stdlib::ddl::CACHE_INVALIDATE_DDL`), applied once by
        // `pylon database initialize`, not per-migration.
        // A HashSet, not a Vec — a junction ("through") type's own `td.table`
        // is the physical join table itself, identical to what the owning
        // type's multilink enumeration below also derives (e.g. `ProductTag`
        // and `Product`'s "tags" multilink both resolve to
        // `"Product.tags"`), so without deduping this would emit the same
        // `CREATE TRIGGER` statement twice for that table.
        let mut cache_trigger_tables: HashSet<(String, String)> = HashSet::new();
        for td in &target.types {
            if td.abstract_ {
                continue;
            }
            cache_trigger_tables.insert((td.module.clone(), td.table.clone()));
            if !td.junction {
                for ml in &td.multilinks {
                    cache_trigger_tables.insert((td.module.clone(), format!("{}.{}", td.table, ml.name)));
                }
                for l in &td.links {
                    if !l.is_junction_backed() {
                        continue;
                    }
                    cache_trigger_tables.insert((td.module.clone(), format!("{}.{}", td.table, l.name)));
                }
            }
        }
        for (module, table) in &cache_trigger_tables {
            let already_present = cur_trigger_map
                .get(&(module.as_str(), table.as_str()))
                .map(|t| t.contains("pylon_cache_invalidate"))
                .unwrap_or(false);
            if !already_present {
                let (key, verb, desc) = owner_of(module, table);
                steps.extend(
                    key,
                    verb,
                    desc,
                    vec![DiffOp {
                        sql: cache_invalidate_trigger_sql(&qn(module, table)),
                        non_transactional: false,
                    }],
                );
            }
        }

        // Drop triggers that no longer exist in the target schema — but not
        // for a table that's being dropped in its entirety (Phase 12): that
        // table's whole existence, triggers included, will be removed via
        // `DROP TABLE ... CASCADE`, so an explicit `DROP TRIGGER` here is
        // both redundant and actively wrong — `owner_of` has no real target
        // type to fold it into for such a table, so it fell back to a bare
        // `Verb::Alter` default, which then won the "first write wins" race
        // for this step's verb/prompt against Phase 12's own correct
        // `Verb::Drop` (confirmed live: `migration create` asked "did you
        // *alter* object type 'Widget'?" for a table being fully dropped).
        for cur_table in &current.tables {
            let key = (cur_table.schema.clone(), cur_table.name.clone());
            if !target_tables.contains(&key) {
                continue;
            }
            let expected = expected_trigger_map.get(&key).cloned().unwrap_or_default();
            for trigger_name in &cur_table.triggers {
                if !expected.contains(trigger_name) {
                    let (owner_key, verb, desc) = owner_of(&cur_table.schema, &cur_table.name);
                    steps.extend(
                        owner_key,
                        verb,
                        desc,
                        vec![
                            DiffOp {
                                sql: format!(
                                    "DROP TRIGGER IF EXISTS {} ON {};",
                                    qi(trigger_name),
                                    qn(&cur_table.schema, &cur_table.name)
                                ),
                                non_transactional: false,
                            },
                            // Generated triggers name their function after
                            // themselves, so dropping one leaves an
                            // unreferenced function behind. That was rare
                            // while trigger names were stable; now that a
                            // name follows its body (see `trigger_names` in
                            // `export`), every codegen change renames, and
                            // the orphans would accumulate — and read as
                            // live triggers to anyone introspecting.
                            //
                            // A no-op for triggers that don't follow that
                            // convention: the cache-invalidate trigger's
                            // function lives in `_pylon`, and the two
                            // interface-exclusive triggers (`..._ins`,
                            // `..._upd`) share one function named after
                            // neither. Nothing shared is reachable by this
                            // name, so this can't drop a function another
                            // trigger still needs.
                            DiffOp {
                                sql: format!(
                                    "DROP FUNCTION IF EXISTS {}();",
                                    qn(&cur_table.schema, trigger_name)
                                ),
                                non_transactional: false,
                            },
                        ],
                    );
                }
            }
        }
    }

    // ── Phase 12: drop removed tables ────────────────────────────────────────
    for cur_table in &current.tables {
        let key = (cur_table.schema.clone(), cur_table.name.clone());
        if !target_tables.contains(&key) {
            steps.push(
                OpKey::Table(cur_table.schema.clone(), cur_table.name.clone()),
                Verb::Drop,
                verbosename_type(&cur_table.schema, &cur_table.name),
                DiffOp {
                    sql: format!(
                        "DROP TABLE IF EXISTS {} CASCADE;",
                        qn(&cur_table.schema, &cur_table.name)
                    ),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 13: drop removed enums ─────────────────────────────────────────
    let target_enum_set: HashSet<(String, String)> = target
        .enums
        .iter()
        .map(|e| (e.module.clone(), e.name.clone()))
        .collect();
    for cur_enum in &current.enums {
        if !target_enum_set.contains(&(cur_enum.schema.clone(), cur_enum.name.clone())) {
            steps.push(
                OpKey::Scalar(cur_enum.schema.clone(), cur_enum.name.clone()),
                Verb::Drop,
                verbosename_scalar(&cur_enum.schema, &cur_enum.name),
                DiffOp {
                    sql: format!(
                        "DROP TYPE IF EXISTS {}.{} CASCADE;",
                        pg_schema(&cur_enum.schema),
                        qi(&cur_enum.name)
                    ),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 14: drop removed domains ───────────────────────────────────────
    let target_domain_set: HashSet<(String, String)> = target
        .scalars
        .iter()
        .map(|s| (s.module.clone(), s.name.clone()))
        .collect();
    for cur_domain in &current.domains {
        if !target_domain_set.contains(&(cur_domain.schema.clone(), cur_domain.name.clone())) {
            steps.push(
                OpKey::Scalar(cur_domain.schema.clone(), cur_domain.name.clone()),
                Verb::Drop,
                verbosename_scalar(&cur_domain.schema, &cur_domain.name),
                DiffOp {
                    sql: format!(
                        "DROP DOMAIN IF EXISTS {}.{} CASCADE;",
                        pg_schema(&cur_domain.schema),
                        qi(&cur_domain.name)
                    ),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 14.5: drop removed sequences (folded into their scalar's drop
    // step by name, matching phase 2.5's own OpKey::Scalar grouping) ────────
    let target_sequence_set: HashSet<(String, String)> = target
        .scalars
        .iter()
        .filter(|s| s.is_sequence)
        .map(|s| (s.module.clone(), format!("{}_seq", s.name)))
        .collect();
    for cur_seq in &current.sequences {
        if !target_sequence_set.contains(&(cur_seq.schema.clone(), cur_seq.name.clone())) {
            let scalar_name = cur_seq.name.strip_suffix("_seq").unwrap_or(&cur_seq.name).to_string();
            steps.push(
                OpKey::Scalar(cur_seq.schema.clone(), scalar_name.clone()),
                Verb::Drop,
                verbosename_scalar(&cur_seq.schema, &scalar_name),
                DiffOp {
                    sql: format!(
                        "DROP SEQUENCE IF EXISTS {}.{};",
                        pg_schema(&cur_seq.schema),
                        qi(&cur_seq.name)
                    ),
                    non_transactional: false,
                },
            );
        }
    }

    // ── Phase 15: drop removed modules ───────────────────────────────────────
    for module in &current.schemas {
        if module == "default" {
            continue;
        } // never drop public
        if !target_schemas.contains(module) {
            steps.push(
                OpKey::Module(module.clone()),
                Verb::Drop,
                verbosename_module(module),
                DiffOp {
                    sql: format!("DROP SCHEMA IF EXISTS {} CASCADE;", pg_schema(module)),
                    non_transactional: false,
                },
            );
        }
    }

    Ok(steps.finish())
}

fn push_tx(ops: &mut Vec<DiffOp>, sql: String) {
    ops.push(DiffOp {
        sql,
        non_transactional: false,
    });
}

// ── Default resolution ────────────────────────────────────────────────────────

/// Return the effective SQL DEFAULT for a property, compiling `default_pyql`
/// with the schema IR compiler if needed.
fn resolve_default(p: &crate::schema::PropertyDescriptor, schema: &SchemaDescriptor) -> Option<String> {
    if let Some(sql) = &p.default_sql {
        return Some(sql.clone());
    }
    if let Some(pyql) = &p.default_pyql {
        return crate::ir::compile_scalar_default(pyql, schema).ok();
    }
    None
}

/// Test-only accessor so `export` can assert its DDL matches what a
/// migration would produce for the same property.
#[cfg(test)]
pub(crate) fn resolve_default_for_test(
    p: &crate::schema::PropertyDescriptor,
    schema: &SchemaDescriptor,
) -> Option<String> {
    resolve_default(p, schema)
}

fn resolve_link_default(l: &crate::schema::LinkDescriptor, schema: &SchemaDescriptor) -> Option<String> {
    if let Some(pyql) = &l.default_pyql {
        return crate::ir::compile_scalar_default(pyql, schema).ok();
    }
    None
}

// ── CREATE TABLE for a new type ───────────────────────────────────────────────

fn emit_create_table(td: &TypeDescriptor, schema: &SchemaDescriptor, ops: &mut Vec<DiffOp>) {
    let mut lines: Vec<String> = Vec::new();
    for p in &td.properties {
        let not_null = if p.nullable { "" } else { " NOT NULL" };
        let default = resolve_default(p, schema)
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        lines.push(format!(
            "    {} {}{}{}",
            qi(&p.name),
            col_type_str(p),
            not_null,
            default
        ));
    }
    for l in &td.links {
        if l.is_junction_backed() {
            continue;
        }
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        let default = resolve_link_default(l, schema)
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        lines.push(format!(
            "    {} uuid{}{}",
            qi(&format!("{}_id", l.name)),
            not_null,
            default
        ));
    }
    let pk_cols: Vec<String> = td.properties.iter().filter(|p| p.is_pk).map(|p| qi(&p.name)).collect();
    if !pk_cols.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }
    push_tx(
        ops,
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{}\n);",
            qn(&td.module, &td.table),
            lines.join(",\n")
        ),
    );
}

/// `CREATE OR REPLACE TRIGGER` statement wiring `qualified_table` into the
/// cache-invalidation notify function (see `stdlib::ddl::CACHE_INVALIDATE_DDL`).
/// Statement-level, not row-level — tier 1 invalidation only needs one notify
/// per write statement.
fn cache_invalidate_trigger_sql(qualified_table: &str) -> String {
    format!(
        "CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON {}\n    FOR EACH STATEMENT EXECUTE FUNCTION _pylon.notify_cache_invalidate();",
        qualified_table
    )
}

// ── Column diff for an existing table ─────────────────────────────────────────

/// `for_migration`: true = migration-file mode, false = watch mode.
/// `fill_cols`: column names that will be backfilled via a fill expression.
///   For migration mode, new NOT NULL columns in this set are added as nullable
///   first (the fill + SET NOT NULL comes later); in watch mode fills are unused.
fn emit_column_diff(
    td: &TypeDescriptor,
    existing: &DbTable,
    ops: &mut Vec<DiffOp>,
    for_migration: bool,
    fill_cols: &HashSet<String>,
    schema: &SchemaDescriptor,
    required_input: &mut Vec<RequiredInput>,
) {
    let existing_col_map: HashMap<&str, &DbColumn> = existing.columns.iter().map(|c| (c.name.as_str(), c)).collect();

    // ── Add new columns ───────────────────────────────────────────────────────
    for p in &td.properties {
        if existing_col_map.contains_key(p.name.as_str()) {
            continue;
        }
        let eff_default = resolve_default(p, schema);
        let needs_fill = for_migration && !p.nullable && eff_default.is_none() && fill_cols.contains(&p.name);
        let not_null = if p.nullable || needs_fill { "" } else { " NOT NULL" };
        let default = eff_default.map(|d| format!(" DEFAULT {}", d)).unwrap_or_default();
        push_tx(
            ops,
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}{}{};",
                qn(&td.module, &td.table),
                qi(&p.name),
                col_type_str(p),
                not_null,
                default
            ),
        );
    }
    for l in &td.links {
        if l.is_junction_backed() {
            continue;
        }
        let col = format!("{}_id", l.name);
        if existing_col_map.contains_key(col.as_str()) {
            continue;
        }
        let eff_default = resolve_link_default(l, schema);
        let needs_fill = for_migration && !l.nullable && eff_default.is_none() && fill_cols.contains(&col);
        let not_null = if l.nullable || needs_fill { "" } else { " NOT NULL" };
        let default = eff_default.map(|d| format!(" DEFAULT {}", d)).unwrap_or_default();
        push_tx(
            ops,
            format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} uuid{}{};",
                qn(&td.module, &td.table),
                qi(&col),
                not_null,
                default
            ),
        );
    }

    // ── Type changes on existing columns ──────────────────────────────────────
    // e.g. a property gaining a registered custom scalar's own DOMAIN (see
    // PropertyDescriptor.column_type), or any other base-type change.
    let type_changes: Vec<(&str, &str)> = td
        .properties
        .iter()
        .filter_map(|p| {
            let cur = existing_col_map.get(p.name.as_str())?;
            if cur.is_generated {
                return None;
            }
            let target_type = col_type_str(p);
            pg_type_changed(target_type, &cur.pg_type).then_some((p.name.as_str(), target_type))
        })
        .collect();
    if !type_changes.is_empty() {
        // A column any interface view selects from can't be retyped directly
        // — Postgres refuses ALTER COLUMN TYPE while a view depends on it —
        // so drop those views first and recreate them (identical DDL,
        // unaffected by a column's own type) right after.
        let affected_views: Vec<(String, String, String)> = crate::export::interface_view_ddl_with_names(schema)
            .into_iter()
            .filter(|(m, n, _)| td.interfaces.contains(&format!("{}::{}", m, n)))
            .collect();
        for (m, n, _) in &affected_views {
            push_tx(ops, format!("DROP VIEW IF EXISTS {};", qn(m, n)));
        }
        for (col, target_type) in &type_changes {
            // The conversion expression is a placeholder, not a blind cast —
            // an interactive caller offers `default_expr` (identical to what
            // this used to emit unconditionally) and lets the user override
            // it with their own PyQL expression before substituting it in;
            // `diff_schema_ops`/`watch` callers that never resolve any
            // `required_input` still get the exact same default behavior via
            // `RequiredInput::default_expr`-as-fallback at the CLI layer.
            let placeholder = format!("cast_expr__{col}");
            let default_expr = format!("{}::{target_type}", qi(col));
            required_input.push(RequiredInput {
                placeholder: placeholder.clone(),
                prompt: format!(
                    "Please specify a conversion expression to alter the type of property '{col}' of {}",
                    verbosename_type(&td.module, &td.name),
                ),
                default_expr,
                type_name: format!("{}::{}", td.module, td.name),
            });
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} TYPE {} USING \\({});",
                    qn(&td.module, &td.table),
                    qi(col),
                    target_type,
                    placeholder
                ),
            );
        }
        for (_, _, ddl) in &affected_views {
            push_tx(ops, ddl.clone());
        }
    }

    // ── Nullability + DEFAULT changes on existing columns ─────────────────────
    for p in &td.properties {
        let Some(cur) = existing_col_map.get(p.name.as_str()) else {
            continue;
        };
        if cur.is_generated {
            continue;
        }
        if !cur.nullable && p.nullable {
            // NOT NULL → nullable: always safe, no fill needed.
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL;",
                    qn(&td.module, &td.table),
                    qi(&p.name)
                ),
            );
        } else if cur.nullable && !p.nullable && !for_migration {
            // nullable → NOT NULL: safe in watch mode (dev DB, typically no rows).
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                    qn(&td.module, &td.table),
                    qi(&p.name)
                ),
            );
            // In migration mode this is intentionally skipped; the fill mechanism
            // emits UPDATE + SET NOT NULL after the main diff body.
        }

        // DEFAULT changes
        let target_default = resolve_default(p, schema);
        let db_default = cur.column_default.as_deref();
        match (&target_default, db_default) {
            (Some(want), Some(have)) if want != have => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                        qn(&td.module, &td.table),
                        qi(&p.name),
                        want
                    ),
                );
            }
            (Some(want), None) => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                        qn(&td.module, &td.table),
                        qi(&p.name),
                        want
                    ),
                );
            }
            (None, Some(_)) => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                        qn(&td.module, &td.table),
                        qi(&p.name)
                    ),
                );
            }
            _ => {}
        }
    }
    for l in &td.links {
        if l.is_junction_backed() {
            continue;
        }
        let col = format!("{}_id", l.name);
        let Some(cur) = existing_col_map.get(col.as_str()) else {
            continue;
        };
        if !cur.nullable && l.nullable {
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL;",
                    qn(&td.module, &td.table),
                    qi(&col)
                ),
            );
        } else if cur.nullable && !l.nullable && !for_migration {
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                    qn(&td.module, &td.table),
                    qi(&col)
                ),
            );
        }

        let target_default = resolve_link_default(l, schema);
        let db_default = cur.column_default.as_deref();
        match (&target_default, db_default) {
            (Some(want), Some(have)) if want != have => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                        qn(&td.module, &td.table),
                        qi(&col),
                        want
                    ),
                );
            }
            (Some(want), None) => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                        qn(&td.module, &td.table),
                        qi(&col),
                        want
                    ),
                );
            }
            (None, Some(_)) => {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                        qn(&td.module, &td.table),
                        qi(&col)
                    ),
                );
            }
            _ => {}
        }
    }

    // ── Drop removed columns ──────────────────────────────────────────────────
    let target_cols: HashSet<String> = td
        .properties
        .iter()
        .map(|p| p.name.clone())
        .chain(
            td.links
                .iter()
                .filter(|l| !l.is_junction_backed())
                .map(|l| format!("{}_id", l.name)),
        )
        .collect();
    for col in &existing.columns {
        let n = col.name.as_str();
        if target_cols.contains(n) {
            continue;
        }
        if n.starts_with("__") && n.ends_with("__") {
            continue;
        }
        push_tx(
            ops,
            format!(
                "ALTER TABLE {} DROP COLUMN IF EXISTS {};",
                qn(&td.module, &td.table),
                qi(n)
            ),
        );
    }
}

// ── FK diff for an existing table ─────────────────────────────────────────────

fn emit_fk_diff(
    td: &TypeDescriptor,
    existing: Option<&DbTable>,
    type_map: &HashMap<String, (&str, &str)>,
    polymorphic: &HashSet<String>,
    ops: &mut Vec<DiffOp>,
) {
    use crate::schema::{DeleteAction, DeleteSide};

    let existing_fk_names: HashSet<&str> = existing
        .map(|e| e.foreign_keys.iter().map(|fk| fk.constraint_name.as_str()).collect())
        .unwrap_or_default();

    for l in &td.links {
        if l.is_junction_backed() {
            continue;
        }
        // Enforced by `export::interface_link_trigger_infos` instead: the
        // target is a view, and PostgreSQL will not reference one.
        if polymorphic.contains(&l.target) {
            continue;
        }
        let cname = format!("{}_{}_fkey", td.table, l.name);
        if existing_fk_names.contains(cname.as_str()) {
            continue;
        }
        let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else {
            continue;
        };
        // See `export::needs_deferred_target_fk`'s doc comment — a
        // Source-side DeleteTarget/DeleteTargetIfOrphan trigger deletes the
        // target while the still-not-yet-removed owner row would otherwise
        // trip an immediate RESTRICT on this very FK (confirmed live via
        // `tests/live_execution_on_delete.rs`); force it deferrable so the
        // check only runs at commit, after both deletes have completed.
        let needs_deferred = crate::export::needs_deferred_target_fk(&l.on_delete);
        let on_delete = l
            .on_delete
            .iter()
            .find(|p| p.side == DeleteSide::Target)
            .map(|p| match &p.action {
                DeleteAction::Restrict if needs_deferred => " DEFERRABLE INITIALLY DEFERRED",
                DeleteAction::Restrict => " ON DELETE RESTRICT",
                DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED",
                DeleteAction::DeleteSource => " ON DELETE CASCADE",
                DeleteAction::Allow => " ON DELETE SET NULL",
                _ => " ON DELETE RESTRICT",
            })
            .unwrap_or(if needs_deferred {
                " DEFERRABLE INITIALLY DEFERRED"
            } else {
                " ON DELETE RESTRICT"
            });
        push_tx(
            ops,
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}(id){};",
                qn(&td.module, &td.table),
                qi(&cname),
                qi(&format!("{}_id", l.name)),
                qn(tgt_module, tgt_table),
                on_delete
            ),
        );
    }
}

// ── Junction table for a new multi-link ───────────────────────────────────────

// Every parameter here is one independent axis of a junction table's shape
// (name, target, through-type, delete policy, cardinality, exclusivity), and
// they come from three different places in the caller. Bundling them into a
// params struct would move the same ten fields one level out without making
// any call site clearer.
#[allow(clippy::too_many_arguments)]
fn emit_junction_table(
    td: &TypeDescriptor,
    ml_name: &str,
    through: Option<&str>,
    schema: &SchemaDescriptor,
    single: bool,
    exclusive: bool,
    ops: &mut Vec<DiffOp>,
) {
    let jt_name = format!("{}.{}", td.table, ml_name);
    let src_on_delete = " ON DELETE CASCADE";
    // The target FK is added by the FK pass below, not inline — the target's
    // own table may not exist yet. See `export::junction_fk_constraints`.
    let mut col_lines = format!(
        "    source uuid NOT NULL REFERENCES {}(id){},\n    target uuid NOT NULL",
        qn(&td.module, &td.table),
        src_on_delete,
    );

    // Extra columns from the through junction type.
    if let Some(through_qname) = through
        && let Some(through_td) = schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == through_qname && t.junction)
    {
        for p in &through_td.properties {
            if p.name == "id" {
                continue;
            }
            let not_null = if p.nullable { "" } else { " NOT NULL" };
            col_lines.push_str(&format!(",\n    {} {}{}", qi(&p.name), col_type_str(p), not_null));
        }
    }

    let pk_clause = if single {
        "PRIMARY KEY (source)"
    } else {
        "PRIMARY KEY (source, target)"
    };
    let unique_clause = if exclusive { ",\n    UNIQUE (target)" } else { "" };
    push_tx(
        ops,
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n{},\n    {}{}\n);",
            qn(&td.module, &jt_name),
            col_lines,
            pk_clause,
            unique_clause,
        ),
    );
}

// ── diff_states: diff two live-DB snapshots (for squash) ──────────────────────

fn diff_states_inner(before: &DbState, after: &DbState) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();

    let before_schemas: HashSet<&str> = before.schemas.iter().map(|s| s.as_str()).collect();
    let before_tables: HashMap<(&str, &str), &DbTable> = before
        .tables
        .iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();
    let before_enums: HashMap<(&str, &str), &DbEnum> = before
        .enums
        .iter()
        .map(|e| ((e.schema.as_str(), e.name.as_str()), e))
        .collect();
    let before_domains: HashSet<(&str, &str)> = before
        .domains
        .iter()
        .map(|d| (d.schema.as_str(), d.name.as_str()))
        .collect();

    // New schemas
    for schema in &after.schemas {
        if schema == "default" {
            continue;
        } // public always exists
        if !before_schemas.contains(schema.as_str()) {
            push_tx(&mut ops, format!("CREATE SCHEMA IF NOT EXISTS {};", pg_schema(schema)));
        }
    }

    // New / altered enums
    for e in &after.enums {
        match before_enums.get(&(e.schema.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e
                    .members
                    .iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                push_tx(
                    &mut ops,
                    format!(
                        "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                        pg_schema(&e.schema),
                        qi(&e.name),
                        members.join(", ")
                    ),
                );
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        push_tx(
                            &mut ops,
                            format!(
                                "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                                pg_schema(&e.schema),
                                qi(&e.name),
                                member.replace('\'', "''")
                            ),
                        );
                    }
                }
            }
        }
    }

    // New domains (simplified: no type reconstruction from DbState)
    for d in &after.domains {
        if !before_domains.contains(&(d.schema.as_str(), d.name.as_str())) {
            // We can't reconstruct the full domain DDL from pg_catalog cheaply;
            // emit a placeholder that will be filled by the squash command.
            push_tx(
                &mut ops,
                format!(
                    "-- TODO: recreate domain {}.{} (reconstruct DDL from source migrations)",
                    pg_schema(&d.schema),
                    qi(&d.name)
                ),
            );
        }
    }

    // New tables
    let mut new_tables: HashSet<(String, String)> = HashSet::new();
    for t in &after.tables {
        let key = (t.schema.as_str(), t.name.as_str());
        match before_tables.get(&key) {
            None => {
                // Emit CREATE TABLE from introspected columns
                emit_create_table_from_db(t, &mut ops);
                new_tables.insert((t.schema.clone(), t.name.clone()));
            }
            Some(before_t) => {
                // Diff columns
                emit_column_diff_from_db(t, before_t, &mut ops);
            }
        }
    }

    // FK constraints for existing tables that gained new FKs
    for t in &after.tables {
        if let Some(before_t) = before_tables.get(&(t.schema.as_str(), t.name.as_str())) {
            let before_fk_names: HashSet<&str> = before_t
                .foreign_keys
                .iter()
                .map(|fk| fk.constraint_name.as_str())
                .collect();
            for fk in &t.foreign_keys {
                if !before_fk_names.contains(fk.constraint_name.as_str()) {
                    // Can't fully reconstruct FK DDL from DbForeignKey without ON DELETE info;
                    // emit best-effort.
                    push_tx(
                        &mut ops,
                        format!(
                            "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{}(id);",
                            pg_schema(&t.schema),
                            qi(&t.name),
                            qi(&fk.constraint_name),
                            qi(&fk.local_column),
                            pg_schema(&fk.ref_schema),
                            qi(&fk.ref_table)
                        ),
                    );
                }
            }
        }
    }

    // New indexes on pre-existing tables (CONCURRENTLY)
    for t in &after.tables {
        let table_is_new = new_tables.contains(&(t.schema.clone(), t.name.clone()));
        let before_idx_names: HashSet<&str> = before_tables
            .get(&(t.schema.as_str(), t.name.as_str()))
            .map(|bt| bt.indexes.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        for idx in &t.indexes {
            if before_idx_names.contains(idx.name.as_str()) {
                continue;
            }
            let use_concurrently = !table_is_new;
            let concurrently = if use_concurrently { "CONCURRENTLY " } else { "" };
            let unique = if idx.is_unique { "UNIQUE " } else { "" };
            let idx_sql = format!(
                "CREATE {unique}INDEX {concurrently}IF NOT EXISTS {} ON {}.{};",
                qi(&idx.name),
                pg_schema(&t.schema),
                qi(&t.name)
            );
            ops.push(DiffOp {
                sql: idx_sql,
                non_transactional: use_concurrently,
            });
        }
    }

    // Drop removed tables
    let after_tables: HashSet<(&str, &str)> = after
        .tables
        .iter()
        .map(|t| (t.schema.as_str(), t.name.as_str()))
        .collect();
    for t in &before.tables {
        if !after_tables.contains(&(t.schema.as_str(), t.name.as_str())) {
            push_tx(
                &mut ops,
                format!("DROP TABLE IF EXISTS {}.{} CASCADE;", pg_schema(&t.schema), qi(&t.name)),
            );
        }
    }

    // Drop removed enums
    let after_enum_set: HashSet<(&str, &str)> = after
        .enums
        .iter()
        .map(|e| (e.schema.as_str(), e.name.as_str()))
        .collect();
    for e in &before.enums {
        if !after_enum_set.contains(&(e.schema.as_str(), e.name.as_str())) {
            push_tx(
                &mut ops,
                format!("DROP TYPE IF EXISTS {}.{} CASCADE;", pg_schema(&e.schema), qi(&e.name)),
            );
        }
    }

    // Drop removed schemas
    let after_schema_set: HashSet<&str> = after.schemas.iter().map(|s| s.as_str()).collect();
    for schema in &before.schemas {
        if schema == "default" {
            continue;
        } // never drop public
        if !after_schema_set.contains(schema.as_str()) {
            push_tx(
                &mut ops,
                format!("DROP SCHEMA IF EXISTS {} CASCADE;", pg_schema(schema)),
            );
        }
    }

    ops
}

fn emit_create_table_from_db(t: &DbTable, ops: &mut Vec<DiffOp>) {
    let mut lines: Vec<String> = Vec::new();
    for col in &t.columns {
        let not_null = if col.nullable { "" } else { " NOT NULL" };
        if col.is_generated {
            // Can't reconstruct the generation expression from DbColumn alone.
            lines.push(format!(
                "    {} {} GENERATED ALWAYS AS (/* see source */) STORED",
                qi(&col.name),
                col.pg_type
            ));
        } else {
            lines.push(format!("    {} {}{}", qi(&col.name), col.pg_type, not_null));
        }
    }
    push_tx(
        ops,
        format!(
            "CREATE TABLE IF NOT EXISTS {}.{} (\n{}\n);",
            pg_schema(&t.schema),
            qi(&t.name),
            lines.join(",\n")
        ),
    );
}

fn emit_column_diff_from_db(after: &DbTable, before: &DbTable, ops: &mut Vec<DiffOp>) {
    let before_cols: HashSet<&str> = before.columns.iter().map(|c| c.name.as_str()).collect();
    let after_cols: HashSet<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();

    for col in &after.columns {
        if !before_cols.contains(col.name.as_str()) {
            let not_null = if col.nullable { "" } else { " NOT NULL" };
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {}.{} ADD COLUMN IF NOT EXISTS {} {}{};",
                    pg_schema(&after.schema),
                    qi(&after.name),
                    qi(&col.name),
                    col.pg_type,
                    not_null
                ),
            );
        }
    }
    for col in &before.columns {
        if !after_cols.contains(col.name.as_str()) {
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {}.{} DROP COLUMN IF EXISTS {};",
                    pg_schema(&after.schema),
                    qi(&after.name),
                    qi(&col.name)
                ),
            );
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EnumDescriptor, LinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};

    fn empty_state() -> DbState {
        DbState::default()
    }

    fn prop(name: &str, pg_type: &str, nullable: bool) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(),
            pg_type: pg_type.into(),
            nullable,
            default_sql: if name == "id" { Some("uuidv7()".into()) } else { None },
            default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: name == "id",
            is_pk: name == "id",
            is_readonly: name == "id",
            rewrites: vec![],
            tuple_members: None,
            column_type: None,
        }
    }

    fn simple_type(module: &str, name: &str, table: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: name.into(),
            module: module.into(),
            table: table.into(),
            abstract_: false,
            materialized: false,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![prop("id", "uuid", false), prop("name", "text", true)],
            links: vec![],
            multilinks: vec![],
            computed: vec![],
            constraints: vec![],
            indexes: vec![],
            partition: None,
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![],
        }
    }

    // ── schema_content_changed ──────────────────────────────────────────────

    #[test]
    fn test_schema_content_changed_detects_a_readonly_only_flip() {
        // The exact previously-broken case: `is_readonly` has zero DDL
        // footprint, so `diff_schema_steps`/`diff_schema_ops` alone would
        // never notice this change at all.
        let before = simple_type("default", "Person", "Person");
        let mut after = before.clone();
        after.properties[1].is_readonly = true; // "name"
        assert_ne!(before.properties[1].is_readonly, after.properties[1].is_readonly);

        let schema_before = SchemaDescriptor {
            types: vec![before],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let schema_after = SchemaDescriptor {
            types: vec![after],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert!(schema_content_changed(&schema_after, Some(&schema_before)));
    }

    #[test]
    fn test_schema_content_changed_detects_a_new_rewrite() {
        let before = simple_type("default", "Person", "Person");
        let mut after = before.clone();
        after.properties[1].rewrites.push(crate::schema::RewriteEntry {
            on: 1,
            handler: "str_upper(.name)".into(),
        });

        let schema_before = SchemaDescriptor {
            types: vec![before],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let schema_after = SchemaDescriptor {
            types: vec![after],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert!(schema_content_changed(&schema_after, Some(&schema_before)));
    }

    #[test]
    fn test_schema_content_changed_is_false_for_identical_schemas() {
        let t = simple_type("default", "Person", "Person");
        let schema = SchemaDescriptor {
            types: vec![t],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let other = schema.clone();
        assert!(!schema_content_changed(&schema, Some(&other)));
    }

    #[test]
    fn test_schema_content_changed_true_against_none_when_target_is_non_empty() {
        let t = simple_type("default", "Person", "Person");
        let schema = SchemaDescriptor {
            types: vec![t],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert!(
            schema_content_changed(&schema, None),
            "no prior snapshot at all must count as changed"
        );
    }

    #[test]
    fn test_schema_content_changed_false_against_none_when_target_is_also_empty() {
        let schema = SchemaDescriptor::default();
        assert!(!schema_content_changed(&schema, None));
    }

    #[test]
    fn test_schema_content_changed_still_true_when_ddl_visible_things_also_changed() {
        // A DDL-visible change (a whole new type) must also register —
        // this function is meant to be OR'd with diff_schema_steps's own
        // result, not treated as mutually exclusive with it.
        let schema_before = SchemaDescriptor::default();
        let schema_after = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert!(schema_content_changed(&schema_after, Some(&schema_before)));
    }

    #[test]
    fn test_schema_content_changed_detects_a_new_channel() {
        // A `Channel` has zero physical DDL footprint — nothing about it
        // ever shows up in `diff_schema_steps`/`diff_schema_ops`, so its
        // mere addition is exactly the class of change this function
        // exists to catch, same as the readonly/rewrite cases above.
        let schema_before = SchemaDescriptor::default();
        let schema_after = SchemaDescriptor {
            channels: vec![crate::schema::ChannelDescriptor {
                name: "UserUpdates".into(),
                module: "default".into(),
                wire_name: "default__user_updates".into(),
                payload: crate::schema::ChannelPayload::Scalar("text".into()),
                description: None,
            }],
            ..Default::default()
        };
        assert!(schema_content_changed(&schema_after, Some(&schema_before)));
    }

    #[test]
    fn test_new_schema_and_table() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("catalog", "Product", "Product")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("CREATE SCHEMA IF NOT EXISTS \"catalog\""),
            "got:\n{joined}"
        );
        assert!(
            joined.contains("CREATE TABLE IF NOT EXISTS \"catalog\".\"Product\""),
            "got:\n{joined}"
        );
        assert!(
            joined.contains("CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON \"catalog\".\"Product\""),
            "new table must get the cache-invalidation trigger; got:\n{joined}"
        );
    }

    fn widget_with_trigger(on: u8, timing: &str, handler: &str) -> TypeDescriptor {
        let mut t = simple_type("default", "Widget", "Widget");
        t.triggers = vec![crate::schema::TriggerDescriptor {
            on,
            timing: timing.into(),
            handler: handler.into(),
        }];
        t
    }

    #[test]
    fn test_new_table_with_user_trigger_emits_the_compiled_trigger_ddl() {
        // Regression guard for the previously-missing incremental-migration
        // path: `export_schema` always emitted a schema's user-declared
        // `Trigger`s, but `diff_schema`/`diff_schema_steps` (the path
        // `migration create` actually uses) never did, so adding a
        // `Trigger(...)` to an existing type had zero effect on a real
        // migration.
        let schema = SchemaDescriptor {
            types: vec![widget_with_trigger(
                1,
                "After",
                "update Widget set { name := __new__.name }",
            )],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("NEW.\"name\""), "got:\n{joined}");
    }

    #[test]
    fn a_trigger_emitted_by_an_older_build_is_replaced() {
        // The delivery half of content-addressed trigger names: because a
        // trigger's name now hashes its body, a database whose trigger was
        // written by an older emitter carries a *different* name than the
        // current one, and that difference is what the diff can see. Before
        // this, the name was derived from the table and pointer alone, so a
        // fix to trigger codegen produced no diff at all and the old body
        // stayed until the table was dropped.
        //
        // Simulated by renaming the trigger in the baseline, which is
        // exactly what an emitter change looks like from the diff's side.
        let schema = SchemaDescriptor {
            types: vec![widget_with_trigger(
                1,
                "After",
                "update Widget set { name := __new__.name }",
            )],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let mut stale = schema_to_db_state(&schema);
        let current_name = stale
            .tables
            .iter()
            .flat_map(|t| t.triggers.iter().cloned())
            .find(|n| n.starts_with("Widget_"))
            .expect("the fixture should project a Widget trigger");
        let stale_name = "Widget_trg_0badc0de".to_string();
        for table in &mut stale.tables {
            for trigger in &mut table.triggers {
                if *trigger == current_name {
                    *trigger = stale_name.clone();
                }
            }
        }

        let joined = diff_schema(&schema, &stale).unwrap().join("\n");
        assert!(
            joined.contains(&format!("DROP TRIGGER IF EXISTS \"{stale_name}\"")),
            "the stale trigger should be dropped, got:\n{joined}"
        );
        assert!(
            joined.contains(&current_name),
            "the current trigger should be created, got:\n{joined}"
        );
        // The stale trigger's function goes with it — a rename would
        // otherwise leave a dead function behind on every codegen change.
        assert!(
            joined.contains(&format!("DROP FUNCTION IF EXISTS \"public\".\"{stale_name}\"()")),
            "the orphaned function should be dropped, got:\n{joined}"
        );
    }

    #[test]
    fn test_user_trigger_already_present_in_offline_baseline_produces_no_further_steps() {
        // Same "phantom step" bug class fixed earlier for cache-invalidate/
        // signal triggers: `schema_to_db_state`'s projection must recognize
        // a user Trigger as already present, or `migration create` would
        // propose recreating it forever, even with zero real schema changes.
        let schema = SchemaDescriptor {
            types: vec![widget_with_trigger(
                1,
                "After",
                "update Widget set { name := __new__.name }",
            )],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let baseline = schema_to_db_state(&schema);
        let steps = diff_schema_steps(&schema, &baseline, &HashMap::new()).unwrap();
        assert!(
            steps.is_empty(),
            "expected zero further migration steps, got: {steps:?}"
        );
    }

    #[test]
    fn test_new_table_with_plain_link_gets_its_fk_constraint() {
        // Regression guard: a brand-new table's `emit_create_table` only ever
        // emits a plain `uuid` column for a single link — the FK constraint
        // itself comes from Phase 6 (`emit_fk_diff`), which used to run only
        // for tables already present in `cur_tables`, silently skipping every
        // link on a table created in the same diff pass.
        let mut order = simple_type("default", "Order", "Order");
        order.links.push(LinkDescriptor {
            name: "customer".into(),
            target: "default::Person".into(),
            nullable: false,
            through: None,
            description: None,
            default_pyql: None,
            is_exclusive: false,
            is_readonly: false,
            rewrites: vec![],
            on_delete: vec![],
        });
        let schema = SchemaDescriptor {
            types: vec![order, simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("ADD CONSTRAINT \"Order_customer_fkey\" FOREIGN KEY (\"customer_id\") REFERENCES \"public\".\"Person\"(id)"),
            "new table's plain link must get its FK constraint in the same diff; got:\n{joined}"
        );
    }

    #[test]
    fn test_cache_invalidate_trigger_not_duplicated_for_junction_through_type() {
        use crate::schema::MultiLinkDescriptor;

        let mut product = simple_type("default", "Product", "Product");
        product.multilinks.push(MultiLinkDescriptor {
            name: "tags".into(),
            target: "default::Tag".into(),
            through: Some("default::ProductTag".into()),
            nullable: false,
            description: None,
            default_pyql: None,
            on_delete: vec![],
        });
        let mut junction = simple_type("default", "ProductTag", "Product.tags");
        junction.junction = true;

        let schema = SchemaDescriptor {
            types: vec![product, junction, simple_type("default", "Tag", "Tag")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let trigger_count = ops
            .iter()
            .filter(|op| op.contains("AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"Product.tags\""))
            .count();
        assert_eq!(
            trigger_count, 1,
            "junction table's own td.table and the owning type's multilink both resolve to \
             the same physical table — must be deduped to one trigger, got {trigger_count} in: {ops:?}"
        );
    }

    fn person_with_junction_backed_spouse() -> SchemaDescriptor {
        let mut person = simple_type("default", "Person", "Person");
        person.links.push(LinkDescriptor {
            name: "spouse".into(),
            target: "default::Person".into(),
            nullable: true,
            through: Some("default::Marriage".into()),
            description: None,
            default_pyql: None,
            is_exclusive: true,
            is_readonly: false,
            rewrites: vec![],
            on_delete: vec![],
        });
        let mut junction = simple_type("default", "Marriage", "Person.spouse");
        junction.junction = true;

        SchemaDescriptor {
            types: vec![person, junction],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        }
    }

    #[test]
    fn test_junction_backed_single_link_creates_junction_table_from_scratch() {
        let schema = person_with_junction_backed_spouse();
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(
            !joined.contains("spouse_id"),
            "no {{name}}_id column/FK for a junction-backed link, got:\n{joined}"
        );
        assert!(
            joined.contains("CREATE TABLE IF NOT EXISTS \"public\".\"Person.spouse\""),
            "got:\n{joined}"
        );
        assert!(joined.contains("PRIMARY KEY (source)"), "got:\n{joined}");
        assert!(joined.contains("UNIQUE (target)"), "got:\n{joined}");
    }

    #[test]
    fn test_junction_backed_single_link_diff_is_idempotent_once_applied() {
        // Regression guard for the drop-detection gap called out in the
        // junction-backed-single-link plan: without including link-through
        // junction tables in the create-loop/drop-detection/cache-trigger
        // sets, the table would get created then immediately flagged for
        // drop as "unknown" on the very next diff. This models the DbState
        // a live Postgres introspection would report right after the
        // create-from-scratch ops above were actually applied.
        let schema = person_with_junction_backed_spouse();
        let state = DbState {
            schemas: vec![],
            tables: vec![
                DbTable {
                    schema: "default".into(),
                    name: "Person".into(),
                    columns: vec![
                        DbColumn {
                            name: "id".into(),
                            pg_type: "uuid".into(),
                            nullable: false,
                            is_generated: false,
                            column_default: Some("uuidv7()".into()),
                        },
                        DbColumn {
                            name: "name".into(),
                            pg_type: "text".into(),
                            nullable: true,
                            is_generated: false,
                            column_default: None,
                        },
                    ],
                    foreign_keys: vec![],
                    indexes: vec![],
                    checks: vec![],
                    triggers: vec!["pylon_cache_invalidate".into()],
                },
                DbTable {
                    schema: "default".into(),
                    name: "Person.spouse".into(),
                    columns: vec![
                        DbColumn {
                            name: "source".into(),
                            pg_type: "uuid".into(),
                            nullable: false,
                            is_generated: false,
                            column_default: None,
                        },
                        DbColumn {
                            name: "target".into(),
                            pg_type: "uuid".into(),
                            nullable: false,
                            is_generated: false,
                            column_default: None,
                        },
                        DbColumn {
                            name: "name".into(),
                            pg_type: "text".into(),
                            nullable: true,
                            is_generated: false,
                            column_default: None,
                        },
                    ],
                    foreign_keys: vec![
                        DbForeignKey {
                            constraint_name: "Person_spouse_source_fkey".into(),
                            local_column: "source".into(),
                            ref_schema: "default".into(),
                            ref_table: "Person".into(),
                        },
                        DbForeignKey {
                            constraint_name: "Person_spouse_target_fkey".into(),
                            local_column: "target".into(),
                            ref_schema: "default".into(),
                            ref_table: "Person".into(),
                        },
                    ],
                    indexes: vec![],
                    checks: vec![],
                    triggers: vec!["pylon_cache_invalidate".into()],
                },
            ],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(
            ops.is_empty(),
            "already-migrated junction-backed single link must diff to no ops, got: {:?}",
            ops
        );
    }

    #[test]
    fn test_cache_invalidate_trigger_backfilled_on_pre_existing_table() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![], // pre-existing table, created before this feature shipped
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"Person\""),
            "pre-existing table missing the trigger must get it backfilled; got:\n{joined}"
        );
    }

    #[test]
    fn test_cache_invalidate_trigger_not_dropped_when_already_present() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec!["pylon_cache_invalidate".into()],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(
            ops.iter()
                .all(|op| !op.contains("DROP TRIGGER") && !op.contains("pylon_cache_invalidate")),
            "already-present trigger must not be re-created or dropped; got: {:?}",
            ops
        );
    }

    #[test]
    fn test_no_ops_when_in_sync() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec!["pylon_cache_invalidate".into()],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(ops.is_empty(), "expected no ops, got: {:?}", ops);
    }

    #[test]
    fn test_add_column() {
        let mut td = simple_type("default", "Person", "Person");
        td.properties.push(prop("email", "text", true));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("ADD COLUMN IF NOT EXISTS \"email\""), "got:\n{joined}");
    }

    #[test]
    fn test_property_type_change_emits_alter_column_type() {
        // A genuine type change (text -> int8) on an existing column must be
        // migrated, not silently left stale.
        let mut td = simple_type("default", "Person", "Person");
        td.properties.push(prop("rating", "int8", true));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                    DbColumn {
                        name: "rating".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains(
                "ALTER TABLE \"public\".\"Person\" ALTER COLUMN \"rating\" TYPE int8 USING \"rating\"::int8;"
            ),
            "got:\n{joined}"
        );
    }

    #[test]
    fn test_property_type_change_surfaces_a_required_cast_expression_step() {
        let mut td = simple_type("default", "Person", "Person");
        td.properties.push(prop("rating", "int8", true));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                    DbColumn {
                        name: "rating".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        let steps = diff_schema_steps(&schema, &state, &HashMap::new()).unwrap();
        let step = steps
            .iter()
            .find(|s| matches!(&s.op_key, OpKey::Table(m, t) if m == "default" && t == "Person"))
            .expect("expected an alter step for Person");

        assert_eq!(step.required_input.len(), 1, "got: {:?}", step.required_input);
        let input = &step.required_input[0];
        assert_eq!(input.placeholder, "cast_expr__rating");
        assert_eq!(input.default_expr, "\"rating\"::int8");
        assert_eq!(input.type_name, "default::Person");

        let placeholder_token = format!("\\({})", input.placeholder);
        assert!(
            step.ddl.iter().any(|op| op.sql.contains(&placeholder_token)),
            "expected the placeholder token in the step's DDL, got: {:?}",
            step.ddl.iter().map(|op| &op.sql).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_equivalent_base_type_spelling_is_not_a_diff() {
        // format_type() reports Postgres's own canonical alias spelling
        // (int8 -> bigint, timestamptz -> timestamp with time zone, ...) —
        // comparing against that verbatim would treat every existing
        // column as "changed" on every diff. Confirms the alias table
        // avoids that false positive.
        let mut td = simple_type("default", "Person", "Person");
        td.properties.push(prop("age", "int8", true));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                    DbColumn {
                        name: "age".into(),
                        pg_type: "bigint".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec!["pylon_cache_invalidate".into()],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(
            ops.iter().all(|op| !op.contains("ALTER COLUMN")),
            "expected no ALTER COLUMN ops, got: {:?}",
            ops
        );
    }

    #[test]
    fn test_registered_scalar_domain_adoption_drops_and_recreates_dependent_interface_view() {
        // A property switching to a registered custom scalar's own DOMAIN
        // (PropertyDescriptor.column_type) on a table an interface view
        // selects from must drop that view before the ALTER (Postgres
        // refuses to retype a column a view depends on) and recreate it
        // afterward — not silently fail, and not leave the view missing.
        use crate::schema::ScalarDescriptor;

        let mut account = simple_type("default", "Account", "Account");
        account.abstract_ = true;
        account.materialized = true;
        account.properties = vec![prop("id", "uuid", false), prop("email", "text", false)];
        account.properties[1].column_type = Some("\"public\".\"Email\"".into());

        let mut individual = simple_type("default", "Individual", "Individual");
        individual.interfaces = vec!["default::Account".into()];
        individual.properties = vec![prop("id", "uuid", false), prop("email", "text", false)];
        individual.properties[1].column_type = Some("\"public\".\"Email\"".into());

        let schema = SchemaDescriptor {
            types: vec![account, individual],
            scalars: vec![ScalarDescriptor {
                name: "Email".into(),
                module: "default".into(),
                base: "Str".into(),
                pg_type: "text".into(),
                check_constraints: vec!["value ~ '@'".into()],
                is_sequence: false,
            }],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };

        // The view's own SELECT text never mentions column types, so its
        // hash is unaffected by this migration — matching that hash in
        // `current` confirms Phase 10 doesn't ALSO try to (redundantly,
        // and invalidly, since it'd already exist) recreate it.
        let view_ddl = crate::export::interface_view_ddl_with_names(&schema)
            .into_iter()
            .find(|(_, n, _)| n == "Account")
            .unwrap()
            .2;

        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Individual".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "email".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec!["pylon_cache_invalidate".into()],
            }],
            views: vec![DbView {
                schema: "default".into(),
                name: "Account".into(),
                body_hash: ddl_hash(&view_ddl),
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        // Migration mode (`diff_schema_ops`, what `pylon migration create`
        // uses) — unlike watch mode, this respects the interface view's
        // unchanged DDL hash and doesn't redundantly re-emit it.
        let ops = diff_schema_ops(&schema, &state).unwrap();
        let joined = ops.iter().map(|op| op.sql.as_str()).collect::<Vec<_>>().join("\n");
        let drop_pos = joined
            .find("DROP VIEW IF EXISTS \"public\".\"Account\"")
            .unwrap_or_else(|| panic!("missing DROP VIEW; got:\n{joined}"));
        let alter_pos = joined
            .find("ALTER TABLE \"public\".\"Individual\" ALTER COLUMN \"email\" TYPE \"public\".\"Email\"")
            .unwrap_or_else(|| panic!("missing ALTER COLUMN TYPE; got:\n{joined}"));
        let create_pos = joined
            .rfind("CREATE VIEW \"public\".\"Account\"")
            .unwrap_or_else(|| panic!("missing CREATE VIEW; got:\n{joined}"));
        assert!(drop_pos < alter_pos, "DROP VIEW must precede the ALTER; got:\n{joined}");
        assert!(
            alter_pos < create_pos,
            "CREATE VIEW must follow the ALTER; got:\n{joined}"
        );
        assert_eq!(
            joined.matches("CREATE VIEW \"public\".\"Account\"").count(),
            1,
            "view must be recreated exactly once, not duplicated by Phase 10; got:\n{joined}"
        );
    }

    fn exclusive_email_account_schema(implementor_names: &[&str]) -> SchemaDescriptor {
        let mut account = simple_type("default", "Account", "Account");
        account.abstract_ = true;
        account.materialized = true;
        account.properties = vec![prop("id", "uuid", false), prop("email", "text", false)];
        account.properties[1].is_exclusive = true;

        let mut types = vec![account];
        for name in implementor_names {
            let mut t = simple_type("default", name, name);
            t.interfaces = vec!["default::Account".into()];
            t.properties = vec![prop("id", "uuid", false), prop("email", "text", false)];
            t.properties[1].is_exclusive = true;
            types.push(t);
        }
        SchemaDescriptor {
            types,
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        }
    }

    #[test]
    fn test_new_implementor_added_to_existing_interface_gets_exclusive_triggers_retroactively() {
        // `Individual` already exists (with its own triggers already
        // applied by a prior migration); `Organization` is a brand new
        // implementor of the same interface — it must get the cross-table
        // exclusive trigger from the moment its table is created, and
        // `Individual`'s already-present triggers must not be re-emitted.
        let schema = exclusive_email_account_schema(&["Individual", "Organization"]);
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Individual".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "email".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![
                    "pylon_cache_invalidate".into(),
                    "_excl_Account_email_ins".into(),
                    "_excl_Account_email_upd".into(),
                ],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        let ops = diff_schema_ops(&schema, &state).unwrap();
        let joined = ops.iter().map(|op| op.sql.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains(
                "CREATE CONSTRAINT TRIGGER \"_excl_Account_email_ins\"\nAFTER INSERT ON \"public\".\"Organization\""
            ),
            "the new implementor must get the exclusive trigger; got:\n{joined}"
        );
        assert!(
            !joined.contains("ON \"public\".\"Individual\""),
            "the already-migrated implementor's existing triggers must not be re-emitted; got:\n{joined}"
        );
    }

    #[test]
    fn test_removing_exclusivity_drops_the_cross_table_triggers() {
        // The target schema no longer marks `email` exclusive at all (the
        // user removed `Exclusive` from the interface) — both the INSERT
        // and UPDATE constraint triggers on every implementor must be
        // dropped, matching any other trigger's removal.
        let mut schema = exclusive_email_account_schema(&["Individual"]);
        for t in &mut schema.types {
            for p in &mut t.properties {
                if p.name == "email" {
                    p.is_exclusive = false;
                }
            }
        }
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Individual".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "email".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![
                    "pylon_cache_invalidate".into(),
                    "_excl_Account_email_ins".into(),
                    "_excl_Account_email_upd".into(),
                ],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        let ops = diff_schema_ops(&schema, &state).unwrap();
        let joined = ops.iter().map(|op| op.sql.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("DROP TRIGGER IF EXISTS \"_excl_Account_email_ins\" ON \"public\".\"Individual\""),
            "got:\n{joined}"
        );
        assert!(
            joined.contains("DROP TRIGGER IF EXISTS \"_excl_Account_email_upd\" ON \"public\".\"Individual\""),
            "got:\n{joined}"
        );
        assert!(
            !joined.contains("DROP TRIGGER IF EXISTS \"pylon_cache_invalidate\""),
            "unrelated triggers must not be touched; got:\n{joined}"
        );
    }

    #[test]
    fn test_new_enum() {
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![],
            enums: vec![EnumDescriptor {
                name: "Status".into(),
                module: "default".into(),
                members: vec!["Active".into(), "Inactive".into()],
            }],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("CREATE TYPE \"public\".\"Status\" AS ENUM"),
            "got:\n{joined}"
        );
    }

    #[test]
    fn test_drop_table() {
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "OldType".into(),
                columns: vec![],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("DROP TABLE IF EXISTS \"public\".\"OldType\" CASCADE"),
            "got:\n{joined}"
        );
    }

    #[test]
    fn test_index_on_existing_table_is_concurrently() {
        use crate::schema::VectorIndexDescriptor;
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            pointers: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        // The table already exists in the DB (pre-existing).
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Post".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema_ops(&schema, &state).unwrap();
        let idx_op = ops.iter().find(|op| op.sql.contains("hnsw")).unwrap();
        assert!(
            idx_op.non_transactional,
            "index on pre-existing table should be non-transactional"
        );
        assert!(
            idx_op.sql.contains("CONCURRENTLY"),
            "should use CONCURRENTLY: {}",
            idx_op.sql
        );
    }

    #[test]
    fn test_required_extensions_empty_without_vector_indexes() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Post", "Post")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert!(required_extensions(&schema).is_empty());
    }

    #[test]
    fn test_missing_extension_ddl_when_vector_index_present_and_not_yet_installed() {
        use crate::schema::VectorIndexDescriptor;
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            pointers: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        assert_eq!(required_extensions(&schema), vec!["vector"]);

        let ddl = missing_extension_ddl(&schema, &DbState::default());
        assert_eq!(ddl, vec!["CREATE EXTENSION IF NOT EXISTS \"vector\";".to_string()]);

        let already_installed = DbState {
            extensions: vec!["vector".into()],
            ..DbState::default()
        };
        assert!(missing_extension_ddl(&schema, &already_installed).is_empty());
    }

    #[test]
    fn test_index_on_new_table_is_transactional() {
        use crate::schema::VectorIndexDescriptor;
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            pointers: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        // Table does NOT exist in the DB → it's new.
        let ops = diff_schema_ops(&schema, &empty_state()).unwrap();
        let idx_op = ops.iter().find(|op| op.sql.contains("hnsw")).unwrap();
        assert!(!idx_op.non_transactional, "index on new table should be transactional");
        assert!(
            !idx_op.sql.contains("CONCURRENTLY"),
            "should NOT use CONCURRENTLY: {}",
            idx_op.sql
        );
    }

    fn sequence_scalar(module: &str, name: &str) -> crate::schema::ScalarDescriptor {
        crate::schema::ScalarDescriptor {
            name: name.into(),
            module: module.into(),
            base: "Sequence".into(),
            pg_type: "int8".into(),
            check_constraints: vec![],
            is_sequence: true,
        }
    }

    #[test]
    fn test_new_sequence_creates_sequence_and_domain() {
        let schema = SchemaDescriptor {
            types: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
            scalars: vec![sequence_scalar("default", "OrderNumber")],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("CREATE SEQUENCE IF NOT EXISTS \"public\".\"OrderNumber_seq\""),
            "got:\n{joined}"
        );
        assert!(
            joined.contains("CREATE DOMAIN \"public\".\"OrderNumber\" AS int8"),
            "got:\n{joined}"
        );
        // Sequence must precede domain
        let seq_pos = joined.find("CREATE SEQUENCE").unwrap();
        let dom_pos = joined.find("CREATE DOMAIN").unwrap();
        assert!(seq_pos < dom_pos, "sequence must be created before domain");
    }

    #[test]
    fn test_no_ops_sequence_already_exists() {
        let schema = SchemaDescriptor {
            types: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
            scalars: vec![sequence_scalar("default", "OrderNumber")],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            domains: vec![DbDomain {
                schema: "default".into(),
                name: "OrderNumber".into(),
            }],
            sequences: vec![DbSequence {
                schema: "default".into(),
                name: "OrderNumber_seq".into(),
            }],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(
            ops.is_empty(),
            "expected no ops when sequence and domain exist, got: {:?}",
            ops
        );
    }

    #[test]
    fn test_drop_removed_sequence() {
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            domains: vec![DbDomain {
                schema: "default".into(),
                name: "OrderNumber".into(),
            }],
            sequences: vec![DbSequence {
                schema: "default".into(),
                name: "OrderNumber_seq".into(),
            }],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(
            joined.contains("DROP DOMAIN IF EXISTS \"public\".\"OrderNumber\""),
            "got:\n{joined}"
        );
        assert!(
            joined.contains("DROP SEQUENCE IF EXISTS \"public\".\"OrderNumber_seq\""),
            "got:\n{joined}"
        );
    }

    // ── MigrationStep grouping ───────────────────────────────────────────────

    #[test]
    fn test_diff_schema_steps_groups_multiple_column_changes_into_one_alter_step() {
        let mut person = simple_type("default", "Person", "Person");
        person.properties.push(prop("nickname", "text", true));
        person.properties.push(prop("age", "int8", true));
        let schema = SchemaDescriptor {
            types: vec![person],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec!["pylon_cache_invalidate".into()],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        let steps = diff_schema_steps(&schema, &state, &HashMap::new()).unwrap();
        let table_steps: Vec<&MigrationStep> = steps
            .iter()
            .filter(|s| matches!(&s.op_key, OpKey::Table(m, t) if m == "default" && t == "Person"))
            .collect();
        assert_eq!(
            table_steps.len(),
            1,
            "two new columns on the same table must produce one step, got: {:?}",
            steps.iter().map(|s| &s.prompt).collect::<Vec<_>>()
        );
        assert_eq!(table_steps[0].verb, Verb::Alter);
        assert_eq!(table_steps[0].prompt, "did you alter object type 'default::Person'?");
        assert_eq!(
            table_steps[0].ddl.len(),
            2,
            "expected one ADD COLUMN per new property, got: {:?}",
            table_steps[0].ddl.iter().map(|d| &d.sql).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_diff_schema_steps_new_table_is_one_create_step_including_its_trigger() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("catalog", "Product", "Product")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let steps = diff_schema_steps(&schema, &empty_state(), &HashMap::new()).unwrap();
        let table_steps: Vec<&MigrationStep> = steps
            .iter()
            .filter(|s| matches!(&s.op_key, OpKey::Table(m, t) if m == "catalog" && t == "Product"))
            .collect();
        assert_eq!(
            table_steps.len(),
            1,
            "got steps: {:?}",
            steps.iter().map(|s| &s.prompt).collect::<Vec<_>>()
        );
        assert_eq!(table_steps[0].verb, Verb::Create);
        assert_eq!(table_steps[0].prompt, "did you create object type 'catalog::Product'?");

        // The cache-invalidation trigger for a brand new table must fold
        // into this same create step, not a separate one (Phase 11.5).
        let joined: String = table_steps[0]
            .ddl
            .iter()
            .map(|d| d.sql.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("CREATE TABLE IF NOT EXISTS \"catalog\".\"Product\""),
            "got:\n{joined}"
        );
        assert!(joined.contains("pylon_cache_invalidate"), "got:\n{joined}");
    }

    #[test]
    fn test_guidance_bans_a_rejected_type_rename_candidate() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Customer", "Customer")],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        is_generated: false,
                        column_default: Some("uuidv7()".into()),
                    },
                    DbColumn {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        is_generated: false,
                        column_default: None,
                    },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            }],
            enums: vec![],
            domains: vec![],
            ..DbState::default()
        };

        let candidates = detect_type_renames(&schema, &state, &Guidance::default());
        assert_eq!(
            candidates.len(),
            1,
            "expected Person -> Customer to be proposed as a rename"
        );

        let mut guidance = Guidance::default();
        guidance.banned_type_renames.insert((
            "default".to_string(),
            "Person".to_string(),
            "default".to_string(),
            "Customer".to_string(),
        ));
        let candidates = detect_type_renames(&schema, &state, &guidance);
        assert!(
            candidates.is_empty(),
            "a banned rename candidate must not be re-proposed"
        );
    }
}
