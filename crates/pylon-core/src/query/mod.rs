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

use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock, RwLock};

use crate::analyze::ShapePathAlias;
use crate::error::PyQLError;
use crate::schema::SchemaDescriptor;
use crate::{analyze, ir, parse, sql};

const CACHE_CAPACITY: usize = 1024;

/// Number of independently-locked cache shards.
///
/// `LruCache::get` has to reorder the recency list, so it needs a *write*
/// lock even on a hit — meaning a single map serializes every compile in the
/// process behind one lock. Sharding by key hash keeps LRU semantics while
/// cutting that contention by this factor. Must be a power of two, since
/// shard selection masks the low bits of the hash.
const CACHE_SHARDS: usize = 16;

/// One cached compilation, stored behind an `Arc` so a hit hands out a
/// pointer rather than deep-cloning the SQL string, parameter names, tag
/// list and entire shape tree (measured at 0.94 µs of a 1.05 µs cache hit).
///
/// The key is kept alongside the value and re-checked on every hit: lookup
/// is by hash alone (so it costs no allocation), and a hash collision must
/// read as a miss, never as "here is some other query's SQL."
struct CacheEntry {
    query: String,
    config: ir::SessionConfig,
    compiled: Arc<CompiledQuery>,
}

// Keyed by (query text, session config) — not query text alone. Compilation
// success/shape/SQL can depend on the config (e.g. an INSERT assigning `id`
// compiles under allow_user_specified_id=true and errors otherwise), so two
// requests for the same query text under different configs must never share
// a cache entry.
type CacheShard = RwLock<LruCache<u64, CacheEntry>>;

static QUERY_CACHE: OnceLock<Vec<CacheShard>> = OnceLock::new();

fn query_cache() -> &'static [CacheShard] {
    QUERY_CACHE.get_or_init(|| {
        let per_shard = NonZeroUsize::new(CACHE_CAPACITY / CACHE_SHARDS).unwrap();
        (0..CACHE_SHARDS).map(|_| RwLock::new(LruCache::new(per_shard))).collect()
    })
}

fn cache_key_hash(query: &str, config: &ir::SessionConfig) -> u64 {
    let mut hasher = DefaultHasher::new();
    query.hash(&mut hasher);
    config.hash(&mut hasher);
    hasher.finish()
}

