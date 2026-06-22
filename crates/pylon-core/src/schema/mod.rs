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
    /// Triggers (own + inherited from abstract parents).
    pub triggers: Vec<TriggerDescriptor>,
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
}

// ── Top-level schema ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SchemaDescriptor {
    pub types: Vec<TypeDescriptor>,
    pub scalars: Vec<ScalarDescriptor>,
    pub enums: Vec<EnumDescriptor>,
    pub globals: Vec<GlobalDescriptor>,
}
