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

//! Renders an "equivalent Python schema declaration" snippet for one
//! `MigrationStep` — shown alongside the DDL preview in the interactive
//! migration CLI, since Pylon has no separate SDL the way some other
//! schema-migration tools do, and DDL alone is less immediately readable
//! than the Python declaration it corresponds to.
//!
//! Best-effort and display-only: covers the common cases (scalar/enum
//! creation, object-type and interface-type creation with the full pointer
//! list, object-type alteration — which also shows the full current
//! pointer list, not just the changed ones, since a step's `MigrationStep`
//! doesn't carry a "before" shape to diff against here — and functions,
//! shown as their original PyQL body rather than the compiled SQL a
//! function's DDL preview otherwise shows, since PyQL's compiled form is a
//! deeply nested CTE tree that reads nothing like what the user actually
//! wrote) and is not meant to be round-trippable. The actual `.py` schema
//! file stays authoritative.

use crate::diff::{MigrationStep, OpKey};
use crate::schema::{EnumDescriptor, FunctionDescriptor, ScalarDescriptor, SchemaDescriptor, TypeDescriptor};

/// `None` for step kinds this renderer doesn't cover yet (modules — see
/// the module doc comment).
pub fn python_snippet_for_step(step: &MigrationStep, schema: &SchemaDescriptor) -> Option<String> {
    match &step.op_key {
        OpKey::Table(module, table) => {
            let td = schema.types.iter().find(|t| &t.module == module && &t.table == table)?;
            Some(python_snippet_for_class(td, "@pylon.type"))
        }
        OpKey::View(module, name) => {
            let td = schema.types.iter().find(|t| &t.module == module && &t.name == name)?;
            Some(python_snippet_for_class(td, "@pylon.interface"))
        }
        OpKey::Scalar(module, name) => {
            if let Some(e) = schema.enums.iter().find(|e| &e.module == module && &e.name == name) {
                return Some(python_snippet_for_enum(e));
            }
            let s = schema.scalars.iter().find(|s| &s.module == module && &s.name == name)?;
            Some(python_snippet_for_scalar(s))
        }
        OpKey::Function(module, name) => {
            let f = schema.functions.iter().find(|f| &f.module == module && &f.name == name)?;
            Some(python_snippet_for_function(f))
        }
        OpKey::Module(_) => None,
    }
}

