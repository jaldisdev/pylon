use crate::error::PyQLError;
use crate::schema::SchemaDescriptor;
use crate::{ir, parse, sql};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cardinality {
    Required,
    Optional,
    Many,
}

/// Internal tree describing one position in the query output shape.
/// Opaque to Python — only the Rust deserializer inspects it.
#[derive(Debug, Clone)]
pub enum ShapeNode {
    /// Leaf value; native PG type.
    Scalar { name: String, position: usize },
    /// Object shape.
    /// `type_name = Some(s)` → named schema type decoded to a registered dataclass.
    /// `type_name = None`    → free type decoded to Pylon's generic Object dataclass.
    /// `name` is the field name within the parent (empty string for the root).
    Object {
        name: String,
        type_name: Option<String>,
        position: usize,
        cardinality: Cardinality,
        fields: Vec<ShapeNode>,
    },
    /// `record[]` column decoded to a Python list.
    Array {
        name: String,
        position: usize,
        element: Box<ShapeNode>,
    },
    /// Anonymous positional tuple decoded to a Python tuple. No type name — no registry lookup.
    Tuple {
        position: usize,
        elements: Vec<ShapeNode>,
    },
}

/// Opaque handle to the output shape of a compiled query.
#[derive(Debug, Clone)]
pub struct ShapeDescriptor {
    pub(crate) root: ShapeNode,
}

/// Typed query parameter value produced during compilation.
#[derive(Debug, Clone)]
pub enum QueryParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
}

/// The output of a successful PyQL compilation.
/// Immutable and safe to cache and reuse across requests.
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    /// PostgreSQL SQL string ready for execution.
    pub sql: String,
    /// Positional bound parameters ($1, $2, …).
    pub params: Vec<QueryParam>,
    /// Opaque shape handle — consumed by the Rust deserializer.
    pub shape: ShapeDescriptor,
}

/// Compile a PyQL query string to SQL against `schema`.
///
/// Synchronous — compilation is CPU-bound; async lives at the DB execution layer.
/// Raises `PyQLError` on any grammar, type, or resolution failure.
pub fn compile(query: &str, schema: &SchemaDescriptor) -> Result<CompiledQuery, PyQLError> {
    let ast = parse::parse(query)?;
    let ir_out = ir::compile(&ast, schema)?;
    let sql_out = sql::emit(&ir_out);
    Ok(CompiledQuery {
        sql: sql_out.sql,
        params: Vec::new(), // parameter binding from ir_out.params resolved at execution time
        shape: sql_out.shape,
    })
}
