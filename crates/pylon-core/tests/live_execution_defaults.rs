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

//! Live-Postgres tests for pointer defaults a column DEFAULT cannot hold.
//!
//! PostgreSQL evaluates a column DEFAULT with no arguments and no query in
//! scope, so a default that reads a session global or selects the object to
//! link to can never be one. Such a default has to be expanded into the
//! insert's own shape instead, which is what makes
//! `created_by := account_of_transaction()` work at all; Pylon emitted a column
//! DEFAULT or nothing, so that default silently left the column NULL on every
//! insert.
//!
//! The shape proven here is jaldis's own: an object-returning function reading
//! a session global, named as a link's default.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_defaults -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{
    FunctionDescriptor, GlobalDescriptor, LinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;

fn ty(name: &str, module: &str, properties: Vec<PropertyDescriptor>, links: Vec<LinkDescriptor>) -> TypeDescriptor {
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
    }
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

/// jaldis's `Creatable.created_by := default::account_of_transaction()` in
/// miniature: `Widget.created_by` defaults to an `Account`-returning function
/// that filters on the `current_account_id` session global.
fn schema_with_a_global_reading_link_default(module: &str) -> SchemaDescriptor {
    let account = ty("Account", module, vec![id_prop(), text_prop("name")], vec![]);
    let mut created_by = link("created_by", &format!("{module}::Account"));
    created_by.nullable = true;
    created_by.default_pyql = Some(format!("{module}::account_of_transaction()"));
    let widget = ty("Widget", module, vec![id_prop(), text_prop("name")], vec![created_by]);
    SchemaDescriptor {
        types: vec![account, widget],
        globals: vec![GlobalDescriptor {
            name: "current_account_id".into(),
            module: module.into(),
            scalar_type: "std::uuid".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        }],
        functions: vec![FunctionDescriptor {
            name: "account_of_transaction".into(),
            module: module.into(),
            params: vec![],
            return_pg_type: format!("{module}::Account"),
            return_is_object: true,
            return_is_set: false,
            return_is_polymorphic: false,
            volatility: "stable".into(),
            body: format!("select {module}::Account filter .id = global {module}::current_account_id"),
        }],
        ..Default::default()
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_link_default_reading_a_global_populates_the_column() {
    let module = unique_module("live_def_global");
    let sd = schema_with_a_global_reading_link_default(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let compiled = query::compile(&format!("insert {module}::Account {{ name := 'Ada' }}"), &sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    let compiled = query::compile(&format!("select {module}::Account {{ id }}"), &sd).unwrap();
    let accounts = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    let account_id = field(&accounts[0], 1).clone();

    // The insert never names `created_by`; the default has to supply it, and
    // can only do so from inside the query, where the global is bound.
    let compiled = query::compile(&format!("insert {module}::Widget {{ name := 'w' }}"), &sd).unwrap();
    assert_eq!(
        compiled.param_names,
        vec![format!("__global__{module}::current_account_id")]
    );
    pool.execute_typed(&compiled.sql, std::slice::from_ref(&account_id))
        .await
        .unwrap();

    let compiled = query::compile(&format!("select {module}::Widget {{ created_by: {{ name }} }}"), &sd).unwrap();
    let rows = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let creator = field(&rows[0], 2);
    assert!(
        matches!(creator, DecodedValue::Composite(_)),
        "created_by must be populated, got {creator:?}"
    );
    assert_eq!(field(creator, 2), &DecodedValue::Str("Ada".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_explicit_value_wins_over_the_default() {
    let module = unique_module("live_def_explicit");
    let sd = schema_with_a_global_reading_link_default(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let compiled = query::compile(&format!("insert {module}::Account {{ name := 'Ada' }}"), &sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();

    // `:= {}` names the pointer, so the default does not apply — and with no
    // global bound the query takes no parameter at all.
    let compiled = query::compile(
        &format!("insert {module}::Widget {{ name := 'w', created_by := {{}} }}"),
        &sd,
    )
    .unwrap();
    assert!(compiled.param_names.is_empty(), "{:?}", compiled.param_names);
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();

    let compiled = query::compile(&format!("select {module}::Widget {{ created_by: {{ name }} }}"), &sd).unwrap();
    let rows = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(field(&rows[0], 2), &DecodedValue::Null);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn the_column_carries_no_default_of_its_own() {
    // `DEFAULT account_of_transaction(…)` is DDL PostgreSQL refuses to run:
    // the function takes the globals argument, which a column DEFAULT has no
    // way to pass. Emitting it used to fail the migration, or be dropped.
    let module = unique_module("live_def_nocol");
    let sd = schema_with_a_global_reading_link_default(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    let rows = pool
        .query_typed(
            &format!(
                "SELECT column_default FROM information_schema.columns \
                 WHERE table_schema = '{module}' AND table_name = 'Widget' \
                 AND column_name = 'created_by_id'"
            ),
            &[],
            &ExtensionOids::default(),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the column must exist");
    // A one-column plain-SQL select decodes as the scalar itself, not a row.
    assert_eq!(rows[0], DecodedValue::Null, "the column must carry no default");
}
