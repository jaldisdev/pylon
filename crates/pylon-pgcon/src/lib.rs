//! Postgres connection pooling and query execution, via `tokio-postgres` +
//! `deadpool-postgres`. No dependency on `pylon-core` or PyO3 — usable as a
//! plain Rust Postgres client crate on its own; `pylon-core` depends on
//! *this* crate for execution, not the other way around.
//!
//! `query_raw` returns raw `tokio_postgres::Row`s; `wire` decodes the
//! composite `result` column those rows carry into `pylon_value::CachedValue`
//! (the shared decode target `pylon-cache` also stores). Typed parameter
//! binding lands in a later phase.

pub mod wire;

pub use wire::{decode_value, ExtensionOids};

use pylon_value::CachedValue;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Captures a column's raw wire bytes regardless of its declared Postgres
/// type. `tokio_postgres`'s own `&[u8]` `FromSql` impl only accepts
/// `BYTEA` — the `result` column pylon-core emits is always `record`
/// (OID 2249), so a plain `row.get::<_, &[u8]>(0)` would panic on the type
/// check. This wrapper's `accepts` is unconditionally `true`, matching the
/// well-established pattern for pulling raw bytes out of any column.
struct RawBytes<'a>(&'a [u8]);

impl<'a> postgres_types::FromSql<'a> for RawBytes<'a> {
    fn from_sql(
        _ty: &postgres_types::Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawBytes(raw))
    }

    fn accepts(_ty: &postgres_types::Type) -> bool {
        true
    }
}

/// Wraps a `CachedValue` for binding as a query parameter. `accepts` is
/// unconditionally `true` (mirroring `RawBytes` above) because the target
/// `Type` isn't known until `Statement::params()` reports it — see
/// `wire::encode_value`, which does the actual type-directed encoding.
#[derive(Debug)]
struct BoundParam<'a>(&'a CachedValue);

impl postgres_types::ToSql for BoundParam<'_> {
    fn to_sql(
        &self,
        ty: &postgres_types::Type,
        out: &mut bytes::BytesMut,
    ) -> std::result::Result<postgres_types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        wire::encode_value(self.0, ty, out)
    }

    fn accepts(_ty: &postgres_types::Type) -> bool {
        true
    }

    postgres_types::to_sql_checked!();
}

// `deadpool_postgres::Pool` is `Arc`-backed internally, so cloning a
// `PgPool` is cheap and shares the same underlying pool — needed at the
// pyo3 boundary, where a lock guard over the process-global pool slot
// can't be held across an `.await` (it isn't `Send`), so callers clone the
// pool out from under the lock first.
#[derive(Clone)]
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

    /// Column 0 of every row as `i64` — a temporary, narrowly-scoped
    /// convenience for validating the pyo3 async boundary (phase 3 of the
    /// driver migration) before the real composite/record decoder exists.
    /// Callers outside that validation path should prefer `query_raw` (or,
    /// once it lands, the `CachedValue`-decoding path).
    pub async fn query_scalar_i64(&self, sql: &str) -> Result<Vec<i64>> {
        let rows = self.query_raw(sql).await?;
        Ok(rows.iter().map(|row| row.get::<_, i64>(0)).collect())
    }

    /// Runs `sql` (expected to produce exactly one column, matching
    /// pylon-core's `SELECT (...) AS result` emission) and decodes that
    /// column of every row via `wire::decode_value`, using its actual
    /// declared Postgres type (not assumed to be `record` — a bare scalar
    /// `result` column decodes just as well through the same path).
    pub async fn query_composite(&self, sql: &str, ext: &ExtensionOids) -> Result<Vec<CachedValue>> {
        let rows = self.query_raw(sql).await?;
        rows.iter().map(|row| decode_result_column(row, ext)).collect()
    }

    /// Runs `sql` with bound `params`, matched positionally to `$1, $2, ...`
    /// — the same convention `pylon-core`'s `param_names` already assumes.
    /// No caller-supplied parameter types: `prepare` asks Postgres itself
    /// to analyze the SQL and report each placeholder's expected `Type`
    /// (`Statement::params()`), which drives `wire::encode_value`'s
    /// encoding directly. Decodes the single result column exactly like
    /// `query_composite`.
    pub async fn query_typed(
        &self,
        sql: &str,
        params: &[CachedValue],
        ext: &ExtensionOids,
    ) -> Result<Vec<CachedValue>> {
        let client = self.pool.get().await?;
        let stmt = client.prepare(sql).await?;
        let bound: Vec<BoundParam<'_>> = params.iter().map(BoundParam).collect();
        let param_refs: Vec<&(dyn postgres_types::ToSql + Sync)> =
            bound.iter().map(|p| p as &(dyn postgres_types::ToSql + Sync)).collect();
        let rows = client.query(&stmt, &param_refs).await?;
        rows.iter().map(|row| decode_result_column(row, ext)).collect()
    }
}

