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
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_migration_diff -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::diff::{
    Verb, diff_schema_steps, diff_schema_steps_with_renames_and_fills, index_amendment_ops, schema_to_db_state,
};
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{
    IndexDescriptor, PropertyDescriptor, SchemaDescriptor, SignalEntry, TypeConstraint, TypeDescriptor,
};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;
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
            bases: vec![],
            properties: vec![id_prop(), text_prop("name")],
            links: vec![],
            multilinks: vec![multilink("related", &qname)],
            computed: vec![],
            constraints: vec![],
            indexes: vec![],
            partition: None,
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
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
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
    assert!(
        further.is_empty(),
        "expected zero further migration steps against the offline baseline, got: {further:?}"
    );

    // This test's own `zero_changes_against_live_introspection_after_apply`
    // sibling does a *full-database* introspection diff, which would
    // otherwise see this test's leftover schema as "should be dropped" —
    // clean up so tests in this file don't interfere with each other.
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
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

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
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
            bases: vec![],
            properties: vec![id_prop(), text_prop("name")],
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
    let type_renames = vec![(
        module.clone(),
        "Widget".to_string(),
        module.clone(),
        "Gadget".to_string(),
    )];

    // `diff_schema_steps_with_renames_and_fills` only *projects* the rename
    // onto its in-memory `current` copy so the rest of the diff sees no
    // further change for this table — it never emits the rename DDL itself.
    // The real `ALTER TABLE ... RENAME TO ...` is the caller's own
    // responsibility (mirrors `pylon/cli/commands/migrations.py`'s
    // `_rename_prompt_loop`, which builds this exact statement directly
    // rather than sourcing it from the diff engine's output).
    pool.batch_execute(&format!(r#"ALTER TABLE "{module}"."Widget" RENAME TO "Gadget";"#))
        .await
        .unwrap();

    let steps = diff_schema_steps_with_renames_and_fills(&v2, &live, &type_renames, &[], &[]).unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }

    let select = query::compile(
        &format!("select {module}::Gadget {{ name }} filter .name = 'keep-me'"),
        &v2,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "renamed row should still be there with its original data, got {rows:?}"
    );
    match &rows[0] {
        DecodedValue::Composite(fields) => {
            assert_eq!(fields.get(2), Some(&DecodedValue::Str("keep-me".to_string())));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
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
            bases: vec![],
            properties: vec![id_prop(), text_prop("code")],
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
    assert!(
        !table_step.required_input.is_empty(),
        "type change should carry a required_input cast expression"
    );

    for op in table_step.resolved_ddl(&HashMap::new()) {
        pool.batch_execute(&op.sql).await.unwrap();
    }

    let select = query::compile(&format!("select {module}::Widget {{ code }} filter .code = 42"), &v2).unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "existing row should have survived the cast with the correct value, got {rows:?}"
    );
    match &rows[0] {
        DecodedValue::Composite(fields) => {
            assert_eq!(fields.get(2), Some(&DecodedValue::I64(42)));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// A `Widget { id, name }` with `name` indexed or not — the two revisions a
/// deleted `pylon.Index` declaration sits between.
fn indexed_schema(module: &str, indexed: bool) -> SchemaDescriptor {
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
            bases: vec![],
            properties: vec![id_prop(), text_prop("name")],
            links: vec![],
            multilinks: vec![],
            computed: vec![],
            constraints: vec![],
            indexes: if indexed {
                vec![IndexDescriptor {
                    pointers: vec!["name".into()],
                    expression: None,
                    unique: false,
                    unless: None,
                }]
            } else {
                vec![]
            },
            partition: None,
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![],
        }],
        ..Default::default()
    }
}

/// Every index Postgres holds on one module's `Widget` table.
async fn widget_index_names(pool: &pylon_pgcon::PgPool, module: &str) -> Vec<String> {
    let rows = pool
        .query_typed(
            &format!(
                "SELECT (indexname)::text FROM pg_indexes \
                 WHERE schemaname = '{module}' AND tablename = 'Widget' ORDER BY indexname"
            ),
            &[],
            &ExtensionOids::default(),
        )
        .await
        .unwrap();
    rows.iter()
        .map(|row| match row {
            DecodedValue::Str(s) => s.clone(),
            other => panic!("expected an index name, got {other:?}"),
        })
        .collect()
}

/// Deleting an `Index` declaration has to drop the index — and under the
/// name Postgres actually holds it by, which is the half no SQL-text
/// assertion can check. Left undropped it was a B-tree nothing in the schema
/// referred to any more, maintained on every write forever.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_removed_plain_index_is_dropped_from_the_database() {
    let module = unique_module("live_migdiff_index");
    let before = indexed_schema(&module, true);
    let after = indexed_schema(&module, false);

    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    // Created through the diff rather than through `export_schema`, because
    // the name is what this test is about: the create pass names the index
    // explicitly, and that is the name a migrated database carries.
    let steps = diff_schema_steps(&before, &pylon_core::diff::DbState::default(), &HashMap::new()).unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }
    assert!(
        widget_index_names(&pool, &module)
            .await
            .contains(&"Widget__name_idx".to_string()),
        "the declaration's index should exist to begin with, got {:?}",
        widget_index_names(&pool, &module).await
    );

    let ops = index_amendment_ops(&after, Some(&before), true);
    assert!(!ops.is_empty(), "removing the declaration should propose DDL");
    for op in &ops {
        pool.batch_execute(&op.sql).await.unwrap();
    }
    assert!(
        !widget_index_names(&pool, &module)
            .await
            .contains(&"Widget__name_idx".to_string()),
        "nothing declares it any more, got {:?}",
        widget_index_names(&pool, &module).await
    );

    // And the amendment is settled: with the declaration gone from both
    // sides there is nothing left to drop. (A full-database `assert_zero_
    // further_steps` would also see every other live suite's leftover
    // schema, so the check stays within this fixture.)
    assert!(
        index_amendment_ops(&after, Some(&after), true).is_empty(),
        "a second pass should propose nothing"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// `export_schema`'s DDL dump left a plain index unnamed, so a database built
/// from it carried Postgres's own auto-name (`Widget_name_idx`) where the diff
/// expects the name it generates (`Widget__name_idx`) — and because the diff
/// settles an index with a key by name, the first `migration create` against
/// such a database proposed a *second* index over the same column. Both sides
/// read the one naming helper now.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_export_installed_plain_index_needs_no_further_migration() {
    let module = unique_module("live_migdiff_exportidx");
    let schema = indexed_schema(&module, true);

    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    assert!(
        widget_index_names(&pool, &module)
            .await
            .contains(&"Widget__name_idx".to_string()),
        "the dump should name it the way the diff does, got {:?}",
        widget_index_names(&pool, &module).await
    );

    // A full-database diff, so only this fixture's own module is inspected —
    // every other live suite's leftover schema is in there too.
    let live = pylon_core::introspect::introspect_db_state(&pool).await.unwrap();
    let proposed: Vec<String> = diff_schema_steps(&schema, &live, &HashMap::new())
        .unwrap()
        .iter()
        .flat_map(|step| step.resolved_ddl(&HashMap::new()))
        .map(|op| op.sql)
        .filter(|sql| sql.contains("INDEX") && sql.contains(&module))
        .collect();
    assert!(
        proposed.is_empty(),
        "the index is already there; nothing to propose, got {proposed:?}"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// A `Subscription` whose `(account, package)` pair is exclusive, with the
/// `unless` predicate the revision under test declares — the two revisions a
/// changed `unless` sits between.
fn exclusive_schema(module: &str, unless: &str) -> SchemaDescriptor {
    let nullable = |name: &str, pg_type: &str| PropertyDescriptor {
        nullable: true,
        pg_type: pg_type.into(),
        ..text_prop(name)
    };
    SchemaDescriptor {
        types: vec![TypeDescriptor {
            name: "Subscription".into(),
            module: module.into(),
            table: "Subscription".into(),
            abstract_: false,
            materialized: true,
            description: None,
            parents: vec![],
            interfaces: vec![],
            bases: vec![],
            properties: vec![
                id_prop(),
                text_prop("account"),
                text_prop("package"),
                nullable("ended_at", "timestamptz"),
                nullable("brand", "text"),
            ],
            links: vec![],
            multilinks: vec![],
            computed: vec![],
            constraints: vec![TypeConstraint::Exclusive {
                pointers: vec!["account".into(), "package".into()],
                unless: Some(unless.into()),
            }],
            indexes: vec![],
            partition: None,
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![],
        }],
        ..Default::default()
    }
}

/// Every index definition Postgres holds on one module's `Subscription`
/// table, `WHERE` clause included — which is the half of a partial unique
/// index that no name or column list can report.
async fn subscription_index_defs(pool: &pylon_pgcon::PgPool, module: &str) -> Vec<String> {
    let rows = pool
        .query_typed(
            &format!(
                "SELECT (indexdef)::text FROM pg_indexes \
                 WHERE schemaname = '{module}' AND tablename = 'Subscription' ORDER BY indexname"
            ),
            &[],
            &ExtensionOids::default(),
        )
        .await
        .unwrap();
    rows.iter()
        .map(|row| match row {
            DecodedValue::Str(s) => s.clone(),
            other => panic!("expected an index definition, got {other:?}"),
        })
        .collect()
}

/// Editing an exclusive's `unless` has to rebuild its partial unique index.
/// The main diff settles one of these by columns and uniqueness and asked of
/// the predicate only whether there *was* one, so a predicate-only change
/// proposed nothing at all and the index went on enforcing the rule it was
/// built with — an exclusive that silently disagreed with its own
/// declaration. Adding a second exclusive over *different* pointers was
/// diffed correctly all along; only this case was missed.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_changed_exclusive_predicate_rebuilds_its_index() {
    let module = unique_module("live_migdiff_excl");
    let before = exclusive_schema(&module, "exists(.ended_at)");
    let after = exclusive_schema(&module, "exists(.ended_at) or exists(.brand)");

    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();

    // Installed through the diff rather than through `export_schema`, because
    // the name is what the drop below has to get right, and the create pass's
    // name is the one a migrated database carries.
    let steps = diff_schema_steps(&before, &pylon_core::diff::DbState::default(), &HashMap::new()).unwrap();
    for step in &steps {
        for op in step.resolved_ddl(&HashMap::new()) {
            pool.batch_execute(&op.sql).await.unwrap();
        }
    }
    let installed = subscription_index_defs(&pool, &module).await;
    assert!(
        installed
            .iter()
            .any(|def| def.contains("Subscription_account_package_0_key") && def.contains("ended_at")),
        "the exclusive's partial unique index should exist to begin with, got {installed:?}"
    );
    assert!(
        !installed.iter().any(|def| def.contains("brand")),
        "no index should mention the new predicate's column yet, got {installed:?}"
    );

    let ops = index_amendment_ops(&after, Some(&before), true);
    let sql: Vec<&str> = ops.iter().map(|op| op.sql.as_str()).collect();
    assert!(
        sql.iter()
            .any(|s| s.starts_with("DROP INDEX CONCURRENTLY") && s.contains("Subscription_account_package_0_key")),
        "the stale index should be dropped, got {sql:?}"
    );
    assert!(
        sql.iter().any(|s| s.starts_with("CREATE UNIQUE INDEX CONCURRENTLY")
            && s.contains("Subscription_account_package_0_key")
            && s.contains("brand")),
        "and re-made from the new predicate, got {sql:?}"
    );
    assert!(
        ops.iter().all(|op| op.non_transactional),
        "CONCURRENTLY cannot run inside a transaction block"
    );

    for op in &ops {
        pool.batch_execute(&op.sql).await.unwrap();
    }
    let amended = subscription_index_defs(&pool, &module).await;
    let unique: Vec<&String> = amended
        .iter()
        .filter(|def| def.contains("Subscription_account_package_0_key"))
        .collect();
    assert_eq!(
        unique.len(),
        1,
        "the rebuild should leave one index, not a second one beside the stale one, got {amended:?}"
    );
    assert!(
        unique[0].contains("brand"),
        "the index Postgres holds should now enforce the declared predicate, got {amended:?}"
    );

    // And the amendment is settled: with both sides on the new predicate
    // there is nothing left to rebuild. (A full-database `assert_zero_
    // further_steps` would also see every other live suite's leftover
    // schema, so the check stays within this fixture.)
    assert!(
        index_amendment_ops(&after, Some(&after), true).is_empty(),
        "a second pass should propose nothing"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// `export_schema` used to leave an exclusive's partial unique index
/// unnamed, so a database built from the dump carried Postgres's own
/// auto-name (`Subscription_account_package_idx`) and the rebuild above had
/// nothing to drop. Both sides read the one naming helper now.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_export_installed_exclusive_index_carries_the_diffs_name() {
    let module = unique_module("live_migdiff_exportexcl");
    let schema = exclusive_schema(&module, "exists(.ended_at)");

    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    let installed = subscription_index_defs(&pool, &module).await;
    assert!(
        installed
            .iter()
            .any(|def| def.contains("Subscription_account_package_0_key")),
        "the dump should name it the way the diff does, got {installed:?}"
    );

    // A full-database diff, so only this fixture's own module is inspected —
    // every other live suite's leftover schema is in there too.
    let live = pylon_core::introspect::introspect_db_state(&pool).await.unwrap();
    let proposed: Vec<String> = diff_schema_steps(&schema, &live, &HashMap::new())
        .unwrap()
        .iter()
        .flat_map(|step| step.resolved_ddl(&HashMap::new()))
        .map(|op| op.sql)
        .filter(|sql| sql.contains("INDEX") && sql.contains(&module))
        .collect();
    assert!(
        proposed.is_empty(),
        "the index is already there; nothing to propose, got {proposed:?}"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}
