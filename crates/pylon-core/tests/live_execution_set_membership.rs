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

//! `in` over a set on its left — live-execution tests. See
//! `live_execution_smoke.rs` for the harness's purpose and how to run these
//! (same pattern, this binary is `--test live_execution_set_membership`).
//!
//! `in` asks its question once per element of the set on its left, so over a
//! set it answers with a set of booleans. Read as a single value instead, the
//! left side became one scalar subquery over the whole set, which Postgres
//! aborts with "more than one row returned by a subquery used as an
//! expression" — an error no SQL-text assertion can see, which is why these
//! run the queries for real.

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
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

/// `Account` as an *interface* with two implementors, which is the shape the
/// reported query had: the set on the right of `in` spans more than one table,
/// so the ids it is tested against come from a union rather than one source.
/// Plus a `Licence` with a multi-link, for the same question asked of a
/// pointer rather than of a binding.
fn membership_schema(module: &str) -> SchemaDescriptor {
    let account_q = format!("{module}::Account");
    let addon_q = format!("{module}::Addon");

    let mut account = ty("Account", module, vec![id_prop(), text_prop("email")]);
    account.abstract_ = true;

    let mut individual = ty("Individual", module, vec![id_prop(), text_prop("email")]);
    individual.interfaces = vec![account_q.clone()];

    let mut organization = ty("Organization", module, vec![id_prop(), text_prop("email")]);
    organization.interfaces = vec![account_q];

    let mut licence = ty("Licence", module, vec![id_prop(), text_prop("name")]);
    licence.multilinks = vec![multilink("required_addons", &addon_q)];

    SchemaDescriptor {
        types: vec![
            account,
            individual,
            organization,
            licence,
            ty("Addon", module, vec![id_prop(), text_prop("name")]),
        ],
        ..Default::default()
    }
}

async fn setup() -> (String, SchemaDescriptor, PgPool) {
    let module = unique_module("live_membership");
    let schema = membership_schema(&module);
    let pool = test_pool().await;
    pool.batch_execute(&export_schema(&schema).unwrap()).await.unwrap();
    (module, schema, pool)
}

async fn run(pool: &PgPool, schema: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, schema).unwrap_or_else(|e| panic!("{pyql}: {e}"));
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

async fn rows(pool: &PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, schema).unwrap_or_else(|e| panic!("{pyql}: {e}"));
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap_or_else(|e| panic!("{pyql}\n{}\n{e}", compiled.sql))
}

/// The booleans a select of them produces, one per row.
fn flags(rows: &[DecodedValue]) -> Vec<bool> {
    rows.iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => match fields.as_slice() {
                [DecodedValue::Bool(b)] => *b,
                other => panic!("expected a one-element boolean row, got {other:?}"),
            },
            other => panic!("expected a Composite row, got {other:?}"),
        })
        .collect()
}

/// The value of the pointer at `position` in each row of a shape query.
fn field(rows: &[DecodedValue], position: usize) -> Vec<DecodedValue> {
    rows.iter()
        .map(|row| match row {
            DecodedValue::Composite(fields) => fields[position].clone(),
            other => panic!("expected a Composite row, got {other:?}"),
        })
        .collect()
}

async fn seed(pool: &PgPool, schema: &SchemaDescriptor, module: &str) {
    for stmt in [
        format!("insert {module}::Individual {{ email := 'ann@example.com' }}"),
        format!("insert {module}::Individual {{ email := 'bo@example.com' }}"),
        format!("insert {module}::Organization {{ email := 'acme@example.com' }}"),
        format!("insert {module}::Addon {{ name := 'backup' }}"),
        format!("insert {module}::Addon {{ name := 'sso' }}"),
        format!("insert {module}::Addon {{ name := 'audit' }}"),
    ] {
        run(pool, schema, &stmt).await;
    }
    run(
        pool,
        schema,
        &format!(
            "insert {module}::Licence {{ name := 'pro', \
             required_addons := (select {module}::Addon filter .name in {{'backup', 'sso'}}) }}"
        ),
    )
    .await;
    run(
        pool,
        schema,
        &format!(
            "insert {module}::Licence {{ name := 'basic', \
             required_addons := (select {module}::Addon filter .name = 'backup') }}"
        ),
    )
    .await;
}

