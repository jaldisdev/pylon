// ── Deletion policies ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteSide {
    Target,
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAction {
    Allow,
    Restrict,
    DeferredRestrict,
    DeleteSource,
    DeleteTarget,
    DeleteTargetIfOrphan,
}

#[derive(Debug, Clone)]
pub struct OnDeletePolicy {
    pub side: DeleteSide,
    pub action: DeleteAction,
}

// ── Mutation rewrites ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RewriteEntry {
    /// Bitmask: 1=Insert, 2=Update, 4=Delete (mirrors Python On IntFlag).
    pub on: u8,
    /// PyQL expression evaluated as the new value.
    pub handler: String,
}

// ── Field descriptors ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PropertyDescriptor {
    pub name: String,
    /// PostgreSQL column type, e.g. `text`, `int8`, `uuid`.
    pub pg_type: String,
    pub nullable: bool,
    /// SQL expression for the column DEFAULT clause.
    pub default_sql: Option<String>,
    pub description: Option<String>,
    /// Pre-compiled SQL CHECK expressions, e.g. `"price >= 0"`.
    pub check_constraints: Vec<String>,
    /// True when a UNIQUE constraint applies to this column alone.
    pub is_exclusive: bool,
    /// True when this column is the table primary key.
    pub is_pk: bool,
    /// True when the transpiler should reject PyQL updates targeting this field.
    pub is_readonly: bool,
    pub rewrites: Vec<RewriteEntry>,
}

#[derive(Debug, Clone)]
pub struct LinkDescriptor {
    pub name: String,
    /// Qualified name of the target type, e.g. `catalog::Category`.
    pub target: String,
    pub nullable: bool,
    pub description: Option<String>,
    /// True when a UNIQUE constraint applies to this FK column alone.
    pub is_exclusive: bool,
    /// True when the transpiler should reject PyQL updates targeting this field.
    pub is_readonly: bool,
    pub rewrites: Vec<RewriteEntry>,
    pub on_delete: Vec<OnDeletePolicy>,
}

#[derive(Debug, Clone)]
pub struct MultiLinkDescriptor {
    pub name: String,
    /// Qualified name of the target type.
    pub target: String,
    /// Qualified name of the explicit junction type, if any.
    pub through: Option<String>,
    pub nullable: bool,
    pub description: Option<String>,
    pub on_delete: Vec<OnDeletePolicy>,
}

#[derive(Debug, Clone)]
pub struct ComputedDescriptor {
    pub name: String,
    /// PyQL expression evaluated at query time.
    pub expression: String,
    /// PostgreSQL return type, if known at schema-build time.
    pub return_type: Option<String>,
}

// ── Type-level constructs ──────────────────────────────────────────────────────

// ── Search index ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchBackend {
    Postgres,
    OpenSearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct SearchFieldDescriptor {
    pub name: String,
    pub weight: SearchWeight,
}

#[derive(Debug, Clone)]
pub struct SearchIndexDescriptor {
    pub index_name: Option<String>,
    pub backend: SearchBackend,
    pub fields: Vec<SearchFieldDescriptor>,
}

impl SearchIndexDescriptor {
    pub fn column_name(&self) -> String {
        match &self.index_name {
            None => "__search__".to_string(),
            Some(name) => format!("__search_{}__", name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VectorIndexDescriptor {
    /// `None` = default (bare) index; `Some(name)` = named index.
    pub index_name: Option<String>,
    /// Source fields whose text is concatenated to form the embedding input.
    pub fields: Vec<String>,
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

#[derive(Debug, Clone)]
pub struct IndexDescriptor {
    /// Column names for a simple or composite index; empty when is_expression=true.
    pub fields: Vec<String>,
    /// PyQL expression for an expression index.
    pub expression: Option<String>,
    pub unique: bool,
    /// PyQL partial-index predicate.
    pub unless: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TriggerDescriptor {
    /// Bitmask: 1=Insert, 2=Update, 4=Delete.
    pub on: u8,
    /// "Before" | "After" | "InsteadOf"
    pub timing: String,
    pub handler: String,
}

#[derive(Debug, Clone)]
pub enum TypeConstraint {
    /// Composite UNIQUE INDEX across multiple fields.
    Exclusive {
        fields: Vec<String>,
        unless: Option<String>,
    },
    /// Arbitrary CHECK constraint expressed as a PyQL boolean expression.
    Expression { expr: String },
}

// ── Type descriptor ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
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
    /// Flattened property fields (includes inherited from abstract parents).
    pub properties: Vec<PropertyDescriptor>,
    /// Flattened link fields.
    pub links: Vec<LinkDescriptor>,
    /// Flattened multi-link fields.
    pub multilinks: Vec<MultiLinkDescriptor>,
    /// Computed (virtual) fields.
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

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone)]
pub struct EnumDescriptor {
    pub name: String,
    pub module: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct FunctionParamDescriptor {
    pub name: String,
    /// PostgreSQL type string, e.g. `int8`, `text`, `uuid`.
    pub pg_type: String,
}

#[derive(Debug, Clone)]
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

// ── Top-level schema ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SchemaDescriptor {
    pub types: Vec<TypeDescriptor>,
    pub scalars: Vec<ScalarDescriptor>,
    pub enums: Vec<EnumDescriptor>,
    pub globals: Vec<GlobalDescriptor>,
    pub functions: Vec<FunctionDescriptor>,
}
