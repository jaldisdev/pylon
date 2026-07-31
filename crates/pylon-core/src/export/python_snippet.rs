//! Renders an "equivalent Python schema declaration" snippet for one
//! `MigrationStep` — shown alongside the DDL preview in the interactive
//! migration CLI, since Pylon has no separate SDL the way some other
//! schema-migration tools do, and DDL alone is less immediately readable
//! than the Python declaration it corresponds to.
//!
//! Best-effort and display-only: covers the common cases (scalar/enum
//! creation, object-type creation with its full pointer list, and
//! object-type alteration — which also shows the full current pointer
//! list, not just the changed ones, since a step's `MigrationStep` doesn't
//! carry a "before" shape to diff against here) and is not meant to be
//! round-trippable. The actual `.py` schema file stays authoritative.

use crate::diff::{MigrationStep, OpKey};
use crate::schema::{EnumDescriptor, ScalarDescriptor, SchemaDescriptor, TypeDescriptor};

/// `None` for step kinds this renderer doesn't cover yet (modules,
/// functions, interface views — see the module doc comment).
pub fn python_snippet_for_step(step: &MigrationStep, schema: &SchemaDescriptor) -> Option<String> {
    match &step.op_key {
        OpKey::Table(module, table) => {
            let td = schema.types.iter().find(|t| &t.module == module && &t.table == table)?;
            Some(python_snippet_for_type(td))
        }
        OpKey::Scalar(module, name) => {
            if let Some(e) = schema.enums.iter().find(|e| &e.module == module && &e.name == name) {
                return Some(python_snippet_for_enum(e));
            }
            let s = schema.scalars.iter().find(|s| &s.module == module && &s.name == name)?;
            Some(python_snippet_for_scalar(s))
        }
        OpKey::Module(_) | OpKey::Function(_, _) | OpKey::View(_, _) => None,
    }
}

fn python_snippet_for_type(td: &TypeDescriptor) -> String {
    let mut lines = vec!["@pylon.type".to_string(), format!("class {}:", td.name)];
    let mut body: Vec<String> = Vec::new();

    for p in &td.properties {
        if p.name == "id" {
            continue; // implicit primary key, never user-declared
        }
        let hint = python_type_hint(&p.pg_type, p.column_type.as_deref());
        let hint = if p.nullable { format!("{hint} | None") } else { hint };
        body.push(format!("    {}: {}", p.name, hint));
    }
    for l in &td.links {
        let hint = format!("Link[{}]", bare_name(&l.target));
        let hint = if l.nullable { format!("{hint} | None") } else { hint };
        body.push(format!("    {}: {}", l.name, hint));
    }
    for ml in &td.multilinks {
        body.push(format!("    {}: MultiLink[{}]", ml.name, bare_name(&ml.target)));
    }

    if body.is_empty() {
        lines.push("    pass".to_string());
    } else {
        lines.extend(body);
    }
    lines.join("\n")
}

fn python_snippet_for_enum(e: &EnumDescriptor) -> String {
    let members: Vec<String> = e.members.iter().map(|m| format!("\"{m}\"")).collect();
    format!("@pylon.enum({})\nclass {}(pylon.Enum):\n    pass", members.join(", "), e.name)
}

fn python_snippet_for_scalar(s: &ScalarDescriptor) -> String {
    format!("@pylon.scalar(pylon.{})\nclass {}(pylon.Scalar):\n    pass", s.base, s.name)
}

