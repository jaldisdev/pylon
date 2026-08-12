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

//! Post-build schema validation.
//!
//! `walk()` (the Python schema builder) checks structure — duplicate names,
//! dangling references, link cycles, interface conformance — but every PyQL
//! body embedded in the schema (function bodies, computed pointers,
//! defaults, mutation rewrites, triggers, aliases, computed globals) used to
//! only ever get compiled lazily, the first time something actually
//! exercised it (a query, a migration, a DDL export) — so a broken one could
//! sit undetected in the schema indefinitely. This module closes that gap by
//! eagerly compiling every one of them at `finalize()` time.
//!
//! Two kinds of check:
//! - **Type consistency** (functions, computed pointers, defaults, rewrites)
//!   — the body compiles *and* its inferred return type matches what's
//!   declared. Best-effort, not exhaustive: `infer_ir_type` only recognizes a
//!   handful of `IrExpr` variants (column refs, casts, literals, enum
//!   members, named tuples, function params, global params) — anything else
//!   (a binop, a function call, an if/else) is silently skipped rather than
//!   rejected. This still catches the common, real-world cases while leaving
//!   complex expressions as a known limitation a future pass can extend
//!   `infer_ir_type` to cover.
//! - **Compile-only** (triggers, aliases, computed globals) — these have no
//!   single declared scalar type to compare against (a trigger handler is
//!   void, an alias/computed-global can select any shape), so only "does it
//!   compile" is checked.

use crate::error::{Position, PyQLError, PyQLFragmentError};
use crate::ir::{
    IrFreeExpr, IrRowSource, IrStmt, compile, compile_expr_in_type, compile_fn_body, compile_scalar_default_typed,
    compile_trigger_handler, infer_ir_type, types_compatible,
};
use crate::schema::SchemaDescriptor;

fn mismatch(context: String, message: String) -> PyQLError {
    PyQLError::Fragment(PyQLFragmentError {
        message,
        position: Position { line: 0, col: 0 },
        context,
    })
}

/// Partition-key column types PostgreSQL range partitioning is supported on
/// here. Range partitioning works on any orderable type, but the automatic
/// "create the next N ranges, drop past retention" maintenance only makes
/// sense against time.
const PARTITIONABLE_PG_TYPES: [&str; 3] = ["timestamptz", "timestamp", "date"];

/// Validates every `Partition` declaration in `schema`.
///
/// Each of these is a constraint PostgreSQL itself would reject later — but
/// later means at migration time, as a raw Postgres error against generated
/// DDL. Catching them here reports them against the schema the author wrote.
pub fn validate_partitions(schema: &SchemaDescriptor) -> Vec<PyQLError> {
    let mut errors = Vec::new();

    for td in &schema.types {
        let Some(part) = &td.partition else { continue };
        let type_name = format!("{}::{}", td.module, td.name);
        let context = format!("{type_name} (partition)");

        // An abstract type has no table, so there is nothing to partition —
        // its fields flatten into each concrete subtype, and partitioning
        // each of those is a decision each one has to make for itself.
        if td.abstract_ && !td.materialized {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "type '{type_name}' is abstract and has no table of its own, so it cannot declare a Partition — \
                     declare it on each concrete type instead"
                ),
            ));
            continue;
        }
        // An interface is a view over its implementors; a view has no
        // storage to partition either.
        if td.abstract_ && td.materialized {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "type '{type_name}' is an interface, backed by a view rather than a table, so it cannot declare \
                     a Partition — declare it on each implementing type instead"
                ),
            ));
            continue;
        }

        let Some(prop) = td.properties.iter().find(|p| p.name == part.pointer) else {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "Partition on '{type_name}' names pointer '{}', which is not a property of this type",
                    part.pointer
                ),
            ));
            continue;
        };

        if !PARTITIONABLE_PG_TYPES.contains(&prop.pg_type.as_str()) {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "Partition on '{type_name}' names property '{}' of type '{}' — the partition key must be a \
                     datetime or date property",
                    part.pointer, prop.pg_type
                ),
            ));
        }

        // PostgreSQL rejects a NULL partition key outright: there is no
        // range for it to land in.
        if prop.nullable {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "Partition on '{type_name}' names optional property '{}' — a partition key can never be empty",
                    part.pointer
                ),
            ));
        }

        if part.premake == 0 {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "Partition on '{type_name}' has premake=0 — with no future partitions pre-created, the first \
                     write past the current range fails"
                ),
            ));
        }
    }

    errors
}

