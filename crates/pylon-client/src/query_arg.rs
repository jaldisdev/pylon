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

//! Query arguments: [`QueryArg`] for a single value, [`QueryArgs`] for the
//! collection a query method takes, and [`ValueOpt`] + [`named_args!`] for
//! building a named collection inline.
//!
//! Mirrors `the upstream Rust client's query_arg` closely enough that a call site moving
//! off the upstream engine's Rust client keeps its argument expressions: `&()` for no
//! arguments, `&(a, b)` for positional `$0`/`$1`, and
//! `&named_args! { "name" => value }` for `$name`.
//!
//! Positional arguments work because PyQL compiles `$0` to the parameter
//! *name* `"0"` — so a tuple is just a named collection whose names are its
//! indices.

use std::collections::HashMap;

use pylon_value::DecodedValue;

/// A single query argument. Implemented for the scalars Pylon can bind, for
/// `Option`/the common `Vec` element types, and for [`DecodedValue`] itself
/// as the escape hatch for anything not covered here.
pub trait QueryArg {
    fn to_decoded(&self) -> DecodedValue;
}

impl<T: QueryArg + ?Sized> QueryArg for &T {
    fn to_decoded(&self) -> DecodedValue {
        (**self).to_decoded()
    }
}

impl QueryArg for DecodedValue {
    fn to_decoded(&self) -> DecodedValue {
        self.clone()
    }
}

/// An absent optional argument (`<optional str>$x`) binds as NULL.
impl<T: QueryArg> QueryArg for Option<T> {
    fn to_decoded(&self) -> DecodedValue {
        match self {
            Some(value) => value.to_decoded(),
            None => DecodedValue::Null,
        }
    }
}

macro_rules! query_arg_via_into {
    ($($target:ty),* $(,)?) => {
        $(
            impl QueryArg for $target {
                fn to_decoded(&self) -> DecodedValue {
                    DecodedValue::from(self.clone())
                }
            }
        )*
    };
}

query_arg_via_into!(bool, i16, i32, i64, f32, f64, String, uuid::Uuid);

impl QueryArg for str {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Str(self.to_string())
    }
}

/// `Vec<u8>` is bytes, not an array of integers — matching
/// `DecodedValue::from(Vec<u8>)` and the upstream engine's own precedence. That is also why
/// the array impls below are enumerated per element type instead of a
/// blanket `impl<T: QueryArg> QueryArg for Vec<T>`: the blanket form would
/// overlap this one, and Rust won't accept the pair on the grounds that
/// `u8` merely happens not to implement `QueryArg` today.
impl QueryArg for Vec<u8> {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Bytes(self.clone())
    }
}

macro_rules! query_arg_array {
    ($($element:ty),* $(,)?) => {
        $(
            impl QueryArg for Vec<$element> {
                fn to_decoded(&self) -> DecodedValue {
                    DecodedValue::Array(self.iter().map(QueryArg::to_decoded).collect())
                }
            }

            impl QueryArg for [$element] {
                fn to_decoded(&self) -> DecodedValue {
                    DecodedValue::Array(self.iter().map(QueryArg::to_decoded).collect())
                }
            }
        )*
    };
}

query_arg_array!(bool, i16, i32, i64, f32, f64, String, uuid::Uuid);

/// `std::datetime`. Pylon stores a timestamp as microseconds from the
/// PostgreSQL epoch (2000-01-01), not the Unix epoch.
impl QueryArg for chrono::DateTime<chrono::Utc> {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Timestamptz(pg_micros(self.naive_utc()))
    }
}

/// `cal::local_datetime`.
impl QueryArg for chrono::NaiveDateTime {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Timestamp(pg_micros(*self))
    }
}

/// `cal::local_date`.
impl QueryArg for chrono::NaiveDate {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Date(self.signed_duration_since(pg_epoch_date()).num_days() as i32)
    }
}

/// `cal::local_time`.
impl QueryArg for chrono::NaiveTime {
    fn to_decoded(&self) -> DecodedValue {
        let midnight = chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("00:00:00 is a valid time");
        DecodedValue::Time(
            self.signed_duration_since(midnight)
                .num_microseconds()
                .unwrap_or_default(),
        )
    }
}

/// A `json` argument. Bound as JSON text rather than as a
/// `DecodedValue::Object`, because that is the form `pylon-pgcon` accepts
/// for a `jsonb` parameter regardless of whether the document's root is an
/// object, and it matches what a the upstream engine call site was already doing by hand
/// (`Json::new_unchecked(serde_json::to_string(&value)?)`).
impl QueryArg for serde_json::Value {
    fn to_decoded(&self) -> DecodedValue {
        DecodedValue::Str(self.to_string())
    }
}

