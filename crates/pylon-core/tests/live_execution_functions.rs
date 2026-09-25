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

//! Live-Postgres tests for `@pylon.function` execution — a scalar-returning
//! function, an object-set-returning function, a function composed inside a
//! larger query, and overload resolution. Scoped to Pylon's own stdlib
//! surface rather than an exhaustive sweep (a huge overload set and
//! named/default-argument calling conventions Pylon's user-function call
//! path doesn't support — only positional args do).
//!
//! Phase 3 this session (`validate.rs`) added compile-time return-type
//! checking for function bodies, but nothing before this file ever proved a
//! compiled `CREATE FUNCTION` actually *executes* correctly against real
//! data — this closes that gap, and is also the first live test targeting
//! `Compiler::compile_expr`'s user-function call-resolution path
//! specifically (`schema.functions.iter().find(...)`), which — see
//! `overload_resolution_ignores_argument_types` below — turns out to
//! resolve overloads by name and argument *count* only, not by type.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_functions -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{
    FunctionDescriptor, FunctionParamDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
};
use pylon_pgcon::{ExtensionOids, PgPool};
use pylon_value::DecodedValue;

fn ty(name: &str, module: &str, properties: Vec<PropertyDescriptor>) -> TypeDescriptor {
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
        abstract_: false,
        materialized: true,
        description: None,
        parents: vec![],
        interfaces: vec![],
        bases: vec![],
        properties,
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

fn int_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "int8".into();
    p
}

fn param(name: &str, pg_type: &str) -> FunctionParamDescriptor {
    FunctionParamDescriptor {
        name: name.into(),
        pg_type: pg_type.into(),
    }
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
}

