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

// Pylon IR — a typed, resolved query plan produced by compiling a PyQL AST
// against a SchemaDescriptor.
//
// Deliberately simple: no set-semantics wrappers, no PathId deduplication.
// Every node is already resolved to a concrete table/column.

mod compiler;
pub mod tags;

pub use compiler::compile;
pub use compiler::compile_constraint_expr;
pub use compiler::compile_expr_in_type;
pub use compiler::compile_expr_unaliased;
pub use compiler::compile_scalar_default;
pub use compiler::compile_scalar_default_typed;
pub use compiler::compile_trigger_handler;
pub use compiler::compile_with_config;
pub(crate) use compiler::infer_ir_type;
pub use compiler::pg_type_to_pyql;
pub(crate) use compiler::types_compatible;
pub use compiler::{GLOBALS_ARG, compile_fn_body, functions_needing_globals};

use crate::parse::ast::{BinOpKind, UnaryOpKind};

// ── Session config ───────────────────────────────────────────────────────────────

/// User-configurable session options affecting compile-time validation.
/// Threaded from the client's `with_config()`
/// (Python) via the ASGI `/api/query` "config" body field; see
/// `pylon/config_options.py` for the full registry of known option names/
/// defaults exposed to the frontend. `compile()` (the 2-arg convenience form,
/// used throughout this crate's own tests and schema-time compilation, which
/// never touches a live client-supplied config) always uses `default()`;
/// `compile_with_config()` is the real entry point a live query request uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SessionConfig {
    /// When false (the default), an INSERT that explicitly assigns a value
    /// to a primary-key ("id") property is a compile error. An UPDATE never
    /// allows assigning `id`, regardless of this flag.
    pub allow_user_specified_id: bool,
}

// ── Top-level statement ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IrStmt {
    /// A SELECT — schema-bound (`select Type { .. }`), free (`select {1,2,3}`,
    /// a set/tuple/free-object literal), or a mix via UNION — no distinction
    /// at this level; see `IrSelect::rows`/`IrRowSource`.
    Select(IrSelect),
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
    /// Pointers projected into each element of the `elements` array.
    pub shape: Vec<IrShapePointer>,
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
    Single {
        source_alias: String,
        fk_col: String,
        target: IrSource,
    },
    /// Traverse a multi-link via a junction table.
    Multi {
        source_alias: String,
        junction_alias: String,
        join: IrMultiLinkJoin,
        target: IrSource,
    },
    /// Reverse of a single FK link: find owner rows whose FK column points to the current row.
    BacklinkSingle {
        source_alias: String,
        fk_col: String,
        target: IrSource,
    },
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
    /// The second field carries a tuple-typed property's real member shape
    /// (nominal or structural — see `resolve_property_tuple_shape`) so a bare
    /// `select Type.tuple_property` gets the same rich `ShapeNode::NamedTuple`
    /// a `Type { tuple_property }` shape query already does, instead of
    /// falling back to an opaque `ShapeNode::Scalar`. `None` for anything
    /// that isn't a bare tuple-typed property reference.
    Scalar(IrExpr, Option<TupleCastShape>),
    /// Final step is a link — return the linked objects with the given shape.
    Object {
        alias: String,
        type_name: String,
        shape: Vec<IrShapePointer>,
    },
}

// ── FREE ROW EXPRESSIONS (set literals, tuples, free objects) ───────────────────

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
    /// Inner is a regular schema SELECT; first scalar pointer is the array element.
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
    /// What each output row comes from. Almost always exactly one `Bound`
    /// (a schema object) — more than one entry, or a `Free` entry, only
    /// happens for a literal set/union (`select {1,2,3}`).
    pub rows: Vec<IrRowSource>,
    /// Only ever `Some` when `rows` is a single `Bound` — a free select can
    /// never have a FILTER (enforced at compile time).
    pub filter: Option<IrExpr>,
    pub order_by: Vec<IrSort>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    pub distinct: bool,
    /// When this SELECT wraps a DML statement (`SELECT (INSERT …) { … }`),
    /// the inner DML is stored here and emitted as a CTE.
    /// `None` for plain `SELECT Type { … }` and for any free select.
    /// Only ever `Some` when `rows` is a single `Bound`.
    pub dml_source: Option<Box<IrStmt>>,
    /// True when this SELECT targets an interface type.
    /// The SQL emitter builds a UNION ALL inline instead of hitting the view.
    /// Only meaningful when `rows` is a single `Bound`.
    pub polymorphic: bool,
    /// Concrete implementors of the interface (populated when `polymorphic = true`).
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// Interface column names used in the UNION ALL branches (e.g. `["id", "email"]`).
    pub poly_columns: Vec<String>,
    /// Trailing `FOR UPDATE`/`FOR SHARE`/... row-locking clause. Validated
    /// at compile time (`Compiler::compile_select`) to only ever be `Some`
    /// on a single schema-bound, non-polymorphic, non-DML-wrapped,
    /// non-`DISTINCT` row source — the same shape Postgres itself requires
    /// output rows to map 1:1 to physical table rows for locking to make
    /// sense.
    pub lock: Option<IrLockClause>,
}

#[derive(Debug, Clone)]
pub struct IrLockClause {
    pub strength: IrLockStrength,
    pub wait: IrLockWait,
}

