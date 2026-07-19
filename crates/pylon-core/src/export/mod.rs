use crate::error::{PyQLError, PyQLFragmentError};
use crate::schema::{DeleteAction, DeleteSide, FunctionDescriptor, OnDeletePolicy, SchemaDescriptor, TypeDescriptor, TypeConstraint};
use std::collections::{BTreeSet, HashMap};

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
        .map(|t| (format!("{}::{}", t.module, t.name), (t.module.as_str(), t.table.as_str())))
        .collect();

    emit_schemas(schema, &mut out);
    emit_enums(schema, &mut out);
    emit_scalars(schema, &mut out);
    emit_scalar_functions(schema, &mut out)?;
    emit_tables(schema, &mut out);
    emit_fk_constraints(schema, &type_map, &mut out);
    emit_link_source_triggers(schema, &type_map, &mut out);
    emit_junction_tables(schema, &type_map, &mut out);
    emit_multilink_deletion_triggers(schema, &type_map, &mut out);
    emit_unique_indexes(schema, &mut out);
    emit_check_constraints(schema, &mut out);
    emit_plain_indexes(schema, &mut out);
    emit_triggers(schema, &mut out);
    emit_interface_views(schema, &mut out);
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
    if module == "default" { "\"public\"".into() } else { qi(module) }
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

// ── Trigger event/timing helpers ───────────────────────────────────────────────

fn trigger_events(on: u8) -> String {
    let mut events = Vec::new();
    if on & 1 != 0 { events.push("INSERT"); }
    if on & 2 != 0 { events.push("UPDATE"); }
    if on & 4 != 0 { events.push("DELETE"); }
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
    let mut modules: BTreeSet<&str> = BTreeSet::new();
    for t in &schema.types   { modules.insert(&t.module); }
    for s in &schema.scalars { modules.insert(&s.module); }
    for e in &schema.enums   { modules.insert(&e.module); }
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
        let members: Vec<String> = e.members.iter()
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
        let checks: Vec<String> = s.check_constraints.iter()
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
        if t.abstract_ || t.junction { continue; }
        emit_one_table(t, out);
    }
}

