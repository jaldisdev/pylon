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
//! every row. Concepts inspired by Gel's own `test_edgeql_delete.py`, not
//! ported literally — Gel's suite spends most of its weight on `ON TARGET
//! DELETE`/`DELETE SOURCE` cascade policies and access-policy interaction,
//! which are already covered by `live_execution_on_delete.rs` and don't
//! exist in Pylon respectively; this file is scoped to the delete
//! statement's own filter/returning behavior, which had no live coverage
//! anywhere in the suite before this file.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_delete -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;
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
    let person = ty("Person", module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    SchemaDescriptor { types: vec![person], ..Default::default() }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap()
}

fn field(row: &CachedValue, i: usize) -> &CachedValue {
    match row {
        CachedValue::Composite(fields) => fields.get(i).unwrap_or(&CachedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn as_str(v: &CachedValue) -> &str {
    match v {
        CachedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

fn as_uuid(v: &CachedValue) -> [u8; 16] {
    match v {
        CachedValue::Uuid(u) => *u,
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn delete_with_filter_removes_only_matching_rows() {
    let module = unique_module("live_delete_filter");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Kid', age := 10 }}")).await;

    exec(&pool, &sd, &format!("delete {module}::Person filter .age < 18")).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert_eq!(rows.len(), 1, "only the matching row should have been deleted, got {rows:?}");
    assert_eq!(as_str(field(&rows[0], 1)), "Alice");
}

#[tokio::test]
#[ignore]
async fn delete_returns_the_ids_of_the_rows_it_removed() {
    let module = unique_module("live_delete_returning");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}")).await;

    let before = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    let before_ids: HashSet<[u8; 16]> = before.iter().map(|r| as_uuid(field(r, 1))).collect();

    let deleted = rows_of(&pool, &sd, &format!("delete {module}::Person")).await;
    assert_eq!(deleted.len(), 2, "expected both rows returned from delete, got {deleted:?}");
    let deleted_ids: HashSet<[u8; 16]> = deleted.iter().map(|r| as_uuid(field(r, 1))).collect();
    assert_eq!(deleted_ids, before_ids, "delete's RETURNING ids must match the rows that actually existed");

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert!(after.is_empty(), "both rows should be gone, got {after:?}");
}

#[tokio::test]
#[ignore]
async fn delete_with_no_matches_is_a_no_op() {
    let module = unique_module("live_delete_no_match");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;

    let deleted = rows_of(&pool, &sd, &format!("delete {module}::Person filter .age > 100")).await;
    assert!(deleted.is_empty(), "no row should match, got {deleted:?}");

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert_eq!(after.len(), 1, "the non-matching row must survive, got {after:?}");
}

#[tokio::test]
#[ignore]
async fn delete_with_no_filter_removes_every_row() {
    let module = unique_module("live_delete_all");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Carol', age := 50 }}")).await;

    exec(&pool, &sd, &format!("delete {module}::Person")).await;

    let after = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert!(after.is_empty(), "an unfiltered delete must remove every row, got {after:?}");
}
