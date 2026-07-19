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
//!     cargo test -p pylon-core --test live_execution -- --ignored
//! ```

use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::{ExtensionOids, PgPool};
use pylon_value::CachedValue;

pub fn test_dsn() -> String {
    std::env::var("PYLON_PGCON_TEST_DSN")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
}

pub async fn test_pool() -> PgPool {
    PgPool::connect(&test_dsn(), 5).await.unwrap()
}

/// Every fixture schema in this suite uses a nanos-suffixed module name so
/// each test run gets its own real Postgres schema — `export_schema` emits
/// `CREATE SCHEMA IF NOT EXISTS <module>` for any non-`default` module
/// (`export/mod.rs`) — giving isolation without needing teardown, matching
/// `migrate.rs`'s existing `unique_table_name()` convention (unique names,
/// no `DROP` afterward).
pub fn unique_module(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{nanos}")
}

pub fn id_prop() -> PropertyDescriptor {
    PropertyDescriptor {
        name: "id".into(),
        pg_type: "uuid".into(),
        nullable: false,
        default_sql: Some("gen_random_uuid()".into()),
        default_pyql: None,
        description: None,
        check_constraints: vec![],
        is_exclusive: true,
        is_pk: true,
        is_readonly: true,
        rewrites: vec![],
        tuple_members: None,
    }
}

pub fn text_prop(name: &str) -> PropertyDescriptor {
    PropertyDescriptor {
        name: name.into(),
        pg_type: "text".into(),
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
    }
}

/// A single `Widget { id, name }` type — just enough to prove the pipeline
/// end-to-end before investing in richer fixtures for the cast-matrix,
/// backlink, and on_delete test groups.
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
