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

//! `with` bindings read back by later bindings and by the statement they
//! belong to, run against a live Postgres. See `live_execution_smoke.rs` for
//! the harness and how to run it (this binary is `--test
//! live_execution_with_bindings`).

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::{ExtensionOids, PgPool};
use pylon_value::DecodedValue;

fn ty(name: &str, module: &str, properties: Vec<PropertyDescriptor>) -> TypeDescriptor {
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

fn optional(link: LinkDescriptor) -> LinkDescriptor {
    LinkDescriptor { nullable: true, ..link }
}

fn grants() -> MultiLinkDescriptor {
    MultiLinkDescriptor {
        nullable: true,
        ..multilink("grants", "")
    }
}

/// A `System` holding `grants`, each naming an `Account` and a `role`, and a
/// `Brand` interface over `Corporate` and `Product` that points at a system
/// twice (`system`, `preview`) and holds `grants` of its own.
fn schema(m: &str) -> SchemaDescriptor {
    let grant_q = format!("{m}::Grant");
    let system_q = format!("{m}::System");
    let brand_q = format!("{m}::Brand");
    let brand_links = vec![
        optional(link("system", &system_q)),
        optional(link("preview", &system_q)),
    ];
    let brand_grants = vec![MultiLinkDescriptor {
        target: grant_q.clone(),
        ..grants()
    }];
    let brand = TypeDescriptor {
        abstract_: true,
        links: brand_links.clone(),
        multilinks: brand_grants.clone(),
        ..ty("Brand", m, vec![id_prop(), text_prop("name")])
    };
    let implementor = |name: &str| TypeDescriptor {
        interfaces: vec![brand_q.clone()],
        links: brand_links.clone(),
        multilinks: brand_grants.clone(),
        ..ty(name, m, vec![id_prop(), text_prop("name")])
    };
    SchemaDescriptor {
        types: vec![
            ty("Account", m, vec![id_prop(), text_prop("name")]),
            TypeDescriptor {
                links: vec![link("account", &format!("{m}::Account"))],
                ..ty("Grant", m, vec![id_prop(), text_prop("role")])
            },
            TypeDescriptor {
                multilinks: vec![MultiLinkDescriptor {
                    target: grant_q.clone(),
                    ..grants()
                }],
                ..ty("System", m, vec![id_prop(), text_prop("name")])
            },
            brand,
            implementor("Corporate"),
            implementor("Product"),
        ],
        ..Default::default()
    }
}

async fn run(pool: &PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap_or_else(|e| panic!("{pyql}: {e}"));
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap_or_else(|e| panic!("{pyql}\n{}\n{e}", compiled.sql))
}

fn names(rows: &[DecodedValue]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => match &fields[2] {
                DecodedValue::Str(s) => s.clone(),
                other => panic!("expected a name, got {other:?}"),
            },
            other => panic!("expected a Composite-shaped row, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

/// One binding asks whether a link of another binding holds a matching row,
/// a second whether an interface row points at that binding and holds a
/// matching row of its own, and the statement filters on both. Each `exists`
/// over a path select used to keep only the path's first row source and drop
/// its joins, so the filter named an alias no FROM clause had — the query
/// compiled and then failed with "missing FROM-clause entry".
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn bindings_holding_exists_over_another_bindings_link_combine_in_a_filter() {
    let m = unique_module("live_with_exists");
    let sd = schema(&m);
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    for setup in [
        format!("insert {m}::Account {{ name := 'owner' }}"),
        format!("insert {m}::Account {{ name := 'viewer' }}"),
        format!("insert {m}::Account {{ name := 'stranger' }}"),
        format!(
            "insert {m}::System {{ name := 'owned', grants := (insert {m}::Grant {{ role := 'Owner', \
             account := (select {m}::Account filter .name = 'owner') }}) }}"
        ),
        format!("insert {m}::System {{ name := 'shown' }}"),
        format!("insert {m}::System {{ name := 'hidden' }}"),
        format!(
            "insert {m}::Product {{ name := 'p', preview := (select {m}::System filter .name = 'shown'), \
             grants := (insert {m}::Grant {{ role := 'Viewer', \
             account := (select {m}::Account filter .name = 'viewer') }}) }}"
        ),
        format!("insert {m}::Corporate {{ name := 'c', system := (select {m}::System filter .name = 'hidden') }}"),
    ] {
        run(&pool, &sd, &setup).await;
    }

    let visible = |system: &str, account: &str| {
        format!(
            "with \
               system := (select {m}::System filter .name = '{system}' limit 1), \
               owned := exists (select system.grants filter .account.name = '{account}' and .role = 'Owner'), \
               shown := exists ( \
                 select {m}::Brand \
                 filter (.system = system or .preview = system) \
                    and exists (select .grants filter .account.name = '{account}') \
               ) \
             select system {{ * }} \
             filter owned or shown \
             limit 1"
        )
    };

    for (system, account, expected) in [
        ("owned", "owner", vec!["owned"]),
        ("owned", "viewer", vec![]),
        ("shown", "viewer", vec!["shown"]),
        ("shown", "owner", vec![]),
        ("hidden", "viewer", vec![]),
        ("hidden", "stranger", vec![]),
    ] {
        let rows = run(&pool, &sd, &visible(system, account)).await;
        assert_eq!(
            names(&rows),
            expected,
            "`{system}` as seen by `{account}` should be {expected:?}"
        );
    }

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{m}\" CASCADE;"))
        .await
        .unwrap();
}

/// `set { preview := (select .preview filter .id != system.id) }` keeps a link
/// unless it equals the value another pointer is being set to — a path
/// select over the row being updated, as a shape already allows, which `set`
/// refused with "expected a type name as SELECT subject".
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_update_sets_a_link_from_a_path_select_on_its_own_row() {
    let m = unique_module("live_with_update_path");
    let sd = schema(&m);
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    for setup in [
        format!("insert {m}::System {{ name := 'one' }}"),
        format!("insert {m}::System {{ name := 'two' }}"),
        format!("insert {m}::Corporate {{ name := 'kept', preview := (select {m}::System filter .name = 'one') }}"),
        format!("insert {m}::Corporate {{ name := 'cleared', preview := (select {m}::System filter .name = 'two') }}"),
    ] {
        run(&pool, &sd, &setup).await;
    }

    // Once through the type itself and once through the interface, whose
    // update is written once per implementor.
    for (subject, brand) in [("Corporate", "kept"), ("Brand", "cleared")] {
        run(
            &pool,
            &sd,
            &format!(
                "with system := (select {m}::System filter .name = 'two' limit 1) \
                 update {m}::{subject} \
                 filter .name = '{brand}' \
                 set {{ \
                   system := system, \
                   preview := (select .preview filter .id != system.id) \
                 }}"
            ),
        )
        .await;
    }

    let rows = run(
        &pool,
        &sd,
        &format!(
            "select {m}::Corporate {{ name, system_name := .system.name, preview_name := .preview.name }} \
             order by .name"
        ),
    )
    .await;
    let seen: Vec<Vec<String>> = rows
        .iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => fields[2..]
                .iter()
                .map(|f| match f {
                    DecodedValue::Str(s) => s.clone(),
                    DecodedValue::Null => "{}".into(),
                    other => format!("{other:?}"),
                })
                .collect(),
            other => panic!("expected a Composite-shaped row, got {other:?}"),
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            vec!["cleared".to_string(), "two".into(), "{}".into()],
            vec!["kept".to_string(), "two".into(), "one".into()],
        ],
        "a preview equal to the new system is cleared, any other kept"
    );

    pool.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{m}\" CASCADE;"))
        .await
        .unwrap();
}

/// The same path select in an insert has no row to read yet, so it is refused
/// when compiled rather than emitted as a correlation to a row the INSERT has
/// no FROM for.
#[test]
fn an_insert_refuses_a_path_select_on_the_row_it_writes() {
    let sd = schema("m");
    let err = query::compile(
        "insert m::Corporate { name := 'x', preview := (select .preview limit 1) }",
        &sd,
    )
    .expect_err("an insert has no row of its own to walk from");
    assert!(
        err.to_string().contains("does not exist yet"),
        "unexpected error: {err}"
    );
}
