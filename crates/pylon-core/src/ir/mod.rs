// Pylon IR — a typed, resolved query plan produced by compiling a PyQL AST
// against a SchemaDescriptor.
//
// Deliberately simpler than Gel's IR: no set-semantics wrappers, no PathId
// deduplication. Every node is already resolved to a concrete table/column.

mod compiler;

pub use compiler::compile;
pub use compiler::compile_expr_in_type;
pub use compiler::compile_fn_body;

use crate::parse::ast::{BinOpKind, UnaryOpKind};

// ── Top-level statement ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IrStmt {
    Select(IrSelect),
    /// A SELECT over a free expression: set literal, tuple, free object, or scalar function.
    FreeSelect(IrFreeSelect),
    /// A flat SELECT produced by absolute path traversal: `select TypeName.link.prop`.
    PathSelect(IrPathSelect),
    Insert(IrInsert),
    Update(IrUpdate),
    Delete(IrDelete),
    /// `for var in iterator union body`
    For(IrFor),
    /// `group Type [shape] [using alias := expr, ...] by key, ...`
    Group(IrGroup),
    /// `select fn(args) { shape }` — a SELECT driven by a user-defined object-returning function.
    FunctionSelect(IrFunctionSelect),
    /// `select vector::search(Type, $vec) { object { … }, distance }` — pgvector similarity search.
    VectorSearch(IrVectorSearch),
    /// `select fts::search(Type, $query) { object { … }, score }` — full-text search.
    FtsSearch(IrFtsSearch),
}

// ── FOR LOOP ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrFor {
    pub var_name: String,
    pub iterator: IrForIterator,
    pub body: Box<IrStmt>,
}

#[derive(Debug, Clone)]
pub enum IrForIterator {
    /// Set literal of scalar values → SQL VALUES clause.
    Values { exprs: Vec<IrExpr>, pg_type: String },
}

// ── GROUP ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrGroup {
    pub source: IrSource,
    /// Fields projected into each element of the `elements` array.
    pub shape: Vec<IrShapeField>,
    /// Ordered list of (key_name, key_expr) — what we GROUP BY.
    pub keys: Vec<(String, IrExpr)>,
}

// ── PATH SELECT (type-rooted path traversal) ────────────────────────────────────

/// `select Person.company.name` — a flat SELECT that starts at a root type and
/// follows links before projecting a final scalar column or object id.
#[derive(Debug, Clone)]
pub struct IrPathSelect {
    pub root: IrSource,
    pub joins: Vec<IrPathJoin>,
    pub result: IrPathResult,
    pub filter: Option<IrExpr>,
    pub order_by: Vec<IrSort>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    pub distinct: bool,
    /// Non-empty when the root is a polymorphic (abstract+materialized) type.
    /// The FROM clause uses a UNION ALL of these instead of the root table directly.
    pub poly_implementors: Vec<IrPolyImplementor>,
}

#[derive(Debug, Clone)]
pub enum IrPathJoin {
    /// Traverse a single (FK) link.
    Single { source_alias: String, fk_col: String, target: IrSource },
    /// Traverse a multi-link via a junction table.
    Multi { source_alias: String, junction_alias: String, join: IrMultiLinkJoin, target: IrSource },
    /// Reverse of a single FK link: find owner rows whose FK column points to the current row.
    BacklinkSingle { source_alias: String, fk_col: String, target: IrSource },
    /// Reverse of a multi-link: traverse the junction table in reverse.
    BacklinkMulti {
        source_alias: String,
        junction_alias: String,
        junction_table: String,
        module: String,
        /// Junction column that points to the owner (forward source).
        owner_col: String,
        /// Junction column that points to the current row (forward target).
        current_col: String,
        target: IrSource,
    },
}

#[derive(Debug, Clone)]
pub enum IrPathResult {
    /// Final result is a scalar expression (column ref or computed expr like EXISTS).
    Scalar(IrExpr),
    /// Final step is a link — return the linked objects with the given shape.
    Object { alias: String, type_name: String, shape: Vec<IrShapeField> },
}

// ── FREE SELECT (expressions, set literals, tuples, free objects) ───────────────

