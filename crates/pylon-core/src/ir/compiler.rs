use crate::error::{
    Position, PyQLError, PyQLResolutionError, PyQLTypeError,
    PyQLUnknownFieldError, PyQLUnknownTypeError,
};
use crate::parse::ast::{
    self, Expr, Literal, NonesOrder, ShapeElement, ShapeOp, SortDirection, Stmt,
};
use crate::schema::{
    LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, SearchBackend,
    TypeDescriptor,
};

use std::collections::HashMap;

use super::{
    IrArraySource, IrBinOp, IrComputedPointer, IrConflict, IrCteDef, IrDelete, IrExpr, IrFor,
    IrForIterator, IrFreeExpr, IrFreeSelect, IrFunctionCall, IrFunctionSelect, IrGlobalCte, IrComputedGlobalCte,
    IrSessionGlobalCte, IrIfElse, IrInsert, IrLiteral,
    IrMultiLinkClear, IrMultiLinkPointer, IrMultiLinkJoin, IrMultiLinkMutation, IrMultiLinkValues,
    IrMultiLinkValueSource,
    IrNulls, IrOutput, IrPathJoin, IrPathResult, IrPathSelect, IrPolyImplementor, IrRewrite,
    IrScalarPointer, IrScalarSetPointer, IrSelect, IrShapePointer, IrSingleLinkPointer, IrSort, IrSortDir, IrSource, IrStmt,
    IrFtsSearch, IrTypeCast, IrUnaryOp, IrUpdate, IrLinkProp, IrGroup, IrVectorSearch,
    VectorEnqueueInfo, SearchEnqueueInfo, TupleCastShape,
};

// ── Public entry point ──────────────────────────────────────────────────────────

/// Compile a parsed PyQL statement against the schema, using default session
/// config (see `crate::ir::SessionConfig`) — used throughout this crate's own
/// tests and schema-time compilation, which never has a live client-supplied
/// config to honor. `compile_with_config` is the real entry a live query
/// request uses.
/// Returns the IR plan and the ordered list of parameter names (matching $1, $2, …).
pub fn compile(stmt: &Stmt, schema: &SchemaDescriptor) -> Result<IrOutput, PyQLError> {
    compile_with_config(stmt, schema, &crate::ir::SessionConfig::default())
}

/// Like `compile`, but honors a caller-supplied `SessionConfig` (e.g.
/// `allow_user_specified_id`) for this one statement.
pub fn compile_with_config(
    stmt: &Stmt,
    schema: &SchemaDescriptor,
    config: &crate::ir::SessionConfig,
) -> Result<IrOutput, PyQLError> {
    let mut c = Compiler::with_config(schema, config.clone());

    // Unwrap top-level WITH block: compile each CTE binding, then the main statement.
    let (ctes, ir) = if let Stmt::With(w) = stmt {
        // Special case: `with search := vector::search(…); select search { … } …`
        // Merge into a single IrVectorSearch rather than going through the CTE machinery.
        if let Some(ir) = try_compile_vs_with_pattern(&mut c, w)? {
            return Ok(IrOutput { stmt: ir, params: c.params, ctes: vec![], global_ctes: c.global_ctes, warnings: c.warnings });
        }
        // Special case: `with search := fts::search(…); select search { … } …`
        if let Some(ir) = try_compile_fts_with_pattern(&mut c, w)? {
            return Ok(IrOutput { stmt: ir, params: c.params, ctes: vec![], global_ctes: c.global_ctes, warnings: c.warnings });
        }

        let mut cte_defs = vec![];
        for alias in &w.aliases {
            let ir_stmt = compile_cte_binding(&mut c, &alias.expr)?;
            let type_name = cte_stmt_type(&ir_stmt);
            c.cte_types.insert(alias.name.clone(), type_name.clone());
            cte_defs.push(IrCteDef { name: alias.name.clone(), stmt: ir_stmt, type_name });
        }
        let main = c.compile_stmt(&w.stmt)?;
        (cte_defs, main)
    } else {
        (vec![], c.compile_stmt(stmt)?)
    };

    Ok(IrOutput { stmt: ir, params: c.params, ctes, global_ctes: c.global_ctes, warnings: c.warnings })
}

/// Detect `with <var> := vector::search(Type, $vec); select <var> { object { … }, distance }`.
/// When matched, compile the whole thing to a single `IrVectorSearch`.
fn try_compile_vs_with_pattern(
    c: &mut Compiler<'_>,
    w: &ast::WithStmt,
) -> Result<Option<IrStmt>, PyQLError> {
    // Only handle exactly one alias that is a bare function call (not a subquery).
    if w.aliases.len() != 1 { return Ok(None); }
    let alias_def = &w.aliases[0];
    let fc = match &alias_def.expr {
        Expr::FunctionCall(fc) => fc,
        _ => return Ok(None),
    };
    if fc.module.as_deref() != Some("vector") || fc.name != "search" { return Ok(None); }

    // Main statement must be `select <alias_name> { … }`.
    let select_stmt = match w.stmt.as_ref() {
        Stmt::Select(s) => s,
        _ => return Ok(None),
    };
    // Unwrap optional Shape wrapper around the result expression.
    let (elements, result_inner): (&[ast::ShapeElement], &Expr) = match &select_stmt.result {
        Expr::Shape(sh) => {
            let inner = sh.expr.as_ref().unwrap_or(&select_stmt.result);
            (sh.elements.as_slice(), inner)
        }
        other => (&[], other),
    };
    // The inner expression should be a path referencing the WITH alias.
    match result_inner {
        Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
            if let ast::PathStep::Name(n) = &p.steps[0] {
                if n != &alias_def.name { return Ok(None); }
            } else { return Ok(None); }
        }
        _ => return Ok(None),
    }

    if let Some(ir) = c.try_compile_vector_search(fc, elements, select_stmt)? {
        Ok(Some(IrStmt::VectorSearch(ir)))
    } else {
        Ok(None)
    }
}

fn try_compile_fts_with_pattern(
    c: &mut Compiler<'_>,
    w: &ast::WithStmt,
) -> Result<Option<IrStmt>, PyQLError> {
    if w.aliases.len() != 1 { return Ok(None); }
    let alias_def = &w.aliases[0];
    let fc = match &alias_def.expr {
        Expr::FunctionCall(fc) => fc,
        _ => return Ok(None),
    };
    if fc.module.as_deref() != Some("fts") || fc.name != "search" { return Ok(None); }

    let select_stmt = match w.stmt.as_ref() {
        Stmt::Select(s) => s,
        _ => return Ok(None),
    };
    let (elements, result_inner): (&[ast::ShapeElement], &Expr) = match &select_stmt.result {
        Expr::Shape(sh) => {
            let inner = sh.expr.as_ref().unwrap_or(&select_stmt.result);
            (sh.elements.as_slice(), inner)
        }
        other => (&[], other),
    };
    match result_inner {
        Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
            if let ast::PathStep::Name(n) = &p.steps[0] {
                if n != &alias_def.name { return Ok(None); }
            } else { return Ok(None); }
        }
        _ => return Ok(None),
    }

    if let Some(ir) = c.try_compile_fts_search(fc, elements, select_stmt)? {
        Ok(Some(IrStmt::FtsSearch(ir)))
    } else {
        Ok(None)
    }
}

fn cte_stmt_type(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::Insert(ins) => ins.target.type_name.clone(),
        IrStmt::Update(upd) => upd.target.type_name.clone(),
        IrStmt::Delete(del) => del.target.type_name.clone(),
        IrStmt::Select(sel) => sel.source.type_name.clone(),
        IrStmt::PathSelect(ps) => ps.root.type_name.clone(),
        IrStmt::FreeSelect(fs) => {
            // Infer the scalar pg_type from the first item so the type is available
            // for UNION mismatch error messages. Returns empty string if unknown.
            if let Some(IrFreeExpr::Scalar(expr)) = fs.items.first() {
                if let Some(t) = infer_ir_type(expr) {
                    return t.to_string();
                }
            }
            String::new()
        }
        IrStmt::For(f) => cte_stmt_type(&f.body),
        IrStmt::Group(g) => g.source.type_name.clone(),
        IrStmt::FunctionSelect(fs) => fs.type_name.clone(),
        IrStmt::VectorSearch(vs) => format!("__vs__{}", vs.source.type_name),
        IrStmt::FtsSearch(fs) => format!("__fts__{}", fs.source.type_name),
    }
}

/// Compile a WITH binding value: a subquery becomes its statement; any other
/// expression is wrapped in a synthetic `select expr` so it can be used as a CTE.
fn compile_cte_binding(c: &mut Compiler<'_>, expr: &Expr) -> Result<IrStmt, PyQLError> {
    if let Expr::SubQuery(s) = expr {
        return c.compile_stmt(s);
    }
    // Non-statement expression (e.g. `<default::Company><uuid>'...'`):
    // treat as `select expr`.
    let fake_sel = ast::SelectStmt {
        result: expr.clone(),
        filter: None,
        order_by: vec![],
        offset: None,
        limit: None,
    };
    c.compile_stmt(&Stmt::Select(fake_sel))
}

/// Compile the PyQL body of a user-defined function for DDL emission.
///
/// Sets `fn_params` on the compiler so that parameter names resolve as `FnParam`
/// nodes rather than raising "expression is not valid in free SELECT context".
pub fn compile_fn_body(
    fn_desc: &crate::schema::FunctionDescriptor,
    schema: &SchemaDescriptor,
) -> Result<super::IrOutput, crate::error::PyQLError> {
    use crate::parse;
    use crate::parse::ast::Stmt;

    let body = fn_desc.body.trim().to_string();
    let body = if body.starts_with("select") || body.starts_with("SELECT")
            || body.starts_with("with") || body.starts_with("WITH") {
        body
    } else {
        format!("select {}", body)
    };

    let ast = parse::parse(&body).map_err(|e| e)?;
    let mut c = Compiler::new(schema);
    for p in &fn_desc.params {
        c.fn_params.insert(p.name.clone(), p.pg_type.clone());
    }

    let (ctes, ir) = if let Stmt::With(w) = &ast {
        let mut cte_defs = vec![];
        for alias in &w.aliases {
            let ir_stmt = compile_cte_binding(&mut c, &alias.expr)?;
            let type_name = cte_stmt_type(&ir_stmt);
            c.cte_types.insert(alias.name.clone(), type_name.clone());
            cte_defs.push(super::IrCteDef { name: alias.name.clone(), stmt: ir_stmt, type_name });
        }
        let main = c.compile_stmt(&w.stmt)?;
        (cte_defs, main)
    } else {
        (vec![], c.compile_stmt(&ast)?)
    };

    Ok(super::IrOutput { stmt: ir, params: c.params, ctes, global_ctes: c.global_ctes, warnings: c.warnings })
}

/// Compile a single PyQL expression in the context of a named type.
/// Used for schema fragments: computed pointers, rewrite handlers, constraint exprs.
pub fn compile_expr_in_type(
    expr: &Expr,
    type_name: &str,
    schema: &SchemaDescriptor,
) -> Result<(IrExpr, Vec<String>), PyQLError> {
    let mut c = Compiler::new(schema);
    let td = c.resolve_type(type_name)?;
    let alias = c.fresh_alias();
    let ir = c.compile_expr(expr, td, &alias)?;
    Ok((ir, c.params))
}

/// Like `compile_expr_in_type` but uses an empty table alias, so that column
/// references emit as bare column names (`"col"` rather than `"a1"."col"`).
/// Used for fill expressions in migration UPDATE SET clauses.
pub fn compile_expr_unaliased(
    expr: &Expr,
    type_name: &str,
    schema: &SchemaDescriptor,
) -> Result<(IrExpr, Vec<String>), PyQLError> {
    let mut c = Compiler::new(schema);
    let td = c.resolve_type(type_name)?;
    let ir = c.compile_expr(expr, td, "")?;
    Ok((ir, c.params))
}

/// Compile a PyQL scalar expression to a SQL string for use as a column DEFAULT.
///
/// Wraps the expression in `SELECT <expr>`, compiles it as a free scalar, and
/// returns the emitted SQL expression (without the SELECT wrapper).
pub fn compile_scalar_default(pyql: &str, schema: &SchemaDescriptor) -> Result<String, String> {
    use crate::parse::ast::Stmt;
    let full = format!("SELECT {}", pyql);
    let ast = crate::parse::parse(&full).map_err(|e| e.message)?;
    let Stmt::Select(sel) = &ast else {
        return Err("default expression must be a select statement".into());
    };
    let mut c = Compiler::new(schema);
    let ir = c.compile_free_expr(&sel.result).map_err(|e| e.to_string())?;
    Ok(crate::sql::emit_expr(&ir))
}

// ── Compiler context ────────────────────────────────────────────────────────────

struct Compiler<'a> {
    schema: &'a SchemaDescriptor,
    /// Ordered parameter names — index + 1 is the $N position in SQL.
    params: Vec<String>,
    alias_counter: usize,
    /// CTE names registered in the enclosing WITH block → qualified type name.
    cte_types: HashMap<String, String>,
    /// FOR loop variables in scope: variable name → pg_type of the scalar iterator.
    for_vars: HashMap<String, String>,
    /// User-defined function parameters in scope (only set during body compilation).
    fn_params: HashMap<String, String>,
    /// Global CTEs collected during compilation (session and computed), in dependency order.
    global_ctes: Vec<IrGlobalCte>,
    /// Non-fatal warnings collected during compilation.
    warnings: Vec<String>,
    /// User-configurable session options — see `SessionConfig`. Always
    /// `default()` for every entry point except `compile_with_config`.
    config: crate::ir::SessionConfig,
}

impl<'a> Compiler<'a> {
    fn new(schema: &'a SchemaDescriptor) -> Self {
        Self::with_config(schema, crate::ir::SessionConfig::default())
    }

    fn with_config(schema: &'a SchemaDescriptor, config: crate::ir::SessionConfig) -> Self {
        Compiler {
            schema,
            params: vec![],
            alias_counter: 0,
            cte_types: HashMap::new(),
            for_vars: HashMap::new(),
            fn_params: HashMap::new(),
            global_ctes: vec![],
            warnings: vec![],
            config,
        }
    }

