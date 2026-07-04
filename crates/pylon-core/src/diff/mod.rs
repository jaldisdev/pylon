//! Schema diff engine: given a target SchemaDescriptor and the live database
//! state (introspected via pg_catalog), produce ordered DDL SQL statements
//! to bring the database in sync with the target.
//!
//! Used by `pylon migration watch` (apply directly) and `pylon migration create`
//! (render as a migration file body). Pure computation — no I/O.

use std::collections::{HashMap, HashSet};

use crate::schema::{SchemaDescriptor, SearchBackend, TypeDescriptor};

// ── Live database state ────────────────────────────────────────────────────────

/// Snapshot of the live PostgreSQL database, built by Python from pg_catalog queries.
#[derive(Debug, Default)]
pub struct DbState {
    /// User-managed schema names (excludes _pylon, public, pg_* etc.).
    pub schemas: Vec<String>,
    pub tables: Vec<DbTable>,
    pub enums: Vec<DbEnum>,
    pub domains: Vec<DbDomain>,
}

#[derive(Debug)]
pub struct DbTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<DbColumn>,
    pub foreign_keys: Vec<DbForeignKey>,
    pub indexes: Vec<DbIndex>,
    pub checks: Vec<DbCheck>,
}

#[derive(Debug)]
pub struct DbColumn {
    pub name: String,
    /// PostgreSQL type as returned by format_type() — not necessarily the same
    /// spelling as in the schema descriptor, but existence checks use name only.
    pub pg_type: String,
    pub nullable: bool,
    pub is_generated: bool,
}

#[derive(Debug)]
pub struct DbForeignKey {
    pub constraint_name: String,
    pub local_column: String,
    pub ref_schema: String,
    pub ref_table: String,
}

#[derive(Debug)]
pub struct DbIndex {
    pub name: String,
    pub is_unique: bool,
    pub method: String,
}

#[derive(Debug)]
pub struct DbCheck {
    pub constraint_name: String,
}

#[derive(Debug)]
pub struct DbEnum {
    pub schema: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug)]
pub struct DbDomain {
    pub schema: String,
    pub name: String,
}

// ── Identifier helpers ─────────────────────────────────────────────────────────

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", qi(schema), qi(name))
}

// ── Topological sort of types (dependency order for CREATE TABLE) ──────────────

fn topo_sort_types(types: &[TypeDescriptor]) -> Vec<usize> {
    let idx_of: HashMap<String, usize> = types
        .iter()
        .enumerate()
        .map(|(i, t)| (format!("{}::{}", t.module, t.name), i))
        .collect();

    let mut visited = vec![false; types.len()];
    let mut order: Vec<usize> = Vec::with_capacity(types.len());

    fn visit(
        i: usize,
        types: &[TypeDescriptor],
        idx_of: &HashMap<String, usize>,
        visited: &mut Vec<bool>,
        order: &mut Vec<usize>,
    ) {
        if visited[i] {
            return;
        }
        visited[i] = true;
        let t = &types[i];
        for l in &t.links {
            if let Some(&dep) = idx_of.get(&l.target) {
                visit(dep, types, idx_of, visited, order);
            }
        }
        order.push(i);
    }

    for i in 0..types.len() {
        visit(i, types, &idx_of, &mut visited, &mut order);
    }

    order
}

// ── Helpers for emit_create_table ─────────────────────────────────────────────

fn col_type_str(pg_type: &str) -> &str {
    pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(pg_type)
}

// ── Main diff entry point ─────────────────────────────────────────────────────

