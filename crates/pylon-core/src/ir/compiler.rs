use crate::error::{
    Position, PyQLError, PyQLResolutionError, PyQLTypeError,
    PyQLUnknownFieldError, PyQLUnknownTypeError,
};
use crate::parse::ast::{
    self, Expr, Literal, NonesOrder, ShapeElement, SortDirection, Stmt,
};
use crate::schema::{
    LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
};

use std::collections::HashMap;

use super::{
    IrBinOp, IrComputedField, IrDelete, IrExpr, IrFunctionCall, IrIfElse, IrInsert, IrLiteral,
    IrMultiLinkField, IrMultiLinkJoin, IrNulls, IrOutput, IrRewrite, IrScalarField, IrSelect,
    IrShapeField, IrSingleLinkField, IrSort, IrSortDir, IrSource, IrStmt, IrTypeCast, IrUnaryOp,
    IrUpdate,
};

// ── Public entry point ──────────────────────────────────────────────────────────

/// Compile a parsed PyQL statement against the schema.
/// Returns the IR plan and the ordered list of parameter names (matching $1, $2, …).
pub fn compile(stmt: &Stmt, schema: &SchemaDescriptor) -> Result<IrOutput, PyQLError> {
    let mut c = Compiler::new(schema);
    let ir = c.compile_stmt(stmt)?;
    Ok(IrOutput { stmt: ir, params: c.params })
}

/// Compile a single PyQL expression in the context of a named type.
/// Used for schema fragments: computed fields, rewrite handlers, constraint exprs.
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

// ── Compiler context ────────────────────────────────────────────────────────────

struct Compiler<'a> {
    schema: &'a SchemaDescriptor,
    /// Ordered parameter names — index + 1 is the $N position in SQL.
    params: Vec<String>,
    alias_counter: usize,
}

