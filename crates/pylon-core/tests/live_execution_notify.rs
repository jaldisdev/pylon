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

//! Live-Postgres tests proving `notify()` end to end: a schema `Trigger`
//! whose handler calls `notify(Channel, payload)` actually fires
//! `pg_notify` when its DDL-exported trigger runs, and a real `LISTEN`ing
//! connection receives it. This is "trigger handler integration" for the
//! `Channel`/`notify()` feature — the compiler-level unit tests in
//! `crates/pylon-core/src/ir/mod.rs` cover the SQL generation itself; this
//! file covers the whole pipeline (schema → DDL → real trigger firing →
//! real NOTIFY delivery).
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_notify -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{ChannelDescriptor, ChannelPayload, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::listener::PgListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
        bases: vec![],
        properties,
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
    }
}

async fn exec(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
}

/// Polls `predicate` until it's true or ~1s has elapsed — notifications
/// arrive on the listener's background task asynchronously, so this can't
/// assume they're already there the instant the triggering query returns
/// (same pattern as `pylon-pgcon`'s own listener tests).
async fn wait_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..50 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The one payload kind with no live coverage before: an Object channel's
/// payload goes out as `jsonb_build_object(...)::text` and comes back
/// through `json.loads` plus per-field type recovery, and nothing proved
/// that round trip against a real database.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn trigger_notify_on_object_channel_delivers_decodable_json() {
    let module = unique_module("live_notify_object");
    let wire_name = format!("{module}__widget_events");

    let qty = pylon_core::schema::PropertyDescriptor {
        pg_type: "int8".into(),
        ..text_prop("qty")
    };
    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name"), qty]);
    widget.triggers = vec![trigger(
        1,
        "After",
        "select notify(WidgetEvents, { label := __new__.name, count := __new__.qty })",
    )];
    let schema = SchemaDescriptor {
        types: vec![widget],
        channels: vec![ChannelDescriptor {
            name: "WidgetEvents".into(),
            module: module.clone(),
            wire_name: wire_name.clone(),
            payload: ChannelPayload::Object(vec![
                ("label".to_string(), "text".to_string()),
                ("count".to_string(), "int8".to_string()),
            ]),
            description: None,
        }],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    let listener = PgListener::connect(&test_dsn(), move |n| {
        received_clone.lock().unwrap().push(n.payload().to_string());
    })
    .await
    .unwrap();
    listener.listen(&wire_name).await.unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'gadget', qty := 7 }}"),
    )
    .await;

    wait_until(|| !received.lock().unwrap().is_empty()).await;

    let payload = received.lock().unwrap()[0].clone();
    // Must be JSON — a ROW()-style composite text form here would mean every
    // Object channel is undecodable by both clients.
    let parsed: serde_json::Value =
        serde_json::from_str(&payload).unwrap_or_else(|e| panic!("payload {payload:?} is not JSON: {e}"));
    assert_eq!(parsed["label"], serde_json::json!("gadget"));
    assert_eq!(parsed["count"], serde_json::json!(7));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn trigger_notify_on_scalar_channel_delivers_the_property_value() {
    let module = unique_module("live_notify_scalar");
    let wire_name = format!("{module}__widget_pings");

    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(1, "After", "select notify(WidgetPings, __new__.name)")];
    let schema = SchemaDescriptor {
        types: vec![widget],
        channels: vec![ChannelDescriptor {
            name: "WidgetPings".into(),
            module: module.clone(),
            wire_name: wire_name.clone(),
            payload: ChannelPayload::Scalar("text".into()),
            description: None,
        }],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    let listener = PgListener::connect(&test_dsn(), move |n| {
        received_clone
            .lock()
            .unwrap()
            .push((n.channel().to_string(), n.payload().to_string()));
    })
    .await
    .unwrap();
    listener.listen(&wire_name).await.unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'gadget' }}"),
    )
    .await;

    wait_until(|| !received.lock().unwrap().is_empty()).await;

    let got = received.lock().unwrap().clone();
    assert_eq!(got, vec![(wire_name, "gadget".to_string())]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn trigger_notify_on_type_channel_delivers_the_new_rows_id() {
    let module = unique_module("live_notify_type");
    let wire_name = format!("{module}__widget_updates");

    let mut widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    widget.triggers = vec![trigger(1, "After", "select notify(WidgetUpdates, __new__)")];
    let schema = SchemaDescriptor {
        types: vec![widget],
        channels: vec![ChannelDescriptor {
            name: "WidgetUpdates".into(),
            module: module.clone(),
            wire_name: wire_name.clone(),
            payload: ChannelPayload::Type(format!("{module}::Widget")),
            description: None,
        }],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    let listener = PgListener::connect(&test_dsn(), move |n| {
        received_clone.lock().unwrap().push(n.payload().to_string());
    })
    .await
    .unwrap();
    listener.listen(&wire_name).await.unwrap();

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Widget {{ name := 'gadget' }}"),
    )
    .await;

    wait_until(|| !received.lock().unwrap().is_empty()).await;

    // Cast id -> text inline in the shape so the query hands back a plain
    // string directly, rather than a raw DecodedValue::Uuid([u8; 16]) this
    // test binary has no dependency available to format back into the
    // dashed textual form the NOTIFY payload itself carries.
    let rows = query::compile(
        &format!("select {module}::Widget {{ id_text := <str>.id }} filter .name = 'gadget'"),
        &schema,
    )
    .unwrap();
    let ids = pool
        .query_typed(&rows.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    // Field 0 is always the implicit `id` column every shape carries;
    // `id_text` (the one explicit computed field) lands at index 1 —
    // matching this suite's own convention (see live_execution_triggers.rs's
    // `field(&rows[0], 1)` for its own single-field shapes).
    let inserted_id_text = field(&ids[0], 1);
    let pylon_value::DecodedValue::Str(inserted_id_text) = inserted_id_text else {
        panic!("expected str, got {inserted_id_text:?}")
    };

    let got = received.lock().unwrap().clone();
    assert_eq!(got, vec![inserted_id_text.clone()]);
}

fn field(row: &pylon_value::DecodedValue, i: usize) -> &pylon_value::DecodedValue {
    match row {
        pylon_value::DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&pylon_value::DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}
