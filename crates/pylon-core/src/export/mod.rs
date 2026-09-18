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

use crate::error::{PyQLError, PyQLFragmentError};
use crate::schema::{
    DeleteAction, DeleteSide, FunctionDescriptor, OnDeletePolicy, SchemaDescriptor, TypeConstraint, TypeDescriptor,
};
use std::collections::{BTreeSet, HashMap, HashSet};

pub mod python_snippet;

/// Export the full schema as a PostgreSQL DDL string.
///
/// Emits, in order:
///   1. CREATE SCHEMA for each module
///   2. CREATE TYPE … AS ENUM for enum types
///   3. CREATE DOMAIN for custom scalars
///   4. CREATE TABLE for concrete object types (property + link stub columns)
///   5. ALTER TABLE ADD CONSTRAINT FOREIGN KEY for single links
///   6. CREATE TABLE for multi-link junction tables
///   7. CREATE UNIQUE INDEX for exclusive properties/constraints
///   8. ALTER TABLE ADD CONSTRAINT CHECK for check constraints
///   9. CREATE INDEX for non-unique indexes
///  10. CREATE FUNCTION + CREATE TRIGGER for trigger descriptors
///  11. CREATE VIEW for interface types (abstract + materialized)
///  12. User-defined functions
///  13. ALTER TABLE ADD COLUMN for vector embedding columns
///  14. CREATE INDEX USING hnsw for vector indexes
pub fn export_schema(schema: &SchemaDescriptor) -> Result<String, PyQLError> {
    let mut out = String::new();

    // "module::Name" → (module, table) for FK target resolution
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

    emit_schemas(schema, &mut out);
    emit_enums(schema, &mut out);
    emit_scalars(schema, &mut out);
    emit_scalar_functions(schema, &mut out)?;
    emit_tables(schema, &mut out);
    emit_fk_constraints(schema, &type_map, &mut out);
    emit_link_source_triggers(schema, &type_map, &mut out);
    emit_junction_tables(schema, &mut out);
    emit_junction_fk_constraints(schema, &type_map, &mut out);
    emit_multilink_deletion_triggers(schema, &type_map, &mut out);
    emit_interface_link_triggers(schema, &mut out);
    emit_signal_triggers(schema, &mut out);
    emit_unique_indexes(schema, &mut out);
    emit_check_constraints(schema, &mut out);
    emit_plain_indexes(schema, &mut out);
    emit_triggers(schema, &mut out)?;
    emit_interface_views(schema, &mut out);
    emit_interface_junction_views(schema, &mut out);
    emit_interface_exclusive_triggers(schema, &mut out);
    emit_object_functions(schema, &mut out)?;
    emit_vector_columns(schema, &mut out);
    emit_vector_indexes(schema, &mut out);
    emit_search_columns(schema, &mut out);
    emit_search_indexes(schema, &mut out);

    Ok(out)
}

// ── Identifier helpers ─────────────────────────────────────────────────────────

/// Quote a PostgreSQL identifier: "name" with internal double-quotes escaped.
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

fn qn(module: &str, name: &str) -> String {
    format!("{}.{}", pg_schema(module), qi(name))
}

// ── FNV-1a hash for stable auto-generated constraint/trigger names ─────────────

fn fnv(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in p.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= b'|' as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// The generated `(function name, qualified function name)` for one trigger.
///
/// **`body` is part of the hash, and has to be.** The migration diff compares
/// triggers by *name* — `DbTrigger` carries nothing else — so a name derived
/// only from the table and pointer is stable across a change to what the
/// trigger actually does. That means a fix to trigger codegen produces no
/// diff and never reaches a database that already exists: the old body stays
/// until someone drops the table. Hashing the body instead makes the name
/// content-addressed, so changing the emitted SQL renames the function, the
/// diff sees one trigger missing and one unexpected, and the existing
/// create/drop machinery replaces it.
///
/// This is the same trick the CHECK-constraint names already use (their hash
/// includes the constraint expression); triggers were the outlier.
fn trigger_names(module: &str, table: &str, pointer: &str, suffix: &str, body: &str) -> (String, String) {
    let hash = fnv(&[table, pointer, suffix, body]);
    let fname = format!("{}_{}_{}", table, pointer, &hash[..8]);
    let fn_qname = qn(module, &fname);
    (fname, fn_qname)
}

// ── Trigger event/timing helpers ───────────────────────────────────────────────

fn trigger_events(on: u8) -> String {
    let mut events = Vec::new();
    if on & 1 != 0 {
        events.push("INSERT");
    }
    if on & 2 != 0 {
        events.push("UPDATE");
    }
    if on & 4 != 0 {
        events.push("DELETE");
    }
    events.join(" OR ")
}

fn trigger_timing(timing: &str) -> &str {
    match timing {
        "Before" => "BEFORE",
        "After" => "AFTER",
        "InsteadOf" => "INSTEAD OF",
        t => t,
    }
}

// ── Phase 1: schemas ───────────────────────────────────────────────────────────

fn emit_schemas(schema: &SchemaDescriptor, out: &mut String) {
    // Every kind of top-level declaration can live in a module of its own,
    // including one with *no* types/scalars/enums at all (a pure-function
    // utility module, a globals-only module, ...) — omitting any of these
    // means that module's own `CREATE SCHEMA` never gets emitted, so its
    // first `CREATE FUNCTION`/global/alias DDL then fails outright with
    // "schema does not exist" (confirmed live).
    let mut modules: BTreeSet<&str> = BTreeSet::new();
    for t in &schema.types {
        modules.insert(&t.module);
    }
    for s in &schema.scalars {
        modules.insert(&s.module);
    }
    for e in &schema.enums {
        modules.insert(&e.module);
    }
    for f in &schema.functions {
        modules.insert(&f.module);
    }
    for g in &schema.globals {
        modules.insert(&g.module);
    }
    for a in &schema.aliases {
        modules.insert(&a.module);
    }
    let non_default: Vec<&str> = modules.into_iter().filter(|m| *m != "default").collect();
    for module in &non_default {
        out.push_str(&format!("CREATE SCHEMA IF NOT EXISTS {};\n", pg_schema(module)));
    }
    if !non_default.is_empty() {
        out.push('\n');
    }
}

// ── Phase 2: enum types ────────────────────────────────────────────────────────

fn emit_enums(schema: &SchemaDescriptor, out: &mut String) {
    for e in &schema.enums {
        let members: Vec<String> = e
            .members
            .iter()
            .map(|m| format!("'{}'", m.replace('\'', "''")))
            .collect();
        out.push_str(&format!(
            "DO $$ BEGIN CREATE TYPE {}.{} AS ENUM ({}); EXCEPTION WHEN duplicate_object THEN NULL; END $$;\n",
            pg_schema(&e.module),
            qi(&e.name),
            members.join(", "),
        ));
    }
    if !schema.enums.is_empty() {
        out.push('\n');
    }
}

// ── Phase 3: custom scalar domains ────────────────────────────────────────────

fn emit_scalars(schema: &SchemaDescriptor, out: &mut String) {
    for s in &schema.scalars {
        if s.is_sequence {
            out.push_str(&format!(
                "CREATE SEQUENCE {}.{};\n",
                pg_schema(&s.module),
                qi(&format!("{}_seq", s.name)),
            ));
        }
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
        out.push_str(&format!(
            "CREATE DOMAIN {}.{} AS {}{};\n",
            pg_schema(&s.module),
            qi(&s.name),
            s.pg_type,
            check_clause,
        ));
    }
    if !schema.scalars.is_empty() {
        out.push('\n');
    }
}

// ── Phase 4: concrete tables ───────────────────────────────────────────────────

fn emit_tables(schema: &SchemaDescriptor, out: &mut String) {
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        emit_one_table(t, Some(schema), out);
    }
}

/// The effective SQL DEFAULT for a property.
///
/// `default_sql` is used as-is; a `default_pyql` expression
/// (`Default(std.uuid_generate_v7())`) is compiled through the same IR
/// compiler the migration path uses, so `export_schema` and `pylon migrate`
/// can't disagree about what a column's default is.
fn column_default(p: &crate::schema::PropertyDescriptor, schema: Option<&SchemaDescriptor>) -> Option<String> {
    if let Some(sql) = &p.default_sql {
        return Some(sql.clone());
    }
    let pyql = p.default_pyql.as_deref()?;
    crate::ir::compile_scalar_default(pyql, schema?).ok()
}

fn emit_one_table(t: &TypeDescriptor, schema: Option<&SchemaDescriptor>, out: &mut String) {
    out.push_str(&format!("CREATE TABLE {} (\n", qn(&t.module, &t.table)));

    let mut lines: Vec<String> = Vec::new();

    // Property columns
    for p in &t.properties {
        let not_null = if p.nullable { "" } else { " NOT NULL" };
        let default = column_default(p, schema)
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        let col_type = p
            .column_type
            .as_deref()
            .unwrap_or_else(|| p.pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(&p.pg_type));
        lines.push(format!("    {} {}{}{}", qi(&p.name), col_type, not_null, default));
    }

    // Link columns — uuid stubs; FK constraints added in phase 5. A
    // junction-backed link has no column here at all — it's stored the
    // same way a multi-link is, via a junction table (`emit_junction_tables`).
    for l in &t.links {
        if l.is_junction_backed() {
            continue;
        }
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        lines.push(format!("    {} uuid{}", qi(&format!("{}_id", l.name)), not_null));
    }

    // Primary key. PostgreSQL requires a partitioned table's key to include
    // its partition column — a uniqueness guarantee it can't enforce across
    // partitions otherwise — so the partition column is appended here rather
    // than left to the schema author to remember.
    let mut pk_cols: Vec<String> = t.properties.iter().filter(|p| p.is_pk).map(|p| qi(&p.name)).collect();
    if let Some(part) = &t.partition {
        let key = qi(&part.pointer);
        if !pk_cols.contains(&key) {
            pk_cols.push(key);
        }
    }
    if !pk_cols.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    out.push_str(&lines.join(",\n"));
    out.push_str("\n)");
    if let Some(part) = &t.partition {
        out.push_str(&format!(" PARTITION BY RANGE ({})", qi(&part.pointer)));
    }
    out.push_str(";\n\n");
    if let Some(part) = &t.partition {
        out.push_str(&partman_setup_sql(&t.module, &t.table, part));
        out.push_str("\n\n");
    }
    out.push_str(&cache_invalidate_trigger_sql(&qn(&t.module, &t.table)));
    out.push_str("\n\n");
}

/// Registers a partitioned table with pg_partman.
///
/// `create_parent` both records the table in `partman.part_config` and
/// creates its initial set of partitions, so this is what makes the
/// `PARTITION BY RANGE` table above actually writable. Guarded by a
/// `part_config` lookup because `create_parent` errors on a table it has
/// already adopted, and this DDL is re-run on every schema export.
///
/// Retention is applied as a follow-up `UPDATE` rather than a `create_parent`
/// argument: it's the one setting that deletes data, so it stays visible as
/// its own statement in the generated migration rather than buried in a
/// function call's argument list.
fn partman_setup_sql(module: &str, table: &str, part: &crate::schema::PartitionDescriptor) -> String {
    let pg_schema = if module == "default" { "public" } else { module };
    // `create_parent` takes the parent table as a *string*, so this is a
    // literal, not an identifier — single-quote escaping, not `qi`.
    let parent = format!("{pg_schema}.{table}").replace('\'', "''");
    let mut out = String::new();

    out.push_str("DO $$ BEGIN\n");
    out.push_str(&format!(
        "    IF NOT EXISTS (SELECT 1 FROM partman.part_config WHERE parent_table = '{parent}') THEN\n"
    ));
    out.push_str(&format!(
        "        PERFORM partman.create_parent(\n\
         \x20           p_parent_table := '{parent}',\n\
         \x20           p_control := '{control}',\n\
         \x20           p_interval := '{interval}',\n\
         \x20           p_premake := {premake}\n\
         \x20       );\n",
        control = part.pointer.replace('\'', "''"),
        interval = part.interval.as_pg_interval(),
        premake = part.premake,
    ));
    out.push_str("    END IF;\nEND $$;\n");

    match part.retention_interval() {
        Some(retention) => {
            out.push_str(&format!(
                "UPDATE partman.part_config\n\
                 \x20   SET retention = '{retention}', retention_keep_table = false\n\
                 \x20   WHERE parent_table = '{parent}';",
            ));
        }
        None => {
            // Explicitly cleared, so removing a `retention=` from the schema
            // actually stops the dropping rather than leaving the last value
            // in place.
            out.push_str(&format!(
                "UPDATE partman.part_config\n    SET retention = NULL\n    WHERE parent_table = '{parent}';",
            ));
        }
    }
    out
}

/// `CREATE OR REPLACE TRIGGER` statement wiring `qualified_table` into the
/// cache-invalidation notify function (see `stdlib::ddl::CACHE_INVALIDATE_DDL`).
/// Statement-level, not row-level — tier 1 invalidation only needs one notify
/// per write statement. Attached unconditionally to every concrete table and
/// junction table so the cache plumbing exists regardless of `[cache].enabled`.
fn cache_invalidate_trigger_sql(qualified_table: &str) -> String {
    format!(
        "CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON {}\n    FOR EACH STATEMENT EXECUTE FUNCTION _pylon.notify_cache_invalidate();",
        qualified_table
    )
}

// ── Interface (polymorphic target) helpers ─────────────────────────────────────

/// Qualified names of the polymorphic interfaces in `schema` — the types
/// `emit_interface_views` renders as a `UNION ALL` view instead of a table.
///
/// A link pointing at one of these has no single table to reference, and
/// PostgreSQL rejects a foreign key whose referenced relation is a view
/// ("referenced relation is not a table"). Such links therefore carry no FK
/// at all; their deletion policy is enforced by the per-implementor triggers
/// `interface_link_trigger_infos` builds instead.
pub(crate) fn polymorphic_types(schema: &SchemaDescriptor) -> HashSet<String> {
    schema
        .types
        .iter()
        .filter(|t| t.abstract_ && t.materialized)
        .map(|t| format!("{}::{}", t.module, t.name))
        .collect()
}

/// Interface qualified name → the concrete types implementing it, in schema
/// order. Shared by the view emitter and the interface-link triggers so the
/// set of tables a polymorphic link can point into is derived in one place.
pub(crate) fn interface_implementors(schema: &SchemaDescriptor) -> HashMap<String, Vec<&TypeDescriptor>> {
    let mut implementors: HashMap<String, Vec<&TypeDescriptor>> = HashMap::new();
    for t in &schema.types {
        if !t.abstract_ {
            for iface in &t.interfaces {
                implementors.entry(iface.clone()).or_default().push(t);
            }
        }
    }
    implementors
}

// ── Deletion policy helpers ────────────────────────────────────────────────────

fn policy_for<'a>(policies: &'a [OnDeletePolicy], side: &DeleteSide) -> Option<&'a DeleteAction> {
    policies.iter().find(|p| &p.side == side).map(|p| &p.action)
}

/// True when the Source-side policy is `DeleteTarget`/`DeleteTargetIfOrphan`
/// — a `BEFORE DELETE` trigger on the owner row (`emit_link_source_triggers`/
/// `emit_multilink_deletion_triggers`) that deletes the target *while the
/// owner row that references it still exists* (triggers fire before the
/// row is actually removed). An immediate (non-deferred) `RESTRICT`/default
/// Target-side FK would see that still-present owner row and reject the
/// trigger's own delete — confirmed live (`live_execution_on_delete.rs`):
/// every `DeleteTarget`/`DeleteTargetIfOrphan` delete failed with
/// "violates RESTRICT setting" until the corresponding Target-side FK was
/// forced deferrable. A permissive Target-side policy (`Allow`/`DeleteSource`)
/// has no such conflict — only the RESTRICT-family default needs forcing.
pub(crate) fn needs_deferred_target_fk(policies: &[OnDeletePolicy]) -> bool {
    policies.iter().any(|p| {
        p.side == DeleteSide::Source
            && matches!(
                p.action,
                DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan
            )
    })
}