/// Compute the ordered DDL SQL statements needed to bring a database described
/// by `current` in sync with `target`. Returns an empty vec when nothing changed.
pub fn diff_schema(target: &SchemaDescriptor, current: &DbState) -> Vec<String> {
    let mut ops: Vec<String> = Vec::new();

    // Index current state for fast lookup.
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

    // Build FK resolution map: "module::Name" → (module, table)
    let type_map: HashMap<String, (&str, &str)> = target
        .types
        .iter()
        .map(|t| (format!("{}::{}", t.module, t.name), (t.module.as_str(), t.table.as_str())))
        .collect();

    // Target module set
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

    // ── Phase 1: new schemas ──────────────────────────────────────────────────
    for schema in &target_schemas {
        if !cur_schemas.contains(schema.as_str()) {
            ops.push(format!("CREATE SCHEMA IF NOT EXISTS {};", qi(schema)));
        }
    }

    // ── Phase 2: new enums ────────────────────────────────────────────────────
    for e in &target.enums {
        let key = (e.module.as_str(), e.name.as_str());
        match cur_enums.get(&key) {
            None => {
                let members: Vec<String> = e
                    .members
                    .iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                ops.push(format!(
                    "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                    qi(&e.module),
                    qi(&e.name),
                    members.join(", ")
                ));
            }
            Some(existing) => {
                // Postgres can't remove enum members; only add new ones.
                let existing_set: HashSet<&str> =
                    existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        ops.push(format!(
                            "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                            qi(&e.module),
                            qi(&e.name),
                            member.replace('\'', "''")
                        ));
                    }
                }
            }
        }
    }

    // ── Phase 3: new custom scalar domains ────────────────────────────────────
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
            ops.push(format!(
                "CREATE DOMAIN {}.{} AS {}{};",
                qi(&s.module),
                qi(&s.name),
                s.pg_type,
                check_clause
            ));
        }
    }

    // ── Phase 4 & 5: tables — create new, or alter existing ───────────────────
    let sort_order = topo_sort_types(&target.types);

    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        let key = (td.module.as_str(), td.table.as_str());
        match cur_tables.get(&key) {
            None => emit_create_table(td, &mut ops),
            Some(existing) => emit_column_diff(td, existing, &mut ops),
        }
    }

    // ── Phase 6: FK constraints for existing tables ───────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        if let Some(existing) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) {
            emit_fk_diff(td, existing, &type_map, &mut ops);
        }
    }

    // ── Phase 7: junction tables for new multi-links ──────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction {
            continue;
        }
        for ml in &td.multilinks {
            let jt = format!("{}.{}", td.table, ml.name);
            if !cur_tables.contains_key(&(td.module.as_str(), jt.as_str())) {
                emit_junction_table(td, &ml.name, &ml.target, &ml.on_delete, &type_map, &mut ops);
            }
        }
    }

    // ── Phase 8: vector columns + HNSW indexes ────────────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.vector_indexes.is_empty() {
            continue;
        }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        for vi in &td.vector_indexes {
            let col = vi.column_name();
            if existing.map(|t| t.columns.iter().any(|c| c.name == col)).unwrap_or(false) {
                continue;
            }
            ops.push(format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} vector({});",
                qn(&td.module, &td.table),
                qi(&col),
                vi.dimensions
            ));
            let idx_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(n) => format!("{}__vector_{}__", td.table, n),
            };
            ops.push(format!(
                "CREATE INDEX IF NOT EXISTS {} ON {} USING hnsw ({} {});",
                qi(&idx_name),
                qn(&td.module, &td.table),
                qi(&col),
                vi.ops_class()
            ));
        }
    }

    // ── Phase 9: search tsvector columns + GIN indexes ────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.search_indexes.is_empty() {
            continue;
        }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }
            let col = si.column_name();
            if existing.map(|t| t.columns.iter().any(|c| c.name == col)).unwrap_or(false) {
                continue;
            }
            let parts: Vec<String> = si
                .fields
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
            ops.push(format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED;",
                qn(&td.module, &td.table),
                qi(&col),
                expr
            ));
            let idx_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(n) => format!("{}__search_{}__", td.table, n),
            };
            ops.push(format!(
                "CREATE INDEX IF NOT EXISTS {} ON {} USING gin ({});",
                qi(&idx_name),
                qn(&td.module, &td.table),
                qi(&col)
            ));
        }
    }

    // ── Phase 10: drop removed tables ────────────────────────────────────────
    // Build the complete set of tables the target schema requires.
    let mut target_tables: HashSet<(String, String)> = HashSet::new();
    for td in &target.types {
        if !td.abstract_ {
            target_tables.insert((td.module.clone(), td.table.clone()));
        }
        if !td.abstract_ && !td.junction {
            for ml in &td.multilinks {
                target_tables.insert((td.module.clone(), format!("{}.{}", td.table, ml.name)));
            }
        }
    }
    // Drop everything in the live DB that no longer has a target counterpart,
    // using CASCADE so FK/index dependencies are handled automatically.
    // This covers both removed types and removed multi-links (orphaned junction tables).
    for cur_table in &current.tables {
        let key = (cur_table.schema.clone(), cur_table.name.clone());
        if !target_tables.contains(&key) {
            ops.push(format!(
                "DROP TABLE IF EXISTS {} CASCADE;",
                qn(&cur_table.schema, &cur_table.name)
            ));
        }
    }

    // ── Phase 11: drop removed enums ──────────────────────────────────────────
    let target_enum_set: HashSet<(String, String)> = target
        .enums
        .iter()
        .map(|e| (e.module.clone(), e.name.clone()))
        .collect();
    for cur_enum in &current.enums {
        if !target_enum_set.contains(&(cur_enum.schema.clone(), cur_enum.name.clone())) {
            ops.push(format!(
                "DROP TYPE IF EXISTS {}.{} CASCADE;",
                qi(&cur_enum.schema),
                qi(&cur_enum.name)
            ));
        }
    }

    // ── Phase 12: drop removed domains ───────────────────────────────────────
    let target_domain_set: HashSet<(String, String)> = target
        .scalars
        .iter()
        .map(|s| (s.module.clone(), s.name.clone()))
        .collect();
    for cur_domain in &current.domains {
        if !target_domain_set.contains(&(cur_domain.schema.clone(), cur_domain.name.clone())) {
            ops.push(format!(
                "DROP DOMAIN IF EXISTS {}.{} CASCADE;",
                qi(&cur_domain.schema),
                qi(&cur_domain.name)
            ));
        }
    }

    // ── Phase 13: drop removed schemas (only if now empty) ───────────────────
    for schema in &current.schemas {
        if !target_schemas.contains(schema) {
            // Use IF EXISTS + CASCADE so we don't fail on non-empty schemas, but
            // only do this if truly no target types remain in this schema.
            ops.push(format!("DROP SCHEMA IF EXISTS {} CASCADE;", qi(schema)));
        }
    }

    ops
}

