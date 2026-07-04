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
#[derive(Debug, Default, Clone)]
pub struct DbState {
    /// User-managed schema names (excludes _pylon, public, pg_* etc.).
    pub schemas: Vec<String>,
    pub tables: Vec<DbTable>,
    pub enums: Vec<DbEnum>,
    pub domains: Vec<DbDomain>,
}

#[derive(Debug, Clone)]
pub struct DbTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<DbColumn>,
    pub foreign_keys: Vec<DbForeignKey>,
    pub indexes: Vec<DbIndex>,
    pub checks: Vec<DbCheck>,
}

#[derive(Debug, Clone)]
pub struct DbColumn {
    pub name: String,
    pub pg_type: String,
    pub nullable: bool,
    pub is_generated: bool,
}

#[derive(Debug, Clone)]
pub struct DbForeignKey {
    pub constraint_name: String,
    pub local_column: String,
    pub ref_schema: String,
    pub ref_table: String,
}

#[derive(Debug, Clone)]
pub struct DbIndex {
    pub name: String,
    pub is_unique: bool,
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct DbCheck {
    pub constraint_name: String,
}

#[derive(Debug, Clone)]
pub struct DbEnum {
    pub schema: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DbDomain {
    pub schema: String,
    pub name: String,
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

// ── Diff operation ─────────────────────────────────────────────────────────────

/// A single DDL operation produced by the diff engine.
#[derive(Debug)]
pub struct DiffOp {
    pub sql: String,
    /// When true the statement must run outside any transaction wrapper —
    /// i.e. it uses `CONCURRENTLY`. `pylon migration create` will insert a
    /// `-- pylon:step non-transactional` marker before these ops.
    pub non_transactional: bool,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute ordered DDL SQL strings to bring a database in sync with `target`.
/// All statements use plain (non-CONCURRENTLY) index creation — suitable for
/// `watch` mode where everything runs inside a single transaction.
pub fn diff_schema(target: &SchemaDescriptor, current: &DbState) -> Vec<String> {
    diff_inner(target, current, false)
        .into_iter()
        .map(|op| op.sql)
        .collect()
}

/// Compute ordered `DiffOp`s suitable for a migration file body.
/// Index creation on pre-existing tables uses `CONCURRENTLY` and is marked
/// `non_transactional = true` so `create` can insert step-boundary markers.
pub fn diff_schema_ops(target: &SchemaDescriptor, current: &DbState) -> Vec<DiffOp> {
    diff_inner(target, current, true)
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
pub fn detect_type_renames(target: &SchemaDescriptor, current: &DbState) -> Vec<TypeRenameCandidate> {
    let target_keys: HashSet<(&str, &str)> = target.types.iter()
        .filter(|t| !t.abstract_ && !t.junction)
        .map(|t| (t.module.as_str(), t.table.as_str()))
        .collect();
    let current_keys: HashSet<(&str, &str)> = current.tables.iter()
        .map(|t| (t.schema.as_str(), t.name.as_str()))
        .collect();

    let dropped: Vec<&DbTable> = current.tables.iter()
        .filter(|t| !target_keys.contains(&(t.schema.as_str(), t.name.as_str())))
        .collect();
    let created: Vec<&TypeDescriptor> = target.types.iter()
        .filter(|t| !t.abstract_ && !t.junction)
        .filter(|t| !current_keys.contains(&(t.module.as_str(), t.table.as_str())))
        .collect();

    if dropped.is_empty() || created.is_empty() {
        return vec![];
    }

    let mut candidates: Vec<TypeRenameCandidate> = Vec::new();
    for dropped_t in &dropped {
        let old_cols: HashSet<&str> = dropped_t.columns.iter()
            .map(|c| c.name.as_str())
            .filter(|n| !n.starts_with("__"))
            .collect();
        for new_type in &created {
            let new_cols: HashSet<&str> = new_type.properties.iter()
                .map(|p| p.name.as_str())
                .collect();
            let intersection = old_cols.intersection(&new_cols).count();
            let union_size = old_cols.union(&new_cols).count();
            if union_size == 0 { continue; }
            let confidence = intersection as f64 / union_size as f64;
            if confidence >= 0.4 {
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
    candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    candidates
}

/// Detect potential column renames within tables that exist in both `current`
/// and `target`. A candidate is a (dropped_col, added_col) pair in the same
/// table with the same Postgres type.
pub fn detect_col_renames(target: &SchemaDescriptor, current: &DbState) -> Vec<ColRenameCandidate> {
    let cur_tables: HashMap<(&str, &str), &DbTable> = current.tables.iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();

    let mut candidates: Vec<ColRenameCandidate> = Vec::new();
    for td in &target.types {
        if td.abstract_ || td.junction { continue; }
        let Some(cur) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) else { continue };

        // Target columns as owned Vec to avoid temporary String lifetime issues.
        let target_cols: Vec<(String, String)> = td.properties.iter()
            .map(|p| (p.name.clone(), col_type_str(&p.pg_type).to_string()))
            .chain(td.links.iter().map(|l| (format!("{}_id", l.name), "uuid".to_string())))
            .collect();

        // Current columns (skip internal __*__ columns).
        let cur_cols: Vec<(&str, &str)> = cur.columns.iter()
            .filter(|c| !c.name.starts_with("__"))
            .map(|c| (c.name.as_str(), c.pg_type.as_str()))
            .collect();

        // Dropped: in current but not in target.
        let dropped: Vec<(&str, &str)> = cur_cols.iter().copied()
            .filter(|(name, _)| !target_cols.iter().any(|(t, _)| t.as_str() == *name))
            .collect();
        // Added: in target but not in current.
        let added: Vec<(&str, &str)> = target_cols.iter()
            .filter(|(name, _)| !cur_cols.iter().any(|&(c, _)| c == name.as_str()))
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();

        if dropped.is_empty() || added.is_empty() { continue; }

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
            if let Some(added_names) = added_by_type.get(pg_type) {
                if dropped_names.len() == 1 && added_names.len() == 1 {
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

/// Like `diff_schema_ops` but incorporates confirmed renames: emits
/// ALTER TABLE RENAME (for types) and ALTER TABLE RENAME COLUMN (for columns)
/// instead of DROP + CREATE pairs for renamed objects.
///
/// `type_renames`: `(old_module, old_table, new_module, new_table)` tuples.
/// `col_renames`:  `(module, table, old_col, new_col)` tuples.
pub fn diff_schema_ops_with_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: &[(String, String, String, String)],
    col_renames: &[(String, String, String, String)],
) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut modified = current.clone();

    // ── Emit type rename DDL and update modified state ────────────────────────
    for (old_mod, old_table, new_mod, new_table) in type_renames {
        if old_mod == new_mod {
            push_tx(&mut ops, format!(
                "ALTER TABLE {} RENAME TO {};",
                qn(old_mod, old_table), qi(new_table)
            ));
        } else {
            push_tx(&mut ops, format!(
                "ALTER TABLE {} SET SCHEMA {};",
                qn(old_mod, old_table), qi(new_mod)
            ));
            push_tx(&mut ops, format!(
                "ALTER TABLE {} RENAME TO {};",
                qn(new_mod, old_table), qi(new_table)
            ));
        }
        // Make diff_inner think the new name already exists (with old columns).
        if let Some(t) = modified.tables.iter_mut()
            .find(|t| &t.schema == old_mod && &t.name == old_table)
        {
            t.schema = new_mod.clone();
            t.name = new_table.clone();
        }
    }

    // ── Emit column rename DDL and update modified state ──────────────────────
    for (module, table, old_col, new_col) in col_renames {
        push_tx(&mut ops, format!(
            "ALTER TABLE {} RENAME COLUMN {} TO {};",
            qn(module, table), qi(old_col), qi(new_col)
        ));
        if let Some(t) = modified.tables.iter_mut()
            .find(|t| &t.schema == module && &t.name == table)
        {
            if let Some(col) = t.columns.iter_mut().find(|c| &c.name == old_col) {
                col.name = new_col.clone();
            }
        }
    }

    // ── Run the standard diff against the modified state ──────────────────────
    let mut diff_ops = diff_inner(target, &modified, true);
    ops.append(&mut diff_ops);
    ops
}

// ── Identifier helpers ────────────────────────────────────────────────────────

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", qi(schema), qi(name))
}

// ── Topological sort (referenced types before referencing) ────────────────────

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
        if visited[i] { return; }
        visited[i] = true;
        for l in &types[i].links {
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

fn col_type_str(pg_type: &str) -> &str {
    pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(pg_type)
}

// ── Core diff implementation ──────────────────────────────────────────────────

fn diff_inner(target: &SchemaDescriptor, current: &DbState, for_migration: bool) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();

    let cur_schemas: HashSet<&str> = current.schemas.iter().map(|s| s.as_str()).collect();
    let cur_tables: HashMap<(&str, &str), &DbTable> = current.tables.iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();
    let cur_enums: HashMap<(&str, &str), &DbEnum> = current.enums.iter()
        .map(|e| ((e.schema.as_str(), e.name.as_str()), e))
        .collect();
    let cur_domains: HashSet<(&str, &str)> = current.domains.iter()
        .map(|d| (d.schema.as_str(), d.name.as_str()))
        .collect();

    let type_map: HashMap<String, (&str, &str)> = target.types.iter()
        .map(|t| (format!("{}::{}", t.module, t.name), (t.module.as_str(), t.table.as_str())))
        .collect();

    let mut target_schemas: HashSet<String> = HashSet::new();
    for t in &target.types  { target_schemas.insert(t.module.clone()); }
    for e in &target.enums  { target_schemas.insert(e.module.clone()); }
    for s in &target.scalars { target_schemas.insert(s.module.clone()); }

    // ── Phase 1: new schemas ──────────────────────────────────────────────────
    for schema in &target_schemas {
        if !cur_schemas.contains(schema.as_str()) {
            push_tx(&mut ops, format!("CREATE SCHEMA IF NOT EXISTS {};", qi(schema)));
        }
    }

    // ── Phase 2: new / altered enums ──────────────────────────────────────────
    for e in &target.enums {
        match cur_enums.get(&(e.module.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e.members.iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                push_tx(&mut ops, format!(
                    "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                    qi(&e.module), qi(&e.name), members.join(", ")
                ));
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        push_tx(&mut ops, format!(
                            "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                            qi(&e.module), qi(&e.name), member.replace('\'', "''")
                        ));
                    }
                }
            }
        }
    }

    // ── Phase 3: new custom scalar domains ────────────────────────────────────
    for s in &target.scalars {
        if !cur_domains.contains(&(s.module.as_str(), s.name.as_str())) {
            let checks: Vec<String> = s.check_constraints.iter()
                .map(|c| format!("    CHECK ({})", c))
                .collect();
            let check_clause = if checks.is_empty() { String::new() } else { format!("\n{}", checks.join("\n")) };
            // Postgres has no CREATE DOMAIN IF NOT EXISTS, so use the same
            // exception-swallowing pattern we use for enums.
            push_tx(&mut ops, format!(
                "DO $do$ BEGIN CREATE DOMAIN {}.{} AS {}{}; \
                 EXCEPTION WHEN duplicate_object THEN NULL; END $do$;",
                qi(&s.module), qi(&s.name), s.pg_type, check_clause
            ));
        }
    }

    // ── Phase 4 & 5: tables (create new or alter existing) ────────────────────
    let sort_order = topo_sort_types(&target.types);

    // Track which tables are created in this diff (needed for CONCURRENTLY decision).
    let mut new_tables: HashSet<(String, String)> = HashSet::new();

    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction { continue; }
        let key = (td.module.as_str(), td.table.as_str());
        match cur_tables.get(&key) {
            None => {
                emit_create_table(td, &mut ops);
                new_tables.insert((td.module.clone(), td.table.clone()));
            }
            Some(existing) => emit_column_diff(td, existing, &mut ops),
        }
    }

    // ── Phase 6: FK constraints for existing tables ───────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction { continue; }
        if let Some(existing) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) {
            emit_fk_diff(td, existing, &type_map, &mut ops);
        }
    }

    // ── Phase 7: junction tables for new multi-links ──────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction { continue; }
        for ml in &td.multilinks {
            let jt = format!("{}.{}", td.table, ml.name);
            if !cur_tables.contains_key(&(td.module.as_str(), jt.as_str())) {
                emit_junction_table(td, &ml.name, &ml.target, &ml.on_delete, &type_map, &mut ops);
                new_tables.insert((td.module.clone(), jt));
            }
        }
    }

    // ── Phase 8: vector columns + indexes ─────────────────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.vector_indexes.is_empty() { continue; }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        let table_is_new = new_tables.contains(&(td.module.clone(), td.table.clone()));

        for vi in &td.vector_indexes {
            let col = vi.column_name();
            if existing.map(|t| t.columns.iter().any(|c| c.name == col)).unwrap_or(false) {
                continue;
            }
            push_tx(&mut ops, format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} vector({});",
                qn(&td.module, &td.table), qi(&col), vi.dimensions
            ));
            let idx_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(n) => format!("{}__vector_{}__", td.table, n),
            };
            // Pre-existing tables: CONCURRENTLY (non-transactional step in migration).
            // New tables: plain CREATE INDEX (no rows, no locking concern).
            let use_concurrently = for_migration && !table_is_new;
            let idx_sql = if use_concurrently {
                format!("CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING hnsw ({} {});",
                    qi(&idx_name), qn(&td.module, &td.table), qi(&col), vi.ops_class())
            } else {
                format!("CREATE INDEX IF NOT EXISTS {} ON {} USING hnsw ({} {});",
                    qi(&idx_name), qn(&td.module, &td.table), qi(&col), vi.ops_class())
            };
            ops.push(DiffOp { sql: idx_sql, non_transactional: use_concurrently });
        }
    }

    // ── Phase 9: search tsvector columns + GIN indexes ────────────────────────
    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.search_indexes.is_empty() { continue; }
        let existing = cur_tables.get(&(td.module.as_str(), td.table.as_str()));
        let table_is_new = new_tables.contains(&(td.module.clone(), td.table.clone()));

        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres { continue; }
            let col = si.column_name();
            if existing.map(|t| t.columns.iter().any(|c| c.name == col)).unwrap_or(false) {
                continue;
            }
            let parts: Vec<String> = si.fields.iter()
                .map(|sf| format!(
                    "setweight(to_tsvector('english', coalesce({}, '')), '{}')",
                    qi(&sf.name), sf.weight.as_str()
                ))
                .collect();
            let expr = if parts.len() == 1 { parts.into_iter().next().unwrap() } else { parts.join(" || ") };
            push_tx(&mut ops, format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED;",
                qn(&td.module, &td.table), qi(&col), expr
            ));
            let idx_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(n) => format!("{}__search_{}__", td.table, n),
            };
            let use_concurrently = for_migration && !table_is_new;
            let idx_sql = if use_concurrently {
                format!("CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING gin ({});",
                    qi(&idx_name), qn(&td.module, &td.table), qi(&col))
            } else {
                format!("CREATE INDEX IF NOT EXISTS {} ON {} USING gin ({});",
                    qi(&idx_name), qn(&td.module, &td.table), qi(&col))
            };
            ops.push(DiffOp { sql: idx_sql, non_transactional: use_concurrently });
        }
    }

    // ── Phase 10: drop removed tables ─────────────────────────────────────────
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
    for cur_table in &current.tables {
        let key = (cur_table.schema.clone(), cur_table.name.clone());
        if !target_tables.contains(&key) {
            push_tx(&mut ops, format!(
                "DROP TABLE IF EXISTS {} CASCADE;",
                qn(&cur_table.schema, &cur_table.name)
            ));
        }
    }

    // ── Phase 11: drop removed enums ──────────────────────────────────────────
    let target_enum_set: HashSet<(String, String)> = target.enums.iter()
        .map(|e| (e.module.clone(), e.name.clone()))
        .collect();
    for cur_enum in &current.enums {
        if !target_enum_set.contains(&(cur_enum.schema.clone(), cur_enum.name.clone())) {
            push_tx(&mut ops, format!(
                "DROP TYPE IF EXISTS {}.{} CASCADE;",
                qi(&cur_enum.schema), qi(&cur_enum.name)
            ));
        }
    }

    // ── Phase 12: drop removed domains ────────────────────────────────────────
    let target_domain_set: HashSet<(String, String)> = target.scalars.iter()
        .map(|s| (s.module.clone(), s.name.clone()))
        .collect();
    for cur_domain in &current.domains {
        if !target_domain_set.contains(&(cur_domain.schema.clone(), cur_domain.name.clone())) {
            push_tx(&mut ops, format!(
                "DROP DOMAIN IF EXISTS {}.{} CASCADE;",
                qi(&cur_domain.schema), qi(&cur_domain.name)
            ));
        }
    }

    // ── Phase 13: drop removed schemas ────────────────────────────────────────
    for schema in &current.schemas {
        if !target_schemas.contains(schema) {
            push_tx(&mut ops, format!("DROP SCHEMA IF EXISTS {} CASCADE;", qi(schema)));
        }
    }

    ops
}

