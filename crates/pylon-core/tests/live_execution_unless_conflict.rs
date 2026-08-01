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

//! Live-Postgres tests for `insert ... unless conflict [on <expr>] [else
//! (update ... set {...})]` — Pylon's upsert syntax, compiled to Postgres's
//! `INSERT ... ON CONFLICT`. The pure SQL-shape unit tests in `sql/mod.rs`
//! (`test_unless_conflict_do_nothing`/`_on_do_nothing`/`_do_update`/
//! `_do_update_no_on`) already prove the *compiled SQL text* is shaped
//! correctly; this is their live-execution counterpart — proving it
//! actually behaves correctly against a real constraint violation: a
//! silent no-op leaves the original row untouched, an `else (update ...)`
//! genuinely upserts in place rather than erroring or creating a duplicate,
//! and a bare column reference inside that `else` update resolves against
//! the *existing* conflicting row's own value, not the attempted insert's.
//! Scenarios inspired by the upsert-shaped cases in the upstream engine's own
//! the upstream insert suite, adapted to Pylon's syntax (`unless conflict`,
//! not the upstream engine's `unless conflict on ... else`, which differs in argument
//! shape) and schema (a plain `Exclusive` property, not the upstream engine's constraint
//! model).
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_unless_conflict -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
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

/// `Product { sku: text (Exclusive), name: text, stock: int8 }` — `sku`'s
/// real unique constraint is what `ON CONFLICT`/`ON CONFLICT ("sku")` needs
/// to actually infer against.
fn product_schema(module: &str) -> SchemaDescriptor {
    let mut sku = text_prop("sku");
    sku.is_exclusive = true;
    let mut stock = text_prop("stock");
    stock.pg_type = "int8".into();
    let product = ty(
        "Product",
        module,
        vec![id_prop(), sku, text_prop("name"), stock],
    );
    SchemaDescriptor {
        types: vec![product],
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

#[tokio::test]
#[ignore]
async fn bare_unless_conflict_silently_keeps_the_original_row() {
    let module = unique_module("live_uc_bare");
    let sd = product_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Product {{ sku := 'ABC', name := 'Original', stock := 0 }}"),
    )
    .await;
    exec(
        &pool, &sd,
        &format!("insert {module}::Product {{ sku := 'ABC', name := 'Attempted Duplicate', stock := 99 }} unless conflict"),
    ).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ name }} filter .sku = 'ABC'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "bare unless conflict must not create a duplicate row, got {rows:?}"
    );
    assert_eq!(
        field(&rows[0], 1),
        &CachedValue::Str("Original".to_string()),
        "the original row must be left untouched"
    );
}

#[tokio::test]
#[ignore]
async fn unless_conflict_on_specific_property_no_ops() {
    let module = unique_module("live_uc_on");
    let sd = product_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Product {{ sku := 'XYZ', name := 'Original', stock := 0 }}"),
    )
    .await;
    exec(
        &pool, &sd,
        &format!("insert {module}::Product {{ sku := 'XYZ', name := 'Duplicate', stock := 5 }} unless conflict on .sku"),
    ).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ name }} filter .sku = 'XYZ'"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        field(&rows[0], 1),
        &CachedValue::Str("Original".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn unless_conflict_else_update_upserts_in_place() {
    let module = unique_module("live_uc_upsert");
    let sd = product_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Product {{ sku := 'DEF', name := 'Original', stock := 0 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!(
            "insert {module}::Product {{ sku := 'DEF', name := 'New Name', stock := 0 }} \
             unless conflict on .sku else (update {module}::Product set {{ name := 'New Name' }})"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ name }} filter .sku = 'DEF'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "an upsert must update the existing row in place, not create a second one, got {rows:?}"
    );
    assert_eq!(
        field(&rows[0], 1),
        &CachedValue::Str("New Name".to_string()),
        "the ELSE update must have taken effect"
    );
}

#[tokio::test]
#[ignore]
async fn unless_conflict_else_update_reads_the_existing_conflicting_rows_value() {
    // `.stock` inside the ELSE update's own assignment compiles down to a
    // *bare* column reference (`compile_conflict_else` uses an empty alias,
    // so the emitted SQL is `"stock"`, not `"t0"."stock"`) — which in
    // Postgres's `ON CONFLICT DO UPDATE SET` context resolves against the
    // *existing* conflicting row's stored value, not the value the
    // attempted (losing) insert tried to write.
    let module = unique_module("live_uc_existing");
    let sd = product_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Product {{ sku := 'GHI', name := 'Widget', stock := 10 }}"),
    )
    .await;
    // Every "restock" attempt tries to insert with stock=1, but the real
    // effect should be incrementing the *existing* row's stock, not
    // resetting it to (or offsetting from) the attempted insert's own 1.
    for _ in 0..3 {
        exec(
            &pool, &sd,
            &format!(
                "insert {module}::Product {{ sku := 'GHI', name := 'Widget', stock := 1 }} \
                 unless conflict on .sku else (update {module}::Product set {{ stock := .stock + 1 }})"
            ),
        ).await;
    }

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ stock }} filter .sku = 'GHI'"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        field(&rows[0], 1),
        &CachedValue::I64(13),
        "stock should have incremented from the existing row's own value each time (10 -> 11 -> 12 -> 13), got {:?}",
        rows[0]
    );
}

#[tokio::test]
#[ignore]
async fn no_conflict_inserts_a_genuinely_new_row() {
    // The non-conflicting path must still behave like a plain insert.
    let module = unique_module("live_uc_new");
    let sd = product_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap())
        .await
        .unwrap();

    exec(&pool, &sd, &format!("insert {module}::Product {{ sku := 'A', name := 'Alpha', stock := 0 }} unless conflict on .sku")).await;
    exec(&pool, &sd, &format!("insert {module}::Product {{ sku := 'B', name := 'Beta', stock := 0 }} unless conflict on .sku")).await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Product {{ sku }} order by .sku"),
    )
    .await;
    assert_eq!(
        rows.len(),
        2,
        "two distinct skus must both be inserted, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("A".to_string()));
    assert_eq!(field(&rows[1], 1), &CachedValue::Str("B".to_string()));
}
