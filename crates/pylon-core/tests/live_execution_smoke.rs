//! Live-Postgres execution tests — companion to `pylon-core`'s SQL-text
//! snapshot tests (`crates/pylon-core/src/sql/mod.rs`). Building a schema,
//! exporting it, and running compiled PyQL against a real Postgres catches
//! bug classes the snapshot tests structurally can't: a compile-time gate
//! rejecting SQL Postgres would have happily accepted, or trigger/cascade
//! logic that only misbehaves once a row is actually deleted.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring the
//! existing live-DB test pattern in `pylon_core::migrate`'s test module and
//! `pylon-pgcon`'s own tests — same DSN env var, same default
//! (`postgresql://postgres:postgres@localhost:5418/app`, matching
//! `pylon-demo`'s `docker-compose.yml`). Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/app \
//!     cargo test -p pylon-core --test live_execution_smoke -- --ignored
//! ```
//!
//! This file is the harness smoke test; see `live_execution_cast_matrix.rs`,
//! `live_execution_backlinks.rs`, and `live_execution_on_delete.rs` for the
//! rest of the suite (shared helpers live in `tests/common/mod.rs`).

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

/// A single `Widget { id, name }` type — just enough to prove the pipeline
/// end-to-end before investing in richer fixtures for the other test groups.
fn smoke_schema(module: &str) -> SchemaDescriptor {
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
    }
}

#[tokio::test]
#[ignore]
async fn insert_and_select_round_trip_a_real_value() {
    let module = unique_module("live_smoke");
    let schema = smoke_schema(&module);
    let ddl = export_schema(&schema).unwrap();

    let pool = test_pool().await;
    pool.batch_execute(&ddl).await.unwrap();

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'hello' }}"), &schema).unwrap();
    assert!(insert.param_names.is_empty(), "fixture query intentionally uses no PyQL params");
    let affected = pool.execute_typed(&insert.sql, &[]).await.unwrap();
    assert_eq!(affected, 1);

    let select = query::compile(
        &format!("select {module}::Widget {{ name }} filter .name = 'hello'"),
        &schema,
    )
    .unwrap();
    let rows = pool.query_typed(&select.sql, &[], &ExtensionOids::default()).await.unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one Widget row back, got {rows:?}");
    match &rows[0] {
        // A schema-backed object row decodes as a positional `Composite`,
        // not a name-keyed `Object` — position 0 is always the
        // auto-injected `__type__` discriminator (see `pylon/query.py`'s
        // `_decode()`, the `"object"` branch), remaining positions are the
        // selected pointers in shape order.
        CachedValue::Composite(fields) => {
            assert_eq!(fields.first(), Some(&CachedValue::Str(format!("{module}::Widget"))));
            assert_eq!(fields.get(1), Some(&CachedValue::Str("hello".to_string())));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}