/// Compile every user function body, computed-pointer expression, and
/// property/link default in `schema`, and collect every declared-vs-actual
/// return-type mismatch found — not fail-fast, so a caller can report every
/// problem in the schema at once rather than a fix-one-rerun loop. Partition
/// declarations (`validate_partitions`) are checked in the same pass, for the
/// same reason.
pub fn validate_schema_types(schema: &SchemaDescriptor) -> Result<(), Vec<PyQLError>> {
    let mut errors = validate_partitions(schema);

    for fd in &schema.functions {
        // Object-returning functions build a row shape, not a single scalar
        // IrExpr — matching-object-shape correctness is a separate, larger
        // problem than this pass's scope.
        if fd.return_is_object {
            continue;
        }
        let context = format!("{}::{}", fd.module, fd.name);
        let ir_output = match compile_fn_body(fd, schema) {
            Ok(o) => o,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let IrStmt::Select(sel) = &ir_output.stmt else { continue };
        let [IrRowSource::Free(IrFreeExpr::Scalar(e))] = sel.rows.as_slice() else {
            continue;
        };
        let Some(actual) = infer_ir_type(e) else { continue };
        if !types_compatible(actual, &fd.return_pg_type) {
            errors.push(mismatch(
                context.clone(),
                format!(
                    "return type mismatch in function '{}': declared {}, body produces {}",
                    context, fd.return_pg_type, actual
                ),
            ));
        }
    }

    for td in &schema.types {
        let type_name = format!("{}::{}", td.module, td.name);

        for cd in &td.computed {
            let Some(declared) = &cd.return_type else { continue };
            let context = format!("{}.{} (computed)", type_name, cd.name);
            let expr_ast = match crate::parse::parse_expr(&cd.expression) {
                Ok(e) => e,
                Err(e) => {
                    errors.push(PyQLError::Syntax(e));
                    continue;
                }
            };
            let ir = match compile_expr_in_type(&expr_ast, &type_name, schema) {
                Ok((ir, _params)) => ir,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            let Some(actual) = infer_ir_type(&ir) else { continue };
            if !types_compatible(actual, declared) {
                errors.push(mismatch(
                    context.clone(),
                    format!(
                        "return type mismatch in computed pointer '{}': declared {}, expression produces {}",
                        context, declared, actual
                    ),
                ));
            }
        }

        for prop in &td.properties {
            let Some(pyql) = &prop.default_pyql else { continue };
            // Best-effort: a default that fails to compile here is silently
            // skipped rather than collected — `pylon migrate` is still the
            // authority on default-expression validity itself, this pass
            // only adds a type-consistency check on top of an already-valid
            // one.
            let Ok((_, ir)) = compile_scalar_default_typed(pyql, schema) else {
                continue;
            };
            let Some(actual) = infer_ir_type(&ir) else { continue };
            if !types_compatible(actual, &prop.pg_type) {
                let context = format!("{}.{} (default)", type_name, prop.name);
                errors.push(mismatch(
                    context.clone(),
                    format!(
                        "default value type mismatch for '{}': expected {}, default produces {}",
                        context, prop.pg_type, actual
                    ),
                ));
            }
        }

        for link in &td.links {
            let Some(pyql) = &link.default_pyql else { continue };
            let Ok((_, ir)) = compile_scalar_default_typed(pyql, schema) else {
                continue;
            };
            let Some(actual) = infer_ir_type(&ir) else { continue };
            if !types_compatible(actual, "uuid") {
                let context = format!("{}.{} (default)", type_name, link.name);
                errors.push(mismatch(
                    context.clone(),
                    format!(
                        "default value type mismatch for '{}': expected uuid, default produces {}",
                        context, actual
                    ),
                ));
            }
        }

        // Mutation rewrites: only `PropertyDescriptor.rewrites` is ever read
        // by the real INSERT/UPDATE compiler (`Compiler::compile_rewrites`)
        // — `LinkDescriptor.rewrites` exists on the struct but nothing
        // compiles it, so there's nothing to validate there yet.
        for prop in &td.properties {
            for rw in &prop.rewrites {
                let context = format!("{}.{} (rewrite)", type_name, prop.name);
                let expr_ast = match crate::parse::parse_expr(&rw.handler) {
                    Ok(e) => e,
                    Err(e) => {
                        errors.push(PyQLError::Syntax(e));
                        continue;
                    }
                };
                let ir = match compile_expr_in_type(&expr_ast, &type_name, schema) {
                    Ok((ir, _params)) => ir,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };
                let Some(actual) = infer_ir_type(&ir) else { continue };
                if !types_compatible(actual, &prop.pg_type) {
                    errors.push(mismatch(
                        context.clone(),
                        format!(
                            "rewrite handler type mismatch for '{}': expected {}, handler produces {}",
                            context, prop.pg_type, actual
                        ),
                    ));
                }
            }
        }

        // Schema-defined triggers: compile-only — a trigger handler has no
        // declared return type to check (it's a side-effecting statement,
        // not a value producer), but it currently only ever gets compiled
        // at DDL-emission time (`export::emit_triggers`), so a broken
        // handler on a type nobody's exported yet would otherwise pass
        // `finalize()` silently.
        for trig in &td.triggers {
            if let Err(e) = compile_trigger_handler(&trig.handler, &type_name, trig.on, schema) {
                errors.push(e);
            }
        }
    }

    // Schema aliases: compile-only — an alias has no declared return type at
    // all (it's just a named query fragment that can select any shape, not
    // just a scalar), so there's nothing to type-compare against. But like
    // triggers, an alias's own body currently only ever gets compiled lazily,
    // the first time a query actually references it (`try_compile_alias_select`)
    // — so a broken alias nobody's queried yet would otherwise pass
    // `finalize()` silently.
    for alias in &schema.aliases {
        let parsed = match crate::parse::parse(&alias.expr) {
            Ok(ast) => ast,
            Err(e) => {
                errors.push(PyQLError::Syntax(e));
                continue;
            }
        };
        if let Err(e) = compile(&parsed, schema) {
            errors.push(e);
        }
    }

    // Computed globals: compile-only, same reasoning as aliases — a computed
    // global (`select User filter .id = global current_user_id`) can select
    // any shape, not just a scalar matching `scalar_type`, and it's also
    // only ever compiled lazily today, the first time a query references it
    // (`Compiler::compile_global`). `Global.default_expr` (as opposed to
    // `computed_expr`) is deliberately not checked here — it's always a raw
    // SQL literal built from a plain Python value at schema-build time
    // (`_python_value_to_sql`), never a PyQL expression, so there's nothing
    // to compile.
    for global in &schema.globals {
        let Some(computed_expr) = &global.computed_expr else {
            continue;
        };
        let parsed = match crate::parse::parse(computed_expr) {
            Ok(ast) => ast,
            Err(e) => {
                errors.push(PyQLError::Syntax(e));
                continue;
            }
        };
        if let Err(e) = compile(&parsed, schema) {
            errors.push(e);
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        AliasDescriptor, ComputedDescriptor, FunctionDescriptor, FunctionParamDescriptor, GlobalDescriptor,
        PropertyDescriptor, RewriteEntry, TriggerDescriptor, TypeDescriptor,
    };

    // ── Partition validation ──────────────────────────────────────────

    fn ts_prop(name: &str, nullable: bool) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(),
            pg_type: "timestamptz".into(),
            nullable,
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
        }
    }

    fn partitioned(part: crate::schema::PartitionDescriptor, props: Vec<PropertyDescriptor>) -> SchemaDescriptor {
        let mut td = person_type(vec![], props);
        td.partition = Some(part);
        SchemaDescriptor {
            types: vec![td],
            ..Default::default()
        }
    }

    fn monthly(pointer: &str) -> crate::schema::PartitionDescriptor {
        crate::schema::PartitionDescriptor {
            pointer: pointer.into(),
            interval: crate::schema::PartitionInterval::Monthly,
            premake: 4,
            retention: None,
        }
    }

    fn only_error(schema: &SchemaDescriptor) -> String {
        let errors = validate_partitions(schema);
        assert_eq!(errors.len(), 1, "expected exactly one error, got {errors:#?}");
        errors[0].to_string()
    }

    #[test]
    fn a_valid_partition_passes() {
        let schema = partitioned(monthly("occurred_at"), vec![ts_prop("occurred_at", false)]);
        assert!(validate_partitions(&schema).is_empty());
    }

    #[test]
    fn a_partition_on_an_abstract_type_is_rejected() {
        // An abstract type has no table to partition — its fields flatten
        // into each concrete subtype.
        let mut schema = partitioned(monthly("occurred_at"), vec![ts_prop("occurred_at", false)]);
        schema.types[0].abstract_ = true;
        schema.types[0].materialized = false;
        assert!(only_error(&schema).contains("abstract"));
    }

    #[test]
    fn a_partition_on_an_interface_is_rejected() {
        // An interface is a view; a view has no storage either.
        let mut schema = partitioned(monthly("occurred_at"), vec![ts_prop("occurred_at", false)]);
        schema.types[0].abstract_ = true;
        schema.types[0].materialized = true;
        assert!(only_error(&schema).contains("interface"));
    }

    #[test]
    fn a_partition_on_an_unknown_pointer_is_rejected() {
        let schema = partitioned(monthly("nope"), vec![ts_prop("occurred_at", false)]);
        assert!(only_error(&schema).contains("not a property"));
    }

    #[test]
    fn a_partition_on_a_non_temporal_property_is_rejected() {
        let mut prop = ts_prop("occurred_at", false);
        prop.pg_type = "text".into();
        let schema = partitioned(monthly("occurred_at"), vec![prop]);
        assert!(only_error(&schema).contains("datetime or date"));
    }

    #[test]
    fn a_partition_on_an_optional_property_is_rejected() {
        // PostgreSQL has no range for a NULL key to land in.
        let schema = partitioned(monthly("occurred_at"), vec![ts_prop("occurred_at", true)]);
        assert!(only_error(&schema).contains("can never be empty"));
    }

    #[test]
    fn premake_zero_is_rejected() {
        let mut part = monthly("occurred_at");
        part.premake = 0;
        let schema = partitioned(part, vec![ts_prop("occurred_at", false)]);
        assert!(only_error(&schema).contains("premake=0"));
    }

    #[test]
    fn date_and_naive_timestamp_keys_are_accepted() {
        for pg_type in ["date", "timestamp"] {
            let mut prop = ts_prop("occurred_at", false);
            prop.pg_type = pg_type.into();
            let schema = partitioned(monthly("occurred_at"), vec![prop]);
            assert!(
                validate_partitions(&schema).is_empty(),
                "{pg_type} should be a valid partition key"
            );
        }
    }

    #[test]
    fn retention_renders_as_a_postgres_interval() {
        use crate::schema::{PartitionDescriptor, PartitionInterval};
        let with = |interval, retention| {
            PartitionDescriptor {
                pointer: "t".into(),
                interval,
                premake: 4,
                retention: Some(retention),
            }
            .retention_interval()
        };
        assert_eq!(with(PartitionInterval::Daily, 30), Some("30 days".to_string()));
        assert_eq!(with(PartitionInterval::Monthly, 12), Some("12 months".to_string()));
        assert_eq!(with(PartitionInterval::Yearly, 7), Some("7 years".to_string()));
        assert_eq!(monthly("t").retention_interval(), None);
    }

    fn person_type(computed: Vec<ComputedDescriptor>, properties: Vec<PropertyDescriptor>) -> TypeDescriptor {
        let mut props = vec![PropertyDescriptor {
            name: "id".into(),
            pg_type: "uuid".into(),
            nullable: false,
            default_sql: Some("uuidv7()".into()),
            default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: true,
            is_pk: true,
            is_readonly: false,
            rewrites: vec![],
            tuple_members: None,
            column_type: None,
        }];
        props.extend(properties);
        TypeDescriptor {
            name: "Person".into(),
            module: "default".into(),
            table: "default_person".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: props,
            links: vec![],
            multilinks: vec![],
            computed,
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

    fn base_property(name: &str, pg_type: &str) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.into(),
            pg_type: pg_type.into(),
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
            column_type: None,
        }
    }

    fn minimal_schema(types: Vec<TypeDescriptor>, functions: Vec<FunctionDescriptor>) -> SchemaDescriptor {
        SchemaDescriptor {
            types,
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions,
            aliases: vec![],
            channels: vec![],
        }
    }

    #[test]
    fn function_return_type_match_passes() {
        let fd = FunctionDescriptor {
            name: "myid".into(),
            module: "default".into(),
            params: vec![FunctionParamDescriptor {
                name: "a".into(),
                pg_type: "int8".into(),
            }],
            return_pg_type: "int8".into(),
            body: "a".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let schema = minimal_schema(vec![], vec![fd]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn function_return_type_mismatch_rejected() {
        let fd = FunctionDescriptor {
            name: "bad".into(),
            module: "default".into(),
            params: vec![FunctionParamDescriptor {
                name: "a".into(),
                pg_type: "text".into(),
            }],
            return_pg_type: "int8".into(),
            body: "a".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let schema = minimal_schema(vec![], vec![fd]);
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("bad"), "{msg}");
        assert!(msg.contains("declared int8"), "{msg}");
        assert!(msg.contains("produces text"), "{msg}");
    }

    #[test]
    fn function_call_body_is_skipped_not_rejected() {
        // `infer_ir_type` doesn't recognize FunctionCall, so a body built
        // from a stdlib call (or anything else it can't type) must be
        // silently skipped rather than falsely flagged — even though this
        // one's declared type is deliberately wrong (str_lower returns
        // text, not int8).
        let fd = FunctionDescriptor {
            name: "caller".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "int8".into(),
            body: "str_lower('X')".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let schema = minimal_schema(vec![], vec![fd]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn computed_return_type_match_passes() {
        let cd = ComputedDescriptor {
            name: "double_id".into(),
            expression: ".id".into(),
            return_type: Some("uuid".into()),
        };
        let td = person_type(vec![cd], vec![]);
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn computed_return_type_mismatch_rejected() {
        let cd = ComputedDescriptor {
            name: "bad".into(),
            expression: ".id".into(),
            return_type: Some("text".into()),
        };
        let td = person_type(vec![cd], vec![]);
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("Person.bad"), "{msg}");
    }

    #[test]
    fn default_type_match_passes() {
        let mut prop = base_property("score", "int8");
        prop.default_pyql = Some("1".into());
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn default_type_mismatch_rejected() {
        let mut prop = base_property("score", "int8");
        prop.default_pyql = Some("'not a number'".into());
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("Person.score"), "{msg}");
    }

    #[test]
    fn rewrite_type_match_passes() {
        let mut prop = base_property("name", "text");
        prop.rewrites = vec![RewriteEntry {
            on: 1,
            handler: "'unnamed'".into(),
        }];
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn rewrite_type_mismatch_rejected() {
        let mut prop = base_property("name", "text");
        prop.rewrites = vec![RewriteEntry {
            on: 1,
            handler: "1".into(),
        }];
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("Person.name (rewrite)"), "{msg}");
        assert!(msg.contains("expected text"), "{msg}");
    }

    #[test]
    fn trigger_handler_compiles_passes() {
        let mut td = person_type(vec![], vec![]);
        td.triggers = vec![TriggerDescriptor {
            on: 1,
            timing: "After".into(),
            handler: "select Person".into(),
        }];
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn trigger_handler_unknown_field_rejected() {
        let mut td = person_type(vec![], vec![]);
        td.triggers = vec![TriggerDescriptor {
            on: 1,
            timing: "After".into(),
            handler: "select Person filter .nonexistent_field = 1".into(),
        }];
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn alias_compiles_passes() {
        let td = person_type(vec![], vec![]);
        let mut schema = minimal_schema(vec![td], vec![]);
        schema.aliases = vec![AliasDescriptor {
            name: "all_people".into(),
            module: "default".into(),
            expr: "select Person".into(),
        }];
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn alias_unknown_type_rejected() {
        let mut schema = minimal_schema(vec![], vec![]);
        schema.aliases = vec![AliasDescriptor {
            name: "bad".into(),
            module: "default".into(),
            expr: "select NoSuchType".into(),
        }];
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
    }

    fn base_global(name: &str) -> GlobalDescriptor {
        GlobalDescriptor {
            name: name.into(),
            module: "default".into(),
            scalar_type: "std::str".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        }
    }

    #[test]
    fn computed_global_compiles_passes() {
        let td = person_type(vec![], vec![]);
        let mut schema = minimal_schema(vec![td], vec![]);
        let mut g = base_global("first_person");
        g.computed_expr = Some("select Person".into());
        schema.globals = vec![g];
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn computed_global_unknown_type_rejected() {
        let mut schema = minimal_schema(vec![], vec![]);
        let mut g = base_global("bad");
        g.computed_expr = Some("select NoSuchType".into());
        schema.globals = vec![g];
        let errs = validate_schema_types(&schema).unwrap_err();
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn session_global_with_no_computed_expr_is_untouched() {
        // A plain (non-computed) global has only a `default_expr`, which is
        // always a pre-built raw SQL literal (see `_python_value_to_sql`),
        // never PyQL — nothing to compile, so this must never be flagged.
        let mut schema = minimal_schema(vec![], vec![]);
        let mut g = base_global("current_user_id");
        g.default_expr = Some("'not actually pyql, just a sql literal'".into());
        schema.globals = vec![g];
        assert!(validate_schema_types(&schema).is_ok());
    }
}
