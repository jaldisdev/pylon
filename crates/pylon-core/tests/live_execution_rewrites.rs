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

//! Live-Postgres tests for property `Rewrite`s — Pylon's analog of the upstream engine's
//! `create rewrite insert|update using (...)`. Scenarios adapted from the upstream engine's
//! own the upstream rewrites suite (noted per test), narrowed to Pylon's
//! feature surface: no `__specified__` (not implemented — a rewrite always
//! sees the row's actual value, whether it came from an explicit assignment
//! or a default, with no way to distinguish the two), no access policies.
//!
//! Unlike `Trigger`, a `Rewrite` is never emitted as its own DDL object —
//! `Compiler::compile_rewrites` inlines the handler directly into whatever
//! INSERT/UPDATE statement is being compiled, so there's no "migration path
//! parity" scenario to cover here the way `live_execution_triggers.rs` has
//! one (nothing schema-diff-shaped to get out of sync).
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_rewrites -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

fn ty(name: &str, module: &str, properties: Vec<pylon_core::schema::PropertyDescriptor>) -> TypeDescriptor {
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

/// A `Log { id, new_name }` audit type, for the trigger-interaction test.
fn log_type(module: &str) -> TypeDescriptor {
    ty("Log", module, vec![id_prop(), text_prop("new_name")])
}

async fn exec(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap()
}

fn field(row: &CachedValue, i: usize) -> &CachedValue {
    match row {
        CachedValue::Composite(fields) => fields.get(i).unwrap_or(&CachedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

/// Bootstraps `_pylon` (schema, `notify_cache_invalidate()`, tracking
/// tables, ...) — every concrete table gets an unconditional
/// `pylon_cache_invalidate` trigger, which references that function.
async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn insert_rewrite_overrides_assigned_value() {
    // Upstream case:rewrites_01 (insert half)
    let module = unique_module("live_rw_insert");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.properties[1].rewrites = vec![rewrite(1, "'inserted'")]; // On.Insert
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'Whiplash' }}")).await;

    let rows = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name }}")).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("inserted".to_string()), "insert rewrite should override the assigned value");
}

#[tokio::test]
#[ignore]
async fn update_rewrite_overrides_assigned_value() {
    // Upstream case:rewrites_01 (update half)
    let module = unique_module("live_rw_update");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.properties[1].rewrites = vec![rewrite(2, "'updated'")]; // On.Update
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'Whiplash' }}")).await;
    // Insert rewrite is not declared, so the insert itself is unaffected.
    let after_insert = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name }}")).await;
    assert_eq!(field(&after_insert[0], 1), &CachedValue::Str("Whiplash".to_string()));

    exec(&pool, &schema, &format!("update {module}::Widget set {{ name := 'The Godfather' }}")).await;
    let after_update = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name }}")).await;
    assert_eq!(field(&after_update[0], 1), &CachedValue::Str("updated".to_string()), "update rewrite should override the assigned value");
}

#[tokio::test]
#[ignore]
async fn insert_rewrite_applies_to_defaulted_value() {
    // Upstream case:rewrites_03 — interaction with a schema-level default:
    // the rewrite runs on whatever value the property ends up with, whether
    // it came from an explicit assignment or its own Default(...).
    let module = unique_module("live_rw_default");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.properties[1].default_sql = Some("'untitled'".into());
    widget.properties[1].rewrites = vec![rewrite(1, ".name ++ ' (new)'")]; // On.Insert
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'Whiplash' }}")).await;
    exec(&pool, &schema, &format!("insert {module}::Widget {{ }}")).await; // no name given — falls back to the default first

    let rows = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name }} order by .name")).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("untitled (new)".to_string()), "rewrite should still apply to the defaulted value");
    assert_eq!(field(&rows[1], 1), &CachedValue::Str("Whiplash (new)".to_string()));
}

#[tokio::test]
#[ignore]
async fn update_rewrite_references_sibling_property() {
    // Not a upstream-ported scenario — Pylon-specific: a rewrite expression that
    // reads a *different* property of the same row (via the same `.`-scoped
    // context a computed pointer gets) and calls a stdlib function, and
    // fires even though its own column (`shout`) is never itself assigned.
    let module = unique_module("live_rw_sibling");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name"), text_prop("shout")]);
    widget.properties[2].rewrites = vec![rewrite(2, "str_upper(.name)")]; // On.Update
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'quiet', shout := 'quiet' }}")).await;
    exec(&pool, &schema, &format!("update {module}::Widget set {{ name := 'loud' }}")).await; // shout not mentioned

    let rows = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name, shout }}")).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("loud".to_string()));
    assert_eq!(field(&rows[0], 2), &CachedValue::Str("LOUD".to_string()), "update rewrite should fire and see the sibling property's new value even though shout wasn't itself assigned");
}

#[tokio::test]
#[ignore]
async fn insert_only_rewrite_does_not_fire_on_update() {
    // Scoping correctness: an On.Insert-only rewrite must not also apply to
    // a later UPDATE that never declared its own rewrite for that property.
    let module = unique_module("live_rw_scope");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.properties[1].rewrites = vec![rewrite(1, "'inserted'")]; // On.Insert only
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'Whiplash' }}")).await;
    exec(&pool, &schema, &format!("update {module}::Widget set {{ name := 'my-real-name' }}")).await;

    let rows = rows_of(&pool, &schema, &format!("select {module}::Widget {{ name }}")).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("my-real-name".to_string()), "an insert-only rewrite must not fire on update");
}

#[tokio::test]
#[ignore]
async fn multiple_rewrites_on_different_properties_do_not_interfere() {
    let module = unique_module("live_rw_multi");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("a"), text_prop("b")]);
    widget.properties[1].rewrites = vec![rewrite(1, "'A'")];
    widget.properties[2].rewrites = vec![rewrite(1, "'B'")];
    let schema = SchemaDescriptor { types: vec![widget], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ a := 'x', b := 'y' }}")).await;

    let rows = rows_of(&pool, &schema, &format!("select {module}::Widget {{ a, b }}")).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &CachedValue::Str("A".to_string()));
    assert_eq!(field(&rows[0], 2), &CachedValue::Str("B".to_string()), "two independent rewrites on the same insert must not interfere with each other");
}

#[tokio::test]
#[ignore]
async fn trigger_observes_rewritten_value_not_original() {
    // Rewrite/Trigger interaction, Pylon-specific (both features exist here,
    // unlike a straight the upstream engine port): an After-Insert trigger's __new__ must
    // see the value *after* the insert rewrite has been applied, not the
    // originally-assigned one — the row a trigger reads is whatever
    // actually got persisted.
    let module = unique_module("live_rw_trigger");
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.properties[1].rewrites = vec![rewrite(1, "'rewritten'")]; // On.Insert
    widget.triggers = vec![trigger(1, "After", &format!("insert {module}::Log {{ new_name := __new__.name }}"))];
    let schema = SchemaDescriptor { types: vec![widget, log_type(&module)], ..Default::default() };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    exec(&pool, &schema, &format!("insert {module}::Widget {{ name := 'original' }}")).await;

    let log_rows = rows_of(&pool, &schema, &format!("select {module}::Log {{ new_name }}")).await;
    assert_eq!(log_rows.len(), 1);
    assert_eq!(field(&log_rows[0], 1), &CachedValue::Str("rewritten".to_string()), "trigger should observe the rewritten value, not the originally-assigned one");
}
