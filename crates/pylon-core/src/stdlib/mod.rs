use std::sync::OnceLock;

mod registry;

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
    /// Installs a function in the `_pylon` schema.
    PylonFunction(&'static str),
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
