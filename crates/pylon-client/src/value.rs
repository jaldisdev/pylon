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

//! The crate's generic result value — Rust has no equivalent of the
//! per-type dataclasses `pylon-py` hydrates results into, so a query result
//! decodes into this instead: a generic, dynamically-typed value rather
//! than a per-type generated struct.

use std::ops::Index;

/// A decoded query result value. One variant per `ShapeNode`/`DecodedValue`
/// kind `decode.rs` knows how to produce — see that module for the walk
/// that builds these.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Uuid(uuid::Uuid),
    /// Arbitrary-precision decimal, in its canonical string form — matches
    /// `pylon_value::DecodedValue::Decimal`'s own representation, since
    /// there's no single obviously-correct native Rust decimal type to
    /// commit this generic client to.
    Decimal(String),
    /// A PostgreSQL `interval` (`std::duration` / `cal::relative_duration`)
    /// — kept as its three raw wire components, same reasoning as
    /// `DecodedValue::Interval`.
    Duration { months: i32, days: i32, microseconds: i64 },
    /// Whole days since the PG epoch (2000-01-01). Backs `cal::local_date`.
    Date(i32),
    /// Microseconds since midnight. Backs `cal::local_time`.
    Time(i64),
    /// Microseconds since the PG epoch, no timezone. Backs `cal::local_datetime`.
    Timestamp(i64),
    /// Microseconds since the PG epoch, UTC. Backs `std::datetime`.
    Timestamptz(i64),
    Range(Box<Range>),
    /// A genuine Postgres array.
    Array(Vec<Value>),
    /// An anonymous positional tuple (`tuple<...>` with no member names).
    Tuple(Vec<Value>),
    /// A schema object or a free/named-tuple object — see `Object`.
    Object(Object),
    /// An enum value, hydrated to its qualified type name + variant label
    /// rather than a generated Rust enum (there's no per-schema-type codegen
    /// on this client, matching the generic-`Object` design as a whole).
    Enum { type_name: String, value: String },
    /// Result of a `group` statement.
    Group(Box<Group>),
    /// Result of a `vector::search` statement.
    VectorSearch { object: Box<Value>, distance: f64 },
    /// Result of an `fts::search` statement.
    FtsSearch { object: Box<Value>, score: f64 },
}

/// A schema object, a free object literal, or a named tuple — all three
/// decode to the same by-name field-access shape. `type_name` is `Some`
/// only for a real schema type (including the concrete type of a
/// polymorphic interface query result) or a registered named tuple;
/// `None` for a free object (`select { a := 1 }`) or an unregistered
/// structural named tuple.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Object {
    pub(crate) type_name: Option<String>,
    /// Field order matches the order the query's shape declared them in.
    pub(crate) fields: Vec<(String, Value)>,
}

impl Object {
    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn fields(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.fields.iter().map(|(n, v)| (n.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Panics if `name` isn't a field on this object.
impl Index<&str> for Object {
    type Output = Value;

    fn index(&self, name: &str) -> &Value {
        self.get(name).unwrap_or_else(|| panic!("Object has no field {name:?}"))
    }
}

/// A PostgreSQL range value. `lower`/`upper` are `None` for an unbounded
/// side; `empty == true` means the whole range is empty (`lower`/`upper`
/// are meaningless then, not "both unbounded").
#[derive(Debug, Clone, PartialEq)]
pub struct Range {
    pub lower: Option<Value>,
    pub upper: Option<Value>,
    pub inc_lower: bool,
    pub inc_upper: bool,
    pub empty: bool,
}

/// Result of a `group` statement: one grouping key, the grouping label
/// list, and the elements sharing that key.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub key: Object,
    pub grouping: Vec<String>,
    pub elements: Vec<Value>,
}