/// The reported query: every individual *is* an account, so every element of
/// the left set answers true — two answers, not one, and not an error.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn one_set_in_another_answers_once_per_element_of_the_left() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with individuals := (select {module}::Individual), \
                  accounts := (select {module}::Account) \
             select individuals in accounts"
        ),
    )
    .await;
    assert_eq!(flags(&found), vec![true, true]);
}

/// And an individual is never an organization, so the same two elements all
/// answer false — the count is the left set's either way, which is what says
/// the answer is per element rather than one verdict on the whole set.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn one_set_not_in_a_disjoint_one_answers_once_per_element_too() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with individuals := (select {module}::Individual), \
                  orgs := (select {module}::Organization) \
             select individuals in orgs"
        ),
    )
    .await;
    assert_eq!(flags(&found), vec![false, false]);

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with individuals := (select {module}::Individual), \
                  orgs := (select {module}::Organization) \
             select individuals not in orgs"
        ),
    )
    .await;
    assert_eq!(flags(&found), vec![true, true]);
}

/// `any(…)`/`all(…)` reduce those per-element answers back to one. The
/// quantifier's argument is a set position, so the set is built the same way
/// there as it is when the select returns it whole — read as a single value
/// instead, the left binding collapsed into a scalar subquery and the query
/// aborted before either quantifier saw anything.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn quantifying_one_sets_membership_in_another_gives_one_answer() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    for (query, expected) in [
        ("all(individuals in accounts)", true),
        ("any(individuals in accounts)", true),
        ("all(individuals in orgs)", false),
        ("any(individuals in orgs)", false),
    ] {
        let found = rows(
            &pool,
            &schema,
            &format!(
                "with individuals := (select {module}::Individual), \
                      accounts := (select {module}::Account), \
                      orgs := (select {module}::Organization) \
                 select {query}"
            ),
        )
        .await;
        assert_eq!(flags(&found), vec![expected], "{query}");
    }
}

/// `test := .required_addons in licensed` is a boolean per required addon.
/// The `pro` licence requires one addon that is licensed and one that is not,
/// which a single verdict over the whole link cannot express.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_multilink_in_a_set_answers_once_per_linked_object() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with licensed := (select {module}::Addon filter .name in {{'backup', 'audit'}}) \
             select {module}::Licence {{ name, test := .required_addons in licensed }} \
             order by .name"
        ),
    )
    .await;
    // Position 0 is `__type__`, 1 the implicit `id`, 2 `name`, 3 `test`.
    assert_eq!(
        field(&found, 2),
        vec![DecodedValue::Str("basic".into()), DecodedValue::Str("pro".into()),]
    );
    let answers: Vec<Vec<bool>> = field(&found, 3)
        .into_iter()
        .map(|value| match value {
            DecodedValue::Array(items) => {
                let mut flags: Vec<bool> = items
                    .into_iter()
                    .map(|item| match item {
                        DecodedValue::Bool(b) => b,
                        other => panic!("expected a boolean element, got {other:?}"),
                    })
                    .collect();
                flags.sort();
                flags
            }
            other => panic!("expected a set of booleans, got {other:?}"),
        })
        .collect();
    assert_eq!(answers, vec![vec![true], vec![false, true]]);
}

/// `all(…)`/`any(…)` are how the query asks for one verdict over the whole
/// set instead — over the set's own elements, not over the rows of the query
/// the answers were gathered in.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn quantifying_a_multilinks_membership_gives_one_answer_per_row() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with licensed := (select {module}::Addon filter .name in {{'backup', 'audit'}}) \
             select {module}::Licence {{ every := all(.required_addons in licensed), \
                                         some := any(.required_addons in licensed) }} \
             order by .name"
        ),
    )
    .await;
    assert_eq!(
        field(&found, 2),
        vec![DecodedValue::Bool(true), DecodedValue::Bool(false)],
        "only `basic` requires nothing but licensed addons"
    );
    assert_eq!(
        field(&found, 3),
        vec![DecodedValue::Bool(true), DecodedValue::Bool(true)],
        "both require at least one licensed addon"
    );
}