    /// Return the CTE name if `expr` is a bare identifier that matches a registered CTE.
    fn resolve_cte_name<'e>(&self, expr: &'e Expr) -> Option<&'e str> {
        if let Expr::Path(p) = expr {
            if !p.partial && p.steps.len() == 1 {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if self.cte_types.contains_key(n.as_str()) {
                        return Some(n.as_str());
                    }
                }
            }
        }
        None
    }

    fn fresh_alias(&mut self) -> String {
        let a = format!("t{}", self.alias_counter);
        self.alias_counter += 1;
        a
    }

    /// Register a named parameter and return its 0-based index.
    fn param_index(&mut self, name: &str) -> usize {
        if let Some(i) = self.params.iter().position(|n| n == name) {
            return i;
        }
        let i = self.params.len();
        self.params.push(name.to_string());
        i
    }

    // ── Global variable resolution ────────────────────────────────────────────────

    fn resolve_global_pg_type(&self, scalar_type: &str) -> String {
        let builtin = match scalar_type {
            "Str"           => Some("text"),
            "Int16"         => Some("int2"),
            "Int32"         => Some("int4"),
            "Int64"         => Some("int8"),
            "Float32"       => Some("float4"),
            "Float64"       => Some("float8"),
            "Decimal"       => Some("numeric"),
            "Bool"          => Some("boolean"),
            "DateTime"      => Some("timestamptz"),
            "LocalDateTime" => Some("timestamp"),
            "LocalDate"     => Some("date"),
            "LocalTime"     => Some("time"),
            "UUID"          => Some("uuid"),
            "Bytes"         => Some("bytea"),
            "Json"          => Some("jsonb"),
            "Duration"      => Some("interval"),
            _               => None,
        };
        if let Some(t) = builtin {
            return t.to_string();
        }
        // Fall back to custom scalar lookup
        self.schema.scalars.iter()
            .find(|s| s.name.split("::").last() == Some(scalar_type) || s.name == scalar_type)
            .map(|s| s.pg_type.clone())
            .unwrap_or_else(|| "text".to_string())
    }

    /// When `global name` (or `global name { shape }`) appears as the top-level
    /// SELECT subject, inline the computed expression rather than going through a
    /// CTE — this returns a full object, not just its id.
    fn try_compile_global_select(
        &mut self,
        outer: &ast::SelectStmt,
        result: &Expr,
        _distinct: bool,
    ) -> Result<Option<IrStmt>, PyQLError> {
        let (global_name, shape_elements): (&str, &[ast::ShapeElement]) = match result {
            Expr::Global(name) => (name.as_str(), &[]),
            Expr::Shape(sh) => match sh.expr.as_ref() {
                Some(Expr::Global(name)) => (name.as_str(), sh.elements.as_slice()),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        let global = self.schema.globals.iter().find(|g| {
            g.name == global_name || format!("{}::{}", g.module, g.name) == global_name
        });
        let global = match global {
            Some(g) => g.clone(),
            None => return Ok(None),
        };
        let computed_expr = match global.computed_expr {
            Some(e) => e,
            None => return Ok(None), // scalar session global — fall through
        };

        let inner_ast = crate::parse::parse(&computed_expr)?;
        let inner_sel = match inner_ast {
            Stmt::Select(sel) => sel,
            _ => return Ok(None),
        };

        let merged_result = if shape_elements.is_empty() {
            inner_sel.result.clone()
        } else {
            Expr::Shape(Box::new(ast::ShapeExpr {
                expr: Some(inner_sel.result.clone()),
                elements: shape_elements.to_vec(),
            }))
        };

        let merged_filter = match (&inner_sel.filter, &outer.filter) {
            (Some(a), Some(b)) => Some(Expr::BinOp(Box::new(ast::BinOp {
                left: a.clone(),
                op: ast::BinOpKind::And,
                right: b.clone(),
            }))),
            (Some(a), None) => Some(a.clone()),
            (None, b) => b.clone(),
        };

        let merged = ast::SelectStmt {
            result: merged_result,
            filter: merged_filter,
            order_by: if outer.order_by.is_empty() {
                inner_sel.order_by.clone()
            } else {
                outer.order_by.clone()
            },
            offset: outer.offset.clone().or(inner_sel.offset.clone()),
            limit: outer.limit.clone().or(inner_sel.limit.clone()),
        };

        let ir = self.compile_stmt(&Stmt::Select(merged))?;
        Ok(Some(ir))
    }

    /// Peel nested `Expr::FieldAccess` layers (`X.a.b` parses as
    /// `FieldAccess{FieldAccess{X, "a"}, "b"}`) into the innermost root
    /// expression plus the ordered chain of field names.
    fn peel_field_access_chain(expr: &Expr) -> (&Expr, Vec<String>) {
        let mut fields = Vec::new();
        let mut current = expr;
        while let Expr::FieldAccess { expr: inner, field } = current {
            fields.push(field.clone());
            current = inner;
        }
        fields.reverse();
        (current, fields)
    }

    /// Resolve `expr` to the inner `select Type filter ...` statement it
    /// stands for, if any — either a literal subquery (`(select Type filter
    /// ...)`) or a computed global whose defining expression is such a
    /// select. Only bare object-type results are recognized (`select Type
    /// ...`, not `select Type { shape }` or a free expression) — that's the
    /// only shape `.field` access after it can be spliced onto as an
    /// additional path step.
    fn resolve_field_owner_select(&self, expr: &Expr) -> Option<ast::SelectStmt> {
        let sel = match expr {
            Expr::SubQuery(stmt) => match stmt.as_ref() {
                Stmt::Select(sel) => sel.clone(),
                _ => return None,
            },
            Expr::Global(name) => {
                let global = self.schema.globals.iter().find(|g| {
                    g.name == *name || format!("{}::{}", g.module, g.name) == *name
                })?;
                let computed_expr = global.computed_expr.as_ref()?;
                match crate::parse::parse(computed_expr).ok()? {
                    Stmt::Select(sel) => sel,
                    _ => return None,
                }
            }
            _ => return None,
        };
        match &sel.result {
            Expr::Path(p) if !p.partial => Some(sel),
            _ => None,
        }
    }

    /// `global name.field` or `(select Type filter ...).field` (and deeper
    /// chains like `.link.field`) used as the top-level SELECT subject:
    /// rather than treating `.field` as jsonb extraction on an opaque value
    /// (which only makes sense for tuple-typed properties — see
    /// `resolve_property_tuple_shape`), splice the field chain onto the
    /// inner select as additional path steps and recompile as an ordinary
    /// path-select. Mirrors `try_compile_global_select`'s filter/modifier
    /// merge, generalized to a field-access result instead of a bare/shape
    /// global reference.
    fn try_compile_field_access_select(
        &mut self,
        outer: &ast::SelectStmt,
        result: &Expr,
    ) -> Result<Option<IrStmt>, PyQLError> {
        let (root, fields) = Self::peel_field_access_chain(result);
        if fields.is_empty() {
            return Ok(None);
        }
        let inner_sel = match self.resolve_field_owner_select(root) {
            Some(sel) => sel,
            None => return Ok(None),
        };
        let Expr::Path(type_path) = &inner_sel.result else {
            return Ok(None);
        };
        let mut steps = type_path.steps.clone();
        steps.extend(fields.into_iter().map(ast::PathStep::Name));
        let merged_result = Expr::Path(ast::Path { steps, partial: false });

        let merged_filter = match (&inner_sel.filter, &outer.filter) {
            (Some(a), Some(b)) => Some(Expr::BinOp(Box::new(ast::BinOp {
                left: a.clone(),
                op: ast::BinOpKind::And,
                right: b.clone(),
            }))),
            (Some(a), None) => Some(a.clone()),
            (None, b) => b.clone(),
        };

        let merged = ast::SelectStmt {
            result: merged_result,
            filter: merged_filter,
            order_by: if outer.order_by.is_empty() {
                inner_sel.order_by.clone()
            } else {
                outer.order_by.clone()
            },
            offset: outer.offset.clone().or(inner_sel.offset.clone()),
            limit: outer.limit.clone().or(inner_sel.limit.clone()),
        };

        let ir = self.compile_stmt(&Stmt::Select(merged))?;
        Ok(Some(ir))
    }

    fn try_compile_alias_select(
        &mut self,
        outer: &ast::SelectStmt,
        result: &Expr,
        distinct: bool,
    ) -> Result<Option<IrStmt>, PyQLError> {
        // Extract the bare path name and any outer shape elements.
        let (path_name, shape_elements): (&str, &[ast::ShapeElement]) = match result {
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    (n.as_str(), &[])
                } else {
                    return Ok(None);
                }
            }
            Expr::Shape(sh) => match sh.expr.as_ref() {
                Some(Expr::Path(p)) if !p.partial && p.steps.len() == 1 => {
                    if let ast::PathStep::Name(n) = &p.steps[0] {
                        (n.as_str(), sh.elements.as_slice())
                    } else {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        // Match against schema aliases (bare name or module::name).
        let alias = self.schema.aliases.iter().find(|a| {
            a.name == path_name || format!("{}::{}", a.module, a.name) == path_name
        });
        let alias = match alias {
            Some(a) => a.clone(),
            None => return Ok(None),
        };

        let inner_ast = crate::parse::parse(&alias.expr)?;
        let inner_sel = match inner_ast {
            Stmt::Select(sel) => sel,
            _ => return Err(self.type_err(&format!(
                "alias '{}' expression must be a select statement", alias.name
            ))),
        };

        // Merge outer shape / filter / modifiers over the alias's select.
        let merged_result = if shape_elements.is_empty() {
            inner_sel.result.clone()
        } else {
            Expr::Shape(Box::new(ast::ShapeExpr {
                expr: Some(inner_sel.result.clone()),
                elements: shape_elements.to_vec(),
            }))
        };

        let merged_filter = match (&inner_sel.filter, &outer.filter) {
            (Some(a), Some(b)) => Some(Expr::BinOp(Box::new(ast::BinOp {
                left: a.clone(),
                op: ast::BinOpKind::And,
                right: b.clone(),
            }))),
            (Some(a), None) => Some(a.clone()),
            (None, b) => b.clone(),
        };

        let merged = ast::SelectStmt {
            result: merged_result,
            filter: merged_filter,
            order_by: if outer.order_by.is_empty() {
                inner_sel.order_by.clone()
            } else {
                outer.order_by.clone()
            },
            offset: outer.offset.clone().or(inner_sel.offset.clone()),
            limit: outer.limit.clone().or(inner_sel.limit.clone()),
        };

        let _ = distinct; // alias selects honour the outer distinct if applied
        let ir = self.compile_stmt(&Stmt::Select(merged))?;
        Ok(Some(ir))
    }

    fn compile_global(&mut self, raw_name: &str) -> Result<IrExpr, PyQLError> {
        let global = self.schema.globals.iter().find(|g| {
            g.name == raw_name || format!("{}::{}", g.module, g.name) == raw_name
        });
        let global = global.ok_or_else(|| {
            PyQLError::Resolution(PyQLResolutionError::UnknownField(PyQLUnknownFieldError {
                message: format!("unknown global: {:?}", raw_name),
                position: Position { line: 0, col: 0 },
            }))
        })?.clone();

        let qualified = format!("{}::{}", global.module, global.name);
        let cte_name = format!("__global__{}", qualified);

        if let Some(computed_expr) = global.computed_expr {
            // Computed global — check for duplicate before compiling
            if self.global_ctes.iter().any(|g| g.cte_name() == cte_name) {
                return Ok(IrExpr::GlobalRef { cte_name });
            }
            // Recursively compile the PyQL expression (shares params and global_ctes)
            let inner_ast = crate::parse::parse(&computed_expr)?;
            let inner_stmt = self.compile_stmt(&inner_ast)?;
            self.global_ctes.push(IrGlobalCte::Computed(IrComputedGlobalCte {
                cte_name: cte_name.clone(),
                qualified_name: qualified,
                stmt: inner_stmt,
            }));
            Ok(IrExpr::GlobalRef { cte_name })
        } else {
            // Session global — allocate parameter slot
            let pg_type = self.resolve_global_pg_type(&global.scalar_type);
            let param_name = format!("__global__{}", qualified);
            let index = self.param_index(&param_name);
            // Register CTE only once
            if !self.global_ctes.iter().any(|g| g.cte_name() == cte_name) {
                self.global_ctes.push(IrGlobalCte::Session(IrSessionGlobalCte {
                    cte_name: cte_name,
                    qualified_name: qualified,
                    param_index: index,
                    pg_type: pg_type.clone(),
                }));
            }
            Ok(IrExpr::GlobalParam { index, pg_type })
        }
    }

    // ── Schema lookups ────────────────────────────────────────────────────────────

    fn resolve_type(&self, name: &str) -> Result<&'a TypeDescriptor, PyQLError> {
        // Accept both "TypeName" and "module::TypeName"
        self.schema
            .types
            .iter()
            .find(|t| {
                t.name == name || format!("{}::{}", t.module, t.name) == name
            })
            .ok_or_else(|| {
                PyQLError::Resolution(PyQLResolutionError::UnknownType(PyQLUnknownTypeError {
                    message: format!("unknown type '{name}'"),
                    position: Position { line: 0, col: 0 },
                }))
            })
    }

    fn resolve_enum(&self, name: &str) -> Option<&'a crate::schema::EnumDescriptor> {
        self.schema.enums.iter().find(|e| {
            e.name == name || format!("{}::{}", e.module, e.name) == name
        })
    }

    /// Resolve a registered (nominal) `@pylon.named_tuple` type by name — used only
    /// to recognize a cast target as a named tuple (member structure isn't
    /// validated here; the value is trusted the same way a plain `<json>` cast is).
    fn resolve_named_tuple(&self, name: &str) -> Option<&'a crate::schema::NamedTupleDescriptor> {
        self.schema.named_tuples.iter().find(|nt| {
            nt.name == name || format!("{}::{}", nt.module, nt.name) == name
        })
    }

    /// Convert a schema-level `TupleMemberDescriptor` into a decode-time
    /// `JsonMember` — recurses for a nested tuple member, resolving a nested
    /// *nominal* member's own registered members too.
    fn tuple_member_to_json_member(&self, m: &crate::schema::TupleMemberDescriptor) -> crate::query::JsonMember {
        use crate::schema::TupleMemberKind;
        let kind = match &m.kind {
            TupleMemberKind::Scalar { .. } => crate::query::JsonMemberKind::Scalar,
            TupleMemberKind::Enum { module, name } => crate::query::JsonMemberKind::Enum {
                enum_type: format!("{}::{}", module, name),
            },
            TupleMemberKind::NamedTuple { module, name } => {
                let qname = format!("{}::{}", module, name);
                let nested_members = self
                    .resolve_named_tuple(&qname)
                    .map(|nt| nt.members.iter().map(|mm| self.tuple_member_to_json_member(mm)).collect())
                    .unwrap_or_default();
                crate::query::JsonMemberKind::Tuple { type_name: Some(qname), members: nested_members }
            }
            TupleMemberKind::Tuple { members } => crate::query::JsonMemberKind::Tuple {
                type_name: None,
                members: members.iter().map(|mm| self.tuple_member_to_json_member(mm)).collect(),
            },
        };
        crate::query::JsonMember { key: m.name.clone(), kind }
    }

    /// Convert a parsed cast-target `ast::TupleTypeElement` into a decode-time
    /// `JsonMember` — the cast-syntax counterpart of `tuple_member_to_json_member`.
    fn ast_tuple_element_to_json_member(&self, elem: &ast::TupleTypeElement) -> crate::query::JsonMember {
        crate::query::JsonMember {
            key: elem.name.clone(),
            kind: self.ast_type_expr_to_json_member_kind(&elem.ty),
        }
    }

    fn ast_type_expr_to_json_member_kind(&self, ty: &ast::TypeExpr) -> crate::query::JsonMemberKind {
        if let ast::TypeExpr::Tuple { elements } = ty {
            return crate::query::JsonMemberKind::Tuple {
                type_name: None,
                members: elements.iter().map(|e| self.ast_tuple_element_to_json_member(e)).collect(),
            };
        }
        let Some((module, name)) = ty.as_named() else {
            return crate::query::JsonMemberKind::Scalar;
        };
        let qname = match module {
            Some(m) => format!("{}::{}", m, name),
            None => name.to_string(),
        };
        if let Some(ed) = self.resolve_enum(&qname) {
            return crate::query::JsonMemberKind::Enum {
                enum_type: format!("{}::{}", ed.module, ed.name),
            };
        }
        if let Some(nt) = self.resolve_named_tuple(&qname) {
            return crate::query::JsonMemberKind::Tuple {
                type_name: Some(format!("{}::{}", nt.module, nt.name)),
                members: nt.members.iter().map(|m| self.tuple_member_to_json_member(m)).collect(),
            };
        }
        crate::query::JsonMemberKind::Scalar
    }

    /// Resolve a cast's own target-type shape for decode-time `ShapeNode`
    /// building — a structural `tuple<...>` cast resolves its elements
    /// directly; a nominal `<module::Name>` cast resolves via the registered
    /// `NamedTupleDescriptor`'s members. `None` for a plain scalar/enum
    /// cast target.
    fn resolve_tuple_cast_shape(&self, ty: &ast::TypeExpr) -> Option<TupleCastShape> {
        match ty {
            ast::TypeExpr::Tuple { elements } => Some(TupleCastShape {
                type_name: None,
                members: elements.iter().map(|e| self.ast_tuple_element_to_json_member(e)).collect(),
            }),
            _ => {
                let (module, name) = ty.as_named()?;
                let qname = match module {
                    Some(m) => format!("{}::{}", m, name),
                    None => name.to_string(),
                };
                let nt = self.resolve_named_tuple(&qname)?;
                Some(TupleCastShape {
                    type_name: Some(format!("{}::{}", nt.module, nt.name)),
                    members: nt.members.iter().map(|m| self.tuple_member_to_json_member(m)).collect(),
                })
            }
        }
    }

    /// Render a cast target `TypeExpr` for error messages, e.g.
    /// `tuple<std::int64, std::str>` or `default::Point` — used by the
    /// compile-time tuple-index bounds check to match Gel's own wording.
    fn type_expr_to_display_str(&self, ty: &ast::TypeExpr) -> String {
        match ty {
            ast::TypeExpr::Tuple { elements } => {
                let inner = elements
                    .iter()
                    .map(|e| match &e.name {
                        Some(n) => format!("{}: {}", n, self.type_expr_to_display_str(&e.ty)),
                        None => self.type_expr_to_display_str(&e.ty),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("tuple<{}>", inner)
            }
            ast::TypeExpr::Array { element } => format!("array<{}>", self.type_expr_to_display_str(element)),
            ast::TypeExpr::Named { module, name } => match module {
                Some(m) => format!("{}::{}", m, name),
                None => type_expr_to_pg(ty)
                    .map(|pg| pg_type_to_pyql(&pg).to_string())
                    .unwrap_or_else(|_| name.clone()),
            },
        }
    }

    /// Resolve a property's tuple-type shape for decode-time `ShapeNode`
    /// building — a nominal `__nt__:module::Name` `pg_type` marker resolves
    /// via the registered `NamedTupleDescriptor`'s own members; a structural
    /// `pylon.Tuple[...]` property carries its own `tuple_members` directly.
    fn resolve_property_tuple_shape(&self, prop: &PropertyDescriptor) -> Option<TupleCastShape> {
        if let Some(qname) = prop.pg_type.strip_prefix("__nt__:") {
            let nt = self.resolve_named_tuple(qname)?;
            return Some(TupleCastShape {
                type_name: Some(qname.to_string()),
                members: nt.members.iter().map(|m| self.tuple_member_to_json_member(m)).collect(),
            });
        }
        prop.tuple_members.as_ref().map(|members| TupleCastShape {
            type_name: None,
            members: members.iter().map(|m| self.tuple_member_to_json_member(m)).collect(),
        })
    }

    /// Emit a bare `<pg_type>expr` scalar cast as a free-select statement — shared
    /// by the enum/named-tuple/structural-tuple cast cases in `compile_stmt`'s
    /// top-level `Expr::TypeCast` handling. For a structural tuple cast whose
    /// source is itself a tuple/named-tuple literal, applies each element's
    /// own cast by position (see `try_compile_tuple_literal_cast_free`)
    /// instead of jsonb-wrapping the raw uncast literal values.
    fn scalar_cast_free_select(&mut self, tc: &ast::TypeCast, pg_type: String, distinct: bool) -> Result<IrStmt, PyQLError> {
        let cast_expr = match &tc.ty {
            ast::TypeExpr::Tuple { elements } => {
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                match self.try_compile_tuple_literal_cast_free(elements, &tc.expr)? {
                    // The literal-decompose path already applies each element's own
                    // cast — still wrap in TypeCast so `tuple_shape` reaches SQL
                    // emission for decode-time ShapeNode building (jsonb_build_*
                    // already produces jsonb, so the outer `::jsonb` is a no-op).
                    Some(ir) => IrExpr::TypeCast(Box::new(IrTypeCast { expr: ir, pg_type, tuple_shape })),
                    None => {
                        let inner = self.compile_free_expr(&tc.expr)?;
                        IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape }))
                    }
                }
            }
            _ => {
                let inner = self.compile_free_expr(&tc.expr)?;
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape }))
            }
        };
        Ok(IrStmt::FreeSelect(IrFreeSelect {
            items: vec![IrFreeExpr::Scalar(cast_expr)],
            order_by: vec![],
            offset: None,
            limit: None,
            distinct,
        }))
    }

    /// Resolve a cast's target `pg_type` string — shared by both `Expr::TypeCast`
    /// compile sites (`compile_free_expr`/`compile_expr`). A structural tuple
    /// always resolves to jsonb; an array resolves to its element's own pg_type
    /// with a `[]` suffix — a real Postgres array, not jsonb, so it decodes
    /// natively (asyncpg already returns a Python list) with no per-member
    /// shape-tracking needed the way tuples require; a named type checks enum,
    /// then registered named tuple, then falls back to the built-in
    /// scalar/pgvector/cal type list.
    fn resolve_cast_pg_type(&self, ty: &ast::TypeExpr) -> Result<String, PyQLError> {
        if matches!(ty, ast::TypeExpr::Tuple { .. }) {
            return Ok("jsonb".to_string());
        }
        if let ast::TypeExpr::Array { element } = ty {
            let element_pg = self.resolve_cast_pg_type(element)?;
            return Ok(format!("{}[]", element_pg));
        }
        let (module, name) = ty.as_named().expect("checked above: not Tuple/Array");
        let qname = match module {
            Some(m) => format!("{}::{}", m, name),
            None => name.to_string(),
        };
        if let Some(ed) = self.resolve_enum(&qname) {
            return Ok(format!("{}.\"{}\"", crate::sql::pg_schema_str(&ed.module), ed.name));
        }
        if self.resolve_named_tuple(&qname).is_some() {
            return Ok("jsonb".to_string());
        }
        type_expr_to_pg(ty)
    }

    /// Cast one tuple-type element's source value to `target_ty` — recurses
    /// via `try_compile_tuple_literal_cast_free` when both the element's own
    /// type and its source value are themselves a nested tuple/named-tuple
    /// literal, so nesting applies per-element casts all the way down;
    /// otherwise a plain scalar/enum/nominal-named-tuple cast.
    fn compile_tuple_element_cast_free(
        &mut self,
        target_ty: &ast::TypeExpr,
        value: &Expr,
    ) -> Result<IrExpr, PyQLError> {
        if let ast::TypeExpr::Tuple { elements } = target_ty {
            if let Some(ir) = self.try_compile_tuple_literal_cast_free(elements, value)? {
                return Ok(ir);
            }
        }
        let inner = self.compile_free_expr(value)?;
        let pg_type = self.resolve_cast_pg_type(target_ty)?;
        Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape: None })))
    }

    /// When casting a tuple/named-tuple *literal* to a structural tuple type,
    /// apply each target element's own cast to its corresponding source value
    /// by position — e.g. `<tuple<int64, str>>('1', 3)` must coerce '1' to
    /// int64 and 3 to str, not just jsonb-wrap the raw literal values
    /// unchanged. Returns `None` when the source isn't a literal tuple/named-
    /// tuple of matching arity (e.g. a `$param`) — the whole value already
    /// arrives pre-shaped in that case, so the caller's generic jsonb-cast
    /// path handles it instead.
    fn try_compile_tuple_literal_cast_free(
        &mut self,
        target_elements: &[ast::TupleTypeElement],
        source: &Expr,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let source_values: Vec<&Expr> = match source {
            Expr::Tuple(vals) if vals.len() == target_elements.len() => vals.iter().collect(),
            Expr::NamedTuple(fields) if fields.len() == target_elements.len() => {
                fields.iter().map(|(_, v)| v).collect()
            }
            _ => return Ok(None),
        };
        let named = target_elements.iter().all(|e| e.name.is_some());
        let mut casted = Vec::with_capacity(target_elements.len());
        for (elem, value) in target_elements.iter().zip(source_values) {
            casted.push(self.compile_tuple_element_cast_free(&elem.ty, value)?);
        }
        if named {
            let fields = target_elements
                .iter()
                .zip(casted)
                .map(|(e, v)| (e.name.clone().unwrap(), v))
                .collect();
            Ok(Some(IrExpr::NamedTuple(fields)))
        } else {
            Ok(Some(IrExpr::Tuple(casted)))
        }
    }

    /// Schema-bound counterpart of `compile_tuple_element_cast_free` — see
    /// its docs. Needed alongside it because `compile_expr`'s recursive calls
    /// thread `td`/`alias` that `compile_free_expr` doesn't have.
    fn compile_tuple_element_cast(
        &mut self,
        target_ty: &ast::TypeExpr,
        value: &Expr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        if let ast::TypeExpr::Tuple { elements } = target_ty {
            if let Some(ir) = self.try_compile_tuple_literal_cast(elements, value, td, alias)? {
                return Ok(ir);
            }
        }
        let inner = self.compile_expr(value, td, alias)?;
        let pg_type = self.resolve_cast_pg_type(target_ty)?;
        Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape: None })))
    }

    /// Schema-bound counterpart of `try_compile_tuple_literal_cast_free` —
    /// see its docs.
    fn try_compile_tuple_literal_cast(
        &mut self,
        target_elements: &[ast::TupleTypeElement],
        source: &Expr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let source_values: Vec<&Expr> = match source {
            Expr::Tuple(vals) if vals.len() == target_elements.len() => vals.iter().collect(),
            Expr::NamedTuple(fields) if fields.len() == target_elements.len() => {
                fields.iter().map(|(_, v)| v).collect()
            }
            _ => return Ok(None),
        };
        let named = target_elements.iter().all(|e| e.name.is_some());
        let mut casted = Vec::with_capacity(target_elements.len());
        for (elem, value) in target_elements.iter().zip(source_values) {
            casted.push(self.compile_tuple_element_cast(&elem.ty, value, td, alias)?);
        }
        if named {
            let fields = target_elements
                .iter()
                .zip(casted)
                .map(|(e, v)| (e.name.clone().unwrap(), v))
                .collect();
            Ok(Some(IrExpr::NamedTuple(fields)))
        } else {
            Ok(Some(IrExpr::Tuple(casted)))
        }
    }

    /// When casting an array *literal* to `array<T>`, apply the element
    /// type's own cast to each element by position — e.g.
    /// `<array<int64>>['1', '3']` must coerce each string element to int64,
    /// not just emit a raw untyped `ARRAY[...]`. Reuses
    /// `compile_tuple_element_cast_free` for the per-element cast since
    /// casting "this value to this target type" is exactly the same
    /// operation regardless of whether the target is a tuple element or an
    /// array element (including decomposing a nested tuple-literal element).
    /// Returns `None` when the source isn't a literal array (e.g. a
    /// `$param` or a sub-select) — the caller's generic cast path handles
    /// those instead.
    fn try_compile_array_literal_cast_free(
        &mut self,
        element_ty: &ast::TypeExpr,
        source: &Expr,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let Expr::Array(elems) = source else { return Ok(None) };
        let casted = elems
            .iter()
            .map(|e| self.compile_tuple_element_cast_free(element_ty, e))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(IrExpr::Array(casted)))
    }

    /// Schema-bound counterpart of `try_compile_array_literal_cast_free` —
    /// see its docs.
    fn try_compile_array_literal_cast(
        &mut self,
        element_ty: &ast::TypeExpr,
        source: &Expr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let Expr::Array(elems) = source else { return Ok(None) };
        let casted = elems
            .iter()
            .map(|e| self.compile_tuple_element_cast(element_ty, e, td, alias))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(IrExpr::Array(casted)))
    }

    fn compile_enum_access(&self, type_ref: &str, variant: &str) -> Result<IrExpr, PyQLError> {
        let ed = self.resolve_enum(type_ref).ok_or_else(|| {
            self.type_err(&format!("unknown type '{}'", type_ref))
        })?;
        if !ed.members.iter().any(|m| m == variant) {
            return Err(self.type_err(&format!(
                "enum '{}::{}' has no member '{}'",
                ed.module, ed.name, variant
            )));
        }
        Ok(IrExpr::EnumLiteral {
            pg_type: format!("{}.\"{}\"", crate::sql::pg_schema_str(&ed.module), ed.name),
            variant: variant.to_string(),
        })
    }

    fn find_poly_implementors(&self, iface_qname: &str) -> Vec<IrPolyImplementor> {
        self.schema.types.iter()
            .filter(|t| !t.abstract_ && t.interfaces.iter().any(|i| i == iface_qname))
            .map(|t| IrPolyImplementor {
                type_name: format!("{}::{}", t.module, t.name),
                table: t.table.clone(),
                module: t.module.clone(),
            })
            .collect()
    }

    fn resolve_property<'t>(
        td: &'t TypeDescriptor,
        name: &str,
    ) -> Option<&'t PropertyDescriptor> {
        td.properties.iter().find(|p| p.name == name)
    }

    fn resolve_link<'t>(td: &'t TypeDescriptor, name: &str) -> Option<&'t LinkDescriptor> {
        td.links.iter().find(|l| l.name == name)
    }

    fn resolve_multilink<'t>(
        td: &'t TypeDescriptor,
        name: &str,
    ) -> Option<&'t MultiLinkDescriptor> {
        td.multilinks.iter().find(|m| m.name == name)
    }

    // ── Statement dispatch ────────────────────────────────────────────────────────

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<IrStmt, PyQLError> {
        match stmt {
            Stmt::Select(s) => {
                let (distinct, result) = match &s.result {
                    Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct =>
                        (true, &u.operand),
                    // detached at select level is a no-op: CTEs and top-level selects
                    // are already independent — strip the wrapper and compile normally.
                    Expr::Detached(inner) => (false, inner.as_ref()),
                    other => (false, other),
                };

                // `select alias_name [{ shape }]` — inline the alias expression.
                if let Some(ir) = self.try_compile_alias_select(s, result, distinct)? {
                    return Ok(ir);
                }

                // `select global name [{ shape }]` — inline the computed expression.
                if let Some(ir) = self.try_compile_global_select(s, result, distinct)? {
                    return Ok(ir);
                }

                // `global name.field` / `(select Type filter ...).field` — splice the
                // field access onto the inner type-select as an additional path step.
                if let Some(ir) = self.try_compile_field_access_select(s, result)? {
                    return Ok(ir);
                }

                // select fn() { shape } — user-defined object-returning function with shape.
                if let Expr::Shape(sh) = result {
                    if let Some(Expr::FunctionCall(fc)) = sh.expr.as_ref() {
                        if let Some(ir) = self.try_compile_fn_object_select(fc, &sh.elements, s, distinct)? {
                            return Ok(IrStmt::FunctionSelect(ir));
                        }
                    }
                }

                // select fn() — bare user-defined object-returning function (no shape).
                if let Expr::FunctionCall(fc) = result {
                    if let Some(ir) = self.try_compile_fn_object_select(fc, &[], s, distinct)? {
                        return Ok(IrStmt::FunctionSelect(ir));
                    }
                }

                // select vector::search(Type, $vec) { object { … }, distance }
                if let Expr::Shape(sh) = result {
                    if let Some(Expr::FunctionCall(fc)) = sh.expr.as_ref() {
                        if let Some(ir) = self.try_compile_vector_search(fc, &sh.elements, s)? {
                            return Ok(IrStmt::VectorSearch(ir));
                        }
                        if let Some(ir) = self.try_compile_fts_search(fc, &sh.elements, s)? {
                            return Ok(IrStmt::FtsSearch(ir));
                        }
                    }
                }
                // bare vector::search / fts::search without shape
                if let Expr::FunctionCall(fc) = result {
                    if let Some(ir) = self.try_compile_vector_search(fc, &[], s)? {
                        return Ok(IrStmt::VectorSearch(ir));
                    }
                    if let Some(ir) = self.try_compile_fts_search(fc, &[], s)? {
                        return Ok(IrStmt::FtsSearch(ir));
                    }
                }

                // (<Module::Type>expr) { shape } — parenthesised id-lookup with shape.
                // The parens are stripped by the parser, leaving Shape { expr: TypeCast }.
                // Only meaningful for a named schema type — a structural tuple cast
                // never denotes an object-type lookup.
                if let Expr::Shape(sh) = result {
                    if let Some(Expr::TypeCast(tc)) = sh.expr.as_ref() {
                        if let Some((module, name)) = tc.ty.as_named() {
                            if module.map(|m| !["std","cal","math","sys","pgvector"].contains(&m)).unwrap_or(false) {
                                let id_filter = Expr::BinOp(Box::new(ast::BinOp {
                                    left: Expr::Path(ast::Path::relative("id")),
                                    op: ast::BinOpKind::Eq,
                                    right: tc.expr.clone(),
                                }));
                                let merged_filter = match &s.filter {
                                    None => Some(id_filter),
                                    Some(existing) => Some(Expr::BinOp(Box::new(ast::BinOp {
                                        left: id_filter,
                                        op: ast::BinOpKind::And,
                                        right: existing.clone(),
                                    }))),
                                };
                                let synthetic = ast::SelectStmt {
                                    result: Expr::Shape(Box::new(ast::ShapeExpr {
                                        expr: Some(Expr::Path(ast::Path::absolute(name))),
                                        elements: sh.elements.clone(),
                                    })),
                                    filter: merged_filter,
                                    order_by: s.order_by.clone(),
                                    offset: s.offset.clone(),
                                    limit: s.limit.clone(),
                                };
                                return self.compile_select(&synthetic, distinct).map(IrStmt::Select);
                            }
                        }
                    }
                }
                // <Module::Type>expr — schema object lookup by id, or a scalar cast
                // (enum, registered named tuple, or a bare structural `tuple<...>`).
                // Stdlib modules are handled by compile_free_expr; only user schema
                // modules (or a structural tuple, which has no module at all) route here.
                const STDLIB_MODULES: &[&str] = &["std", "cal", "math", "sys", "pgvector"];
                if let Expr::TypeCast(tc) = result {
                    // A structural tuple cast is always a plain scalar (jsonb) cast —
                    // never an object-type lookup — so it's handled directly, before
                    // any of the qname-based (named-type-only) checks below.
                    if matches!(&tc.ty, ast::TypeExpr::Tuple { .. }) {
                        return self.scalar_cast_free_select(tc, "jsonb".to_string(), distinct);
                    }
                    if let Some((module, name)) = tc.ty.as_named() {
                        let qname = match module {
                            Some(m) => format!("{}::{}", m, name),
                            None => name.to_string(),
                        };
                        if let Some(ed) = self.resolve_enum(&qname) {
                            let pg_type = format!("\"{}\".\"{}\"", ed.module, ed.name);
                            return self.scalar_cast_free_select(tc, pg_type, distinct);
                        }
                        if self.resolve_named_tuple(&qname).is_some() {
                            return self.scalar_cast_free_select(tc, "jsonb".to_string(), distinct);
                        }
                        if module.map(|m| !STDLIB_MODULES.contains(&m)).unwrap_or(false) {
                            return self.compile_schema_cast_select(s, tc).map(IrStmt::Select);
                        }
                    }
                }
                // Path traversal: `select TypeName.link.prop` or `select TypeName.link { shape }`.
                if let Expr::Path(p) = result {
                    if !p.partial && p.steps.len() == 2 {
                        if let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice() {
                            if self.resolve_enum(type_ref).is_some() {
                                let expr = self.compile_enum_access(type_ref, variant)?;
                                return Ok(IrStmt::FreeSelect(IrFreeSelect {
                                    items: vec![IrFreeExpr::Scalar(expr)],
                                    order_by: vec![],
                                    offset: None,
                                    limit: None,
                                    distinct,
                                }));
                            }
                        }
                    }
                    if !p.partial && p.steps.len() > 1 {
                        return self.compile_path_select(s, p, &[], distinct).map(IrStmt::PathSelect);
                    }
                }
                if let Expr::Shape(sh) = result {
                    if let Some(Expr::Path(p)) = sh.expr.as_ref() {
                        if !p.partial && p.steps.len() > 1 {
                            return self.compile_path_select(s, p, &sh.elements, distinct)
                                .map(IrStmt::PathSelect);
                        }
                    }
                }
                // assert_exists/assert_distinct with SubQuery arg → set-returning assert
                if let Expr::FunctionCall(f) = result {
                    if (f.module.is_none() || f.module.as_deref() == Some("std"))
                        && matches!(f.name.as_str(), "assert_exists" | "assert_distinct")
                        && f.args.len() >= 1
                    {
                        if let Expr::SubQuery(inner_stmt) = &f.args[0] {
                            let inner = self.compile_subquery_to_array_source(inner_stmt)?;
                            let offset = s.offset.as_ref()
                                .map(|e| self.compile_free_expr(e))
                                .transpose()?;
                            let limit = s.limit.as_ref()
                                .map(|e| self.compile_free_expr(e))
                                .transpose()?;
                            return Ok(IrStmt::FreeSelect(IrFreeSelect {
                                items: vec![IrFreeExpr::AssertSet {
                                    fn_name: f.name.clone(),
                                    inner: Box::new(inner),
                                }],
                                order_by: vec![],
                                offset,
                                limit,
                                distinct,
                            }));
                        }
                    }
                }
                // Expression containing a type-rooted path: `select fn(TypeName.link.prop, ...)`.
                if let Some(root) = self.find_path_root_in_expr(result) {
                    return self.compile_expr_as_path_select(s, result, &root, distinct)
                        .map(IrStmt::PathSelect);
                }
                // `select TypeName is CheckType` — iterate source type, return bool per row.
                if let Expr::TypeIs { expr, ty } = result {
                    if let Expr::Path(p) = expr.as_ref() {
                        if !p.partial && p.steps.len() == 1 {
                            if let ast::PathStep::Name(src_name) = &p.steps[0] {
                                let src_td = self.resolve_type(src_name)?;
                                {
                                    let src_qname = format!("{}::{}", src_td.module, src_td.name);
                                    let (ty_module, ty_name) = ty.as_named()
                                        .ok_or_else(|| self.type_err("cannot use IS with a tuple or array type"))?;
                                    let check_module = ty_module.unwrap_or(&src_td.module);
                                    let check_name = format!("{}::{}", check_module, ty_name);
                                    self.resolve_type(&check_name)?;
                                    let check_qname = check_name;
                                    let src_table = src_td.table.clone();
                                    let src_abstract = src_td.abstract_;
                                    let src_materialized = src_td.materialized;
                                    let src_interfaces = src_td.interfaces.clone();
                                    let src_alias = self.fresh_alias();
                                    let poly_implementors = if src_abstract && src_materialized {
                                        self.find_poly_implementors(&src_qname)
                                    } else {
                                        vec![]
                                    };
                                    let bool_expr = if check_qname == src_qname
                                        || src_interfaces.iter().any(|i| i == &check_qname) {
                                        IrExpr::Literal(IrLiteral::Bool(true))
                                    } else if src_abstract && src_materialized {
                                        IrExpr::BinOp(Box::new(IrBinOp {
                                            left: IrExpr::ColumnRef {
                                                alias: src_alias.clone(),
                                                column: "__type__".into(),
                                                pg_type: "text".into(),
                                            },
                                            op: crate::parse::ast::BinOpKind::Eq,
                                            right: IrExpr::Literal(IrLiteral::Str(check_qname)),
                                        }))
                                    } else {
                                        IrExpr::Literal(IrLiteral::Bool(false))
                                    };
                                    let ps = IrPathSelect {
                                        root: IrSource {
                                            type_name: src_qname,
                                            table: src_table,
                                            alias: src_alias,
                                        },
                                        joins: vec![],
                                        result: IrPathResult::Scalar(bool_expr, None),
                                        filter: s.filter.as_ref()
                                            .map(|_| Err(self.type_err("FILTER is not supported on type-is SELECT")))
                                            .transpose()?,
                                        order_by: vec![],
                                        offset: None,
                                        limit: None,
                                        distinct,
                                        poly_implementors,
                                    };
                                    return Ok(IrStmt::PathSelect(ps));
                                }
                            }
                        }
                    }
                }
                // Catch mixed object/scalar UNION before dispatching further.
                if let Err(e) = self.check_union_type_compat(result) {
                    return Err(e);
                }
                if self.is_free_result(result) {
                    self.compile_free_select(s, distinct).map(IrStmt::FreeSelect)
                } else {
                    self.compile_select(s, distinct).map(IrStmt::Select)
                }
            }
            Stmt::Insert(s) => self.compile_insert(s).map(IrStmt::Insert),
            Stmt::Update(s) => self.compile_update(s).map(IrStmt::Update),
            Stmt::Delete(s) => self.compile_delete(s).map(IrStmt::Delete),
            Stmt::Group(s) => self.compile_group(s).map(IrStmt::Group),
            // Nested WITH blocks (e.g. inside a subquery): inline the CTE types
            // into the current compiler scope so references resolve correctly.
            // The actual CTE SQL is handled at the top-level compile() boundary.
            Stmt::With(w) => {
                for alias in &w.aliases {
                    let ir_inner = compile_cte_binding(self, &alias.expr)?;
                    let type_name = cte_stmt_type(&ir_inner);
                    self.cte_types.insert(alias.name.clone(), type_name);
                }
                self.compile_stmt(&w.stmt)
            }
            Stmt::For(f) => self.compile_for(f).map(IrStmt::For),
        }
    }

    /// `<Module::Type>expr` in SELECT position is a schema object lookup:
    /// select the object whose `id` equals `expr`.  Semantically identical to
    /// `SELECT Type FILTER .id = expr` plus any modifiers on the outer SELECT.
    fn compile_schema_cast_select(
        &mut self,
        sel: &ast::SelectStmt,
        tc: &ast::TypeCast,
    ) -> Result<IrSelect, PyQLError> {
        let id_filter = Expr::BinOp(Box::new(ast::BinOp {
            left: Expr::Path(ast::Path::relative("id")),
            op: ast::BinOpKind::Eq,
            right: tc.expr.clone(),
        }));
        let merged_filter = match &sel.filter {
            None => Some(id_filter),
            Some(existing) => Some(Expr::BinOp(Box::new(ast::BinOp {
                left: id_filter,
                op: ast::BinOpKind::And,
                right: existing.clone(),
            }))),
        };
        // Callers only reach this function after confirming `tc.ty` is `Named`
        // (a structural tuple is never an object-type lookup).
        let (_, name) = tc.ty.as_named()
            .ok_or_else(|| self.type_err("cannot use a tuple or array type as a schema object cast"))?;
        let synthetic = ast::SelectStmt {
            result: Expr::Path(ast::Path::absolute(name)),
            filter: merged_filter,
            order_by: sel.order_by.clone(),
            offset: sel.offset.clone(),
            limit: sel.limit.clone(),
        };
        self.compile_select(&synthetic, false)
    }

    // ── PATH SELECT ───────────────────────────────────────────────────────────────

    /// `select TypeName.link.prop` / `select TypeName.link { shape }`.
    /// Walks the path, building JOIN steps, then projects the final pointer or object.
    fn compile_path_select(
        &mut self,
        sel: &ast::SelectStmt,
        path: &ast::Path,
        shape_elements: &[ShapeElement],
        distinct: bool,
    ) -> Result<IrPathSelect, PyQLError> {
        use ast::PathStep;

        let root_name = match &path.steps[0] {
            PathStep::Name(n) => n.as_str(),
            _ => return Err(self.type_err("path traversal must start with a type name")),
        };
        let root_td = self.resolve_type(root_name)?;
        let root_alias = self.fresh_alias();
        let root = IrSource {
            type_name: format!("{}::{}", root_td.module, root_td.name),
            table: root_td.table.clone(),
            alias: root_alias.clone(),
        };

        let mut joins: Vec<IrPathJoin> = vec![];
        let mut current_td = root_td;
        let mut current_alias = root_alias;

        let steps = &path.steps[1..];
        let mut idx = 0;
        while idx < steps.len() {
            let step = &steps[idx];
            let is_last = |extra: usize| idx + extra == steps.len() - 1;

            // Type intersection standalone (not after backlink): narrows current_td.
            if let PathStep::TypeIntersection(type_ref) = step {
                let type_name = match &type_ref.module {
                    Some(m) => format!("{}::{}", m, type_ref.name),
                    None => type_ref.name.clone(),
                };
                current_td = self.resolve_type(&type_name)?;
                idx += 1;
                continue;
            }

            // Backlink: .<link_name — optionally followed by [is OwnerType].
            if let PathStep::Backlink(link_name) = step {
                // Peek ahead: if the next step is [is Type], use it to identify the owner.
                let owner_hint = steps.get(idx + 1).and_then(|s| {
                    if let PathStep::TypeIntersection(tr) = s { Some(tr.clone()) } else { None }
                });
                let consumed_extra = if owner_hint.is_some() { 1 } else { 0 };

                let owner_td: &TypeDescriptor = if let Some(ref tr) = owner_hint {
                    let type_name = match &tr.module {
                        Some(m) => format!("{}::{}", m, tr.name),
                        None => tr.name.clone(),
                    };
                    let td = self.resolve_type(&type_name)?;
                    let current_qname = format!("{}::{}", current_td.module, current_td.name);
                    let link_targets_current = td.links.iter().any(|l| l.name == *link_name && l.target == current_qname)
                        || td.multilinks.iter().any(|ml| ml.name == *link_name && ml.target == current_qname);
                    if !link_targets_current {
                        return Err(self.type_err(&format!(
                            "link '{}::{}' does not target '{}'; backlink is not valid here",
                            type_name, link_name, current_qname,
                        )));
                    }
                    td
                } else {
                    // No hint — search for any type whose link/multilink targets current_td.
                    let current_qname = format!("{}::{}", current_td.module, current_td.name);
                    self.schema.types.iter()
                        .find(|t| {
                            t.links.iter().any(|l| l.name == *link_name && l.target == current_qname)
                            || t.multilinks.iter().any(|ml| ml.name == *link_name && ml.target == current_qname)
                        })
                        .ok_or_else(|| self.type_err(&format!(
                            "no type has a link '{}' targeting '{}'",
                            link_name, current_qname,
                        )))?
                };

                let target_alias = self.fresh_alias();
                let target = IrSource {
                    type_name: format!("{}::{}", owner_td.module, owner_td.name),
                    table: owner_td.table.clone(),
                    alias: target_alias.clone(),
                };

                // Determine if the link is single (FK) or multi (junction).
                if owner_td.links.iter().any(|l| l.name == *link_name) {
                    joins.push(IrPathJoin::BacklinkSingle {
                        source_alias: current_alias.clone(),
                        fk_col: format!("{}_id", link_name),
                        target,
                    });
                } else {
                    let ml = owner_td.multilinks.iter().find(|ml| ml.name == *link_name).unwrap();
                    let junction_alias = self.fresh_alias();
                    let (junction_table, module) = match &ml.through {
                        Some(through_qname) => {
                            let through_td = self.resolve_type(through_qname)?;
                            (through_td.table.clone(), through_td.module.clone())
                        }
                        None => (
                            format!("{}.{}", owner_td.table, ml.name),
                            owner_td.module.clone(),
                        ),
                    };
                    joins.push(IrPathJoin::BacklinkMulti {
                        source_alias: current_alias.clone(),
                        junction_alias,
                        junction_table,
                        module,
                        owner_col: "source".to_string(),
                        current_col: "target".to_string(),
                        target,
                    });
                }

                if is_last(consumed_extra) {
                    let shape = self.compile_shape(shape_elements, owner_td, &target_alias,
                        &owner_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", owner_td.module, owner_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers(sel, owner_td, &target_alias)?;
                    return Ok(IrPathSelect { root, joins, result, filter, order_by, offset, limit, distinct, poly_implementors: vec![] });
                }
                current_td = owner_td;
                current_alias = target_alias;
                idx += 1 + consumed_extra;
                continue;
            }

            let step_name = match step {
                PathStep::Name(n) => n.as_str(),
                _ => return Err(self.type_err("only name steps are supported in path traversal")),
            };

            // Check scalar property first.
            if let Some(p) = current_td.properties.iter().find(|p| p.name == step_name) {
                if !is_last(0) {
                    // Named tuple properties (nominal `__nt__:` marker) and structural
                    // tuple properties (`tuple_members`) both allow further field
                    // access via jsonb operators.
                    if p.pg_type.starts_with("__nt__:") || p.tuple_members.is_some() {
                        let base = IrExpr::ColumnRef {
                            alias: current_alias.clone(),
                            column: p.name.clone(),
                            pg_type: p.pg_type.clone(),
                        };
                        let remaining = &steps[idx + 1..];
                        let mut ir: IrExpr = base;
                        for step in remaining {
                            let field = match step {
                                ast::PathStep::Name(n) => n.clone(),
                                _ => return Err(self.type_err(
                                    "only field name steps are valid inside a named tuple",
                                )),
                            };
                            ir = IrExpr::JsonbField { expr: Box::new(ir), field };
                        }
                        let (filter, order_by, offset, limit) =
                            self.compile_path_modifiers(sel, current_td, &current_alias)?;
                        return Ok(IrPathSelect {
                            root,
                            joins,
                            result: IrPathResult::Scalar(ir, None),
                            filter,
                            order_by,
                            offset,
                            limit,
                            distinct,
                            poly_implementors: vec![],
                        });
                    }
                    return Err(self.type_err(&format!(
                        "'{step_name}' is a scalar property, not a link — cannot traverse further"
                    )));
                }
                let tuple_shape = self.resolve_property_tuple_shape(p);
                let result = IrPathResult::Scalar(IrExpr::ColumnRef {
                    alias: current_alias.clone(),
                    column: p.name.clone(),
                    pg_type: p.pg_type.clone(),
                }, tuple_shape);
                let (filter, order_by, offset, limit) =
                    self.compile_path_modifiers(sel, current_td, &current_alias)?;
                return Ok(IrPathSelect { root, joins, result, filter, order_by, offset, limit, distinct, poly_implementors: vec![] });
            }

            // Single link.
            if let Some(l) = Self::resolve_link(current_td, step_name) {
                let target_td = self.resolve_type(&l.target)?;
                let target_alias = self.fresh_alias();
                let target = IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: target_alias.clone(),
                };
                joins.push(IrPathJoin::Single {
                    source_alias: current_alias.clone(),
                    fk_col: format!("{}_id", l.name),
                    target,
                });
                if is_last(0) {
                    let shape = self.compile_shape(shape_elements, target_td, &target_alias,
                        &target_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers(sel, target_td, &target_alias)?;
                    return Ok(IrPathSelect { root, joins, result, filter, order_by, offset, limit, distinct, poly_implementors: vec![] });
                }
                current_td = target_td;
                current_alias = target_alias;
                idx += 1;
                continue;
            }

            // Multi-link.
            if let Some(ml) = Self::resolve_multilink(current_td, step_name) {
                let target_td = self.resolve_type(&ml.target)?;
                let target_alias = self.fresh_alias();
                let junction_alias = self.fresh_alias();
                let target = IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: target_alias.clone(),
                };
                let join_info = if let Some(through_qname) = &ml.through {
                    let through_td = self.resolve_type(through_qname)?;
                    if through_td.junction {
                        IrMultiLinkJoin::Standard {
                            junction_table: through_td.table.clone(),
                            module: through_td.module.clone(),
                        }
                    } else {
                        let source_qname = format!("{}::{}", current_td.module, current_td.name);
                        let source_col = through_td.links.iter()
                            .find(|l| l.target == source_qname)
                            .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                                message: format!("through type {through_qname} has no link to {source_qname}"),
                                position: Position { line: 0, col: 0 },
                            }))?.name.clone();
                        let target_col = through_td.links.iter()
                            .find(|l| l.target == ml.target && l.name != source_col)
                            .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                            .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                                message: format!("through type {through_qname} has no link to target {}", ml.target),
                                position: Position { line: 0, col: 0 },
                            }))?.name.clone();
                        IrMultiLinkJoin::Through {
                            junction_table: through_td.table.clone(),
                            module: through_td.module.clone(),
                            source_col,
                            target_col,
                        }
                    }
                } else {
                    IrMultiLinkJoin::Standard {
                        junction_table: format!("{}.{}", current_td.table, ml.name),
                        module: current_td.module.clone(),
                    }
                };
                joins.push(IrPathJoin::Multi {
                    source_alias: current_alias.clone(),
                    junction_alias,
                    join: join_info,
                    target,
                });
                if is_last(0) {
                    let shape = self.compile_shape(shape_elements, target_td, &target_alias,
                        &target_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers(sel, target_td, &target_alias)?;
                    return Ok(IrPathSelect { root, joins, result, filter, order_by, offset, limit, distinct, poly_implementors: vec![] });
                }
                current_td = target_td;
                current_alias = target_alias;
                idx += 1;
                continue;
            }

            return Err(self.type_err(&format!(
                "type '{}' has no property or link named '{step_name}'",
                current_td.name
            )));
        }

        // Should be unreachable: steps is non-empty (we checked len > 1 before dispatch).
        Err(self.type_err("empty path traversal"))
    }

    fn compile_path_modifiers(
        &mut self,
        sel: &ast::SelectStmt,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<(Option<IrExpr>, Vec<IrSort>, Option<IrExpr>, Option<IrExpr>), PyQLError> {
        let filter = sel.filter.as_ref()
            .map(|f| self.compile_expr(f, td, alias))
            .transpose()?;
        let order_by = sel.order_by.iter()
            .map(|s| self.compile_sort(s, td, alias))
            .collect::<Result<Vec<_>, _>>()?;
        let offset = sel.offset.as_ref()
            .map(|e| self.compile_expr(e, td, alias))
            .transpose()?;
        let limit = sel.limit.as_ref()
            .map(|e| self.compile_expr(e, td, alias))
            .transpose()?;
        Ok((filter, order_by, offset, limit))
    }

    // ── Expression-over-type dispatch ─────────────────────────────────────────────

    /// Recursively find the first absolute path (non-partial, multi-step) in an expression
    /// and return its root type name if it resolves to a known schema type.
    fn find_path_root_in_expr(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Path(p) if !p.partial && p.steps.len() > 1 => {
                if let ast::PathStep::Name(root) = &p.steps[0] {
                    if self.resolve_type(root).is_ok() { return Some(root.clone()); }
                }
                None
            }
            Expr::FunctionCall(f) =>
                f.args.iter().find_map(|a| self.find_path_root_in_expr(a)),
            Expr::BinOp(b) =>
                self.find_path_root_in_expr(&b.left)
                    .or_else(|| self.find_path_root_in_expr(&b.right)),
            Expr::UnaryOp(u) => self.find_path_root_in_expr(&u.operand),
            _ => None,
        }
    }

    /// Rewrite absolute paths rooted at `root_name` to relative (partial) paths.
    fn rewrite_abs_to_partial(expr: Expr, root_name: &str) -> Expr {
        match expr {
            Expr::Path(ref p) if !p.partial => {
                if let ast::PathStep::Name(first) = &p.steps[0] {
                    if first == root_name && p.steps.len() > 1 {
                        return Expr::Path(ast::Path {
                            steps: p.steps[1..].to_vec(),
                            partial: true,
                        });
                    }
                }
                expr
            }
            Expr::FunctionCall(f) => Expr::FunctionCall(ast::FunctionCall {
                module: f.module,
                name: f.name,
                args: f.args.into_iter()
                    .map(|a| Self::rewrite_abs_to_partial(a, root_name))
                    .collect(),
                kwargs: f.kwargs,
            }),
            Expr::BinOp(b) => Expr::BinOp(Box::new(ast::BinOp {
                left: Self::rewrite_abs_to_partial(b.left, root_name),
                op: b.op,
                right: Self::rewrite_abs_to_partial(b.right, root_name),
            })),
            Expr::UnaryOp(u) => Expr::UnaryOp(Box::new(ast::UnaryOp {
                op: u.op,
                operand: Self::rewrite_abs_to_partial(u.operand, root_name),
            })),
            other => other,
        }
    }

    /// Compile an expression that contains a type-rooted absolute path as a flat
    /// path select, iterating over the root type and projecting the expression as
    /// a scalar computed pointer.
    fn compile_expr_as_path_select(
        &mut self,
        sel: &ast::SelectStmt,
        result: &Expr,
        root_type_name: &str,
        distinct: bool,
    ) -> Result<IrPathSelect, PyQLError> {
        // BinOp where one side is the absolute type-rooted path:
        // Build the join chain for the path side, then apply the comparison
        // element-wise so we get one boolean per traversal element (not one EXISTS per root).
        if let Expr::BinOp(b) = result {
            let (path_expr, value_expr, flip) = match (&b.left, &b.right) {
                (Expr::Path(p), v) if !p.partial => (p, v, false),
                (v, Expr::Path(p)) if !p.partial => (p, v, true),
                _ => return self.compile_expr_as_path_select_fallback(sel, result, root_type_name, distinct),
            };
            // Compile value side first so we can report its type in errors
            let root_td = self.resolve_type(root_type_name)?;
            // Build PathSelect for the path (builds root + joins + scalar/object result)
            let ast_path = ast::Path { partial: false, steps: path_expr.steps.clone() };
            let mut ps = self.compile_path_select(sel, &ast_path, &[], distinct)?;
            let val_ir = self.compile_expr(value_expr, root_td, &ps.root.alias)?;
            // Extract the scalar result from the path
            let scalar_col = match ps.result {
                IrPathResult::Scalar(e, _) => e,
                IrPathResult::Object { type_name, .. } => {
                    let val_type = infer_ir_type(&val_ir)
                        .map(pg_type_to_pyql)
                        .unwrap_or("unknown");
                    return Err(PyQLError::Type(PyQLTypeError {
                        message: format!(
                            "operator '{}' cannot be applied to operands of type '{}' and '{}'",
                            b.op,
                            type_name,
                            val_type,
                        ),
                        position: Position { line: 0, col: 0 },
                    }));
                }
            };
            let (l, r) = if flip { (val_ir, scalar_col) } else { (scalar_col, val_ir) };
            ps.result = IrPathResult::Scalar(IrExpr::BinOp(Box::new(IrBinOp {
                left: l, op: b.op.clone(), right: r,
            })), None);
            return Ok(ps);
        }
        self.compile_expr_as_path_select_fallback(sel, result, root_type_name, distinct)
    }

    fn compile_expr_as_path_select_fallback(
        &mut self,
        sel: &ast::SelectStmt,
        result: &Expr,
        root_type_name: &str,
        distinct: bool,
    ) -> Result<IrPathSelect, PyQLError> {
        let td = self.resolve_type(root_type_name)?;
        let alias = self.fresh_alias();
        let root = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };
        let rewritten = Self::rewrite_abs_to_partial(result.clone(), root_type_name);
        let expr = self.compile_expr(&rewritten, td, &alias)?;
        let (filter, order_by, offset, limit) =
            self.compile_path_modifiers(sel, td, &alias)?;
        Ok(IrPathSelect {
            root,
            joins: vec![],
            result: IrPathResult::Scalar(expr, None),
            filter,
            order_by,
            offset,
            limit,
            distinct,
            poly_implementors: vec![],
        })
    }

    /// Compile a SubQuery stmt to an `IrArraySource` for use in assert functions.
    /// The result is `ARRAY(SELECT scalar FROM compiled_inner)`.
    fn compile_subquery_to_array_source(&mut self, stmt: &Stmt) -> Result<IrArraySource, PyQLError> {
        match self.compile_stmt(stmt)? {
            IrStmt::Select(s) => Ok(IrArraySource::Select(s)),
            IrStmt::PathSelect(ps) => Ok(IrArraySource::PathSelect(ps)),
            _ => Err(self.type_err(
                "assert functions require a schema-bound SELECT as argument",
            )),
        }
    }

    /// `exists` in schema-bound expression context (FILTER, computed pointer, etc.).
    fn compile_exists_operand(
        &mut self,
        operand: &Expr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        match operand {
            // exists $param  /  exists <type>$param → $N IS NOT NULL
            Expr::Parameter(name) => {
                let idx = self.param_index(name);
                Ok(ir_is_not_null(IrExpr::Param { index: idx }))
            }
            // exists <type>expr → expr IS NOT NULL (cast result is always a scalar)
            Expr::TypeCast(_) => {
                let inner = self.compile_expr(operand, td, alias)?;
                Ok(ir_is_not_null(inner))
            }

            // exists .<link[is Type] → EXISTS(SELECT 1 FROM type WHERE type.link_id = alias.id)
            Expr::Path(p) if p.partial && matches!(p.steps.first(), Some(ast::PathStep::Backlink(_))) => {
                let current_qname = format!("{}::{}", td.module, td.name);
                let exists = self.compile_backlink_as_exists(&p.steps, None, &current_qname, alias)?;
                return Ok(exists);
            }

            // exists .prop → alias.col IS NOT NULL
            // exists .link → alias.link_id IS NOT NULL
            // exists .multilink → EXISTS(SELECT 1 FROM junction WHERE src = alias.id)
            Expr::Path(p) if p.partial && p.steps.len() == 1 => {
                let pointer_name = match &p.steps[0] {
                    ast::PathStep::Name(n) => n.as_str(),
                    _ => return Err(self.type_err("exists: invalid path step")),
                };
                if let Some(prop) = Self::resolve_property(td, pointer_name) {
                    return Ok(ir_is_not_null(IrExpr::ColumnRef {
                        alias: alias.to_string(),
                        column: prop.name.clone(),
                        pg_type: prop.pg_type.clone(),
                    }));
                }
                if let Some(link) = Self::resolve_link(td, pointer_name) {
                    return Ok(ir_is_not_null(IrExpr::ColumnRef {
                        alias: alias.to_string(),
                        column: format!("{}_id", link.name),
                        pg_type: "uuid".to_string(),
                    }));
                }
                if Self::resolve_multilink(td, pointer_name).is_some() {
                    return self.compile_multilink_exists_check(pointer_name, td, alias);
                }
                Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)))
            }

            // exists (select ...) → EXISTS(SELECT 1 FROM ...)
            Expr::SubQuery(stmt) => {
                self.compile_subquery_exists(stmt)
            }

            // Fallback: any scalar expression → expr IS NOT NULL
            other => {
                let inner = self.compile_expr(other, td, alias)?;
                Ok(ir_is_not_null(inner))
            }
        }
    }

    /// `exists` in free (no schema context) expressions — params and subqueries only.
    fn compile_exists_free(&mut self, operand: &Expr) -> Result<IrExpr, PyQLError> {
        match operand {
            Expr::Parameter(name) => {
                let idx = self.param_index(name);
                Ok(ir_is_not_null(IrExpr::Param { index: idx }))
            }
            // exists <type>expr → expr IS NOT NULL (cast result is always scalar)
            Expr::TypeCast(_) => {
                let inner = self.compile_free_expr(operand)?;
                Ok(ir_is_not_null(inner))
            }
            Expr::SubQuery(stmt) => {
                self.compile_subquery_exists(stmt)
            }
            // Fallback: any scalar literal or expression → expr IS NOT NULL
            other => {
                let inner = self.compile_free_expr(other)?;
                Ok(ir_is_not_null(inner))
            }
        }
    }

    /// Compile `exists (select ...)` → `EXISTS(SELECT 1 FROM ... WHERE ...)`.
    fn compile_subquery_exists(&mut self, stmt: &Stmt) -> Result<IrExpr, PyQLError> {
        match self.compile_stmt(stmt)? {
            IrStmt::Select(s) => {
                let inner = IrExpr::Subquery(Box::new(IrSelect {
                    source: s.source,
                    shape: vec![],
                    filter: s.filter,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    distinct: false,
                    dml_source: None,
                    polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                }));
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: ast::UnaryOpKind::Exists, operand: inner })))
            }
            IrStmt::PathSelect(ps) => {
                // EXISTS(SELECT 1 FROM root [JOINs] WHERE filter)
                // Reuse the path select but signal "exists" via a dedicated IR node
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: IrExpr::Subquery(Box::new(IrSelect {
                        source: ps.root,
                        shape: vec![],
                        filter: ps.filter,
                        order_by: vec![],
                        offset: None,
                        limit: None,
                        distinct: false,
                        dml_source: None,
                        polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                    })),
                })))
            }
            _ => Err(self.type_err("exists requires a SELECT expression")),
        }
    }

    /// `EXISTS(SELECT 1 FROM junction WHERE junction.source = alias.id)` for a multi-link.
    /// Build the `IrSelect` over the junction/FK-target rows for a multilink,
    /// correlated to the current row (`alias.id`) — shared by `exists
    /// .multilink` and `count(.multilink)`.
    fn multilink_correlation_select(
        &mut self,
        ml_name: &str,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrSelect, PyQLError> {
        let ml = Self::resolve_multilink(td, ml_name).unwrap();
        let ml_through = ml.through.clone();
        let td_module = td.module.clone();
        let td_name = td.name.clone();
        let td_table = td.table.clone();
        let jt_alias = self.fresh_alias();

        let (jt_table, jt_module, jt_src_col) = if let Some(through_qname) = &ml_through {
            let through_td = self.resolve_type(through_qname)?;
            if through_td.junction {
                (through_td.table.clone(), through_td.module.clone(), "source".to_string())
            } else {
                let source_qname = format!("{}::{}", td_module, td_name);
                let src_col = through_td.links.iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                        message: format!("through type {through_qname} has no link to {source_qname}"),
                        position: Position { line: 0, col: 0 },
                    }))?.name.clone();
                (through_td.table.clone(), through_td.module.clone(), format!("{}_id", src_col))
            }
        } else {
            (format!("{}.{}", td_table, ml_name), td_module.clone(), "source".to_string())
        };

        let filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef { alias: jt_alias.clone(), column: jt_src_col, pg_type: "uuid".to_string() },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef { alias: alias.to_string(), column: "id".to_string(), pg_type: "uuid".to_string() },
        }));
        Ok(IrSelect {
            source: IrSource {
                type_name: format!("{}::__jt__", jt_module),
                table: jt_table,
                alias: jt_alias,
            },
            shape: vec![],
            filter: Some(filter),
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        })
    }

    fn compile_multilink_exists_check(
        &mut self,
        ml_name: &str,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let inner = self.multilink_correlation_select(ml_name, td, alias)?;
        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: IrExpr::Subquery(Box::new(inner)),
        })))
    }

    /// Returns true when the SELECT result expression is not a schema type reference.
    /// Check that a UNION expression doesn't mix object types with scalars.
    /// Called before dispatch so we can give a clear error instead of "expected a type name".
    fn check_union_type_compat(&self, expr: &Expr) -> Result<(), PyQLError> {
        let Expr::Union(a, b) = expr else { return Ok(()) };
        let a_free = self.is_free_result(a);
        let b_free = self.is_free_result(b);
        if a_free != b_free {
            let left = self.union_operand_type_display(a);
            let right = self.union_operand_type_display(b);
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "operator 'UNION' cannot be applied to operands of type '{}' and '{}'",
                    left, right,
                ),
                position: Position { line: 0, col: 0 },
            }));
        }
        Ok(())
    }

    fn union_operand_type_display(&self, expr: &Expr) -> String {
        match expr {
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if let Some(t) = self.cte_types.get(n.as_str()) {
                        if t.contains("::") {
                            return t.clone(); // object CTE: "default::Person"
                        }
                        if !t.is_empty() {
                            return pg_type_to_pyql(t).to_string(); // scalar CTE: "std::int64"
                        }
                    }
                }
                // Bare type name reference
                if let Ok(td) = self.resolve_type(
                    p.steps.first().and_then(|s| if let ast::PathStep::Name(n) = s { Some(n.as_str()) } else { None }).unwrap_or("")
                ) {
                    return format!("{}::{}", td.module, td.name);
                }
            }
            Expr::Literal(Literal::Int(_)) => return "std::int64".to_string(),
            Expr::Literal(Literal::Str(_)) => return "std::str".to_string(),
            Expr::Literal(Literal::Float(_)) => return "std::float64".to_string(),
            Expr::Literal(Literal::Bool(_)) => return "std::bool".to_string(),
            _ => {}
        }
        "unknown".to_string()
    }

    fn is_free_result(&self, expr: &Expr) -> bool {
        let expr = match expr {
            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct => &u.operand,
            Expr::Detached(inner) => inner.as_ref(),
            other => other,
        };
        match expr {
            Expr::Path(p) if !p.partial => {
                if p.steps.len() == 1 {
                    if let ast::PathStep::Name(n) = &p.steps[0] {
                        // For-loop variable is a scalar, not a schema type reference.
                        if self.for_vars.contains_key(n.as_str()) {
                            return true;
                        }
                        // Scalar CTE: type string has no "::" (object types always do).
                        if self.cte_types.get(n.as_str()).map(|t| !t.contains("::")).unwrap_or(false) {
                            return true;
                        }
                    }
                }
                false
            }
            Expr::Shape(s) if s.expr.is_some() => false,
            Expr::SubQuery(_) => false,
            Expr::Union(a, b) => self.is_free_result(a) && self.is_free_result(b),
            _ => true,
        }
    }

    // ── FREE SELECT ───────────────────────────────────────────────────────────────

    fn collect_union_items(
        &mut self,
        expr: &Expr,
        items: &mut Vec<IrFreeExpr>,
    ) -> Result<(), PyQLError> {
        match expr {
            Expr::Union(a, b) => {
                self.collect_union_items(a, items)?;
                self.collect_union_items(b, items)?;
            }
            Expr::Set(exprs) => {
                for e in exprs {
                    self.collect_union_items(e, items)?;
                }
            }
            other => {
                items.push(IrFreeExpr::Scalar(self.compile_free_expr(other)?));
            }
        }
        Ok(())
    }

    fn compile_free_select(
        &mut self,
        sel: &ast::SelectStmt,
        distinct: bool,
    ) -> Result<IrFreeSelect, PyQLError> {
        if sel.filter.is_some() {
            return Err(self.type_err("FILTER is not supported on free SELECT expressions"));
        }

        let result_expr = match &sel.result {
            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct => &u.operand,
            other => other,
        };

        let items: Vec<IrFreeExpr> = match result_expr {
            Expr::Union(_, _) | Expr::Set(_) => {
                let mut union_items = vec![];
                self.collect_union_items(result_expr, &mut union_items)?;
                // Type-check UNION operands: all scalar branches must be in the same type family.
                let mut first: Option<(String, String)> = None; // (pg_type, pyql_name)
                for item in &union_items {
                    if let IrFreeExpr::Scalar(expr) = item {
                        if let Some(t) = infer_ir_type(expr) {
                            let t = t.to_string();
                            if let Some((ft, fq)) = &first {
                                if !types_compatible(ft, &t) {
                                    return Err(PyQLError::Type(PyQLTypeError {
                                        message: format!(
                                            "operator 'UNION' cannot be applied to operands of type '{}' and '{}'",
                                            fq,
                                            pg_type_to_pyql(&t),
                                        ),
                                        position: Position { line: 0, col: 0 },
                                    }));
                                }
                            } else {
                                first = Some((t.clone(), pg_type_to_pyql(&t).to_string()));
                            }
                        }
                    }
                }
                union_items
            }
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if self.cte_types.get(n.as_str()).map(|t| !t.contains("::")).unwrap_or(false) {
                        vec![IrFreeExpr::CtePassthrough(n.clone())]
                    } else {
                        vec![IrFreeExpr::Scalar(self.compile_free_expr(result_expr)?)]
                    }
                } else {
                    vec![IrFreeExpr::Scalar(self.compile_free_expr(result_expr)?)]
                }
            }
            Expr::Shape(s) if s.expr.is_none() => {
                let fields = s
                    .elements
                    .iter()
                    .map(|el| -> Result<(String, IrExpr), PyQLError> {
                        let name = path_leaf(&el.path)?.to_string();
                        let expr = el.compexpr.as_ref().ok_or_else(|| {
                            self.type_err(
                                "free object field must have a value expression (':= expr')",
                            )
                        })?;
                        Ok((name, self.compile_free_expr(expr)?))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                vec![IrFreeExpr::FreeObject(fields)]
            }
            Expr::Tuple(exprs) => {
                let ir = exprs
                    .iter()
                    .map(|e| self.compile_free_expr(e))
                    .collect::<Result<_, _>>()?;
                vec![IrFreeExpr::Tuple(ir)]
            }
            Expr::NamedTuple(fields) => {
                let ir = fields
                    .iter()
                    .map(|(name, e)| Ok((name.clone(), self.compile_free_expr(e)?)))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                vec![IrFreeExpr::Scalar(IrExpr::NamedTuple(ir))]
            }
            other => vec![IrFreeExpr::Scalar(self.compile_free_expr(other)?)],
        };

        let order_by = sel
            .order_by
            .iter()
            .map(|s| -> Result<IrSort, PyQLError> {
                Ok(IrSort {
                    expr: self.compile_free_expr(&s.expr)?,
                    direction: match s.direction {
                        SortDirection::Asc => IrSortDir::Asc,
                        SortDirection::Desc => IrSortDir::Desc,
                    },
                    nulls: match s.nones {
                        NonesOrder::First => IrNulls::First,
                        NonesOrder::Last => IrNulls::Last,
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let offset = sel
            .offset
            .as_ref()
            .map(|e| self.compile_free_expr(e))
            .transpose()?;
        let limit = sel
            .limit
            .as_ref()
            .map(|e| self.compile_free_expr(e))
            .transpose()?;

        Ok(IrFreeSelect { items, order_by, offset, limit, distinct })
    }

    /// Compile an expression that has no schema type context (no .name references).
    fn compile_free_expr(&mut self, expr: &Expr) -> Result<IrExpr, PyQLError> {
        match expr {
            Expr::Literal(lit) => Ok(IrExpr::Literal(match lit {
                Literal::Str(s) => IrLiteral::Str(s.clone()),
                Literal::Int(n) => IrLiteral::Int(*n),
                Literal::Float(f) => IrLiteral::Float(*f),
                Literal::Bool(b) => IrLiteral::Bool(*b),
            })),

            Expr::Parameter(name) => {
                let index = self.param_index(name);
                Ok(IrExpr::Param { index })
            }

            Expr::Global(name) => self.compile_global(name),

            Expr::Index { expr: e, index: i } => {
                let ir_expr = self.compile_free_expr(e)?;
                let ir_index = self.compile_free_expr(i)?;
                let is_array = is_array_expr(&ir_expr);
                Ok(IrExpr::Subscript { expr: Box::new(ir_expr), index: Box::new(ir_index), is_array })
            }

            Expr::Slice { expr: e, lower: lo, upper: hi } => {
                let ir_expr = self.compile_free_expr(e)?;
                let is_array = is_array_expr(&ir_expr);
                let ir_lower = lo.as_ref().map(|x| self.compile_free_expr(x)).transpose()?;
                let ir_upper = hi.as_ref().map(|x| self.compile_free_expr(x)).transpose()?;
                Ok(IrExpr::Slice {
                    expr: Box::new(ir_expr),
                    lower: ir_lower.map(Box::new),
                    upper: ir_upper.map(Box::new),
                    is_array,
                })
            }

            Expr::FunctionCall(f) => {
                // sequence_next / sequence_reset: type-ref arg → nextval/setval SQL
                if (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && (f.name == "sequence_next" || f.name == "sequence_reset")
                {
                    return self.compile_sequence_fn(f);
                }

                // assert_single with SubQuery arg → _pylon.assert_single(ARRAY(subquery))
                if (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && f.name == "assert_single"
                    && f.args.len() >= 1
                {
                    if let Expr::SubQuery(inner_stmt) = &f.args[0] {
                        let inner = self.compile_subquery_to_array_source(inner_stmt)?;
                        return Ok(IrExpr::FunctionCall(IrFunctionCall {
                            schema: Some("_pylon".to_string()),
                            name: "assert_single".to_string(),
                            args: vec![IrExpr::ArrayFromSelect(Box::new(inner))],
                            sql_template: None,
                        }));
                    }
                }
                // count(TypeName) or count((select TypeName ...))
                // Aggregate function with a single schema-type ref or subquery arg → AggOverQuery.
                if f.args.len() == 1 {
                    let arg = &f.args[0];
                    let inner_sel: Option<ast::SelectStmt> = match arg {
                        Expr::Path(p) if !p.partial => {
                            // Resolve as a schema type if it matches a known type (not enum).
                            let qname = p.steps.iter().filter_map(|s| {
                                if let ast::PathStep::Name(n) = s { Some(n.as_str()) } else { None }
                            }).collect::<Vec<_>>().join("::");
                            let is_schema_type = self.schema.types.iter().any(|t| {
                                format!("{}::{}", t.module, t.name) == qname || t.name == qname
                            });
                            if is_schema_type {
                                Some(ast::SelectStmt {
                                    result: arg.clone(),
                                    filter: None,
                                    order_by: vec![],
                                    offset: None,
                                    limit: None,
                                })
                            } else {
                                None
                            }
                        }
                        Expr::SubQuery(stmt) => {
                            if let ast::Stmt::Select(inner) = stmt.as_ref() {
                                Some(inner.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(sel) = inner_sel {
                        use crate::stdlib::{lookup, ImplStrategy};
                        let ns = f.module.as_deref().unwrap_or("std");
                        let overloads = lookup(ns, &f.name);
                        let best = overloads.iter().find(|d| d.params.len() == 1).or_else(|| overloads.first());
                        if let Some(d) = best {
                            if let ImplStrategy::SqlBuiltin(sql_name) = &d.impl_strategy {
                                let fn_name = sql_name.to_string();
                                let inner_ir = self.compile_select(&sel, false)?;
                                return Ok(IrExpr::AggOverQuery { fn_name, inner: Box::new(inner_ir) });
                            }
                        }
                    }
                }

                // If any argument is a set literal, this must be an aggregate.
                // Compile as AggOverSet rather than a regular function call.
                let set_arg_idx = f.args.iter().position(|a| matches!(a, Expr::Set(_)));
                if let Some(idx) = set_arg_idx {
                    if let Expr::Set(set_elems) = &f.args[idx] {
                        use crate::stdlib::{lookup, ImplStrategy};
                        let ns = f.module.as_deref().unwrap_or("std");
                        let overloads = lookup(ns, &f.name);
                        let best = overloads
                            .iter()
                            .find(|d| d.params.len() == f.args.len())
                            .or_else(|| overloads.first());
                        let (schema, fn_name) = match best.map(|d| &d.impl_strategy) {
                            Some(ImplStrategy::SqlBuiltin(sql_name)) =>
                                (None, sql_name.to_string()),
                            Some(_) => return Err(self.type_err(&format!(
                                "function '{}::{}' cannot be called with a set literal in this context",
                                ns, f.name
                            ))),
                            None => return Err(self.type_err(&format!(
                                "function '{}::{}' does not exist", ns, f.name
                            ))),
                        };
                        let elems = set_elems
                            .iter()
                            .map(|e| self.compile_free_expr(e))
                            .collect::<Result<Vec<_>, _>>()?;
                        return Ok(IrExpr::AggOverSet { fn_name, schema, elems });
                    }
                }
                let args = f
                    .args
                    .iter()
                    .map(|a| self.compile_free_expr(a))
                    .collect::<Result<Vec<_>, _>>()?;
                self.resolve_fn_call(f.module.as_deref(), &f.name, args)
            }

            Expr::TypeCast(tc) => {
                // `<AnyType>{}` — an empty set cast to any type, e.g. clearing
                // an optional link (`<Company>{}`) — is always just NULL,
                // regardless of what pg_type the cast target would otherwise
                // resolve to (a schema object type name isn't a scalar cast
                // target at all, so resolve_cast_pg_type couldn't handle it
                // below anyway). Generalizes the same bare-`{}`-in-assignment-
                // position special case in compile_assignments_inner to any
                // expression context, matching real EdgeQL semantics.
                if matches!(&tc.expr, Expr::Set(elems) if elems.is_empty()) {
                    return Ok(IrExpr::Null);
                }
                if let ast::TypeExpr::Tuple { elements } = &tc.ty {
                    if let Some(ir) = self.try_compile_tuple_literal_cast_free(elements, &tc.expr)? {
                        let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                        let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                        return Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: ir, pg_type, tuple_shape })));
                    }
                }
                if let ast::TypeExpr::Array { element } = &tc.ty {
                    if let Some(ir) = self.try_compile_array_literal_cast_free(element, &tc.expr)? {
                        let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                        return Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: ir, pg_type, tuple_shape: None })));
                    }
                }
                let inner = self.compile_free_expr(&tc.expr)?;
                let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape })))
            }

            Expr::BinOp(b) => {
                let left = self.compile_free_expr(&b.left)?;
                let right = self.compile_free_expr(&b.right)?;
                if let (Some(lt), Some(rt)) = (infer_ir_type(&left), infer_ir_type(&right)) {
                    if !types_compatible(lt, rt) {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "operator '{op}' cannot be applied to operands of type \
                                 '{lq}' and '{rq}'",
                                op = b.op,
                                lq = pg_type_to_pyql(lt),
                                rq = pg_type_to_pyql(rt),
                            ),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                }
                Ok(IrExpr::BinOp(Box::new(IrBinOp { left, op: b.op.clone(), right })))
            }

            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Exists => {
                self.compile_exists_free(&u.operand)
            }

            Expr::UnaryOp(u) => {
                let operand = self.compile_free_expr(&u.operand)?;
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: u.op.clone(), operand })))
            }

            Expr::IfElse(ie) => {
                let condition = self.compile_free_expr(&ie.condition)?;
                let if_ = self.compile_free_expr(&ie.if_expr)?;
                let else_ = self.compile_free_expr(&ie.else_expr)?;
                Ok(IrExpr::IfElse(Box::new(IrIfElse { condition, if_, else_ })))
            }

            Expr::Array(elems) => {
                let items = elems
                    .iter()
                    .map(|e| self.compile_free_expr(e))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::Array(items))
            }

            Expr::NamedTuple(fields) => {
                let ir = fields
                    .iter()
                    .map(|(name, e)| Ok((name.clone(), self.compile_free_expr(e)?)))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::NamedTuple(ir))
            }

            Expr::Tuple(elems) => {
                let ir = elems
                    .iter()
                    .map(|e| self.compile_free_expr(e))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::Tuple(ir))
            }

            Expr::FieldAccess { expr: inner, field } => {
                if let Expr::NamedTuple(fields) = inner.as_ref() {
                    let (_, val) = fields.iter().find(|(k, _)| k == field).ok_or_else(|| {
                        self.type_err(&format!(
                            "{field} is not a member of {}",
                            named_tuple_type_str(fields)
                        ))
                    })?;
                    return self.compile_free_expr(val);
                }
                let ir = self.compile_free_expr(inner)?;
                Ok(IrExpr::JsonbField { expr: Box::new(ir), field: field.clone() })
            }

            Expr::TupleIndex { expr: inner, index } => {
                match inner.as_ref() {
                    Expr::Tuple(elems) => {
                        let elem = elems.get(*index).ok_or_else(|| {
                            self.type_err(&format!(
                                "{index} is not a member of {}",
                                positional_tuple_type_str(elems)
                            ))
                        })?;
                        self.compile_free_expr(elem)
                    }
                    Expr::NamedTuple(fields) => {
                        let (_, val) = fields.get(*index).ok_or_else(|| {
                            self.type_err(&format!(
                                "{index} is not a member of {}",
                                named_tuple_type_str(fields)
                            ))
                        })?;
                        self.compile_free_expr(val)
                    }
                    // Not a literal to constant-fold — emit a generic runtime
                    // jsonb positional access (`$param.1`, `(<tuple<...>>expr).1`, …).
                    // When the source is a cast to a statically-known tuple type,
                    // bounds-check the index against its arity at compile time
                    // (matches Gel: `2 is not a member of tuple<std::int64, std::str>`).
                    _ => {
                        if let Expr::TypeCast(tc) = inner.as_ref() {
                            if let Some(shape) = self.resolve_tuple_cast_shape(&tc.ty) {
                                if *index >= shape.members.len() {
                                    return Err(self.type_err(&format!(
                                        "{index} is not a member of {}",
                                        self.type_expr_to_display_str(&tc.ty)
                                    )));
                                }
                            }
                        }
                        let ir = self.compile_free_expr(inner)?;
                        Ok(IrExpr::JsonbIndex { expr: Box::new(ir), index: *index })
                    }
                }
            }

            Expr::Path(p) if p.partial => Err(self.type_err(
                "property reference (.name) is not valid in free SELECT; \
                 use a schema-bound SELECT instead",
            )),

            // Enum member access in free context: `default::Gender.Female`
            Expr::Path(p) if !p.partial && p.steps.len() == 2 => {
                if let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice() {
                    if self.resolve_enum(type_ref).is_some() {
                        return self.compile_enum_access(type_ref, variant);
                    }
                }
                Err(self.type_err("expression is not valid in free SELECT context"))
            }

            // CTE name, for-loop variable, or function parameter used as a value in free context
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if self.for_vars.contains_key(n.as_str()) {
                        return Ok(IrExpr::ForVar { name: n.clone() });
                    }
                    if let Some(t) = self.cte_types.get(n.as_str()) {
                        let scalar = !t.contains("::");
                        return Ok(IrExpr::CteRef { name: n.clone(), scalar });
                    }
                    if let Some(pg_type) = self.fn_params.get(n.as_str()) {
                        return Ok(IrExpr::FnParam { name: n.clone(), pg_type: pg_type.clone() });
                    }
                }
                Err(self.type_err("expression is not valid in free SELECT context"))
            }

            Expr::Set(elems) if elems.is_empty() => Ok(IrExpr::Null),

            Expr::Set(elems) => {
                let compiled: Result<Vec<_>, _> =
                    elems.iter().map(|e| self.compile_free_expr(e)).collect();
                let mut compiled = compiled?;
                if compiled.len() == 1 {
                    Ok(compiled.remove(0))
                } else {
                    Err(self.type_err(
                        "multi-element set literal is not supported in free SELECT context",
                    ))
                }
            }

            // detached has no effect in already-free context
            Expr::Detached(inner) => self.compile_free_expr(inner),

            _ => Err(self.type_err("expression is not valid in free SELECT context")),
        }
    }

    // ── SELECT ────────────────────────────────────────────────────────────────────

    fn compile_select(&mut self, sel: &ast::SelectStmt, distinct: bool) -> Result<IrSelect, PyQLError> {
        let result_expr = match &sel.result {
            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct => &u.operand,
            Expr::Detached(inner) => inner.as_ref(),
            other => other,
        };
        let (type_name, shape_elements, inner_stmt, cte_name) =
            self.extract_type_and_shape(result_expr)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let table = match cte_name {
            Some(ref cte) => format!("@cte:{}", cte),
            None => td.table.clone(),
        };
        let source = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table,
            alias: alias.clone(),
        };

        let shape = self.compile_shape(shape_elements, td, &alias, &td.module)?;

        let filter = sel
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;

        let order_by = sel
            .order_by
            .iter()
            .map(|s| self.compile_sort(s, td, &alias))
            .collect::<Result<Vec<_>, _>>()?;

        let offset = sel
            .offset
            .as_ref()
            .map(|e| self.compile_expr(e, td, &alias))
            .transpose()?;

        let limit = sel
            .limit
            .as_ref()
            .map(|e| self.compile_expr(e, td, &alias))
            .transpose()?;

        // Compile the inner DML if this is a SELECT-over-DML / SELECT-over-SELECT.
        let dml_source = inner_stmt
            .map(|s| self.compile_stmt(s).map(Box::new))
            .transpose()?;

        let polymorphic = td.abstract_ && td.materialized;
        let (poly_implementors, poly_columns) = if polymorphic {
            let iface_qname = format!("{}::{}", td.module, td.name);
            let implementors = self.find_poly_implementors(&iface_qname);
            let columns: Vec<String> = td.properties.iter().map(|p| p.name.clone())
                .chain(td.links.iter().map(|l| format!("{}_id", l.name)))
                .collect();
            (implementors, columns)
        } else {
            (vec![], vec![])
        };

        Ok(IrSelect { source, shape, filter, order_by, offset, limit, distinct, dml_source, polymorphic, poly_implementors, poly_columns })
    }

    /// Unwrap `Shape(expr, elements)` or bare `Path` from a SELECT result.
    /// Returns (type_name, shape_elements, optional_inner_stmt, optional_cte_name).
    /// The inner stmt is Some when the subject is `(INSERT …)` / `(SELECT …)` etc.
    /// The cte_name is Some when the subject is a WITH-block CTE reference.
    fn extract_type_and_shape<'e>(
        &self,
        expr: &'e Expr,
    ) -> Result<(String, &'e [ShapeElement], Option<&'e Stmt>, Option<String>), PyQLError> {
        match expr {
            Expr::Shape(s) => {
                // s.expr is Option<Expr> (not Box), so use as_ref() not as_deref()
                let (type_name, cte_name, inner) = match s.expr.as_ref() {
                    Some(Expr::SubQuery(stmt)) => {
                        (self.dml_subject_type(stmt)?, None, Some(stmt.as_ref()))
                    }
                    Some(Expr::Path(p)) if !p.partial && p.steps.len() == 1 => {
                        if let ast::PathStep::Name(n) = &p.steps[0] {
                            if let Some(t) = self.cte_types.get(n.as_str()) {
                                if t.contains("::") {
                                    (t.clone(), Some(n.clone()), None)
                                } else {
                                    (self.expr_as_type_name(s.expr.as_ref().unwrap())?, None, None)
                                }
                            } else {
                                (self.expr_as_type_name(s.expr.as_ref().unwrap())?, None, None)
                            }
                        } else {
                            (self.expr_as_type_name(s.expr.as_ref().unwrap())?, None, None)
                        }
                    }
                    Some(inner) => (self.expr_as_type_name(inner)?, None, None),
                    None => {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "shape without subject expression".into(),
                            position: Position { line: 0, col: 0 },
                        }))
                    }
                };
                Ok((type_name, &s.elements, inner, cte_name))
            }
            // Bare `SELECT (DML)` without an outer shape
            Expr::SubQuery(stmt) => {
                Ok((self.dml_subject_type(stmt)?, &[], Some(stmt.as_ref()), None))
            }
            // Bare CTE object reference: `select cte_name`
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if let Some(t) = self.cte_types.get(n.as_str()) {
                        if t.contains("::") {
                            return Ok((t.clone(), &[], None, Some(n.clone())));
                        }
                    }
                }
                Ok((self.expr_as_type_name(expr)?, &[], None, None))
            }
            _ => Ok((self.expr_as_type_name(expr)?, &[], None, None)),
        }
    }

    fn compile_for(&mut self, f: &ast::ForStmt) -> Result<IrFor, PyQLError> {
        // Compile the iterator expression to determine scalar type and VALUES list.
        let (exprs, pg_type) = match &f.iterator {
            Expr::Set(elems) => {
                let compiled: Result<Vec<_>, _> =
                    elems.iter().map(|e| self.compile_free_expr(e)).collect();
                let compiled = compiled?;
                let raw = compiled.first()
                    .and_then(|e| infer_ir_type(e))
                    .unwrap_or("text");
                let pg_type = literal_sentinel_to_pg(raw).to_string();
                (compiled, pg_type)
            }
            other => {
                let e = self.compile_free_expr(other)?;
                let raw = infer_ir_type(&e).unwrap_or("text");
                let pg_type = literal_sentinel_to_pg(raw).to_string();
                (vec![e], pg_type)
            }
        };

        // Register the for variable so the body can reference it.
        let prev = self.for_vars.insert(f.var.clone(), pg_type.clone());
        let body = self.compile_stmt(&f.body)?;
        // Restore previous for-var (or remove if none existed).
        match prev {
            Some(old) => { self.for_vars.insert(f.var.clone(), old); }
            None => { self.for_vars.remove(&f.var); }
        }

        Ok(IrFor {
            var_name: f.var.clone(),
            iterator: IrForIterator::Values { exprs, pg_type },
            body: Box::new(body),
        })
    }

    fn compile_group(&mut self, g: &ast::GroupStmt) -> Result<IrGroup, PyQLError> {
        // Resolve the subject type — may be a schema type or a CTE alias.
        let (type_name, cte_name) = match &g.subject {
            Expr::Path(p) if !p.partial => {
                match p.steps.as_slice() {
                    [ast::PathStep::Name(n)] => {
                        if let Some(t) = self.cte_types.get(n.as_str()) {
                            (t.clone(), Some(n.clone()))
                        } else {
                            (n.clone(), None)
                        }
                    }
                    [ast::PathStep::Name(m), ast::PathStep::Name(n)] =>
                        (format!("{}::{}", m, n), None),
                    _ => return Err(PyQLError::Type(PyQLTypeError {
                        message: format!("unsupported group subject: {:?}", g.subject),
                        position: Position { line: 0, col: 0 },
                    })),
                }
            }
            _ => return Err(PyQLError::Type(PyQLTypeError {
                message: "group subject must be a type name".to_string(),
                position: Position { line: 0, col: 0 },
            })),
        };

        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let fq_type_name = format!("{}::{}", td.module, td.name);
        let table = match cte_name {
            Some(ref cte) => format!("@cte:{}", cte),
            None => td.table.clone(),
        };
        let source = IrSource {
            type_name: fq_type_name.clone(),
            table,
            alias: alias.clone(),
        };
        let module = td.module.clone();

        // Compile the element shape. No explicit shape → implicit { id }, matching Gel semantics.
        let shape = self.compile_shape(
            g.shape.as_deref().unwrap_or(&[]),
            td,
            &alias,
            &module,
        )?;

        // Build a map from using-alias → compiled expression.
        let td = self.resolve_type(&type_name)?;
        let mut using_map: HashMap<String, IrExpr> = HashMap::new();
        for (alias_name, expr) in &g.using {
            let ir = self.compile_expr(expr, td, &alias)?;
            using_map.insert(alias_name.clone(), ir);
        }

        // Compile BY keys: each is either an Ident (using-alias ref) or a partial Path (.prop).
        let mut keys: Vec<(String, IrExpr)> = vec![];
        let td = self.resolve_type(&type_name)?;
        for by_expr in &g.by {
            match by_expr {
                Expr::Path(p) if p.partial && p.steps.len() == 1 => {
                    if let ast::PathStep::Name(prop) = &p.steps[0] {
                        // `.prop` shorthand: infer alias = prop name, expr = column ref.
                        let ir = self.compile_expr(by_expr, td, &alias)?;
                        keys.push((prop.clone(), ir));
                    } else {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "group by path must be a simple property".to_string(),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                }
                Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                    if let ast::PathStep::Name(name) = &p.steps[0] {
                        // Bare ident: must be a using alias.
                        let ir = using_map.get(name).ok_or_else(|| PyQLError::Type(PyQLTypeError {
                            message: format!("group by references unknown alias '{}'", name),
                            position: Position { line: 0, col: 0 },
                        }))?;
                        keys.push((name.clone(), ir.clone()));
                    } else {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "group by identifier must be a simple name".to_string(),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                }
                _ => return Err(PyQLError::Type(PyQLTypeError {
                    message: format!("unsupported group by expression: {:?}", by_expr),
                    position: Position { line: 0, col: 0 },
                })),
            }
        }

        Ok(IrGroup { source, shape, keys })
    }

    /// Extract the target type name from a DML or inner SELECT statement.
    fn dml_subject_type(&self, stmt: &Stmt) -> Result<String, PyQLError> {
        match stmt {
            Stmt::Insert(ins) => Ok(ins.subject.name.clone()),
            Stmt::Update(upd) => self.expr_as_type_name(&upd.subject),
            Stmt::Delete(del) => self.expr_as_type_name(&del.subject),
            Stmt::With(w) => self.dml_subject_type(&w.stmt),
            Stmt::For(f) => self.dml_subject_type(&f.body),
            Stmt::Group(g) => self.expr_as_type_name(&g.subject),
            Stmt::Select(sel) => {
                // <Module::Type>expr — type name comes from the cast target
                if let Expr::TypeCast(tc) = &sel.result {
                    if let Some((module, name)) = tc.ty.as_named() {
                        if module.map(|m| m != "std").unwrap_or(false) {
                            return Ok(name.to_string());
                        }
                    }
                }
                // SELECT-over-SELECT: get the type from the inner select's result
                let (type_name, _, _, _) = self.extract_type_and_shape(&sel.result)?;
                Ok(type_name)
            }
        }
    }

    fn expr_as_type_name(&self, expr: &Expr) -> Result<String, PyQLError> {
        match expr {
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    return Ok(n.clone());
                }
                Err(self.type_err("expected a type name"))
            }
            _ => Err(self.type_err("expected a type name as SELECT subject")),
        }
    }

    // ── INSERT ────────────────────────────────────────────────────────────────────

    fn compile_insert(&mut self, ins: &ast::InsertStmt) -> Result<IrInsert, PyQLError> {
        let type_name = format!("{}", ins.subject.name);
        let td = self.resolve_type(&type_name)?;
        if td.abstract_ && td.materialized {
            return Err(self.type_err(&format!(
                "cannot insert into interface type '{}::{}'; insert into a concrete type instead",
                td.module, td.name
            )));
        }
        let alias = self.fresh_alias();
        let target = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        // Multi-link shape elements (`tags := ...` / `tags += ...`) populate
        // the junction table once the row exists; everything else goes
        // through the normal scalar/link assignment path.
        let mut multi_link_appends = vec![];
        let mut scalar_elements: Vec<ShapeElement> = vec![];
        for el in &ins.shape {
            let pointer_name = match path_leaf(&el.path) {
                Ok(n) => n,
                Err(_) => { scalar_elements.push(el.clone()); continue; }
            };
            if let Some(ml) = Self::resolve_multilink(td, pointer_name) {
                match el.op {
                    ShapeOp::Remove => return Err(self.type_err(&format!(
                        "cannot use `-=` for multi-link '{pointer_name}' in an insert; \
                         there is nothing to remove from yet"
                    ))),
                    ShapeOp::Assign | ShapeOp::Append => {
                        if let Some(expr) = &el.compexpr {
                            let (jt, module, src_col, tgt_col, through_td) =
                                self.multilink_junction_info(td, ml)?;
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            multi_link_appends.push(IrMultiLinkMutation {
                                junction_table: jt, module, source_col: src_col,
                                target_col: tgt_col, values,
                            });
                        }
                    }
                }
            } else {
                scalar_elements.push(el.clone());
            }
        }

        let assignments = self.compile_assignments(&scalar_elements, td, &alias)?;
        // Compile INSERT rewrites; substitute column refs so they are valid in VALUES.
        let assignment_map: HashMap<String, IrExpr> =
            assignments.iter().map(|(c, e)| (c.clone(), e.clone())).collect();
        let rewrites = self.compile_rewrites(td, &alias, 1)?
            .into_iter()
            .map(|rw| IrRewrite {
                column: rw.column,
                expr: substitute_col_refs(rw.expr, &assignment_map),
            })
            .collect();
        let unless_conflict = ins.unless_conflict.as_ref()
            .map(|uc| self.compile_conflict(uc, td))
            .transpose()?;
        let returning = Self::pk_returning(td);
        let type_name = format!("{}::{}", td.module, td.name);
        let enqueue_vector = td.vector_indexes.iter()
            .map(|vi| VectorEnqueueInfo {
                type_name: type_name.clone(),
                index_name: vi.index_name.clone(),
            })
            .collect();
        let enqueue_search = collect_search_enqueue(td, &type_name, "index");

        Ok(IrInsert {
            target,
            assignments,
            unless_conflict,
            rewrites,
            returning,
            enqueue_vector,
            enqueue_search,
            multi_link_appends,
        })
    }

    fn compile_assignments(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        self.compile_assignments_inner(elements, td, alias, false)
    }

    fn compile_assignments_for_update(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        self.compile_assignments_inner(elements, td, alias, true)
    }

    fn compile_assignments_inner(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
        deny_readonly: bool,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        elements
            .iter()
            .map(|el| {
                let pointer_name = path_leaf(&el.path)?;
                let expr = el.compexpr.as_ref().ok_or_else(|| {
                    PyQLError::Type(PyQLTypeError {
                        message: format!("INSERT pointer '{pointer_name}' has no value expression"),
                        position: Position { line: 0, col: 0 },
                    })
                })?;

                // Validate the pointer exists
                let column = if let Some(p) = Self::resolve_property(td, pointer_name) {
                    // A primary-key ("id") property: an UPDATE never allows
                    // reassigning it (deny_readonly is true there, regardless
                    // of the session config); an INSERT only allows an
                    // explicit value when allow_user_specified_id is set —
                    // matches Gel's own `allow_user_specified_id` semantics.
                    if p.is_pk && (deny_readonly || !self.config.allow_user_specified_id) {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "cannot assign to property 'id'".to_string(),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                    if deny_readonly && p.is_readonly {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: format!("cannot update property '{pointer_name}': it is declared as read-only"),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                    p.name.clone()
                } else if let Some(l) = Self::resolve_link(td, pointer_name) {
                    if deny_readonly && l.is_readonly {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: format!("cannot update link '{pointer_name}': it is declared as read-only"),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                    // Link assignment via subquery: `company := (SELECT Company FILTER ...)`
                    // Compile as a scalar subquery returning the target pk (the FK uuid).
                    let fk_col = format!("{}_id", l.name);
                    if let Expr::SubQuery(inner_stmt) = expr {
                        let ir_expr = self.compile_link_subquery(inner_stmt)?;
                        return Ok((fk_col, ir_expr));
                    }
                    fk_col
                } else {
                    return Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)));
                };

                // `{}` (empty set) in assignment position means NULL.
                let ir_expr = if matches!(expr, Expr::Set(v) if v.is_empty()) {
                    IrExpr::Null
                } else {
                    self.compile_expr(expr, td, alias)?
                };
                Ok((column, ir_expr))
            })
            .collect()
    }

    // ── UPDATE ────────────────────────────────────────────────────────────────────

    fn compile_update(&mut self, upd: &ast::UpdateStmt) -> Result<IrUpdate, PyQLError> {
        let type_name = self.expr_as_type_name(&upd.subject)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let target = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        let filter = upd
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;

        // Classify shape elements by kind.
        let mut multi_link_clears = vec![];
        let mut multi_link_replaces = vec![];
        let mut multi_link_appends = vec![];
        let mut multi_link_removals = vec![];
        let mut scalar_elements: Vec<ShapeElement> = vec![];

        for el in &upd.shape {
            let pointer_name = match path_leaf(&el.path) {
                Ok(n) => n,
                Err(_) => { scalar_elements.push(el.clone()); continue; }
            };

            if let Some(ml) = Self::resolve_multilink(td, pointer_name) {
                let (jt, module, src_col, tgt_col, through_td) =
                    self.multilink_junction_info(td, ml)?;

                match el.op {
                    ShapeOp::Assign => {
                        let is_empty = el.compexpr.as_ref()
                            .map(|e| matches!(e, Expr::Set(v) if v.is_empty()))
                            .unwrap_or(false);
                        if is_empty {
                            // := {} — clear all junction rows
                            multi_link_clears.push(IrMultiLinkClear {
                                junction_table: jt,
                                module,
                                source_col: src_col,
                            });
                        } else if let Some(expr) = &el.compexpr {
                            // := expr — replace (clear + insert)
                            multi_link_clears.push(IrMultiLinkClear {
                                junction_table: jt.clone(),
                                module: module.clone(),
                                source_col: src_col.clone(),
                            });
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            multi_link_replaces.push(IrMultiLinkMutation {
                                junction_table: jt, module, source_col: src_col,
                                target_col: tgt_col, values,
                            });
                        }
                    }
                    ShapeOp::Append => {
                        if let Some(expr) = &el.compexpr {
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            multi_link_appends.push(IrMultiLinkMutation {
                                junction_table: jt, module, source_col: src_col,
                                target_col: tgt_col, values,
                            });
                        }
                    }
                    ShapeOp::Remove => {
                        if let Some(expr) = &el.compexpr {
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            if has_any_link_props(&values) {
                                return Err(self.type_err(
                                    "link properties (`@prop := value`) cannot be assigned \
                                     when removing a link (`-=`)"
                                ));
                            }
                            multi_link_removals.push(IrMultiLinkMutation {
                                junction_table: jt, module, source_col: src_col,
                                target_col: tgt_col, values,
                            });
                        }
                    }
                }
            } else {
                scalar_elements.push(el.clone());
            }
        }

        let assignments = self.compile_assignments_for_update(&scalar_elements, td, &alias)?;
        let assignment_map: HashMap<String, IrExpr> =
            assignments.iter().map(|(c, e)| (c.clone(), e.clone())).collect();
        let rewrites = self.compile_rewrites(td, &alias, 2)?
            .into_iter()
            .map(|rw| IrRewrite {
                column: rw.column,
                expr: substitute_col_refs(rw.expr, &assignment_map),
            })
            .collect();
        let returning = Self::pk_returning(td);

        let poly_implementors = if td.abstract_ && td.materialized {
            self.find_poly_implementors(&format!("{}::{}", td.module, td.name))
        } else {
            vec![]
        };

        // Only enqueue indexes whose source pointers are touched by this update.
        let written_cols: std::collections::HashSet<&str> =
            assignments.iter().map(|(c, _)| c.as_str()).collect();
        let type_name = format!("{}::{}", td.module, td.name);
        let enqueue_vector = td.vector_indexes.iter()
            .filter(|vi| vi.pointers.iter().any(|f| written_cols.contains(f.as_str())))
            .map(|vi| VectorEnqueueInfo {
                type_name: type_name.clone(),
                index_name: vi.index_name.clone(),
            })
            .collect();
        let enqueue_search = collect_search_enqueue(td, &type_name, "index");

        Ok(IrUpdate {
            target, filter, assignments, rewrites, returning,
            multi_link_clears, multi_link_replaces,
            multi_link_appends, multi_link_removals,
            poly_implementors,
            enqueue_vector,
            enqueue_search,
        })
    }

    /// Extract junction table info for a multi-link: (junction_table, module,
    /// source_col, target_col, through_td). `through_td` is the junction
    /// type's own TypeDescriptor for a `through(...)` multi-link (needed to
    /// validate/compile `@prop := expr` link-property assignments against its
    /// real properties) — `None` for a Standard (implicit) junction table,
    /// which has no user-declared properties at all.
    fn multilink_junction_info(
        &mut self,
        td: &TypeDescriptor,
        ml: &MultiLinkDescriptor,
    ) -> Result<(String, String, String, String, Option<&'a TypeDescriptor>), PyQLError> {
        match &ml.through {
            None => Ok((
                format!("{}.{}", td.table, ml.name),
                td.module.clone(),
                "source".to_string(),
                "target".to_string(),
                None,
            )),
            Some(through_qname) => {
                let through_td = self.resolve_type(through_qname)?;
                let src_type = format!("{}::{}", td.module, td.name);
                let source_col = through_td.links.iter()
                    .find(|l| l.target == src_type)
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "source".to_string());
                let tgt_type = &ml.target;
                // A self-referencing through-link (source type == target
                // type, e.g. Person.friends via a PersonFriend with two
                // Person-typed links) would otherwise match the same link
                // for both sides — prefer a differently-named one first,
                // matching the tie-break already used for the read-side
                // join resolution elsewhere in this file.
                let target_col = through_td.links.iter()
                    .find(|l| &l.target == tgt_type && l.name != source_col)
                    .or_else(|| through_td.links.iter().find(|l| &l.target == tgt_type))
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "target".to_string());
                Ok((through_td.table.clone(), through_td.module.clone(), source_col, target_col, Some(through_td)))
            }
        }
    }

    /// Compile the RHS of a multilink `+=`, `-=`, or `:= expr` into an
    /// `IrMultiLinkValues`. `td`/`alias` are the record being updated (link-
    /// property value expressions like `@weight := <float64>$w` compile
    /// against this scope, same as any other UPDATE SET assignment — they
    /// cannot reference the linked target's own properties, only the outer
    /// record's or bound params/literals). `through_td` is the junction
    /// type's own TypeDescriptor for a `through(...)` multi-link, or `None`
    /// for a Standard junction (which has no properties to assign).
    fn compile_multilink_values(
        &mut self,
        expr: &Expr,
        td: &'a TypeDescriptor,
        alias: &str,
        through_td: Option<&'a TypeDescriptor>,
    ) -> Result<IrMultiLinkValues, PyQLError> {
        // `a union b` — combine both sides; each keeps its own link_props
        // (different targets in one `+=` can carry different property values).
        if let Expr::Union(a, b) = expr {
            let left = self.compile_multilink_values(a, td, alias, through_td)?;
            let right = self.compile_multilink_values(b, td, alias, through_td)?;
            return Ok(IrMultiLinkValues {
                source: IrMultiLinkValueSource::Union(Box::new(left), Box::new(right)),
                link_props: vec![],
            });
        }

        // `expr { @prop := value, ... }` — link-property assignments layered
        // onto an inner target-selecting expression.
        if let Expr::Shape(shape) = expr {
            let inner_expr = shape.expr.as_ref().ok_or_else(|| {
                self.type_err("multilink value shape must have a base expression")
            })?;
            let mut inner = self.compile_multilink_values(inner_expr, td, alias, through_td)?;

            let Some(through) = through_td else {
                return Err(self.type_err(
                    "link properties (`@prop := value`) are only valid on a multi-link \
                     declared with `through(...)`"
                ));
            };

            for el in &shape.elements {
                let prop_name = match el.path.steps.as_slice() {
                    [ast::PathStep::LinkProp(name)] => name.clone(),
                    _ => return Err(self.type_err(
                        "only `@prop := value` link-property assignments are valid here"
                    )),
                };
                let prop = Self::resolve_property(through, &prop_name).ok_or_else(|| {
                    self.field_err(&prop_name, &format!("{}::{}", through.module, through.name))
                })?;
                if prop.is_readonly {
                    return Err(self.type_err(&format!(
                        "cannot set link property '{prop_name}': it is declared as read-only"
                    )));
                }
                let value_expr = el.compexpr.as_ref().ok_or_else(|| {
                    self.type_err(&format!("link property '{prop_name}' must be assigned a value"))
                })?;
                let ir_expr = self.compile_expr(value_expr, td, alias)?;
                inner.link_props.push((prop.name.clone(), ir_expr));
            }
            return Ok(inner);
        }

        // CTE reference: bare name matching a registered CTE
        if let Some(name) = self.resolve_cte_name(expr) {
            return Ok(IrMultiLinkValues {
                source: IrMultiLinkValueSource::CteRef(name.to_string()),
                link_props: vec![],
            });
        }

        // Parenthesised subquery
        if let Expr::SubQuery(inner) = expr {
            return match self.compile_stmt(inner)? {
                IrStmt::Select(s) => Ok(IrMultiLinkValues {
                    source: IrMultiLinkValueSource::Select(Box::new(s)),
                    link_props: vec![],
                }),
                IrStmt::PathSelect(ps) => Ok(IrMultiLinkValues {
                    source: IrMultiLinkValueSource::PathSelect(Box::new(ps)),
                    link_props: vec![],
                }),
                _ => Err(self.type_err(
                    "multilink value must resolve to a SELECT or path query"
                )),
            };
        }

        // Absolute path expression (type reference or path traversal)
        if let Expr::Path(p) = expr {
            if !p.partial {
                let fake_sel = ast::SelectStmt {
                    result: expr.clone(),
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                };
                return match self.compile_stmt(&Stmt::Select(fake_sel))? {
                    IrStmt::PathSelect(ps) => Ok(IrMultiLinkValues {
                        source: IrMultiLinkValueSource::PathSelect(Box::new(ps)),
                        link_props: vec![],
                    }),
                    IrStmt::Select(s) => Ok(IrMultiLinkValues {
                        source: IrMultiLinkValueSource::Select(Box::new(s)),
                        link_props: vec![],
                    }),
                    _ => Err(self.type_err("expected a path expression for multilink value")),
                };
            }
        }

        Err(self.type_err(
            "multilink value must be a CTE reference, parenthesised subquery, or type path"
        ))
    }

    // ── DELETE ────────────────────────────────────────────────────────────────────

    fn compile_delete(&mut self, del: &ast::DeleteStmt) -> Result<IrDelete, PyQLError> {
        let type_name = self.expr_as_type_name(&del.subject)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let target = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        let filter = del
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;

        let returning = Self::pk_returning(td);

        let poly_implementors = if td.abstract_ && td.materialized {
            self.find_poly_implementors(&format!("{}::{}", td.module, td.name))
        } else {
            vec![]
        };
        let qname = format!("{}::{}", td.module, td.name);
        let enqueue_search = collect_search_enqueue(td, &qname, "delete");

        Ok(IrDelete { target, filter, returning, poly_implementors, enqueue_search })
    }

    // ── Shape compilation ─────────────────────────────────────────────────────────

    fn compile_shape(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<Vec<IrShapePointer>, PyQLError> {
        if elements.is_empty() {
            // No explicit shape: implicit { id } only, matching Gel semantics.
            return Ok(Self::pk_returning(td));
        }

        let mut pointers = vec![];
        for el in elements {
            if let Some(splat) = &el.splat {
                // Check if this is a type-intersection splat: [is Type].*
                if let Some(ast::PathStep::TypeIntersection(type_ref)) = el.path.steps.first() {
                    let type_ref = type_ref.clone();
                    pointers.extend(self.compile_type_intersection_splat(&type_ref, splat, td, alias)?);
                } else {
                    pointers.extend(self.compile_splat(splat, td, alias, module)?);
                }
            } else {
                pointers.push(self.compile_shape_element(el, td, alias, module)?);
            }
        }
        Ok(pointers)
    }

    /// Expand `*` → all scalars; `**` → all scalars + all single links with implicit `{ id }`.
    fn compile_splat(
        &mut self,
        splat: &ast::Splat,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<Vec<IrShapePointer>, PyQLError> {
        let mut pointers: Vec<IrShapePointer> = td
            .properties
            .iter()
            .map(|p| IrShapePointer::Scalar(IrScalarPointer {
                alias: p.name.clone(),
                column: p.name.clone(),
                pg_type: p.pg_type.clone(),
                tuple_shape: self.resolve_property_tuple_shape(p),
            }))
            .collect();

        for cd in &td.computed.clone() {
            let expr_ast = crate::parse::parse_expr(&cd.expression)
                .map_err(|e| PyQLError::Syntax(e))?;
            let ir = self.compile_expr(&expr_ast, td, alias)?;
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                alias: cd.name.clone(),
                expr: ir,
            }));
        }

        if matches!(splat, ast::Splat::Deep) {
            for l in &td.links {
                let target_td = self.resolve_type(&l.target)?;
                let sub_alias = self.fresh_alias();
                let sub_shape = Self::pk_returning(target_td);
                let subquery = IrSelect {
                    source: IrSource {
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: sub_alias.clone(),
                    },
                    shape: sub_shape,
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    distinct: false,
                    dml_source: None,
                    polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                };
                pointers.push(IrShapePointer::SingleLink(IrSingleLinkPointer {
                    alias: l.name.clone(),
                    fk_column: format!("{}_id", l.name),
                    target_pk: "id".to_string(),
                    subquery,
                }));
            }

            for ml in &td.multilinks {
                let sub_alias = self.fresh_alias();
                let target_td = self.resolve_type(&ml.target)?;
                let sub_shape = Self::pk_returning(target_td);

                let join = if let Some(through_qname) = &ml.through {
                    let through_td = self.resolve_type(through_qname)?;
                    if through_td.junction {
                        IrMultiLinkJoin::Standard {
                            junction_table: through_td.table.clone(),
                            module: through_td.module.clone(),
                        }
                    } else {
                        let source_qname = format!("{}::{}", td.module, td.name);
                        let source_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == source_qname)
                            .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                                message: format!(
                                    "through type {through_qname} has no link to source type {source_qname}"
                                ),
                                position: Position { line: 0, col: 0 },
                            }))?
                            .name
                            .clone();
                        let target_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == ml.target && l.name != source_col)
                            .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                            .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                                message: format!(
                                    "through type {through_qname} has no link to target type {}",
                                    ml.target
                                ),
                                position: Position { line: 0, col: 0 },
                            }))?
                            .name
                            .clone();
                        IrMultiLinkJoin::Through {
                            junction_table: through_td.table.clone(),
                            module: through_td.module.clone(),
                            source_col,
                            target_col,
                        }
                    }
                } else {
                    IrMultiLinkJoin::Standard {
                        junction_table: format!("{}.{}", td.table, ml.name),
                        module: module.to_string(),
                    }
                };

                let subquery = IrSelect {
                    source: IrSource {
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: sub_alias.clone(),
                    },
                    shape: sub_shape,
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    distinct: false,
                    dml_source: None,
                    polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                };

                pointers.push(IrShapePointer::MultiLink(IrMultiLinkPointer {
                    alias: ml.name.clone(),
                    join,
                    subquery,
                    link_properties: vec![],
                }));
            }
        }

        Ok(pointers)
    }

    // ── Type-intersection helpers ─────────────────────────────────────────────────

    /// Expand `[is ConcreteType].*` → scalar subqueries for each non-inherited property.
    fn compile_type_intersection_splat(
        &mut self,
        type_ref: &ast::ObjectRef,
        splat: &ast::Splat,
        parent_td: &TypeDescriptor,
        parent_alias: &str,
    ) -> Result<Vec<IrShapePointer>, PyQLError> {
        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let concrete_td = self.resolve_type(&type_name)?;
        let concrete_qname = format!("{}::{}", concrete_td.module, concrete_td.name);
        let concrete_table = concrete_td.table.clone();

        // Interface properties: those in the parent (interface) td
        let interface_props: std::collections::HashSet<String> = parent_td.properties.iter()
            .map(|p| p.name.clone())
            .collect();

        // Emit scalar subquery for each property not already in the interface
        let props: Vec<_> = concrete_td.properties.iter()
            .filter(|p| !interface_props.contains(&p.name))
            .cloned()
            .collect();

        // For deep splat, also include links
        let links: Vec<_> = if matches!(splat, ast::Splat::Deep) {
            concrete_td.links.iter().cloned().collect()
        } else {
            vec![]
        };

        let mut pointers = vec![];
        for prop in props {
            let sub_alias = self.fresh_alias();
            let filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: sub_alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ColumnRef {
                    alias: parent_alias.to_string(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
            }));
            let subquery = IrSelect {
                source: IrSource {
                    type_name: concrete_qname.clone(),
                    table: concrete_table.clone(),
                    alias: sub_alias,
                },
                shape: vec![IrShapePointer::Scalar(IrScalarPointer {
                    alias: prop.name.clone(),
                    column: prop.name.clone(),
                    pg_type: prop.pg_type.clone(),
                    tuple_shape: self.resolve_property_tuple_shape(&prop),
                })],
                filter: Some(filter),
                order_by: vec![],
                offset: None,
                limit: None,
                distinct: false,
                dml_source: None,
                polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
            };
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                alias: prop.name.clone(),
                expr: IrExpr::Subquery(Box::new(subquery)),
            }));
        }

        // For deep splat, include single-link pointers as subqueries
        for link in links {
            let target_td = self.resolve_type(&link.target)?;
            let sub_alias = self.fresh_alias();
            let filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: sub_alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ColumnRef {
                    alias: parent_alias.to_string(),
                    column: format!("{}_id", link.name),
                    pg_type: "uuid".to_string(),
                },
            }));
            let sub_shape = Self::pk_returning(target_td);
            let subquery = IrSelect {
                source: IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias,
                },
                shape: sub_shape,
                filter: Some(filter),
                order_by: vec![],
                offset: None,
                limit: None,
                distinct: false,
                dml_source: None,
                polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
            };
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                alias: link.name.clone(),
                expr: IrExpr::Subquery(Box::new(subquery)),
            }));
        }

        Ok(pointers)
    }

    /// Compile `[is ConcreteType].pointer_name` → `IrShapePointer`.
    fn compile_type_intersection_pointer(
        &mut self,
        type_ref: &ast::ObjectRef,
        tail_steps: &[ast::PathStep],
        parent_alias: &str,
    ) -> Result<IrShapePointer, PyQLError> {
        let expr = self.compile_type_intersection_expr_steps(type_ref, tail_steps, parent_alias)?;
        // Alias is the last Name step
        let alias = match tail_steps.last() {
            Some(ast::PathStep::Name(n)) => n.clone(),
            _ => return Err(self.type_err("type intersection must end with a pointer name")),
        };
        Ok(IrShapePointer::Computed(IrComputedPointer { alias, expr }))
    }

    /// Compile `[is Type].name` as an `IrExpr` (for computed pointer / expression context).
    fn compile_type_intersection_expr(
        &mut self,
        steps: &[ast::PathStep],
        _td: &TypeDescriptor,
        parent_alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        use ast::PathStep;
        let type_ref = match steps.first() {
            Some(PathStep::TypeIntersection(tr)) => tr.clone(),
            _ => return Err(self.type_err("expected type intersection")),
        };
        self.compile_type_intersection_expr_steps(&type_ref, &steps[1..], parent_alias)
    }

    /// Shared: build scalar subquery for `[is ConcreteType]` + tail pointer steps.
    fn compile_type_intersection_expr_steps(
        &mut self,
        type_ref: &ast::ObjectRef,
        tail_steps: &[ast::PathStep],
        parent_alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        use ast::PathStep;
        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let concrete_td = self.resolve_type(&type_name)?;
        let concrete_qname = format!("{}::{}", concrete_td.module, concrete_td.name);
        let concrete_table = concrete_td.table.clone();

        let pointer_name = match tail_steps.first() {
            Some(PathStep::Name(n)) => n.as_str(),
            _ => return Err(self.type_err(
                "type intersection must be followed by a pointer name, e.g. [is Type].name"
            )),
        };

        let prop = concrete_td.properties.iter().find(|p| p.name == pointer_name)
            .ok_or_else(|| self.field_err(pointer_name, &concrete_qname))?;
        let prop_name = prop.name.clone();
        let prop_type = prop.pg_type.clone();

        let sub_alias = self.fresh_alias();
        let filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: sub_alias.clone(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef {
                alias: parent_alias.to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
        }));

        Ok(IrExpr::Subquery(Box::new(IrSelect {
            source: IrSource {
                type_name: concrete_qname,
                table: concrete_table,
                alias: sub_alias,
            },
            shape: vec![IrShapePointer::Scalar(IrScalarPointer {
                alias: prop_name.clone(),
                column: prop_name,
                pg_type: prop_type,
                tuple_shape: self.resolve_property_tuple_shape(prop),
            })],
            filter: Some(filter),
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        })))
    }

    fn compile_shape_element(
        &mut self,
        el: &ShapeElement,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<IrShapePointer, PyQLError> {
        // Type intersection pointer: [is Type].pointer_name (without compexpr)
        if let Some(ast::PathStep::TypeIntersection(type_ref)) = el.path.steps.first() {
            if el.compexpr.is_none() && el.path.steps.len() >= 2 {
                let type_ref = type_ref.clone();
                return self.compile_type_intersection_pointer(&type_ref, &el.path.steps[1..], alias);
            }
        }

        let pointer_name = path_leaf(&el.path)?;

        // __type__ is a virtual property: the fully-qualified type name as a string.
        // It's always injected at position 0 for internal use; explicit inclusion adds
        // it as a regular computed pointer at a later position so Python can read it.
        if pointer_name == "__type__" && el.compexpr.is_none() {
            let expr = if td.abstract_ && td.materialized {
                IrExpr::ColumnRef {
                    alias: alias.to_string(),
                    column: "__type__".to_string(),
                    pg_type: "text".to_string(),
                }
            } else {
                IrExpr::Literal(IrLiteral::Str(format!("{}::{}", td.module, td.name)))
            };
            return Ok(IrShapePointer::Computed(IrComputedPointer {
                alias: "__type__".to_string(),
                expr,
            }));
        }

        // Computed override: `pointer := expr`
        if let Some(compexpr) = &el.compexpr {
            // `alias := .multilink` → rename a multilink, same semantics as a regular pointer
            if let Expr::Path(p) = compexpr {
                if p.partial && p.steps.len() == 1 {
                    if let ast::PathStep::Name(ml_name) = &p.steps[0] {
                        if Self::resolve_multilink(td, ml_name).is_some() {
                            let ml_name = ml_name.clone();
                            return self.compile_multilink_pointer(
                                pointer_name, &ml_name, td, alias, module, el,
                            );
                        }
                    }
                }
            }
            let ir = self.compile_expr(compexpr, td, alias)?;
            // Cross-scope TypeIs: promote to set-valued shape pointer.
            if let IrExpr::ArrayFromSelect(src) = ir {
                if let IrArraySource::RawExpr { source, poly_implementors, poly_columns, expr } = *src {
                    return Ok(IrShapePointer::ScalarSet(IrScalarSetPointer {
                        alias: pointer_name.to_string(),
                        source,
                        poly_implementors,
                        poly_columns,
                        bool_expr: expr,
                    }));
                }
                return Ok(IrShapePointer::Computed(IrComputedPointer {
                    alias: pointer_name.to_string(),
                    expr: IrExpr::ArrayFromSelect(src),
                }));
            }
            return Ok(IrShapePointer::Computed(IrComputedPointer {
                alias: pointer_name.to_string(),
                expr: ir,
            }));
        }

        // Scalar property
        if let Some(p) = Self::resolve_property(td, pointer_name) {
            return Ok(IrShapePointer::Scalar(IrScalarPointer {
                alias: pointer_name.to_string(),
                column: p.name.clone(),
                pg_type: p.pg_type.clone(),
                tuple_shape: self.resolve_property_tuple_shape(p),
            }));
        }

        // Single link
        if let Some(l) = Self::resolve_link(td, pointer_name) {
            let target_td = self.resolve_type(&l.target)?;
            let sub_alias = self.fresh_alias();
            let nested_elements = el.nested.as_deref().unwrap_or(&[]);
            let sub_shape =
                self.compile_shape(nested_elements, target_td, &sub_alias, &target_td.module.clone())?;
            let subquery = IrSelect {
                source: IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias,
                },
                shape: sub_shape,
                filter: None,
                order_by: vec![],
                offset: None,
                limit: None,
                distinct: false,
                dml_source: None,
                polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
            };
            return Ok(IrShapePointer::SingleLink(IrSingleLinkPointer {
                alias: pointer_name.to_string(),
                fk_column: format!("{}_id", l.name),
                target_pk: "id".to_string(),
                subquery,
            }));
        }

        // Multi-link
        if Self::resolve_multilink(td, pointer_name).is_some() {
            return self.compile_multilink_pointer(pointer_name, pointer_name, td, alias, module, el);
        }

        // Schema-defined computed pointer
        if let Some(cd) = td.computed.iter().find(|c| c.name == pointer_name) {
            let expr_ast = crate::parse::parse_expr(&cd.expression)
                .map_err(|e| PyQLError::Syntax(e))?;
            let ir = self.compile_expr(&expr_ast, td, alias)?;
            return Ok(IrShapePointer::Computed(IrComputedPointer {
                alias: pointer_name.to_string(),
                expr: ir,
            }));
        }

        Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)))
    }

    fn compile_multilink_pointer(
        &mut self,
        output_alias: &str,
        ml_name: &str,
        td: &TypeDescriptor,
        _parent_alias: &str,
        module: &str,
        el: &ShapeElement,
    ) -> Result<IrShapePointer, PyQLError> {
        let ml = Self::resolve_multilink(td, ml_name)
            .expect("caller verified multilink exists")
            .clone();
        let target_td = self.resolve_type(&ml.target)?;
        let sub_alias = self.fresh_alias();
        let nested_elements = el.nested.as_deref().unwrap_or(&[]);

        // Partition @prop link-property elements from regular shape elements.
        let mut link_properties: Vec<IrLinkProp> = Vec::new();
        let mut regular_els: Vec<ShapeElement> = Vec::new();
        for nel in nested_elements {
            if let [ast::PathStep::LinkProp(name)] = nel.path.steps.as_slice() {
                link_properties.push(IrLinkProp { name: name.clone() });
            } else {
                regular_els.push(nel.clone());
            }
        }

        let sub_shape =
            self.compile_shape(&regular_els, target_td, &sub_alias, &target_td.module.clone())?;

        let join = if let Some(through_qname) = &ml.through {
            let through_td = self.resolve_type(through_qname)?;
            if through_td.junction {
                // Junction type: columns are always named `source` and `target`.
                IrMultiLinkJoin::Standard {
                    junction_table: through_td.table.clone(),
                    module: through_td.module.clone(),
                }
            } else {
                let source_qname = format!("{}::{}", td.module, td.name);
                let source_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                        message: format!(
                            "through type {through_qname} has no link to source type {source_qname}"
                        ),
                        position: Position { line: 0, col: 0 },
                    }))?
                    .name
                    .clone();
                let target_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == ml.target && l.name != source_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                    .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                        message: format!(
                            "through type {through_qname} has no link to target type {}",
                            ml.target
                        ),
                        position: Position { line: 0, col: 0 },
                    }))?
                    .name
                    .clone();
                IrMultiLinkJoin::Through {
                    junction_table: through_td.table.clone(),
                    module: through_td.module.clone(),
                    source_col,
                    target_col,
                }
            }
        } else {
            IrMultiLinkJoin::Standard {
                junction_table: format!("{}.{}", td.table, ml_name),
                module: module.to_string(),
            }
        };

        let subquery = IrSelect {
            source: IrSource {
                type_name: format!("{}::{}", target_td.module, target_td.name),
                table: target_td.table.clone(),
                alias: sub_alias.clone(),
            },
            shape: sub_shape,
            filter: el
                .filter
                .as_ref()
                .map(|f| self.compile_expr(f, target_td, &sub_alias))
                .transpose()?,
            order_by: el
                .order_by
                .iter()
                .map(|s| self.compile_sort(s, target_td, &sub_alias))
                .collect::<Result<_, _>>()?,
            offset: el
                .offset
                .as_ref()
                .map(|e| self.compile_expr(e, target_td, &sub_alias))
                .transpose()?,
            limit: el
                .limit
                .as_ref()
                .map(|e| self.compile_expr(e, target_td, &sub_alias))
                .transpose()?,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        };

        Ok(IrShapePointer::MultiLink(IrMultiLinkPointer {
            alias: output_alias.to_string(),
            join,
            subquery,
            link_properties,
        }))
    }

    // ── Expression compilation ────────────────────────────────────────────────────

    fn compile_expr(
        &mut self,
        expr: &Expr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        match expr {
            Expr::Path(p) => self.compile_path(p, td, alias),

            Expr::Parameter(name) => {
                let index = self.param_index(name);
                Ok(IrExpr::Param { index })
            }

            Expr::Global(name) => self.compile_global(name),

            Expr::Index { expr: e, index: i } => {
                let ir_expr = self.compile_expr(e, td, alias)?;
                let ir_index = self.compile_expr(i, td, alias)?;
                let is_array = is_array_expr(&ir_expr);
                Ok(IrExpr::Subscript { expr: Box::new(ir_expr), index: Box::new(ir_index), is_array })
            }

            Expr::Slice { expr: e, lower: lo, upper: hi } => {
                let ir_expr = self.compile_expr(e, td, alias)?;
                let is_array = is_array_expr(&ir_expr);
                let ir_lower = lo.as_ref().map(|x| self.compile_expr(x, td, alias)).transpose()?;
                let ir_upper = hi.as_ref().map(|x| self.compile_expr(x, td, alias)).transpose()?;
                Ok(IrExpr::Slice {
                    expr: Box::new(ir_expr),
                    lower: ir_lower.map(Box::new),
                    upper: ir_upper.map(Box::new),
                    is_array,
                })
            }

            Expr::Literal(lit) => Ok(IrExpr::Literal(match lit {
                Literal::Str(s) => IrLiteral::Str(s.clone()),
                Literal::Int(n) => IrLiteral::Int(*n),
                Literal::Float(f) => IrLiteral::Float(*f),
                Literal::Bool(b) => IrLiteral::Bool(*b),
            })),

            Expr::BinOp(b) => {
                if let Some(exists) = self.try_backlink_exists(b, td, alias)? {
                    return Ok(exists);
                }
                if let Some(exists) = self.try_multilink_exists(b, td, alias)? {
                    return Ok(exists);
                }
                let left = self.compile_expr(&b.left, td, alias)?;
                let right = self.compile_expr(&b.right, td, alias)?;
                if let (Some(lt), Some(rt)) = (infer_ir_type(&left), infer_ir_type(&right)) {
                    if !types_compatible(lt, rt) {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "operator '{op}' cannot be applied to operands of type \
                                 '{lq}' and '{rq}'",
                                op = b.op,
                                lq = pg_type_to_pyql(lt),
                                rq = pg_type_to_pyql(rt),
                            ),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                }
                Ok(IrExpr::BinOp(Box::new(IrBinOp { left, op: b.op.clone(), right })))
            }

            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Exists => {
                self.compile_exists_operand(&u.operand, td, alias)
            }

            Expr::UnaryOp(u) => {
                let operand = self.compile_expr(&u.operand, td, alias)?;
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: u.op.clone(), operand })))
            }

            Expr::FunctionCall(f) => {
                // assert_single/exists/distinct with SubQuery arg
                if (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && matches!(f.name.as_str(), "assert_single" | "assert_exists" | "assert_distinct")
                    && f.args.len() >= 1
                {
                    if let Expr::SubQuery(inner_stmt) = &f.args[0] {
                        let inner = self.compile_subquery_to_array_source(inner_stmt)?;
                        let fn_pg = match f.name.as_str() {
                            "assert_single" => "assert_single",
                            "assert_exists"  => "assert_exists",
                            _               => "assert_distinct",
                        };
                        return Ok(IrExpr::FunctionCall(IrFunctionCall {
                            schema: Some("_pylon".to_string()),
                            name: fn_pg.to_string(),
                            args: vec![IrExpr::ArrayFromSelect(Box::new(inner))],
                            sql_template: None,
                        }));
                    }
                }
                // contains(.multilink.scalar, value) → EXISTS (set-membership semantics)
                if (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && f.name == "contains"
                    && f.args.len() == 2
                {
                    if let Expr::Path(p) = &f.args[0] {
                        if p.partial && p.steps.len() >= 2 {
                            if let ast::PathStep::Name(ln) = &p.steps[0] {
                                if Self::resolve_multilink(td, ln).is_some() {
                                    let synthetic = ast::BinOp {
                                        left: f.args[0].clone(),
                                        op: ast::BinOpKind::Eq,
                                        right: f.args[1].clone(),
                                    };
                                    if let Some(exists) =
                                        self.try_multilink_exists(&synthetic, td, alias)?
                                    {
                                        return Ok(exists);
                                    }
                                }
                            }
                        }
                    }
                }
                // count(.multilink) / other single-arg aggregates over a multilink —
                // correlate via the junction/FK table (AggOverQuery) rather than
                // treating `.multilink` as an ordinary scalar path (which it isn't).
                if f.args.len() == 1 {
                    if let Expr::Path(p) = &f.args[0] {
                        if p.partial && p.steps.len() == 1 {
                            if let ast::PathStep::Name(ml_name) = &p.steps[0] {
                                if Self::resolve_multilink(td, ml_name).is_some() {
                                    use crate::stdlib::{lookup, ImplStrategy};
                                    let ns = f.module.as_deref().unwrap_or("std");
                                    let overloads = lookup(ns, &f.name);
                                    let best = overloads.iter().find(|d| d.params.len() == 1).or_else(|| overloads.first());
                                    if let Some(ImplStrategy::SqlBuiltin(sql_name)) = best.map(|d| &d.impl_strategy) {
                                        let fn_name = sql_name.to_string();
                                        let inner = self.multilink_correlation_select(ml_name, td, alias)?;
                                        return Ok(IrExpr::AggOverQuery { fn_name, inner: Box::new(inner) });
                                    }
                                }
                            }
                        }
                    }
                }
                let args = f
                    .args
                    .iter()
                    .map(|a| self.compile_expr(a, td, alias))
                    .collect::<Result<Vec<_>, _>>()?;
                self.resolve_fn_call(f.module.as_deref(), &f.name, args)
            }

            Expr::TypeCast(tc) => {
                // See the identical check in compile_free_expr's TypeCast
                // handling — `<AnyType>{}` is always just NULL.
                if matches!(&tc.expr, Expr::Set(elems) if elems.is_empty()) {
                    return Ok(IrExpr::Null);
                }
                if let ast::TypeExpr::Tuple { elements } = &tc.ty {
                    if let Some(ir) = self.try_compile_tuple_literal_cast(elements, &tc.expr, td, alias)? {
                        let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                        let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                        return Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: ir, pg_type, tuple_shape })));
                    }
                }
                if let ast::TypeExpr::Array { element } = &tc.ty {
                    if let Some(ir) = self.try_compile_array_literal_cast(element, &tc.expr, td, alias)? {
                        let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                        return Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: ir, pg_type, tuple_shape: None })));
                    }
                }
                let inner = self.compile_expr(&tc.expr, td, alias)?;
                let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type, tuple_shape })))
            }

            Expr::IfElse(ie) => {
                let condition = self.compile_expr(&ie.condition, td, alias)?;
                let if_ = self.compile_expr(&ie.if_expr, td, alias)?;
                let else_ = self.compile_expr(&ie.else_expr, td, alias)?;
                Ok(IrExpr::IfElse(Box::new(IrIfElse { condition, if_, else_ })))
            }

            Expr::Array(elems) => {
                let items = elems.iter()
                    .map(|e| self.compile_expr(e, td, alias))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::Array(items))
            }

            Expr::NamedTuple(fields) => {
                let ir = fields
                    .iter()
                    .map(|(name, e)| Ok((name.clone(), self.compile_expr(e, td, alias)?)))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::NamedTuple(ir))
            }

            Expr::Tuple(elems) => {
                let ir = elems
                    .iter()
                    .map(|e| self.compile_expr(e, td, alias))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::Tuple(ir))
            }

            Expr::FieldAccess { expr: inner, field } => {
                // Constant-fold on named tuple literals; otherwise emit jsonb field access.
                if let Expr::NamedTuple(fields) = inner.as_ref() {
                    let (_, val) = fields.iter().find(|(k, _)| k == field).ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "{field} is not a member of {}",
                                named_tuple_type_str(fields)
                            ),
                            position: Position { line: 0, col: 0 },
                        })
                    })?;
                    return self.compile_expr(val, td, alias);
                }
                let ir = self.compile_expr(inner, td, alias)?;
                Ok(IrExpr::JsonbField { expr: Box::new(ir), field: field.clone() })
            }

            Expr::TupleIndex { expr: inner, index } => {
                match inner.as_ref() {
                    Expr::Tuple(elems) => {
                        let elem = elems.get(*index).ok_or_else(|| PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "{index} is not a member of {}",
                                positional_tuple_type_str(elems)
                            ),
                            position: Position { line: 0, col: 0 },
                        }))?;
                        self.compile_expr(elem, td, alias)
                    }
                    Expr::NamedTuple(fields) => {
                        let (_, val) = fields.get(*index).ok_or_else(|| PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "{index} is not a member of {}",
                                named_tuple_type_str(fields)
                            ),
                            position: Position { line: 0, col: 0 },
                        }))?;
                        self.compile_expr(val, td, alias)
                    }
                    // Not a literal to constant-fold — emit a generic runtime
                    // jsonb positional access. When the source is a cast to a
                    // statically-known tuple type, bounds-check the index
                    // against its arity at compile time (matches Gel:
                    // `2 is not a member of tuple<std::int64, std::str>`).
                    _ => {
                        if let Expr::TypeCast(tc) = inner.as_ref() {
                            if let Some(shape) = self.resolve_tuple_cast_shape(&tc.ty) {
                                if *index >= shape.members.len() {
                                    return Err(self.type_err(&format!(
                                        "{index} is not a member of {}",
                                        self.type_expr_to_display_str(&tc.ty)
                                    )));
                                }
                            }
                        }
                        let ir = self.compile_expr(inner, td, alias)?;
                        Ok(IrExpr::JsonbIndex { expr: Box::new(ir), index: *index })
                    }
                }
            }

            Expr::Shape(_) | Expr::Set(_) => {
                Err(PyQLError::Type(PyQLTypeError {
                    message: "shapes and set literals are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }))
            }

            Expr::SubQuery(_) => Err(PyQLError::Type(PyQLTypeError {
                message: "sub-statement (SELECT/INSERT/UPDATE/DELETE) used as expression is \
                           only valid as the subject of a SELECT result"
                    .into(),
                position: Position { line: 0, col: 0 },
            })),

            Expr::Union(_, _) => Err(PyQLError::Type(PyQLTypeError {
                message: "union is not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            Expr::Except(_, _) => Err(PyQLError::Type(PyQLTypeError {
                message: "except is not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            // detached in schema-bound context: compile inner as an independent subquery,
            // bypassing the implicit root-matches-td correlation rewrite.
            Expr::Detached(inner) => {
                if let Some(root) = self.find_path_root_in_expr(inner) {
                    let synthetic = ast::SelectStmt {
                        result: *inner.clone(),
                        filter: None, order_by: vec![], offset: None, limit: None,
                    };
                    let ps = self.compile_expr_as_path_select(&synthetic, inner, &root, false)?;
                    return Ok(IrExpr::PathSubquery(Box::new(ps)));
                }
                // No type-rooted path — compile inner without schema binding
                self.compile_free_expr(inner)
            }

            Expr::TypeIs { expr, ty } => self.compile_type_is(expr, ty, td, alias),
        }
    }

    fn compile_type_is(
        &mut self,
        expr: &Expr,
        ty: &ast::TypeExpr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let self_qname = format!("{}::{}", td.module, td.name);

        // Determine whether `expr` refers to the current scope or a different type.
        // A 1-step absolute path matching the current td → same scope.
        let (source_qname, cross_scope) = match expr {
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if n == &td.name || n == &self_qname {
                        (self_qname.clone(), false)
                    } else {
                        // Attempt to resolve as another type.
                        match self.resolve_type(n) {
                            Ok(other) => (format!("{}::{}", other.module, other.name), true),
                            Err(_) => (self_qname.clone(), false),
                        }
                    }
                } else {
                    (self_qname.clone(), false)
                }
            }
            _ => (self_qname.clone(), false),
        };

        let (ty_module, ty_name) = ty.as_named()
            .ok_or_else(|| self.type_err("cannot use IS with a tuple or array type"))?;
        let check_module = ty_module.unwrap_or(td.module.as_str());
        let check_qname = format!("{}::{}", check_module, ty_name);
        self.resolve_type(&check_qname)?;

        if !cross_scope {
            // Scalar bool using the current alias.
            return Ok(self.type_check_bool_expr(&source_qname, &check_qname, td, alias));
        }

        // Cross-scope: ARRAY(SELECT bool_expr FROM source_table).
        // Collect everything needed before calling fresh_alias (which needs &mut self).
        let (source_table, source_abstract, source_materialized,
             poly_implementors, poly_columns) = {
            let src_td = self.resolve_type(&source_qname)?;
            let table = src_td.table.clone();
            let abstract_ = src_td.abstract_;
            let materialized = src_td.materialized;
            let imps = if abstract_ && materialized {
                self.find_poly_implementors(&source_qname)
            } else {
                vec![]
            };
            let cols = if abstract_ && materialized {
                src_td.properties.iter().map(|p| p.name.clone())
                    .chain(src_td.links.iter().map(|l| format!("{}_id", l.name)))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            (table, abstract_, materialized, imps, cols)
        };

        let src_alias = self.fresh_alias();

        let bool_expr = if check_qname == source_qname {
            IrExpr::Literal(IrLiteral::Bool(true))
        } else if source_abstract && source_materialized {
            IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: src_alias.clone(),
                    column: "__type__".into(),
                    pg_type: "text".into(),
                },
                op: crate::parse::ast::BinOpKind::Eq,
                right: IrExpr::Literal(IrLiteral::Str(check_qname)),
            }))
        } else {
            IrExpr::Literal(IrLiteral::Bool(false))
        };

        let source = IrSource {
            type_name: source_qname,
            table: source_table,
            alias: src_alias,
        };

        Ok(IrExpr::ArrayFromSelect(Box::new(IrArraySource::RawExpr {
            source,
            poly_implementors,
            poly_columns,
            expr: bool_expr,
        })))
    }

    /// Build the boolean `IrExpr` for `source_qname is check_qname` in the current row's scope.
    fn type_check_bool_expr(
        &self,
        source_qname: &str,
        check_qname: &str,
        td: &TypeDescriptor,
        alias: &str,
    ) -> IrExpr {
        if check_qname == source_qname || td.interfaces.iter().any(|i| i == check_qname) {
            IrExpr::Literal(IrLiteral::Bool(true))
        } else if td.abstract_ && td.materialized {
            IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: alias.to_string(),
                    column: "__type__".into(),
                    pg_type: "text".into(),
                },
                op: crate::parse::ast::BinOpKind::Eq,
                right: IrExpr::Literal(IrLiteral::Str(check_qname.to_string())),
            }))
        } else {
            IrExpr::Literal(IrLiteral::Bool(false))
        }
    }

    fn compile_path(
        &mut self,
        p: &ast::Path,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        if !p.partial {
            if p.steps.len() == 1 {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    // For-loop variable used in schema-bound context.
                    if self.for_vars.contains_key(n.as_str()) {
                        return Ok(IrExpr::ForVar { name: n.clone() });
                    }
                    // Allow CTE names as references in expression context.
                    if let Some(t) = self.cte_types.get(n.as_str()) {
                        let scalar = !t.contains("::");
                        return Ok(IrExpr::CteRef { name: n.clone(), scalar });
                    }
                    // __type__ without a leading dot still means the current object's type.
                    if n == "__type__" {
                        return Ok(if td.abstract_ && td.materialized {
                            IrExpr::ColumnRef {
                                alias: alias.to_string(),
                                column: "__type__".to_string(),
                                pg_type: "text".to_string(),
                            }
                        } else {
                            IrExpr::Literal(IrLiteral::Str(format!("{}::{}", td.module, td.name)))
                        });
                    }
                }
            }
            // Enum member access: `default::Gender.Female`
            if p.steps.len() == 2 {
                if let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice() {
                    if self.resolve_enum(type_ref).is_some() {
                        return self.compile_enum_access(type_ref, variant);
                    }
                }
            }
            // Absolute path rooted at the current td: `TypeName.prop` inside a schema-bound
            // expression (e.g. the value side of a BinOp in compile_expr_as_path_select).
            // Rewrite to a relative path and compile normally.
            if p.steps.len() > 1 {
                if let ast::PathStep::Name(root) = &p.steps[0] {
                    let qualified = format!("{}::{}", td.module, td.name);
                    if *root == td.name || *root == qualified {
                        let relative = ast::Path { steps: p.steps[1..].to_vec(), partial: true };
                        return self.compile_path(&relative, td, alias);
                    }
                }
            }
            return Err(PyQLError::Type(PyQLTypeError {
                message: "absolute paths are not valid in expression context; use .name".into(),
                position: Position { line: 0, col: 0 },
            }));
        }

        // Type intersection in expression: [is Type].name — scalar subquery
        if p.partial && matches!(p.steps.first(), Some(ast::PathStep::TypeIntersection(_))) {
            return self.compile_type_intersection_expr(&p.steps, td, alias);
        }

        if p.steps.len() == 2 {
            return self.compile_path_2step(p, td, alias);
        }

        if p.steps.len() != 1 {
            return Err(PyQLError::Type(PyQLTypeError {
                message: "path traversal deeper than 2 steps is not yet supported".into(),
                position: Position { line: 0, col: 0 },
            }));
        }

        let pointer_name = match &p.steps[0] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "type intersections are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }))
            }
        };

        // __type__ as an expression: for polymorphic (interface) types, read from the
        // inline union column; for concrete types, emit the static qualified name.
        if pointer_name == "__type__" {
            return Ok(if td.abstract_ && td.materialized {
                IrExpr::ColumnRef {
                    alias: alias.to_string(),
                    column: "__type__".to_string(),
                    pg_type: "text".to_string(),
                }
            } else {
                IrExpr::Literal(IrLiteral::Str(format!("{}::{}", td.module, td.name)))
            });
        }

        if let Some(prop) = Self::resolve_property(td, pointer_name) {
            return Ok(IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: prop.name.clone(),
                pg_type: prop.pg_type.clone(),
            });
        }

        if let Some(link) = Self::resolve_link(td, pointer_name) {
            // FK column reference (uuid) — e.g. `.company` → `t0."company_id"`
            return Ok(IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: format!("{}_id", link.name),
                pg_type: "uuid".to_string(),
            });
        }

        // Schema-defined computed pointer: inline the expression in place.
        if let Some(cd) = td.computed.iter().find(|c| c.name == pointer_name) {
            let expr_ast = crate::parse::parse_expr(&cd.expression)
                .map_err(|e| PyQLError::Syntax(e))?;
            return self.compile_expr(&expr_ast, td, alias);
        }

        Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)))
    }

    fn compile_path_2step(
        &mut self,
        p: &ast::Path,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let link_name = match &p.steps[0] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => return Err(PyQLError::Type(PyQLTypeError {
                message: "type intersections are not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),
        };
        let pointer_name = match &p.steps[1] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => return Err(PyQLError::Type(PyQLTypeError {
                message: "type intersections are not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),
        };

        if let Some(link) = Self::resolve_link(td, link_name) {
            let fk_col = format!("{}_id", link_name);
            if pointer_name == "id" {
                return Ok(IrExpr::ColumnRef {
                    alias: alias.to_string(),
                    column: fk_col,
                    pg_type: "uuid".to_string(),
                });
            }
            let target_td = self.resolve_type(&link.target)?;
            if let Some(prop) = Self::resolve_property(target_td, pointer_name) {
                let ft_alias = self.fresh_alias();
                return Ok(IrExpr::Subquery(Box::new(IrSelect {
                    source: IrSource {
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: ft_alias.clone(),
                    },
                    shape: vec![IrShapePointer::Scalar(IrScalarPointer {
                        alias: prop.name.clone(),
                        column: prop.name.clone(),
                        pg_type: prop.pg_type.clone(),
                        tuple_shape: self.resolve_property_tuple_shape(prop),
                    })],
                    filter: Some(IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef {
                            alias: ft_alias.clone(),
                            column: "id".to_string(),
                            pg_type: "uuid".to_string(),
                        },
                        op: ast::BinOpKind::Eq,
                        right: IrExpr::ColumnRef {
                            alias: alias.to_string(),
                            column: fk_col,
                            pg_type: "uuid".to_string(),
                        },
                    }))),
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    distinct: false,
                    dml_source: None,
                    polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                })));
            }
            let target_name = link.target.clone();
            return Err(self.field_err(pointer_name, &target_name));
        }

        if Self::resolve_multilink(td, link_name).is_some() {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "multi-link path '.{link_name}.{pointer_name}' must be used inside a comparison, \
                     e.g.: filter .{link_name}.{pointer_name} = value"
                ),
                position: Position { line: 0, col: 0 },
            }));
        }

        Err(self.field_err(link_name, &format!("{}::{}", td.module, td.name)))
    }

    // ── Backlink compilation ─────────────────────────────────────────────────────

    /// Detect a backlink path on either side of a BinOp and compile as EXISTS.
    fn try_backlink_exists(
        &mut self,
        b: &ast::BinOp,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Option<IrExpr>, PyQLError> {
        use ast::PathStep;
        fn is_backlink(p: &ast::Path) -> bool {
            p.partial && matches!(p.steps.first(), Some(PathStep::Backlink(_)))
        }
        let (path_steps, value_ast, flip) = if let Expr::Path(p) = &b.left {
            if is_backlink(p) { (p.steps.as_slice(), &b.right, false) }
            else { return Ok(None); }
        } else if let Expr::Path(p) = &b.right {
            if is_backlink(p) { (p.steps.as_slice(), &b.left, true) }
            else { return Ok(None); }
        } else {
            return Ok(None);
        };
        let value_expr = self.compile_expr(value_ast, td, alias)?;
        let current_qname = format!("{}::{}", td.module, td.name);
        let exists = self.compile_backlink_as_exists(
            path_steps, Some((b.op.clone(), value_expr, flip)), &current_qname, alias,
        )?;
        Ok(Some(exists))
    }

    /// Compile a path `[Backlink(name), TypeIntersect(type), ...rest]` into EXISTS.
    /// `comparison` is `Some((op, value_expr, flip))` when used in a comparison filter.
    /// `current_qname` is the fully-qualified name of the object type being filtered.
    fn compile_backlink_as_exists(
        &mut self,
        steps: &[ast::PathStep],
        comparison: Option<(ast::BinOpKind, IrExpr, bool)>,
        current_qname: &str,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        use ast::PathStep;

        let backlink_name = match steps.first() {
            Some(PathStep::Backlink(n)) => n.clone(),
            _ => return Err(self.type_err("internal: expected backlink step")),
        };
        let type_ref = match steps.get(1) {
            Some(PathStep::TypeIntersection(tr)) => tr,
            _ => return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "backlink '.< {backlink_name}' requires a type intersection, \
                     e.g.: .< {backlink_name}[is SomeType]"
                ),
                position: Position { line: 0, col: 0 },
            })),
        };

        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let target_td = self.resolve_type(&type_name)?;
        let target_qname = format!("{}::{}", target_td.module, target_td.name);
        let target_table = target_td.table.clone();

        // Verify target has a link named `backlink_name` pointing to the current type.
        if !target_td.links.iter().any(|l| l.name == backlink_name && l.target == current_qname) {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "type {} has no link '{}' pointing to {}",
                    target_qname, backlink_name, current_qname,
                ),
                position: Position { line: 0, col: 0 },
            }));
        }
        let fk_col = format!("{}_id", backlink_name);
        let t_alias = self.fresh_alias();

        // Join condition: target.fk_col = current.id
        let join_cond = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: t_alias.clone(),
                column: fk_col,
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
        }));

        let rest = &steps[2..];
        let tail_cond = self.compile_backlink_tail(rest, comparison, &target_qname, &t_alias)?;

        let filter = match tail_cond {
            Some(tc) => IrExpr::BinOp(Box::new(IrBinOp {
                left: join_cond,
                op: ast::BinOpKind::And,
                right: tc,
            })),
            None => join_cond,
        };

        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: IrExpr::Subquery(Box::new(IrSelect {
                source: IrSource { type_name: target_qname, table: target_table, alias: t_alias },
                shape: vec![],
                filter: Some(filter),
                order_by: vec![],
                offset: None,
                limit: None,
                distinct: false,
                dml_source: None,
                polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
            })),
        })))
    }

    /// Compile the tail steps after `[Backlink, TypeIntersect]`.
    fn compile_backlink_tail(
        &mut self,
        steps: &[ast::PathStep],
        comparison: Option<(ast::BinOpKind, IrExpr, bool)>,
        target_qname: &str,
        t_alias: &str,
    ) -> Result<Option<IrExpr>, PyQLError> {
        use ast::PathStep;

        if steps.is_empty() {
            return Ok(None);
        }

        // Chained backlink: recurse (current_qname is now target_qname of the outer backlink)
        if matches!(steps.first(), Some(PathStep::Backlink(_))) {
            let inner = self.compile_backlink_as_exists(steps, comparison, target_qname, t_alias)?;
            return Ok(Some(inner));
        }

        let target_td = self.resolve_type(target_qname)?;

        // Single property or link FK
        if let [PathStep::Name(pointer_name)] = steps {
            if let Some(prop) = Self::resolve_property(target_td, pointer_name) {
                let col = IrExpr::ColumnRef {
                    alias: t_alias.to_string(),
                    column: prop.name.clone(),
                    pg_type: prop.pg_type.clone(),
                };
                return Ok(Some(Self::apply_comparison(col, comparison)));
            }
            if let Some(link) = Self::resolve_link(target_td, pointer_name) {
                let col = IrExpr::ColumnRef {
                    alias: t_alias.to_string(),
                    column: format!("{}_id", link.name),
                    pg_type: "uuid".to_string(),
                };
                return Ok(Some(Self::apply_comparison(col, comparison)));
            }
            return Err(self.field_err(pointer_name, target_qname));
        }

        // Two forward steps: link then property (single FK join)
        if let [PathStep::Name(link_name), PathStep::Name(prop_name)] = steps {
            if let Some(link) = Self::resolve_link(target_td, link_name) {
                let link_target = link.target.clone();
                let fk_col = format!("{}_id", link_name);
                let link_target_td = self.resolve_type(&link_target)?;
                let link_target_qname = format!("{}::{}", link_target_td.module, link_target_td.name);
                let link_target_table = link_target_td.table.clone();
                if let Some(prop) = Self::resolve_property(link_target_td, prop_name) {
                    let l_alias = self.fresh_alias();
                    let id_cond = IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef {
                            alias: l_alias.clone(),
                            column: "id".to_string(),
                            pg_type: "uuid".to_string(),
                        },
                        op: ast::BinOpKind::Eq,
                        right: IrExpr::ColumnRef {
                            alias: t_alias.to_string(),
                            column: fk_col,
                            pg_type: "uuid".to_string(),
                        },
                    }));
                    let col = IrExpr::ColumnRef {
                        alias: l_alias.clone(),
                        column: prop.name.clone(),
                        pg_type: prop.pg_type.clone(),
                    };
                    let prop_cond = Self::apply_comparison(col, comparison);
                    let full = IrExpr::BinOp(Box::new(IrBinOp {
                        left: id_cond,
                        op: ast::BinOpKind::And,
                        right: prop_cond,
                    }));
                    return Ok(Some(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                        op: ast::UnaryOpKind::Exists,
                        operand: IrExpr::Subquery(Box::new(IrSelect {
                            source: IrSource {
                                type_name: link_target_qname,
                                table: link_target_table,
                                alias: l_alias,
                            },
                            shape: vec![],
                            filter: Some(full),
                            order_by: vec![],
                            offset: None,
                            limit: None,
                            distinct: false,
                            dml_source: None,
                            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                        })),
                    }))));
                }
            }
        }

        Err(PyQLError::Type(PyQLTypeError {
            message: "backlink path tail is unsupported (expected a property name)".to_string(),
            position: Position { line: 0, col: 0 },
        }))
    }

    /// Apply an optional comparison to a column ref, defaulting to IS NOT NULL.
    fn apply_comparison(col: IrExpr, comparison: Option<(ast::BinOpKind, IrExpr, bool)>) -> IrExpr {
        match comparison {
            Some((op, val, flip)) => {
                let (l, r) = if flip { (val, col) } else { (col, val) };
                IrExpr::BinOp(Box::new(IrBinOp { left: l, op, right: r }))
            }
            None => ir_is_not_null(col),
        }
    }

    /// If `b` has a multi-link path (any depth) on either side, compile as EXISTS over the junction.
    fn try_multilink_exists(
        &mut self,
        b: &ast::BinOp,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Option<IrExpr>, PyQLError> {
        fn ml_first_name(steps: &[ast::PathStep]) -> Option<&str> {
            if steps.len() < 2 { return None; }
            match &steps[0] { ast::PathStep::Name(n) => Some(n.as_str()), _ => None }
        }

        let (path_steps, value_ast, flip) = if let Expr::Path(p) = &b.left {
            if p.partial {
                if let Some(ln) = ml_first_name(&p.steps) {
                    if Self::resolve_multilink(td, ln).is_some() {
                        (p.steps.as_slice(), &b.right, false)
                    } else { return Ok(None); }
                } else { return Ok(None); }
            } else { return Ok(None); }
        } else if let Expr::Path(p) = &b.right {
            if p.partial {
                if let Some(ln) = ml_first_name(&p.steps) {
                    if Self::resolve_multilink(td, ln).is_some() {
                        (p.steps.as_slice(), &b.left, true)
                    } else { return Ok(None); }
                } else { return Ok(None); }
            } else { return Ok(None); }
        } else {
            return Ok(None);
        };

        let value_expr = self.compile_expr(value_ast, td, alias)?;
        let ml_name = match &path_steps[0] { ast::PathStep::Name(n) => n.clone(), _ => return Ok(None) };
        let ml = Self::resolve_multilink(td, &ml_name).unwrap();

        // Warn: multi-link traversal in a comparison returns a set, not a single boolean.
        // The query works (compiled as EXISTS), but `any()` makes the intent explicit.
        {
            let pointer_path: Vec<_> = path_steps.iter().map(|s| match s {
                ast::PathStep::Name(n) => n.as_str(),
                _ => "?",
            }).collect();
            self.warnings.push(format!(
                "possibly more than one element returned by an expression in a FILTER clause \
                 (multi-link '.{}'); wrap with any() to make intent explicit",
                pointer_path.join("."),
            ));
        }

        // Clone what we need to avoid borrow conflicts with self below.
        let ml_target = ml.target.clone();
        let ml_through = ml.through.clone();
        let td_module = td.module.clone();
        let td_name = td.name.clone();
        let td_table = td.table.clone();

        // tail steps are everything after the multi-link name (path_steps[1..])
        let tail_steps: Vec<ast::PathStep> = path_steps[1..].to_vec();

        let jt_alias = self.fresh_alias();

        // Resolve junction table columns.
        let (jt_table, jt_module, jt_src_col, jt_tgt_col) = if let Some(through_qname) = &ml_through {
            let through_td = self.resolve_type(through_qname)?;
            if through_td.junction {
                (
                    through_td.table.clone(),
                    through_td.module.clone(),
                    "source".to_string(),
                    "target".to_string(),
                )
            } else {
                let source_qname = format!("{}::{}", td_module, td_name);
                let src_col = through_td
                    .links.iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                        message: format!("through type {through_qname} has no link to {source_qname}"),
                        position: Position { line: 0, col: 0 },
                    }))?
                    .name.clone();
                let tgt_col = through_td
                    .links.iter()
                    .find(|l| l.target == ml_target && l.name != src_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == ml_target))
                    .ok_or_else(|| PyQLError::Type(PyQLTypeError {
                        message: format!("through type {through_qname} has no link to {ml_target}"),
                        position: Position { line: 0, col: 0 },
                    }))?
                    .name.clone();
                (
                    through_td.table.clone(),
                    through_td.module.clone(),
                    format!("{}_id", src_col),
                    format!("{}_id", tgt_col),
                )
            }
        } else {
            (
                format!("{}.{}", td_table, ml_name),
                td_module.clone(),
                "source".to_string(),
                "target".to_string(),
            )
        };

        // source filter: jt.src_col = parent.id
        let src_filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: jt_alias.clone(),
                column: jt_src_col,
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
        }));

        // Build tail filter: what to compare inside the junction/target EXISTS
        let tail_filter = self.compile_path_tail_filter(
            &tail_steps,
            b.op.clone(),
            value_expr,
            flip,
            &ml_target,
            &jt_alias,
            &jt_tgt_col,
        )?;

        let full_filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: src_filter,
            op: ast::BinOpKind::And,
            right: tail_filter,
        }));

        // EXISTS(SELECT 1 FROM junction jt WHERE ...)
        let jt_source = IrSource {
            type_name: format!("{}::__jt__", jt_module),
            table: jt_table,
            alias: jt_alias,
        };
        let inner = IrExpr::Subquery(Box::new(IrSelect {
            source: jt_source,
            shape: vec![],
            filter: Some(full_filter),
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        }));

        Ok(Some(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: inner,
        }))))
    }

    /// Build a filter expression for the tail steps after the multi-link in an EXISTS context.
    ///
    /// `steps` = path steps after the multi-link name (e.g. ["company", "id"] for .friends.company.id)
    /// `jt_alias` = alias of the junction table row
    /// `jt_tgt_col` = column in the junction table holding the target id (e.g. "target" or "friend_id")
    /// `target_type` = qualified name of the multi-link's target type (e.g. "default::Person")
    #[allow(clippy::too_many_arguments)]
    fn compile_path_tail_filter(
        &mut self,
        steps: &[ast::PathStep],
        op: ast::BinOpKind,
        value_expr: IrExpr,
        flip: bool,
        target_type: &str,
        jt_alias: &str,
        jt_tgt_col: &str,
    ) -> Result<IrExpr, PyQLError> {
        // steps should have at least 1 element
        let first_name = match steps.first() {
            Some(ast::PathStep::Name(n)) => n.clone(),
            _ => return Err(self.type_err("expected a property or link name in path")),
        };

        let target_td = self.resolve_type(target_type)?;
        let target_table = target_td.table.clone();

        if steps.len() == 1 {
            // Terminal step: must be a scalar property or "id"
            if first_name == "id" {
                // FK optimisation: compare jt.target directly
                let col_ref = IrExpr::ColumnRef {
                    alias: jt_alias.to_string(),
                    column: jt_tgt_col.to_string(),
                    pg_type: "uuid".to_string(),
                };
                let (l, r) = if flip { (value_expr, col_ref) } else { (col_ref, value_expr) };
                return Ok(IrExpr::BinOp(Box::new(IrBinOp { left: l, op, right: r })));
            }
            // Check if it's a link (object) rather than a scalar
            let is_link = target_td.links.iter().any(|l| l.name == first_name)
                || target_td.multilinks.iter().any(|l| l.name == first_name);
            if is_link {
                let target_display = target_type.replace("::", ".");
                return Err(PyQLError::Type(PyQLTypeError {
                    message: format!(
                        "operator '{op}' cannot be applied to operands of type '{target_display}' and the value type",
                        op = &op,
                    ),
                    position: Position { line: 0, col: 0 },
                }));
            }
            let prop = target_td.properties.iter()
                .find(|p| p.name == first_name)
                .ok_or_else(|| self.field_err(&first_name, target_type))?;
            let prop_name = prop.name.clone();
            let prop_pg = prop.pg_type.clone();
            let tgt_alias = self.fresh_alias();
            // Build EXISTS(SELECT 1 FROM target WHERE target.id = jt.target AND target.prop op value)
            let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef { alias: tgt_alias.clone(), column: "id".to_string(), pg_type: "uuid".to_string() },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ColumnRef { alias: jt_alias.to_string(), column: jt_tgt_col.to_string(), pg_type: "uuid".to_string() },
            }));
            let prop_col = IrExpr::ColumnRef { alias: tgt_alias.clone(), column: prop_name, pg_type: prop_pg };
            let (pl, pr) = if flip { (value_expr, prop_col) } else { (prop_col, value_expr) };
            let prop_filter = IrExpr::BinOp(Box::new(IrBinOp { left: pl, op, right: pr }));
            let full = IrExpr::BinOp(Box::new(IrBinOp {
                left: id_filter,
                op: ast::BinOpKind::And,
                right: prop_filter,
            }));
            let inner = IrExpr::Subquery(Box::new(IrSelect {
                source: IrSource {
                    type_name: target_type.to_string(),
                    table: target_table,
                    alias: tgt_alias,
                },
                shape: vec![],
                filter: Some(full),
                order_by: vec![],
                offset: None,
                limit: None,
                distinct: false,
                dml_source: None,
                polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
            }));
            return Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: ast::UnaryOpKind::Exists, operand: inner })));
        }

        // steps.len() >= 2: first_name must be a single link (not multi)
        if target_td.multilinks.iter().any(|l| l.name == first_name) {
            return Err(self.type_err(
                "nested multi-link traversal in comparison is not yet supported",
            ));
        }
        let link = target_td.links.iter()
            .find(|l| l.name == first_name)
            .ok_or_else(|| self.field_err(&first_name, target_type))?;
        let next_target = link.target.clone();
        let fk_col = format!("{}_id", first_name);
        let tgt_alias = self.fresh_alias();

        // FK optimisation for [single_link, "id"]:
        if steps.len() == 2 {
            if let Some(ast::PathStep::Name(n)) = steps.get(1) {
                if n == "id" {
                    // Compare tgt.{fk_col} (the FK in current target) directly
                    // We need an EXISTS over the target to access fk_col
                    // Actually: EXISTS(target WHERE target.id = jt.target AND target.{fk_col} op value)
                    let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef { alias: tgt_alias.clone(), column: "id".to_string(), pg_type: "uuid".to_string() },
                        op: ast::BinOpKind::Eq,
                        right: IrExpr::ColumnRef { alias: jt_alias.to_string(), column: jt_tgt_col.to_string(), pg_type: "uuid".to_string() },
                    }));
                    let fk_ref = IrExpr::ColumnRef { alias: tgt_alias.clone(), column: fk_col, pg_type: "uuid".to_string() };
                    let (fl, fr) = if flip { (value_expr, fk_ref) } else { (fk_ref, value_expr) };
                    let fk_filter = IrExpr::BinOp(Box::new(IrBinOp { left: fl, op, right: fr }));
                    let full = IrExpr::BinOp(Box::new(IrBinOp {
                        left: id_filter,
                        op: ast::BinOpKind::And,
                        right: fk_filter,
                    }));
                    let inner = IrExpr::Subquery(Box::new(IrSelect {
                        source: IrSource {
                            type_name: target_type.to_string(),
                            table: target_table,
                            alias: tgt_alias,
                        },
                        shape: vec![],
                        filter: Some(full),
                        order_by: vec![],
                        offset: None,
                        limit: None,
                        distinct: false,
                        dml_source: None,
                        polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
                    }));
                    return Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: ast::UnaryOpKind::Exists, operand: inner })));
                }
            }
        }

        // General case: EXISTS(target WHERE target.id = jt.jt_tgt_col AND <tail_filter for steps[1..]>)
        // Recursive call uses tgt_alias.fk_col as the "pointer to the next type's id"
        let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef { alias: tgt_alias.clone(), column: "id".to_string(), pg_type: "uuid".to_string() },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef { alias: jt_alias.to_string(), column: jt_tgt_col.to_string(), pg_type: "uuid".to_string() },
        }));
        let nested_filter = self.compile_path_tail_filter(
            &steps[1..],
            op,
            value_expr,
            flip,
            &next_target,
            &tgt_alias,
            &fk_col,
        )?;
        let full = IrExpr::BinOp(Box::new(IrBinOp {
            left: id_filter,
            op: ast::BinOpKind::And,
            right: nested_filter,
        }));
        let inner = IrExpr::Subquery(Box::new(IrSelect {
            source: IrSource {
                type_name: target_type.to_string(),
                table: target_table,
                alias: tgt_alias,
            },
            shape: vec![],
            filter: Some(full),
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        }));
        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: ast::UnaryOpKind::Exists, operand: inner })))
    }

    /// Compile `UNLESS CONFLICT [ON expr] [ELSE (UPDATE …)]` into `IrConflict`.
    fn compile_conflict(
        &mut self,
        uc: &ast::UnlessConflict,
        td: &TypeDescriptor,
    ) -> Result<IrConflict, PyQLError> {
        // ON clause: compile with empty alias → bare column name (`"col"` not `"t0"."col"`)
        // so the emitter produces `ON CONFLICT ("name")` not `ON CONFLICT ("t0"."name")`.
        let on = uc.on.as_ref()
            .map(|e| self.compile_expr(e, td, ""))
            .transpose()?;
        let do_update = uc.else_.as_ref()
            .map(|e| self.compile_conflict_else(e))
            .transpose()?;
        Ok(IrConflict { on, do_update })
    }

    /// Compile the ELSE clause of UNLESS CONFLICT, which must be `(UPDATE Type SET { … })`.
    ///
    /// Assignments are compiled with an empty table alias so ColumnRefs emit as bare
    /// column names — valid in PostgreSQL's `DO UPDATE SET` context, where bare names
    /// reference the existing (conflicting) row.
    fn compile_conflict_else(
        &mut self,
        expr: &Expr,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        let Expr::SubQuery(stmt) = expr else {
            return Err(self.type_err(
                "UNLESS CONFLICT ELSE must be an UPDATE expression, e.g. ELSE (UPDATE …)",
            ));
        };
        let Stmt::Update(upd) = stmt.as_ref() else {
            return Err(self.type_err(
                "UNLESS CONFLICT ELSE must be an UPDATE expression",
            ));
        };
        let type_name = self.expr_as_type_name(&upd.subject)?;
        let upd_td = self.resolve_type(&type_name)?;
        // Filter on the ELSE UPDATE is ignored — PostgreSQL infers the conflicting
        // row from the ON CONFLICT target automatically.
        self.compile_assignments_for_update(&upd.shape, upd_td, "")
    }

    /// Compile `(SELECT TargetType FILTER …)` as a scalar subquery for use in a
    /// link assignment (`company := (SELECT Company FILTER .name = $co)`).
    /// Returns `IrExpr::Subquery` whose shape is the target pk — the SQL emitter
    /// renders this as `(SELECT "alias"."id" FROM … WHERE …)`.
    fn compile_link_subquery(&mut self, stmt: &Stmt) -> Result<IrExpr, PyQLError> {
        let Stmt::Select(sel) = stmt else {
            return Err(self.type_err(
                "only SELECT is valid as a link assignment value; \
                 use SELECT (INSERT …) { id } to assign from a DML result",
            ));
        };
        let type_name = self.expr_as_type_name(&sel.result)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();

        let filter = sel
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;

        Ok(IrExpr::Subquery(Box::new(IrSelect {
            source: IrSource {
                type_name: format!("{}::{}", td.module, td.name),
                table: td.table.clone(),
                alias,
            },
            shape: Self::pk_returning(td),
            filter,
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            dml_source: None,
            polymorphic: false, poly_implementors: vec![], poly_columns: vec![],
        })))
    }

    fn compile_sort(
        &mut self,
        s: &ast::SortExpr,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrSort, PyQLError> {
        Ok(IrSort {
            expr: self.compile_expr(&s.expr, td, alias)?,
            direction: match s.direction {
                SortDirection::Asc => IrSortDir::Asc,
                SortDirection::Desc => IrSortDir::Desc,
            },
            nulls: match s.nones {
                NonesOrder::First => IrNulls::First,
                NonesOrder::Last => IrNulls::Last,
            },
        })
    }

    // ── Stdlib function resolution ────────────────────────────────────────────────

    /// Look up `name` in the stdlib (namespace = `module` or `"std"`) and produce
    /// the correct `IrExpr::FunctionCall` based on the matching `ImplStrategy`.
    /// Falls through to a plain call if no overload is found (unknown / PG built-in).
    fn resolve_fn_call(
        &self,
        module: Option<&str>,
        name: &str,
        args: Vec<IrExpr>,
    ) -> Result<IrExpr, PyQLError> {
        use crate::stdlib::{lookup, ImplStrategy};

        let ns = module.unwrap_or("std");
        let overloads = lookup(ns, name);

        // Pick the overload whose parameter types best match the argument types.
        // Fall back to the first registered overload when no type info is available.
        let best = overloads
            .iter()
            .find(|d| {
                d.params.len() == args.len()
                    && d.params.iter().zip(&args).all(|(p, a)| pylon_type_matches(a, &p.ty))
            })
            .or_else(|| overloads.first());

        let (schema, resolved_name, sql_template) = if let Some(desc) = best {
            match &desc.impl_strategy {
                ImplStrategy::SqlBuiltin(sql_name) =>
                    (None, sql_name.to_string(), None),
                ImplStrategy::SqlExpression(tmpl) =>
                    (None, name.to_string(), Some(tmpl.to_string())),
                ImplStrategy::PylonFunction(def) =>
                    (Some("_pylon".to_string()), def.name.to_string(), None),
                // SqlOperator / TranspilerIntrinsic: pass through; handled elsewhere
                _ => (module.map(str::to_string), name.to_string(), None),
            }
        } else {
            // Fall back to user-defined scalar functions.
            let effective_module = module.unwrap_or("default");
            let user_fn = self.schema.functions.iter().find(|f| {
                let module_matches = module.map(|m| m == f.module.as_str()).unwrap_or(true);
                module_matches && f.name == name && !f.return_is_object
            });
            if let Some(fd) = user_fn {
                if fd.params.len() != args.len() {
                    return Err(self.type_err(&format!(
                        "function '{}::{}' expects {} argument(s), got {}",
                        fd.module, fd.name, fd.params.len(), args.len()
                    )));
                }
                let cast_args = fd.params.iter().zip(args).map(|(p, a)| {
                    IrExpr::TypeCast(Box::new(super::IrTypeCast {
                        expr: a,
                        pg_type: p.pg_type.clone(),
                    tuple_shape: None, }))
                }).collect();
                return Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: Some(fd.module.clone()),
                    name: fd.name.clone(),
                    args: cast_args,
                    sql_template: None,
                }));
            }
            let qualified = format!("{}::{}", effective_module, name);
            return Err(self.type_err(&format!(
                "function '{qualified}' does not exist"
            )));
        };

        Ok(IrExpr::FunctionCall(super::IrFunctionCall {
            schema,
            name: resolved_name,
            args,
            sql_template,
        }))
    }

    // ── Sequence function helpers ──────────────────────────────────────────────────

    /// Compile `sequence_next(SeqType)` → `nextval('"module"."Name_seq"')`
    /// and `sequence_reset(SeqType[, val])` → `setval(...)`.
    fn compile_sequence_fn(&mut self, fc: &ast::FunctionCall) -> Result<IrExpr, PyQLError> {
        let (module, scalar_name) = self.resolve_sequence_scalar_arg(fc)?;

        if fc.name == "sequence_next" {
            if fc.args.len() != 1 {
                return Err(self.type_err("sequence_next takes exactly 1 argument"));
            }
            let sql = format!("nextval('\"{}\".\"{}_seq\"')", module, scalar_name);
            return Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                schema: None,
                name: "nextval".into(),
                args: vec![],
                sql_template: Some(sql),
            }));
        }

        // sequence_reset
        match fc.args.len() {
            1 => {
                let sql = format!("setval('\"{}\".\"{}_seq\"', 1, false)", module, scalar_name);
                Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: None,
                    name: "setval".into(),
                    args: vec![],
                    sql_template: Some(sql),
                }))
            }
            2 => {
                let val = self.compile_free_expr(&fc.args[1])?;
                let sql = format!("setval('\"{}\".\"{}_seq\"', $1, true)", module, scalar_name);
                Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: None,
                    name: "setval".into(),
                    args: vec![val],
                    sql_template: Some(sql),
                }))
            }
            _ => Err(self.type_err("sequence_reset takes 1 or 2 arguments")),
        }
    }

    /// Resolve the first argument of a sequence function to `(module, scalar_name)`.
    /// The argument must be an unqualified path that names a sequence scalar in the schema.
    fn resolve_sequence_scalar_arg(&self, fc: &ast::FunctionCall) -> Result<(String, String), PyQLError> {
        use crate::parse::ast::{Expr, Path, PathStep};

        let arg = fc.args.first().ok_or_else(|| self.type_err(
            &format!("{}() requires a sequence scalar type as its first argument", fc.name)
        ))?;

        // The parser encodes `module::Name` as a single PathStep::Name("module::Name"),
        // so we split on "::" here to recover the module part.
        let (arg_module, arg_name): (Option<&str>, &str) = match arg {
            Expr::Path(Path { steps, partial: false }) => match steps.as_slice() {
                [PathStep::Name(s)] => {
                    if let Some((m, n)) = s.split_once("::") {
                        (Some(m), n)
                    } else {
                        (None, s.as_str())
                    }
                }
                _ => return Err(self.type_err(&format!(
                    "{}(): first argument must be a sequence scalar type name (e.g. OrderNumber or default::OrderNumber)",
                    fc.name
                ))),
            },
            _ => return Err(self.type_err(&format!(
                "{}(): first argument must be a sequence scalar type name (e.g. OrderNumber or default::OrderNumber)",
                fc.name
            ))),
        };

        let scalar = self.schema.scalars.iter().find(|s| {
            s.is_sequence
                && s.name == arg_name
                && arg_module.map(|m| m == s.module.as_str()).unwrap_or(true)
        });

        match scalar {
            Some(s) => Ok((s.module.clone(), s.name.clone())),
            None => Err(self.type_err(&format!(
                "{}(): '{}' is not a known sequence scalar type",
                fc.name, arg_name
            ))),
        }
    }

    // ── User-defined function helpers ─────────────────────────────────────────────

    /// Try to compile `fn(args) { shape }` as a `FunctionSelect` for an object-returning
    /// user function.  Returns `None` if no matching user function exists (so the caller
    /// can fall through to other dispatch paths).
    fn try_compile_fn_object_select(
        &mut self,
        fc: &ast::FunctionCall,
        elements: &[ast::ShapeElement],
        s: &ast::SelectStmt,
        distinct: bool,
    ) -> Result<Option<IrFunctionSelect>, PyQLError> {
        use crate::schema::FunctionDescriptor;

        let fd: Option<&FunctionDescriptor> = self.schema.functions.iter().find(|f| {
            let module_matches = fc.module.as_deref().map(|m| m == f.module.as_str()).unwrap_or(true);
            module_matches && f.name == fc.name && f.return_is_object
        });
        let fd = match fd {
            Some(f) => f,
            None => return Ok(None),
        };
        if fd.params.len() != fc.args.len() {
            return Err(self.type_err(&format!(
                "function '{}::{}' expects {} argument(s), got {}",
                fd.module, fd.name, fd.params.len(), fc.args.len()
            )));
        }

        let fn_module = fd.module.clone();
        let fn_name = fd.name.clone();
        let return_type_name = fd.return_pg_type.clone(); // qualified type name for object returns
        let polymorphic = fd.return_is_polymorphic;

        let fn_args = fc.args.iter()
            .map(|a| self.compile_free_expr(a))
            .collect::<Result<Vec<_>, _>>()?;

        let alias = self.fresh_alias();

        // Resolve the return type to build the shape.
        let td = self.resolve_type(&return_type_name)?;
        let td = td.clone();

        let (poly_implementors, poly_columns) = if polymorphic {
            self.collect_poly_info(&return_type_name)
        } else {
            (vec![], vec![])
        };

        let td_module = td.module.clone();
        // Build shape against the return type (treat alias as the source alias).
        let shape = self.compile_shape(elements, &td, &alias, &td_module)?;
        let (filter, order_by, offset, limit) = self.compile_path_modifiers(s, &td, &alias)?;

        Ok(Some(IrFunctionSelect {
            fn_module,
            fn_name,
            fn_args,
            alias,
            type_name: return_type_name,
            polymorphic,
            poly_implementors,
            poly_columns,
            shape,
            filter,
            order_by,
            offset,
            limit,
            distinct,
        }))
    }

    /// Collect poly_implementors and poly_columns for a polymorphic return type.
    fn collect_poly_info(&self, type_name: &str) -> (Vec<IrPolyImplementor>, Vec<String>) {
        let implementors = self.find_poly_implementors(type_name);
        let columns = if let Some(td) = self.schema.types.iter().find(|t| {
            format!("{}::{}", t.module, t.name) == type_name
        }) {
            td.properties.iter().map(|p| p.name.clone()).collect()
        } else {
            vec![]
        };
        (implementors, columns)
    }

    // ── vector::search ────────────────────────────────────────────────────────────

    /// Try to compile `vector::search` from a function-call AST node.
    ///
    /// Two overloads are supported:
    /// - `vector::search(TypeName, $vec [, index_name := '…'])` — pre-computed vector
    /// - `vector::search(TypeName, query := $text [, index_name := '…'])` — text overload;
    ///   the Python layer embeds the text and injects `__deferred_vec__` before execution.
    ///
    /// Returns `None` if the call is not `vector::search`.
    fn try_compile_vector_search(
        &mut self,
        fc: &ast::FunctionCall,
        elements: &[ast::ShapeElement],
        s: &ast::SelectStmt,
    ) -> Result<Option<IrVectorSearch>, PyQLError> {
        if fc.module.as_deref() != Some("vector") || fc.name != "search" {
            return Ok(None);
        }

        // Detect text overload: `query :=` kwarg present (no positional second arg needed).
        let text_query_arg = fc.kwargs.iter()
            .find(|(k, _)| k == "query")
            .map(|(_, v)| v);
        let is_text_overload = text_query_arg.is_some();

        if !is_text_overload && fc.args.len() < 2 {
            return Err(self.type_err(
                "vector::search requires either a positional vector argument or `query := $text`"
            ));
        }

        // First argument: a type name or a filtered subquery narrowing the candidate set.
        //   - Bare name:     `Product` or `default::Product`
        //   - Subquery:      `(select Product filter .price < 100)`
        //
        // For the subquery form we store the raw inner filter AST and compile it below,
        // after the source alias is generated, so property column refs use the right alias.
        let (type_qname, inner_filter_ast): (String, Option<ast::Expr>) = match &fc.args[0] {
            ast::Expr::Path(p) if !p.partial => {
                let name = p.steps.iter()
                    .filter_map(|s| if let ast::PathStep::Name(n) = s { Some(n.as_str()) } else { None })
                    .collect::<Vec<_>>().join("::");
                let td = self.resolve_type(&name)
                    .map_err(|_| self.type_err(&format!("vector::search: '{}' is not a known type", name)))?;
                (format!("{}::{}", td.module, td.name), None)
            }
            ast::Expr::SubQuery(stmt) => {
                if let ast::Stmt::Select(inner_sel) = stmt.as_ref() {
                    let inner_type_name = match &inner_sel.result {
                        ast::Expr::Path(p) if !p.partial => {
                            p.steps.iter()
                                .filter_map(|s| if let ast::PathStep::Name(n) = s { Some(n.as_str()) } else { None })
                                .collect::<Vec<_>>().join("::")
                        }
                        _ => return Err(self.type_err(
                            "vector::search: subquery first argument must select a single type (e.g. select Product filter …)"
                        )),
                    };
                    let td = self.resolve_type(&inner_type_name)
                        .map_err(|_| self.type_err(&format!("vector::search: '{}' is not a known type", inner_type_name)))?;
                    let qname = format!("{}::{}", td.module, td.name);
                    (qname, inner_sel.filter.clone())
                } else {
                    return Err(self.type_err("vector::search: subquery first argument must be a SELECT"));
                }
            }
            _ => return Err(self.type_err(
                "vector::search: first argument must be a type name or a filtered subquery (e.g. select Product filter …)"
            )),
        };

        // Optional named argument: index_name := '…'
        let index_name: Option<String> = fc.kwargs.iter()
            .find(|(k, _)| k == "index_name")
            .and_then(|(_, v)| if let ast::Expr::Literal(ast::Literal::Str(s)) = v { Some(s.clone()) } else { None });

        // Resolve the type and find the VectorIndex.
        let td = self.resolve_type(&type_qname)?.clone();
        let vi = td.vector_indexes.iter().find(|vi| vi.index_name.as_deref() == index_name.as_deref())
            .ok_or_else(|| {
                let key = index_name.as_deref().unwrap_or("<default>");
                self.type_err(&format!("type '{}' has no vector index '{}'", type_qname, key))
            })?;

        let vector_col = vi.column_name();
        let distance_op = match vi.metric.as_str() {
            "euclidean"     => "<->",
            "inner_product" => "<#>",
            _               => "<=>",  // cosine (default)
        };

        // Build query expression and inference fields.
        let (query_expr, inference_query_param_name, inference_query_literal,
             inference_model, inference_type_name, inference_index_name);

        if is_text_overload {
            // Text overload: register __deferred_vec__ as the SQL param; Python injects
            // the embedding result into it before executing the query.
            let vec_idx = self.param_index("__deferred_vec__");
            let vec_param = IrExpr::Param { index: vec_idx };
            // Cast float8[] → vector so asyncpg can encode the Python list[float] natively.
            let inner_cast = IrExpr::TypeCast(Box::new(IrTypeCast {
                expr: vec_param,
                pg_type: "float8[]".to_string(),
            tuple_shape: None, }));
            query_expr = IrExpr::TypeCast(Box::new(IrTypeCast {
                expr: inner_cast,
                pg_type: "vector".to_string(),
            tuple_shape: None, }));
            let query_arg = text_query_arg.unwrap();
            inference_query_param_name = Some(match query_arg {
                ast::Expr::Parameter(name) => name.clone(),
                _ => String::new(),
            });
            inference_query_literal = match query_arg {
                ast::Expr::Literal(ast::Literal::Str(s)) => Some(s.clone()),
                _ => None,
            };
            inference_model = Some(vi.model.clone());
            inference_type_name = Some(type_qname.clone());
            inference_index_name = Some(index_name.clone());
        } else {
            // Vector overload: second positional arg is the pre-computed vector.
            let raw_query_expr = self.compile_free_expr(&fc.args[1])?;
            query_expr = IrExpr::TypeCast(Box::new(IrTypeCast {
                expr: raw_query_expr,
                pg_type: "vector".to_string(),
            tuple_shape: None, }));
            inference_query_param_name = None;
            inference_query_literal = None;
            inference_model = None;
            inference_type_name = None;
            inference_index_name = None;
        }

        let alias = self.fresh_alias();
        let source = IrSource {
            type_name: type_qname.clone(),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        // Compile inner_filter_ast (from subquery first arg) now that we have the alias,
        // so property column refs (e.g. `.price`) use the correct table alias.
        let pre_filter: Option<IrExpr> = match inner_filter_ast {
            Some(ref f) => Some(self.compile_expr(f, &td, &alias)?),
            None => None,
        };

        // Compile the object shape from `object { … }` inside the shape elements.
        // `elements` is the outer shape (`{ object { … }, distance }`).
        // We find the `object` element and take its sub-shape; everything else is ignored
        // at compile time (distance is always emitted; unknown pointers are an error).
        let mut object_shape: Vec<IrShapePointer> = vec![];
        for el in elements {
            if el.splat.is_some() { continue; } // ignore splat in outer shape
            let pointer_name = match el.path.steps.first() {
                Some(ast::PathStep::Name(n)) => n.as_str(),
                _ => continue,
            };
            match pointer_name {
                "distance" => { /* always emitted; no sub-shape */ }
                "object" => {
                    let sub_els = el.nested.as_deref().unwrap_or(&[]);
                    object_shape = self.compile_shape(sub_els, &td, &alias, &td.module)?;
                }
                other => {
                    return Err(self.type_err(&format!(
                        "vector::search result has no pointer '{}'; valid pointers are 'object' and 'distance'",
                        other
                    )));
                }
            }
        }

        let (outer_filter, order_by_distance, offset, limit) = self.compile_vs_modifiers(s)?;

        // Merge pre_filter (from subquery first arg) with any outer filter via AND.
        let filter = match (pre_filter, outer_filter) {
            (Some(a), Some(b)) => Some(IrExpr::BinOp(Box::new(IrBinOp {
                left: a,
                op: ast::BinOpKind::And,
                right: b,
            }))),
            (Some(f), None) | (None, Some(f)) => Some(f),
            (None, None) => None,
        };

        Ok(Some(IrVectorSearch {
            source,
            vector_col,
            distance_op,
            query_expr,
            object_shape,
            filter,
            order_by_distance,
            offset,
            limit,
            inference_query_param_name,
            inference_query_literal,
            inference_model,
            inference_type_name,
            inference_index_name,
        }))
    }

    /// Compile `order by`, `filter`, `offset`, `limit` for a VectorSearch source.
    /// Recognises `.distance` (relative path) as the distance expression.
    fn compile_vs_modifiers(
        &mut self,
        s: &ast::SelectStmt,
    ) -> Result<(Option<IrExpr>, Option<IrSortDir>, Option<IrExpr>, Option<IrExpr>), PyQLError> {
        let mut order_by_distance: Option<IrSortDir> = None;
        for sort in &s.order_by {
            let is_distance = matches!(&sort.expr,
                ast::Expr::Path(p) if p.partial && p.steps.len() == 1
                    && matches!(&p.steps[0], ast::PathStep::Name(n) if n == "distance")
            );
            if is_distance {
                let dir = match sort.direction {
                    ast::SortDirection::Desc => IrSortDir::Desc,
                    ast::SortDirection::Asc  => IrSortDir::Asc,
                };
                order_by_distance = Some(dir);
            } else {
                return Err(self.type_err("vector::search: only 'order by .distance' is supported as a sort key"));
            }
        }

        let filter = match &s.filter {
            Some(f) => Some(self.compile_free_expr(f)?),
            None => None,
        };
        let offset = match &s.offset {
            Some(o) => Some(self.compile_free_expr(o)?),
            None => None,
        };
        let limit = match &s.limit {
            Some(l) => Some(self.compile_free_expr(l)?),
            None => None,
        };

        Ok((filter, order_by_distance, offset, limit))
    }

    // ── fts::search ──────────────────────────────────────────────────────────────

    /// Try to compile `fts::search(TypeName, $query [, index_name := '…'] [, mode := '…'])`.
    /// Returns `None` if the call is not `fts::search`.
    fn try_compile_fts_search(
        &mut self,
        fc: &ast::FunctionCall,
        elements: &[ast::ShapeElement],
        s: &ast::SelectStmt,
    ) -> Result<Option<IrFtsSearch>, PyQLError> {
        if fc.module.as_deref() != Some("fts") || fc.name != "search" {
            return Ok(None);
        }
        if fc.args.len() < 2 {
            return Err(self.type_err("fts::search requires at least 2 arguments: (TypeName, $query)"));
        }

        // First argument: a bare type name reference.
        let type_qname = match &fc.args[0] {
            ast::Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    let td = self.resolve_type(n)
                        .map_err(|_| self.type_err(&format!("fts::search: '{}' is not a known type", n)))?;
                    format!("{}::{}", td.module, td.name)
                } else {
                    return Err(self.type_err("fts::search: first argument must be a type name"));
                }
            }
            _ => return Err(self.type_err("fts::search: first argument must be a bare type name")),
        };

        // Optional named arguments.
        let index_name: Option<String> = fc.kwargs.iter()
            .find(|(k, _)| k == "index_name")
            .and_then(|(_, v)| if let ast::Expr::Literal(ast::Literal::Str(s)) = v { Some(s.clone()) } else { None });

        let mode_str = fc.kwargs.iter()
            .find(|(k, _)| k == "mode")
            .and_then(|(_, v)| if let ast::Expr::Literal(ast::Literal::Str(s)) = v { Some(s.as_str()) } else { None })
            .unwrap_or("BestFields");

        let tsquery_fn: &'static str = match mode_str {
            "Phrase" => "phraseto_tsquery",
            _ => "websearch_to_tsquery",   // BestFields and PhrasePrefix both use websearch
        };

        // Resolve the type and find the SearchIndex.
        let td = self.resolve_type(&type_qname)?.clone();
        let si = td.search_indexes.iter()
            .find(|si| si.index_name.as_deref() == index_name.as_deref())
            .ok_or_else(|| {
                let key = index_name.as_deref().unwrap_or("<default>");
                self.type_err(&format!("type '{}' has no search index '{}'", type_qname, key))
            })?;

        let backend = si.backend.clone();
        let search_col = si.column_name();
        let is_deferred = backend != SearchBackend::Postgres;
        let deferred_index_name = if is_deferred {
            Some(si.deferred_index_name(&td.module, &td.name))
        } else {
            None
        };

        // Second argument: the query text expression.
        // For deferred backends, IDs/scores are the only SQL params ($1/$2). The query
        // text is consumed by the Python layer before SQL execution, so we extract its
        // name/literal directly from the AST without registering a SQL param for it.
        let query_expr;
        let deferred_query_param_name;
        let deferred_query_literal;
        let deferred_ids_param;
        let deferred_scores_param;

        if is_deferred {
            let ids_idx = self.param_index("__deferred_ids__");
            let scores_idx = self.param_index("__deferred_scores__");
            deferred_ids_param = Some(ids_idx);
            deferred_scores_param = Some(scores_idx);
            deferred_query_param_name = match &fc.args[1] {
                ast::Expr::Parameter(name) => Some(name.clone()),
                _ => None,
            };
            deferred_query_literal = match &fc.args[1] {
                ast::Expr::Literal(ast::Literal::Str(s)) => Some(s.clone()),
                _ => None,
            };
            // Placeholder — not used in SQL for the deferred backend.
            query_expr = IrExpr::Literal(crate::ir::IrLiteral::Str(String::new()));
        } else {
            query_expr = self.compile_free_expr(&fc.args[1])?;
            deferred_query_param_name = None;
            deferred_query_literal = None;
            deferred_ids_param = None;
            deferred_scores_param = None;
        }

        let alias = self.fresh_alias();
        let source = IrSource {
            type_name: type_qname.clone(),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        // Compile the object shape from `object { … }` in the outer shape.
        let mut object_shape: Vec<IrShapePointer> = vec![];
        for el in elements {
            if el.splat.is_some() { continue; }
            let pointer_name = match el.path.steps.first() {
                Some(ast::PathStep::Name(n)) => n.as_str(),
                _ => continue,
            };
            match pointer_name {
                "score" => { /* always emitted */ }
                "object" => {
                    let sub_els = el.nested.as_deref().unwrap_or(&[]);
                    object_shape = self.compile_shape(sub_els, &td, &alias, &td.module)?;
                }
                other => {
                    return Err(self.type_err(&format!(
                        "fts::search result has no pointer '{}'; valid pointers are 'object' and 'score'",
                        other
                    )));
                }
            }
        }

        let (filter, order_by_rank, offset, limit) = self.compile_fts_modifiers(s)?;

        Ok(Some(IrFtsSearch {
            source,
            backend,
            search_col,
            tsquery_fn,
            query_expr,
            object_shape,
            filter,
            order_by_rank,
            offset,
            limit,
            deferred_index_name,
            deferred_query_param_name,
            deferred_query_literal,
            deferred_ids_param,
            deferred_scores_param,
        }))
    }

    /// Compile `order by`, `filter`, `offset`, `limit` for a FtsSearch source.
    /// Recognises `.score` (relative path) as the score expression.
    fn compile_fts_modifiers(
        &mut self,
        s: &ast::SelectStmt,
    ) -> Result<(Option<IrExpr>, Option<IrSortDir>, Option<IrExpr>, Option<IrExpr>), PyQLError> {
        let mut order_by_rank: Option<IrSortDir> = None;
        for sort in &s.order_by {
            let is_rank = matches!(&sort.expr,
                ast::Expr::Path(p) if p.partial && p.steps.len() == 1
                    && matches!(&p.steps[0], ast::PathStep::Name(n) if n == "score")
            );
            if is_rank {
                let dir = match sort.direction {
                    ast::SortDirection::Desc => IrSortDir::Desc,
                    ast::SortDirection::Asc  => IrSortDir::Asc,
                };
                order_by_rank = Some(dir);
            } else {
                return Err(self.type_err("fts::search: only 'order by .score' is supported as a sort key"));
            }
        }

        let filter = match &s.filter {
            Some(f) => Some(self.compile_free_expr(f)?),
            None => None,
        };
        let offset = match &s.offset {
            Some(o) => Some(self.compile_free_expr(o)?),
            None => None,
        };
        let limit = match &s.limit {
            Some(l) => Some(self.compile_free_expr(l)?),
            None => None,
        };

        Ok((filter, order_by_rank, offset, limit))
    }

    // ── Error helpers ─────────────────────────────────────────────────────────────

    fn type_err(&self, msg: &str) -> PyQLError {
        PyQLError::Type(PyQLTypeError {
            message: msg.to_string(),
            position: Position { line: 0, col: 0 },
        })
    }

    fn field_err(&self, field: &str, type_name: &str) -> PyQLError {
        PyQLError::Resolution(PyQLResolutionError::UnknownField(PyQLUnknownFieldError {
            message: format!("object type '{type_name}' has no link or property '{field}'"),
            position: Position { line: 0, col: 0 },
        }))
    }

    // ── Default returning (pk only, matching Gel's bare DML behaviour) ────────────

    /// For bare DML (not wrapped in SELECT) return only primary-key properties,
    /// matching Gel: `INSERT … ` returns `{ id }`, same for UPDATE/DELETE.
    fn pk_returning(td: &TypeDescriptor) -> Vec<IrShapePointer> {
        td.properties
            .iter()
            .filter(|p| p.is_pk)
            .map(|p| {
                IrShapePointer::Scalar(IrScalarPointer {
                    alias: p.name.clone(),
                    column: p.name.clone(),
                    pg_type: p.pg_type.clone(),
                tuple_shape: None, })
            })
            .collect()
    }

    // ── Mutation rewrite compilation ─────────────────────────────────────────────

    /// Compile all active rewrites on `td`'s properties for the given event mask
    /// (1 = INSERT, 2 = UPDATE). Uses `self` so the alias counter and param list
    /// are shared with the surrounding statement.
    fn compile_rewrites(
        &mut self,
        td: &TypeDescriptor,
        alias: &str,
        on_mask: u8,
    ) -> Result<Vec<IrRewrite>, PyQLError> {
        let mut out = Vec::new();
        for prop in &td.properties {
            for rw in &prop.rewrites {
                if rw.on & on_mask == 0 {
                    continue;
                }
                let expr_ast = crate::parse::parse_expr(&rw.handler)
                    .map_err(PyQLError::Syntax)?;
                let ir_expr = self.compile_expr(&expr_ast, td, alias)?;
                out.push(IrRewrite { column: prop.name.clone(), expr: ir_expr });
            }
        }
        Ok(out)
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────────

/// True if any node in a multilink value tree (including both sides of any
/// nested `union`) carries a `@prop := value` link-property assignment.
fn has_any_link_props(vals: &IrMultiLinkValues) -> bool {
    if !vals.link_props.is_empty() {
        return true;
    }
    match &vals.source {
        IrMultiLinkValueSource::Union(a, b) => has_any_link_props(a) || has_any_link_props(b),
        _ => false,
    }
}

/// Extract the single pointer name from a relative path used in a shape element.
fn path_leaf(p: &ast::Path) -> Result<&str, PyQLError> {
    match p.steps.as_slice() {
        [ast::PathStep::Name(n)] => Ok(n.as_str()),
        _ => Err(PyQLError::Type(PyQLTypeError {
            message: "expected a simple pointer name in shape element".into(),
            position: Position { line: 0, col: 0 },
        })),
    }
}

/// Map a PyQL type expression to a PostgreSQL type string.
fn type_expr_to_pg(ty: &ast::TypeExpr) -> Result<String, PyQLError> {
    let Some((module, bare_name)) = ty.as_named() else {
        // Callers resolve a structural tuple/array directly before ever
        // reaching this function (see `resolve_cast_pg_type`) — reaching here
        // with one would be an internal bug, not a user-facing scenario.
        return Err(PyQLError::Type(PyQLTypeError {
            message: "internal error: structural tuple/array type reached type_expr_to_pg".into(),
            position: Position { line: 0, col: 0 },
        }));
    };

    // pgvector:: types map directly to PostgreSQL types.
    if module == Some("pgvector") {
        return match bare_name {
            "vector" => Ok("vector".to_string()),
            other => Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown pgvector type '{other}'; valid types are: vector"),
                position: Position { line: 0, col: 0 },
            })),
        };
    }

    // cal:: types map directly to PostgreSQL types.
    if module == Some("cal") {
        let pg = match bare_name {
            "local_datetime" => "timestamp",
            "local_date"     => "date",
            "local_time"     => "time",
            "relative_duration" | "date_duration" => "interval",
            other => return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "unknown cal type '{other}'; \
                     valid types are: local_datetime, local_date, local_time, \
                     relative_duration, date_duration"
                ),
                position: Position { line: 0, col: 0 },
            })),
        };
        return Ok(pg.to_string());
    }

    let name = match module {
        Some("std") | None => bare_name,
        Some(m) => {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown type '{}::{}'", m, bare_name),
                position: Position { line: 0, col: 0 },
            }))
        }
    };
    Ok(match name {
        "str" => "text",
        "int16" => "int2",
        "int32" => "int4",
        "int64" => "int8",
        "float32" => "float4",
        "float64" => "float8",
        "bool" => "boolean",
        "uuid" => "uuid",
        "bytes" => "bytea",
        "json" => "jsonb",
        "decimal" => "numeric",
        "datetime" => "timestamptz",
        "date" => "date",
        "time" => "time",
        "duration" => "interval",
        other => return Err(PyQLError::Type(PyQLTypeError {
            message: format!("unknown type '{other}'"),
            position: Position { line: 0, col: 0 },
        })),
    }
    .to_string())
}

