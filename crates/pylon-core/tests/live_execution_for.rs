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

//! Live-Postgres tests for `FOR x IN {...} UNION (...)` — bulk insert (the
//! `emit_for_insert` VALUES-CTE path), a `SELECT` body (the
//! `CROSS JOIN LATERAL` path), the for-variable composing inside a body
//! expression (not just a bare assignment), the empty-iterator no-op
//! case, and an `UPDATE` body appending the row it iterates (the
//! `emit_for_update` path, which drives both the update and its junction
//! rows from the iteration — a LATERAL cannot hold DML). A `DELETE` body is
//! still refused, so this file doesn't exercise one.
//!
//! No prior live test in this suite ever executed a `FOR` query against
//! real data, and there were no pure unit tests for `FOR`'s SQL emission at
//! all before this session — this is the first coverage of either kind.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_for -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::DecodedValue;
use std::collections::HashSet;

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

fn int_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "int8".into();
    p
}

fn person_schema(module: &str) -> SchemaDescriptor {
    let person = ty("Person", module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    SchemaDescriptor {
        types: vec![person],
        ..Default::default()
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

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
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

fn as_i64(v: &DecodedValue) -> i64 {
    match v {
        DecodedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_loop_bulk_inserts_one_row_per_iterator_value() {
    let module = unique_module("live_for_bulk_insert");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("for n in {{'Alice', 'Bob', 'Carol'}} union (insert {module}::Person {{ name := n, age := 0 }})"),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name }} order by .name"),
    )
    .await;
    assert_eq!(rows.len(), 3, "expected one row per iterator value, got {rows:?}");
    let names: Vec<&str> = rows.iter().map(|r| as_str(field(r, 1))).collect();
    assert_eq!(names, vec!["Alice", "Bob", "Carol"]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_loop_variable_composes_inside_insert_body_expression() {
    let module = unique_module("live_for_compose");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // The for-variable isn't just bare-assigned — it's used inside an
    // arithmetic expression (`n * 10`), proving `IrExpr::ForVar` resolves
    // correctly deep inside a compiled body expression, not only at the
    // top level of a shape assignment.
    exec(
        &pool,
        &sd,
        &format!("for n in {{1, 2, 3}} union (insert {module}::Person {{ name := 'p', age := n * 10 }})"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ age }} order by .age")).await;
    let ages: Vec<i64> = rows.iter().map(|r| as_i64(field(r, 1))).collect();
    assert_eq!(ages, vec![10, 20, 30]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_loop_select_body_cross_joins_lateral_per_iterator_value() {
    let module = unique_module("live_for_select");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Alice', age := 30 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Bob', age := 65 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Person {{ name := 'Carol', age := 40 }}"),
    )
    .await;

    // A SELECT body run once per iterator value, unioning the matches —
    // the `CROSS JOIN LATERAL` path in `emit_for_stmt`, distinct from the
    // bulk-insert path exercised above.
    let rows = rows_of(
        &pool,
        &sd,
        &format!("for age in {{30, 65}} union (select {module}::Person {{ name }} filter .age = age)"),
    )
    .await;
    let names: HashSet<&str> = rows.iter().map(|r| as_str(field(r, 1))).collect();
    assert_eq!(names, HashSet::from(["Alice", "Bob"]), "got {rows:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_loop_with_empty_iterator_set_is_a_no_op() {
    let module = unique_module("live_for_empty");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("for n in {{}} union (insert {module}::Person {{ name := n, age := 0 }})"),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person")).await;
    assert_eq!(rows.len(), 0, "an empty iterator set must insert nothing, got {rows:?}");
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_loop_appends_only_the_row_it_is_iterating() {
    // The junction rows have to be driven from the iteration. Compiled from
    // an uncorrelated value subquery instead, `friends += p` reads the whole
    // table and links every Person to every other one.
    let module = unique_module("live_for_ml_append");
    let mut person = ty("Person", &module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    person.multilinks = vec![multilink("friends", &format!("{module}::Person"))];
    let sd = SchemaDescriptor {
        types: vec![person],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for name in ["ana", "bo", "cy"] {
        exec(
            &pool,
            &sd,
            &format!("insert {module}::Person {{ name := '{name}', age := 1 }}"),
        )
        .await;
    }

    exec(
        &pool,
        &sd,
        &format!(
            "with others := (select {module}::Person filter .name != 'ana') \
             for p in others union (update {module}::Person filter .name = 'ana' set {{ friends += p }})"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name, friends: {{ name }} }} order by .name"),
    )
    .await;
    let friend_counts: Vec<(String, usize)> = rows
        .iter()
        .map(|row| {
            let name = as_str(field(row, 1)).to_string();
            let friends = match field(row, 2) {
                DecodedValue::Array(items) => items.len(),
                DecodedValue::Null => 0,
                other => panic!("expected an array of friends, got {other:?}"),
            };
            (name, friends)
        })
        .collect();
    assert_eq!(
        friend_counts,
        vec![("ana".into(), 2), ("bo".into(), 0), ("cy".into(), 0)],
        "only the iterated rows are linked, and only onto the updated row"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn nested_for_loops_insert_once_per_pair() {
    // One iterator CTE per loop, the inner carrying the outer's key. Emitted as
    // one flat iteration instead, the inner walk loses which outer row it
    // belongs to and every pair collapses together.
    let module = unique_module("live_for_nested");
    let mut person = ty("Person", &module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    person.multilinks = vec![multilink("friends", &format!("{module}::Person"))];
    let post = ty("Post", &module, vec![id_prop(), text_prop("title")]);
    let sd = SchemaDescriptor {
        types: vec![person, post],
        ..Default::default()
    };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for name in ["ana", "bo", "cy", "di"] {
        exec(
            &pool,
            &sd,
            &format!("insert {module}::Person {{ name := '{name}', age := 1 }}"),
        )
        .await;
    }
    // ana befriends bo and cy; di befriends cy alone.
    for (who, friends) in [("ana", vec!["bo", "cy"]), ("di", vec!["cy"])] {
        for friend in friends {
            exec(
                &pool,
                &sd,
                &format!(
                    "update {module}::Person filter .name = '{who}' set {{ friends += \
                     (select detached {module}::Person filter .name = '{friend}') }}"
                ),
            )
            .await;
        }
    }

    exec(
        &pool,
        &sd,
        &format!(
            "with pairs := (for p in (select {module}::Person) union ( \
               for f in p.friends union ( \
                 insert {module}::Post {{ title := p.name ++ '->' ++ f.name }} \
               ) \
             )) select pairs"
        ),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Post {{ title }} order by .title")).await;
    let titles: Vec<&str> = rows.iter().map(|row| as_str(field(row, 1))).collect();
    assert_eq!(
        titles,
        vec!["ana->bo", "ana->cy", "di->cy"],
        "one row per (outer, inner) pair, each carrying both loop variables"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn set_operations_run_over_their_elements() {
    // Read as the array it stands for, `count(a intersect b)` counts the array
    // — one, whatever the operation yielded.
    let module = unique_module("live_set_ops");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for (pyql, expected) in [
        ("select count((array_unpack(['x','y','z']) intersect array_unpack(['y','z','w'])))", 2),
        ("select count((array_unpack(['x']) intersect array_unpack(['y'])))", 0),
        ("select count((array_unpack(['x','y','z']) except array_unpack(['y'])))", 2),
    ] {
        let rows = rows_of(&pool, &sd, pyql).await;
        assert_eq!(as_i64(field(&rows[0], 0)), expected, "{pyql}");
    }

    for (pyql, expected) in [
        ("select exists(array_unpack(['x','y']) intersect array_unpack(['y']))", true),
        ("select exists(array_unpack(['x']) intersect array_unpack(['y']))", false),
    ] {
        let rows = rows_of(&pool, &sd, pyql).await;
        let got = matches!(field(&rows[0], 0), DecodedValue::Bool(b) if *b);
        assert_eq!(got, expected, "{pyql}");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn updating_the_loop_variable_touches_only_the_iterated_rows() {
    let module = unique_module("live_for_upd");
    let sd = person_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for (name, age) in [("Ann", 10), ("Bo", 20), ("Cy", 30)] {
        exec(
            &pool,
            &sd,
            &format!("insert {module}::Person {{ name := '{name}', age := {age} }}"),
        )
        .await;
    }

    exec(
        &pool,
        &sd,
        &format!(
            "with young := (select {module}::Person filter .age < 15) \
             for p in young union (update p set {{ name := 'TOUCHED' }})"
        ),
    )
    .await;

    let rows = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }} order by .name")).await;
    let names: Vec<String> = rows.iter().map(|r| as_str(field(r, 1)).to_string()).collect();
    assert_eq!(
        names,
        vec!["Bo".to_string(), "Cy".to_string(), "TOUCHED".to_string()],
        "only the iterated row should change"
    );
}

/// A `for` body that updates *and* inserts into a multi-link: the insert runs
/// once per iteration and the junction has to pair each row with the one that
/// iteration inserted. Two people with different names make a mis-pairing
/// visible — swapping them would still produce two rows and two links.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_nested_insert_in_a_for_body_pairs_with_its_own_iteration() {
    let module = unique_module("live_for_nested");
    let mut sd = person_schema(&module);
    let mut note = ty("Note", &module, vec![id_prop(), text_prop("body")]);
    note.multilinks = vec![];
    sd.types.push(note);
    let person = sd.types.iter_mut().find(|t| t.name == "Person").expect("Person");
    person.multilinks = vec![multilink("notes", &format!("{module}::Note"))];
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for name in ["Ann", "Bo"] {
        exec(
            &pool,
            &sd,
            &format!("insert {module}::Person {{ name := '{name}', age := 30 }}"),
        )
        .await;
    }

    exec(
        &pool,
        &sd,
        &format!(
            "with ps := (select {module}::Person) \
             for p in ps union (update p set {{ notes += (insert {module}::Note {{ body := p.name }}) }})"
        ),
    )
    .await;

    let rows = rows_of(
        &pool,
        &sd,
        &format!("select {module}::Person {{ name, n := .notes {{ body }} }} order by .name"),
    )
    .await;
    assert_eq!(rows.len(), 2, "both people should come back, got {rows:?}");
    for row in &rows {
        let name = as_str(field(row, 1)).to_string();
        let notes = match field(row, 2) {
            pylon_value::DecodedValue::Array(items) => items.clone(),
            other => panic!("expected an array of notes, got {other:?}"),
        };
        assert_eq!(notes.len(), 1, "{name} should have exactly one note, got {notes:?}");
        assert_eq!(
            as_str(field(&notes[0], 1)),
            name,
            "each note should belong to the person whose name it carries"
        );
    }
}
