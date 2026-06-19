#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardinalityMode {
    Required,
    Optional,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Scalar,
    Link,
    MultiLink,
    Computed,
}

#[derive(Debug, Clone)]
pub struct FieldDescriptor {
    pub name: String,
    pub kind: FieldKind,
    pub cardinality: CardinalityMode,
    /// Type name for Link / MultiLink; None for Scalar and Computed.
    pub target: Option<String>,
    /// PG type name for Scalar fields; None for links.
    pub scalar_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TypeDescriptor {
    pub name: String,
    pub fields: Vec<FieldDescriptor>,
    /// True for @pylon.type(abstract=True) — no DDL unless also materialized.
    pub abstract_: bool,
    /// True for @pylon.type(materialized=True) — a CREATE VIEW is emitted for abstract types.
    pub materialized: bool,
}

#[derive(Debug, Clone)]
pub struct ScalarDescriptor {
    /// Qualified scalar name, e.g. `default::EmailStr`.
    pub name: String,
    /// Pylon base scalar, e.g. `Str`, `Int64`.
    pub base: String,
    /// PostgreSQL base type, e.g. `text`, `int8`.
    pub pg_type: String,
    /// SQL CHECK expressions pre-compiled from Python constraints.
    pub constraints: Vec<String>,
    pub module: String,
}

#[derive(Debug, Clone)]
pub struct SchemaDescriptor {
    pub types: Vec<TypeDescriptor>,
    /// User-defined custom scalars only; built-in scalars are known to pylon-core natively.
    pub scalars: Vec<ScalarDescriptor>,
}
