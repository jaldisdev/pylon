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
        .map(|t| (format!("{}::{}", t.module, t.name), (t.module.as_str(), t.table.as_str())))
        .collect();

    // Collect all module names that contribute a Postgres schema.
    let mut schema_set: BTreeSet<String> = BTreeSet::new();
    for t in &schema.types  { schema_set.insert(t.module.clone()); }
    for e in &schema.enums  { schema_set.insert(e.module.clone()); }
    for s in &schema.scalars { schema_set.insert(s.module.clone()); }

    let schemas: Vec<String> = schema_set.into_iter().collect();

    // Enums
    let enums: Vec<DbEnum> = schema.enums.iter()
        .map(|e| DbEnum { schema: e.module.clone(), name: e.name.clone(), members: e.members.clone() })
        .collect();

    // Domains (custom scalars)
    let domains: Vec<DbDomain> = schema.scalars.iter()
        .map(|s| DbDomain { schema: s.module.clone(), name: s.name.clone() })
        .collect();

    // Sequences (sequence scalars only)
    let sequences: Vec<DbSequence> = schema.scalars.iter()
        .filter(|s| s.is_sequence)
        .map(|s| DbSequence { schema: s.module.clone(), name: format!("{}_seq", s.name) })
        .collect();

    let excl_trigger_names = expected_excl_trigger_names(schema);
    let mut tables: Vec<DbTable> = Vec::new();

    for td in &schema.types {
        if td.abstract_ || td.junction { continue; }

        // Columns: properties + link FK stubs + vector/search generated columns
        let mut columns: Vec<DbColumn> = Vec::new();
        for p in &td.properties {
            columns.push(DbColumn {
                name: p.name.clone(),
                pg_type: col_type_str(&p.pg_type).to_string(),
                nullable: p.nullable,
                is_generated: false,
                column_default: resolve_default(p, schema),
            });
        }
        for l in &td.links {
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
            if si.backend != SearchBackend::Postgres { continue; }
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
            if l.is_exclusive {
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
                indexes.push(DbIndex { name: idx_name, is_unique: true, method: "btree".to_string() });
            }
        }
        // Plain indexes (Postgres auto-names these too).
        for (i, idx) in td.indexes.iter().enumerate() {
            let name = if idx.expression.is_some() {
                format!("{}__expr{}_idx", td.table, i)
            } else {
                format!("{}__{}_idx", td.table, idx.pointers.join("_"))
            };
            indexes.push(DbIndex { name, is_unique: idx.unique, method: "btree".to_string() });
        }
        // Vector HNSW indexes
        for vi in &td.vector_indexes {
            let idx_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(n) => format!("{}__vector_{}__", td.table, n),
            };
            indexes.push(DbIndex { name: idx_name, is_unique: false, method: "hnsw".to_string() });
        }
        // Search GIN indexes
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres { continue; }
            let idx_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(n) => format!("{}__search_{}__", td.table, n),
            };
            indexes.push(DbIndex { name: idx_name, is_unique: false, method: "gin".to_string() });
        }

        // CHECK constraints — names are hash-based in the real schema; approximate here.
        let mut checks: Vec<DbCheck> = Vec::new();
        for p in &td.properties {
            for (i, _) in p.check_constraints.iter().enumerate() {
                checks.push(DbCheck { constraint_name: format!("{}_{}_check_{}", td.table, p.name, i) });
            }
        }
        for (i, constraint) in td.constraints.iter().enumerate() {
            use crate::schema::TypeConstraint;
            if let TypeConstraint::Expression { .. } = constraint {
                checks.push(DbCheck { constraint_name: format!("{}_expr_check_{}", td.table, i) });
            }
        }

        let triggers = excl_trigger_names
            .get(&(td.module.clone(), td.table.clone()))
            .cloned()
            .unwrap_or_default();
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
            let jt_name = format!("{}.{}", td.table, ml.name);
            let mut jt_columns = vec![
                DbColumn { name: "source".to_string(), pg_type: "uuid".to_string(), nullable: false, is_generated: false, column_default: None },
                DbColumn { name: "target".to_string(), pg_type: "uuid".to_string(), nullable: false, is_generated: false, column_default: None },
            ];

            // Extra columns from the through junction type
            if let Some(through_qname) = &ml.through {
                if let Some(through_td) = schema.types.iter().find(|t| {
                    format!("{}::{}", t.module, t.name) == *through_qname && t.junction
                }) {
                    for p in &through_td.properties {
                        if p.name == "id" { continue; }
                        let pg_type = col_type_str(&p.pg_type).to_string();
                        jt_columns.push(DbColumn {
                            name: p.name.clone(),
                            pg_type,
                            nullable: p.nullable,
                            is_generated: false,
                            column_default: p.default_sql.clone(),
                        });
                    }
                }
            }

            let mut jt_fks = Vec::new();
            let src_fk_name = format!("{}_{}_source_fkey", td.table, ml.name);
            jt_fks.push(DbForeignKey {
                constraint_name: src_fk_name,
                local_column: "source".to_string(),
                ref_schema: td.module.clone(),
                ref_table: td.table.clone(),
            });
            if let Some((tgt_schema, tgt_table)) = type_map.get(&ml.target) {
                let tgt_fk_name = format!("{}_{}_target_fkey", td.table, ml.name);
                jt_fks.push(DbForeignKey {
                    constraint_name: tgt_fk_name,
                    local_column: "target".to_string(),
                    ref_schema: tgt_schema.to_string(),
                    ref_table: tgt_table.to_string(),
                });
            }

            tables.push(DbTable {
                schema: td.module.clone(),
                name: jt_name,
                columns: jt_columns,
                foreign_keys: jt_fks,
                indexes: vec![],
                checks: vec![],
                triggers: vec![],
            });
        }
    }

    // Views (interface types)
    let views: Vec<DbView> = crate::export::interface_view_ddl_with_names(schema)
        .into_iter()
        .map(|(module, name, ddl)| DbView { schema: module, name, body_hash: ddl_hash(&ddl) })
        .collect();

    // User-defined functions
    let functions: Vec<DbFunction> = crate::export::function_ddl_with_names(schema)
        .unwrap_or_default()
        .into_iter()
        .map(|(module, name, ddl)| DbFunction { schema: module, name, body_hash: ddl_hash(&ddl) })
        .collect();

    DbState { schemas, tables, enums, domains, sequences, views, functions }
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

