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
//!   declared. A body that does not compile at all is an error here, not a
//!   skip: nothing downstream re-reports it, because `export`'s
//!   `column_default` and `diff`'s `resolve_default` both drop an
//!   uncompilable default with `.ok()`, which is how a pointer declared
//!   `Default('std::uuid_generate_v7j()')` — a function that does not exist
//!   — used to reach Postgres as a column with no default at all and fail
//!   on the first insert instead.
//!
//!   The type half is still not exhaustive: `infer_ir_type` types what it
//!   recognizes (column refs, casts, literals, enum members, named tuples,
//!   function params, global params, slices, arithmetic, and any function
//!   call whose resolution recorded a scalar return type) and anything it
//!   cannot type is skipped rather than rejected.
//! - **Compile-only** (triggers, aliases, computed globals) — these have no
//!   single declared scalar type to compare against (a trigger handler is
//!   void, an alias/computed-global can select any shape), so only "does it
//!   compile" is checked.

use crate::error::{Position, PyQLError, PyQLFragmentError};
use crate::ir::{
    IrFreeExpr, IrRowSource, IrStmt, compile, compile_fn_body, compile_scalar_default_typed, compile_trigger_handler,
    infer_ir_type, types_compatible,
};
use crate::schema::SchemaDescriptor;

