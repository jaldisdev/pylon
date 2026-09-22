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

//! Live-Postgres tests for `Global`s — session globals (client-injected
//! per-request) and computed globals (a PyQL expression evaluated at query
//! time, which may itself reference a session global). Pylon has no
//! `set global`/`reset global` session commands — a session global is
//! always bound as an ordinary query parameter per call, never mutated
//! session-side — so the behaviors proven here are (a session
//! global's value flows correctly into a referencing query; a computed
//! global correctly reads another global inside its own expression) are the
//! same ones worth proving here.
//!
//! This is also the regression test for a real bug found and fixed in this
//! session: `resolve_global_pg_type` matched bare Python class names
//! (`"UUID"`, `"Str"`) but `GlobalDescriptor.scalar_type` is actually a
//! PyQL-style qualified name (`"std::uuid"`, `"array<std::str>"`) — every
//! session global of a builtin scalar type silently resolved to `text`,
//! which only surfaced as a genuine SQL type error once a query actually
//! compared it against a real non-text column (confirmed live, then fixed;
//! see `Compiler::resolve_global_pg_type`'s own doc comment).
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_globals -- --ignored
//! ```

mod common;

use common::*;
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{GlobalDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
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

fn session_global(name: &str, module: &str, scalar_type: &str) -> GlobalDescriptor {
    GlobalDescriptor {
        name: name.into(),
        module: module.into(),
        scalar_type: scalar_type.into(),
        required: false,
        default_expr: None,
        computed_expr: None,
    }
}

fn computed_global(name: &str, module: &str, scalar_type: &str, expr: &str) -> GlobalDescriptor {
    GlobalDescriptor {
        name: name.into(),
        module: module.into(),
        scalar_type: scalar_type.into(),
        required: false,
        default_expr: None,
        computed_expr: Some(expr.into()),
    }
}

async fn exec(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

/// Compile `pyql` and execute it with `params` bound positionally in
/// `compiled.param_names`' own order — the caller is responsible for
/// knowing that order (every test here references exactly one global, so
/// it's always a single-element params slice).
async fn rows_with_params(
    pool: &pylon_pgcon::PgPool,
    sd: &SchemaDescriptor,
    pyql: &str,
    params: &[DecodedValue],
) -> Vec<DecodedValue> {
    let compiled = query::compile(pyql, sd).unwrap();
    pool.query_typed(&compiled.sql, params, &ExtensionOids::default())
        .await
        .unwrap()
}

async fn rows_of(pool: &pylon_pgcon::PgPool, sd: &SchemaDescriptor, pyql: &str) -> Vec<DecodedValue> {
    rows_with_params(pool, sd, pyql, &[]).await
}

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
    }
}

async fn bootstrap(pool: &pylon_pgcon::PgPool) {
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn session_global_of_uuid_type_filters_correctly() {
    // The exact type shape the bug lived in: a Global[UUID] compared
    // against a real uuid column. Before the fix this failed to even
    // compile ("operator '=' cannot be applied to operands of type
    // 'std::uuid' and 'std::str'"), since the global's pg_type silently
    // resolved to "text" instead of "uuid".
    let module = unique_module("live_g_uuid");
    let widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    let sd = SchemaDescriptor {
        types: vec![widget],
        globals: vec![session_global("viewer_id", &module, "std::uuid")],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Widget {{ name := 'Alice' }}")).await;
    let inserted = rows_of(&pool, &sd, &format!("select {module}::Widget {{ id }}")).await;
    let DecodedValue::Composite(shape) = &inserted[0] else {
        panic!("expected Composite")
    };
    let viewer_id = shape[1].clone();

    let rows = rows_with_params(
        &pool,
        &sd,
        &format!("select {module}::Widget {{ name }} filter .id = global viewer_id"),
        &[viewer_id],
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], 1), &DecodedValue::Str("Alice".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn session_global_unbound_is_null() {
    let module = unique_module("live_g_null");
    let widget = ty("Widget", &module, vec![id_prop(), text_prop("name")]);
    let sd = SchemaDescriptor {
        types: vec![widget],
        globals: vec![session_global("viewer_id", &module, "std::uuid")],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Widget {{ name := 'Alice' }}")).await;

    let rows = rows_with_params(
        &pool,
        &sd,
        &format!("select {module}::Widget {{ name }} filter .id = global viewer_id"),
        &[DecodedValue::Null],
    )
    .await;
    assert!(
        rows.is_empty(),
        "an unbound (NULL) global must not accidentally match every row, got {rows:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn session_global_of_array_type_resolves_correctly() {
    // Covers the array<...> branch resolve_global_pg_type's fix added —
    // pylon-demo's own `favorite_tags: Global[list[Str] | None]` is this
    // exact shape.
    let module = unique_module("live_g_array");
    let sd = SchemaDescriptor {
        types: vec![],
        globals: vec![session_global("tags", &module, "array<std::str>")],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    let rows = rows_with_params(
        &pool,
        &sd,
        "select array_join(global tags, ',')",
        &[DecodedValue::Array(vec![
            DecodedValue::Str("a".into()),
            DecodedValue::Str("b".into()),
        ])],
    )
    .await;
    assert_eq!(rows.len(), 1);
    let DecodedValue::Composite(shape) = &rows[0] else {
        panic!("expected Composite")
    };
    assert_eq!(shape[0], DecodedValue::Str("a,b".to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn computed_global_reads_a_session_global_it_references() {
    // The regression scenario itself: a computed global's own expression
    // references a *different* session global — this is exactly what
    // `resolve_global_pg_type` being wrong broke, since the session
    // global's pg_type is resolved the same way regardless of which
    // expression ends up referencing it.
    let module = unique_module("live_g_computed");
    let person = ty("Person", &module, vec![id_prop(), text_prop("name")]);
    let sd = SchemaDescriptor {
        types: vec![person],
        globals: vec![
            session_global("current_user_id", &module, "std::uuid"),
            computed_global(
                "current_user",
                &module,
                &format!("{module}::Person"),
                &format!("select {module}::Person filter .id = global current_user_id"),
            ),
        ],
        ..Default::default()
    };

    let pool = test_pool().await;
    bootstrap(&pool).await;
    pool.batch_execute(&export_schema(&sd).unwrap()).await.unwrap();

    exec(&pool, &sd, &format!("insert {module}::Person {{ name := 'Ada' }}")).await;
    let inserted = rows_of(&pool, &sd, &format!("select {module}::Person {{ id }}")).await;
    let DecodedValue::Composite(shape) = &inserted[0] else {
        panic!("expected Composite")
    };
    let person_id = shape[1].clone();

    let rows = rows_with_params(&pool, &sd, "select global current_user { name }", &[person_id]).await;
    assert_eq!(
        rows.len(),
        1,
        "computed global should resolve to exactly the referenced Person, got {rows:?}"
    );
    assert_eq!(field(&rows[0], 1), &DecodedValue::Str("Ada".to_string()));
}