/// Returns the `ON DELETE …` / `DEFERRABLE …` suffix for a FK constraint
/// based on the Target-side policy. `is_deferred` is set for DeferredRestrict.
fn target_fk_suffix(policies: &[OnDeletePolicy]) -> String {
    match policy_for(policies, &DeleteSide::Target).unwrap_or(&DeleteAction::Restrict) {
        DeleteAction::Restrict if needs_deferred_target_fk(policies) => " DEFERRABLE INITIALLY DEFERRED".into(),
        DeleteAction::Restrict => " ON DELETE RESTRICT".into(),
        DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED".into(),
        DeleteAction::DeleteSource => " ON DELETE CASCADE".into(),
        DeleteAction::Allow => " ON DELETE SET NULL".into(),
        _ => " ON DELETE RESTRICT".into(),
    }
}

/// Returns the `ON DELETE …` suffix for the source FK in a junction table.
///
/// Always CASCADE: complex Source policies (DeleteTarget, DeleteTargetIfOrphan) are
/// handled by triggers emitted in `emit_multilink_deletion_triggers`.
fn source_jt_fk_suffix(_policies: &[OnDeletePolicy]) -> &'static str {
    " ON DELETE CASCADE"
}

/// Returns the `ON DELETE …` suffix for the target FK in a junction table.
fn target_jt_fk_suffix(policies: &[OnDeletePolicy]) -> String {
    match policy_for(policies, &DeleteSide::Target).unwrap_or(&DeleteAction::Restrict) {
        DeleteAction::Restrict if needs_deferred_target_fk(policies) => " DEFERRABLE INITIALLY DEFERRED".into(),
        DeleteAction::Restrict => " ON DELETE RESTRICT".into(),
        DeleteAction::DeferredRestrict => " DEFERRABLE INITIALLY DEFERRED".into(),
        DeleteAction::Allow => " ON DELETE CASCADE".into(),
        // DeleteSource on a multilink: junction ON DELETE CASCADE + trigger (emitted separately)
        DeleteAction::DeleteSource => " ON DELETE CASCADE".into(),
        _ => " ON DELETE RESTRICT".into(),
    }
}

// ── Phase 5: FK constraints for single links ───────────────────────────────────

fn emit_fk_constraints(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>, out: &mut String) {
    let polymorphic = polymorphic_types(schema);
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for l in &t.links {
            if l.is_junction_backed() {
                continue;
            }
            if polymorphic.contains(&l.target) {
                continue;
            }
            let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else {
                continue;
            };
            let cname = qi(&format!("{}_{}_fkey", t.table, l.name));
            let suffix = target_fk_suffix(&l.on_delete);
            out.push_str(&format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}(id){};\n",
                qn(&t.module, &t.table),
                cname,
                qi(&format!("{}_id", l.name)),
                qn(tgt_module, tgt_table),
                suffix,
            ));
            emitted = true;
        }
    }
    if emitted {
        out.push('\n');
    }
}

/// `(module, junction table, constraint name, ddl)` for every junction's
/// target-side FK.
///
/// A junction table is created inside its *source* type's step, but its target
/// FK points at a completely unrelated type whose own table may be created much
/// later — `"account"."Account.tags"` referencing `"account"."Tag"` failed
/// exactly that way. Emitting the FK inline therefore imposes an ordering the
/// migration engine has no way to satisfy in general, so the column is created
/// bare and the constraint added in a later pass, exactly as a single link's
/// already is. A link to an interface gets no FK at all (see
/// `polymorphic_types`).
pub fn junction_fk_constraints(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
) -> Vec<(String, String, String, String)> {
    let polymorphic = polymorphic_types(schema);
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        let pointers = t
            .multilinks
            .iter()
            .map(|ml| (ml.name.as_str(), &ml.target, &ml.on_delete))
            .chain(
                t.links
                    .iter()
                    .filter(|l| l.is_junction_backed())
                    .map(|l| (l.name.as_str(), &l.target, &l.on_delete)),
            );
        for (name, target, on_delete) in pointers {
            if polymorphic.contains(target) {
                continue;
            }
            let Some((tgt_module, tgt_table)) = type_map.get(target) else {
                continue;
            };
            let jt_name = format!("{}.{}", t.table, name);
            // `{table}_{link}_target_fkey`, not the dotted junction table name:
            // this is the convention the rest of the engine already expects for
            // a junction's foreign keys.
            let cname = format!("{}_{}_target_fkey", t.table, name);
            let ddl = format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY (target) REFERENCES {}(id){};",
                qn(&t.module, &jt_name),
                qi(&cname),
                qn(tgt_module, tgt_table),
                target_jt_fk_suffix(on_delete),
            );
            result.push((t.module.clone(), jt_name, cname, ddl));
        }
    }
    result
}

fn emit_junction_fk_constraints(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>, out: &mut String) {
    let constraints = junction_fk_constraints(schema, type_map);
    for (_, _, _, ddl) in &constraints {
        out.push_str(ddl);
        out.push('\n');
    }
    if !constraints.is_empty() {
        out.push('\n');
    }
}

// ── Trigger emit helper ────────────────────────────────────────────────────────

fn emit_before_delete_trigger(fn_qname: &str, trigger_name: &str, table_qname: &str, body: &str, out: &mut String) {
    out.push_str(&format!(
        "CREATE OR REPLACE FUNCTION {fn_qname}()\n\
         RETURNS trigger LANGUAGE plpgsql AS $$\n\
         BEGIN\n"
    ));
    out.push_str(body);
    out.push_str(&format!(
        "\n    RETURN OLD;\nEND;\n$$;\n\n\
         CREATE TRIGGER {trigger_name}\n\
         BEFORE DELETE ON {table_qname}\n\
         FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n"
    ));
}

/// Like `emit_before_delete_trigger`, but `AFTER DELETE` — required
/// whenever the trigger body's own delete can cascade back onto the same
/// row the trigger is firing for (the multilink target-side `DeleteSource`
/// case: deleting a target cascades to delete the junction row, whose
/// trigger deletes the owner, whose own junction-cleanup cascade would
/// otherwise try to delete that same still-being-deleted junction row
/// again). A `BEFORE DELETE` trigger there hits Postgres's own "tuple to be
/// deleted was already modified by an operation triggered by the current
/// command" — confirmed live (`tests/live_execution_on_delete.rs`) — and
/// Postgres's error hint is literally to use `AFTER` instead, since by then
/// the row is actually gone and the reentrant cascade has nothing to touch.
fn emit_after_delete_trigger(fn_qname: &str, trigger_name: &str, table_qname: &str, body: &str, out: &mut String) {
    out.push_str(&format!(
        "CREATE OR REPLACE FUNCTION {fn_qname}()\n\
         RETURNS trigger LANGUAGE plpgsql AS $$\n\
         BEGIN\n"
    ));
    out.push_str(body);
    out.push_str(&format!(
        "\n    RETURN NULL;\nEND;\n$$;\n\n\
         CREATE TRIGGER {trigger_name}\n\
         AFTER DELETE ON {table_qname}\n\
         FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n"
    ));
}

/// Like `emit_after_delete_trigger`, but fires on `events` (e.g.
/// `"INSERT OR DELETE"`) — used for the `@pylon.signal` capture trigger
/// (`signal_trigger_infos`), scoped to exactly the operations at least one
/// registered handler cares about, so a type with only an `On.Insert`
/// handler doesn't pay for capturing (and draining) Update/Delete rows
/// nobody asked for. `TG_OP` inside the body still distinguishes which of
/// `events` actually fired.
fn emit_after_mutation_trigger(
    fn_qname: &str,
    trigger_name: &str,
    table_qname: &str,
    events: &str,
    body: &str,
    out: &mut String,
) {
    out.push_str(&format!(
        "CREATE OR REPLACE FUNCTION {fn_qname}()\n\
         RETURNS trigger LANGUAGE plpgsql AS $$\n\
         BEGIN\n"
    ));
    out.push_str(body);
    out.push_str(&format!(
        "\n    RETURN NULL;\nEND;\n$$;\n\n\
         CREATE OR REPLACE TRIGGER {trigger_name}\n\
         AFTER {events} ON {table_qname}\n\
         FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n"
    ));
}

/// The physical table(s) a link target resolves to, qualified and ready to
/// interpolate. A concrete target is exactly one table; a polymorphic one is
/// every implementor of the interface, since the view itself is not deletable
/// (a `UNION ALL` view is not auto-updatable). `None` when the target resolves
/// to nothing at all, or to an interface that nothing implements.
fn target_table_qnames(
    target: &str,
    type_map: &HashMap<String, (&str, &str)>,
    polymorphic: &HashSet<String>,
    implementors: &HashMap<String, Vec<&TypeDescriptor>>,
) -> Option<Vec<String>> {
    if polymorphic.contains(target) {
        let impls = implementors.get(target)?;
        if impls.is_empty() {
            return None;
        }
        return Some(impls.iter().map(|i| qn(&i.module, &i.table)).collect());
    }
    let (tgt_module, tgt_table) = type_map.get(target)?;
    Some(vec![qn(tgt_module, tgt_table)])
}

// ── Phase 5.5: source-side deletion triggers for single links ──────────────────

/// Structured description of one deletion-policy trigger (either a
/// single-link Source-side `DeleteTarget`/`DeleteTargetIfOrphan`, a
/// multilink Source-side `DeleteTarget`/`DeleteTargetIfOrphan`, or a
/// multilink Target-side `DeleteSource`). Each trigger has its own
/// dedicated function (unlike the exclusive-constraint triggers, no
/// function sharing), so `ddl` carries the combined `CREATE FUNCTION` +
/// `CREATE TRIGGER` block. Used by both `export_schema` (unconditional
/// emission) and the diff engine (`diff/mod.rs`, comparing against live
/// `DbTable.triggers`) so the two DDL-generation paths can't drift the way
/// they did before — the incremental migration path used to never emit
/// these triggers at all.
pub struct DeletionTriggerInfo {
    pub table_module: String,
    /// The table the `CREATE TRIGGER` attaches to (the link's/multilink's
    /// own owner table for Source-side triggers; the junction table for a
    /// multilink Target-side trigger).
    pub table_name: String,
    pub trigger_name: String,
    pub ddl: String,
}

fn link_source_trigger_infos(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
) -> Vec<DeletionTriggerInfo> {
    let polymorphic = polymorphic_types(schema);
    let implementors = interface_implementors(schema);
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for l in &t.links {
            // A junction-backed link has no `{name}_id` column to trigger
            // off of — its deletion-policy triggers are emitted alongside
            // multi-links' own, against the junction table's source/target
            // columns instead (`multilink_deletion_trigger_infos`).
            if l.is_junction_backed() {
                continue;
            }
            let src_action = policy_for(&l.on_delete, &DeleteSide::Source).unwrap_or(&DeleteAction::Allow);
            match src_action {
                DeleteAction::Allow => continue,
                DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan => {}
                _ => continue,
            }
            let suffix = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                "del_orphan"
            } else {
                "del_target"
            };
            let tbl_qname = qn(&t.module, &t.table);
            let col = qi(&format!("{}_id", l.name));
            let Some(tgt_qnames) = target_table_qnames(&l.target, type_map, &polymorphic, &implementors) else {
                continue;
            };
            let deletes = tgt_qnames
                .iter()
                .map(|tgt_qname| format!("DELETE FROM {tgt_qname} WHERE id = OLD.{col};"))
                .collect::<Vec<_>>();

            let body = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                let indented = deletes
                    .iter()
                    .map(|d| format!("        {d}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "    IF NOT EXISTS (\n        SELECT 1 FROM {tbl_qname} WHERE {col} = OLD.{col} AND id != OLD.id\n    ) THEN\n{indented}\n    END IF;"
                )
            } else {
                deletes
                    .iter()
                    .map(|d| format!("    {d}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            let (fname, fn_qname) = trigger_names(&t.module, &t.table, &l.name, suffix, &body);

            let mut ddl = String::new();
            emit_before_delete_trigger(&fn_qname, &qi(&fname), &tbl_qname, &body, &mut ddl);
            result.push(DeletionTriggerInfo {
                table_module: t.module.clone(),
                table_name: t.table.clone(),
                trigger_name: fname,
                ddl,
            });
        }
    }
    result
}

fn emit_link_source_triggers(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>, out: &mut String) {
    for info in link_source_trigger_infos(schema, type_map) {
        out.push_str(&info.ddl);
    }
}

// ── Phase 6: junction tables for multi-links ───────────────────────────────────

fn emit_junction_tables(schema: &SchemaDescriptor, out: &mut String) {
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for ml in &t.multilinks {
            emit_one_junction_table(
                schema,
                t,
                &ml.name,
                &ml.on_delete,
                ml.through.as_deref(),
                false,
                false,
                out,
            );
        }
        // A junction-backed single link is stored exactly like a
        // multi-link's junction table, just constrained to at most one
        // row per source (D3): `PRIMARY KEY (source)` instead of
        // `(source, target)`, plus `UNIQUE (target)` when the link is
        // also declared exclusive.
        for l in &t.links {
            if !l.is_junction_backed() {
                continue;
            }
            emit_one_junction_table(
                schema,
                t,
                &l.name,
                &l.on_delete,
                l.through.as_deref(),
                true,
                l.is_exclusive,
                out,
            );
        }
    }
}

// Same ten independent axes as `diff::emit_junction_table` — see the note
// there for why these stay as parameters rather than a params struct.
#[allow(clippy::too_many_arguments)]
fn emit_one_junction_table(
    schema: &SchemaDescriptor,
    t: &TypeDescriptor,
    name: &str,
    on_delete: &[OnDeletePolicy],
    through: Option<&str>,
    single: bool,
    exclusive: bool,
    out: &mut String,
) {
    let jt_name = format!("{}.{}", t.table, name);
    let src_suffix = source_jt_fk_suffix(on_delete);
    out.push_str(&format!(
        "CREATE TABLE {} (\n    source uuid NOT NULL REFERENCES {}(id){},\n",
        qn(&t.module, &jt_name),
        qn(&t.module, &t.table),
        src_suffix,
    ));
    // No inline `REFERENCES`: the target table may not exist yet, so the FK is
    // added afterwards by `emit_junction_fk_constraints`, the same way a single
    // link's FK waits for `emit_fk_constraints`. See that function for why.
    out.push_str("    target uuid NOT NULL,\n");

    // Extra property columns from a junction through type.
    if let Some(through_qname) = through {
        let through_td = schema
            .types
            .iter()
            .find(|td| format!("{}::{}", td.module, td.name) == *through_qname);
        if let Some(td) = through_td
            && td.junction
        {
            for p in &td.properties {
                if p.name == "id" {
                    continue;
                }
                let not_null = if p.nullable { "" } else { " NOT NULL" };
                out.push_str(&format!("    {} {}{},\n", qi(&p.name), p.pg_type, not_null));
            }
        }
    }

    if single {
        out.push_str("    PRIMARY KEY (source)");
    } else {
        out.push_str("    PRIMARY KEY (source, target)");
    }
    if exclusive {
        out.push_str(",\n    UNIQUE (target)\n);\n\n");
    } else {
        out.push_str("\n);\n\n");
    }
    out.push_str(&cache_invalidate_trigger_sql(&qn(&t.module, &jt_name)));
    out.push_str("\n\n");
}

// ── Phase 6.5: multilink deletion policy triggers ──────────────────────────────