fn emit_one_table(t: &TypeDescriptor, out: &mut String) {
    out.push_str(&format!("CREATE TABLE {} (\n", qn(&t.module, &t.table)));

    let mut lines: Vec<String> = Vec::new();

    // Property columns
    for p in &t.properties {
        let not_null = if p.nullable { "" } else { " NOT NULL" };
        let default = p.default_sql.as_deref()
            .map(|d| format!(" DEFAULT {}", d))
            .unwrap_or_default();
        let col_type = p.pg_type.strip_prefix("__nt__:").map(|_| "jsonb").unwrap_or(&p.pg_type);
        lines.push(format!("    {} {}{}{}", qi(&p.name), col_type, not_null, default));
    }

    // Link columns — uuid stubs; FK constraints added in phase 5
    for l in &t.links {
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        lines.push(format!("    {} uuid{}", qi(&format!("{}_id", l.name)), not_null));
    }

    // Primary key
    let pk_cols: Vec<String> = t.properties.iter()
        .filter(|p| p.is_pk)
        .map(|p| qi(&p.name))
        .collect();
    if !pk_cols.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    out.push_str(&lines.join(",\n"));
    out.push_str("\n);\n\n");
    out.push_str(&cache_invalidate_trigger_sql(&qn(&t.module, &t.table)));
    out.push_str("\n\n");
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
            && matches!(p.action, DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan)
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

fn emit_fk_constraints(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        for l in &t.links {
            let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else { continue };
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

// ── Trigger emit helper ────────────────────────────────────────────────────────

fn emit_before_delete_trigger(
    fn_qname: &str,
    trigger_name: &str,
    table_qname: &str,
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
fn emit_after_delete_trigger(
    fn_qname: &str,
    trigger_name: &str,
    table_qname: &str,
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
         CREATE TRIGGER {trigger_name}\n\
         AFTER DELETE ON {table_qname}\n\
         FOR EACH ROW EXECUTE FUNCTION {fn_qname}();\n\n"
    ));
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

fn link_source_trigger_infos(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>) -> Vec<DeletionTriggerInfo> {
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        for l in &t.links {
            let src_action = policy_for(&l.on_delete, &DeleteSide::Source)
                .unwrap_or(&DeleteAction::Allow);
            match src_action {
                DeleteAction::Allow => continue,
                DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan => {}
                _ => continue,
            }
            let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else { continue };
            let suffix = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                "del_orphan"
            } else {
                "del_target"
            };
            let hash = fnv(&[&t.table, &l.name, suffix]);
            let fname = format!("{}_{}_{}", t.table, l.name, &hash[..8]);
            let fn_qname = qn(&t.module, &fname);
            let tbl_qname = qn(&t.module, &t.table);
            let tgt_qname = qn(tgt_module, tgt_table);
            let col = qi(&format!("{}_id", l.name));

            let body = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                format!("    IF NOT EXISTS (\n        SELECT 1 FROM {tbl_qname} WHERE {col} = OLD.{col} AND id != OLD.id\n    ) THEN\n        DELETE FROM {tgt_qname} WHERE id = OLD.{col};\n    END IF;")
            } else {
                format!("    DELETE FROM {tgt_qname} WHERE id = OLD.{col};")
            };

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

fn emit_link_source_triggers(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    for info in link_source_trigger_infos(schema, type_map) {
        out.push_str(&info.ddl);
    }
}

// ── Phase 6: junction tables for multi-links ───────────────────────────────────

fn emit_junction_tables(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        for ml in &t.multilinks {
            let jt_name = format!("{}.{}", t.table, ml.name);
            let src_suffix = source_jt_fk_suffix(&ml.on_delete);
            out.push_str(&format!(
                "CREATE TABLE {} (\n    source uuid NOT NULL REFERENCES {}(id){},\n",
                qn(&t.module, &jt_name),
                qn(&t.module, &t.table),
                src_suffix,
            ));
            if let Some((tgt_module, tgt_table)) = type_map.get(&ml.target) {
                let tgt_suffix = target_jt_fk_suffix(&ml.on_delete);
                out.push_str(&format!(
                    "    target uuid NOT NULL REFERENCES {}(id){},\n",
                    qn(tgt_module, tgt_table),
                    tgt_suffix,
                ));
            } else {
                out.push_str("    target uuid NOT NULL,\n");
            }

            // Extra property columns from a junction through type.
            if let Some(through_qname) = &ml.through {
                let through_td = schema.types.iter().find(|td| {
                    format!("{}::{}", td.module, td.name) == *through_qname
                });
                if let Some(td) = through_td {
                    if td.junction {
                        for p in &td.properties {
                            if p.name == "id" { continue; }
                            let not_null = if p.nullable { "" } else { " NOT NULL" };
                            out.push_str(&format!("    {} {}{},\n", qi(&p.name), p.pg_type, not_null));
                        }
                    }
                }
            }

            out.push_str("    PRIMARY KEY (source, target)\n);\n\n");
            out.push_str(&cache_invalidate_trigger_sql(&qn(&t.module, &jt_name)));
            out.push_str("\n\n");
        }
    }
}

// ── Phase 6.5: multilink deletion policy triggers ──────────────────────────────

fn multilink_deletion_trigger_infos(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>) -> Vec<DeletionTriggerInfo> {
    let mut result = Vec::new();
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        for ml in &t.multilinks {
            let jt_name = format!("{}.{}", t.table, ml.name);
            let jt_qname = qn(&t.module, &jt_name);

            // Source-side: DeleteTarget / DeleteTargetIfOrphan
            let src_action = policy_for(&ml.on_delete, &DeleteSide::Source)
                .unwrap_or(&DeleteAction::Allow);
            if matches!(src_action, DeleteAction::DeleteTarget | DeleteAction::DeleteTargetIfOrphan) {
                if let Some((tgt_module, tgt_table)) = type_map.get(&ml.target) {
                    let suffix = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                        "del_orphan"
                    } else {
                        "del_target"
                    };
                    let hash = fnv(&[&t.table, &ml.name, suffix]);
                    let fname = format!("{}_{}_{}_{}", t.table, ml.name, suffix, &hash[..8]);
                    let fn_qname = qn(&t.module, &fname);
                    let tgt_qname = qn(tgt_module, tgt_table);

                    let body = if matches!(src_action, DeleteAction::DeleteTargetIfOrphan) {
                        format!("    IF NOT EXISTS (\n        SELECT 1 FROM {jt_qname} WHERE target = OLD.target AND source != OLD.source\n    ) THEN\n        DELETE FROM {tgt_qname} WHERE id = OLD.target;\n    END IF;")
                    } else {
                        format!("    DELETE FROM {tgt_qname} WHERE id = OLD.target;")
                    };

                    let mut ddl = String::new();
                    emit_before_delete_trigger(&fn_qname, &qi(&fname), &jt_qname, &body, &mut ddl);
                    result.push(DeletionTriggerInfo {
                        table_module: t.module.clone(),
                        table_name: jt_name.clone(),
                        trigger_name: fname,
                        ddl,
                    });
                }
            }

            // Target-side: DeleteSource — when target deleted (cascade removes junction row),
            // also delete the source object.
            let tgt_action = policy_for(&ml.on_delete, &DeleteSide::Target)
                .unwrap_or(&DeleteAction::Restrict);
            if matches!(tgt_action, DeleteAction::DeleteSource) {
                let hash = fnv(&[&t.table, &ml.name, "del_source"]);
                let fname = format!("{}_{}_{}", t.table, ml.name, &hash[..8]);
                let fn_qname = qn(&t.module, &fname);
                let src_qname = qn(&t.module, &t.table);

                let body = format!("    DELETE FROM {src_qname} WHERE id = OLD.source;");
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
    }
    result
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
pub fn deletion_policy_trigger_infos(schema: &SchemaDescriptor, type_map: &HashMap<String, (&str, &str)>) -> Vec<DeletionTriggerInfo> {
    let mut result = link_source_trigger_infos(schema, type_map);
    result.extend(multilink_deletion_trigger_infos(schema, type_map));
    result
}

// ── Phase 7: unique indexes ────────────────────────────────────────────────────

fn emit_unique_indexes(schema: &SchemaDescriptor, out: &mut String) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        let qname = qn(&t.module, &t.table);

        for p in &t.properties {
            if p.is_exclusive && !p.is_pk {
                out.push_str(&format!(
                    "CREATE UNIQUE INDEX ON {} ({});\n",
                    qname, qi(&p.name),
                ));
                emitted = true;
            }
        }
        for l in &t.links {
            if l.is_exclusive {
                out.push_str(&format!(
                    "CREATE UNIQUE INDEX ON {} ({});\n",
                    qname, qi(&format!("{}_id", l.name)),
                ));
                emitted = true;
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive { pointers: fields, unless } = c {
                let cols: Vec<String> = fields.iter().map(|f| qi(f)).collect();
                let where_clause = unless.as_deref()
                    .map(|u| format!(" WHERE NOT ({})", u))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "CREATE UNIQUE INDEX ON {} ({}){};\n",
                    qname, cols.join(", "), where_clause,
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
        if t.abstract_ || t.junction { continue; }
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
        if t.abstract_ || t.junction { continue; }
        let qname = qn(&t.module, &t.table);

        for idx in &t.indexes {
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let body = if let Some(expr) = &idx.expression {
                format!("({})", expr)
            } else {
                let cols: Vec<String> = idx.pointers.iter().map(|f| qi(f)).collect();
                format!("({})", cols.join(", "))
            };
            let where_clause = idx.unless.as_deref()
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

fn emit_triggers(schema: &SchemaDescriptor, out: &mut String) {
    for t in &schema.types {
        if t.abstract_ || t.junction { continue; }
        let table_qname = qn(&t.module, &t.table);

        for trig in &t.triggers {
            let hash = fnv(&[
                &t.table,
                &trig.on.to_string(),
                trig.timing.as_str(),
                trig.handler.as_str(),
            ]);
            let fname = format!("{}_{}", t.table, &hash[..12]);
            let fn_qname = qn(&t.module, &fname);
            let events = trigger_events(trig.on);
            let timing = trigger_timing(&trig.timing);
            let body = trig.handler.trim_end_matches(';');

            out.push_str(&format!(
                "CREATE OR REPLACE FUNCTION {}()\n\
                 RETURNS trigger LANGUAGE plpgsql AS $$\n\
                 BEGIN\n\
                 {body};\n\
                 RETURN NEW;\n\
                 END;\n\
                 $$;\n\n",
                fn_qname,
            ));
            out.push_str(&format!(
                "CREATE TRIGGER {}\n\
                 {} {} ON {}\n\
                 FOR EACH ROW EXECUTE FUNCTION {}();\n\n",
                qi(&fname), timing, events, table_qname, fn_qname,
            ));
        }
    }
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
        if !(t.abstract_ && t.materialized) { continue; }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() { continue; }
        let mut ddl = String::new();
        emit_one_interface_view(t, impls, &mut ddl);
        let ddl = ddl.trim().to_string();
        if !ddl.is_empty() {
            result.push((t.module.clone(), t.name.clone(), ddl));
        }
    }
    result
}

/// Return `CREATE OR REPLACE FUNCTION` DDL for every user-defined function in `schema`.
pub fn function_ddl(schema: &SchemaDescriptor) -> Result<Vec<String>, crate::error::PyQLError> {
    function_ddl_with_names(schema)
        .map(|v| v.into_iter().map(|(_, _, ddl)| ddl).collect())
}

/// Like `function_ddl` but also returns the module and function name for each entry.
pub fn function_ddl_with_names(schema: &SchemaDescriptor) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema.functions.iter()
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

/// DDL for scalar (non-object-returning) functions only — safe to emit before tables.
pub fn scalar_function_ddl_with_names(schema: &SchemaDescriptor) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema.functions.iter()
        .filter(|fd| !fd.return_is_object)
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

/// DDL for object-returning functions only — must be emitted after tables exist.
pub fn object_function_ddl_with_names(schema: &SchemaDescriptor) -> Result<Vec<(String, String, String)>, crate::error::PyQLError> {
    schema.functions.iter()
        .filter(|fd| fd.return_is_object)
        .map(|fd| emit_one_function(fd, schema).map(|ddl| (fd.module.clone(), fd.name.clone(), ddl)))
        .collect()
}

// ── Phase 11: interface views ──────────────────────────────────────────────────

fn emit_one_interface_view(t: &TypeDescriptor, impls: &[&TypeDescriptor], out: &mut String) {
    let cols: Vec<String> = t.properties.iter()
        .map(|p| qi(&p.name))
        .chain(t.links.iter().map(|l| qi(&format!("{}_id", l.name))))
        .collect();
    let col_list = cols.join(", ");
    let selects: Vec<String> = impls.iter()
        .map(|impl_t| format!("    SELECT {} FROM {}", col_list, qn(&impl_t.module, &impl_t.table)))
        .collect();
    out.push_str(&format!("CREATE VIEW {} AS\n", qn(&t.module, &t.table)));
    out.push_str(&selects.join("\n    UNION ALL\n"));
    out.push_str(";\n\n");
}

fn emit_interface_views(schema: &SchemaDescriptor, out: &mut String) {
    let mut implementors: HashMap<String, Vec<&TypeDescriptor>> = HashMap::new();
    for t in &schema.types {
        if !t.abstract_ {
            for iface in &t.interfaces {
                implementors.entry(iface.clone()).or_default().push(t);
            }
        }
    }
    for t in &schema.types {
        if !(t.abstract_ && t.materialized) { continue; }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() { continue; }
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

fn make_excl_info(iface: &TypeDescriptor, fields: &[String], impl_t: &TypeDescriptor) -> ExclTriggerInfo {
    let fn_name = excl_fn_name(&iface.table, fields);
    let fn_qname = format!("{}.{}", pg_schema(&iface.module), qi(&fn_name));
    let view_qname = qn(&iface.module, &iface.table);
    let tbl_qname = qn(&impl_t.module, &impl_t.table);

    let field_conds: Vec<String> = fields.iter()
        .map(|f| format!("{} = NEW.{}", qi(f), qi(f)))
        .collect();
    let where_clause = format!("{} AND \"id\" <> NEW.\"id\"", field_conds.join(" AND "));

    let detail_keys = fields.join(", ");
    let detail_vals = fields.iter()
        .map(|f| format!("NEW.{}::text", qi(f)))
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
    let of_cols = fields.iter().map(|f| qi(f)).collect::<Vec<_>>().join(", ");
    let when_clause = fields.iter()
        .map(|f| format!("OLD.{} IS DISTINCT FROM NEW.{}", qi(f), qi(f)))
        .collect::<Vec<_>>()
        .join(" OR ");

    let ins_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER INSERT ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW EXECUTE FUNCTION {}();",
        qi(&ins_trigger_name), tbl_qname, fn_qname,
    );
    let upd_ddl = format!(
        "CREATE CONSTRAINT TRIGGER {}\n\
         AFTER UPDATE OF {} ON {}\n\
         DEFERRABLE INITIALLY DEFERRED\n\
         FOR EACH ROW WHEN ({})\n\
         EXECUTE FUNCTION {}();",
        qi(&upd_trigger_name), of_cols, tbl_qname, when_clause, fn_qname,
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
        if !(t.abstract_ && t.materialized) { continue; }
        let key = format!("{}::{}", t.module, t.name);
        let Some(impls) = implementors.get(&key) else { continue };
        if impls.is_empty() { continue; }

        for p in &t.properties {
            if !p.is_exclusive || p.is_pk { continue; }
            let fields = vec![p.name.clone()];
            for impl_t in impls {
                result.push(make_excl_info(t, &fields, impl_t));
            }
        }
        for l in &t.links {
            if !l.is_exclusive { continue; }
            let fields = vec![format!("{}_id", l.name)];
            for impl_t in impls {
                result.push(make_excl_info(t, &fields, impl_t));
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive { pointers: fields, .. } = c {
                for impl_t in impls {
                    result.push(make_excl_info(t, fields, impl_t));
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
    let params_sql = fd.params.iter()
        .map(|p| format!("{} {}", qi(&p.name), p.pg_type))
        .collect::<Vec<_>>()
        .join(", ");

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
    let td = schema.types.iter().find(|t| {
        format!("{}::{}", t.module, t.name) == *type_name
    });
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
        cols.push(format!("{} uuid", qi(&format!("{}_id", l.name))));
    }
    cols.join(", ")
}

// ── Phase 13: vector embedding columns ────────────────────────────────────────

fn emit_vector_columns(schema: &SchemaDescriptor, out: &mut String) {
    for td in &schema.types {
        if td.abstract_ || td.vector_indexes.is_empty() { continue; }
        for vi in &td.vector_indexes {
            out.push_str(&format!(
                "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} vector({});\n",
                qn(&td.module, &td.table),
                qi(&vi.column_name()),
                vi.dimensions,
            ));
        }
    }
    if schema.types.iter().any(|t| !t.abstract_ && !t.vector_indexes.is_empty()) {
        out.push('\n');
    }
}

// ── Phase 14: vector HNSW indexes ─────────────────────────────────────────────

fn emit_vector_indexes(schema: &SchemaDescriptor, out: &mut String) {
    for td in &schema.types {
        if td.abstract_ || td.vector_indexes.is_empty() { continue; }
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
        if td.abstract_ { continue; }
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres { continue; }

            // Build: setweight(to_tsvector('english', coalesce(col, '')), 'W') || ...
            let parts: Vec<String> = si.pointers.iter().map(|sf| {
                let col = qi(&sf.name);
                let w = sf.weight.as_str();
                format!("setweight(to_tsvector('english', coalesce({col}, '')), '{w}')")
            }).collect();

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
        if td.abstract_ { continue; }
        for si in &td.search_indexes {
            if si.backend != SearchBackend::Postgres { continue; }

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
/// startup and cached; the worker runs it with `asyncpg.fetch(sql, [ids])`.
pub fn compile_index_fetch(
    type_name: &str,
    index_name: Option<&str>,
    schema: &SchemaDescriptor,
) -> Result<String, PyQLError> {
    let td = schema.types.iter().find(|t| {
        format!("{}::{}", t.module, t.name) == type_name
    }).ok_or_else(|| PyQLError::Fragment(PyQLFragmentError {
        message: format!("compile_index_fetch: unknown type '{}'", type_name),
        context: type_name.to_string(),
        position: crate::error::Position { line: 0, col: 0 },
    }))?;

    let vi = td.vector_indexes.iter().find(|vi| vi.index_name.as_deref() == index_name)
        .ok_or_else(|| {
            let key = index_name.unwrap_or("<default>");
            PyQLError::Fragment(PyQLFragmentError {
                message: format!("compile_index_fetch: no vector index '{}' on type '{}'", key, type_name),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    let field_exprs = vi.pointers.iter().map(|f| {
        // Resolve the field's pg_type to decide whether an explicit cast is needed.
        let pg_type = td.properties.iter()
            .find(|p| p.name == *f)
            .map(|p| p.pg_type.as_str())
            .unwrap_or("text");
        let col = qi(f);
        if pg_type == "text" { col } else { format!("{}::text", col) }
    }).collect::<Vec<_>>();

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
    let td = schema.types.iter().find(|t| {
        format!("{}::{}", t.module, t.name) == type_name
    }).ok_or_else(|| PyQLError::Fragment(PyQLFragmentError {
        message: format!("compile_search_index_fetch: unknown type '{}'", type_name),
        context: type_name.to_string(),
        position: crate::error::Position { line: 0, col: 0 },
    }))?;

    use crate::schema::SearchBackend;
    let si = td.search_indexes.iter()
        .find(|si| si.index_name.as_deref() == index_name && si.backend != SearchBackend::Postgres)
        .ok_or_else(|| {
            let key = index_name.unwrap_or("<default>");
            PyQLError::Fragment(PyQLFragmentError {
                message: format!("compile_search_index_fetch: no remote SearchIndex '{}' on type '{}'", key, type_name),
                context: type_name.to_string(),
                position: crate::error::Position { line: 0, col: 0 },
            })
        })?;

    let field_exprs = si.pointers.iter().map(|sf| {
        let pg_type = td.properties.iter()
            .find(|p| p.name == sf.name)
            .map(|p| p.pg_type.as_str())
            .unwrap_or("text");
        let col = qi(&sf.name);
        if pg_type == "text" { col } else { format!("{}::text", col) }
    }).collect::<Vec<_>>();

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
        DeleteAction, DeleteSide, FunctionDescriptor, FunctionParamDescriptor, MultiLinkDescriptor,
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
                tuple_members: None, },
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
                tuple_members: None, },
            ],
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

    fn minimal_schema(fns: Vec<FunctionDescriptor>) -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![person_type()],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: fns,
            aliases: vec![],
        }
    }

    #[test]
    fn test_emit_one_table_includes_cache_invalidate_trigger() {
        let mut out = String::new();
        emit_one_table(&person_type(), &mut out);
        assert!(
            out.contains("CREATE OR REPLACE TRIGGER pylon_cache_invalidate\n    AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"Person\""),
            "got:\n{out}"
        );
    }

    #[test]
    fn test_emit_scalar_function_ddl() {
        let fd = FunctionDescriptor {
            name: "mysum".into(),
            module: "math".into(),
            params: vec![
                FunctionParamDescriptor { name: "a".into(), pg_type: "int8".into() },
                FunctionParamDescriptor { name: "b".into(), pg_type: "int8".into() },
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
        assert!(ddl.contains("CREATE OR REPLACE FUNCTION \"math\".\"mysum\""), "got:\n{}", ddl);
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
        assert!(ddl.contains("CREATE OR REPLACE FUNCTION \"public\".\"adults\"()"), "got:\n{}", ddl);
        assert!(ddl.contains("RETURNS TABLE("), "got:\n{}", ddl);
        assert!(ddl.contains("\"id\" uuid"), "got:\n{}", ddl);
        assert!(ddl.contains("\"age\" int8"), "got:\n{}", ddl);
        assert!(ddl.contains("STABLE"), "got:\n{}", ddl);
        assert!(ddl.contains("SELECT * FROM"), "got:\n{}", ddl);
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
            functions: vec![], aliases: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("CREATE SEQUENCE \"public\".\"OrderNumber_seq\""), "got:\n{}", ddl);
        assert!(ddl.contains("CREATE DOMAIN \"public\".\"OrderNumber\" AS int8"), "got:\n{}", ddl);
        // Sequence must precede domain in the output
        let seq_pos = ddl.find("CREATE SEQUENCE").unwrap();
        let dom_pos = ddl.find("CREATE DOMAIN").unwrap();
        assert!(seq_pos < dom_pos, "sequence must appear before domain");
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
        let policies = vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTarget }];
        assert_eq!(target_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");

        let policies = vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTargetIfOrphan }];
        assert_eq!(target_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");
    }

    #[test]
    fn test_target_fk_suffix_unaffected_when_no_source_side_policy() {
        // The fix above must not change behavior for the ordinary case.
        assert_eq!(target_fk_suffix(&[]), " ON DELETE RESTRICT");
        let policies = vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::Allow }];
        assert_eq!(target_fk_suffix(&policies), " ON DELETE SET NULL");
    }

    #[test]
    fn test_target_jt_fk_suffix_forces_deferrable_when_source_side_deletes_target() {
        // Same fix, multilink junction-table variant.
        let policies = vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTargetIfOrphan }];
        assert_eq!(target_jt_fk_suffix(&policies), " DEFERRABLE INITIALLY DEFERRED");
    }

    fn org_type(module: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: "Org".into(), module: module.into(), table: "Org".into(),
            abstract_: false, materialized: true, description: None,
            parents: vec![], interfaces: vec![],
            properties: vec![PropertyDescriptor {
                name: "id".into(), pg_type: "uuid".into(), nullable: false,
                default_sql: Some("gen_random_uuid()".into()), default_pyql: None,
                description: None, check_constraints: vec![], is_exclusive: true,
                is_pk: true, is_readonly: true, rewrites: vec![], tuple_members: None,
            }],
            links: vec![], multilinks: vec![], computed: vec![], constraints: vec![],
            indexes: vec![], vector_indexes: vec![], search_indexes: vec![],
            triggers: vec![], junction: false,
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
            on_delete: vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::DeleteSource }],
        }];
        let schema = SchemaDescriptor {
            types: vec![org_type(module), owner],
            scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![],
            functions: vec![], aliases: vec![],
        };
        let ddl = export_schema(&schema).unwrap();
        assert!(ddl.contains("AFTER DELETE ON \"public\".\"Product.tags\""), "got:\n{ddl}");
        assert!(!ddl.contains("BEFORE DELETE ON \"public\".\"Product.tags\""), "got:\n{ddl}");
    }
}