/// Shared by object types (`@pylon.type`) and interface types
/// (`@pylon.interface`) — same pointer declarations either way.
fn python_snippet_for_class(td: &TypeDescriptor, decorator: &str) -> String {
    let mut lines = vec![decorator.to_string(), format!("class {}:", td.name)];
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

fn python_snippet_for_function(f: &FunctionDescriptor) -> String {
    let params: Vec<String> = f.params.iter().map(|p| format!("{}: {}", p.name, python_type_hint(&p.pg_type, None))).collect();

    let mut return_hint = if f.return_is_object { bare_name(&f.return_pg_type).to_string() } else { python_type_hint(&f.return_pg_type, None) };
    if f.return_is_set {
        return_hint = format!("set[{return_hint}]");
    }

    format!(
        "@pylon.function\ndef {}({}) -> {}:\n    \"\"\"\n{}\n    \"\"\"",
        f.name,
        params.join(", "),
        return_hint,
        indent_block(f.body.trim(), "    "),
    )
}

/// Indents every line of `text` by `indent` — a multi-line PyQL body needs
/// each of its own lines re-indented under the docstring, not just the
/// first (its *relative* indentation between lines, e.g. a `with`
/// binding's continuation, is left untouched — only a uniform baseline is
/// added).
fn indent_block(text: &str, indent: &str) -> String {
    text.lines()
        .map(|line| if line.is_empty() { line.to_string() } else { format!("{indent}{line}") })
        .collect::<Vec<_>>()
        .join("\n")
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
            required_input: vec![],
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
    fn renders_an_interface_type_as_pylon_interface_not_the_view_sql() {
        let mut td = empty_type("default", "Account", "Account");
        td.abstract_ = true;
        td.materialized = true;
        td.properties.push(prop("id", "uuid", false));
        td.properties.push(prop("email", "text", false));
        let schema = SchemaDescriptor {
            types: vec![td],
            scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            required_input: vec![],
            op_key: OpKey::View("default".into(), "Account".into()),
        };
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(snippet, "@pylon.interface\nclass Account:\n    email: str");
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
            required_input: vec![],
            op_key: OpKey::Scalar("default".into(), "Status".into()),
        };
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(snippet, "@pylon.enum(\"Active\", \"Inactive\")\nclass Status(pylon.Enum):\n    pass");
    }

    #[test]
    fn renders_a_scalar_function_as_its_pyql_body_not_compiled_sql() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], aliases: vec![],
            functions: vec![FunctionDescriptor {
                name: "get_content_type".into(),
                module: "default".into(),
                params: vec![crate::schema::FunctionParamDescriptor { name: "uuid_val".into(), pg_type: "uuid".into() }],
                return_pg_type: "int2".into(),
                return_is_object: false,
                return_is_set: false,
                return_is_polymorphic: false,
                volatility: "immutable".into(),
                body: "select 1".into(),
            }],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            required_input: vec![],
            op_key: OpKey::Function("default".into(), "get_content_type".into()),
        };
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(
            snippet,
            "@pylon.function\ndef get_content_type(uuid_val: pylon.UUID) -> pylon.Int16:\n    \"\"\"\n    select 1\n    \"\"\""
        );
    }

    #[test]
    fn indents_every_line_of_a_multiline_pyql_body() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], aliases: vec![],
            functions: vec![FunctionDescriptor {
                name: "get_content_type".into(),
                module: "default".into(),
                params: vec![crate::schema::FunctionParamDescriptor { name: "uuid_val".into(), pg_type: "uuid".into() }],
                return_pg_type: "int8".into(),
                return_is_object: false,
                return_is_set: false,
                return_is_polymorphic: false,
                volatility: "immutable".into(),
                body: "with\n  h := str_replace(<str>uuid_val, '-', ''),\n  ct_bytes := std::from_hex(h[20:24])\nselect ct_bytes".into(),
            }],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            required_input: vec![],
            op_key: OpKey::Function("default".into(), "get_content_type".into()),
        };
        let snippet = python_snippet_for_step(&step, &schema).unwrap();
        assert_eq!(
            snippet,
            "@pylon.function\ndef get_content_type(uuid_val: pylon.UUID) -> pylon.Int64:\n    \"\"\"\n    with\n      h := str_replace(<str>uuid_val, '-', ''),\n      ct_bytes := std::from_hex(h[20:24])\n    select ct_bytes\n    \"\"\""
        );
    }

    #[test]
    fn function_snippet_survives_the_real_diff_pipeline() {
        // Regression guard: `renders_a_scalar_function_...` above only
        // exercises `python_snippet_for_step` with a hand-built `MigrationStep`
        // — this instead goes through the actual
        // `diff_schema_steps_with_renames_and_fills` entry point the CLI
        // calls, to catch a mismatch between the OpKey a real diff pass
        // produces and what this renderer looks up.
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], aliases: vec![],
            functions: vec![FunctionDescriptor {
                name: "get_content_type".into(),
                module: "default".into(),
                params: vec![crate::schema::FunctionParamDescriptor { name: "uuid_val".into(), pg_type: "uuid".into() }],
                return_pg_type: "int2".into(),
                return_is_object: false,
                return_is_set: false,
                return_is_polymorphic: false,
                volatility: "immutable".into(),
                body: "select 1".into(),
            }],
        };
        let steps = crate::diff::diff_schema_steps_with_renames_and_fills(
            &schema, &crate::diff::DbState::default(), &[], &[], &[],
        ).unwrap();
        let step = steps.iter().find(|s| s.prompt.contains("get_content_type")).expect("expected a step for get_content_type");
        let snippet = python_snippet_for_step(step, &schema);
        assert!(snippet.is_some(), "expected a snippet for the function step, prompt was: {:?}", step.prompt);
    }

    #[test]
    fn returns_none_for_a_module_step() {
        let schema = SchemaDescriptor {
            types: vec![], scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        };
        let step = MigrationStep {
            prompt: String::new(), verb: Verb::Create, object_desc: String::new(), ddl: vec![],
            required_input: vec![],
            op_key: OpKey::Module("catalog".into()),
        };
        assert!(python_snippet_for_step(&step, &schema).is_none());
    }
}
