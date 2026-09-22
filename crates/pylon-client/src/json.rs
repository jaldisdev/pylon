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

//! A decoded result rendered as JSON, the way Gel renders its own: objects
//! carry exactly the pointers their shape selected, in shape order;
//! temporal values are ISO 8601 strings; durations are ISO 8601 durations;
//! bytes are base64; enums are their label; decimals keep every digit.

use chrono::{Duration, NaiveDate, NaiveTime};
use pylon_core::query::ShapeNode;
use pylon_value::DecodedValue;

use crate::value::Value;

/// One result row, decoded against `shape`, as a JSON document.
pub fn row_to_json(shape: &ShapeNode, row: &DecodedValue) -> String {
    to_json(&crate::decode::decode(shape, row))
}

pub fn to_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int64(n) => out.push_str(&n.to_string()),
        Value::Float64(f) => out.push_str(&serde_json::to_string(f).unwrap_or_else(|_| "null".to_string())),
        Value::Str(s) => write_str(s, out),
        Value::Bytes(bytes) => write_str(&base64(bytes), out),
        Value::Uuid(u) => write_str(&u.hyphenated().to_string(), out),
        // Postgres renders `NaN`/`Infinity` for numeric too; JSON has no
        // literal for either.
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => out.push_str(s),
        Value::Decimal(s) => write_str(s, out),
        Value::Duration {
            months,
            days,
            microseconds,
        } => write_str(&iso_duration(*months, *days, *microseconds), out),
        Value::Date(days) => write_str(&date(*days).to_string(), out),
        Value::Time(micros) => write_str(&time(*micros), out),
        Value::Timestamp(micros) => write_str(&timestamp(*micros), out),
        Value::Timestamptz(micros) => write_str(&format!("{}+00:00", timestamp(*micros)), out),
        Value::Range(range) => {
            if range.empty {
                out.push_str(r#"{"empty": true}"#);
                return;
            }
            out.push_str(r#"{"lower": "#);
            write(range.lower.as_ref().unwrap_or(&Value::Null), out);
            out.push_str(&format!(r#", "inc_lower": {}, "upper": "#, range.inc_lower));
            write(range.upper.as_ref().unwrap_or(&Value::Null), out);
            out.push_str(&format!(r#", "inc_upper": {}}}"#, range.inc_upper));
        }
        Value::Array(items) | Value::Tuple(items) => write_list(items.iter(), out),
        Value::Object(object) => write_fields(object.fields(), out),
        Value::Enum { value, .. } => write_str(value, out),
        Value::Group(group) => {
            out.push_str(r#"{"key": "#);
            write_fields(group.key.fields(), out);
            out.push_str(r#", "grouping": "#);
            out.push_str(&serde_json::to_string(&group.grouping).unwrap_or_else(|_| "[]".to_string()));
            out.push_str(r#", "elements": "#);
            write_list(group.elements.iter(), out);
            out.push('}');
        }
        Value::VectorSearch { object, distance } => {
            out.push_str(r#"{"object": "#);
            write(object, out);
            out.push_str(r#", "distance": "#);
            write(&Value::Float64(*distance), out);
            out.push('}');
        }
        Value::FtsSearch { object, score } => {
            out.push_str(r#"{"object": "#);
            write(object, out);
            out.push_str(r#", "score": "#);
            write(&Value::Float64(*score), out);
            out.push('}');
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()));
}

fn write_list<'v>(items: impl Iterator<Item = &'v Value>, out: &mut String) {
    out.push('[');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write(item, out);
    }
    out.push(']');
}

fn write_fields<'v>(fields: impl Iterator<Item = (&'v str, &'v Value)>, out: &mut String) {
    out.push('{');
    for (i, (name, value)) in fields.enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_str(name, out);
        out.push_str(": ");
        write(value, out);
    }
    out.push('}');
}

fn pg_epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default()
}

fn date(days: i32) -> NaiveDate {
    pg_epoch() + Duration::days(i64::from(days))
}

/// `19:44:36`, `19:44:36.5` — the fraction only when there is one, without
/// trailing zeros.
fn time(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let clock = NaiveTime::from_num_seconds_from_midnight_opt(seconds.rem_euclid(86_400) as u32, 0)
        .unwrap_or_default()
        .format("%H:%M:%S")
        .to_string();
    format!("{clock}{}", fraction_suffix(fraction))
}

fn timestamp(micros: i64) -> String {
    let days = micros.div_euclid(86_400_000_000);
    let within_day = micros.rem_euclid(86_400_000_000);
    format!("{}T{}", date(days as i32), time(within_day))
}

fn fraction_suffix(fraction_micros: i64) -> String {
    if fraction_micros == 0 {
        return String::new();
    }
    format!(".{fraction_micros:06}").trim_end_matches('0').to_string()
}

/// `PT1H30M2.5S`, `P1M2DT3H`, `PT0S` for nothing at all.
fn iso_duration(months: i32, days: i32, micros: i64) -> String {
    let mut out = String::from("P");
    let (years, months) = (months / 12, months % 12);
    for (amount, unit) in [(i64::from(years), 'Y'), (i64::from(months), 'M'), (i64::from(days), 'D')] {
        if amount != 0 {
            out.push_str(&format!("{amount}{unit}"));
        }
    }
    let negative = micros < 0;
    let magnitude = micros.unsigned_abs();
    let hours = magnitude / 3_600_000_000;
    let minutes = magnitude % 3_600_000_000 / 60_000_000;
    let seconds = magnitude % 60_000_000 / 1_000_000;
    let fraction = (magnitude % 1_000_000) as i64;
    let sign = if negative { "-" } else { "" };
    let mut clock = String::new();
    if hours != 0 {
        clock.push_str(&format!("{sign}{hours}H"));
    }
    if minutes != 0 {
        clock.push_str(&format!("{sign}{minutes}M"));
    }
    if seconds != 0 || fraction != 0 {
        clock.push_str(&format!("{sign}{seconds}{}S", fraction_suffix(fraction)));
    }
    if !clock.is_empty() {
        out.push('T');
        out.push_str(&clock);
    }
    if out == "P" {
        out.push_str("T0S");
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporal_values_match_gels_rendering() {
        // 2026-09-21 is 9760 days after 2000-01-01.
        assert_eq!(date(9760).to_string(), "2026-09-21");
        assert_eq!(time(71_076_000_000), "19:44:36");
        assert_eq!(time(71_076_500_000), "19:44:36.5");
        assert_eq!(timestamp(9760 * 86_400_000_000 + 71_076_224_624), "2026-09-21T19:44:36.224624");
    }

    #[test]
    fn durations_match_gels_rendering() {
        assert_eq!(iso_duration(0, 0, 5_402_500_000), "PT1H30M2.5S");
        assert_eq!(iso_duration(1, 2, 10_800_000_000), "P1M2DT3H");
        assert_eq!(iso_duration(0, 0, 0), "PT0S");
    }

    #[test]
    fn bytes_are_base64() {
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"a"), "YQ==");
    }

    #[test]
    fn decimals_keep_their_digits() {
        assert_eq!(to_json(&Value::Decimal("12.3400".to_string())), "12.3400");
        assert_eq!(to_json(&Value::Decimal("NaN".to_string())), "\"NaN\"");
    }
}
