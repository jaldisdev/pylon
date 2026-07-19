// ── Deletion policies ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeleteSide {
    Target,
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeleteAction {
    Allow,
    Restrict,
    DeferredRestrict,
    DeleteSource,
    DeleteTarget,
    DeleteTargetIfOrphan,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OnDeletePolicy {
    pub side: DeleteSide,
    pub action: DeleteAction,
}

// ── Mutation rewrites ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewriteEntry {
    /// Bitmask: 1=Insert, 2=Update, 4=Delete (mirrors Python On IntFlag).
    pub on: u8,
    /// PyQL expression evaluated as the new value.
    pub handler: String,
}

// ── Pointer descriptors ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PropertyDescriptor {
    pub name: String,
    /// PostgreSQL column type, e.g. `text`, `int8`, `uuid`.
    pub pg_type: String,
    pub nullable: bool,
    /// SQL expression for the column DEFAULT clause.
    pub default_sql: Option<String>,
    /// PyQL expression to be compiled to SQL at DDL-emit time.
    /// Takes precedence over `default_sql` when both could be set (they won't be).
    pub default_pyql: Option<String>,
    pub description: Option<String>,
    /// Pre-compiled SQL CHECK expressions, e.g. `"price >= 0"`.
    pub check_constraints: Vec<String>,
    /// True when a UNIQUE constraint applies to this column alone.
    pub is_exclusive: bool,
    /// True when this column is the table primary key.
    pub is_pk: bool,
    /// True when the transpiler should reject PyQL updates targeting this pointer.
    pub is_readonly: bool,
    pub rewrites: Vec<RewriteEntry>,
    /// `Some` only for a structural `pylon.Tuple[...]`-typed property — its
    /// element shape, for decode-time `ShapeNode` building. A *nominal*
    /// `@pylon.named_tuple`-typed property instead carries its shape via the
    /// `__nt__:module::Name` `pg_type` marker + `NamedTupleDescriptor.members`.
    pub tuple_members: Option<Vec<TupleMemberDescriptor>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkDescriptor {
    pub name: String,
    /// Qualified name of the target type, e.g. `catalog::Category`.
    pub target: String,
    pub nullable: bool,
    pub description: Option<String>,
    pub default_pyql: Option<String>,
    /// True when a UNIQUE constraint applies to this FK column alone.
    pub is_exclusive: bool,
    /// True when the transpiler should reject PyQL updates targeting this pointer.
    pub is_readonly: bool,
    pub rewrites: Vec<RewriteEntry>,
    pub on_delete: Vec<OnDeletePolicy>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultiLinkDescriptor {
    pub name: String,
    /// Qualified name of the target type.
    pub target: String,
    /// Qualified name of the explicit junction type, if any.
    pub through: Option<String>,
    pub nullable: bool,
    pub description: Option<String>,
    pub default_pyql: Option<String>,
    pub on_delete: Vec<OnDeletePolicy>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ComputedDescriptor {
    pub name: String,
    /// PyQL expression evaluated at query time.
    pub expression: String,
    /// PostgreSQL return type, if known at schema-build time.
    pub return_type: Option<String>,
}

// ── Type-level constructs ──────────────────────────────────────────────────────

// ── Search index ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchBackend {
    Postgres,
    OpenSearch,
    Meilisearch,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchWeight {
    A,
    B,
    C,
    D,
}

impl SearchWeight {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchWeight::A => "A",
            SearchWeight::B => "B",
            SearchWeight::C => "C",
            SearchWeight::D => "D",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchPointerDescriptor {
    pub name: String,
    pub weight: SearchWeight,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchIndexDescriptor {
    pub index_name: Option<String>,
    pub backend: SearchBackend,
    pub pointers: Vec<SearchPointerDescriptor>,
}

impl SearchIndexDescriptor {
    pub fn column_name(&self) -> String {
        match &self.index_name {
            None => "__search__".to_string(),
            Some(name) => format!("__search_{}__", name),
        }
    }

    /// Deferred search index name: `"module__table[__name]"` in lowercase.
    pub fn deferred_index_name(&self, module: &str, type_name: &str) -> String {
        let base = format!("{}__{}", module, type_name).to_lowercase();
        match &self.index_name {
            None => base,
            Some(n) => format!("{}__{}", base, n.to_lowercase()),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexDescriptor {
    /// `None` = default (bare) index; `Some(name)` = named index.
    pub index_name: Option<String>,
    /// Source pointers whose text is concatenated to form the embedding input.
    pub pointers: Vec<String>,
    /// Embedding model identifier, e.g. `"mistral-embed"`.
    pub model: String,
    /// Distance metric: `"cosine"` | `"euclidean"` | `"inner_product"`.
    pub metric: String,
    /// Embedding dimension, e.g. `1024`.
    pub dimensions: u32,
}

impl VectorIndexDescriptor {
    /// PostgreSQL column name for this index's vector column.
    pub fn column_name(&self) -> String {
        match &self.index_name {
            None => "__vector__".to_string(),
            Some(name) => format!("__vector_{}__", name),
        }
    }

    /// pgvector operator class for the configured metric.
    pub fn ops_class(&self) -> &'static str {
        match self.metric.as_str() {
            "euclidean" => "vector_l2_ops",
            "inner_product" => "vector_ip_ops",
            _ => "vector_cosine_ops",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexDescriptor {
    /// Pointer names for a simple or composite index; empty when is_expression=true.
    pub pointers: Vec<String>,
    /// PyQL expression for an expression index.
    pub expression: Option<String>,
    pub unique: bool,
    /// PyQL partial-index predicate.
    pub unless: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TriggerDescriptor {
    /// Bitmask: 1=Insert, 2=Update, 4=Delete.
    pub on: u8,
    /// "Before" | "After" | "InsteadOf"
    pub timing: String,
    pub handler: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TypeConstraint {
    /// Composite UNIQUE INDEX across multiple pointers.
    Exclusive {
        pointers: Vec<String>,
        unless: Option<String>,
    },
    /// Arbitrary CHECK constraint expressed as a PyQL boolean expression.
    Expression { expr: String },
}

// ── Type descriptor ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TypeDescriptor {
    /// Unqualified name, e.g. `Product`.
    pub name: String,
    /// Pylon module, e.g. `catalog`.
    pub module: String,
    /// PostgreSQL table (or view) name, e.g. `catalog_product`.
    pub table: String,
    /// True for `@pylon.abstract` / `@pylon.interface`.
    pub abstract_: bool,
    /// True when a PostgreSQL VIEW is emitted (always true for `@pylon.interface`).
    pub materialized: bool,
    pub description: Option<String>,
    /// Qualified names of abstract (non-materialized) parents.
    pub parents: Vec<String>,
    /// Qualified names of interface parents.
    pub interfaces: Vec<String>,
    /// Flattened properties (includes inherited from abstract parents).
    pub properties: Vec<PropertyDescriptor>,
    /// Flattened links.
    pub links: Vec<LinkDescriptor>,
    /// Flattened multi-links.
    pub multilinks: Vec<MultiLinkDescriptor>,
    /// Computed (virtual) pointers.
    pub computed: Vec<ComputedDescriptor>,
    /// Composite UNIQUE and CHECK constraints (own + inherited from abstract parents).
    pub constraints: Vec<TypeConstraint>,
    /// Non-unique indexes (own + inherited from abstract parents).
    pub indexes: Vec<IndexDescriptor>,
    /// Vector (embedding) indexes.
    pub vector_indexes: Vec<VectorIndexDescriptor>,
    /// Full-text search indexes.
    pub search_indexes: Vec<SearchIndexDescriptor>,
    /// Triggers (own + inherited from abstract parents).
    pub triggers: Vec<TriggerDescriptor>,
    /// True for `@pylon.junction` — type is a junction table for a MultiLink.
    pub junction: bool,
}

// ── Scalar / enum / global descriptors ────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScalarDescriptor {
    /// Qualified scalar name, e.g. `default::Email`.
    pub name: String,
    pub module: String,
    /// Pylon base scalar, e.g. `Str`, `Int64`.
    pub base: String,
    /// PostgreSQL base type, e.g. `text`, `int8`.
    pub pg_type: String,
    /// Pre-compiled SQL CHECK expressions for the DOMAIN constraint.
    pub check_constraints: Vec<String>,
    /// True when the scalar extends `pylon.Sequence` — generates a PostgreSQL SEQUENCE.
    pub is_sequence: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnumDescriptor {
    pub name: String,
    pub module: String,
    pub members: Vec<String>,
}

/// One member's type within a named-tuple/tuple-shaped value — recursive so a
/// member can itself be a nested tuple. Drives decode-time shape building
/// (`ShapeNode`) so a jsonb-backed tuple value decodes with real per-member
/// types instead of an opaque dict/list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TupleMemberKind {
    Scalar { pg_type: String },
    Enum { module: String, name: String },
    /// A member typed as a registered `@pylon.named_tuple` class.
    NamedTuple { module: String, name: String },
    /// A member typed as a nested structural `pylon.Tuple[...]`.
    Tuple { members: Vec<TupleMemberDescriptor> },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TupleMemberDescriptor {
    /// `None` for an unnamed/positional element of a structural tuple.
    pub name: Option<String>,
    pub kind: TupleMemberKind,
}

/// A registered (nominal) `@pylon.named_tuple` type — used both for cast-target
/// resolution (`<module::Name>expr` → jsonb) and, via `members`, for decoding a
/// value read back from a column/cast of this type with real per-member types
/// instead of an opaque dict.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NamedTupleDescriptor {
    pub name: String,
    pub module: String,
    pub members: Vec<TupleMemberDescriptor>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GlobalDescriptor {
    pub name: String,
    pub module: String,
    pub scalar_type: String,
    pub required: bool,
    pub default_expr: Option<String>,
    /// PyQL expression string for computed globals (e.g. `select User filter .id = global current_user_id`).
    /// When set, the global is computed from this expression at query time rather than injected as a parameter.
    pub computed_expr: Option<String>,
}

// ── Function descriptors ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunctionParamDescriptor {
    pub name: String,
    /// PostgreSQL type string, e.g. `int8`, `text`, `uuid`.
    pub pg_type: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunctionDescriptor {
    pub name: String,
    pub module: String,
    pub params: Vec<FunctionParamDescriptor>,
    /// For scalar returns: the PG type (e.g. `int8`).
    /// For object returns: the qualified type name (e.g. `default::Account`).
    pub return_pg_type: String,
    pub return_is_object: bool,
    /// True when the return is `set[T]`.
    pub return_is_set: bool,
    /// True when the return type is a polymorphic interface (abstract + materialized).
    pub return_is_polymorphic: bool,
    /// "immutable" | "stable" | "volatile"
    pub volatility: String,
    /// PyQL expression string (the function body from the docstring).
    pub body: String,
}

// ── Alias descriptor ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AliasDescriptor {
    pub name: String,
    pub module: String,
    /// PyQL expression string (the alias body).
    pub expr: String,
}

// ── Top-level schema ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SchemaDescriptor {
    pub types: Vec<TypeDescriptor>,
    pub scalars: Vec<ScalarDescriptor>,
    pub enums: Vec<EnumDescriptor>,
    pub named_tuples: Vec<NamedTupleDescriptor>,
    pub globals: Vec<GlobalDescriptor>,
    pub functions: Vec<FunctionDescriptor>,
    pub aliases: Vec<AliasDescriptor>,
}
