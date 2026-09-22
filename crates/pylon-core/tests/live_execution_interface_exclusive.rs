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

//! Cross-table `exclusive` enforcement for interface types — live-execution
//! tests. See `live_execution_smoke.rs` for the harness's purpose and how
//! to run these (same pattern, this binary is `--test
//! live_execution_interface_exclusive`).
//!
//! This whole area had **zero** prior test coverage — the DDL generator
//! (`export::interface_exclusive_trigger_infos` / `make_excl_info`) and its
//! migration-diff mirror (`diff/mod.rs`'s Phase 11.5) were never exercised
//! by so much as a fast SQL-text snapshot test, let alone a real insert
//! against a real database. Every case here inserts real rows into real
//! implementor tables and asserts on whether the insert/update actually
//! succeeded or failed — the only way to confirm a `DEFERRABLE INITIALLY
//! DEFERRED` constraint trigger checking a UNION-ALL interface view does
//! what it's supposed to.
//!
//! Confirmed live: a per-implementor `CREATE UNIQUE INDEX` catches
//! same-table duplicates (ordinary Postgres uniqueness, nothing special),
//! and the cross-table constraint trigger catches a duplicate landing in a
//! *different* implementor's table — including one introduced by an
//! `UPDATE`, and including within a single transaction across two
//! different tables (caught only because the trigger is deferred to
//! statement/commit time, by which point both new rows already exist).
//! NULL values on a nullable exclusive property are correctly never
//! considered duplicates of each other, matching plain SQL UNIQUE
//! semantics.
//!
//! The junction-backed-link cases below insert through compiled PyQL
//! (`insert ... { employer := (select Company filter ...) }`) exactly like
//! every other case in this file — this only works because of a fix, in
//! the same change as this test file, to `junction_info_for`/
//! `build_multilink_join` and their ~18 duplicated call sites in
//! `ir/compiler.rs`: they used to resolve a junction-backed link's physical
//! table from `through_td.table` (the `through()` type's own table field),
//! which only happened to be correct because the Python walker pre-renames
//! it for a *single* hardcoded owner+link pair. A `through()` type shared
//! across multiple concrete implementors — exactly what an
//! interface-inherited junction-backed link produces — needs a physically
//! separate table per implementor (`emit_one_junction_table` already did
//! this correctly on the DDL side), so table names are now always derived
//! from the *calling* owner type instead, matching the no-`through()` case
//! that already worked this way.

mod common;

use common::*;
use pylon_core::diff::{DbState, diff_schema};
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{PropertyDescriptor, SchemaDescriptor, TypeConstraint, TypeDescriptor};
use pylon_pgcon::{ExtensionOids, PgPool};
use pylon_value::DecodedValue;

fn exclusive_prop(name: &str, nullable: bool) -> PropertyDescriptor {
    PropertyDescriptor {
        name: name.into(),
        pg_type: "text".into(),
        nullable,
        default_sql: None,
        default_pyql: None,
        description: None,
        check_constraints: vec![],
        is_exclusive: true,
        is_pk: false,
        is_readonly: false,
        rewrites: vec![],
        tuple_members: None,
        column_type: None,
    }
}