fn push_tx(ops: &mut Vec<DiffOp>, sql: String) {
    ops.push(DiffOp { sql, non_transactional: false });
}

// ── CREATE TABLE for a new type ───────────────────────────────────────────────

fn emit_create_table(td: &TypeDescriptor, ops: &mut Vec<DiffOp>) {
    let mut lines: Vec<String> = Vec::new();
    for p in &td.properties {
        let not_null = if p.nullable { "" } else { " NOT NULL" };
        let default = p.default_sql.as_deref()
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        lines.push(format!("    {} {}{}{}", qi(&p.name), col_type_str(&p.pg_type), not_null, default));
    }
    for l in &td.links {
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        lines.push(format!("    {} uuid{}", qi(&format!("{}_id", l.name)), not_null));
    }
    let pk_cols: Vec<String> = td.properties.iter().filter(|p| p.is_pk).map(|p| qi(&p.name)).collect();
    if !pk_cols.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }
    push_tx(ops, format!(
        "CREATE TABLE IF NOT EXISTS {} (\n{}\n);",
        qn(&td.module, &td.table),
        lines.join(",\n")
    ));
}

// ── Column diff for an existing table ─────────────────────────────────────────

fn emit_column_diff(td: &TypeDescriptor, existing: &DbTable, ops: &mut Vec<DiffOp>) {
    let existing_cols: HashSet<&str> = existing.columns.iter().map(|c| c.name.as_str()).collect();

    for p in &td.properties {
        if !existing_cols.contains(p.name.as_str()) {
            let not_null = if p.nullable { "" } else { " NOT NULL" };
            let default = p.default_sql.as_deref()
                .map(|d| format!(" DEFAULT {}", d))
                .unwrap_or_default();
            push_tx(ops, format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}{}{};",
                qn(&td.module, &td.table), qi(&p.name), col_type_str(&p.pg_type), not_null, default
            ));
        }
    }
    for l in &td.links {
        let col = format!("{}_id", l.name);
        if !existing_cols.contains(col.as_str()) {
            let not_null = if l.nullable { "" } else { " NOT NULL" };
            push_tx(ops, format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} uuid{};",
                qn(&td.module, &td.table), qi(&col), not_null
            ));
        }
    }
    // Drop columns no longer in the target (skip Pylon-managed __*__ columns).
    let target_cols: HashSet<String> = td.properties.iter().map(|p| p.name.clone())
        .chain(td.links.iter().map(|l| format!("{}_id", l.name)))
        .collect();
    for col in &existing.columns {
        let n = col.name.as_str();
        if target_cols.contains(n) { continue; }
        if n.starts_with("__") && n.ends_with("__") { continue; }
        push_tx(ops, format!(
            "ALTER TABLE {} DROP COLUMN IF EXISTS {};",
            qn(&td.module, &td.table), qi(n)
        ));
    }
}

