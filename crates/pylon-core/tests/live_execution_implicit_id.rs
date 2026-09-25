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

//! Live-Postgres tests for the `id` a shape gets without asking for one.
//!
//! The upstream engine puts `id` at the front of every object shape it compiles for the
//! binary protocol, and `the upstream Python client` requests that unconditionally
//! (`INJECT_OUTPUT_OBJECT_IDS` in `protocol.pyx`), so `o.id` works on a
//! result even where the query only named other pointers. Pylon used to
//! return the shape verbatim, which made `.id` on `options: { value }` read
//! as unset rather than as the row's id — silently, since the field exists
//! on the schema class either way.
//!
//! What these tests pin, following the upstream engine's `_get_shape_configuration_inner`:
//!
//!   * an explicit shape gains an `id` in front, at the root and nested;
//!   * a shape that names `id` itself keeps its own, once, where it wrote it;
//!   * `*` already covers `id`, so nothing is added beside it;
//!   * a mutation body gets nothing — a link value there is read as one
//!     column, and a second column makes the subquery illegal;
//!   * `<json>` gets nothing, matching the upstream engine, whose JSON output carries no
//!     implicit id either.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_implicit_id -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::query::ShapeNode;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;

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

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

/// The executed rows alongside the shape they decode against — both halves
/// matter here, since the point is that a column is present *and* named.
async fn run(
    pool: &pylon_pgcon::PgPool,
    sd: &SchemaDescriptor,
    pyql: &str,
) -> (Vec<DecodedValue>, pylon_core::query::ShapeDescriptor) {
    let compiled = query::compile(pyql, sd).unwrap();
    let rows = pool
        .query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    (rows, compiled.shape.clone())
}

fn root_pointers(shape: &pylon_core::query::ShapeDescriptor) -> &[ShapeNode] {
    match &shape.root {
        ShapeNode::Object { pointers, .. } => pointers,
        other => panic!("expected an object root, got {other:?}"),
    }
}

