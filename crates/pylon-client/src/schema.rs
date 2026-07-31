//! Fetches the schema snapshot from `_pylon."Schema"` — written by
//! `pylon migration apply` (and by `pylon migration watch`'s dev-mode sync)
//! via `pylon_core::migrate::write_schema_snapshot`, the same source
//! `pylon-lsp` (`crates/pylon-lsp/src/schema.rs`) polls — so this crate can
//! compile PyQL without an embedded Python interpreter of its own.
//!
//! A bare edit to a Python schema file has no effect here: nothing writes
//! this snapshot until a migration actually applies (or a dev-mode watch
//! syncs), by design — see `pylon_core::migrate::write_schema_snapshot`'s
//! doc comment. A `Client` fetches this once at construction — a
//! query-serving process doesn't want an extra round trip on every request
//! — and only re-fetches when a caller explicitly asks via
//! `Client::reload_schema()`.

use pylon_core::schema::SchemaDescriptor;

use crate::error::{Error, Result};

pub(crate) async fn fetch(pool: &pylon_pgcon::PgPool) -> Result<SchemaDescriptor> {
    let snapshot_json = pylon_core::migrate::read_schema_snapshot(pool)
        .await
        .map_err(|e| match e {
            pylon_core::migrate::MigrateError::Db(e) => Error::Db(e),
            pylon_core::migrate::MigrateError::Integrity(e) => {
                unreachable!("read_schema_snapshot never returns a migration-integrity error: {e}")
            }
        })?
        .ok_or(Error::NoSchemaSnapshot)?;
    Ok(serde_json::from_str(&snapshot_json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN").unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
    }

    async fn test_pool() -> pylon_pgcon::PgPool {
        let pool = pylon_pgcon::PgPool::connect(&test_dsn(), 5).await.unwrap();
        pool.batch_execute("CREATE SCHEMA IF NOT EXISTS _pylon").await.unwrap();
        pylon_core::migrate::ensure_tracking_tables(&pool).await.unwrap();
        pool
    }

    /// `_pylon."Schema"` is a shared singleton row across this whole test
    /// DSN — save and restore whatever was there before rather than leaving
    /// test data behind (see `pylon_core::migrate`'s own `schema_snapshot_round_trips`
    /// test for the identical concern).
    #[tokio::test]
    #[ignore]
    async fn fetches_a_valid_snapshot() {
        let pool = test_pool().await;
        let previous = pylon_core::migrate::read_schema_snapshot(&pool).await.unwrap();

        let snapshot = serde_json::to_string(&SchemaDescriptor::default()).unwrap();
        pylon_core::migrate::write_schema_snapshot(&pool, &snapshot).await.unwrap();

        assert!(fetch(&pool).await.is_ok());

        match previous {
            Some(prior) => pylon_core::migrate::write_schema_snapshot(&pool, &prior).await.unwrap(),
            None => pool.batch_execute(r#"DELETE FROM _pylon."Schema""#).await.unwrap(),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn missing_snapshot_is_a_clear_error() {
        let pool = test_pool().await;
        let previous = pylon_core::migrate::read_schema_snapshot(&pool).await.unwrap();
        pool.batch_execute(r#"DELETE FROM _pylon."Schema""#).await.unwrap();

        assert!(matches!(fetch(&pool).await, Err(Error::NoSchemaSnapshot)));

        if let Some(prior) = previous {
            pylon_core::migrate::write_schema_snapshot(&pool, &prior).await.unwrap();
        }
    }
}
