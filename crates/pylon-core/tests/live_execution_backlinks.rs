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

//! Backlink traversal (`.<link_name[is Type]`) live-execution tests. See
//! `live_execution_smoke.rs` for the harness's purpose and how to run these
//! (same pattern, this binary is `--test live_execution_backlinks`).
//!
//! Backlink traversal had **zero** prior test coverage anywhere in the
//! codebase. Investigating what to test here surfaced that Pylon's backlink
//! support was considerably narrower than initially assumed — both gaps
//! found are now fixed:
//!
//! 1. `compile_backlink_as_exists` (`ir/compiler.rs`) only checked
//!    `target_td.links`, never `target_td.multilinks`, so
//!    `filter exists .<multilink_name[is Type]` (a self-referential
//!    multilink like `Person.friends`) failed to compile even though the
//!    equivalent single-link case worked fine. Now resolves a
//!    multilink-sourced backlink via a nested junction-table `EXISTS`,
//!    mirroring how a forward multilink's own existence check already works
//!    (`multilink_correlation_select`).
//! 2. A backlink could only be used inside a `filter exists .<...>` /
//!    single-comparison context — not to select/project actual backlinked
//!    objects as a computed pointer's value (`teams := .<org[is Team] { ... }`),
//!    which is what a "nested backlink" query actually needs. Fixed by
//!    teaching `compile_shape_element`'s computed-pointer dispatch to
//!    recognize a backlink-rooted path and route it through a new
//!    `compile_backlink_pointer`, mirroring `compile_multilink_pointer`'s
//!    correlated-subquery construction for forward multilinks — new
//!    `IrMultiLinkJoin::BacklinkFk`/`BacklinkJunction` variants describe the
//!    (reversed) correlation, reusing the existing `IrShapePointer::MultiLink`
//!    array-of-objects emission wholesale.
//!
//! Still out of scope: a backlink followed by further path traversal
//! without a shape (`.<org[is Team].name`, a scalar set rather than an
//! object array) and a backlink as an entire top-level `select` subject
//! (`select .<org[is Team] { name }`) — neither is what "nested backlinks
//! in a shape" needs, and both are separably small if ever requested.

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{SchemaDescriptor, TypeDescriptor};

fn ty(
    name: &str,
    module: &str,
    properties: Vec<pylon_core::schema::PropertyDescriptor>,
    links: Vec<pylon_core::schema::LinkDescriptor>,
    multilinks: Vec<pylon_core::schema::MultiLinkDescriptor>,
) -> TypeDescriptor {
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
        links,
        multilinks,
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

/// `Org <- Team <- Member <- Task` (three chained forward links) plus a
/// self-referential `Person.friends` multilink — enough to exercise a
/// 3-level nested backlink shape and both backlink sources (single link,
/// multilink) in both a filter/exists context and a shape-position context.
fn backlinks_schema(module: &str) -> SchemaDescriptor {
    let org_q = format!("{module}::Org");
    let team_q = format!("{module}::Team");
    let member_q = format!("{module}::Member");
    let person_q = format!("{module}::Person");
    SchemaDescriptor {
        types: vec![
            ty("Org", module, vec![id_prop(), text_prop("name")], vec![], vec![]),
            ty(
                "Team",
                module,
                vec![id_prop(), text_prop("name")],
                vec![link("org", &org_q)],
                vec![],
            ),
            ty(
                "Member",
                module,
                vec![id_prop(), text_prop("name")],
                vec![link("team", &team_q)],
                vec![],
            ),
            ty(
                "Task",
                module,
                vec![id_prop(), text_prop("title")],
                vec![link("assignee", &member_q)],
                vec![],
            ),
            ty(
                "Person",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![multilink("friends", &person_q)],
            ),
        ],
        ..Default::default()
    }
}

async fn setup() -> (String, SchemaDescriptor, pylon_pgcon::PgPool) {
    let module = unique_module("live_backlinks");
    let schema = backlinks_schema(&module);
    let ddl = export_schema(&schema).unwrap();
    let pool = test_pool().await;
    pool.batch_execute(&ddl).await.unwrap();
    (module, schema, pool)
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn single_link_sourced_backlink_exists_filter_returns_the_right_orgs() {
    let (module, schema, pool) = setup().await;

    for stmt in [
        format!("insert {module}::Org {{ name := 'HasTeam' }}"),
        format!("insert {module}::Org {{ name := 'NoTeam' }}"),
        format!("insert {module}::Team {{ name := 'Alpha', org := (select {module}::Org filter .name = 'HasTeam') }}"),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }

    let select = query::compile(
        &format!("select {module}::Org {{ name }} filter exists .<org[is {module}::Team]"),
        &schema,
    )
    .unwrap();

    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly the 1 Org with a Team backlinked, got {rows:?}"
    );
    let pylon_value::DecodedValue::Composite(fields) = &rows[0] else {
        panic!("expected a Composite-shaped Org row, got {:?}", rows[0]);
    };
    assert_eq!(
        fields.get(1),
        Some(&pylon_value::DecodedValue::Str("HasTeam".to_string()))
    );
}

/// Regression test for gap 1 (see file doc comment) — a self-referential
/// multilink backlink now compiles and, more importantly, actually returns
/// the right rows once run against a real Postgres: Alice appends Bob to
/// her `friends`, so from Bob's side `exists .<friends[is Person]` must be
/// true only for Bob, and a third, unconnected Person must not match.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn multilink_sourced_backlink_exists_filter_returns_the_right_people() {
    let (module, schema, pool) = setup().await;

    for stmt in [
        format!("insert {module}::Person {{ name := 'Alice' }}"),
        format!("insert {module}::Person {{ name := 'Bob' }}"),
        format!("insert {module}::Person {{ name := 'Carol' }}"),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }

    // Alice adds Bob as a friend (one-directional append) — Carol stays unconnected.
    let update = query::compile(
        &format!(
            "update {module}::Person filter .name = 'Alice' \
             set {{ friends += (select {module}::Person filter .name = 'Bob') }}"
        ),
        &schema,
    )
    .unwrap();
    pool.execute_typed(&update.sql, &[]).await.unwrap();

    let select = query::compile(
        &format!("select {module}::Person {{ name }} filter exists .<friends[is {module}::Person] order by .name"),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly the 1 Person (Bob) listed in someone else's friends, got {rows:?}"
    );
    let pylon_value::DecodedValue::Composite(fields) = &rows[0] else {
        panic!("expected a Composite-shaped Person row, got {:?}", rows[0]);
    };
    assert_eq!(fields.get(1), Some(&pylon_value::DecodedValue::Str("Bob".to_string())));
}

