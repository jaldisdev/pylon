use super::{FnDescriptor, FnVolatility, ImplStrategy, Param, PylonFnDef, PylonType, SqlLanguage};

use ImplStrategy::{SqlBuiltin as B, SqlExpression as E, SqlOperator as O, TranspilerIntrinsic as I};
use PylonType::{
    Any, AnyOrderable, AnyPoint, Array, BigInt, Bool, Bytes, Datetime, Decimal, Duration, Float32,
    Float64, Int16, Int32, Int64, Json, LocalDate, LocalDatetime, LocalTime, Multirange, Optional,
    Range, RelativeDuration, Set, Str, Tuple, Uuid, Vector,
};

// ── Type helpers ─────────────────────────────────────────────────────────────

fn arr(t: PylonType) -> PylonType { Array(Box::new(t)) }
fn set_of(t: PylonType) -> PylonType { Set(Box::new(t)) }
fn opt(t: PylonType) -> PylonType { Optional(Box::new(t)) }
fn ro(t: PylonType) -> PylonType { Range(Box::new(t)) }
fn mr(t: PylonType) -> PylonType { Multirange(Box::new(t)) }
fn tup(ts: Vec<PylonType>) -> PylonType { Tuple(ts) }

// ── Param helpers ─────────────────────────────────────────────────────────────

fn p(name: &'static str, ty: PylonType) -> Param { Param { name, ty, variadic: false } }
fn pv(name: &'static str, ty: PylonType) -> Param { Param { name, ty, variadic: true } }

// ── Descriptor helpers ────────────────────────────────────────────────────────

fn f(ns: &'static str, name: &'static str, params: Vec<Param>, ret: PylonType, impl_: ImplStrategy) -> FnDescriptor {
    FnDescriptor { namespace: ns, name, params, return_type: ret, impl_strategy: impl_, cast_target: false }
}

fn fc(ns: &'static str, name: &'static str, params: Vec<Param>, ret: PylonType, impl_: ImplStrategy) -> FnDescriptor {
    FnDescriptor { namespace: ns, name, params, return_type: ret, impl_strategy: impl_, cast_target: true }
}

// ── PylonFnDef helpers ────────────────────────────────────────────────────────

fn sql(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::Sql, volatility: FnVolatility::Immutable,
        strict: true, returns_override: None, body,
    })
}

fn sql_returns(name: &'static str, returns: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::Sql, volatility: FnVolatility::Immutable,
        strict: true, returns_override: Some(returns), body,
    })
}

fn plpgsql(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::PlPgSql, volatility: FnVolatility::Immutable,
        strict: true, returns_override: None, body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT — for overloads where an optional `msg` param
/// may legitimately be NULL when the caller omits it.
fn plpgsql_stable_nullable(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::PlPgSql, volatility: FnVolatility::Stable,
        strict: false, returns_override: Some("anyarray"), body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT, returning `boolean` (for assert 2-arg).
fn plpgsql_stable_nullable_bool(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::PlPgSql, volatility: FnVolatility::Stable,
        strict: false, returns_override: Some("boolean"), body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT, returning `anyelement` (for assert_single 2-arg).
fn plpgsql_stable_nullable_elem(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::PlPgSql, volatility: FnVolatility::Stable,
        strict: false, returns_override: Some("anyelement"), body,
    })
}

fn plpgsql_stable_returns(name: &'static str, returns: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name, language: SqlLanguage::PlPgSql, volatility: FnVolatility::Stable,
        strict: true, returns_override: Some(returns), body,
    })
}

