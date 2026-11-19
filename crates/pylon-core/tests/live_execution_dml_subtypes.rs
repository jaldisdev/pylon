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

//! Live-Postgres tests for a shape selected over a write to a concrete type
//! with a concrete subtype — `A` and `B(A)`, each in a table of its own with
//! no discriminator column. A plain select of `A` reads both tables through a
//! union that supplies each row's `__type__`; a write's own rows have to say
//! which type they are some other way.
//!
//! Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test --release -p pylon-db-core --test live_execution_dml_subtypes -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
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

/// `A { name, tag: Tag?, notes: multi Note }` and `B(A)`, plus `Tag` with a
/// subtype `SubTag(Tag)` of its own, so a nested insert of a `Tag` is a write
/// to a type with subtypes too.
fn schema(m: &str) -> SchemaDescriptor {
    let mut a = ty("A", m, vec![id_prop(), text_prop("name")]);
    let mut tag = link("tag", &format!("{m}::Tag"));
    tag.nullable = true;
    a.links = vec![tag];
    let mut notes = multilink("notes", &format!("{m}::Note"));
    notes.nullable = true;
    a.multilinks = vec![notes];
    let mut b = a.clone();
    b.name = "B".into();
    b.table = "B".into();
    b.bases = vec![format!("{m}::A")];

    let tag = ty("Tag", m, vec![id_prop(), text_prop("label")]);
    let mut sub_tag = tag.clone();
    sub_tag.name = "SubTag".into();
    sub_tag.table = "SubTag".into();
    sub_tag.bases = vec![format!("{m}::Tag")];

    SchemaDescriptor {
        types: vec![a, b, tag, sub_tag, ty("Note", m, vec![id_prop(), text_prop("title")])],
        ..Default::default()
    }
}

async fn setup(prefix: &str) -> (pylon_pgcon::PgPool, SchemaDescriptor, String) {
    let m = unique_module(prefix);
    let sd = schema(&m);
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();
    (pool, sd, m)
}

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap_or_else(|e| panic!("{pyql}: {e}"));
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap_or_else(|e| panic!("{pyql}\n{}\n{e}", compiled.sql))
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn s(v: &DecodedValue) -> String {
    match v {
        DecodedValue::Str(s) => s.clone(),
        other => panic!("expected Str, got {other:?}"),
    }
}

/// `[type, id, …]` rows of a shape, as `type name` per row plus the
/// fields from position 2 on, rendered as strings.
fn typed(rows: &[DecodedValue]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let DecodedValue::Composite(fields) = row else {
                panic!("expected a Composite-shaped row, got {row:?}")
            };
            std::iter::once(s(&fields[0]))
                .chain(fields[2..].iter().map(render))
                .collect()
        })
        .collect()
}

fn render(v: &DecodedValue) -> String {
    match v {
        DecodedValue::Str(s) => s.clone(),
        DecodedValue::Composite(_) => format!("{:?}", typed(std::slice::from_ref(v))[0]),
        DecodedValue::Array(items) => format!("{:?}", typed(items)),
        DecodedValue::Null => "{}".into(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_shape_over_an_insert_names_the_inserted_type() {
    let (pool, sd, m) = setup("live_dmlsub_insert").await;
    for t in ["A", "B"] {
        let q = format!("{m}::{t}");
        let rows = rows_of(
            &pool,
            &sd,
            &format!("select (insert {q} {{ name := 'x' }}) {{ name, t := .__type__ }} limit 1"),
        )
        .await;
        assert_eq!(
            typed(&rows),
            vec![vec![q.clone(), "x".into(), q.clone()]],
            "{t}, wrapped"
        );

        let rows = rows_of(
            &pool,
            &sd,
            &format!("with p := (insert {q} {{ name := 'y' }}) select p {{ name, __type__ }}"),
        )
        .await;
        assert_eq!(typed(&rows), vec![vec![q.clone(), "y".into(), q.clone()]], "{t}, bound");

        let rows = rows_of(
            &pool,
            &sd,
            &format!("with p := (insert {q} {{ name := 'z' }}) select p.__type__"),
        )
        .await;
        assert_eq!(s(field(&rows[0], 0)), q, "{t}, path");

        let rows = rows_of(&pool, &sd, &format!("select (insert {q} {{ name := 's' }}) {{ * }}")).await;
        assert_eq!(s(field(&rows[0], 0)), q, "{t}, splat");

        let rows = rows_of(
            &pool,
            &sd,
            &format!("select (for n in {{'f1', 'f2'}} union (insert {q} {{ name := n }})) {{ name }} order by .name"),
        )
        .await;
        assert_eq!(
            typed(&rows),
            vec![vec![q.clone(), "f1".into()], vec![q.clone(), "f2".into()]],
            "{t}, for"
        );
    }

    // The union a plain select reads still says which type each row is.
    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {m}::A {{ name }} filter .name = 'x' order by .__type__"),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![vec![format!("{m}::A"), "x".into()], vec![format!("{m}::B"), "x".into()]]
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_shape_over_an_update_or_delete_names_each_rows_type() {
    let (pool, sd, m) = setup("live_dmlsub_upd").await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::A {{ name := 'a' }}) {{ name }}"),
    )
    .await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::B {{ name := 'b' }}) {{ name }}"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select (update {m}::A set {{ name := .name ++ '2' }}) {{ name, t := .__type__ }} order by .name"),
    )
    .await;
    let (a, b) = (format!("{m}::A"), format!("{m}::B"));
    assert_eq!(
        typed(&rows),
        vec![
            vec![a.clone(), "a2".into(), a.clone()],
            vec![b.clone(), "b2".into(), b.clone()]
        ]
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!("with gone := (delete {m}::A) select gone {{ name }} order by .name"),
    )
    .await;
    assert_eq!(typed(&rows), vec![vec![a, "a2".into()], vec![b, "b2".into()]]);
}