fn interface_ty(name: &str, module: &str, properties: Vec<PropertyDescriptor>) -> TypeDescriptor {
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
        abstract_: true,
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

fn implementor_ty(
    name: &str,
    module: &str,
    interface_qname: &str,
    inherited_properties: Vec<PropertyDescriptor>,
    own_properties: Vec<PropertyDescriptor>,
) -> TypeDescriptor {
    let mut properties = inherited_properties;
    properties.extend(own_properties);
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
        abstract_: false,
        materialized: true,
        description: None,
        parents: vec![],
        interfaces: vec![interface_qname.into()],
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

/// One interface (`Account`, exclusive non-nullable `email`) with two
/// implementors (`Individual`/`Organization`, each with one own property)
/// — the minimal shape needed to exercise cross-table exclusivity.
fn account_schema(module: &str) -> SchemaDescriptor {
    let account_q = format!("{module}::Account");
    SchemaDescriptor {
        types: vec![
            interface_ty("Account", module, vec![id_prop(), exclusive_prop("email", false)]),
            implementor_ty(
                "Individual",
                module,
                &account_q,
                vec![id_prop(), exclusive_prop("email", false)],
                vec![text_prop("first_name")],
            ),
            implementor_ty(
                "Organization",
                module,
                &account_q,
                vec![id_prop(), exclusive_prop("email", false)],
                vec![text_prop("legal_name")],
            ),
        ],
        ..Default::default()
    }
}

/// Same shape, but `email` is nullable — for the NULL-handling case.
fn nullable_account_schema(module: &str) -> SchemaDescriptor {
    let account_q = format!("{module}::Account");
    SchemaDescriptor {
        types: vec![
            interface_ty("Account", module, vec![id_prop(), exclusive_prop("email", true)]),
            implementor_ty(
                "Individual",
                module,
                &account_q,
                vec![id_prop(), exclusive_prop("email", true)],
                vec![text_prop("first_name")],
            ),
            implementor_ty(
                "Organization",
                module,
                &account_q,
                vec![id_prop(), exclusive_prop("email", true)],
                vec![text_prop("legal_name")],
            ),
        ],
        ..Default::default()
    }
}

/// A composite exclusive constraint (`unique on (first_name, email)`)
/// declared directly on the interface — exercises the `TypeConstraint`
/// path (as opposed to a single exclusive property), spanning implementors
/// whose own extra property differs (`first_name` vs `legal_name`), so
/// only `Individual` can even participate in this particular composite.
fn composite_account_schema(module: &str) -> SchemaDescriptor {
    let account_q = format!("{module}::Account");
    let mut account = interface_ty(
        "Account",
        module,
        vec![id_prop(), text_prop("email"), text_prop("first_name")],
    );
    account.constraints.push(TypeConstraint::Exclusive {
        pointers: vec!["first_name".into(), "email".into()],
        unless: None,
    });
    let mut individual_a = implementor_ty(
        "IndividualA",
        module,
        &account_q,
        vec![id_prop(), text_prop("email"), text_prop("first_name")],
        vec![],
    );
    individual_a.constraints.push(TypeConstraint::Exclusive {
        pointers: vec!["first_name".into(), "email".into()],
        unless: None,
    });
    let mut individual_b = implementor_ty(
        "IndividualB",
        module,
        &account_q,
        vec![id_prop(), text_prop("email"), text_prop("first_name")],
        vec![],
    );
    individual_b.constraints.push(TypeConstraint::Exclusive {
        pointers: vec!["first_name".into(), "email".into()],
        unless: None,
    });
    SchemaDescriptor {
        types: vec![account, individual_a, individual_b],
        ..Default::default()
    }
}

/// An interface (`Account`) with a junction-backed exclusive single link
/// (`employer: Link[Company, through(Employment), Exclusive]`) inherited by
/// two implementors — each implementor gets its own physically separate
/// junction table (`"Individual.employer"` / `"Organization.employer"`;
/// `through(Employment)` only ever contributes extra link-property
/// columns, never a shared physical table), so this exercises
/// `junction_excl_view_ddl_with_names`/`make_excl_junction_info` rather
/// than the plain-object-column path every other schema in this file uses.
fn employer_account_schema(module: &str) -> SchemaDescriptor {
    use pylon_core::schema::LinkDescriptor;

    let account_q = format!("{module}::Account");
    let company_q = format!("{module}::Company");
    let employment_q = format!("{module}::Employment");

    let employer_link = LinkDescriptor {
        name: "employer".into(),
        target: company_q.clone(),
        nullable: true,
        through: Some(employment_q.clone()),
        description: None,
        default_pyql: None,
        is_exclusive: true,
        is_readonly: false,
        rewrites: vec![],
        on_delete: vec![],
    };

    let mut account = interface_ty("Account", module, vec![id_prop(), text_prop("name")]);
    account.links.push(employer_link.clone());

    let mut individual = implementor_ty(
        "Individual",
        module,
        &account_q,
        vec![id_prop(), text_prop("name")],
        vec![],
    );
    individual.links.push(employer_link.clone());

    let mut organization = implementor_ty(
        "Organization",
        module,
        &account_q,
        vec![id_prop(), text_prop("name")],
        vec![],
    );
    organization.links.push(employer_link);

    let company = simple_named_type(module, "Company");
    let mut employment = simple_named_type(module, "Employment");
    employment.junction = true;
    employment.properties = vec![];

    SchemaDescriptor {
        types: vec![account, individual, organization, company, employment],
        ..Default::default()
    }
}

fn simple_named_type(module: &str, name: &str) -> TypeDescriptor {
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
        properties: vec![id_prop(), text_prop("name")],
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

async fn setup(schema: &SchemaDescriptor) -> PgPool {
    let ddl = export_schema(schema).unwrap();
    let pool = test_pool().await;
    pool.batch_execute(&ddl).await.unwrap();
    pool
}

async fn exec(pool: &PgPool, schema: &SchemaDescriptor, pyql: &str) -> Result<(), pylon_pgcon::Error> {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.map(|_| ())
}

async fn rows_of(pool: &PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn same_table_duplicate_is_rejected() {
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'B' }}"),
    )
    .await;
    assert!(
        result.is_err(),
        "a second Individual with the same email must be rejected by the per-table UNIQUE index"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn cross_table_duplicate_is_rejected() {
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::Organization {{ email := 'a@x.com', legal_name := 'Corp' }}"),
    )
    .await;
    assert!(
        result.is_err(),
        "an Organization with the same email as an existing Individual must be rejected by the cross-table constraint trigger"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn distinct_emails_across_implementors_succeed() {
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::Organization {{ email := 'b@x.com', legal_name := 'Corp' }}"),
    )
    .await;
    assert!(
        result.is_ok(),
        "distinct emails across different implementors must both succeed"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn same_transaction_cross_table_duplicate_is_still_caught() {
    // Both new rows exist by the time the DEFERRED trigger actually runs
    // (end of statement/transaction), so even inserting the conflicting
    // pair back-to-back in one implicit transaction must still fail —
    // confirms the constraint trigger isn't only checking pre-existing
    // committed state.
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    let insert_individual = query::compile(
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
        &schema,
    )
    .unwrap();
    let insert_org = query::compile(
        &format!("insert {module}::Organization {{ email := 'a@x.com', legal_name := 'Corp' }}"),
        &schema,
    )
    .unwrap();
    let result = pool
        .batch_execute(&format!("{}\n{}", insert_individual.sql, insert_org.sql))
        .await;
    assert!(
        result.is_err(),
        "two conflicting inserts across implementors in one transaction must still be rejected"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn updating_into_a_cross_table_duplicate_is_rejected() {
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &schema,
        &format!("insert {module}::Organization {{ email := 'b@x.com', legal_name := 'Corp' }}"),
    )
    .await
    .unwrap();

    let result = exec(
        &pool,
        &schema,
        &format!("update {module}::Organization filter .legal_name = 'Corp' set {{ email := 'a@x.com' }}"),
    )
    .await;
    assert!(
        result.is_err(),
        "updating Organization's email to collide with an existing Individual's must be rejected"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn updating_an_unrelated_field_does_not_trigger_the_check() {
    let module = unique_module("live_excl");
    let schema = account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("update {module}::Individual filter .email = 'a@x.com' set {{ first_name := 'Renamed' }}"),
    )
    .await;
    assert!(
        result.is_ok(),
        "updating a field other than the exclusive one must not run the exclusive check at all"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn null_values_are_never_considered_duplicates_of_each_other() {
    let module = unique_module("live_excl");
    let schema = nullable_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::Organization {{ legal_name := 'Corp' }}"),
    )
    .await;
    assert!(
        result.is_ok(),
        "two different implementors both leaving a nullable exclusive property NULL must not collide (matches plain SQL UNIQUE semantics)"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn composite_exclusive_constraint_is_enforced_across_implementors() {
    let module = unique_module("live_excl");
    let schema = composite_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::IndividualA {{ email := 'a@x.com', first_name := 'Alice' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::IndividualB {{ email := 'a@x.com', first_name := 'Alice' }}"),
    )
    .await;
    assert!(
        result.is_err(),
        "the same (first_name, email) pair across implementors must violate the composite exclusive constraint"
    );

    let ok = exec(
        &pool,
        &schema,
        &format!("insert {module}::IndividualB {{ email := 'a@x.com', first_name := 'Bob' }}"),
    )
    .await;
    assert!(
        ok.is_ok(),
        "a differing first_name means the composite pair no longer collides"
    );
}

/// Mirrors `live_execution_on_delete.rs`'s
/// `migration_path_emits_working_deletion_policy_triggers` — the
/// incremental-migration path (`diff_schema` against an empty `DbState`)
/// shares `interface_exclusive_trigger_infos` with `export_schema`, but
/// confirms the *emitted, applied* DDL from that path actually works too,
/// not just that it type-checks.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn migration_path_emits_working_exclusive_triggers() {
    let module = unique_module("live_excl_mig");
    let schema = account_schema(&module);
    let ddl_ops = diff_schema(&schema, &DbState::default()).unwrap();
    let pool = test_pool().await;
    for op in &ddl_ops {
        pool.batch_execute(op).await.unwrap();
    }

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Individual {{ email := 'a@x.com', first_name := 'A' }}"),
    )
    .await
    .unwrap();
    let result = exec(
        &pool,
        &schema,
        &format!("insert {module}::Organization {{ email := 'a@x.com', legal_name := 'Corp' }}"),
    )
    .await;
    assert!(
        result.is_err(),
        "a cross-table duplicate must be rejected via the migration-path-emitted trigger too"
    );
}

// ── Junction-backed exclusive link (cross-implementor helper view) ─────────────

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn junction_backed_cross_table_duplicate_target_is_rejected() {
    let module = unique_module("live_excl_jt");
    let schema = employer_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Acme' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool, &schema,
        &format!("insert {module}::Individual {{ name := 'Alice', employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await.unwrap();

    let result = exec(
        &pool, &schema,
        &format!("insert {module}::Organization {{ name := 'Beta', employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await;
    assert!(
        result.is_err(),
        "an Organization can't take the same employer as an existing Individual — the cross-implementor junction-view trigger must reject it"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn junction_backed_distinct_targets_across_implementors_succeed() {
    let module = unique_module("live_excl_jt");
    let schema = employer_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Acme' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Globex' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool, &schema,
        &format!("insert {module}::Individual {{ name := 'Alice', employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await.unwrap();
    let result = exec(
        &pool, &schema,
        &format!("insert {module}::Organization {{ name := 'Beta', employer := (select {module}::Company filter .name = 'Globex') }}"),
    ).await;
    assert!(
        result.is_ok(),
        "distinct employers across different implementors must both succeed"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn junction_backed_updating_into_a_cross_table_duplicate_is_rejected() {
    let module = unique_module("live_excl_jt");
    let schema = employer_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Acme' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Globex' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool, &schema,
        &format!("insert {module}::Individual {{ name := 'Alice', employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await.unwrap();
    exec(
        &pool, &schema,
        &format!("insert {module}::Organization {{ name := 'Beta', employer := (select {module}::Company filter .name = 'Globex') }}"),
    ).await.unwrap();

    let result = exec(
        &pool, &schema,
        &format!("update {module}::Organization filter .name = 'Beta' set {{ employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await;
    assert!(
        result.is_err(),
        "updating Organization's employer to collide with Individual's must be rejected"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn junction_backed_read_through_the_interface_view_resolves_the_link() {
    // Confirms the fix's other half: reading `employer` back — including
    // through the *interface's* own path, not just each concrete
    // implementor's — resolves correctly now that the junction table name
    // is derived from the calling owner instead of `through_td.table`.
    let module = unique_module("live_excl_jt");
    let schema = employer_account_schema(&module);
    let pool = setup(&schema).await;

    exec(
        &pool,
        &schema,
        &format!("insert {module}::Company {{ name := 'Acme' }}"),
    )
    .await
    .unwrap();
    exec(
        &pool, &schema,
        &format!("insert {module}::Individual {{ name := 'Alice', employer := (select {module}::Company filter .name = 'Acme') }}"),
    ).await.unwrap();

    let rows = rows_of(
        &pool,
        &schema,
        &format!("select {module}::Individual {{ name, employer: {{ name }} }}"),
    )
    .await;
    let DecodedValue::Composite(fields) = &rows[0] else {
        panic!("expected Composite, got {:?}", rows[0])
    };
    // fields: [type_tag, name, employer] — employer itself: [type_tag, name].
    let DecodedValue::Composite(employer_fields) = &fields[2] else {
        panic!("expected employer to decode as Composite, got {:?}", fields[2])
    };
    assert_eq!(employer_fields[1], DecodedValue::Str("Acme".into()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_multilink_append_through_the_interface_reaches_each_implementors_rows() {
    // Each implementor keeps its own junction table, so an update written
    // against the interface has to write the one belonging to each row.
    let module = unique_module("live_iface_ml_append");
    let mut schema = account_schema(&module);
    let tag_q = format!("{module}::Tag");
    for td in schema.types.iter_mut() {
        td.multilinks = vec![multilink("tags", &tag_q)];
    }
    schema.types.push(TypeDescriptor {
        multilinks: vec![],
        ..implementor_ty("Tag", &module, "", vec![id_prop(), text_prop("name")], vec![])
    });
    if let Some(tag) = schema.types.last_mut() {
        tag.interfaces = vec![];
    }
    let pool = setup(&schema).await;

    for stmt in [
        format!("insert {module}::Individual {{ email := 'a@x', first_name := 'A' }}"),
        format!("insert {module}::Organization {{ email := 'b@x', legal_name := 'B' }}"),
        format!("insert {module}::Tag {{ name := 'vip' }}"),
    ] {
        exec(&pool, &schema, &stmt).await.unwrap();
    }
    exec(
        &pool,
        &schema,
        &format!("update {module}::Account filter .email = 'b@x' set {{ tags += (select {module}::Tag) }}"),
    )
    .await
    .unwrap();

    let tag_counts = |type_name: &str| format!("select {module}::{type_name} {{ n := count(.tags) }}");
    let count_of = |rows: Vec<DecodedValue>| match rows.as_slice() {
        [DecodedValue::Composite(fields)] => fields.get(1).cloned(),
        other => panic!("expected one row, got {other:?}"),
    };
    assert_eq!(count_of(rows_of(&pool, &schema, &tag_counts("Individual")).await), Some(DecodedValue::I64(0)));
    assert_eq!(count_of(rows_of(&pool, &schema, &tag_counts("Organization")).await), Some(DecodedValue::I64(1)));
}
