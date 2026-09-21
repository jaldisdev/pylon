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

//! Live-Postgres tests for link properties on a `Through[...]`-backed
//! multi-link — writing
//! them via `@prop := value` on insert/`+=`, re-linking upserts, `union`ed
//! multi-target appends with distinct per-target values, `-=` removal, and
//! reading them back via `@prop` in a shape. Scoped to the write/read
//! mechanics Pylon's own
//! `Through[...]` feature actually supports (already exercised by pure
//! SQL-shape unit tests in `sql/mod.rs` — this is their live-execution
//! counterpart).
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_linkprops -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
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

fn float_prop(name: &str) -> PropertyDescriptor {
    PropertyDescriptor {
        name: name.into(),
        pg_type: "float8".into(),
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

/// `Product`/`Tag`/`ProductTag` — the same shape `pylon-demo`'s own schema
/// uses, and the pure-unit-test fixture (`sql::tests::make_schema_with_through_and_prop`)
/// mirrors. `ProductTag` declares no `source`/`target` link pointers of its
/// own — `multilink_junction_info`'s defaults handle that, matching the real
/// demo schema exactly.
fn schema(module: &str) -> SchemaDescriptor {
    let product = {
        let mut t = ty("Product", module, vec![id_prop(), text_prop("name")]);
        t.multilinks = vec![multilink_through(
            "tags",
            &format!("{module}::Tag"),
            &format!("{module}::ProductTag"),
        )];
        t
    };
    let tag = ty("Tag", module, vec![id_prop(), text_prop("name")]);
    let mut product_tag = ty("ProductTag", module, vec![id_prop(), float_prop("weight")]);
    product_tag.junction = true;
    SchemaDescriptor {
        types: vec![product, tag, product_tag],
        ..Default::default()
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

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn as_f64(v: &DecodedValue) -> f64 {
    match v {
        DecodedValue::F64(n) => *n,
        other => panic!("expected a float, got {other:?}"),
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn insert_with_link_property_round_trips() {
    let module = unique_module("live_lp_insert");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'electronics' }}")).await;
    exec(
        &pool,
        &sd,
        &format!(
            "insert {module}::Product {{ name := 'Headphones', \
             tags := (select {module}::Tag filter .name = 'electronics') {{ @weight := 1.5 }} }}"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ tags: {{ name, @weight }} }}"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Array(tags) = &shape[1] else {
        panic!("expected an Array for tags, got {:?}", shape[1])
    };
    assert_eq!(tags.len(), 1);
    assert_eq!(field(&tags[0], 1), &DecodedValue::Str("electronics".to_string()));
    assert!(
        (as_f64(field(&tags[0], 2)) - 1.5).abs() < f64::EPSILON,
        "expected weight 1.5, got {:?}",
        tags[0]
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn append_link_property_upserts_on_reappend() {
    // Re-`+=`ing the *same* target with a new @weight must update the
    // existing junction row's property in place, not leave the old value
    // stale (a bare ON CONFLICT DO NOTHING would silently keep 1.0).
    let module = unique_module("live_lp_reappend");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'sale' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Product {{ name := 'Widget' }}")).await;

    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Product set {{ \
             tags += (select {module}::Tag filter .name = 'sale') {{ @weight := 1.0 }} }}"
        ),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Product set {{ \
             tags += (select {module}::Tag filter .name = 'sale') {{ @weight := 9.0 }} }}"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ tags: {{ name, @weight }} }}"),
    )
    .await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Array(tags) = &shape[1] else {
        panic!("expected an Array for tags")
    };
    assert_eq!(
        tags.len(),
        1,
        "re-appending the same target must not create a duplicate junction row, got {tags:?}"
    );
    assert!(
        (as_f64(field(&tags[0], 2)) - 9.0).abs() < f64::EPSILON,
        "re-append must update the weight in place, got {:?}",
        tags[0]
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn append_union_lands_distinct_values_on_correct_targets() {
    // The realistic multi-checkbox scenario: several targets appended in one
    // `+=`, each carrying its own distinct property value, via a `union` of
    // individually-shaped target selects.
    let module = unique_module("live_lp_union");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'a' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'b' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Product {{ name := 'Widget' }}")).await;

    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Product set {{ tags += \
             (select {module}::Tag filter .name = 'a') {{ @weight := 1.0 }} \
             union (select {module}::Tag filter .name = 'b') {{ @weight := 2.0 }} }}"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ tags: {{ name, @weight }} order by .name }}"),
    )
    .await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Array(tags) = &shape[1] else {
        panic!("expected an Array for tags")
    };
    assert_eq!(tags.len(), 2);
    assert_eq!(field(&tags[0], 1), &DecodedValue::Str("a".to_string()));
    assert!(
        (as_f64(field(&tags[0], 2)) - 1.0).abs() < f64::EPSILON,
        "tag 'a' should carry weight 1.0, got {:?}",
        tags[0]
    );
    assert_eq!(field(&tags[1], 1), &DecodedValue::Str("b".to_string()));
    assert!(
        (as_f64(field(&tags[1], 2)) - 2.0).abs() < f64::EPSILON,
        "tag 'b' should carry weight 2.0, got {:?}",
        tags[1]
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn remove_link_clears_the_junction_row() {
    let module = unique_module("live_lp_remove");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'temp' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Product {{ name := 'Widget' }}")).await;
    exec(
        &pool, &sd,
        &format!("update {module}::Product set {{ tags += (select {module}::Tag filter .name = 'temp') {{ @weight := 1.0 }} }}"),
    ).await;
    exec(
        &pool,
        &sd,
        &format!("update {module}::Product set {{ tags -= (select {module}::Tag filter .name = 'temp') }}"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Product {{ tags: {{ name }} }}")).await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Array(tags) = &shape[1] else {
        panic!("expected an Array for tags")
    };
    assert!(
        tags.is_empty(),
        "-= should have removed the junction row entirely, got {tags:?}"
    );

    // The Tag itself must still exist — only the junction row was removed.
    let tag_rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Tag {{ name }} filter .name = 'temp'"),
    )
    .await;
    assert_eq!(
        tag_rows.len(),
        1,
        "removing the link must not delete the target Tag row itself"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_link_property_value_reads_the_walked_links_current_value() {
    // `tags += (select .tags { @weight := @weight + 1.0 } filter …)` — the
    // `@weight` on the right is the junction row the walk crosses.
    let module = unique_module("live_lp_read_current");
    let sd = schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'sale' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Tag {{ name := 'new' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Product {{ name := 'Widget' }}")).await;
    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Product set {{ \
             tags += (select {module}::Tag) {{ @weight := 1.0 }} }}"
        ),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!(
            "update {module}::Product set {{ \
             tags += (select .tags {{ @weight := @weight + 1.5 }} filter .name = 'sale') }}"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ tags: {{ name, @weight }} order by .name }}"),
    )
    .await;
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    let DecodedValue::Array(tags) = &shape[1] else {
        panic!("expected an Array for tags")
    };
    let weights: Vec<f64> = tags.iter().map(|t| as_f64(field(t, 2))).collect();
    assert_eq!(weights, vec![1.0, 2.5], "only the walked-and-filtered link moves, got {tags:?}");
}