/// A nested insert in an update writes every table of a type with subtypes,
/// as the same update without a shape around it does.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_update_with_a_nested_insert_reaches_the_subtypes_rows() {
    let (pool, sd, m) = setup("live_dmlsub_nested").await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::A {{ name := 'a' }}) {{ name }}"),
    )
    .await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::B {{ name := 'b' }}) {{ name }}"),
    )
    .await;
    let (a, b, tag) = (format!("{m}::A"), format!("{m}::B"), format!("{m}::Tag"));
    let tagged = format!("{:?}", vec![tag.clone(), "fresh".into(), tag.clone()]);

    let rows = rows_of(
        &pool,
        &sd,
        &format!(
            "select (update {m}::A set {{ tag := (insert {m}::Tag {{ label := 'fresh' }}) }}) \
             {{ name, tag: {{ label, __type__ }} }} order by .name"
        ),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![
            vec![a.clone(), "a".into(), tagged.clone()],
            vec![b.clone(), "b".into(), tagged.clone()]
        ]
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {m}::A {{ name, tag: {{ label, __type__ }} }} order by .name"),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![vec![a, "a".into(), tagged.clone()], vec![b, "b".into(), tagged]],
        "both tables written"
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!(
            "select (insert {m}::B {{ name := 'c', tag := (insert {m}::Tag {{ label := 'own' }}) }}) \
             {{ tag: {{ label, t := .__type__ }} }}"
        ),
    )
    .await;
    assert_eq!(
        render(field(&rows[0], 2)),
        format!("{:?}", vec![tag.clone(), "own".into(), tag])
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_update_of_a_multilink_reaches_the_subtypes_rows() {
    let (pool, sd, m) = setup("live_dmlsub_multi").await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::A {{ name := 'a' }}) {{ name }}"),
    )
    .await;
    rows_of(
        &pool,
        &sd,
        &format!("select (insert {m}::B {{ name := 'b' }}) {{ name }}"),
    )
    .await;
    let (a, b, note) = (format!("{m}::A"), format!("{m}::B"), format!("{m}::Note"));
    let noted = format!("{:?}", vec![vec![note.clone(), "n".to_string()]]);

    let rows = rows_of(
        &pool,
        &sd,
        &format!(
            "select (update {m}::A set {{ notes += (insert {m}::Note {{ title := 'n' }}) }}) \
             {{ name, notes: {{ title }} }} order by .name"
        ),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![
            vec![a.clone(), "a".into(), noted.clone()],
            vec![b.clone(), "b".into(), noted.clone()]
        ]
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {m}::A {{ name, notes: {{ title }} }} order by .name"),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![
            vec![a.clone(), "a".into(), noted.clone()],
            vec![b, "b".into(), noted.clone()]
        ],
        "both junctions written"
    );

    let rows = rows_of(
        &pool,
        &sd,
        &format!(
            "select (insert {m}::A {{ name := 'c', notes := (insert {m}::Note {{ title := 'n' }}) }}) \
             {{ name, notes: {{ title }} }}"
        ),
    )
    .await;
    assert_eq!(
        typed(&rows),
        vec![vec![a, "c".into(), noted]],
        "an insert's own junction rows"
    );
}