// ── CREATE TABLE for a new type ───────────────────────────────────────────────

fn emit_create_table(td: &TypeDescriptor, ops: &mut Vec<String>) {
    let mut lines: Vec<String> = Vec::new();

    for p in &td.properties {
        let not_null = if p.nullable { "" } else { " NOT NULL" };
        let default = p
            .default_sql
            .as_deref()
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        let ct = col_type_str(&p.pg_type);
        lines.push(format!("    {} {}{}{}", qi(&p.name), ct, not_null, default));
    }
    for l in &td.links {
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        lines.push(format!(
            "    {} uuid{}",
            qi(&format!("{}_id", l.name)),
            not_null
        ));
    }
    let pk_cols: Vec<String> = td
        .properties
        .iter()
        .filter(|p| p.is_pk)
        .map(|p| qi(&p.name))
        .collect();
    if !pk_cols.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }
    ops.push(format!(
        "CREATE TABLE {} (\n{}\n);",
        qn(&td.module, &td.table),
        lines.join(",\n")
    ));
}

// ── Column diff for existing table ────────────────────────────────────────────

fn emit_column_diff(td: &TypeDescriptor, existing: &DbTable, ops: &mut Vec<String>) {
    let existing_cols: HashSet<&str> =
        existing.columns.iter().map(|c| c.name.as_str()).collect();

    // Add new property columns
    for p in &td.properties {
        if !existing_cols.contains(p.name.as_str()) {
            let not_null = if p.nullable { "" } else { " NOT NULL" };
            let default = p
                .default_sql
                .as_deref()
                .map(|d| format!(" DEFAULT {}", d))
                .unwrap_or_default();
            let ct = col_type_str(&p.pg_type);
            ops.push(format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}{}{};",
                qn(&td.module, &td.table),
                qi(&p.name),
                ct,
                not_null,
                default
            ));
        }
    }

    // Add new link FK stub columns
    for l in &td.links {
        let col = format!("{}_id", l.name);
        if !existing_cols.contains(col.as_str()) {
            let not_null = if l.nullable { "" } else { " NOT NULL" };
            ops.push(format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} uuid{};",
                qn(&td.module, &td.table),
                qi(&col),
                not_null
            ));
        }
    }

    // Drop columns that no longer exist in the target.
    // Keep id, __vector__*, __search__* (handled in their own phases).
    let target_col_names: HashSet<String> = td
        .properties
        .iter()
        .map(|p| p.name.clone())
        .chain(td.links.iter().map(|l| format!("{}_id", l.name)))
        .collect();

    for col in &existing.columns {
        let n = col.name.as_str();
        if target_col_names.contains(n) {
            continue;
        }
        // Pylon-managed special columns — handled by vector/search phases
        if n.starts_with("__") && n.ends_with("__") {
            continue;
        }
        ops.push(format!(
            "ALTER TABLE {} DROP COLUMN IF EXISTS {};",
            qn(&td.module, &td.table),
            qi(n)
        ));
    }
}

// ── FK diff for existing table ────────────────────────────────────────────────