fn multilink_deletion_trigger_infos(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
) -> Vec<DeletionTriggerInfo> {
    let polymorphic = polymorphic_types(schema);
    let implementors = interface_implementors(schema);
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for ml in &t.multilinks {
            push_junction_deletion_triggers(
                t,
                &ml.name,
                &ml.target,
                &ml.on_delete,
                type_map,
                &polymorphic,
                &implementors,
                &mut result,
            );
        }
        // A junction-backed single link's junction table has the same
        // source/target columns as a multi-link's, so the same
        // deletion-policy trigger bodies apply unchanged.
        for l in &t.links {
            if !l.is_junction_backed() {
                continue;
            }
            push_junction_deletion_triggers(
                t,
                &l.name,
                &l.target,
                &l.on_delete,
                type_map,
                &polymorphic,
                &implementors,
                &mut result,
            );
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn push_junction_deletion_triggers(
    t: &TypeDescriptor,
    name: &str,
    target: &str,
    on_delete: &[OnDeletePolicy],
    type_map: &HashMap<String, (&str, &str)>,
    polymorphic: &HashSet<String>,
    implementors: &HashMap<String, Vec<&TypeDescriptor>>,
    result: &mut Vec<DeletionTriggerInfo>,
) {
    let jt_name = format!("{}.{}", t.table, name);
    let jt_qname = qn(&t.module, &jt_name);

    // Source-side: DeleteTarget / DeleteTargetIfOrphan
    let src_action = policy_for(on_delete, &DeleteSide::Source).unwrap_or(&DeleteAction::Allow);
    if matches!(
        src_action,
        DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan
    ) && let Some(tgt_qnames) = target_table_qnames(target, type_map, polymorphic, implementors)
    {
        let suffix = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
            "del_orphan"
        } else {
            "del_target"
        };
        let deletes = tgt_qnames
            .iter()
            .map(|tgt_qname| format!("DELETE FROM {tgt_qname} WHERE id = OLD.target;"))
            .collect::<Vec<_>>();

        let body = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
            let indented = deletes
                .iter()
                .map(|d| format!("        {d}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "    IF NOT EXISTS (\n        SELECT 1 FROM {jt_qname} WHERE target = OLD.target AND source != OLD.source\n    ) THEN\n{indented}\n    END IF;"
            )
        } else {
            deletes
                .iter()
                .map(|d| format!("    {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Keeps `suffix` in the rendered name (unlike `trigger_names`), since
        // both variants below hang off the same table/pointer pair.
        let hash = fnv(&[&t.table, name, suffix, &body]);
        let fname = format!("{}_{}_{}_{}", t.table, name, suffix, &hash[..8]);
        let fn_qname = qn(&t.module, &fname);

        let mut ddl = String::new();
        emit_before_delete_trigger(&fn_qname, &qi(&fname), &jt_qname, &body, &mut ddl);
        result.push(DeletionTriggerInfo {
            table_module: t.module.clone(),
            table_name: jt_name.clone(),
            trigger_name: fname,
            ddl,
        });
    }

    // Target-side: DeleteSource — when target deleted (cascade removes junction row),
    // also delete the source object.
    let tgt_action = policy_for(on_delete, &DeleteSide::Target).unwrap_or(&DeleteAction::Restrict);
    if matches!(tgt_action, DeleteAction::DeleteSource) {
        let src_qname = qn(&t.module, &t.table);

        let body = format!("    DELETE FROM {src_qname} WHERE id = OLD.source;");
        let (fname, fn_qname) = trigger_names(&t.module, &t.table, name, "del_source", &body);

        let mut ddl = String::new();
        emit_after_delete_trigger(&fn_qname, &qi(&fname), &jt_qname, &body, &mut ddl);
        result.push(DeletionTriggerInfo {
            table_module: t.module.clone(),
            table_name: jt_name.clone(),
            trigger_name: fname,
            ddl,
        });
    }
}

// ── Phase 6.6: target-side enforcement for polymorphic links ───────────────────

/// Emits, for one link whose target is an interface, the trigger that stands
/// in for the foreign key such a link cannot have.
///
/// `referencing` is the table holding the pointer (the owner table for a
/// single link, the junction table for a multi-link) and `column` the column
/// within it. One function is generated per (referencing table, pointer) and
/// shared by a `CREATE TRIGGER` on every implementor, mirroring how
/// `make_excl_info` shares one function across an interface's implementors.
#[allow(clippy::too_many_arguments)]
fn push_interface_link_triggers(
    module: &str,
    referencing: &str,
    column: &str,
    pointer: &str,
    src_table: &str,
    via_junction: bool,
    on_delete: &[OnDeletePolicy],
    impls: &[&TypeDescriptor],
    result: &mut Vec<DeletionTriggerInfo>,
) {
    let action = policy_for(on_delete, &DeleteSide::Target).unwrap_or(&DeleteAction::Restrict);
    let col = qi(column);

    // A Source-side cascade deletes the target from a BEFORE DELETE trigger on
    // the owner row, which still exists at that point — the same conflict
    // `needs_deferred_target_fk` documents for a real FK. Deferring the check
    // to commit lets both deletes land first.
    let deferred = matches!(action, DeleteAction::DeferredRestrict) || needs_deferred_target_fk(on_delete);

    let body = match action {
        DeleteAction::Allow if via_junction => {
            // Junction row: `Allow` drops the membership, matching the
            // ON DELETE CASCADE the junction's target FK used to carry.
            format!("    DELETE FROM {referencing} WHERE {col} = OLD.id;")
        }
        DeleteAction::Allow => format!("    UPDATE {referencing} SET {col} = NULL WHERE {col} = OLD.id;"),
        DeleteAction::DeleteSource => format!("    DELETE FROM {referencing} WHERE {col} = OLD.id;"),
        _ => format!(
            "    IF EXISTS (SELECT 1 FROM {referencing} WHERE {col} = OLD.id) THEN\n\
             \x20       RAISE foreign_key_violation\n\
             \x20         USING MESSAGE = 'update or delete on table \"' || TG_TABLE_NAME || '\" violates foreign key constraint on table {src_table}',\n\
             \x20               DETAIL = format('Key (id)=(%s) is still referenced from table \"{src_table}\".', OLD.id);\n\
             \x20   END IF;"
        ),
    };

    let suffix = match action {
        DeleteAction::Allow => "ifl_allow",
        DeleteAction::DeleteSource => "ifl_del_source",
        _ => "ifl_restrict",
    };

    for impl_t in impls {
        let hash = fnv(&[&impl_t.table, src_table, pointer, suffix, &body]);
        let fname = format!("_ifl_{}_{}_{}", src_table, pointer, &hash[..8]);
        let fn_qname = qn(module, &fname);
        let impl_qname = qn(&impl_t.module, &impl_t.table);

        let mut ddl = String::new();
        if deferred {
            // A constraint trigger can only fire AFTER, so the referencing
            // rows are checked once the whole statement (or transaction) has
            // settled rather than mid-delete.
            ddl.push_str(&format!(
                "CREATE OR REPLACE FUNCTION {fn_qname}()\n\
                 RETURNS trigger LANGUAGE plpgsql AS $$\n\
                 BEGIN\n{body}\n    RETURN NULL;\nEND;\n$$;\n\n\
                 CREATE CONSTRAINT TRIGGER {}\n\
                 AFTER DELETE ON {impl_qname}\n\
                 DEFERRABLE INITIALLY DEFERRED\n\
                 FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n",
                qi(&fname),
            ));
        } else {
            emit_before_delete_trigger(&fn_qname, &qi(&fname), &impl_qname, &body, &mut ddl);
        }

        result.push(DeletionTriggerInfo {
            table_module: impl_t.module.clone(),
            table_name: impl_t.table.clone(),
            trigger_name: fname,
            ddl,
        });
    }
}

/// Target-side deletion enforcement for every link and multi-link pointing at
/// an interface. These carry the integrity the suppressed foreign keys would
/// otherwise have provided — see `polymorphic_types`.
fn interface_link_trigger_infos(schema: &SchemaDescriptor) -> Vec<DeletionTriggerInfo> {
    let polymorphic = polymorphic_types(schema);
    let implementors = interface_implementors(schema);
    let mut result = Vec::new();

    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for l in &t.links {
            if !polymorphic.contains(&l.target) {
                continue;
            }
            let Some(impls) = implementors.get(&l.target) else {
                continue;
            };
            let via_junction = l.is_junction_backed();
            let (referencing, column) = if via_junction {
                (qn(&t.module, &format!("{}.{}", t.table, l.name)), "target".to_string())
            } else {
                (qn(&t.module, &t.table), format!("{}_id", l.name))
            };
            push_interface_link_triggers(
                &t.module,
                &referencing,
                &column,
                &l.name,
                &t.table,
                via_junction,
                &l.on_delete,
                impls,
                &mut result,
            );
        }
        for ml in &t.multilinks {
            if !polymorphic.contains(&ml.target) {
                continue;
            }
            let Some(impls) = implementors.get(&ml.target) else {
                continue;
            };
            let referencing = qn(&t.module, &format!("{}.{}", t.table, ml.name));
            push_interface_link_triggers(
                &t.module,
                &referencing,
                "target",
                &ml.name,
                &t.table,
                true,
                &ml.on_delete,
                impls,
                &mut result,
            );
        }
    }
    result
}

fn emit_interface_link_triggers(schema: &SchemaDescriptor, out: &mut String) {
    for info in interface_link_trigger_infos(schema) {
        out.push_str(&info.ddl);
    }
}

fn emit_multilink_deletion_triggers(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    for info in multilink_deletion_trigger_infos(schema, type_map) {
        out.push_str(&info.ddl);
    }
}

/// Combined single-link + multilink deletion-policy trigger specs for
/// `schema`. Public entry point for `diff/mod.rs` — see `DeletionTriggerInfo`.
pub fn deletion_policy_trigger_infos(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
) -> Vec<DeletionTriggerInfo> {
    let mut result = link_source_trigger_infos(schema, type_map);
    result.extend(multilink_deletion_trigger_infos(schema, type_map));
    result.extend(interface_link_trigger_infos(schema));
    result
}

// ── Post-commit signal capture triggers ─────────────────────────────────────────

/// One capture trigger per concrete type with at least one `@pylon.signal`
/// handler registered — `td.signals` non-empty is the only condition, so a
/// type nobody's listening to gets no trigger and pays no per-mutation
/// cost. Fires on every operation; the function body itself uses `TG_OP`
/// to decide what to populate. Public entry point for `diff/mod.rs`,
/// mirroring `deletion_policy_trigger_infos`.
pub fn signal_trigger_infos(schema: &SchemaDescriptor) -> Vec<DeletionTriggerInfo> {
    use crate::schema::SearchBackend;

    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction || t.signals.is_empty() {
            continue;
        }

        let qname = format!("{}::{}", t.module, t.name);
        let qname_literal = format!("'{}'", qname.replace('\'', "''"));
        let tbl_qname = qn(&t.module, &t.table);

        // Scope the trigger's event list to exactly the operations at
        // least one registered handler cares about (On.Insert=1,
        // On.Update=2, On.Delete=4) — a type with only an On.Insert
        // handler shouldn't also capture (and force the dispatcher to
        // drain) Update/Delete rows nobody asked for.
        let combined_on = t.signals.iter().fold(0u8, |acc, s| acc | s.on);
        let mut events = Vec::new();
        if combined_on & 1 != 0 {
            events.push("INSERT");
        }
        if combined_on & 2 != 0 {
            events.push("UPDATE");
        }
        if combined_on & 4 != 0 {
            events.push("DELETE");
        }
        let events_str = events.join(" OR ");

        // Columns that hold Pylon-maintained index state rather than
        // actual business data — a `VectorIndex`'s embedding column (an
        // ordinary column the vector worker writes back to asynchronously
        // after re-embedding, confirmed live: an unrelated property update
        // enqueues a re-embed job, and the worker's own `UPDATE` on that
        // column fires this same trigger a second time) and a Postgres
        // `SearchIndex`'s generated tsvector column (always recomputed in
        // lock-step with the columns it's derived from, so excluding it
        // never masks a real change, it's just consistent with the vector
        // case). Skipping these keeps a signal handler's `UPDATE` firing
        // scoped to changes a caller actually made, not Pylon's own
        // index-maintenance side effects on the same row.
        let index_cols: Vec<String> = t
            .vector_indexes
            .iter()
            .map(|vi| vi.column_name())
            .chain(
                t.search_indexes
                    .iter()
                    .filter(|si| si.backend == SearchBackend::Postgres)
                    .map(|si| si.column_name()),
            )
            .collect();

        let update_guard = if combined_on & 2 != 0 && !index_cols.is_empty() {
            let strip: String = index_cols
                .iter()
                .map(|c| format!(" - '{}'", c.replace('\'', "''")))
                .collect();
            format!(
                "    IF TG_OP = 'UPDATE' AND (to_jsonb(OLD){strip}) = (to_jsonb(NEW){strip}) THEN\n        \
                 RETURN NULL;\n    \
                 END IF;\n"
            )
        } else {
            String::new()
        };

        let body = format!(
            "{update_guard}    INSERT INTO _pylon.\"SignalOutbox\" (type_name, operation, old_row, new_row)\n    \
             VALUES (\n        \
             {qname_literal},\n        \
             TG_OP,\n        \
             CASE WHEN TG_OP IN ('UPDATE', 'DELETE') THEN to_jsonb(OLD) ELSE NULL END,\n        \
             CASE WHEN TG_OP IN ('INSERT', 'UPDATE') THEN to_jsonb(NEW) ELSE NULL END\n    \
             );"
        );

        // `events_str` joins the hash too: the trigger's event list is part
        // of the emitted `CREATE TRIGGER`, so a type gaining an `On.Delete`
        // handler has to re-emit even though the body is unchanged.
        let hash = fnv(&[&t.table, "signal", &events_str, &body]);
        let fname = format!("{}_signal_{}", t.table, &hash[..8]);
        let fn_qname = qn(&t.module, &fname);

        let mut ddl = String::new();
        emit_after_mutation_trigger(&fn_qname, &qi(&fname), &tbl_qname, &events_str, &body, &mut ddl);
        result.push(DeletionTriggerInfo {
            table_module: t.module.clone(),
            table_name: t.table.clone(),
            trigger_name: fname,
            ddl,
        });
    }
    result
}

fn emit_signal_triggers(schema: &SchemaDescriptor, out: &mut String) {
    for info in signal_trigger_infos(schema) {
        out.push_str(&info.ddl);
    }
}

/// The column a composite constraint's pointer name refers to.
///
/// A link stores its value in `{name}_id`, not `{name}`. The single-pointer
/// paths already append that suffix; the composite ones used to pass the
/// pointer name straight through, so an `Exclusive(("order", "parent"))` over a
/// link called `parent` produced `OLD."parent"` in its trigger and
/// `("parent")` in its unique index — neither of which is a real column.
fn constraint_column(t: &TypeDescriptor, pointer: &str) -> String {
    if t.links.iter().any(|l| l.name == pointer && !l.is_junction_backed()) {
        format!("{}_id", pointer)
    } else {
        pointer.to_string()
    }
}

// ── Phase 7: unique indexes ────────────────────────────────────────────────────

fn emit_unique_indexes(schema: &SchemaDescriptor, out: &mut String) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        let qname = qn(&t.module, &t.table);

        for p in &t.properties {
            if p.is_exclusive && !p.is_pk {
                out.push_str(&format!("CREATE UNIQUE INDEX ON {} ({});\n", qname, qi(&p.name),));
                emitted = true;
            }
        }
        for l in &t.links {
            // A junction-backed exclusive link has no `{name}_id` column to
            // index — its uniqueness is a `UNIQUE (target)` constraint on
            // the junction table itself, emitted by `emit_one_junction_table`.
            if l.is_exclusive && !l.is_junction_backed() {
                out.push_str(&format!(
                    "CREATE UNIQUE INDEX ON {} ({});\n",
                    qname,
                    qi(&format!("{}_id", l.name)),
                ));
                emitted = true;
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive {
                pointers: fields,
                unless,
            } = c
            {
                let cols: Vec<String> = fields.iter().map(|f| qi(&constraint_column(t, f))).collect();
                let where_clause = unless
                    .as_deref()
                    .map(|u| format!(" WHERE NOT ({})", u))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "CREATE UNIQUE INDEX ON {} ({}){};\n",
                    qname,
                    cols.join(", "),
                    where_clause,
                ));
                emitted = true;
            }
        }
    }
    if emitted {
        out.push('\n');
    }
}

// ── Phase 8: check constraints ─────────────────────────────────────────────────

fn emit_check_constraints(schema: &SchemaDescriptor, out: &mut String) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        let qname = qn(&t.module, &t.table);

        for p in &t.properties {
            for expr in &p.check_constraints {
                let hash = fnv(&[&t.table, &p.name, expr.as_str()]);
                let cname = qi(&format!("{}_{}_{}_check", t.table, p.name, &hash[..8]));
                out.push_str(&format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});\n",
                    qname, cname, expr,
                ));
                emitted = true;
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Expression { expr } = c {
                let hash = fnv(&[&t.table, expr.as_str()]);
                let cname = qi(&format!("{}_{}_check", t.table, &hash[..8]));
                out.push_str(&format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({});\n",
                    qname, cname, expr,
                ));
                emitted = true;
            }
        }
    }
    if emitted {
        out.push('\n');
    }
}

