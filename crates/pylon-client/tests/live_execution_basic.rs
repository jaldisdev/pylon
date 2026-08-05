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

//! Live-Postgres integration tests for `pylon-client` — same harness
//! convention as `pylon-core/tests/live_execution_*.rs`: `#[ignore]`d,
//! run explicitly against a real Postgres via `PYLON_PGCON_TEST_DSN`
//! (required — no hardcoded fallback).
//!
//! Each test gets its own nanos-suffixed module/schema (mirrors
//! `pylon-core/tests/common/mod.rs`'s `unique_module`) so nothing needs
//! manual cleanup and concurrent `cargo test` runs never collide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pylon_client::{CachedValue, Client, Isolation, Value};
use pylon_core::export::export_schema;
use pylon_core::schema::{
    ChannelDescriptor, ChannelPayload, PropertyDescriptor, SchemaDescriptor, TriggerDescriptor, TypeDescriptor,
};

fn test_dsn() -> String {
    std::env::var("PYLON_PGCON_TEST_DSN")
        .expect("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
}

fn unique_module(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{seq}")
}

fn id_prop() -> PropertyDescriptor {
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
        column_type: None,
    }
}

fn text_prop(name: &str) -> PropertyDescriptor {
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
        column_type: None,
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

/// One `Person { name }` type in a fresh module, plus a `default::viewer_id`
/// global — enough surface to exercise query/query_single/execute/globals/
/// transactions end to end.
fn person_schema(module: &str) -> SchemaDescriptor {
    SchemaDescriptor {
        types: vec![TypeDescriptor {
            name: "Person".into(),
            module: module.into(),
            table: "Person".into(),
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

/// Applies `schema`'s DDL against a real Postgres and writes it to
/// `_pylon."Schema"` (the same singleton row `Builder::build` fetches from —
/// see `pylon_core::migrate::write_schema_snapshot`), returning a `Client`
/// pointed at that DSN. Exercises the real DB-fetch path, not just an
/// in-memory `SchemaDescriptor`.
///
/// `_pylon."Schema"` is one row shared by the whole test DSN — unlike
/// `unique_module`'s per-test isolation, concurrent `setup`/`setup_with_cache`
/// calls race on this same row, so this file's tests must run with
/// `--test-threads=1` (or otherwise serialized), not cargo's default
/// parallelism.
async fn setup(schema: &SchemaDescriptor) -> Client {
    let ddl = export_schema(schema).unwrap();
    let pool = pylon_pgcon::PgPool::connect(&test_dsn(), 5).await.unwrap();
    pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
    pylon_core::migrate::ensure_tracking_tables(&pool).await.unwrap();
    // Every concrete table's DDL below includes an unconditional
    // `pylon_cache_invalidate` trigger referencing
    // `_pylon.notify_cache_invalidate()` — only `export_stdlib()` creates
    // that function, so it must run first (mirrors every other
    // `live_execution_*.rs` file's `bootstrap()` helper).
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();
    pylon_core::migrate::write_schema_snapshot(&pool, &serde_json::to_string(schema).unwrap()).await.unwrap();

    Client::builder(test_dsn()).max_pool_size(5).build().await.unwrap()
}

/// Like `setup`, but also opts into read-through caching at a fresh temp
/// LMDB directory — for tests exercising `Builder::cache`.
async fn setup_with_cache(schema: &SchemaDescriptor) -> Client {
    let ddl = export_schema(schema).unwrap();
    let pool = pylon_pgcon::PgPool::connect(&test_dsn(), 5).await.unwrap();
    pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
    pylon_core::migrate::ensure_tracking_tables(&pool).await.unwrap();
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();
    pylon_core::migrate::write_schema_snapshot(&pool, &serde_json::to_string(schema).unwrap()).await.unwrap();

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let cache_dir = std::env::temp_dir().join(format!("pylon-client-live-test-cache-{nanos}"));

    Client::builder(test_dsn())
        .max_pool_size(5)
        .cache(cache_dir, 10)
        .build()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn query_and_execute_round_trip() {
    let module = unique_module("live_client_basic");
    let client = setup(&person_schema(&module)).await;

    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("Alice".into()))],
        )
        .await
        .unwrap();

    let rows = client.query(&format!("select {module}::Person {{ name }}"), &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let Value::Object(person) = &rows[0] else { panic!("expected Object, got {:?}", rows[0]) };
    assert_eq!(person.get("name"), Some(&Value::Str("Alice".into())));
    assert_eq!(person.type_name(), Some(format!("{module}::Person").as_str()));
}

#[tokio::test]
#[ignore]
async fn query_single_enforces_cardinality() {
    let module = unique_module("live_client_single");
    let client = setup(&person_schema(&module)).await;

    assert_eq!(client.query_single(&format!("select {module}::Person"), &[]).await.unwrap(), None);

    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("Bob".into()))],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("Carol".into()))],
        )
        .await
        .unwrap();

    let err = client.query_single(&format!("select {module}::Person"), &[]).await.unwrap_err();
    assert!(matches!(err, pylon_client::Error::ResultCardinality { got: 2 }), "got: {err:?}");
}

