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
use super::{E, NamedDefault, f, p, plpgsql, pn, sql};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f(
            "cal",
            "to_local_datetime",
            vec![p("dt", Datetime), p("timezone", Str)],
            LocalDatetime,
            E("$1 AT TIME ZONE $2"),
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
        f(
            "cal",
            "to_local_datetime",
            vec![p("s", Str), p("fmt", Str)],
            LocalDatetime,
            E("to_timestamp($1,$2)::timestamp"),
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
            vec![p("s", Str), p("fmt", Str)],
            LocalDate,
            E("to_date($1,$2)"),
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
        f(
            "cal",
            "to_local_time",
            vec![p("s", Str), p("fmt", Str)],
            LocalTime,
            E("to_timestamp($1,$2)::time"),
        ),
        // PylonFunction: PG extract requires a keyword field, not a text argument.
        f(
            "cal",
            "local_datetime_get",
            vec![p("dt", LocalDatetime), p("el", Str)],
            Float64,
            plpgsql(
                "local_datetime_get",
                r#"DECLARE result float8;
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
END"#,
            ),
        ),
        f(
            "cal",
            "date_get",
            vec![p("d", LocalDate), p("el", Str)],
            Float64,
            plpgsql(
                "date_get",
                r#"DECLARE result float8;
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
                r#"DECLARE result float8;
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
END"#,
            ),
        ),
        f(
            "cal",
            "to_duration",
            vec![
                p("days", Int64),
                p("hours", Int64),
                p("minutes", Int64),
                p("seconds", Float64),
            ],
            RelativeDuration,
            sql(
                "to_relative_duration",
                "SELECT make_interval(days => $1::int, hours => $2::int, mins => $3::int, secs => $4)",
            ),
        ),
        // Fuller positional form of cal::to_relative_duration (whose params
        // are conventionally NAMED ONLY — Pylon has no named-only
        // parameter support, so this is positional instead).
        f(
            "cal",
            "to_duration",
            vec![
                p("years", Int64),
                p("months", Int64),
                p("days", Int64),
                p("hours", Int64),
                p("minutes", Int64),
                p("seconds", Float64),
                p("microseconds", Int64),
            ],
            RelativeDuration,
            sql(
                "to_relative_duration_full",
                "SELECT make_interval(years => $1::int, months => $2::int, days => $3::int, \
                 hours => $4::int, mins => $5::int, secs => $6) \
                 + ($7::text || ' microseconds')::interval",
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
            E("(make_interval(years => $1::int, months => $2::int, days => $3::int, hours => $4::int, \
               mins => $5::int, secs => $6) + make_interval(secs => $7 / 1000000.0))"),
        ),
        f(
            "cal",
            "duration_normalize_hours",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_hours", "SELECT justify_hours($1)"),
        ),
        f(
            "cal",
            "duration_normalize_days",
            vec![p("d", RelativeDuration)],
            RelativeDuration,
            sql("duration_normalize_days", "SELECT justify_days(justify_hours($1))"),
        ),
    ]
}