// ── Phase 9: non-unique indexes ────────────────────────────────────────────────

fn emit_plain_indexes(schema: &SchemaDescriptor, out: &mut String) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        let qname = qn(&t.module, &t.table);

        for idx in &t.indexes {
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let body = if let Some(expr) = &idx.expression {
                format!("({})", expr)
            } else {
                let cols: Vec<String> = idx.pointers.iter().map(|f| qi(f)).collect();
                format!("({})", cols.join(", "))
            };
            let where_clause = idx
                .unless
                .as_deref()
                .map(|u| format!(" WHERE NOT ({})", u))
                .unwrap_or_default();
            out.push_str(&format!(
                "CREATE {}INDEX ON {} {}{};\n",
                unique, qname, body, where_clause,
            ));
            emitted = true;
        }
    }
    if emitted {
        out.push('\n');
    }
}

// ── Phase 10: trigger functions + triggers ─────────────────────────────────────

fn emit_triggers(schema: &SchemaDescriptor, out: &mut String) -> Result<(), PyQLError> {
    for info in user_trigger_infos(schema)? {
        out.push_str(&info.ddl);
    }
    Ok(())
}

/// The correct plpgsql `RETURN` statement for a trigger function, given its
/// `timing`/`on` — the return value is entirely ignored for an `AFTER`
/// trigger (`RETURN NULL;` is always safe, and sidesteps `NEW` being
/// unassigned on a pure `DELETE`, which would otherwise be a runtime error
/// the moment the trigger fires). A `BEFORE`/`INSTEAD OF` trigger's return
/// value *is* significant (it's what actually gets persisted/considered
/// "the operation"), so it must return whichever of `NEW`/`OLD` is defined
/// for the event that actually fired — `NEW` is never assigned during a
/// pure `DELETE`, and `OLD` is never assigned during a pure `INSERT`.
fn trigger_return_statement(timing: &str, on: u8) -> &'static str {
    if timing == "After" {
        return "RETURN NULL;";
    }
    let has_delete = on & 4 != 0;
    let has_insert_or_update = on & (1 | 2) != 0;
    match (has_delete, has_insert_or_update) {
        (true, true) => "IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;",
        (true, false) => "RETURN OLD;",
        _ => "RETURN NEW;",
    }
}

/// The stable, hash-derived name a user-declared `Trigger` will get, given
/// its owning table and its own content — shared by `user_trigger_infos`
/// (which needs it to build DDL) and `user_trigger_names` (which needs
/// only the name, not the DDL, and stays infallible because of it).
fn trigger_ddl_name(table: &str, trig: &crate::schema::TriggerDescriptor) -> String {
    let hash = fnv(&[table, &trig.on.to_string(), trig.timing.as_str(), trig.handler.as_str()]);
    format!("{table}_{}", &hash[..12])
}

/// Just the `(module, table, trigger_name)` every user-declared `Trigger`
/// should produce — no PyQL compilation, so (unlike `user_trigger_infos`)
/// this is infallible and cheap enough for `diff::expected_triggers` to
/// call on every diff/snapshot, not just when a trigger is actually being
/// added.
pub fn user_trigger_names(schema: &SchemaDescriptor) -> Vec<(String, String, String)> {
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        for trig in &t.triggers {
            result.push((t.module.clone(), t.table.clone(), trigger_ddl_name(&t.table, trig)));
        }
    }
    result
}

/// One `CREATE FUNCTION` + `CREATE TRIGGER` pair per user-declared schema
/// `Trigger`. Public entry point for `diff/mod.rs`, mirroring
/// `deletion_policy_trigger_infos`/`signal_trigger_infos` — the single
/// source of truth both `export_schema` and migration-diff drift detection
/// consult, so they can't drift apart the way cache/signal triggers once
/// did (see `diff::expected_triggers`'s own doc comment).
///
/// Every statement `query::compile` produces ends in a `RETURNING (...) AS
/// result` (or is a bare `SELECT`) — required so a normal PyQL caller can
/// decode a result row, but it means plpgsql refuses to run it as a bare
/// statement ("query has no destination for result data"). The generated
/// function declares a throwaway `record` local and appends `INTO
/// _pylon_trigger_result` to swallow it — a non-`STRICT` `INTO` is fine
/// with any row count (0, 1, or many), matching "fire and forget" exactly.
pub fn user_trigger_infos(schema: &SchemaDescriptor) -> Result<Vec<DeletionTriggerInfo>, PyQLError> {
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction {
            continue;
        }
        let table_qname = qn(&t.module, &t.table);
        let type_name = format!("{}::{}", t.module, t.name);

        for trig in &t.triggers {
            let fname = trigger_ddl_name(&t.table, trig);
            let fn_qname = qn(&t.module, &fname);
            let events = trigger_events(trig.on);
            let timing = trigger_timing(&trig.timing);
            let return_stmt = trigger_return_statement(&trig.timing, trig.on);

            let body_sql =
                crate::query::compile_trigger_handler(&trig.handler, &type_name, trig.on, schema).map_err(|e| {
                    let msg = format!("error in trigger handler for '{type_name}': {e}");
                    PyQLError::Fragment(PyQLFragmentError {
                        message: msg,
                        context: type_name.clone(),
                        position: crate::error::Position { line: 0, col: 0 },
                    })
                })?;
            let body_sql = body_sql.trim_end_matches(';');

            let ddl = format!(
                "CREATE OR REPLACE FUNCTION {fn_qname}()\n\
                 RETURNS trigger LANGUAGE plpgsql AS $$\n\
                 DECLARE\n\
                 \t_pylon_trigger_result record;\n\
                 BEGIN\n\
                 {body_sql} INTO _pylon_trigger_result;\n\
                 {return_stmt}\n\
                 END;\n\
                 $$;\n\n\
                 CREATE OR REPLACE TRIGGER {}\n\
                 {timing} {events} ON {table_qname}\n\
                 FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n",
                qi(&fname),
            );

            result.push(DeletionTriggerInfo {
                table_module: t.module.clone(),
                table_name: t.table.clone(),
                trigger_name: fname,
                ddl,
            });
        }
    }
    Ok(result)
}

/// Return `CREATE OR REPLACE VIEW` DDL for every interface type in `schema`.
pub fn interface_view_ddl(schema: &SchemaDescriptor) -> Vec<String> {
    interface_view_ddl_with_names(schema)
        .into_iter()
        .map(|(_, _, ddl)| ddl)
        .collect()
}

/// Like `interface_view_ddl` but also returns the module and view name for each entry.
pub fn interface_view_ddl_with_names(schema: &SchemaDescriptor) -> Vec<(String, String, String)> {
    let mut implementors: HashMap<String, Vec<&TypeDescriptor>> = HashMap::new();
    for t in &schema.types {
        if !t.abstract_ {
            for iface in &t.interfaces {
                implementors.entry(iface.clone()).or_default().push(t);
            }
        }
    }
    let mut result = Vec::new();
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) {
            continue;
        }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() {
            continue;
        }
        let mut ddl = String::new();
        emit_one_interface_view(t, impls, &mut ddl);
        let ddl = ddl.trim().to_string();
        if !ddl.is_empty() {
            result.push((t.module.clone(), t.name.clone(), ddl));
        }
    }
    result
}

/// The view name a junction-backed exclusive link's cross-implementor
/// helper view gets — `"{iface_table}.{link_name}"`, matching the same
/// `{table}.{name}` convention an ordinary junction table already uses
/// (e.g. `"Product.tags"`), just one level up (per-interface instead of
/// per-implementor).
fn interface_junction_view_name(iface_table: &str, link_name: &str) -> String {
    format!("{}.{}", iface_table, link_name)
}

/// Union views over each implementor's junction table, one per (interface,
/// multi-link or junction-backed link).
///
/// A multi-link's rows never live on the owner row — they live one level down
/// in a junction table, and because abstract pointers are flattened, *every*
/// implementor gets its own (`emit_one_junction_table`'s
/// `jt_name = "{impl.table}.{name}"`). Nothing physical therefore exists at the
/// interface's own `"{iface.table}.{name}"`, which is exactly the relation the
/// query compiler addresses when a multi-link is traversed from a polymorphic
/// root — `select Account { emails }` compiled to SQL against
/// `"Account.emails"` and failed with `relation does not exist`. These views
/// supply it.
///
/// Also what `make_excl_junction_info` queries for cross-implementor
/// exclusivity, which is the narrower case this started as.
pub fn interface_junction_view_ddl_with_names(schema: &SchemaDescriptor) -> Vec<(String, String, String)> {
    let implementors = interface_implementors(schema);
    let mut result = Vec::new();
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) {
            continue;
        }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() {
            continue;
        }

        // Every junction-shaped pointer on the interface: multi-links always,
        // single links only when a `through()` type puts them in a junction.
        let pointers: Vec<(&str, Option<&str>)> = t
            .multilinks
            .iter()
            .map(|ml| (ml.name.as_str(), ml.through.as_deref()))
            .chain(
                t.links
                    .iter()
                    .filter(|l| l.is_junction_backed())
                    .map(|l| (l.name.as_str(), l.through.as_deref())),
            )
            .collect();

        for (link_name, through) in pointers {
            // A `through()` type's own properties are real columns on each
            // junction table, so the view has to carry them or `@propname`
            // would be unreachable through the interface.
            let mut columns = vec!["source".to_string(), "target".to_string()];
            if let Some(through_qname) = through
                && let Some(td) = schema
                    .types
                    .iter()
                    .find(|td| format!("{}::{}", td.module, td.name) == through_qname && td.junction)
            {
                columns.extend(td.properties.iter().filter(|p| p.name != "id").map(|p| qi(&p.name)));
            }
            let column_list = columns.join(", ");

            let view_name = interface_junction_view_name(&t.table, link_name);
            let selects: Vec<String> = impls
                .iter()
                .map(|impl_t| {
                    let jt_name = format!("{}.{}", impl_t.table, link_name);
                    format!("    SELECT {} FROM {}", column_list, qn(&impl_t.module, &jt_name))
                })
                .collect();
            let ddl = format!(
                "CREATE VIEW {} AS\n{};",
                qn(&t.module, &view_name),
                selects.join("\n    UNION ALL\n"),
            );
            result.push((t.module.clone(), view_name, ddl));
        }
    }
    result
}

fn emit_interface_junction_views(schema: &SchemaDescriptor, out: &mut String) {
    for (_, _, ddl) in interface_junction_view_ddl_with_names(schema) {
        out.push_str(&ddl);
        out.push_str("\n\n");
    }
}

/// Return `CREATE OR REPLACE FUNCTION` DDL for every user-defined function in `schema`.
pub fn function_ddl(schema: &SchemaDescriptor) -> Result<Vec<String>, crate::error::PyQLError> {
    function_ddl_with_names(schema).map(|v| v.into_iter().map(|(_, _, ddl)| ddl).collect())
}

/// Like `function_ddl` but also returns the module and function name for each entry.
pub fn function_ddl_with_names(
    schema: &SchemaDescriptor,
) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema
        .functions
        .iter()
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

/// DDL for scalar (non-object-returning) functions only — safe to emit before tables.
pub fn scalar_function_ddl_with_names(
    schema: &SchemaDescriptor,
) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema
        .functions
        .iter()
        .filter(|fd| !fd.return_is_object)
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

/// DDL for object-returning functions only — must be emitted after tables exist.
pub fn object_function_ddl_with_names(
    schema: &SchemaDescriptor,
) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema
        .functions
        .iter()
        .filter(|fd| fd.return_is_object)
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

// ── Phase 11: interface views ──────────────────────────────────────────────────

fn emit_one_interface_view(t: &TypeDescriptor, impls: &[&TypeDescriptor], out: &mut String) {
    let cols: Vec<String> = t
        .properties
        .iter()
        .map(|p| qi(&p.name))
        .chain(
            t.links
                .iter()
                .filter(|l| !l.is_junction_backed())
                .map(|l| qi(&format!("{}_id", l.name))),
        )
        .collect();
    let col_list = cols.join(", ");
    let selects: Vec<String> = impls
        .iter()
        .map(|impl_t| format!("    SELECT {} FROM {}", col_list, qn(&impl_t.module, &impl_t.table)))
        .collect();
    out.push_str(&format!("CREATE VIEW {} AS\n", qn(&t.module, &t.table)));
    out.push_str(&selects.join("\n    UNION ALL\n"));
    out.push_str(";\n\n");
}

fn emit_interface_views(schema: &SchemaDescriptor, out: &mut String) {
    let implementors = interface_implementors(schema);
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) {
            continue;
        }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() {
            continue;
        }
        emit_one_interface_view(t, impls, out);
    }
}

// ── Phase 11.5: interface exclusive constraint triggers ────────────────────────

/// Structured description of one cross-table exclusive constraint trigger group.
/// Used by the diff engine to detect added/removed triggers without re-parsing DDL.
pub struct ExclTriggerInfo {
    pub fn_module: String,
    pub fn_name: String,
    pub fn_ddl: String,
    pub impl_module: String,
    pub impl_table: String,
    pub ins_trigger_name: String,
    pub ins_ddl: String,
    pub upd_trigger_name: String,
    pub upd_ddl: String,
}

fn excl_fn_name(iface_table: &str, fields: &[String]) -> String {
    format!("_excl_{}_{}", iface_table, fields.join("_"))
}

fn make_excl_info(
    iface: &TypeDescriptor,
    fields: &[String],
    columns: &[String],
    impl_t: &TypeDescriptor,
) -> ExclTriggerInfo {
    let fn_name = excl_fn_name(&iface.table, fields);
    let fn_qname = format!("{}.{}", pg_schema(&iface.module), qi(&fn_name));
    let view_qname = qn(&iface.module, &iface.table);
    let tbl_qname = qn(&impl_t.module, &impl_t.table);

    let field_conds: Vec<String> = columns.iter().map(|c| format!("{} = NEW.{}", qi(c), qi(c))).collect();
    let where_clause = format!("{} AND \"id\" <> NEW.\"id\"", field_conds.join(" AND "));

    let detail_keys = columns.join(", ");
    let detail_vals = columns
        .iter()
        .map(|c| format!("NEW.{}::text", qi(c)))
        .collect::<Vec<_>>()
        .join(" || ', ' || ");

    let fn_ddl = format!(
        "CREATE OR REPLACE FUNCTION {}()\n\
         RETURNS trigger LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
           IF EXISTS (\n\
             SELECT 1 FROM {}\n\
             WHERE {}\n\
           ) THEN\n\
             RAISE unique_violation\n\
               USING CONSTRAINT = '{}',\n\
                     DETAIL = format('Key ({})=(%s) already exists.', {});\n\
           END IF;\n\
           RETURN NEW;\n\
         END;\n\
         $$;",
        fn_qname, view_qname, where_clause, fn_name, detail_keys, detail_vals,
    );

    let ins_trigger_name = format!("{}_ins", fn_name);
    let upd_trigger_name = format!("{}_upd", fn_name);
    let of_cols = columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");
    let when_clause = columns
        .iter()
        .map(|c| format!("OLD.{} IS DISTINCT FROM NEW.{}", qi(c), qi(c)))
        .collect::<Vec<_>>()
        .join(" OR ");

    let ins_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER INSERT ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW EXECUTE FUNCTION {}();",
        qi(&ins_trigger_name),
        tbl_qname,
        fn_qname,
    );
    let upd_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER UPDATE OF {} ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW WHEN ({})\n\
         EXECUTE FUNCTION {}();",
        qi(&upd_trigger_name),
        of_cols,
        tbl_qname,
        when_clause,
        fn_qname,
    );

    ExclTriggerInfo {
        fn_module: iface.module.clone(),
        fn_name,
        fn_ddl,
        impl_module: impl_t.module.clone(),
        impl_table: impl_t.table.clone(),
        ins_trigger_name,
        ins_ddl,
        upd_trigger_name,
        upd_ddl,
    }
}