// ── FK diff for an existing table ─────────────────────────────────────────────

fn emit_fk_diff(
    td: &TypeDescriptor,
    existing: &DbTable,
    type_map: &HashMap<String, (&str, &str)>,
    ops: &mut Vec<DiffOp>,
) {
    use crate::schema::{DeleteAction, DeleteSide};

    let existing_fk_names: HashSet<&str> = existing.foreign_keys.iter()
        .map(|fk| fk.constraint_name.as_str())
        .collect();

    for l in &td.links {
        let cname = format!("{}_{}_fkey", td.table, l.name);
        if existing_fk_names.contains(cname.as_str()) { continue; }
        let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else { continue };
        let on_delete = l.on_delete.iter()
            .find(|p| p.side == DeleteSide::Target)
            .map(|p| match &p.action {
                DeleteAction::Restrict => " ON DELETE RESTRICT",
                DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED",
                DeleteAction::DeleteSource => " ON DELETE CASCADE",
                DeleteAction::Allow => " ON DELETE SET NULL",
                _ => " ON DELETE RESTRICT",
            })
            .unwrap_or(" ON DELETE RESTRICT");
        push_tx(ops, format!(
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
    ops: &mut Vec<DiffOp>,
) {
    use crate::schema::{DeleteAction, DeleteSide};

    let jt_name = format!("{}.{}", td.table, ml_name);
    let src_on_delete = " ON DELETE CASCADE";
    let tgt_on_delete = on_delete.iter()
        .find(|p| p.side == DeleteSide::Target)
        .map(|p| match &p.action {
            DeleteAction::Restrict => " ON DELETE RESTRICT",
            DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED",
            DeleteAction::Allow => " ON DELETE CASCADE",
            DeleteAction::DeleteSource => " ON DELETE CASCADE",
            _ => " ON DELETE RESTRICT",
        })
        .unwrap_or(" ON DELETE RESTRICT");

    let tgt_ref = type_map.get(ml_target).map(|(m, t)| qn(m, t))
        .unwrap_or_else(|| qi(ml_target));

    push_tx(ops, format!(
        "CREATE TABLE IF NOT EXISTS {} (\n    source uuid NOT NULL REFERENCES {}(id){},\n    target uuid NOT NULL REFERENCES {}(id){},\n    PRIMARY KEY (source, target)\n);",
        qn(&td.module, &jt_name),
        qn(&td.module, &td.table),
        src_on_delete,
        tgt_ref,
        tgt_on_delete
    ));
}

// ── diff_states: diff two live-DB snapshots (for squash) ──────────────────────

fn diff_states_inner(before: &DbState, after: &DbState) -> Vec<DiffOp> {
    let mut ops: Vec<DiffOp> = Vec::new();

    let before_schemas: HashSet<&str> = before.schemas.iter().map(|s| s.as_str()).collect();
    let before_tables: HashMap<(&str, &str), &DbTable> = before.tables.iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();
    let before_enums: HashMap<(&str, &str), &DbEnum> = before.enums.iter()
        .map(|e| ((e.schema.as_str(), e.name.as_str()), e))
        .collect();
    let before_domains: HashSet<(&str, &str)> = before.domains.iter()
        .map(|d| (d.schema.as_str(), d.name.as_str()))
        .collect();

    // New schemas
    for schema in &after.schemas {
        if !before_schemas.contains(schema.as_str()) {
            push_tx(&mut ops, format!("CREATE SCHEMA IF NOT EXISTS {};", qi(schema)));
        }
    }

    // New / altered enums
    for e in &after.enums {
        match before_enums.get(&(e.schema.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e.members.iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                push_tx(&mut ops, format!(
                    "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                    qi(&e.schema), qi(&e.name), members.join(", ")
                ));
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        push_tx(&mut ops, format!(
                            "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                            qi(&e.schema), qi(&e.name), member.replace('\'', "''")
                        ));
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
            push_tx(&mut ops, format!(
                "-- TODO: recreate domain {}.{} (reconstruct DDL from source migrations)",
                qi(&d.schema), qi(&d.name)
            ));
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
            let before_fk_names: HashSet<&str> = before_t.foreign_keys.iter()
                .map(|fk| fk.constraint_name.as_str())
                .collect();
            for fk in &t.foreign_keys {
                if !before_fk_names.contains(fk.constraint_name.as_str()) {
                    // Can't fully reconstruct FK DDL from DbForeignKey without ON DELETE info;
                    // emit best-effort.
                    push_tx(&mut ops, format!(
                        "ALTER TABLE {}.{} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{}(id);",
                        qi(&t.schema), qi(&t.name), qi(&fk.constraint_name),
                        qi(&fk.local_column), qi(&fk.ref_schema), qi(&fk.ref_table)
                    ));
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
            if before_idx_names.contains(idx.name.as_str()) { continue; }
            let use_concurrently = !table_is_new;
            let concurrently = if use_concurrently { "CONCURRENTLY " } else { "" };
            let unique = if idx.is_unique { "UNIQUE " } else { "" };
            let idx_sql = format!(
                "CREATE {unique}INDEX {concurrently}IF NOT EXISTS {} ON {}.{};",
                qi(&idx.name), qi(&t.schema), qi(&t.name)
            );
            ops.push(DiffOp { sql: idx_sql, non_transactional: use_concurrently });
        }
    }

    // Drop removed tables
    let after_tables: HashSet<(&str, &str)> = after.tables.iter()
        .map(|t| (t.schema.as_str(), t.name.as_str()))
        .collect();
    for t in &before.tables {
        if !after_tables.contains(&(t.schema.as_str(), t.name.as_str())) {
            push_tx(&mut ops, format!(
                "DROP TABLE IF EXISTS {}.{} CASCADE;",
                qi(&t.schema), qi(&t.name)
            ));
        }
    }

    // Drop removed enums
    let after_enum_set: HashSet<(&str, &str)> = after.enums.iter()
        .map(|e| (e.schema.as_str(), e.name.as_str()))
        .collect();
    for e in &before.enums {
        if !after_enum_set.contains(&(e.schema.as_str(), e.name.as_str())) {
            push_tx(&mut ops, format!(
                "DROP TYPE IF EXISTS {}.{} CASCADE;",
                qi(&e.schema), qi(&e.name)
            ));
        }
    }

    // Drop removed schemas
    let after_schema_set: HashSet<&str> = after.schemas.iter().map(|s| s.as_str()).collect();
    for schema in &before.schemas {
        if !after_schema_set.contains(schema.as_str()) {
            push_tx(&mut ops, format!("DROP SCHEMA IF EXISTS {} CASCADE;", qi(schema)));
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
            lines.push(format!("    {} {} GENERATED ALWAYS AS (/* see source */) STORED", qi(&col.name), col.pg_type));
        } else {
            lines.push(format!("    {} {}{}", qi(&col.name), col.pg_type, not_null));
        }
    }
    push_tx(ops, format!(
        "CREATE TABLE IF NOT EXISTS {}.{} (\n{}\n);",
        qi(&t.schema), qi(&t.name),
        lines.join(",\n")
    ));
}

fn emit_column_diff_from_db(after: &DbTable, before: &DbTable, ops: &mut Vec<DiffOp>) {
    let before_cols: HashSet<&str> = before.columns.iter().map(|c| c.name.as_str()).collect();
    let after_cols: HashSet<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();

    for col in &after.columns {
        if !before_cols.contains(col.name.as_str()) {
            let not_null = if col.nullable { "" } else { " NOT NULL" };
            push_tx(ops, format!(
                "ALTER TABLE {}.{} ADD COLUMN IF NOT EXISTS {} {}{};",
                qi(&after.schema), qi(&after.name), qi(&col.name), col.pg_type, not_null
            ));
        }
    }
    for col in &before.columns {
        if !after_cols.contains(col.name.as_str()) {
            push_tx(ops, format!(
                "ALTER TABLE {}.{} DROP COLUMN IF EXISTS {};",
                qi(&after.schema), qi(&after.name), qi(&col.name)
            ));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EnumDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};

    fn empty_state() -> DbState { DbState::default() }

    fn prop(name: &str, pg_type: &str, nullable: bool) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(), pg_type: pg_type.into(), nullable,
            default_sql: if name == "id" { Some("uuidv7()".into()) } else { None },
            description: None, check_constraints: vec![],
            is_exclusive: name == "id", is_pk: name == "id",
            is_readonly: name == "id", rewrites: vec![],
        }
    }

    fn simple_type(module: &str, name: &str, table: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: name.into(), module: module.into(), table: table.into(),
            abstract_: false, materialized: false, description: None,
            parents: vec![], interfaces: vec![],
            properties: vec![prop("id", "uuid", false), prop("name", "text", true)],
            links: vec![], multilinks: vec![], computed: vec![],
            constraints: vec![], indexes: vec![],
            vector_indexes: vec![], search_indexes: vec![], triggers: vec![],
            junction: false,
        }
    }

    #[test]
    fn test_new_schema_and_table() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("catalog", "Product", "Product")],
            scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        let ops = diff_schema(&schema, &empty_state());
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE SCHEMA IF NOT EXISTS \"catalog\""), "got:\n{joined}");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS \"catalog\".\"Product\""), "got:\n{joined}");
    }

    #[test]
    fn test_no_ops_when_in_sync() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![],
            }],
            enums: vec![], domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
        assert!(ops.is_empty(), "expected no ops, got: {:?}", ops);
    }

    #[test]
    fn test_add_column() {
        let mut td = simple_type("default", "Person", "Person");
        td.properties.push(prop("email", "text", true));
        let schema = SchemaDescriptor {
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![],
            }],
            enums: vec![], domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
        let joined = ops.join("\n");
        assert!(joined.contains("ADD COLUMN IF NOT EXISTS \"email\""), "got:\n{joined}");
    }

    #[test]
    fn test_new_enum() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![],
            enums: vec![EnumDescriptor {
                name: "Status".into(), module: "default".into(),
                members: vec!["Active".into(), "Inactive".into()],
            }],
            globals: vec![], functions: vec![],
        };
        let ops = diff_schema(&schema, &empty_state());
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE TYPE \"default\".\"Status\" AS ENUM"), "got:\n{joined}");
    }

    #[test]
    fn test_drop_table() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "OldType".into(),
                columns: vec![], foreign_keys: vec![], indexes: vec![], checks: vec![],
            }],
            enums: vec![], domains: vec![],
        };
        let ops = diff_schema(&schema, &state);
        let joined = ops.join("\n");
        assert!(joined.contains("DROP TABLE IF EXISTS \"default\".\"OldType\" CASCADE"), "got:\n{joined}");
    }

    #[test]
    fn test_index_on_existing_table_is_concurrently() {
        use crate::schema::{VectorIndexDescriptor};
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            fields: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        // The table already exists in the DB (pre-existing).
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Post".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![],
            }],
            enums: vec![], domains: vec![],
        };
        let ops = diff_schema_ops(&schema, &state);
        let idx_op = ops.iter().find(|op| op.sql.contains("hnsw")).unwrap();
        assert!(idx_op.non_transactional, "index on pre-existing table should be non-transactional");
        assert!(idx_op.sql.contains("CONCURRENTLY"), "should use CONCURRENTLY: {}", idx_op.sql);
    }

    #[test]
    fn test_index_on_new_table_is_transactional() {
        use crate::schema::VectorIndexDescriptor;
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            fields: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        };
        // Table does NOT exist in the DB → it's new.
        let ops = diff_schema_ops(&schema, &empty_state());
        let idx_op = ops.iter().find(|op| op.sql.contains("hnsw")).unwrap();
        assert!(!idx_op.non_transactional, "index on new table should be transactional");
        assert!(!idx_op.sql.contains("CONCURRENTLY"), "should NOT use CONCURRENTLY: {}", idx_op.sql);
    }
}