/// Compute the expected constraint trigger names per concrete table for all
/// interface-level exclusive constraints in `schema`.
///
/// Returns a map from `(module, table)` to the list of trigger names that
/// should exist on that concrete table.
fn expected_excl_trigger_names(schema: &SchemaDescriptor) -> HashMap<(String, String), Vec<String>> {
    use crate::schema::TypeConstraint;

    let mut impl_map: HashMap<String, Vec<&TypeDescriptor>> = HashMap::new();
    for t in &schema.types {
        if !t.abstract_ {
            for iface in &t.interfaces {
                impl_map.entry(iface.clone()).or_default().push(t);
            }
        }
    }

    let mut result: HashMap<(String, String), Vec<String>> = HashMap::new();
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) { continue; }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = impl_map.get(&key) else { continue };
        if impls.is_empty() { continue; }

        let mut fields_list: Vec<Vec<String>> = Vec::new();
        for p in &t.properties {
            if p.is_exclusive && !p.is_pk { fields_list.push(vec![p.name.clone()]); }
        }
        for l in &t.links {
            if l.is_exclusive { fields_list.push(vec![format!("{}_id", l.name)]); }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive { pointers: fields, .. } = c { fields_list.push(fields.clone()); }
        }

        for fields in &fields_list {
            let fn_name = format!("_excl_{}_{}", t.table, fields.join("_"));
            for impl_t in impls {
                let entry = result
                    .entry((impl_t.module.clone(), impl_t.table.clone()))
                    .or_default();
                entry.push(format!("{}_ins", fn_name));
                entry.push(format!("{}_upd", fn_name));
            }
        }
    }
    result
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
pub fn diff_schema(target: &SchemaDescriptor, current: &DbState) -> Result<Vec<String>, String> {
    Ok(diff_inner(target, current, false, &HashMap::new())?
        .into_iter()
        .map(|op| op.sql)
        .collect())
}

