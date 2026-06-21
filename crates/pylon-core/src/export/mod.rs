use crate::error::PyQLError;
use crate::schema::{SchemaDescriptor, TypeDescriptor, TypeConstraint};
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
    emit_tables(schema, &mut out);
    emit_fk_constraints(schema, &type_map, &mut out);
    emit_junction_tables(schema, &type_map, &mut out);
    emit_unique_indexes(schema, &mut out);
    emit_check_constraints(schema, &mut out);
    emit_plain_indexes(schema, &mut out);
    emit_triggers(schema, &mut out);
    emit_interface_views(schema, &mut out);

    Ok(out)
}

// ── Identifier helpers ─────────────────────────────────────────────────────────

/// Quote a PostgreSQL identifier: "name" with internal double-quotes escaped.
fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Schema-qualified name: "module"."name"
fn qn(module: &str, name: &str) -> String {
    format!("{}.{}", qi(module), qi(name))
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
    for module in &modules {
        out.push_str(&format!("CREATE SCHEMA IF NOT EXISTS {};\n", qi(module)));
    }
    if !modules.is_empty() {
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
            "CREATE TYPE {}.{} AS ENUM ({});\n",
            qi(&e.module),
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
            qi(&s.module),
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
        if t.abstract_ { continue; }
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
        lines.push(format!("    {} {}{}{}", qi(&p.name), p.pg_type, not_null, default));
    }

    // Link columns — uuid stubs; FK constraints added in phase 5
    for l in &t.links {
        let not_null = if l.nullable { "" } else { " NOT NULL" };
        lines.push(format!("    {} uuid{}", qi(&l.name), not_null));
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
}

// ── Phase 5: FK constraints for single links ───────────────────────────────────

fn emit_fk_constraints(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ { continue; }
        for l in &t.links {
            let Some((tgt_module, tgt_table)) = type_map.get(&l.target) else { continue };
            let cname = qi(&format!("{}_{}_fkey", t.table, l.name));
            out.push_str(&format!(
                "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}(id);\n",
                qn(&t.module, &t.table),
                cname,
                qi(&l.name),
                qn(tgt_module, tgt_table),
            ));
            emitted = true;
        }
    }
    if emitted {
        out.push('\n');
    }
}

// ── Phase 6: junction tables for multi-links ───────────────────────────────────

fn emit_junction_tables(
    schema: &SchemaDescriptor,
    type_map: &HashMap<String, (&str, &str)>,
    out: &mut String,
) {
    for t in &schema.types {
        if t.abstract_ { continue; }
        for ml in &t.multilinks {
            let jt_name = format!("{}.{}", t.table, ml.name);
            out.push_str(&format!(
                "CREATE TABLE {} (\n    source uuid NOT NULL REFERENCES {}(id),\n",
                qn(&t.module, &jt_name),
                qn(&t.module, &t.table),
            ));
            if let Some((tgt_module, tgt_table)) = type_map.get(&ml.target) {
                out.push_str(&format!(
                    "    target uuid NOT NULL REFERENCES {}(id),\n",
                    qn(tgt_module, tgt_table),
                ));
            } else {
                out.push_str("    target uuid NOT NULL,\n");
            }
            out.push_str("    PRIMARY KEY (source, target)\n);\n\n");
        }
    }
}

// ── Phase 7: unique indexes ────────────────────────────────────────────────────

fn emit_unique_indexes(schema: &SchemaDescriptor, out: &mut String) {
    let mut emitted = false;
    for t in &schema.types {
        if t.abstract_ { continue; }
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
                    qname, qi(&l.name),
                ));
                emitted = true;
            }
        }
        for c in &t.constraints {
            if let TypeConstraint::Exclusive { fields, unless } = c {
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
        if t.abstract_ { continue; }
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
        if t.abstract_ { continue; }
        let qname = qn(&t.module, &t.table);

        for idx in &t.indexes {
            let unique = if idx.unique { "UNIQUE " } else { "" };
            let body = if let Some(expr) = &idx.expression {
                format!("({})", expr)
            } else {
                let cols: Vec<String> = idx.fields.iter().map(|f| qi(f)).collect();
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
        if t.abstract_ { continue; }
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

// ── Phase 11: interface views ──────────────────────────────────────────────────

fn emit_interface_views(schema: &SchemaDescriptor, out: &mut String) {
    // Build reverse map: "module::Name" → concrete types that implement it
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

        // Columns: interface's own properties + link columns
        let cols: Vec<String> = t.properties.iter()
            .map(|p| qi(&p.name))
            .chain(t.links.iter().map(|l| qi(&l.name)))
            .collect();
        let col_list = cols.join(", ");

        let selects: Vec<String> = impls.iter()
            .map(|impl_t| {
                format!("    SELECT {} FROM {}", col_list, qn(&impl_t.module, &impl_t.table))
            })
            .collect();

        out.push_str(&format!("CREATE VIEW {} AS\n", qn(&t.module, &t.table)));
        out.push_str(&selects.join("\n    UNION ALL\n"));
        out.push_str(";\n\n");
    }
}

/// Compile a schema-level PyQL expression fragment to a raw SQL expression string.
///
/// Separate entry point from `compile()` — called only by the schema exporter,
/// never by application code.
pub(crate) fn compile_fragment(
    _expression: &str,
    _context: &FragmentContext,
    _schema: &SchemaDescriptor,
) -> Result<String, PyQLError> {
    todo!("Fragment compilation not yet implemented")
}

/// Enclosing context for compiling a schema-level PyQL fragment.
#[derive(Debug, Clone)]
pub struct FragmentContext {
    /// Module-qualified name of the enclosing type, e.g. `default::Product`.
    pub enclosing_type: String,
    /// Name of the field this fragment belongs to, if any.
    pub field_name: Option<String>,
    /// Variables in scope for this fragment, e.g. `__subject__` for constraints/rewrites.
    pub scope_vars: Vec<String>,
}
