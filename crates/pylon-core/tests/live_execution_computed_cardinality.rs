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

//! Live-Postgres tests for how many values a walk through a *computed* link
//! stands for.
//!
//! `Computed[Link[HandleData], "select .data order by .created_at desc limit
//! 1"]` is single by its own declaration: the `limit 1` is what makes it one
//! row per owner however many `.data` crosses. That limit is only
//! expressible per source row as a `JOIN LATERAL`, and a free object's field
//! decided its cardinality from the *join kinds* the walk compiled to — so
//! every such computed read as multi and its value came back as a
//! one-element array. In jaldis that handed a list to code expecting a
//! string and took down every subscription quote with `AttributeError:
//! 'list' object has no attribute 'upper'`.
//!
//! The same walk read in a schema-bound shape (`Handle { c :=
//! .latest_data.company }`) always answered the value, since that path asks
//! the schema whether the walk is multi rather than the compiled joins — so
//! the assertions below are equally a check that the two agree.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_computed_cardinality -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{
    ComputedDescriptor, LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;

/// `Handle { value, data: multilink HandleData, latest_data: Computed[Link[
/// HandleData], "select .data order by .seq desc limit 1"] }` — a multi-link
/// and the single computed that picks one row out of it.
fn computed_schema(module: &str, latest_limit: &str) -> SchemaDescriptor {
    let ty = |name: &str,
              properties: Vec<PropertyDescriptor>,
              links: Vec<LinkDescriptor>,
              multilinks: Vec<MultiLinkDescriptor>,
              computed: Vec<ComputedDescriptor>| TypeDescriptor {
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
        links,
        multilinks,
        computed,
        constraints: vec![],
        indexes: vec![],
        partition: None,
        vector_indexes: vec![],
        search_indexes: vec![],
        triggers: vec![],
        junction: false,
        signals: vec![],
    };
    let seq = PropertyDescriptor {
        pg_type: "int8".into(),
        ..text_prop("seq")
    };
    SchemaDescriptor {
        types: vec![
            ty(
                "HandleData",
                vec![id_prop(), text_prop("company"), seq],
                vec![],
                vec![],
                vec![],
            ),
            ty(
                "Handle",
                vec![id_prop(), text_prop("value")],
                vec![],
                vec![multilink("data", &format!("{module}::HandleData"))],
                vec![ComputedDescriptor {
                    name: "latest_data".into(),
                    expression: format!("select .data order by .seq desc {latest_limit}"),
                    return_type: None,
                    link_target: Some(format!("{module}::HandleData")),
                    link_multi: false,
                }],
            ),
        ],
        ..Default::default()
    }
}

async fn setup(latest_limit: &str) -> (String, SchemaDescriptor, pylon_pgcon::PgPool) {
    let module = unique_module("live_computed_card");
    let schema = computed_schema(&module, latest_limit);
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    for stmt in [
        format!("insert {module}::HandleData {{ company := 'Jaldis B.V.', seq := 2 }}"),
        format!("insert {module}::HandleData {{ company := 'Older Name', seq := 1 }}"),
        format!(
            "insert {module}::Handle {{ value := 'mario@example.test', \
             data := (select {module}::HandleData) }}"
        ),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }
    (module, schema, pool)
}

/// The value each row of a one-field result carries.
async fn values_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
        .iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => fields.first().cloned().unwrap_or(DecodedValue::Null),
            other => other.clone(),
        })
        .collect()
}

/// The reported case: a free object's field reading a computed link's own
/// property. The computed's `limit 1` makes it single, so the field holds
/// the value — not a one-element array of it.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_free_objects_field_reads_a_capped_computed_as_one_value() {
    let (module, schema, pool) = setup("limit 1").await;
    let pyql = format!(
        "with handle := (select detached {module}::Handle filter .value = 'mario@example.test' limit 1)
         select {{ company := handle.latest_data.company }}"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql).await,
        vec![DecodedValue::Str("Jaldis B.V.".into())],
        "the computed picks one row, so its company is a value"
    );
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// Without a `limit 1` of its own the computed does stand for a set, and the
/// field holds every company it reaches — the cap is what decides this, not
/// the lateral join the ordering forces either way.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_uncapped_computed_still_reads_as_a_set() {
    let (module, schema, pool) = setup("").await;
    let pyql = format!(
        "with handle := (select detached {module}::Handle filter .value = 'mario@example.test' limit 1)
         select {{ company := handle.latest_data.company }}"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql).await,
        vec![DecodedValue::Array(vec![
            DecodedValue::Str("Jaldis B.V.".into()),
            DecodedValue::Str("Older Name".into()),
        ])],
        "nothing caps the computed, so the field holds the whole set"
    );
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// The spellings that always worked, asserted alongside so they agree rather
/// than only the one that was fixed: the computed capped by an explicit
/// sub-select at the call site, and the same walk read in a schema-bound
/// shape.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn every_spelling_of_the_same_value_agrees() {
    let (module, schema, pool) = setup("limit 1").await;
    let direct = format!(
        "with handle := (select detached {module}::Handle filter .value = 'mario@example.test' limit 1)
         select {{ company := handle.latest_data.company }}"
    );
    let capped_at_the_call_site = format!(
        "with handle := (select detached {module}::Handle filter .value = 'mario@example.test' limit 1)
         select {{ company := (select handle.latest_data limit 1).company }}"
    );
    let expected = values_of(&pool, &schema, &direct).await;
    assert_eq!(
        values_of(&pool, &schema, &capped_at_the_call_site).await,
        expected,
        "a `limit 1` written at the call site should answer what the declared one does"
    );

    // The schema-bound shape reads `__type__` and `id` ahead of its own
    // fields, so the field under test is the third.
    let bound = format!("select {module}::Handle {{ company := .latest_data.company }}");
    let compiled = query::compile(&bound, &schema).unwrap();
    let rows = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    let shaped: Vec<DecodedValue> = rows
        .iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => fields.get(2).cloned().unwrap_or(DecodedValue::Null),
            other => other.clone(),
        })
        .collect();
    assert_eq!(
        shaped, expected,
        "a schema-bound shape should answer what the free object does"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}
