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

//! A complete snapshot of a database's structure, and the DDL that carries
//! one to another.
//!
//! `DbState` (see the parent module) describes a database only as far as
//! comparing it against a compiled `SchemaDescriptor` needs: a view or a
//! function is a hash, a trigger is a name, a foreign key is three columns
//! with no `ON DELETE`. That is enough to answer "does the database already
//! have what the schema asks for" — the question `migration create` asks —
//! because whatever is missing gets re-rendered from the schema.
//!
//! `pylon migration squash` has no schema to render from. It applies a range
//! of migrations to a shadow database and has to express what they did using
//! only what it can read back out of `pg_catalog`, so every object it carries
//! across has to be reconstructable from this snapshot alone. That is what a
//! `DbDump` is: names *and* the DDL, taken from `pg_get_constraintdef`,
//! `pg_get_indexdef`, `pg_get_triggerdef`, `pg_get_viewdef` and
//! `pg_get_functiondef` — the catalog's own rendering, not a second emitter
//! guessing at one.
//!
//! Schema names here are PostgreSQL's own (`public`, not the `default`
//! module `DbState` translates it to): every `pg_get_*def` string is written
//! in those terms, and one snapshot that spells a schema two ways would
//! diff against itself.
//!
//! `diff_dumps` refuses rather than approximating. Anything two snapshots
//! disagree about that cannot be expressed as DDL — an enum that lost a
//! member, a composite type with no attributes — is an error, not a comment
//! in an otherwise plausible-looking migration.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{DiffOp, NEW_TYPE_SUFFIX, push_tx, qi};

