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

//! Live-Postgres tests for schema `Trigger`s — Pylon's analog of the upstream engine's
//! `create trigger ... do (...)`. Scenarios adapted from the upstream engine's own trigger
//! test suite (noted per test), narrowed to Pylon's feature surface: no
//! access policies, no set-scoped ("for all") triggers, always per-row.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_triggers -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::diff::{DbState, diff_schema_steps};
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;
use std::collections::HashMap;

fn ty(
    name: &str,
    module: &str,
    properties: Vec<pylon_core::schema::PropertyDescriptor>,
) -> TypeDescriptor {
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

/// A `Log { id, old_name, new_name }` audit type — both columns nullable,
/// since a given trigger normally only ever populates one of them.
fn log_type(module: &str) -> TypeDescriptor {
    let mut old_name = text_prop("old_name");
    old_name.nullable = true;
    let mut new_name = text_prop("new_name");
    new_name.nullable = true;
    ty("Log", module, vec![id_prop(), old_name, new_name])
}

async fn exec(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(
    pool: &pylon_pgcon::PgPool,
    schema: &SchemaDescriptor,
    pyql: &str,
) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, schema).unwrap();
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

/// Bootstraps `_pylon` (schema, `notify_cache_invalidate()`, tracking
/// tables, ...) — every concrete table gets an unconditional
/// `pylon_cache_invalidate` trigger, which references that function.
async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn insert_writes_to_another_type() {
    // Upstream case:triggers_insert_01
    let module = unique_module("live_trig_insert");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(
        1, // On.Insert
        "After",
        &format!("insert {module}::Log {{ new_name := __new__.name }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'gadget' }}"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ new_name }} filter .new_name = 'gadget'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "trigger should have written exactly one Log row, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &DecodedValue::Str("gadget".to_string()));
}

#[tokio::test]
#[ignore]
async fn delete_reads_old_row() {
    // Upstream case:triggers_delete_01
    let module = unique_module("live_trig_delete");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(
        4, // On.Delete
        "After",
        &format!("insert {module}::Log {{ old_name := __old__.name }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'doomed' }}"),
    )
    .await;
    exec(
        &pool,
        &schema,
        &format!("delete {module}::Widget filter .name = 'doomed'"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ old_name }} filter .old_name = 'doomed'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "trigger should have captured the pre-delete name, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &DecodedValue::Str("doomed".to_string()));
}

#[tokio::test]
#[ignore]
async fn update_reads_both_old_and_new() {
    // Upstream case:triggers_update_01
    let module = unique_module("live_trig_update");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(
        2, // On.Update
        "After",
        &format!("insert {module}::Log {{ old_name := __old__.name, new_name := __new__.name }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'before' }}"),
    )
    .await;
    exec(
        &pool,
        &schema,
        &format!("update {module}::Widget filter .name = 'before' set {{ name := 'after' }}"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ old_name, new_name }} filter .new_name = 'after'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "trigger should have captured both old and new names, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &DecodedValue::Str("before".to_string()));
    assert_eq!(field(&rows[0], 2), &DecodedValue::Str("after".to_string()));
}

#[tokio::test]
#[ignore]
async fn multiple_independent_triggers_all_fire() {
    // Upstream case:triggers_mixed_01/02 — several triggers on one type,
    // including one combined-event trigger (Insert|Update|Delete) that
    // legally references *neither* anchor (Insert and Delete are both in
    // its mask — see `compile_trigger_handler`'s doc comment) and instead
    // just records a constant marker every time it fires.
    let module = unique_module("live_trig_multi");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![
        trigger(
            1,
            "After",
            &format!("insert {module}::Log {{ new_name := __new__.name }}"),
        ), // Insert-only
        trigger(
            6,
            "After",
            &format!("insert {module}::Log {{ old_name := __old__.name }}"),
        ), // Update|Delete
        trigger(
            7,
            "After",
            &format!("insert {module}::Log {{ new_name := 'touched' }}"),
        ), // Insert|Update|Delete
    ];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'w1' }}"),
    )
    .await; // fires trigger 1 + 3
    exec(
        &pool,
        &schema,
        &format!("update {module}::Widget filter .name = 'w1' set {{ name := 'w2' }}"),
    )
    .await; // fires trigger 2 + 3
    exec(
        &pool,
        &schema,
        &format!("delete {module}::Widget filter .name = 'w2'"),
    )
    .await; // fires trigger 2 + 3

    let touched = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ new_name }} filter .new_name = 'touched'"),
    )
    .await;
    assert_eq!(
        touched.len(),
        3,
        "combined-event trigger should fire on every one of the 3 operations, got {touched:?}"
    );

    let inserts = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ new_name }} filter .new_name = 'w1'"),
    )
    .await;
    assert_eq!(
        inserts.len(),
        1,
        "insert-only trigger should fire exactly once, got {inserts:?}"
    );

    let old_names = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ old_name }} filter .old_name = 'w1'"),
    )
    .await;
    assert_eq!(
        old_names.len(),
        1,
        "update should have captured old_name='w1' once (from the update), got {old_names:?}"
    );
    let old_names_w2 = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ old_name }} filter .old_name = 'w2'"),
    )
    .await;
    assert_eq!(
        old_names_w2.len(),
        1,
        "delete should have captured old_name='w2' once, got {old_names_w2:?}"
    );
}