/// Regression test for gap 2 (see file doc comment) — the actual "deeply
/// nested backlinks" scenario: a computed pointer sourced from a
/// single-link backlink (`Org.<org[is Team]`), itself containing another
/// computed pointer sourced from a further single-link backlink
/// (`Team.<team[is Member]`), returns the right two-level object tree.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn nested_single_link_backlink_shape_returns_the_correct_two_level_chain() {
    let (module, schema, pool) = setup().await;

    for stmt in [
        format!("insert {module}::Org {{ name := 'Acme' }}"),
        format!("insert {module}::Team {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
        format!("insert {module}::Team {{ name := 'Beta', org := (select {module}::Org filter .name = 'Acme') }}"),
        format!("insert {module}::Member {{ name := 'Ann', team := (select {module}::Team filter .name = 'Alpha') }}"),
        format!("insert {module}::Member {{ name := 'Bob', team := (select {module}::Team filter .name = 'Alpha') }}"),
        format!("insert {module}::Member {{ name := 'Cid', team := (select {module}::Team filter .name = 'Beta') }}"),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }

    let select = query::compile(
        &format!(
            "select {module}::Org {{ \
                name, \
                teams := .<org[is {module}::Team] {{ \
                    name, \
                    members := .<team[is {module}::Member] {{ name }} \
                }} \
            }} filter .name = 'Acme'"
        ),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one Org row, got {rows:?}");

    let pylon_value::DecodedValue::Composite(org_fields) = &rows[0] else {
        panic!("expected a Composite-shaped Org row, got {:?}", rows[0]);
    };
    // [0] = __type__, [1] = name, [2] = teams (an Array of Composite Team rows)
    assert_eq!(
        org_fields.get(1),
        Some(&pylon_value::DecodedValue::Str("Acme".to_string()))
    );
    let pylon_value::DecodedValue::Array(teams) = &org_fields[2] else {
        panic!("expected teams to decode as an Array, got {:?}", org_fields[2]);
    };
    assert_eq!(
        teams.len(),
        2,
        "Org should have exactly 2 backlinked Teams, got {teams:?}"
    );

    let mut member_counts: Vec<usize> = teams
        .iter()
        .map(|t| {
            let pylon_value::DecodedValue::Composite(team_fields) = t else {
                panic!("expected a Composite-shaped Team row, got {t:?}");
            };
            let pylon_value::DecodedValue::Array(members) = &team_fields[2] else {
                panic!("expected members to decode as an Array, got {:?}", team_fields[2]);
            };
            members.len()
        })
        .collect();
    member_counts.sort();
    assert_eq!(
        member_counts,
        vec![1, 2],
        "Alpha should have 2 members, Beta should have 1, got {member_counts:?}"
    );
}

/// Same idea as the two-level Org/Team/Member test, but sourced from a
/// self-referential *multilink* backlink instead of a chain of single
/// links — confirms `IrMultiLinkJoin::BacklinkJunction` (not just
/// `BacklinkFk`) works in a shape position, not only in filter/exists.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn multilink_sourced_backlink_shape_returns_the_right_followers() {
    let (module, schema, pool) = setup().await;

    for stmt in [
        format!("insert {module}::Person {{ name := 'Alice' }}"),
        format!("insert {module}::Person {{ name := 'Bob' }}"),
        format!("insert {module}::Person {{ name := 'Carol' }}"),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }

    // Alice and Carol both add Bob as a friend.
    for friender in ["Alice", "Carol"] {
        let update = query::compile(
            &format!(
                "update {module}::Person filter .name = '{friender}' \
                 set {{ friends += (select {module}::Person filter .name = 'Bob') }}"
            ),
            &schema,
        )
        .unwrap();
        pool.execute_typed(&update.sql, &[]).await.unwrap();
    }

    let select = query::compile(
        &format!(
            "select {module}::Person {{ \
                name, \
                followers := .<friends[is {module}::Person] {{ name }} \
            }} filter .name = 'Bob'"
        ),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let pylon_value::DecodedValue::Composite(bob_fields) = &rows[0] else {
        panic!("expected a Composite-shaped Person row, got {:?}", rows[0]);
    };
    let pylon_value::DecodedValue::Array(followers) = &bob_fields[2] else {
        panic!("expected followers to decode as an Array, got {:?}", bob_fields[2]);
    };
    let mut follower_names: Vec<String> = followers
        .iter()
        .map(|f| {
            let pylon_value::DecodedValue::Composite(fields) = f else {
                panic!("expected a Composite-shaped follower row, got {f:?}");
            };
            let Some(pylon_value::DecodedValue::Str(name)) = fields.get(1) else {
                panic!("expected a Str name at position 1, got {fields:?}");
            };
            name.clone()
        })
        .collect();
    follower_names.sort();
    assert_eq!(follower_names, vec!["Alice".to_string(), "Carol".to_string()]);
}

/// Confirms there's no artificial depth limit on nested backlink shapes:
/// `compile_backlink_pointer` calls `compile_shape`, which calls
/// `compile_shape_element`, which can call `compile_backlink_pointer`
/// again for a nested element — a plain recursive descent with no depth
/// counter or cap anywhere in that path. One level further than the
/// Org/Team/Member test (`Org <- Team <- Member <- Task`, 3 backlink hops)
/// to demonstrate it's genuinely open-ended, not coincidentally capped at 2.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn triple_nested_single_link_backlink_shape_returns_the_correct_chain() {
    let (module, schema, pool) = setup().await;

    for stmt in [
        format!("insert {module}::Org {{ name := 'Acme' }}"),
        format!("insert {module}::Team {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
        format!("insert {module}::Member {{ name := 'Ann', team := (select {module}::Team filter .name = 'Alpha') }}"),
        format!(
            "insert {module}::Task {{ title := 'Ship it', assignee := (select {module}::Member filter .name = 'Ann') }}"
        ),
        format!(
            "insert {module}::Task {{ title := 'Review PR', assignee := (select {module}::Member filter .name = 'Ann') }}"
        ),
    ] {
        let compiled = query::compile(&stmt, &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }

    let select = query::compile(
        &format!(
            "select {module}::Org {{ \
                name, \
                teams := .<org[is {module}::Team] {{ \
                    name, \
                    members := .<team[is {module}::Member] {{ \
                        name, \
                        tasks := .<assignee[is {module}::Task] {{ title }} \
                    }} \
                }} \
            }} filter .name = 'Acme'"
        ),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let pylon_value::DecodedValue::Composite(org_fields) = &rows[0] else {
        panic!("expected a Composite-shaped Org row, got {:?}", rows[0]);
    };
    let pylon_value::DecodedValue::Array(teams) = &org_fields[2] else {
        panic!("expected teams to decode as an Array, got {:?}", org_fields[2]);
    };
    assert_eq!(teams.len(), 1);
    let pylon_value::DecodedValue::Composite(team_fields) = &teams[0] else {
        panic!("expected a Composite-shaped Team row, got {:?}", teams[0]);
    };
    let pylon_value::DecodedValue::Array(members) = &team_fields[2] else {
        panic!("expected members to decode as an Array, got {:?}", team_fields[2]);
    };
    assert_eq!(members.len(), 1);
    let pylon_value::DecodedValue::Composite(member_fields) = &members[0] else {
        panic!("expected a Composite-shaped Member row, got {:?}", members[0]);
    };
    assert_eq!(
        member_fields.get(1),
        Some(&pylon_value::DecodedValue::Str("Ann".to_string()))
    );
    let pylon_value::DecodedValue::Array(tasks) = &member_fields[2] else {
        panic!("expected tasks to decode as an Array, got {:?}", member_fields[2]);
    };
    let mut titles: Vec<String> = tasks
        .iter()
        .map(|t| {
            let pylon_value::DecodedValue::Composite(fields) = t else {
                panic!("expected a Composite-shaped Task row, got {t:?}");
            };
            let Some(pylon_value::DecodedValue::Str(title)) = fields.get(1) else {
                panic!("expected a Str title at position 1, got {fields:?}");
            };
            title.clone()
        })
        .collect();
    titles.sort();
    assert_eq!(titles, vec!["Review PR".to_string(), "Ship it".to_string()]);
}

/// A multi-link read as a bare value, not traversed through:
/// `filter <binding> in .friends`. The single-step resolver in
/// `compile_path` knew properties, links and computeds but never
/// multi-links, so this reported the pointer as unknown — while the
/// suggester, which does know them, offered the same name straight back.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_bare_multilink_reads_as_the_set_of_rows_on_its_far_side() {
    let (module, schema, pool) = setup().await;

    for name in ["Alice", "Bob", "Carol"] {
        let compiled = query::compile(&format!("insert {module}::Person {{ name := '{name}' }}"), &schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }
    let update = query::compile(
        &format!(
            "update {module}::Person filter .name = 'Alice' \
             set {{ friends += (select {module}::Person filter .name = 'Bob') }}"
        ),
        &schema,
    )
    .unwrap();
    pool.execute_typed(&update.sql, &[]).await.unwrap();

    let select = query::compile(
        &format!(
            "with bob := (select detached {module}::Person filter .name = 'Bob' limit 1) \
             select {module}::Person {{ name }} filter bob in .friends order by .name"
        ),
        &schema,
    )
    .unwrap();
    let rows = pool
        .query_typed(&select.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "only Alice lists Bob as a friend — a membership test that matched everyone \
         or no one would still have compiled, got {rows:?}"
    );
    let pylon_value::DecodedValue::Composite(fields) = &rows[0] else {
        panic!("expected a Composite-shaped Person row, got {:?}", rows[0]);
    };
    assert_eq!(fields.get(1), Some(&pylon_value::DecodedValue::Str("Alice".to_string())));
}

async fn rows(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<pylon_value::DecodedValue> {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.query_typed(&compiled.sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
        .unwrap()
}

async fn seed_teams(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, module: &str) {
    for stmt in [
        format!("insert {module}::Org {{ name := 'HasTeam' }}"),
        format!("insert {module}::Org {{ name := 'NoTeam' }}"),
        format!("insert {module}::Team {{ name := 'Beta', org := (select {module}::Org filter .name = 'HasTeam') }}"),
        format!("insert {module}::Team {{ name := 'Alpha', org := (select {module}::Org filter .name = 'HasTeam') }}"),
        format!("insert {module}::Member {{ name := 'Mo', team := (select {module}::Team filter .name = 'Alpha') }}"),
    ] {
        let compiled = query::compile(&stmt, schema).unwrap();
        pool.execute_typed(&compiled.sql, &[]).await.unwrap();
    }
}

fn text_fields(rows: &[pylon_value::DecodedValue], position: usize) -> Vec<Option<String>> {
    rows.iter()
        .map(|row| match row {
            pylon_value::DecodedValue::Composite(fields) => match fields.get(position) {
                Some(pylon_value::DecodedValue::Str(s)) => Some(s.clone()),
                _ => None,
            },
            other => panic!("expected a Composite row, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_union_of_walks_yields_the_values_of_both() {
    let (module, schema, pool) = setup().await;
    seed_teams(&pool, &schema, &module).await;

    let found = rows(&pool, &schema, &format!("select {module}::Team.name union {module}::Member.name")).await;
    let mut names: Vec<_> = text_fields(&found, 0).into_iter().flatten().collect();
    names.sort();
    assert_eq!(names, vec!["Alpha", "Beta", "Mo"]);
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_pointer_read_off_a_walks_shape_is_evaluated_per_row() {
    let (module, schema, pool) = setup().await;
    seed_teams(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "select {module}::Org {{ name, has_alpha := any(.<org[is {module}::Team] {{ alpha := .name = 'Alpha' }}.alpha) }} \
             order by .name"
        ),
    )
    .await;
    let flags: Vec<_> = found
        .iter()
        .map(|row| match row {
            pylon_value::DecodedValue::Composite(fields) => fields.get(2).cloned(),
            other => panic!("expected a Composite row, got {other:?}"),
        })
        .collect();
    // An org with no teams has nothing to be true of: `any` of nothing is false.
    assert_eq!(
        flags,
        vec![Some(pylon_value::DecodedValue::Bool(true)), Some(pylon_value::DecodedValue::Bool(false))]
    );
}

