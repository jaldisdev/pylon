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

//! Live-Postgres tests for `GROUP ... BY` — single-key grouping, a
//! `USING alias := expr` computed key, composite (multi-key) grouping, and
//! the implicit `{ id }` shape when none is given. Scoped to what Pylon
//! supports: `GroupStmt` takes a flat list of `BY` keys, with no
//! `BY CUBE(...)`/`ROLLUP(...)`, no multi-set grouping sets, and no nested
//! `GROUP` subqueries — see `Compiler::compile_group`.
//!
//! No prior live test in this suite ever executed a `GROUP` query against
//! real data — `emit_group` (`sql/mod.rs`) has pure unit tests for the
//! generated SQL shape, but nothing before this file proved the emitted
//! `array_agg(ROW(...))`/grouping-array machinery actually round-trips
//! through the wire decoder correctly.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_group -- --ignored
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

fn bool_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "bool".into();
    p
}

/// `Employee` plus a `Team` whose `members` multi-link reaches it, so a group
/// can be written over a *walk* (`group t.members by .department`) rather than
/// over the type itself.
fn team_schema(module: &str) -> SchemaDescriptor {
    let mut sd = employee_schema(module);
    let mut team = ty("Team", module, vec![id_prop(), text_prop("name")]);
    team.multilinks = vec![multilink("members", &format!("{module}::Employee"))];
    sd.types.push(team);
    sd
}