/// Check whether a compiled expression is compatible with a PylonType parameter.
/// Used for overload selection when multiple overloads share the same name.
/// Emit `($1 IS NOT NULL)` for a given IR expression.
fn ir_is_not_null(expr: IrExpr) -> IrExpr {
    IrExpr::FunctionCall(IrFunctionCall {
        schema: None,
        name: String::new(),
        args: vec![expr],
        sql_template: Some("($1 IS NOT NULL)".to_string()),
    })
}

fn pylon_type_matches(expr: &IrExpr, ty: &crate::stdlib::PylonType) -> bool {
    use crate::stdlib::PylonType as PT;
    match ty {
        // Wildcard params always match.
        PT::Any | PT::AnyOrderable | PT::AnyPoint => true,
        PT::Array(_) => is_array_expr(expr),
        PT::Json => infer_ir_type(expr) == Some("jsonb"),
        PT::Bytes => infer_ir_type(expr) == Some("bytea"),
        PT::Str => infer_ir_type(expr) == Some("text"),
        PT::Bool => infer_ir_type(expr) == Some("boolean"),
        PT::Uuid => infer_ir_type(expr) == Some("uuid"),
        PT::Int16 | PT::Int32 | PT::Int64 | PT::BigInt =>
            matches!(infer_ir_type(expr), Some(t) if INT_TYPES.contains(&t)),
        PT::Float32 | PT::Float64 =>
            matches!(infer_ir_type(expr), Some(t) if FLOAT_TYPES.contains(&t)),
        PT::Range(_) => matches!(infer_ir_type(expr), Some(t) if t.ends_with("range") && !t.starts_with('m')),
        PT::Multirange(_) => matches!(infer_ir_type(expr), Some(t) if t.starts_with("multi")),
        // For unrecognised / complex types, allow (don't reject).
        _ => true,
    }
}