/// Like `make_excl_info`, but for a junction-backed exclusive link — the
/// value being deduplicated (`target`) lives in each implementor's own
/// separate junction table (`"{impl.table}.{link_name}"`), never on the
/// owner row itself, so both the trigger's own query (against
/// `interface_junction_view_ddl_with_names`'s helper view) and the constraint
/// trigger's attachment point (the junction table, not `impl_t` itself)
/// differ from the plain-property/plain-link case.
fn make_excl_junction_info(iface: &TypeDescriptor, link_name: &str, impl_t: &TypeDescriptor) -> ExclTriggerInfo {
    let fn_name = excl_fn_name(&iface.table, std::slice::from_ref(&link_name.to_string()));
    let fn_qname = format!("{}.{}", pg_schema(&iface.module), qi(&fn_name));
    let view_qname = qn(&iface.module, &interface_junction_view_name(&iface.table, link_name));
    let jt_name = format!("{}.{}", impl_t.table, link_name);
    let jt_qname = qn(&impl_t.module, &jt_name);

    let where_clause = "\"target\" = NEW.\"target\" AND \"source\" <> NEW.\"source\"";

    let fn_ddl = format!(
        "CREATE OR REPLACE FUNCTION {}()\n\
         RETURNS trigger LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
           IF EXISTS (\n\
             SELECT 1 FROM {}\n\
             WHERE {}\n\
           ) THEN\n\
             RAISE unique_violation\n\
               USING CONSTRAINT = '{}',\n\
                     DETAIL = format('Key (target)=(%s) already exists.', NEW.\"target\"::text);\n\
           END IF;\n\
           RETURN NEW;\n\
         END;\n\
         $$;",
        fn_qname, view_qname, where_clause, fn_name,
    );

    let ins_trigger_name = format!("{}_ins", fn_name);
    let upd_trigger_name = format!("{}_upd", fn_name);

    let ins_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER INSERT ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW EXECUTE FUNCTION {}();",
        qi(&ins_trigger_name),
        jt_qname,
        fn_qname,
    );
    let upd_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER UPDATE OF \"target\" ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW WHEN (OLD.\"target\" IS DISTINCT FROM NEW.\"target\")\n\
         EXECUTE FUNCTION {}();",
        qi(&upd_trigger_name),
        jt_qname,
        fn_qname,
    );

    ExclTriggerInfo {
        fn_module: iface.module.clone(),
        fn_name,
        fn_ddl,
        impl_module: impl_t.module.clone(),
        impl_table: jt_name,
        ins_trigger_name,
        ins_ddl,
        upd_trigger_name,
        upd_ddl,
    }
}

/// Collect all cross-table exclusive constraint trigger specs for `schema`.
///
/// Returns one `ExclTriggerInfo` per (interface exclusive constraint, concrete implementor).
/// The same `fn_name` may appear multiple times (once per implementor); callers should
/// deduplicate when emitting `CREATE OR REPLACE FUNCTION`.
pub fn interface_exclusive_trigger_infos(schema: &SchemaDescriptor) -> Vec<ExclTriggerInfo> {
    let mut implementors: HashMap<String, Vec<&TypeDescriptor>> = HashMap::new();
    for t in &schema.types {
        if !t.abstract_ {
            for iface in &t.interfaces {
                implementors.entry(iface.clone()).or_default().push(t);
            }
        }
    }

    let mut result = Vec::new();
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) {
            continue;
        }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() {
            continue;
        }

        for p in &t.properties {
            if !p.is_exclusive || p.is_pk {
                continue;
            }
            let fields = vec![p.name.clone()];
            for impl_t in impls {
                result.push(make_excl_info(t, &fields, &fields, impl_t));
            }
        }
        for l in &t.links {
            if !l.is_exclusive {
                continue;
            }
            // A junction-backed exclusive link has no `{name}_id` column
            // on the owner row — its value lives one level down, in each
            // implementor's own separate junction table (a `through()`
            // type only ever contributes extra property columns, never a
            // shared physical table — see `interface_junction_view_ddl_with_
            // names`'s own doc comment) — so it needs the junction-specific
            // helper view + trigger builder instead of the plain
            // object-column path every other exclusive pointer here uses.
            if l.is_junction_backed() {
                for impl_t in impls {
                    result.push(make_excl_junction_info(t, &l.name, impl_t));
                }
                continue;
            }
            let fields = vec![format!("{}_id", l.name)];
            for impl_t in impls {
                result.push(make_excl_info(t, &fields, &fields, impl_t));
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive { pointers: fields, .. } = c {
                let columns: Vec<String> = fields.iter().map(|f| constraint_column(t, f)).collect();
                for impl_t in impls {
                    result.push(make_excl_info(t, fields, &columns, impl_t));
                }
            }
        }
    }
    result
}

fn emit_interface_exclusive_triggers(schema: &SchemaDescriptor, out: &mut String) {
    use std::collections::HashSet;
    let mut fn_emitted: HashSet<String> = HashSet::new();
    for info in interface_exclusive_trigger_infos(schema) {
        if fn_emitted.insert(info.fn_name.clone()) {
            out.push_str(&info.fn_ddl);
            out.push_str("\n\n");
        }
        out.push_str(&info.ins_ddl);
        out.push('\n');
        out.push_str(&info.upd_ddl);
        out.push_str("\n\n");
    }
}

// ── Phase 12: user-defined functions ─────────────────────────────────────────

fn emit_scalar_functions(schema: &SchemaDescriptor, out: &mut String) -> Result<(), PyQLError> {
    for fd in schema.functions.iter().filter(|fd| !fd.return_is_object) {
        let ddl = emit_one_function(fd, schema)?;
        out.push_str(&ddl);
        out.push('\n');
    }
    Ok(())
}

fn emit_object_functions(schema: &SchemaDescriptor, out: &mut String) -> Result<(), PyQLError> {
    for fd in schema.functions.iter().filter(|fd| fd.return_is_object) {
        let ddl = emit_one_function(fd, schema)?;
        out.push_str(&ddl);
        out.push('\n');
    }
    Ok(())
}

fn emit_one_function(fd: &FunctionDescriptor, schema: &SchemaDescriptor) -> Result<String, PyQLError> {
    use crate::ir::compile_fn_body;
    use crate::sql::emit_fn_body;

    // Compile the body PyQL to an IR statement.
    let ir_output = compile_fn_body(fd, schema).map_err(|e| {
        let msg = format!("error in function '{}::{}' body: {}", fd.module, fd.name, e);
        PyQLError::Fragment(PyQLFragmentError {
            message: msg,
            context: format!("{}::{}", fd.module, fd.name),
            position: crate::error::Position { line: 0, col: 0 },
        })
    })?;

    // Emit the raw SQL body.
    let body_sql = emit_fn_body(&ir_output);

    // Parameter list: "name" pg_type, ...
    //
    // A body that reads a session global (or calls something that does) takes
    // the globals argument first — see `ir::compiler::GLOBALS_ARG`. It is added
    // only when needed rather than to every function, so the `id` default's
    // `generate_typed_id` and friends stay two-argument calls on the insert
    // path; the cost is that gaining or losing the *first* global in a body
    // changes the signature.
    let mut param_parts: Vec<String> = Vec::with_capacity(fd.params.len() + 1);
    if ir_output.uses_globals_arg {
        param_parts.push(format!("{} jsonb", qi(crate::ir::GLOBALS_ARG)));
    }
    param_parts.extend(fd.params.iter().map(|p| format!("{} {}", qi(&p.name), p.pg_type)));
    let params_sql = param_parts.join(", ");

    // RETURNS clause.
    let returns_sql = if fd.return_is_object {
        // Object-returning: look up the return type to build RETURNS TABLE columns.
        let type_columns = emit_fn_return_table(fd, schema);
        if fd.return_is_set {
            format!("TABLE({})", type_columns)
        } else {
            // Single-object return: PostgreSQL doesn't have a good way to express
            // "optional single row" in SQL functions, so use TABLE (LIMIT 1 in body).
            format!("TABLE({})", type_columns)
        }
    } else if fd.return_is_set {
        format!("SETOF {}", fd.return_pg_type)
    } else {
        fd.return_pg_type.clone()
    };

    let volatility_kw = match fd.volatility.as_str() {
        "immutable" => "IMMUTABLE",
        "stable" => "STABLE",
        _ => "VOLATILE",
    };

    Ok(format!(
        "CREATE OR REPLACE FUNCTION {fn_name}({params})\nRETURNS {returns}\nLANGUAGE SQL {vol}\nAS $$\n    {body}\n$$;\n",
        fn_name = qn(&fd.module, &fd.name),
        params = params_sql,
        returns = returns_sql,
        vol = volatility_kw,
        body = body_sql,
    ))
}

fn emit_fn_return_table(fd: &FunctionDescriptor, schema: &SchemaDescriptor) -> String {
    let type_name = &fd.return_pg_type; // qualified type name for object returns
    let td = schema
        .types
        .iter()
        .find(|t| format!("{}::{}", t.module, t.name) == *type_name);
    let Some(td) = td else {
        return "__type__ text, id uuid".to_string();
    };

    let mut cols: Vec<String> = Vec::new();
    if fd.return_is_polymorphic {
        cols.push("__type__ text".to_string());
    }
    for p in &td.properties {
        let pg_type = p.pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(&p.pg_type);
        cols.push(format!("{} {}", qi(&p.name), pg_type));
    }
    for l in &td.links {
        if l.is_junction_backed() {
            continue;
        }
        cols.push(format!("{} uuid", qi(&format!("{}_id", l.name))));
    }
    cols.join(", ")
}

// ── Phase 13: vector embedding columns ────────────────────────────────────────

fn emit_vector_columns(schema: &SchemaDescriptor, out: &mut String) {
    for td in &schema.types {
        if td.abstract_ || td.vector_indexes.is_empty() {
            continue;
        }
        for vi in &td.vector_indexes {
            out.push_str(&format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} vector({});\n",
                qn(&td.module, &td.table),
                qi(&vi.column_name()),
                vi.dimensions,
            ));
        }
    }
    if schema
        .types
        .iter()
        .any(|t| !t.abstract_ && !t.vector_indexes.is_empty())
    {
        out.push('\n');
    }
}

// ── Phase 14: vector HNSW indexes ─────────────────────────────────────────────

fn emit_vector_indexes(schema: &SchemaDescriptor, out: &mut String) {
    for td in &schema.types {
        if td.abstract_ || td.vector_indexes.is_empty() {
            continue;
        }
        for vi in &td.vector_indexes {
            let index_name = match &vi.index_name {
                None => format!("{}__vector__", td.table),
                Some(name) => format!("{}__vector_{}__", td.table, name),
            };
            out.push_str(&format!(
                "CREATE INDEX IF NOT EXISTS {} ON {} USING hnsw ({} {});\n",
                qi(&index_name),
                qn(&td.module, &td.table),
                qi(&vi.column_name()),
                vi.ops_class(),
            ));
        }
    }
}

// ── Phase 15: search tsvector generated columns (Postgres backend) ─────────────

fn emit_search_columns(schema: &SchemaDescriptor, out: &mut String) {
    use crate::schema::SearchBackend;

    let mut emitted = false;
    for td in &schema.types {
        if td.abstract_ {
            continue;
        }
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }

            // Build: setweight(to_tsvector('english', coalesce(col, '')), 'W') || ...
            let parts: Vec<String> = si
                .pointers
                .iter()
                .map(|sf| {
                    let col = qi(&sf.name);
                    let w = sf.weight.as_str();
                    format!("setweight(to_tsvector('english', coalesce({col}, '')), '{w}')")
                })
                .collect();

            let expr = if parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                parts.join(" || ")
            };

            out.push_str(&format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED;\n",
                qn(&td.module, &td.table),
                qi(&si.column_name()),
                expr,
            ));
            emitted = true;
        }
    }
    if emitted {
        out.push('\n');
    }
}

// ── Phase 16: search GIN indexes (Postgres backend) ───────────────────────────

fn emit_search_indexes(schema: &SchemaDescriptor, out: &mut String) {
    use crate::schema::SearchBackend;

    for td in &schema.types {
        if td.abstract_ {
            continue;
        }
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres {
                continue;
            }

            let col = si.column_name();
            let index_name = match &si.index_name {
                None => format!("{}__search__", td.table),
                Some(name) => format!("{}__search_{}__", td.table, name),
            };
            out.push_str(&format!(
                "CREATE INDEX IF NOT EXISTS {} ON {} USING gin ({});\n",
                qi(&index_name),
                qn(&td.module, &td.table),
                qi(&col),
            ));
        }
    }
}

// ── compile_index_fetch ────────────────────────────────────────────────────────