#[derive(Debug, Clone)]
pub enum IrLockStrength {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

#[derive(Debug, Clone)]
pub enum IrLockWait {
    Block,
    NoWait,
    SkipLocked,
}

/// One SELECT output row's source — either a real schema object (with a
/// FROM clause and projected column shape) or a free literal expression
/// (set/tuple/free-object/scalar). Replaces the former `IrSelect`/
/// `IrFreeSelect` type-level split, which caused real bugs (the same logic
/// reimplemented twice, independently, and drifting) before this merge.
#[derive(Debug, Clone)]
pub enum IrRowSource {
    Bound {
        source: IrSource,
        shape: Vec<IrShapePointer>,
    },
    Free(IrFreeExpr),
}

impl IrSelect {
    /// The overwhelming-majority shape: one schema-bound row, no DML, no
    /// polymorphism — what most correlated-subquery/EXISTS builders want.
    pub fn schema_bound(source: IrSource, shape: Vec<IrShapePointer>, filter: Option<IrExpr>) -> Self {
        IrSelect {
            rows: vec![IrRowSource::Bound { source, shape }],
            filter,
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        }
    }
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

/// A set-valued scalar computed pointer from cross-scope `TypeIs`.
/// Emits `COALESCE(array_agg(ROW(bool_expr)::record), ARRAY[]::record[]) FROM source`.
#[derive(Debug, Clone)]
pub struct IrScalarSetPointer {
    pub alias: String,
    pub source: IrSource,
    pub poly_implementors: Vec<IrPolyImplementor>,
    pub poly_columns: Vec<String>,
    pub bool_expr: IrExpr,
}

#[derive(Debug, Clone)]
pub enum IrShapePointer {
    Scalar(IrScalarPointer),
    SingleLink(IrSingleLinkPointer),
    MultiLink(IrMultiLinkPointer),
    Computed(IrComputedPointer),
    ScalarSet(IrScalarSetPointer),
}

/// A property column included in the output shape.
#[derive(Debug, Clone)]
pub struct IrScalarPointer {
    /// The output key (what the user wrote, e.g. `name`).
    pub alias: String,
    /// The PostgreSQL column name on the source table.
    pub column: String,
    /// The PostgreSQL type string, e.g. `text`, `int8`.
    pub pg_type: String,
    /// `Some` when this property is a named-tuple type (nominal, via the
    /// `__nt__:` `pg_type` marker, or structural, via `pylon.Tuple[...]`)
    /// whose member shape is statically known — see `TupleCastShape`.
    pub tuple_shape: Option<TupleCastShape>,
    /// Source byte offset of the originating `ast::ShapeElement`, if this
    /// pointer came from one written directly in the query (`analyze`'s
    /// marker placement — see `analyze.rs`); `None` for a pointer synthesized
    /// by the compiler itself (splat expansion, implicit `{ id }`, etc.).
    pub marker_offset: Option<usize>,
}

/// A single-valued link included in the output shape. Emitted as a
/// correlated scalar subquery — either a plain FK-column correlation, or,
/// for a junction-backed single link, the same join shape a multi-link
/// uses, just implicitly capped to at most one row per source.
#[derive(Debug, Clone)]
pub struct IrSingleLinkPointer {
    pub alias: String,
    pub correlation: IrSingleLinkCorrelation,
    /// Nested SELECT producing the linked object.
    pub subquery: IrSelect,
    /// Extra scalar columns from a junction through type (`@prop` syntax) —
    /// always empty for `IrSingleLinkCorrelation::Fk`, since a plain FK
    /// column has no junction row to read properties from.
    pub link_properties: Vec<IrLinkProp>,
    /// See `IrScalarPointer::marker_offset`.
    pub marker_offset: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum IrSingleLinkCorrelation {
    /// `parent.<fk_column> = target.<target_pk>`.
    Fk {
        /// FK column on the source table, e.g. `category_id`.
        fk_column: String,
        /// PK column on the target table, e.g. `id`.
        target_pk: String,
    },
    /// Junction-backed — reuses `IrMultiLinkJoin`, the same join a
    /// multi-link's own correlated subquery uses.
    Junction {
        join: IrMultiLinkJoin,
        /// PK column on the target table, e.g. `id`.
        target_pk: String,
    },
}

/// A multi-valued link included in the output shape.
/// Emitted as a correlated subquery using `array_agg(ROW(...)::record)`.
#[derive(Debug, Clone)]
pub struct IrMultiLinkPointer {
    pub alias: String,
    /// Join table or FK column identifying the source side.
    pub join: IrMultiLinkJoin,
    /// Nested SELECT producing the linked objects.
    pub subquery: IrSelect,
    /// Extra scalar columns from a junction table (`@prop` syntax).
    pub link_properties: Vec<IrLinkProp>,
    /// See `IrScalarPointer::marker_offset`.
    pub marker_offset: Option<usize>,
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
    /// Reverse of a single (FK) link — the owner type's rows are correlated
    /// directly by their FK column, no junction table involved. Still
    /// many-valued (several owner rows can point at the same current row),
    /// so this shares `IrMultiLinkPointer`'s array_agg-based emission
    /// rather than the singular `IrShapePointer::Link` representation.
    BacklinkFk {
        /// FK column on the owner (sub-select) table, e.g. `org_id`.
        fk_col: String,
    },
    /// Reverse of a multi-link — same junction table a forward `Standard`/
    /// `Through` multi-link would use, but with the owner/current column
    /// roles swapped: the sub-select's own rows correlate via `owner_col`,
    /// the current (outer) row correlates via `current_col`.
    BacklinkJunction {
        junction_table: String,
        module: String,
        /// Junction column that references the owner (sub-select) type's id.
        owner_col: String,
        /// Junction column that references the current (outer) row's id.
        current_col: String,
    },
}

/// A computed pointer: an expression aliased to a name.
#[derive(Debug, Clone)]
pub struct IrComputedPointer {
    pub alias: String,
    pub expr: IrExpr,
    /// See `IrScalarPointer::marker_offset`.
    pub marker_offset: Option<usize>,
}

// ── Vector index enqueue ────────────────────────────────────────────────────────

/// Identifies one vector index that needs an outbox row written when a
/// mutation touches its source pointers.
#[derive(Debug, Clone)]
pub struct VectorEnqueueInfo {
    /// Schema-qualified type name, e.g. `"default::Product"`.
    pub type_name: String,
    /// `None` = default index, `Some(name)` = named index.
    pub index_name: Option<String>,
}

/// Identifies one OpenSearch- or Meilisearch-backed SearchIndex that needs
/// an outbox row written (`Postgres`-backed indexes use a synchronously
/// trigger-maintained tsvector column instead — never enqueued here).
#[derive(Debug, Clone)]
pub struct SearchEnqueueInfo {
    pub type_name: String,
    pub index_name: Option<String>,
    /// `"index"` for insert/update, `"delete"` for delete.
    pub operation: &'static str,
    /// `OpenSearch` or `Meilisearch` — determines the outbox row's
    /// `_pylon."IndexKind"` value.
    pub backend: crate::schema::SearchBackend,
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
    pub returning: Vec<IrShapePointer>,
    /// Vector indexes on this type that need outbox rows written.
    pub enqueue_vector: Vec<VectorEnqueueInfo>,
    /// OpenSearch-backed SearchIndexes that need outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
    /// `tags := expr` / `tags += expr` in the insert shape — populates the
    /// junction table for a multi-link at creation time. Unlike IrUpdate,
    /// there's no clear/remove list: a brand-new row has no prior junction
    /// rows to clear or remove from.
    pub multi_link_appends: Vec<IrMultiLinkMutation>,
    /// DML (INSERT/UPDATE/DELETE) discovered nested inside a link-assignment
    /// value — `author := (select (insert Person {...}) { id })` — hoisted
    /// into its own `WITH` CTE ahead of this insert, since Postgres has no
    /// way to run a nested INSERT inside a VALUES list otherwise. When
    /// non-empty, the emitter switches this insert's own row source from
    /// `VALUES (...)` to `SELECT ... FROM <cte>, ...`, and any assignment
    /// value referencing one of these CTEs is a plain `ColumnRef` to its
    /// `id` column. See `Compiler::compile_link_subquery`.
    pub nested_ctes: Vec<IrCteDef>,
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
    pub returning: Vec<IrShapePointer>,
    /// Vector indexes whose source pointers are touched by this update.
    pub enqueue_vector: Vec<VectorEnqueueInfo>,
    /// OpenSearch-backed SearchIndexes that need outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
    /// Populated when updating an interface type; one entry per concrete implementor.
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// The interface's own physical columns (properties + `{link}_id`) — the
    /// only columns every implementor table is guaranteed to share, so a
    /// per-implementor UNION ALL fan-out (see poly_implementors) can only
    /// ever RETURNING this common subset, never `*`. Empty unless
    /// poly_implementors is also non-empty.
    pub poly_columns: Vec<String>,
    /// `friends := {}` — DELETE all junction rows for this object.
    pub multi_link_clears: Vec<IrMultiLinkClear>,
    /// `friends := expr` — clear + insert (both lists share the same index).
    pub multi_link_replaces: Vec<IrMultiLinkMutation>,
    /// `friends += expr` — INSERT junction rows.
    pub multi_link_appends: Vec<IrMultiLinkMutation>,
    /// `friends -= expr` — DELETE specific junction rows.
    pub multi_link_removals: Vec<IrMultiLinkMutation>,
    /// Same as `IrInsert::nested_ctes` — hoisted nested DML from a link
    /// assignment value in this update's own SET shape. Only supported
    /// (see `Compiler::compile_update`) when this update has no multi-link
    /// mutation, no interface fan-out, and nothing to enqueue; combining
    /// those with a nested DML value is a compile error for now rather than
    /// attempting to emit anything, until that combination is implemented.
    pub nested_ctes: Vec<IrCteDef>,
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
    /// The set of target objects (plus any `@prop := expr` link-property
    /// assignments attached to them).
    pub values: IrMultiLinkValues,
    /// True for a junction-backed single link — the junction table's own
    /// unique constraint is `PRIMARY KEY (source)` alone (not `(source,
    /// target)`), so an `ON CONFLICT` target naming both columns would
    /// reference a constraint that doesn't exist.
    pub single: bool,
}

/// The set of target object ids for a multi-link mutation, plus any
/// link-property assignments (`@prop := expr`) attached directly to this
/// source — e.g. `(select Tag filter .id = $a) { @weight := <float64>$w }`.
/// A `Union` node's own `link_props` is always empty; each side carries its
/// own instead (different targets in one `+=` can have different property
/// values — see `emit_ml_append_cte` in sql/mod.rs for how the SQL layer
/// reconciles a heterogeneous property-name set across union branches).
#[derive(Debug, Clone)]
pub struct IrMultiLinkValues {
    pub source: IrMultiLinkValueSource,
    pub link_props: Vec<(String, IrExpr)>,
}

/// How to obtain the target object IDs for a multi-link mutation.
#[derive(Debug, Clone)]
pub enum IrMultiLinkValueSource {
    /// Reference to a named CTE: `FROM "cte_name"`.
    CteRef(String),
    /// A regular schema SELECT (use source table + filter to get ids).
    Select(Box<IrSelect>),
    /// A path-traversal SELECT (root + joins, final result is the id).
    PathSelect(Box<IrPathSelect>),
    /// `a union b` — combine two target sets (e.g. distinct-typed adds, or an
    /// existing-select add alongside a same-batch forward-referenced insert).
    Union(Box<IrMultiLinkValues>, Box<IrMultiLinkValues>),
}

// ── DELETE ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrDelete {
    pub target: IrSource,
    pub filter: Option<IrExpr>,
    pub returning: Vec<IrShapePointer>,
    /// Populated when deleting from an interface type; one entry per concrete implementor.
    pub poly_implementors: Vec<IrPolyImplementor>,
    /// The interface's own physical columns — see IrUpdate::poly_columns.
    pub poly_columns: Vec<String>,
    /// OpenSearch-backed SearchIndexes that need delete outbox rows written.
    pub enqueue_search: Vec<SearchEnqueueInfo>,
}

// ── Expressions ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IrExpr {
    /// A resolved column reference, e.g. `t0.name`.
    ColumnRef {
        alias: String,
        column: String,
        pg_type: String,
    },
    /// A positional query parameter `$N` (0-based index internally).
    Param {
        index: usize,
    },
    Literal(IrLiteral),
    BinOp(Box<IrBinOp>),
    UnaryOp(Box<IrUnaryOp>),
    FunctionCall(IrFunctionCall),
    TypeCast(Box<IrTypeCast>),
    IfElse(Box<IrIfElse>),
    /// A scalar subquery (used for computed pointers that are themselves selects).
    Subquery(Box<IrSelect>),
    /// An array literal: `[1, 2, 3]`.
    Array(Vec<IrExpr>),
    /// The empty set `{}` used as an assignment value — emits SQL `NULL`.
    Null,
    /// An aggregate function applied to an inline set literal `fn({e1, e2, ...})`.
    /// Emits: `(SELECT fn_name(v) FROM (SELECT e1 UNION ALL ...) AS _set(v))`
    AggOverSet {
        fn_name: String,
        schema: Option<String>,
        elems: Vec<IrExpr>,
    },
    /// An aggregate function applied to a full SELECT query: `count(Person)` or `count((select Person))`.
    /// Emits: `(SELECT fn_name(*) FROM (inner) _agg)`
    AggOverQuery {
        fn_name: String,
        inner: Box<IrSelect>,
    },
    /// A reference to a named CTE used in expression context.
    /// `scalar = true`  → emits `(SELECT "result" FROM "cte_name")`
    /// `scalar = false` → emits `(SELECT "id"     FROM "cte_name")`
    CteRef {
        name: String,
        scalar: bool,
    },
    /// A single-field access on a WITH-bound free object: `with x := { a
    /// := 1 } select x.a`. The CTE body exposes each free-object field as
    /// its own named column (alongside the whole-object `result` column
    /// `CtePassthrough`/`CteRef` use) so this can reference it directly
    /// rather than reconstructing/decoding the opaque `result` composite.
    /// Emits `(SELECT "field" FROM "name")`.
    CteFieldRef {
        name: String,
        field: String,
    },
    /// Reference to the current for-loop iterator variable.
    /// Emits `"_for_{name}"."v"`.
    ForVar {
        name: String,
    },
    /// `ARRAY(SELECT scalar FROM source [JOINs] [WHERE filter])`.
    /// Used as the array argument to `_pylon.assert_single/exists/distinct`.
    ArrayFromSelect(Box<IrArraySource>),
    /// An enum member access: `default::Gender.Female` → `'Female'::"default"."Gender"`.
    EnumLiteral {
        pg_type: String,
        variant: String,
    },
    /// Named tuple construction: `(x := 1.0, y := 2.0)` → `jsonb_build_object('x', 1.0, 'y', 2.0)`.
    /// `is_free_object` is true when this actually came from `{ x := 1.0 }`
    /// (curly-brace shape syntax, no subject) rather than `(x := 1.0)`
    /// (paren tuple syntax) — same jsonb encoding/decoding either way, but
    /// the frontend needs to know which one it was to render an expandable
    /// `Object {x: 1.0}` vs a `(x := 1.0)` literal display correctly (see
    /// ShapeNode::NamedTuple's own is_free_object).
    NamedTuple {
        fields: Vec<(String, IrExpr)>,
        is_free_object: bool,
    },
    /// Positional tuple construction: `(1, 'x')` → `jsonb_build_array(1, 'x')`.
    Tuple(Vec<IrExpr>),
    /// Session global: emits `$N::pg_type` directly. The parameter slot carries the `__global__` prefix.
    GlobalParam {
        index: usize,
        pg_type: String,
    },
    /// Computed global reference: emits `(SELECT "value" FROM "cte_name")`.
    GlobalRef {
        cte_name: String,
    },
    /// Index access `expr[i]`: `substr(expr, i+1, 1)` for strings/bytes, `(expr)[i+1]` for arrays.
    Subscript {
        expr: Box<IrExpr>,
        index: Box<IrExpr>,
        is_array: bool,
    },
    /// Named tuple / jsonb field access: `(expr)->'field'` (returns jsonb).
    JsonbField {
        expr: Box<IrExpr>,
        field: String,
    },
    /// Positional tuple index into a jsonb array: `(expr)->index` (returns jsonb).
    /// Runtime fallback for `.N` tuple indexing when `expr` isn't a literal
    /// tuple constant-foldable at compile time (e.g. a $param or cast result).
    JsonbIndex {
        expr: Box<IrExpr>,
        index: usize,
    },
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
    FnParam {
        name: String,
        pg_type: String,
    },
    /// Verbatim SQL text, emitted parenthesized exactly as given. Never
    /// produced by ordinary PyQL compilation — only used to substitute an
    /// INSERT rewrite's self-reference (`.name`) to a property that has no
    /// explicit assignment in this statement with that property's own
    /// `default_sql`, the same value Postgres's column DEFAULT would have
    /// produced. A plain `INSERT ... VALUES (...)` has no FROM-clause for a
    /// real `ColumnRef` to resolve against (confirmed live — "missing
    /// FROM-clause entry"), unlike UPDATE's SET clause, which can reference
    /// the table's own alias validly, so this substitution is INSERT-only.
    RawSql(String),
}

// ── Vector search ─────────────────────────────────────────────────────────────

/// `select vector::search(Type, $vec) { object { … }, distance }`
///
/// Emits a single SELECT from the type's table that computes the distance
/// inline and returns a virtual `{ object, distance }` shape.
///
/// Text overload: `vector::search(Type, query := $text)` — the Python layer
/// embeds the text first and injects the resulting vector as `__deferred_vec__`.
#[derive(Debug, Clone)]
pub struct IrVectorSearch {
    /// The searched type as an `IrSource` (table + alias).
    pub source: IrSource,
    /// pgvector column name, e.g. `__vector__`.
    pub vector_col: String,
    /// pgvector distance operator: `<=>`, `<->`, or `<#>`.
    pub distance_op: &'static str,
    /// The query vector expression (e.g. `$1::vector`).
    /// For the text overload this is the `__deferred_vec__` param cast to vector.
    pub query_expr: IrExpr,
    /// Pointers to include in the `object` sub-tuple (from the `object { … }` shape).
    /// Empty means no explicit shape was given; the SQL emitter uses all properties.
    pub object_shape: Vec<IrShapePointer>,
    pub filter: Option<IrExpr>,
    /// `None` = no ORDER BY; `Some(dir)` = ORDER BY distance in that direction.
    /// Only distance ordering is supported for v1.
    pub order_by_distance: Option<IrSortDir>,
    pub offset: Option<IrExpr>,
    pub limit: Option<IrExpr>,
    // ── text overload (inference) fields ──────────────────────────────────────
    /// Name of the user's `query :=` param; empty string when an inline literal.
    pub inference_query_param_name: Option<String>,
    /// Inline literal query text when `query := 'some text'`.
    pub inference_query_literal: Option<String>,
    /// Embedding model identifier from the `VectorIndexDescriptor`, e.g. `"mistral-embed"`.
    pub inference_model: Option<String>,
    /// Qualified type name for provider lookup, e.g. `"default::Product"`.
    pub inference_type_name: Option<String>,
    /// Vector index name for provider lookup (`None` = default index).
    pub inference_index_name: Option<Option<String>>,
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
    /// Pointers to include in the `object` sub-tuple.
    pub object_shape: Vec<IrShapePointer>,
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
    pub shape: Vec<IrShapePointer>,
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
    /// `Some` when the cast target is a named-tuple type (nominal or
    /// structural) whose member shape is statically known — drives building
    /// a rich `ShapeNode::NamedTuple` with real per-member decode instead of
    /// an opaque jsonb blob.
    pub tuple_shape: Option<TupleCastShape>,
}

#[derive(Debug, Clone)]
pub struct TupleCastShape {
    /// `Some` for a nominal `@pylon.named_tuple` cast target (hydrates to
    /// the registered dataclass); `None` for a structural `tuple<...>`.
    pub type_name: Option<String>,
    pub members: Vec<crate::query::JsonMember>,
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
pub enum IrSortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub enum IrNulls {
    First,
    Last,
}

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
    /// Boxed: a computed global carries a whole compiled sub-select and is
    /// ~8x the size of a session global, and these live in a `Vec` where the
    /// session variant is the common case.
    Computed(Box<IrComputedGlobalCte>),
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
    /// True when this is a function body that reads a session global, or
    /// forwards the globals argument to a callee that does — i.e. when the
    /// function needs `GLOBALS_ARG` in its signature.
    pub uses_globals_arg: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use super::{IrFreeExpr, IrLiteral};
    use crate::parse;
    use crate::schema::{
        ChannelDescriptor, ChannelPayload, ComputedDescriptor, GlobalDescriptor, LinkDescriptor, MultiLinkDescriptor,
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
                            default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: true,
                            is_pk: true,
                            is_readonly: true,
                            rewrites: vec![],
                            tuple_members: None,
                            column_type: None,
                        },
                        PropertyDescriptor {
                            name: "name".into(),
                            pg_type: "text".into(),
                            nullable: false,
                            default_sql: None,
                            default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                            tuple_members: None,
                            column_type: None,
                        },
                        PropertyDescriptor {
                            name: "age".into(),
                            pg_type: "int8".into(),
                            nullable: true,
                            default_sql: None,
                            default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                            tuple_members: None,
                            column_type: None,
                        },
                    ],
                    links: vec![LinkDescriptor {
                        name: "company".into(),
                        target: "default::Company".into(),
                        nullable: true,
                        through: None,
                        description: None,
                        default_pyql: None,
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
                        default_pyql: None,
                        on_delete: vec![],
                    }],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    partition: None,
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                    signals: vec![],
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
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                        tuple_members: None,
                        column_type: None,
                    }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    partition: None,
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                    signals: vec![],
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
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                        tuple_members: None,
                        column_type: None,
                    }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    partition: None,
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                    signals: vec![],
                },
            ],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        }
    }

    fn compile(query: &str) -> IrOutput {
        let schema = make_schema();
        let ast = parse::parse(query).expect("parse failed");
        super::compile(&ast, &schema).expect("IR compile failed")
    }

    /// Extract the single schema-bound row's `(source, shape)` from a
    /// `SELECT` — panics if the select isn't schema-bound (i.e. is a free
    /// select), which is what most tests expect.
    fn bound(sel: &IrSelect) -> (&IrSource, &[IrShapePointer]) {
        match sel.rows.as_slice() {
            [IrRowSource::Bound { source, shape }] => (source, shape),
            _ => panic!("expected a single schema-bound row"),
        }
    }

    /// Extract the free-row items from a `SELECT` — panics if any row is
    /// schema-bound, which is what free-select tests expect.
    fn free_items(sel: &IrSelect) -> Vec<&IrFreeExpr> {
        sel.rows
            .iter()
            .map(|r| match r {
                IrRowSource::Free(item) => item,
                IrRowSource::Bound { .. } => panic!("expected a free row"),
            })
            .collect()
    }

    #[test]
    fn test_select_resolves_source() {
        let ir = compile("SELECT Person { name, age }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let (source, shape) = bound(&sel);
        assert_eq!(source.table, "person");
        assert_eq!(source.type_name, "default::Person");
        assert_eq!(shape.len(), 2);
        assert!(matches!(shape[0], IrShapePointer::Scalar(_)));
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
        let (_, shape) = bound(&sel);
        assert_eq!(shape.len(), 2);
        let IrShapePointer::SingleLink(link) = &shape[1] else {
            panic!("expected SingleLink")
        };
        assert_eq!(link.alias, "company");
        let IrSingleLinkCorrelation::Fk { fk_column, .. } = &link.correlation else {
            panic!("expected Fk correlation")
        };
        assert_eq!(fk_column, "company_id");
        assert_eq!(bound(&link.subquery).0.table, "company");
    }

    #[test]
    fn test_select_multi_link() {
        let ir = compile("SELECT Person { name, posts { title } }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let (_, shape) = bound(&sel);
        let IrShapePointer::MultiLink(ml) = &shape[1] else {
            panic!("expected MultiLink")
        };
        assert_eq!(ml.alias, "posts");
        assert_eq!(bound(&ml.subquery).0.table, "post");
        let IrMultiLinkJoin::Standard { junction_table, .. } = &ml.join else {
            panic!()
        };
        assert_eq!(junction_table, "person.posts");
    }

    #[test]
    fn test_select_no_shape_returns_id_only() {
        let ir = compile("SELECT Person");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        // Bare SELECT Type returns only { id }.
        let (_, shape) = bound(&sel);
        assert_eq!(shape.len(), 1);
        let IrShapePointer::Scalar(f) = &shape[0] else { panic!() };
        assert_eq!(f.alias, "id");
    }

    #[test]
    fn test_free_select_set_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT {1, 2, 3}").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        assert_eq!(items.len(), 3);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Literal(IrLiteral::Int(1)))
        ));
    }

    #[test]
    fn test_free_select_free_object() {
        let schema = make_schema();
        let ast = parse::parse("SELECT { foo := 'bar', n := 42 }").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        assert_eq!(items.len(), 1);
        let IrFreeExpr::FreeObject(fields) = &items[0] else {
            panic!("expected FreeObject")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "foo");
        assert_eq!(fields[1].0, "n");
    }

    #[test]
    fn test_free_select_tuple() {
        let schema = make_schema();
        let ast = parse::parse("SELECT (1, 'hello')").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], IrFreeExpr::Tuple(_)));
    }

    #[test]
    fn test_free_select_scalar_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT 42").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Literal(IrLiteral::Int(42)))
        ));
    }

    #[test]
    fn test_free_select_function_call() {
        let schema = make_schema();
        let ast = parse::parse("SELECT str_lower('HELLO')").unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        assert!(matches!(items[0], IrFreeExpr::Scalar(IrExpr::FunctionCall(_))));
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
        assert!(
            msg.contains("std::uuid") && msg.contains("std::str"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn test_type_error_str_eq_int() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = 42").unwrap();
        let err = super::compile(&ast, &schema).err().expect("expected type error");
        let msg = err.to_string();
        assert!(
            msg.contains("std::str") && msg.contains("std::int64"),
            "unexpected: {msg}"
        );
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
    fn test_nested_dml_link_value_combines_with_multilink_mutation_in_the_same_update() {
        // A link value sourced from a hoisted nested INSERT/UPDATE/DELETE
        // (`company := (select (insert Company {...}) { id })`) and a
        // multi-link mutation (`posts +=`) in the same UPDATE both compile
        // — `IrUpdate` carries both `nested_ctes` and `multi_link_appends`,
        // and `emit_update_stmt`'s junction-CTE branch threads the former
        // through into the `_ids` UPDATE's own FROM clause. See the SQL-shape
        // assertion in `sql::tests::
        // test_update_link_value_from_nested_insert_combines_with_multilink_mutation`
        // for the actual emitted structure.
        let schema = make_schema();
        let ast = parse::parse(
            "UPDATE Person FILTER .id = $id SET { \
                 company := (select (insert Company { name := 'Acme' }) { id }), \
                 posts += (SELECT Post FILTER .title = $t) \
             }",
        )
        .unwrap();
        let ir = super::compile(&ast, &schema).unwrap();
        let IrStmt::Update(upd) = ir.stmt else {
            panic!("expected Update")
        };
        assert_eq!(upd.nested_ctes.len(), 1);
        assert_eq!(upd.multi_link_appends.len(), 1);
    }

    #[test]
    fn test_unknown_pointer_error() {
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
        // Add a computed pointer to Person
        schema.types[0].computed.push(ComputedDescriptor {
            name: "upper_name".into(),
            expression: "str_upper(.name)".into(),
            return_type: Some("text".into()),
        });
        schema
    }

    #[test]
    fn test_computed_pointer_in_shape() {
        let schema = make_schema_with_computed();
        let ast = parse::parse("SELECT Person { upper_name }").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        // upper_name should compile to a Computed shape pointer
        let (_, shape) = bound(&sel);
        assert!(
            shape
                .iter()
                .any(|f| matches!(f, IrShapePointer::Computed(c) if c.alias == "upper_name"))
        );
    }

    #[test]
    fn test_computed_pointer_in_expression_context() {
        let schema = make_schema_with_computed();
        let ast = parse::parse("SELECT Person { x := str_lower(.upper_name) }").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let (_, shape) = bound(&sel);
        assert!(
            shape
                .iter()
                .any(|f| matches!(f, IrShapePointer::Computed(c) if c.alias == "x"))
        );
    }

    #[test]
    fn test_count_over_multilink_in_computed_shape_element() {
        // Regression: `count(.posts)` inside a computed shape element
        // previously failed with "object type 'default::Person' has no link
        // or property 'posts'" — compile_path only checked scalar
        // properties/single-links, never multilinks.
        let ir = compile("SELECT Person { post_count := count(.posts) }");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let (_, shape) = bound(&sel);
        let computed = shape
            .iter()
            .find_map(|f| match f {
                IrShapePointer::Computed(c) if c.alias == "post_count" => Some(c),
                _ => None,
            })
            .expect("expected post_count computed pointer");
        assert!(matches!(computed.expr, IrExpr::AggOverQuery { .. }));
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
            scalar_type: "std::uuid".into(),
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
    fn test_session_global_pg_type_matches_pyql_type_name() {
        // Regression: `GlobalDescriptor.scalar_type` is a PyQL-style type
        // name built by the Python walker's `_pyql_type_name` (e.g.
        // "std::uuid"), never a bare class name like "UUID" —
        // `resolve_global_pg_type` used to match against the latter and
        // silently fall back to "text" for every builtin-typed session
        // global, which only surfaced once something actually compiled a
        // query/expression comparing the global against a real uuid column.
        let mut schema = make_schema();
        schema.globals.push(GlobalDescriptor {
            name: "viewer_id".into(),
            module: "default".into(),
            scalar_type: "std::uuid".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });
        let ast = parse::parse("SELECT Person FILTER .id = global viewer_id").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrGlobalCte::Session(session) = &ir.global_ctes[0] else {
            panic!("expected a session global CTE");
        };
        assert_eq!(session.pg_type, "uuid");
    }

    #[test]
    fn test_computed_global_field_access_compiles_as_path_select() {
        // Regression: `global name.field` previously wrapped the global's
        // opaque CTE reference in a jsonb `->` extraction (only valid for
        // tuple-typed values), producing "operator does not exist: uuid ->
        // unknown" for an object-typed computed global.
        let mut schema = make_schema();
        schema.globals.push(GlobalDescriptor {
            name: "current_user".into(),
            module: "default".into(),
            scalar_type: "Person".into(),
            required: false,
            default_expr: None,
            computed_expr: Some("select default::Person filter .id = <uuid>$session_user_id".into()),
        });
        let ast = parse::parse("SELECT global current_user.id").unwrap();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::PathSelect(sel) = ir.stmt else {
            panic!("expected a path select, not a free select")
        };
        assert_eq!(sel.root.type_name, "default::Person");
    }

    #[test]
    fn test_subquery_field_access_compiles_as_path_select() {
        // Regression: `(select Type filter ...).field` hit the generic free-
        // expression fallback ("expression is not valid in free SELECT
        // context") because bare subqueries aren't valid free expressions —
        // it should splice `.field` onto the inner select as a path step.
        let ast = parse::parse("SELECT (SELECT default::Person FILTER .age > 20).name").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::PathSelect(sel) = ir.stmt else {
            panic!("expected a path select, not a free select")
        };
        assert_eq!(sel.root.type_name, "default::Person");
    }

    #[test]
    fn test_string_index_compiles() {
        let ast = parse::parse("SELECT 'hello'[1]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let items = free_items(&sel);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Subscript { is_array: false, .. })
        ));
    }

    #[test]
    fn test_array_index_compiles() {
        let ast = parse::parse("SELECT [1, 2, 3][0]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let items = free_items(&sel);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Subscript { is_array: true, .. })
        ));
    }

    #[test]
    fn test_string_slice_compiles() {
        let ast = parse::parse("SELECT 'hello'[1:3]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let items = free_items(&sel);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Slice { is_array: false, .. })
        ));
    }

    #[test]
    fn test_array_slice_compiles() {
        let ast = parse::parse("SELECT [1, 2, 3][0:2]").unwrap();
        let schema = make_schema();
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else { panic!() };
        let items = free_items(&sel);
        assert!(matches!(
            items[0],
            IrFreeExpr::Scalar(IrExpr::Slice { is_array: true, .. })
        ));
    }

    fn make_schema_with_alias() -> SchemaDescriptor {
        use crate::schema::AliasDescriptor;
        let mut schema = make_schema();
        schema.aliases.push(AliasDescriptor {
            name: "ActivePersons".into(),
            module: "default".into(),
            expr: "select Person filter .age >= 18".into(),
        });
        schema
    }

    fn make_schema_with_sequence() -> crate::schema::SchemaDescriptor {
        use crate::schema::ScalarDescriptor;
        let mut schema = make_schema();
        schema.scalars.push(ScalarDescriptor {
            name: "OrderNumber".into(),
            module: "default".into(),
            base: "Sequence".into(),
            pg_type: "int8".into(),
            check_constraints: vec![],
            is_sequence: true,
        });
        schema
    }

    fn make_schema_with_channels() -> SchemaDescriptor {
        let mut schema = make_schema();
        schema.channels.push(ChannelDescriptor {
            name: "Pings".into(),
            module: "default".into(),
            wire_name: "default__pings".into(),
            payload: ChannelPayload::Scalar("text".into()),
            description: None,
        });
        schema.channels.push(ChannelDescriptor {
            name: "SearchReady".into(),
            module: "default".into(),
            wire_name: "default__search_ready".into(),
            payload: ChannelPayload::Object(vec![
                ("doc_id".into(), "uuid".into()),
                ("score".into(), "float8".into()),
            ]),
            description: None,
        });
        schema.channels.push(ChannelDescriptor {
            name: "PersonUpdates".into(),
            module: "default".into(),
            wire_name: "default__person_updates".into(),
            payload: ChannelPayload::Type("default::Person".into()),
            description: None,
        });
        schema
    }

    fn compile_notify_expr(query: &str) -> String {
        let schema = make_schema_with_channels();
        let ast = parse::parse(query).expect("parse failed");
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        let IrFreeExpr::Scalar(expr) = items[0] else {
            panic!("expected scalar")
        };
        crate::sql::emit_expr(expr)
    }

    fn notify_compile_err(query: &str) -> String {
        let schema = make_schema_with_channels();
        let ast = parse::parse(query).expect("parse failed");
        format!(
            "{}",
            super::compile(&ast, &schema).err().expect("expected a compile error")
        )
    }

    #[test]
    fn test_notify_scalar_channel_emits_pg_notify() {
        let sql = compile_notify_expr("SELECT notify(Pings, 'hello')");
        assert_eq!(sql, "pg_notify('default__pings', ('hello')::text)", "got: {sql}");
    }

    #[test]
    fn test_notify_rejects_unknown_channel() {
        let err = notify_compile_err("SELECT notify(NoSuchChannel, 'hi')");
        assert!(err.contains("not a known Channel"), "got: {err}");
    }

    #[test]
    fn test_notify_object_channel_emits_jsonb_build_object() {
        let sql = compile_notify_expr(
            "SELECT notify(SearchReady, { doc_id := <uuid>'3fa85f64-5717-4562-b3fc-2c963f66afa6', score := 0.5 })",
        );
        assert_eq!(
            sql,
            "pg_notify('default__search_ready', (jsonb_build_object('doc_id', ('3fa85f64-5717-4562-b3fc-2c963f66afa6')::uuid, 'score', (0.5::float8)))::text)",
            "got: {sql}"
        );
    }

    #[test]
    fn test_notify_object_channel_rejects_wrong_fields() {
        let err = notify_compile_err("SELECT notify(SearchReady, { doc_id := 'x' })");
        assert!(
            err.contains("payload fields") && err.contains("don't match"),
            "got: {err}"
        );
    }

    #[test]
    fn test_notify_object_channel_rejects_non_shape_payload() {
        let err = notify_compile_err("SELECT notify(SearchReady, 'not an object')");
        assert!(err.contains("free object literal"), "got: {err}");
    }

    #[test]
    fn test_notify_type_channel_rejects_arbitrary_payload() {
        let err = notify_compile_err("SELECT notify(PersonUpdates, 'not an anchor')");
        assert!(err.contains("must name an object of that type"), "got: {err}");
    }

    #[test]
    fn notify_composes_with_a_with_block_binding() {
        // The shape a notify-after-write actually wants: the mutation and
        // the notification in one statement, in one transaction. This used
        // to be a compile error — `notify` on an object channel only
        // accepted the `__new__`/`__old__` anchors a trigger binds.
        let sql = compile_notify_expr(
            "WITH updated := (UPDATE Person FILTER .id = <uuid>$id SET { name := 'x' }) \
             SELECT notify(PersonUpdates, updated)",
        );
        assert!(sql.contains("pg_notify"), "got: {sql}");
        // The payload is the bound object's id, read out of its CTE.
        assert!(sql.contains("\"id\""), "payload should be the CTE's id: {sql}");
        assert!(sql.contains("updated"), "should reference the with-block CTE: {sql}");
    }

    #[test]
    fn notify_rejects_a_with_block_binding_of_the_wrong_type() {
        let err = notify_compile_err("WITH other := (SELECT Company) SELECT notify(PersonUpdates, other)");
        assert!(err.contains("expects a payload of type"), "got: {err}");
    }

    #[test]
    fn test_notify_type_channel_via_trigger_new_anchor() {
        let schema = make_schema_with_channels();
        let ir_out = super::compile_trigger_handler(
            "select notify(PersonUpdates, __new__)",
            "Person",
            1, // On::Insert — binds __new__ only (on_mask & 4 == 0), no __old__
            &schema,
        )
        .expect("trigger handler compile failed");
        let IrStmt::Select(sel) = ir_out.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        let IrFreeExpr::Scalar(expr) = items[0] else {
            panic!("expected scalar")
        };
        let sql = crate::sql::emit_expr(expr);
        assert_eq!(
            sql, "pg_notify('default__person_updates', (NEW.\"id\")::text)",
            "got: {sql}"
        );
    }

    #[test]
    fn test_notify_scalar_channel_via_trigger_new_property_access() {
        // A bare `select notify(...)` trigger handler has no type at its own
        // root, so it compiles as a *free* select — `__new__.name` only
        // resolves at all because `compile_notify` falls back to a bound
        // anchor as a stand-in ctx when the ambient one is None (confirmed
        // live via live_execution_notify.rs before this fallback existed:
        // it failed with "expression is not valid in free SELECT context").
        let schema = make_schema_with_channels();
        let ir_out = super::compile_trigger_handler(
            "select notify(Pings, __new__.name)",
            "Person",
            1, // On::Insert
            &schema,
        )
        .expect("trigger handler compile failed");
        let IrStmt::Select(sel) = ir_out.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        let IrFreeExpr::Scalar(expr) = items[0] else {
            panic!("expected scalar")
        };
        let sql = crate::sql::emit_expr(expr);
        assert_eq!(sql, "pg_notify('default__pings', (NEW.\"name\")::text)", "got: {sql}");
    }

    #[test]
    fn test_notify_type_channel_rejects_bare_reference_outside_trigger() {
        // __new__ has no binding at all in a plain (non-trigger) compile.
        let err = notify_compile_err("SELECT notify(PersonUpdates, __new__)");
        assert!(err.contains("only bound inside a trigger handler"), "got: {err}");
    }

    #[test]
    fn notify_rejects_an_oversized_concatenation_at_compile_time() {
        // Neither half is over the cap on its own, so the old literal-only
        // check passed this straight through to fail at runtime — where it
        // aborts the transaction that sent the notification.
        let half = "x".repeat(4500);
        let err = notify_compile_err(&format!("SELECT notify_raw('c', '{half}' ++ '{half}')"));
        assert!(err.contains("8000-byte"), "got: {err}");
        assert!(err.contains("at least"), "got: {err}");
    }

    #[test]
    fn notify_allows_a_concatenation_that_still_fits() {
        let part = "x".repeat(3000);
        let sql = compile_notify_expr(&format!("SELECT notify_raw('c', '{part}' ++ '{part}')"));
        assert!(sql.contains("pg_notify"), "got: {sql}");
    }

    #[test]
    fn test_notify_raw_emits_pg_notify_with_two_args() {
        let sql = compile_notify_expr("SELECT notify_raw('any_channel', 'raw payload')");
        assert_eq!(sql, "pg_notify('any_channel', 'raw payload')", "got: {sql}");
    }

    #[test]
    fn test_notify_payload_literal_over_cap_rejected() {
        let huge = "x".repeat(8000);
        let err = notify_compile_err(&format!("SELECT notify(Pings, '{huge}')"));
        assert!(err.contains("NOTIFY payload limit"), "got: {err}");
    }

    #[test]
    fn test_notify_arity_error() {
        let err = notify_compile_err("SELECT notify(Pings)");
        assert!(err.contains("takes exactly 2 arguments"), "got: {err}");
    }

    fn compile_seq(query: &str) -> String {
        let schema = make_schema_with_sequence();
        let ast = parse::parse(query).expect("parse failed");
        let ir = super::compile(&ast, &schema).expect("IR compile failed");
        let IrStmt::Select(sel) = ir.stmt else {
            panic!("expected Select")
        };
        let items = free_items(&sel);
        let IrFreeExpr::Scalar(expr) = items[0] else {
            panic!("expected scalar")
        };
        crate::sql::emit_expr(expr)
    }

    #[test]
    fn test_sequence_next_emits_nextval() {
        let sql = compile_seq("SELECT sequence_next(OrderNumber)");
        assert_eq!(sql, r#"nextval('"default"."OrderNumber_seq"')"#, "got: {sql}");
    }

    #[test]
    fn test_sequence_reset_no_val_emits_setval_initial() {
        let sql = compile_seq("SELECT sequence_reset(OrderNumber)");
        assert_eq!(sql, r#"setval('"default"."OrderNumber_seq"', 1, false)"#, "got: {sql}");
    }

    #[test]
    fn test_sequence_reset_with_val_emits_setval() {
        let sql = compile_seq("SELECT sequence_reset(OrderNumber, 1000)");
        assert_eq!(
            sql, r#"setval('"default"."OrderNumber_seq"', 1000, true)"#,
            "got: {sql}"
        );
    }

    #[test]
    fn test_sequence_next_rejects_non_sequence_type() {
        let schema = make_schema();
        let ast = parse::parse("SELECT sequence_next(Person)").unwrap();
        assert!(super::compile(&ast, &schema).is_err());
    }

    #[test]
    fn test_alias_bare_compiles_to_type_select() {
        let schema = make_schema_with_alias();
        let ast = parse::parse("SELECT ActivePersons").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        let sql = crate::sql::emit(&ir).sql;
        assert!(sql.contains("\"person\""), "expected person table, got: {sql}");
        assert!(sql.contains("18"), "expected age filter, got: {sql}");
    }

    #[test]
    fn test_alias_with_outer_filter_merges() {
        let schema = make_schema_with_alias();
        let ast = parse::parse("SELECT ActivePersons FILTER .name = 'Alice'").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        let sql = crate::sql::emit(&ir).sql;
        assert!(sql.contains("\"person\""), "expected person table, got: {sql}");
        assert!(sql.contains("18"), "expected alias filter, got: {sql}");
        assert!(sql.contains("'Alice'"), "expected outer filter, got: {sql}");
    }

    #[test]
    fn test_alias_module_qualified_resolves() {
        let schema = make_schema_with_alias();
        let ast = parse::parse("SELECT default::ActivePersons").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        let sql = crate::sql::emit(&ir).sql;
        assert!(sql.contains("\"person\""), "expected person table, got: {sql}");
    }

    #[test]
    fn test_alias_with_shape() {
        let schema = make_schema_with_alias();
        let ast = parse::parse("SELECT ActivePersons { name, age }").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        let sql = crate::sql::emit(&ir).sql;
        assert!(sql.contains("\"name\""), "expected name pointer, got: {sql}");
        assert!(sql.contains("\"age\""), "expected age pointer, got: {sql}");
    }

    #[test]
    fn test_alias_whose_own_body_has_a_shape_plus_outer_shape() {
        // Regression: when the alias's own body *also* declares a shape
        // (a legitimate, documented pattern — e.g. `select Type { field }
        // order by ... limit ...`), the outer shape used to wrap the
        // inner Shape node wholesale instead of the type reference inside
        // it, producing a Shape-of-a-Shape the compiler rejected with
        // "expected a type name as SELECT subject" (confirmed live).
        use crate::schema::AliasDescriptor;
        let mut schema = make_schema();
        schema.aliases.push(AliasDescriptor {
            name: "OldestActive".into(),
            module: "default".into(),
            expr: "select Person { name } order by .age desc limit 1".into(),
        });
        let ast = parse::parse("SELECT OldestActive { name, age }").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        let sql = crate::sql::emit(&ir).sql;
        assert!(sql.contains("\"name\""), "expected name pointer, got: {sql}");
        assert!(sql.contains("\"age\""), "expected age pointer, got: {sql}");
        assert!(
            sql.contains("ORDER BY") && sql.contains("LIMIT"),
            "alias's own order/limit must still apply, got: {sql}"
        );
    }

    #[test]
    fn test_positional_param_names() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = $0").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        assert_eq!(ir.params, vec!["0"]);
    }

    #[test]
    fn test_multiple_positional_param_names_in_order() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = $0 AND .age > $1").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        assert_eq!(ir.params, vec!["0", "1"]);
    }

    #[test]
    fn test_repeated_positional_param_single_slot() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = $0 OR .name = $0").unwrap();
        let ir = super::compile(&ast, &schema).expect("compile failed");
        assert_eq!(ir.params, vec!["0"], "repeated $0 must occupy a single slot");
    }
}
