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

use super::{
    Any, AnyOrderable, AnyPoint, BigInt, Bool, Bytes, Datetime, Decimal, Duration, Float32, Float64, FnDescriptor,
    Int16, Int32, Int64, Json, LocalDate, LocalDatetime, LocalTime, Str, Uuid,
};
use super::{
    B, E, I, NamedDefault, O, arr, f, fc, mr, opt, p, plpgsql, plpgsql_nullable, plpgsql_stable_nullable,
    plpgsql_stable_nullable_bool, plpgsql_stable_nullable_elem, plpgsql_stable_returns, pn, pn_as, pv, ro, set_of, sql,
    sql_returns, tup,
};
use crate::stdlib::FnVolatility::{Modifying, Stable, Volatile};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        // ── std:: aggregate ──────────────────────────────────────────────────
        f("std", "count", vec![p("s", set_of(Any))], Int64, B("count")),
        f("std", "sum", vec![p("s", set_of(Int16))], Int64, B("sum")),
        f("std", "sum", vec![p("s", set_of(Int32))], Int64, B("sum")),
        f("std", "sum", vec![p("s", set_of(Int64))], Int64, B("sum")),
        f("std", "sum", vec![p("s", set_of(Float32))], Float32, B("sum")),
        f("std", "sum", vec![p("s", set_of(Float64))], Float64, B("sum")),
        f("std", "sum", vec![p("s", set_of(Decimal))], Decimal, B("sum")),
        f(
            "std",
            "min",
            vec![p("s", set_of(AnyOrderable))],
            opt(AnyOrderable),
            B("min"),
        ),
        f(
            "std",
            "max",
            vec![p("s", set_of(AnyOrderable))],
            opt(AnyOrderable),
            B("max"),
        ),
        f("std", "mean", vec![p("s", set_of(Float64))], Float64, B("avg")),
        f("std", "mean", vec![p("s", set_of(Decimal))], Decimal, B("avg")),
        // PG's avg(bigint) returns numeric; cast down to the declared float64 return type.
        f(
            "std",
            "mean",
            vec![p("s", set_of(Int64))],
            Float64,
            E("avg($1)::float8"),
        ),
        f("std", "all", vec![p("vals", set_of(Bool))], Bool, B("bool_and")),
        f("std", "any", vec![p("vals", set_of(Bool))], Bool, B("bool_or")),
        f("std", "array_agg", vec![p("s", set_of(Any))], arr(Any), B("array_agg")),
        // ── std:: set ────────────────────────────────────────────────────────
        f(
            "std",
            "enumerate",
            vec![p("s", set_of(Any))],
            set_of(tup(vec![Int64, Any])),
            sql_returns(
                "enumerate",
                "TABLE(index bigint, value anyelement)",
                "SELECT (ordinality - 1)::bigint, elem \
                 FROM unnest($1) WITH ORDINALITY AS t(elem, ordinality)",
            ),
        ),
        f(
            "std",
            "assert_single",
            vec![p("s", set_of(Any))],
            opt(Any),
            plpgsql_stable_returns(
                "assert_single",
                "anyelement",
                r#"DECLARE n int := cardinality($1);
BEGIN
    IF n > 1 THEN
        RAISE EXCEPTION 'assert_single: expected at most 1 element, got %', n
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1[1];
END"#,
            ),
        ),
        f(
            "std",
            "assert_single",
            vec![p("s", set_of(Any)), pn_as("msg", "message", Str, NamedDefault::Empty)],
            opt(Any),
            plpgsql_stable_nullable_elem(
                "assert_single",
                r#"DECLARE n int := cardinality($1);
BEGIN
    IF n > 1 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, format('assert_single: expected at most 1 element, got %s', n)),
            ERRCODE = 'P0002';
    END IF;
    RETURN $1[1];
END"#,
            ),
        ),
        f(
            "std",
            "assert_exists",
            vec![p("s", set_of(Any))],
            set_of(Any),
            plpgsql_stable_returns(
                "assert_exists",
                "anyarray",
                r#"BEGIN
    IF cardinality($1) = 0 THEN
        RAISE EXCEPTION 'assert_exists: expected at least 1 element, got none'
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        f(
            "std",
            "assert_exists",
            vec![p("s", set_of(Any)), pn_as("msg", "message", Str, NamedDefault::Empty)],
            set_of(Any),
            plpgsql_stable_nullable(
                "assert_exists",
                r#"BEGIN
    IF cardinality($1) = 0 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, 'assert_exists: expected at least 1 element, got none'),
            ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        f(
            "std",
            "assert_distinct",
            vec![p("s", set_of(Any))],
            set_of(Any),
            plpgsql_stable_returns(
                "assert_distinct",
                "anyarray",
                r#"DECLARE has_dupes bool;
BEGIN
    SELECT EXISTS(
        SELECT 1 FROM unnest($1) t(v) GROUP BY v HAVING count(*) > 1
    ) INTO has_dupes;
    IF has_dupes THEN
        RAISE EXCEPTION 'assert_distinct: duplicate elements in set'
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        f(
            "std",
            "assert_distinct",
            vec![p("s", set_of(Any)), pn_as("msg", "message", Str, NamedDefault::Empty)],
            set_of(Any),
            plpgsql_stable_nullable(
                "assert_distinct",
                r#"DECLARE has_dupes bool;
BEGIN
    SELECT EXISTS(
        SELECT 1 FROM unnest($1) t(v) GROUP BY v HAVING count(*) > 1
    ) INTO has_dupes;
    IF has_dupes THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, 'assert_distinct: duplicate elements in set'),
            ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        f(
            "std",
            "assert",
            vec![p("condition", Bool)],
            Bool,
            plpgsql_stable_returns(
                "assert",
                "boolean",
                r#"BEGIN
    IF NOT $1 THEN
        RAISE EXCEPTION 'assert: assertion failed'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        f(
            "std",
            "assert",
            vec![p("condition", Bool), pn_as("msg", "message", Str, NamedDefault::Empty)],
            Bool,
            plpgsql_stable_nullable_bool(
                "assert",
                r#"BEGIN
    IF $1 IS NULL OR NOT $1 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, 'assert: assertion failed'),
            ERRCODE = 'P0001';
    END IF;
    RETURN $1;
END"#,
            ),
        ),
        // ── std:: string ─────────────────────────────────────────────────────
        f("std", "str_lower", vec![p("s", Str)], Str, B("lower")),
        f("std", "str_upper", vec![p("s", Str)], Str, B("upper")),
        f("std", "str_title", vec![p("s", Str)], Str, B("initcap")),
        f("std", "str_pad_start", vec![p("s", Str), p("n", Int64)], Str, B("lpad")),
        f(
            "std",
            "str_pad_start",
            vec![p("s", Str), p("n", Int64), p("fill", Str)],
            Str,
            B("lpad"),
        ),
        f("std", "str_pad_end", vec![p("s", Str), p("n", Int64)], Str, B("rpad")),
        f(
            "std",
            "str_pad_end",
            vec![p("s", Str), p("n", Int64), p("fill", Str)],
            Str,
            B("rpad"),
        ),
        f("std", "str_trim", vec![p("s", Str)], Str, B("btrim")),
        f("std", "str_trim", vec![p("s", Str), p("trim", Str)], Str, B("btrim")),
        f("std", "str_trim_start", vec![p("s", Str)], Str, B("ltrim")),
        f(
            "std",
            "str_trim_start",
            vec![p("s", Str), p("trim", Str)],
            Str,
            B("ltrim"),
        ),
        f("std", "str_trim_end", vec![p("s", Str)], Str, B("rtrim")),
        f(
            "std",
            "str_trim_end",
            vec![p("s", Str), p("trim", Str)],
            Str,
            B("rtrim"),
        ),
        f("std", "str_repeat", vec![p("s", Str), p("n", Int64)], Str, B("repeat")),
        f(
            "std",
            "str_replace",
            vec![p("s", Str), p("old", Str), p("new", Str)],
            Str,
            B("replace"),
        ),
        f("std", "str_reverse", vec![p("s", Str)], Str, B("reverse")),
        f(
            "std",
            "str_split",
            vec![p("s", Str), p("delim", Str)],
            arr(Str),
            E("string_to_array($1, $2)"),
        ),
        f(
            "std",
            "str_contains",
            vec![p("s", Str), p("sub", Str)],
            Bool,
            E("strpos($1, $2) > 0"),
        ),
        f(
            "std",
            "str_starts_with",
            vec![p("s", Str), p("prefix", Str)],
            Bool,
            B("starts_with"),
        ),
        f(
            "std",
            "str_ends_with",
            vec![p("s", Str), p("suffix", Str)],
            Bool,
            E("right($1, length($2)) = $2"),
        ),
        // 0-based indexing at PyQL level; +1 adjusts to PostgreSQL's 1-based convention.
        f(
            "std",
            "str_slice",
            vec![p("s", Str), p("start", Int64)],
            Str,
            E("substr($1, $2 + 1)"),
        ),
        f(
            "std",
            "str_slice",
            vec![p("s", Str), p("start", Int64), p("end", Int64)],
            Str,
            E("substr($1, $2 + 1, $3 - $2)"),
        ),
        f("std", "str_len", vec![p("s", Str)], Int64, B("length")),
        f(
            "std",
            "re_match",
            vec![p("pattern", Str), p("s", Str)],
            arr(Str),
            sql("re_match", "SELECT coalesce(regexp_match($2, $1), '{}'::text[])"),
        ),
        f(
            "std",
            "re_match_all",
            vec![p("pattern", Str), p("s", Str)],
            set_of(arr(Str)),
            // SETOF text[] derived automatically from Set(Array(Str))
            sql("re_match_all", "SELECT m FROM regexp_matches($2, $1, 'g') m"),
        ),
        // Argument order swapped vs PostgreSQL: PyQL re_replace(pattern, sub, s) → regexp_replace(s, pattern, sub).
        f(
            "std",
            "re_replace",
            vec![p("pattern", Str), p("sub", Str), p("s", Str)],
            Str,
            E("regexp_replace($3, $1, $2)"),
        ),
        f(
            "std",
            "re_replace",
            vec![p("pattern", Str), p("sub", Str), p("s", Str), p("flags", Str)],
            Str,
            E("regexp_replace($3, $1, $2, $4)"),
        ),
        f(
            "std",
            "re_test",
            vec![p("pattern", Str), p("s", Str)],
            Bool,
            E("$2 ~ $1"),
        ),
        // 0-based find; -1 adjusts PostgreSQL's 1-based strpos result.
        f(
            "std",
            "find",
            vec![p("haystack", Str), p("needle", Str)],
            Int64,
            E("strpos($1, $2) - 1"),
        ),
        // Same over an array. `array_position` is 1-based like `strpos`, but
        // yields NULL rather than 0 when the element is absent, so the miss
        // has to be folded to -1 explicitly.
        f(
            "std",
            "find",
            vec![p("haystack", arr(Any)), p("needle", Any)],
            Int64,
            E("coalesce(array_position($1, $2) - 1, -1)"),
        ),
        // ── std:: numeric ────────────────────────────────────────────────────
        f("std", "abs", vec![p("n", Int16)], Int16, B("abs")),
        f("std", "abs", vec![p("n", Int32)], Int32, B("abs")),
        f("std", "abs", vec![p("n", Int64)], Int64, B("abs")),
        f("std", "abs", vec![p("n", Float32)], Float32, B("abs")),
        f("std", "abs", vec![p("n", Float64)], Float64, B("abs")),
        f("std", "abs", vec![p("n", Decimal)], Decimal, B("abs")),
        f("std", "ceil", vec![p("n", Float64)], Float64, B("ceil")),
        f("std", "ceil", vec![p("n", Decimal)], Decimal, B("ceil")),
        f("std", "floor", vec![p("n", Float64)], Float64, B("floor")),
        f("std", "floor", vec![p("n", Decimal)], Decimal, B("floor")),
        f("std", "round", vec![p("n", Float64)], Float64, B("round")),
        f(
            "std",
            "round",
            vec![p("n", Float64), p("d", Int64)],
            Float64,
            E("round($1, $2)"),
        ),
        f("std", "round", vec![p("n", Decimal)], Decimal, B("round")),
        f(
            "std",
            "round",
            vec![p("n", Decimal), p("d", Int64)],
            Decimal,
            E("round($1, $2)"),
        ),
        f("std", "sign", vec![p("n", Int64)], Int64, B("sign")),
        f("std", "sign", vec![p("n", Float64)], Float64, B("sign")),
        f("std", "sign", vec![p("n", Decimal)], Decimal, B("sign")),
        f("std", "sqrt", vec![p("n", Float64)], Float64, B("sqrt")),
        f("std", "sqrt", vec![p("n", Decimal)], Decimal, B("sqrt")),
        f("std", "random", vec![], Float64, B("random")).vol(Volatile),
        // ── std:: generic / polymorphic ──────────────────────────────────────
        f("std", "len", vec![p("s", Str)], Int64, B("length")),
        f("std", "len", vec![p("b", Bytes)], Int64, B("length")),
        f("std", "len", vec![p("a", arr(Any))], Int64, E("array_length($1, 1)")),
        // str: handle empty needle (strpos returns 0 for '' in some PG versions)
        f(
            "std",
            "contains",
            vec![p("haystack", Str), p("needle", Str)],
            Bool,
            E("(CASE WHEN ($2) = '' THEN TRUE ELSE strpos($1, $2) != 0 END)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", Bytes), p("needle", Bytes)],
            Bool,
            E("(position($2 in $1) != 0)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", arr(Any)), p("needle", Any)],
            Bool,
            E("($1 @> ARRAY[$2])"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", Json), p("needle", Json)],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", ro(AnyPoint)), p("needle", ro(AnyPoint))],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", ro(AnyPoint)), p("needle", AnyPoint)],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", mr(AnyPoint)), p("needle", mr(AnyPoint))],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", mr(AnyPoint)), p("needle", ro(AnyPoint))],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", mr(AnyPoint)), p("needle", AnyPoint)],
            Bool,
            E("($1 @> $2)"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", ro(LocalDate)), p("needle", LocalDate)],
            Bool,
            E("($1 @> ($2::date))"),
        ),
        f(
            "std",
            "contains",
            vec![p("haystack", mr(LocalDate)), p("needle", LocalDate)],
            Bool,
            E("($1 @> ($2::date))"),
        ),
        // ── std:: uuid ───────────────────────────────────────────────────────
        f("std", "uuid_generate_v4", vec![], Uuid, B("uuidv4")).vol(Volatile),
        f("std", "uuid_generate_v7", vec![], Uuid, B("uuidv7")).vol(Volatile),
        f(
            "std",
            "uuid_extract_timestamp",
            vec![p("u", Uuid)],
            Datetime,
            B("uuid_extract_timestamp"),
        ),
        f(
            "std",
            "uuid_extract_version",
            vec![p("u", Uuid)],
            Int64,
            B("uuid_extract_version"),
        ),
        f(
            "std",
            "to_uuid",
            vec![p("val", Bytes)],
            Uuid,
            plpgsql(
                "to_uuid",
                r#"BEGIN
    IF length($1) != 16 THEN
        RAISE EXCEPTION 'to_uuid(): the argument must be exactly 16 bytes long';
    END IF;
    RETURN encode($1, 'hex')::uuid;
END"#,
            ),
        ),
        // ── std:: json ───────────────────────────────────────────────────────
        fc("std", "to_json", vec![p("s", Str)], Json, E("$1::jsonb")),
        f("std", "json_typeof", vec![p("j", Json)], Str, B("jsonb_typeof")),
        // path is variadic and last → VARIADIC text[] in PG
        f(
            "std",
            "json_get",
            vec![p("j", Json), pv("path", Str)],
            opt(Json),
            sql("json_get", "SELECT $1 #> $2"),
        ),
        // path is variadic but not last → collected into text[] by the transpiler
        f(
            "std",
            "json_set",
            vec![p("j", Json), pv("path", Str), p("val", Json)],
            Json,
            sql("json_set", "SELECT jsonb_set($1, $2, $3)"),
        ),
        f(
            "std",
            "json_array_unpack",
            vec![p("j", Json)],
            set_of(Json),
            E("jsonb_array_elements($1)"),
        ),
        f(
            "std",
            "json_object_unpack",
            vec![p("j", Json)],
            set_of(tup(vec![Str, Json])),
            E("jsonb_each($1)"),
        ),
        f(
            "std",
            "json_array_length",
            vec![p("j", Json)],
            opt(Int64),
            B("jsonb_array_length"),
        ),
        // ── std:: bitwise ────────────────────────────────────────────────────
        f(
            "std",
            "bit_and",
            vec![p("l", Int16), p("r", Int16)],
            Int16,
            E("($1 & $2)"),
        ),
        f(
            "std",
            "bit_and",
            vec![p("l", Int32), p("r", Int32)],
            Int32,
            E("($1 & $2)"),
        ),
        f(
            "std",
            "bit_and",
            vec![p("l", Int64), p("r", Int64)],
            Int64,
            E("($1 & $2)"),
        ),
        f(
            "std",
            "bit_or",
            vec![p("l", Int16), p("r", Int16)],
            Int16,
            E("($1 | $2)"),
        ),
        f(
            "std",
            "bit_or",
            vec![p("l", Int32), p("r", Int32)],
            Int32,
            E("($1 | $2)"),
        ),
        f(
            "std",
            "bit_or",
            vec![p("l", Int64), p("r", Int64)],
            Int64,
            E("($1 | $2)"),
        ),
        f(
            "std",
            "bit_xor",
            vec![p("l", Int16), p("r", Int16)],
            Int16,
            E("($1 # $2)"),
        ),
        f(
            "std",
            "bit_xor",
            vec![p("l", Int32), p("r", Int32)],
            Int32,
            E("($1 # $2)"),
        ),
        f(
            "std",
            "bit_xor",
            vec![p("l", Int64), p("r", Int64)],
            Int64,
            E("($1 # $2)"),
        ),
        f("std", "bit_not", vec![p("r", Int16)], Int16, E("(~$1)")),
        f("std", "bit_not", vec![p("r", Int32)], Int32, E("(~$1)")),
        f("std", "bit_not", vec![p("r", Int64)], Int64, E("(~$1)")),
        f(
            "std",
            "bit_count",
            vec![p("val", Int16)],
            Int64,
            E("bit_count($1::int4::bit(16))"),
        ),
        f(
            "std",
            "bit_count",
            vec![p("val", Int32)],
            Int64,
            E("bit_count($1::bit(32))"),
        ),
        f(
            "std",
            "bit_count",
            vec![p("val", Int64)],
            Int64,
            E("bit_count($1::bit(64))"),
        ),
        f(
            "std",
            "bit_lshift",
            vec![p("val", Int16), p("n", Int64)],
            Int16,
            E("(($1::int8 << $2)::int2)"),
        ),
        f(
            "std",
            "bit_lshift",
            vec![p("val", Int32), p("n", Int64)],
            Int32,
            E("(($1::int8 << $2)::int4)"),
        ),
        f(
            "std",
            "bit_lshift",
            vec![p("val", Int64), p("n", Int64)],
            Int64,
            E("($1 << $2)"),
        ),
        f(
            "std",
            "bit_rshift",
            vec![p("val", Int16), p("n", Int64)],
            Int16,
            E("(($1::int8 >> $2)::int2)"),
        ),
        f(
            "std",
            "bit_rshift",
            vec![p("val", Int32), p("n", Int64)],
            Int32,
            E("(($1::int8 >> $2)::int4)"),
        ),
        f(
            "std",
            "bit_rshift",
            vec![p("val", Int64), p("n", Int64)],
            Int64,
            E("($1 >> $2)"),
        ),
        f("std", "to_hex", vec![p("n", Int16)], Str, E("to_hex($1::int8)")),
        f("std", "to_hex", vec![p("n", Int32)], Str, E("to_hex($1::int8)")),
        f("std", "to_hex", vec![p("n", Int64)], Str, B("to_hex")),
        // ── std:: bytes ──────────────────────────────────────────────────────
        f(
            "std",
            "bytes_get_bit",
            vec![p("b", Bytes), p("n", Int64)],
            Int64,
            B("get_bit"),
        ),
        f(
            "std",
            "bytes_get",
            vec![p("b", Bytes), p("n", Int64)],
            Int64,
            E("get_byte($1, $2)"),
        ),
        f("std", "from_hex", vec![p("s", Str)], Bytes, E("decode($1, 'hex')")),
        // Postgres wraps its base64 output every 76 characters; this must not.
        f(
            "enc",
            "base64_encode",
            vec![p("data", Bytes)],
            Str,
            E("translate(encode($1, 'base64'), E'\\n', '')"),
        ),
        f(
            "enc",
            "base64_decode",
            vec![p("data", Str)],
            Bytes,
            E("decode($1, 'base64')"),
        ),
        f(
            "std",
            "to_bytes",
            vec![p("s", Str), p("encoding", Str)],
            Bytes,
            sql("to_bytes", "SELECT convert_to($1, $2)"),
        ),
        // A UUID's 16 bytes, big-endian: `0199a144-…-00049e57387b` gives
        // `AZmhRFRzjCqvmgAEnlc4ew==`.
        f(
            "std",
            "to_bytes",
            vec![p("val", Uuid)],
            Bytes,
            sql("to_bytes_uuid", "SELECT decode(replace(($1)::text, '-', ''), 'hex')"),
        ),
        // `to_int16`/`to_int32`/`to_int64` over raw bytes, with byte order
        // selected by `std::Endian`. The width of the `bit(N)` cast is what
        // fixes how many bytes each one consumes; `Little` reverses them first.
        // The 32-bit orders were checked against a live instance, which gives
        // -1638451077/2067290014 for the last four bytes of
        // `0199a144-5473-8c2a-af9a-00049e57387b`.
        //
        // `to_int16` needs the extra arithmetic because PostgreSQL casts `bit`
        // to `int4`/`int8` but not to `int2`: `bit(16)::int4` zero-extends, so
        // 0x9E57 arrives as 40535 rather than -25001, and the `+ 32768 % 65536
        // - 32768` wrap is what restores the sign before narrowing to `int2`.
        //
        // `to_bytes(intN, Endian)` in the other direction is still missing.
        f(
            "std",
            "to_int16",
            vec![p("val", Bytes), p("endian", Str)],
            Int16,
            sql(
                "to_int16_bytes",
                "SELECT ((CASE WHEN $2 = 'Big' THEN ('x' || encode($1, 'hex'))::bit(16)::int4 ELSE ('x' || encode(substr($1, 2, 1) || substr($1, 1, 1), 'hex'))::bit(16)::int4 END + 32768) % 65536 - 32768)::int2",
            ),
        ),
        f(
            "std",
            "to_int32",
            vec![p("val", Bytes), p("endian", Str)],
            Int32,
            sql(
                "to_int32_bytes",
                "SELECT CASE WHEN $2 = 'Big'                  THEN ('x' || encode($1, 'hex'))::bit(32)::int4                  ELSE ('x' || encode(substr($1, 4, 1) || substr($1, 3, 1) || substr($1, 2, 1) || substr($1, 1, 1), 'hex'))::bit(32)::int4 END",
            ),
        ),
        f(
            "std",
            "to_int64",
            vec![p("val", Bytes), p("endian", Str)],
            Int64,
            sql(
                "to_int64_bytes",
                "SELECT CASE WHEN $2 = 'Big' THEN ('x' || encode($1, 'hex'))::bit(64)::int8 ELSE ('x' || encode(substr($1, 8, 1) || substr($1, 7, 1) || substr($1, 6, 1) || substr($1, 5, 1) || substr($1, 4, 1) || substr($1, 3, 1) || substr($1, 2, 1) || substr($1, 1, 1), 'hex'))::bit(64)::int8 END",
            ),
        ),
        // ── std:: array ──────────────────────────────────────────────────────
        // 0-based indexing at PyQL level; +1 adjusts to PostgreSQL's 1-based arrays.
        f(
            "std",
            "array_get",
            vec![p("a", arr(Any)), p("i", Int64)],
            opt(Any),
            E("($1)[$2 + 1]"),
        ),
        f("std", "array_unpack", vec![p("a", arr(Any))], set_of(Any), B("unnest")),
        f(
            "std",
            "array_join",
            vec![p("a", arr(Str)), p("delim", Str)],
            Str,
            E("array_to_string($1, $2)"),
        ),
        f(
            "std",
            "array_slice",
            vec![p("a", arr(Any)), p("start", Int64)],
            arr(Any),
            E("$1[$2 + 1:]"),
        ),
        f(
            "std",
            "array_slice",
            vec![p("a", arr(Any)), p("start", Int64), p("end", Int64)],
            arr(Any),
            E("$1[$2 + 1:$3]"),
        ),
        f(
            "std",
            "array_index_of",
            vec![p("a", arr(Any)), p("el", Any)],
            Int64,
            sql(
                "array_index_of",
                "SELECT coalesce(\
                    (SELECT (i - 1)::int8 \
                     FROM generate_subscripts($1, 1) t(i) \
                     WHERE $1[i] IS NOT DISTINCT FROM $2 \
                     LIMIT 1), \
                 -1::int8)",
            ),
        ),
        f(
            "std",
            "array_fill",
            vec![p("el", Any), p("n", Int64)],
            arr(Any),
            E("array_fill($1, ARRAY[$2::int])"),
        ),
        f(
            "std",
            "array_replace",
            vec![p("a", arr(Any)), p("old", Any), p("new", Any)],
            arr(Any),
            B("array_replace"),
        ),
        f(
            "std",
            "array_reverse",
            vec![p("a", arr(Any))],
            arr(Any),
            B("array_reverse"),
        ),
        // 0-based indexing at PyQL level, matching array_get/array_slice's convention above.
        f(
            "std",
            "array_set",
            vec![p("a", arr(Any)), p("idx", Int64), p("val", Any)],
            arr(Any),
            E("(($1)[1:$2] || ARRAY[$3] || ($1)[$2 + 2:])"),
        ),
        f(
            "std",
            "array_insert",
            vec![p("a", arr(Any)), p("idx", Int64), p("val", Any)],
            arr(Any),
            E("(($1)[1:$2] || ARRAY[$3] || ($1)[$2 + 1:])"),
        ),
        f(
            "std",
            "array_rotate",
            vec![p("a", arr(Any)), p("n", Int64)],
            arr(Any),
            plpgsql(
                "array_rotate",
                r#"DECLARE len int := cardinality($1); k int;
BEGIN
    IF len = 0 THEN RETURN $1; END IF;
    k := (($2::int % len) + len) % len;
    IF k = 0 THEN RETURN $1; END IF;
    RETURN $1[k + 1:] || $1[1:k];
END"#,
            ),
        ),
        // ── std:: range ──────────────────────────────────────────────────────
        // TranspilerIntrinsic: the transpiler substitutes type-specific PG
        // constructors (int8range, tstzrange, …) at compile time. No _pylon function.
        f(
            "std",
            "range",
            vec![p("lower", AnyPoint), p("upper", AnyPoint)],
            ro(AnyPoint),
            I("range"),
        ),
        f(
            "std",
            "range",
            vec![
                p("lower", AnyPoint),
                p("upper", AnyPoint),
                p("inc_lower", Bool),
                p("inc_upper", Bool),
            ],
            ro(AnyPoint),
            I("range"),
        ),
        f("std", "range", vec![p("empty", Bool)], ro(AnyPoint), I("range")),
        f(
            "std",
            "range_unpack",
            vec![p("r", ro(AnyPoint))],
            set_of(AnyPoint),
            E("generate_series($1)"),
        ),
        f(
            "std",
            "range_unpack",
            vec![p("r", ro(AnyPoint)), p("step", AnyPoint)],
            set_of(AnyPoint),
            E("generate_series(lower($1), upper($1), $2)"),
        ),
        f(
            "std",
            "range_get_lower",
            vec![p("r", ro(AnyPoint))],
            opt(AnyPoint),
            B("lower"),
        ),
        f(
            "std",
            "range_get_upper",
            vec![p("r", ro(AnyPoint))],
            opt(AnyPoint),
            B("upper"),
        ),
        f("std", "range_is_empty", vec![p("r", ro(AnyPoint))], Bool, B("isempty")),
        f(
            "std",
            "range_is_inclusive_lower",
            vec![p("r", ro(AnyPoint))],
            Bool,
            B("lower_inc"),
        ),
        f(
            "std",
            "range_is_inclusive_upper",
            vec![p("r", ro(AnyPoint))],
            Bool,
            B("upper_inc"),
        ),
        f(
            "std",
            "overlaps",
            vec![p("a", ro(AnyPoint)), p("b", ro(AnyPoint))],
            Bool,
            O("&&"),
        ),
        f(
            "std",
            "multirange",
            vec![p("ranges", arr(ro(AnyPoint)))],
            mr(AnyPoint),
            I("multirange"),
        ),
        f(
            "std",
            "strictly_below",
            vec![p("l", ro(AnyPoint)), p("r", ro(AnyPoint))],
            Bool,
            O("<<"),
        ),
        f(
            "std",
            "strictly_below",
            vec![p("l", mr(AnyPoint)), p("r", mr(AnyPoint))],
            Bool,
            O("<<"),
        ),
        f(
            "std",
            "strictly_above",
            vec![p("l", ro(AnyPoint)), p("r", ro(AnyPoint))],
            Bool,
            O(">>"),
        ),
        f(
            "std",
            "strictly_above",
            vec![p("l", mr(AnyPoint)), p("r", mr(AnyPoint))],
            Bool,
            O(">>"),
        ),
        f(
            "std",
            "bounded_above",
            vec![p("l", ro(AnyPoint)), p("r", ro(AnyPoint))],
            Bool,
            O("&<"),
        ),
        f(
            "std",
            "bounded_above",
            vec![p("l", mr(AnyPoint)), p("r", mr(AnyPoint))],
            Bool,
            O("&<"),
        ),
        f(
            "std",
            "bounded_below",
            vec![p("l", ro(AnyPoint)), p("r", ro(AnyPoint))],
            Bool,
            O("&>"),
        ),
        f(
            "std",
            "bounded_below",
            vec![p("l", mr(AnyPoint)), p("r", mr(AnyPoint))],
            Bool,
            O("&>"),
        ),
        f(
            "std",
            "adjacent",
            vec![p("l", ro(AnyPoint)), p("r", ro(AnyPoint))],
            Bool,
            O("-|-"),
        ),
        f(
            "std",
            "adjacent",
            vec![p("l", mr(AnyPoint)), p("r", mr(AnyPoint))],
            Bool,
            O("-|-"),
        ),
        f(
            "std",
            "multirange_unpack",
            vec![p("val", mr(AnyPoint))],
            set_of(ro(AnyPoint)),
            B("unnest"),
        ),
        // ── std:: datetime ───────────────────────────────────────────────────
        f("std", "datetime_current", vec![], Datetime, E("clock_timestamp()")).vol(Volatile),
        // Stable, not immutable: fixed for the duration of one transaction /
        // statement, but different between them.
        f(
            "std",
            "datetime_of_transaction",
            vec![],
            Datetime,
            E("transaction_timestamp()"),
        )
        .vol(Stable),
        f(
            "std",
            "datetime_of_statement",
            vec![],
            Datetime,
            E("statement_timestamp()"),
        )
        .vol(Stable),
        // PylonFunction: PG extract requires a keyword field, not a text argument.
        f(
            "std",
            "datetime_get",
            vec![p("dt", Datetime), p("el", Str)],
            Float64,
            plpgsql(
                "datetime_get",
                r#"BEGIN
    IF $2 = 'epochseconds' THEN
        RETURN date_part('epoch', $1);
    END IF;
    IF $2 NOT IN ('century', 'day', 'decade', 'dow', 'doy', 'hour', 'isodow', 'isoyear',
                  'microseconds', 'millennium', 'milliseconds', 'minutes', 'month',
                  'quarter', 'seconds', 'week', 'year') THEN
        RAISE EXCEPTION 'invalid unit for std::datetime_get: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: epochseconds, century, day, decade, dow, doy, hour, isodow, isoyear, microseconds, millennium, milliseconds, minutes, month, quarter, seconds, week, year.';
    END IF;
    RETURN date_part($2, $1);
END"#,
            ),
        ),
        // `cal::local_datetime` reads the same units off the same table. The
        // overload belongs to `std::`, beside the `datetime` one, rather than
        // to the `cal::` namespace its argument type comes from.
        f(
            "std",
            "datetime_get",
            vec![p("dt", LocalDatetime), p("el", Str)],
            Float64,
            plpgsql(
                "datetime_get",
                r#"BEGIN
    IF $2 = 'epochseconds' THEN
        RETURN date_part('epoch', $1);
    END IF;
    IF $2 NOT IN ('century', 'day', 'decade', 'dow', 'doy', 'hour', 'isodow', 'isoyear',
                  'microseconds', 'millennium', 'milliseconds', 'minutes', 'month',
                  'quarter', 'seconds', 'week', 'year') THEN
        RAISE EXCEPTION 'invalid unit for std::datetime_get: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: epochseconds, century, day, decade, dow, doy, hour, isodow, isoyear, microseconds, millennium, milliseconds, minutes, month, quarter, seconds, week, year.';
    END IF;
    RETURN date_part($2, $1);
END"#,
            ),
        ),
        // Note argument swap: PyQL truncate(dt, unit) → date_trunc(unit, dt).
        // `quarters` is spelled `quarter` by PostgreSQL, and PostgreSQL's own
        // vocabulary is wider than the accepted one, so the unit is checked
        // rather than passed straight through.
        f(
            "std",
            "datetime_truncate",
            vec![p("dt", Datetime), p("unit", Str)],
            Datetime,
            plpgsql(
                "datetime_truncate",
                r#"BEGIN
    IF $2 = 'quarters' THEN
        RETURN date_trunc('quarter', $1);
    END IF;
    IF $2 NOT IN ('microseconds', 'milliseconds', 'seconds', 'minutes', 'hours', 'days',
                  'weeks', 'months', 'years', 'decades', 'centuries') THEN
        RAISE EXCEPTION 'invalid unit for std::datetime_truncate: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: microseconds, milliseconds, seconds, minutes, hours, days, weeks, months, quarters, years, decades, centuries.';
    END IF;
    RETURN date_trunc($2, $1);
END"#,
            ),
        ),
        f(
            "std",
            "datetime_shift",
            vec![p("dt", Datetime), p("delta", Duration)],
            Datetime,
            E("$1 + $2"),
        ),
        f(
            "std",
            "duration_get",
            vec![p("d", Duration), p("el", Str)],
            Float64,
            plpgsql(
                "duration_get",
                r#"BEGIN
    IF $2 = 'totalseconds' THEN
        RETURN date_part('epoch', $1);
    END IF;
    IF $2 NOT IN ('hour', 'minutes', 'seconds', 'milliseconds', 'microseconds') THEN
        RAISE EXCEPTION 'invalid unit for std::duration_get: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: hour, minutes, seconds, milliseconds, microseconds, and totalseconds.';
    END IF;
    RETURN date_part($2, $1);
END"#,
            ),
        ),
        f(
            "std",
            "duration_to_seconds",
            vec![p("d", Duration)],
            Decimal,
            E("extract(epoch from $1)"),
        ),
        f(
            "std",
            "duration_truncate",
            vec![p("dt", Duration), p("unit", Str)],
            Duration,
            plpgsql(
                "duration_truncate",
                r#"BEGIN
    IF $2 NOT IN ('microseconds', 'milliseconds', 'seconds', 'minutes', 'hours') THEN
        RAISE EXCEPTION 'invalid unit for std::duration_truncate: %', $2;
    END IF;
    RETURN date_trunc($2, $1);
END"#,
            ),
        ),
        f(
            "std",
            "to_datetime",
            vec![p("s", Str), p("fmt", opt(Str))],
            Datetime,
            plpgsql_nullable(
                "to_datetime",
                r#"BEGIN
    IF $2 IS NULL THEN
        RETURN _pylon.to_datetime($1);
    END IF;
    IF $2 = '' THEN
        RAISE EXCEPTION 'to_datetime(): "fmt" argument must be a non-empty string'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF $2 !~ '^(("([^"\\]|\\.)*")|([^"]+))*(TZH).*$' THEN
        RAISE EXCEPTION 'missing required time zone in format: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Use one or both of the following: TZH, TZM';
    END IF;
    RETURN to_timestamp($1, $2);
END"#,
            ),
        ),
        f(
            "std",
            "to_datetime",
            vec![
                p("year", Int64),
                p("month", Int64),
                p("day", Int64),
                p("hour", Int64),
                p("min", Int64),
                p("sec", Float64),
                p("timezone", Str),
            ],
            Datetime,
            sql(
                "to_datetime",
                "SELECT make_timestamptz($1::int, $2::int, $3::int, $4::int, $5::int, $6, $7)",
            ),
        ),
        // The inverse of `cal::to_local_datetime(dt, timezone)`: read a wall
        // clock reading as an instant in a given zone.
        f(
            "std",
            "to_datetime",
            vec![p("local", LocalDatetime), p("zone", Str)],
            Datetime,
            E("($1 AT TIME ZONE $2)"),
        ),
        // Single-arg ISO 8601 parsing; cast target for str → datetime. The
        // zone is required — without it `::timestamptz` silently reads the
        // string in whatever zone the session happens to be in.
        fc(
            "std",
            "to_datetime",
            vec![p("s", Str)],
            Datetime,
            plpgsql(
                "to_datetime",
                r#"BEGIN
    IF $1 !~ '^\s*((\d{4}-\d{2}-\d{2}|\d{8})[ tT](\d{2}(:\d{2}(:\d{2}(\.\d+)?)?)?|\d{2,6}(\.\d+)?)([zZ]|[-+](\d{2,4}|\d{2}:\d{2})))\s*$' THEN
        RAISE EXCEPTION 'invalid input syntax for type datetime: %', quote_literal($1)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Please use ISO8601 format. Example: 2010-12-27T23:59:59-07:00';
    END IF;
    RETURN $1::timestamptz;
END"#,
            ),
        ),
        f(
            "std",
            "to_datetime",
            vec![p("epoch_seconds", Decimal)],
            Datetime,
            E("to_timestamp($1)"),
        ),
        f(
            "std",
            "to_duration",
            // Named-only, each defaulting to 0, and `microseconds` alongside
            // the rest, which `to_duration(seconds := …)` relies on. Declared
            // positionally these were unreachable by the names every call site
            // actually writes.
            vec![
                pn("hours", Int64, NamedDefault::Int(0)),
                pn("minutes", Int64, NamedDefault::Int(0)),
                pn("seconds", Float64, NamedDefault::Int(0)),
                pn("microseconds", Int64, NamedDefault::Int(0)),
            ],
            Duration,
            sql(
                "to_duration",
                "SELECT make_interval(hours => $1::int, mins => $2::int, secs => $3 + ($4::float8 / 1000000))",
            ),
        ),
        // ── std:: type conversion ────────────────────────────────────────────
        // ── to_str ───────────────────────────────────────────────────────────
        // `fmt` is optional on every overload that takes one: left out, or
        // passed as an empty set, the value renders the way a `<str>` cast
        // renders it. An empty *string* is refused rather than taken to mean
        // "no format", which is what `to_char` would otherwise do with it.
        //
        // `to_json` renders a timestamp as ISO 8601 whatever the session's
        // DateStyle is, which `::text` does not — it would give
        // `2026-01-16 12:34:56+00`, with a space and a two-digit offset.
        fc(
            "std",
            "to_str",
            vec![p("v", Datetime)],
            Str,
            E("trim(to_json($1)::text, '\"')"),
        ),
        f(
            "std",
            "to_str",
            vec![p("v", Datetime), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN trim(to_json($1)::text, '\"') WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc(
            "std",
            "to_str",
            vec![p("v", LocalDatetime)],
            Str,
            E("trim(to_json($1)::text, '\"')"),
        ),
        f(
            "std",
            "to_str",
            vec![p("v", LocalDatetime), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN trim(to_json($1)::text, '\"') WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc(
            "std",
            "to_str",
            vec![p("v", LocalDate)],
            Str,
            E("trim(to_json($1)::text, '\"')"),
        ),
        f(
            "std",
            "to_str",
            vec![p("v", LocalDate), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN trim(to_json($1)::text, '\"') WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc("std", "to_str", vec![p("v", LocalTime)], Str, E("$1::text")),
        // `to_char` has no `time` overload, so the time is composed onto a
        // date first. A fixed date keeps the result deterministic — composing
        // onto *today's* would leak the current date — and only a format
        // naming date fields can tell which one was used.
        f(
            "std",
            "to_str",
            vec![p("v", LocalTime), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char(date '2000-01-01' + $1, $2) END",
            ),
        ),
        // An interval renders as ISO 8601 (`PT1H30M`) because every Pylon
        // connection pins `intervalstyle` — see `pylon_pgcon::session_config`.
        fc("std", "to_str", vec![p("v", Duration)], Str, E("$1::text")),
        f(
            "std",
            "to_str",
            vec![p("v", Duration), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc("std", "to_str", vec![p("v", Int16)], Str, E("$1::text")),
        fc("std", "to_str", vec![p("v", Int32)], Str, E("$1::text")),
        fc("std", "to_str", vec![p("v", Int64)], Str, E("$1::text")),
        // The narrower integers and `float32` reach the formatting overloads
        // through implicit widening.
        f(
            "std",
            "to_str",
            vec![p("v", Int64), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc("std", "to_str", vec![p("v", Float32)], Str, E("$1::text")),
        fc("std", "to_str", vec![p("v", Float64)], Str, E("$1::text")),
        f(
            "std",
            "to_str",
            vec![p("v", Float64), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc("std", "to_str", vec![p("v", Decimal)], Str, E("$1::text")),
        fc("std", "to_str", vec![p("v", BigInt)], Str, E("$1::text")),
        // `bigint` is `numeric` too, so this one overload serves both.
        f(
            "std",
            "to_str",
            vec![p("v", Decimal), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE to_char($1, $2) END",
            ),
        ),
        fc("std", "to_str", vec![p("v", Json)], Str, E("$1::text")),
        // `pretty` is the only format json takes; anything else is refused
        // rather than handed to a formatter it means nothing to.
        f(
            "std",
            "to_str",
            vec![p("v", Json), p("fmt", opt(Str))],
            Str,
            E(
                "CASE WHEN $2 IS NULL THEN $1::text WHEN $2 = 'pretty' THEN jsonb_pretty($1) WHEN $2 = '' THEN _pylon.raise_invalid_parameter('to_str(): \"fmt\" argument must be a non-empty string') ELSE _pylon.raise_invalid_parameter('to_str(): format ''' || $2 || ''' is invalid') END",
            ),
        ),
        f("std", "to_str", vec![p("v", Bytes)], Str, E("convert_from($1, 'UTF8')")),
        // Superseded by `array_join`; kept because the call still resolves.
        f(
            "std",
            "to_str",
            vec![p("array", arr(Str)), p("delimiter", Str)],
            Str,
            E("array_to_string($1, $2)"),
        ),
        fc("std", "to_int16", vec![p("s", Str)], Int16, E("$1::int2")),
        f("std", "to_int16", vec![p("b", Bool)], Int16, E("$1::int2")),
        fc("std", "to_int32", vec![p("s", Str)], Int32, E("$1::int4")),
        f("std", "to_int32", vec![p("b", Bool)], Int32, E("$1::int4")),
        fc("std", "to_int64", vec![p("s", Str)], Int64, E("$1::int8")),
        f("std", "to_int64", vec![p("b", Bool)], Int64, E("$1::int8")),
        fc("std", "to_float32", vec![p("s", Str)], Float32, E("$1::float4")),
        f("std", "to_float32", vec![p("n", Int64)], Float32, E("$1::float4")),
        fc("std", "to_float64", vec![p("s", Str)], Float64, E("$1::float8")),
        f("std", "to_float64", vec![p("n", Int64)], Float64, E("$1::float8")),
        fc("std", "to_decimal", vec![p("s", Str)], Decimal, E("$1::numeric")),
        f("std", "to_decimal", vec![p("n", Int64)], Decimal, E("$1::numeric")),
        fc("std", "to_bigint", vec![p("s", Str)], BigInt, E("$1::numeric")),
        f("std", "to_bigint", vec![p("n", Int64)], BigInt, E("$1::numeric")),
        fc("std", "to_bool", vec![p("s", Str)], Bool, E("$1::bool")),
        // int → bool: PG has no native int::bool; _pylon.to_bool maps 0 → false, else true.
        fc(
            "std",
            "to_bool",
            vec![p("n", Int16)],
            Bool,
            sql("to_bool", "SELECT $1 <> 0::int2"),
        ),
        fc(
            "std",
            "to_bool",
            vec![p("n", Int32)],
            Bool,
            sql("to_bool", "SELECT $1 <> 0::int4"),
        ),
        fc(
            "std",
            "to_bool",
            vec![p("n", Int64)],
            Bool,
            sql("to_bool", "SELECT $1 <> 0::int8"),
        ),
        // ── std:: sequences ──────────────────────────────────────────────────
        // TranspilerIntrinsic: first arg is a sequence scalar type name resolved
        // by the compiler; emits nextval(...) / setval(...) directly.
        f("std", "sequence_next", vec![p("seq", Any)], Int64, I("sequence_next")).vol(Modifying),
        f("std", "sequence_reset", vec![p("seq", Any)], Int64, I("sequence_reset")).vol(Modifying),
        f(
            "std",
            "sequence_reset",
            vec![p("seq", Any), p("val", Int64)],
            Int64,
            I("sequence_reset"),
        )
        .vol(Modifying),
    ]
}
