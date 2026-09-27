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

//! PostgreSQL's binary `numeric` format, to and from a decimal string.
//!
//! `numeric` is arbitrary precision and so is `DecodedValue::Decimal`, which
//! carries its digits as text. Converting through a fixed-precision Rust
//! decimal type in between loses whatever does not fit: `rust_decimal`, which
//! this module replaces, holds 96 bits of mantissa and at most 28 decimal
//! places, and its `FromStr` abandons a value that overflows that mantissa
//! *before* it applies the exponent — so `4E-7` parsed from 65 significant
//! digits came out as `3.9999999999999998…`, ten million times too large,
//! with no error. Nothing here rounds, so there is nothing to lose track of.
//!
//! Wire layout (`numeric_send`/`numeric_recv` in Postgres's `numeric.c`):
//! `i16 ndigits`, `i16 weight`, `u16 sign`, `i16 dscale`, then `ndigits`
//! base-10000 digit groups as `u16`. `weight` is the power of 10000 the first
//! group carries, so the value is `digits[0] * 10000^weight + digits[1] *
//! 10000^(weight-1) + …`. `dscale` is the *display* scale — how many decimal
//! places to print — and is what makes `12.50` round-trip as `12.50` rather
//! than `12.5`.

use bytes::BufMut;

use crate::error::{Error, Result};

/// Base-10000, as `NBASE` in `numeric.c`. Four decimal digits per group.
const DIGITS_PER_GROUP: usize = 4;

const SIGN_POSITIVE: u16 = 0x0000;
const SIGN_NEGATIVE: u16 = 0x4000;
const SIGN_NAN: u16 = 0xC000;
const SIGN_POSITIVE_INFINITY: u16 = 0xD000;
const SIGN_NEGATIVE_INFINITY: u16 = 0xF000;

/// Postgres's own bounds on a `numeric` value (`numeric.c`): at most this many
/// digits before the point, and this many after. Checked here so an absurd
/// exponent is reported as a bad value instead of materialising gigabytes of
/// zeroes or silently wrapping the `i16` scale fields.
const MAX_INTEGER_DIGITS: usize = 131_072;
const MAX_FRACTION_DIGITS: usize = 16_383;

fn invalid(text: &str, reason: &str) -> Error {
    Error::message(format!("invalid decimal value {text:?}: {reason}"))
}

/// A decimal string split into a sign, a run of digits, and the power of ten
/// that run is scaled by — `value = sign * digits * 10^exponent`.
struct Parsed {
    negative: bool,
    digits: String,
    exponent: i64,
}

/// Parses the mantissa/exponent forms Python's `Decimal.__str__` and
/// Postgres's own `numeric_out` produce: an optional sign, digits with an
/// optional point, and an optional `e`/`E` exponent.
fn parse(text: &str) -> Result<Parsed> {
    let trimmed = text.trim();
    let (negative, unsigned) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };

    let (mantissa, exponent_text) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };

    let (integer, fraction) = match mantissa.find('.') {
        Some(at) => (&mantissa[..at], &mantissa[at + 1..]),
        None => (mantissa, ""),
    };

    if integer.is_empty() && fraction.is_empty() {
        return Err(invalid(text, "no digits"));
    }
    if !integer.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(invalid(text, "not a decimal number"));
    }

    let exponent: i64 = match exponent_text {
        Some(value) => value
            .trim()
            .parse()
            .map_err(|_| invalid(text, "exponent is not an integer"))?,
        None => 0,
    };

    Ok(Parsed {
        negative,
        digits: format!("{integer}{fraction}"),
        // The fraction's digits were read as if they sat left of the point,
        // so the exponent has to pay them back.
        exponent: exponent - fraction.len() as i64,
    })
}