/// A FILTER still wants one verdict for the row, which is the reading it has
/// always had: the licence survives when any of its addons is licensed.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_multilinks_membership_in_a_filter_keeps_its_single_verdict() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with licensed := (select {module}::Addon filter .name = 'sso') \
             select {module}::Licence {{ name }} filter .required_addons in licensed"
        ),
    )
    .await;
    assert_eq!(field(&found, 2), vec![DecodedValue::Str("pro".into())]);
}

/// A quantifier in a FILTER has to ask its own question: `all` used to fold
/// into the same bare EXISTS over the junction as `any`, which answers "some
/// element matches" — so `all` silently agreed with `any` on every row. Only
/// `basic` requires nothing but licensed addons; both licences require at
/// least one.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn all_and_any_in_a_filter_select_different_rows() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let names = |found: &[DecodedValue]| {
        let mut names: Vec<String> = field(found, 2)
            .into_iter()
            .map(|v| match v {
                DecodedValue::Str(s) => s,
                other => panic!("expected a name, got {other:?}"),
            })
            .collect();
        names.sort();
        names
    };

    // Spelled with `in`, and with the `=` that means the same membership.
    for condition in ["in licensed", "= licensed"] {
        let query = |quantifier: &str| {
            format!(
                "with licensed := (select {module}::Addon filter .name in {{'backup', 'audit'}}) \
                 select {module}::Licence {{ name }} filter {quantifier}(.required_addons {condition})"
            )
        };
        assert_eq!(
            names(&rows(&pool, &schema, &query("all")).await),
            vec!["basic"],
            "{condition}"
        );
        assert_eq!(
            names(&rows(&pool, &schema, &query("any")).await),
            vec!["basic", "pro"],
            "{condition}"
        );
    }
}

/// A walk with a tail takes the other `all` shape — one EXISTS with the test
/// negated around each element — and a set literal is a haystack it can be
/// tested against at all, which used to fail to compile outright.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn all_over_a_walk_with_a_tail_tests_every_element() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let names = |found: &[DecodedValue]| {
        let mut names: Vec<String> = field(found, 2)
            .into_iter()
            .map(|v| match v {
                DecodedValue::Str(s) => s,
                other => panic!("expected a name, got {other:?}"),
            })
            .collect();
        names.sort();
        names
    };

    let found = rows(
        &pool,
        &schema,
        &format!("select {module}::Licence {{ name }} filter all(.required_addons.name in {{'backup', 'audit'}})"),
    )
    .await;
    assert_eq!(names(&found), vec!["basic"]);

    let found = rows(
        &pool,
        &schema,
        &format!("select {module}::Licence {{ name }} filter any(.required_addons.name in {{'backup', 'audit'}})"),
    )
    .await;
    assert_eq!(names(&found), vec!["basic", "pro"]);
}

/// Comparing the link to a binding of more than one row is membership, not a
/// comparison against the one value a scalar subquery can return — which
/// Postgres aborted on ("more than one row returned by a subquery used as an
/// expression") the moment the binding held two.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn comparing_a_multilink_to_a_multi_row_binding_runs() {
    let (module, schema, pool) = setup().await;
    seed(&pool, &schema, &module).await;

    let found = rows(
        &pool,
        &schema,
        &format!(
            "with licensed := (select {module}::Addon filter .name in {{'sso', 'audit'}}) \
             select {module}::Licence {{ name }} filter .required_addons = licensed"
        ),
    )
    .await;
    assert_eq!(field(&found, 2), vec![DecodedValue::Str("pro".into())]);
}