/// A SELECT that does not reference a schema type.
/// Emitted as one or more UNION ALL branches with a ROW(…) wrapper.
#[derive(Debug, Clone)]
pub struct IrFreeSelect {
    /// One item per UNION ALL branch (set literals expand to multiple items).
    pub items: Vec<IrFreeExpr>,
    pub order_by: Vec<IrSort>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub enum IrFreeExpr {
    /// A single scalar value: `SELECT {1}`, `SELECT 'hello'`, `SELECT func()`.
    Scalar(IrExpr),
    /// A free object: `SELECT { foo := 'bar', n := 42 }`.
    FreeObject(Vec<(String, IrExpr)>),
    /// An anonymous tuple: `SELECT (1, 'x')`.
    Tuple(Vec<IrExpr>),
    /// A set-returning assert: `SELECT ROW(v) FROM unnest(_pylon.fn(ARRAY(inner))) v`.
    /// Used for `assert_exists` and `assert_distinct` which pass through the set.
    AssertSet { fn_name: String, inner: Box<IrArraySource> },
    /// Pass all rows from a scalar CTE through: `SELECT "result" FROM "cte_name"`.
    CtePassthrough(String),
}

/// Source for `ARRAY(SELECT scalar FROM ...)` — used by assert functions.
#[derive(Debug, Clone)]
pub enum IrArraySource {
    /// Inner is a regular schema SELECT; first scalar field is the array element.
    Select(IrSelect),
    /// Inner is a path traversal SELECT; scalar result is the array element.
    PathSelect(IrPathSelect),
    /// `ARRAY(SELECT expr FROM source)` — cross-scope type-is iteration.
    RawExpr {
        source: IrSource,
        /// Non-empty when source is polymorphic; replaces `source` with a UNION ALL.
        poly_implementors: Vec<IrPolyImplementor>,
        poly_columns: Vec<String>,
        expr: IrExpr,
    },
}

// ── SELECT ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrSelect {
    pub source: IrSource,
    pub shape: Vec<IrShapeField>,
    pub filter: Option<IrExpr>,
    pub order_by: Vec<IrSort>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    pub distinct: bool,
    /// When this SELECT wraps a DML statement (`SELECT (INSERT …) { … }`),
    /// the inner DML is stored here and emitted as a CTE.
    /// `None` for plain `SELECT Type { … }`.
    pub dml_source: Option<Box<IrStmt>>,
    /// True when this SELECT targets an interface type.
    /// The SQL emitter builds a UNION ALL inline instead of hitting the view.
    pub polymorphic: bool,
    /// Concrete implementors of the interface (populated when `polymorphic = true`).
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// Interface column names used in the UNION ALL branches (e.g. `["id", "email"]`).
    pub poly_columns: Vec<String>,
}

/// One concrete type that implements a polymorphic interface.
#[derive(Debug, Clone)]
pub struct IrPolyImplementor {
    /// Qualified type name, e.g. `default::Individual`.
    pub type_name: String,
    /// PostgreSQL table name.
    pub table: String,
    /// Schema / module name.
    pub module: String,
}

/// The relation being queried: a resolved type with its PostgreSQL table name
/// and a unique alias for this occurrence in the query.
#[derive(Debug, Clone)]
pub struct IrSource {
    /// Qualified type name, e.g. `catalog::Product`.
    pub type_name: String,
    /// PostgreSQL table name, e.g. `catalog_product`.
    pub table: String,
    /// Alias used in emitted SQL, e.g. `t0`.
    pub alias: String,
}

/// A set-valued scalar computed field from cross-scope `TypeIs`.
/// Emits `COALESCE(array_agg(ROW(bool_expr)::record), ARRAY[]::record[]) FROM source`.
#[derive(Debug, Clone)]
pub struct IrScalarSetField {
    pub alias: String,
    pub source: IrSource,
    pub poly_implementors: Vec<IrPolyImplementor>,
    pub poly_columns: Vec<String>,
    pub bool_expr: IrExpr,
}

#[derive(Debug, Clone)]
pub enum IrShapeField {
    Scalar(IrScalarField),
    SingleLink(IrSingleLinkField),
    MultiLink(IrMultiLinkField),
    Computed(IrComputedField),
    ScalarSet(IrScalarSetField),
}

/// A property column included in the output shape.
#[derive(Debug, Clone)]
pub struct IrScalarField {
    /// The output key (what the user wrote, e.g. `name`).
    pub alias: String,
    /// The PostgreSQL column name on the source table.
    pub column: String,
    /// The PostgreSQL type string, e.g. `text`, `int8`.
    pub pg_type: String,
}

/// A single-valued FK link included in the output shape.
/// Emitted as a correlated scalar subquery.
#[derive(Debug, Clone)]
pub struct IrSingleLinkField {
    pub alias: String,
    /// FK column on the source table, e.g. `category_id`.
    pub fk_column: String,
    /// PK column on the target table, e.g. `id`.
    pub target_pk: String,
    /// Nested SELECT producing the linked object.
    pub subquery: IrSelect,
}

/// A multi-valued link included in the output shape.
/// Emitted as a correlated subquery using `array_agg(ROW(...)::record)`.
#[derive(Debug, Clone)]
pub struct IrMultiLinkField {
    pub alias: String,
    /// Join table or FK column identifying the source side.
    pub join: IrMultiLinkJoin,
    /// Nested SELECT producing the linked objects.
    pub subquery: IrSelect,
    /// Extra scalar columns from a junction table (`@prop` syntax).
    pub link_properties: Vec<IrLinkProp>,
}

