use std::sync::{OnceLock, RwLock};
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::error::PyQLError;
use crate::schema::SchemaDescriptor;
use crate::{ir, parse, sql};

const CACHE_CAPACITY: usize = 1024;

static QUERY_CACHE: OnceLock<RwLock<LruCache<String, CompiledQuery>>> = OnceLock::new();

fn query_cache() -> &'static RwLock<LruCache<String, CompiledQuery>> {
    QUERY_CACHE.get_or_init(|| {
        RwLock::new(LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap()))
    })
}

/// Discard all cached compiled queries. Call when the schema is reloaded.
pub fn clear_query_cache() {
    if let Some(cache) = QUERY_CACHE.get() {
        cache.write().unwrap().clear();
    }
}

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
    /// Outer tuple layout mirrors `VectorSearch`: pos 0 = NULL, pos 1 = object, pos 2 = score.
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
/// Execution plan for a deferred (remote) `fts::search` query.
/// The Python client uses this to drive the two-phase execution:
/// 1. Call the remote search backend with `query_param_name`'s value → get (id, score) pairs
/// 2. Inject them as `__deferred_ids__` / `__deferred_scores__` params and run `sql` against Postgres
#[derive(Debug, Clone)]
pub struct DeferredSearchPlan {
    /// OpenSearch index to query.
    pub index_name: String,
    /// The user's query-text kwarg name (e.g. `"query"` for `fts::search(T, $query)`).
    /// Empty string when the query text is an inline literal.
    pub query_param_name: String,
    /// Inline literal query text — set when the query is `fts::search(T, 'literal text')`.
    pub query_literal: Option<String>,
    /// Requested result size (limit), if known at compile time.
    pub size: Option<usize>,
}

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
    /// Set when the query uses a deferred (remote) backend; drives two-phase execution.
    pub deferred_search_plan: Option<DeferredSearchPlan>,
}

/// Compile a PyQL expression string in the context of a named type to a bare SQL
/// expression suitable for use in an UPDATE SET clause.
///
/// Column references (`.field`) are emitted without a table alias because UPDATE
/// SET expressions reference the current row directly.  Query parameters (`$name`)
/// are rejected — fill expressions must be literal values or field references.
pub fn compile_fill_expr(
    type_name: &str,
    expr_str: &str,
    schema: &SchemaDescriptor,
) -> Result<String, crate::error::PyQLError> {
    let expr_ast = parse::parse_expr(expr_str)?;
    let (ir_expr, params) = ir::compile_expr_unaliased(&expr_ast, type_name, schema)?;
    if !params.is_empty() {
        return Err(crate::error::PyQLError::Syntax(crate::error::PyQLSyntaxError {
            message: "fill expressions may not contain query parameters".into(),
            position: crate::error::Position { line: 0, col: 0 },
        }));
    }
    Ok(sql::emit_expr(&ir_expr))
}

/// Compile a PyQL query string to SQL against `schema`.
///
/// Results are cached in a process-global LRU (capacity 1024). Call
/// `clear_query_cache()` when the schema is reloaded to avoid stale entries.
/// Synchronous — compilation is CPU-bound; async lives at the DB execution layer.
/// Raises `PyQLError` on any grammar, type, or resolution failure.
pub fn compile(query: &str, schema: &SchemaDescriptor) -> Result<CompiledQuery, PyQLError> {
    {
        let mut cache = query_cache().write().unwrap();
        if let Some(cached) = cache.get(query) {
            return Ok(cached.clone());
        }
    }
    let compiled = compile_uncached(query, schema)?;
    query_cache().write().unwrap().put(query.to_string(), compiled.clone());
    Ok(compiled)
}

fn compile_uncached(query: &str, schema: &SchemaDescriptor) -> Result<CompiledQuery, PyQLError> {
    let ast = parse::parse(query)?;
    let ir_out = ir::compile(&ast, schema)?;
    let sql_out = sql::emit(&ir_out);
    Ok(CompiledQuery {
        sql: sql_out.sql,
        param_names: ir_out.params,
        params: Vec::new(),
        shape: sql_out.shape,
        warnings: ir_out.warnings,
        deferred_search_plan: sql_out.deferred_search_plan,
    })
}