// ── The snapshot ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbDump {
    /// User-managed PostgreSQL schema names, `public` included.
    pub schemas: Vec<String>,
    pub extensions: Vec<String>,
    pub enums: Vec<DumpEnum>,
    pub domains: Vec<DumpDomain>,
    pub composites: Vec<DumpComposite>,
    pub sequences: Vec<DumpSequence>,
    pub tables: Vec<DumpTable>,
    pub views: Vec<DumpView>,
    pub functions: Vec<DumpFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpEnum {
    pub schema: String,
    pub name: String,
    /// In `enumsortorder`, which is the order the type declares them in.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpDomain {
    pub schema: String,
    pub name: String,
    /// `format_type(typbasetype, typtypmod)` — the type the domain is over.
    pub base_type: String,
    pub not_null: bool,
    pub default_expr: Option<String>,
    pub checks: Vec<DumpConstraint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpComposite {
    pub schema: String,
    pub name: String,
    /// In attribute order, which is part of the type's identity — a tuple's
    /// members are read by position.
    pub attributes: Vec<DumpAttribute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpAttribute {
    pub name: String,
    pub pg_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpSequence {
    pub schema: String,
    pub name: String,
    pub data_type: String,
    pub start_value: i64,
    pub increment_by: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpTable {
    pub schema: String,
    pub name: String,
    /// In attribute order.
    pub columns: Vec<DumpColumn>,
    pub constraints: Vec<DumpConstraint>,
    pub indexes: Vec<DumpIndex>,
    pub triggers: Vec<DumpTrigger>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpColumn {
    pub name: String,
    /// `format_type` — the same spelling on both sides of a diff, so these
    /// compare as plain strings.
    pub pg_type: String,
    pub nullable: bool,
    pub default_expr: Option<String>,
    /// The expression behind `GENERATED ALWAYS AS (…) STORED`.
    pub generated_expr: Option<String>,
}

/// A table or domain constraint, with the `pg_get_constraintdef` text that
/// recreates it — `CHECK ((…))`, `PRIMARY KEY (…)`, `FOREIGN KEY (…)
/// REFERENCES … ON DELETE CASCADE`, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpConstraint {
    pub name: String,
    pub kind: ConstraintKind,
    pub definition: String,
    /// The index this constraint owns, when it has one. Such an index is
    /// created by the constraint, so emitting it separately would make a
    /// second one.
    pub index: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    ForeignKey,
    Exclude,
}

impl ConstraintKind {
    /// `pg_constraint.contype`.
    pub fn from_contype(contype: &str) -> Option<Self> {
        match contype {
            "p" => Some(Self::PrimaryKey),
            "u" => Some(Self::Unique),
            "c" => Some(Self::Check),
            "f" => Some(Self::ForeignKey),
            "x" => Some(Self::Exclude),
            // 'n' (NOT NULL, as Postgres 18 records it) and 't' (constraint
            // trigger) are carried by the column and the trigger they belong
            // to, not as constraints of their own.
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpIndex {
    pub name: String,
    /// `pg_get_indexdef` — a complete `CREATE INDEX`, operator class and
    /// expression and all.
    pub definition: String,
    /// The constraint that owns this index, if any.
    pub constraint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpTrigger {
    pub name: String,
    /// `pg_get_triggerdef` — a complete `CREATE [CONSTRAINT] TRIGGER`.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpView {
    pub schema: String,
    pub name: String,
    /// `pg_get_viewdef` — the `SELECT` alone, without `CREATE VIEW … AS`.
    pub definition: String,
    /// The relations this view reads, as `(schema, name)`. A view over a
    /// view has to be created second, and nothing about a name says which
    /// way round that is.
    pub depends_on: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpFunction {
    pub schema: String,
    pub name: String,
    /// `pg_get_function_identity_arguments` — what names this overload in a
    /// `DROP FUNCTION`. Empty for a function taking no arguments; a bare
    /// name is ambiguous as soon as two overloads share one.
    pub identity_args: String,
    /// `pg_get_functiondef` — already a `CREATE OR REPLACE FUNCTION`.
    pub definition: String,
}

// ── Keys ──────────────────────────────────────────────────────────────────────

type Key<'a> = (&'a str, &'a str);

fn q(schema: &str, name: &str) -> String {
    format!("{}.{}", qi(schema), qi(name))
}

impl DbDump {
    fn table(&self, schema: &str, name: &str) -> Option<&DumpTable> {
        self.tables.iter().find(|t| t.schema == schema && t.name == name)
    }
}

impl DumpFunction {
    /// What identifies one overload: two functions of the same name taking
    /// different arguments are two objects.
    fn key(&self) -> (&str, &str, &str) {
        (&self.schema, &self.name, &self.identity_args)
    }

    fn signature(&self) -> String {
        format!("{}({})", q(&self.schema, &self.name), self.identity_args)
    }
}

// ── The diff ──────────────────────────────────────────────────────────────────

/// Ordered DDL carrying a database from `before` to `after`, for a migration
/// file body.
///
/// Both snapshots are read out of a live database, so an object is "the
/// same" here when its catalog DDL is byte-for-byte what it was — not when
/// it merely still exists. That makes the comparison symmetric: run this
/// both ways and two empty results mean the two databases hold the same
/// structure, which is how `pylon migration squash` proves its output
/// equivalent to the migrations it replaces.
pub fn diff_dumps(before: &DbDump, after: &DbDump) -> Result<Vec<DiffOp>, String> {
    let mut ops: Vec<DiffOp> = Vec::new();

    // Column work that cannot happen with a dependent view or trigger in
    // place, and so decides what gets taken down first.
    let disturbed = disturbed_tables(before, after);
    let views_down = views_to_take_down(before, after, &disturbed);
    let triggers_down = triggers_to_take_down(before, after, &disturbed);

    // ── Take down what the structural work below cannot happen underneath ──
    for v in views_down.iter().rev() {
        push_tx(&mut ops, format!("DROP VIEW IF EXISTS {};", q(&v.schema, &v.name)));
    }
    for (table, trigger) in &triggers_down {
        push_tx(
            &mut ops,
            format!(
                "DROP TRIGGER IF EXISTS {} ON {};",
                qi(&trigger.name),
                q(&table.schema, &table.name)
            ),
        );
    }

    emit_schemas(before, after, &mut ops);
    emit_extensions(before, after, &mut ops);
    emit_enums(before, after, &mut ops)?;
    emit_domains(before, after, &mut ops);
    emit_composites(before, after, &mut ops)?;
    emit_sequences(before, after, &mut ops);
    emit_tables(before, after, &mut ops)?;
    emit_constraints(before, after, &mut ops);
    emit_indexes(before, after, &mut ops);
    emit_functions(before, after, &mut ops);
    emit_views(before, after, &views_down, &mut ops)?;
    emit_triggers(before, after, &triggers_down, &mut ops);

    // ── Drops, dependents before what they depend on ───────────────────────
    // Tables before functions, not after: a table on its way out still has
    // its triggers until it goes (they are not dropped individually — the
    // `CASCADE` takes them), and `DROP FUNCTION` refuses while a trigger
    // still points at it.
    emit_dropped_triggers(before, after, &mut ops);
    emit_dropped_indexes(before, after, &mut ops);
    emit_dropped_constraints(before, after, &mut ops);
    emit_dropped_tables(before, after, &mut ops);
    emit_dropped_functions(before, after, &mut ops);
    emit_dropped_sequences(before, after, &mut ops);
    emit_dropped_composites(before, after, &mut ops);
    emit_dropped_domains(before, after, &mut ops);
    emit_dropped_enums(before, after, &mut ops);
    emit_dropped_schemas(before, after, &mut ops);

    Ok(ops)
}

// ── Schemas and extensions ────────────────────────────────────────────────────

fn emit_schemas(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let known: HashSet<&str> = before.schemas.iter().map(|s| s.as_str()).collect();
    for schema in &after.schemas {
        // `public` is always there, and a database owner who dropped it did
        // so on purpose.
        if schema == "public" || known.contains(schema.as_str()) {
            continue;
        }
        push_tx(ops, format!("CREATE SCHEMA IF NOT EXISTS {};", qi(schema)));
    }
}

fn emit_dropped_schemas(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<&str> = after.schemas.iter().map(|s| s.as_str()).collect();
    for schema in &before.schemas {
        if schema == "public" || kept.contains(schema.as_str()) {
            continue;
        }
        push_tx(ops, format!("DROP SCHEMA IF EXISTS {} CASCADE;", qi(schema)));
    }
}

/// Extensions are only ever added. Dropping one is not something a schema
/// change implies — another database on the same cluster may well be using
/// it — and `DbDump`'s own symmetry does not need it: an extension on one
/// side and not the other shows up as an addition when the two snapshots are
/// compared the other way round, which is how equivalence is checked.
fn emit_extensions(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let known: HashSet<&str> = before.extensions.iter().map(|s| s.as_str()).collect();
    for ext in &after.extensions {
        if known.contains(ext.as_str()) {
            continue;
        }
        push_tx(ops, format!("CREATE EXTENSION IF NOT EXISTS {};", qi(ext)));
    }
}

// ── Enums ─────────────────────────────────────────────────────────────────────

fn emit_enums(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) -> Result<(), String> {
    let known: HashMap<Key<'_>, &DumpEnum> = before
        .enums
        .iter()
        .map(|e| ((e.schema.as_str(), e.name.as_str()), e))
        .collect();

    for e in &after.enums {
        let qname = q(&e.schema, &e.name);
        match known.get(&(e.schema.as_str(), e.name.as_str())) {
            None => {
                let members: Vec<String> = e.members.iter().map(|m| sql_literal(m)).collect();
                push_tx(
                    ops,
                    format!(
                        "DO $$ BEGIN CREATE TYPE {qname} AS ENUM ({}); \
                         EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
                        members.join(", ")
                    ),
                );
            }
            Some(existing) => {
                // A member that is gone cannot be taken out of an enum type
                // at all: the type has to be rebuilt, with every column
                // holding it converted across. The migrations being squashed
                // did that somehow; nothing here can tell how.
                if let Some(lost) = existing.members.iter().find(|m| !e.members.contains(m)) {
                    return Err(format!(
                        "scalar type {}.{} no longer has member '{}'. PostgreSQL cannot remove an \
                         enum member, so this range cannot be expressed as a single migration — \
                         keep the migrations that rebuild the type unsquashed.",
                        e.schema, e.name, lost
                    ));
                }
                for member in &e.members {
                    if existing.members.contains(member) {
                        continue;
                    }
                    push_tx(
                        ops,
                        format!("ALTER TYPE {qname} ADD VALUE IF NOT EXISTS {};", sql_literal(member)),
                    );
                }
            }
        }
    }
    Ok(())
}

fn emit_dropped_enums(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<Key<'_>> = after
        .enums
        .iter()
        .map(|e| (e.schema.as_str(), e.name.as_str()))
        .collect();
    for e in &before.enums {
        if kept.contains(&(e.schema.as_str(), e.name.as_str())) {
            continue;
        }
        push_tx(ops, format!("DROP TYPE IF EXISTS {} CASCADE;", q(&e.schema, &e.name)));
    }
}

// ── Domains ───────────────────────────────────────────────────────────────────

fn emit_domains(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let known: HashMap<Key<'_>, &DumpDomain> = before
        .domains
        .iter()
        .map(|d| ((d.schema.as_str(), d.name.as_str()), d))
        .collect();

    for d in &after.domains {
        let qname = q(&d.schema, &d.name);
        match known.get(&(d.schema.as_str(), d.name.as_str())) {
            None => {
                let mut clauses = String::new();
                if let Some(default) = &d.default_expr {
                    clauses.push_str(&format!(" DEFAULT {default}"));
                }
                if d.not_null {
                    clauses.push_str(" NOT NULL");
                }
                for check in &d.checks {
                    clauses.push_str(&format!(" CONSTRAINT {} {}", qi(&check.name), check.definition));
                }
                push_tx(
                    ops,
                    format!(
                        "DO $do$ BEGIN CREATE DOMAIN {qname} AS {}{clauses}; \
                         EXCEPTION WHEN duplicate_object THEN NULL; END $do$;",
                        d.base_type
                    ),
                );
            }
            Some(existing) => {
                // The base type is the one part of a domain that cannot be
                // altered. Two snapshots disagreeing on it means the domain
                // was dropped and remade, which `emit_dropped_domains` does
                // not see (the name is still there) — so say it here.
                if existing.base_type != d.base_type {
                    push_tx(ops, format!("DROP DOMAIN IF EXISTS {qname} CASCADE;"));
                    let mut clauses = String::new();
                    if let Some(default) = &d.default_expr {
                        clauses.push_str(&format!(" DEFAULT {default}"));
                    }
                    if d.not_null {
                        clauses.push_str(" NOT NULL");
                    }
                    for check in &d.checks {
                        clauses.push_str(&format!(" CONSTRAINT {} {}", qi(&check.name), check.definition));
                    }
                    push_tx(ops, format!("CREATE DOMAIN {qname} AS {}{clauses};", d.base_type));
                    continue;
                }
                if existing.default_expr != d.default_expr {
                    match &d.default_expr {
                        Some(default) => push_tx(ops, format!("ALTER DOMAIN {qname} SET DEFAULT {default};")),
                        None => push_tx(ops, format!("ALTER DOMAIN {qname} DROP DEFAULT;")),
                    }
                }
                if existing.not_null != d.not_null {
                    let verb = if d.not_null { "SET" } else { "DROP" };
                    push_tx(ops, format!("ALTER DOMAIN {qname} {verb} NOT NULL;"));
                }
                for check in &d.checks {
                    let same = existing
                        .checks
                        .iter()
                        .any(|c| c.name == check.name && c.definition == check.definition);
                    if same {
                        continue;
                    }
                    if existing.checks.iter().any(|c| c.name == check.name) {
                        push_tx(
                            ops,
                            format!("ALTER DOMAIN {qname} DROP CONSTRAINT {};", qi(&check.name)),
                        );
                    }
                    push_tx(
                        ops,
                        format!(
                            "ALTER DOMAIN {qname} ADD CONSTRAINT {} {};",
                            qi(&check.name),
                            check.definition
                        ),
                    );
                }
                for check in &existing.checks {
                    if !d.checks.iter().any(|c| c.name == check.name) {
                        push_tx(
                            ops,
                            format!("ALTER DOMAIN {qname} DROP CONSTRAINT IF EXISTS {};", qi(&check.name)),
                        );
                    }
                }
            }
        }
    }
}

fn emit_dropped_domains(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<Key<'_>> = after
        .domains
        .iter()
        .map(|d| (d.schema.as_str(), d.name.as_str()))
        .collect();
    for d in &before.domains {
        if kept.contains(&(d.schema.as_str(), d.name.as_str())) {
            continue;
        }
        push_tx(ops, format!("DROP DOMAIN IF EXISTS {};", q(&d.schema, &d.name)));
    }
}

// ── Composite types ───────────────────────────────────────────────────────────

fn create_composite_sql(c: &DumpComposite, as_name: &str) -> String {
    let attributes: Vec<String> = c
        .attributes
        .iter()
        .map(|a| format!("{} {}", qi(&a.name), a.pg_type))
        .collect();
    format!(
        "DO $$ BEGIN CREATE TYPE {as_name} AS ({}); EXCEPTION WHEN duplicate_object THEN NULL; END $$;",
        attributes.join(", ")
    )
}

fn replacement_ref(c: &DumpComposite) -> String {
    q(&c.schema, &format!("{}{}", c.name, NEW_TYPE_SUFFIX))
}

/// Every column holding `c`, as `(schema, table, column, is_array)`.
/// `format_type` drops the schema qualifier for a type that is on the
/// search path, so these match on the type's own name.
fn columns_of_composite(dump: &DbDump, c: &DumpComposite) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    for t in &dump.tables {
        for col in &t.columns {
            let is_array = col.pg_type.ends_with("[]");
            let base = col.pg_type.strip_suffix("[]").unwrap_or(&col.pg_type);
            if base.rsplit('.').next().unwrap_or(base).trim_matches('"') == c.name {
                out.push((t.schema.clone(), t.name.clone(), col.name.clone(), is_array));
            }
        }
    }
    out
}

fn emit_composites(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) -> Result<(), String> {
    let known: HashMap<Key<'_>, &DumpComposite> = before
        .composites
        .iter()
        .map(|c| ((c.schema.as_str(), c.name.as_str()), c))
        .collect();

    let mut replaced: Vec<&DumpComposite> = Vec::new();
    for c in &after.composites {
        if c.attributes.is_empty() {
            return Err(format!(
                "composite type {}.{} has no attributes to recreate it from.",
                c.schema, c.name
            ));
        }
        match known.get(&(c.schema.as_str(), c.name.as_str())) {
            None => push_tx(ops, create_composite_sql(c, &q(&c.schema, &c.name))),
            Some(existing) if existing.attributes != c.attributes => replaced.push(c),
            Some(_) => {}
        }
    }

    // A type in use cannot be altered into shape, so each replacement is
    // built alongside the type it replaces and every column moved over
    // before any of the old types go — the same sequence the
    // schema-to-database diff uses, and for the same reason: every
    // replacement before every column, every column before every drop.
    if replaced.is_empty() {
        return Ok(());
    }
    for c in &replaced {
        push_tx(ops, create_composite_sql(c, &replacement_ref(c)));
    }
    for c in &replaced {
        for (schema, table, column, is_array) in columns_of_composite(after, c) {
            let new_type = match is_array {
                true => format!("{}[]", replacement_ref(c)),
                false => replacement_ref(c),
            };
            push_tx(
                ops,
                format!(
                    "ALTER TABLE {} ALTER COLUMN {} TYPE {new_type} USING {};",
                    q(&schema, &table),
                    qi(&column),
                    super::tuple_conversion_expr(&column, &new_type),
                ),
            );
        }
    }
    for c in replaced.iter().rev() {
        push_tx(ops, format!("DROP TYPE {};", q(&c.schema, &c.name)));
    }
    for c in &replaced {
        push_tx(
            ops,
            format!("ALTER TYPE {} RENAME TO {};", replacement_ref(c), qi(&c.name)),
        );
    }
    Ok(())
}

fn emit_dropped_composites(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<Key<'_>> = after
        .composites
        .iter()
        .map(|c| (c.schema.as_str(), c.name.as_str()))
        .collect();
    for c in &before.composites {
        if kept.contains(&(c.schema.as_str(), c.name.as_str())) {
            continue;
        }
        push_tx(ops, format!("DROP TYPE IF EXISTS {};", q(&c.schema, &c.name)));
    }
}

// ── Sequences ─────────────────────────────────────────────────────────────────

fn emit_sequences(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let known: HashMap<Key<'_>, &DumpSequence> = before
        .sequences
        .iter()
        .map(|s| ((s.schema.as_str(), s.name.as_str()), s))
        .collect();

    for s in &after.sequences {
        let qname = q(&s.schema, &s.name);
        let params = format!(
            "AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} {}",
            s.data_type,
            s.increment_by,
            s.min_value,
            s.max_value,
            s.start_value,
            if s.cycle { "CYCLE" } else { "NO CYCLE" },
        );
        match known.get(&(s.schema.as_str(), s.name.as_str())) {
            None => push_tx(ops, format!("CREATE SEQUENCE IF NOT EXISTS {qname} {params};")),
            Some(existing) if **existing != *s => push_tx(ops, format!("ALTER SEQUENCE {qname} {params};")),
            Some(_) => {}
        }
    }
}

fn emit_dropped_sequences(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<Key<'_>> = after
        .sequences
        .iter()
        .map(|s| (s.schema.as_str(), s.name.as_str()))
        .collect();
    for s in &before.sequences {
        if kept.contains(&(s.schema.as_str(), s.name.as_str())) {
            continue;
        }
        push_tx(ops, format!("DROP SEQUENCE IF EXISTS {};", q(&s.schema, &s.name)));
    }
}

// ── Tables and columns ────────────────────────────────────────────────────────

fn column_clause(col: &DumpColumn) -> String {
    if let Some(expr) = &col.generated_expr {
        return format!("{} {} GENERATED ALWAYS AS ({expr}) STORED", qi(&col.name), col.pg_type);
    }
    let mut clause = format!("{} {}", qi(&col.name), col.pg_type);
    if let Some(default) = &col.default_expr {
        clause.push_str(&format!(" DEFAULT {default}"));
    }
    if !col.nullable {
        clause.push_str(" NOT NULL");
    }
    clause
}

fn emit_tables(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) -> Result<(), String> {
    for t in &after.tables {
        let qname = q(&t.schema, &t.name);
        let Some(existing) = before.table(&t.schema, &t.name) else {
            if t.columns.is_empty() {
                return Err(format!(
                    "table {}.{} has no columns to recreate it from.",
                    t.schema, t.name
                ));
            }
            let lines: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_clause(c))).collect();
            push_tx(
                ops,
                format!("CREATE TABLE IF NOT EXISTS {qname} (\n{}\n);", lines.join(",\n")),
            );
            continue;
        };

        let known: HashMap<&str, &DumpColumn> = existing.columns.iter().map(|c| (c.name.as_str(), c)).collect();
        for col in &t.columns {
            let Some(was) = known.get(col.name.as_str()) else {
                push_tx(
                    ops,
                    format!("ALTER TABLE {qname} ADD COLUMN IF NOT EXISTS {};", column_clause(col)),
                );
                continue;
            };
            // Whether a column is generated at all is not alterable. The
            // value is derived from the row, so taking the column down and
            // putting it back loses nothing.
            if was.generated_expr.is_some() != col.generated_expr.is_some() {
                push_tx(ops, format!("ALTER TABLE {qname} DROP COLUMN {};", qi(&col.name)));
                push_tx(ops, format!("ALTER TABLE {qname} ADD COLUMN {};", column_clause(col)));
                continue;
            }
            if was.pg_type != col.pg_type {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {qname} ALTER COLUMN {} TYPE {};",
                        qi(&col.name),
                        col.pg_type
                    ),
                );
            }
            if let (Some(before_expr), Some(after_expr)) = (&was.generated_expr, &col.generated_expr) {
                if before_expr != after_expr {
                    push_tx(
                        ops,
                        format!(
                            "ALTER TABLE {qname} ALTER COLUMN {} SET EXPRESSION AS ({after_expr});",
                            qi(&col.name)
                        ),
                    );
                }
                continue; // A generated column has no default and no NOT NULL of its own.
            }
            if was.default_expr != col.default_expr {
                match &col.default_expr {
                    Some(default) => push_tx(
                        ops,
                        format!(
                            "ALTER TABLE {qname} ALTER COLUMN {} SET DEFAULT {default};",
                            qi(&col.name)
                        ),
                    ),
                    None => push_tx(
                        ops,
                        format!("ALTER TABLE {qname} ALTER COLUMN {} DROP DEFAULT;", qi(&col.name)),
                    ),
                }
            }
            if was.nullable != col.nullable {
                let verb = if col.nullable { "DROP" } else { "SET" };
                push_tx(
                    ops,
                    format!("ALTER TABLE {qname} ALTER COLUMN {} {verb} NOT NULL;", qi(&col.name)),
                );
            }
        }

        let kept: HashSet<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        for col in &existing.columns {
            if !kept.contains(col.name.as_str()) {
                push_tx(
                    ops,
                    format!("ALTER TABLE {qname} DROP COLUMN IF EXISTS {};", qi(&col.name)),
                );
            }
        }
    }
    Ok(())
}

fn emit_dropped_tables(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for t in &before.tables {
        if after.table(&t.schema, &t.name).is_none() {
            push_tx(ops, format!("DROP TABLE IF EXISTS {} CASCADE;", q(&t.schema, &t.name)));
        }
    }
}

/// Tables whose column work a dependent view or trigger would block: a
/// column being dropped, retyped, or rebuilt. Adding one disturbs nothing.
fn disturbed_tables(before: &DbDump, after: &DbDump) -> HashSet<(String, String)> {
    let mut out = HashSet::new();
    for t in &after.tables {
        let Some(existing) = before.table(&t.schema, &t.name) else {
            continue;
        };
        let after_cols: HashMap<&str, &DumpColumn> = t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
        let disturbed = existing
            .columns
            .iter()
            .any(|was| match after_cols.get(was.name.as_str()) {
                None => true,
                Some(now) => was.pg_type != now.pg_type || was.generated_expr.is_some() != now.generated_expr.is_some(),
            });
        if disturbed {
            out.insert((t.schema.clone(), t.name.clone()));
        }
    }
    // A table on its way out takes its own views and triggers with it
    // (`CASCADE`), so it is not one of these.
    out
}

// ── Table constraints ─────────────────────────────────────────────────────────

/// A foreign key can only be added once what it points at has its primary
/// key, so every key and unique constraint in the whole dump goes first.
fn constraint_phase(kind: ConstraintKind) -> u8 {
    match kind {
        ConstraintKind::PrimaryKey | ConstraintKind::Unique | ConstraintKind::Exclude => 0,
        ConstraintKind::Check => 1,
        ConstraintKind::ForeignKey => 2,
    }
}

fn emit_constraints(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for phase in 0..=2 {
        for t in &after.tables {
            let qname = q(&t.schema, &t.name);
            let existing = before.table(&t.schema, &t.name);
            for c in t.constraints.iter().filter(|c| constraint_phase(c.kind) == phase) {
                let was = existing.and_then(|e| e.constraints.iter().find(|x| x.name == c.name));
                match was {
                    Some(was) if was.definition == c.definition => continue,
                    // Same name, different rule: a constraint cannot be
                    // altered, so the old one goes first.
                    Some(_) => push_tx(ops, format!("ALTER TABLE {qname} DROP CONSTRAINT {};", qi(&c.name))),
                    None => {}
                }
                push_tx(
                    ops,
                    format!("ALTER TABLE {qname} ADD CONSTRAINT {} {};", qi(&c.name), c.definition),
                );
            }
        }
    }
}

fn emit_dropped_constraints(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for t in &before.tables {
        // A dropped table takes its constraints with it.
        let Some(now) = after.table(&t.schema, &t.name) else {
            continue;
        };
        for c in &t.constraints {
            if !now.constraints.iter().any(|x| x.name == c.name) {
                push_tx(
                    ops,
                    format!(
                        "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {};",
                        q(&t.schema, &t.name),
                        qi(&c.name)
                    ),
                );
            }
        }
    }
}

// ── Indexes ───────────────────────────────────────────────────────────────────

/// `pg_get_indexdef` renders `CREATE [UNIQUE] INDEX <name> ON …`. An index
/// on a table that already held rows before this migration is built
/// `CONCURRENTLY` — the same choice `migration create` makes — and every
/// index is created `IF NOT EXISTS`, both of which go in the one place the
/// grammar allows: right after `INDEX`.
fn index_create_sql(definition: &str, concurrently: bool) -> Option<String> {
    let (head, rest) = match definition.strip_prefix("CREATE UNIQUE INDEX ") {
        Some(rest) => ("CREATE UNIQUE INDEX ", rest),
        None => ("CREATE INDEX ", definition.strip_prefix("CREATE INDEX ")?),
    };
    let modifiers = if concurrently {
        "CONCURRENTLY IF NOT EXISTS "
    } else {
        "IF NOT EXISTS "
    };
    Some(format!("{head}{modifiers}{rest};"))
}

fn emit_indexes(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for t in &after.tables {
        let existing = before.table(&t.schema, &t.name);
        for idx in &t.indexes {
            // An index a constraint owns is created by that constraint.
            if idx.constraint.is_some() {
                continue;
            }
            let was = existing.and_then(|e| e.indexes.iter().find(|x| x.name == idx.name));
            match was {
                Some(was) if was.definition == idx.definition => continue,
                Some(_) => push_tx(ops, format!("DROP INDEX IF EXISTS {};", q(&t.schema, &idx.name))),
                None => {}
            }
            // Only a table that was already there can hold rows this would
            // lock out; a table this same migration creates cannot.
            let concurrently = existing.is_some();
            match index_create_sql(&idx.definition, concurrently) {
                Some(sql) => ops.push(DiffOp {
                    sql,
                    non_transactional: concurrently,
                }),
                // Not something `pg_get_indexdef` produces, but a dump is
                // read from a live database and this would rather emit the
                // catalog's own text than silently leave the index out.
                None => push_tx(ops, format!("{};", idx.definition.trim_end_matches(';'))),
            }
        }
    }
}

fn emit_dropped_indexes(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for t in &before.tables {
        let Some(now) = after.table(&t.schema, &t.name) else {
            continue;
        };
        for idx in &t.indexes {
            if idx.constraint.is_some() {
                continue;
            }
            if !now.indexes.iter().any(|x| x.name == idx.name) {
                push_tx(ops, format!("DROP INDEX IF EXISTS {};", q(&t.schema, &idx.name)));
            }
        }
    }
}

// ── Functions ─────────────────────────────────────────────────────────────────

fn emit_functions(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let known: HashMap<(&str, &str, &str), &DumpFunction> = before.functions.iter().map(|f| (f.key(), f)).collect();
    for f in &after.functions {
        if known.get(&f.key()).map(|was| was.definition == f.definition) == Some(true) {
            continue;
        }
        // `pg_get_functiondef` is already a `CREATE OR REPLACE`.
        push_tx(ops, format!("{};", f.definition.trim_end().trim_end_matches(';')));
    }
}

fn emit_dropped_functions(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    let kept: HashSet<(&str, &str, &str)> = after.functions.iter().map(|f| f.key()).collect();
    for f in &before.functions {
        if !kept.contains(&f.key()) {
            push_tx(ops, format!("DROP FUNCTION IF EXISTS {};", f.signature()));
        }
    }
}

// ── Views ─────────────────────────────────────────────────────────────────────

/// Views that have to come down before the column work below them, in
/// creation order — callers take them down in reverse.
///
/// A view whose definition changed is one of these, and so is every view
/// reading a table whose columns are being dropped or retyped, however many
/// views deep. A view that is simply gone is handled by the caller.
fn views_to_take_down<'a>(
    before: &'a DbDump,
    after: &'a DbDump,
    disturbed: &HashSet<(String, String)>,
) -> Vec<&'a DumpView> {
    let after_views: HashMap<Key<'_>, &DumpView> = after
        .views
        .iter()
        .map(|v| ((v.schema.as_str(), v.name.as_str()), v))
        .collect();

    let mut doomed: HashSet<(String, String)> = HashSet::new();
    for v in &before.views {
        let key = (v.schema.clone(), v.name.clone());
        let changed = match after_views.get(&(v.schema.as_str(), v.name.as_str())) {
            None => true, // gone in `after` — down it goes, and stays down
            Some(now) => now.definition != v.definition,
        };
        if changed || v.depends_on.iter().any(|d| disturbed.contains(d)) {
            doomed.insert(key);
        }
    }
    // A view over a doomed view is doomed too — `DROP VIEW` without
    // `CASCADE` refuses while a dependent is still there.
    loop {
        let grown: Vec<(String, String)> = before
            .views
            .iter()
            .filter(|v| !doomed.contains(&(v.schema.clone(), v.name.clone())))
            .filter(|v| v.depends_on.iter().any(|d| doomed.contains(d)))
            .map(|v| (v.schema.clone(), v.name.clone()))
            .collect();
        if grown.is_empty() {
            break;
        }
        doomed.extend(grown);
    }

    let mut out: Vec<&DumpView> = before
        .views
        .iter()
        .filter(|v| doomed.contains(&(v.schema.clone(), v.name.clone())))
        .collect();
    out.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    sort_views_by_dependency(&mut out);
    out
}

/// Depended-on views before the ones reading them. Stable for views with no
/// relation to each other, and tolerant of a cycle (which Postgres cannot
/// hold anyway) rather than looping forever.
fn sort_views_by_dependency(views: &mut Vec<&DumpView>) {
    let mut placed: HashSet<(String, String)> = HashSet::new();
    let mut out: Vec<&DumpView> = Vec::with_capacity(views.len());
    let mut pending: Vec<&DumpView> = std::mem::take(views);
    while !pending.is_empty() {
        let in_set: HashSet<(String, String)> = pending.iter().map(|v| (v.schema.clone(), v.name.clone())).collect();
        let mut progressed = false;
        let mut next: Vec<&DumpView> = Vec::new();
        for v in pending {
            let waiting = v
                .depends_on
                .iter()
                .any(|d| in_set.contains(d) && !placed.contains(d) && *d != (v.schema.clone(), v.name.clone()));
            if waiting {
                next.push(v);
            } else {
                placed.insert((v.schema.clone(), v.name.clone()));
                out.push(v);
                progressed = true;
            }
        }
        if !progressed {
            out.extend(next);
            break;
        }
        pending = next;
    }
    *views = out;
}

fn emit_views(before: &DbDump, after: &DbDump, taken_down: &[&DumpView], ops: &mut Vec<DiffOp>) -> Result<(), String> {
    let down: HashSet<(&str, &str)> = taken_down
        .iter()
        .map(|v| (v.schema.as_str(), v.name.as_str()))
        .collect();
    let known: HashMap<Key<'_>, &DumpView> = before
        .views
        .iter()
        .map(|v| ((v.schema.as_str(), v.name.as_str()), v))
        .collect();

    let mut to_create: Vec<&DumpView> = Vec::new();
    for v in &after.views {
        if v.definition.trim().is_empty() {
            return Err(format!(
                "view {}.{} has no definition to recreate it from.",
                v.schema, v.name
            ));
        }
        let key = (v.schema.as_str(), v.name.as_str());
        let unchanged = known.get(&key).map(|was| was.definition == v.definition) == Some(true);
        if unchanged && !down.contains(&key) {
            continue;
        }
        to_create.push(v);
    }
    sort_views_by_dependency(&mut to_create);

    for v in to_create {
        let qname = q(&v.schema, &v.name);
        // `CREATE OR REPLACE VIEW` refuses to change a view's column list,
        // which is exactly what an interface gaining or losing a pointer
        // does — so a rebuild drops first. Already down if it was taken
        // down above, and `IF EXISTS` makes saying so twice harmless. A view
        // this migration is the first to create needs none of that.
        if known.contains_key(&(v.schema.as_str(), v.name.as_str())) {
            push_tx(ops, format!("DROP VIEW IF EXISTS {qname};"));
        }
        push_tx(
            ops,
            format!("CREATE VIEW {qname} AS\n{};", v.definition.trim().trim_end_matches(';')),
        );
    }
    Ok(())
}

// ── Triggers ──────────────────────────────────────────────────────────────────

/// Triggers that have to come down before the column work below them: one
/// whose definition changed, and one on a table whose columns are being
/// dropped or retyped (a trigger declared `UPDATE OF <column>` depends on
/// that column, and the column cannot go while it is there).
fn triggers_to_take_down<'a>(
    before: &'a DbDump,
    after: &'a DbDump,
    disturbed: &HashSet<(String, String)>,
) -> Vec<(&'a DumpTable, &'a DumpTrigger)> {
    let mut out = Vec::new();
    for t in &before.tables {
        // A table being dropped outright takes its triggers with it.
        let Some(now) = after.table(&t.schema, &t.name) else {
            continue;
        };
        let table_disturbed = disturbed.contains(&(t.schema.clone(), t.name.clone()));
        for trigger in &t.triggers {
            let changed = match now.triggers.iter().find(|x| x.name == trigger.name) {
                None => continue, // dropped outright — `emit_dropped_triggers` has it
                Some(now) => now.definition != trigger.definition,
            };
            if changed || table_disturbed {
                out.push((t, trigger));
            }
        }
    }
    out
}

fn emit_triggers(before: &DbDump, after: &DbDump, taken_down: &[(&DumpTable, &DumpTrigger)], ops: &mut Vec<DiffOp>) {
    let down: HashSet<(&str, &str, &str)> = taken_down
        .iter()
        .map(|(t, trigger)| (t.schema.as_str(), t.name.as_str(), trigger.name.as_str()))
        .collect();

    for t in &after.tables {
        let existing = before.table(&t.schema, &t.name);
        for trigger in &t.triggers {
            let was = existing.and_then(|e| e.triggers.iter().find(|x| x.name == trigger.name));
            let key = (t.schema.as_str(), t.name.as_str(), trigger.name.as_str());
            if was.map(|w| w.definition == trigger.definition) == Some(true) && !down.contains(&key) {
                continue;
            }
            // There is no `CREATE OR REPLACE TRIGGER` that can change a
            // trigger's timing or its `UPDATE OF` columns, so a rebuild
            // drops first — harmless for one that is already down, and not
            // needed at all on a table this migration creates.
            if existing.is_some() {
                push_tx(
                    ops,
                    format!(
                        "DROP TRIGGER IF EXISTS {} ON {};",
                        qi(&trigger.name),
                        q(&t.schema, &t.name)
                    ),
                );
            }
            push_tx(ops, format!("{};", trigger.definition.trim_end().trim_end_matches(';')));
        }
    }
}

fn emit_dropped_triggers(before: &DbDump, after: &DbDump, ops: &mut Vec<DiffOp>) {
    for t in &before.tables {
        let Some(now) = after.table(&t.schema, &t.name) else {
            continue;
        };
        for trigger in &t.triggers {
            if !now.triggers.iter().any(|x| x.name == trigger.name) {
                push_tx(
                    ops,
                    format!(
                        "DROP TRIGGER IF EXISTS {} ON {};",
                        qi(&trigger.name),
                        q(&t.schema, &t.name)
                    ),
                );
            }
        }
    }
}

// ── Shared ────────────────────────────────────────────────────────────────────

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(schema: &str, name: &str, columns: Vec<DumpColumn>) -> DumpTable {
        DumpTable {
            schema: schema.into(),
            name: name.into(),
            columns,
            constraints: vec![],
            indexes: vec![],
            triggers: vec![],
        }
    }

    fn column(name: &str, pg_type: &str) -> DumpColumn {
        DumpColumn {
            name: name.into(),
            pg_type: pg_type.into(),
            nullable: true,
            default_expr: None,
            generated_expr: None,
        }
    }

    fn sql(ops: &[DiffOp]) -> String {
        ops.iter().map(|o| o.sql.as_str()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn an_unchanged_dump_diffs_to_nothing() {
        // The property the squash command's own verification rests on: two
        // databases holding the same structure produce no DDL in either
        // direction, so anything at all coming back means they differ.
        let mut dump = DbDump {
            schemas: vec!["public".into(), "shop".into()],
            extensions: vec!["vector".into()],
            ..Default::default()
        };
        dump.enums.push(DumpEnum {
            schema: "public".into(),
            name: "Status".into(),
            members: vec!["draft".into(), "sent".into()],
        });
        dump.tables.push(DumpTable {
            constraints: vec![DumpConstraint {
                name: "Order_pkey".into(),
                kind: ConstraintKind::PrimaryKey,
                definition: "PRIMARY KEY (id)".into(),
                index: Some("Order_pkey".into()),
            }],
            indexes: vec![DumpIndex {
                name: "Order_pkey".into(),
                definition: "CREATE UNIQUE INDEX \"Order_pkey\" ON public.\"Order\" USING btree (id)".into(),
                constraint: Some("Order_pkey".into()),
            }],
            triggers: vec![DumpTrigger {
                name: "_rw_Order_total".into(),
                definition: "CREATE TRIGGER _rw_Order_total BEFORE UPDATE OF total ON public.\"Order\" \
                             FOR EACH ROW EXECUTE FUNCTION public.\"_rw_Order_total\"()"
                    .into(),
            }],
            ..table("public", "Order", vec![column("id", "uuid")])
        });
        dump.views.push(DumpView {
            schema: "public".into(),
            name: "Sellable".into(),
            definition: "SELECT id FROM \"Order\"".into(),
            depends_on: vec![("public".into(), "Order".into())],
        });
        dump.functions.push(DumpFunction {
            schema: "public".into(),
            name: "_rw_Order_total".into(),
            identity_args: String::new(),
            definition: "CREATE OR REPLACE FUNCTION public.\"_rw_Order_total\"() RETURNS trigger \
                         LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"
                .into(),
        });

        assert!(diff_dumps(&dump, &dump).unwrap().is_empty());
    }

    #[test]
    fn a_new_table_brings_its_constraints_indexes_triggers_and_views() {
        // Bug the squash command shipped with: `diff_states` emitted tables
        // and indexes and silently left out every trigger, function and
        // view, so a squashed migration built a schema with no cache
        // invalidation, no cascade deletes and no interface views.
        let before = DbDump {
            schemas: vec!["public".into()],
            ..Default::default()
        };
        let after = DbDump {
            schemas: vec!["public".into()],
            tables: vec![DumpTable {
                constraints: vec![
                    DumpConstraint {
                        name: "Person_pkey".into(),
                        kind: ConstraintKind::PrimaryKey,
                        definition: "PRIMARY KEY (id)".into(),
                        index: Some("Person_pkey".into()),
                    },
                    DumpConstraint {
                        name: "Person_team_id_fkey".into(),
                        kind: ConstraintKind::ForeignKey,
                        definition: "FOREIGN KEY (team_id) REFERENCES public.\"Team\"(id) ON DELETE CASCADE".into(),
                        index: None,
                    },
                    DumpConstraint {
                        name: "Person_name_ab12cd34_check".into(),
                        kind: ConstraintKind::Check,
                        definition: "CHECK ((char_length(name) <= 60))".into(),
                        index: None,
                    },
                ],
                indexes: vec![DumpIndex {
                    name: "Person_name_idx".into(),
                    definition: "CREATE INDEX \"Person_name_idx\" ON public.\"Person\" USING btree (name)".into(),
                    constraint: None,
                }],
                triggers: vec![DumpTrigger {
                    name: "pylon_cache_invalidate".into(),
                    definition: "CREATE TRIGGER pylon_cache_invalidate AFTER INSERT ON public.\"Person\" \
                                 FOR EACH ROW EXECUTE FUNCTION _pylon.cache_invalidate()"
                        .into(),
                }],
                ..table(
                    "public",
                    "Person",
                    vec![column("id", "uuid"), column("name", "text"), column("team_id", "uuid")],
                )
            }],
            views: vec![DumpView {
                schema: "public".into(),
                name: "Actor".into(),
                definition: "SELECT id, name FROM \"Person\"".into(),
                depends_on: vec![("public".into(), "Person".into())],
            }],
            functions: vec![DumpFunction {
                schema: "public".into(),
                name: "adults".into(),
                identity_args: String::new(),
                definition: "CREATE OR REPLACE FUNCTION public.adults() RETURNS SETOF uuid LANGUAGE sql \
                             AS $$ SELECT id FROM \"Person\" $$"
                    .into(),
            }],
            ..Default::default()
        };

        let joined = sql(&diff_dumps(&before, &after).unwrap());
        assert!(
            joined.contains("CREATE TABLE IF NOT EXISTS \"public\".\"Person\""),
            "{joined}"
        );
        assert!(
            joined.contains("ADD CONSTRAINT \"Person_pkey\" PRIMARY KEY (id)"),
            "{joined}"
        );
        assert!(
            joined.contains("ON DELETE CASCADE"),
            "a foreign key keeps its delete policy: {joined}"
        );
        assert!(joined.contains("CHECK ((char_length(name) <= 60))"), "{joined}");
        assert!(
            joined.contains("CREATE INDEX IF NOT EXISTS \"Person_name_idx\""),
            "{joined}"
        );
        assert!(joined.contains("CREATE TRIGGER pylon_cache_invalidate"), "{joined}");
        assert!(
            joined.contains("CREATE OR REPLACE FUNCTION public.adults()"),
            "{joined}"
        );
        assert!(joined.contains("CREATE VIEW \"public\".\"Actor\" AS"), "{joined}");
        // Its own index comes with the primary key, not a second time.
        assert!(
            !joined.contains("CREATE UNIQUE INDEX IF NOT EXISTS \"Person_pkey\""),
            "{joined}"
        );
    }

    #[test]
    fn a_new_tables_index_is_not_built_concurrently() {
        // Nothing can be querying a table this same migration creates, and a
        // `CONCURRENTLY` build would force a step boundary for nothing.
        let after = DbDump {
            schemas: vec!["public".into()],
            tables: vec![DumpTable {
                indexes: vec![DumpIndex {
                    name: "Person_name_idx".into(),
                    definition: "CREATE INDEX \"Person_name_idx\" ON public.\"Person\" USING btree (name)".into(),
                    constraint: None,
                }],
                ..table("public", "Person", vec![column("id", "uuid")])
            }],
            ..Default::default()
        };
        let ops = diff_dumps(&DbDump::default(), &after).unwrap();
        let index_op = ops.iter().find(|o| o.sql.contains("Person_name_idx")).unwrap();
        assert!(!index_op.sql.contains("CONCURRENTLY"), "{}", index_op.sql);
        assert!(!index_op.non_transactional);
    }

    #[test]
    fn an_index_on_a_pre_existing_table_is_built_concurrently() {
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![table("public", "Person", vec![column("id", "uuid")])],
            ..Default::default()
        };
        let mut after = before.clone();
        after.tables[0].indexes.push(DumpIndex {
            name: "Person_id_idx".into(),
            definition: "CREATE INDEX \"Person_id_idx\" ON public.\"Person\" USING btree (id)".into(),
            constraint: None,
        });
        let ops = diff_dumps(&before, &after).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].sql,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS \"Person_id_idx\" ON public.\"Person\" USING btree (id);"
        );
        assert!(ops[0].non_transactional);
    }

    #[test]
    fn a_changed_view_comes_down_before_the_column_it_reads_changes() {
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![table(
                "public",
                "Person",
                vec![column("id", "uuid"), column("nickname", "text")],
            )],
            views: vec![DumpView {
                schema: "public".into(),
                name: "Actor".into(),
                definition: "SELECT id, nickname FROM \"Person\"".into(),
                depends_on: vec![("public".into(), "Person".into())],
            }],
            ..Default::default()
        };
        let mut after = before.clone();
        after.tables[0].columns.pop();
        after.views[0].definition = "SELECT id FROM \"Person\"".into();

        let ops = diff_dumps(&before, &after).unwrap();
        let joined = sql(&ops);
        let drop_view = joined.find("DROP VIEW IF EXISTS \"public\".\"Actor\"").unwrap();
        let drop_col = joined.find("DROP COLUMN IF EXISTS \"nickname\"").unwrap();
        let create_view = joined.find("CREATE VIEW").unwrap();
        assert!(drop_view < drop_col, "{joined}");
        assert!(drop_col < create_view, "{joined}");
    }

    #[test]
    fn an_unchanged_view_over_a_changed_column_is_still_rebuilt() {
        // The view's own text is the same, but the column underneath it is
        // being retyped — which Postgres refuses while the view reads it.
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![table("public", "Person", vec![column("age", "integer")])],
            views: vec![DumpView {
                schema: "public".into(),
                name: "Actor".into(),
                definition: "SELECT age FROM \"Person\"".into(),
                depends_on: vec![("public".into(), "Person".into())],
            }],
            ..Default::default()
        };
        let mut after = before.clone();
        after.tables[0].columns[0].pg_type = "bigint".into();

        let joined = sql(&diff_dumps(&before, &after).unwrap());
        assert!(joined.contains("DROP VIEW IF EXISTS \"public\".\"Actor\""), "{joined}");
        assert!(joined.contains("CREATE VIEW \"public\".\"Actor\" AS"), "{joined}");
        assert!(
            joined.find("DROP VIEW").unwrap() < joined.find("ALTER COLUMN \"age\" TYPE bigint").unwrap(),
            "{joined}"
        );
    }

    #[test]
    fn a_view_over_a_view_is_created_after_the_one_it_reads() {
        let after = DbDump {
            schemas: vec!["public".into()],
            views: vec![
                DumpView {
                    schema: "public".into(),
                    name: "Outer".into(),
                    definition: "SELECT id FROM \"Inner\"".into(),
                    depends_on: vec![("public".into(), "Inner".into())],
                },
                DumpView {
                    schema: "public".into(),
                    name: "Inner".into(),
                    definition: "SELECT id FROM \"Person\"".into(),
                    depends_on: vec![("public".into(), "Person".into())],
                },
            ],
            ..Default::default()
        };
        let joined = sql(&diff_dumps(&DbDump::default(), &after).unwrap());
        assert!(
            joined.find("CREATE VIEW \"public\".\"Inner\"").unwrap()
                < joined.find("CREATE VIEW \"public\".\"Outer\"").unwrap(),
            "{joined}"
        );
    }

    #[test]
    fn a_lost_enum_member_is_refused_rather_than_approximated() {
        let before = DbDump {
            enums: vec![DumpEnum {
                schema: "public".into(),
                name: "Status".into(),
                members: vec!["draft".into(), "sent".into()],
            }],
            ..Default::default()
        };
        let mut after = before.clone();
        after.enums[0].members = vec!["draft".into()];

        let err = diff_dumps(&before, &after).unwrap_err();
        assert!(err.contains("no longer has member 'sent'"), "{err}");
    }

    #[test]
    fn a_domain_keeps_its_base_type_default_and_checks() {
        let after = DbDump {
            schemas: vec!["public".into()],
            domains: vec![DumpDomain {
                schema: "public".into(),
                name: "EmailStr".into(),
                base_type: "text".into(),
                not_null: false,
                default_expr: Some("''::text".into()),
                checks: vec![DumpConstraint {
                    name: "EmailStr_ab12cd34_check".into(),
                    kind: ConstraintKind::Check,
                    definition: "CHECK ((char_length(VALUE) <= 320))".into(),
                    index: None,
                }],
            }],
            ..Default::default()
        };
        let joined = sql(&diff_dumps(&DbDump::default(), &after).unwrap());
        assert!(
            joined.contains("CREATE DOMAIN \"public\".\"EmailStr\" AS text"),
            "{joined}"
        );
        assert!(joined.contains("DEFAULT ''::text"), "{joined}");
        assert!(
            joined.contains("CONSTRAINT \"EmailStr_ab12cd34_check\" CHECK ((char_length(VALUE) <= 320))"),
            "{joined}"
        );
    }

    #[test]
    fn a_dropped_function_is_dropped_by_its_whole_signature() {
        // Two overloads share a name, so a name alone names neither.
        let before = DbDump {
            functions: vec![DumpFunction {
                schema: "public".into(),
                name: "mysum".into(),
                identity_args: "a bigint, b bigint".into(),
                definition: "CREATE OR REPLACE FUNCTION public.mysum(a bigint, b bigint) RETURNS bigint \
                             LANGUAGE sql AS $$ SELECT a + b $$"
                    .into(),
            }],
            ..Default::default()
        };
        let joined = sql(&diff_dumps(&before, &DbDump::default()).unwrap());
        assert_eq!(
            joined,
            "DROP FUNCTION IF EXISTS \"public\".\"mysum\"(a bigint, b bigint);"
        );
    }

    #[test]
    fn a_dropped_table_goes_before_the_function_its_trigger_calls() {
        // The trigger is never dropped on its own — the table's `CASCADE`
        // takes it — so dropping the function first is dropping it out from
        // under a trigger that still exists, which Postgres refuses.
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![DumpTable {
                triggers: vec![DumpTrigger {
                    name: "_rw_Archive_note".into(),
                    definition: "CREATE TRIGGER _rw_Archive_note BEFORE INSERT ON public.\"Archive\" \
                                 FOR EACH ROW EXECUTE FUNCTION public.\"_rw_Archive_note\"()"
                        .into(),
                }],
                ..table("public", "Archive", vec![column("note", "text")])
            }],
            functions: vec![DumpFunction {
                schema: "public".into(),
                name: "_rw_Archive_note".into(),
                identity_args: String::new(),
                definition: "CREATE OR REPLACE FUNCTION public.\"_rw_Archive_note\"() RETURNS trigger \
                             LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"
                    .into(),
            }],
            ..Default::default()
        };
        let after = DbDump {
            schemas: vec!["public".into()],
            ..Default::default()
        };

        let joined = sql(&diff_dumps(&before, &after).unwrap());
        assert!(
            joined.find("DROP TABLE IF EXISTS \"public\".\"Archive\"").unwrap()
                < joined.find("DROP FUNCTION IF EXISTS").unwrap(),
            "{joined}"
        );
    }

    #[test]
    fn a_changed_trigger_is_dropped_before_being_remade() {
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![DumpTable {
                triggers: vec![DumpTrigger {
                    name: "_ifl_Person_team".into(),
                    definition: "CREATE TRIGGER _ifl_Person_team AFTER DELETE ON public.\"Person\" \
                                 FOR EACH ROW EXECUTE FUNCTION public.\"_ifl_Person_team\"()"
                        .into(),
                }],
                ..table("public", "Person", vec![column("id", "uuid")])
            }],
            ..Default::default()
        };
        let mut after = before.clone();
        after.tables[0].triggers[0].definition = after.tables[0].triggers[0]
            .definition
            .replace("AFTER DELETE", "AFTER DELETE OR UPDATE");

        let joined = sql(&diff_dumps(&before, &after).unwrap());
        assert!(
            joined.find("DROP TRIGGER IF EXISTS \"_ifl_Person_team\"").unwrap()
                < joined
                    .find("CREATE TRIGGER _ifl_Person_team AFTER DELETE OR UPDATE")
                    .unwrap(),
            "{joined}"
        );
    }

    #[test]
    fn a_changed_check_constraint_replaces_the_one_of_the_same_name() {
        let before = DbDump {
            schemas: vec!["public".into()],
            tables: vec![DumpTable {
                constraints: vec![DumpConstraint {
                    name: "Person_name_check".into(),
                    kind: ConstraintKind::Check,
                    definition: "CHECK ((char_length(name) <= 20))".into(),
                    index: None,
                }],
                ..table("public", "Person", vec![column("name", "text")])
            }],
            ..Default::default()
        };
        let mut after = before.clone();
        after.tables[0].constraints[0].definition = "CHECK ((char_length(name) <= 40))".into();

        let ops = diff_dumps(&before, &after).unwrap();
        assert_eq!(
            sql(&ops),
            "ALTER TABLE \"public\".\"Person\" DROP CONSTRAINT \"Person_name_check\";\n\
             ALTER TABLE \"public\".\"Person\" ADD CONSTRAINT \"Person_name_check\" \
             CHECK ((char_length(name) <= 40));"
        );
    }

    #[test]
    fn a_foreign_key_is_added_after_every_primary_key() {
        let after = DbDump {
            schemas: vec!["public".into()],
            tables: vec![
                DumpTable {
                    constraints: vec![DumpConstraint {
                        name: "Person_team_id_fkey".into(),
                        kind: ConstraintKind::ForeignKey,
                        definition: "FOREIGN KEY (team_id) REFERENCES public.\"Team\"(id)".into(),
                        index: None,
                    }],
                    ..table("public", "Person", vec![column("team_id", "uuid")])
                },
                DumpTable {
                    constraints: vec![DumpConstraint {
                        name: "Team_pkey".into(),
                        kind: ConstraintKind::PrimaryKey,
                        definition: "PRIMARY KEY (id)".into(),
                        index: Some("Team_pkey".into()),
                    }],
                    ..table("public", "Team", vec![column("id", "uuid")])
                },
            ],
            ..Default::default()
        };
        let joined = sql(&diff_dumps(&DbDump::default(), &after).unwrap());
        assert!(
            joined.find("ADD CONSTRAINT \"Team_pkey\"").unwrap()
                < joined.find("ADD CONSTRAINT \"Person_team_id_fkey\"").unwrap(),
            "{joined}"
        );
    }
}
