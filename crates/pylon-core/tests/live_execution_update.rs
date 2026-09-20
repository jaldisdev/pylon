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
//! `live_execution_delete.rs`). `+=`/`-=` multi-link
//! append/remove semantics are already covered in depth by
//! `live_execution_linkprops.rs`, so this file is scoped to plain
//! scalar/single-link `:=` updates, which had no live coverage of their
//! own anywhere in the suite before this file (only indirectly, as setup
//! steps inside other files' scenarios).
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_update -- --ignored
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

fn schema_with_post(module: &str) -> SchemaDescriptor {
    let person = ty("Person", module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    let mut post = ty("Post", module, vec![id_prop(), text_prop("title")]);
    post.links = vec![link("author", &format!("{module}::Person"))];
    SchemaDescriptor {
        types: vec![person, post],
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

fn as_i64(v: &DecodedValue) -> i64 {
    match v {
        DecodedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
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
async fn update_with_filter_modifies_only_matching_rows() {
    let module = unique_module("live_update_filter");
    let sd = schema_with_post(&module);
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
        &format!("update {module}::Person filter .name = 'Alice' set {{ age := 99 }}"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name, age }} order by .name"),
    )
    .await;
    assert_eq!(as_i64(field(&rows[0], 2)), 99, "Alice should be updated");
    assert_eq!(as_i64(field(&rows[1], 2)), 40, "Bob must be untouched");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn update_self_referential_expression_reads_the_existing_row_value() {
    let module = unique_module("live_update_self_ref");
    let sd = schema_with_post(&module);
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
        &format!("update {module}::Person filter .name = 'Alice' set {{ age := .age + 1 }}"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ age }}")).await;
    assert_eq!(
        as_i64(field(&rows[0], 1)),
        31,
        "age must be read-then-incremented, not overwritten blind"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn update_replaces_a_single_link() {
    let module = unique_module("live_update_link");
    let sd = schema_with_post(&module);
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
        &format!(
            "insert {module}::Post {{ title := 'Hello', author := (select {module}::Person filter .name = 'Alice') }}"
        ),
    )
    .await;

    exec(&pool, &sd, &format!(
        "update {module}::Post filter .title = 'Hello' set {{ author := (select {module}::Person filter .name = 'Bob') }}"
    )).await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Post {{ author: {{ name }} }}")).await;
    assert_eq!(rows.len(), 1);
    // Post's shape: [type-tag, author]; author link's own row: [type-tag, name].
    let author = field(&rows[0], 1);
    assert_eq!(
        as_str(field(author, 1)),
        "Bob",
        "the link should now point at Bob, got {rows:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn update_replaces_a_single_link_with_a_nested_insert() {
    let module = unique_module("live_update_nested_link");
    let sd = schema_with_post(&module);
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
        &format!(
            "insert {module}::Post {{ title := 'Hello', author := (select {module}::Person filter .name = 'Alice') }}"
        ),
    )
    .await;

    // The link value is sourced from a brand-new row, not an existing one —
    // exercises `IrUpdate::nested_ctes`' WITH-CTE hoisting (`Compiler::
    // compile_update`), not just `compile_insert`'s.
    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Post filter .title = 'Hello' \
         set {{ author := (select (insert {module}::Person {{ name := 'Bob', age := 40 }}) {{ id }}) }}"
        ),
    )
    .await;

    let people = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name }} order by .name"),
    )
    .await;
    assert_eq!(
        people.len(),
        2,
        "the nested insert must have actually created a new Person row"
    );

    let rows = rows_of(&pool, &sd, &format!("select {module}::Post {{ author: {{ name }} }}")).await;
    assert_eq!(rows.len(), 1);
    let author = field(&rows[0], 1);
    assert_eq!(
        as_str(field(author, 1)),
        "Bob",
        "the link should now point at the newly-inserted Bob, got {rows:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn update_returns_the_ids_of_the_rows_it_touched() {
    let module = unique_module("live_update_returning");
    let sd = schema_with_post(&module);
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

    let alice_id = {
        let rows = rows_of(&pool, &sd, &format!("select {module}::Person filter .name = 'Alice'")).await;
        as_uuid(field(&rows[0], 1))
    };

    let touched = rows_of(
        &pool,
        &sd,
        &format!("update {module}::Person filter .name = 'Alice' set {{ age := 31 }}"),
    )
    .await;
    assert_eq!(touched.len(), 1);
    assert_eq!(as_uuid(field(&touched[0], 1)), alice_id);

    // A non-matching filter touches (and returns) nothing.
    let none_touched = rows_of(
        &pool,
        &sd,
        &format!("update {module}::Person filter .name = 'Nobody' set {{ age := 0 }}"),
    )
    .await;
    assert!(none_touched.is_empty());

    let ages: HashSet<i64> = rows_of(&pool, &sd, &format!("select {module}::Person {{ age }}"))
        .await
        .iter()
        .map(|r| as_i64(field(r, 1)))
        .collect();
    assert_eq!(ages, HashSet::from([31, 40]));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_upsert_runs_exactly_one_of_its_two_branches() {
    // `(insert …) if not exists x else (update x set …)` — both branches are
    // data-modifying CTEs, which Postgres runs whether or not anything reads
    // them, so each has to carry its own condition. Asserted against the rows:
    // a branch guarded only on the read side leaves identical-looking SQL.
    let module = unique_module("live_upsert");
    let sd = schema_with_post(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let upsert = format!(
        "with existing := (select {module}::Person filter .name = 'Alice' limit 1) \
         select ((insert {module}::Person {{ name := 'Alice', age := 30 }}) \
         if not exists existing else (update existing set {{ age := 31 }}))"
    );

    exec(&pool, &sd, &upsert).await;
    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name, age }}")).await;
    assert_eq!(rows.len(), 1, "the first run should insert exactly one row, got {rows:?}");
    assert_eq!(as_i64(field(&rows[0], 2)), 30, "the update branch must not have run");

    exec(&pool, &sd, &upsert).await;
    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name, age }}")).await;
    assert_eq!(rows.len(), 1, "the second run must not insert again, got {rows:?}");
    assert_eq!(as_i64(field(&rows[0], 2)), 31, "the update branch should have run");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_assert_on_a_pointer_actually_raises() {
    // `p := assert_exists(.posts { … })` compiles the pointer from the
    // argument and checks the rows it aggregates. Exercised live twice over:
    // an assert dropped on the floor still yields a perfectly good query that
    // simply never raises, and the first attempt at this emitted SQL Postgres
    // rejected outright ("PL/pgSQL functions cannot accept type record[]").
    let module = unique_module("live_assert_ptr");
    let mut sd = schema_with_post(&module);
    let person = sd.types.iter_mut().find(|t| t.name == "Person").expect("Person");
    person.multilinks = vec![multilink("posts", &format!("{module}::Post"))];
    // `posts` is what this test links through, so `Post.author` is beside the
    // point and only in the way as a required link.
    let post = sd.types.iter_mut().find(|t| t.name == "Post").expect("Post");
    post.links[0].nullable = true;
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;

    let pyql = format!("select {module}::Person {{ name, p := assert_exists(.posts {{ title }}) }}");
    let compiled = query::compile(&pyql, &sd).unwrap();
    let error = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .expect_err("assert_exists over an empty pointer must raise")
        .to_string();
    assert!(
        error.contains("assert_exists"),
        "the raise should come from the assert, got: {error}"
    );

    exec(
        &pool,
        &sd,
        &format!("update {module}::Person filter .name = 'Alice' set {{ posts := (insert {module}::Post {{ title := 'Hello' }}) }}"),
    )
    .await;
    let rows = rows_of(&pool, &sd, &pyql).await;
    assert_eq!(rows.len(), 1, "with a post present the assert should pass, got {rows:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn assert_distinct_on_a_pointer_catches_duplicate_rows() {
    // The check compares the aggregated rows rendered as text, so this proves
    // the rendering distinguishes rows the way row equality would: two posts
    // by the same author make `.posts.author` a set with a repeat in it.
    let module = unique_module("live_assert_distinct");
    let mut sd = schema_with_post(&module);
    let person = sd.types.iter_mut().find(|t| t.name == "Person").expect("Person");
    person.multilinks = vec![multilink("posts", &format!("{module}::Post"))];
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;
    for title in ["First", "Second"] {
        exec(
            &pool,
            &sd,
            &format!(
                "update {module}::Person filter .name = 'Alice' set {{ posts += (insert {module}::Post \
                 {{ title := '{title}', author := (select detached {module}::Person filter .name = 'Alice' limit 1) }}) }}"
            ),
        )
        .await;
    }

    let pyql = format!("select {module}::Person {{ name, a := assert_distinct(.posts.author {{ name }}) }}");
    let compiled = query::compile(&pyql, &sd).unwrap();
    let error = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .expect_err("two posts by one author make the author set non-distinct")
        .to_string();
    assert!(
        error.contains("assert_distinct"),
        "the raise should come from the assert, got: {error}"
    );
}
