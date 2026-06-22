// Pylon IR — a typed, resolved query plan produced by compiling a PyQL AST
// against a SchemaDescriptor.
//
// Deliberately simpler than the upstream engine's IR: no set-semantics wrappers, no PathId
// deduplication. Every node is already resolved to a concrete table/column.

mod compiler;

pub use compiler::compile;
pub use compiler::compile_expr_in_type;

use crate::parse::ast::{BinOpKind, UnaryOpKind};

// ── Top-level statement ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum IrStmt {
    Select(IrSelect),
    Insert(IrInsert),
    Update(IrUpdate),
    Delete(IrDelete),
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
    /// When this SELECT wraps a DML statement (`SELECT (INSERT …) { … }`),
    /// the inner DML is stored here and emitted as a CTE.
    /// `None` for plain `SELECT Type { … }`.
    pub dml_source: Option<Box<IrStmt>>,
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

#[derive(Debug, Clone)]
pub enum IrShapeField {
    Scalar(IrScalarField),
    SingleLink(IrSingleLinkField),
    MultiLink(IrMultiLinkField),
    Computed(IrComputedField),
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
    /// Explicit through type — resolved at a later phase.
    Through { through_type: String },
}

/// A computed field: an expression aliased to a name.
#[derive(Debug, Clone)]
pub struct IrComputedField {
    pub alias: String,
    pub expr: IrExpr,
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
}

// ── DELETE ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IrDelete {
    pub target: IrSource,
    pub filter: Option<IrExpr>,
    pub returning: Vec<IrShapeField>,
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

/// The result of the IR compilation step.
/// Carries the query plan and the ordered list of named parameters, which the
/// SQL emitter uses to emit `$1 … $N` and the client uses to bind values.
pub struct IrOutput {
    pub stmt: IrStmt,
    /// Ordered parameter names, positionally matching `$1`, `$2`, … in the SQL.
    pub params: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use crate::schema::{
        LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
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
                            default_sql: Some("gen_random_uuid()".into()),
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
                    triggers: vec![],
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
                    triggers: vec![],
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
                    triggers: vec![],
                },
            ],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
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
        assert_eq!(link.fk_column, "company");
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
        // Bare SELECT Type returns only { id }, matching the upstream engine semantics.
        assert_eq!(sel.shape.len(), 1);
        let IrShapeField::Scalar(f) = &sel.shape[0] else { panic!() };
        assert_eq!(f.alias, "id");
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
}
