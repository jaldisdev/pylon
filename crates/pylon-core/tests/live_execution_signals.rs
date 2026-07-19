//! Post-commit signal capture trigger live-execution tests. See
//! `live_execution_smoke.rs` for the harness's purpose and how to run
//! these (same pattern, this binary is `--test live_execution_signals`).
//!
//! This only tests the Rust half of the pipeline — that a mutation on a
//! type with a registered signal actually lands a correctly-shaped row in
//! `_pylon."SignalOutbox"`. Invoking the Python handler itself is a
//! separate, Python-side integration test (`pylon.signal` dispatcher),
//! since a Rust test can't hold a live Python callable.

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::stdlib::ddl::export_stdlib;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, SignalEntry, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

fn ty_with_signal(name: &str, module: &str, on: u8) -> TypeDescriptor {
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
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
        signals: vec![SignalEntry { on }],
    }
}

async fn setup(on: u8) -> (String, SchemaDescriptor, pylon_pgcon::PgPool) {
    let module = unique_module("live_signals");
    let schema = SchemaDescriptor {
        types: vec![ty_with_signal("Widget", &module, on)],
        ..Default::default()
    };
    let pool = test_pool().await;
    // Stdlib bootstrap DDL (idempotent — CREATE TABLE IF NOT EXISTS etc.)
    // owns _pylon."SignalOutbox"; not part of export_schema's per-schema DDL.
    pool.batch_execute(&export_stdlib()).await.unwrap();
    let ddl = export_schema(&schema).unwrap();
    pool.batch_execute(&ddl).await.unwrap();
    (module, schema, pool)
}

async fn outbox_rows_for(pool: &pylon_pgcon::PgPool, type_name: &str) -> Vec<CachedValue> {
    pool.query_typed_named(
        "SELECT type_name, operation, old_row, new_row FROM _pylon.\"SignalOutbox\" WHERE type_name = $1 ORDER BY enqueued_at",
        &[CachedValue::Str(type_name.to_string())],
        &ExtensionOids::default(),
    )
    .await
    .unwrap()
}

fn field<'a>(row: &'a CachedValue, name: &str) -> &'a CachedValue {
    let CachedValue::Object(fields) = row else { panic!("expected Object, got {row:?}") };
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v).unwrap_or_else(|| panic!("no field {name} in {row:?}"))
}

#[tokio::test]
#[ignore]
async fn insert_writes_new_row_only() {
    let (module, schema, pool) = setup(1 /* Insert */).await;
    let type_name = format!("{module}::Widget");

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'Alpha' }}"), &schema).unwrap();
    pool.execute_typed(&insert.sql, &[]).await.unwrap();

    let rows = outbox_rows_for(&pool, &type_name).await;
    assert_eq!(rows.len(), 1, "expected exactly one outbox row, got {rows:?}");
    assert_eq!(field(&rows[0], "operation"), &CachedValue::Str("INSERT".to_string()));
    assert_eq!(field(&rows[0], "old_row"), &CachedValue::Null, "old_row must be NULL for an Insert");
    assert_eq!(field(field(&rows[0], "new_row"), "name"), &CachedValue::Str("Alpha".to_string()));
}

#[tokio::test]
#[ignore]
async fn update_writes_both_old_and_new_row() {
    let (module, schema, pool) = setup(2 /* Update */).await;
    let type_name = format!("{module}::Widget");

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'Alpha' }}"), &schema).unwrap();
    pool.execute_typed(&insert.sql, &[]).await.unwrap();
    // Insert alone shouldn't enqueue anything — only Update is registered.
    assert_eq!(outbox_rows_for(&pool, &type_name).await.len(), 0);

    let update = query::compile(&format!("update {module}::Widget filter .name = 'Alpha' set {{ name := 'Beta' }}"), &schema).unwrap();
    pool.execute_typed(&update.sql, &[]).await.unwrap();

    let rows = outbox_rows_for(&pool, &type_name).await;
    assert_eq!(rows.len(), 1, "expected exactly one outbox row for the Update, got {rows:?}");
    assert_eq!(field(&rows[0], "operation"), &CachedValue::Str("UPDATE".to_string()));
    assert_eq!(field(field(&rows[0], "old_row"), "name"), &CachedValue::Str("Alpha".to_string()));
    assert_eq!(field(field(&rows[0], "new_row"), "name"), &CachedValue::Str("Beta".to_string()));
}

#[tokio::test]
#[ignore]
async fn delete_writes_old_row_only() {
    let (module, schema, pool) = setup(4 /* Delete */).await;
    let type_name = format!("{module}::Widget");

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'Alpha' }}"), &schema).unwrap();
    pool.execute_typed(&insert.sql, &[]).await.unwrap();
    assert_eq!(outbox_rows_for(&pool, &type_name).await.len(), 0, "Insert shouldn't enqueue when only Delete is registered");

    let delete = query::compile(&format!("delete {module}::Widget filter .name = 'Alpha'"), &schema).unwrap();
    pool.execute_typed(&delete.sql, &[]).await.unwrap();

    let rows = outbox_rows_for(&pool, &type_name).await;
    assert_eq!(rows.len(), 1, "expected exactly one outbox row for the Delete, got {rows:?}");
    assert_eq!(field(&rows[0], "operation"), &CachedValue::Str("DELETE".to_string()));
    assert_eq!(field(&rows[0], "new_row"), &CachedValue::Null, "new_row must be NULL for a Delete");
    assert_eq!(field(field(&rows[0], "old_row"), "name"), &CachedValue::Str("Alpha".to_string()));
}

#[tokio::test]
#[ignore]
async fn unregistered_operations_on_the_same_type_enqueue_nothing() {
    // Only Insert is registered — the trigger's event list is scoped to
    // exactly the registered operations (`signal_trigger_infos` builds
    // "INSERT" alone here, not "INSERT OR UPDATE OR DELETE"), so an Update
    // on the same type shouldn't fire the trigger at all, not just be
    // filtered out after firing.
    let (module, schema, pool) = setup(1 /* Insert */).await;
    let type_name = format!("{module}::Widget");

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'Alpha' }}"), &schema).unwrap();
    pool.execute_typed(&insert.sql, &[]).await.unwrap();
    assert_eq!(outbox_rows_for(&pool, &type_name).await.len(), 1);

    let update = query::compile(&format!("update {module}::Widget filter .name = 'Alpha' set {{ name := 'Beta' }}"), &schema).unwrap();
    pool.execute_typed(&update.sql, &[]).await.unwrap();
    assert_eq!(
        outbox_rows_for(&pool, &type_name).await.len(), 1,
        "the Update must not enqueue anything — On.Insert was the only registered operation"
    );
}
