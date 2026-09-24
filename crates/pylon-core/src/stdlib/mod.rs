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

use std::sync::OnceLock;

pub mod ddl;
mod registry;
pub use ddl::export_stdlib;

// ── Type system ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PylonType {
    // Scalar primitives
    Str,
    Bool,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal,
    BigInt,
    Uuid,
    Json,
    Bytes,
    // Temporal
    Datetime,
    Duration,
    // cal:: types
    LocalDatetime,
    LocalDate,
    LocalTime,
    RelativeDuration,
    // pgvector:: types
    Vector,
    // postgis:: types
    Geometry,
    Geography,
    Box2D,
    Box3D,
    // Polymorphic
    Any,
    AnyOrderable,
    AnyPoint,
    // Composite
    Array(Box<PylonType>),
    Set(Box<PylonType>),
    Optional(Box<PylonType>),
    Range(Box<PylonType>),
    Multirange(Box<PylonType>),
    Tuple(Vec<PylonType>),
}

impl PylonType {
    /// The PostgreSQL type a plain scalar travels as, as the compiler's type
    /// inference spells it; `None` for anything polymorphic or composite.
    pub fn scalar_pg_type(&self) -> Option<&'static str> {
        use PylonType::*;
        Some(match self {
            Str => "text",
            Bool => "boolean",
            Int16 => "int2",
            Int32 => "int4",
            Int64 => "int8",
            Float32 => "float4",
            Float64 => "float8",
            Decimal | BigInt => "numeric",
            Uuid => "uuid",
            Json => "jsonb",
            Bytes => "bytea",
            Datetime => "timestamptz",
            Duration | RelativeDuration => "interval",
            LocalDatetime => "timestamp",
            LocalDate => "date",
            LocalTime => "time",
            _ => return None,
        })
    }

    /// PyQL-facing spelling of the type, as a user would write it in a query
    /// (`str`, `array<int64>`, `range<datetime>`). Distinct from
    /// `ddl::pg_type`, which renders the PostgreSQL side.
    pub fn pyql_name(&self) -> String {
        use PylonType::*;
        match self {
            Str => "str".into(),
            Bool => "bool".into(),
            Int16 => "int16".into(),
            Int32 => "int32".into(),
            Int64 => "int64".into(),
            Float32 => "float32".into(),
            Float64 => "float64".into(),
            Decimal => "decimal".into(),
            BigInt => "bigint".into(),
            Uuid => "uuid".into(),
            Json => "json".into(),
            Bytes => "bytes".into(),
            Datetime => "datetime".into(),
            Duration => "duration".into(),
            LocalDatetime => "cal::local_datetime".into(),
            LocalDate => "cal::local_date".into(),
            LocalTime => "cal::local_time".into(),
            RelativeDuration => "cal::relative_duration".into(),
            Vector => "pgvector::vector".into(),
            Geometry => "postgis::geometry".into(),
            Geography => "postgis::geography".into(),
            Box2D => "postgis::box2d".into(),
            Box3D => "postgis::box3d".into(),
            Any => "any".into(),
            AnyOrderable => "anyorderable".into(),
            AnyPoint => "anypoint".into(),
            Array(inner) => format!("array<{}>", inner.pyql_name()),
            Set(inner) => format!("set<{}>", inner.pyql_name()),
            Optional(inner) => format!("optional<{}>", inner.pyql_name()),
            Range(inner) => format!("range<{}>", inner.pyql_name()),
            Multirange(inner) => format!("multirange<{}>", inner.pyql_name()),
            Tuple(ts) => format!(
                "tuple<{}>",
                ts.iter().map(|t| t.pyql_name()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// True for a set-typed position — the marker that distinguishes an
    /// aggregate parameter (`std::count(set<any>)`) or a set-returning
    /// result (`std::array_unpack`) from an ordinary scalar one.
    pub fn is_set(&self) -> bool {
        matches!(self, PylonType::Set(_))
    }
}

// ── PylonFunction definition ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlLanguage {
    Sql,
    PlPgSql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnVolatility {
    /// Same arguments always produce the same result — safe anywhere.
    Immutable,
    /// Result is fixed within a single statement, but may vary between
    /// statements (session settings, current transaction time).
    Stable,
    /// Result may differ on every call (`random()`, `uuidv7()`,
    /// `clock_timestamp()`). Wanted in a pointer default, almost always a
    /// bug inside a filter predicate.
    Volatile,
    /// Volatile *and* side-effecting — advancing or resetting a sequence.
    /// Never admissible in an expression the caller expects to be a pure
    /// predicate, since it would fire once per row.
    Modifying,
}

/// The SQL definition for one `_pylon` schema function overload.
///
/// Each `FnDescriptor` with `ImplStrategy::PylonFunction(def)` installs a
/// separate overload in PostgreSQL — PG resolves them by argument types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PylonFnDef {
    /// Unqualified name in the `_pylon` schema, e.g. `"to_bool"`.
    pub name: &'static str,
    pub language: SqlLanguage,
    pub volatility: FnVolatility,
    /// When false the function is called even when arguments are NULL.
    /// Required for optional `msg` parameters that legitimately accept NULL.
    pub strict: bool,
    /// Override for the PostgreSQL RETURNS clause. When `None`, derived from
    /// the owning `FnDescriptor`'s `return_type`.
    pub returns_override: Option<&'static str>,
    /// SQL body — the content between `$$` delimiters.
    pub body: &'static str,
}

// ── Implementation strategy ──────────────────────────────────────────────────

/// How the transpiler should emit a stdlib function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImplStrategy {
    /// Delegates to a named PostgreSQL built-in. No `_pylon` function installed.
    SqlBuiltin(&'static str),
    /// Inline SQL template; `$1`, `$2`, … are positional placeholders.
    SqlExpression(&'static str),
    /// Maps to a SQL infix operator; transpiler emits `$1 op $2`.
    SqlOperator(&'static str),
    /// Installs a function in the `_pylon` schema via `export_stdlib()`.
    PylonFunction(PylonFnDef),
    /// Special transpiler rewriting — no `_pylon` function is installed.
    /// The transpiler substitutes type-specific PG expressions at compile time
    /// (e.g. `range` → `int8range(...)`, `multirange` → `int8multirange(...)`).
    TranspilerIntrinsic(&'static str),
}

// ── Parameter ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Param {
    pub name: &'static str,
    pub ty: PylonType,
    /// True for `name: type...` variadic parameters.
    pub variadic: bool,
    /// `named only name: type = default` — passed only as `name := value`,
    /// and this value when left out.
    pub named_only: Option<NamedDefault>,
    /// The name a call passes it by when that is not `name` -- which is also
    /// the SQL function's parameter name, and so cannot change once
    /// installed (`CREATE OR REPLACE` refuses a renamed parameter).
    pub keyword: Option<&'static str>,
}

impl Param {
    /// The name a call passes this parameter by.
    pub fn keyword(&self) -> &'static str {
        self.keyword.unwrap_or(self.name)
    }
}

/// What a named-only parameter stands at when a call leaves it out.
#[derive(Debug, Clone, Copy)]
pub enum NamedDefault {
    Int(i64),
    /// `<str>{}` — no value.
    Empty,
}

// ── Function descriptor ──────────────────────────────────────────────────────

/// One overload of a stdlib function.
#[derive(Debug, Clone)]
pub struct FnDescriptor {
    /// PyQL namespace: `"std"`, `"math"`, or `"cal"`.
    pub namespace: &'static str,
    /// Unqualified function name, e.g. `"count"`, `"str_lower"`.
    pub name: &'static str,
    /// Ordered parameter list for this overload.
    pub params: Vec<Param>,
    /// Return type for this overload.
    pub return_type: PylonType,
    /// How the transpiler should emit this call.
    pub impl_strategy: ImplStrategy,
    /// True when this overload backs a `Function`-strategy type cast entry.
    pub cast_target: bool,
    /// Call-result stability, independent of `impl_strategy` — a
    /// `SqlBuiltin` like `uuidv7()` is volatile even though it installs no
    /// `PylonFnDef` of its own. Consumers gate on this: a pointer default
    /// *wants* volatile, a filter predicate almost never does.
    pub volatility: FnVolatility,
}

// ── Static registry ──────────────────────────────────────────────────────────

static STDLIB: OnceLock<Vec<FnDescriptor>> = OnceLock::new();

/// Return the full stdlib registry, initializing it on first call.
pub fn registry() -> &'static [FnDescriptor] {
    STDLIB.get_or_init(registry::build)
}

impl FnVolatility {
    /// Lowercase wire name, as consumed by the Python `std` namespace gate.
    pub fn as_str(&self) -> &'static str {
        match self {
            FnVolatility::Immutable => "immutable",
            FnVolatility::Stable => "stable",
            FnVolatility::Volatile => "volatile",
            FnVolatility::Modifying => "modifying",
        }
    }
}

impl FnDescriptor {
    /// True when any parameter is set-typed — i.e. this overload is an
    /// aggregate and only makes sense over a multilink path or a subquery,
    /// never over a single scalar pointer.
    pub fn is_aggregate(&self) -> bool {
        self.params.iter().any(|p| p.ty.is_set())
    }

    /// True when the call yields a set rather than a single value
    /// (`std::array_unpack`) — meaningless inside a filter predicate.
    pub fn returns_set(&self) -> bool {
        self.return_type.is_set()
    }

    /// True for `TranspilerIntrinsic` overloads, which need compile-time type
    /// context and so can't be validated by arity alone.
    pub fn is_intrinsic(&self) -> bool {
        matches!(self.impl_strategy, ImplStrategy::TranspilerIntrinsic(_))
    }

    /// True when the overload accepts a trailing variadic parameter, so any
    /// argument count at or above `params.len() - 1` is legal.
    pub fn is_variadic(&self) -> bool {
        self.params.last().is_some_and(|p| p.variadic)
    }
}

/// Return all overloads for the given namespace + name pair.
pub fn lookup(namespace: &str, name: &str) -> Vec<&'static FnDescriptor> {
    registry()
        .iter()
        .filter(|f| f.namespace == namespace && f.name == name)
        .collect()
}