fn employee_schema(module: &str) -> SchemaDescriptor {
    let employee = ty(
        "Employee",
        module,
        vec![
            id_prop(),
            text_prop("name"),
            text_prop("department"),
            int_prop("age"),
            bool_prop("active"),
        ],
    );
    SchemaDescriptor {
        types: vec![employee],
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

async fn group_rows(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
}

fn fields(row: &DecodedValue) -> &[DecodedValue] {
    match row {
        DecodedValue::Composite(fields) => fields,
        other => panic!("expected a Composite-shaped group row, got {other:?}"),
    }
}

fn as_str(v: &DecodedValue) -> &str {
    match v {
        DecodedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

fn as_array(v: &DecodedValue) -> &[DecodedValue] {
    match v {
        DecodedValue::Array(items) => items,
        other => panic!("expected Array, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn group_by_single_property_partitions_rows_into_correct_groups() {
    let module = unique_module("live_group_single");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 32, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 28, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Dave', department := 'sales', age := 40, active := true }}"),
    )
    .await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!("group {module}::Employee {{ name }} by .department"),
    )
    .await;
    assert_eq!(rows.len(), 2, "expected exactly 2 department groups, got {rows:?}");

    // key=1, grouping=2, elements=3 for a single-key group.
    let mut by_department: std::collections::HashMap<String, HashSet<String>> = std::collections::HashMap::new();
    for row in &rows {
        let f = fields(row);
        let department = as_str(&f[1]).to_string();
        let grouping = as_array(&f[2]);
        assert_eq!(grouping.len(), 1);
        assert_eq!(as_str(&grouping[0]), "department");

        let elements = as_array(&f[3]);
        let names: HashSet<String> = elements
            .iter()
            .map(|el| {
                let el_fields = fields(el);
                // element row: [type-tag, id, name]
                as_str(&el_fields[2]).to_string()
            })
            .collect();
        by_department.insert(department, names);
    }

    assert_eq!(
        by_department.get("eng").cloned(),
        Some(HashSet::from(["Alice".to_string(), "Bob".to_string()])),
    );
    assert_eq!(
        by_department.get("sales").cloned(),
        Some(HashSet::from(["Carol".to_string(), "Dave".to_string()])),
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn group_using_computed_alias_buckets_by_derived_value() {
    let module = unique_module("live_group_using");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 22, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 25, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 31, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Dave', department := 'sales', age := 39, active := true }}"),
    )
    .await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!("group {module}::Employee {{ name }} using decade := .age // 10 by decade"),
    )
    .await;
    assert_eq!(rows.len(), 2, "expected exactly 2 decade buckets, got {rows:?}");

    let mut by_decade: std::collections::HashMap<i64, HashSet<String>> = std::collections::HashMap::new();
    for row in &rows {
        let f = fields(row);
        let decade = match &f[1] {
            DecodedValue::I64(n) => *n,
            other => panic!("expected I64 decade key, got {other:?}"),
        };
        let elements = as_array(&f[3]);
        let names: HashSet<String> = elements
            .iter()
            .map(|el| {
                let el_fields = fields(el);
                as_str(&el_fields[2]).to_string()
            })
            .collect();
        by_decade.insert(decade, names);
    }

    assert_eq!(
        by_decade.get(&2).cloned(),
        Some(HashSet::from(["Alice".to_string(), "Bob".to_string()])),
    );
    assert_eq!(
        by_decade.get(&3).cloned(),
        Some(HashSet::from(["Carol".to_string(), "Dave".to_string()])),
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn group_by_multiple_keys_produces_composite_grouping() {
    let module = unique_module("live_group_composite");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 32, active := false }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 28, active := true }}"),
    )
    .await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!("group {module}::Employee {{ name }} by .department, .active"),
    )
    .await;
    // Every row has a distinct (department, active) pair, so 3 groups.
    assert_eq!(rows.len(), 3, "expected 3 composite groups, got {rows:?}");

    // key1=1, key2=2, grouping=3, elements=4 for a two-key group.
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for row in &rows {
        let f = fields(row);
        let department = as_str(&f[1]).to_string();
        let active = match &f[2] {
            DecodedValue::Bool(b) => *b,
            other => panic!("expected Bool active key, got {other:?}"),
        };
        let grouping = as_array(&f[3]);
        assert_eq!(grouping.len(), 2);
        assert_eq!(as_str(&grouping[0]), "department");
        assert_eq!(as_str(&grouping[1]), "active");

        let elements = as_array(&f[4]);
        assert_eq!(
            elements.len(),
            1,
            "each composite group should have exactly one row here"
        );
        seen.insert((department, active));
    }

    assert_eq!(
        seen,
        HashSet::from([
            ("eng".to_string(), true),
            ("eng".to_string(), false),
            ("sales".to_string(), true),
        ]),
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn group_with_no_explicit_shape_defaults_to_id_only() {
    let module = unique_module("live_group_noshape");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}"),
    )
    .await;

    let rows = group_rows(&pool, &sd, &format!("group {module}::Employee by .department")).await;
    assert_eq!(rows.len(), 1);
    let f = fields(&rows[0]);
    let elements = as_array(&f[3]);
    assert_eq!(elements.len(), 1);
    let el_fields = fields(&elements[0]);
    // Implicit shape is `{ id }` only: [type-tag, id] — no `name`/`department`/etc.
    assert_eq!(el_fields.len(), 2, "expected only [type-tag, id], got {el_fields:?}");
    assert!(
        matches!(&el_fields[1], DecodedValue::Uuid(_)),
        "expected id to be a Uuid, got {:?}",
        el_fields[1]
    );
}

/// A group whose subject is a walk rather than a type name. The steps used to
/// be joined with `::` and looked up as a type, so `group t.members by …`
/// reported an unknown type `t::members`. Grouping the landing type without
/// narrowing to the walked rows would be the other way to get this wrong —
/// hence two teams, and an assertion that only one team's members are counted.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn group_over_a_walk_covers_only_the_rows_the_walk_reaches() {
    let module = unique_module("live_group_walk");
    let sd = team_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    for (name, dept) in [("Ann", "eng"), ("Bo", "eng"), ("Cy", "ops"), ("Di", "legal")] {
        exec(
            &pool,
            &sd,
            &format!(
                "insert {module}::Employee {{ name := '{name}', department := '{dept}', age := 30, active := true }}"
            ),
        )
        .await;
    }
    exec(
        &pool,
        &sd,
        &format!(
            "insert {module}::Team {{ name := 'core', members := (select {module}::Employee filter .department = 'eng') }}"
        ),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!(
            "insert {module}::Team {{ name := 'other', members := (select {module}::Employee filter .department = 'legal') }}"
        ),
    )
    .await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!(
            "with t := (select {module}::Team filter .name = 'core' limit 1) \
             group t.members by .department"
        ),
    )
    .await;
    assert_eq!(
        rows.len(),
        1,
        "only the walked team's members should be grouped — 'ops' and 'legal' belong to no group here, got {rows:?}"
    );
}

/// `department` optional, so a row can fall into the empty-key group.
fn optional_department(mut sd: SchemaDescriptor) -> SchemaDescriptor {
    for prop in sd.types.iter_mut().flat_map(|t| t.properties.iter_mut()) {
        if prop.name == "department" {
            prop.nullable = true;
        }
    }
    sd
}

async fn seed_departments(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, module: &str) {
    for (name, dept, age) in [
        ("Alice", "eng", 30),
        ("Bob", "eng", 32),
        ("Carol", "sales", 28),
        ("Dave", "sales", 40),
    ] {
        exec(
            pool,
            sd,
            &format!(
                "insert {module}::Employee {{ name := '{name}', department := '{dept}', age := {age}, active := true }}"
            ),
        )
        .await;
    }
    // No department: the rows whose key is empty group together too.
    exec(
        pool,
        sd,
        &format!("insert {module}::Employee {{ name := 'Eve', age := 50, active := true }}"),
    )
    .await;
}