fn pointer_names(pointers: &[ShapeNode]) -> Vec<&str> {
    pointers
        .iter()
        .map(|p| match p {
            ShapeNode::Scalar { name, .. }
            | ShapeNode::Object { name, .. }
            | ShapeNode::Array { name, .. }
            | ShapeNode::Enum { name, .. } => name.as_str(),
            other => panic!("unexpected pointer node {other:?}"),
        })
        .collect()
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn as_str(v: &DecodedValue) -> &str {
    match v {
        DecodedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

/// The members of a jsonb document, in order. A `<json>` cast renders the
/// row as `to_jsonb(ROW(...))`, so the keys are positional (`f1`, `f2`, …)
/// and it is the member *count* that says whether an id was added.
fn json_members(v: &DecodedValue) -> &[(String, DecodedValue)] {
    match v {
        DecodedValue::Object(members) => members,
        other => panic!("expected a jsonb object, got {other:?}"),
    }
}

/// `Person` behind a single link to `Company` and a multi-link from `Team`,
/// with one row of each — the smallest schema that exercises a root shape, a
/// nested link shape and both kinds of mutation body.
async fn fixture(prefix: &str) -> (pylon_pgcon::PgPool, SchemaDescriptor, String) {
    let module = unique_module(prefix);
    let company = ty("Company", &module, vec![id_prop(), text_prop("name")]);
    let mut person = ty("Person", &module, vec![id_prop(), text_prop("name")]);
    person.links = vec![link("employer", &format!("{module}::Company"))];
    let mut team = ty("Team", &module, vec![id_prop(), text_prop("name")]);
    team.multilinks = vec![multilink("members", &format!("{module}::Person"))];
    let sd = SchemaDescriptor {
        types: vec![company, person, team],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;
    exec(&pool, &sd, &format!("insert {module}::Company {{ name := 'Acme' }}")).await;
    exec(
        &pool,
        &sd,
        &format!(
            "insert {module}::Person {{ \
             name := 'Alice', \
             employer := (select {module}::Company filter .name = 'Acme' limit 1) \
         }}"
        ),
    )
    .await;
    (pool, sd, module)
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_shape_that_named_no_id_still_returns_one() {
    let (pool, sd, module) = fixture("live_implicit_id_root").await;

    let (rows, shape) = run(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    let pointers = root_pointers(&shape);
    assert_eq!(
        pointer_names(pointers),
        vec!["__type__", "id", "name"],
        "the id belongs in front of the pointers the query wrote"
    );
    assert!(
        matches!(&shape.root, ShapeNode::Object { has_implicit_id, .. } if *has_implicit_id),
        "the root must be marked, so JSON output can leave the id out again"
    );

    assert_eq!(rows.len(), 1);
    // A uuid arrives as its own DecodedValue; what matters is that the
    // column is really there and populated, not just named in the shape.
    assert!(
        matches!(field(&rows[0], 1), DecodedValue::Uuid(_)),
        "position 1 should hold the row's id, got {:?}",
        field(&rows[0], 1)
    );
    assert_eq!(as_str(field(&rows[0], 2)), "Alice");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_nested_link_shape_gets_its_own_id() {
    let (pool, sd, module) = fixture("live_implicit_id_nested").await;

    let (rows, shape) = run(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name, employer: {{ name }} }}"),
    )
    .await;
    let pointers = root_pointers(&shape);
    assert_eq!(pointer_names(pointers), vec!["__type__", "id", "name", "employer"]);

    let ShapeNode::Object {
        pointers: nested,
        has_implicit_id,
        ..
    } = &pointers[3]
    else {
        panic!("expected employer to be an object pointer, got {:?}", pointers[3])
    };
    assert_eq!(pointer_names(nested), vec!["__type__", "id", "name"]);
    assert!(*has_implicit_id);

    // `options: { value }` in jaldis is exactly this shape — the nested id
    // is the one whose absence silently produced a `None`.
    let employer = field(&rows[0], 3);
    assert!(
        matches!(field(employer, 1), DecodedValue::Uuid(_)),
        "the nested object carries its own id, got {:?}",
        field(employer, 1)
    );
    assert_eq!(as_str(field(employer, 2)), "Acme");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_id_the_query_wrote_itself_is_not_duplicated() {
    let (pool, sd, module) = fixture("live_implicit_id_explicit").await;

    for pyql in [
        format!("select {module}::Person {{ id, name }}"),
        // Written second, it stays second: the shape is the user's, and
        // nothing is inserted in front of it.
        format!("select {module}::Person {{ name, id }}"),
        // `*` already expands to every property, `id` among them.
        format!("select {module}::Person {{ * }}"),
    ] {
        let (_, shape) = run(&pool, &sd, &pyql).await;
        let pointers = root_pointers(&shape);
        let names = pointer_names(pointers);
        assert_eq!(
            names.iter().filter(|n| **n == "id").count(),
            1,
            "{pyql} produced {names:?}"
        );
        assert!(
            matches!(&shape.root, ShapeNode::Object { has_implicit_id, .. } if !*has_implicit_id),
            "{pyql}: an id the query asked for is not the implicit one"
        );
    }

    let (_, shape) = run(&pool, &sd, &format!("select {module}::Person {{ name, id }}")).await;
    assert_eq!(pointer_names(root_pointers(&shape)), vec!["__type__", "name", "id"]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_multi_link_assigned_a_shaped_set_still_writes_its_rows() {
    let (pool, sd, module) = fixture("live_implicit_id_mutation").await;

    // The junction rows read each value as one column, so an implicit `id`
    // beside the `name` this selects would make the value two columns wide.
    // This is the shape jaldis writes an assessment answer's options with.
    exec(
        &pool,
        &sd,
        &format!(
            "with picked := (select {module}::Person {{ name }} filter .name = 'Alice') \
             insert {module}::Team {{ name := 'Crew', members := picked }}"
        ),
    )
    .await;

    let (rows, _) = run(
        &pool,
        &sd,
        &format!("select {module}::Team {{ name, members: {{ name }} }}"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    let members = match field(&rows[0], 3) {
        DecodedValue::Array(items) => items,
        other => panic!("expected members to decode as an array, got {other:?}"),
    };
    assert_eq!(
        members.len(),
        1,
        "the junction row must actually have been written, not silently skipped"
    );
    assert_eq!(as_str(field(&members[0], 2)), "Alice");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_json_cast_carries_no_id_it_was_not_given() {
    let (pool, sd, module) = fixture("live_implicit_id_json").await;

    // `<json>` renders the value as text, so an added key would show up in
    // it. the upstream engine compiles its JSON output with implicit ids off for the same
    // reason, and `a CLI query` on the same shape prints `{"name": ...}` alone.
    let (rows, _) = run(
        &pool,
        &sd,
        &format!("select <json>(select {module}::Person {{ name }})"),
    )
    .await;
    assert_eq!(rows.len(), 1);
    let document = match &rows[0] {
        DecodedValue::Array(items) if items.len() == 1 => &items[0],
        other => panic!("expected one rendered document, got {other:?}"),
    };
    let members = json_members(document);
    assert_eq!(
        members.len(),
        2,
        "the type discriminator and `name` alone — an implicit id would be a third: {members:?}"
    );
    assert_eq!(as_str(&members[1].1), "Alice");

    // The same, nested: a json-cast pointer inside an ordinary shape. The
    // shape around it still gets its own id.
    let (rows, shape) = run(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name, blob := <json>(select {module}::Company {{ name }} limit 1) }}"),
    )
    .await;
    assert_eq!(
        pointer_names(root_pointers(&shape)),
        vec!["__type__", "id", "name", "blob"]
    );
    let blob = json_members(field(&rows[0], 3));
    assert_eq!(blob.len(), 2, "the cast's own document carries no id: {blob:?}");
    assert_eq!(as_str(&blob[1].1), "Acme");
}
