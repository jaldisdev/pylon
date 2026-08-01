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

use super::{CastEntry, CastStrategy};
use crate::stdlib::PylonType::{
    BigInt, Bool, Datetime, Decimal, Duration, Float32, Float64, Int16, Int32, Int64, Json,
    LocalDate, LocalDatetime, LocalTime, Str, Uuid,
};
use CastStrategy::{Function as Fn, Implicit, Sql};

fn c(source: impl Into<crate::stdlib::PylonType>, target: impl Into<crate::stdlib::PylonType>, strategy: CastStrategy) -> CastEntry {
    CastEntry { source: source.into(), target: target.into(), strategy }
}

pub(super) fn build() -> Vec<CastEntry> {
    vec![
        // ── Implicit casts ───────────────────────────────────────────────────
        // Inserted silently during type inference; PG handles these natively.
        c(Int16, Int32,   Implicit),
        c(Int16, Int64,   Implicit),
        c(Int16, Float32, Implicit),
        c(Int16, Float64, Implicit),
        c(Int16, Decimal, Implicit),
        c(Int16, BigInt,  Implicit),
        c(Int32, Int64,   Implicit),
        c(Int32, Float64, Implicit),
        c(Int32, Decimal, Implicit),
        c(Int32, BigInt,  Implicit),
        c(Int64, Decimal, Implicit),
        c(Int64, BigInt,  Implicit),
        c(Float32, Float64, Implicit),

        // ── Sql casts ────────────────────────────────────────────────────────
        // User writes `<target>expr`; transpiler emits `expr::pg_type`.
        // Narrowing casts may overflow at runtime (PG default behaviour).
        c(Int64,         Int32,     Sql("int4")),
        c(Int64,         Int16,     Sql("int2")),
        c(Int32,         Int16,     Sql("int2")),
        c(Float64,       Float32,   Sql("float4")),
        c(Float64,       Int64,     Sql("int8")),
        c(Float64,       Decimal,   Sql("numeric")),
        c(Decimal,       Float64,   Sql("float8")),
        c(Decimal,       Int64,     Sql("int8")),
        c(Decimal,       BigInt,    Sql("numeric")),
        c(BigInt,        Int64,     Sql("int8")),
        // bigint and decimal are both `numeric` in PG; ::numeric is a no-op at runtime.
        c(BigInt,        Decimal,   Sql("numeric")),
        c(Bool,          Int16,     Sql("int2")),
        c(Bool,          Int32,     Sql("int4")),
        c(Bool,          Int64,     Sql("int8")),
        c(Str,           Uuid,      Sql("uuid")),
        c(LocalDatetime, LocalDate, Sql("date")),
        c(LocalDatetime, LocalTime, Sql("time")),

        // ── Function casts: str → scalar ─────────────────────────────────────
        // Transpiler resolves to the stdlib function and uses its ImplStrategy.
        // Most emit inline SQL (SqlExpression); to_datetime and int→bool use PylonFunction.
        c(Str, Int16,    Fn("to_int16")),
        c(Str, Int32,    Fn("to_int32")),
        c(Str, Int64,    Fn("to_int64")),
        c(Str, Float32,  Fn("to_float32")),
        c(Str, Float64,  Fn("to_float64")),
        c(Str, Decimal,  Fn("to_decimal")),
        c(Str, BigInt,   Fn("to_bigint")),
        c(Str, Bool,     Fn("to_bool")),
        // Single-arg no-fmt ISO 8601 parsing via _pylon.to_datetime(text).
        c(Str, Datetime, Fn("to_datetime")),
        c(Str, Json,     Fn("to_json")),

        // ── Function casts: scalar → str ──────────────────────────────────────
        c(Int16,    Str, Fn("to_str")),
        c(Int32,    Str, Fn("to_str")),
        c(Int64,    Str, Fn("to_str")),
        c(Float32,  Str, Fn("to_str")),
        c(Float64,  Str, Fn("to_str")),
        c(Decimal,  Str, Fn("to_str")),
        c(BigInt,   Str, Fn("to_str")),
        c(Bool,     Str, Fn("to_str")),
        c(Uuid,     Str, Fn("to_str")),
        // No-fmt overload only; to_str(datetime, fmt) is callable-only.
        c(Datetime, Str, Fn("to_str")),
        c(Duration, Str, Fn("to_str")),
        c(Json,     Str, Fn("to_str")),

        // ── Function casts: int → bool ────────────────────────────────────────
        // PG has no native int::bool cast; _pylon.to_bool handles 0 → false, else true.
        c(Int16, Bool, Fn("to_bool")),
        c(Int32, Bool, Fn("to_bool")),
        c(Int64, Bool, Fn("to_bool")),
    ]
}
