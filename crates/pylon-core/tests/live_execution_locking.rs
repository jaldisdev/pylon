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

//! Live-Postgres tests for `FOR UPDATE`/`FOR SHARE` row-locking clauses —
//! specifically the two behaviors that only a real, concurrent, multi-
//! connection scenario can prove (a pure SQL-text snapshot test, like the
//! ones in `sql/mod.rs`, can't exercise actual row-lock contention):
//! `SKIP LOCKED` letting a second transaction claim a different row instead
//! of blocking on one an open transaction already holds (the classic
//! job-queue dequeue pattern this feature exists for), and `NOWAIT` failing
//! immediately with Postgres's own `55P03`/`LOCK_NOT_AVAILABLE` error
//! instead of blocking.
//!
//! Deliberately NOT covered here: plain `FOR UPDATE` (no `NOWAIT`/`SKIP
//! LOCKED`) actually blocking until the lock is released. Postgres's own
//! blocking behavior isn't something Pylon's compiler could get wrong (it
//! just emits the bare `FOR UPDATE` text — see `sql::tests::
//! test_for_update_defaults_to_blocking` for that), and proving a genuine
//! indefinite block live would need a timeout/race in the test itself,
//! trading a real reliability cost for coverage of Postgres's own
//! documented guarantee rather than Pylon's.
//!
//! Gated behind `#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]` and `PYLON_PGCON_TEST_DSN`, mirroring every
//! other file in this suite. Run with:
//!
//! ```text
//! PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test \
//!     cargo test -p pylon-core --test live_execution_locking -- --ignored
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

fn job_schema(module: &str) -> SchemaDescriptor {
    let job = ty(
        "Job",
        module,
        vec![id_prop(), text_prop("status"), int_prop("priority")],
    );
    SchemaDescriptor {
        types: vec![job],
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

fn field(row: &DecodedValue, i: usize) -> &DecodedValue {
    match row {
        DecodedValue::Composite(fields) => fields.get(i).unwrap_or(&DecodedValue::Null),
        other => panic!("expected a Composite-shaped row, got {other:?}"),
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
async fn skip_locked_lets_a_second_transaction_claim_a_different_row() {
    let module = unique_module("live_lock_skip");
    let sd = job_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Job {{ status := 'pending', priority := 1 }}"),
    )
    .await;
    exec(
        &pool,
        &sd,
        &format!("insert {module}::Job {{ status := 'pending', priority := 2 }}"),
    )
    .await;

    // The classic job-queue dequeue query: claim the next pending job,
    // skipping anything another worker already has locked.
    let pick_sql = query::compile(
        &format!(
            "select {module}::Job {{ priority }} filter .status = 'pending' \
             order by .priority asc limit 1 for update skip locked"
        ),
        &sd,
    )
    .unwrap()
    .sql;

    let tx1 = pool.begin_default().await.unwrap();
    let rows1 = tx1
        .query_typed(&pick_sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows1.len(), 1);
    assert_eq!(
        as_i64(field(&rows1[0], 1)),
        1,
        "tx1 should have claimed the lowest-priority pending job"
    );

    // tx1 deliberately has not committed yet — its lock on priority-1 is
    // still held. A second, concurrent transaction running the exact same
    // "claim the next pending job" query must skip that locked row rather
    // than blocking on it, and pick the other pending job instead.
    let tx2 = pool.begin_default().await.unwrap();
    let rows2 = tx2
        .query_typed(&pick_sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows2.len(), 1);
    assert_eq!(
        as_i64(field(&rows2[0], 1)),
        2,
        "tx2 must skip tx1's locked row and claim the other one"
    );

    // A third transaction now has nothing left to claim — both pending
    // jobs are locked (one by each open transaction).
    let tx3 = pool.begin_default().await.unwrap();
    let rows3 = tx3
        .query_typed(&pick_sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert!(rows3.is_empty(), "both pending jobs are already locked, got {rows3:?}");
    tx3.commit().await.unwrap();

    tx1.commit().await.unwrap();
    tx2.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn nowait_fails_immediately_instead_of_blocking_on_a_locked_row() {
    let module = unique_module("live_lock_nowait");
    let sd = job_schema(&module);
    let pool = test_pool().await;
    bootstrap(&pool, &sd).await;

    exec(
        &pool,
        &sd,
        &format!("insert {module}::Job {{ status := 'pending', priority := 1 }}"),
    )
    .await;

    let claim_sql = query::compile(
        &format!("select {module}::Job {{ priority }} filter .status = 'pending' for update"),
        &sd,
    )
    .unwrap()
    .sql;
    let claim_nowait_sql = query::compile(
        &format!("select {module}::Job {{ priority }} filter .status = 'pending' for update nowait"),
        &sd,
    )
    .unwrap()
    .sql;

    let tx1 = pool.begin_default().await.unwrap();
    let rows1 = tx1
        .query_typed(&claim_sql, &[], &ExtensionOids::default())
        .await
        .unwrap();
    assert_eq!(rows1.len(), 1, "tx1 should have locked the only pending job");

    // tx1 still holds the lock — a concurrent NOWAIT claim on the same row
    // must fail right away with Postgres's own lock_not_available error,
    // not block waiting for tx1 to finish.
    let tx2 = pool.begin_default().await.unwrap();
    let err = tx2
        .query_typed(&claim_nowait_sql, &[], &ExtensionOids::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.sqlstate(),
        Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE),
        "expected NOWAIT's own lock_not_available error, got: {err:?}",
    );

    tx1.commit().await.unwrap();
    // tx2's connection is left in an aborted transaction state after the
    // error above (any real statement on it now would raise "current
    // transaction is aborted") — roll it back rather than trying to commit.
    tx2.rollback().await.unwrap();
}