/// Splits `parsed` into the digits before and after the decimal point,
/// materialising the exponent as zeroes. `numeric`'s only exponent is
/// `weight`, which counts whole groups of four, so a value has to be spelled
/// out to be grouped.
fn place_point(text: &str, parsed: &Parsed) -> Result<(String, String)> {
    let digits = parsed.digits.trim_start_matches('0');
    if digits.is_empty() {
        // Zero: keep the scale the caller asked for, drop the digits.
        let scale = usize::try_from(-parsed.exponent).unwrap_or(0).min(MAX_FRACTION_DIGITS);
        return Ok((String::new(), "0".repeat(scale)));
    }

    if parsed.exponent >= 0 {
        let zeroes = usize::try_from(parsed.exponent).map_err(|_| invalid(text, "exponent too large"))?;
        if digits.len() + zeroes > MAX_INTEGER_DIGITS {
            return Err(invalid(text, "too many digits before the decimal point"));
        }
        return Ok((format!("{digits}{}", "0".repeat(zeroes)), String::new()));
    }

    let places = usize::try_from(-parsed.exponent).map_err(|_| invalid(text, "exponent too small"))?;
    if places > MAX_FRACTION_DIGITS {
        return Err(invalid(text, "too many digits after the decimal point"));
    }
    if digits.len() > places {
        let at = digits.len() - places;
        if at > MAX_INTEGER_DIGITS {
            return Err(invalid(text, "too many digits before the decimal point"));
        }
        Ok((digits[..at].to_string(), digits[at..].to_string()))
    } else {
        // Smaller than one, so the point is left of every digit and the gap
        // between them is zeroes: 4E-7 is `.0000004`.
        Ok((String::new(), format!("{}{digits}", "0".repeat(places - digits.len()))))
    }
}

/// Reads four decimal digits, zero-padding a short final chunk.
fn group_of(digits: &[u8], from: usize) -> u16 {
    let mut value = 0u16;
    for offset in 0..DIGITS_PER_GROUP {
        let digit = digits.get(from + offset).map_or(0, |b| u16::from(b - b'0'));
        value = value * 10 + digit;
    }
    value
}

/// Encodes a decimal string as binary `numeric`, exactly — no rounding.
pub fn encode(text: &str, out: &mut bytes::BytesMut) -> Result<()> {
    if let Some(sign) = special_sign(text) {
        write(out, 0, sign, 0, 0, &[]);
        return Ok(());
    }

    let parsed = parse(text)?;
    let (integer, fraction) = place_point(text, &parsed)?;
    let dscale = i16::try_from(fraction.len()).map_err(|_| invalid(text, "scale out of range"))?;

    // Groups are aligned on the point: the integer side pads on the left so
    // its last group ends at the point, the fraction side pads on the right.
    let integer_padding = (DIGITS_PER_GROUP - integer.len() % DIGITS_PER_GROUP) % DIGITS_PER_GROUP;
    let padded_integer = format!("{}{integer}", "0".repeat(integer_padding));

    let mut groups: Vec<u16> = Vec::new();
    for at in (0..padded_integer.len()).step_by(DIGITS_PER_GROUP) {
        groups.push(group_of(padded_integer.as_bytes(), at));
    }
    let integer_groups = groups.len();
    for at in (0..fraction.len()).step_by(DIGITS_PER_GROUP) {
        groups.push(group_of(fraction.as_bytes(), at));
    }

    // `weight` counts from the group holding the point, so an empty integer
    // side starts at -1 and every leading zero group pushes it further down.
    let leading_zeroes = groups.iter().take_while(|group| **group == 0).count();
    let mut weight = integer_groups as i64 - 1 - leading_zeroes as i64;
    groups.drain(..leading_zeroes);
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }

    let sign = if parsed.negative && !groups.is_empty() {
        SIGN_NEGATIVE
    } else {
        SIGN_POSITIVE
    };
    let ndigits = i16::try_from(groups.len()).map_err(|_| invalid(text, "too many digits"))?;
    let weight = i16::try_from(weight).map_err(|_| invalid(text, "exponent out of range"))?;

    write(out, ndigits, sign, weight, dscale, &groups);
    Ok(())
}

/// `NaN` and the infinities carry no digits, only a sign field. Spellings
/// cover Python's `Decimal` (`NaN`, `Infinity`), Rust's float `Display`
/// (`NaN`, `inf`) and Postgres's own output.
fn special_sign(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    let (negative, bare) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if bare.eq_ignore_ascii_case("nan") || bare.eq_ignore_ascii_case("snan") {
        return Some(SIGN_NAN);
    }
    if bare.eq_ignore_ascii_case("inf") || bare.eq_ignore_ascii_case("infinity") {
        return Some(if negative {
            SIGN_NEGATIVE_INFINITY
        } else {
            SIGN_POSITIVE_INFINITY
        });
    }
    None
}