/// A single link property pulled from a junction table.
#[derive(Debug, Clone)]
pub struct IrLinkProp {
    /// The column name in the junction table and alias in the output.
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum IrMultiLinkJoin {
    /// Standard Pylon junction table (`{source_table}.{link_name}`) with
    /// `source uuid` and `target uuid` columns.
    Standard {
        /// Unqualified junction table name, e.g. `person.posts`.
        junction_table: String,
        /// Schema module for quoting, e.g. `default`.
        module: String,
    },
    /// Explicit through type with resolved FK columns.
    Through {
        /// PostgreSQL table of the junction type.
        junction_table: String,
        /// Schema module of the junction type.
        module: String,
        /// Column on the junction table that references the source type's id.
        source_col: String,
        /// Column on the junction table that references the target type's id.
        target_col: String,
    },
}

/// A computed field: an expression aliased to a name.
#[derive(Debug, Clone)]
pub struct IrComputedField {
    pub alias: String,
    pub expr: IrExpr,
}

// ── Vector index enqueue ────────────────────────────────────────────────────────

/// Identifies one vector index that needs an outbox row written when a
/// mutation touches its source fields.
#[derive(Debug, Clone)]
pub struct VectorEnqueueInfo {
    /// Schema-qualified type name, e.g. `"default::Product"`.
    pub type_name: String,
    /// `None` = default index, `Some(name)` = named index.
    pub index_name: Option<String>,
}

/// Identifies one OpenSearch-backed SearchIndex that needs an outbox row written.
#[derive(Debug, Clone)]
pub struct SearchEnqueueInfo {
    pub type_name: String,
    pub index_name: Option<String>,
    /// `"index"` for insert/update, `"delete"` for delete.
    pub operation: &'static str,
}

// ── INSERT ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrInsert {
    pub target: IrSource,
    /// Each element is (column_name, value_expr).
    pub assignments: Vec<(String, IrExpr)>,
    pub unless_conflict: Option<IrConflict>,
    /// Schema-defined rewrites that override or augment the inserted columns.
    pub rewrites: Vec<IrRewrite>,
    /// Shape to return after insert (for RETURNING clause).
    pub returning: Vec<IrShapeField>,
    /// Vector indexes on this type that need outbox rows written.
    pub enqueue_vector: Vec<VectorEnqueueInfo>,
    /// OpenSearch-backed SearchIndexes that need outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
}

#[derive(Debug, Clone)]
pub struct IrConflict {
    /// Column expression for `ON CONFLICT (col)`. None → any conflict.
    pub on: Option<IrExpr>,
    /// `DO UPDATE SET` assignments. None → `DO NOTHING`.
    pub do_update: Option<Vec<(String, IrExpr)>>,
}

// ── UPDATE ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrUpdate {
    pub target: IrSource,
    pub filter: Option<IrExpr>,
    pub assignments: Vec<(String, IrExpr)>,
    /// Schema-defined rewrites appended to the SET clause.
    pub rewrites: Vec<IrRewrite>,
    pub returning: Vec<IrShapeField>,
    /// Vector indexes whose source fields are touched by this update.
    pub enqueue_vector: Vec<VectorEnqueueInfo>,
    /// OpenSearch-backed SearchIndexes that need outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
    /// Populated when updating an interface type; one entry per concrete implementor.
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// `friends := {}` — DELETE all junction rows for this object.
    pub multi_link_clears: Vec<IrMultiLinkClear>,
    /// `friends := expr` — clear + insert (both lists share the same index).
    pub multi_link_replaces: Vec<IrMultiLinkMutation>,
    /// `friends += expr` — INSERT junction rows.
    pub multi_link_appends: Vec<IrMultiLinkMutation>,
    /// `friends -= expr` — DELETE specific junction rows.
    pub multi_link_removals: Vec<IrMultiLinkMutation>,
}

#[derive(Debug, Clone)]
pub struct IrMultiLinkClear {
    pub junction_table: String,
    pub module: String,
    /// Column on the junction table referencing the source object's id.
    pub source_col: String,
}

/// A multi-link mutation: insert or delete specific rows in a junction table.
#[derive(Debug, Clone)]
pub struct IrMultiLinkMutation {
    pub junction_table: String,
    pub module: String,
    /// Column on the junction table referencing the source (updated) object's id.
    pub source_col: String,
    /// Column on the junction table referencing the target object's id.
    pub target_col: String,
    /// The set of target objects.
    pub values: IrMultiLinkValues,
}

