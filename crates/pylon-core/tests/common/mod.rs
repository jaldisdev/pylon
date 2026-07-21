//! Shared helpers for the `live_execution_*` live-Postgres integration test
//! binaries — see any of those files for the harness's purpose and how to
//! run them. Each `tests/live_execution_*.rs` file is its own Cargo test
//! binary, so this lives under `tests/common/` (no trailing file named
//! `common.rs` alongside it) specifically so Cargo does *not* treat it as
//! an additional top-level test binary of its own.
//!
//! Every helper here is used by at least one `live_execution_*.rs` binary,
//! but not all of them — since each binary compiles this module separately,
//! rustc has no visibility into siblings and flags whichever subset a given
//! binary doesn't call as dead code.
#![allow(dead_code)]

use pylon_core::query;
use pylon_core::schema::{LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor};
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
/// no `DROP` afterward). Nanos alone isn't quite enough — concurrent tests
/// (the default) can land on the same clock reading if the OS's timer
/// resolution is coarser than 1ns, causing a real `CREATE SCHEMA` collision
/// (caught live: two tests both got `..._1784464536118769000`) — an
/// in-process atomic counter appended alongside guarantees uniqueness
/// regardless of clock granularity.
pub fn unique_module(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{seq}")
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

/// Required (non-nullable), non-exclusive forward single link — a
/// backlink's source side (e.g. `Team.org: link Org`).
pub fn link(name: &str, target_qname: &str) -> LinkDescriptor {
    LinkDescriptor {
        name: name.into(),
        target: target_qname.into(),
        nullable: false,
        through: None,
        description: None,
        default_pyql: None,
        is_exclusive: false,
        is_readonly: false,
        rewrites: vec![],
        on_delete: vec![],
    }
}

/// Non-nullable multilink with no explicit `through` type (an implicit
/// junction table gets generated) — e.g. a self-referential
/// `Person.friends: multilink Person`.
pub fn multilink(name: &str, target_qname: &str) -> MultiLinkDescriptor {
    MultiLinkDescriptor {
        name: name.into(),
        target: target_qname.into(),
        through: None,
        nullable: false,
        description: None,
        default_pyql: None,
        on_delete: vec![],
    }
}

/// Compiles and live-executes a schema-free scalar `select` expression,
/// returning the single wrapped value — a scalar result decodes as a
/// one-element `Composite` (`SELECT ROW(v) AS result, v FROM (...)`, see
/// `sql::emit`), not a bare value or a name-keyed `Object`.
pub async fn eval_scalar(pool: &PgPool, expr: &str) -> CachedValue {
    let schema = SchemaDescriptor::default();
    let compiled = query::compile(&format!("select {expr}"), &schema).unwrap();
    let rows = pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap();
    assert_eq!(rows.len(), 1, "scalar select should return exactly one row, got {rows:?}");
    match rows.into_iter().next().unwrap() {
        CachedValue::Composite(mut fields) if fields.len() == 1 => fields.remove(0),
        other => panic!("expected a one-element Composite wrapping the scalar, got {other:?}"),
    }
}
