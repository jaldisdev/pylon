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

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn the_natural_logarithm_of_a_decimal_stays_a_decimal() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "math::ln(<decimal>1)").await,
        DecodedValue::Decimal("0.0000000000000000".to_string())
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn the_range_accessors_read_a_multirange() {
    let pool = stdlib_pool().await;
    let one_to_five = "multirange([range(<int64>1, <int64>5)])";
    assert_eq!(
        eval_scalar(&pool, &format!("range_get_lower({one_to_five})")).await,
        DecodedValue::I64(1)
    );
    assert_eq!(
        eval_scalar(&pool, &format!("range_get_upper({one_to_five})")).await,
        DecodedValue::I64(5)
    );
    assert_eq!(
        eval_scalar(&pool, &format!("range_is_empty({one_to_five})")).await,
        DecodedValue::Bool(false)
    );
    assert_eq!(
        eval_scalar(&pool, &format!("range_is_inclusive_lower({one_to_five})")).await,
        DecodedValue::Bool(true)
    );
    assert_eq!(
        eval_scalar(&pool, &format!("range_is_inclusive_upper({one_to_five})")).await,
        DecodedValue::Bool(false)
    );
    assert_eq!(
        eval_scalar(
            &pool,
            &format!("overlaps({one_to_five}, multirange([range(<int64>4, <int64>9)]))")
        )
        .await,
        DecodedValue::Bool(true)
    );
    assert_eq!(
        eval_scalar(
            &pool,
            &format!("overlaps({one_to_five}, multirange([range(<int64>5, <int64>9)]))")
        )
        .await,
        DecodedValue::Bool(false)
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn find_locates_a_byte_sequence() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "find(<bytes>'abc', <bytes>'b')").await,
        DecodedValue::I64(1)
    );
    assert_eq!(
        eval_scalar(&pool, "find(<bytes>'abc', <bytes>'z')").await,
        DecodedValue::I64(-1)
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn find_from_a_position_skips_the_earlier_occurrences() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "find([1, 2, 3, 2], 2, 0)").await,
        DecodedValue::I64(1)
    );
    assert_eq!(
        eval_scalar(&pool, "find([1, 2, 3, 2], 2, 2)").await,
        DecodedValue::I64(3)
    );
    assert_eq!(
        eval_scalar(&pool, "find([1, 2, 3, 2], 2, 4)").await,
        DecodedValue::I64(-1)
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn bit_count_counts_the_set_bits_in_bytes() {
    let pool = stdlib_pool().await;
    assert_eq!(eval_scalar(&pool, "bit_count(<bytes>'ab')").await, DecodedValue::I64(6));
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn json_get_falls_back_to_the_default_it_is_given() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "json_get(to_json('{\"a\": 1}'), 'a', default := to_json('7'))").await,
        DecodedValue::I64(1)
    );
    assert_eq!(
        eval_scalar(&pool, "json_get(to_json('{}'), 'a', default := to_json('7'))").await,
        DecodedValue::I64(7)
    );
    assert_eq!(
        eval_scalar(
            &pool,
            "json_get(to_json('{\"a\": {\"b\": 2}}'), 'a', 'b', default := to_json('7'))"
        )
        .await,
        DecodedValue::I64(2)
    );
    assert_eq!(
        eval_scalar(&pool, "json_get(to_json('{}'), 'a', 'b')").await,
        DecodedValue::Null
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn json_set_writes_the_value_at_a_path_of_any_depth() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "json_set(to_json('{\"a\": 1}'), 'a', value := to_json('2'))").await,
        DecodedValue::Object(vec![("a".to_string(), DecodedValue::I64(2))])
    );
    assert_eq!(
        eval_scalar(
            &pool,
            "json_set(to_json('{\"a\": {\"b\": 1}}'), 'a', 'b', value := to_json('2'))"
        )
        .await,
        DecodedValue::Object(vec![(
            "a".to_string(),
            DecodedValue::Object(vec![("b".to_string(), DecodedValue::I64(2))])
        )])
    );
}

/// A missing key is only created when the call says it may be.
#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn json_set_honours_create_if_missing() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "json_set(to_json('{}'), 'a', value := to_json('2'))").await,
        DecodedValue::Object(vec![("a".to_string(), DecodedValue::I64(2))])
    );
    assert_eq!(
        eval_scalar(
            &pool,
            "json_set(to_json('{}'), 'a', value := to_json('2'), create_if_missing := false)"
        )
        .await,
        DecodedValue::Object(vec![])
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn json_set_treats_an_empty_value_as_the_call_asks() {
    let pool = stdlib_pool().await;
    let target = "to_json('{\"a\": 1}')";
    assert_eq!(
        eval_scalar(&pool, &format!("json_set({target}, 'a', value := <json>{{}})")).await,
        DecodedValue::Null
    );
    assert_eq!(
        eval_scalar(
            &pool,
            &format!(
                "json_set({target}, 'a', value := <json>{{}}, \
                 empty_treatment := JsonEmpty.ReturnTarget)"
            )
        )
        .await,
        DecodedValue::Object(vec![("a".to_string(), DecodedValue::I64(1))])
    );
    assert_eq!(
        eval_scalar(
            &pool,
            &format!("json_set({target}, 'a', value := <json>{{}}, empty_treatment := JsonEmpty.UseNull)")
        )
        .await,
        DecodedValue::Object(vec![("a".to_string(), DecodedValue::Null)])
    );
    assert_eq!(
        eval_scalar(
            &pool,
            &format!("json_set({target}, 'a', value := <json>{{}}, empty_treatment := JsonEmpty.DeleteKey)")
        )
        .await,
        DecodedValue::Object(vec![])
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn array_get_falls_back_to_the_default_it_is_given() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "array_get([1, 2], 1, default := 9)").await,
        DecodedValue::I64(2)
    );
    assert_eq!(
        eval_scalar(&pool, "array_get([1, 2], 5, default := 9)").await,
        DecodedValue::I64(9)
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn re_replace_takes_its_flags_by_name() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "re_replace('a', 'X', 'aaa')").await,
        DecodedValue::Str("Xaa".to_string())
    );
    assert_eq!(
        eval_scalar(&pool, "re_replace('a', 'X', 'aaa', flags := 'g')").await,
        DecodedValue::Str("XXX".to_string())
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via PYLON_PGCON_TEST_DSN"]
async fn array_join_concatenates_bytes_with_a_delimiter() {
    let pool = stdlib_pool().await;
    assert_eq!(
        eval_scalar(&pool, "array_join([<bytes>'a', <bytes>'b'], <bytes>'x')").await,
        DecodedValue::Bytes(b"axb".to_vec())
    );
}
