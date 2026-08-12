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
//! Keeps partitioned tables ahead of incoming writes.
//!
//! pg_partman creates a table's partitions when `create_parent` first runs
//! and then does nothing further on its own — `run_maintenance` is what
//! creates the next ranges and drops ones past retention, and something has
//! to call it. Nothing does by default, which is a quiet failure: everything
//! works until the day writes reach a range nobody created, and then every
//! insert past that boundary fails at once.
//!
//! Unlike the index workers this is a plain timer, with no LISTEN/NOTIFY
//! wakeup — nothing *happens* that should trigger maintenance. What matters
//! is only that it runs comfortably more often than one partition interval,
//! so `premake` never gets a chance to run out.

use std::time::Duration;

use pylon_pgcon::PgPool;

use crate::error::Result;

/// How often to run maintenance.
///
/// One hour against a daily interval — the shortest partition width Pylon
/// supports — leaves 24 maintenance passes per partition. Even at
/// `premake = 1` the worker would have to miss almost a full day of runs
/// before a write could reach an uncreated range.
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);

/// `p_analyze := false` because analyzing every partition on every pass is
/// far more expensive than the maintenance itself, and autovacuum already
/// handles statistics.
const RUN_MAINTENANCE_SQL: &str = "CALL partman.run_maintenance_proc(p_analyze := false)";

/// True when this database has pg_partman installed. Maintenance is skipped
/// entirely otherwise, rather than failing once an hour forever.
pub async fn partman_installed(pool: &PgPool) -> Result<bool> {
    let rows = pool
        .query_typed(
            "SELECT (EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_partman')) AS result",
            &[],
            pool.types(),
        )
        .await?;
    Ok(matches!(rows.first(), Some(pylon_value::DecodedValue::Bool(true))))
}

/// Runs one maintenance pass.
pub async fn run_maintenance(pool: &PgPool) -> Result<()> {
    pool.batch_execute(RUN_MAINTENANCE_SQL).await?;
    Ok(())
}

/// Runs maintenance every `interval`, forever.
///
/// Runs once immediately on start rather than waiting out the first
/// interval: a process that has just come up may have been down long enough
/// for maintenance to be overdue already, and that is exactly when the gap
/// matters.
///
/// A failed pass is logged and the loop continues. The alternative — exiting
/// — would mean one transient error silently stops all future maintenance,
/// which is the failure this worker exists to prevent.
pub async fn run(dsn: &str, interval: Duration) -> Result<()> {
    let pool = PgPool::connect(dsn, 1).await?;

    if !partman_installed(&pool).await? {
        eprintln!("PartitionMaintenanceWorker: pg_partman is not installed; nothing to maintain");
        return Ok(());
    }

    loop {
        match run_maintenance(&pool).await {
            Ok(()) => {}
            Err(e) => eprintln!("PartitionMaintenanceWorker: maintenance pass failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_runs_far_more_often_than_the_shortest_interval() {
        // The guarantee the default rests on: many passes per partition, so
        // a few missed runs can't exhaust `premake`.
        let daily = Duration::from_secs(24 * 3600);
        assert!(
            DEFAULT_MAINTENANCE_INTERVAL * 10 < daily,
            "maintenance must run at least 10x per partition interval"
        );
    }

    #[test]
    fn maintenance_call_does_not_analyze() {
        assert!(RUN_MAINTENANCE_SQL.contains("p_analyze := false"));
    }

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").expect("PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests")
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
    async fn reports_whether_partman_is_installed() {
        let pool = PgPool::connect(&test_dsn(), 1).await.unwrap();
        // Either answer is correct — the point is that the probe itself runs
        // without erroring on a database that doesn't have the extension,
        // which is what lets `run` skip cleanly instead of failing forever.
        let installed = partman_installed(&pool).await.unwrap();
        if installed {
            run_maintenance(&pool).await.unwrap();
        }
    }
}