/// Compute ordered `DiffOp`s suitable for a migration file body.
/// Index creation on pre-existing tables uses `CONCURRENTLY` and is marked
/// `non_transactional = true` so `create` can insert step-boundary markers.
pub fn diff_schema_ops(target: &SchemaDescriptor, current: &DbState) -> Result<Vec<DiffOp>, String> {
    diff_inner(target, current, true, &HashMap::new())
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
) -> Result<Vec<DiffOp>, String> {
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
    let mut diff_ops = diff_inner(target, &modified, true, &HashMap::new())?;
    ops.append(&mut diff_ops);
    Ok(ops)
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
    let cur_tables: HashMap<(&str, &str), &DbTable> = current.tables.iter()
        .map(|t| ((t.schema.as_str(), t.name.as_str()), t))
        .collect();

    let mut result: Vec<FillRequired> = Vec::new();

    for td in &target.types {
        if td.abstract_ || td.junction { continue; }
        let Some(cur) = cur_tables.get(&(td.module.as_str(), td.table.as_str())) else { continue };

        let cur_col_map: HashMap<&str, &DbColumn> = cur.columns.iter()
            .map(|c| (c.name.as_str(), c))
            .collect();

        for p in &td.properties {
            if p.nullable || p.is_pk { continue; }
            match cur_col_map.get(p.name.as_str()) {
                None => {
                    // New required column — only needs a fill when there's no default.
                    if p.default_sql.is_none() {
                        result.push(FillRequired {
                            module: td.module.clone(),
                            table: td.table.clone(),
                            column: p.name.clone(),
                            pg_type: col_type_str(&p.pg_type).to_string(),
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
                        pg_type: col_type_str(&p.pg_type).to_string(),
                        type_name: td.name.clone(),
                        is_new_column: false,
                        default_sql: p.default_sql.clone(),
                    });
                }
                _ => {}
            }
        }

        for l in &td.links {
            if l.nullable { continue; }
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

    // ── Apply type renames ────────────────────────────────────────────────────
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
        if let Some(t) = modified.tables.iter_mut()
            .find(|t| &t.schema == old_mod && &t.name == old_table)
        {
            t.schema = new_mod.clone();
            t.name = new_table.clone();
        }
    }

    // ── Apply column renames ──────────────────────────────────────────────────
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

    // ── Build fill index so emit_column_diff can defer NOT NULL for fills ─────
    let mut fill_index: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for (module, table, col, _) in fills {
        fill_index
            .entry((module.clone(), table.clone()))
            .or_default()
            .insert(col.clone());
    }

    // ── Standard diff (renames already resolved in `modified`) ───────────────
    let mut diff_ops = diff_inner(target, &modified, true, &fill_index)?;
    ops.append(&mut diff_ops);

    // ── Fill DDL: UPDATE backfill + SET NOT NULL ──────────────────────────────
    for (module, table, col, fill_expr) in fills {
        push_tx(&mut ops, format!(
            "UPDATE {} SET {} = {} WHERE {} IS NULL;",
            qn(module, table), qi(col), fill_expr, qi(col)
        ));
        push_tx(&mut ops, format!(
            "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
            qn(module, table), qi(col)
        ));
    }

    Ok(ops)
}

// ── Identifier helpers ────────────────────────────────────────────────────────

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn pg_schema(module: &str) -> String {
    if module == "default" { "\"public\"".into() } else { qi(module) }
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", pg_schema(schema), qi(name))
}

// ── Topological sort (referenced types before referencing) ────────────────────

fn topo_sort_types(types: &[TypeDescriptor]) -> Result<Vec<usize>, String> {
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
        colour: &mut Vec<u8>,
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        match colour[i] {
            2 => return Ok(()),   // already fully processed
            1 => return Err(format!(  // back-edge → cycle
                "circular type dependency involving '{}::{}'",
                types[i].module, types[i].name
            )),
            _ => {}
        }
        colour[i] = 1;
        // Dependencies: FK links only. Multilinks don't create FK columns on
        // the source table — the junction table does — so they are not ordering
        // constraints for the source type itself.
        for l in &types[i].links {
            if let Some(&dep) = idx_of.get(&l.target) {
                visit(dep, types, idx_of, colour, order)?;
            }
        }
        colour[i] = 2;
        order.push(i);
        Ok(())
    }

    for i in 0..types.len() {
        visit(i, types, &idx_of, &mut colour, &mut order)?;
    }
    Ok(order)
}

fn col_type_str(pg_type: &str) -> &str {
    pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(pg_type)
}

// ── Core diff implementation ──────────────────────────────────────────────────

fn diff_inner(
    target: &SchemaDescriptor,
    current: &DbState,
    for_migration: bool,
    fill_index: &HashMap<(String, String), HashSet<String>>,
) -> Result<Vec<DiffOp>, String> {
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
    let cur_sequences: HashSet<(&str, &str)> = current.sequences.iter()
        .map(|s| (s.schema.as_str(), s.name.as_str()))
        .collect();
    let cur_views: HashMap<(&str, &str), &str> = current.views.iter()
        .map(|v| ((v.schema.as_str(), v.name.as_str()), v.body_hash.as_str()))
        .collect();
    let cur_functions: HashMap<(&str, &str), &str> = current.functions.iter()
        .map(|f| ((f.schema.as_str(), f.name.as_str()), f.body_hash.as_str()))
        .collect();

    let type_map: HashMap<String, (&str, &str)> = target.types.iter()
        .map(|t| (format!("{}::{}", t.module, t.name), (t.module.as_str(), t.table.as_str())))
        .collect();

    let mut target_schemas: HashSet<String> = HashSet::new();
    for t in &target.types  { target_schemas.insert(t.module.clone()); }
    for e in &target.enums  { target_schemas.insert(e.module.clone()); }
    for s in &target.scalars { target_schemas.insert(s.module.clone()); }

    // ── Phase 1: schemas ─────────────────────────────────────────────────────
    for schema in &target_schemas {
        if schema == "default" { continue; } // public always exists
        if !cur_schemas.contains(schema.as_str()) {
            push_tx(&mut ops, format!("CREATE SCHEMA IF NOT EXISTS {};", pg_schema(schema)));
        }
    }

    // ── Phase 2: enums ───────────────────────────────────────────────────────
    for e in &target.enums {
        match cur_enums.get(&(e.module.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e.members.iter()
                    .map(|m| format!("'{}'", m.replace('\'', "''")))
                    .collect();
                push_tx(&mut ops, format!(
                    "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); \
                     EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                    pg_schema(&e.module), qi(&e.name), members.join(", ")
                ));
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        push_tx(&mut ops, format!(
                            "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                            pg_schema(&e.module), qi(&e.name), member.replace('\'', "''")
                        ));
                    }
                }
            }
        }
    }

    // ── Phase 2.5: sequences (for sequence scalars) ───────────────────────────
    for s in &target.scalars {
        if s.is_sequence {
            let seq_name = format!("{}_seq", s.name);
            if !cur_sequences.contains(&(s.module.as_str(), seq_name.as_str())) {
                push_tx(&mut ops, format!(
                    "CREATE SEQUENCE IF NOT EXISTS {}.{};",
                    pg_schema(&s.module), qi(&seq_name)
                ));
            }
        }
    }

    // ── Phase 3: custom scalar domains ───────────────────────────────────────
    for s in &target.scalars {
        if !cur_domains.contains(&(s.module.as_str(), s.name.as_str())) {
            let checks: Vec<String> = s.check_constraints.iter()
                .map(|c| format!("    CHECK ({})", c))
                .collect();
            let check_clause = if checks.is_empty() { String::new() } else { format!("\n{}", checks.join("\n")) };
            push_tx(&mut ops, format!(
                "DO $do$ BEGIN CREATE DOMAIN {}.{} AS {}{}; \
                 EXCEPTION WHEN duplicate_object THEN NULL; END $do$;",
                pg_schema(&s.module), qi(&s.name), s.pg_type, check_clause
            ));
        }
    }

    // ── Phase 3.5: scalar functions (before tables — table DEFAULTs may call them) ──
    let scalar_fn_ddls = crate::export::scalar_function_ddl_with_names(target)
        .map_err(|e| e.to_string())?;
    for (module, name, ddl) in scalar_fn_ddls {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_functions.get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit { push_tx(&mut ops, ddl); }
    }

    // ── Phase 4 & 5: tables (create new or alter existing) ───────────────────
    let sort_order = topo_sort_types(&target.types)?;

    // Track which tables are created in this diff (needed for CONCURRENTLY decision).
    let mut new_tables: HashSet<(String, String)> = HashSet::new();

    for &i in &sort_order {
        let td = &target.types[i];
        if td.abstract_ || td.junction { continue; }
        let key = (td.module.as_str(), td.table.as_str());
        match cur_tables.get(&key) {
            None => {
                emit_create_table(td, target, &mut ops);
                new_tables.insert((td.module.clone(), td.table.clone()));
            }
            Some(existing) => {
                let fill_cols = fill_index
                    .get(&(td.module.clone(), td.table.clone()))
                    .cloned()
                    .unwrap_or_default();
                emit_column_diff(td, existing, &mut ops, for_migration, &fill_cols, target);
            }
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
                emit_junction_table(td, &ml.name, &ml.target, ml.through.as_deref(), &ml.on_delete, &type_map, target, &mut ops);
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
            let parts: Vec<String> = si.pointers.iter()
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

    // ── Phase 10: interface views (after all tables exist) ───────────────────
    // In watch mode always re-emit (CREATE OR REPLACE VIEW is idempotent).
    // In migration mode only emit new or changed views.
    for (module, name, ddl) in crate::export::interface_view_ddl_with_names(target) {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_views.get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit { push_tx(&mut ops, ddl); }
    }

    // ── Phase 11: object-returning functions (after tables and views exist) ──
    let obj_fn_ddls = crate::export::object_function_ddl_with_names(target)
        .map_err(|e| e.to_string())?;
    for (module, name, ddl) in obj_fn_ddls {
        let emit = if for_migration {
            let hash = ddl_hash(&ddl);
            cur_functions.get(&(module.as_str(), name.as_str()))
                .map(|&h| h != hash)
                .unwrap_or(true)
        } else {
            true
        };
        if emit { push_tx(&mut ops, ddl); }
    }

    // ── Phase 11.5: interface exclusive constraint triggers ──────────────────
    {
        let infos = crate::export::interface_exclusive_trigger_infos(target);
        let cur_trigger_map: HashMap<(&str, &str), HashSet<&str>> = current.tables.iter()
            .map(|t| (
                (t.schema.as_str(), t.name.as_str()),
                t.triggers.iter().map(|n| n.as_str()).collect::<HashSet<_>>(),
            ))
            .collect();

        // Track expected triggers per table (for the drop phase below).
        let mut expected_trigger_map: HashMap<(String, String), HashSet<String>> = HashMap::new();
        // Track which trigger functions have been emitted in this diff pass.
        let mut fn_emitted: HashSet<String> = HashSet::new();

        for info in &infos {
            let table_key = (info.impl_module.clone(), info.impl_table.clone());
            expected_trigger_map.entry(table_key).or_default()
                .extend([info.ins_trigger_name.clone(), info.upd_trigger_name.clone()]);

            let cur = cur_trigger_map
                .get(&(info.impl_module.as_str(), info.impl_table.as_str()))
                .cloned()
                .unwrap_or_default();
            let need_ins = !cur.contains(info.ins_trigger_name.as_str());
            let need_upd = !cur.contains(info.upd_trigger_name.as_str());
            if need_ins || need_upd {
                if fn_emitted.insert(info.fn_name.clone()) {
                    push_tx(&mut ops, info.fn_ddl.clone());
                }
                if need_ins { push_tx(&mut ops, info.ins_ddl.clone()); }
                if need_upd { push_tx(&mut ops, info.upd_ddl.clone()); }
            }
        }

        // Drop triggers that no longer exist in the target schema.
        for cur_table in &current.tables {
            let key = (cur_table.schema.clone(), cur_table.name.clone());
            let expected = expected_trigger_map.get(&key).cloned().unwrap_or_default();
            for trigger_name in &cur_table.triggers {
                if !expected.contains(trigger_name) {
                    push_tx(&mut ops, format!(
                        "DROP TRIGGER IF EXISTS {} ON {};",
                        qi(trigger_name), qn(&cur_table.schema, &cur_table.name)
                    ));
                }
            }
        }
    }

    // ── Phase 12: drop removed tables ────────────────────────────────────────
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

    // ── Phase 13: drop removed enums ─────────────────────────────────────────
    let target_enum_set: HashSet<(String, String)> = target.enums.iter()
        .map(|e| (e.module.clone(), e.name.clone()))
        .collect();
    for cur_enum in &current.enums {
        if !target_enum_set.contains(&(cur_enum.schema.clone(), cur_enum.name.clone())) {
            push_tx(&mut ops, format!(
                "DROP TYPE IF EXISTS {}.{} CASCADE;",
                pg_schema(&cur_enum.schema), qi(&cur_enum.name)
            ));
        }
    }

    // ── Phase 14: drop removed domains ───────────────────────────────────────
    let target_domain_set: HashSet<(String, String)> = target.scalars.iter()
        .map(|s| (s.module.clone(), s.name.clone()))
        .collect();
    for cur_domain in &current.domains {
        if !target_domain_set.contains(&(cur_domain.schema.clone(), cur_domain.name.clone())) {
            push_tx(&mut ops, format!(
                "DROP DOMAIN IF EXISTS {}.{} CASCADE;",
                pg_schema(&cur_domain.schema), qi(&cur_domain.name)
            ));
        }
    }

    // ── Phase 14.5: drop removed sequences ───────────────────────────────────
    let target_sequence_set: HashSet<(String, String)> = target.scalars.iter()
        .filter(|s| s.is_sequence)
        .map(|s| (s.module.clone(), format!("{}_seq", s.name)))
        .collect();
    for cur_seq in &current.sequences {
        if !target_sequence_set.contains(&(cur_seq.schema.clone(), cur_seq.name.clone())) {
            push_tx(&mut ops, format!(
                "DROP SEQUENCE IF EXISTS {}.{};",
                pg_schema(&cur_seq.schema), qi(&cur_seq.name)
            ));
        }
    }

    // ── Phase 15: drop removed schemas ───────────────────────────────────────
    for schema in &current.schemas {
        if schema == "default" { continue; } // never drop public
        if !target_schemas.contains(schema) {
            push_tx(&mut ops, format!("DROP SCHEMA IF EXISTS {} CASCADE;", pg_schema(schema)));
        }
    }

    Ok(ops)
}

fn push_tx(ops: &mut Vec<DiffOp>, sql: String) {
    ops.push(DiffOp { sql, non_transactional: false });
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
        lines.push(format!("    {} {}{}{}", qi(&p.name), col_type_str(&p.pg_type), not_null, default));
    }
    for l in &td.links {
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        let default = resolve_link_default(l, schema)
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        lines.push(format!("    {} uuid{}{}", qi(&format!("{}_id", l.name)), not_null, default));
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
) {
    let existing_col_map: HashMap<&str, &DbColumn> = existing.columns.iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    // ── Add new columns ───────────────────────────────────────────────────────
    for p in &td.properties {
        if existing_col_map.contains_key(p.name.as_str()) { continue; }
        let eff_default = resolve_default(p, schema);
        let needs_fill = for_migration && !p.nullable && eff_default.is_none()
            && fill_cols.contains(&p.name);
        let not_null = if p.nullable || needs_fill { "" } else { " NOT NULL" };
        let default = eff_default
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        push_tx(ops, format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}{}{};",
            qn(&td.module, &td.table), qi(&p.name), col_type_str(&p.pg_type), not_null, default
        ));
    }
    for l in &td.links {
        let col = format!("{}_id", l.name);
        if existing_col_map.contains_key(col.as_str()) { continue; }
        let eff_default = resolve_link_default(l, schema);
        let needs_fill = for_migration && !l.nullable && eff_default.is_none()
            && fill_cols.contains(&col);
        let not_null = if l.nullable || needs_fill { "" } else { " NOT NULL" };
        let default = eff_default
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        push_tx(ops, format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} uuid{}{};",
            qn(&td.module, &td.table), qi(&col), not_null, default
        ));
    }

    // ── Nullability + DEFAULT changes on existing columns ─────────────────────
    for p in &td.properties {
        let Some(cur) = existing_col_map.get(p.name.as_str()) else { continue };
        if cur.is_generated { continue; }
        if !cur.nullable && p.nullable {
            // NOT NULL → nullable: always safe, no fill needed.
            push_tx(ops, format!(
                "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL;",
                qn(&td.module, &td.table), qi(&p.name)
            ));
        } else if cur.nullable && !p.nullable && !for_migration {
            // nullable → NOT NULL: safe in watch mode (dev DB, typically no rows).
            push_tx(ops, format!(
                "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                qn(&td.module, &td.table), qi(&p.name)
            ));
            // In migration mode this is intentionally skipped; the fill mechanism
            // emits UPDATE + SET NOT NULL after the main diff body.
        }

        // DEFAULT changes
        let target_default = resolve_default(p, schema);
        let db_default = cur.column_default.as_deref();
        match (&target_default, db_default) {
            (Some(want), Some(have)) if want != have => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qn(&td.module, &td.table), qi(&p.name), want
                ));
            }
            (Some(want), None) => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qn(&td.module, &td.table), qi(&p.name), want
                ));
            }
            (None, Some(_)) => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                    qn(&td.module, &td.table), qi(&p.name)
                ));
            }
            _ => {}
        }
    }
    for l in &td.links {
        let col = format!("{}_id", l.name);
        let Some(cur) = existing_col_map.get(col.as_str()) else { continue };
        if !cur.nullable && l.nullable {
            push_tx(ops, format!(
                "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL;",
                qn(&td.module, &td.table), qi(&col)
            ));
        } else if cur.nullable && !l.nullable && !for_migration {
            push_tx(ops, format!(
                "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL;",
                qn(&td.module, &td.table), qi(&col)
            ));
        }

        let target_default = resolve_link_default(l, schema);
        let db_default = cur.column_default.as_deref();
        match (&target_default, db_default) {
            (Some(want), Some(have)) if want != have => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qn(&td.module, &td.table), qi(&col), want
                ));
            }
            (Some(want), None) => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qn(&td.module, &td.table), qi(&col), want
                ));
            }
            (None, Some(_)) => {
                push_tx(ops, format!(
                    "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                    qn(&td.module, &td.table), qi(&col)
                ));
            }
            _ => {}
        }
    }

    // ── Drop removed columns ──────────────────────────────────────────────────
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
    through: Option<&str>,
    on_delete: &[crate::schema::OnDeletePolicy],
    type_map: &HashMap<String, (&str, &str)>,
    schema: &SchemaDescriptor,
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

    let mut col_lines = format!(
        "    source uuid NOT NULL REFERENCES {}(id){},\n    target uuid NOT NULL REFERENCES {}(id){}",
        qn(&td.module, &td.table), src_on_delete,
        tgt_ref, tgt_on_delete,
    );

    // Extra columns from the through junction type.
    if let Some(through_qname) = through {
        if let Some(through_td) = schema.types.iter().find(|t| {
            format!("{}::{}", t.module, t.name) == through_qname && t.junction
        }) {
            for p in &through_td.properties {
                if p.name == "id" { continue; }
                let not_null = if p.nullable { "" } else { " NOT NULL" };
                let pg_type = p.pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(&p.pg_type);
                col_lines.push_str(&format!(",\n    {} {}{}", qi(&p.name), pg_type, not_null));
            }
        }
    }

    push_tx(ops, format!(
        "CREATE TABLE IF NOT EXISTS {} (\n{},\n    PRIMARY KEY (source, target)\n);",
        qn(&td.module, &jt_name),
        col_lines,
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
        if schema == "default" { continue; } // public always exists
        if !before_schemas.contains(schema.as_str()) {
            push_tx(&mut ops, format!("CREATE SCHEMA IF NOT EXISTS {};", pg_schema(schema)));
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
                    pg_schema(&e.schema), qi(&e.name), members.join(", ")
                ));
            }
            Some(existing) => {
                let existing_set: HashSet<&str> = existing.members.iter().map(|m| m.as_str()).collect();
                for member in &e.members {
                    if !existing_set.contains(member.as_str()) {
                        push_tx(&mut ops, format!(
                            "ALTER TYPE {}.{} ADD VALUE IF NOT EXISTS '{}';",
                            pg_schema(&e.schema), qi(&e.name), member.replace('\'', "''")
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
                pg_schema(&d.schema), qi(&d.name)
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
                        pg_schema(&t.schema), qi(&t.name), qi(&fk.constraint_name),
                        qi(&fk.local_column), pg_schema(&fk.ref_schema), qi(&fk.ref_table)
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
                qi(&idx.name), pg_schema(&t.schema), qi(&t.name)
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
                pg_schema(&t.schema), qi(&t.name)
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
                pg_schema(&e.schema), qi(&e.name)
            ));
        }
    }

    // Drop removed schemas
    let after_schema_set: HashSet<&str> = after.schemas.iter().map(|s| s.as_str()).collect();
    for schema in &before.schemas {
        if schema == "default" { continue; } // never drop public
        if !after_schema_set.contains(schema.as_str()) {
            push_tx(&mut ops, format!("DROP SCHEMA IF EXISTS {} CASCADE;", pg_schema(schema)));
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
        pg_schema(&t.schema), qi(&t.name),
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
                pg_schema(&after.schema), qi(&after.name), qi(&col.name), col.pg_type, not_null
            ));
        }
    }
    for col in &before.columns {
        if !after_cols.contains(col.name.as_str()) {
            push_tx(ops, format!(
                "ALTER TABLE {}.{} DROP COLUMN IF EXISTS {};",
                pg_schema(&after.schema), qi(&after.name), qi(&col.name)
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
                        default_pyql: None,
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
            scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE SCHEMA IF NOT EXISTS \"catalog\""), "got:\n{joined}");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS \"catalog\".\"Product\""), "got:\n{joined}");
    }

    #[test]
    fn test_no_ops_when_in_sync() {
        let schema = SchemaDescriptor {
            types: vec![simple_type("default", "Person", "Person")],
            scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false, column_default: Some("uuidv7()".into()) },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false, column_default: None },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![], triggers: vec![],
            }],
            enums: vec![], domains: vec![],
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
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Person".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false, column_default: Some("uuidv7()".into()) },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false, column_default: None },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![], triggers: vec![],
            }],
            enums: vec![], domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
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
            globals: vec![], functions: vec![], aliases: vec![],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE TYPE \"public\".\"Status\" AS ENUM"), "got:\n{joined}");
    }

    #[test]
    fn test_drop_table() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "OldType".into(),
                columns: vec![], foreign_keys: vec![], indexes: vec![], checks: vec![], triggers: vec![],
            }],
            enums: vec![], domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("DROP TABLE IF EXISTS \"public\".\"OldType\" CASCADE"), "got:\n{joined}");
    }

    #[test]
    fn test_index_on_existing_table_is_concurrently() {
        use crate::schema::{VectorIndexDescriptor};
        let mut td = simple_type("default", "Post", "Post");
        td.vector_indexes.push(VectorIndexDescriptor {
            index_name: None,
            pointers: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        // The table already exists in the DB (pre-existing).
        let state = DbState {
            schemas: vec!["default".into()],
            tables: vec![DbTable {
                schema: "default".into(), name: "Post".into(),
                columns: vec![
                    DbColumn { name: "id".into(), pg_type: "uuid".into(), nullable: false, is_generated: false, column_default: Some("uuidv7()".into()) },
                    DbColumn { name: "name".into(), pg_type: "text".into(), nullable: true, is_generated: false, column_default: None },
                ],
                foreign_keys: vec![], indexes: vec![], checks: vec![], triggers: vec![],
            }],
            enums: vec![], domains: vec![],
            ..DbState::default()
        };
        let ops = diff_schema_ops(&schema, &state).unwrap();
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
            pointers: vec!["name".into()],
            model: "test".into(),
            metric: "cosine".into(),
            dimensions: 1536,
        });
        let schema = SchemaDescriptor {
            types: vec![td], scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        // Table does NOT exist in the DB → it's new.
        let ops = diff_schema_ops(&schema, &empty_state()).unwrap();
        let idx_op = ops.iter().find(|op| op.sql.contains("hnsw")).unwrap();
        assert!(!idx_op.non_transactional, "index on new table should be transactional");
        assert!(!idx_op.sql.contains("CONCURRENTLY"), "should NOT use CONCURRENTLY: {}", idx_op.sql);
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
            types: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
            scalars: vec![sequence_scalar("default", "OrderNumber")],
        };
        let ops = diff_schema(&schema, &empty_state()).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("CREATE SEQUENCE IF NOT EXISTS \"public\".\"OrderNumber_seq\""), "got:\n{joined}");
        assert!(joined.contains("CREATE DOMAIN \"public\".\"OrderNumber\" AS int8"), "got:\n{joined}");
        // Sequence must precede domain
        let seq_pos = joined.find("CREATE SEQUENCE").unwrap();
        let dom_pos = joined.find("CREATE DOMAIN").unwrap();
        assert!(seq_pos < dom_pos, "sequence must be created before domain");
    }

    #[test]
    fn test_no_ops_sequence_already_exists() {
        let schema = SchemaDescriptor {
            types: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
            scalars: vec![sequence_scalar("default", "OrderNumber")],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            domains: vec![DbDomain { schema: "default".into(), name: "OrderNumber".into() }],
            sequences: vec![DbSequence { schema: "default".into(), name: "OrderNumber_seq".into() }],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        assert!(ops.is_empty(), "expected no ops when sequence and domain exist, got: {:?}", ops);
    }

    #[test]
    fn test_drop_removed_sequence() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let state = DbState {
            schemas: vec!["default".into()],
            domains: vec![DbDomain { schema: "default".into(), name: "OrderNumber".into() }],
            sequences: vec![DbSequence { schema: "default".into(), name: "OrderNumber_seq".into() }],
            ..DbState::default()
        };
        let ops = diff_schema(&schema, &state).unwrap();
        let joined = ops.join("\n");
        assert!(joined.contains("DROP DOMAIN IF EXISTS \"public\".\"OrderNumber\""), "got:\n{joined}");
        assert!(joined.contains("DROP SEQUENCE IF EXISTS \"public\".\"OrderNumber_seq\""), "got:\n{joined}");
    }
}
