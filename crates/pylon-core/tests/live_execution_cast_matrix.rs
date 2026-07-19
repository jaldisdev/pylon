//! Cast/operator matrix live-execution tests. See `live_execution_smoke.rs`
//! for the harness's purpose and how to run these (same pattern, this
//! binary is `--test live_execution_cast_matrix`).
//!
//! Regression coverage for the bug where `types_compatible()` rejected mixed
//! int/float and int/decimal arithmetic at compile time even though Postgres
//! itself accepts the generated SQL fine (see `ir/compiler.rs::types_compatible`
//! and its `sql/mod.rs` snapshot tests). These live-execution tests go one
//! step further than the snapshot tests: they confirm the SQL Postgres
//! receives not only parses but returns the numerically correct value.

mod common;

use common::*;
use pylon_value::CachedValue;

#[tokio::test]
#[ignore]
async fn mixed_int_and_float_arithmetic_returns_the_correct_value() {
    let pool = test_pool().await;
    assert_eq!(eval_scalar(&pool, "<int16>1 + <float32>2.0").await, CachedValue::F64(3.0));
    assert_eq!(eval_scalar(&pool, "<float64>1.5 + <int64>2").await, CachedValue::F64(3.5));
}

#[tokio::test]
#[ignore]
async fn mixed_int_and_decimal_arithmetic_returns_the_correct_value() {
    let pool = test_pool().await;
    assert_eq!(eval_scalar(&pool, "<int64>1 + <decimal>2.5").await, CachedValue::Decimal("3.5".to_string()));
    assert_eq!(eval_scalar(&pool, "<decimal>10 - <int16>3").await, CachedValue::Decimal("7".to_string()));
}

#[tokio::test]
#[ignore]
async fn same_family_arithmetic_still_returns_the_correct_value() {
    // Guards against the fix accidentally changing behavior for the
    // already-working same-family cases (int-int, float-float).
    let pool = test_pool().await;
    assert_eq!(eval_scalar(&pool, "<int16>1 + <int64>2").await, CachedValue::I64(3));
    assert_eq!(eval_scalar(&pool, "<float32>1.5 + <float64>2.5").await, CachedValue::F64(4.0));
}