/// Enums the stdlib itself defines, as opposed to a schema's own. Kept here
/// rather than in a `SchemaDescriptor` because they have no PostgreSQL enum
/// type behind them — no migration creates one — so a member compiles to a
/// plain `text` literal that the function consuming it switches on.
///
/// Mirrors the upstream engine's `std::Endian`, member order included (verified against a live
/// instance: `enum_values` is `["Little", "Big"]`).
static STDLIB_ENUMS: OnceLock<Vec<crate::schema::EnumDescriptor>> = OnceLock::new();

pub fn stdlib_enums() -> &'static [crate::schema::EnumDescriptor] {
    STDLIB_ENUMS.get_or_init(|| {
        vec![crate::schema::EnumDescriptor {
            name: "Endian".to_string(),
            module: "std".to_string(),
            members: vec!["Little".to_string(), "Big".to_string()],
        }]
    })
}

/// Resolve a stdlib enum by bare (`Endian`) or qualified (`std::Endian`) name.
pub fn lookup_enum(name: &str) -> Option<&'static crate::schema::EnumDescriptor> {
    stdlib_enums()
        .iter()
        .find(|e| e.name == name || format!("{}::{}", e.module, e.name) == name)
}

/// Iterate over every overload that backs a `Function`-strategy cast.
pub fn cast_targets() -> impl Iterator<Item = &'static FnDescriptor> {
    registry().iter().filter(|f| f.cast_target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Volatility is marked per *overload*, so a multi-overload name is easy
    /// to half-annotate — `std::sequence_reset` has two, and marking only one
    /// left the other claiming to be immutable. Callers gate on volatility,
    /// so a single missed overload is a real hole rather than cosmetic.
    /// Nothing in the stdlib legitimately varies volatility across overloads
    /// of one name, so requiring agreement costs nothing and closes the gap.
    #[test]
    fn volatility_is_consistent_across_overloads() {
        let mut seen: HashMap<(&str, &str), FnVolatility> = HashMap::new();
        for d in registry() {
            let key = (d.namespace, d.name);
            match seen.get(&key) {
                Some(existing) => assert_eq!(
                    *existing, d.volatility,
                    "{}::{} declares more than one volatility across its overloads ({:?} vs {:?}) \
                     — every overload of a name must agree",
                    d.namespace, d.name, existing, d.volatility,
                ),
                None => {
                    seen.insert(key, d.volatility);
                }
            }
        }
    }

    /// A `PylonFunction` carries its own volatility for DDL emission; the
    /// descriptor carries one for the call gate. They describe the same
    /// function and must not drift apart.
    #[test]
    fn descriptor_volatility_matches_pylon_fn_def() {
        for d in registry() {
            if let ImplStrategy::PylonFunction(def) = &d.impl_strategy {
                assert_eq!(
                    def.volatility, d.volatility,
                    "{}::{} declares {:?} on its PylonFnDef but {:?} on its descriptor",
                    d.namespace, d.name, def.volatility, d.volatility,
                );
            }
        }
    }
}
