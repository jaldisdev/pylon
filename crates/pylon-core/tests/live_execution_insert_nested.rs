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

//! Live-Postgres tests for `INSERT` shapes not covered elsewhere:
//! `select (insert ...) { ... }` chaining (reading the newly inserted row's
//! own fields back out, not just its `id`), and a multilink assigned
//! directly at insert time (`members := {...}`) rather than appended after
//! the fact via `+=`. Concepts inspired by Gel's own
//! `test_edgeql_insert.py`, not ported literally — `+=`/`-=` multi-link
//! mutation and link-property (`Through[...]`) round-tripping are already
//! covered in depth by `live_execution_linkprops.rs` and
//! `live_execution_backlinks.rs`; this file is scoped to the two shapes
//! above, neither of which had any live coverage before this file.
//!
//! Deliberately NOT covered: nested DML as a link's value
//! (`author := (insert Person {...})`, or the `select (insert ...) { id }`
//! workaround its rejection error recommends). Postgres has no way to run a
//! nested `INSERT` inside another statement's value list without hoisting
//! it into a `WITH` CTE first, and nothing in the IR does that hoisting
//! today (`IrInsert` has no CTE-hoisting field — see
//! `Compiler::compile_link_subquery`'s own doc comment). This is a real,
//! deliberately-untouched gap, not an oversight.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_insert_nested -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;
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
        properties,
        links: vec![],
        multilinks: vec![],
        computed: vec![],
        constraints: vec![],
        indexes: vec![],
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

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap()
}

fn field(row: &CachedValue, i: usize) -> &CachedValue {
    match row {
        CachedValue::Composite(fields) => fields.get(i).unwrap_or(&CachedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

fn as_str(v: &CachedValue) -> &str {
    match v {
        CachedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

fn as_i64(v: &CachedValue) -> i64 {
    match v {
        CachedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
    }
}

fn as_array(v: &CachedValue) -> &[CachedValue] {
    match v {
        CachedValue::Array(items) => items,
        other => panic!("expected Array, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn select_insert_shape_chaining_reads_the_newly_inserted_rows_fields() {
    let module = unique_module("live_insert_select_chain");
    let person = ty("Person", &module, vec![id_prop(), text_prop("name"), int_prop("age")]);
    let sd = SchemaDescriptor { types: vec![person], ..Default::default() };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    // Not just RETURNING the id — the outer shape reads back real fields
    // of the row the inner insert just created.
    let rows = rows_of(&pool, &sd, &format!(
        "select (insert {module}::Person {{ name := 'Bob', age := 25 }}) {{ name, age }}"
    )).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(as_str(field(&rows[0], 1)), "Bob");
    assert_eq!(as_i64(field(&rows[0], 2)), 25);

    // And the row is durably there afterward, not just visible transiently
    // in the chained shape.
    let after = rows_of(&pool, &sd, &format!("select {module}::Person {{ name }}")).await;
    assert_eq!(after.len(), 1);
    assert_eq!(as_str(field(&after[0], 1)), "Bob");
}

#[tokio::test]
#[ignore]
async fn insert_assigns_a_multilink_directly_not_via_append() {
    let module = unique_module("live_insert_multilink_assign");
    let person = ty("Person", &module, vec![id_prop(), text_prop("name")]);
    let mut team = ty("Team", &module, vec![id_prop(), text_prop("name")]);
    team.multilinks = vec![multilink("members", &format!("{module}::Person"))];
    let sd = SchemaDescriptor { types: vec![person, team], ..Default::default() };
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Alice' }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Bob' }}")).await;

    // `:=` at insert time (not `+=` on an already-existing row) — a
    // different code path in `Compiler::compile_insert`'s multilink
    // handling than the append/remove path `live_execution_linkprops.rs`
    // already covers.
    exec(&pool, &sd, &format!(
        "insert {module}::Team {{ \
             name := 'Alpha', \
             members := {module}::Person \
         }}"
    )).await;

    let teams = rows_of(&pool, &sd, &format!("select {module}::Team {{ members: {{ name }} }}")).await;
    assert_eq!(teams.len(), 1);
    let members = as_array(field(&teams[0], 1));
    let names: HashSet<String> = members.iter().map(|m| as_str(field(m, 1)).to_string()).collect();
    assert_eq!(names, HashSet::from(["Alice".to_string(), "Bob".to_string()]));
}