fn write(out: &mut bytes::BytesMut, ndigits: i16, sign: u16, weight: i16, dscale: i16, groups: &[u16]) {
    out.put_i16(ndigits);
    out.put_i16(weight);
    out.put_u16(sign);
    out.put_i16(dscale);
    for group in groups {
        out.put_u16(*group);
    }
}

/// Decodes binary `numeric` into a decimal string, keeping every digit the
/// server sent and the scale it asked for — `12.50` stays `12.50`.
pub fn decode(data: &[u8]) -> Result<String> {
    if data.len() < 8 {
        return Err(Error::message(format!(
            "malformed numeric: expected at least 8 bytes of header, got {}",
            data.len()
        )));
    }
    let ndigits = i16::from_be_bytes(data[0..2].try_into()?);
    let weight = i16::from_be_bytes(data[2..4].try_into()?);
    let sign = u16::from_be_bytes(data[4..6].try_into()?);
    let dscale = i16::from_be_bytes(data[6..8].try_into()?);

    match sign {
        SIGN_NAN => return Ok("NaN".to_string()),
        SIGN_POSITIVE_INFINITY => return Ok("Infinity".to_string()),
        SIGN_NEGATIVE_INFINITY => return Ok("-Infinity".to_string()),
        SIGN_POSITIVE | SIGN_NEGATIVE => {}
        other => return Err(Error::message(format!("malformed numeric: unknown sign 0x{other:04X}"))),
    }

    let ndigits = usize::try_from(ndigits).map_err(|_| Error::message("malformed numeric: negative ndigits"))?;
    let expected = 8 + ndigits * 2;
    if data.len() < expected {
        return Err(Error::message(format!(
            "malformed numeric: {ndigits} digit groups need {expected} bytes, got {}",
            data.len()
        )));
    }
    let groups: Vec<u16> = (0..ndigits)
        .map(|at| u16::from_be_bytes(data[8 + at * 2..10 + at * 2].try_into().unwrap()))
        .collect();

    // Every group from the most significant down to the one holding the point.
    // A group past what was sent reads as zero: 10000 is one group of `1` at
    // weight 1, with the group below the point left out.
    let mut integer = String::new();
    if weight >= 0 {
        for index in 0..=usize::try_from(weight).unwrap_or(0) {
            let group = groups.get(index).copied().unwrap_or(0);
            if integer.is_empty() {
                integer.push_str(&group.to_string());
            } else {
                integer.push_str(&format!("{group:04}"));
            }
        }
    }
    if integer.is_empty() {
        integer.push('0');
    }

    let scale = usize::try_from(dscale).map_err(|_| Error::message("malformed numeric: negative dscale"))?;
    let mut fraction = String::new();
    if scale > 0 {
        // The fraction starts in the group after the point, which may be past
        // the end of what was sent — trailing zero groups are not
        // transmitted, so a missing group reads as zero.
        let mut index = weight as i64 + 1;
        while fraction.len() < scale {
            let group = usize::try_from(index)
                .ok()
                .and_then(|at| groups.get(at).copied())
                .unwrap_or(0);
            fraction.push_str(&format!("{group:04}"));
            index += 1;
        }
        fraction.truncate(scale);
    }

    let magnitude = if scale > 0 {
        format!("{integer}.{fraction}")
    } else {
        integer
    };
    let zero = magnitude.bytes().all(|b| b == b'0' || b == b'.');
    Ok(if sign == SIGN_NEGATIVE && !zero {
        format!("-{magnitude}")
    } else {
        magnitude
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> String {
        let mut buffer = bytes::BytesMut::new();
        encode(text, &mut buffer).expect("encodes");
        decode(&buffer).expect("decodes")
    }

    #[test]
    fn keeps_a_value_that_overflows_a_96_bit_mantissa() {
        // What `Decimal(0.0000004)` stringifies to. `rust_decimal` parsed this
        // as 3.9999999999999998…, having dropped the `E-7` when the mantissa
        // overflowed, and reported no error.
        let text = "3.9999999999999998189924473035450347424557548947632312774658203125E-7";
        assert_eq!(
            round_trip(text),
            "0.00000039999999999999998189924473035450347424557548947632312774658203125"
        );
    }

    #[test]
    fn keeps_a_value_with_a_large_positive_exponent() {
        // `rust_decimal` rejected 29 to 31 outright and silently divided
        // anything above by its own exponent.
        assert_eq!(
            round_trip("1.2222222222222222222222222222222E+40"),
            "12222222222222222222222222222222000000000"
        );
        assert_eq!(round_trip("1E+29"), "100000000000000000000000000000");
    }

    #[test]
    fn honours_the_scale_it_was_given() {
        assert_eq!(round_trip("12.50"), "12.50");
        assert_eq!(round_trip("12.5"), "12.5");
        assert_eq!(round_trip("0.000"), "0.000");
    }

    #[test]
    fn round_trips_the_ordinary_cases() {
        for text in [
            "0",
            "1",
            "-1",
            "12345",
            "-9999.001",
            "0.5",
            "-0.5",
            "0.0000004",
            "1000000",
            "9999",
            "10000",
            "0.0001",
            "0.00009999",
            "123456789012345678901234567890.123456789012345678901234567890",
        ] {
            assert_eq!(round_trip(text), text, "{text}");
        }
    }

    #[test]
    fn normalises_the_forms_postgres_does_not_spell_that_way() {
        assert_eq!(round_trip("4E-7"), "0.0000004");
        assert_eq!(round_trip("+1.5"), "1.5");
        assert_eq!(round_trip(".5"), "0.5");
        assert_eq!(round_trip("5."), "5");
        assert_eq!(round_trip("-0"), "0");
        assert_eq!(round_trip("1e3"), "1000");
    }

    #[test]
    fn carries_nan_and_the_infinities() {
        assert_eq!(round_trip("NaN"), "NaN");
        assert_eq!(round_trip("Infinity"), "Infinity");
        assert_eq!(round_trip("-Infinity"), "-Infinity");
        assert_eq!(round_trip("inf"), "Infinity");
        assert_eq!(round_trip("-inf"), "-Infinity");
    }

    #[test]
    fn aligns_digit_groups_on_the_decimal_point() {
        // 1.2345 straddles a group boundary: [1, 2345] at weight 0, dscale 4.
        let mut buffer = bytes::BytesMut::new();
        encode("1.2345", &mut buffer).unwrap();
        assert_eq!(&buffer[..], &[0, 2, 0, 0, 0, 0, 0, 4, 0, 1, 0x09, 0x29][..]);
        assert_eq!(decode(&buffer).unwrap(), "1.2345");
    }

    #[test]
    fn drops_leading_and_trailing_zero_groups() {
        // 0.00000004: one group of `4` at weight -2, nothing transmitted for
        // the empty group between it and the point.
        let mut buffer = bytes::BytesMut::new();
        encode("0.00000004", &mut buffer).unwrap();
        assert_eq!(&buffer[..], &[0, 1, 0xFF, 0xFE, 0, 0, 0, 8, 0, 4][..]);
        assert_eq!(decode(&buffer).unwrap(), "0.00000004");
    }

    #[test]
    fn rejects_what_is_not_a_decimal() {
        for text in ["", "abc", "1.2.3", "1e", "1eX", "--1", "1 2"] {
            let mut buffer = bytes::BytesMut::new();
            assert!(encode(text, &mut buffer).is_err(), "{text:?} should not encode");
        }
    }

    #[test]
    fn rejects_an_exponent_postgres_could_not_hold() {
        let mut buffer = bytes::BytesMut::new();
        assert!(encode("1E+200000", &mut buffer).is_err());
        assert!(encode("1E-20000", &mut buffer).is_err());
    }

    #[test]
    fn rejects_a_truncated_header() {
        assert!(decode(&[0, 1, 0, 0, 0, 0, 0]).is_err());
        // Claims one digit group but sends none of it.
        assert!(decode(&[0, 1, 0, 0, 0, 0, 0, 0]).is_err());
    }
}