/// How to obtain the target object IDs for a multi-link mutation.
#[derive(Debug, Clone)]
pub enum IrMultiLinkValues {
    /// Reference to a named CTE: `FROM "cte_name"`.
    CteRef(String),
    /// A regular schema SELECT (use source table + filter to get ids).
    Select(Box<IrSelect>),
    /// A path-traversal SELECT (root + joins, final result is the id).
    PathSelect(Box<IrPathSelect>),
}

// ── DELETE ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrDelete {
    pub target: IrSource,
    pub filter: Option<IrExpr>,
    pub returning: Vec<IrShapeField>,
    /// Populated when deleting from an interface type; one entry per concrete implementor.
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// OpenSearch-backed SearchIndexes that need delete outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
}

// ── Expressions ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IrExpr {
    /// A resolved column reference, e.g. `t0.name`.
    ColumnRef { alias: String, column: String, pg_type: String },
    /// A positional query parameter `$N` (0-based index internally).
    Param { index: usize },
    Literal(IrLiteral),
    BinOp(Box<IrBinOp>),
    UnaryOp(Box<IrUnaryOp>),
    FunctionCall(IrFunctionCall),
    TypeCast(Box<IrTypeCast>),
    IfElse(Box<IrIfElse>),
    /// A scalar subquery (used for computed fields that are themselves selects).
    Subquery(Box<IrSelect>),
    /// An array literal: `[1, 2, 3]`.
    Array(Vec<IrExpr>),
    /// The empty set `{}` used as an assignment value — emits SQL `NULL`.
    Null,
    /// An aggregate function applied to an inline set literal `fn({e1, e2, ...})`.
    /// Emits: `(SELECT fn_name(v) FROM (SELECT e1 UNION ALL ...) AS _set(v))`
    AggOverSet { fn_name: String, schema: Option<String>, elems: Vec<IrExpr> },
    /// A reference to a named CTE used in expression context.
    /// `scalar = true`  → emits `(SELECT "result" FROM "cte_name")`
    /// `scalar = false` → emits `(SELECT "id"     FROM "cte_name")`
    CteRef { name: String, scalar: bool },
    /// Reference to the current for-loop iterator variable.
    /// Emits `"_for_{name}"."v"`.
    ForVar { name: String },
    /// `ARRAY(SELECT scalar FROM source [JOINs] [WHERE filter])`.
    /// Used as the array argument to `_pylon.assert_single/exists/distinct`.
    ArrayFromSelect(Box<IrArraySource>),
    /// An enum member access: `default::Gender.Female` → `'Female'::"default"."Gender"`.
    EnumLiteral { pg_type: String, variant: String },
    /// Named tuple construction: `(x := 1.0, y := 2.0)` → `jsonb_build_object('x', 1.0, 'y', 2.0)`.
    NamedTuple(Vec<(String, IrExpr)>),
    /// Session global: emits `$N::pg_type` directly. The parameter slot carries the `__global__` prefix.
    GlobalParam { index: usize, pg_type: String },
    /// Computed global reference: emits `(SELECT "value" FROM "cte_name")`.
    GlobalRef { cte_name: String },
    /// Index access `expr[i]`: `substr(expr, i+1, 1)` for strings/bytes, `(expr)[i+1]` for arrays.
    Subscript { expr: Box<IrExpr>, index: Box<IrExpr>, is_array: bool },
    /// Named tuple / jsonb field access: `(expr)->'field'` (returns jsonb).
    JsonbField { expr: Box<IrExpr>, field: String },
    /// Slice access `expr[lower:upper]`: `substr` for strings/bytes, PG subscript for arrays.
    Slice {
        expr: Box<IrExpr>,
        lower: Option<Box<IrExpr>>,
        upper: Option<Box<IrExpr>>,
        is_array: bool,
    },
    /// Detached path as a scalar subquery: `(SELECT scalar FROM root [JOINs])`.
    /// Used when `detached TypeName.prop` appears in a schema-bound expression context.
    PathSubquery(Box<IrPathSelect>),
    /// A named parameter reference inside a user-defined function body.
    /// Emitted as a double-quoted SQL identifier: `"param_name"`.
    FnParam { name: String, pg_type: String },
}

// ── Vector search ─────────────────────────────────────────────────────────────

/// `select vector::search(Type, $vec) { object { … }, distance }`
///
/// Emits a single SELECT from the type's table that computes the distance
/// inline and returns a virtual `{ object, distance }` shape.
#[derive(Debug, Clone)]
pub struct IrVectorSearch {
    /// The searched type as an `IrSource` (table + alias).
    pub source: IrSource,
    /// pgvector column name, e.g. `__vector__`.
    pub vector_col: String,
    /// pgvector distance operator: `<=>`, `<->`, or `<#>`.
    pub distance_op: &'static str,
    /// The query vector expression (e.g. `$1::vector`).
    pub query_expr: IrExpr,
    /// Fields to include in the `object` sub-tuple (from the `object { … }` shape).
    /// Empty means no explicit shape was given; the SQL emitter uses all properties.
    pub object_shape: Vec<IrShapeField>,
    pub filter: Option<IrExpr>,
    /// `None` = no ORDER BY; `Some(dir)` = ORDER BY distance in that direction.
    /// Only distance ordering is supported for v1.
    pub order_by_distance: Option<IrSortDir>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
}