// ── Registry ──────────────────────────────────────────────────────────────────

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        // ── std:: aggregate ──────────────────────────────────────────────────
        f("std", "count",     vec![p("s", set_of(Any))],          Int64,   B("count")),
        f("std", "sum",       vec![p("s", set_of(Int16))],        Int64,   B("sum")),
        f("std", "sum",       vec![p("s", set_of(Int32))],        Int64,   B("sum")),
        f("std", "sum",       vec![p("s", set_of(Int64))],        Int64,   B("sum")),
        f("std", "sum",       vec![p("s", set_of(Float32))],      Float32, B("sum")),
        f("std", "sum",       vec![p("s", set_of(Float64))],      Float64, B("sum")),
        f("std", "sum",       vec![p("s", set_of(Decimal))],      Decimal, B("sum")),
        f("std", "min",       vec![p("s", set_of(AnyOrderable))], opt(AnyOrderable), B("min")),
        f("std", "max",       vec![p("s", set_of(AnyOrderable))], opt(AnyOrderable), B("max")),
        f("std", "mean",      vec![p("s", set_of(Float64))],      Float64, B("avg")),
        f("std", "mean",      vec![p("s", set_of(Decimal))],      Decimal, B("avg")),
        f("std", "all",       vec![p("vals", set_of(Bool))],      Bool,    B("bool_and")),
        f("std", "any",       vec![p("vals", set_of(Bool))],      Bool,    B("bool_or")),
        f("std", "array_agg", vec![p("s", set_of(Any))],          arr(Any), B("array_agg")),

        // ── std:: set ────────────────────────────────────────────────────────
        f("std", "enumerate",
            vec![p("s", set_of(Any))],
            set_of(tup(vec![Int64, Any])),
            sql_returns("enumerate",
                "TABLE(index bigint, value anyelement)",
                "SELECT (ordinality - 1)::bigint, elem \
                 FROM unnest($1) WITH ORDINALITY AS t(elem, ordinality)")),

        f("std", "assert_single",
            vec![p("s", set_of(Any))],
            opt(Any),
            plpgsql_stable_returns("assert_single", "anyelement", r#"DECLARE n int := cardinality($1);
BEGIN
    IF n > 1 THEN
        RAISE EXCEPTION 'assert_single: expected at most 1 element, got %', n
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1[1];
END"#)),

        f("std", "assert_single",
            vec![p("s", set_of(Any)), p("msg", Str)],
            opt(Any),
            plpgsql_stable_nullable_elem("assert_single", r#"DECLARE n int := cardinality($1);
BEGIN
    IF n > 1 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, format('assert_single: expected at most 1 element, got %s', n)),
            ERRCODE = 'P0002';
    END IF;
    RETURN $1[1];
END"#)),

        f("std", "assert_exists",
            vec![p("s", set_of(Any))],
            set_of(Any),
            plpgsql_stable_returns("assert_exists", "anyarray", r#"BEGIN
    IF cardinality($1) = 0 THEN
        RAISE EXCEPTION 'assert_exists: expected at least 1 element, got none'
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#)),

        f("std", "assert_exists",
            vec![p("s", set_of(Any)), p("msg", Str)],
            set_of(Any),
            plpgsql_stable_nullable("assert_exists", r#"BEGIN
    IF cardinality($1) = 0 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, 'assert_exists: expected at least 1 element, got none'),
            ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#)),

        f("std", "assert_distinct",
            vec![p("s", set_of(Any))],
            set_of(Any),
            plpgsql_stable_returns("assert_distinct", "anyarray", r#"DECLARE has_dupes bool;
BEGIN
    SELECT EXISTS(
        SELECT 1 FROM unnest($1) t(v) GROUP BY v HAVING count(*) > 1
    ) INTO has_dupes;
    IF has_dupes THEN
        RAISE EXCEPTION 'assert_distinct: duplicate elements in set'
            USING ERRCODE = 'P0002';
    END IF;
    RETURN $1;
END"#)),

        f("std", "assert_distinct",
            vec![p("s", set_of(Any)), p("msg", Str)],
            set_of(Any),
            plpgsql_stable_nullable("assert_distinct", r#"DECLARE has_dupes bool;
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
END"#)),

        f("std", "assert",
            vec![p("condition", Bool)],
            Bool,
            plpgsql_stable_returns("assert", "boolean", r#"BEGIN
    IF NOT $1 THEN
        RAISE EXCEPTION 'assert: assertion failed'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN $1;
END"#)),

        f("std", "assert",
            vec![p("condition", Bool), p("msg", Str)],
            Bool,
            plpgsql_stable_nullable_bool("assert", r#"BEGIN
    IF $1 IS NULL OR NOT $1 THEN
        RAISE EXCEPTION USING
            MESSAGE = coalesce($2, 'assert: assertion failed'),
            ERRCODE = 'P0001';
    END IF;
    RETURN $1;
END"#)),

        // ── std:: string ─────────────────────────────────────────────────────
        f("std", "str_lower",       vec![p("s", Str)],                              Str,      B("lower")),
        f("std", "str_upper",       vec![p("s", Str)],                              Str,      B("upper")),
        f("std", "str_title",       vec![p("s", Str)],                              Str,      B("initcap")),
        f("std", "str_pad_start",   vec![p("s", Str), p("n", Int64)],               Str,      B("lpad")),
        f("std", "str_pad_start",   vec![p("s", Str), p("n", Int64), p("fill", Str)], Str,    B("lpad")),
        f("std", "str_pad_end",     vec![p("s", Str), p("n", Int64)],               Str,      B("rpad")),
        f("std", "str_pad_end",     vec![p("s", Str), p("n", Int64), p("fill", Str)], Str,    B("rpad")),
        f("std", "str_trim",        vec![p("s", Str)],                              Str,      B("btrim")),
        f("std", "str_trim",        vec![p("s", Str), p("trim", Str)],              Str,      B("btrim")),
        f("std", "str_trim_start",  vec![p("s", Str)],                              Str,      B("ltrim")),
        f("std", "str_trim_start",  vec![p("s", Str), p("trim", Str)],              Str,      B("ltrim")),
        f("std", "str_trim_end",    vec![p("s", Str)],                              Str,      B("rtrim")),
        f("std", "str_trim_end",    vec![p("s", Str), p("trim", Str)],              Str,      B("rtrim")),
        f("std", "str_repeat",      vec![p("s", Str), p("n", Int64)],               Str,      B("repeat")),
        f("std", "str_replace",     vec![p("s", Str), p("old", Str), p("new", Str)], Str,     B("replace")),
        f("std", "str_reverse",     vec![p("s", Str)],                              Str,      B("reverse")),
        f("std", "str_split",       vec![p("s", Str), p("delim", Str)],             arr(Str), E("string_to_array($1, $2)")),
        f("std", "str_contains",    vec![p("s", Str), p("sub", Str)],               Bool,     E("strpos($1, $2) > 0")),
        f("std", "str_starts_with", vec![p("s", Str), p("prefix", Str)],            Bool,     B("starts_with")),
        f("std", "str_ends_with",   vec![p("s", Str), p("suffix", Str)],            Bool,     E("right($1, length($2)) = $2")),
        // 0-based indexing at PyQL level; +1 adjusts to PostgreSQL's 1-based convention.
        f("std", "str_slice",       vec![p("s", Str), p("start", Int64)],                     Str, E("substr($1, $2 + 1)")),
        f("std", "str_slice",       vec![p("s", Str), p("start", Int64), p("end", Int64)],    Str, E("substr($1, $2 + 1, $3 - $2)")),
        f("std", "str_len",         vec![p("s", Str)],                              Int64,    B("length")),

        f("std", "re_match",
            vec![p("pattern", Str), p("s", Str)],
            arr(Str),
            sql("re_match",
                "SELECT coalesce(regexp_match($2, $1), '{}'::text[])")),

        f("std", "re_match_all",
            vec![p("pattern", Str), p("s", Str)],
            set_of(arr(Str)),
            // SETOF text[] derived automatically from Set(Array(Str))
            sql("re_match_all",
                "SELECT m FROM regexp_matches($2, $1, 'g') m")),

        // Argument order swapped vs PostgreSQL: PyQL re_replace(pattern, sub, s) → regexp_replace(s, pattern, sub).
        f("std", "re_replace",      vec![p("pattern", Str), p("sub", Str), p("s", Str)],               Str, E("regexp_replace($3, $1, $2)")),
        f("std", "re_replace",      vec![p("pattern", Str), p("sub", Str), p("s", Str), p("flags", Str)], Str, E("regexp_replace($3, $1, $2, $4)")),
        f("std", "re_test",         vec![p("pattern", Str), p("s", Str)],           Bool,     E("$2 ~ $1")),
        // 0-based find; -1 adjusts PostgreSQL's 1-based strpos result.
        f("std", "find",            vec![p("haystack", Str), p("needle", Str)],     Int64,    E("strpos($1, $2) - 1")),

        // ── std:: numeric ────────────────────────────────────────────────────
        f("std", "abs",   vec![p("n", Int16)],   Int16,   B("abs")),
        f("std", "abs",   vec![p("n", Int32)],   Int32,   B("abs")),
        f("std", "abs",   vec![p("n", Int64)],   Int64,   B("abs")),
        f("std", "abs",   vec![p("n", Float32)], Float32, B("abs")),
        f("std", "abs",   vec![p("n", Float64)], Float64, B("abs")),
        f("std", "abs",   vec![p("n", Decimal)], Decimal, B("abs")),
        f("std", "ceil",  vec![p("n", Float64)], Float64, B("ceil")),
        f("std", "ceil",  vec![p("n", Decimal)], Decimal, B("ceil")),
        f("std", "floor", vec![p("n", Float64)], Float64, B("floor")),
        f("std", "floor", vec![p("n", Decimal)], Decimal, B("floor")),
        f("std", "round", vec![p("n", Float64)],                     Float64, B("round")),
        f("std", "round", vec![p("n", Float64), p("d", Int64)],      Float64, E("round($1, $2)")),
        f("std", "round", vec![p("n", Decimal)],                     Decimal, B("round")),
        f("std", "round", vec![p("n", Decimal), p("d", Int64)],      Decimal, E("round($1, $2)")),
        f("std", "sign",  vec![p("n", Int64)],   Int64,   B("sign")),
        f("std", "sign",  vec![p("n", Float64)], Float64, B("sign")),
        f("std", "sign",  vec![p("n", Decimal)], Decimal, B("sign")),
        f("std", "sqrt",  vec![p("n", Float64)], Float64, B("sqrt")),
        f("std", "sqrt",  vec![p("n", Decimal)], Decimal, B("sqrt")),

        // ── std:: generic / polymorphic ──────────────────────────────────────
        f("std", "len",      vec![p("s", Str)],                              Int64, B("length")),
        f("std", "len",      vec![p("b", Bytes)],                            Int64, B("length")),
        f("std", "len",      vec![p("a", arr(Any))],                         Int64, E("array_length($1, 1)")),
        // str: handle empty needle (strpos returns 0 for '' in some PG versions)
        f("std", "contains", vec![p("haystack", Str),                    p("needle", Str)],                Bool, E("(CASE WHEN ($2) = '' THEN TRUE ELSE strpos($1, $2) != 0 END)")),
        f("std", "contains", vec![p("haystack", Bytes),                  p("needle", Bytes)],              Bool, E("(position($2 in $1) != 0)")),
        f("std", "contains", vec![p("haystack", arr(Any)),               p("needle", Any)],                Bool, E("($1 @> ARRAY[$2])")),
        f("std", "contains", vec![p("haystack", Json),                   p("needle", Json)],               Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", ro(AnyPoint)),           p("needle", ro(AnyPoint))],       Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", ro(AnyPoint)),           p("needle", AnyPoint)],           Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", mr(AnyPoint)),           p("needle", mr(AnyPoint))],       Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", mr(AnyPoint)),           p("needle", ro(AnyPoint))],       Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", mr(AnyPoint)),           p("needle", AnyPoint)],           Bool, E("($1 @> $2)")),
        f("std", "contains", vec![p("haystack", ro(LocalDate)),          p("needle", LocalDate)],          Bool, E("($1 @> ($2::date))")),
        f("std", "contains", vec![p("haystack", mr(LocalDate)),          p("needle", LocalDate)],          Bool, E("($1 @> ($2::date))")),

        // ── std:: uuid ───────────────────────────────────────────────────────
        f("std", "uuid_generate_v4",       vec![],          Uuid,     B("uuidv4")),
        f("std", "uuid_generate_v7",       vec![],          Uuid,     B("uuidv7")),
        f("std", "uuid_extract_timestamp", vec![p("u", Uuid)], Datetime, B("uuid_extract_timestamp")),
        f("std", "uuid_extract_version",   vec![p("u", Uuid)], Int64,    B("uuid_extract_version")),

        // ── std:: json ───────────────────────────────────────────────────────
        fc("std", "to_json",            vec![p("s", Str)],                            Json,         E("$1::jsonb")),
        f( "std", "json_typeof",        vec![p("j", Json)],                           Str,           B("jsonb_typeof")),
        // path is variadic and last → VARIADIC text[] in PG
        f( "std", "json_get",
            vec![p("j", Json), pv("path", Str)],
            opt(Json),
            sql("json_get", "SELECT $1 #> $2")),
        // path is variadic but not last → collected into text[] by the transpiler
        f( "std", "json_set",
            vec![p("j", Json), pv("path", Str), p("val", Json)],
            Json,
            sql("json_set", "SELECT jsonb_set($1, $2, $3)")),
        f( "std", "json_array_unpack",  vec![p("j", Json)],                           set_of(Json),  E("jsonb_array_elements($1)")),
        f( "std", "json_object_unpack", vec![p("j", Json)],                           set_of(tup(vec![Str, Json])), E("jsonb_each($1)")),
        f( "std", "json_array_length",  vec![p("j", Json)],                           opt(Int64),    B("jsonb_array_length")),

        // ── std:: bitwise ────────────────────────────────────────────────────
        f("std", "bit_lshift", vec![p("val", Int16), p("n", Int64)], Int16, E("(($1::int8 << $2)::int2)")),
        f("std", "bit_lshift", vec![p("val", Int32), p("n", Int64)], Int32, E("(($1::int8 << $2)::int4)")),
        f("std", "bit_lshift", vec![p("val", Int64), p("n", Int64)], Int64, E("($1 << $2)")),
        f("std", "bit_rshift", vec![p("val", Int16), p("n", Int64)], Int16, E("(($1::int8 >> $2)::int2)")),
        f("std", "bit_rshift", vec![p("val", Int32), p("n", Int64)], Int32, E("(($1::int8 >> $2)::int4)")),
        f("std", "bit_rshift", vec![p("val", Int64), p("n", Int64)], Int64, E("($1 >> $2)")),
        f("std", "to_hex",     vec![p("n", Int16)],                   Str,  E("to_hex($1::int8)")),
        f("std", "to_hex",     vec![p("n", Int32)],                   Str,  E("to_hex($1::int8)")),
        f("std", "to_hex",     vec![p("n", Int64)],                   Str,  B("to_hex")),
        f("std", "to_hex",     vec![p("n", Int32)],                   Str,  E("to_hex($1::int8)")),

        // ── std:: bytes ──────────────────────────────────────────────────────
        f("std", "bytes_get_bit", vec![p("b", Bytes), p("n", Int64)], Int64, B("get_bit")),
        f("std", "bytes_get",     vec![p("b", Bytes), p("n", Int64)], Int64, E("get_byte($1, $2)")),
        f("std", "from_hex",      vec![p("s", Str)],                  Bytes, E("decode($1, 'hex')")),
        f("std", "to_bytes",
            vec![p("s", Str), p("encoding", Str)],
            Bytes,
            sql("to_bytes", "SELECT convert_to($1, $2)")),

        // ── std:: array ──────────────────────────────────────────────────────
        // 0-based indexing at PyQL level; +1 adjusts to PostgreSQL's 1-based arrays.
        f("std", "array_get",      vec![p("a", arr(Any)), p("i", Int64)],              opt(Any),    E("($1)[$2 + 1]")),
        f("std", "array_unpack",   vec![p("a", arr(Any))],                             set_of(Any), B("unnest")),
        f("std", "array_join",     vec![p("a", arr(Str)), p("delim", Str)],            Str,         E("array_to_string($1, $2)")),
        f("std", "array_slice",    vec![p("a", arr(Any)), p("start", Int64)],          arr(Any),    E("$1[$2 + 1:]")),
        f("std", "array_slice",    vec![p("a", arr(Any)), p("start", Int64), p("end", Int64)], arr(Any), E("$1[$2 + 1:$3]")),
        f("std", "array_index_of",
            vec![p("a", arr(Any)), p("el", Any)],
            Int64,
            sql("array_index_of",
                "SELECT coalesce(\
                    (SELECT (i - 1)::int8 \
                     FROM generate_subscripts($1, 1) t(i) \
                     WHERE $1[i] IS NOT DISTINCT FROM $2 \
                     LIMIT 1), \
                 -1::int8)")),
        f("std", "array_fill",     vec![p("el", Any), p("n", Int64)],                  arr(Any),   E("array_fill($1, ARRAY[$2::int])")),
        f("std", "array_replace",  vec![p("a", arr(Any)), p("old", Any), p("new", Any)], arr(Any), B("array_replace")),
        f("std", "array_reverse",  vec![p("a", arr(Any))],                             arr(Any),   B("array_reverse")),
        f("std", "array_rotate",
            vec![p("a", arr(Any)), p("n", Int64)],
            arr(Any),
            plpgsql("array_rotate", r#"DECLARE len int := cardinality($1); k int;
BEGIN
    IF len = 0 THEN RETURN $1; END IF;
    k := (($2::int % len) + len) % len;
    IF k = 0 THEN RETURN $1; END IF;
    RETURN $1[k + 1:] || $1[1:k];
END"#)),

        // ── std:: range ──────────────────────────────────────────────────────
        // TranspilerIntrinsic: the transpiler substitutes type-specific PG
        // constructors (int8range, tstzrange, …) at compile time. No _pylon function.
        f("std", "range",        vec![p("lower", AnyPoint), p("upper", AnyPoint)], ro(AnyPoint), I("range")),
        f("std", "range",        vec![p("lower", AnyPoint), p("upper", AnyPoint), p("inc_lower", Bool), p("inc_upper", Bool)], ro(AnyPoint), I("range")),
        f("std", "range",        vec![p("empty", Bool)], ro(AnyPoint), I("range")),
        f("std", "range_unpack", vec![p("r", ro(AnyPoint))],                       set_of(AnyPoint), E("generate_series($1)")),
        f("std", "range_unpack", vec![p("r", ro(AnyPoint)), p("step", AnyPoint)],  set_of(AnyPoint), E("generate_series(lower($1), upper($1), $2)")),
        f("std", "range_get_lower",          vec![p("r", ro(AnyPoint))], opt(AnyPoint), B("lower")),
        f("std", "range_get_upper",          vec![p("r", ro(AnyPoint))], opt(AnyPoint), B("upper")),
        f("std", "range_is_empty",           vec![p("r", ro(AnyPoint))], Bool, B("isempty")),
        f("std", "range_is_inclusive_lower", vec![p("r", ro(AnyPoint))], Bool, B("lower_inc")),
        f("std", "range_is_inclusive_upper", vec![p("r", ro(AnyPoint))], Bool, B("upper_inc")),
        f("std", "overlaps",     vec![p("a", ro(AnyPoint)), p("b", ro(AnyPoint))], Bool, O("&&")),
        f("std", "multirange",   vec![p("ranges", arr(ro(AnyPoint)))], mr(AnyPoint), I("multirange")),

        // ── std:: datetime ───────────────────────────────────────────────────
        f("std", "datetime_current",        vec![], Datetime, E("clock_timestamp()")),
        f("std", "datetime_of_transaction", vec![], Datetime, E("transaction_timestamp()")),
        f("std", "datetime_of_statement",   vec![], Datetime, E("statement_timestamp()")),

        // PylonFunction: PG extract requires a keyword field, not a text argument.
        f("std", "datetime_get",
            vec![p("dt", Datetime), p("el", Str)],
            Float64,
            plpgsql("datetime_get", r#"DECLARE result float8;
BEGIN
    CASE $2
        WHEN 'year'        THEN result := extract(year         FROM $1);
        WHEN 'month'       THEN result := extract(month        FROM $1);
        WHEN 'day'         THEN result := extract(day          FROM $1);
        WHEN 'hour'        THEN result := extract(hour         FROM $1);
        WHEN 'minute'      THEN result := extract(minute       FROM $1);
        WHEN 'second'      THEN result := extract(second       FROM $1);
        WHEN 'microsecond' THEN result := extract(microseconds FROM $1);
        WHEN 'millisecond' THEN result := extract(milliseconds FROM $1);
        WHEN 'epoch'       THEN result := extract(epoch        FROM $1);
        WHEN 'timezone'    THEN result := extract(timezone     FROM $1);
        WHEN 'dow'         THEN result := extract(dow          FROM $1);
        WHEN 'doy'         THEN result := extract(doy          FROM $1);
        WHEN 'week'        THEN result := extract(week         FROM $1);
        WHEN 'quarter'     THEN result := extract(quarter      FROM $1);
        ELSE RAISE EXCEPTION 'datetime_get: unknown field: %', $2;
    END CASE;
    RETURN result;
END"#)),

        // Note argument swap: PyQL truncate(dt, unit) → date_trunc(unit, dt).
        f("std", "datetime_truncate", vec![p("dt", Datetime), p("unit", Str)], Datetime, E("date_trunc($2, $1)")),
        f("std", "datetime_shift",    vec![p("dt", Datetime), p("delta", Duration)], Datetime, E("$1 + $2")),

        f("std", "duration_get",
            vec![p("d", Duration), p("el", Str)],
            Float64,
            plpgsql("duration_get", r#"DECLARE result float8;
BEGIN
    CASE $2
        WHEN 'hours'        THEN result := extract(hour         FROM $1);
        WHEN 'minutes'      THEN result := extract(minute       FROM $1);
        WHEN 'seconds'      THEN result := extract(second       FROM $1);
        WHEN 'microseconds' THEN result := extract(microseconds FROM $1);
        WHEN 'milliseconds' THEN result := extract(milliseconds FROM $1);
        WHEN 'epoch'        THEN result := extract(epoch        FROM $1);
        ELSE RAISE EXCEPTION 'duration_get: unknown field: %', $2;
    END CASE;
    RETURN result;
END"#)),

        f("std", "duration_to_seconds", vec![p("d", Duration)], Decimal, E("extract(epoch from $1)")),

        f("std", "to_datetime", vec![p("s", Str), p("fmt", Str)], Datetime, E("to_timestamp($1, $2)")),
        f("std", "to_datetime",
            vec![p("year", Int64), p("month", Int64), p("day", Int64),
                 p("hour", Int64), p("min", Int64), p("sec", Float64), p("timezone", Str)],
            Datetime,
            sql("to_datetime",
                "SELECT make_timestamptz($1::int, $2::int, $3::int, $4::int, $5::int, $6, $7)")),
        // Single-arg ISO 8601 parsing; cast target for str → datetime.
        fc("std", "to_datetime",
            vec![p("s", Str)],
            Datetime,
            sql("to_datetime", "SELECT $1::timestamptz")),

        f("std", "to_datetime", vec![p("epoch_seconds", Decimal)], Datetime, E("to_timestamp($1)")),

        f("std", "to_duration",
            vec![p("hours", Int64), p("minutes", Int64), p("seconds", Float64)],
            Duration,
            sql("to_duration",
                "SELECT make_interval(hours => $1::int, mins => $2::int, secs => $3)")),

        // ── std:: type conversion ────────────────────────────────────────────
        f( "std", "to_str",    vec![p("v", Datetime), p("fmt", Str)], Str,     E("to_char($1, $2)")),
        fc("std", "to_str",    vec![p("v", Datetime)],                Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Int16)],                   Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Int32)],                   Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Int64)],                   Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Float32)],                 Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Float64)],                 Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Decimal)],                 Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", BigInt)],                  Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Bool)],                    Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Json)],                    Str,     E("$1::text")),
        f( "std", "to_str",    vec![p("v", Bytes), p("encoding", Str)], Str,
            sql("to_str_bytes", "SELECT convert_from($1, $2)")),
        fc("std", "to_str",    vec![p("v", Duration)],                Str,     E("$1::text")),
        fc("std", "to_str",    vec![p("v", Uuid)],                    Str,     E("$1::text")),

        fc("std", "to_int16",  vec![p("s", Str)],   Int16,   E("$1::int2")),
        f( "std", "to_int16",  vec![p("b", Bool)],  Int16,   E("$1::int2")),
        fc("std", "to_int32",  vec![p("s", Str)],   Int32,   E("$1::int4")),
        f( "std", "to_int32",  vec![p("b", Bool)],  Int32,   E("$1::int4")),
        fc("std", "to_int64",  vec![p("s", Str)],   Int64,   E("$1::int8")),
        f( "std", "to_int64",  vec![p("b", Bool)],  Int64,   E("$1::int8")),
        fc("std", "to_float32",vec![p("s", Str)],   Float32, E("$1::float4")),
        f( "std", "to_float32",vec![p("n", Int64)], Float32, E("$1::float4")),
        fc("std", "to_float64",vec![p("s", Str)],   Float64, E("$1::float8")),
        f( "std", "to_float64",vec![p("n", Int64)], Float64, E("$1::float8")),
        fc("std", "to_decimal",vec![p("s", Str)],   Decimal, E("$1::numeric")),
        f( "std", "to_decimal",vec![p("n", Int64)], Decimal, E("$1::numeric")),
        fc("std", "to_bigint", vec![p("s", Str)],   BigInt,  E("$1::numeric")),
        f( "std", "to_bigint", vec![p("n", Int64)], BigInt,  E("$1::numeric")),

        fc("std", "to_bool", vec![p("s", Str)],  Bool, E("$1::bool")),
        // int → bool: PG has no native int::bool; _pylon.to_bool maps 0 → false, else true.
        fc("std", "to_bool",
            vec![p("n", Int16)], Bool,
            sql("to_bool", "SELECT $1 <> 0::int2")),
        fc("std", "to_bool",
            vec![p("n", Int32)], Bool,
            sql("to_bool", "SELECT $1 <> 0::int4")),
        fc("std", "to_bool",
            vec![p("n", Int64)], Bool,
            sql("to_bool", "SELECT $1 <> 0::int8")),

        // ── math:: ───────────────────────────────────────────────────────────
        f("math", "pi",         vec![],                                     Float64, E("pi()")),
        f("math", "e",          vec![],                                     Float64, E("exp(1.0)")),
        f("math", "exp",        vec![p("n", Float64)],                      Float64, B("exp")),
        f("math", "ln",         vec![p("n", Float64)],                      Float64, B("ln")),
        f("math", "log",        vec![p("n", Float64)],                      Float64, B("log")),
        // Two-arg form: PyQL log(n, base) → PG log(base, n) — arguments are swapped.
        f("math", "log",        vec![p("n", Float64), p("base", Float64)],  Float64, E("log($2, $1)")),
        f("math", "log2",       vec![p("n", Float64)],                      Float64, E("log(2.0, $1)")),
        f("math", "log10",      vec![p("n", Float64)],                      Float64, B("log")),
        f("math", "sin",        vec![p("n", Float64)],                      Float64, B("sin")),
        f("math", "cos",        vec![p("n", Float64)],                      Float64, B("cos")),
        f("math", "tan",        vec![p("n", Float64)],                      Float64, B("tan")),
        f("math", "asin",       vec![p("n", Float64)],                      Float64, B("asin")),
        f("math", "acos",       vec![p("n", Float64)],                      Float64, B("acos")),
        f("math", "atan",       vec![p("n", Float64)],                      Float64, B("atan")),
        f("math", "atan2",      vec![p("y", Float64), p("x", Float64)],     Float64, B("atan2")),
        f("math", "stddev",     vec![p("s", set_of(Float64))],              Float64, B("stddev_samp")),
        f("math", "stddev",     vec![p("s", set_of(Decimal))],              Decimal, B("stddev_samp")),
        f("math", "stddev_pop", vec![p("s", set_of(Float64))],              Float64, B("stddev_pop")),
        f("math", "stddev_pop", vec![p("s", set_of(Decimal))],              Decimal, B("stddev_pop")),
        f("math", "var",        vec![p("s", set_of(Float64))],              Float64, B("var_samp")),
        f("math", "var",        vec![p("s", set_of(Decimal))],              Decimal, B("var_samp")),
        f("math", "var_pop",    vec![p("s", set_of(Float64))],              Float64, B("var_pop")),
        f("math", "var_pop",    vec![p("s", set_of(Decimal))],              Decimal, B("var_pop")),

        // ── cal:: ────────────────────────────────────────────────────────────
        f("cal", "to_local_datetime", vec![p("dt", Datetime), p("timezone", Str)], LocalDatetime, E("$1 AT TIME ZONE $2")),
        f("cal", "to_local_datetime", vec![p("year", Int64), p("month", Int64), p("day", Int64), p("hour", Int64), p("min", Int64), p("sec", Float64)], LocalDatetime, E("make_timestamp($1,$2,$3,$4,$5,$6)")),
        f("cal", "to_local_datetime", vec![p("s", Str), p("fmt", Str)], LocalDatetime, E("to_timestamp($1,$2)::timestamp")),
        f("cal", "to_local_date",     vec![p("dt", LocalDatetime)],     LocalDate, E("$1::date")),
        f("cal", "to_local_date",     vec![p("year", Int64), p("month", Int64), p("day", Int64)], LocalDate, E("make_date($1,$2,$3)")),
        f("cal", "to_local_date",     vec![p("s", Str), p("fmt", Str)], LocalDate, E("to_date($1,$2)")),
        f("cal", "to_local_time",     vec![p("dt", LocalDatetime)],     LocalTime, E("$1::time")),
        f("cal", "to_local_time",     vec![p("hour", Int64), p("min", Int64), p("sec", Float64)], LocalTime, E("make_time($1,$2,$3)")),
        f("cal", "to_local_time",     vec![p("s", Str), p("fmt", Str)], LocalTime, E("to_timestamp($1,$2)::time")),

        // PylonFunction: PG extract requires a keyword field, not a text argument.
        f("cal", "local_datetime_get",
            vec![p("dt", LocalDatetime), p("el", Str)],
            Float64,
            plpgsql("local_datetime_get", r#"DECLARE result float8;
BEGIN
    CASE $2
        WHEN 'year'        THEN result := extract(year         FROM $1);
        WHEN 'month'       THEN result := extract(month        FROM $1);
        WHEN 'day'         THEN result := extract(day          FROM $1);
        WHEN 'hour'        THEN result := extract(hour         FROM $1);
        WHEN 'minute'      THEN result := extract(minute       FROM $1);
        WHEN 'second'      THEN result := extract(second       FROM $1);
        WHEN 'microsecond' THEN result := extract(microseconds FROM $1);
        WHEN 'millisecond' THEN result := extract(milliseconds FROM $1);
        WHEN 'epoch'       THEN result := extract(epoch        FROM $1);
        WHEN 'dow'         THEN result := extract(dow          FROM $1);
        WHEN 'doy'         THEN result := extract(doy          FROM $1);
        WHEN 'week'        THEN result := extract(week         FROM $1);
        WHEN 'quarter'     THEN result := extract(quarter      FROM $1);
        ELSE RAISE EXCEPTION 'local_datetime_get: unknown field: %', $2;
    END CASE;
    RETURN result;
END"#)),

        f("cal", "date_get",
            vec![p("d", LocalDate), p("el", Str)],
            Float64,
            plpgsql("date_get", r#"DECLARE result float8;
BEGIN
    CASE $2
        WHEN 'year'    THEN result := extract(year    FROM $1);
        WHEN 'month'   THEN result := extract(month   FROM $1);
        WHEN 'day'     THEN result := extract(day     FROM $1);
        WHEN 'dow'     THEN result := extract(dow     FROM $1);
        WHEN 'doy'     THEN result := extract(doy     FROM $1);
        WHEN 'week'    THEN result := extract(week    FROM $1);
        WHEN 'quarter' THEN result := extract(quarter FROM $1);
        ELSE RAISE EXCEPTION 'date_get: unknown field: %', $2;
    END CASE;
    RETURN result;
END"#)),

        f("cal", "time_get",
            vec![p("t", LocalTime), p("el", Str)],
            Float64,
            plpgsql("time_get", r#"DECLARE result float8;
BEGIN
    CASE $2
        WHEN 'hour'        THEN result := extract(hour         FROM $1);
        WHEN 'minute'      THEN result := extract(minute       FROM $1);
        WHEN 'second'      THEN result := extract(second       FROM $1);
        WHEN 'microsecond' THEN result := extract(microseconds FROM $1);
        WHEN 'millisecond' THEN result := extract(milliseconds FROM $1);
        ELSE RAISE EXCEPTION 'time_get: unknown field: %', $2;
    END CASE;
    RETURN result;
END"#)),

        f("cal", "to_duration",
            vec![p("days", Int64), p("hours", Int64), p("minutes", Int64), p("seconds", Float64)],
            RelativeDuration,
            sql("to_relative_duration",
                "SELECT make_interval(days => $1::int, hours => $2::int, mins => $3::int, secs => $4)")),

        f("cal", "duration_normalize_hours",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_hours", "SELECT justify_hours($1)")),

        f("cal", "duration_normalize_days",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_days", "SELECT justify_days(justify_hours($1))")),

        // ── sys ───────────────────────────────────────────────────────────────

        f("sys", "get_current_database",
            vec![],
            Str,
            B("current_database")),

        f("sys", "get_version_as_str",
            vec![],
            Str,
            E(concat!("'", env!("CARGO_PKG_VERSION"), "'"))),

        f("sys", "get_version",
            vec![],
            Tuple(vec![Int64, Int64, Str, Int64, Array(Box::new(Str))]),
            E(env!("PYLON_VERSION_ROW"))),

        // ── std:: sequences ──────────────────────────────────────────────────
        // TranspilerIntrinsic: first arg is a sequence scalar type name resolved
        // by the compiler; emits nextval(...) / setval(...) directly.
        f("std", "sequence_next",  vec![p("seq", Any)],                  Int64, I("sequence_next")),
        f("std", "sequence_reset", vec![p("seq", Any)],                  Int64, I("sequence_reset")),
        f("std", "sequence_reset", vec![p("seq", Any), p("val", Int64)], Int64, I("sequence_reset")),

        // ── pgvector:: ────────────────────────────────────────────────────────
        f("pgvector", "euclidean_distance",  vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <-> $2)")),
        f("pgvector", "cosine_distance",     vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <=> $2)")),
        f("pgvector", "neg_inner_product",   vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <#> $2)")),
        f("pgvector", "inner_product",       vec![p("a", Vector), p("b", Vector)], Float64, E("(0.0 - ($1 <#> $2))")),

        // ── crypto:: (pgcrypto) ─────────────────────────────────────────────
        // Mirrors Gel's `ext::pgcrypto` under the `crypto` prefix, straight
        // passthrough to PostgreSQL's `pgcrypto` extension (`CREATE
        // EXTENSION IF NOT EXISTS pgcrypto;` must be run on the target
        // database — same expectation as `pgvector`'s `vector` extension,
        // neither of which Pylon auto-provisions).
        f("crypto", "digest", vec![p("data", Str),   p("type", Str)], Bytes, B("digest")),
        f("crypto", "digest", vec![p("data", Bytes), p("type", Str)], Bytes, B("digest")),
        f("crypto", "hmac",   vec![p("data", Str),   p("key", Str),   p("type", Str)], Bytes, B("hmac")),
        f("crypto", "hmac",   vec![p("data", Bytes), p("key", Bytes), p("type", Str)], Bytes, B("hmac")),
        // Zero-arg form defaults to blowfish ("bf"), matching Gel's own default.
        f("crypto", "gen_salt", vec![],                                          Str, E("gen_salt('bf')")),
        f("crypto", "gen_salt", vec![p("type", Str)],                            Str, B("gen_salt")),
        // pgcrypto's gen_salt(type, iter_count) takes iter_count as int4; Pylon's
        // int64 needs an explicit narrowing cast (PG has no implicit int8 -> int4).
        f("crypto", "gen_salt", vec![p("type", Str), p("iter_count", Int64)],    Str, E("gen_salt($1, $2::int4)")),
        f("crypto", "crypt",  vec![p("password", Str), p("salt", Str)],         Str, B("crypt")),
    ]
}