#[tokio::test]
#[ignore]
async fn globals_fill_the_dunder_global_param_slot() {
    let mut schema = person_schema(&unique_module("live_client_globals"));
    schema.globals.push(pylon_core::schema::GlobalDescriptor {
        name: "viewer_name".into(),
        module: schema.types[0].module.clone(),
        scalar_type: "text".into(),
        required: false,
        default_expr: None,
        computed_expr: None,
    });
    let module = schema.types[0].module.clone();
    let client = setup(&schema).await;

    let authed = client.with_globals([(format!("{module}::viewer_name"), CachedValue::Str("Dave".into()))]);
    let rows = authed.query("select global viewer_name", &[]).await.unwrap();
    assert_eq!(rows, vec![Value::Str("Dave".into())]);

    // A sibling view without the global sees it as unset (NULL).
    let rows = client.query("select global viewer_name", &[]).await.unwrap();
    assert_eq!(rows, vec![Value::Null]);
}

#[tokio::test]
#[ignore]
async fn transaction_commits_on_success() {
    let module = unique_module("live_client_tx");
    let client = setup(&person_schema(&module)).await;

    client
        .transaction(Isolation::Serializable, |tx| {
            let module = module.clone();
            Box::pin(async move {
                tx.execute(
                    &format!("insert {module}::Person {{ name := <str>$name }}"),
                    &[("name", CachedValue::Str("Erin".into()))],
                )
                .await
            })
        })
        .await
        .unwrap();

    let rows = client.query(&format!("select {module}::Person {{ name }}"), &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
#[ignore]
async fn transaction_rolls_back_on_error_and_does_not_retry_non_retriable_errors() {
    let module = unique_module("live_client_tx_rollback");
    let client = setup(&person_schema(&module)).await;

    let mut attempts = 0;
    let result: Result<(), pylon_client::Error> = client
        .transaction(Isolation::Serializable, |tx| {
            attempts += 1;
            let module = module.clone();
            Box::pin(async move {
                tx.execute(
                    &format!("insert {module}::Person {{ name := <str>$name }}"),
                    &[("name", CachedValue::Str("Frank".into()))],
                )
                .await?;
                // An unknown link name is a compile error — not retriable —
                // so the whole transaction (including the insert above)
                // must roll back, and the loop must not retry.
                tx.execute(&format!("select {module}::Person.nonexistent_link"), &[]).await
            })
        })
        .await;

    assert!(result.is_err());
    assert_eq!(attempts, 1, "a non-retriable error must not be retried");

    let rows = client.query(&format!("select {module}::Person"), &[]).await.unwrap();
    assert!(rows.is_empty(), "the insert must have been rolled back, got: {rows:?}");
}

#[tokio::test]
#[ignore]
async fn cached_query_serves_stale_data_until_something_else_invalidates_it() {
    let module = unique_module("live_client_cache");
    let client = setup_with_cache(&person_schema(&module)).await;

    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("Alice".into()))],
        )
        .await
        .unwrap();

    let query = format!("select {module}::Person {{ name }}");
    let first = client.query(&query, &[]).await.unwrap();
    assert_eq!(first.len(), 1);
    let Value::Object(person) = &first[0] else { panic!("expected Object") };
    assert_eq!(person.get("name"), Some(&Value::Str("Alice".into())));

    // Mutate the underlying row directly, bypassing the cache entirely.
    client
        .raw_connection()
        .execute_typed(&format!("UPDATE \"{module}\".\"Person\" SET name = 'Mutated'"), &[])
        .await
        .unwrap();

    // No invalidation listener is running (by design — see Builder::cache's
    // docs), so the second call must still serve the stale cached value.
    let second = client.query(&query, &[]).await.unwrap();
    assert_eq!(second, first, "a read-through cache hit must return the stale cached value");

    // Confirm the cache actually holds an entry (not just a coincidental
    // real re-fetch that happened to match).
    let stats = client.cache_stat().unwrap().expect("cache was configured");
    assert!(stats.entry_count >= 1);

    client.cache_clear().unwrap();
    let stats_after_clear = client.cache_stat().unwrap().unwrap();
    assert_eq!(stats_after_clear.entry_count, 0);

    // After clearing the cache, the same query now genuinely re-fetches
    // and observes the mutation.
    let third = client.query(&query, &[]).await.unwrap();
    let Value::Object(person) = &third[0] else { panic!("expected Object") };
    assert_eq!(person.get("name"), Some(&Value::Str("Mutated".into())));
}

// ── Client::listen() ────────────────────────────────────────────────────────

/// Waits (up to ~5s) for `listener.recv()` to yield something — a real
/// `NOTIFY` arrives on the listener's own background task asynchronously,
/// so this can't assume it's already there the instant the triggering
/// query returns (same convention `pylon-pgcon`'s own listener tests use).
async fn recv_with_timeout(listener: &mut pylon_client::ChannelListener) -> pylon_client::Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
        .await
        .expect("timed out waiting for a notification")
        .expect("listener closed with no notification")
}