// ── Full-text search ──────────────────────────────────────────────────────────

/// `select fts::search(Type, $query) { object { … }, score }`
///
/// Emits a SELECT with a `WHERE tsvector @@ tsquery` filter and a `ts_rank`
/// score returned alongside the matched object.
#[derive(Debug, Clone)]
pub struct IrFtsSearch {
    /// The searched type as an `IrSource` (table + alias).
    pub source: IrSource,
    /// Backend that owns this search index.
    pub backend: crate::schema::SearchBackend,
    /// tsvector column name, e.g. `__search__` (Postgres backend only).
    pub search_col: String,
    /// PostgreSQL tsquery constructor (Postgres backend only).
    pub tsquery_fn: &'static str,
    /// The query text expression (e.g. `$1`).
    pub query_expr: IrExpr,
    /// Fields to include in the `object` sub-tuple.
    pub object_shape: Vec<IrShapeField>,
    pub filter: Option<IrExpr>,
    pub order_by_rank: Option<IrSortDir>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    /// Remote index name (derived from type + index_name, set for deferred backends).
    pub deferred_index_name: Option<String>,
    /// Name of the user's query-text param (e.g. "query"), for the deferred search plan.
    pub deferred_query_param_name: Option<String>,
    /// Inline literal query text (when not a param), for the deferred search plan.
    pub deferred_query_literal: Option<String>,
    /// Param index for the uuid[] IDs injected by the Python layer (deferred backend).
    pub deferred_ids_param: Option<usize>,
    /// Param index for the float8[] scores injected by the Python layer (deferred backend).
    pub deferred_scores_param: Option<usize>,
}

// ── User-defined function SELECT ─────────────────────────────────────────────

/// `select fn(args) { shape }` — a SELECT over the result set of a user-defined
/// object-returning function.  Emits `FROM "module"."fn"(args) AS alias`.
#[derive(Debug, Clone)]
pub struct IrFunctionSelect {
    pub fn_module: String,
    pub fn_name: String,
    pub fn_args: Vec<IrExpr>,
    /// Alias for the function result row source.
    pub alias: String,
    /// Qualified return type name (e.g. `account::Account`).
    pub type_name: String,
    /// True when the return type is a polymorphic interface.
    pub polymorphic: bool,
    /// Concrete implementors when polymorphic = true.
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// Interface column names for the UNION ALL branches.
    pub poly_columns: Vec<String>,
    pub shape: Vec<IrShapeField>,
    pub filter: Option<IrExpr>,
    pub order_by: Vec<IrSort>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub struct IrBinOp {
    pub left: IrExpr,
    pub op: BinOpKind,
    pub right: IrExpr,
}

#[derive(Debug, Clone)]
pub struct IrUnaryOp {
    pub op: UnaryOpKind,
    pub operand: IrExpr,
}

#[derive(Debug, Clone)]
pub struct IrFunctionCall {
    pub schema: Option<String>,
    pub name: String,
    pub args: Vec<IrExpr>,
    /// Set for `SqlExpression` impls: raw SQL template where `$1`, `$2`, … are
    /// replaced with the emitted arg expressions.
    pub sql_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IrTypeCast {
    pub expr: IrExpr,
    /// PostgreSQL cast target, e.g. `text`, `int8`, `uuid`.
    pub pg_type: String,
}

#[derive(Debug, Clone)]
pub struct IrIfElse {
    pub condition: IrExpr,
    pub if_: IrExpr,
    pub else_: IrExpr,
}

#[derive(Debug, Clone)]
pub enum IrLiteral {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

// ── Sort ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrSort {
    pub expr: IrExpr,
    pub direction: IrSortDir,
    pub nulls: IrNulls,
}

#[derive(Debug, Clone)]
pub enum IrSortDir { Asc, Desc }

#[derive(Debug, Clone)]
pub enum IrNulls { First, Last }

// ── Compiled output ──────────────────────────────────────────────────────────────

/// A compiled mutation rewrite: a property column whose value is overridden by
/// a schema-defined expression at INSERT/UPDATE time.
#[derive(Debug, Clone)]
pub struct IrRewrite {
    /// PostgreSQL column name of the property being overridden.
    pub column: String,
    /// Compiled expression that produces the override value.
    /// For INSERT: column refs are substituted with the corresponding assignment
    /// expressions so the result is self-contained in a VALUES clause.
    /// For UPDATE: column refs use the table alias and are valid in a SET clause.
    pub expr: IrExpr,
}

/// One `name := (stmt)` binding from a WITH block.
#[derive(Debug, Clone)]
pub struct IrCteDef {
    pub name: String,
    pub stmt: IrStmt,
    /// Qualified type name of the result set (e.g. `"default::Person"`).
    /// Empty for free expressions.
    pub type_name: String,
}

/// A session global CTE: `WITH "cte_name" AS (SELECT $N::pg_type AS "value")`.
#[derive(Debug, Clone)]
pub struct IrSessionGlobalCte {
    pub cte_name: String,
    pub qualified_name: String,
    pub param_index: usize,
    pub pg_type: String,
}

/// A computed global CTE: `WITH "cte_name" AS (<compiled stmt returning "value" column>)`.
#[derive(Debug, Clone)]
pub struct IrComputedGlobalCte {
    pub cte_name: String,
    pub qualified_name: String,
    pub stmt: IrStmt,
}

#[derive(Debug, Clone)]
pub enum IrGlobalCte {
    Session(IrSessionGlobalCte),
    Computed(IrComputedGlobalCte),
}

impl IrGlobalCte {
    pub fn cte_name(&self) -> &str {
        match self {
            Self::Session(s) => &s.cte_name,
            Self::Computed(c) => &c.cte_name,
        }
    }
}

/// The result of the IR compilation step.
/// Carries the query plan and the ordered list of named parameters, which the
/// SQL emitter uses to emit `$1 … $N` and the client uses to bind values.
pub struct IrOutput {
    pub stmt: IrStmt,
    /// Ordered parameter names, positionally matching `$1`, `$2`, … in the SQL.
    /// Global params use the `__global__module::name` prefix; user params use bare names.
    pub params: Vec<String>,
    /// User-defined CTE bindings from a WITH block, in declaration order.
    pub ctes: Vec<IrCteDef>,
    /// Global variable CTEs (session-injected or computed), in dependency order.
    pub global_ctes: Vec<IrGlobalCte>,
    /// Non-fatal warnings produced during compilation.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    #[allow(unused_imports)]
    use super::{IrFreeExpr, IrLiteral};
    use crate::schema::{
        ComputedDescriptor, GlobalDescriptor, LinkDescriptor, MultiLinkDescriptor,
        PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
    };

