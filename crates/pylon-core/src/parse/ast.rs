// AST nodes for PyQL queries.

// ── Statements ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Group(GroupStmt),
    /// `with alias := (stmt), ... main_stmt`
    With(WithStmt),
    /// `for var in iterator union body`
    For(ForStmt),
    /// `analyze <stmt>` — run the inner statement through Postgres's
    /// EXPLAIN ANALYZE and report a query plan instead of the inner
    /// statement's own result.
    Analyze(Box<Stmt>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupStmt {
    /// The type or expression being grouped.
    pub subject: Expr,
    /// Optional shape restricting which fields appear in `elements`.
    pub shape: Option<Vec<ShapeElement>>,
    /// `USING alias := expr, ...` bindings.
    pub using: Vec<(String, Expr)>,
    /// `BY expr, ...` — each item is an Ident (using-alias ref) or a partial Path (.prop).
    pub by: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithStmt {
    pub aliases: Vec<CteDef>,
    pub stmt: Box<Stmt>,
}

/// One `name := (inner_stmt)` binding in a WITH block.
#[derive(Debug, Clone, PartialEq)]
pub struct CteDef {
    pub name: String,
    pub expr: Expr,
}

/// `for [optional] var in iterator union body`
#[derive(Debug, Clone, PartialEq)]
pub struct ForStmt {
    pub var: String,
    pub optional: bool,
    pub iterator: Expr,
    pub body: Box<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub result: Expr,
    pub filter: Option<Expr>,
    pub order_by: Vec<SortExpr>,
    pub offset: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub subject: ObjectRef,
    pub shape: Vec<ShapeElement>,
    pub unless_conflict: Option<UnlessConflict>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnlessConflict {
    pub on: Option<Expr>,
    pub else_: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub subject: Expr,
    pub filter: Option<Expr>,
    pub shape: Vec<ShapeElement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub subject: Expr,
    pub filter: Option<Expr>,
}

// ── Expressions ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Path(Path),
    Shape(Box<ShapeExpr>),
    BinOp(Box<BinOp>),
    UnaryOp(Box<UnaryOp>),
    FunctionCall(FunctionCall),
    TypeCast(Box<TypeCast>),
    IfElse(Box<IfElse>),
    Literal(Literal),
    Parameter(String),
    Tuple(Vec<Expr>),
    NamedTuple(Vec<(String, Expr)>),
    Array(Vec<Expr>),
    /// A set literal: `{1, 2, 'hello'}`. Multiple values produce multiple rows.
    Set(Vec<Expr>),
    /// A parenthesised statement used as an expression:
    /// `(INSERT ...)`, `(UPDATE ...)`, `(DELETE ...)`, `(SELECT ...)`.
    SubQuery(Box<Stmt>),
    /// Binary set union: `expr union expr` — compiles to UNION ALL.
    Union(Box<Expr>, Box<Expr>),
    /// Binary set difference: `expr except expr` — rows in the left operand
    /// not present in the right, compiled to `EXCEPT`.
    Except(Box<Expr>, Box<Expr>),
    /// A global variable reference: `global name` or `global module::name`.
    Global(String),
    /// Index access: `expr[i]` (0-based).
    Index { expr: Box<Expr>, index: Box<Expr> },
    /// Slice access: `expr[lower:upper]` (0-based, either bound may be absent).
    Slice { expr: Box<Expr>, lower: Option<Box<Expr>>, upper: Option<Box<Expr>> },
    /// Named tuple field access on a non-path expression: `(name := 'a', age := 1).name`.
    FieldAccess { expr: Box<Expr>, field: String },
    /// Positional tuple element on a non-path expression: `(1, 3.14, 'red').2`.
    TupleIndex { expr: Box<Expr>, index: usize },
    /// `detached expr` — evaluate `expr` independently of the current implicit scope.
    Detached(Box<Expr>),
    /// `expr is TypeName` — runtime type check; returns bool.
    TypeIs { expr: Box<Expr>, ty: TypeExpr },
}

// ── Paths ──────────────────────────────────────────────────────────────────────

/// A traversal from a root type or property/link through zero or more steps.
/// `partial = true` when the path starts with `.`, meaning it is relative to
/// the current object (__subject__) rather than an absolute type reference.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub steps: Vec<PathStep>,
    pub partial: bool,
}

impl Path {
    pub fn absolute(name: impl Into<String>) -> Self {
        Path {
            steps: vec![PathStep::Name(name.into())],
            partial: false,
        }
    }

    pub fn relative(name: impl Into<String>) -> Self {
        Path {
            steps: vec![PathStep::Name(name.into())],
            partial: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathStep {
    /// A property or link name: `.name`, `posts`
    Name(String),
    /// Type intersection filter: `[is TypeName]`
    TypeIntersection(ObjectRef),
    /// Link property access: `@source` in a link context
    LinkProp(String),
    /// Backlink traversal: `.<link_name` — objects whose `link_name` points to the current object
    Backlink(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectRef {
    pub module: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn unqualified(name: impl Into<String>) -> Self {
        ObjectRef { module: None, name: name.into() }
    }
    pub fn qualified(module: impl Into<String>, name: impl Into<String>) -> Self {
        ObjectRef { module: Some(module.into()), name: name.into() }
    }
}

// ── Shape operators ────────────────────────────────────────────────────────────

/// The assignment operator used on a shape element in UPDATE SET { ... }.
/// Only relevant for multi-link fields; scalar/single-link fields always use Assign.
#[derive(Debug, Clone, PartialEq)]
pub enum ShapeOp {
    /// `:=` — replace the entire value (clear + insert for multi-links).
    Assign,
    /// `+=` — append to a multi-link set.
    Append,
    /// `-=` — remove from a multi-link set.
    Remove,
}

// ── Shapes ─────────────────────────────────────────────────────────────────────

/// A shape expression: `Expr { element, element, ... }`.
/// `expr = None` only in INSERT bodies where the subject is implicit.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeExpr {
    pub expr: Option<Expr>,
    pub elements: Vec<ShapeElement>,
    /// Source byte offset of `expr`'s first token, if any — used only to
    /// place the root `analyze` marker in the echoed query text (see
    /// `analyze.rs`); not populated (and not needed) outside that path.
    pub marker_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Splat {
    /// `*` — expand to all scalar properties.
    Shallow,
    /// `**` — expand to all scalar properties and all single links (with implicit `{ id }`).
    Deep,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShapeElement {
    /// Relative path being shaped, e.g. `.name` or `.posts`.
    pub path: Path,
    /// When set, this element is a wildcard expansion rather than a named field.
    pub splat: Option<Splat>,
    /// Nested shape for links: `.posts { title, body }`.
    pub nested: Option<Vec<ShapeElement>>,
    /// Computed override or assignment: `.total := .price * .qty` or `friends += expr`.
    pub compexpr: Option<Expr>,
    /// Assignment operator (only meaningful for UPDATE SET elements on multi-links).
    pub op: ShapeOp,
    /// Per-element link modifiers.
    pub filter: Option<Expr>,
    pub order_by: Vec<SortExpr>,
    pub offset: Option<Expr>,
    pub limit: Option<Expr>,
    /// Source byte offset of this element's first token — used only to place
    /// per-pointer `analyze` markers in the echoed query text (see
    /// `analyze.rs`); not populated (and not needed) outside that path.
    pub marker_offset: Option<usize>,
}

impl ShapeElement {
    pub fn splat(kind: Splat) -> Self {
        ShapeElement {
            path: Path { steps: vec![], partial: true },
            splat: Some(kind),
            nested: None,
            compexpr: None,
            op: ShapeOp::Assign,
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            marker_offset: None,
        }
    }
}

// ── Operators ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct BinOp {
    pub left: Expr,
    pub op: BinOpKind,
    pub right: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Like,
    Ilike,
    NotLike,
    NotIlike,
    In,
    NotIn,
    Coalesce,
    Concat,
}

impl std::fmt::Display for BinOpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Add => "+", Self::Sub => "-", Self::Mul => "*",
            Self::Div => "/", Self::FloorDiv => "//", Self::Mod => "%",
            Self::Pow => "^", Self::Eq => "=", Self::Ne => "!=",
            Self::Lt => "<", Self::Le => "<=", Self::Gt => ">", Self::Ge => ">=",
            Self::And => "and", Self::Or => "or",
            Self::Like => "like", Self::Ilike => "ilike",
            Self::NotLike => "not like", Self::NotIlike => "not ilike",
            Self::In => "in", Self::NotIn => "not in",
            Self::Coalesce => "??", Self::Concat => "++",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnaryOp {
    pub op: UnaryOpKind,
    pub operand: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnaryOpKind {
    Not,
    Minus,
    Exists,
    Distinct,
}

// ── Function calls ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCall {
    pub module: Option<String>,
    pub name: String,
    pub args: Vec<Expr>,
    pub kwargs: Vec<(String, Expr)>,
}

// ── Type cast ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct TypeCast {
    pub expr: Expr,
    pub ty: TypeExpr,
}

/// A type expression appearing in a cast (`<TypeExpr>expr`) or a type-is check
/// (`expr[is TypeExpr]`, `expr IS TypeExpr`).
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// A named/qualified type reference: `str`, `default::Point`, `cal::local_date`.
    Named { module: Option<String>, name: String },
    /// A structural tuple type: `tuple<str, bool>` or `tuple<r: int16, g: int16>`.
    /// Elements are either all-named or all-unnamed (checked at parse time) and may
    /// nest arbitrarily (an element's own `ty` can itself be `TypeExpr::Tuple`).
    Tuple { elements: Vec<TupleTypeElement> },
    /// A one-dimensional array type: `array<str>`. The element type may be
    /// anything except another array (checked at parse time) — Pylon arrays
    /// are always one-dimensional, matching a plain Postgres `T[]` column.
    Array { element: Box<TypeExpr> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TupleTypeElement {
    pub name: Option<String>,
    pub ty: Box<TypeExpr>,
}

impl TypeExpr {
    /// Convenience constructor for the common unqualified-name case (matches the
    /// old plain-struct call sites before `TypeExpr` became an enum).
    pub fn named(module: Option<String>, name: impl Into<String>) -> Self {
        TypeExpr::Named { module, name: name.into() }
    }

    /// `(module, name)` for a `Named` type expr; `None` for `Tuple`/`Array` —
    /// used by the object-type-cast / `IS` / type-intersection contexts, which
    /// only ever mean something for a named schema type (never a structural
    /// tuple or array).
    pub fn as_named(&self) -> Option<(Option<&str>, &str)> {
        match self {
            TypeExpr::Named { module, name } => Some((module.as_deref(), name.as_str())),
            TypeExpr::Tuple { .. } | TypeExpr::Array { .. } => None,
        }
    }
}

// ── If / Else ──────────────────────────────────────────────────────────────────

/// PyQL `expr IF cond ELSE expr` ternary.
#[derive(Debug, Clone, PartialEq)]
pub struct IfElse {
    pub if_expr: Expr,
    pub condition: Expr,
    pub else_expr: Expr,
}

// ── Literals ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

// ── Sort ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SortExpr {
    pub expr: Expr,
    pub direction: SortDirection,
    pub nones: NonesOrder,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NonesOrder {
    First,
    Last,
}
