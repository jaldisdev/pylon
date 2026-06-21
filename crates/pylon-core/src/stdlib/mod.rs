use std::sync::OnceLock;

mod registry;
pub mod ddl;
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

// ── PylonFunction definition ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlLanguage {
    Sql,
    PlPgSql,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FnVolatility {
    Immutable,
    Stable,
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
}

// ── Static registry ──────────────────────────────────────────────────────────

static STDLIB: OnceLock<Vec<FnDescriptor>> = OnceLock::new();

/// Return the full stdlib registry, initializing it on first call.
pub fn registry() -> &'static [FnDescriptor] {
    STDLIB.get_or_init(registry::build)
}

/// Return all overloads for the given namespace + name pair.
pub fn lookup(namespace: &str, name: &str) -> Vec<&'static FnDescriptor> {
    registry()
        .iter()
        .filter(|f| f.namespace == namespace && f.name == name)
        .collect()
}

/// Iterate over every overload that backs a `Function`-strategy cast.
pub fn cast_targets() -> impl Iterator<Item = &'static FnDescriptor> {
    registry().iter().filter(|f| f.cast_target)
}
