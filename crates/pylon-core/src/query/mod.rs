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
    /// The `result` column IS the value — not wrapped in ROW(). Used for array literals
    /// where asyncpg can't decode array OIDs inside anonymous composites.
    RawScalar,
    /// Like RawScalar but the value is a decoded JSON object (from a <json> cast).
    /// REPL displays it as `Json("...")`.
    JsonScalar,
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
    /// Named tuple decoded from jsonb. When `type_name` is Some, hydrated to the registered class.
    NamedTuple {
        name: String,
        position: usize,
        type_name: Option<String>,
    },
    /// Enum value arrived as text; hydrated to the Python enum class keyed by `enum_type`.
    Enum {
        name: String,
        position: usize,
        /// Pylon-qualified name, e.g. `default::Gender`.
        enum_type: String,
    },
    /// Result of a `vector::search` statement.
    /// The outer `result` tuple has three slots:
    ///   0 → NULL (virtual type, no registry lookup)
    ///   `object_position` → the object sub-tuple (decoded as a Pylon object)
    ///   `distance_position` → the distance scalar (float64)
    VectorSearch {
        object_position: usize,
        distance_position: usize,
        object_node: Box<ShapeNode>,
    },
    /// Result of a `fts::search` statement.
    /// Outer tuple layout mirrors `VectorSearch`: pos 0 = NULL, pos 1 = object, pos 2 = rank.
    FtsSearch {
        object_position: usize,
        rank_position: usize,
        object_node: Box<ShapeNode>,
    },
    /// Result of a `group` statement: each row is a free object with key/grouping/elements.
    Group {
        /// One ShapeNode per grouping key (carries name, position, and type).
        /// Positions are 1-based in the outer tuple (pos 0 is the NULL type slot).
        key_nodes: Vec<ShapeNode>,
        /// Position of the `ARRAY[key_names...]::text[]` in the outer tuple.
        grouping_position: usize,
        /// Position of the `array_agg(elements)` in the outer tuple.
        elements_position: usize,
        /// Shape node for each element in the elements array.
        element: Box<ShapeNode>,
    },
}

/// Opaque handle to the output shape of a compiled query.
#[derive(Debug, Clone)]
pub struct ShapeDescriptor {
    pub root: ShapeNode,
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
    /// Ordered parameter names matching $1, $2, … in the SQL.
    /// The client uses this to map kwargs to positional arguments.
    pub param_names: Vec<String>,
    /// Typed bound parameters — populated at execution time, empty after compilation.
    pub params: Vec<QueryParam>,
    /// Opaque shape handle — consumed by the Rust deserializer.
    pub shape: ShapeDescriptor,
    /// Non-fatal warnings produced during compilation.
    pub warnings: Vec<String>,
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
        param_names: ir_out.params,
        params: Vec::new(),
        shape: sql_out.shape,
        warnings: ir_out.warnings,
    })
}