    fn make_schema() -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![
                TypeDescriptor {
                    name: "Person".into(),
                    module: "default".into(),
                    table: "person".into(),
                    abstract_: false,
                    materialized: false,
                    description: None,
                    parents: vec![],
                    interfaces: vec![],
                    properties: vec![
                        PropertyDescriptor {
                            name: "id".into(),
                            pg_type: "uuid".into(),
                            nullable: false,
                            default_sql: Some("uuidv7()".into()),
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: true,
                            is_pk: true,
                            is_readonly: true,
                            rewrites: vec![],
                        },
                        PropertyDescriptor {
                            name: "name".into(),
                            pg_type: "text".into(),
                            nullable: false,
                            default_sql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                        },
                        PropertyDescriptor {
                            name: "age".into(),
                            pg_type: "int8".into(),
                            nullable: true,
                            default_sql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                        },
                    ],
                    links: vec![LinkDescriptor {
                        name: "company".into(),
                        target: "default::Company".into(),
                        nullable: true,
                        description: None,
                        is_exclusive: false,
                        is_readonly: false,
                        rewrites: vec![],
                        on_delete: vec![],
                    }],
                    multilinks: vec![MultiLinkDescriptor {
                        name: "posts".into(),
                        target: "default::Post".into(),
                        through: None,
                        nullable: false,
                        description: None,
                        on_delete: vec![],
                    }],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
                TypeDescriptor {
                    name: "Company".into(),
                    module: "default".into(),
                    table: "company".into(),
                    abstract_: false,
                    materialized: false,
                    description: None,
                    parents: vec![],
                    interfaces: vec![],
                    properties: vec![PropertyDescriptor {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        default_sql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
                TypeDescriptor {
                    name: "Post".into(),
                    module: "default".into(),
                    table: "post".into(),
                    abstract_: false,
                    materialized: false,
                    description: None,
                    parents: vec![],
                    interfaces: vec![],
                    properties: vec![PropertyDescriptor {
                        name: "title".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        default_sql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
            ],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
            functions: vec![],
        }
    }

    fn compile(query: &str) -> IrOutput {
        let schema = make_schema();
        let ast = parse::parse(query).expect("parse failed");
        super::compile(&ast, &schema).expect("IR compile failed")
    }

    #[test]
    fn test_select_resolves_source() {
        let ir = compile("SELECT Person { name, age }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        assert_eq!(sel.source.table, "person");
        assert_eq!(sel.source.type_name, "default::Person");
        assert_eq!(sel.shape.len(), 2);
        assert!(matches!(sel.shape[0], IrShapeField::Scalar(_)));
    }

    #[test]
    fn test_select_filter_param_ordering() {
        let ir = compile("SELECT Person { name } FILTER .name = $name AND .age > $min_age");
        assert_eq!(ir.params, vec!["name", "min_age"]);
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        assert!(sel.filter.is_some());
    }

    #[test]
    fn test_select_single_link() {
        let ir = compile("SELECT Person { name, company { name } }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        assert_eq!(sel.shape.len(), 2);
        let IrShapeField::SingleLink(link) = &sel.shape[1] else { panic!("expected SingleLink") };
        assert_eq!(link.alias, "company");
        assert_eq!(link.fk_column, "company_id");
        assert_eq!(link.subquery.source.table, "company");
    }

    #[test]
    fn test_select_multi_link() {
        let ir = compile("SELECT Person { name, posts { title } }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let IrShapeField::MultiLink(ml) = &sel.shape[1] else { panic!("expected MultiLink") };
        assert_eq!(ml.alias, "posts");
        assert_eq!(ml.subquery.source.table, "post");
        let IrMultiLinkJoin::Standard { junction_table, .. } = &ml.join else { panic!() };
        assert_eq!(junction_table, "person.posts");
    }

    #[test]
    fn test_select_no_shape_returns_id_only() {
        let ir = compile("SELECT Person");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        // Bare SELECT Type returns only { id }, matching Gel semantics.
        assert_eq!(sel.shape.len(), 1);
        let IrShapeField::Scalar(f) = &sel.shape[0] else { panic!() };
        assert_eq!(f.alias, "id");
    }

    #[test]
    fn test_free_select_set_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT {1, 2, 3}").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::FreeSelect(sel) = ir.stmt else { panic!("expected FreeSelect") };
        assert_eq!(sel.items.len(), 3);
        assert!(matches!(sel.items[0], IrFreeExpr::Scalar(IrExpr::Literal(IrLiteral::Int(1)))));
    }

    #[test]
    fn test_free_select_free_object() {
        let schema = make_schema();
        let ast = parse::parse("SELECT { foo := 'bar', n := 42 }").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::FreeSelect(sel) = ir.stmt else { panic!("expected FreeSelect") };
        assert_eq!(sel.items.len(), 1);
        let IrFreeExpr::FreeObject(fields) = &sel.items[0] else { panic!("expected FreeObject") };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "foo");
        assert_eq!(fields[1].0, "n");
    }

    #[test]
    fn test_free_select_tuple() {
        let schema = make_schema();
        let ast = parse::parse("SELECT (1, 'hello')").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::FreeSelect(sel) = ir.stmt else { panic!("expected FreeSelect") };
        assert_eq!(sel.items.len(), 1);
        assert!(matches!(sel.items[0], IrFreeExpr::Tuple(_)));
    }

    #[test]
    fn test_free_select_scalar_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT 42").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::FreeSelect(sel) = ir.stmt else { panic!("expected FreeSelect") };
        assert_eq!(sel.items.len(), 1);
        assert!(matches!(sel.items[0], IrFreeExpr::Scalar(IrExpr::Literal(IrLiteral::Int(42)))));
    }

    #[test]
    fn test_free_select_function_call() {
        let schema = make_schema();
        let ast = parse::parse("SELECT str_lower('HELLO')").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::FreeSelect(sel) = ir.stmt else { panic!("expected FreeSelect") };
        assert!(matches!(sel.items[0], IrFreeExpr::Scalar(IrExpr::FunctionCall(_))));
    }

    #[test]
    fn test_free_select_rejects_dot_path() {
        let schema = make_schema();
        let ast = parse::parse("SELECT {.name}").unwrap();
        assert!(super::compile(&ast, &schema).is_err());
    }

    #[test]
    fn test_type_error_uuid_eq_str() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .id = 'not-a-uuid'").unwrap();
        let err = super::compile(&ast, &schema).err().expect("expected type error");
        let msg = err.to_string();
        assert!(msg.contains("std::uuid") && msg.contains("std::str"), "unexpected: {msg}");
    }

    #[test]
    fn test_type_error_str_eq_int() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = 42").unwrap();
        let err = super::compile(&ast, &schema).err().expect("expected type error");
        let msg = err.to_string();
        assert!(msg.contains("std::str") && msg.contains("std::int64"), "unexpected: {msg}");
    }

    #[test]
    fn test_int_literal_compatible_with_all_int_columns() {
        // age is int8; a bare integer literal is compatible with any int column
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .age = 30").unwrap();
        assert!(super::compile(&ast, &schema).is_ok());
    }

    #[test]
    fn test_cast_int16_compatible_with_int8_column() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .age = <int16>30").unwrap();
        assert!(super::compile(&ast, &schema).is_ok());
    }

    #[test]
    fn test_unknown_type_error() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Ghost { name }").unwrap();
        assert!(super::compile(&ast, &schema).is_err());
    }

    #[test]
    fn test_unknown_field_error() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person { nonexistent }").unwrap();
        assert!(super::compile(&ast, &schema).is_err());
    }

