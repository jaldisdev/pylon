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

//! Live-Postgres tests for `expr is Type` written where a single boolean is
//! required — a FILTER, or the condition of an `if … else`.
//!
//! A check that names a type other than the one being selected asks the
//! question once per row of that type, and Pylon emitted that as
//! `ARRAY(SELECT … FROM <that table>)`. A `boolean[]` is not something
//! Postgres will take as a WHERE clause or a CASE/WHEN condition, so the
//! whole query was rejected at parse time by the server — which is why these
//! tests execute the SQL rather than only inspecting it.
//!
//! What they pin:
//!
//!   * a check against a non-polymorphic type answers the same for every
//!     row, so it reads as the constant it is;
//!   * a check against an interface really does answer per row, and still
//!     collapses to one value a filter can take.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_type_check -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::{ExtensionOids, PgPool};
use pylon_value::DecodedValue;

fn bool_prop(name: &str) -> PropertyDescriptor {
    PropertyDescriptor {
        name: name.into(),
        pg_type: "bool".into(),
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

fn ty(
    name: &str,
    module: &str,
    abstract_: bool,
    interfaces: Vec<String>,
    properties: Vec<PropertyDescriptor>,
) -> TypeDescriptor {
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
        abstract_,
        materialized: true,
        description: None,
        parents: vec![],
        interfaces,
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

/// `Brief` as an interface over two implementors — the type the queries
/// select — plus two hierarchies outside it for the check to name. Both
/// `Brand` and `Vendor` are interfaces with two implementors each, matching
/// jaldis's own `brand::Brand`: a non-polymorphic outsider folds away for a
/// reason that does not reach the polymorphic case, so it is the polymorphic
/// one that has to be pinned. Several rows of each, since the bug only shows
/// once more than one row answers.
async fn fixture() -> (PgPool, SchemaDescriptor, String) {
    let module = unique_module("live_type_check");
    let iface = format!("{module}::Brief");
    let brand = format!("{module}::Brand");
    let vendor = format!("{module}::Vendor");
    let schema = SchemaDescriptor {
        types: vec![
            ty(
                "Brief",
                &module,
                true,
                vec![],
                vec![id_prop(), bool_prop("active"), text_prop("label")],
            ),
            ty(
                "CustomBrief",
                &module,
                false,
                vec![iface.clone()],
                vec![id_prop(), bool_prop("active"), text_prop("label")],
            ),
            ty(
                "SystemBrief",
                &module,
                false,
                vec![iface],
                vec![id_prop(), bool_prop("active"), text_prop("label")],
            ),
            ty("Brand", &module, true, vec![], vec![id_prop(), text_prop("kind")]),
            ty(
                "HouseBrand",
                &module,
                false,
                vec![brand.clone()],
                vec![id_prop(), text_prop("kind")],
            ),
            ty(
                "PartnerBrand",
                &module,
                false,
                vec![brand],
                vec![id_prop(), text_prop("kind")],
            ),
            ty("Vendor", &module, true, vec![], vec![id_prop(), text_prop("kind")]),
            ty(
                "Reseller",
                &module,
                false,
                vec![vendor.clone()],
                vec![id_prop(), text_prop("kind")],
            ),
            ty(
                "Wholesaler",
                &module,
                false,
                vec![vendor],
                vec![id_prop(), text_prop("kind")],
            ),
        ],
        ..Default::default()
    };
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();
    for pyql in [
        format!("insert {module}::CustomBrief {{ active := true, label := 'custom' }}"),
        format!("insert {module}::SystemBrief {{ active := true, label := 'system' }}"),
        format!("insert {module}::SystemBrief {{ active := false, label := 'retired' }}"),
        format!("insert {module}::HouseBrand {{ kind := 'house' }}"),
        format!("insert {module}::HouseBrand {{ kind := 'house-two' }}"),
        format!("insert {module}::PartnerBrand {{ kind := 'partner' }}"),
        format!("insert {module}::Reseller {{ kind := 'reseller' }}"),
        format!("insert {module}::Wholesaler {{ kind := 'wholesaler' }}"),
    ] {
        let compiled = query::compile(&pyql, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }
    (pool, schema, module)
}

/// The `label` of every row the query returns, sorted so the assertions
/// don't depend on scan order.
async fn labels(pool: &PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<String> {
    let compiled = query::compile(pyql, schema).unwrap();
    let rows = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| {
            let DecodedValue::Composite(fields) = row else {
                panic!("expected a Composite-shaped row, got {row:?}")
            };
            match fields.last().expect("the shape asked for label") {
                DecodedValue::Str(s) => s.clone(),
                other => panic!("expected label to decode as text, got {other:?}"),
            }
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_check_against_a_disjoint_hierarchy_is_a_condition_postgres_takes() {
    // jaldis's own shape: `filter (… if brand::Brand is brand::CustomBrief
    // else true)` inside `select brand::Brief`, where `Brand` is itself an
    // interface over three rows. No `Brand` can carry a `Brief`'s type, so
    // the `if` always takes its else branch and the filter is `.active`
    // alone. Two ways this used to break: as `boolean[]`, which Postgres
    // refuses outright, and then as one answer per Brand row, which failed
    // the single-value check with "expected at most 1 element, got 3".
    let (pool, schema, module) = fixture().await;
    let found = labels(
        &pool,
        &schema,
        &format!(
            "select {module}::Brief {{ label }} \
             filter (.label = 'custom' if {module}::Brand is {module}::CustomBrief else .active)"
        ),
    )
    .await;
    assert_eq!(found, vec!["custom".to_string(), "system".to_string()]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_check_that_really_differs_per_row_says_which_check_it_was() {
    // What is left after both folds genuinely needs a row: `Vendor` holds a
    // `Reseller` and a `Wholesaler`, so `is Reseller` is true for one and
    // false for the other and there is no single value to filter on. That is
    // a real error — but it has to name the check, since `assert_single`
    // appears nowhere in the query.
    let (pool, schema, module) = fixture().await;
    let pyql = format!("select {module}::Brief {{ label }} filter ({module}::Vendor is {module}::Reseller)");
    let compiled = query::compile(&pyql, &schema).unwrap();
    let error = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .expect_err("two Vendor rows disagree, so there is no single value");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains(&format!(
            "is {module}::Reseller' is asked once for every {module}::Vendor object"
        )),
        "the message has to name the check, not the helper enforcing it:\n{rendered}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_check_over_one_object_answers_from_that_object() {
    // And it does resolve once the source really is one object: narrowing
    // `Vendor` to the single `Reseller` row leaves one answer, which is the
    // value the filter reads.
    let (pool, schema, module) = fixture().await;
    let found = labels(
        &pool,
        &schema,
        &format!(
            "with one := (select {module}::Vendor filter .kind = 'reseller' limit 1) \
             select {module}::Brief {{ label }} filter (one is {module}::Reseller)"
        ),
    )
    .await;
    assert_eq!(
        found,
        vec!["custom".to_string(), "retired".to_string(), "system".to_string()]
    );
}
