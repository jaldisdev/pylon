//! Live-Postgres integration tests for `pylon-client` — same harness
//! convention as `pylon-core/tests/live_execution_*.rs`: `#[ignore]`d,
//! run explicitly against a real Postgres (`PYLON_PGCON_TEST_DSN`, default
//! `postgresql://postgres:postgres@localhost:5418/app`).
//!
//! Each test gets its own nanos-suffixed module/schema (mirrors
//! `pylon-core/tests/common/mod.rs`'s `unique_module`) so nothing needs
//! manual cleanup and concurrent `cargo test` runs never collide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pylon_client::{CachedValue, Client, Isolation, Value};
use pylon_core::export::export_schema;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};

fn test_dsn() -> String {
    std::env::var("PYLON_PGCON_TEST_DSN")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
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

/// Writes `schema` to a fresh temp file and applies its DDL against a real
/// Postgres, returning a `Client` pointed at that schema/DSN — exercises
/// the real `.pylon/schema.json` file-loading path (`Builder::schema_path`),
/// not just an in-memory `SchemaDescriptor`.
async fn setup(schema: &SchemaDescriptor) -> Client {
    let ddl = export_schema(schema).unwrap();
    let pool = pylon_pgcon::PgPool::connect(&test_dsn(), 5).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("pylon-client-live-test-{nanos}.json"));
    std::fs::write(&path, serde_json::to_string(schema).unwrap()).unwrap();

    Client::builder(test_dsn()).max_pool_size(5).schema_path(path).build().await.unwrap()
}

/// Like `setup`, but also opts into read-through caching at a fresh temp
/// LMDB directory — for tests exercising `Builder::cache`.
async fn setup_with_cache(schema: &SchemaDescriptor) -> Client {
    let ddl = export_schema(schema).unwrap();
    let pool = pylon_pgcon::PgPool::connect(&test_dsn(), 5).await.unwrap();
    pool.batch_execute(&ddl).await.unwrap();

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("pylon-client-live-test-{nanos}.json"));
    std::fs::write(&path, serde_json::to_string(schema).unwrap()).unwrap();
    let cache_dir = std::env::temp_dir().join(format!("pylon-client-live-test-cache-{nanos}"));

    Client::builder(test_dsn())
        .max_pool_size(5)
        .schema_path(path)
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
