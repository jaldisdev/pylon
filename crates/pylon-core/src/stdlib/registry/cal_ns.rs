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

use super::{Datetime, Float64, FnDescriptor, Int64, LocalDate, LocalDatetime, LocalTime, RelativeDuration, Str};
use super::{E, NamedDefault, f, opt, p, plpgsql, plpgsql_nullable, pn, sql};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f(
            "cal",
            "to_local_datetime",
            vec![p("dt", Datetime), p("timezone", Str)],
            LocalDatetime,
            E("($1 AT TIME ZONE $2)"),
        ),
        f(
            "cal",
            "to_local_datetime",
            vec![
                p("year", Int64),
                p("month", Int64),
                p("day", Int64),
                p("hour", Int64),
                p("min", Int64),
                p("sec", Float64),
            ],
            LocalDatetime,
            E("make_timestamp($1,$2,$3,$4,$5,$6)"),
        ),
        // A bare string is ISO 8601. PostgreSQL's own `::timestamp` accepts
        // far more than that — `01/16/2026`, `Jan 16 2026`, a trailing zone,
        // orderings that depend on the session's DateStyle — so the shape is
        // checked before the cast.
        f(
            "cal",
            "to_local_datetime",
            vec![p("s", Str)],
            LocalDatetime,
            plpgsql(
                "to_local_datetime",
                r#"BEGIN
    IF $1 !~ '^\s*((\d{4}-\d{2}-\d{2}|\d{8})[ tT](\d{2}(:\d{2}(:\d{2}(\.\d+)?)?)?|\d{2,6}(\.\d+)?))\s*$' THEN
        RAISE EXCEPTION 'invalid input syntax for type cal::local_datetime: %', quote_literal($1)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Please use ISO8601 format. Example 2010-04-18T09:27:00';
    END IF;
    RETURN $1::timestamp;
END"#,
            ),
        ),
        // `fmt` is optional: left out, or passed as an empty set, the string
        // is read as ISO 8601 by the overload above.
        f(
            "cal",
            "to_local_datetime",
            vec![p("s", Str), p("fmt", opt(Str))],
            LocalDatetime,
            plpgsql_nullable(
                "to_local_datetime",
                r#"BEGIN
    IF $2 IS NULL THEN
        RETURN _pylon.to_local_datetime($1);
    END IF;
    IF $2 = '' THEN
        RAISE EXCEPTION 'to_local_datetime(): "fmt" argument must be a non-empty string'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF $2 ~ '^(("([^"\\]|\\.)*")|([^"]+))*(TZH|TZM).*$' THEN
        RAISE EXCEPTION 'unexpected time zone in format: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format';
    END IF;
    RETURN to_timestamp($1, $2)::timestamp;
END"#,
            ),
        ),
        f(
            "cal",
            "to_local_date",
            vec![p("dt", Datetime), p("timezone", Str)],
            LocalDate,
            E("($1 AT TIME ZONE $2)::date"),
        ),
        f(
            "cal",
            "to_local_date",
            vec![p("dt", LocalDatetime)],
            LocalDate,
            E("$1::date"),
        ),
        f(
            "cal",
            "to_local_date",
            vec![p("year", Int64), p("month", Int64), p("day", Int64)],
            LocalDate,
            E("make_date($1,$2,$3)"),
        ),
        f(
            "cal",
            "to_local_date",
            vec![p("s", Str)],
            LocalDate,
            plpgsql(
                "to_local_date",
                r#"BEGIN
    IF $1 !~ '^\s*(\d{4}-\d{2}-\d{2}|\d{8})\s*$' THEN
        RAISE EXCEPTION 'invalid input syntax for type cal::local_date: %', quote_literal($1)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Please use ISO8601 format. Example 2010-04-18';
    END IF;
    RETURN $1::date;
END"#,
            ),
        ),
        f(
            "cal",
            "to_local_date",
            vec![p("s", Str), p("fmt", opt(Str))],
            LocalDate,
            plpgsql_nullable(
                "to_local_date",
                r#"BEGIN
    IF $2 IS NULL THEN
        RETURN _pylon.to_local_date($1);
    END IF;
    IF $2 = '' THEN
        RAISE EXCEPTION 'to_local_date(): "fmt" argument must be a non-empty string'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF $2 ~ '^(("([^"\\]|\\.)*")|([^"]+))*(TZH|TZM).*$' THEN
        RAISE EXCEPTION 'unexpected time zone in format: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format';
    END IF;
    RETURN to_date($1, $2);
END"#,
            ),
        ),
        f(
            "cal",
            "to_local_time",
            vec![p("dt", Datetime), p("timezone", Str)],
            LocalTime,
            E("($1 AT TIME ZONE $2)::time"),
        ),
        f(
            "cal",
            "to_local_time",
            vec![p("dt", LocalDatetime)],
            LocalTime,
            E("$1::time"),
        ),
        f(
            "cal",
            "to_local_time",
            vec![p("hour", Int64), p("min", Int64), p("sec", Float64)],
            LocalTime,
            E("make_time($1,$2,$3)"),
        ),
        // `24:00:00` is a valid PostgreSQL `time` but not a valid ISO 8601
        // time of day, so it is rejected after the cast — the pattern cannot
        // tell 24 from 23.
        f(
            "cal",
            "to_local_time",
            vec![p("s", Str)],
            LocalTime,
            plpgsql(
                "to_local_time",
                r#"DECLARE result time;
BEGIN
    IF $1 !~ '^\s*(\d{2}(:\d{2}(:\d{2}(\.\d+)?)?)?|\d{2,6}(\.\d+)?)\s*$' THEN
        RAISE EXCEPTION 'invalid input syntax for type cal::local_time: %', quote_literal($1)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Please use ISO8601 format. Examples: 18:43:27 or 18:43';
    END IF;
    result := $1::time;
    IF date_part('hour', result) = 24 THEN
        RAISE EXCEPTION 'cal::local_time field value out of range: %', quote_literal($1)
            USING ERRCODE = 'invalid_datetime_format';
    END IF;
    RETURN result;
END"#,
            ),
        ),
        f(
            "cal",
            "to_local_time",
            vec![p("s", Str), p("fmt", opt(Str))],
            LocalTime,
            plpgsql_nullable(
                "to_local_time",
                r#"BEGIN
    IF $2 IS NULL THEN
        RETURN _pylon.to_local_time($1);
    END IF;
    IF $2 = '' THEN
        RAISE EXCEPTION 'to_local_time(): "fmt" argument must be a non-empty string'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF $2 ~ '^(("([^"\\]|\\.)*")|([^"]+))*(TZH|TZM).*$' THEN
        RAISE EXCEPTION 'unexpected time zone in format: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format';
    END IF;
    RETURN to_timestamp($1, $2)::time;
END"#,
            ),
        ),
        // PylonFunction: the accepted units are checked before `date_part`
        // sees them, so an unknown one names this function rather than PG's.
        f(
            "cal",
            "date_get",
            vec![p("d", LocalDate), p("el", Str)],
            Float64,
            plpgsql(
                "date_get",
                r#"BEGIN
    IF $2 NOT IN ('century', 'day', 'decade', 'dow', 'doy', 'isodow', 'isoyear',
                  'millennium', 'month', 'quarter', 'week', 'year') THEN
        RAISE EXCEPTION 'invalid unit for cal::date_get: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: century, day, decade, dow, doy, isodow, isoyear, millennium, month, quarter, week, year.';
    END IF;
    RETURN date_part($2, $1);
END"#,
            ),
        ),
        f(
            "cal",
            "time_get",
            vec![p("t", LocalTime), p("el", Str)],
            Float64,
            plpgsql(
                "time_get",
                r#"BEGIN
    IF $2 = 'midnightseconds' THEN
        RETURN date_part('epoch', $1);
    END IF;
    IF $2 NOT IN ('hour', 'microseconds', 'milliseconds', 'minutes', 'seconds') THEN
        RAISE EXCEPTION 'invalid unit for cal::time_get: %', quote_literal($2)
            USING ERRCODE = 'invalid_datetime_format',
                  HINT = 'Supported units: hour, microseconds, midnightseconds, milliseconds, minutes, seconds.';
    END IF;
    RETURN date_part($2, $1);
END"#,
            ),
        ),
        f(
            "cal",
            "to_relative_duration",
            vec![
                pn("years", Int64, NamedDefault::Int(0)),
                pn("months", Int64, NamedDefault::Int(0)),
                pn("days", Int64, NamedDefault::Int(0)),
                pn("hours", Int64, NamedDefault::Int(0)),
                pn("minutes", Int64, NamedDefault::Int(0)),
                pn("seconds", Float64, NamedDefault::Int(0)),
                pn("microseconds", Int64, NamedDefault::Int(0)),
            ],
            RelativeDuration,
            E(
                "(make_interval(years => $1::int, months => $2::int, days => $3::int, hours => $4::int, \
               mins => $5::int, secs => $6) + make_interval(secs => $7 / 1000000.0))",
            ),
        ),
        // `cal::date_duration` is `interval` as well, so this returns the same
        // PostgreSQL type as `to_relative_duration` — what separates them is
        // that the units below a day cannot be given here.
        f(
            "cal",
            "to_date_duration",
            vec![
                pn("years", Int64, NamedDefault::Int(0)),
                pn("months", Int64, NamedDefault::Int(0)),
                pn("days", Int64, NamedDefault::Int(0)),
            ],
            RelativeDuration,
            E("make_interval(years => $1::int, months => $2::int, days => $3::int)"),
        ),
        f(
            "cal",
            "duration_normalize_hours",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_hours", "SELECT justify_hours($1)"),
        ),
        // `justify_days` alone: 30-day chunks become months, and hours are
        // left where they are. Rolling hours up into days first would make
        // `720 hours` normalize to one month.
        f(
            "cal",
            "duration_normalize_days",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_days", "SELECT justify_days($1)"),
        ),
    ]
}