    #[test]
    fn test_insert_compiles_assignments() {
        let ir = compile("INSERT Person { name := 'Alice', age := 30 }");
        let IrStmt::Insert(ins) = ir.stmt else { panic!() };
        assert_eq!(ins.target.table, "person");
        assert_eq!(ins.assignments.len(), 2);
        assert_eq!(ins.assignments[0].0, "name");
        assert_eq!(ins.assignments[1].0, "age");
    }

    #[test]
    fn test_delete_compiles_filter() {
        let ir = compile("DELETE Person FILTER .name = $name");
        let IrStmt::Delete(del) = ir.stmt else { panic!() };
        assert!(del.filter.is_some());
        assert_eq!(ir.params, vec!["name"]);
    }

    fn make_schema_with_computed() -> SchemaDescriptor {
        let mut schema = make_schema();
        // Add a computed field to Person
        schema.types[0].computed.push(ComputedDescriptor {
            name: "upper_name".into(),
            expression: "str_upper(.name)".into(),
            return_type: Some("text".into()),
        });
        schema
    }

    #[test]
    fn test_computed_field_in_shape() {
        let schema = make_schema_with_computed();
        let ast = parse::parse("SELECT Person { upper_name }").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        // upper_name should compile to a Computed shape field
        assert!(sel.shape.iter().any(|f| matches!(f, IrShapeField::Computed(c) if c.alias == "upper_name")));
    }

