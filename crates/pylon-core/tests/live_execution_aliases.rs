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

//! Live-Postgres tests for schema `Alias`es — a named, reusable PyQL
//! expression referenced like any other type name, with the outer query's
//! own filter/shape/order-by/offset/limit merged onto the alias's own
//! select (`Compiler::try_compile_alias_select`). Concepts inspired by the
//! filter/clause/limit-interaction scenarios in the upstream engine's own
//! the upstream expr_aliases suite, not ported literally — that suite is
//! mostly built around the upstream engine's `CREATE ALIAS` DDL object and schema
//! introspection, which Pylon's `Alias` (a plain module-level annotation,
//! no DDL/introspection surface of its own) doesn't have.
//!
//! Aliases got eager-compile validation this session (`validate.rs`) —
//! this is the live-execution counterpart, proving the merge semantics
//! actually produce correct query results against real data, not just that
//! the alias's own body compiles in isolation.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_aliases -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{AliasDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

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

fn bool_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "boolean".into();
    p
}

fn int_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "int8".into();
    p
}

/// `Person { name, age, active }` plus an `ActiveAlias := select Person
/// filter .active = true` and a `Youngest := select Person { name } order
/// by .age asc limit 1`.
fn schema(module: &str) -> SchemaDescriptor {
    let person = ty(
        "Person",
        module,
        vec![
            id_prop(),
            text_prop("name"),
            int_prop("age"),
            bool_prop("active"),
        ],
    );
    let active_alias = AliasDescriptor {
        name: "ActiveAlias".into(),
        module: module.into(),
        expr: format!("select {module}::Person filter .active = true"),
    };
    let youngest_alias = AliasDescriptor {
        name: "Youngest".into(),
        module: module.into(),
        expr: format!("select {module}::Person {{ name }} order by .age asc limit 1"),
    };
    SchemaDescriptor {
        types: vec![person],
        aliases: vec![active_alias, youngest_alias],
        ..Default::default()
    }
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(
    pool: &pylon_pgcon::PgPool,
    sd: &SchemaDescriptor,
    pyql: &str,
) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
}

fn field(row: &CachedValue, i: usize) -> &CachedValue {
    match row {
        CachedValue::Composite(fields) => fields.get(i).unwrap_or(&CachedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib())
        .await
        .unwrap();
}

async fn seed(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, module: &str) {
    exec(
        pool,
        sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30, active := true }}"),
    )
    .await;
    exec(
        pool,
        sd,
        &format!("insert {module}::Person {{ name := 'Bob', age := 25, active := false }}"),
    )
    .await;
    exec(
        pool,
        sd,
        &format!("insert {module}::Person {{ name := 'Carol', age := 40, active := true }}"),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn plain_select_uses_the_aliases_own_filter() {
    let module = unique_module("live_alias_filter");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::ActiveAlias {{ name }} order by .name"),
    )
    .await;
    assert_eq!(
        rows.len(),
        2,
        "only the two active people should match, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("Alice".to_string()));
    assert_eq!(field(&rows[1], 1), &CachedValue::Str("Carol".to_string()));
}

#[tokio::test]
#[ignore]
async fn outer_filter_ands_with_the_aliases_own_filter() {
    let module = unique_module("live_alias_and");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    // Bob is inactive, so even though the outer filter matches his name,
    // the alias's own `active = true` must still exclude him.
    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::ActiveAlias {{ name }} filter .name = 'Bob'"),
    )
    .await;
    assert!(
        rows.is_empty(),
        "outer filter must AND with the alias's own filter, not replace it, got {rows:?}"
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::ActiveAlias {{ name }} filter .name = 'Alice'"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("Alice".to_string()));
}

#[tokio::test]
#[ignore]
async fn outer_order_by_overrides_the_aliases_own_order_by() {
    let module = unique_module("live_alias_order");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    // ActiveAlias declares no order of its own; give one explicitly and
    // confirm it's honored (descending by name: Carol, Alice).
    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::ActiveAlias {{ name }} order by .name desc"),
    )
    .await;
    assert_eq!(rows.len(), 2);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("Carol".to_string()));
    assert_eq!(field(&rows[1], 1), &CachedValue::Str("Alice".to_string()));
}

#[tokio::test]
#[ignore]
async fn aliases_own_order_and_limit_apply_with_no_outer_override() {
    let module = unique_module("live_alias_limit");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    // Youngest already has its own `order by .age asc limit 1` — plain
    // `select Youngest { name }` with no further modifiers must honor them.
    let rows = rows_of(&pool, &sd, &format!("select {module}::Youngest {{ name }}")).await;
    assert_eq!(
        rows.len(),
        1,
        "the alias's own limit must apply, got {rows:?}"
    );
    assert_eq!(
        field(&rows[0], 1),
        &CachedValue::Str("Bob".to_string()),
        "Bob is the youngest (25)"
    );
}

#[tokio::test]
#[ignore]
async fn outer_limit_overrides_the_aliases_own_limit() {
    let module = unique_module("live_alias_outer_limit");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Youngest {{ name }} limit 2"),
    )
    .await;
    assert_eq!(
        rows.len(),
        2,
        "an explicit outer limit must override the alias's own limit 1, got {rows:?}"
    );
}

#[tokio::test]
#[ignore]
async fn outer_shape_replaces_the_aliases_own_shape() {
    let module = unique_module("live_alias_shape");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();
    seed(&pool, &sd, &module).await;

    // Youngest's own shape only projects `name` — an outer shape asking for
    // `age` too must still work (the outer shape replaces, not merges with,
    // the alias's own).
    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Youngest {{ name, age }}"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("Bob".to_string()));
    assert_eq!(field(&rows[0], 2), &CachedValue::I64(25));
}
