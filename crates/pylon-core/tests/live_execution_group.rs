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
//! the implicit `{ id }` shape when none is given. Concepts inspired by
//! Gel's own `test_edgeql_group.py`, not ported literally (Gel's grouping
//! supports `BY CUBE(...)`/`ROLLUP(...)`/multi-set grouping sets and nested
//! `GROUP` subqueries; Pylon's `GroupStmt` only supports a flat list of
//! `BY` keys — see `Compiler::compile_group`).
//!
//! No prior live test in this suite ever executed a `GROUP` query against
//! real data — `emit_group` (`sql/mod.rs`) has pure unit tests for the
//! generated SQL shape, but nothing before this file proved the emitted
//! `array_agg(ROW(...))`/grouping-array machinery actually round-trips
//! through the wire decoder correctly.
//!
//! Gated behind `#[ignore]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5418/pylon_migration_test \
//!     cargo test -p pylon-core --test live_execution_group -- --ignored
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

fn bool_prop(name: &str) -> PropertyDescriptor {
    let mut p = text_prop(name);
    p.pg_type = "bool".into();
    p
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
    SchemaDescriptor { types: vec![employee], ..Default::default() }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool.batch_execute(&export_schema(sd).unwrap()).await.unwrap();
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn group_rows(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap()
}

fn fields(row: &CachedValue) -> &[CachedValue] {
    match row {
        CachedValue::Composite(fields) => fields,
        other => panic!("expected a Composite-shaped group row, got {other:?}"),
    }
}

fn as_str(v: &CachedValue) -> &str {
    match v {
        CachedValue::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
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
async fn group_by_single_property_partitions_rows_into_correct_groups() {
    let module = unique_module("live_group_single");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 32, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 28, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Dave', department := 'sales', age := 40, active := true }}")).await;

    let rows = group_rows(&pool, &sd, &format!("group {module}::Employee {{ name }} by .department")).await;
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
        let names: HashSet<String> = elements.iter().map(|el| {
            let el_fields = fields(el);
            // element row: [type-tag, name]
            as_str(&el_fields[1]).to_string()
        }).collect();
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
#[ignore]
async fn group_using_computed_alias_buckets_by_derived_value() {
    let module = unique_module("live_group_using");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 22, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 25, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 31, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Dave', department := 'sales', age := 39, active := true }}")).await;

    let rows = group_rows(
        &pool, &sd,
        &format!("group {module}::Employee {{ name }} using decade := .age // 10 by decade"),
    ).await;
    assert_eq!(rows.len(), 2, "expected exactly 2 decade buckets, got {rows:?}");

    let mut by_decade: std::collections::HashMap<i64, HashSet<String>> = std::collections::HashMap::new();
    for row in &rows {
        let f = fields(row);
        let decade = match &f[1] {
            CachedValue::I64(n) => *n,
            other => panic!("expected I64 decade key, got {other:?}"),
        };
        let elements = as_array(&f[3]);
        let names: HashSet<String> = elements.iter().map(|el| {
            let el_fields = fields(el);
            as_str(&el_fields[1]).to_string()
        }).collect();
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
#[ignore]
async fn group_by_multiple_keys_produces_composite_grouping() {
    let module = unique_module("live_group_composite");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Bob', department := 'eng', age := 32, active := false }}")).await;
    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Carol', department := 'sales', age := 28, active := true }}")).await;

    let rows = group_rows(
        &pool, &sd,
        &format!("group {module}::Employee {{ name }} by .department, .active"),
    ).await;
    // Every row has a distinct (department, active) pair, so 3 groups.
    assert_eq!(rows.len(), 3, "expected 3 composite groups, got {rows:?}");

    // key1=1, key2=2, grouping=3, elements=4 for a two-key group.
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for row in &rows {
        let f = fields(row);
        let department = as_str(&f[1]).to_string();
        let active = match &f[2] {
            CachedValue::Bool(b) => *b,
            other => panic!("expected Bool active key, got {other:?}"),
        };
        let grouping = as_array(&f[3]);
        assert_eq!(grouping.len(), 2);
        assert_eq!(as_str(&grouping[0]), "department");
        assert_eq!(as_str(&grouping[1]), "active");

        let elements = as_array(&f[4]);
        assert_eq!(elements.len(), 1, "each composite group should have exactly one row here");
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
#[ignore]
async fn group_with_no_explicit_shape_defaults_to_id_only() {
    let module = unique_module("live_group_noshape");
    let sd = employee_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(&pool, &sd, &format!("insert {module}::Employee {{ name := 'Alice', department := 'eng', age := 30, active := true }}")).await;

    let rows = group_rows(&pool, &sd, &format!("group {module}::Employee by .department")).await;
    assert_eq!(rows.len(), 1);
    let f = fields(&rows[0]);
    let elements = as_array(&f[3]);
    assert_eq!(elements.len(), 1);
    let el_fields = fields(&elements[0]);
    // Implicit shape is `{ id }` only: [type-tag, id] — no `name`/`department`/etc.
    assert_eq!(el_fields.len(), 2, "expected only [type-tag, id], got {el_fields:?}");
    assert!(matches!(&el_fields[1], CachedValue::Uuid(_)), "expected id to be a Uuid, got {:?}", el_fields[1]);
}