/// Why `expr` yields more than one value, if it does.
///
/// A pointer declared with a single scalar type (`Computed[pylon.Str, …]`)
/// promises one value per row. Two expressions quietly break that promise:
/// a path crossing a multilink, which compiles to `ARRAY(SELECT …)` and so
/// hands back a `text[]` behind a declared `text`; and a call to a
/// set-returning function, which PostgreSQL expands into rows wherever it
/// sits. Neither produced an error before — the first widened the value
/// silently and the second only failed once a query ran.
///
/// Only the top of the expression is examined, looking through the wrappers
/// a computed routinely picks up (a cast, a `coalesce`) — which covers a
/// pointer whose whole body is the offending expression, the form both of
/// these actually take. One buried inside a larger expression is not caught
/// here.
fn multi_valued(expr: &crate::ir::IrExpr, schema: &SchemaDescriptor) -> Option<String> {
    use crate::ir::IrExpr as E;
    match expr {
        E::FunctionCall(f) if f.schema.is_none() && f.name == "coalesce" => {
            f.args.first().and_then(|a| multi_valued(a, schema))
        }
        E::TypeCast(c) => multi_valued(&c.expr, schema),
        E::ArrayFromSelect(_) => Some("a path that crosses a multilink, so it yields many values".into()),
        E::SetOp { mode, .. } if *mode == crate::ir::SetOpMode::Array => {
            Some("a set operation, so it yields many values".into())
        }
        E::FunctionCall(f) => {
            let module = f.schema.as_deref()?;
            let fd = schema
                .functions
                .iter()
                .find(|d| d.module == module && d.name == f.name && d.return_is_set)?;
            Some(format!(
                "a call to set-returning function '{}::{}', so it yields many values",
                fd.module, fd.name
            ))
        }
        _ => None,
    }
}

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
            // Compiled as the pointer it is, not as a bare expression: a
            // computed that selects objects (`(select .emails limit 1)`)
            // legitimately has no scalar type, and compiling it as an
            // expression would reject it instead of skipping the check.
            let ir = match crate::ir::compile_computed_in_type(cd, &type_name, schema) {
                Ok(Some(ir)) => ir,
                Ok(None) => continue,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            // Cardinality before type: an array-valued expression has no
            // scalar type to compare, so reporting the mismatch as a *type*
            // error would name the wrong problem even when one is inferable.
            // A computed that declares an array type is asking for the many
            // values and is left alone.
            if !declared.ends_with("[]")
                && let Some(why) = multi_valued(&ir, schema)
            {
                errors.push(mismatch(
                    context.clone(),
                    format!(
                        "cardinality mismatch in computed pointer '{context}': declared {declared}, a single \
                         value, but the expression is {why} — declare it as an array \
                         (e.g. Computed[pylon.Array[...], …]) or reduce it to one value \
                         (e.g. with 'limit 1', 'assert_single()', or an aggregate)"
                    ),
                ));
                continue;
            }
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
            let context = format!("{}.{} (default)", type_name, prop.name);
            // A default a column DEFAULT cannot hold is expanded into the
            // insert instead, and checked there — see the loop over
            // `inlined_pointer_defaults` below, which reports for the
            // concrete types this abstract one's pointers land on.
            let Ok((_, ir)) = compile_scalar_default_typed(pyql, schema) else {
                continue;
            };
            if crate::ir::default_blocker(&ir).is_some() {
                continue;
            }
            let Some(actual) = infer_ir_type(&ir) else { continue };
            if !types_compatible(actual, &prop.pg_type) {
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
            let context = format!("{}.{} (default)", type_name, link.name);
            // As for a property above: one a column DEFAULT cannot hold is
            // the insert's to apply, and the insert's to be checked against.
            let Ok((_, ir)) = compile_scalar_default_typed(pyql, schema) else {
                continue;
            };
            if crate::ir::default_blocker(&ir).is_some() {
                continue;
            }
            let Some(actual) = infer_ir_type(&ir) else { continue };
            if !types_compatible(actual, "uuid") {
                errors.push(mismatch(
                    context.clone(),
                    format!(
                        "default value type mismatch for '{}': expected uuid, default produces {}",
                        context, actual
                    ),
                ));
            }
        }

        // Defaults a column DEFAULT cannot hold, compiled the way an insert
        // expands them into its own shape. A default that compiles as neither
        // is a real error, and this is where it surfaces.
        if !td.abstract_ && !td.junction {
            for (pointer, pyql) in crate::ir::inlined_pointer_defaults(td, schema) {
                let context = format!("{type_name}.{pointer} (default)");
                let (column, ir) = match crate::ir::compile_inlined_default(&type_name, &pointer, &pyql, schema) {
                    Ok(assignment) => assignment,
                    Err(e) => {
                        errors.push(mismatch(context.clone(), format!("default for '{context}': {e}")));
                        continue;
                    }
                };
                let Some(actual) = infer_ir_type(&ir) else { continue };
                let declared = td
                    .properties
                    .iter()
                    .find(|p| p.name == column)
                    .map(|p| p.pg_type.as_str())
                    .unwrap_or("uuid");
                if !types_compatible(actual, declared) {
                    errors.push(mismatch(
                        context.clone(),
                        format!(
                            "default value type mismatch for '{context}': expected {declared}, \
                             default produces {actual}"
                        ),
                    ));
                }
            }
        }

        // Mutation rewrites, compiled the way the type's `BEFORE` triggers
        // run them: against the row being written.
        if !td.abstract_ && !td.junction {
            for on in [1u8, 2] {
                let assignments = match crate::ir::compile_rewrite_assignments(&type_name, on, schema) {
                    Ok(assignments) => assignments,
                    Err(e) => {
                        let context = format!("{type_name} (rewrite)");
                        errors.push(mismatch(context.clone(), format!("{context}: {e}")));
                        continue;
                    }
                };
                for assignment in assignments {
                    let pg_type = td
                        .properties
                        .iter()
                        .find(|p| p.name == assignment.pointer)
                        .map(|p| p.pg_type.as_str())
                        .unwrap_or("uuid");
                    let Some(actual) = infer_ir_type(&assignment.ir) else {
                        continue;
                    };
                    if !types_compatible(actual, pg_type) {
                        let context = format!("{}.{} (rewrite)", type_name, assignment.pointer);
                        errors.push(mismatch(
                            context.clone(),
                            format!(
                                "rewrite handler type mismatch for '{}': expected {}, handler produces {}",
                                context, pg_type, actual
                            ),
                        ));
                    }
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
                let context = format!("{type_name} (trigger)");
                let handler: String = trig.handler.chars().take(80).collect();
                errors.push(mismatch(context.clone(), format!("{context} `{handler}…`: {e}")));
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
        LinkDescriptor, PropertyDescriptor, RewriteEntry, TriggerDescriptor, TypeDescriptor,
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
            bases: vec![],
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
            ..Default::default()
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
    fn function_call_body_return_type_is_checked() {
        // A body that is a stdlib call is typed by the overload that call
        // resolves to, so a declared type the call cannot produce is caught
        // here rather than at the first query that runs it. `str_lower`
        // returns text, not int8.
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
        let errs = validate_schema_types(&schema).unwrap_err();
        let msg = errs[0].to_string();
        assert!(msg.contains("declared int8"), "{msg}");
        assert!(msg.contains("produces text"), "{msg}");
    }

    #[test]
    fn a_function_body_calling_a_user_function_checks_its_return_type() {
        // The caller's declared type is checked against the *callee's*
        // declared type — which needs the call itself to carry a type, not
        // just the stdlib ones.
        let callee = FunctionDescriptor {
            name: "gives_text".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "text".into(),
            body: "'x'".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let caller = FunctionDescriptor {
            name: "caller".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "int8".into(),
            body: "default::gives_text()".into(),
            return_is_object: false,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let schema = minimal_schema(vec![], vec![callee, caller]);
        let errs = validate_schema_types(&schema).unwrap_err();
        let msg = errs[0].to_string();
        assert!(msg.contains("declared int8"), "{msg}");
        assert!(msg.contains("produces text"), "{msg}");
    }

    #[test]
    fn computed_return_type_match_passes() {
        let cd = ComputedDescriptor {
            name: "double_id".into(),
            expression: ".id".into(),
            return_type: Some("uuid".into()),
            link_target: None,
            link_multi: false,
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
            link_target: None,
            link_multi: false,
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
    fn a_default_naming_a_function_that_does_not_exist_is_rejected() {
        // The case this whole hard-fail exists for: a schema converted from
        // a system whose own spelling was `uuid_generate_v7j` kept the name,
        // and every consumer of the default dropped it with `.ok()` — so the
        // column shipped with no DEFAULT at all and the first insert failed
        // on NOT NULL, a long way from the declaration that caused it.
        let mut prop = base_property("token", "uuid");
        prop.default_pyql = Some("std::uuid_generate_v7j()".into());
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("Person.token"), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
    }

    #[test]
    fn a_default_calling_a_real_function_wrongly_is_rejected() {
        let mut prop = base_property("name", "text");
        prop.default_pyql = Some("std::str_lower('A', 'B')".into());
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("Person.name"), "{msg}");
        assert!(msg.contains("takes 1 argument(s), got 2"), "{msg}");
    }

    #[test]
    fn a_default_that_is_a_valid_stdlib_call_still_passes() {
        let mut prop = base_property("token", "uuid");
        prop.default_pyql = Some("std::uuid_generate_v7()".into());
        let td = person_type(vec![], vec![prop]);
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn a_computed_calling_a_set_returning_function_declared_single_is_rejected() {
        let fd = FunctionDescriptor {
            name: "gives_many".into(),
            module: "default".into(),
            params: vec![],
            return_pg_type: "int8".into(),
            body: "{1, 2}".into(),
            return_is_object: false,
            return_is_set: true,
            return_is_polymorphic: false,
            volatility: "immutable".into(),
        };
        let cd = ComputedDescriptor {
            name: "n".into(),
            expression: "default::gives_many()".into(),
            return_type: Some("int8".into()),
            link_target: None,
            link_multi: false,
        };
        let td = person_type(vec![cd], vec![]);
        let schema = minimal_schema(vec![td], vec![fd]);
        let errs = validate_schema_types(&schema).unwrap_err();
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("cardinality mismatch"), "{msg}");
        assert!(msg.contains("gives_many"), "{msg}");
    }

    #[test]
    fn a_computed_crossing_a_multilink_declared_single_is_rejected() {
        // `.friends.name` compiles to `ARRAY(SELECT …)` — a text[] behind a
        // pointer that declared plain text.
        let cd = ComputedDescriptor {
            name: "friend_names".into(),
            expression: ".friends.name".into(),
            return_type: Some("text".into()),
            link_target: None,
            link_multi: false,
        };
        let mut td = person_type(vec![cd], vec![base_property("name", "text")]);
        td.multilinks = vec![crate::schema::MultiLinkDescriptor {
            name: "friends".into(),
            target: "default::Person".into(),
            through: None,
            nullable: true,
            description: None,
            default_pyql: None,
            on_delete: vec![],
            is_exclusive: false,
        }];
        let schema = minimal_schema(vec![td], vec![]);
        let errs = validate_schema_types(&schema).unwrap_err();
        let (_, msg, _) = errs[0].class_name_message_position();
        assert!(msg.contains("cardinality mismatch"), "{msg}");
        assert!(msg.contains("multilink"), "{msg}");
    }

    #[test]
    fn the_same_computed_declared_as_an_array_passes() {
        let cd = ComputedDescriptor {
            name: "friend_names".into(),
            expression: ".friends.name".into(),
            return_type: Some("text[]".into()),
            link_target: None,
            link_multi: false,
        };
        let mut td = person_type(vec![cd], vec![base_property("name", "text")]);
        td.multilinks = vec![crate::schema::MultiLinkDescriptor {
            name: "friends".into(),
            target: "default::Person".into(),
            through: None,
            nullable: true,
            description: None,
            default_pyql: None,
            on_delete: vec![],
            is_exclusive: false,
        }];
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
    }

    #[test]
    fn link_default_selecting_an_object_moves_into_the_insert() {
        // `DEFAULT (SELECT …)` is DDL PostgreSQL refuses to run, so this one
        // gets no column DEFAULT — the insert applies it instead, which is
        // how every default reaching for an object is applied.
        let mut td = person_type(vec![], vec![base_property("name", "text")]);
        td.links = vec![LinkDescriptor {
            name: "manager".into(),
            target: "default::Person".into(),
            nullable: true,
            through: None,
            description: None,
            default_pyql: Some("(select Person filter .name = 'boss' limit 1)".into()),
            is_exclusive: false,
            is_readonly: false,
            rewrites: vec![],
            on_delete: vec![],
        }];
        let schema = minimal_schema(vec![td], vec![]);
        validate_schema_types(&schema).expect("an inlined default is not an error");
        let inlined = crate::ir::inlined_pointer_defaults(&schema.types[0], &schema);
        assert_eq!(inlined.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["manager"]);
    }

    #[test]
    fn link_default_that_is_a_constant_passes() {
        let mut td = person_type(vec![], vec![base_property("name", "text")]);
        td.links = vec![LinkDescriptor {
            name: "manager".into(),
            target: "default::Person".into(),
            nullable: true,
            through: None,
            description: None,
            default_pyql: Some("<uuid>'00000000-0000-0000-0000-000000000000'".into()),
            is_exclusive: false,
            is_readonly: false,
            rewrites: vec![],
            on_delete: vec![],
        }];
        let schema = minimal_schema(vec![td], vec![]);
        assert!(validate_schema_types(&schema).is_ok());
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
