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

//! Live-Postgres tests for `??` over a walk off a `with` binding.
//!
//! `A ?? B` answers on `A` being the *empty set*, and under Pylon's
//! empty-set semantics a property that is NULL and a link that is absent
//! both reach nothing. So the one thing `??` must not do is let the walk
//! decide whether there is a row at all: folded into the enclosing select's
//! own row source, an absent link joins to nothing and a NULL property fails
//! the walk's own `IS NOT NULL` guard — the row disappears, the coalesce
//! never runs, and the fallback it exists to supply is unreachable. That is
//! what these tests pin, one shape per way of reaching nothing.
//!
//! Binding the walk first (`held := account.preferences.default_currency`,
//! then `held ?? …`) always answered correctly, as did reading it in a free
//! object's field or in a shape — so the assertions below are equally a
//! check that the four spellings agree.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_coalesce -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{LinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;

/// `Account { name, preferences? }` over `AccountPreferences { label,
/// default_currency? }` — an optional link onto an optional property, the
/// two independent ways `account.preferences.default_currency` reaches
/// nothing. `label` repeats the owning account's name, so the preferences
/// row can also be bound on its own.
fn coalesce_schema(module: &str) -> SchemaDescriptor {
    let optional = |name: &str| PropertyDescriptor {
        nullable: true,
        ..text_prop(name)
    };
    let ty = |name: &str, properties: Vec<PropertyDescriptor>, links: Vec<LinkDescriptor>| TypeDescriptor {
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
    };
    let mut preferences = link("preferences", &format!("{module}::AccountPreferences"));
    preferences.nullable = true;
    SchemaDescriptor {
        types: vec![
            ty(
                "AccountPreferences",
                vec![id_prop(), text_prop("label"), optional("default_currency")],
                vec![],
            ),
            ty("Account", vec![id_prop(), text_prop("name")], vec![preferences]),
        ],
        ..Default::default()
    }
}

async fn setup() -> (String, SchemaDescriptor, pylon_pgcon::PgPool) {
    let module = unique_module("live_coalesce");
    let schema = coalesce_schema(&module);
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();

    // `holds` has preferences naming a currency, `empty` has preferences
    // naming none, `none` has no preferences at all.
    for stmt in [
        format!(
            "insert {module}::Account {{ name := 'holds', preferences := (insert {module}::AccountPreferences {{ label := 'holds', default_currency := 'USD' }}) }}"
        ),
        format!(
            "insert {module}::Account {{ name := 'empty', preferences := (insert {module}::AccountPreferences {{ label := 'empty' }}) }}"
        ),
        format!("insert {module}::Account {{ name := 'none' }}"),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }
    (module, schema, pool)
}

/// The scalar each row of a single-value result carries.
async fn values_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str, account: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(
        &compiled.sql,
        &[DecodedValue::Str(account.to_string())],
        &ExtensionOids::default(),
    )
    .await
    .unwrap()
    .iter()
    .map(|row| match row {
        DecodedValue::Composite(fields) => fields.first().cloned().unwrap_or(DecodedValue::Null),
        other => other.clone(),
    })
    .collect()
}

/// The reported case: a two-hop walk whose last step is a NULL property.
/// This answered the empty set — in jaldis, every account that had set no
/// currency of its own read as unpriced rather than falling back to the
/// package's.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_null_property_two_hops_out_falls_back() {
    let (module, schema, pool) = setup().await;
    let pyql = format!(
        "with
           account := (select detached {module}::Account filter .name = <str>$0 limit 1),
           currency := account.preferences.default_currency ?? 'EUR'
         select currency"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql, "empty").await,
        vec![DecodedValue::Str("EUR".into())],
        "preferences are there and name no currency, so the fallback is the answer"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql, "holds").await,
        vec![DecodedValue::Str("USD".into())],
        "a currency that is set is still the answer"
    );
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// The other way the same walk reaches nothing: the *link* is absent, so
/// there is no preferences row to read a currency off at all.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_absent_link_on_the_way_falls_back() {
    let (module, schema, pool) = setup().await;
    let pyql = format!(
        "with
           account := (select detached {module}::Account filter .name = <str>$0 limit 1),
           currency := account.preferences.default_currency ?? 'EUR'
         select currency"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql, "none").await,
        vec![DecodedValue::Str("EUR".into())],
        "no preferences row to read, which is the empty set `??` answers on"
    );
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// One hop, not two: a NULL property read straight off the binding. Same
/// defect — the walk's own `IS NOT NULL` guard dropped the row before the
/// coalesce could run.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_null_property_one_hop_out_falls_back() {
    let (module, schema, pool) = setup().await;
    let pyql = format!(
        "with
           prefs := (select detached {module}::AccountPreferences filter .label = <str>$0 limit 1)
         select (prefs.default_currency ?? 'EUR')"
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql, "empty").await,
        vec![DecodedValue::Str("EUR".into())],
    );
    assert_eq!(
        values_of(&pool, &schema, &pyql, "holds").await,
        vec![DecodedValue::Str("USD".into())],
    );
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}

/// The spellings that always worked, asserted alongside so the four agree
/// rather than only the one that was fixed: the walk bound to a name of its
/// own first, written as an explicit sub-select, and read in a free object's
/// field.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn every_spelling_of_the_same_fallback_agrees() {
    let (module, schema, pool) = setup().await;
    let bound = format!(
        "with
           account := (select detached {module}::Account filter .name = <str>$0 limit 1),
           held := account.preferences.default_currency,
           currency := held ?? 'EUR'
         select currency"
    );
    let subselect = format!(
        "with account := (select detached {module}::Account filter .name = <str>$0 limit 1)
         select ((select account.preferences.default_currency) ?? 'EUR')"
    );
    let free_object = format!(
        "with account := (select detached {module}::Account filter .name = <str>$0 limit 1)
         select {{ currency := account.preferences.default_currency ?? 'EUR' }}"
    );
    let direct = format!(
        "with
           account := (select detached {module}::Account filter .name = <str>$0 limit 1),
           currency := account.preferences.default_currency ?? 'EUR'
         select currency"
    );
    for name in ["holds", "empty", "none"] {
        let expected = values_of(&pool, &schema, &direct, name).await;
        for (label, pyql) in [
            ("bound", &bound),
            ("sub-select", &subselect),
            ("free object", &free_object),
        ] {
            assert_eq!(
                values_of(&pool, &schema, pyql, name).await,
                expected,
                "the {label} spelling should agree with the direct one for '{name}'"
            );
        }
    }
    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{module}\" CASCADE;"))
        .await
        .unwrap();
}