fn as_i64(v: &DecodedValue) -> i64 {
    match v {
        DecodedValue::I64(n) => *n,
        other => panic!("expected I64, got {other:?}"),
    }
}

/// Each row's `name`, read from an element row `[type-tag, id, name]`.
fn element_names(rows: &[DecodedValue]) -> HashSet<String> {
    rows.iter().map(|row| as_str(&fields(row)[2]).to_string()).collect()
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn shape_over_a_group_reads_its_key_and_aggregates_its_elements() {
    let module = unique_module("live_group_projection");
    let sd = optional_department(employee_schema(&module));
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;
    seed_departments(&pool, &sd, &module).await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!(
            "select (group {module}::Employee using d := .department by d) {{ \
               d := .key.d, n := count(.elements), oldest := max(.elements.age) \
             }} order by max(.elements.age) desc limit 2"
        ),
    )
    .await;
    // [type slot, d, n, oldest]; the empty-key group sorts first on age 50.
    let got: Vec<(Option<String>, i64, i64)> = rows
        .iter()
        .map(|row| {
            let f = fields(row);
            let key = match &f[1] {
                DecodedValue::Null => None,
                other => Some(as_str(other).to_string()),
            };
            (key, as_i64(&f[2]), as_i64(&f[3]))
        })
        .collect();
    assert_eq!(got, vec![(None, 1, 50), (Some("sales".to_string()), 2, 40)]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn for_over_a_group_takes_the_first_elements_of_each_key() {
    let module = unique_module("live_group_for");
    let sd = optional_department(employee_schema(&module));
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;
    seed_departments(&pool, &sd, &module).await;

    let oldest = group_rows(
        &pool,
        &sd,
        &format!(
            "with g := (group {module}::Employee by .department) \
             for x in g union (select x.elements order by .age desc limit 1) {{ name }}"
        ),
    )
    .await;
    assert_eq!(
        element_names(&oldest),
        HashSet::from(["Bob", "Dave", "Eve"].map(String::from))
    );

    let youngest = group_rows(
        &pool,
        &sd,
        &format!(
            "for x in (group {module}::Employee by .department) \
             union (select x.elements order by .age limit 1) {{ name }}"
        ),
    )
    .await;
    assert_eq!(
        element_names(&youngest),
        HashSet::from(["Alice", "Carol", "Eve"].map(String::from))
    );

    // Bound in a `with` itself, the loop is read back like any object binding.
    let bound = group_rows(
        &pool,
        &sd,
        &format!(
            "with youngest := (for x in (group {module}::Employee by .department) \
               union (select x.elements order by .age limit 1)) \
             select youngest {{ name }} filter .age < 50"
        ),
    )
    .await;
    assert_eq!(
        element_names(&bound),
        HashSet::from(["Alice", "Carol"].map(String::from))
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn free_object_field_holds_every_row_its_select_yields() {
    let module = unique_module("live_group_free_field");
    let sd = optional_department(employee_schema(&module));
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;
    seed_departments(&pool, &sd, &module).await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!(
            "select {{ \
               everyone := (select {module}::Employee {{ name }}), \
               first := (select {module}::Employee {{ name }} order by .age limit 1), \
               per_department := (select (group {module}::Employee by .department) {{ \
                 n := count(.elements) \
               }}) \
             }}"
        ),
    )
    .await;
    let [row] = rows.as_slice() else {
        panic!("expected one free object, got {rows:?}")
    };
    let f = fields(row);
    assert_eq!(element_names(as_array(&f[0])).len(), 5);
    assert_eq!(as_str(&fields(&f[1])[2]), "Carol");
    let mut counts: Vec<i64> = as_array(&f[2]).iter().map(|g| as_i64(&fields(g)[1])).collect();
    counts.sort();
    assert_eq!(counts, vec![1, 2, 2]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn asserted_select_over_a_binding_keeps_the_shape_written_after_it() {
    let module = unique_module("live_group_assert_binding");
    let sd = optional_department(team_schema(&module));
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;
    seed_departments(&pool, &sd, &module).await;
    exec(&pool, &sd, &format!("insert {module}::Team {{ name := 'core' }}")).await;

    let rows = group_rows(
        &pool,
        &sd,
        &format!(
            "with engineers := (select {module}::Employee filter .department = 'eng') \
             select {module}::Team {{ members := (select assert_exists(engineers)) {{ name }} }} \
             filter .name = 'core'"
        ),
    )
    .await;
    let [team] = rows.as_slice() else {
        panic!("expected one team, got {rows:?}")
    };
    assert_eq!(
        element_names(as_array(&fields(team)[2])),
        HashSet::from(["Alice", "Bob"].map(String::from))
    );
}