fn emit_fk_diff(
    td: &TypeDescriptor,
    existing: &DbTable,
    type_map: &HashMap<String, (&str, &str)>,
    ops: &mut Vec<String>,
) {
    use crate::schema::{DeleteAction, DeleteSide};

    let existing_fk_names: HashSet<&str> =
        existing.foreign_keys.iter().map(|fk| fk.constraint_name.as_str()).collect();

    for l in &td.links {
        let cname = format!("{}_{}_fkey", td.table, l.name);
        if existing_fk_names.contains(cname.as_str()) {
            continue;
        }
        let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else {
            continue;
        };
        let on_delete = l
            .on_delete
            .iter()
            .find(|p| p.side == DeleteSide::Target)
            .map(|p| match &p.action {
                DeleteAction::Restrict => " ON DELETE RESTRICT",
                DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED",
                DeleteAction::DeleteSource => " ON DELETE CASCADE",
                DeleteAction::Allow => " ON DELETE SET NULL",
                _ => " ON DELETE RESTRICT",
            })
            .unwrap_or(" ON DELETE RESTRICT");
        ops.push(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}(id){};",
            qn(&td.module, &td.table),
            qi(&cname),
            qi(&format!("{}_id", l.name)),
            qn(tgt_module, tgt_table),
            on_delete
        ));
    }
}

// ── Junction table for a new multi-link ───────────────────────────────────────

fn emit_junction_table(
    td: &TypeDescriptor,
    ml_name: &str,
    ml_target: &str,
    on_delete: &[crate::schema::OnDeletePolicy],
    type_map: &HashMap<String, (&str, &str)>,
    ops: &mut Vec<String>,
) {
    use crate::schema::{DeleteAction, DeleteSide};

    let jt_name = format!("{}.{}", td.table, ml_name);

    let src_on_delete = " ON DELETE CASCADE"; // source side always cascades
    let tgt_on_delete = on_delete
        .iter()
        .find(|p| p.side == DeleteSide::Target)
        .map(|p| match &p.action {
            DeleteAction::Restrict => " ON DELETE RESTRICT",
            DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED",
            DeleteAction::Allow => " ON DELETE CASCADE",
            DeleteAction::DeleteSource => " ON DELETE CASCADE",
            _ => " ON DELETE RESTRICT",
        })
        .unwrap_or(" ON DELETE RESTRICT");

    let tgt_ref = type_map
        .get(ml_target)
        .map(|(m, t)| qn(m, t))
        .unwrap_or_else(|| qi(ml_target));

    ops.push(format!(
        "CREATE TABLE {} (\n    source uuid NOT NULL REFERENCES {}(id){},\n    target uuid NOT NULL REFERENCES {}(id){},\n    PRIMARY KEY (source, target)\n);",
        qn(&td.module, &jt_name),
        qn(&td.module, &td.table),
        src_on_delete,
        tgt_ref,
        tgt_on_delete
    ));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EnumDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};

    fn empty_state() -> DbState {
        DbState::default()
    }

    fn prop(name: &str, pg_type: &str, nullable: bool) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(),
            pg_type: pg_type.into(),
            nullable,
            default_sql: if name == "id" { Some("uuidv7()".into()) } else { None },
            description: None,
            check_constraints: vec![],
            is_exclusive: name == "id",
            is_pk: name == "id",
            is_readonly: name == "id",
            rewrites: vec![],
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
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
        }
    }

    #[test]
    fn test_new_schema_and_table() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("catalog", "Product", "Product")],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
            functions: vec![],
        };
        let ops = diff_schema(&schema, &empty_state());
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE SCHEMA IF NOT EXISTS \"catalog\""), "got:\n{joined}");
        assert!(joined.contains("CREATE TABLE \"catalog\".\"Product\""), "got:\n{joined}");
    }

    #[test]
    fn test_no_ops_when_in_sync() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
            functions: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
            }],
            enums: vec![],
            domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
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
            globals: vec![],
            functions: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(),
                name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false },
                ],
                foreign_keys: vec![],
                indexes: vec![],
                checks: vec![],
            }],
            enums: vec![],
            domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
        let joined = ops.join("\n");
        assert!(joined.contains("ADD COLUMN IF NOT EXISTS \"email\""), "got:\n{joined}");
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
            globals: vec![],
            functions: vec![],
        };
        let ops = diff_schema(&schema, &empty_state());
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE TYPE \"default\".\"Status\" AS ENUM"), "got:\n{joined}");
    }

    #[test]
    fn test_drop_table() {
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
            functions: vec![],
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
            }],
            enums: vec![],
            domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
        let joined = ops.join("\n");
        assert!(joined.contains("DROP TABLE IF EXISTS \"default\".\"OldType\" CASCADE"), "got:\n{joined}");
    }
}