fn pg_epoch_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("2000-01-01 is a valid date")
}

/// Microseconds from the PostgreSQL epoch. Saturates rather than wrapping
/// for a datetime far enough out to overflow — a value that extreme is a
/// caller bug, and a silently wrapped timestamp would be worse than a
/// clamped one.
fn pg_micros(value: chrono::NaiveDateTime) -> i64 {
    let epoch = pg_epoch_date().and_hms_opt(0, 0, 0).expect("00:00:00 is a valid time");
    value
        .signed_duration_since(epoch)
        .num_microseconds()
        .unwrap_or(i64::MAX)
}

/// An argument value inside [`named_args!`], constructible from anything
/// that is a [`QueryArg`]. The Pylon counterpart of
/// `the upstream Rust client's optional value`, and the reason `named_args!` can mix
/// argument types in one collection.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueOpt(DecodedValue);

impl<T: QueryArg> From<T> for ValueOpt {
    fn from(value: T) -> Self {
        ValueOpt(value.to_decoded())
    }
}

impl ValueOpt {
    pub fn into_decoded(self) -> DecodedValue {
        self.0
    }
}

impl From<ValueOpt> for DecodedValue {
    fn from(value: ValueOpt) -> Self {
        value.0
    }
}

/// The collection of arguments a query method takes. `&()` when there are
/// none, a tuple for positional `$0`/`$1`/…, a `HashMap` or a slice of
/// `(name, value)` pairs for named ones.
pub trait QueryArgs {
    /// Borrows the names out of `self` (which the query method holds by
    /// reference for the whole call) so only the values are cloned.
    fn to_params(&self) -> Vec<(&str, DecodedValue)>;
}

impl QueryArgs for () {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        Vec::new()
    }
}

impl QueryArgs for [(&str, DecodedValue)] {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        self.iter().map(|(name, value)| (*name, value.clone())).collect()
    }
}

impl<const N: usize> QueryArgs for [(&str, DecodedValue); N] {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        self.as_slice().to_params()
    }
}

impl QueryArgs for Vec<(&str, DecodedValue)> {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        self.as_slice().to_params()
    }
}

/// Keyed by [`ValueOpt`] rather than by any `V: QueryArg`, because
/// `ValueOpt` deliberately does *not* implement `QueryArg`: it is built from
/// one via a blanket `From`, and making it a `QueryArg` too would collide
/// with the standard library's reflexive `impl<T> From<T> for T`. the upstream engine's
/// client draws the same line for the same reason.
impl QueryArgs for HashMap<&str, ValueOpt> {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        self.iter().map(|(name, value)| (*name, value.0.clone())).collect()
    }
}

impl QueryArgs for HashMap<String, ValueOpt> {
    fn to_params(&self) -> Vec<(&str, DecodedValue)> {
        self.iter()
            .map(|(name, value)| (name.as_str(), value.0.clone()))
            .collect()
    }
}

/// `$0`, `$1`, … compile to these parameter names, so a positional tuple
/// needs no allocation to name its own elements.
const POSITIONAL_NAMES: [&str; 12] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"];

macro_rules! impl_query_args_for_tuple {
    ($($index:tt : $param:ident),+) => {
        impl<$($param: QueryArg),+> QueryArgs for ($($param,)+) {
            fn to_params(&self) -> Vec<(&str, DecodedValue)> {
                vec![$((POSITIONAL_NAMES[$index], self.$index.to_decoded())),+]
            }
        }
    };
}

impl_query_args_for_tuple!(0: A);
impl_query_args_for_tuple!(0: A, 1: B);
impl_query_args_for_tuple!(0: A, 1: B, 2: C);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J, 10: K);
impl_query_args_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H, 8: I, 9: J, 10: K, 11: L);

/// Builds a named argument collection for a query with `$name` parameters:
///
/// ```no_run
/// # use pylon_client::named_args;
/// # fn go(client: &pylon_client::Client, id: uuid::Uuid) {
/// let args = named_args! { "id" => id, "label" => "urgent".to_string() };
/// # let _ = (client, args);
/// # }
/// ```
///
/// Mirrors `the upstream Rust client's named_args!` — same syntax, same
/// `HashMap<&str, ValueOpt>` result, with a trailing comma allowed.
#[macro_export]
macro_rules! named_args {
    ($($key:expr => $value:expr,)+) => { $crate::named_args!($($key => $value),+) };
    ($($key:expr => $value:expr),*) => {{
        let mut args = ::std::collections::HashMap::<&str, $crate::ValueOpt>::new();
        $(
            args.insert($key, $crate::ValueOpt::from($value));
        )*
        args
    }};
}