/// Build the SQL that fetches source text for a batch of objects to embed.
///
/// Returns a query of the form:
/// ```sql
/// SELECT id, concat_ws(E'\n', field1, field2::text) AS source_text
/// FROM "module"."table"
/// WHERE id = ANY($1::uuid[])
/// ```
///
/// Non-text source fields are cast to `::text`. The query is compiled once at
/// startup and cached; the worker runs it with the id array bound as `$1`.
pub fn compile_index_fetch(
    type_name: &str,
    index_name: Option<&str>,
    schema: &SchemaDescriptor,
) -> Result<String, PyQLError> {
    let td = schema
        .types
        .iter()
        .find(|t| format!("{}::{}", t.module, t.name) == type_name)
        .ok_or_else(|| {
            PyQLError::Fragment(PyQLFragmentError {
                message: format!("compile_index_fetch: unknown type '{}'", type_name),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    let vi = td
        .vector_indexes
        .iter()
        .find(|vi| vi.index_name.as_deref() == index_name)
        .ok_or_else(|| {
            let key = index_name.unwrap_or("<default>");
            PyQLError::Fragment(PyQLFragmentError {
                message: format!("compile_index_fetch: no vector index '{}' on type '{}'", key, type_name),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    let field_exprs = vi
        .pointers
        .iter()
        .map(|f| {
            // Resolve the field's pg_type to decide whether an explicit cast is needed.
            let pg_type = td
                .properties
                .iter()
                .find(|p| p.name == *f)
                .map(|p| p.pg_type.as_str())
                .unwrap_or("text");
            let col = qi(f);
            if pg_type == "text" {
                col
            } else {
                format!("{}::text", col)
            }
        })
        .collect::<Vec<_>>();

    let concat = if field_exprs.len() == 1 {
        field_exprs.into_iter().next().unwrap()
    } else {
        format!("concat_ws(E'\\n', {})", field_exprs.join(", "))
    };

    Ok(format!(
        "SELECT \"id\", {} AS source_text\nFROM {}\nWHERE \"id\" = ANY($1::uuid[])",
        concat,
        qn(&td.module, &td.table),
    ))
}

/// Like `compile_index_fetch` but for OpenSearch-backed SearchIndexes.
/// Returns SQL that fetches source-text fields for a batch of object IDs.
pub fn compile_search_index_fetch(
    type_name: &str,
    index_name: Option<&str>,
    schema: &SchemaDescriptor,
) -> Result<String, PyQLError> {
    let td = schema
        .types
        .iter()
        .find(|t| format!("{}::{}", t.module, t.name) == type_name)
        .ok_or_else(|| {
            PyQLError::Fragment(PyQLFragmentError {
                message: format!("compile_search_index_fetch: unknown type '{}'", type_name),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    use crate::schema::SearchBackend;
    let si = td
        .search_indexes
        .iter()
        .find(|si| si.index_name.as_deref() == index_name && si.backend != SearchBackend::Postgres)
        .ok_or_else(|| {
            let key = index_name.unwrap_or("<default>");
            PyQLError::Fragment(PyQLFragmentError {
                message: format!(
                    "compile_search_index_fetch: no remote SearchIndex '{}' on type '{}'",
                    key, type_name
                ),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    let field_exprs = si
        .pointers
        .iter()
        .map(|sf| {
            let pg_type = td
                .properties
                .iter()
                .find(|p| p.name == sf.name)
                .map(|p| p.pg_type.as_str())
                .unwrap_or("text");
            let col = qi(&sf.name);
            if pg_type == "text" {
                col
            } else {
                format!("{}::text", col)
            }
        })
        .collect::<Vec<_>>();

    let concat = if field_exprs.len() == 1 {
        field_exprs.into_iter().next().unwrap()
    } else {
        format!("concat_ws(E'\\n', {})", field_exprs.join(", "))
    };

    Ok(format!(
        "SELECT \"id\", {} AS source_text\nFROM {}\nWHERE \"id\" = ANY($1::uuid[])",
        concat,
        qn(&td.module, &td.table),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        DeleteAction, DeleteSide, FunctionDescriptor, FunctionParamDescriptor, LinkDescriptor, MultiLinkDescriptor,
        OnDeletePolicy, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
    };

    fn person_type() -> TypeDescriptor {
        TypeDescriptor {
            name: "Person".into(),
            module: "default".into(),
            table: "Person".into(),
            abstract_: false,
            materialized: false,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![
                PropertyDescriptor {
                    name: "id".into(),
                    pg_type: "uuid".into(),
                    nullable: false,
                    default_sql: Some("uuidv7()".into()),
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: true,
                    is_pk: true,
                    is_readonly: true,
                    rewrites: vec![],
                    tuple_members: None,
                    column_type: None,
                },
                PropertyDescriptor {
                    name: "age".into(),
                    pg_type: "int8".into(),
                    nullable: true,
                    default_sql: None,
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                    tuple_members: None,
                    column_type: None,
                },
            ],
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

    fn minimal_schema(fns: Vec<FunctionDescriptor>) -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![person_type()],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: fns,
            aliases: vec![],
            channels: vec![],
        }
    }

    // ── Polymorphic (interface) link targets ───────────────────────────────

    /// `Account` as an interface, `Individual`/`Organization` implementing it,
    /// plus whatever referencing types the caller supplies.
    fn interface_schema(referencing: Vec<TypeDescriptor>) -> SchemaDescriptor {
        fn bare(name: &str, interfaces: Vec<String>, abstract_: bool, materialized: bool) -> TypeDescriptor {
            TypeDescriptor {
                name: name.into(),
                module: "default".into(),
                table: name.into(),
                abstract_,
                materialized,
                description: None,
                parents: vec![],
                interfaces,
                properties: vec![],
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
        let mut types = vec![
            bare("Account", vec![], true, true),
            bare("Individual", vec!["default::Account".into()], false, false),
            bare("Organization", vec!["default::Account".into()], false, false),
        ];
        types.extend(referencing);
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

    fn link_to_account(name: &str, on_delete: Vec<OnDeletePolicy>, through: Option<String>) -> LinkDescriptor {
        LinkDescriptor {
            name: name.into(),
            target: "default::Account".into(),
            nullable: true,
            description: None,
            default_pyql: None,
            is_exclusive: false,
            is_readonly: false,
            rewrites: vec![],
            on_delete,
            through,
        }
    }

    fn referencing_type(
        name: &str,
        links: Vec<LinkDescriptor>,
        multilinks: Vec<MultiLinkDescriptor>,
    ) -> TypeDescriptor {
        TypeDescriptor {
            name: name.into(),
            module: "default".into(),
            table: name.into(),
            abstract_: false,
            materialized: false,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![],
            links,
            multilinks,
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

    #[test]
    fn test_multilink_on_an_interface_gets_a_union_view_over_the_implementors() {
        // The query compiler addresses `"Iface.link"` when a multi-link is
        // traversed from a polymorphic root, but the pointer is flattened so
        // only per-implementor junction tables physically exist. Without this
        // view `select Account { emails }` compiled fine and then failed at
        // runtime with `relation "Account.emails" does not exist`.
        let mut schema = interface_schema(vec![referencing_type("Email", vec![], vec![])]);
        for t in schema.types.iter_mut() {
            if t.name == "Account" || t.name == "Individual" || t.name == "Organization" {
                t.multilinks.push(MultiLinkDescriptor {
                    name: "emails".into(),
                    target: "default::Email".into(),
                    through: None,
                    nullable: true,
                    description: None,
                    default_pyql: None,
                    on_delete: vec![],
                });
            }
        }
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("CREATE VIEW \"public\".\"Account.emails\""),
            "no union view for the interface's multi-link, got:\n{}",
            ddl
        );
        for implementor in ["Individual", "Organization"] {
            assert!(
                ddl.contains(&format!("FROM \"public\".\"{}.emails\"", implementor)),
                "union view misses {}, got:\n{}",
                implementor,
                ddl
            );
        }
    }

    #[test]
    fn test_link_to_interface_emits_no_foreign_key() {
        // PostgreSQL rejects a FK whose referenced relation is a view, and an
        // interface is exactly that — so the link must carry no FK at all.
        let schema = interface_schema(vec![referencing_type(
            "Session",
            vec![link_to_account("account", vec![], None)],
            vec![],
        )]);
        let ddl = export_schema(&schema).unwrap();
        assert!(
            !ddl.contains("Session_account_fkey"),
            "a link targeting an interface must not get a FK, got:\n{}",
            ddl
        );
        assert!(ddl.contains("CREATE VIEW \"public\".\"Account\""), "got:\n{}", ddl);
    }

    #[test]
    fn test_link_to_interface_enforces_restrict_on_every_implementor() {
        let schema = interface_schema(vec![referencing_type(
            "Session",
            vec![link_to_account("account", vec![], None)],
            vec![],
        )]);
        let ddl = export_schema(&schema).unwrap();
        for implementor in ["Individual", "Organization"] {
            assert!(
                ddl.contains(&format!("BEFORE DELETE ON \"public\".\"{}\"", implementor)),
                "missing enforcement trigger on {}, got:\n{}",
                implementor,
                ddl
            );
        }
        assert!(ddl.contains("RAISE foreign_key_violation"), "got:\n{}", ddl);
        assert!(
            ddl.contains("SELECT 1 FROM \"public\".\"Session\" WHERE \"account_id\" = OLD.id"),
            "got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_interface_link_allow_nulls_the_column_but_clears_a_junction_row() {
        // `Allow` on a plain link is ON DELETE SET NULL; on a multi-link it is
        // the junction's ON DELETE CASCADE. The trigger stand-ins must keep
        // that distinction — both sides are a qualified name containing a dot,
        // so they can't be told apart by inspecting the table name.
        let allow = vec![OnDeletePolicy {
            side: DeleteSide::Target,
            action: DeleteAction::Allow,
        }];
        let schema = interface_schema(vec![
            referencing_type(
                "AuditEntry",
                vec![link_to_account("actor", allow.clone(), None)],
                vec![],
            ),
            referencing_type(
                "Watchlist",
                vec![],
                vec![MultiLinkDescriptor {
                    name: "watched".into(),
                    target: "default::Account".into(),
                    through: None,
                    nullable: true,
                    description: None,
                    default_pyql: None,
                    on_delete: allow,
                }],
            ),
        ]);
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("UPDATE \"public\".\"AuditEntry\" SET \"actor_id\" = NULL WHERE \"actor_id\" = OLD.id;"),
            "single link Allow must null the column, got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("DELETE FROM \"public\".\"Watchlist.watched\" WHERE \"target\" = OLD.id;"),
            "multi-link Allow must drop the junction row, got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_multilink_to_interface_junction_target_has_no_reference() {
        let schema = interface_schema(vec![referencing_type(
            "Watchlist",
            vec![],
            vec![MultiLinkDescriptor {
                name: "watched".into(),
                target: "default::Account".into(),
                through: None,
                nullable: true,
                description: None,
                default_pyql: None,
                on_delete: vec![],
            }],
        )]);
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("    target uuid NOT NULL,\n"),
            "junction target must be a bare uuid column, got:\n{}",
            ddl
        );
        assert!(
            !ddl.contains("target uuid NOT NULL REFERENCES \"public\".\"Account\""),
            "got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_source_side_cascade_deletes_from_implementors_not_the_view() {
        // `DELETE FROM` a UNION ALL view is not auto-updatable, so the cascade
        // has to name each implementor table.
        let schema = interface_schema(vec![referencing_type(
            "Session",
            vec![link_to_account(
                "account",
                vec![OnDeletePolicy {
                    side: DeleteSide::Source,
                    action: DeleteAction::DeleteTarget,
                }],
                None,
            )],
            vec![],
        )]);
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("DELETE FROM \"public\".\"Individual\" WHERE id = OLD.\"account_id\";"),
            "got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("DELETE FROM \"public\".\"Organization\" WHERE id = OLD.\"account_id\";"),
            "got:\n{}",
            ddl
        );
        assert!(
            !ddl.contains("DELETE FROM \"public\".\"Account\" WHERE id ="),
            "must never delete through the interface view, got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_link_to_concrete_type_still_gets_its_foreign_key() {
        // Guard against the suppression leaking onto ordinary links.
        let mut schema = interface_schema(vec![referencing_type(
            "Session",
            vec![LinkDescriptor {
                name: "owner".into(),
                target: "default::Individual".into(),
                nullable: true,
                description: None,
                default_pyql: None,
                is_exclusive: false,
                is_readonly: false,
                rewrites: vec![],
                on_delete: vec![],
                through: None,
            }],
            vec![],
        )]);
        schema.types.retain(|t| t.name != "Organization");
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("FOREIGN KEY (\"owner_id\") REFERENCES \"public\".\"Individual\"(id)"),
            "got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_emit_one_table_includes_cache_invalidate_trigger() {
        let mut out = String::new();
        emit_one_table(&person_type(), None, &mut out);
        assert!(
            out.contains("CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"Person\""),
            "got:\n{out}"
        );
    }

    /// A `default_pyql` expression has to reach the emitted DDL. It didn't
    /// before: this generator read only `default_sql`, so a
    /// `Default(std::uuid_generate_v7())` was stored, type-checked, and then
    /// silently dropped — while the migration path (`diff::resolve_default`)
    /// compiled it correctly. Two generators disagreeing about the same
    /// schema is worse than either behaviour alone, so they're pinned
    /// together here.
    #[test]
    fn test_emit_table_compiles_a_pyql_default() {
        let mut td = person_type();
        td.properties[1].default_sql = None;
        td.properties[1].default_pyql = Some("std::uuid_generate_v7()".into());
        td.properties[1].pg_type = "uuid".into();

        let schema = SchemaDescriptor {
            types: vec![td.clone()],
            ..minimal_schema(vec![])
        };

        let mut out = String::new();
        emit_one_table(&td, Some(&schema), &mut out);
        assert!(
            out.contains("\"age\" uuid NULL DEFAULT uuidv7()") || out.contains("DEFAULT uuidv7()"),
            "got:\n{out}"
        );
    }

    #[test]
    fn test_export_and_migration_agree_on_a_pyql_default() {
        let mut td = person_type();
        td.properties[1].default_sql = None;
        td.properties[1].default_pyql = Some("std::uuid_generate_v7()".into());
        td.properties[1].pg_type = "uuid".into();
        let schema = SchemaDescriptor {
            types: vec![td.clone()],
            ..minimal_schema(vec![])
        };

        let mut out = String::new();
        emit_one_table(&td, Some(&schema), &mut out);
        let from_migration = crate::diff::resolve_default_for_test(&td.properties[1], &schema);

        assert_eq!(from_migration.as_deref(), Some("uuidv7()"));
        assert!(
            out.contains(&format!("DEFAULT {}", from_migration.unwrap())),
            "export DDL disagrees with the migration path:\n{out}"
        );
    }

    fn trig(on: u8, timing: &str, handler: &str) -> crate::schema::TriggerDescriptor {
        crate::schema::TriggerDescriptor {
            on,
            timing: timing.into(),
            handler: handler.into(),
        }
    }

    fn schema_with_trigger(trigger: crate::schema::TriggerDescriptor) -> SchemaDescriptor {
        let mut t = person_type();
        t.triggers = vec![trigger];
        SchemaDescriptor {
            types: vec![t],
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
    fn test_trigger_new_anchor_resolves_to_new_alias() {
        // On.Insert = 1. Also documents the *non*-recursive boundary: this
        // handler updates Person (its own type), but the trigger only
        // fires on Insert, and Update isn't in that mask, so there's no
        // way this handler's own write could refire it.
        let schema = schema_with_trigger(trig(1, "After", "update Person set { age := __new__.age }"));
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("NEW.\"age\""), "got:\n{ddl}");
    }

    #[test]
    fn test_trigger_that_would_refire_itself_is_rejected() {
        // On.Insert = 1, handler inserts into its own type — every insert
        // would refire this same trigger, forever.
        let schema = schema_with_trigger(trig(1, "After", "insert Person { age := __new__.age }"));
        let err = export_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("is recursive"), "got: {err}");
    }

    #[test]
    fn test_recursive_insert_wrapped_in_a_select_shape_is_still_caught() {
        // `select (insert Person {...}) { age }` — the DML lives in
        // `IrSelect::dml_source`, not as the top-level statement.
        let schema = schema_with_trigger(trig(
            1,
            "After",
            "select (insert Person { age := __new__.age }) { age }",
        ));
        let err = export_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("is recursive"), "got: {err}");
    }

    #[test]
    fn test_recursive_check_is_scoped_to_the_triggers_own_events() {
        // On.Delete = 4, handler *inserts* into its own type — not
        // recursive, since an insert can never refire a Delete-only
        // trigger (contrast with the Insert-only case above).
        let schema = schema_with_trigger(trig(4, "After", "insert Person { age := __old__.age }"));
        assert!(export_schema(&schema).is_ok());
    }

    #[test]
    fn test_trigger_old_anchor_resolves_to_old_alias() {
        // On.Delete = 4
        let schema = schema_with_trigger(trig(4, "After", "update Person set { age := __old__.age }"));
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("OLD.\"age\""), "got:\n{ddl}");
    }

    #[test]
    fn test_trigger_update_can_reference_both_new_and_old() {
        // On.Update = 2 — a schema-bound but DML-free handler (a filter
        // expression, not `update Person set {...}`, which would be
        // genuinely self-recursive for an Update-only trigger and get
        // rejected by the recursion check below).
        let schema = schema_with_trigger(trig(2, "After", "select Person filter (__new__.age = __old__.age)"));
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("NEW.\"age\""), "got:\n{ddl}");
        assert!(ddl.contains("OLD.\"age\""), "got:\n{ddl}");
    }

    #[test]
    fn test_trigger_insert_only_cannot_reference_old() {
        let schema = schema_with_trigger(trig(1, "After", "update Person set { age := __old__.age }"));
        let err = export_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("__old__ cannot be used"), "got: {err}");
    }

    #[test]
    fn test_trigger_delete_only_cannot_reference_new() {
        let schema = schema_with_trigger(trig(4, "After", "update Person set { age := __new__.age }"));
        let err = export_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("__new__ cannot be used"), "got: {err}");
    }

    #[test]
    fn test_trigger_combined_insert_update_cannot_reference_old() {
        // On.Insert | On.Update = 3 — __new__ legal, __old__ still isn't.
        let ok = schema_with_trigger(trig(3, "After", "select Person filter (__new__.age > 0)"));
        assert!(export_schema(&ok).is_ok());

        let bad = schema_with_trigger(trig(3, "After", "select Person filter (__old__.age > 0)"));
        let err = export_schema(&bad).unwrap_err();
        assert!(err.to_string().contains("__old__ cannot be used"), "got: {err}");
    }

    #[test]
    fn test_trigger_after_timing_returns_null() {
        let schema = schema_with_trigger(trig(1, "After", "update Person set { age := __new__.age }"));
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("RETURN NULL;"), "got:\n{ddl}");
    }

    #[test]
    fn test_trigger_before_timing_returns_new_or_old_appropriately() {
        let insert_only = schema_with_trigger(trig(1, "Before", "update Person set { age := __new__.age }"));
        let ddl = export_schema(&insert_only).unwrap();
        assert!(
            ddl.contains("RETURN NEW;"),
            "insert-only Before should return NEW, got:\n{ddl}"
        );

        let delete_only = schema_with_trigger(trig(4, "Before", "update Person set { age := __old__.age }"));
        let ddl = export_schema(&delete_only).unwrap();
        assert!(
            ddl.contains("RETURN OLD;"),
            "delete-only Before should return OLD, got:\n{ddl}"
        );

        // On.Insert | On.Delete = 5 — combined with Delete needs the conditional.
        // Neither anchor is legal for this combination (see
        // `compile_trigger_handler`'s doc comment), so the handler here
        // deliberately references neither.
        let combined = schema_with_trigger(trig(5, "Before", "update Person set { age := 1 }"));
        let ddl = export_schema(&combined).unwrap();
        assert!(
            ddl.contains("IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;"),
            "got:\n{ddl}"
        );
    }

    #[test]
    fn test_multiple_triggers_get_separate_functions() {
        let mut t = person_type();
        t.triggers = vec![
            trig(1, "After", "update Person set { age := __new__.age }"),
            trig(4, "Before", "update Person set { age := __old__.age }"),
        ];
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
        let ddl = export_schema(&schema).unwrap();
        let fn_count = ddl.matches("CREATE OR REPLACE FUNCTION \"public\".\"Person_").count();
        assert_eq!(fn_count, 2, "expected one function per Trigger(...), got:\n{ddl}");
    }

    #[test]
    fn test_emit_scalar_function_ddl() {
        let fd = FunctionDescriptor {
            name: "mysum".into(),
            module: "math".into(),
            params: vec![
                FunctionParamDescriptor {
                    name: "a".into(),
                    pg_type: "int8".into(),
                },
                FunctionParamDescriptor {
                    name: "b".into(),
                    pg_type: "int8".into(),
                },
            ],
            return_pg_type: "int8".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
            body: "a + b".into(),
        };
        let schema = minimal_schema(vec![fd.clone()]);
        let ddl = emit_one_function(&fd, &schema).unwrap();
        assert!(
            ddl.contains("CREATE OR REPLACE FUNCTION \"math\".\"mysum\""),
            "got:\n{}",
            ddl
        );
        assert!(ddl.contains("\"a\" int8, \"b\" int8"), "got:\n{}", ddl);
        assert!(ddl.contains("RETURNS int8"), "got:\n{}", ddl);
        assert!(ddl.contains("IMMUTABLE"), "got:\n{}", ddl);
        assert!(ddl.contains("SELECT"), "got:\n{}", ddl);
    }

    #[test]
    fn test_emit_setof_function_ddl() {
        let fd = FunctionDescriptor {
            name: "counters".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "int8".into(),
            return_is_object: false,
            return_is_set: true,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "1".into(),
        };
        let schema = minimal_schema(vec![fd.clone()]);
        let ddl = emit_one_function(&fd, &schema).unwrap();
        assert!(ddl.contains("RETURNS SETOF int8"), "got:\n{}", ddl);
        assert!(ddl.contains("STABLE"), "got:\n{}", ddl);
    }

    #[test]
    fn test_emit_object_function_ddl() {
        let fd = FunctionDescriptor {
            name: "adults".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "default::Person".into(),
            return_is_object: true,
            return_is_set: true,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "select Person filter .age > 18".into(),
        };
        let schema = minimal_schema(vec![fd.clone()]);
        let ddl = emit_one_function(&fd, &schema).unwrap();
        assert!(
            ddl.contains("CREATE OR REPLACE FUNCTION \"public\".\"adults\"()"),
            "got:\n{}",
            ddl
        );
        assert!(ddl.contains("RETURNS TABLE("), "got:\n{}", ddl);
        assert!(ddl.contains("\"id\" uuid"), "got:\n{}", ddl);
        assert!(ddl.contains("\"age\" int8"), "got:\n{}", ddl);
        assert!(ddl.contains("STABLE"), "got:\n{}", ddl);
        assert!(ddl.contains("SELECT * FROM"), "got:\n{}", ddl);
    }

    #[test]
    fn test_object_function_can_reference_its_own_parameter() {
        // An object-returning body compiles its filter against a schema anchor,
        // which used not to consult `fn_params` — so the bare `min_age` was
        // rejected as "absolute paths are not valid in expression context",
        // while the identical reference in a scalar-returning body worked.
        let fd = FunctionDescriptor {
            name: "older_than".into(),
            module: "default".into(),
            params: vec![FunctionParamDescriptor {
                name: "min_age".into(),
                pg_type: "int8".into(),
            }],
            return_pg_type: "default::Person".into(),
            return_is_object: true,
            return_is_set: true,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "select Person filter .age > min_age".into(),
        };
        let schema = minimal_schema(vec![fd.clone()]);
        let ddl = emit_one_function(&fd, &schema).unwrap();
        assert!(
            ddl.contains("\"min_age\" int8"),
            "parameter missing from the signature, got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("\"min_age\")") || ddl.contains("= \"min_age\"") || ddl.contains("> \"min_age\""),
            "parameter not referenced in the body, got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_a_function_reading_a_global_takes_the_globals_argument() {
        // A session global is a query parameter everywhere else, which a
        // `CREATE FUNCTION` body cannot bind — it used to emit a bare `$1`.
        use crate::schema::GlobalDescriptor;
        let reader = FunctionDescriptor {
            name: "cutoff".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "timestamptz".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "global snapshot_at ?? datetime_of_transaction()".into(),
        };
        let plain = FunctionDescriptor {
            name: "bump".into(),
            module: "default".into(),
            params: vec![FunctionParamDescriptor {
                name: "n".into(),
                pg_type: "int8".into(),
            }],
            return_pg_type: "int8".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
            body: "n + 1".into(),
        };
        let mut schema = minimal_schema(vec![reader.clone(), plain.clone()]);
        schema.globals.push(GlobalDescriptor {
            name: "snapshot_at".into(),
            module: "default".into(),
            scalar_type: "datetime".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });

        let reader_ddl = emit_one_function(&reader, &schema).unwrap();
        assert!(
            reader_ddl.contains("\"__pylon_json_globals__\" jsonb"),
            "the global reader should take the argument, got:\n{}",
            reader_ddl
        );
        assert!(
            reader_ddl.contains("__pylon_json_globals__ ->> 'default::snapshot_at'"),
            "the body should read the global out of it, got:\n{}",
            reader_ddl
        );
        assert!(
            !reader_ddl.contains("$1"),
            "no parameter placeholder should survive, got:\n{}",
            reader_ddl
        );

        // A function with no globals anywhere in reach keeps its own signature
        // — this is the deliberate difference from the upstream engine, which adds the
        // argument to every function.
        let plain_ddl = emit_one_function(&plain, &schema).unwrap();
        assert!(
            !plain_ddl.contains("__pylon_json_globals__"),
            "a function that cannot reach a global should be untouched, got:\n{}",
            plain_ddl
        );
    }

    #[test]
    fn test_the_globals_argument_is_forwarded_to_a_callee_that_needs_it() {
        use crate::schema::GlobalDescriptor;
        let reader = FunctionDescriptor {
            name: "cutoff".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "timestamptz".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "global snapshot_at ?? datetime_of_transaction()".into(),
        };
        let caller = FunctionDescriptor {
            name: "is_past".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "bool".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: "cutoff() < datetime_of_transaction()".into(),
        };
        let mut schema = minimal_schema(vec![reader, caller.clone()]);
        schema.globals.push(GlobalDescriptor {
            name: "snapshot_at".into(),
            module: "default".into(),
            scalar_type: "datetime".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });

        // `is_past` reads no global itself; it needs the argument only because
        // what it calls does, which is why the analysis has to be a fixpoint.
        let ddl = emit_one_function(&caller, &schema).unwrap();
        assert!(
            ddl.contains("\"__pylon_json_globals__\" jsonb"),
            "a caller of a global reader needs the argument too, got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("cutoff\"((__pylon_json_globals__))") || ddl.contains("__pylon_json_globals__)"),
            "it should forward the argument, got:\n{}",
            ddl
        );
    }

    #[test]
    fn test_emit_sequence_scalar_ddl() {
        use crate::schema::ScalarDescriptor;
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![ScalarDescriptor {
                name: "OrderNumber".into(),
                module: "default".into(),
                base: "Sequence".into(),
                pg_type: "int8".into(),
                check_constraints: vec![],
                is_sequence: true,
            }],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("CREATE SEQUENCE \"public\".\"OrderNumber_seq\""),
            "got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("CREATE DOMAIN \"public\".\"OrderNumber\" AS int8"),
            "got:\n{}",
            ddl
        );
        // Sequence must precede domain in the output
        let seq_pos = ddl.find("CREATE SEQUENCE").unwrap();
        let dom_pos = ddl.find("CREATE DOMAIN").unwrap();
        assert!(seq_pos < dom_pos, "sequence must appear before domain");
    }

    #[test]
    fn test_registered_scalar_domain_is_used_as_the_column_type() {
        // A property typed with a *registered* custom scalar must get that
        // scalar's own DOMAIN as its actual column type (via
        // `PropertyDescriptor.column_type`) — not just a same-named,
        // never-referenced `CREATE DOMAIN` sitting next to a plain-base-type
        // column, which is what this looked like before this fix.
        use crate::schema::ScalarDescriptor;
        let schema = SchemaDescriptor {
            types: vec![TypeDescriptor {
                name: "Contact".into(),
                module: "default".into(),
                table: "Contact".into(),
                abstract_: false,
                materialized: true,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
                    name: "email".into(),
                    pg_type: "text".into(),
                    nullable: false,
                    default_sql: None,
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                    tuple_members: None,
                    column_type: Some("\"public\".\"EmailStr\"".into()),
                }],
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
            }],
            scalars: vec![ScalarDescriptor {
                name: "EmailStr".into(),
                module: "default".into(),
                base: "Str".into(),
                pg_type: "text".into(),
                check_constraints: vec!["value ~ '^[^@]+@[^@]+\\.[^@]+$'".into()],
                is_sequence: false,
            }],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("CREATE DOMAIN \"public\".\"EmailStr\" AS text\n    CHECK (value ~ '^[^@]+@[^@]+\\.[^@]+$')"),
            "got:\n{}",
            ddl
        );
        assert!(
            ddl.contains("\"email\" \"public\".\"EmailStr\" NOT NULL"),
            "column must use the domain type, not the plain base type — got:\n{}",
            ddl
        );
    }

    // ── interface cross-table exclusive constraint tests ──────────────────────

    fn account_interface_schema() -> SchemaDescriptor {
        let mut account = TypeDescriptor {
            name: "Account".into(),
            module: "default".into(),
            table: "Account".into(),
            abstract_: true,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![PropertyDescriptor {
                name: "email".into(),
                pg_type: "text".into(),
                nullable: false,
                default_sql: None,
                default_pyql: None,
                description: None,
                check_constraints: vec![],
                is_exclusive: true,
                is_pk: false,
                is_readonly: false,
                rewrites: vec![],
                tuple_members: None,
                column_type: None,
            }],
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
        };
        let mut individual = account.clone();
        individual.name = "Individual".into();
        individual.table = "Individual".into();
        individual.abstract_ = false;
        individual.materialized = true;
        individual.interfaces = vec!["default::Account".into()];
        let mut organization = individual.clone();
        organization.name = "Organization".into();
        organization.table = "Organization".into();
        account.constraints = vec![]; // interface itself has no table of its own to constrain
        SchemaDescriptor {
            types: vec![account, individual, organization],
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
    fn test_interface_exclusive_property_gets_per_table_index_and_cross_table_trigger() {
        let schema = account_interface_schema();
        let ddl = export_schema(&schema).unwrap();

        assert!(
            ddl.contains("CREATE UNIQUE INDEX ON \"public\".\"Individual\" (\"email\")"),
            "got:\n{ddl}"
        );
        assert!(
            ddl.contains("CREATE UNIQUE INDEX ON \"public\".\"Organization\" (\"email\")"),
            "got:\n{ddl}"
        );

        // One shared trigger function (not duplicated per implementor)...
        assert_eq!(
            ddl.matches("CREATE OR REPLACE FUNCTION \"public\".\"_excl_Account_email\"")
                .count(),
            1,
            "the trigger function must be emitted exactly once, shared by every implementor; got:\n{ddl}"
        );
        // ...checking the interface's own UNION-ALL view, not a single table.
        assert!(ddl.contains("SELECT 1 FROM \"public\".\"Account\""), "got:\n{ddl}");

        // ...but a constraint trigger attached to *each* implementor's own table.
        assert!(
            ddl.contains(
                "CREATE CONSTRAINT TRIGGER \"_excl_Account_email_ins\"\nAFTER INSERT ON \"public\".\"Individual\""
            ),
            "got:\n{ddl}"
        );
        assert!(
            ddl.contains(
                "CREATE CONSTRAINT TRIGGER \"_excl_Account_email_ins\"\nAFTER INSERT ON \"public\".\"Organization\""
            ),
            "got:\n{ddl}"
        );
        assert!(ddl.contains("DEFERRABLE INITIALLY DEFERRED"), "got:\n{ddl}");
        assert!(
            ddl.contains("CREATE CONSTRAINT TRIGGER \"_excl_Account_email_upd\"\nAFTER UPDATE OF \"email\""),
            "the UPDATE trigger must only fire when the exclusive column itself changes; got:\n{ddl}"
        );
    }

    #[test]
    fn test_junction_backed_exclusive_link_gets_a_cross_implementor_helper_view_and_trigger() {
        // A junction-backed exclusive link on an interface has no
        // `{name}_id` column on the owner row to check via the object
        // view — its own separate helper view (unioning `(source,
        // target)` across every implementor's own junction table) and
        // trigger, attached to each implementor's *junction* table (not
        // the owner table itself).
        use crate::schema::LinkDescriptor;
        let mut schema = account_interface_schema();
        for t in &mut schema.types {
            t.properties.retain(|p| p.name != "email");
            if t.name == "Account" || t.name == "Individual" || t.name == "Organization" {
                t.links.push(LinkDescriptor {
                    name: "owner".into(),
                    target: "default::Person".into(),
                    nullable: true,
                    through: Some("default::AccountOwner".into()),
                    description: None,
                    default_pyql: None,
                    is_exclusive: true,
                    is_readonly: false,
                    rewrites: vec![],
                    on_delete: vec![],
                });
            }
        }

        let views = interface_junction_view_ddl_with_names(&schema);
        assert_eq!(views.len(), 1, "expected exactly one helper view, got: {views:?}");
        let (view_module, view_name, view_ddl) = &views[0];
        assert_eq!(view_module, "default");
        assert_eq!(view_name, "Account.owner");
        assert!(
            view_ddl.contains("SELECT source, target FROM \"public\".\"Individual.owner\""),
            "got:\n{view_ddl}"
        );
        assert!(
            view_ddl.contains("SELECT source, target FROM \"public\".\"Organization.owner\""),
            "got:\n{view_ddl}"
        );

        let infos = interface_exclusive_trigger_infos(&schema);
        assert_eq!(
            infos.len(),
            2,
            "expected one entry per implementor, got: {}",
            infos.len()
        );
        assert!(
            infos.iter().any(|i| i.impl_table == "Individual.owner"
                && i.ins_ddl.contains("AFTER INSERT ON \"public\".\"Individual.owner\"")),
            "the constraint trigger must attach to the implementor's own *junction* table, not the owner table"
        );
        assert!(
            infos
                .iter()
                .any(|i| i.fn_ddl.contains("SELECT 1 FROM \"public\".\"Account.owner\"")),
            "the trigger function must query the helper view"
        );
        assert!(infos.iter().all(|i| i.upd_ddl.contains("AFTER UPDATE OF \"target\"")));
    }

    // ── on_delete regression tests (bugs caught by live-execution testing) ────────

    #[test]
    fn test_target_fk_suffix_forces_deferrable_when_source_side_deletes_target() {
        // Regression: a Source-side DeleteTarget/DeleteTargetIfOrphan
        // trigger deletes the target from a BEFORE DELETE trigger on the
        // owner row, which still exists at that point — an immediate
        // (non-deferred) RESTRICT on the same FK's Target side rejects that
        // nested delete every time. Confirmed live
        // (`tests/live_execution_on_delete.rs`) before this fix.
        let policies = vec![OnDeletePolicy {
            side: DeleteSide::Source,
            action: DeleteAction::DeleteTarget,
        }];
        assert_eq!(target_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");

        let policies = vec![OnDeletePolicy {
            side: DeleteSide::Source,
            action: DeleteAction::DeleteTargetIfOrphan,
        }];
        assert_eq!(target_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");
    }

    #[test]
    fn test_target_fk_suffix_unaffected_when_no_source_side_policy() {
        // The fix above must not change behavior for the ordinary case.
        assert_eq!(target_fk_suffix(&[]), " ON DELETE RESTRICT");
        let policies = vec![OnDeletePolicy {
            side: DeleteSide::Target,
            action: DeleteAction::Allow,
        }];
        assert_eq!(target_fk_suffix(&policies), " ON DELETE SET NULL");
    }

    #[test]
    fn test_target_jt_fk_suffix_forces_deferrable_when_source_side_deletes_target() {
        // Same fix, multilink junction-table variant.
        let policies = vec![OnDeletePolicy {
            side: DeleteSide::Source,
            action: DeleteAction::DeleteTargetIfOrphan,
        }];
        assert_eq!(target_jt_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");
    }

    fn org_type(module: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: "Org".into(),
            module: module.into(),
            table: "Org".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![PropertyDescriptor {
                name: "id".into(),
                pg_type: "uuid".into(),
                nullable: false,
                default_sql: Some("gen_random_uuid()".into()),
                default_pyql: None,
                description: None,
                check_constraints: vec![],
                is_exclusive: true,
                is_pk: true,
                is_readonly: true,
                rewrites: vec![],
                tuple_members: None,
                column_type: None,
            }],
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

    #[test]
    fn test_multilink_target_delete_source_trigger_is_after_not_before() {
        // Regression: the target-side DeleteSource trigger on a multilink's
        // junction table used to be BEFORE DELETE, which self-conflicts —
        // deleting the target cascades to delete the junction row, whose
        // BEFORE trigger deletes the owner, whose own junction-cleanup
        // cascade then tries to delete that same still-being-deleted
        // junction row again. Postgres rejects this with "tuple to be
        // deleted was already modified by an operation triggered by the
        // current command" and its own hint says to use AFTER instead —
        // confirmed live (`tests/live_execution_on_delete.rs`) before this fix.
        let module = "default";
        let mut owner = org_type(module);
        owner.name = "Product".into();
        owner.table = "Product".into();
        owner.multilinks = vec![MultiLinkDescriptor {
            name: "tags".into(),
            target: format!("{module}::Org"),
            through: None,
            nullable: false,
            description: None,
            default_pyql: None,
            on_delete: vec![OnDeletePolicy {
                side: DeleteSide::Target,
                action: DeleteAction::DeleteSource,
            }],
        }];
        let schema = SchemaDescriptor {
            types: vec![org_type(module), owner],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("AFTER DELETE ON \"public\".\"Product.tags\""),
            "got:\n{ddl}"
        );
        assert!(
            !ddl.contains("BEFORE DELETE ON \"public\".\"Product.tags\""),
            "got:\n{ddl}"
        );
    }

    // ── what stdlib reaches persisted DDL ───────────────────────────────────────

    /// Every `_pylon.<name>(` reference in `ddl`, ignoring the stdlib's own
    /// `CREATE OR REPLACE FUNCTION _pylon.<name>(...)` headers — those are
    /// definitions, not references.
    fn referenced_pylon_functions(ddl: &str) -> std::collections::BTreeSet<String> {
        const DEFINITION: &str = "CREATE OR REPLACE FUNCTION _pylon.";
        let mut found = std::collections::BTreeSet::new();
        let mut rest = ddl;
        while let Some(pos) = rest.find("_pylon.") {
            let is_definition = rest[..pos]
                .len()
                .checked_sub(DEFINITION.len() - "_pylon.".len())
                .is_some_and(|start| rest[start..pos + "_pylon.".len()].ends_with(DEFINITION));
            let after = &rest[pos + "_pylon.".len()..];
            let name_len = after
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            let name = &after[..name_len];
            // Only a call — `_pylon."IndexOutbox"` and bare type references
            // are not function references.
            if !is_definition && !name.is_empty() && after[name_len..].starts_with('(') {
                found.insert(name.to_string());
            }
            rest = &rest[pos + "_pylon.".len()..];
        }
        found
    }

    #[test]
    fn only_known_stdlib_functions_reach_persisted_ddl() {
        // A `_pylon` function named inside emitted DDL is baked into the
        // catalog — a plpgsql trigger body, a column DEFAULT — and stays
        // there until something rewrites it. Postgres does not
        // dependency-track plpgsql bodies, so changing such a function's
        // *signature* breaks those databases silently, at the moment the
        // trigger next fires.
        //
        // That is survivable while the set is tiny and its members are
        // frozen — see "Function signatures are append-only" in
        // CONTRIBUTING.md. This test exists so the set cannot grow
        // unnoticed: if it fails, either keep the new function out of
        // persisted DDL, or accept that its signature is now frozen too and
        // add it below deliberately.
        // `tags[0]` is what pulls a stdlib call into the trigger body;
        // the signal entry adds the other persisted-trigger path.
        let mut person = person_type();
        person.materialized = true;
        person.properties.push(PropertyDescriptor {
            name: "tags".into(),
            pg_type: "text[]".into(),
            nullable: true,
            default_sql: None,
            default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: false,
            is_pk: false,
            is_readonly: false,
            rewrites: vec![],
            tuple_members: None,
            column_type: None,
        });
        person.triggers = vec![trig(
            1,
            "After",
            "update Person set { age := <std::int64>__new__.tags[0] }",
        )];
        person.signals = vec![crate::schema::SignalEntry { on: 1 }];
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

        let ddl = export_schema(&schema).unwrap();
        let referenced = referenced_pylon_functions(&ddl);
        let allowed: std::collections::BTreeSet<String> = ["array_subscript", "notify_cache_invalidate"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(
            referenced.is_subset(&allowed),
            "new stdlib functions reached persisted DDL: {:?}\n\
             see this test's comment before widening the allowlist",
            referenced.difference(&allowed).collect::<Vec<_>>(),
        );
        // The fixture has to actually exercise the path, or this passes vacuously.
        assert!(
            referenced.contains("array_subscript"),
            "fixture no longer bakes a stdlib call; it is not testing anything"
        );
    }

    // ── content-addressed trigger names ─────────────────────────────────────────

    /// `Product.tags -> Org` multilink with a target-side DeleteSource
    /// policy, so the junction table carries a generated trigger whose body
    /// names `owner_table`.
    fn multilink_trigger_schema(owner_table: &str) -> SchemaDescriptor {
        let module = "default";
        let mut owner = org_type(module);
        owner.name = owner_table.into();
        owner.table = owner_table.into();
        owner.multilinks = vec![MultiLinkDescriptor {
            name: "tags".into(),
            target: format!("{module}::Org"),
            through: None,
            nullable: false,
            description: None,
            default_pyql: None,
            on_delete: vec![OnDeletePolicy {
                side: DeleteSide::Target,
                action: DeleteAction::DeleteSource,
            }],
        }];
        SchemaDescriptor {
            types: vec![org_type(module), owner],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        }
    }

    fn trigger_names_of(schema: &SchemaDescriptor) -> Vec<String> {
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
        let mut names: Vec<String> = deletion_policy_trigger_infos(schema, &type_map)
            .into_iter()
            .map(|i| i.trigger_name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_trigger_name_is_stable_for_an_unchanged_schema() {
        // The other half of the property below: names must not churn when
        // nothing changed, or every migration would drop and recreate every
        // trigger in the database.
        let schema = multilink_trigger_schema("Product");
        assert_eq!(trigger_names_of(&schema), trigger_names_of(&schema));
    }

    #[test]
    fn a_trigger_name_changes_when_its_body_changes() {
        // The point of hashing the body: the migration diff compares
        // triggers by name only, so if a change to the emitted body left the
        // name alone, the diff would report nothing and a database that
        // already exists would keep running the old body forever. Here the
        // owner table name is what reaches the body (`DELETE FROM <owner>`).
        let before = trigger_names_of(&multilink_trigger_schema("Product"));
        let after = trigger_names_of(&multilink_trigger_schema("Widget"));
        assert_ne!(before, after, "trigger name did not follow its body");
    }

    #[test]
    fn a_signal_trigger_name_follows_its_event_list() {
        // `events_str` is in the hash as well as the body: a type gaining an
        // On.Delete handler re-emits `CREATE TRIGGER ... AFTER INSERT OR
        // DELETE`, even though the function body is byte-identical.
        let names_for = |on: u8| {
            let module = "default";
            let mut t = org_type(module);
            t.signals = vec![crate::schema::SignalEntry { on }];
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
            signal_trigger_infos(&schema)
                .into_iter()
                .map(|i| i.trigger_name)
                .collect::<Vec<_>>()
        };
        assert_ne!(names_for(1), names_for(1 | 4));
        assert_eq!(names_for(1), names_for(1));
    }

    // ── junction-backed single links ────────────────────────────────────────────

    #[test]
    fn test_junction_backed_single_link_gets_a_source_pk_junction_table_no_fk_column() {
        let module = "default";
        let mut owner = org_type(module);
        owner.name = "Person".into();
        owner.table = "Person".into();
        owner.links = vec![LinkDescriptor {
            name: "spouse".into(),
            target: format!("{module}::Org"),
            nullable: true,
            through: Some(format!("{module}::Marriage")),
            description: None,
            default_pyql: None,
            is_exclusive: true,
            is_readonly: false,
            rewrites: vec![],
            on_delete: vec![],
        }];
        let schema = SchemaDescriptor {
            types: vec![org_type(module), owner],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();

        assert!(
            !ddl.contains("spouse_id"),
            "no {{name}}_id column/FK for a junction-backed link, got:\n{ddl}"
        );
        assert!(ddl.contains("CREATE TABLE \"public\".\"Person.spouse\""), "got:\n{ddl}");
        assert!(
            ddl.contains("PRIMARY KEY (source)"),
            "single-link junction table must be capped to one row per source, got:\n{ddl}"
        );
        assert!(
            ddl.contains("UNIQUE (target)"),
            "exclusive single link must also be unique on the target side, got:\n{ddl}"
        );
        assert!(
            !ddl.contains("CREATE UNIQUE INDEX ON \"public\".\"Person\" (\"spouse_id\")"),
            "got:\n{ddl}"
        );
    }

    // ── @pylon.signal capture triggers ──────────────────────────────────────────

    #[test]
    fn test_type_with_a_signal_gets_a_capture_trigger() {
        use crate::schema::SignalEntry;
        let mut with_signal = person_type();
        with_signal.signals = vec![SignalEntry { on: 5 }]; // Insert | Delete
        let schema = SchemaDescriptor {
            types: vec![with_signal],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains("AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"Person\""),
            "got:\n{ddl}"
        );
        assert!(ddl.contains("_pylon.\"SignalOutbox\""), "got:\n{ddl}");
        assert!(ddl.contains("'default::Person'"), "got:\n{ddl}");
    }

    #[test]
    fn test_type_without_a_signal_gets_no_capture_trigger() {
        // person_type()'s default fixture has an empty `signals` list —
        // no trigger, no per-mutation cost, for types nobody's listening to.
        let schema = minimal_schema(vec![]);
        let ddl = export_schema(&schema).unwrap();
        assert!(!ddl.contains("SignalOutbox"), "got:\n{ddl}");
    }

    #[test]
    fn test_signal_update_capture_skips_index_maintenance_only_changes() {
        // A type with a VectorIndex or Postgres SearchIndex gets an async
        // write-back to its embedding/tsvector column (the vector worker's
        // own `UPDATE`, confirmed live) that shouldn't itself look like a
        // caller-made mutation to a signal handler — the generated trigger
        // function must skip firing when OLD/NEW only differ in those
        // index-maintenance columns.
        use crate::schema::{SignalEntry, VectorIndexDescriptor};
        let mut with_signal = person_type();
        with_signal.signals = vec![SignalEntry { on: 2 }]; // Update
        with_signal.vector_indexes = vec![VectorIndexDescriptor {
            index_name: None,
            pointers: vec!["name".into()],
            model: "mistral-embed".into(),
            metric: "cosine".into(),
            dimensions: 1024,
        }];
        let schema = SchemaDescriptor {
            types: vec![with_signal],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(
            ddl.contains(
                "IF TG_OP = 'UPDATE' AND (to_jsonb(OLD) - '__vector__') = (to_jsonb(NEW) - '__vector__') THEN"
            ),
            "got:\n{ddl}"
        );
        assert!(ddl.contains("RETURN NULL;\n    END IF;"), "got:\n{ddl}");
    }

    #[test]
    fn test_signal_update_capture_has_no_guard_without_an_index() {
        // person_type() has no vector/search indexes — nothing to exclude,
        // so the trigger body shouldn't carry the extra jsonb-diff guard.
        use crate::schema::SignalEntry;
        let mut with_signal = person_type();
        with_signal.signals = vec![SignalEntry { on: 2 }]; // Update
        let schema = SchemaDescriptor {
            types: vec![with_signal],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(!ddl.contains("IF TG_OP = 'UPDATE'"), "got:\n{ddl}");
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;
    use crate::schema::{PartitionDescriptor, PartitionInterval, PropertyDescriptor};

    fn prop(name: &str, pg_type: &str, is_pk: bool) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(),
            pg_type: pg_type.into(),
            nullable: false,
            default_sql: None,
            default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: false,
            is_pk,
            is_readonly: false,
            rewrites: vec![],
            tuple_members: None,
            column_type: None,
        }
    }

    fn event_schema(retention: Option<u32>) -> SchemaDescriptor {
        let mut td = TypeDescriptor {
            name: "Event".into(),
            module: "default".into(),
            table: "Event".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![prop("id", "uuid", true), prop("occurred_at", "timestamptz", false)],
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
        };
        td.partition = Some(PartitionDescriptor {
            pointer: "occurred_at".into(),
            interval: PartitionInterval::Monthly,
            premake: 4,
            retention,
        });
        SchemaDescriptor {
            types: vec![td],
            ..Default::default()
        }
    }

    #[test]
    fn a_partitioned_table_declares_its_range_key() {
        let ddl = export_schema(&event_schema(None)).unwrap();
        assert!(ddl.contains("PARTITION BY RANGE (\"occurred_at\")"), "got:\n{ddl}");
    }

    #[test]
    fn the_partition_key_is_added_to_the_primary_key() {
        // PostgreSQL rejects a partitioned table whose primary key doesn't
        // include the partition column, so this is not optional.
        let ddl = export_schema(&event_schema(None)).unwrap();
        assert!(ddl.contains("PRIMARY KEY (\"id\", \"occurred_at\")"), "got:\n{ddl}");
    }

    #[test]
    fn an_unpartitioned_table_is_untouched() {
        let mut schema = event_schema(None);
        schema.types[0].partition = None;
        let ddl = export_schema(&schema).unwrap();
        assert!(!ddl.contains("PARTITION BY"), "got:\n{ddl}");
        assert!(!ddl.contains("partman"), "got:\n{ddl}");
        assert!(ddl.contains("PRIMARY KEY (\"id\")"), "got:\n{ddl}");
    }

    #[test]
    fn registration_is_guarded_so_re_export_is_idempotent() {
        // `create_parent` errors on a table it has already adopted, and this
        // DDL is re-run on every export.
        let ddl = export_schema(&event_schema(None)).unwrap();
        assert!(
            ddl.contains("IF NOT EXISTS (SELECT 1 FROM partman.part_config"),
            "got:\n{ddl}"
        );
        assert!(ddl.contains("partman.create_parent("), "got:\n{ddl}");
        assert!(ddl.contains("p_interval := '1 month'"), "got:\n{ddl}");
        assert!(ddl.contains("p_premake := 4"), "got:\n{ddl}");
    }

    #[test]
    fn retention_is_applied_when_declared() {
        let ddl = export_schema(&event_schema(Some(12))).unwrap();
        assert!(ddl.contains("SET retention = '12 months'"), "got:\n{ddl}");
        assert!(ddl.contains("retention_keep_table = false"), "got:\n{ddl}");
    }

    #[test]
    fn retention_is_cleared_when_not_declared() {
        // Removing `retention=` from the schema has to actually stop the
        // dropping, not leave the last value sitting in part_config.
        let ddl = export_schema(&event_schema(None)).unwrap();
        assert!(ddl.contains("SET retention = NULL"), "got:\n{ddl}");
    }

    #[test]
    fn a_partitioned_schema_requires_the_partman_extension() {
        let exts = crate::diff::required_extensions(&event_schema(None));
        assert!(exts.contains(&"pg_partman"), "got: {exts:?}");
    }
}
