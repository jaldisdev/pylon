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

//! Live-Postgres migration-diff correctness tests — companion to
//! `live_execution_smoke.rs`, but targeting the diff engine's *content*
//! (does it propose the right changes, and only the right changes) rather
//! than PyQL execution: real data survives real migrations against a real
//! Postgres, not just "the DDL compiled."
//!
//! Scenarios 1-2 are the regression guard for a real bug found and fixed
//! this session: `schema_to_db_state`'s baseline never recorded
//! cache-invalidation/signal triggers as "already present," so `migration
//! create` proposed recreating them forever, even with zero real schema
//! changes. Either scenario would have caught it immediately.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_migration_diff -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::diff::{diff_schema_steps, diff_schema_steps_with_renames_and_fills, schema_to_db_state, Verb};
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, SignalEntry, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;
use std::collections::HashMap;

/// A `Widget { id, name }` type plus a self-referential `related` multilink
/// (so a junction table gets its own cache-invalidate trigger too) and a
/// signal registration (so the capture trigger/function get emitted) — the
/// exact combination that exposed the phantom-trigger bug: cache-invalidate
/// triggers on both the main table and the junction table, plus a signal
/// capture trigger, none of which `schema_to_db_state`'s baseline used to
/// record as already present.
fn triggered_schema(module: &str) -> SchemaDescriptor {
    let qname = format!("{module}::Widget");
    SchemaDescriptor {
        types: vec![TypeDescriptor {
            name: "Widget".into(),
            module: module.into(),
            table: "Widget".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![id_prop(), text_prop("name")],
            links: vec![],
            multilinks: vec![multilink("related", &qname)],
            computed: vec![],
            constraints: vec![],
            indexes: vec![],
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![SignalEntry { on: 1 | 2 | 4 }],
        }],
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn phantom_trigger_regression_second_create_reports_zero_changes() {
    let module = unique_module("live_migdiff_offline");
    let schema = triggered_schema(&module);

    let empty = pylon_core::diff::DbState::default();
    let steps = diff_schema_steps(&schema, &empty, &HashMap::new()).unwrap();
    let pool = test_pool().await;
    // Every concrete table gets an unconditional `pylon_cache_invalidate`
    // trigger (see `expected_triggers()` in `diff/mod.rs`), which references
    // `_pylon.notify_cache_invalidate()` — that function (and the `_pylon`
    // schema itself) only exist once `export_stdlib()`'s DDL has run,
    // normally done once via `pylon database install`.
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }

    // The offline projection exactly as stored in `_pylon."Migrations".db_state`
    // — this is the baseline `migration create` actually diffs against when a
    // tip row exists, and the one that broke.
    let baseline = schema_to_db_state(&schema);
    let further = diff_schema_steps(&schema, &baseline, &HashMap::new()).unwrap();
    assert!(further.is_empty(), "expected zero further migration steps against the offline baseline, got: {further:?}");

    // This test's own `zero_changes_against_live_introspection_after_apply`
    // sibling does a *full-database* introspection diff, which would
    // otherwise see this test's leftover schema as "should be dropped" —
    // clean up so tests in this file don't interfere with each other.
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;")).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn zero_changes_against_live_introspection_after_apply() {
    let module = unique_module("live_migdiff_introspect");
    let schema = triggered_schema(&module);
    let ddl = export_schema(&schema).unwrap();

    let pool = test_pool().await;
    // Every concrete table gets an unconditional `pylon_cache_invalidate`
    // trigger (see `expected_triggers()` in `diff/mod.rs`), which references
    // `_pylon.notify_cache_invalidate()` — that function (and the `_pylon`
    // schema itself) only exist once `export_stdlib()`'s DDL has run,
    // normally done once via `pylon database install`.
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();

    // Catches drift the other direction from the offline-baseline scenario
    // above: live introspection vs. schema, not schema-projection vs. schema.
    // NOTE: this is a *full-database* diff, so it only holds if every other
    // test in this file has already cleaned up its own schema by the time
    // this one runs (see each sibling test's own cleanup at the end).
    assert_zero_further_steps(&pool, &schema).await;

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;")).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn rename_detected_and_applied_survives_real_data() {
    let module = unique_module("live_migdiff_rename");

    let v1 = SchemaDescriptor {
        types: vec![TypeDescriptor {
            name: "Widget".into(),
            module: module.clone(),
            table: "Widget".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![id_prop(), text_prop("name")],
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
        }],
        ..Default::default()
    };
    let ddl = export_schema(&v1).unwrap();

    let pool = test_pool().await;
    // Every concrete table gets an unconditional `pylon_cache_invalidate`
    // trigger (see `expected_triggers()` in `diff/mod.rs`), which references
    // `_pylon.notify_cache_invalidate()` — that function (and the `_pylon`
    // schema itself) only exist once `export_stdlib()`'s DDL has run,
    // normally done once via `pylon database install`.
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'keep-me' }}"), &v1).unwrap();
    assert_eq!(pool.execute_typed(&insert.sql, &[]).await.unwrap(), 1);

    // v2: same type, renamed Widget -> Gadget, same columns.
    let mut v2 = v1.clone();
    v2.types[0].name = "Gadget".into();
    v2.types[0].table = "Gadget".into();

    let live = pylon_core::introspect::introspect_db_state(&pool).await.unwrap();
    let type_renames = vec![(module.clone(), "Widget".to_string(), module.clone(), "Gadget".to_string())];

    // `diff_schema_steps_with_renames_and_fills` only *projects* the rename
    // onto its in-memory `current` copy so the rest of the diff sees no
    // further change for this table — it never emits the rename DDL itself.
    // The real `ALTER TABLE ... RENAME TO ...` is the caller's own
    // responsibility (mirrors `pylon/cli/commands/migrations.py`'s
    // `_rename_prompt_loop`, which builds this exact statement directly
    // rather than sourcing it from the diff engine's output).
    pool.batch_execute(&format!(r#"ALTER TABLE "{module}"."Widget" RENAME TO "Gadget";"#)).await.unwrap();

    let steps = diff_schema_steps_with_renames_and_fills(&v2, &live, &type_renames, &[], &[]).unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }

    let select =
        query::compile(&format!("select {module}::Gadget {{ name }} filter .name = 'keep-me'"), &v2).unwrap();
    let rows = pool.query_typed(&select.sql, &[], &ExtensionOids::default()).await.unwrap();
    assert_eq!(rows.len(), 1, "renamed row should still be there with its original data, got {rows:?}");
    match &rows[0] {
        CachedValue::Composite(fields) => {
            assert_eq!(fields.get(1), Some(&CachedValue::Str("keep-me".to_string())));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;")).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn property_type_change_casts_existing_data() {
    let module = unique_module("live_migdiff_cast");

    let v1 = SchemaDescriptor {
        types: vec![TypeDescriptor {
            name: "Widget".into(),
            module: module.clone(),
            table: "Widget".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            properties: vec![id_prop(), text_prop("code")],
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
        }],
        ..Default::default()
    };
    let ddl = export_schema(&v1).unwrap();

    let pool = test_pool().await;
    // Every concrete table gets an unconditional `pylon_cache_invalidate`
    // trigger (see `expected_triggers()` in `diff/mod.rs`), which references
    // `_pylon.notify_cache_invalidate()` — that function (and the `_pylon`
    // schema itself) only exist once `export_stdlib()`'s DDL has run,
    // normally done once via `pylon database install`.
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();

    let insert = query::compile(&format!("insert {module}::Widget {{ code := '42' }}"), &v1).unwrap();
    assert_eq!(pool.execute_typed(&insert.sql, &[]).await.unwrap(), 1);

    // v2: `code` widened from text to int8 — the type-changing-column path
    // that gets a `required_input` cast-expression placeholder.
    let mut v2 = v1.clone();
    v2.types[0].properties[1].pg_type = "int8".into();

    let live = pylon_core::introspect::introspect_db_state(&pool).await.unwrap();
    let steps = diff_schema_steps(&v2, &live, &HashMap::new()).unwrap();
    let table_step = steps
        .iter()
        .find(|s| s.verb == Verb::Alter && s.object_desc.contains("Widget"))
        .expect("expected an alter step for Widget's type change");
    assert!(!table_step.required_input.is_empty(), "type change should carry a required_input cast expression");

    for op in table_step.resolved_ddl(&HashMap::new()) {
        pool.batch_execute(&op.sql).await.unwrap();
    }

    let select = query::compile(&format!("select {module}::Widget {{ code }} filter .code = 42"), &v2).unwrap();
    let rows = pool.query_typed(&select.sql, &[], &ExtensionOids::default()).await.unwrap();
    assert_eq!(rows.len(), 1, "existing row should have survived the cast with the correct value, got {rows:?}");
    match &rows[0] {
        CachedValue::Composite(fields) => {
            assert_eq!(fields.get(1), Some(&CachedValue::I64(42)));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;")).await.unwrap();
}