    #[test]
    fn test_computed_field_in_expression_context() {
        let schema = make_schema_with_computed();
        let ast = parse::parse("SELECT Person { x := str_lower(.upper_name) }").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        assert!(sel.shape.iter().any(|f| matches!(f, IrShapeField::Computed(c) if c.alias == "x")));
    }

    #[test]
    fn test_multi_sort_with_then() {
        let ir = compile("SELECT Person { name } ORDER BY .name THEN .age");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        assert_eq!(sel.order_by.len(), 2);
    }

    #[test]
    fn test_multi_link_filter_emits_warning() {
        let ir = compile("SELECT Person { name } FILTER .posts.title = 'hello'");
        assert!(!ir.warnings.is_empty(), "expected a warning for multi-link in filter");
        assert!(ir.warnings[0].contains("posts"));
    }

    #[test]
    fn test_session_global_produces_cte() {
        let mut schema = make_schema();
        schema.globals.push(GlobalDescriptor {
            name: "viewer_id".into(),
            module: "default".into(),
            scalar_type: "UUID".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });
        let ast = parse::parse("SELECT Person FILTER .id = global viewer_id").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        assert_eq!(ir.global_ctes.len(), 1);
        assert_eq!(ir.global_ctes[0].cte_name(), "__global__default::viewer_id");
        assert_eq!(ir.params, vec!["__global__default::viewer_id"]);
    }

    #[test]
    fn test_string_index_compiles() {
        let ast = parse::parse("SELECT 'hello'[1]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::FreeSelect(fs) = ir.stmt else { panic!() };
        assert!(matches!(
            &fs.items[0],
            IrFreeExpr::Scalar(IrExpr::Subscript { is_array: false, .. })
        ));
    }

    #[test]
    fn test_array_index_compiles() {
        let ast = parse::parse("SELECT [1, 2, 3][0]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::FreeSelect(fs) = ir.stmt else { panic!() };
        assert!(matches!(
            &fs.items[0],
            IrFreeExpr::Scalar(IrExpr::Subscript { is_array: true, .. })
        ));
    }

    #[test]
    fn test_string_slice_compiles() {
        let ast = parse::parse("SELECT 'hello'[1:3]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::FreeSelect(fs) = ir.stmt else { panic!() };
        assert!(matches!(
            &fs.items[0],
            IrFreeExpr::Scalar(IrExpr::Slice { is_array: false, .. })
        ));
    }

    #[test]
    fn test_array_slice_compiles() {
        let ast = parse::parse("SELECT [1, 2, 3][0:2]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::FreeSelect(fs) = ir.stmt else { panic!() };
        assert!(matches!(
            &fs.items[0],
            IrFreeExpr::Scalar(IrExpr::Slice { is_array: true, .. })
        ));
    }
}