async fn rows_with_params(
    pool: &pylon_pgcon::PgPool,
    sd: &SchemaDescriptor,
    pyql: &str,
    params: &[DecodedValue],
) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, params, &ExtensionOids::default())
        .await
        .unwrap()
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn scalar_function_computes_correctly() {
    let module = unique_module("live_fn_scalar");
    let discount = FunctionDescriptor {
        name: "discount_price".into(),
        module: module.clone(),
        params: vec![param("price", "numeric"), param("pct", "numeric")],
        return_pg_type: "numeric".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "price * (1 - pct / 100)".into(),
    };
    let sd = SchemaDescriptor {
        types: vec![],
        functions: vec![discount],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::discount_price(100, 25)")).await;
    assert_eq!(rows.len(), 1);
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Decimal(s) = &shape[0] else {
        panic!("expected Decimal, got {:?}", shape[0])
    };
    assert_eq!(s.parse::<f64>().unwrap(), 75.0, "got {s}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn object_set_returning_function_filters_correctly() {
    let module = unique_module("live_fn_objset");
    let person = ty("Person", &module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    let adults = FunctionDescriptor {
        name: "adults".into(),
        module: module.clone(),
        params: vec![],
        return_pg_type: format!("{module}::Person"),
        return_is_object: true,
        return_is_set: true,
        return_is_polymorphic: false,
        volatility: "stable".into(),
        body: format!("select {module}::Person filter .age >= 18"),
    };
    let sd = SchemaDescriptor {
        types: vec![person],
        functions: vec![adults],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Kid', age := 10 }}"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::adults() {{ name }}")).await;
    assert_eq!(rows.len(), 1, "only the adult should match, got {rows:?}");
    assert_eq!(field(&rows[0], 2), &DecodedValue::Str("Alice".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn function_composes_inside_a_larger_query() {
    let module = unique_module("live_fn_compose");
    let double = FunctionDescriptor {
        name: "double".into(),
        module: module.clone(),
        params: vec![param("n", "int8")],
        return_pg_type: "int8".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "n * 2".into(),
    };
    let person = ty("Person", &module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    let sd = SchemaDescriptor {
        types: vec![person],
        functions: vec![double],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Bob', age := 10 }}"),
    )
    .await;

    // The function's result feeds directly into a filter on a different type.
    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name }} filter {module}::double(.age) > 40"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "only Alice (age 30 -> double 60) should match, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 2), &DecodedValue::Str("Alice".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn overload_resolution_by_argument_count() {
    let module = unique_module("live_fn_overload_count");
    let one_arg = FunctionDescriptor {
        name: "greet".into(),
        module: module.clone(),
        params: vec![param("name", "text")],
        return_pg_type: "text".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "'Hello, ' ++ name".into(),
    };
    let two_arg = FunctionDescriptor {
        name: "greet".into(),
        module: module.clone(),
        params: vec![param("greeting", "text"), param("name", "text")],
        return_pg_type: "text".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "greeting ++ ', ' ++ name".into(),
    };
    let sd = SchemaDescriptor {
        types: vec![],
        functions: vec![one_arg, two_arg],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::greet('Alice')")).await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    assert_eq!(shape[0], DecodedValue::Str("Hello, Alice".to_string()));

    let rows = rows_of(&pool, &sd, &format!("select {module}::greet('Hi', 'Bob')")).await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    assert_eq!(shape[0], DecodedValue::Str("Hi, Bob".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn function_call_argument_gets_cast_to_the_declared_param_type() {
    let module = unique_module("live_fn_arg_cast");
    let discount = FunctionDescriptor {
        name: "discount_price".into(),
        module: module.clone(),
        params: vec![param("price", "numeric"), param("pct", "numeric")],
        return_pg_type: "numeric".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "price * (1 - pct / 100)".into(),
    };
    let sd = SchemaDescriptor {
        types: vec![],
        functions: vec![discount],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // Bound $0/$1 positional parameters — not literals baked into the PyQL
    // text — must still get wrapped in the function's own declared-param
    // cast (`Compiler::compile_expr`'s user-function fallback wraps every
    // arg in `IrExpr::TypeCast` to `fd.params[i].pg_type` regardless of
    // whether the arg expression is a literal or a bound parameter).
    let rows = rows_with_params(
        &pool,
        &sd,
        &format!("select {module}::discount_price($0, $1)"),
        &[
            DecodedValue::Decimal("200".to_string()),
            DecodedValue::Decimal("50".to_string()),
        ],
    )
    .await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Decimal(s) = &shape[0] else {
        panic!("expected Decimal, got {:?}", shape[0])
    };
    assert_eq!(s.parse::<f64>().unwrap(), 100.0, "got {s}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn function_call_mixes_a_bound_parameter_and_a_literal_argument() {
    let module = unique_module("live_fn_mixed_args");
    let discount = FunctionDescriptor {
        name: "discount_price".into(),
        module: module.clone(),
        params: vec![param("price", "numeric"), param("pct", "numeric")],
        return_pg_type: "numeric".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "price * (1 - pct / 100)".into(),
    };
    let sd = SchemaDescriptor {
        types: vec![],
        functions: vec![discount],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // First arg is a bound parameter, second is a literal baked straight
    // into the PyQL text — both must resolve through the same call.
    let rows = rows_with_params(
        &pool,
        &sd,
        &format!("select {module}::discount_price($0, 25)"),
        &[DecodedValue::Decimal("100".to_string())],
    )
    .await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Decimal(s) = &shape[0] else {
        panic!("expected Decimal, got {:?}", shape[0])
    };
    assert_eq!(s.parse::<f64>().unwrap(), 75.0, "got {s}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn function_call_composed_as_an_argument_to_another_function_call() {
    let module = unique_module("live_fn_nested_call");
    let double = FunctionDescriptor {
        name: "double".into(),
        module: module.clone(),
        params: vec![param("n", "int8")],
        return_pg_type: "int8".into(),
        return_is_object: false,
        return_is_set: false,
        return_is_polymorphic: false,
        volatility: "immutable".into(),
        body: "n * 2".into(),
    };
    let sd = SchemaDescriptor {
        types: vec![],
        functions: vec![double],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // double(double(3)) — the inner call's IR result must be a valid
    // argument expression to the outer call, not just a top-level scalar.
    let rows = rows_of(&pool, &sd, &format!("select {module}::double({module}::double(3))")).await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    assert_eq!(shape[0], DecodedValue::I64(12));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn named_only_arguments_default_what_they_leave_out() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    let same = eval_scalar(
        &pool,
        "cal::to_relative_duration(days := 1, hours := 2) = cal::to_relative_duration(hours := 26)",
    )
    .await;
    assert_eq!(same, DecodedValue::Bool(true));
    let empty = SchemaDescriptor::default();
    assert!(
        query::compile("select cal::to_relative_duration(30)", &empty).is_err(),
        "a named-only parameter cannot be passed by position"
    );
    assert!(
        query::compile("select cal::to_relative_duration(weeks := 1)", &empty).is_err(),
        "an argument the function does not declare is refused"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_zone_picks_the_datetime_overload_and_a_format_the_string_one() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    // 23:30 UTC is already the next day in Amsterdam (UTC+1 in January).
    let date = eval_scalar(
        &pool,
        "cal::to_local_date(<datetime>'2026-01-15T23:30:00Z', 'Europe/Amsterdam') = cal::to_local_date(2026, 1, 16)",
    )
    .await;
    assert_eq!(date, DecodedValue::Bool(true));
    let time = eval_scalar(
        &pool,
        "cal::to_local_time(<datetime>'2026-01-15T23:30:00Z', 'Europe/Amsterdam') = cal::to_local_time(0, 30, 0)",
    )
    .await;
    assert_eq!(time, DecodedValue::Bool(true));
    let datetime = eval_scalar(
        &pool,
        "cal::to_local_datetime(<datetime>'2026-01-15T23:30:00Z', 'Europe/Amsterdam') = cal::to_local_datetime(2026, 1, 16, 0, 30, 0)",
    )
    .await;
    assert_eq!(datetime, DecodedValue::Bool(true));

    // Two strings still reach the parsing overload, not the zone one.
    let parsed = eval_scalar(
        &pool,
        "cal::to_local_date('2026-01-16', 'YYYY-MM-DD') = cal::to_local_date(2026, 1, 16)",
    )
    .await;
    assert_eq!(parsed, DecodedValue::Bool(true));

    // And the inverse direction, `std::to_datetime(local, zone)`.
    let back = eval_scalar(
        &pool,
        "to_datetime(cal::to_local_datetime(2026, 1, 16, 0, 30, 0), 'Europe/Amsterdam') = <datetime>'2026-01-15T23:30:00Z'",
    )
    .await;
    assert_eq!(back, DecodedValue::Bool(true));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_string_with_no_format_is_read_as_iso_8601() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    for expr in [
        "cal::to_local_date('2026-01-16') = cal::to_local_date(2026, 1, 16)",
        // The compact ISO spelling, and surrounding whitespace, are accepted.
        "cal::to_local_date('20260116') = cal::to_local_date(2026, 1, 16)",
        "cal::to_local_date('  2026-01-16  ') = cal::to_local_date(2026, 1, 16)",
        "cal::to_local_time('12:34:56') = cal::to_local_time(12, 34, 56)",
        "cal::to_local_time('12:34') = cal::to_local_time(12, 34, 0)",
        "cal::to_local_time('123456') = cal::to_local_time(12, 34, 56)",
        "cal::to_local_datetime('2026-01-16T12:34:56') = cal::to_local_datetime(2026, 1, 16, 12, 34, 56)",
        "cal::to_local_datetime('2026-01-16 12:34:56') = cal::to_local_datetime(2026, 1, 16, 12, 34, 56)",
        // An empty set for `fmt` is the same as leaving it out.
        "cal::to_local_date('2026-01-16', <optional str>{}) = cal::to_local_date(2026, 1, 16)",
    ] {
        assert_eq!(eval_scalar(&pool, expr).await, DecodedValue::Bool(true), "{expr}");
    }

    // Everything PostgreSQL would otherwise accept but ISO 8601 does not.
    for (expr, message) in [
        (
            "cal::to_local_date('01/16/2026')",
            "invalid input syntax for type cal::local_date",
        ),
        (
            "cal::to_local_date('Jan 16, 2026')",
            "invalid input syntax for type cal::local_date",
        ),
        (
            "cal::to_local_date('2026-1-6')",
            "invalid input syntax for type cal::local_date",
        ),
        (
            "cal::to_local_datetime('2026-01-16T12:34:56Z')",
            "invalid input syntax for type cal::local_datetime",
        ),
        (
            "cal::to_local_time('noon')",
            "invalid input syntax for type cal::local_time",
        ),
        // A valid PostgreSQL `time`, but not a valid time of day.
        (
            "cal::to_local_time('24:00:00')",
            "cal::local_time field value out of range",
        ),
        (
            "cal::to_local_date('2026-01-16', '')",
            "\"fmt\" argument must be a non-empty string",
        ),
        (
            "cal::to_local_datetime('2026-01-16 12:34+02', 'YYYY-MM-DD HH24:MITZH')",
            "unexpected time zone in format",
        ),
    ] {
        assert_error_contains(&pool, expr, message).await;
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_date_duration_is_built_from_years_months_and_days() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    let same = eval_scalar(
        &pool,
        "cal::to_date_duration(years := 1, months := 2, days := 3) = <cal::date_duration>'1 year 2 months 3 days'",
    )
    .await;
    assert_eq!(same, DecodedValue::Bool(true));
    let empty = SchemaDescriptor::default();
    assert!(
        query::compile("select cal::to_date_duration(1, 2, 3)", &empty).is_err(),
        "a named-only parameter cannot be passed by position"
    );
    assert!(
        query::compile("select cal::to_date_duration(hours := 1)", &empty).is_err(),
        "a date duration has no units below a day"
    );

    // 30-day chunks become months; hours are left alone rather than rolled
    // up into days first.
    let days = eval_scalar(
        &pool,
        "cal::duration_normalize_days(<cal::relative_duration>'45 days') = <cal::relative_duration>'1 month 15 days'",
    )
    .await;
    assert_eq!(days, DecodedValue::Bool(true));
    let hours_untouched = eval_scalar(
        &pool,
        "cal::duration_normalize_days(<cal::relative_duration>'720 hours') = <cal::relative_duration>'720 hours'",
    )
    .await;
    assert_eq!(hours_untouched, DecodedValue::Bool(true));
    let hours = eval_scalar(
        &pool,
        "cal::duration_normalize_hours(<cal::relative_duration>'720 hours') = <cal::relative_duration>'30 days'",
    )
    .await;
    assert_eq!(hours, DecodedValue::Bool(true));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_element_is_named_as_postgres_names_it() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    for (expr, expected) in [
        ("cal::date_get(<cal::local_date>'2026-01-16', 'isodow')", 5.0),
        ("cal::date_get(<cal::local_date>'2026-01-16', 'millennium')", 3.0),
        ("cal::time_get(<cal::local_time>'12:34:56', 'minutes')", 34.0),
        ("cal::time_get(<cal::local_time>'12:34:56', 'midnightseconds')", 45296.0),
        (
            "datetime_get(<cal::local_datetime>'2026-01-16T12:34:56', 'minutes')",
            34.0,
        ),
        ("datetime_get(<datetime>'2026-01-16T12:34:56Z', 'seconds')", 56.0),
        ("duration_get(<duration>'90 minutes', 'totalseconds')", 5400.0),
    ] {
        assert_eq!(eval_scalar(&pool, expr).await, DecodedValue::F64(expected), "{expr}");
    }

    // The singular spellings, and the ones PostgreSQL knows but the accepted
    // vocabulary does not, are rejected by name.
    for (expr, message) in [
        (
            "cal::time_get(<cal::local_time>'12:34:56', 'minute')",
            "invalid unit for cal::time_get",
        ),
        (
            "cal::date_get(<cal::local_date>'2026-01-16', 'minutes')",
            "invalid unit for cal::date_get",
        ),
        (
            "datetime_get(<datetime>'2026-01-16T12:34:56Z', 'timezone')",
            "invalid unit for std::datetime_get",
        ),
        (
            "datetime_truncate(<datetime>'2026-01-16T12:34:56Z', 'quarter')",
            "invalid unit for std::datetime_truncate",
        ),
        (
            "duration_get(<duration>'90 minutes', 'hours')",
            "invalid unit for std::duration_get",
        ),
    ] {
        assert_error_contains(&pool, expr, message).await;
    }

    // `quarters` is the accepted spelling of PostgreSQL's `quarter`.
    let quarters = eval_scalar(
        &pool,
        "datetime_truncate(<datetime>'2026-01-16T12:34:56Z', 'quarters') = <datetime>'2026-01-01T00:00:00Z'",
    )
    .await;
    assert_eq!(quarters, DecodedValue::Bool(true));
}

/// Runs `select <expr>` and asserts it fails with a message containing
/// `needle` — the accept/reject half of stdlib parity, which `eval_scalar`
/// cannot express because it unwraps.
async fn assert_error_contains(pool: &PgPool, expr: &str, needle: &str) {
    let schema = SchemaDescriptor::default();
    let compiled = query::compile(&format!("select {expr}"), &schema).expect("should compile");
    let error = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .expect_err(&format!("{expr} should have failed"));
    let message = error.to_string();
    assert!(message.contains(needle), "{expr}: {needle:?} not in {message:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn base64_round_trips_without_line_breaks() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    let decoded = eval_scalar(&pool, "to_str(enc::base64_decode('YXxi'))").await;
    assert_eq!(decoded, DecodedValue::Str("a|b".to_string()));
    // Postgres wraps every 76 characters; 60 bytes encode to 80.
    let encoded = eval_scalar(&pool, "enc::base64_encode(to_bytes(str_repeat('x', 60), 'UTF8'))").await;
    let DecodedValue::Str(text) = encoded else {
        panic!("expected Str, got {encoded:?}")
    };
    assert_eq!(text.len(), 80);
    assert!(!text.contains('\n'), "got {text:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_negative_index_counts_from_the_end() {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    let last = eval_scalar(&pool, "str_split('marketplace::MemberPlanLicense', '::')[-1]").await;
    assert_eq!(last, DecodedValue::Str("MemberPlanLicense".to_string()));
    assert_eq!(eval_scalar(&pool, "[1, 2, 3][-3]").await, DecodedValue::I64(1));
    assert_eq!(
        eval_scalar(&pool, "'abc'[-1]").await,
        DecodedValue::Str("c".to_string())
    );

    let empty = SchemaDescriptor::default();
    for (expr, message) in [
        ("[1, 2][-3]", "array index -3 is out of bounds"),
        ("'ab'[-3]", "string index -3 is out of bounds"),
        ("to_bytes('ab', 'UTF8')[-3]", "byte string index -3 is out of bounds"),
    ] {
        let compiled = query::compile(&format!("select {expr}"), &empty).unwrap();
        let error = pool
            .query_typed(&compiled.sql, &[], &ExtensionOids::default())
            .await
            .expect_err("an index before the start is out of bounds")
            .to_string();
        assert!(error.contains(message), "{expr}: expected {message:?}, got: {error}");
    }
}
