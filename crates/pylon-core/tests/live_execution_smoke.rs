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

//! Live-Postgres execution tests — companion to `pylon-core`'s SQL-text
//! snapshot tests (`crates/pylon-core/src/sql/mod.rs`). Building a schema,
//! exporting it, and running compiled PyQL against a real Postgres catches
//! bug classes the snapshot tests structurally can't: a compile-time gate
//! rejecting SQL Postgres would have happily accepted, or trigger/cascade
//! logic that only misbehaves once a row is actually deleted.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring the
//! existing live-DB test pattern in `pylon_core::migrate`'s test module and
//! `pylon-pgcon`'s own tests — same DSN env var, same default
//! (`postgresql://postgres:postgres@localhost:5432/pylon_live_test`, matching
//! `pylon-demo`'s `docker-compose.yml`). Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
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
use pylon_value::DecodedValue;

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
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn insert_and_select_round_trip_a_real_value() {
    let module = unique_module("live_smoke");
    let schema = smoke_schema(&module);
    let ddl = export_schema(&schema).unwrap();

    let pool = test_pool().await;
    pool.batch_execute(&ddl).await.unwrap();

    let insert = query::compile(&format!("insert {module}::Widget {{ name := 'hello' }}"), &schema).unwrap();
    assert!(
        insert.param_names.is_empty(),
        "fixture query intentionally uses no PyQL params"
    );
    let affected = pool.execute_typed(&insert.sql, &[]).await.unwrap();
    assert_eq!(affected, 1);

    let select = query::compile(
        &format!("select {module}::Widget {{ name }} filter .name = 'hello'"),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one Widget row back, got {rows:?}");
    match &rows[0] {
        // A schema-backed object row decodes as a positional `Composite`,
        // not a name-keyed `Object` — position 0 is always the
        // auto-injected `__type__` discriminator (see `pylon/query.py`'s
        // `_decode()`, the `"object"` branch), remaining positions are the
        // selected pointers in shape order.
        DecodedValue::Composite(fields) => {
            assert_eq!(fields.first(), Some(&DecodedValue::Str(format!("{module}::Widget"))));
            assert_eq!(fields.get(1), Some(&DecodedValue::Str("hello".to_string())));
        }
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_abstract_type_reads_the_rows_of_the_types_inheriting_it() {
    // An abstract type has no table, so selecting it has to read the tables of
    // the concrete types naming it as a parent.
    let module = unique_module("live_abstract");
    let named_q = format!("{module}::Named");
    let widget = smoke_schema(&module).types.remove(0);
    let named = TypeDescriptor {
        name: "Named".into(),
        table: "Named".into(),
        abstract_: true,
        materialized: false,
        ..widget.clone()
    };
    let concrete = |name: &str| TypeDescriptor {
        name: name.into(),
        table: name.into(),
        parents: vec![named_q.clone()],
        ..widget.clone()
    };
    let schema = SchemaDescriptor {
        types: vec![named, concrete("Widget"), concrete("Gadget")],
        ..Default::default()
    };
    let pool = test_pool().await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();
    for (type_name, name) in [("Widget", "a"), ("Gadget", "b")] {
        let insert = query::compile(&format!("insert {module}::{type_name} {{ name := '{name}' }}"), &schema).unwrap();
        pool.execute_typed(&insert.sql, &[]).await.unwrap();
    }

    for pyql in [
        format!("select {named_q} {{ name }} order by .name"),
        format!("with named := (select {named_q} filter .name != '') select named {{ name }} order by .name"),
    ] {
        let select = query::compile(&pyql, &schema).unwrap();
        let rows = pool
            .query_typed(&select.sql, &[], &ExtensionOids::default())
            .await
            .unwrap();
        let read: Vec<(DecodedValue, DecodedValue)> = rows
            .iter()
            .map(|row| match row {
                DecodedValue::Composite(fields) => (fields[0].clone(), fields[1].clone()),
                other => panic!("expected a Composite-shaped row, got {other:?}"),
            })
            .collect();
        assert_eq!(
            read,
            vec![
                (
                    DecodedValue::Str(format!("{module}::Widget")),
                    DecodedValue::Str("a".to_string())
                ),
                (
                    DecodedValue::Str(format!("{module}::Gadget")),
                    DecodedValue::Str("b".to_string())
                ),
            ],
            "{pyql}"
        );
    }
}
