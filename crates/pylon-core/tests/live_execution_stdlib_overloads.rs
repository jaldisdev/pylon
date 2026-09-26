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

//! Live-Postgres coverage for stdlib overloads that a compile-time test alone
//! cannot vouch for. See `live_execution_smoke.rs` for the harness's purpose
//! and how to run these (this binary is `--test live_execution_stdlib_overloads`).
//!
//! An overload resolving is not the same as an overload working: the registry
//! names a PostgreSQL function or expression, and nothing before this file
//! proved that the named one exists with the argument types the call reaches
//! it with. Every value below is the one the expression is defined to yield,
//! so a resolvable-but-unexecutable entry fails here rather than in a caller's
//! query.

mod common;

use common::*;
use pylon_pgcon::PgPool;
use pylon_value::DecodedValue;

async fn stdlib_pool() -> PgPool {
    let pool = test_pool().await;
    pool.batch_execute(&pylon_core::stdlib::export_stdlib()).await.unwrap();
    pool
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_numeric_parser_reads_a_formatted_string() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "to_int16('1,234', '9G999')").await,
        DecodedValue::I64(1234)
    );
    assert_eq!(
        eval_scalar(&pool, "to_int32('1,234', '9G999')").await,
        DecodedValue::I64(1234)
    );
    assert_eq!(
        eval_scalar(&pool, "to_int64('1,234,567', '9G999G999')").await,
        DecodedValue::I64(1234567)
    );
    assert_eq!(
        eval_scalar(&pool, "to_float32('1234.5', '9999.9')").await,
        DecodedValue::F64(1234.5)
    );
    assert_eq!(
        eval_scalar(&pool, "to_float64('1234.5', '9999.9')").await,
        DecodedValue::F64(1234.5)
    );
    assert_eq!(
        eval_scalar(&pool, "to_decimal('1234.5', '9999.9')").await,
        DecodedValue::Decimal("1234.5".to_string())
    );
    assert_eq!(
        eval_scalar(&pool, "to_bigint('1,234', '9G999')").await,
        DecodedValue::Decimal("1234".to_string())
    );
}

/// Left out, the format is not applied at all — the string parses the way the
/// single-argument overload parses it.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn a_numeric_parser_given_no_format_reads_the_plain_string() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "to_int64('1234', <str>{})").await,
        DecodedValue::I64(1234)
    );
    assert_eq!(
        eval_scalar(&pool, "to_decimal('12.5', <str>{})").await,
        DecodedValue::Decimal("12.5".to_string())
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn to_bytes_encodes_a_string_and_a_json_value_as_utf8() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "to_bytes('ab')").await,
        DecodedValue::Bytes(b"ab".to_vec())
    );
    assert_eq!(
        eval_scalar(&pool, "to_bytes(to_json('42'))").await,
        DecodedValue::Bytes(b"42".to_vec())
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn to_bytes_writes_an_integer_in_the_byte_order_it_is_given() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "to_bytes(<int16>-2, Endian.Big)").await,
        DecodedValue::Bytes(vec![0xff, 0xfe])
    );
    assert_eq!(
        eval_scalar(&pool, "to_bytes(<int16>-2, Endian.Little)").await,
        DecodedValue::Bytes(vec![0xfe, 0xff])
    );
    assert_eq!(
        eval_scalar(&pool, "to_bytes(<int32>1, Endian.Big)").await,
        DecodedValue::Bytes(vec![0x00, 0x00, 0x00, 0x01])
    );
    assert_eq!(
        eval_scalar(&pool, "to_bytes(<int32>1, Endian.Little)").await,
        DecodedValue::Bytes(vec![0x01, 0x00, 0x00, 0x00])
    );
    assert_eq!(
        eval_scalar(&pool, "to_bytes(<int64>-1, Endian.Big)").await,
        DecodedValue::Bytes(vec![0xff; 8])
    );
}

/// The two directions are each other's inverse, negative values included.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn an_integer_survives_a_round_trip_through_bytes() {
    let pool = stdlib_pool().await;
    for endian in ["Endian.Big", "Endian.Little"] {
        assert_eq!(
            eval_scalar(&pool, &format!("to_int16(to_bytes(<int16>-25001, {endian}), {endian})")).await,
            DecodedValue::I64(-25001)
        );
        assert_eq!(
            eval_scalar(
                &pool,
                &format!("to_int32(to_bytes(<int32>-1638451077, {endian}), {endian})")
            )
            .await,
            DecodedValue::I64(-1638451077)
        );
        assert_eq!(
            eval_scalar(&pool, &format!("to_int64(to_bytes(<int64>-7, {endian}), {endian})")).await,
            DecodedValue::I64(-7)
        );
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn to_datetime_reads_epoch_seconds_as_a_float() {
    let pool = stdlib_pool().await;
    // 2001-09-09T01:46:40.5Z, half a second past a round unix billion, in
    // microseconds from the 2000-01-01 epoch the wire counts from.
    assert_eq!(
        eval_scalar(&pool, "to_datetime(<float64>1000000000.5)").await,
        DecodedValue::Timestamptz(53_315_200_500_000)
    );
}
