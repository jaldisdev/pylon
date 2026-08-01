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

//! Live-Postgres tests for the bare `UPDATE` statement: a filtered scalar
//! property update, a self-referential update expression that reads the
//! row's *existing* value (`age := .age + 1`), replacing a single link
//! (both from an existing row and from a nested `insert`, the latter
//! exercising `IrUpdate::nested_ctes`' WITH-CTE hoisting), and the
//! `RETURNING id` shape every update implicitly gets (`Compiler::
//! compile_update` always returns `pk_returning`, same as `DELETE` — see
//! `live_execution_delete.rs`). Concepts inspired by the upstream engine's own
//! the upstream update suite, not ported literally — `+=`/`-=` multi-link
//! append/remove semantics are already covered in depth by
//! `live_execution_linkprops.rs`, so this file is scoped to plain
//! scalar/single-link `:=` updates, which had no live coverage of their
//! own anywhere in the suite before this file (only indirectly, as setup
//! steps inside other files' scenarios).
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_update -- --ignored
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

fn schema_with_post(module: &str) -> SchemaDescriptor {
    let person = ty("Person", module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    let mut post = ty("Post", module, vec![id_prop(), text_prop("title")]);
    post.links = vec![link("author", &format!("{module}::Person"))];
    SchemaDescriptor { types: vec![person, post], ..Default::default() }
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

fn as_i64(v: &CachedValue) -> i64 {
    match v {
        CachedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
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
async fn update_with_filter_modifies_only_matching_rows() {
    let module = unique_module("live_update_filter");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}")).await;

    exec(&pool, &sd, &format!("update {module}::Person filter .name = 'Alice' set {{ age := 99 }}")).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name, age }} order by .name")).await;
    assert_eq!(as_i64(field(&rows[0], 2)), 99, "Alice should be updated");
    assert_eq!(as_i64(field(&rows[1], 2)), 40, "Bob must be untouched");
}

#[tokio::test]
#[ignore]
async fn update_self_referential_expression_reads_the_existing_row_value() {
    let module = unique_module("live_update_self_ref");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;

    exec(&pool, &sd, &format!("update {module}::Person filter .name = 'Alice' set {{ age := .age + 1 }}")).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ age }}")).await;
    assert_eq!(as_i64(field(&rows[0], 1)), 31, "age must be read-then-incremented, not overwritten blind");
}

#[tokio::test]
#[ignore]
async fn update_replaces_a_single_link() {
    let module = unique_module("live_update_link");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}")).await;
    exec(&pool, &sd, &format!(
        "insert {module}::Post {{ title := 'Hello', author := (select {module}::Person filter .name = 'Alice') }}"
    )).await;

    exec(&pool, &sd, &format!(
        "update {module}::Post filter .title = 'Hello' set {{ author := (select {module}::Person filter .name = 'Bob') }}"
    )).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Post {{ author: {{ name }} }}")).await;
    assert_eq!(rows.len(), 1);
    // Post's shape: [type-tag, author]; author link's own row: [type-tag, name].
    let author = field(&rows[0], 1);
    assert_eq!(as_str(field(author, 1)), "Bob", "the link should now point at Bob, got {rows:?}");
}

#[tokio::test]
#[ignore]
async fn update_replaces_a_single_link_with_a_nested_insert() {
    let module = unique_module("live_update_nested_link");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!(
        "insert {module}::Post {{ title := 'Hello', author := (select {module}::Person filter .name = 'Alice') }}"
    )).await;

    // The link value is sourced from a brand-new row, not an existing one —
    // exercises `IrUpdate::nested_ctes`' WITH-CTE hoisting (`Compiler::
    // compile_update`), not just `compile_insert`'s.
    exec(&pool, &sd, &format!(
        "update {module}::Post filter .title = 'Hello' \
         set {{ author := (select (insert {module}::Person {{ name := 'Bob', age := 40 }}) {{ id }}) }}"
    )).await;

    let people = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }} order by .name")).await;
    assert_eq!(people.len(), 2, "the nested insert must have actually created a new Person row");

    let rows = rows_of(&pool, &sd, &format!("select {module}::Post {{ author: {{ name }} }}")).await;
    assert_eq!(rows.len(), 1);
    let author = field(&rows[0], 1);
    assert_eq!(as_str(field(author, 1)), "Bob", "the link should now point at the newly-inserted Bob, got {rows:?}");
}

#[tokio::test]
#[ignore]
async fn update_returns_the_ids_of_the_rows_it_touched() {
    let module = unique_module("live_update_returning");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob', age := 40 }}")).await;

    let alice_id = {
        let rows = rows_of(&pool, &sd, &format!("select {module}::Person filter .name = 'Alice'")).await;
        as_uuid(field(&rows[0], 1))
    };

    let touched = rows_of(&pool, &sd, &format!("update {module}::Person filter .name = 'Alice' set {{ age := 31 }}")).await;
    assert_eq!(touched.len(), 1);
    assert_eq!(as_uuid(field(&touched[0], 1)), alice_id);

    // A non-matching filter touches (and returns) nothing.
    let none_touched = rows_of(&pool, &sd, &format!("update {module}::Person filter .name = 'Nobody' set {{ age := 0 }}")).await;
    assert!(none_touched.is_empty());

    let ages: HashSet<i64> = rows_of(&pool, &sd, &format!("select {module}::Person {{ age }}")).await
        .iter().map(|r| as_i64(field(r, 1))).collect();
    assert_eq!(ages, HashSet::from([31, 40]));
}