/// Discard all cached compiled queries. Call when the schema is reloaded.
pub fn clear_query_cache() {
    if let Some(shards) = QUERY_CACHE.get() {
        for shard in shards {
            shard.write().unwrap().clear();
        }
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
    /// where an array is returned as its own top-level column.
    RawScalar,
    /// Like RawScalar but the value is a decoded JSON object (from a <json> cast).
    /// REPL displays it as `Json("...")`.
    JsonScalar,
    /// Object shape.
    /// `type_name = Some(s)` → named schema type decoded to a registered dataclass.
    /// `type_name = None`    → free type decoded to Pylon's generic Object dataclass.
    /// `name` is the pointer name within the parent (empty string for the root).
    Object {
        name: String,
        type_name: Option<String>,
        position: usize,
        cardinality: Cardinality,
        pointers: Vec<ShapeNode>,
    },
    /// `record[]` column decoded to a Python list.
    Array {
        name: String,
        position: usize,
        element: Box<ShapeNode>,
    },
    /// Anonymous positional tuple decoded to a Python tuple. No type name — no registry lookup.
    Tuple { position: usize, elements: Vec<ShapeNode> },
    /// Named tuple decoded from jsonb. When `type_name` is Some, hydrated to the registered class.
    /// `members` carries the full per-member decode plan when statically known (a registered
    /// NamedTupleDescriptor's members, a structural `pylon.Tuple[...]` property's tuple_members,
    /// or a `<tuple<...>>`/nominal cast's own target type) — `None` falls back to decoding the
    /// raw jsonb value generically (dict/list, no per-member typing).
    NamedTuple {
        name: String,
        position: usize,
        type_name: Option<String>,
        members: Option<Vec<JsonMember>>,
        /// True when this value came from `{ x := 1.0 }` (curly-brace free-
        /// object syntax) rather than `(x := 1.0)` (paren tuple syntax) —
        /// decoded identically either way (both are jsonb), but the
        /// frontend's own value-shape-tag tree (pylon/query.py's
        /// shape_value_tags) uses this to tell the Python API layer to
        /// describe it as an "object" rather than a "namedTuple", so
        /// JsonTree renders an expandable `Object {x: 1.0}` instead of a
        /// non-expandable `(x := 1.0)` tuple literal.
        is_free_object: bool,
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

/// One member's decode plan within a jsonb-backed tuple value
/// (`ShapeNode::NamedTuple.members`) — recursive so a member can itself be a
/// nested tuple.
#[derive(Debug, Clone)]
pub struct JsonMember {
    /// `None` for a positional/unnamed element of a structural tuple
    /// (the value is a jsonb array); `Some` for a named member (a jsonb
    /// object key) — either a nominal named-tuple field or a named
    /// structural element.
    pub key: Option<String>,
    pub kind: JsonMemberKind,
}

#[derive(Debug, Clone)]
pub enum JsonMemberKind {
    /// Plain scalar — the jsonb value's own native JSON type is already
    /// correct (number/string/bool), used as-is.
    Scalar,
    /// Value arrived as a jsonb string; hydrate to the Python enum class
    /// keyed by `enum_type` (Pylon-qualified name, e.g. `default::Gender`).
    Enum { enum_type: String },
    /// Nested tuple member — recurse. `type_name` hydrates to a registered
    /// dataclass when present (nominal); `None` decodes to a plain tuple
    /// (all-positional members) or a dynamically-built dataclass (named,
    /// unregistered structural).
    Tuple {
        type_name: Option<String>,
        members: Vec<JsonMember>,
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

/// Pre-execution inference plan — set when the query requires an external model call
/// before the SQL can be executed.  Python switches on the variant.
#[derive(Debug, Clone)]
pub enum InferencePlan {
    /// `fts::search` with a remote backend (OpenSearch or Meilisearch).
    /// Python fetches (id, score) pairs from the backend, then injects them as
    /// `__deferred_ids__` / `__deferred_scores__` params and runs `sql` against Postgres.
    Search {
        /// Remote backend identifier: `"opensearch"` or `"meilisearch"`.
        backend: String,
        /// Remote index name.
        index_name: String,
        /// Name of the user's query-text param; empty string when an inline literal.
        query_param_name: String,
        /// Inline literal query text.
        query_literal: Option<String>,
        /// Requested result size (limit), if known at compile time.
        size: Option<usize>,
    },
    /// `vector::search(TypeName, query := $text)` text overload.
    /// Python embeds the text via the configured model provider, then injects the
    /// resulting vector as `__deferred_vec__` and runs `sql` against Postgres.
    Embedding {
        /// Embedding model identifier from the schema, e.g. `"mistral-embed"`.
        model_name: String,
        /// Qualified type name for provider lookup, e.g. `"default::Product"`.
        type_name: String,
        /// Vector index name for provider lookup (`None` = default index).
        index_name: Option<String>,
        /// Name of the user's `query :=` param; empty string when an inline literal.
        query_param_name: String,
        /// Inline literal query text.
        query_literal: Option<String>,
    },
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
    /// Set when the query requires a pre-execution model call.
    pub inference_plan: Option<InferencePlan>,
    /// Every schema-qualified table (`"schema.table"`) this statement reads
    /// from or writes to — see `ir::tags::collect_tags`. For a SELECT, the
    /// set of tags to cache this result under; for an INSERT/UPDATE/DELETE,
    /// the set of tags a cache layer must invalidate after the write commits.
    pub tags: Vec<String>,
    /// True when executing this statement writes to any of `tags`. Lets a
    /// cache layer act on the invalidation duty described above without
    /// re-parsing the SQL to guess whether a write happened.
    pub mutates: bool,
    /// `Some` only for `analyze <query>` — the shape-path↔SQL-alias map an
    /// `analyze` execution needs to correlate Postgres's `EXPLAIN` plan
    /// nodes back to the query's own shape (see `analyze` module). `None`
    /// for every other query, which doesn't pay for this extra shape walk.
    pub analyze_paths: Option<Vec<ShapePathAlias>>,
    /// This query's stable shape id — see `crate::shape_id` and `shape_id()`.
    /// Computed once at compile time; fill it with `derive_shape_id` if you
    /// ever build a `CompiledQuery` outside `compile_uncached`.
    pub shape_id: Arc<str>,
}

/// The shape id for a given SQL string and result shape — the derivation
/// `compile_uncached` uses to populate `CompiledQuery::shape_id`, exposed so
/// anything else constructing a `CompiledQuery` by hand can fill that field
/// consistently rather than inventing its own value.
pub fn derive_shape_id(sql: &str, shape: &ShapeDescriptor) -> Arc<str> {
    crate::shape_id::query_shape_id(sql, &format!("{shape:?}")).into()
}

impl CompiledQuery {
    /// This query's stable shape id — see `crate::shape_id`.
    ///
    /// Independent of the values bound to it, which is what makes it safe as
    /// a metric label: a query run a million times with different parameters
    /// reports one label value, not a million.
    ///
    /// Precomputed rather than derived per call. Every execution reports this
    /// label, and deriving it meant `Debug`-formatting the entire shape tree
    /// into a throwaway `String` and hashing it — measured at 3.65 µs per
    /// execution on a 20-field shape, all of it repeated work, since a
    /// `CompiledQuery` is immutable.
    pub fn shape_id(&self) -> Arc<str> {
        self.shape_id.clone()
    }

    /// A stable hash of this query's SQL, for callers that need to key a
    /// cache entry on the statement without moving the SQL text itself
    /// around. Same derivation as `shape_id`, minus the shape.
    pub fn sql_id(&self) -> &str {
        // The shape id already covers the SQL — a query's shape can't change
        // without its SQL changing — so a second hash would be redundant.
        &self.shape_id
    }
}

/// A coarse bucket for a failed query, for use as a metric label.
///
/// Deliberately coarse: an unbounded label (a raw error message, a SQLSTATE,
/// a constraint name) is exactly the thing that blows up a metrics backend's
/// cardinality. Anything finer belongs on a span or in a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// A constraint the write violated — unique, foreign key, check, not-null.
    ConstraintViolation,
    /// Serialization failure or deadlock: the caller can retry.
    Contention,
    /// Statement or lock timeout.
    Timeout,
    /// Couldn't reach or stay connected to the database.
    Connection,
    /// The query never reached the database — bad PyQL, unknown type/pointer.
    Compile,
    Other,
}

impl ErrorClass {
    /// The label value. `&'static str` so it can't accidentally become
    /// unbounded.
    pub fn as_label(self) -> &'static str {
        match self {
            ErrorClass::ConstraintViolation => "constraint_violation",
            ErrorClass::Contention => "contention",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Connection => "connection",
            ErrorClass::Compile => "compile",
            ErrorClass::Other => "other",
        }
    }

    /// Buckets a SQLSTATE by its two-character class, which is how the
    /// standard already groups them — so a code this was never written
    /// against still lands somewhere sensible instead of in `Other`.
    pub fn from_sqlstate(code: &str) -> Self {
        match code {
            "40001" => ErrorClass::Contention,
            "40P01" => ErrorClass::Contention,
            "57014" => ErrorClass::Timeout,
            "55P03" => ErrorClass::Timeout,
            _ => match code.get(..2) {
                // 23 = integrity constraint violation.
                Some("23") => ErrorClass::ConstraintViolation,
                // 08 = connection exception.
                Some("08") => ErrorClass::Connection,
                // 40 = transaction rollback.
                Some("40") => ErrorClass::Contention,
                _ => ErrorClass::Other,
            },
        }
    }
}

/// Whether a query succeeded, and if not, how it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error(ErrorClass),
}

impl Outcome {
    pub fn as_label(self) -> &'static str {
        match self {
            Outcome::Ok => "success",
            Outcome::Error(_) => "error",
        }
    }
}

/// Timing and result facts about one query execution.
///
/// Returned alongside the result rather than reported from inside the
/// execution path, so the caller decides what to do with it — record a
/// metric, attach it to a span, log it, or ignore it. Keeping the decision
/// out here is what lets the same execution path serve an instrumented
/// server and an uninstrumented script.
#[derive(Debug, Clone)]
pub struct ExecutionMetadata {
    /// PyQL → SQL compilation. Zero on a compile-cache hit, which is itself
    /// the signal that the cache is working.
    pub compile_duration: std::time::Duration,
    /// Time in the database, from handing over the SQL to having the rows.
    pub execute_duration: std::time::Duration,
    /// See `CompiledQuery::shape_id`.
    pub query_shape_id: String,
    /// `None` for a statement that returns no rows, which is distinct from
    /// `Some(0)` — a query that ran and matched nothing.
    pub rows_returned: Option<u64>,
    pub outcome: Outcome,
}

impl ExecutionMetadata {
    /// Compile plus execute — what a caller timing "the query" means.
    pub fn total_duration(&self) -> std::time::Duration {
        self.compile_duration + self.execute_duration
    }
}

/// Compile a PyQL expression string in the context of a named type to a bare SQL
/// expression suitable for use in an UPDATE SET clause.
///
/// Pointer references (`.name`) are emitted without a table alias because UPDATE
/// SET expressions reference the current row directly.  Query parameters (`$name`)
/// are rejected — fill expressions must be literal values or pointer references.
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

/// Compile a schema `Trigger`'s `handler` PyQL statement (e.g. `insert Note
/// { note := __new__.name }`) to a full SQL statement, for embedding in the
/// generated plpgsql trigger function body — see `ir::compile_trigger_handler`
/// for the `__new__`/`__old__` row-context binding rules. `on_mask` is
/// Pylon's `On` bitmask (1=Insert, 2=Update, 4=Delete), matching
/// `TriggerDescriptor::on`. Query parameters (`$name`) are rejected, same
/// rule as `compile_fill_expr` — a trigger handler has no caller to supply
/// them.
pub fn compile_trigger_handler(
    handler: &str,
    type_name: &str,
    on_mask: u8,
    schema: &SchemaDescriptor,
) -> Result<String, crate::error::PyQLError> {
    let ir_out = ir::compile_trigger_handler(handler, type_name, on_mask, schema)?;
    if !ir_out.params.is_empty() {
        return Err(crate::error::PyQLError::Syntax(crate::error::PyQLSyntaxError {
            message: "trigger handlers may not contain query parameters".into(),
            position: crate::error::Position { line: 0, col: 0 },
        }));
    }
    Ok(sql::emit(&ir_out).sql)
}

/// Compile a PyQL query string to SQL against `schema`, using default
/// session config (see `ir::SessionConfig`) — for schema-time/test callers
/// with no live client-supplied config. `compile_with_config` is the real
/// entry a query request uses.
///
/// Results are cached in a process-global LRU (capacity 1024). Call
/// `clear_query_cache()` when the schema is reloaded to avoid stale entries.
/// Synchronous — compilation is CPU-bound; async lives at the DB execution layer.
/// Raises `PyQLError` on any grammar, type, or resolution failure.
pub fn compile(query: &str, schema: &SchemaDescriptor) -> Result<Arc<CompiledQuery>, PyQLError> {
    compile_with_config(query, schema, &ir::SessionConfig::default())
}

/// Like `compile`, but honors a caller-supplied `SessionConfig` for this query.
///
/// Returns an `Arc` — callers share one immutable compilation rather than
/// each getting a deep copy of it.
pub fn compile_with_config(
    query: &str,
    schema: &SchemaDescriptor,
    config: &ir::SessionConfig,
) -> Result<Arc<CompiledQuery>, PyQLError> {
    let hash = cache_key_hash(query, config);
    let shard = &query_cache()[(hash as usize) % CACHE_SHARDS];
    {
        let mut cache = shard.write().unwrap();
        if let Some(entry) = cache.get(&hash) {
            // Verify, don't assume: a 64-bit collision is vanishingly rare
            // but handing back the wrong query's SQL would be silent and
            // catastrophic, so a mismatch falls through to a real compile.
            if entry.query == query && &entry.config == config {
                return Ok(entry.compiled.clone());
            }
        }
    }
    let compiled = Arc::new(compile_uncached(query, schema, config)?);
    shard.write().unwrap().put(
        hash,
        CacheEntry {
            query: query.to_string(),
            config: config.clone(),
            compiled: compiled.clone(),
        },
    );
    Ok(compiled)
}

fn compile_uncached(
    query: &str,
    schema: &SchemaDescriptor,
    config: &ir::SessionConfig,
) -> Result<CompiledQuery, PyQLError> {
    let ast = parse::parse(query)?;
    let is_analyze = matches!(ast, parse::Stmt::Analyze(_));
    let ir_out = ir::compile_with_config(&ast, schema, config)?;
    // `analyze`'s own shape-path walk is skipped for every other query — no
    // reason to pay for it when nothing will read `analyze_paths`.
    let analyze_paths = is_analyze.then(|| {
        let mut paths = analyze::collect_shape_path_aliases(&ir_out.stmt);
        // The root marker has no IR pointer of its own to carry it (see
        // `root_marker_offset`'s own doc comment) — filled in here from the
        // original AST, still in scope at this point.
        if let Some(root) = paths.iter_mut().find(|p| p.path == "root") {
            root.marker_offset = analyze::root_marker_offset(&ast);
        }
        paths
    });
    let tags = ir::tags::collect_tags(&ir_out);
    let mutates = stmt_mutates(&ir_out.stmt) || ir_out.ctes.iter().any(|c| stmt_mutates(&c.stmt));
    let sql_out = sql::emit(&ir_out);
    let shape_id = derive_shape_id(&sql_out.sql, &sql_out.shape);
    Ok(CompiledQuery {
        sql: sql_out.sql,
        param_names: ir_out.params,
        params: Vec::new(),
        shape: sql_out.shape,
        warnings: ir_out.warnings,
        inference_plan: sql_out.inference_plan,
        tags,
        mutates,
        analyze_paths,
        shape_id,
    })
}

/// Whether executing this statement writes to any of its `tags`.
///
/// Not just the outermost node: a `FOR` body, a `WITH` binding
/// (`with c := (insert Company {...}) select c`), and a select over a DML
/// source (`select (insert Person {...}) { id }`) all write while presenting
/// as something else.
fn stmt_mutates(stmt: &ir::IrStmt) -> bool {
    match stmt {
        ir::IrStmt::Insert(_) | ir::IrStmt::Update(_) | ir::IrStmt::Delete(_) => true,
        ir::IrStmt::For(f) => stmt_mutates(&f.body),
        ir::IrStmt::Select(sel) => sel.dml_source.as_deref().is_some_and(stmt_mutates),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{PropertyDescriptor, TypeDescriptor};

    fn make_schema() -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![TypeDescriptor {
                name: "Person".into(),
                module: "default".into(),
                table: "person".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
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
            }],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
            channels: vec![],
        }
    }

    #[test]
    fn test_analyze_paths_is_none_for_a_plain_query() {
        let schema = make_schema();
        let compiled = compile("select Person { id }", &schema).unwrap();
        assert!(compiled.analyze_paths.is_none());
    }

    #[test]
    fn test_analyze_paths_is_populated_for_an_analyze_query() {
        let schema = make_schema();
        let query = "analyze select Person { id }";
        let compiled = compile(query, &schema).unwrap();
        let paths = compiled
            .analyze_paths
            .as_ref()
            .expect("analyze query should populate analyze_paths");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].path, "root");
        let offset = paths[0].marker_offset.expect("root path should carry a marker offset");
        assert_eq!(&query[offset..offset + "Person".len()], "Person");
    }
}