fn literal_sentinel_to_pg(t: &str) -> &str {
    match t {
        "__int_literal" => "int8",
        "__float_literal" => "float8",
        other => other,
    }
}

fn is_array_expr(expr: &IrExpr) -> bool {
    match expr {
        IrExpr::Array(_) | IrExpr::ArrayFromSelect(_) => true,
        IrExpr::ColumnRef { pg_type, .. } => pg_type.ends_with("[]"),
        IrExpr::TypeCast(tc) => tc.pg_type.ends_with("[]"),
        _ => false,
    }
}

fn expr_to_std_type(expr: &ast::Expr) -> &'static str {
    match expr {
        ast::Expr::Literal(ast::Literal::Str(_)) => "std::str",
        ast::Expr::Literal(ast::Literal::Int(_)) => "std::int64",
        ast::Expr::Literal(ast::Literal::Float(_)) => "std::float64",
        ast::Expr::Literal(ast::Literal::Bool(_)) => "std::bool",
        _ => "anytype",
    }
}

fn named_tuple_type_str(fields: &[(String, ast::Expr)]) -> String {
    let inner = fields
        .iter()
        .map(|(k, v)| format!("{}: {}", k, expr_to_std_type(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("tuple<{}>", inner)
}

fn positional_tuple_type_str(elems: &[ast::Expr]) -> String {
    let inner = elems.iter().map(expr_to_std_type).collect::<Vec<_>>().join(", ");
    format!("tuple<{}>", inner)
}

fn infer_ir_type(expr: &IrExpr) -> Option<&str> {
    match expr {
        IrExpr::ColumnRef { pg_type, .. } => Some(pg_type.as_str()),
        IrExpr::TypeCast(tc) => Some(tc.pg_type.as_str()),
        IrExpr::Literal(lit) => Some(match lit {
            IrLiteral::Str(_) => "text",
            IrLiteral::Int(_) => "__int_literal",
            IrLiteral::Float(_) => "__float_literal",
            IrLiteral::Bool(_) => "boolean",
        }),
        IrExpr::EnumLiteral { pg_type, .. } => Some(pg_type.as_str()),
        IrExpr::NamedTuple(_) => Some("jsonb"),
        IrExpr::GlobalParam { pg_type, .. } => Some(pg_type.as_str()),
        _ => None,
    }
}

const INT_TYPES: &[&str] = &["int2", "int4", "int8", "__int_literal"];
const FLOAT_TYPES: &[&str] = &["float4", "float8", "__float_literal"];

fn types_compatible(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_int = INT_TYPES.contains(&a);
    let b_int = INT_TYPES.contains(&b);
    if a_int && b_int {
        return true;
    }
    let a_float = FLOAT_TYPES.contains(&a);
    let b_float = FLOAT_TYPES.contains(&b);
    a_float && b_float
}

/// Collect `SearchEnqueueInfo` for all OpenSearch-backed search indexes on a type.
fn collect_search_enqueue(
    td: &TypeDescriptor,
    type_name: &str,
    operation: &'static str,
) -> Vec<SearchEnqueueInfo> {
    td.search_indexes.iter()
        .filter(|si| si.backend == SearchBackend::OpenSearch)
        .map(|si| SearchEnqueueInfo {
            type_name: type_name.to_string(),
            index_name: si.index_name.clone(),
            operation,
        })
        .collect()
}

fn pg_type_to_pyql(pg: &str) -> &str {
    match pg {
        "text" | "varchar" => "std::str",
        "uuid"             => "std::uuid",
        "int2"             => "std::int16",
        "int4"             => "std::int32",
        "int8"             => "std::int64",
        "float4"           => "std::float32",
        "float8"           => "std::float64",
        "boolean"          => "std::bool",
        "numeric"          => "std::decimal",
        "timestamptz"      => "std::datetime",
        "__int_literal"    => "std::int64",
        "__float_literal"  => "std::float64",
        other              => other,
    }
}

/// Walk an `IrExpr` tree and replace every `ColumnRef` whose column name appears
/// in `bindings` with the bound expression.
///
/// Used for INSERT rewrites: the handler `lower(.name)` compiles to
/// `FunctionCall(lower, [ColumnRef("name")])`. If `name := $1` in the INSERT,
/// substituting produces `FunctionCall(lower, [Param(0)])`, which is valid in a
/// VALUES clause.
pub(super) fn substitute_col_refs(
    expr: IrExpr,
    bindings: &HashMap<String, IrExpr>,
) -> IrExpr {
    match expr {
        IrExpr::ColumnRef { ref column, .. } => {
            if let Some(replacement) = bindings.get(column) {
                replacement.clone()
            } else {
                expr
            }
        }
        IrExpr::BinOp(op) => IrExpr::BinOp(Box::new(IrBinOp {
            left: substitute_col_refs(op.left, bindings),
            op: op.op,
            right: substitute_col_refs(op.right, bindings),
        })),
        IrExpr::UnaryOp(op) => IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: op.op,
            operand: substitute_col_refs(op.operand, bindings),
        })),
        IrExpr::FunctionCall(f) => IrExpr::FunctionCall(IrFunctionCall {
            schema: f.schema,
            name: f.name,
            args: f.args.into_iter().map(|a| substitute_col_refs(a, bindings)).collect(),
            sql_template: f.sql_template,
        }),
        IrExpr::TypeCast(c) => IrExpr::TypeCast(Box::new(IrTypeCast {
            expr: substitute_col_refs(c.expr, bindings),
            pg_type: c.pg_type,
        tuple_shape: None, })),
        IrExpr::IfElse(ie) => IrExpr::IfElse(Box::new(IrIfElse {
            condition: substitute_col_refs(ie.condition, bindings),
            if_: substitute_col_refs(ie.if_, bindings),
            else_: substitute_col_refs(ie.else_, bindings),
        })),
        IrExpr::Array(elems) => {
            IrExpr::Array(elems.into_iter().map(|e| substitute_col_refs(e, bindings)).collect())
        }
        IrExpr::NamedTuple(fields) => {
            IrExpr::NamedTuple(fields.into_iter().map(|(k, v)| (k, substitute_col_refs(v, bindings))).collect())
        }
        // Literals, Params, Subqueries — no column refs to substitute
        other => other,
    }
}
