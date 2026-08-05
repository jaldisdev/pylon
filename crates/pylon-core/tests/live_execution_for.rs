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

//! Live-Postgres tests for `FOR x IN {...} UNION (...)` — bulk insert (the
//! `emit_for_insert` VALUES-CTE path), a `SELECT` body (the
//! `CROSS JOIN LATERAL` path), the for-variable composing inside a body
//! expression (not just a bare assignment), and the empty-iterator no-op
//! case. Concepts inspired by Gel's own `test_edgeql_for.py`, not ported
//! literally — Pylon's `ForStmt` iterator only supports a scalar `{...}`
//! set literal (`Compiler::compile_for`'s `IrForIterator::Values`), not
//! Gel's arbitrary set-returning-expression iterators (e.g. `for x in
//! Person union (...)`), and `emit_for_stmt` only implements `Insert` and
//! `Select`/`PathSelect` bodies — an `UPDATE`/`DELETE` for-loop body hits an
//! unimplemented `panic!`, so this file doesn't exercise those.
//!
//! No prior live test in this suite ever executed a `FOR` query against
//! real data, and there were no pure unit tests for `FOR`'s SQL emission at
//! all before this session — this is the first coverage of either kind.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_for -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;
use std::collections::HashSet;

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
        properties,
        links: vec![],
        multilinks: vec![],
        computed: vec![],
        constraints: vec![],
        indexes: vec![],
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

fn person_schema(module: &str) -> SchemaDescriptor {
    let person = ty(
        "Person",
        module,
        vec![id_prop(), text_prop("name"), int_prop("age")],
    );
    SchemaDescriptor {
        types: vec![person],
        ..Default::default()
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib())
        .await
        .unwrap();
    pool.batch_execute(&export_schema(sd).unwrap())
        .await
        .unwrap();
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(
    pool: &pylon_pgcon::PgPool,
    sd: &SchemaDescriptor,
    pyql: &str,
) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn as_str(v: &DecodedValue) -> &str {
    match v {
        DecodedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

fn as_i64(v: &DecodedValue) -> i64 {
    match v {
        DecodedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn for_loop_bulk_inserts_one_row_per_iterator_value() {
    let module = unique_module("live_for_bulk_insert");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool, &sd,
        &format!("for n in {{'Alice', 'Bob', 'Carol'}} union (insert {module}::Person {{ name := n, age := 0 }})"),
    ).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name }} order by .name"),
    )
    .await;
    assert_eq!(
        rows.len(),
        3,
        "expected one row per iterator value, got {rows:?}"
    );
    let names: Vec<&str> = rows.iter().map(|r| as_str(field(r, 1))).collect();
    assert_eq!(names, vec!["Alice", "Bob", "Carol"]);
}

#[tokio::test]
#[ignore]
async fn for_loop_variable_composes_inside_insert_body_expression() {
    let module = unique_module("live_for_compose");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // The for-variable isn't just bare-assigned — it's used inside an
    // arithmetic expression (`n * 10`), proving `IrExpr::ForVar` resolves
    // correctly deep inside a compiled body expression, not only at the
    // top level of a shape assignment.
    exec(
        &pool,
        &sd,
        &format!(
            "for n in {{1, 2, 3}} union (insert {module}::Person {{ name := 'p', age := n * 10 }})"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ age }} order by .age"),
    )
    .await;
    let ages: Vec<i64> = rows.iter().map(|r| as_i64(field(r, 1))).collect();
    assert_eq!(ages, vec![10, 20, 30]);
}

#[tokio::test]
#[ignore]
async fn for_loop_select_body_cross_joins_lateral_per_iterator_value() {
    let module = unique_module("live_for_select");
    let sd = person_schema(&module);
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
        &format!("insert {module}::Person {{ name := 'Bob', age := 65 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Carol', age := 40 }}"),
    )
    .await;

    // A SELECT body run once per iterator value, unioning the matches —
    // the `CROSS JOIN LATERAL` path in `emit_for_stmt`, distinct from the
    // bulk-insert path exercised above.
    let rows = rows_of(
        &pool,
        &sd,
        &format!(
            "for age in {{30, 65}} union (select {module}::Person {{ name }} filter .age = age)"
        ),
    )
    .await;
    let names: HashSet<&str> = rows.iter().map(|r| as_str(field(r, 1))).collect();
    assert_eq!(names, HashSet::from(["Alice", "Bob"]), "got {rows:?}");
}

#[tokio::test]
#[ignore]
async fn for_loop_with_empty_iterator_set_is_a_no_op() {
    let module = unique_module("live_for_empty");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("for n in {{}} union (insert {module}::Person {{ name := n, age := 0 }})"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert_eq!(
        rows.len(),
        0,
        "an empty iterator set must insert nothing, got {rows:?}"
    );
}