#[tokio::test]
#[ignore]
async fn listen_decodes_a_scalar_channel_payload() {
    let module = unique_module("live_client_listen_scalar");
    let mut schema = person_schema(&module);
    schema.types[0].triggers = vec![TriggerDescriptor {
        on: 1, // On::Insert
        timing: "After".into(),
        handler: "select notify(Pings, __new__.name)".into(),
    }];
    schema.channels = vec![ChannelDescriptor {
        name: "Pings".into(),
        module: module.clone(),
        wire_name: format!("{module}__pings"),
        payload: ChannelPayload::Scalar("text".into()),
        description: None,
    }];
    let client = setup(&schema).await;

    let mut listener = client.listen("Pings").await.unwrap();
    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("gadget".into()))],
        )
        .await
        .unwrap();

    assert_eq!(recv_with_timeout(&mut listener).await.unwrap(), Value::Str("gadget".to_string()));
}

#[tokio::test]
#[ignore]
async fn listen_decodes_a_type_channel_payload_as_the_rows_id() {
    let module = unique_module("live_client_listen_type");
    let mut schema = person_schema(&module);
    schema.types[0].triggers = vec![TriggerDescriptor {
        on: 1, // On::Insert
        timing: "After".into(),
        handler: "select notify(PersonUpdates, __new__)".into(),
    }];
    schema.channels = vec![ChannelDescriptor {
        name: "PersonUpdates".into(),
        module: module.clone(),
        wire_name: format!("{module}__person_updates"),
        payload: ChannelPayload::Type(format!("{module}::Person")),
        description: None,
    }];
    let client = setup(&schema).await;

    let mut listener = client.listen("PersonUpdates").await.unwrap();
    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name }}"),
            &[("name", CachedValue::Str("gadget".into()))],
        )
        .await
        .unwrap();

    let payload = recv_with_timeout(&mut listener).await.unwrap();
    let Value::Uuid(id) = payload else { panic!("expected Uuid, got {payload:?}") };

    let rows = client.query(&format!("select {module}::Person {{ id }}"), &[]).await.unwrap();
    let Value::Object(person) = &rows[0] else { panic!("expected Object") };
    assert_eq!(person.get("id"), Some(&Value::Uuid(id)));
}

#[tokio::test]
#[ignore]
async fn listen_decodes_an_object_channel_payload() {
    let module = unique_module("live_client_listen_object");
    let mut schema = person_schema(&module);
    schema.types[0].properties.push(float_prop("score"));
    schema.types[0].triggers = vec![TriggerDescriptor {
        on: 1, // On::Insert
        timing: "After".into(),
        handler: "select notify(PersonReady, { name := __new__.name, score := __new__.score })".into(),
    }];
    schema.channels = vec![ChannelDescriptor {
        name: "PersonReady".into(),
        module: module.clone(),
        wire_name: format!("{module}__person_ready"),
        payload: ChannelPayload::Object(vec![("name".into(), "text".into()), ("score".into(), "float8".into())]),
        description: None,
    }];
    let client = setup(&schema).await;

    let mut listener = client.listen("PersonReady").await.unwrap();
    client
        .execute(
            &format!("insert {module}::Person {{ name := <str>$name, score := <float64>$score }}"),
            &[("name", CachedValue::Str("gadget".into())), ("score", CachedValue::F64(0.75))],
        )
        .await
        .unwrap();

    let payload = recv_with_timeout(&mut listener).await.unwrap();
    let Value::Object(obj) = payload else { panic!("expected Object, got {payload:?}") };
    assert_eq!(obj.get("name"), Some(&Value::Str("gadget".to_string())));
    assert_eq!(obj.get("score"), Some(&Value::Float64(0.75)));
}

#[tokio::test]
#[ignore]
async fn listen_raises_on_a_malformed_payload() {
    let module = unique_module("live_client_listen_malformed");
    let mut schema = person_schema(&module);
    schema.channels = vec![ChannelDescriptor {
        name: "Ids".into(),
        module: module.clone(),
        wire_name: format!("{module}__ids"),
        payload: ChannelPayload::Type(format!("{module}::Person")),
        description: None,
    }];
    let client = setup(&schema).await;

    let mut listener = client.listen("Ids").await.unwrap();

    // Bypass notify()'s own compile-time shape validation entirely — a raw
    // NOTIFY with text that isn't a valid uuid, exactly the kind of
    // mismatch `listen()` must surface as an error rather than silently
    // drop.
    client
        .raw_connection()
        .execute_typed(&format!("SELECT pg_notify('{module}__ids', 'not-a-uuid')"), &[])
        .await
        .unwrap();

    let err = recv_with_timeout(&mut listener).await.err().expect("expected a malformed-payload error");
    assert!(matches!(err, pylon_client::Error::MalformedPayload(_)), "got: {err:?}");
}
