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

//! Live-Postgres tests for the bare `DELETE` statement itself — filtered
//! delete, the `RETURNING id` shape every delete implicitly gets
//! (`Compiler::compile_delete` always returns `pk_returning`, never a
//! user-supplied shape — Pylon's `DeleteStmt` has no shape field at all),
//! a zero-match filter being a no-op, and an unfiltered delete removing
//! every row. Cascade policies (`ON TARGET DELETE`/`DELETE SOURCE`) are
//! already covered by `live_execution_on_delete.rs`, so this file is
//! scoped to the delete
//! statement's own filter/returning behavior, which had no live coverage
//! anywhere in the suite before this file.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_delete -- --ignored
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

fn person_schema(module: &str) -> SchemaDescriptor {
    let person = ty("Person", module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    SchemaDescriptor {
        types: vec![person],
        ..Default::default()
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
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

fn as_uuid(v: &DecodedValue) -> [u8; 16] {
    match v {
        DecodedValue::Uuid(u) => *u,
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn delete_with_filter_removes_only_matching_rows() {
    let module = unique_module("live_delete_filter");
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
        &format!("insert {module}::Person {{ name := 'Kid', age := 10 }}"),
    )
    .await;

    exec(&pool, &sd, &format!("delete {module}::Person filter .age < 18")).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert_eq!(
        rows.len(),
        1,
        "only the matching row should have been deleted, got {rows:?}"
    );
    assert_eq!(as_str(field(&rows[0], 2)), "Alice");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn delete_returns_the_ids_of_the_rows_it_removed() {
    let module = unique_module("live_delete_returning");
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
        &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}"),
    )
    .await;

    let before = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    let before_ids: HashSet<[u8; 16]> = before.iter().map(|r| as_uuid(field(r, 1))).collect();

    let deleted = rows_of(&pool, &sd, &format!("delete {module}::Person")).await;
    assert_eq!(
        deleted.len(),
        2,
        "expected both rows returned from delete, got {deleted:?}"
    );
    let deleted_ids: HashSet<[u8; 16]> = deleted.iter().map(|r| as_uuid(field(r, 1))).collect();
    assert_eq!(
        deleted_ids, before_ids,
        "delete's RETURNING ids must match the rows that actually existed"
    );

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert!(after.is_empty(), "both rows should be gone, got {after:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn delete_with_no_matches_is_a_no_op() {
    let module = unique_module("live_delete_no_match");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;

    let deleted = rows_of(&pool, &sd, &format!("delete {module}::Person filter .age > 100")).await;
    assert!(deleted.is_empty(), "no row should match, got {deleted:?}");

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert_eq!(after.len(), 1, "the non-matching row must survive, got {after:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn delete_with_no_filter_removes_every_row() {
    let module = unique_module("live_delete_all");
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
        &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Carol', age := 50 }}"),
    )
    .await;

    exec(&pool, &sd, &format!("delete {module}::Person")).await;

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert!(
        after.is_empty(),
        "an unfiltered delete must remove every row, got {after:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_delete_guarded_by_a_false_condition_removes_nothing() {
    // `(delete …) if cond else {}` is a data-modifying CTE, which Postgres
    // runs whether or not anything reads it — so the condition has to narrow
    // the delete itself. Asserted against the rows, not the SQL: a guard that
    // only filters what is read back looks identical in the emitted text.
    let module = unique_module("live_delete_guard");
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
        &format!("select (delete {module}::Person filter .age > 18) if false else {{}}"),
    )
    .await;
    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert_eq!(rows.len(), 1, "a false guard must delete nothing, got {rows:?}");

    exec(
        &pool,
        &sd,
        &format!("select (delete {module}::Person filter .age > 18) if true else {{}}"),
    )
    .await;
    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert!(rows.is_empty(), "a true guard must still delete, got {rows:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn deleting_a_binding_removes_only_the_rows_it_names() {
    // `delete previous` where `previous` is a `with` binding. The binding
    // decides the table, so resolving it to its type and stopping there would
    // empty the table instead — which is why this is asserted against the
    // surviving rows rather than the emitted SQL.
    let module = unique_module("live_delete_binding");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for (name, age) in [("Alice", 30), ("Bob", 40), ("Kid", 10)] {
        exec(
            &pool,
            &sd,
            &format!("insert {module}::Person {{ name := '{name}', age := {age} }}"),
        )
        .await;
    }

    exec(
        &pool,
        &sd,
        &format!("with doomed := (select {module}::Person filter .age < 18) delete doomed"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert_eq!(rows.len(), 2, "only the rows the binding names should go, got {rows:?}");
}