#[tokio::test]
#[ignore]
async fn trigger_updates_a_linked_row_of_another_type() {
    // Upstream case:triggers_double_01 — not just an audit-log insert,
    // a real cross-type mutation of a *linked* row.
    let module = unique_module("live_trig_double");
    let mut purchase = ty("Purchase", &module, vec![id_prop(), text_prop("total")]);
    purchase.properties[1] = {
        let mut p = purchase.properties[1].clone();
        p.pg_type = "int8".into();
        p.nullable = false;
        p
    };
    let mut item = ty("LineItem", &module, vec![id_prop()]);
    let mut value_prop = text_prop("value");
    value_prop.pg_type = "int8".into();
    item.properties.push(value_prop);
    item.links = vec![link("parent", &format!("{module}::Purchase"))];
    item.triggers = vec![trigger(
        1, // On.Insert
        "After",
        &format!(
            "update {module}::Purchase filter .id = __new__.parent set {{ total := __new__.value }}"
        ),
    )];
    let schema = SchemaDescriptor {
        types: vec![purchase, item],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Purchase {{ total := 0 }}"),
    )
    .await;
    exec(
        &pool, &schema,
        &format!("insert {module}::LineItem {{ parent := (select {module}::Purchase limit 1), value := 99 }}"),
    ).await;

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Purchase {{ total }}"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        field(&rows[0], 1),
        &DecodedValue::I64(99),
        "trigger should have updated the linked Purchase's total"
    );
}

#[tokio::test]
#[ignore]
async fn trigger_chaining_across_types() {
    // Upstream case:triggers_chain_01 — an insert trigger on A inserts
    // into B, and B's own insert trigger also fires (real Postgres row-
    // trigger cascading, no special Pylon support needed — just correct DDL).
    let module = unique_module("live_trig_chain");
    let mut a = ty("TypeA", &module, vec![id_prop(), text_prop("name")]);
    a.triggers = vec![trigger(
        1,
        "After",
        &format!("insert {module}::TypeB {{ tag := __new__.name }}"),
    )];
    let b = ty("TypeB", &module, vec![id_prop(), text_prop("tag")]);
    let mut b = b;
    b.triggers = vec![trigger(
        1,
        "After",
        &format!("insert {module}::Log {{ new_name := __new__.tag }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![a, b, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::TypeA {{ name := 'chain-test' }}"),
    )
    .await;

    let b_rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::TypeB {{ tag }} filter .tag = 'chain-test'"),
    )
    .await;
    assert_eq!(
        b_rows.len(),
        1,
        "A's trigger should have inserted a TypeB row, got {b_rows:?}"
    );

    let log_rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ new_name }} filter .new_name = 'chain-test'"),
    )
    .await;
    assert_eq!(
        log_rows.len(),
        1,
        "B's own trigger should have fired too (chained), got {log_rows:?}"
    );
}

#[tokio::test]
#[ignore]
async fn before_trigger_does_not_block_the_operation_it_fires_on() {
    // Exercises the RETURN-statement fix live: a `Before` trigger's return
    // value is what Postgres actually persists/allows — the wrong RETURN
    // (or referencing an unassigned NEW/OLD) would either fail outright or
    // silently block the operation. Here the handler doesn't self-modify
    // the row, just confirms the operation completes and the audit fires.
    let module = unique_module("live_trig_before");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(
        4, // On.Delete
        "Before",
        &format!("insert {module}::Log {{ old_name := __old__.name }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap())
        .await
        .unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'before-delete' }}"),
    )
    .await;
    exec(
        &pool,
        &schema,
        &format!("delete {module}::Widget filter .name = 'before-delete'"),
    )
    .await;

    let remaining = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Widget {{ name }} filter .name = 'before-delete'"),
    )
    .await;
    assert!(
        remaining.is_empty(),
        "the delete itself must still have gone through, got {remaining:?}"
    );

    let log_rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ old_name }} filter .old_name = 'before-delete'"),
    )
    .await;
    assert_eq!(
        log_rows.len(),
        1,
        "Before trigger should still have fired and audited, got {log_rows:?}"
    );
}

#[tokio::test]
#[ignore]
async fn migration_path_emits_the_same_working_trigger() {
    // Mirrors `live_execution_on_delete.rs`'s own final "migration path
    // parity" test — proves `diff_schema_steps` (the incremental-migration
    // path `migration create` actually uses) emits a trigger that works
    // identically to `export_schema`'s fresh-install path, closing the gap
    // `expected_triggers()`/the new Phase 11.5 add-loop was built for.
    let module = unique_module("live_trig_migpath");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(
        1,
        "After",
        &format!("insert {module}::Log {{ new_name := __new__.name }}"),
    )];
    let schema = SchemaDescriptor {
        types: vec![widget, log_type(&module)],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;

    let steps = diff_schema_steps(&schema, &DbState::default(), &HashMap::new()).unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'via-diff' }}"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Log {{ new_name }} filter .new_name = 'via-diff'"),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "trigger emitted via the diff path should work identically, got {rows:?}"
    );
}