/// The trailing unqualified segment of a possibly `module::Name`-qualified
/// identifier, or of a schema-qualified quoted DDL reference like
/// `"module"."Name"` once dequoted.
fn bare_name(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// Best-effort PostgreSQL base type -> Pylon schema-DSL type hint. Not
/// exact for every case (arrays of registered scalars, named tuples) —
/// this is illustrative display only, not a compiler.
fn python_type_hint(pg_type: &str, column_type: Option<&str>) -> String {
    if let Some(ct) = column_type {
        // A registered custom scalar's own DOMAIN reference, schema-qualified
        // and quoted (e.g. `"module"."Name"`) — show just the bare class name.
        let dequoted = ct.replace("\".\"", "::").replace('"', "");
        return bare_name(&dequoted).to_string();
    }
    if let Some(elem) = pg_type.strip_suffix("[]") {
        return format!("list[{}]", python_type_hint(elem, None));
    }
    if let Some(nt_name) = pg_type.strip_prefix("__nt__:") {
        return bare_name(nt_name).to_string();
    }
    if pg_type.starts_with('"') {
        // Enum-typed property, same schema-qualified-and-quoted shape as
        // a registered scalar's `column_type`.
        let dequoted = pg_type.replace("\".\"", "::").replace('"', "");
        return bare_name(&dequoted).to_string();
    }
    match pg_type {
        "text" | "varchar" => "str".to_string(),
        "boolean" => "bool".to_string(),
        "float8" => "float".to_string(),
        "int4" => "int".to_string(),
        "int2" => "pylon.Int16".to_string(),
        "int8" => "pylon.Int64".to_string(),
        "float4" => "pylon.Float32".to_string(),
        "numeric" => "pylon.Decimal".to_string(),
        "timestamptz" => "pylon.DateTime".to_string(),
        "timestamp" => "pylon.LocalDateTime".to_string(),
        "date" => "pylon.LocalDate".to_string(),
        "time" => "pylon.LocalTime".to_string(),
        "uuid" => "pylon.UUID".to_string(),
        "bytea" => "pylon.Bytes".to_string(),
        "jsonb" => "pylon.Json".to_string(),
        "interval" => "pylon.Duration".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Verb;

    fn type_step(td: TypeDescriptor, verb: Verb) -> MigrationStep {
        MigrationStep {
            prompt: String::new(),
            verb,
            object_desc: String::new(),
            ddl: vec![],
            op_key: OpKey::Table(td.module.clone(), td.table.clone()),
        }
    }

    fn empty_type(module: &str, name: &str, table: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: name.into(), module: module.into(), table: table.into(),
            abstract_: false, materialized: false, description: None,
            parents: vec![], interfaces: vec![],
            properties: vec![], links: vec![], multilinks: vec![], computed: vec![],
            constraints: vec![], indexes: vec![],
            vector_indexes: vec![], search_indexes: vec![], triggers: vec![],
            junction: false, signals: vec![],
        }
    }

    fn prop(name: &str, pg_type: &str, nullable: bool) -> crate::schema::PropertyDescriptor {
        crate::schema::PropertyDescriptor {
            name: name.into(), pg_type: pg_type.into(), nullable,
            default_sql: None, default_pyql: None, description: None,
            check_constraints: vec![], is_exclusive: false, is_pk: name == "id",
            is_readonly: name == "id", rewrites: vec![],
            tuple_members: None, column_type: None,
        }
    }

    #[test]
    fn renders_a_simple_object_type() {
        let mut td = empty_type("blog", "Post", "Post");
        td.properties.push(prop("id", "uuid", false));
        td.properties.push(prop("title", "text", false));
        td.properties.push(prop("views", "int8", true));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let step = type_step(schema.types[0].clone(), Verb::Create);
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(
            snippet,
            "@pylon.type\nclass Post:\n    title: str\n    views: pylon.Int64 | None"
        );
    }

    #[test]
    fn renders_an_enum() {
        let schema = SchemaDescriptor {
            types: vec![],
            scalars: vec![],
            enums: vec![EnumDescriptor { name: "Status".into(), module: "default".into(), members: vec!["Active".into(), "Inactive".into()] }],
            named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            op_key: OpKey::Scalar("default".into(), "Status".into()),
        };
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(snippet, "@pylon.enum(\"Active\", \"Inactive\")\nclass Status(pylon.Enum):\n    pass");
    }

    #[test]
    fn returns_none_for_a_module_step() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            op_key: OpKey::Module("catalog".into()),
        };
        assert!(python_snippet_for_step(&step, &schema).is_none());
    }
}