/// Decodes a row's column 0 (the `result` column pylon-core's SQL always
/// projects) into a `CachedValue`, using its actual declared Postgres type.
/// Column 0 itself can be SQL NULL at the top level (not just a NULL
/// *field within* a composite, which `decode_record`/`decode_array`
/// already handle) — `RawBytes` has no `from_sql_null` override, so a
/// direct `row.try_get::<_, RawBytes>(0)` errors on a null column; going
/// through `Option<RawBytes>` (which `postgres_types` implements generically
/// for any `T: FromSql`, yielding `None` for SQL NULL) avoids that.
fn decode_result_column(row: &tokio_postgres::Row, ext: &ExtensionOids) -> Result<CachedValue> {
    let oid = row.columns()[0].type_().oid();
    match row.try_get::<_, Option<RawBytes>>(0)? {
        None => Ok(CachedValue::Null),
        Some(RawBytes(bytes)) => wire::decode_value(oid, bytes, ext),
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
    async fn query_scalar_i64_casts_to_the_right_width() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let values = pool.query_scalar_i64("SELECT 42::int8").await.unwrap();
        assert_eq!(values, vec![42]);
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

    // ── query_composite against real Postgres wire data ─────────────────
    //
    // The wire::tests module already checks decode_value against
    // hand-crafted byte buffers — these tests instead run real SQL through
    // a real connection, so any mismatch between my assumptions about
    // Postgres's binary format and what Postgres actually sends shows up
    // here, not just in self-consistent hand-rolled fixtures.

    #[tokio::test]
    #[ignore]
    async fn decodes_a_bare_scalar_result_column() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let rows = pool.query_composite("SELECT 42::int8 AS result", &ExtensionOids::default()).await.unwrap();
        assert_eq!(rows, vec![CachedValue::I64(42)]);
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_a_composite_matching_pylon_cores_own_emission_shape() {
        // Mirrors exactly what `sql/mod.rs::emit_bound_select` emits:
        // `SELECT (type_disc, col1, col2, ...) AS result`.
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "SELECT ('Person'::text, 'Alice'::text, 30::int8, NULL::text) AS result";
        let rows = pool.query_composite(sql, &ExtensionOids::default()).await.unwrap();
        assert_eq!(
            rows,
            vec![CachedValue::Array(vec![
                CachedValue::Str("Person".into()),
                CachedValue::Str("Alice".into()),
                CachedValue::I64(30),
                CachedValue::Null,
            ])]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_nested_composite_and_array_of_composite_for_real() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "SELECT (\
            'Product'::text, \
            ROW('Tag'::text, 'sale'::text), \
            ARRAY[ROW(1::int8), ROW(2::int8)]::record[]\
        ) AS result";
        let rows = pool.query_composite(sql, &ExtensionOids::default()).await.unwrap();
        assert_eq!(
            rows,
            vec![CachedValue::Array(vec![
                CachedValue::Str("Product".into()),
                CachedValue::Array(vec![CachedValue::Str("Tag".into()), CachedValue::Str("sale".into())]),
                CachedValue::Array(vec![
                    CachedValue::Array(vec![CachedValue::I64(1)]),
                    CachedValue::Array(vec![CachedValue::I64(2)]),
                ]),
            ])]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_array_of_text_for_real() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "SELECT (ARRAY['a', 'b', NULL]::text[]) AS result";
        let rows = pool.query_composite(sql, &ExtensionOids::default()).await.unwrap();
        assert_eq!(
            rows,
            vec![CachedValue::Array(vec![
                CachedValue::Str("a".into()),
                CachedValue::Str("b".into()),
                CachedValue::Null,
            ])]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_numeric_and_jsonb_and_uuid_for_real() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "SELECT (\
            12.50::numeric, \
            '{\"a\": 1, \"b\": [1,2]}'::jsonb, \
            '11111111-1111-1111-1111-111111111111'::uuid\
        ) AS result";
        let rows = pool.query_composite(sql, &ExtensionOids::default()).await.unwrap();
        let CachedValue::Array(fields) = &rows[0] else { panic!("expected Array") };
        assert_eq!(fields[0], CachedValue::Decimal("12.50".to_string()));
        assert_eq!(
            fields[1],
            CachedValue::Object(vec![
                ("a".into(), CachedValue::I64(1)),
                ("b".into(), CachedValue::Array(vec![CachedValue::I64(1), CachedValue::I64(2)])),
            ])
        );
        assert_eq!(fields[2], CachedValue::Uuid([0x11; 16]));
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_bytea_for_real() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let rows = pool.query_composite("SELECT '\\xdeadbeef'::bytea AS result", &ExtensionOids::default()).await.unwrap();
        assert_eq!(rows, vec![CachedValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])]);
    }

    #[tokio::test]
    #[ignore]
    async fn decodes_enum_cast_to_text_for_real() {
        // pylon-core always ::text-casts enum-typed columns (sql/mod.rs's
        // emit_scalar) specifically so the runtime-assigned enum OID never
        // needs to be known statically — this is the actually-exercised path.
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "DO $$ BEGIN CREATE TYPE pgcon_test_enum AS ENUM ('a', 'b'); \
                   EXCEPTION WHEN duplicate_object THEN NULL; END $$;";
        pool.query_raw(sql).await.ok();
        let rows = pool
            .query_composite("SELECT ('a'::pgcon_test_enum::text) AS result", &ExtensionOids::default())
            .await
            .unwrap();
        assert_eq!(rows, vec![CachedValue::Str("a".to_string())]);
    }

    // ── query_typed: bound-parameter round trips against real Postgres ──
    //
    // Each test binds a CachedValue as $1, has Postgres echo it straight
    // back out (so both encode_value AND decode_value are exercised in one
    // pass — a mismatch in either direction fails the assertion), matching
    // exactly how a real PyQL query binds a param and gets a result back.

    async fn round_trip(pool: &PgPool, pg_type: &str, param: CachedValue) -> CachedValue {
        let sql = format!("SELECT ($1::{pg_type}) AS result");
        let rows = pool.query_typed(&sql, &[param], &ExtensionOids::default()).await.unwrap();
        rows.into_iter().next().unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_bool_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(round_trip(&pool, "bool", CachedValue::Bool(true)).await, CachedValue::Bool(true));
        assert_eq!(round_trip(&pool, "bool", CachedValue::Bool(false)).await, CachedValue::Bool(false));
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_integer_params_at_every_width() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(round_trip(&pool, "int2", CachedValue::I64(30)).await, CachedValue::I64(30));
        assert_eq!(round_trip(&pool, "int4", CachedValue::I64(70_000)).await, CachedValue::I64(70_000));
        assert_eq!(
            round_trip(&pool, "int8", CachedValue::I64(9_223_372_036_854_775_807)).await,
            CachedValue::I64(9_223_372_036_854_775_807)
        );
        assert_eq!(round_trip(&pool, "int8", CachedValue::I64(-1)).await, CachedValue::I64(-1));
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_float_params() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(round_trip(&pool, "float4", CachedValue::F64(1.5)).await, CachedValue::F64(1.5));
        assert_eq!(round_trip(&pool, "float8", CachedValue::F64(2.25)).await, CachedValue::F64(2.25));
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_text_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(
            round_trip(&pool, "text", CachedValue::Str("héllo 🎉".to_string())).await,
            CachedValue::Str("héllo 🎉".to_string())
        );
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_bytea_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(
            round_trip(&pool, "bytea", CachedValue::Bytes(vec![1, 2, 3, 255])).await,
            CachedValue::Bytes(vec![1, 2, 3, 255])
        );
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_uuid_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let bytes = [0x11u8; 16];
        assert_eq!(round_trip(&pool, "uuid", CachedValue::Uuid(bytes)).await, CachedValue::Uuid(bytes));
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_numeric_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(
            round_trip(&pool, "numeric", CachedValue::Decimal("12.50".to_string())).await,
            CachedValue::Decimal("12.50".to_string())
        );
        assert_eq!(
            round_trip(&pool, "numeric", CachedValue::Decimal("-9999.001".to_string())).await,
            CachedValue::Decimal("-9999.001".to_string())
        );
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_null_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        assert_eq!(round_trip(&pool, "int8", CachedValue::Null).await, CachedValue::Null);
        assert_eq!(round_trip(&pool, "text", CachedValue::Null).await, CachedValue::Null);
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_array_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let param = CachedValue::Array(vec![
            CachedValue::Str("a".into()),
            CachedValue::Str("b".into()),
            CachedValue::Null,
        ]);
        assert_eq!(round_trip(&pool, "text[]", param.clone()).await, param);
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_int_array_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let param = CachedValue::Array(vec![CachedValue::I64(1), CachedValue::I64(2), CachedValue::I64(3)]);
        assert_eq!(round_trip(&pool, "int8[]", param.clone()).await, param);
    }

    #[tokio::test]
    #[ignore]
    async fn round_trips_jsonb_object_param() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let param = CachedValue::Object(vec![
            ("a".into(), CachedValue::I64(1)),
            ("b".into(), CachedValue::Str("two".into())),
            ("c".into(), CachedValue::Array(vec![CachedValue::I64(1), CachedValue::I64(2)])),
        ]);
        assert_eq!(round_trip(&pool, "jsonb", param.clone()).await, param);
    }

    #[tokio::test]
    #[ignore]
    async fn query_typed_matches_pylon_cores_own_param_binding_convention() {
        // $1, $2, ... positional, matching multiple params in one query —
        // the same shape a real PyQL query with several kwargs produces.
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let sql = "SELECT ($1::text, $2::int8, $3::bool) AS result";
        let params = vec![CachedValue::Str("Alice".into()), CachedValue::I64(30), CachedValue::Bool(true)];
        let rows = pool.query_typed(sql, &params, &ExtensionOids::default()).await.unwrap();
        assert_eq!(
            rows,
            vec![CachedValue::Array(vec![
                CachedValue::Str("Alice".into()),
                CachedValue::I64(30),
                CachedValue::Bool(true),
            ])]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn wrong_param_count_returns_an_error_not_a_panic() {
        let pool = PgPool::connect(&test_dsn(), 5).await.unwrap();
        let result = pool.query_typed("SELECT $1::int8, $2::int8", &[CachedValue::I64(1)], &ExtensionOids::default()).await;
        assert!(result.is_err());
    }
}