impl<'a> Compiler<'a> {
    fn new(schema: &'a SchemaDescriptor) -> Self {
        Compiler { schema, params: vec![], alias_counter: 0 }
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
            Stmt::Select(s) => self.compile_select(s).map(IrStmt::Select),
            Stmt::Insert(s) => self.compile_insert(s).map(IrStmt::Insert),
            Stmt::Update(s) => self.compile_update(s).map(IrStmt::Update),
            Stmt::Delete(s) => self.compile_delete(s).map(IrStmt::Delete),
        }
    }

    // ── SELECT ────────────────────────────────────────────────────────────────────

    fn compile_select(&mut self, sel: &ast::SelectStmt) -> Result<IrSelect, PyQLError> {
        let (type_name, shape_elements, inner_stmt) =
            self.extract_type_and_shape(&sel.result)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let source = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
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

        Ok(IrSelect { source, shape, filter, order_by, offset, limit, dml_source })
    }

    /// Unwrap `Shape(expr, elements)` or bare `Path` from a SELECT result.
    /// Returns (type_name, shape_elements, optional_inner_stmt).
    /// The inner stmt is Some when the subject is `(INSERT …)` / `(SELECT …)` etc.
    fn extract_type_and_shape<'e>(
        &self,
        expr: &'e Expr,
    ) -> Result<(String, &'e [ShapeElement], Option<&'e Stmt>), PyQLError> {
        match expr {
            Expr::Shape(s) => {
                // s.expr is Option<Expr> (not Box), so use as_ref() not as_deref()
                let (type_name, inner) = match s.expr.as_ref() {
                    Some(Expr::SubQuery(stmt)) => {
                        (self.dml_subject_type(stmt)?, Some(stmt.as_ref()))
                    }
                    Some(inner) => (self.expr_as_type_name(inner)?, None),
                    None => {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "shape without subject expression".into(),
                            position: Position { line: 0, col: 0 },
                        }))
                    }
                };
                Ok((type_name, &s.elements, inner))
            }
            // Bare `SELECT (DML)` without an outer shape
            Expr::SubQuery(stmt) => {
                Ok((self.dml_subject_type(stmt)?, &[], Some(stmt.as_ref())))
            }
            _ => Ok((self.expr_as_type_name(expr)?, &[], None)),
        }
    }

    /// Extract the target type name from a DML or inner SELECT statement.
    fn dml_subject_type(&self, stmt: &Stmt) -> Result<String, PyQLError> {
        match stmt {
            Stmt::Insert(ins) => Ok(ins.subject.name.clone()),
            Stmt::Update(upd) => self.expr_as_type_name(&upd.subject),
            Stmt::Delete(del) => self.expr_as_type_name(&del.subject),
            Stmt::Select(sel) => {
                // SELECT-over-SELECT: get the type from the inner select's result
                let (type_name, _, _) = self.extract_type_and_shape(&sel.result)?;
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
        let alias = self.fresh_alias();
        let target = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        let assignments = self.compile_assignments(&ins.shape, td, &alias)?;
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
        let returning = Self::pk_returning(td);

        Ok(IrInsert {
            target,
            assignments,
            unless_conflict: None,
            rewrites,
            returning,
        })
    }

    fn compile_assignments(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        elements
            .iter()
            .map(|el| {
                let field_name = path_leaf(&el.path)?;
                let expr = el.compexpr.as_ref().ok_or_else(|| {
                    PyQLError::Type(PyQLTypeError {
                        message: format!("INSERT field '{field_name}' has no value expression"),
                        position: Position { line: 0, col: 0 },
                    })
                })?;

                // Validate the field exists
                let column = if let Some(p) = Self::resolve_property(td, field_name) {
                    p.name.clone()
                } else if let Some(l) = Self::resolve_link(td, field_name) {
                    // Link assignment via subquery: `company := (SELECT Company FILTER ...)`
                    // Compile as a scalar subquery returning the target pk (the FK uuid).
                    if let Expr::SubQuery(inner_stmt) = expr {
                        let ir_expr = self.compile_link_subquery(inner_stmt)?;
                        return Ok((l.name.clone(), ir_expr));
                    }
                    l.name.clone()
                } else {
                    return Err(self.field_err(field_name, &td.name));
                };

                let ir_expr = self.compile_expr(expr, td, alias)?;
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

        let assignments = self.compile_assignments(&upd.shape, td, &alias)?;
        // UPDATE rewrites reference the live row via the table alias — no substitution needed.
        let rewrites = self.compile_rewrites(td, &alias, 2)?;
        let returning = Self::pk_returning(td);

        Ok(IrUpdate { target, filter, assignments, rewrites, returning })
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

        Ok(IrDelete { target, filter, returning })
    }

    // ── Shape compilation ─────────────────────────────────────────────────────────

    fn compile_shape(
        &mut self,
        elements: &[ShapeElement],
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<Vec<IrShapeField>, PyQLError> {
        if elements.is_empty() {
            // No explicit shape: include all scalar properties.
            return Ok(td
                .properties
                .iter()
                .map(|p| {
                    IrShapeField::Scalar(IrScalarField {
                        alias: p.name.clone(),
                        column: p.name.clone(),
                        pg_type: p.pg_type.clone(),
                    })
                })
                .collect());
        }

        elements.iter().map(|el| self.compile_shape_element(el, td, alias, module)).collect()
    }

    fn compile_shape_element(
        &mut self,
        el: &ShapeElement,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<IrShapeField, PyQLError> {
        let field_name = path_leaf(&el.path)?;

        // Computed override: `field := expr`
        if let Some(compexpr) = &el.compexpr {
            let ir = self.compile_expr(compexpr, td, alias)?;
            return Ok(IrShapeField::Computed(IrComputedField {
                alias: field_name.to_string(),
                expr: ir,
            }));
        }

        // Scalar property
        if let Some(p) = Self::resolve_property(td, field_name) {
            return Ok(IrShapeField::Scalar(IrScalarField {
                alias: field_name.to_string(),
                column: p.name.clone(),
                pg_type: p.pg_type.clone(),
            }));
        }

        // Single link
        if let Some(l) = Self::resolve_link(td, field_name) {
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
                dml_source: None,
            };
            return Ok(IrShapeField::SingleLink(IrSingleLinkField {
                alias: field_name.to_string(),
                fk_column: l.name.clone(),
                target_pk: "id".to_string(),
                subquery,
            }));
        }

        // Multi-link
        if let Some(ml) = Self::resolve_multilink(td, field_name) {
            let target_td = self.resolve_type(&ml.target)?;
            let sub_alias = self.fresh_alias();
            let nested_elements = el.nested.as_deref().unwrap_or(&[]);
            let sub_shape =
                self.compile_shape(nested_elements, target_td, &sub_alias, &target_td.module.clone())?;

            let join = IrMultiLinkJoin::Standard {
                junction_table: format!("{}.{}", td.table, ml.name),
                module: module.to_string(),
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
                dml_source: None,
            };

            return Ok(IrShapeField::MultiLink(IrMultiLinkField {
                alias: field_name.to_string(),
                join,
                subquery,
            }));
        }

        Err(self.field_err(field_name, &td.name))
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

            Expr::Literal(lit) => Ok(IrExpr::Literal(match lit {
                Literal::Str(s) => IrLiteral::Str(s.clone()),
                Literal::Int(n) => IrLiteral::Int(*n),
                Literal::Float(f) => IrLiteral::Float(*f),
                Literal::Bool(b) => IrLiteral::Bool(*b),
            })),

            Expr::BinOp(b) => {
                let left = self.compile_expr(&b.left, td, alias)?;
                let right = self.compile_expr(&b.right, td, alias)?;
                Ok(IrExpr::BinOp(Box::new(IrBinOp { left, op: b.op.clone(), right })))
            }

            Expr::UnaryOp(u) => {
                let operand = self.compile_expr(&u.operand, td, alias)?;
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp { op: u.op.clone(), operand })))
            }

            Expr::FunctionCall(f) => {
                let args = f
                    .args
                    .iter()
                    .map(|a| self.compile_expr(a, td, alias))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: f.module.clone(),
                    name: f.name.clone(),
                    args,
                }))
            }

            Expr::TypeCast(tc) => {
                let inner = self.compile_expr(&tc.expr, td, alias)?;
                let pg_type = type_expr_to_pg(&tc.ty)?;
                Ok(IrExpr::TypeCast(Box::new(IrTypeCast { expr: inner, pg_type })))
            }

            Expr::IfElse(ie) => {
                let condition = self.compile_expr(&ie.condition, td, alias)?;
                let if_ = self.compile_expr(&ie.if_expr, td, alias)?;
                let else_ = self.compile_expr(&ie.else_expr, td, alias)?;
                Ok(IrExpr::IfElse(Box::new(IrIfElse { condition, if_, else_ })))
            }

            Expr::Shape(_) | Expr::Tuple(_) | Expr::NamedTuple(_) | Expr::Array(_) => {
                Err(PyQLError::Type(PyQLTypeError {
                    message: "shapes, tuples, and arrays are not valid in expression context"
                        .into(),
                    position: Position { line: 0, col: 0 },
                }))
            }

            Expr::SubQuery(_) => Err(PyQLError::Type(PyQLTypeError {
                message: "sub-statement (SELECT/INSERT/UPDATE/DELETE) used as expression is \
                           only valid as the subject of a SELECT result"
                    .into(),
                position: Position { line: 0, col: 0 },
            })),
        }
    }

    fn compile_path(
        &mut self,
        p: &ast::Path,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        if !p.partial {
            return Err(PyQLError::Type(PyQLTypeError {
                message: "absolute paths are not valid in expression context; use .field".into(),
                position: Position { line: 0, col: 0 },
            }));
        }

        if p.steps.len() != 1 {
            return Err(PyQLError::Type(PyQLTypeError {
                message: "nested path traversal in expressions is not yet supported".into(),
                position: Position { line: 0, col: 0 },
            }));
        }

        let field_name = match &p.steps[0] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "type intersections are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }))
            }
        };

        if let Some(prop) = Self::resolve_property(td, field_name) {
            return Ok(IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: prop.name.clone(),
                pg_type: prop.pg_type.clone(),
            });
        }

        if let Some(link) = Self::resolve_link(td, field_name) {
            // FK column reference (uuid)
            return Ok(IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: link.name.clone(),
                pg_type: "uuid".to_string(),
            });
        }

        Err(self.field_err(field_name, &td.name))
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
            dml_source: None,
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

    // ── Error helpers ─────────────────────────────────────────────────────────────

    fn type_err(&self, msg: &str) -> PyQLError {
        PyQLError::Type(PyQLTypeError {
            message: msg.to_string(),
            position: Position { line: 0, col: 0 },
        })
    }

    fn field_err(&self, field: &str, type_name: &str) -> PyQLError {
        PyQLError::Resolution(PyQLResolutionError::UnknownField(PyQLUnknownFieldError {
            message: format!("type '{type_name}' has no field '{field}'"),
            position: Position { line: 0, col: 0 },
        }))
    }

    // ── Default returning (pk only, matching Gel's bare DML behaviour) ────────────

    /// For bare DML (not wrapped in SELECT) return only primary-key properties,
    /// matching Gel: `INSERT … ` returns `{ id }`, same for UPDATE/DELETE.
    fn pk_returning(td: &TypeDescriptor) -> Vec<IrShapeField> {
        td.properties
            .iter()
            .filter(|p| p.is_pk)
            .map(|p| {
                IrShapeField::Scalar(IrScalarField {
                    alias: p.name.clone(),
                    column: p.name.clone(),
                    pg_type: p.pg_type.clone(),
                })
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

/// Extract the single field name from a relative path used in a shape element.
fn path_leaf(p: &ast::Path) -> Result<&str, PyQLError> {
    match p.steps.as_slice() {
        [ast::PathStep::Name(n)] => Ok(n.as_str()),
        _ => Err(PyQLError::Type(PyQLTypeError {
            message: "expected a simple field name in shape element".into(),
            position: Position { line: 0, col: 0 },
        })),
    }
}

/// Map a PyQL type expression to a PostgreSQL type string.
fn type_expr_to_pg(ty: &ast::TypeExpr) -> Result<String, PyQLError> {
    let name = match ty.module.as_deref() {
        Some("std") | None => ty.name.as_str(),
        Some(m) => {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown type module '{m}'"),
                position: Position { line: 0, col: 0 },
            }))
        }
    };
    Ok(match name {
        "str" | "Str" => "text",
        "int16" | "Int16" => "int2",
        "int32" | "Int32" => "int4",
        "int64" | "Int64" | "int" | "Int" => "int8",
        "float32" | "Float32" => "float4",
        "float64" | "Float64" | "float" | "Float" => "float8",
        "bool" | "Bool" => "boolean",
        "uuid" | "Uuid" => "uuid",
        "bytes" | "Bytes" => "bytea",
        "json" | "Json" => "jsonb",
        "decimal" | "Decimal" => "numeric",
        "datetime" | "Datetime" => "timestamptz",
        "date" | "Date" => "date",
        "time" | "Time" => "time",
        other => other, // pass through for user-defined types / domains
    }
    .to_string())
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
        }),
        IrExpr::TypeCast(c) => IrExpr::TypeCast(Box::new(IrTypeCast {
            expr: substitute_col_refs(c.expr, bindings),
            pg_type: c.pg_type,
        })),
        IrExpr::IfElse(ie) => IrExpr::IfElse(Box::new(IrIfElse {
            condition: substitute_col_refs(ie.condition, bindings),
            if_: substitute_col_refs(ie.if_, bindings),
            else_: substitute_col_refs(ie.else_, bindings),
        })),
        // Literals, Params, Subqueries — no column refs to substitute
        other => other,
    }
}
