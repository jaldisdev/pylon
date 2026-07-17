//! Postgres connection pooling and query execution, via `tokio-postgres` +
//! `deadpool-postgres`. No dependency on `pylon-core` or PyO3 — usable as a
//! plain Rust Postgres client crate on its own; `pylon-core` depends on
//! *this* crate for execution, not the other way around.
//!
//! This is the foundation phase only: connect + run a query, get raw
//! `tokio_postgres::Row`s back. Composite/record decoding into
//! `pylon_value::CachedValue` (the shared decode target `pylon-cache` also
//! stores) and typed parameter binding land in later phases.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct PgPool {
    pool: deadpool_postgres::Pool,
}

impl PgPool {
    /// Connects using a `postgresql://` DSN, matching the DSN Python's
    /// `DatabaseConfig`/`_build_dsn` already produces today. No TLS support
    /// yet — no SSL/TLS surface exists anywhere in the project currently
    /// (confirmed by a full-repo grep during planning), so this isn't a
    /// regression; it's simply not needed until it is.
    pub async fn connect(dsn: &str, max_size: usize) -> Result<Self> {
        let pg_config: tokio_postgres::Config = dsn.parse()?;
        let manager = deadpool_postgres::Manager::new(pg_config, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(manager)
            .max_size(max_size)
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .build()?;
        Ok(Self { pool })
    }

    /// Executes `sql` with no parameters and returns the raw rows.
    /// Composite/record decoding is a later phase — this only proves the
    /// pool can connect and round-trip a query end to end.
    pub async fn query_raw(&self, sql: &str) -> Result<Vec<tokio_postgres::Row>> {
        let client = self.pool.get().await?;
        let rows = client.query(sql, &[]).await?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Postgres required — the dockerized demo DB used throughout this
    /// session (`~/Development/pylon-demo`, `docker compose up`), overridable
    /// via `PYLON_PGCON_TEST_DSN`. Not run by default (`cargo test -- --ignored`
    /// to opt in) so the default test run stays hermetic.
    fn test_dsn() -> String {
        std::env::var("PYLON_PGCON_TEST_DSN")
            .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5418/app".to_string())
    }

    #[tokio::test]
    #[ignore]
    async fn connects_and_round_trips_a_scalar_query() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let rows = pool.query_raw("SELECT 1 + 1").await.unwrap();
        assert_eq!(rows.len(), 1);
        let value: i32 = rows[0].get(0);
        assert_eq!(value, 2);
    }

    #[tokio::test]
    #[ignore]
    async fn pool_is_reused_across_multiple_queries() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        for i in 0..5 {
            let rows = pool.query_raw(&format!("SELECT {i}")).await.unwrap();
            let value: i32 = rows[0].get(0);
            assert_eq!(value, i);
        }
    }

    #[tokio::test]
    #[ignore]
    async fn invalid_dsn_fails_to_connect() {
        let result = PgPool::connect("not-a-valid-dsn", 5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn bad_sql_returns_an_error_not_a_panic() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let result = pool.query_raw("SELECT this is not valid sql").await;
        assert!(result.is_err());
    }
}
