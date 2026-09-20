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

use crate::error::{
    Position, PyQLError, PyQLResolutionError, PyQLTypeError, PyQLUnknownFieldError, PyQLUnknownTypeError,
};
use crate::parse::ast::{self, Expr, Literal, NonesOrder, ShapeElement, ShapeOp, SortDirection, Stmt};
use crate::schema::{
    FunctionDescriptor, LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, SearchBackend,
    TypeDescriptor,
};

use std::collections::HashMap;

use super::{
    IrArraySource, IrBinOp, IrComputedGlobalCte, IrComputedPointer, IrConflict, IrCteDef, IrDelete, IrExpr, IrFor,
    IrForIterator, IrFreeExpr, IrFtsSearch, IrFunctionCall, IrFunctionSelect, IrGlobalCte, IrGroup, IrIfElse, IrInsert,
    IrLinkProp, IrLiteral, IrLockClause, IrLockStrength, IrLockWait, IrMultiLinkClear, IrMultiLinkJoin,
    IrMultiLinkMutation, IrMultiLinkPointer, IrMultiLinkValueSource, IrMultiLinkValues, IrNulls, IrOutput, IrPathJoin,
    IrPathResult, IrPathSelect, IrPolyFanout, IrPolyImplementor, IrRewrite, IrRowSource, IrScalarPointer,
    IrScalarSetPointer, IrSelect, IrSessionGlobalCte, IrShapePointer, IrSingleLinkCorrelation, IrSingleLinkPointer,
    IrSort, IrSortDir, IrSource, IrStmt, IrTypeCast, IrUnaryOp, IrUpdate, IrVectorSearch, SearchEnqueueInfo,
    TupleCastShape, VectorEnqueueInfo,
};

// ── Compiled-clause tuples ──────────────────────────────────────────────────────
//
// The three helpers that pull a statement's trailing clauses apart all return
// the same shape: the pieces in source order, each optional because a query
// need not spell any of them out. Named here so the signatures read as one
// concept rather than as an anonymous 4-tuple repeated three times.

/// `(filter, order_by, offset, limit)` — a plain `select`'s trailing clauses.
type SelectModifiers = (Option<IrExpr>, Vec<IrSort>, Option<IrExpr>, Option<IrExpr>);

/// `(filter, distance/rank direction, offset, limit)` — the trailing clauses of
/// a `vector::search`/`fts::search` select, where `order by` is constrained to
/// the search score rather than an arbitrary sort list.
type SearchModifiers = (Option<IrExpr>, Option<IrSortDir>, Option<IrExpr>, Option<IrExpr>);

/// `(type name, shape elements, sub-statement, link-property alias)` — the
/// parts of an expression that names a type and optionally shapes it.
type TypeAndShape<'e> = (String, &'e [ShapeElement], Option<&'e Stmt>, Option<String>);

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
            return Ok(IrOutput {
                stmt: ir,
                params: c.params,
                ctes: vec![],
                global_ctes: c.global_ctes,
                warnings: c.warnings,
                uses_globals_arg: c.used_globals_arg,
            });
        }
        // Special case: `with search := fts::search(…); select search { … } …`
        if let Some(ir) = try_compile_fts_with_pattern(&mut c, w)? {
            return Ok(IrOutput {
                stmt: ir,
                params: c.params,
                ctes: vec![],
                global_ctes: c.global_ctes,
                warnings: c.warnings,
                uses_globals_arg: c.used_globals_arg,
            });
        }

        let mut cte_defs = vec![];
        for alias in &w.aliases {
            if let Some(declared) = declared_pointers_of(&alias.expr) {
                c.cte_declared_pointers.insert(alias.name.clone(), declared);
            }
            let ir_stmt = compile_cte_binding(&mut c, &alias.expr)?;
            let type_name = c.register_cte(&alias.name, &ir_stmt);
            cte_defs.push(IrCteDef {
                name: alias.name.clone(),
                stmt: ir_stmt,
                type_name,
            });
        }
        let main = c.compile_stmt(&w.stmt)?;
        (cte_defs, main)
    } else {
        (vec![], c.compile_stmt(stmt)?)
    };

    let mut ctes = ctes;
    ctes.extend(std::mem::take(&mut c.hoisted_ctes));
    Ok(IrOutput {
        stmt: ir,
        params: c.params,
        ctes,
        global_ctes: c.global_ctes,
        warnings: c.warnings,
        uses_globals_arg: c.used_globals_arg,
    })
}

/// Detect `with <var> := vector::search(Type, $vec); select <var> { object { … }, distance }`.
/// When matched, compile the whole thing to a single `IrVectorSearch`.
fn try_compile_vs_with_pattern(c: &mut Compiler<'_>, w: &ast::WithStmt) -> Result<Option<IrStmt>, PyQLError> {
    // Only handle exactly one alias that is a bare function call (not a subquery).
    if w.aliases.len() != 1 {
        return Ok(None);
    }
    let alias_def = &w.aliases[0];
    let fc = match &alias_def.expr {
        Expr::FunctionCall(fc) => fc,
        _ => return Ok(None),
    };
    if fc.module.as_deref() != Some("vector") || fc.name != "search" {
        return Ok(None);
    }

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
                if n != &alias_def.name {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }

    if let Some(ir) = c.try_compile_vector_search(fc, elements, select_stmt)? {
        Ok(Some(IrStmt::VectorSearch(ir)))
    } else {
        Ok(None)
    }
}

fn try_compile_fts_with_pattern(c: &mut Compiler<'_>, w: &ast::WithStmt) -> Result<Option<IrStmt>, PyQLError> {
    if w.aliases.len() != 1 {
        return Ok(None);
    }
    let alias_def = &w.aliases[0];
    let fc = match &alias_def.expr {
        Expr::FunctionCall(fc) => fc,
        _ => return Ok(None),
    };
    if fc.module.as_deref() != Some("fts") || fc.name != "search" {
        return Ok(None);
    }

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
                if n != &alias_def.name {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    }

    if let Some(ir) = c.try_compile_fts_search(fc, elements, select_stmt)? {
        Ok(Some(IrStmt::FtsSearch(ir)))
    } else {
        Ok(None)
    }
}

/// How many times a path may expand a computed pointer into its own path
/// before we call it a cycle. Chains are normal (a computed over a computed);
/// a chain this long is a schema that refers to itself.
/// AND a path traversal's own FILTER together with whatever conditions the
/// computed pointers spliced into it contributed. Every join in a path
/// select is inner, so a spliced computed's filter means the same thing as a
/// WHERE condition on the whole traversal.
fn and_conditions(filter: Option<IrExpr>, extra: Vec<IrExpr>) -> Option<IrExpr> {
    extra.into_iter().fold(filter, |acc, cond| match acc {
        Some(existing) => Some(IrExpr::BinOp(Box::new(IrBinOp {
            left: existing,
            op: ast::BinOpKind::And,
            right: cond,
        }))),
        None => Some(cond),
    })
}

const MAX_COMPUTED_SPLICES: usize = 32;

/// `infer_ir_type` as an owned name, plus the array literal it cannot report:
/// an array's type is built from its elements rather than carried on the node.
fn ir_value_type_name(expr: &IrExpr) -> String {
    if let IrExpr::Array(elements) = expr {
        let element = elements.first().and_then(infer_ir_type).unwrap_or("text");
        return format!("{}[]", literal_sentinel_to_pg(element));
    }
    infer_ir_type(expr).map(|t| t.to_string()).unwrap_or_default()
}

/// The computed pointers a `with` binding's own shape declares, if any.
fn declared_pointers_of(expr: &Expr) -> Option<Vec<ShapeElement>> {
    let Expr::SubQuery(stmt) = expr else {
        return None;
    };
    let shape = match innermost_select(stmt)? {
        ast::SelectStmt {
            result: Expr::Shape(sh),
            ..
        } => sh,
        _ => return None,
    };
    let declared: Vec<ShapeElement> = shape
        .elements
        .iter()
        .filter(|el| el.compexpr.is_some())
        .cloned()
        .collect();
    (!declared.is_empty()).then_some(declared)
}

/// The select a statement ultimately is, past any `with` blocks.
fn innermost_select(stmt: &Stmt) -> Option<&ast::SelectStmt> {
    match stmt {
        Stmt::Select(sel) => Some(sel),
        Stmt::With(w) => innermost_select(&w.stmt),
        _ => None,
    }
}

fn cte_stmt_type(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::Insert(ins) => ins.target.type_name.clone(),
        IrStmt::Update(upd) => upd.target.type_name.clone(),
        IrStmt::Delete(del) => del.target.type_name.clone(),
        IrStmt::Select(sel) => match sel.rows.first() {
            Some(IrRowSource::Bound { source, .. }) => source.type_name.clone(),
            // Infer the scalar pg_type from the first free item so the type
            // is available for UNION mismatch error messages. Returns empty
            // string if unknown.
            Some(IrRowSource::Free(IrFreeExpr::Scalar(expr))) => ir_value_type_name(expr),
            _ => String::new(),
        },
        // What a path select *yields*, not what it starts from: `with xs :=
        // (select Person.name)` binds text, not `default::Person`, and the
        // difference decides whether a reference to it reads the CTE's
        // `result` column or its `id` (see `resolve_name_ref`).
        IrStmt::PathSelect(ps) => match &ps.result {
            IrPathResult::Scalar(expr, _) => ir_value_type_name(expr),
            IrPathResult::Object { type_name, .. } => type_name.clone(),
        },
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
        lock: None,
    };
    c.compile_stmt(&Stmt::Select(fake_sel))
}

/// Qualified names of the functions that take `GLOBALS_ARG`.
///
/// A function needs it when its body reads a session global, *or* when it calls
/// a function that needs it — so this is a fixpoint, not a single pass: the
/// first pass finds the direct readers, later passes find their callers. A body
/// that fails to compile is skipped; that failure is reported by validation,
/// and guessing at its globals here would only produce a second, worse error.
pub fn functions_needing_globals(schema: &SchemaDescriptor) -> std::collections::HashSet<String> {
    let mut needs: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let mut changed = false;
        for fd in &schema.functions {
            let qualified = format!("{}::{}", fd.module, fd.name);
            if needs.contains(&qualified) {
                continue;
            }
            if let Ok(out) = compile_fn_body_with(fd, schema, &needs)
                && out.uses_globals_arg
            {
                needs.insert(qualified);
                changed = true;
            }
        }
        if !changed {
            return needs;
        }
    }
}

/// Compile the PyQL body of a user-defined function for DDL emission.
///
/// Sets `fn_params` on the compiler so that parameter names resolve as `FnParam`
/// nodes rather than raising "expression is not valid in free SELECT context".
pub fn compile_fn_body(
    fn_desc: &crate::schema::FunctionDescriptor,
    schema: &SchemaDescriptor,
) -> Result<super::IrOutput, crate::error::PyQLError> {
    compile_fn_body_with(fn_desc, schema, &functions_needing_globals(schema))
}

/// `compile_fn_body` with the globals-argument set supplied, so the fixpoint in
/// `functions_needing_globals` can call this without recursing into itself.
pub fn compile_fn_body_with(
    fn_desc: &crate::schema::FunctionDescriptor,
    schema: &SchemaDescriptor,
    fns_needing_globals: &std::collections::HashSet<String>,
) -> Result<super::IrOutput, crate::error::PyQLError> {
    use crate::parse;
    use crate::parse::ast::Stmt;

    let body = fn_desc.body.trim().to_string();
    // A body that is already a statement stands on its own; only a bare
    // expression needs the `select` that turns it into one. Wrapping a
    // statement instead would bury it as a sub-select and lose its clauses.
    let starts_a_stmt = ["select", "with", "for", "group", "insert", "update", "delete"]
        .iter()
        .any(|keyword| {
            body.len() > keyword.len()
                && body[..keyword.len()].eq_ignore_ascii_case(keyword)
                && !body.as_bytes()[keyword.len()].is_ascii_alphanumeric()
                && body.as_bytes()[keyword.len()] != b'_'
        });
    let body = if starts_a_stmt {
        body
    } else {
        format!("select {}", body)
    };

    let ast = parse::parse(&body)?;
    let mut c = Compiler::new(schema);
    c.in_fn_body = true;
    c.fns_needing_globals = Some(fns_needing_globals.clone());
    for p in &fn_desc.params {
        c.fn_params.insert(p.name.clone(), p.pg_type.clone());
    }

    let (ctes, ir) = if let Stmt::With(w) = &ast {
        let mut cte_defs = vec![];
        for alias in &w.aliases {
            let ir_stmt = compile_cte_binding(&mut c, &alias.expr)?;
            let type_name = cte_stmt_type(&ir_stmt);
            c.cte_types.insert(alias.name.clone(), type_name.clone());
            cte_defs.push(super::IrCteDef {
                name: alias.name.clone(),
                stmt: ir_stmt,
                type_name,
            });
        }
        let main = c.compile_stmt(&w.stmt)?;
        (cte_defs, main)
    } else {
        (vec![], c.compile_stmt(&ast)?)
    };

    let mut ctes = ctes;
    ctes.extend(std::mem::take(&mut c.hoisted_ctes));
    Ok(super::IrOutput {
        stmt: ir,
        params: c.params,
        ctes,
        global_ctes: c.global_ctes,
        warnings: c.warnings,
        uses_globals_arg: c.used_globals_arg,
    })
}

/// Compile a schema `Trigger`'s `handler` PyQL statement for DDL emission —
/// same shape as `compile_fn_body`, but instead of `fn_params` this binds
/// `__new__`/`__old__` as the inserted/updated/deleted row (see
/// `Compiler::special_anchors`'s own doc comment), gated by `on_mask`
/// (Pylon's `On` bitmask: 1=Insert, 2=Update, 4=Delete): `__old__` is bound whenever
/// `on_mask` does *not* include Insert (so Update-only, Delete-only, and
/// Update+Delete all get it — but never a mask that includes Insert, even
/// combined with Update, since a shared trigger function has no old row on
/// the Insert branch of that combination); `__new__` is bound whenever
/// `on_mask` does *not* include Delete, by the mirror-image argument. A
/// mask combining Insert and Delete (with no Update) legally binds
/// neither. Referencing an anchor that isn't bound is a compile error, not
/// a runtime NULL.
pub fn compile_trigger_handler(
    handler: &str,
    type_name: &str,
    on_mask: u8,
    schema: &SchemaDescriptor,
) -> Result<super::IrOutput, crate::error::PyQLError> {
    use crate::parse;
    use crate::parse::ast::Stmt;

    let ast = parse::parse(handler.trim())?;
    let mut c = Compiler::new(schema);
    // The trigger's own type — `__new__`/`__old__` always resolve against
    // this, regardless of what type happens to be the ambient `td` where
    // the anchor is textually used (e.g. inside `insert Note { note :=
    // __new__.name }`, the ambient td at that point is `Note`, not this).
    let owner_td = c.resolve_type(type_name)?;
    if on_mask & 4 == 0 {
        c.special_anchors
            .insert("__new__".to_string(), (owner_td, "NEW".to_string()));
    }
    if on_mask & 1 == 0 {
        c.special_anchors
            .insert("__old__".to_string(), (owner_td, "OLD".to_string()));
    }

    let (ctes, ir) = if let Stmt::With(w) = &ast {
        let mut cte_defs = vec![];
        for alias in &w.aliases {
            let ir_stmt = compile_cte_binding(&mut c, &alias.expr)?;
            let cte_type_name = cte_stmt_type(&ir_stmt);
            c.cte_types.insert(alias.name.clone(), cte_type_name.clone());
            cte_defs.push(super::IrCteDef {
                name: alias.name.clone(),
                stmt: ir_stmt,
                type_name: cte_type_name,
            });
        }
        let main = c.compile_stmt(&w.stmt)?;
        (cte_defs, main)
    } else {
        (vec![], c.compile_stmt(&ast)?)
    };

    let recursive_kind = recursive_dml_event(&ir, type_name)
        .or_else(|| ctes.iter().find_map(|cte| recursive_dml_event(&cte.stmt, type_name)));
    if let Some(kind) = recursive_kind
        && kind & on_mask != 0
    {
        return Err(crate::error::PyQLError::Fragment(crate::error::PyQLFragmentError {
            message: format!(
                "trigger on {type_name} is recursive: its handler {}s its own type, \
                     which this trigger also fires on",
                dml_event_word(kind),
            ),
            context: type_name.to_string(),
            position: crate::error::Position { line: 0, col: 0 },
        }));
    }

    let mut ctes = ctes;
    ctes.extend(std::mem::take(&mut c.hoisted_ctes));
    Ok(super::IrOutput {
        stmt: ir,
        params: c.params,
        ctes,
        global_ctes: c.global_ctes,
        warnings: c.warnings,
        uses_globals_arg: c.used_globals_arg,
    })
}

fn dml_event_word(kind: u8) -> &'static str {
    match kind {
        1 => "insert",
        2 => "update",
        4 => "delete",
        _ => "mutate",
    }
}

/// Walks a compiled trigger handler's IR looking for a nested `INSERT`/
/// `UPDATE`/`DELETE` targeting `owner_type` — the same type the trigger
/// itself is declared on. Returns that DML's own event bit (1/2/4) the
/// first time one is found, so the caller can check it against the
/// trigger's own `on_mask`: a handler that inserts into its own type only
/// matters if this trigger *also* fires on Insert — the mask determines
/// what would actually refire, not just "does it touch itself at all".
///
/// Deliberately not exhaustive: covers the direct statement, a `SELECT
/// (INSERT/UPDATE/DELETE …) { … }` wrapper, and `for x in … union (…)`
/// loop bodies — the shapes every trigger handler in this codebase's own
/// test suite actually uses. A DML nested inside a free tuple/set literal
/// (`select { (insert A {...}), (insert B {...}) }`) isn't walked; a
/// handler written that way relies on Postgres's own runtime recursion-
/// depth guard instead, same fallback as before this check existed.
fn recursive_dml_event(stmt: &IrStmt, owner_type: &str) -> Option<u8> {
    match stmt {
        IrStmt::Insert(ins) if ins.target.type_name == owner_type => Some(1),
        IrStmt::Update(upd) if upd.target.type_name == owner_type => Some(2),
        IrStmt::Delete(del) if del.target.type_name == owner_type => Some(4),
        IrStmt::Select(sel) => sel
            .dml_source
            .as_deref()
            .and_then(|inner| recursive_dml_event(inner, owner_type)),
        IrStmt::For(for_stmt) => recursive_dml_event(&for_stmt.body, owner_type),
        _ => None,
    }
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

/// Compile a schema-declared computed pointer the same way a shape that
/// included it would, for validation.
///
/// Returns the scalar expression when the computed is scalar-valued, and
/// `None` when it compiles to an object (link) pointer — `(select .emails
/// filter .primary limit 1)` and friends, which have no scalar type for a
/// declared return type to be checked against.
pub fn compile_computed_in_type(
    cd: &crate::schema::ComputedDescriptor,
    type_name: &str,
    schema: &SchemaDescriptor,
) -> Result<Option<IrExpr>, PyQLError> {
    let mut c = Compiler::new(schema);
    let td = c.resolve_type(type_name)?;
    let module = td.module.clone();
    let alias = c.fresh_alias();
    match c.compile_declared_computed(cd, td, &alias, &module, None, &[])? {
        IrShapePointer::Computed(p) => Ok(Some(p.expr)),
        _ => Ok(None),
    }
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
    compile_scalar_default_typed(pyql, schema).map(|(sql, _ir)| sql)
}

/// Like `compile_scalar_default`, but also returns the compiled `IrExpr` —
/// used by schema-type-consistency validation (`crate::validate`) to infer
/// the default's actual produced type via `infer_ir_type`.
/// Compile a PyQL boolean expression in a type's context into the SQL a CHECK
/// constraint needs.
///
/// Columns emit unqualified (via `compile_expr_unaliased`), which is what a
/// table-level CHECK wants — it has no alias to qualify against.
pub fn compile_constraint_expr(
    pyql: &str,
    type_name: &str,
    schema: &SchemaDescriptor,
) -> Result<String, crate::error::PyQLError> {
    use crate::parse::ast::Stmt;
    let ast = crate::parse::parse(&format!("SELECT {pyql}"))?;
    let Stmt::Select(sel) = &ast else {
        return Err(PyQLError::Type(PyQLTypeError {
            message: "constraint expression must be an expression".into(),
            position: Position { line: 0, col: 0 },
        }));
    };
    let (ir, _params) = compile_expr_unaliased(&sel.result, type_name, schema)?;
    Ok(crate::sql::emit_expr(&ir))
}

pub fn compile_scalar_default_typed(pyql: &str, schema: &SchemaDescriptor) -> Result<(String, IrExpr), String> {
    use crate::parse::ast::Stmt;
    let full = format!("SELECT {}", pyql);
    let ast = crate::parse::parse(&full).map_err(|e| e.message)?;
    let Stmt::Select(sel) = &ast else {
        return Err("default expression must be a select statement".into());
    };
    let mut c = Compiler::new(schema);
    let ir = c.compile_free_expr(&sel.result).map_err(|e| e.to_string())?;
    let sql = crate::sql::emit_expr(&ir);
    Ok((sql, ir))
}

// ── Compiler context ────────────────────────────────────────────────────────────

struct Compiler<'a> {
    schema: &'a SchemaDescriptor,
    /// Ordered parameter names — index + 1 is the $N position in SQL.
    params: Vec<String>,
    alias_counter: usize,
    /// CTE names registered in the enclosing WITH block → qualified type name.
    cte_types: HashMap<String, String>,
    /// CTE names bound to a single free row (free object/scalar/tuple, not
    /// a schema object) → that row's `IrFreeExpr` — lets `root.field`
    /// resolve to `IrExpr::CteFieldRef` when `root` is such a binding,
    /// instead of failing as an unresolvable schema-path root.
    cte_free_items: HashMap<String, IrFreeExpr>,
    /// FOR loop variables in scope: variable name → pg_type of the scalar iterator.
    for_vars: HashMap<String, String>,
    /// For-loop variables that iterate objects, by qualified type name. The
    /// variable itself holds the row's key (see `compile_for`), so a path
    /// rooted at one reads its table back by that key.
    for_var_types: HashMap<String, String>,
    /// The condition a `(insert …) if cond else {}` puts on the insert about
    /// to be compiled — see `IrInsert::guard`.
    pending_insert_guard: Option<Expr>,
    /// Pointers a `with` binding declared in its own shape (`offering := (
    /// select Offering { publisher := … })`). They exist nowhere on the type,
    /// so a later `offering { publisher }` has to find them here.
    cte_declared_pointers: HashMap<String, Vec<ShapeElement>>,
    /// Those of the binding whose shape is being compiled right now.
    active_declared_pointers: Vec<ShapeElement>,
    /// The schema-bound selects currently being compiled, innermost last.
    ///
    /// Only consulted for `detached`: an absolute `TypeName.prop` inside a
    /// detached select means the *enclosing* select's row, so the innermost
    /// entry is skipped and the next matching one used.
    anchors: Vec<SelectAnchor>,
    /// CTE definitions a nested `with` contributed from somewhere the
    /// emitter has no WITH clause of its own — an expression, say. They are
    /// appended to the statement's own CTEs at the top-level boundary.
    hoisted_ctes: Vec<IrCteDef>,
    /// `(through type, junction alias)` of the multi-link whose own
    /// modifiers are being compiled — what a bare `@prop` in `filter
    /// (@primary = true)` resolves against. `None` on the stack means a link
    /// with no through type, which has no link properties at all.
    link_prop_scope: Vec<Option<(String, String)>>,
    /// Set when `compile_stmt` strips a select-level `detached`, and taken by
    /// the `compile_path_modifiers` that compiles that select's own clauses.
    pending_detached: bool,
    /// Set while a path continues past a sub-select's own subject, e.g. the
    /// `.plan.tier` of `(select .licences filter not exists .ended_at limit
    /// 1).plan.tier`: the subject's landing row is what FILTER/ORDER BY/LIMIT
    /// scope to, not the type the whole path ends on.
    modifier_anchor: Option<(String, String)>,
    /// WITH bindings that name a path off the enclosing object. They read that
    /// object's alias, which a CTE emitted ahead of the FROM clause cannot
    /// see, so they stand in for their value wherever the name is used.
    inline_bindings: std::collections::HashMap<String, IrExpr>,
    /// True while compiling a `@pylon.function` body. A session global cannot
    /// be a query parameter there — a `CREATE FUNCTION` body has nothing to
    /// bind one to — so it is read out of the `__pylon_json_globals__`
    /// argument instead. See `GLOBALS_ARG`.
    in_fn_body: bool,
    /// Qualified names of functions that take the globals argument, so a call
    /// to one can be given it.
    ///
    /// Computed on first use rather than up front: working it out means
    /// compiling every function body in the schema, and the overwhelming
    /// majority of queries never call a user function at all. Function-body
    /// compilation seeds it explicitly (`compile_fn_body_with`), which is also
    /// what keeps the fixpoint from recursing into itself.
    fns_needing_globals: Option<std::collections::HashSet<String>>,
    /// Set when this body read a global or forwarded the argument to a callee
    /// — i.e. when the function being compiled needs the argument itself.
    used_globals_arg: bool,
    /// User-defined function parameters in scope (only set during body compilation).
    fn_params: HashMap<String, String>,
    /// `__new__`/`__old__` row-context bindings, only set during trigger-handler
    /// compilation (`compile_trigger_handler`) — maps the anchor name to
    /// *the trigger's own type* and the table alias its properties/links
    /// should resolve against (`"NEW"`/`"OLD"`). Stored explicitly (not just
    /// the alias string) because `__new__.x`/`__old__.x` can appear nested
    /// inside a sub-statement targeting a *different* type (e.g. `insert Note
    /// { note := __new__.name }` — the ambient `td` at that point is `Note`,
    /// not the trigger's own type), so resolution can't rely on whatever
    /// `td` happens to be in scope where the anchor is used. An anchor not
    /// legal for the trigger's `on` mask (e.g. `__old__` in an Insert-only
    /// trigger) is simply absent from this map, and fails resolution the
    /// same way any other unknown identifier does.
    special_anchors: HashMap<String, (&'a TypeDescriptor, String)>,
    /// Global CTEs collected during compilation (session and computed), in dependency order.
    global_ctes: Vec<IrGlobalCte>,
    /// Nested DML hoisted out of a link-assignment value currently being
    /// compiled (`author := (select (insert Person {...}) { id })`) — see
    /// `compile_link_subquery`'s doc comment. `compile_insert`/
    /// `compile_update` save-and-clear this on entry and drain it back into
    /// their own `IrInsert`/`IrUpdate::nested_ctes` on exit, so nesting
    /// (a nested insert whose own link value nests another insert) attaches
    /// each level's discoveries to the right statement.
    pending_nested_ctes: Vec<IrCteDef>,
    nested_cte_counter: usize,
    /// Non-fatal warnings collected during compilation.
    warnings: Vec<String>,
    /// Depth of `any()`/`all()` arguments currently being compiled. Wrapping a
    /// set-valued comparison in one of those *is* the explicit intent the
    /// multi-link FILTER warning asks for, so the warning stays quiet inside.
    explicit_set_depth: usize,
    /// User-configurable session options — see `SessionConfig`. Always
    /// `default()` for every entry point except `compile_with_config`.
    config: crate::ir::SessionConfig,
}

/// One schema-bound select in the enclosing chain — see `Compiler::anchors`.
struct SelectAnchor {
    type_name: String,
    qualified: String,
    alias: String,
    detached: bool,
}

/// The synthetic argument carrying session globals into a function body.
///
/// A session global is normally a query parameter, which a `CREATE FUNCTION`
/// body cannot have — it would emit a bare `$1` nothing binds. Following the upstream engine's
/// `__edb_json_globals__`, the caller packs the globals into one jsonb value
/// and passes it as a leading argument. One opaque argument rather than one
/// per global keeps the function's signature independent of which globals its
/// body happens to mention, so editing a body does not churn its signature
/// (and, with PostgreSQL overloading, leave a stale one behind).
pub const GLOBALS_ARG: &str = "__pylon_json_globals__";

/// SQL reading one global out of `GLOBALS_ARG`.
fn globals_arg_read(qualified: &str, pg_type: &str) -> String {
    let key = qualified.replace('\'', "''");
    if let Some(element) = pg_type.strip_suffix("[]") {
        // `->>` would hand back the JSON array's text, not an array, so the
        // elements are unpacked instead. The `jsonb_typeof` guard matters: an
        // unset global arrives as JSON null, and unpacking that raises "cannot
        // extract elements from a scalar". The `coalesce` keeps an empty array
        // an empty array rather than letting `array_agg` turn it into NULL.
        format!(
            "(case when jsonb_typeof({GLOBALS_ARG} -> '{key}') = 'array' \
             then coalesce((select array_agg(value::{element}) \
             from jsonb_array_elements_text({GLOBALS_ARG} -> '{key}') as value), '{{}}'::{pg_type}) \
             else null end)"
        )
    } else {
        format!("(({GLOBALS_ARG} ->> '{key}')::{pg_type})")
    }
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
            cte_free_items: HashMap::new(),
            for_vars: HashMap::new(),
            for_var_types: HashMap::new(),
            pending_insert_guard: None,
            cte_declared_pointers: HashMap::new(),
            active_declared_pointers: vec![],
            fn_params: HashMap::new(),
            special_anchors: HashMap::new(),
            global_ctes: vec![],
            pending_nested_ctes: vec![],
            nested_cte_counter: 0,
            warnings: vec![],
            explicit_set_depth: 0,
            config,
            anchors: Vec::new(),
            link_prop_scope: Vec::new(),
            hoisted_ctes: Vec::new(),
            pending_detached: false,
            modifier_anchor: None,
            inline_bindings: std::collections::HashMap::new(),
            in_fn_body: false,
            fns_needing_globals: None,
            used_globals_arg: false,
        }
    }

    /// Register a compiled WITH binding under `name`: records its type (or
    /// empty string for a free binding) in `cte_types`, and — when it's a
    /// single free row — its `IrFreeExpr` in `cte_free_items` so a later
    /// `name.field` reference can resolve to `IrExpr::CteFieldRef`.
    /// True when `name` is bound to a value rather than to an object set —
    /// a scalar WITH binding, or one inlined by `bind_inline_if_correlated`.
    fn is_value_binding(&self, name: &str) -> bool {
        self.inline_bindings.contains_key(name) || self.cte_types.get(name).is_some_and(|t| !t.contains("::"))
    }

    /// Bind `name` to the value of a relative path off the enclosing object —
    /// `with handle_id := .id`. Returns false when the binding is anything
    /// else, which the caller hoists into a CTE as usual.
    fn bind_inline_if_correlated(&mut self, name: &str, expr: &Expr) -> Result<bool, PyQLError> {
        if self.anchors.is_empty() || !matches!(expr, Expr::Path(p) if p.partial) {
            return Ok(false);
        }
        let ir = self.compile_free_expr(expr)?;
        self.inline_bindings.insert(name.to_string(), ir);
        Ok(true)
    }

    fn register_cte(&mut self, name: &str, ir_stmt: &IrStmt) -> String {
        let type_name = cte_stmt_type(ir_stmt);
        self.cte_types.insert(name.to_string(), type_name.clone());
        if let IrStmt::Select(sel) = ir_stmt
            && let [IrRowSource::Free(item)] = sel.rows.as_slice()
        {
            self.cte_free_items.insert(name.to_string(), item.clone());
        }
        type_name
    }

    /// Resolve `root.field1.field2. ... fieldN` where `root` is a WITH-bound
    /// free object (e.g. `with x := { a := { b := 1 } } select x.a.b`) —
    /// `None` when `root` isn't such a binding, so callers fall back to
    /// their normal path resolution. The first step reads the CTE's own
    /// per-field column (`IrExpr::CteFieldRef`, materialized once); any
    /// further steps index into that value as jsonb (`IrExpr::JsonbField`),
    /// since a nested free-object *field* is jsonb the moment it's not the
    /// top-level row itself — validated statically wherever the nesting is
    /// itself a free-object literal (so a typo like `x.a.typo` still gets a
    /// compile error instead of silently returning SQL NULL).
    fn resolve_cte_field_chain(&self, root: &str, steps: &[&str]) -> Option<Result<IrExpr, PyQLError>> {
        let (first, rest) = steps.split_first()?;
        let fields = match self.cte_free_items.get(root)? {
            IrFreeExpr::FreeObject(fields) => fields,
            _ => return None,
        };
        let mut current: &IrExpr = match fields.iter().find(|(n, _)| n == first) {
            Some((_, e)) => e,
            None => {
                return Some(Err(
                    self.type_err(&format!("free object '{root}' has no field '{first}'"))
                ));
            }
        };
        let mut expr = IrExpr::CteFieldRef {
            name: root.to_string(),
            field: first.to_string(),
        };
        for step in rest {
            if let IrExpr::NamedTuple {
                fields: nested,
                is_free_object: true,
            } = current
            {
                match nested.iter().find(|(n, _)| n == step) {
                    Some((_, next)) => current = next,
                    None => {
                        return Some(Err(
                            self.type_err(&format!("{step} is not a member of the nested free object"))
                        ));
                    }
                }
            }
            expr = IrExpr::JsonbField {
                expr: Box::new(expr),
                field: step.to_string(),
            };
        }
        Some(Ok(expr))
    }

    /// `resolve_cte_field_chain`, but taking the whole `root.f1.f2...` path
    /// directly — `None` when the path isn't an absolute multi-step name
    /// chain (so, in particular, whenever a step is anything other than a
    /// plain name, e.g. a type intersection or backlink).
    fn resolve_cte_path(&self, p: &ast::Path) -> Option<Result<IrExpr, PyQLError>> {
        if p.partial || p.steps.len() < 2 {
            return None;
        }
        let ast::PathStep::Name(root) = &p.steps[0] else {
            return None;
        };
        let mut steps = Vec::with_capacity(p.steps.len() - 1);
        for step in &p.steps[1..] {
            match step {
                ast::PathStep::Name(n) => steps.push(n.as_str()),
                _ => return None,
            }
        }
        self.resolve_cte_field_chain(root, &steps)
    }

    /// Return the CTE name if `expr` is a bare identifier that matches a registered CTE.
    fn resolve_cte_name<'e>(&self, expr: &'e Expr) -> Option<&'e str> {
        if let Expr::Path(p) = expr
            && !p.partial
            && p.steps.len() == 1
            && let ast::PathStep::Name(n) = &p.steps[0]
            && self.cte_types.contains_key(n.as_str())
        {
            return Some(n.as_str());
        }
        None
    }

    fn fresh_alias(&mut self) -> String {
        let a = format!("t{}", self.alias_counter);
        self.alias_counter += 1;
        a
    }

    /// A distinct naming scheme from `fresh_alias`'s `t0`, `t1`, ... (table
    /// aliases) so a hoisted nested-DML CTE name can never collide with one
    /// — see `pending_nested_ctes`.
    fn fresh_nested_cte_name(&mut self) -> String {
        let n = self.nested_cte_counter;
        self.nested_cte_counter += 1;
        format!("_nested_dml_{n}")
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

    /// `scalar_type` is a PyQL-style type-name string built by the Python
    /// walker's `_pyql_type_name` (e.g. `"std::str"`, `"std::uuid"`,
    /// `"cal::local_date"`, `"default::Gender"`, `"array<std::str>"`) — never
    /// a bare Python class name. Mirrors every shape `_pyql_type_name` can
    /// produce for a `Global[T]` annotation.
    fn resolve_global_pg_type(&self, scalar_type: &str) -> String {
        let builtin = match scalar_type {
            "std::str" => Some("text"),
            "std::int16" => Some("int2"),
            "std::int32" => Some("int4"),
            "std::int64" => Some("int8"),
            "std::float32" => Some("float4"),
            "std::float64" => Some("float8"),
            "std::decimal" => Some("numeric"),
            "std::bool" => Some("boolean"),
            "std::datetime" => Some("timestamptz"),
            "cal::local_datetime" => Some("timestamp"),
            "cal::local_date" => Some("date"),
            "cal::local_time" => Some("time"),
            "std::uuid" => Some("uuid"),
            "std::bytes" => Some("bytea"),
            "std::json" => Some("jsonb"),
            "std::duration" => Some("interval"),
            _ => None,
        };
        if let Some(t) = builtin {
            return t.to_string();
        }
        if let Some(inner) = scalar_type.strip_prefix("array<").and_then(|s| s.strip_suffix('>')) {
            return format!("{}[]", self.resolve_global_pg_type(inner));
        }
        if scalar_type.starts_with("tuple<") {
            return "jsonb".to_string();
        }
        if let Some(ed) = self.resolve_enum(scalar_type) {
            return format!("\"{}\".\"{}\"", ed.module, ed.name);
        }
        if self.resolve_named_tuple(scalar_type).is_some() {
            return "jsonb".to_string();
        }
        // Fall back to a registered custom scalar lookup.
        self.schema
            .scalars
            .iter()
            .find(|s| s.name == scalar_type || format!("{}::{}", s.module, s.name) == scalar_type)
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

        let global = self
            .schema
            .globals
            .iter()
            .find(|g| g.name == global_name || format!("{}::{}", g.module, g.name) == global_name);
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
                marker_offset: None,
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
            lock: outer.lock.clone().or(inner_sel.lock.clone()),
        };

        let ir = self.compile_stmt(&Stmt::Select(merged))?;
        Ok(Some(ir))
    }

    /// Reading one field off a free object. the upstream engine compiles a free shape into a
    /// real object type, so `{ device := d { id }, … }.device` is an ordinary
    /// path step through a pointer and yields whatever that pointer holds —
    /// an object stays an object. Extracting it out of the jsonb the free
    /// object would otherwise build flattens it back to raw JSON. Any field
    /// left unread still runs: a mutation among them is a data-modifying CTE,
    /// which Postgres executes whether or not the outer query reads it.
    fn project_free_object_field(expr: IrExpr, field: &str) -> IrExpr {
        if let IrExpr::NamedTuple {
            fields,
            is_free_object: true,
        } = &expr
            && let Some((_, value)) = fields.iter().find(|(name, _)| name == field)
        {
            return value.clone();
        }
        IrExpr::JsonbField {
            expr: Box::new(expr),
            field: field.to_string(),
        }
    }

    /// The operands of a union written entirely of relative paths, which are
    /// correlated to the enclosing row and so cannot be hoisted.
    fn union_of_relative_paths(expr: &Expr) -> Option<Vec<ast::Path>> {
        fn walk(expr: &Expr, out: &mut Vec<ast::Path>) -> bool {
            match expr {
                Expr::Union(a, b) => walk(a, out) && walk(b, out),
                Expr::Path(p) if p.partial => {
                    out.push(p.clone());
                    true
                }
                _ => false,
            }
        }
        if !matches!(expr, Expr::Union(_, _)) {
            return None;
        }
        let mut out = vec![];
        walk(expr, &mut out).then_some(out)
    }

    /// `(select Licence filter …) { id }` — a shape written after a
    /// parenthesised sub-select rather than inside it.
    ///
    /// The two mean the same thing, so the shape is pushed onto the inner
    /// statement's own result and the whole thing compiled as the select it
    /// already was. A `with` block is carried through to its inner statement.
    fn shape_over_subquery(&mut self, sh: &ast::ShapeExpr) -> Result<Option<IrExpr>, PyQLError> {
        fn push_shape(stmt: &Stmt, elements: &[ShapeElement]) -> Option<Stmt> {
            match stmt {
                Stmt::Select(sel) => {
                    if matches!(sel.result, Expr::Shape(_)) {
                        return None;
                    }
                    let mut shaped = sel.clone();
                    shaped.result = Expr::Shape(Box::new(ast::ShapeExpr {
                        expr: Some(sel.result.clone()),
                        elements: elements.to_vec(),
                        marker_offset: None,
                    }));
                    Some(Stmt::Select(shaped))
                }
                Stmt::With(w) => {
                    let inner = push_shape(&w.stmt, elements)?;
                    let mut carried = w.clone();
                    carried.stmt = Box::new(inner);
                    Some(Stmt::With(carried))
                }
                _ => None,
            }
        }
        let Some(Expr::SubQuery(stmt)) = sh.expr.as_ref() else {
            return Ok(None);
        };
        let Some(shaped) = push_shape(stmt.as_ref(), &sh.elements) else {
            return Ok(None);
        };
        let single = matches!(
            innermost_select(&shaped).and_then(|s| s.limit.as_ref()),
            Some(Expr::Literal(ast::Literal::Int(1)))
        );
        let IrStmt::Select(select) = self.compile_stmt(&shaped)? else {
            return Ok(None);
        };
        if !matches!(select.rows.as_slice(), [IrRowSource::Bound { .. }]) {
            return Ok(None);
        }
        Ok(Some(if single {
            IrExpr::ObjectSubquery(Box::new(select))
        } else {
            IrExpr::ArrayFromSelect(Box::new(IrArraySource::ObjectSelect(Box::new(select))))
        }))
    }

    /// `(select … limit 1).account { id, name }` — a shape written on what a
    /// projection off a sub-select lands on. Returns the sub-statement and
    /// the field chain, so the shape can ride along with the splice instead
    /// of being rejected as a shape in expression position.
    fn shape_over_subquery_projection(sh: &ast::ShapeExpr) -> Option<(&Stmt, Vec<String>)> {
        let inner = sh.expr.as_ref()?;
        if !matches!(inner, Expr::FieldAccess { .. }) {
            return None;
        }
        let (base, fields) = Self::peel_field_access_chain(inner);
        match base {
            Expr::SubQuery(stmt) => Some((stmt.as_ref(), fields)),
            _ => None,
        }
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
                let global = self
                    .schema
                    .globals
                    .iter()
                    .find(|g| g.name == *name || format!("{}::{}", g.module, g.name) == *name)?;
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
        let field_count = fields.len();
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
            lock: outer.lock.clone().or(inner_sel.lock.clone()),
        };

        // The inner select's own filter and ordering speak about its subject,
        // not about what the field chain projects off it: `(select Individual
        // filter .id = $a).credentials.password` filters Individuals, not
        // Credentials. `tail` is what pins them there. Only safe when the
        // outer select contributed none of its own — the merge folds both
        // into one clause, and an outer filter does belong at the end.
        if outer.filter.is_none() && outer.order_by.is_empty() {
            let Expr::Path(merged_path) = &merged.result else {
                unreachable!("built as a path just above")
            };
            let ps = self.compile_path_select_with_tail(&merged, merged_path, &[], false, field_count)?;
            return Ok(Some(IrStmt::PathSelect(ps)));
        }

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
        let alias = self
            .schema
            .aliases
            .iter()
            .find(|a| a.name == path_name || format!("{}::{}", a.module, a.name) == path_name);
        let alias = match alias {
            Some(a) => a.clone(),
            None => return Ok(None),
        };

        let inner_ast = crate::parse::parse(&alias.expr)?;
        let inner_sel = match inner_ast {
            Stmt::Select(sel) => sel,
            _ => return Err(self.type_err(&format!("alias '{}' expression must be a select statement", alias.name))),
        };

        // Merge outer shape / filter / modifiers over the alias's select.
        // `inner_sel.result` is itself an `Expr::Shape` whenever the
        // alias's own body declares a shape (e.g. `select Type { field }
        // order by ... limit ...`, a legitimate, documented pattern — an
        // alias's own body may combine a shape with modifiers) — wrapping
        // it wholesale as the outer shape's own `expr` would nest a Shape
        // inside a Shape, which the compiler's SELECT-subject resolution
        // rejects ("expected a type name as SELECT subject", confirmed
        // live). The outer shape must bind to the same underlying *type*
        // reference the alias's own shape does, not to the alias's shape
        // node itself — so unwrap through it first.
        let inner_base = match &inner_sel.result {
            Expr::Shape(sh) => sh.expr.clone(),
            other => Some(other.clone()),
        };
        let merged_result = if shape_elements.is_empty() {
            inner_sel.result.clone()
        } else {
            Expr::Shape(Box::new(ast::ShapeExpr {
                expr: inner_base,
                elements: shape_elements.to_vec(),
                marker_offset: None,
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
            lock: outer.lock.clone().or(inner_sel.lock.clone()),
        };

        let _ = distinct; // alias selects honour the outer distinct if applied
        let ir = self.compile_stmt(&Stmt::Select(merged))?;
        Ok(Some(ir))
    }

    fn compile_global(&mut self, raw_name: &str) -> Result<IrExpr, PyQLError> {
        let global = self
            .schema
            .globals
            .iter()
            .find(|g| g.name == raw_name || format!("{}::{}", g.module, g.name) == raw_name);
        let global = global
            .ok_or_else(|| {
                PyQLError::Resolution(PyQLResolutionError::UnknownField(PyQLUnknownFieldError {
                    message: format!("unknown global: {:?}", raw_name),
                    position: Position { line: 0, col: 0 },
                }))
            })?
            .clone();

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
            self.global_ctes
                .push(IrGlobalCte::Computed(Box::new(IrComputedGlobalCte {
                    cte_name: cte_name.clone(),
                    qualified_name: qualified,
                    stmt: inner_stmt,
                })));
            Ok(IrExpr::GlobalRef { cte_name })
        } else if self.in_fn_body {
            // Inside a function body there is no parameter to bind, so the
            // value is read out of the `__pylon_json_globals__` argument the
            // caller packs. Mirrors the upstream engine's `__edb_json_globals__`.
            let pg_type = self.resolve_global_pg_type(&global.scalar_type);
            self.used_globals_arg = true;
            // Wrapped in a cast rather than returned bare: `RawSql` carries no
            // type, so overload resolution fell back to text and picked the
            // `str` `find` for an `array<str>` global (`strpos(text[], text)
            // does not exist`).
            Ok(IrExpr::TypeCast(Box::new(super::IrTypeCast {
                expr: IrExpr::RawSql(globals_arg_read(&qualified, &pg_type)),
                pg_type,
                tuple_shape: None,
            })))
        } else {
            // Session global — allocate parameter slot
            let pg_type = self.resolve_global_pg_type(&global.scalar_type);
            let param_name = format!("__global__{}", qualified);
            let index = self.param_index(&param_name);
            // Register CTE only once
            if !self.global_ctes.iter().any(|g| g.cte_name() == cte_name) {
                self.global_ctes.push(IrGlobalCte::Session(IrSessionGlobalCte {
                    cte_name,
                    qualified_name: qualified,
                    param_index: index,
                    pg_type: pg_type.clone(),
                }));
            }
            Ok(IrExpr::GlobalParam { index, pg_type })
        }
    }

    // ── Schema lookups ────────────────────────────────────────────────────────────

    /// The object type a WITH binding names, if it binds one — object type
    /// strings are qualified, free/scalar bindings' are not.
    fn cte_object_type(&self, name: &str) -> Option<String> {
        self.cte_types.get(name).filter(|t| t.contains("::")).cloned()
    }

    fn resolve_type(&self, name: &str) -> Result<&'a TypeDescriptor, PyQLError> {
        // Accept both "TypeName" and "module::TypeName"
        self.schema
            .types
            .iter()
            .find(|t| t.name == name || format!("{}::{}", t.module, t.name) == name)
            .ok_or_else(|| {
                PyQLError::Resolution(PyQLResolutionError::UnknownType(PyQLUnknownTypeError {
                    message: format!("unknown type '{name}'"),
                    position: Position { line: 0, col: 0 },
                }))
            })
    }

    fn resolve_enum(&self, name: &str) -> Option<&'a crate::schema::EnumDescriptor> {
        self.schema
            .enums
            .iter()
            .find(|e| e.name == name || format!("{}::{}", e.module, e.name) == name)
    }

    /// Resolve a `Channel` by name (bare or `module::Name`) — used by
    /// `notify()` to find the declared payload shape its second argument
    /// must match.
    fn resolve_channel(&self, name: &str) -> Option<&'a crate::schema::ChannelDescriptor> {
        self.schema.find_channel(name)
    }

    /// Resolve a registered custom scalar (`pylon.scalar(..., name=...)` or
    /// the `@pylon.scalar` decorator form) by name — used to recognize a
    /// cast target as that scalar's own PostgreSQL DOMAIN (see
    /// `resolve_cast_pg_type`); an anonymous (unregistered) scalar has no
    /// name reachable here at all, so it's never a valid cast target.
    fn resolve_scalar(&self, name: &str) -> Option<&'a crate::schema::ScalarDescriptor> {
        self.schema
            .scalars
            .iter()
            .find(|s| s.name == name || format!("{}::{}", s.module, s.name) == name)
    }

    /// Resolve a registered (nominal) `@pylon.named_tuple` type by name — used only
    /// to recognize a cast target as a named tuple (member structure isn't
    /// validated here; the value is trusted the same way a plain `<json>` cast is).
    fn resolve_named_tuple(&self, name: &str) -> Option<&'a crate::schema::NamedTupleDescriptor> {
        self.schema
            .named_tuples
            .iter()
            .find(|nt| nt.name == name || format!("{}::{}", nt.module, nt.name) == name)
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
                    .map(|nt| {
                        nt.members
                            .iter()
                            .map(|mm| self.tuple_member_to_json_member(mm))
                            .collect()
                    })
                    .unwrap_or_default();
                crate::query::JsonMemberKind::Tuple {
                    type_name: Some(qname),
                    members: nested_members,
                }
            }
            TupleMemberKind::Tuple { members } => crate::query::JsonMemberKind::Tuple {
                type_name: None,
                members: members.iter().map(|mm| self.tuple_member_to_json_member(mm)).collect(),
            },
        };
        crate::query::JsonMember {
            key: m.name.clone(),
            kind,
        }
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
                members: elements
                    .iter()
                    .map(|e| self.ast_tuple_element_to_json_member(e))
                    .collect(),
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
                members: elements
                    .iter()
                    .map(|e| self.ast_tuple_element_to_json_member(e))
                    .collect(),
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
    /// compile-time tuple-index bounds check.
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
    /// own cast by position (see `try_compile_tuple_literal_cast_ctx`)
    /// instead of jsonb-wrapping the raw uncast literal values.
    fn scalar_cast_free_select(
        &mut self,
        tc: &ast::TypeCast,
        pg_type: String,
        distinct: bool,
    ) -> Result<IrStmt, PyQLError> {
        let cast_expr = match &tc.ty {
            ast::TypeExpr::Tuple { elements } => {
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                match self.try_compile_tuple_literal_cast_ctx(elements, &tc.expr, None)? {
                    // The literal-decompose path already applies each element's own
                    // cast — still wrap in TypeCast so `tuple_shape` reaches SQL
                    // emission for decode-time ShapeNode building (jsonb_build_*
                    // already produces jsonb, so the outer `::jsonb` is a no-op).
                    Some(ir) => IrExpr::TypeCast(Box::new(IrTypeCast {
                        expr: ir,
                        pg_type,
                        tuple_shape,
                    })),
                    None => {
                        let inner = self.compile_expr_ctx(&tc.expr, None)?;
                        IrExpr::TypeCast(Box::new(IrTypeCast {
                            expr: inner,
                            pg_type,
                            tuple_shape,
                        }))
                    }
                }
            }
            _ => {
                let inner = self.compile_expr_ctx(&tc.expr, None)?;
                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                IrExpr::TypeCast(Box::new(IrTypeCast {
                    expr: inner,
                    pg_type,
                    tuple_shape,
                }))
            }
        };
        Ok(IrStmt::Select(IrSelect {
            rows: vec![IrRowSource::Free(IrFreeExpr::Scalar(cast_expr))],
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            distinct,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        }))
    }

    /// Resolve a cast's target `pg_type` string — shared by both `Expr::TypeCast`
    /// compile sites (`compile_free_expr`/`compile_expr`). A structural tuple
    /// always resolves to jsonb; an array resolves to its element's own pg_type
    /// with a `[]` suffix — a real Postgres array, not jsonb, so it decodes
    /// natively (the driver already returns a Python list) with no per-member
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
        // A registered custom scalar casts directly to its own DOMAIN (not
        // just its base type) — Postgres enforces the domain's CHECK right
        // at cast time, the same way it would on column assignment (see
        // `PropertyDescriptor.column_type`'s own doc comment for why this
        // is safe to do everywhere else too).
        if let Some(sd) = self.resolve_scalar(&qname) {
            return Ok(format!("{}.\"{}\"", crate::sql::pg_schema_str(&sd.module), sd.name));
        }
        type_expr_to_pg(ty)
    }

    /// Cast one tuple-type element's source value to `target_ty` — recurses
    /// via `try_compile_tuple_literal_cast_ctx` when both the element's own
    /// type and its source value are themselves a nested tuple/named-tuple
    /// literal, so nesting applies per-element casts all the way down;
    /// otherwise a plain scalar/enum/nominal-named-tuple cast.
    fn compile_tuple_element_cast_ctx(
        &mut self,
        target_ty: &ast::TypeExpr,
        value: &Expr,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<IrExpr, PyQLError> {
        if let ast::TypeExpr::Tuple { elements } = target_ty
            && let Some(ir) = self.try_compile_tuple_literal_cast_ctx(elements, value, ctx)?
        {
            return Ok(ir);
        }
        let inner = self.compile_expr_ctx(value, ctx)?;
        let pg_type = self.resolve_cast_pg_type(target_ty)?;
        Ok(IrExpr::TypeCast(Box::new(IrTypeCast {
            expr: inner,
            pg_type,
            tuple_shape: None,
        })))
    }

    /// When casting a tuple/named-tuple *literal* to a structural tuple type,
    /// apply each target element's own cast to its corresponding source value
    /// by position — e.g. `<tuple<int64, str>>('1', 3)` must coerce '1' to
    /// int64 and 3 to str, not just jsonb-wrap the raw literal values
    /// unchanged. Returns `None` when the source isn't a literal tuple/named-
    /// tuple of matching arity (e.g. a `$param`) — the whole value already
    /// arrives pre-shaped in that case, so the caller's generic jsonb-cast
    /// path handles it instead.
    fn try_compile_tuple_literal_cast_ctx(
        &mut self,
        target_elements: &[ast::TupleTypeElement],
        source: &Expr,
        ctx: Option<(&TypeDescriptor, &str)>,
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
            casted.push(self.compile_tuple_element_cast_ctx(&elem.ty, value, ctx)?);
        }
        if named {
            let fields = target_elements
                .iter()
                .zip(casted)
                .map(|(e, v)| (e.name.clone().unwrap(), v))
                .collect();
            Ok(Some(IrExpr::NamedTuple {
                fields,
                is_free_object: false,
            }))
        } else {
            Ok(Some(IrExpr::Tuple(casted)))
        }
    }

    /// When casting an array *literal* to `array<T>`, apply the element
    /// type's own cast to each element by position — e.g.
    /// `<array<int64>>['1', '3']` must coerce each string element to int64,
    /// not just emit a raw untyped `ARRAY[...]`. Reuses
    /// `compile_tuple_element_cast_ctx` for the per-element cast since
    /// casting "this value to this target type" is exactly the same
    /// operation regardless of whether the target is a tuple element or an
    /// array element (including decomposing a nested tuple-literal element).
    /// Returns `None` when the source isn't a literal array (e.g. a
    /// `$param` or a sub-select) — the caller's generic cast path handles
    /// those instead.
    fn try_compile_array_literal_cast_ctx(
        &mut self,
        element_ty: &ast::TypeExpr,
        source: &Expr,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let Expr::Array(elems) = source else { return Ok(None) };
        let casted = elems
            .iter()
            .map(|e| self.compile_tuple_element_cast_ctx(element_ty, e, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(IrExpr::Array(casted)))
    }

    fn compile_enum_access(&self, type_ref: &str, variant: &str) -> Result<IrExpr, PyQLError> {
        let ed = self
            .resolve_enum(type_ref)
            .ok_or_else(|| self.type_err(&format!("unknown type '{}'", type_ref)))?;
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

    /// The type a path's root names — a real type, or the object type a
    /// `with` binding stands for.
    ///
    /// A binding is a row source just like a type name, so anything that
    /// re-resolves a root after `compile_path_select` has already accepted it
    /// has to look it up the same way, or a bound alias reads as an unknown
    /// type.
    fn resolve_path_root(&self, root_name: &str) -> Result<&'a TypeDescriptor, PyQLError> {
        if let Some(qualified) = self.for_var_types.get(root_name) {
            return self.resolve_type(qualified);
        }
        match self.cte_types.get(root_name).filter(|t| t.contains("::")).cloned() {
            Some(bound) => self.resolve_type(&bound),
            None => self.resolve_type(root_name),
        }
    }

    fn find_poly_implementors(&self, iface_qname: &str) -> Vec<IrPolyImplementor> {
        self.schema
            .types
            .iter()
            .filter(|t| !t.abstract_ && t.interfaces.iter().any(|i| i == iface_qname))
            .map(|t| IrPolyImplementor {
                type_name: format!("{}::{}", t.module, t.name),
                table: t.table.clone(),
                module: t.module.clone(),
            })
            .collect()
    }

    fn resolve_property<'t>(td: &'t TypeDescriptor, name: &str) -> Option<&'t PropertyDescriptor> {
        td.properties.iter().find(|p| p.name == name)
    }

    fn resolve_link<'t>(td: &'t TypeDescriptor, name: &str) -> Option<&'t LinkDescriptor> {
        td.links.iter().find(|l| l.name == name)
    }

    fn resolve_multilink<'t>(td: &'t TypeDescriptor, name: &str) -> Option<&'t MultiLinkDescriptor> {
        td.multilinks.iter().find(|m| m.name == name)
    }

    /// `@prop` read off the junction row of the multi-link currently in
    /// scope (`link_prop_scope`). Only its own modifiers put one there —
    /// anywhere else there is no junction row to read.
    fn compile_link_prop_ref(&mut self, prop_name: &str) -> Result<IrExpr, PyQLError> {
        let Some(scope) = self.link_prop_scope.last().cloned() else {
            return Err(self.type_err(&format!(
                "'@{prop_name}' is a link property, so it is only valid in the modifiers or shape of the \
                 link it belongs to, e.g. 'locators: {{ … }} filter @{prop_name}'"
            )));
        };
        let Some((through_qname, junction_alias)) = scope else {
            return Err(self.type_err(&format!(
                "this link has no link properties, so '@{prop_name}' cannot be read — \
                 declare the link with a `Through[...]` type to give it some"
            )));
        };
        let through_td = self.resolve_type(&through_qname)?;
        let Some(prop) = Self::resolve_property(through_td, prop_name) else {
            return Err(self.field_err(prop_name, &through_qname));
        };
        Ok(IrExpr::ColumnRef {
            alias: junction_alias,
            column: prop.name.clone(),
            pg_type: prop.pg_type.clone(),
        })
    }

    /// Whether a link declared with `target` reaches the type named
    /// `current_qname` — directly, or because that type implements the
    /// interface the link points at. A link to an interface accepts every
    /// implementor, so a backlink from one is exactly as valid as a backlink
    /// from the interface itself.
    fn link_target_reaches(&self, target: &str, current_qname: &str) -> bool {
        if target == current_qname {
            return true;
        }
        self.resolve_type(current_qname)
            .is_ok_and(|td| td.interfaces.iter().any(|i| i == target))
    }

    /// A computed pointer visible on `td` — its own, or one declared on an
    /// interface it implements. An interface's computeds are not copied into
    /// its implementors the way its stored pointers are, so without the
    /// second lookup `.account.tier` reported that `tier` was missing while
    /// suggesting `tier` (the suggester already searched interfaces).
    fn resolve_computed(&self, td: &TypeDescriptor, name: &str) -> Option<crate::schema::ComputedDescriptor> {
        if let Some(cd) = td.computed.iter().find(|c| c.name == name) {
            return Some(cd.clone());
        }
        td.interfaces
            .iter()
            .filter_map(|iface| self.resolve_type(iface).ok())
            .find_map(|itd| itd.computed.iter().find(|c| c.name == name))
            .cloned()
    }

    // ── Statement dispatch ────────────────────────────────────────────────────────

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<IrStmt, PyQLError> {
        match stmt {
            Stmt::Select(s) => {
                let (distinct, result) = match &s.result {
                    Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct => (true, &u.operand),
                    // `detached` marks this select as independent of the one
                    // enclosing it. At the top level that is already true, but
                    // for a select nested in a filter it is the whole point:
                    // it is what lets `select X filter not exists (select
                    // detached X filter .k = X.k)` mean "no *other* X", rather
                    // than comparing the inner row with itself. Stripping the
                    // wrapper outright made that anti-join a tautology
                    // (`"t1"."label" = "t1"."label"`) with no error.
                    Expr::Detached(inner) => {
                        self.pending_detached = true;
                        (false, inner.as_ref())
                    }
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
                if let Expr::Shape(sh) = result
                    && let Some(Expr::FunctionCall(fc)) = sh.expr.as_ref()
                    && let Some(ir) = self.try_compile_fn_object_select(fc, &sh.elements, s, distinct)?
                {
                    return Ok(IrStmt::FunctionSelect(ir));
                }

                // select fn() — bare user-defined object-returning function (no shape).
                if let Expr::FunctionCall(fc) = result
                    && let Some(ir) = self.try_compile_fn_object_select(fc, &[], s, distinct)?
                {
                    return Ok(IrStmt::FunctionSelect(ir));
                }

                // select vector::search(Type, $vec) { object { … }, distance }
                if let Expr::Shape(sh) = result
                    && let Some(Expr::FunctionCall(fc)) = sh.expr.as_ref()
                {
                    if let Some(ir) = self.try_compile_vector_search(fc, &sh.elements, s)? {
                        return Ok(IrStmt::VectorSearch(ir));
                    }
                    if let Some(ir) = self.try_compile_fts_search(fc, &sh.elements, s)? {
                        return Ok(IrStmt::FtsSearch(ir));
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
                if let Expr::Shape(sh) = result
                    && let Some(Expr::TypeCast(tc)) = sh.expr.as_ref()
                    && let Some((module, name)) = tc.ty.as_named()
                    && module
                        .map(|m| !["std", "cal", "math", "sys", "pgvector", "crypto", "postgis"].contains(&m))
                        .unwrap_or(false)
                {
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
                    // Keep the module qualification so an unknown
                    // target reports its full name, not a bare one.
                    let qname = match module {
                        Some(m) => format!("{}::{}", m, name),
                        None => name.to_string(),
                    };
                    let synthetic = ast::SelectStmt {
                        result: Expr::Shape(Box::new(ast::ShapeExpr {
                            expr: Some(Expr::Path(ast::Path::absolute(&qname))),
                            elements: sh.elements.clone(),
                            marker_offset: None,
                        })),
                        filter: merged_filter,
                        order_by: s.order_by.clone(),
                        offset: s.offset.clone(),
                        limit: s.limit.clone(),
                        lock: s.lock.clone(),
                    };
                    return self
                        .compile_select(&synthetic, &synthetic.result, distinct)
                        .map(IrStmt::Select);
                }
                // <Module::Type>expr — schema object lookup by id, or a scalar cast
                // (enum, registered named tuple, or a bare structural `tuple<...>`).
                // Stdlib modules are handled by compile_free_expr; only user schema
                // modules (or a structural tuple, which has no module at all) route here.
                const STDLIB_MODULES: &[&str] = &["std", "cal", "math", "sys", "pgvector", "crypto", "postgis"];
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
                        if let Some(sd) = self.resolve_scalar(&qname) {
                            let pg_type = format!("{}.\"{}\"", crate::sql::pg_schema_str(&sd.module), sd.name);
                            return self.scalar_cast_free_select(tc, pg_type, distinct);
                        }
                        if module.map(|m| !STDLIB_MODULES.contains(&m)).unwrap_or(false) {
                            return self.compile_schema_cast_select(s, tc).map(IrStmt::Select);
                        }
                    }
                }
                // Path traversal: `select TypeName.link.prop` or `select TypeName.link { shape }`.
                if let Expr::Path(p) = result {
                    if !p.partial
                        && p.steps.len() == 2
                        && let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice()
                        && self.resolve_enum(type_ref).is_some()
                    {
                        let expr = self.compile_enum_access(type_ref, variant)?;
                        return Ok(IrStmt::Select(IrSelect {
                            rows: vec![IrRowSource::Free(IrFreeExpr::Scalar(expr))],
                            filter: None,
                            order_by: vec![],
                            offset: None,
                            limit: None,
                            distinct,
                            dml_source: None,
                            polymorphic: false,
                            poly_implementors: vec![],
                            poly_columns: vec![],
                            lock: None,
                        }));
                    }
                    // `root.field1.field2...` where `root` is a WITH-bound
                    // free object (not a real/CTE-bound schema type) —
                    // resolve to a field reference (possibly chained through
                    // nested free objects) instead of falling into
                    // compile_path_select, which only knows schema paths.
                    if let Some(resolved) = self.resolve_cte_path(p) {
                        let expr = resolved?;
                        return Ok(IrStmt::Select(IrSelect {
                            rows: vec![IrRowSource::Free(IrFreeExpr::Scalar(expr))],
                            filter: None,
                            order_by: vec![],
                            offset: None,
                            limit: None,
                            distinct,
                            dml_source: None,
                            polymorphic: false,
                            poly_implementors: vec![],
                            poly_columns: vec![],
                            lock: None,
                        }));
                    }
                    if !p.partial && p.steps.len() > 1 {
                        return self.compile_path_select(s, p, &[], distinct).map(IrStmt::PathSelect);
                    }
                }
                if let Expr::Shape(sh) = result
                    && let Some(Expr::Path(p)) = sh.expr.as_ref()
                    && !p.partial
                    && p.steps.len() > 1
                {
                    return self
                        .compile_path_select(s, p, &sh.elements, distinct)
                        .map(IrStmt::PathSelect);
                }
                // assert_exists/assert_distinct with SubQuery arg → set-returning assert
                if let Expr::FunctionCall(f) = result
                    && (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && matches!(f.name.as_str(), "assert_exists" | "assert_distinct")
                    && !f.args.is_empty()
                    && let Expr::SubQuery(inner_stmt) = &f.args[0]
                {
                    let offset = s.offset.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
                    let limit = s.limit.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
                    // A set of objects has to come back as rows, not as the
                    // bare ids the array form carries — so the assert vets the
                    // ids and the type's own rows are selected by them.
                    if let Ok(type_name) = self.dml_subject_type(inner_stmt)
                        && let Ok(td) = self.resolve_type(&type_name)
                    {
                        let td_module = td.module.clone();
                        let td_name = td.name.clone();
                        let td_table = td.table.clone();
                        let pk = td
                            .properties
                            .iter()
                            .find(|p| p.is_pk)
                            .map(|p| (p.name.clone(), p.pg_type.clone()))
                            .unwrap_or_else(|| ("id".to_string(), "uuid".to_string()));
                        let inner_ir = self.compile_stmt(inner_stmt)?;
                        let alias = self.fresh_alias();
                        let vetted = IrExpr::FunctionCall(IrFunctionCall {
                            schema: Some("_pylon".to_string()),
                            name: f.name.clone(),
                            args: vec![IrExpr::ArrayFromSelect(Box::new(IrArraySource::StmtColumn {
                                stmt: Box::new(inner_ir),
                                column: pk.0.clone(),
                            }))],
                            sql_template: None,
                        });
                        let filter = IrExpr::BinOp(Box::new(IrBinOp {
                            left: IrExpr::ColumnRef {
                                alias: alias.clone(),
                                column: pk.0.clone(),
                                pg_type: pk.1.clone(),
                            },
                            op: ast::BinOpKind::In,
                            right: vetted,
                        }));
                        let shape = vec![IrShapePointer::Scalar(IrScalarPointer {
                            marker_offset: None,
                            alias: "id".to_string(),
                            column: pk.0,
                            pg_type: pk.1,
                            tuple_shape: None,
                        })];
                        let source = IrSource {
                            poly: None,
                            type_name: format!("{td_module}::{td_name}"),
                            table: td_table,
                            alias,
                        };
                        let mut select = IrSelect::schema_bound(source, shape, Some(filter));
                        select.offset = offset;
                        select.limit = limit;
                        select.distinct = distinct;
                        return Ok(IrStmt::Select(select));
                    }
                    let inner = self.compile_subquery_to_array_source(inner_stmt)?;
                    return Ok(IrStmt::Select(IrSelect {
                        rows: vec![IrRowSource::Free(IrFreeExpr::AssertSet {
                            fn_name: f.name.clone(),
                            inner: Box::new(inner),
                        })],
                        filter: None,
                        order_by: vec![],
                        offset,
                        limit,
                        distinct,
                        dml_source: None,
                        polymorphic: false,
                        poly_implementors: vec![],
                        poly_columns: vec![],
                        lock: None,
                    }));
                }
                // Expression containing a type-rooted path: `select fn(TypeName.link.prop, ...)`.
                if let Some(root) = self.find_path_root_in_expr(result) {
                    return self
                        .compile_expr_as_path_select(s, result, &root, distinct)
                        .map(IrStmt::PathSelect);
                }
                // `select TypeName is CheckType` — iterate source type, return bool per row.
                if let Expr::TypeIs { expr, ty } = result
                    && let Expr::Path(p) = expr.as_ref()
                    && !p.partial
                    && p.steps.len() == 1
                    && let ast::PathStep::Name(src_name) = &p.steps[0]
                {
                    let src_td = self.resolve_type(src_name)?;
                    {
                        let src_qname = format!("{}::{}", src_td.module, src_td.name);
                        let (ty_module, ty_name) = ty
                            .as_named()
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
                        let bool_expr = if check_qname == src_qname || src_interfaces.iter().any(|i| i == &check_qname)
                        {
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
                                poly: None,
                                type_name: src_qname,
                                table: src_table,
                                alias: src_alias,
                            },
                            joins: vec![],
                            result: IrPathResult::Scalar(bool_expr, None),
                            filter: s
                                .filter
                                .as_ref()
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
                // Catch mixed object/scalar UNION before dispatching further.
                self.check_union_type_compat(result)?;
                if self.is_free_result(result) {
                    self.compile_free_select(s, result, distinct).map(IrStmt::Select)
                } else {
                    self.compile_select(s, result, distinct).map(IrStmt::Select)
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
                    if self.bind_inline_if_correlated(&alias.name, &alias.expr)? {
                        continue;
                    }
                    let ir_inner = compile_cte_binding(self, &alias.expr)?;
                    let type_name = self.register_cte(&alias.name, &ir_inner);
                    self.hoisted_ctes.push(IrCteDef {
                        name: alias.name.clone(),
                        stmt: ir_inner,
                        type_name,
                    });
                }
                self.compile_stmt(&w.stmt)
            }
            Stmt::For(f) => self.compile_for(f).map(IrStmt::For),
            // `analyze` only changes how the query is *executed* (see
            // `analyze.rs`) — the inner statement's IR is identical either
            // way, so this layer just unwraps and compiles it normally.
            Stmt::Analyze(inner) => self.compile_stmt(inner),
        }
    }

    /// `<Module::Type>expr` in SELECT position is a schema object lookup:
    /// select the object whose `id` equals `expr`.  Semantically identical to
    /// `SELECT Type FILTER .id = expr` plus any modifiers on the outer SELECT.
    fn compile_schema_cast_select(&mut self, sel: &ast::SelectStmt, tc: &ast::TypeCast) -> Result<IrSelect, PyQLError> {
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
        let (module, name) = tc
            .ty
            .as_named()
            .ok_or_else(|| self.type_err("cannot use a tuple or array type as a schema object cast"))?;
        // Keep the module qualification in the synthetic path so a genuinely
        // unknown target (neither an object type, enum, scalar, nor named
        // tuple) reports its full name via `resolve_type`'s own error,
        // instead of silently dropping the module and reporting a bare name.
        let qname = match module {
            Some(m) => format!("{}::{}", m, name),
            None => name.to_string(),
        };
        let synthetic = ast::SelectStmt {
            result: Expr::Path(ast::Path::absolute(&qname)),
            filter: merged_filter,
            order_by: sel.order_by.clone(),
            offset: sel.offset.clone(),
            limit: sel.limit.clone(),
            lock: sel.lock.clone(),
        };
        self.compile_select(&synthetic, &synthetic.result, false)
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
        self.compile_path_select_with_tail(sel, path, shape_elements, distinct, 0)
    }

    /// `compile_path_select` where the last `tail` steps were appended by a
    /// projection off a sub-select — `(select … limit 1).plan.tier` — so
    /// `sel`'s own modifiers belong to the step before them, not to the end
    /// of the path.
    fn compile_path_select_with_tail(
        &mut self,
        sel: &ast::SelectStmt,
        path: &ast::Path,
        shape_elements: &[ShapeElement],
        distinct: bool,
        tail: usize,
    ) -> Result<IrPathSelect, PyQLError> {
        let outer_anchor = self.modifier_anchor.take();
        let mut result = self.compile_path_select_inner(sel, path, shape_elements, distinct, tail);
        self.modifier_anchor = outer_anchor;
        if let Ok(path_select) = &mut result {
            self.resolve_join_fanouts(path_select);
        }
        result
    }

    fn compile_path_select_inner(
        &mut self,
        sel: &ast::SelectStmt,
        path: &ast::Path,
        shape_elements: &[ShapeElement],
        distinct: bool,
        mut tail: usize,
    ) -> Result<IrPathSelect, PyQLError> {
        use ast::PathStep;

        let root_name = match &path.steps[0] {
            PathStep::Name(n) => n.as_str(),
            _ => return Err(self.type_err("path traversal must start with a type name")),
        };
        // A WITH-block CTE bound to an object type (e.g. `with user :=
        // (select global current_user) select user.gender`) can be
        // traversed just like a real type name — resolve its underlying
        // type and source from the `@cte:` sentinel table (the same
        // mechanism `compile_select`'s bare-CTE-object-select case uses)
        // instead of failing with "unknown type '{root_name}'".
        let cte_object_type = self.cte_types.get(root_name).filter(|t| t.contains("::")).cloned();
        let root_td = self.resolve_path_root(root_name)?;
        let root_alias = self.fresh_alias();
        let root = IrSource {
            poly: None,
            type_name: format!("{}::{}", root_td.module, root_td.name),
            table: match &cte_object_type {
                Some(_) => format!("@cte:{}", root_name),
                None => root_td.table.clone(),
            },
            alias: root_alias.clone(),
        };
        // `for c in (select Person) union (select c.name)` — the loop variable
        // holds the row's key, so the walk starts from that row rather than
        // from the whole table.
        let for_var_root = self.for_var_types.contains_key(root_name).then(|| {
            IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: root_alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ForVar {
                    name: root_name.to_string(),
                },
            }))
        });

        let mut joins: Vec<IrPathJoin> = vec![];
        let mut current_td = root_td;
        let mut current_alias = root_alias;

        // Owned rather than borrowed: traversing *through* a computed
        // pointer splices that computed's own path in place of the step
        // naming it (see the computed branch at the bottom of the loop).
        let mut steps: Vec<PathStep> = path.steps[1..].to_vec();
        // `(step index, filter)` a spliced computed contributed, to compile
        // against the type that step lands on once the loop reaches it.
        let mut pending_filters: Vec<(usize, Expr)> = vec![];
        let mut extra_conditions: Vec<IrExpr> = for_var_root.into_iter().collect();
        // The junction the traversal last crossed, for a bare `@prop` in the
        // select's own modifiers.
        let mut junction_scope: Option<(String, String)> = None;
        let mut splices = 0usize;
        let mut idx = 0;
        if tail > 0 && tail >= steps.len() {
            self.modifier_anchor = Some((
                format!("{}::{}", current_td.module, current_td.name),
                current_alias.clone(),
            ));
        }
        while idx < steps.len() {
            let owned_step = steps[idx].clone();
            let step = &owned_step;
            let n_steps = steps.len();
            let is_last = |extra: usize| idx + extra == n_steps - 1;
            if tail > 0 && idx == n_steps - tail {
                self.modifier_anchor = Some((
                    format!("{}::{}", current_td.module, current_td.name),
                    current_alias.clone(),
                ));
            }

            // A spliced computed's own filter belongs to the step it landed
            // on, which is the one just processed.
            while let Some(pos) = pending_filters.iter().position(|(i, _)| *i + 1 == idx) {
                let (_, f) = pending_filters.remove(pos);
                let cond = self.compile_expr(&f, current_td, &current_alias)?;
                extra_conditions.push(cond);
            }

            // Type intersection standalone (not after backlink): narrows current_td.
            if let PathStep::TypeIntersection(type_ref) = step {
                let type_name = match &type_ref.module {
                    Some(m) => format!("{}::{}", m, type_ref.name),
                    None => type_ref.name.clone(),
                };
                let narrowed = self.resolve_type(&type_name)?;
                // Narrowing to a type with a relation of its own moves to that
                // relation, joined on the shared id: an interface's view has
                // none of its implementors' own columns, and two unrelated
                // types share no ids at all, so the join finds nothing — which
                // is exactly what an impossible intersection yields. A mixin
                // has no relation; its columns are already on the current row.
                let narrowed_has_relation = !narrowed.abstract_ || narrowed.materialized;
                if narrowed_has_relation && narrowed.table != current_td.table {
                    let target_alias = self.fresh_alias();
                    let target = IrSource {
                        poly: None,
                        type_name: format!("{}::{}", narrowed.module, narrowed.name),
                        table: narrowed.table.clone(),
                        alias: target_alias.clone(),
                    };
                    joins.push(IrPathJoin::Single {
                        source_alias: current_alias.clone(),
                        fk_col: "id".to_string(),
                        target,
                    });
                    current_alias = target_alias;
                }
                current_td = narrowed;
                idx += 1;
                continue;
            }

            // Backlink: .<link_name — optionally followed by [is OwnerType].
            if let PathStep::Backlink(link_name) = step {
                // Peek ahead: if the next step is [is Type], use it to identify the owner.
                let owner_hint = steps.get(idx + 1).and_then(|s| {
                    if let PathStep::TypeIntersection(tr) = s {
                        Some(tr.clone())
                    } else {
                        None
                    }
                });
                let consumed_extra = if owner_hint.is_some() { 1 } else { 0 };

                let owner_td: &TypeDescriptor = if let Some(ref tr) = owner_hint {
                    let type_name = match &tr.module {
                        Some(m) => format!("{}::{}", m, tr.name),
                        None => tr.name.clone(),
                    };
                    let td = self.resolve_type(&type_name)?;
                    let current_qname = format!("{}::{}", current_td.module, current_td.name);
                    let link_targets_current =
                        td.links
                            .iter()
                            .any(|l| l.name == *link_name && self.link_target_reaches(&l.target, &current_qname))
                            || td.multilinks.iter().any(|ml| {
                                ml.name == *link_name && self.link_target_reaches(&ml.target, &current_qname)
                            });
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
                    self.schema
                        .types
                        .iter()
                        .find(|t| {
                            t.links
                                .iter()
                                .any(|l| l.name == *link_name && self.link_target_reaches(&l.target, &current_qname))
                                || t.multilinks.iter().any(|ml| {
                                    ml.name == *link_name && self.link_target_reaches(&ml.target, &current_qname)
                                })
                        })
                        .ok_or_else(|| {
                            self.type_err(&format!(
                                "no type has a link '{}' targeting '{}'",
                                link_name, current_qname,
                            ))
                        })?
                };

                let target_alias = self.fresh_alias();
                let target = IrSource {
                    poly: None,
                    type_name: format!("{}::{}", owner_td.module, owner_td.name),
                    table: owner_td.table.clone(),
                    alias: target_alias.clone(),
                };

                // Determine if the link is single (FK), junction-backed
                // single (same shape as a backlinked multi-link), or multi
                // (junction).
                if let Some(l) = owner_td.links.iter().find(|l| l.name == *link_name) {
                    if l.is_junction_backed() {
                        let junction_alias = self.fresh_alias();
                        let (junction_table, module, owner_col, current_col, _) =
                            self.link_junction_info(owner_td, l)?;
                        joins.push(IrPathJoin::BacklinkMulti {
                            source_alias: current_alias.clone(),
                            junction_alias,
                            junction_table,
                            module,
                            owner_col,
                            current_col,
                            target,
                        });
                    } else {
                        joins.push(IrPathJoin::BacklinkSingle {
                            source_alias: current_alias.clone(),
                            fk_col: format!("{}_id", link_name),
                            target,
                        });
                    }
                } else {
                    let ml = owner_td
                        .multilinks
                        .iter()
                        .find(|ml| ml.name == *link_name)
                        .unwrap()
                        .clone();
                    let junction_alias = self.fresh_alias();
                    let (junction_table, module, _, _, _) = self.multilink_junction_info(owner_td, &ml)?;
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
                    let shape =
                        self.compile_shape(shape_elements, owner_td, &target_alias, &owner_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", owner_td.module, owner_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers_scoped(sel, owner_td, &target_alias, junction_scope.clone())?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter: and_conditions(filter, extra_conditions),
                        order_by,
                        offset,
                        limit,
                        distinct,
                        poly_implementors: vec![],
                    });
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

            // `__type__` is a virtual scalar property holding the
            // fully-qualified type name: a real discriminator column on a
            // polymorphic (interface) source, a constant on a concrete one.
            if step_name == "__type__" {
                if !is_last(0) {
                    return Err(
                        self.type_err("'__type__' is the type's name, not an object — it cannot be traversed further")
                    );
                }
                let polymorphic = current_td.abstract_ && current_td.materialized;
                let expr = if polymorphic {
                    IrExpr::ColumnRef {
                        alias: current_alias.clone(),
                        column: "__type__".to_string(),
                        pg_type: "text".to_string(),
                    }
                } else {
                    IrExpr::Literal(IrLiteral::Str(format!("{}::{}", current_td.module, current_td.name)))
                };
                // A CTE-backed root already carries the discriminator column
                // from the binding's own polymorphic expansion; re-expanding it
                // here would read the base type again and drop the binding's
                // filter.
                let poly_implementors = if polymorphic && joins.is_empty() && !root.table.starts_with("@cte:") {
                    self.find_poly_implementors(&root.type_name.clone())
                } else {
                    vec![]
                };
                let (filter, order_by, offset, limit) =
                    self.compile_path_modifiers_scoped(sel, current_td, &current_alias, junction_scope.clone())?;
                return Ok(IrPathSelect {
                    root,
                    joins,
                    result: IrPathResult::Scalar(expr, None),
                    filter: and_conditions(filter, extra_conditions),
                    order_by,
                    offset,
                    limit,
                    distinct,
                    poly_implementors,
                });
            }

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
                                _ => return Err(self.type_err("only field name steps are valid inside a named tuple")),
                            };
                            ir = IrExpr::JsonbField {
                                expr: Box::new(ir),
                                field,
                            };
                        }
                        let (filter, order_by, offset, limit) = self.compile_path_modifiers_scoped(
                            sel,
                            current_td,
                            &current_alias,
                            junction_scope.clone(),
                        )?;
                        return Ok(IrPathSelect {
                            root,
                            joins,
                            result: IrPathResult::Scalar(ir, None),
                            filter: and_conditions(filter, extra_conditions),
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
                let result = IrPathResult::Scalar(
                    IrExpr::ColumnRef {
                        alias: current_alias.clone(),
                        column: p.name.clone(),
                        pg_type: p.pg_type.clone(),
                    },
                    tuple_shape,
                );
                let (filter, order_by, offset, limit) =
                    self.compile_path_modifiers_scoped(sel, current_td, &current_alias, junction_scope.clone())?;
                return Ok(IrPathSelect {
                    root,
                    joins,
                    result,
                    filter: and_conditions(filter, extra_conditions),
                    order_by,
                    offset,
                    limit,
                    distinct,
                    poly_implementors: vec![],
                });
            }

            // Single link.
            if let Some(l) = Self::resolve_link(current_td, step_name) {
                let target_td = self.resolve_type(&l.target)?;
                let target_alias = self.fresh_alias();
                let target = IrSource {
                    poly: None,
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: target_alias.clone(),
                };
                if l.is_junction_backed() {
                    // Same join shape a multi-link's own path step uses
                    // (D1) — `PRIMARY KEY (source)` on the junction table
                    // already guarantees at most one matching row, so no
                    // extra cardinality handling is needed here.
                    let join = self.build_multilink_join(current_td, &l.name, &l.target, &l.through)?;
                    let junction_alias = self.fresh_alias();
                    junction_scope = l.through.clone().map(|t| (t, junction_alias.clone()));
                    joins.push(IrPathJoin::Multi {
                        source_alias: current_alias.clone(),
                        junction_alias,
                        join,
                        target,
                    });
                } else {
                    joins.push(IrPathJoin::Single {
                        source_alias: current_alias.clone(),
                        fk_col: format!("{}_id", l.name),
                        target,
                    });
                }
                if is_last(0) {
                    let shape =
                        self.compile_shape(shape_elements, target_td, &target_alias, &target_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers_scoped(sel, target_td, &target_alias, junction_scope.clone())?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter: and_conditions(filter, extra_conditions),
                        order_by,
                        offset,
                        limit,
                        distinct,
                        poly_implementors: vec![],
                    });
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
                    poly: None,
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: target_alias.clone(),
                };
                let join_info = if let Some(through_qname) = &ml.through {
                    let through_td = self.resolve_type(through_qname)?;
                    if through_td.junction {
                        // See `junction_info_for`'s doc comment: owner-derived,
                        // never `through_td.table` itself.
                        IrMultiLinkJoin::Standard {
                            junction_table: format!("{}.{}", current_td.table, ml.name),
                            module: current_td.module.clone(),
                        }
                    } else {
                        let source_qname = format!("{}::{}", current_td.module, current_td.name);
                        let source_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == source_qname)
                            .ok_or_else(|| {
                                PyQLError::Type(PyQLTypeError {
                                    message: format!("through type {through_qname} has no link to {source_qname}"),
                                    position: Position { line: 0, col: 0 },
                                })
                            })?
                            .name
                            .clone();
                        let target_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == ml.target && l.name != source_col)
                            .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                            .ok_or_else(|| {
                                PyQLError::Type(PyQLTypeError {
                                    message: format!(
                                        "through type {through_qname} has no link to target {}",
                                        ml.target
                                    ),
                                    position: Position { line: 0, col: 0 },
                                })
                            })?
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
                        junction_table: format!("{}.{}", current_td.table, ml.name),
                        module: current_td.module.clone(),
                    }
                };
                junction_scope = ml.through.clone().map(|t| (t, junction_alias.clone()));
                joins.push(IrPathJoin::Multi {
                    source_alias: current_alias.clone(),
                    junction_alias,
                    join: join_info,
                    target,
                });
                if is_last(0) {
                    let shape =
                        self.compile_shape(shape_elements, target_td, &target_alias, &target_td.module.clone())?;
                    let result = IrPathResult::Object {
                        alias: target_alias.clone(),
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        shape,
                    };
                    let (filter, order_by, offset, limit) =
                        self.compile_path_modifiers_scoped(sel, target_td, &target_alias, junction_scope.clone())?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter: and_conditions(filter, extra_conditions),
                        order_by,
                        offset,
                        limit,
                        distinct,
                        poly_implementors: vec![],
                    });
                }
                current_td = target_td;
                current_alias = target_alias;
                idx += 1;
                continue;
            }

            // A computed pointer declared on the type reached so far (or on
            // an interface it implements).
            if let Some(cd) = self.resolve_computed(current_td, step_name) {
                let expr_ast = crate::parse::parse_pointer_expr(&cd.expression).map_err(PyQLError::Syntax)?;

                // Traversing *through* it: a computed has no stored column
                // for a join to hang off, but if it just names a link — with
                // or without a filter — its own path can take the step's
                // place, and its filter rides along on the spliced segment.
                // Chaining computeds this way is routine, so the splice
                // re-enters the loop and expands again if it lands on
                // another one.
                if !is_last(0)
                    && let Some((p, _, modifiers)) = Self::pointer_subject(&expr_ast)
                    && p.partial
                    && !p.steps.is_empty()
                {
                    // Order/offset/limit of its own pick one row *per source
                    // row*, which no plain join expresses — so the computed's
                    // own traversal becomes a correlated LATERAL and the path
                    // continues from its result.
                    if let Some(m) = modifiers
                        && (m.limit.is_some() || m.offset.is_some() || !m.order_by.is_empty())
                    {
                        let mut inner_steps =
                            vec![PathStep::Name(format!("{}::{}", current_td.module, current_td.name))];
                        inner_steps.extend(p.steps.iter().cloned());
                        let inner_path = ast::Path {
                            steps: inner_steps,
                            partial: false,
                        };
                        let inner_sel = ast::SelectStmt {
                            result: Expr::Path(inner_path.clone()),
                            filter: m.filter.clone(),
                            order_by: m.order_by.clone(),
                            offset: m.offset.clone(),
                            limit: m.limit.clone(),
                            lock: None,
                        };
                        let mut inner = self.compile_path_select(&inner_sel, &inner_path, &[], false)?;
                        Self::correlate_path_select(&mut inner, &current_alias);
                        let IrPathResult::Object { type_name, .. } = &inner.result else {
                            return Err(self.type_err(&format!(
                                "computed pointer '{step_name}' is a scalar — a path cannot continue through it"
                            )));
                        };
                        let target_td = self.resolve_type(&type_name.clone())?;
                        let target_alias = self.fresh_alias();
                        let target = IrSource {
                            poly: None,
                            type_name: format!("{}::{}", target_td.module, target_td.name),
                            table: target_td.table.clone(),
                            alias: target_alias.clone(),
                        };
                        joins.push(IrPathJoin::Lateral {
                            inner: Box::new(inner),
                            target,
                        });
                        current_td = target_td;
                        current_alias = target_alias;
                        idx += 1;
                        continue;
                    }
                    splices += 1;
                    if splices > MAX_COMPUTED_SPLICES {
                        return Err(self.type_err(&format!(
                            "computed pointer '{step_name}' expands into itself — \
                             a path cannot be resolved through a cycle of computed pointers"
                        )));
                    }
                    let filter = modifiers.and_then(|m| m.filter.clone());
                    let spliced = p.steps.clone();
                    if tail > 0 && idx >= steps.len() - tail {
                        tail += spliced.len() - 1;
                    }
                    let landing = idx + spliced.len() - 1;
                    steps.splice(idx..idx + 1, spliced);
                    // Every filter already queued for a later step shifts
                    // along with it.
                    let shift = landing - idx;
                    for (i, _) in pending_filters.iter_mut() {
                        if *i > idx {
                            *i += shift;
                        }
                    }
                    if let Some(f) = filter {
                        pending_filters.push((landing, f));
                    }
                    continue;
                }

                // A computed backed by an object-returning function —
                // `translation := latest(.id)`. There is no path to splice
                // in, so the call itself becomes the next row source: a
                // LATERAL join, since its arguments read the alias the
                // traversal has reached.
                if let Some((fc, modifiers)) = Self::function_subject(&expr_ast)
                    && let Some(fd) = self.resolve_object_fn(fc)
                {
                    if fd.params.len() != fc.args.len() {
                        return Err(self.type_err(&format!(
                            "function '{}::{}' expects {} argument(s), got {}",
                            fd.module,
                            fd.name,
                            fd.params.len(),
                            fc.args.len()
                        )));
                    }
                    let (fn_module, fn_name, return_type_name) =
                        (fd.module.clone(), fd.name.clone(), fd.return_pg_type.clone());
                    let mut args = fc
                        .args
                        .iter()
                        .map(|a| self.compile_expr(a, current_td, &current_alias))
                        .collect::<Result<Vec<_>, _>>()?;
                    let qualified = format!("{fn_module}::{fn_name}");
                    if let Some(globals) = self.globals_arg_for_call(&qualified)? {
                        args.insert(0, globals);
                    }
                    let target_td = self.resolve_type(&return_type_name)?;
                    let target_alias = self.fresh_alias();
                    let target = IrSource {
                        poly: None,
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: target_alias.clone(),
                    };
                    joins.push(IrPathJoin::Function {
                        fn_module,
                        fn_name,
                        args,
                        target,
                    });
                    if let Some(f) = modifiers.and_then(|m| m.filter.clone()) {
                        let cond = self.compile_expr(&f, target_td, &target_alias)?;
                        extra_conditions.push(cond);
                    }
                    if is_last(0) {
                        let shape =
                            self.compile_shape(shape_elements, target_td, &target_alias, &target_td.module.clone())?;
                        let result = IrPathResult::Object {
                            alias: target_alias.clone(),
                            type_name: format!("{}::{}", target_td.module, target_td.name),
                            shape,
                        };
                        let (filter, order_by, offset, limit) =
                            self.compile_path_modifiers_scoped(sel, target_td, &target_alias, junction_scope.clone())?;
                        return Ok(IrPathSelect {
                            root,
                            joins,
                            result,
                            filter: and_conditions(filter, extra_conditions),
                            order_by,
                            offset,
                            limit,
                            distinct,
                            poly_implementors: vec![],
                        });
                    }
                    current_td = target_td;
                    current_alias = target_alias;
                    idx += 1;
                    continue;
                }

                if !is_last(0) {
                    return Err(self.type_err(&format!(
                        "'{step_name}' is a computed pointer — it has no stored column to traverse further through"
                    )));
                }
                let expr = self.compile_expr(&expr_ast, current_td, &current_alias)?;
                let (filter, order_by, offset, limit) =
                    self.compile_path_modifiers_scoped(sel, current_td, &current_alias, junction_scope.clone())?;
                return Ok(IrPathSelect {
                    root,
                    joins,
                    result: IrPathResult::Scalar(expr, None),
                    filter: and_conditions(filter, extra_conditions),
                    order_by,
                    offset,
                    limit,
                    distinct,
                    poly_implementors: vec![],
                });
            }

            return Err(self.field_err(step_name, &format!("{}::{}", current_td.module, current_td.name)));
        }

        // Should be unreachable: steps is non-empty (we checked len > 1 before dispatch).
        Err(self.type_err("empty path traversal"))
    }

    /// `compile_path_modifiers` with the junction the traversal last crossed
    /// in scope, so a bare `@prop` in the select's own FILTER/ORDER BY can
    /// read it. Pushes nothing when there is no junction, leaving `@prop` to
    /// report that it has no link to belong to.
    fn compile_path_modifiers_scoped(
        &mut self,
        sel: &ast::SelectStmt,
        td: &TypeDescriptor,
        alias: &str,
        junction: Option<(String, String)>,
    ) -> Result<SelectModifiers, PyQLError> {
        let pushed = junction.is_some();
        if pushed {
            self.link_prop_scope.push(junction);
        }
        let anchored = self.modifier_anchor.take();
        let result = match &anchored {
            Some((qualified, anchor_alias)) => {
                let anchor_td = self.resolve_type(qualified)?;
                let anchor_alias = anchor_alias.clone();
                self.compile_path_modifiers(sel, anchor_td, &anchor_alias)
            }
            None => self.compile_path_modifiers(sel, td, alias),
        };
        if pushed {
            self.link_prop_scope.pop();
        }
        result
    }

    fn compile_path_modifiers(
        &mut self,
        sel: &ast::SelectStmt,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<SelectModifiers, PyQLError> {
        self.anchors.push(SelectAnchor {
            type_name: td.name.clone(),
            qualified: format!("{}::{}", td.module, td.name),
            alias: alias.to_string(),
            detached: std::mem::take(&mut self.pending_detached),
        });
        let result = self.compile_path_modifiers_inner(sel, td, alias);
        self.anchors.pop();
        result
    }

    fn compile_path_modifiers_inner(
        &mut self,
        sel: &ast::SelectStmt,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<SelectModifiers, PyQLError> {
        let filter = sel
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, alias))
            .transpose()?;
        let order_by = sel
            .order_by
            .iter()
            .map(|s| self.compile_sort(s, td, alias))
            .collect::<Result<Vec<_>, _>>()?;
        let offset = sel
            .offset
            .as_ref()
            .map(|e| self.compile_expr(e, td, alias))
            .transpose()?;
        let limit = sel
            .limit
            .as_ref()
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
                if let ast::PathStep::Name(root) = &p.steps[0]
                    && (self.resolve_type(root).is_ok() || self.cte_object_type(root).is_some())
                {
                    return Some(root.clone());
                }
                None
            }
            Expr::FunctionCall(f) => f.args.iter().find_map(|a| self.find_path_root_in_expr(a)),
            Expr::BinOp(b) => self
                .find_path_root_in_expr(&b.left)
                .or_else(|| self.find_path_root_in_expr(&b.right)),
            Expr::UnaryOp(u) => self.find_path_root_in_expr(&u.operand),
            _ => None,
        }
    }

    /// Rewrite absolute paths rooted at `root_name` to relative (partial) paths.
    fn rewrite_abs_to_partial(expr: Expr, root_name: &str) -> Expr {
        match expr {
            Expr::Path(ref p) if !p.partial => {
                if let ast::PathStep::Name(first) = &p.steps[0]
                    && first == root_name
                    && p.steps.len() > 1
                {
                    return Expr::Path(ast::Path {
                        steps: p.steps[1..].to_vec(),
                        partial: true,
                    });
                }
                expr
            }
            Expr::FunctionCall(f) => Expr::FunctionCall(ast::FunctionCall {
                module: f.module,
                name: f.name,
                args: f
                    .args
                    .into_iter()
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

    /// The pointers a sub-select's shape declares, as the relative paths they
    /// stand for: `{ handle := [is Handle].handle }` → `handle` → `[is
    /// Handle].handle`. `None` when any element is something other than a
    /// named pointer defined by a relative path, which this substitution
    /// cannot stand in for.
    fn shape_alias_paths(elements: &[ShapeElement]) -> Option<Vec<(String, ast::Path)>> {
        let mut defs = Vec::with_capacity(elements.len());
        for element in elements {
            let [ast::PathStep::Name(name)] = element.path.steps.as_slice() else {
                return None;
            };
            match &element.compexpr {
                Some(Expr::Path(p)) if p.partial => defs.push((name.clone(), p.clone())),
                _ => return None,
            }
        }
        Some(defs)
    }

    /// Replace each `.alias` a sub-select's shape declares with the path it
    /// stands for, so the select's own FILTER/ORDER BY read the same thing the
    /// shape projects.
    fn substitute_shape_aliases(expr: Expr, defs: &[(String, ast::Path)]) -> Expr {
        match expr {
            Expr::Path(ref p) if p.partial => {
                let Some(ast::PathStep::Name(first)) = p.steps.first() else {
                    return expr;
                };
                let Some((_, definition)) = defs.iter().find(|(name, _)| name == first) else {
                    return expr;
                };
                let mut steps = definition.steps.clone();
                steps.extend(p.steps[1..].iter().cloned());
                Expr::Path(ast::Path { steps, partial: true })
            }
            Expr::FunctionCall(f) => Expr::FunctionCall(ast::FunctionCall {
                module: f.module,
                name: f.name,
                args: f
                    .args
                    .into_iter()
                    .map(|a| Self::substitute_shape_aliases(a, defs))
                    .collect(),
                kwargs: f.kwargs,
            }),
            Expr::BinOp(b) => Expr::BinOp(Box::new(ast::BinOp {
                left: Self::substitute_shape_aliases(b.left, defs),
                op: b.op,
                right: Self::substitute_shape_aliases(b.right, defs),
            })),
            Expr::UnaryOp(u) => Expr::UnaryOp(Box::new(ast::UnaryOp {
                op: u.op,
                operand: Self::substitute_shape_aliases(u.operand, defs),
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
        // `array_agg(a.sessions.id)` — a single-argument aggregate over a
        // path. The aggregate applies to the set the path traverses *to*, so
        // the traversal becomes this select's own row source and the aggregate
        // wraps the column it lands on. Compiling the path as an expression
        // instead yields the array it stands for, and aggregating that nests it
        // one level deep.
        if let Expr::FunctionCall(f) = result
            && f.args.len() == 1
            && let Expr::Path(p) = &f.args[0]
            && !p.partial
            && p.steps.len() > 1
        {
            let arg_path = ast::Path {
                partial: false,
                steps: p.steps.clone(),
            };
            let mut ps = self.compile_path_select(sel, &arg_path, &[], distinct)?;
            if let IrPathResult::Scalar(column, _) = ps.result.clone() {
                let call = self.resolve_fn_call(f.module.as_deref(), &f.name, vec![column])?;
                ps.result = IrPathResult::Scalar(call, None);
                return Ok(ps);
            }
        }

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
            let root_td = match self.cte_object_type(root_type_name) {
                Some(t) => self.resolve_type(&t)?,
                None => self.resolve_type(root_type_name)?,
            };
            // Build PathSelect for the path (builds root + joins + scalar/object result)
            let ast_path = ast::Path {
                partial: false,
                steps: path_expr.steps.clone(),
            };
            let mut ps = self.compile_path_select(sel, &ast_path, &[], distinct)?;
            let val_ir = self.compile_expr(value_expr, root_td, &ps.root.alias)?;
            // Extract the scalar result from the path
            let scalar_col = match ps.result {
                IrPathResult::Scalar(e, _) => e,
                IrPathResult::Object { type_name, .. } => {
                    let val_type = infer_ir_type(&val_ir).map(pg_type_to_pyql).unwrap_or("unknown");
                    return Err(PyQLError::Type(PyQLTypeError {
                        message: format!(
                            "operator '{}' cannot be applied to operands of type '{}' and '{}'",
                            b.op, type_name, val_type,
                        ),
                        position: Position { line: 0, col: 0 },
                    }));
                }
            };
            let (l, r) = if flip {
                (val_ir, scalar_col)
            } else {
                (scalar_col, val_ir)
            };
            ps.result = IrPathResult::Scalar(
                IrExpr::BinOp(Box::new(IrBinOp {
                    left: l,
                    op: b.op.clone(),
                    right: r,
                })),
                None,
            );
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
        let cte_object_type = self.cte_object_type(root_type_name);
        let td = match &cte_object_type {
            Some(t) => self.resolve_type(t)?,
            None => self.resolve_type(root_type_name)?,
        };
        let alias = self.fresh_alias();
        let root = IrSource {
            poly: None,
            type_name: format!("{}::{}", td.module, td.name),
            table: match &cte_object_type {
                Some(_) => format!("@cte:{}", root_type_name),
                None => td.table.clone(),
            },
            alias: alias.clone(),
        };
        let rewritten = Self::rewrite_abs_to_partial(result.clone(), root_type_name);
        let expr = self.compile_expr(&rewritten, td, &alias)?;
        let (filter, order_by, offset, limit) = self.compile_path_modifiers(sel, td, &alias)?;
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
            IrStmt::Select(s) if matches!(s.rows.as_slice(), [IrRowSource::Bound { .. }]) => {
                Ok(IrArraySource::Select(s))
            }
            IrStmt::PathSelect(ps) => Ok(IrArraySource::PathSelect(Box::new(ps))),
            _ => Err(self.type_err("assert functions require a schema-bound SELECT as argument")),
        }
    }

    /// `exists` in either schema-bound (FILTER, computed pointer, etc. —
    /// `ctx = Some((td, alias))`) or free (`ctx = None`) expression context.
    /// The `Path`-based arms (backlink / prop / link / multilink existence)
    /// only apply schema-bound — guarded on `ctx.is_some()` — since none of
    /// those concepts exist without a schema type in scope; everything else
    /// (`Parameter`, `TypeCast`, `SubQuery`, and the scalar-expression
    /// fallback) is identical either way and just threads `ctx` through.
    fn compile_exists_ctx(
        &mut self,
        operand: &Expr,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<IrExpr, PyQLError> {
        match operand {
            // exists $param  /  exists <type>$param → $N IS NOT NULL
            Expr::Parameter(name) => {
                let idx = self.param_index(name);
                Ok(ir_is_not_null(IrExpr::Param { index: idx }))
            }
            // exists <type>expr → expr IS NOT NULL (cast result is always a scalar)
            Expr::TypeCast(_) => {
                let inner = self.compile_expr_ctx(operand, ctx)?;
                Ok(ir_is_not_null(inner))
            }

            // exists .<link[is Type] → EXISTS(SELECT 1 FROM type WHERE type.link_id = alias.id)
            Expr::Path(p)
                if ctx.is_some() && p.partial && matches!(p.steps.first(), Some(ast::PathStep::Backlink(_))) =>
            {
                let (td, alias) = ctx.unwrap();
                let current_qname = format!("{}::{}", td.module, td.name);
                let exists = self.compile_backlink_as_exists(&p.steps, None, &current_qname, alias)?;
                Ok(exists)
            }

            // exists .prop → alias.col IS NOT NULL
            // exists .link → alias.link_id IS NOT NULL
            // exists .multilink → EXISTS(SELECT 1 FROM junction WHERE src = alias.id)
            Expr::Path(p) if ctx.is_some() && p.partial && p.steps.len() == 1 => {
                let (td, alias) = ctx.unwrap();
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
                    if link.is_junction_backed() {
                        return self.compile_junction_link_exists_check(link, td, alias);
                    }
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

            // `exists ((select .prices filter …))` — a sub-select over a
            // relative path is relative to the enclosing object, so it needs
            // the same rooting and correlation `count((select .prices))`
            // already gets. Compiled without them it resolves in free context
            // and reports the pointer as unknown.
            Expr::SubQuery(stmt)
                if ctx.is_some()
                    && matches!(
                        stmt.as_ref(),
                        Stmt::Select(inner) if matches!(&inner.result, Expr::Path(p) if p.partial)
                    ) =>
            {
                let Stmt::Select(inner) = stmt.as_ref() else {
                    unreachable!("checked by the guard")
                };
                let Expr::Path(path) = &inner.result else {
                    unreachable!("checked by the guard")
                };
                let (td, alias) = ctx.expect("checked by the guard");
                let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
                steps.extend(path.steps.iter().cloned());
                let full_path = ast::Path { steps, partial: false };
                let rooted = ast::SelectStmt {
                    result: Expr::Path(full_path.clone()),
                    filter: inner.filter.clone(),
                    order_by: inner.order_by.clone(),
                    offset: inner.offset.clone(),
                    limit: inner.limit.clone(),
                    lock: None,
                };
                let mut ps = self.compile_path_select(&rooted, &full_path, &[], false)?;
                Self::correlate_path_select(&mut ps, alias);
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: IrExpr::PathSubquery(Box::new(ps)),
                })))
            }

            // exists (select ...) → EXISTS(SELECT 1 FROM ...)
            Expr::SubQuery(stmt) => self.compile_subquery_exists(stmt),

            // Fallback: any scalar expression → expr IS NOT NULL. When `ctx`
            // is `None` and `operand` is a partial `Path` not caught above
            // (guards failed since ctx.is_some() was false), this recurses
            // into the free-context Path handling, which itself produces the
            // "not valid in free SELECT" error — same as before the merge.
            other => {
                let inner = self.compile_expr_ctx(other, ctx)?;
                Ok(ir_is_not_null(inner))
            }
        }
    }

    /// Compile `exists (select ...)` → `EXISTS(SELECT 1 FROM ... WHERE ...)`.
    fn compile_subquery_exists(&mut self, stmt: &Stmt) -> Result<IrExpr, PyQLError> {
        match self.compile_stmt(stmt)? {
            IrStmt::Select(s) => {
                let mut rows = s.rows;
                if rows.len() != 1 {
                    return Err(self.type_err("exists requires a schema-bound SELECT expression"));
                }
                let IrRowSource::Bound { source, .. } = rows.remove(0) else {
                    return Err(self.type_err("exists requires a schema-bound SELECT expression"));
                };
                let inner = IrExpr::Subquery(Box::new(IrSelect::schema_bound(source, vec![], s.filter)));
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: inner,
                })))
            }
            IrStmt::PathSelect(ps) => {
                // EXISTS(SELECT 1 FROM root [JOINs] WHERE filter)
                // Reuse the path select but signal "exists" via a dedicated IR node
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(ps.root, vec![], ps.filter))),
                })))
            }
            _ => Err(self.type_err("exists requires a SELECT expression")),
        }
    }

    /// `EXISTS(SELECT 1 FROM junction WHERE junction.source = alias.id)` for a multi-link.
    /// Build the `IrSelect` over the junction/FK-target rows for a multilink,
    /// correlated to the current row (`alias.id`) — shared by `exists
    /// .multilink` and `count(.multilink)`.
    /// `EXISTS(SELECT 1 FROM junction WHERE junction.<source_col> = alias.id)`,
    /// correlated to the current row — shared by a multi-link's own `exists
    /// .multilink`/`count(.multilink)` and a junction-backed single link's
    /// `exists .link` (D2: same junction-info resolution either way).
    fn junction_correlation_select(
        &mut self,
        td: &TypeDescriptor,
        alias: &str,
        name: &str,
        target: &str,
        through: &Option<String>,
    ) -> Result<IrSelect, PyQLError> {
        let jt_alias = self.fresh_alias();
        let (jt_table, jt_module, jt_src_col, _, _) = self.junction_info_for(td, name, target, through)?;

        let filter = IrExpr::BinOp(Box::new(IrBinOp {
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
        Ok(IrSelect::schema_bound(
            IrSource {
                poly: None,
                type_name: format!("{}::__jt__", jt_module),
                table: jt_table,
                alias: jt_alias,
            },
            vec![],
            Some(filter),
        ))
    }

    fn multilink_correlation_select(
        &mut self,
        ml_name: &str,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrSelect, PyQLError> {
        let ml = Self::resolve_multilink(td, ml_name).unwrap();
        let (name, target, through) = (ml.name.clone(), ml.target.clone(), ml.through.clone());
        self.junction_correlation_select(td, alias, &name, &target, &through)
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

    /// Same as `compile_multilink_exists_check`, for `exists .link` where
    /// `link` is a junction-backed single link.
    fn compile_junction_link_exists_check(
        &mut self,
        l: &LinkDescriptor,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let (name, target, through) = (l.name.clone(), l.target.clone(), l.through.clone());
        let inner = self.junction_correlation_select(td, alias, &name, &target, &through)?;
        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: IrExpr::Subquery(Box::new(inner)),
        })))
    }

    /// A junction-backed single link's target id, as a scalar correlated
    /// subquery — `(SELECT jt.<target_col> FROM junction AS jt WHERE
    /// jt.<source_col> = alias.id)`. Stands in wherever a plain single
    /// link's `{name}_id` FK column would otherwise be referenced directly
    /// as a scalar uuid expression (bare `.link` in a filter/order-by,
    /// `.link.id`, or correlating `.link.<other prop>`).
    fn junction_target_id_expr(
        &mut self,
        td: &TypeDescriptor,
        l: &LinkDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let (name, target, through) = (l.name.clone(), l.target.clone(), l.through.clone());
        let (jt_table, jt_module, jt_src_col, jt_tgt_col, _) = self.junction_info_for(td, &name, &target, &through)?;
        let jt_alias = self.fresh_alias();
        let filter = IrExpr::BinOp(Box::new(IrBinOp {
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
        let select = IrSelect::schema_bound(
            IrSource {
                poly: None,
                type_name: format!("{}::__jt__", jt_module),
                table: jt_table,
                alias: jt_alias,
            },
            vec![IrShapePointer::Scalar(IrScalarPointer {
                marker_offset: None,
                alias: "target".to_string(),
                column: jt_tgt_col,
                pg_type: "uuid".to_string(),
                tuple_shape: None,
            })],
            Some(filter),
        );
        Ok(IrExpr::Subquery(Box::new(select)))
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
                if let ast::PathStep::Name(n) = &p.steps[0]
                    && let Some(t) = self.cte_types.get(n.as_str())
                {
                    if t.contains("::") {
                        return t.clone(); // object CTE: "default::Person"
                    }
                    if !t.is_empty() {
                        return pg_type_to_pyql(t).to_string(); // scalar CTE: "std::int64"
                    }
                }
                // Bare type name reference
                if let Ok(td) = self.resolve_type(
                    p.steps
                        .first()
                        .and_then(|s| {
                            if let ast::PathStep::Name(n) = s {
                                Some(n.as_str())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(""),
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
                if p.steps.len() == 1
                    && let ast::PathStep::Name(n) = &p.steps[0]
                {
                    // A for-loop variable over values is a scalar; one over
                    // objects stands for a row, and `select c` is a schema-
                    // bound select of its type.
                    if self.for_vars.contains_key(n.as_str()) {
                        return !self.for_var_types.contains_key(n.as_str());
                    }
                    // Scalar CTE: type string has no "::" (object types always do).
                    if self.is_value_binding(n.as_str()) {
                        return true;
                    }
                    // Function parameter — only populated while compiling a
                    // function body (see `compile_fn_body`), so a bare `select a`
                    // where `a` is a param must resolve as the param, not be
                    // misread as a schema-type-name select.
                    if self.fn_params.contains_key(n.as_str()) {
                        return true;
                    }
                }
                false
            }
            Expr::Shape(s) if s.expr.is_some() => false,
            Expr::SubQuery(_) => false,
            Expr::Union(a, b) => self.is_free_result(a) && self.is_free_result(b),
            // `A if cond else B` picks between two *sets*; when those are
            // object sets the result is an object set too, not the scalar an
            // if/else over values gives. An empty branch says nothing about
            // which it is, so it does not get a vote.
            Expr::IfElse(ie) => {
                let empty = |e: &Expr| matches!(e, Expr::Set(items) if items.is_empty());
                match (empty(&ie.if_expr), empty(&ie.else_expr)) {
                    (true, true) => true,
                    (true, false) => self.is_free_result(&ie.else_expr),
                    (false, true) => self.is_free_result(&ie.if_expr),
                    (false, false) => self.is_free_result(&ie.if_expr) || self.is_free_result(&ie.else_expr),
                }
            }
            _ => true,
        }
    }

    /// True when `expr` is a bare 1-step name bound to a free (non-object)
    /// WITH binding — the same "scalar CTE" check `is_free_result` uses for
    /// its `Expr::Path` arm, factored out so `Expr::Shape`'s handling of
    /// "shape applied to a non-object" can reuse it too.
    fn is_free_cte_ref(&self, expr: &Expr) -> bool {
        let Expr::Path(p) = expr else { return false };
        if p.partial || p.steps.len() != 1 {
            return false;
        }
        let ast::PathStep::Name(n) = &p.steps[0] else {
            return false;
        };
        self.is_value_binding(n.as_str())
    }

    // ── FREE SELECT ───────────────────────────────────────────────────────────────

    /// A mutation standing where a value is expected. Postgres cannot run DML
    /// in a value list, so it becomes a data-modifying CTE — which it runs to
    /// completion whether or not the outer query reads it — and what stands
    /// here reads the ids back, so the value still names the rows the
    /// mutation touched.
    fn dml_as_value(&mut self, stmt: &Stmt) -> Result<IrExpr, PyQLError> {
        let (cte_name, type_name) = self.hoist_dml_as_cte(stmt)?;
        // Read back through a subquery rather than naming the CTE's column
        // directly: a free select has no FROM of its own for the reference to
        // resolve against.
        let source = IrSource {
            poly: None,
            type_name,
            table: format!("@cte:{cte_name}"),
            alias: self.fresh_alias(),
        };
        Ok(IrExpr::Subquery(Box::new(IrSelect::schema_bound(
            source,
            // An empty shape would emit the `SELECT 1` an EXISTS wants, which
            // says nothing about which rows the value stands for.
            vec![IrShapePointer::Scalar(IrScalarPointer {
                marker_offset: None,
                alias: "id".to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
                tuple_shape: None,
            })],
            None,
        ))))
    }

    fn collect_union_items(&mut self, expr: &Expr, items: &mut Vec<IrFreeExpr>) -> Result<(), PyQLError> {
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
            // `select { (update A set …), (update B set …) }` — several
            // mutations in one statement. Postgres cannot run DML in a value
            // list, so each becomes a data-modifying CTE, which it runs to
            // completion whether or not the outer query reads it; the item
            // itself reads back the id, so the set still stands for the rows
            // the mutations touched.
            Expr::SubQuery(stmt) if matches!(stmt.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) => {
                items.push(IrFreeExpr::Scalar(self.dml_as_value(stmt.as_ref())?));
            }
            other => {
                items.push(IrFreeExpr::Scalar(self.compile_free_expr(other)?));
            }
        }
        Ok(())
    }

    /// A free object's field holding an object with a shape (`{ device := d
    /// { id } }`). the upstream engine compiles a free shape into a real object type whose
    /// fields are real pointers, so an object field stays an object; without
    /// this it would compile to the object's bare id, which is not what was
    /// asked for.
    fn free_object_link_field(&mut self, expr: &Expr) -> Result<Option<IrExpr>, PyQLError> {
        let Expr::Shape(sh) = expr else {
            return Ok(None);
        };
        let Some(Expr::Path(p)) = sh.expr.as_ref() else {
            return Ok(None);
        };
        if p.partial {
            return Ok(None);
        }
        let rest_steps = p.steps.as_slice();
        let Some(ast::PathStep::Name(name)) = rest_steps.first() else {
            return Ok(None);
        };
        let cte_name = self.cte_object_type(name).map(|_| name.clone());
        let Ok(td) = self.resolve_path_root(name) else {
            return Ok(None);
        };
        // `account := resource.account { id }` — the field holds what a walk
        // off the binding lands on, so it is that walk with the shape on it
        // rather than a plain read of the binding.
        if rest_steps.len() > 1 {
            let synthetic = ast::SelectStmt {
                result: Expr::Path(p.clone()),
                filter: None,
                order_by: vec![],
                offset: None,
                limit: None,
                lock: None,
            };
            let path_select = self.compile_path_select(&synthetic, p, &sh.elements, false)?;
            return Ok(Some(IrExpr::ObjectPathSubquery(Box::new(path_select))));
        }
        let alias = self.fresh_alias();
        let source = IrSource {
            poly: self.poly_fanout_for(&format!("{}::{}", td.module, td.name)),
            type_name: format!("{}::{}", td.module, td.name),
            table: match &cte_name {
                Some(cte) => format!("@cte:{cte}"),
                None => td.table.clone(),
            },
            alias: alias.clone(),
        };
        // Same as reading the binding by name: its own shape may have
        // declared pointers that exist on no type.
        let outer_declared = std::mem::replace(
            &mut self.active_declared_pointers,
            cte_name
                .as_deref()
                .and_then(|n| self.cte_declared_pointers.get(n).cloned())
                .unwrap_or_default(),
        );
        let shape = self.compile_shape(&sh.elements, td, &alias, &td.module);
        self.active_declared_pointers = outer_declared;
        // A for-loop variable holds one row's key, so the select has to be
        // narrowed to it -- unfiltered it would read the whole table.
        let filter = self.for_var_types.contains_key(name).then(|| {
            IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ForVar { name: name.clone() },
            }))
        });
        Ok(Some(IrExpr::ObjectSubquery(Box::new(IrSelect::schema_bound(
            source, shape?, filter,
        )))))
    }

    /// One field of a free object written as a whole SELECT's result. A
    /// mutation is allowed here for the same reason it is allowed as an item
    /// of a free set — see `dml_as_value`.
    fn free_object_field(&mut self, expr: &Expr) -> Result<IrExpr, PyQLError> {
        if let Some(object) = self.free_object_link_field(expr)? {
            return Ok(object);
        }
        match expr {
            Expr::SubQuery(stmt) if matches!(stmt.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) => {
                self.dml_as_value(stmt.as_ref())
            }
            other => self.compile_free_expr(other),
        }
    }

    fn compile_free_select(
        &mut self,
        sel: &ast::SelectStmt,
        result_expr: &Expr,
        distinct: bool,
    ) -> Result<IrSelect, PyQLError> {
        let items: Vec<IrFreeExpr> = match result_expr {
            Expr::Union(_, _) | Expr::Set(_) => {
                let mut union_items = vec![];
                self.collect_union_items(result_expr, &mut union_items)?;
                // Type-check UNION operands: all scalar branches must be in the same type family.
                let mut first: Option<(String, String)> = None; // (pg_type, pyql_name)
                for item in &union_items {
                    if let IrFreeExpr::Scalar(expr) = item
                        && let Some(t) = infer_ir_type(expr)
                    {
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
                union_items
            }
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if self
                        .cte_types
                        .get(n.as_str())
                        .map(|t| !t.contains("::"))
                        .unwrap_or(false)
                    {
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
                            self.type_err("free object field must have a value expression (':= expr')")
                        })?;
                        Ok((name, self.free_object_field(expr)?))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                vec![IrFreeExpr::FreeObject(fields)]
            }
            Expr::Tuple(exprs) => {
                // An element may hold an object with a shape
                // (`select (offering { id }, revision { id })`), which is the
                // same question a free object's field asks.
                let ir = exprs
                    .iter()
                    .map(|e| self.free_object_field(e))
                    .collect::<Result<_, _>>()?;
                vec![IrFreeExpr::Tuple(ir)]
            }
            Expr::NamedTuple(fields) => {
                let ir = fields
                    .iter()
                    .map(|(name, e)| Ok((name.clone(), self.free_object_field(e)?)))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                // jsonb has no member kind for an object, so a tuple holding
                // one is emitted as a composite row instead; the rest stay on
                // the jsonb encoding they have always had.
                let holds_an_object = ir
                    .iter()
                    .any(|(_, e)| matches!(e, IrExpr::ObjectSubquery(_) | IrExpr::ObjectPathSubquery(_)));
                if holds_an_object {
                    vec![IrFreeExpr::NamedTupleRow(ir)]
                } else {
                    vec![IrFreeExpr::Scalar(IrExpr::NamedTuple {
                        fields: ir,
                        is_free_object: false,
                    })]
                }
            }
            other => vec![IrFreeExpr::Scalar(self.compile_free_expr(other)?)],
        };

        let order_by = sel
            .order_by
            .iter()
            .map(|s| self.compile_sort_ctx(s, None))
            .collect::<Result<Vec<_>, _>>()?;

        let offset = sel.offset.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
        let limit = sel.limit.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
        let filter = sel.filter.as_ref().map(|f| self.compile_free_filter(f)).transpose()?;

        Ok(IrSelect {
            rows: items.into_iter().map(IrRowSource::Free).collect(),
            filter,
            order_by,
            offset,
            limit,
            distinct,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        })
    }

    /// The FILTER of a free SELECT, which has no row to apply itself to: it
    /// gates the result the select already produced.
    ///
    /// A condition over a type-rooted path is a *set* of booleans, one per row
    /// of that type, so the result survives when any of them holds — the same
    /// reading PyQL gives it, and the reason for the warning: a filter is
    /// meant to be one boolean, and `any()` says so outright.
    fn compile_free_filter(&mut self, filter: &Expr) -> Result<IrExpr, PyQLError> {
        let Some(root) = self.find_path_root_in_expr(filter) else {
            return self.compile_free_expr(filter);
        };
        let td = self.resolve_type(&root)?;
        let alias = self.fresh_alias();
        let source = IrSource {
            poly: None,
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };
        let condition = self.compile_expr(&Self::rewrite_abs_to_partial(filter.clone(), &root), td, &alias)?;
        self.warnings.push(format!(
            "possibly more than one element returned by an expression in a FILTER clause \
             (every '{root}'); wrap with any() to make intent explicit",
        ));
        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(source, vec![], Some(condition)))),
        })))
    }

    // ── SELECT ────────────────────────────────────────────────────────────────────

    /// The `(qualified type, source table)` of each branch of an object-set
    /// union, or `None` if any branch is something other than a direct
    /// reference to an object set (a WITH binding or a bare type name).
    fn object_union_branches(&self, expr: &Expr) -> Option<Vec<(String, String)>> {
        fn flatten<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
            match expr {
                Expr::Union(a, b) => {
                    flatten(a, out);
                    flatten(b, out);
                }
                other => out.push(other),
            }
        }
        if !matches!(expr, Expr::Union(_, _)) {
            return None;
        }
        let mut operands: Vec<&Expr> = vec![];
        flatten(expr, &mut operands);

        let mut branches: Vec<(String, String)> = vec![];
        for operand in operands {
            let Expr::Path(path) = operand else { return None };
            if path.partial || path.steps.len() != 1 {
                return None;
            }
            let ast::PathStep::Name(name) = &path.steps[0] else {
                return None;
            };
            match self.cte_object_type(name) {
                Some(qualified) => branches.push((qualified, format!("@cte:{name}"))),
                None => match self.resolve_type(name) {
                    Ok(td) => branches.push((format!("{}::{}", td.module, td.name), td.table.clone())),
                    Err(_) => return None,
                },
            }
        }
        Some(branches)
    }

    /// `select (a union b) { shape }` where every branch is a set of the same
    /// object type — a WITH binding or a bare type name. Each branch becomes
    /// its own bound row; the SQL layer unions them in the FROM clause, so the
    /// shape and modifiers are compiled once, against the shared type.
    ///
    /// `Ok(None)` when the result isn't an object union, leaving the ordinary
    /// single-source path (and its own errors) in charge.
    /// Give every operand of an object union a name to be read by.
    ///
    /// `object_union_branches` recognises operands that already name a
    /// relation -- a type, or a `with` binding -- because a union is emitted
    /// as one relation per branch. An operand written inline (a sub-select, or
    /// an insert in a get-or-create) names nothing yet, so it is hoisted into
    /// a CTE of its own and replaced by that name, leaving an ordinary union
    /// of names behind.
    fn name_union_operands(&mut self, expr: &Expr) -> Result<Option<Expr>, PyQLError> {
        let Expr::Union(left, right) = expr else {
            return Ok(None);
        };
        let mut rewritten = false;
        let mut name_one = |compiler: &mut Self, operand: &Expr| -> Result<Expr, PyQLError> {
            match operand {
                Expr::Union(_, _) => match compiler.name_union_operands(operand)? {
                    Some(inner) => {
                        rewritten = true;
                        Ok(inner)
                    }
                    None => Ok(operand.clone()),
                },
                Expr::SubQuery(stmt) => {
                    let (cte_name, type_name) = compiler.hoist_dml_as_cte(stmt.as_ref())?;
                    // An insert names its subject unqualified (`Credentials`),
                    // a select yields an already-qualified name; a branch that
                    // resolves to no object type is not one of these at all.
                    let Ok(td) = compiler.resolve_type(&type_name) else {
                        return Ok(operand.clone());
                    };
                    compiler
                        .cte_types
                        .insert(cte_name.clone(), format!("{}::{}", td.module, td.name));
                    rewritten = true;
                    Ok(Expr::Path(ast::Path {
                        steps: vec![ast::PathStep::Name(cte_name)],
                        partial: false,
                    }))
                }
                other => Ok(other.clone()),
            }
        };
        let left = name_one(self, left)?;
        let right = name_one(self, right)?;
        if !rewritten {
            return Ok(None);
        }
        Ok(Some(Expr::Union(Box::new(left), Box::new(right))))
    }

    /// `A if cond else B` over object sets, as the union it is.
    ///
    /// Picking between two *sets* is not the scalar `CASE` an if/else over
    /// values compiles to: it stands for A's rows when the condition holds and
    /// B's when it does not, which is `(A filter cond) union (B filter not
    /// cond)`. Rewritten here rather than given its own IR, so it reaches the
    /// union machinery that already knows how to read one relation per branch.
    fn object_if_else_as_union(&mut self, expr: &Expr) -> Option<Expr> {
        let Expr::IfElse(ie) = expr else {
            return None;
        };
        fn is_empty_set(expr: &Expr) -> bool {
            matches!(expr, Expr::Set(items) if items.is_empty())
        }
        let yields_objects = |branch: &Expr| match branch {
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => match &p.steps[0] {
                ast::PathStep::Name(n) => self.cte_object_type(n).is_some() || self.resolve_type(n).is_ok(),
                _ => false,
            },
            Expr::SubQuery(stmt) => {
                // A select over a walk (`select resource.revisions`) yields
                // objects too, and names no type for `dml_subject_type` to
                // report; what it lands on is what the branch carries.
                if let Some(ast::SelectStmt {
                    result: Expr::Path(path),
                    ..
                }) = innermost_select(stmt.as_ref())
                    && !path.partial
                    && path.steps.len() > 1
                    && let Some(ast::PathStep::Name(root)) = path.steps.first()
                    && let Ok(root_td) = self.resolve_path_root(root)
                {
                    return matches!(
                        self.walk_path_types(root_td, &path.steps[1..], MAX_COMPUTED_SPLICES),
                        (_, Some(_))
                    );
                }
                matches!(
                    self.dml_subject_type(stmt.as_ref()),
                    Ok(name) if self.resolve_type(&name).is_ok()
                )
            }
            // A branch may carry the shape the result is read with.
            Expr::Shape(sh) => match sh.expr.as_ref() {
                Some(Expr::SubQuery(stmt)) => {
                    matches!(self.dml_subject_type(stmt.as_ref()), Ok(name) if self.resolve_type(&name).is_ok())
                }
                Some(Expr::Path(p)) if !p.partial && p.steps.len() == 1 => match &p.steps[0] {
                    ast::PathStep::Name(n) => self.cte_object_type(n).is_some() || self.resolve_type(n).is_ok(),
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        };
        // `{}` contributes no rows, so the union degenerates to the other
        // branch under its own guard -- which is what the upstream engine's own rewrite
        // (`SELECT A WHERE Cond UNION ALL SELECT B WHERE NOT Cond`) reduces
        // to when one side is empty.
        // A mutation under a condition is *not* handled here. Guarding the
        // branch only filters what is read back: the mutation is its own CTE
        // and Postgres runs it regardless, so `(insert …) if cond else {}`
        // would insert even when the condition is false -- silently, which is
        // far worse than the compile error it replaced. It needs the
        // condition folded into the mutation itself.
        fn mutating_stmt(branch: &Expr) -> Option<&Stmt> {
            match branch {
                Expr::SubQuery(stmt) => Some(stmt.as_ref()),
                Expr::Shape(sh) => match sh.expr.as_ref() {
                    Some(Expr::SubQuery(stmt)) => Some(stmt.as_ref()),
                    _ => None,
                },
                _ => None,
            }
            .filter(|stmt| matches!(stmt, Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)))
        }
        // Guarding a branch only filters what is read back, while the
        // mutation is its own CTE that Postgres runs regardless -- so the
        // condition has to go into the mutation itself. An insert can take
        // one; an update or delete has nowhere to put it yet, so those stay
        // refused rather than compiling to something unconditional.
        for branch in [&ie.if_expr, &ie.else_expr] {
            if let Some(stmt) = mutating_stmt(branch)
                && !matches!(stmt, Stmt::Insert(_))
            {
                return None;
            }
        }
        let guarded_insert = |branch: &Expr, other: &Expr| {
            mutating_stmt(branch).is_some() && matches!(other, Expr::Set(items) if items.is_empty())
        };
        if guarded_insert(&ie.if_expr, &ie.else_expr) {
            self.pending_insert_guard = Some(ie.condition.clone());
            return Some(ie.if_expr.clone());
        }
        if guarded_insert(&ie.else_expr, &ie.if_expr) {
            self.pending_insert_guard = Some(Expr::UnaryOp(Box::new(ast::UnaryOp {
                op: ast::UnaryOpKind::Not,
                operand: ie.condition.clone(),
            })));
            return Some(ie.else_expr.clone());
        }
        if mutating_stmt(&ie.if_expr).is_some() || mutating_stmt(&ie.else_expr).is_some() {
            return None;
        }
        let (if_empty, else_empty) = (is_empty_set(&ie.if_expr), is_empty_set(&ie.else_expr));
        if if_empty && else_empty {
            return None;
        }
        if !(if_empty || yields_objects(&ie.if_expr)) || !(else_empty || yields_objects(&ie.else_expr)) {
            return None;
        }
        let guard = |branch: &Expr, condition: Expr| {
            Expr::SubQuery(Box::new(Stmt::Select(ast::SelectStmt {
                result: branch.clone(),
                filter: Some(condition),
                order_by: vec![],
                offset: None,
                limit: None,
                lock: None,
            })))
        };
        let negated = Expr::UnaryOp(Box::new(ast::UnaryOp {
            op: ast::UnaryOpKind::Not,
            operand: ie.condition.clone(),
        }));
        if else_empty {
            return Some(guard(&ie.if_expr, ie.condition.clone()));
        }
        if if_empty {
            return Some(guard(&ie.else_expr, negated));
        }
        Some(Expr::Union(
            Box::new(guard(&ie.if_expr, ie.condition.clone())),
            Box::new(guard(&ie.else_expr, negated)),
        ))
    }

    fn try_compile_object_union_select(
        &mut self,
        sel: &ast::SelectStmt,
        result_expr: &Expr,
        distinct: bool,
    ) -> Result<Option<IrSelect>, PyQLError> {
        let as_union;
        let (union_expr, shape_elements): (&Expr, &[ShapeElement]) = match result_expr {
            Expr::Union(_, _) => (result_expr, &[]),
            Expr::IfElse(_) => match self.object_if_else_as_union(result_expr) {
                // One branch empty leaves a single guarded set, not a union;
                // that is an ordinary select over what the branch names.
                Some(rewritten @ Expr::SubQuery(_)) => {
                    return self.compile_select(sel, &rewritten, distinct).map(Some);
                }
                Some(rewritten) if !matches!(rewritten, Expr::Union(_, _)) => {
                    return self.compile_select(sel, &rewritten, distinct).map(Some);
                }
                Some(rewritten) => {
                    as_union = rewritten;
                    (&as_union, &[] as &[ShapeElement])
                }
                None => return Ok(None),
            },
            Expr::Shape(shape) => match &shape.expr {
                Some(inner @ Expr::Union(_, _)) => (inner, shape.elements.as_slice()),
                Some(inner @ Expr::IfElse(_)) => match self.object_if_else_as_union(inner) {
                    Some(Expr::SubQuery(stmt)) => {
                        let shaped = Expr::Shape(Box::new(ast::ShapeExpr {
                            expr: Some(Expr::SubQuery(stmt)),
                            elements: shape.elements.clone(),
                            marker_offset: None,
                        }));
                        return self.compile_select(sel, &shaped, distinct).map(Some);
                    }
                    Some(rewritten) if !matches!(rewritten, Expr::Union(_, _)) => {
                        let shaped = match rewritten {
                            Expr::Shape(_) => rewritten,
                            other => Expr::Shape(Box::new(ast::ShapeExpr {
                                expr: Some(other),
                                elements: shape.elements.clone(),
                                marker_offset: None,
                            })),
                        };
                        return self.compile_select(sel, &shaped, distinct).map(Some);
                    }
                    Some(rewritten) => {
                        as_union = rewritten;
                        (&as_union, shape.elements.as_slice())
                    }
                    None => return Ok(None),
                },
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        let named;
        let union_expr = match self.name_union_operands(union_expr)? {
            Some(rewritten) => {
                named = rewritten;
                &named
            }
            None => union_expr,
        };
        let Some(branches) = self.object_union_branches(union_expr) else {
            return Ok(None);
        };

        let (first_type, _) = &branches[0];
        if let Some((other, _)) = branches.iter().find(|(t, _)| t != first_type) {
            return Err(self.type_err(&format!(
                "operator 'UNION' cannot be applied to operands of type '{first_type}' and '{other}'"
            )));
        }
        if sel.lock.is_some() {
            return Err(self.type_err(
                "FOR UPDATE/SHARE cannot be used on a UNION — its rows come from more than one \
                 source, which a single locking clause can't target",
            ));
        }
        let td = self.resolve_type(first_type)?;
        let alias = self.fresh_alias();

        self.anchors.push(SelectAnchor {
            type_name: td.name.clone(),
            qualified: format!("{}::{}", td.module, td.name),
            alias: alias.clone(),
            detached: std::mem::take(&mut self.pending_detached),
        });
        let clauses = (|compiler: &mut Self| -> Result<_, PyQLError> {
            let shape = compiler.compile_shape(shape_elements, td, &alias, &td.module)?;
            let filter = sel
                .filter
                .as_ref()
                .map(|f| compiler.compile_expr(f, td, &alias))
                .transpose()?;
            let order_by = sel
                .order_by
                .iter()
                .map(|o| compiler.compile_sort(o, td, &alias))
                .collect::<Result<Vec<_>, _>>()?;
            let offset = sel
                .offset
                .as_ref()
                .map(|e| compiler.compile_expr(e, td, &alias))
                .transpose()?;
            let limit = sel
                .limit
                .as_ref()
                .map(|e| compiler.compile_expr(e, td, &alias))
                .transpose()?;
            Ok((shape, filter, order_by, offset, limit))
        })(self);
        self.anchors.pop();
        let (shape, mut filter, order_by, offset, limit) = clauses?;
        // A select whose subject is a for-loop variable reads the one row that
        // variable holds, not the whole table.
        if let Some(var) = Self::subject_name(result_expr).filter(|n| self.for_var_types.contains_key(n)) {
            let narrowed = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ForVar { name: var },
            }));
            filter = and_conditions(filter, vec![narrowed]);
        }

        let rows = branches
            .into_iter()
            .map(|(type_name, table)| IrRowSource::Bound {
                source: IrSource {
                    poly: None,
                    type_name,
                    table,
                    alias: alias.clone(),
                },
                shape: shape.clone(),
            })
            .collect();

        Ok(Some(IrSelect {
            rows,
            filter,
            order_by,
            offset,
            limit,
            distinct,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        }))
    }

    /// The bare name a select's result is written as, whether it carries a
    /// shape or not.
    fn subject_name(result_expr: &Expr) -> Option<String> {
        let path = match result_expr {
            Expr::Path(p) => p,
            Expr::Shape(sh) => match sh.expr.as_ref()? {
                Expr::Path(p) => p,
                _ => return None,
            },
            _ => return None,
        };
        if path.partial {
            return None;
        }
        match path.steps.as_slice() {
            [ast::PathStep::Name(n)] => Some(n.clone()),
            _ => None,
        }
    }

    fn compile_select(
        &mut self,
        sel: &ast::SelectStmt,
        result_expr: &Expr,
        distinct: bool,
    ) -> Result<IrSelect, PyQLError> {
        if let Some(union_select) = self.try_compile_object_union_select(sel, result_expr, distinct)? {
            return Ok(union_select);
        }
        let (type_name, shape_elements, inner_stmt, cte_name) = self.extract_type_and_shape(result_expr)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let table = match cte_name {
            Some(ref cte) => format!("@cte:{}", cte),
            None => td.table.clone(),
        };
        let source = IrSource {
            poly: None,
            type_name: format!("{}::{}", td.module, td.name),
            table,
            alias: alias.clone(),
        };

        // On the stack for as long as this select's own clauses are being
        // compiled, so a `detached` select nested in one of them can find the
        // row it is being compared against. See `enclosing_anchor`.
        self.anchors.push(SelectAnchor {
            type_name: td.name.clone(),
            qualified: format!("{}::{}", td.module, td.name),
            alias: alias.clone(),
            detached: std::mem::take(&mut self.pending_detached),
        });
        // A binding read back by name brings whatever its own shape declared
        // (`offering { publisher }`), which is on no type and has to be found
        // through the binding it was written on.
        let outer_declared = std::mem::replace(
            &mut self.active_declared_pointers,
            cte_name
                .as_deref()
                .and_then(|name| self.cte_declared_pointers.get(name).cloned())
                .unwrap_or_default(),
        );
        let clauses = (|compiler: &mut Self| -> Result<_, PyQLError> {
            let shape = compiler.compile_shape(shape_elements, td, &alias, &td.module)?;
            let filter = sel
                .filter
                .as_ref()
                .map(|f| compiler.compile_expr(f, td, &alias))
                .transpose()?;
            let order_by = sel
                .order_by
                .iter()
                .map(|s| compiler.compile_sort(s, td, &alias))
                .collect::<Result<Vec<_>, _>>()?;
            let offset = sel
                .offset
                .as_ref()
                .map(|e| compiler.compile_expr(e, td, &alias))
                .transpose()?;
            let limit = sel
                .limit
                .as_ref()
                .map(|e| compiler.compile_expr(e, td, &alias))
                .transpose()?;
            Ok((shape, filter, order_by, offset, limit))
        })(self);
        self.anchors.pop();
        self.active_declared_pointers = outer_declared;
        let (shape, filter, order_by, offset, limit) = clauses?;

        // Compile the inner DML if this is a SELECT-over-DML / SELECT-over-SELECT.
        let dml_source = inner_stmt.map(|s| self.compile_stmt(s).map(Box::new)).transpose()?;
        let mut shape = shape;
        if let Some(dml) = &dml_source {
            // `SELECT (INSERT …)` has no user-written CTE name: the emitter
            // wraps the DML in one of its own (`sql::DML_CTE`), and the
            // junction CTEs are named after it.
            let dml_cte = cte_name.as_deref().unwrap_or(crate::sql::DML_CTE);
            Self::read_nested_links_from_their_ctes(dml, Some(dml_cte), &mut shape);
        }

        let polymorphic = td.abstract_ && td.materialized;
        let (poly_implementors, poly_columns) = if polymorphic {
            let iface_qname = format!("{}::{}", td.module, td.name);
            (self.find_poly_implementors(&iface_qname), Self::poly_dml_columns(td))
        } else {
            (vec![], vec![])
        };

        // `FOR UPDATE`/`FOR SHARE`/... — only valid when every output row
        // maps 1:1 to a single physical table row, the same restriction
        // Postgres itself enforces (it rejects the same combinations with
        // its own "FOR UPDATE is not allowed with ..." errors). `DISTINCT`
        // and an interface (polymorphic) target both break that mapping;
        // `SELECT (INSERT/UPDATE/DELETE …) { ... }` has nothing left to
        // lock, since the DML already ran by the time this SELECT reads it.
        let lock = match &sel.lock {
            None => None,
            Some(lc) => {
                if distinct {
                    return Err(self.type_err(
                        "FOR UPDATE/SHARE cannot be combined with DISTINCT — Postgres can't \
                         guarantee the result rows map 1:1 to physical table rows",
                    ));
                }
                if polymorphic {
                    return Err(self.type_err(
                        "FOR UPDATE/SHARE cannot be used on an interface type — its rows span \
                         multiple underlying tables, which a single locking clause can't target",
                    ));
                }
                if dml_source.is_some() {
                    return Err(self.type_err(
                        "FOR UPDATE/SHARE cannot be used on SELECT (INSERT/UPDATE/DELETE …) — \
                         there's nothing left to lock once the DML has already run",
                    ));
                }
                Some(IrLockClause {
                    strength: match lc.strength {
                        ast::LockStrength::Update => IrLockStrength::Update,
                        ast::LockStrength::NoKeyUpdate => IrLockStrength::NoKeyUpdate,
                        ast::LockStrength::Share => IrLockStrength::Share,
                        ast::LockStrength::KeyShare => IrLockStrength::KeyShare,
                    },
                    wait: match lc.wait {
                        ast::LockWait::Block => IrLockWait::Block,
                        ast::LockWait::NoWait => IrLockWait::NoWait,
                        ast::LockWait::SkipLocked => IrLockWait::SkipLocked,
                    },
                })
            }
        };

        Ok(IrSelect {
            rows: vec![IrRowSource::Bound { source, shape }],
            filter,
            order_by,
            offset,
            limit,
            distinct,
            dml_source,
            polymorphic,
            poly_implementors,
            poly_columns,
            lock,
        })
    }

    /// Unwrap `Shape(expr, elements)` or bare `Path` from a SELECT result.
    /// Returns (type_name, shape_elements, optional_inner_stmt, optional_cte_name).
    /// The inner stmt is Some when the subject is `(INSERT …)` / `(SELECT …)` etc.
    /// The cte_name is Some when the subject is a WITH-block CTE reference.
    fn extract_type_and_shape<'e>(&self, expr: &'e Expr) -> Result<TypeAndShape<'e>, PyQLError> {
        match expr {
            Expr::Shape(s) => {
                // s.expr is Option<Expr> (not Box), so use as_ref() not as_deref()
                let (type_name, cte_name, inner) = match s.expr.as_ref() {
                    Some(Expr::SubQuery(stmt)) => (self.dml_subject_type(stmt)?, None, Some(stmt.as_ref())),
                    Some(Expr::Path(p)) if !p.partial && p.steps.len() == 1 => {
                        if let ast::PathStep::Name(n) = &p.steps[0] {
                            if let Some(t) = self.for_var_types.get(n.as_str()) {
                                (t.clone(), None, None)
                            } else if let Some(t) = self.cte_types.get(n.as_str()) {
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
                        }));
                    }
                };
                Ok((type_name, &s.elements, inner, cte_name))
            }
            // Bare `SELECT (DML)` without an outer shape
            Expr::SubQuery(stmt) => Ok((self.dml_subject_type(stmt)?, &[], Some(stmt.as_ref()), None)),
            // Bare CTE object reference: `select cte_name`
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    if let Some(t) = self.for_var_types.get(n.as_str()) {
                        return Ok((t.clone(), &[], None, None));
                    }
                    if let Some(t) = self.cte_types.get(n.as_str())
                        && t.contains("::")
                    {
                        return Ok((t.clone(), &[], None, Some(n.clone())));
                    }
                }
                Ok((self.expr_as_type_name(expr)?, &[], None, None))
            }
            _ => Ok((self.expr_as_type_name(expr)?, &[], None, None)),
        }
    }

    fn compile_for(&mut self, f: &ast::ForStmt) -> Result<IrFor, PyQLError> {
        // Compile the iterator to determine what one loop variable binds to.
        let (iterator, pg_type, yielded_object_type) = match &f.iterator {
            Expr::Set(elems) => {
                let compiled: Result<Vec<_>, _> = elems.iter().map(|e| self.compile_free_expr(e)).collect();
                let exprs = compiled?;
                let raw = exprs.first().and_then(|e| infer_ir_type(e)).unwrap_or("text");
                let pg_type = literal_sentinel_to_pg(raw).to_string();
                (
                    IrForIterator::Values {
                        exprs,
                        pg_type: pg_type.clone(),
                    },
                    pg_type,
                    None,
                )
            }
            // A derived set — every row of it, not the single value a scalar
            // subquery in a one-row `VALUES` would collapse it to.
            Expr::SubQuery(stmt) => {
                let inner = self.compile_stmt(stmt)?;
                if !matches!(inner, IrStmt::Select(_) | IrStmt::PathSelect(_)) {
                    return Err(self.type_err(
                        "for-loop iterator: only a select can be iterated over — \
                         bind the statement in a `with` first",
                    ));
                }
                let yielded = cte_stmt_type(&inner);
                let scalar = !yielded.contains("::");
                let pg_type = if scalar {
                    let raw = if yielded.is_empty() { "text" } else { yielded.as_str() };
                    literal_sentinel_to_pg(raw).to_string()
                } else {
                    "uuid".to_string()
                };
                (
                    IrForIterator::Query {
                        stmt: Box::new(inner),
                        scalar,
                    },
                    pg_type,
                    (!scalar).then_some(yielded),
                )
            }
            // A WITH binding names a set, so the loop runs once per row of
            // it — not once over the single value a scalar subquery would
            // collapse it to.
            Expr::Path(p)
                if !p.partial
                    && p.steps.len() == 1
                    && matches!(&p.steps[0], ast::PathStep::Name(n) if self.cte_types.contains_key(n.as_str())) =>
            {
                let ast::PathStep::Name(name) = &p.steps[0] else {
                    unreachable!("checked by the guard")
                };
                let yielded = self.cte_types.get(name.as_str()).cloned().unwrap_or_default();
                let scalar = !yielded.contains("::");
                let pg_type = if scalar {
                    let raw = if yielded.is_empty() { "text" } else { yielded.as_str() };
                    literal_sentinel_to_pg(raw).to_string()
                } else {
                    "uuid".to_string()
                };
                let source = IrSource {
                    poly: None,
                    type_name: yielded.clone(),
                    table: format!("@cte:{name}"),
                    alias: self.fresh_alias(),
                };
                (
                    IrForIterator::Query {
                        stmt: Box::new(IrStmt::Select(IrSelect::schema_bound(source, vec![], None))),
                        scalar,
                    },
                    pg_type,
                    (!scalar).then_some(yielded),
                )
            }
            // `for o in invitations.organizations union (…)` — a walk off a
            // binding names a set to iterate just as a sub-select does; it is
            // only written without the parentheses.
            Expr::Path(p) if !p.partial && p.steps.len() > 1 => {
                let synthetic = ast::SelectStmt {
                    result: Expr::Path(p.clone()),
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    lock: None,
                };
                let inner = self.compile_stmt(&Stmt::Select(synthetic))?;
                let yielded = cte_stmt_type(&inner);
                let scalar = !yielded.contains("::");
                let pg_type = if scalar {
                    let raw = if yielded.is_empty() { "text" } else { yielded.as_str() };
                    literal_sentinel_to_pg(raw).to_string()
                } else {
                    "uuid".to_string()
                };
                (
                    IrForIterator::Query {
                        stmt: Box::new(inner),
                        scalar,
                    },
                    pg_type,
                    (!scalar).then_some(yielded),
                )
            }
            other => {
                let e = self.compile_free_expr(other)?;
                let raw = infer_ir_type(&e).unwrap_or("text");
                let pg_type = literal_sentinel_to_pg(raw).to_string();
                (
                    IrForIterator::Values {
                        exprs: vec![e],
                        pg_type: pg_type.clone(),
                    },
                    pg_type,
                    None,
                )
            }
        };

        // Register the for variable so the body can reference it.
        let prev = self.for_vars.insert(f.var.clone(), pg_type.clone());
        let prev_type = match yielded_object_type {
            Some(qualified) => self.for_var_types.insert(f.var.clone(), qualified),
            None => self.for_var_types.remove(&f.var),
        };
        let hoisted_before = self.hoisted_ctes.len();
        let body = self.compile_stmt(&f.body)?;
        let body_ctes: Vec<IrCteDef> = self.hoisted_ctes.split_off(hoisted_before);
        // Restore previous for-var (or remove if none existed).
        match prev {
            Some(old) => {
                self.for_vars.insert(f.var.clone(), old);
            }
            None => {
                self.for_vars.remove(&f.var);
            }
        }
        match prev_type {
            Some(old) => {
                self.for_var_types.insert(f.var.clone(), old);
            }
            None => {
                self.for_var_types.remove(&f.var);
            }
        }

        // Checked here rather than left to the SQL emitter: `emit_for_stmt`
        // only implements these three body kinds, and reaching it with any
        // other one used to abort the process instead of reporting a PyQL
        // error the caller could act on.
        let body_kind = match &body {
            IrStmt::Insert(_) | IrStmt::Select(_) | IrStmt::PathSelect(_) => None,
            // An update is driven from the iteration itself (`UPDATE … FROM
            // <iterated set>`) rather than from a LATERAL, which cannot hold
            // DML. A multi-link mutation inside one would need its junction
            // rows driven from the iteration too, which it is not yet.
            IrStmt::Update(upd)
                if upd.multi_link_appends.is_empty()
                    && upd.multi_link_clears.is_empty()
                    && upd.multi_link_replaces.is_empty()
                    && upd.multi_link_removals.is_empty()
                    && upd.poly_implementors.is_empty() =>
            {
                None
            }
            IrStmt::Update(_) => Some("update"),
            IrStmt::Delete(_) => Some("delete"),
            IrStmt::For(_) => Some("nested for"),
            IrStmt::Group(_) => Some("group"),
            _ => Some("this statement"),
        };
        if let Some(kind) = body_kind {
            return Err(self.type_err(&format!(
                "for-loop body: {kind} is not supported as a `for` body — use insert or select"
            )));
        }

        Ok(IrFor {
            var_name: f.var.clone(),
            iterator,
            body: Box::new(body),
            body_ctes,
        })
    }

    fn compile_group(&mut self, g: &ast::GroupStmt) -> Result<IrGroup, PyQLError> {
        // Resolve the subject type — may be a schema type or a CTE alias.
        let (type_name, cte_name) = match &g.subject {
            Expr::Path(p) if !p.partial => match p.steps.as_slice() {
                [ast::PathStep::Name(n)] => {
                    if let Some(t) = self.cte_types.get(n.as_str()) {
                        (t.clone(), Some(n.clone()))
                    } else {
                        (n.clone(), None)
                    }
                }
                [ast::PathStep::Name(m), ast::PathStep::Name(n)] => (format!("{}::{}", m, n), None),
                _ => {
                    return Err(PyQLError::Type(PyQLTypeError {
                        message: format!("unsupported group subject: {:?}", g.subject),
                        position: Position { line: 0, col: 0 },
                    }));
                }
            },
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "group subject must be a type name".to_string(),
                    position: Position { line: 0, col: 0 },
                }));
            }
        };

        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let fq_type_name = format!("{}::{}", td.module, td.name);
        let table = match cte_name {
            Some(ref cte) => format!("@cte:{}", cte),
            None => td.table.clone(),
        };
        let source = IrSource {
            poly: None,
            type_name: fq_type_name.clone(),
            table,
            alias: alias.clone(),
        };
        let module = td.module.clone();

        // Compile the element shape. No explicit shape → implicit { id }.
        let shape = self.compile_shape(g.shape.as_deref().unwrap_or(&[]), td, &alias, &module)?;

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
                        let ir = using_map.get(name).ok_or_else(|| {
                            PyQLError::Type(PyQLTypeError {
                                message: format!("group by references unknown alias '{}'", name),
                                position: Position { line: 0, col: 0 },
                            })
                        })?;
                        keys.push((name.clone(), ir.clone()));
                    } else {
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: "group by identifier must be a simple name".to_string(),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                }
                _ => {
                    return Err(PyQLError::Type(PyQLTypeError {
                        message: format!("unsupported group by expression: {:?}", by_expr),
                        position: Position { line: 0, col: 0 },
                    }));
                }
            }
        }

        // `filter` restricts the grouped rows; `order by`/`offset`/`limit`
        // apply within each group, so they're compiled against the same
        // element scope the shape is.
        let td = self.resolve_type(&type_name)?;
        let synthetic = ast::SelectStmt {
            result: g.subject.clone(),
            filter: g.filter.clone(),
            order_by: g.order_by.clone(),
            offset: g.offset.clone(),
            limit: g.limit.clone(),
            lock: None,
        };
        let (filter, order_by, offset, limit) = self.compile_path_modifiers(&synthetic, td, &alias)?;

        Ok(IrGroup {
            source,
            shape,
            keys,
            filter,
            order_by,
            offset,
            limit,
        })
    }

    /// Extract the target type name from a DML or inner SELECT statement.
    fn dml_subject_type(&self, stmt: &Stmt) -> Result<String, PyQLError> {
        match stmt {
            Stmt::Insert(ins) => Ok(ins.subject.name.clone()),
            Stmt::Update(upd) => self.subject_type_name(&upd.subject),
            Stmt::Delete(del) => self.subject_type_name(&del.subject),
            Stmt::With(w) => self.dml_subject_type(&w.stmt),
            Stmt::For(f) => self.dml_subject_type(&f.body),
            Stmt::Analyze(inner) => self.dml_subject_type(inner),
            Stmt::Group(g) => self.expr_as_type_name(&g.subject),
            Stmt::Select(sel) => {
                // <Module::Type>expr — type name comes from the cast target
                if let Expr::TypeCast(tc) = &sel.result
                    && let Some((module, name)) = tc.ty.as_named()
                    && module.map(|m| m != "std").unwrap_or(false)
                {
                    return Ok(name.to_string());
                }
                // `select resource.revisions` — a walk names no type of its
                // own; what it lands on is the type the statement yields.
                if let Expr::Path(path) = &sel.result
                    && !path.partial
                    && path.steps.len() > 1
                    && let Some(ast::PathStep::Name(root)) = path.steps.first()
                    && let Ok(root_td) = self.resolve_path_root(root)
                    && let (_, Some(target)) = self.walk_path_types(root_td, &path.steps[1..], MAX_COMPUTED_SPLICES)
                {
                    return Ok(format!("{}::{}", target.module, target.name));
                }
                // SELECT-over-SELECT: get the type from the inner select's result
                let (type_name, _, _, _) = self.extract_type_and_shape(&sel.result)?;
                Ok(type_name)
            }
        }
    }

    /// `id in (…)` over the rows a DML subject path traverses to, so a
    /// mutation written against a traversal touches those rows and no others.
    fn compile_subject_path_rows(&mut self, subject: &ast::Path, target_alias: &str) -> Result<IrExpr, PyQLError> {
        // Traversing to `.id` rather than stopping at the objects: a path that
        // ends on a link or a type intersection has nothing to project, and the
        // ids are what the narrowing compares against anyway.
        let mut steps = subject.steps.clone();
        steps.push(ast::PathStep::Name("id".to_string()));
        let ids = ast::Path { steps, partial: false };
        let synthetic = ast::SelectStmt {
            result: Expr::Path(ids.clone()),
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            lock: None,
        };
        let rows = self.compile_path_select(&synthetic, &ids, &[], false)?;
        Ok(IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: target_alias.to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::In,
            right: IrExpr::ArrayFromSelect(Box::new(IrArraySource::PathSelect(Box::new(rows)))),
        })))
    }

    /// The type a DML subject names, however it is written: a type, a `with`
    /// binding, or a traversal that ends on one.
    fn subject_type_name(&self, subject: &Expr) -> Result<String, PyQLError> {
        if let Expr::Path(p) = subject
            && p.steps.len() > 1
            && let Some(ast::PathStep::Name(root)) = p.steps.first()
            && let Ok(root_td) = self.resolve_path_root(root)
            && let (_, Some(target)) = self.walk_path_types(root_td, &p.steps[1..], MAX_COMPUTED_SPLICES)
        {
            return Ok(format!("{}::{}", target.module, target.name));
        }
        // A subject that is just a `with` binding names rows, not a type, so
        // the type is the one the binding was bound to. `compile_update`
        // wants the binding's own name (it narrows the update to those rows);
        // a reader asking what type the statement yields wants this.
        if let Expr::Path(p) = subject
            && !p.partial
            && let [ast::PathStep::Name(root)] = p.steps.as_slice()
            && let Some(bound) = self.cte_object_type(root)
        {
            return Ok(bound);
        }
        self.expr_as_type_name(subject)
    }

    fn expr_as_type_name(&self, expr: &Expr) -> Result<String, PyQLError> {
        match expr {
            // `detached T` names the same type; the prefix only says the set
            // is not correlated with the enclosing one.
            Expr::Detached(inner) => self.expr_as_type_name(inner),
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0] {
                    return Ok(n.clone());
                }
                Err(self.type_err("expected a type name"))
            }
            // `select (a union b)` as a SELECT-over-SELECT subject: every
            // branch carries the same object type, and that is the subject.
            Expr::Union(_, _) => {
                let branches = self
                    .object_union_branches(expr)
                    .ok_or_else(|| self.type_err("expected a type name as SELECT subject"))?;
                let (first, _) = &branches[0];
                if branches.iter().all(|(t, _)| t == first) {
                    return Ok(first.clone());
                }
                Err(self.type_err("expected a type name as SELECT subject"))
            }
            _ => Err(self.type_err("expected a type name as SELECT subject")),
        }
    }

    // ── INSERT ────────────────────────────────────────────────────────────────────

    fn compile_insert(&mut self, ins: &ast::InsertStmt) -> Result<IrInsert, PyQLError> {
        // Isolate this insert's own nested-DML discoveries (see
        // `pending_nested_ctes`'s doc comment) from whatever an enclosing
        // compile (e.g. this insert itself being the nested DML inside an
        // *outer* insert's link value) had pending, so each level attaches
        // only its own CTEs to its own `IrInsert`.
        let outer_pending_nested_ctes = std::mem::take(&mut self.pending_nested_ctes);
        let type_name = ins.subject.name.to_string();
        let td = self.resolve_type(&type_name)?;
        if td.abstract_ && td.materialized {
            return Err(self.type_err(&format!(
                "cannot insert into interface type '{}::{}'; insert into a concrete type instead",
                td.module, td.name
            )));
        }
        let alias = self.fresh_alias();
        let target = IrSource {
            poly: None,
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
                Err(_) => {
                    scalar_elements.push(el.clone());
                    continue;
                }
            };
            if let Some(ml) = Self::resolve_multilink(td, pointer_name) {
                match el.op {
                    ShapeOp::Remove => {
                        return Err(self.type_err(&format!(
                            "cannot use `-=` for multi-link '{pointer_name}' in an insert; \
                         there is nothing to remove from yet"
                        )));
                    }
                    ShapeOp::Assign | ShapeOp::Append => {
                        if let Some(expr) = &el.compexpr {
                            let (jt, module, src_col, tgt_col, through_td) = self.multilink_junction_info(td, ml)?;
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            multi_link_appends.push(IrMultiLinkMutation {
                                junction_table: jt,
                                module,
                                source_col: src_col,
                                target_col: tgt_col,
                                values,
                                single: false,
                            });
                        }
                    }
                }
            } else if let Some(l) = Self::resolve_link(td, pointer_name).filter(|l| l.is_junction_backed()) {
                // A junction-backed single link is "a multi-link capped to
                // one row" (D1) — on insert there's nothing to replace yet,
                // so `:=` populates the junction table the same way a
                // multi-link's own `:=`/`+=` does above.
                if matches!(el.op, ShapeOp::Remove) {
                    return Err(self.type_err(&format!(
                        "cannot use `-=` for link '{pointer_name}' in an insert; \
                         there is nothing to remove from yet"
                    )));
                }
                if let Some(expr) = &el.compexpr {
                    // `:= {}` / `:= <Type>{}` at insert time — same as
                    // omitting the pointer entirely: nothing to append, no
                    // junction row created yet.
                    if !is_empty_set_expr(expr) {
                        let (jt, module, src_col, tgt_col, through_td) = self.link_junction_info(td, l)?;
                        let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                        multi_link_appends.push(IrMultiLinkMutation {
                            junction_table: jt,
                            module,
                            source_col: src_col,
                            target_col: tgt_col,
                            values,
                            single: true,
                        });
                    }
                }
            } else {
                scalar_elements.push(el.clone());
            }
        }

        let assignments = self.compile_assignments(&scalar_elements, td, &alias)?;
        // Compile INSERT rewrites; substitute column refs so they are valid in
        // VALUES — a plain `INSERT ... VALUES (...)` has no FROM-clause for a
        // real ColumnRef (`"t0"."name"`) to resolve against (confirmed live:
        // "missing FROM-clause entry for table t0"), unlike UPDATE's SET
        // clause, which can validly reference the table's own alias. For a
        // property with no explicit assignment here, fall back to its own
        // `default_sql` (the same value Postgres's column DEFAULT would
        // produce) rather than leaving its self-reference unsubstituted — a
        // `default_pyql` default isn't covered by this fallback (it would
        // need a full recursive compile at this point, not just a raw-SQL
        // substitution), so a rewrite self-referencing a `default_pyql`
        // property with no explicit assignment still hits the same error.
        let mut assignment_map: HashMap<String, IrExpr> =
            assignments.iter().map(|(c, e)| (c.clone(), e.clone())).collect();
        for prop in &td.properties {
            if !assignment_map.contains_key(&prop.name)
                && let Some(default_sql) = &prop.default_sql
            {
                assignment_map.insert(prop.name.clone(), IrExpr::RawSql(default_sql.clone()));
            }
        }
        let rewrites = self
            .compile_rewrites(td, &alias, 1)?
            .into_iter()
            .map(|rw| IrRewrite {
                column: rw.column,
                expr: substitute_col_refs(rw.expr, &assignment_map),
            })
            .collect();
        let unless_conflict = match ins.unless_conflict.as_ref() {
            Some(uc) => {
                let (conflict, else_appends) = self.compile_conflict(uc, td)?;
                multi_link_appends.extend(else_appends);
                Some(conflict)
            }
            None => None,
        };
        let returning = Self::pk_returning(td);
        let type_name = format!("{}::{}", td.module, td.name);
        let enqueue_vector = td
            .vector_indexes
            .iter()
            .map(|vi| VectorEnqueueInfo {
                type_name: type_name.clone(),
                index_name: vi.index_name.clone(),
            })
            .collect();
        let enqueue_search = collect_search_enqueue(td, &type_name, "index");
        let nested_ctes = std::mem::replace(&mut self.pending_nested_ctes, outer_pending_nested_ctes);

        let guard = match self.pending_insert_guard.take() {
            Some(condition) => Some(self.compile_expr(&condition, td, &alias)?),
            None => None,
        };
        Ok(IrInsert {
            guard,
            target,
            assignments,
            unless_conflict,
            rewrites,
            returning,
            enqueue_vector,
            enqueue_search,
            multi_link_appends,
            nested_ctes,
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
                    // explicit value when allow_user_specified_id is set.
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
                    if l.is_junction_backed() {
                        // Reachable only via `UNLESS CONFLICT ... ELSE (UPDATE
                        // ... SET { ... })` — `compile_insert`/`compile_update`'s
                        // own shape-classification loop intercepts a junction-
                        // backed link before it ever reaches this generic
                        // scalar-assignment path; the ELSE clause has no
                        // junction-mutation mechanism of its own to reuse.
                        return Err(PyQLError::Type(PyQLTypeError {
                            message: format!(
                                "'{pointer_name}' is a junction-backed link and cannot be \
                                 assigned inside an UNLESS CONFLICT ELSE clause"
                            ),
                            position: Position { line: 0, col: 0 },
                        }));
                    }
                    // Link assignment via subquery: `company := (SELECT Company FILTER ...)`
                    // Compile as a scalar subquery returning the target pk (the FK uuid).
                    // A one-element set is that element — `credentials := {
                    // (insert Credentials { … }) }` says the same thing as
                    // assigning the insert directly.
                    let fk_col = format!("{}_id", l.name);
                    let value = match expr {
                        Expr::Set(elements) if elements.len() == 1 => &elements[0],
                        other => other,
                    };
                    if let Expr::SubQuery(inner_stmt) = value {
                        let ir_expr = self.compile_link_subquery(inner_stmt)?;
                        return Ok((fk_col, ir_expr));
                    }
                    // `created_by := account_of_transaction()` — an
                    // object-returning function names the row to link to, so
                    // what is stored is its key, the same as for a select.
                    // The schema itself writes this as a link default.
                    if let Expr::FunctionCall(fc) = value
                        && let Some(ir_expr) =
                            self.try_compile_fn_scalar_subquery(fc, &["id".to_string()], None, None)?
                    {
                        return Ok((fk_col, ir_expr));
                    }
                    fk_col
                } else if Self::resolve_multilink(td, pointer_name).is_some() {
                    // Same reach as the junction-backed link above: the
                    // shape-classification loop in `compile_insert` and
                    // `compile_update` takes multi-links first, so one only
                    // arrives here from an `UNLESS CONFLICT ... ELSE` clause,
                    // which has no junction-mutation mechanism to reuse.
                    // Saying the pointer does not exist sends the reader
                    // looking for a typo in a name that is plainly right.
                    return Err(PyQLError::Type(PyQLTypeError {
                        message: format!(
                            "'{pointer_name}' is a multi-link and cannot be mutated inside an \
                             UNLESS CONFLICT ELSE clause"
                        ),
                        position: Position { line: 0, col: 0 },
                    }));
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
        // See `compile_insert`'s identical save/restore of `pending_nested_ctes`.
        let outer_pending_nested_ctes = std::mem::take(&mut self.pending_nested_ctes);
        // `update account.preferences[is IndividualPreferences] set …` — the
        // subject is a traversal rather than a name, so the rows to update are
        // the ones it lands on: the table is the type the walk ends on, and the
        // update is narrowed to the ids the traversal yields.
        if let Expr::Path(subject) = &upd.subject
            && subject.steps.len() > 1
            && let Some(ast::PathStep::Name(root)) = subject.steps.first()
            && let Ok(root_td) = self.resolve_path_root(root)
            && let (_, Some(target_td)) = self.walk_path_types(root_td, &subject.steps[1..], MAX_COMPUTED_SPLICES)
        {
            let narrowed = ast::UpdateStmt {
                subject: Expr::Path(ast::Path {
                    steps: vec![ast::PathStep::Name(format!("{}::{}", target_td.module, target_td.name))],
                    partial: false,
                }),
                filter: upd.filter.clone(),
                shape: upd.shape.clone(),
            };
            self.pending_nested_ctes = outer_pending_nested_ctes;
            let mut ir = self.compile_update(&narrowed)?;
            // Built after the update, so the comparison names that update's own
            // alias: with a nested statement's CTE in the FROM, a bare `id`
            // could mean either relation.
            let rows = self.compile_subject_path_rows(subject, &ir.target.alias)?;
            ir.filter = Some(and_conditions(ir.filter.take(), vec![rows]).expect("row set is present"));
            return Ok(ir);
        }
        let type_name = self.expr_as_type_name(&upd.subject)?;
        // `update account set …` where `account` is a `with` binding: the
        // binding names the rows to update, so it decides the table *and*
        // narrows the update to its own rows. Without the narrowing this would
        // resolve to the type and rewrite every row in the table.
        let bound_rows = self.cte_object_type(&type_name);
        let td = self.resolve_path_root(&type_name)?;
        let alias = self.fresh_alias();
        let target = IrSource {
            poly: None,
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        let declared_filter = upd
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;
        let filter = match bound_rows {
            Some(_) => {
                let membership = IrExpr::BinOp(Box::new(IrBinOp {
                    left: IrExpr::ColumnRef {
                        alias: alias.clone(),
                        column: "id".to_string(),
                        pg_type: "uuid".to_string(),
                    },
                    op: ast::BinOpKind::In,
                    right: IrExpr::ArrayFromSelect(Box::new(IrArraySource::Select(IrSelect::schema_bound(
                        IrSource {
                            poly: None,
                            type_name: format!("{}::{}", td.module, td.name),
                            table: format!("@cte:{type_name}"),
                            alias: self.fresh_alias(),
                        },
                        vec![],
                        None,
                    )))),
                }));
                Some(and_conditions(declared_filter, vec![membership]).expect("membership is present"))
            }
            None => declared_filter,
        };

        // Classify shape elements by kind.
        let mut multi_link_clears = vec![];
        let mut multi_link_replaces = vec![];
        let mut multi_link_appends = vec![];
        let mut multi_link_removals = vec![];
        let mut scalar_elements: Vec<ShapeElement> = vec![];

        for el in &upd.shape {
            let pointer_name = match path_leaf(&el.path) {
                Ok(n) => n,
                Err(_) => {
                    scalar_elements.push(el.clone());
                    continue;
                }
            };

            if let Some(ml) = Self::resolve_multilink(td, pointer_name) {
                let (jt, module, src_col, tgt_col, through_td) = self.multilink_junction_info(td, ml)?;

                match el.op {
                    ShapeOp::Assign => {
                        let is_empty = el
                            .compexpr
                            .as_ref()
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
                                junction_table: jt,
                                module,
                                source_col: src_col,
                                target_col: tgt_col,
                                values,
                                single: false,
                            });
                        }
                    }
                    ShapeOp::Append => {
                        if let Some(expr) = &el.compexpr {
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            multi_link_appends.push(IrMultiLinkMutation {
                                junction_table: jt,
                                module,
                                source_col: src_col,
                                target_col: tgt_col,
                                values,
                                single: false,
                            });
                        }
                    }
                    ShapeOp::Remove => {
                        if let Some(expr) = &el.compexpr {
                            let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                            if has_any_link_props(&values) {
                                return Err(self.type_err(
                                    "link properties (`@prop := value`) cannot be assigned \
                                     when removing a link (`-=`)",
                                ));
                            }
                            multi_link_removals.push(IrMultiLinkMutation {
                                junction_table: jt,
                                module,
                                source_col: src_col,
                                target_col: tgt_col,
                                values,
                                single: false,
                            });
                        }
                    }
                }
            } else if let Some(l) = Self::resolve_link(td, pointer_name).filter(|l| l.is_junction_backed()) {
                // Same replace-via-clear-then-insert shape a multi-link's
                // own `:=` uses — only `:=` is meaningful for a single
                // link, whether FK-backed or junction-backed.
                if !matches!(el.op, ShapeOp::Assign) {
                    return Err(self.type_err(&format!(
                        "'{pointer_name}' is a single link; only `:=` is supported, not `+=`/`-=`"
                    )));
                }
                let (jt, module, src_col, tgt_col, through_td) = self.link_junction_info(td, l)?;
                let is_empty = el.compexpr.as_ref().map(is_empty_set_expr).unwrap_or(false);
                if is_empty {
                    // := {} — clear the junction row
                    multi_link_clears.push(IrMultiLinkClear {
                        junction_table: jt,
                        module,
                        source_col: src_col,
                    });
                } else if let Some(expr) = &el.compexpr {
                    multi_link_clears.push(IrMultiLinkClear {
                        junction_table: jt.clone(),
                        module: module.clone(),
                        source_col: src_col.clone(),
                    });
                    let values = self.compile_multilink_values(expr, td, &alias, through_td)?;
                    multi_link_replaces.push(IrMultiLinkMutation {
                        junction_table: jt,
                        module,
                        source_col: src_col,
                        target_col: tgt_col,
                        values,
                        single: true,
                    });
                }
            } else {
                scalar_elements.push(el.clone());
            }
        }

        let assignments = self.compile_assignments_for_update(&scalar_elements, td, &alias)?;
        let assignment_map: HashMap<String, IrExpr> = assignments.iter().map(|(c, e)| (c.clone(), e.clone())).collect();
        let rewrites = self
            .compile_rewrites(td, &alias, 2)?
            .into_iter()
            .map(|rw| IrRewrite {
                column: rw.column,
                expr: substitute_col_refs(rw.expr, &assignment_map),
            })
            .collect();
        let returning = Self::pk_returning(td);

        let (poly_implementors, poly_columns) = if td.abstract_ && td.materialized {
            (
                self.find_poly_implementors(&format!("{}::{}", td.module, td.name)),
                Self::poly_dml_columns(td),
            )
        } else {
            (vec![], vec![])
        };

        // Only enqueue indexes whose source pointers are touched by this update.
        let written_cols: std::collections::HashSet<&str> = assignments.iter().map(|(c, _)| c.as_str()).collect();
        let type_name = format!("{}::{}", td.module, td.name);
        let enqueue_vector: Vec<VectorEnqueueInfo> = td
            .vector_indexes
            .iter()
            .filter(|vi| vi.pointers.iter().any(|f| written_cols.contains(f.as_str())))
            .map(|vi| VectorEnqueueInfo {
                type_name: type_name.clone(),
                index_name: vi.index_name.clone(),
            })
            .collect();
        let enqueue_search = collect_search_enqueue(td, &type_name, "index");
        // Nested-DML CTEs (see IrUpdate::nested_ctes) compose with every
        // other UPDATE shape — multi-link mutation, interface-type fan-out,
        // and vector/search enqueue each have their own emitter branch
        // (emit_update_stmt / emit_poly_update_stmt) that now prepends
        // these CTEs and adds the FROM clause a hoisted CTE reference needs.
        let nested_ctes = std::mem::replace(&mut self.pending_nested_ctes, outer_pending_nested_ctes);

        Ok(IrUpdate {
            target,
            filter,
            assignments,
            rewrites,
            returning,
            multi_link_clears,
            multi_link_replaces,
            multi_link_appends,
            multi_link_removals,
            poly_implementors,
            poly_columns,
            enqueue_vector,
            enqueue_search,
            nested_ctes,
        })
    }

    /// Extract junction table info for a multi-link (or a junction-backed
    /// single link, which shares this exact storage shape — D2):
    /// (junction_table, module, source_col, target_col, through_td).
    /// `through_td` is the junction type's own TypeDescriptor for a
    /// `Through[...]` link (needed to validate/compile `@prop := expr`
    /// link-property assignments against its real properties) — `None` for
    /// a Standard (implicit) junction table, which has no user-declared
    /// properties at all.
    fn junction_info_for(
        &mut self,
        td: &TypeDescriptor,
        name: &str,
        target: &str,
        through: &Option<String>,
    ) -> Result<(String, String, String, String, Option<&'a TypeDescriptor>), PyQLError> {
        match through {
            None => Ok((
                format!("{}.{}", td.table, name),
                td.module.clone(),
                "source".to_string(),
                "target".to_string(),
                None,
            )),
            Some(through_qname) => {
                let through_td = self.resolve_type(through_qname)?;
                let src_type = format!("{}::{}", td.module, td.name);
                let source_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == src_type)
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "source".to_string());
                // A self-referencing through-link (source type == target
                // type, e.g. Person.friends via a PersonFriend with two
                // Person-typed links) would otherwise match the same link
                // for both sides — prefer a differently-named one first,
                // matching the tie-break already used for the read-side
                // join resolution elsewhere in this file.
                let target_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == target && l.name != source_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == target))
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "target".to_string());
                // A dedicated `@pylon.junction` type has no physical table
                // of its own — `emit_one_junction_table` always names it
                // `"{owner.table}.{name}"`, one per *owner* (so that e.g.
                // an interface-inherited junction-backed link gets one
                // physically separate table per concrete implementor,
                // never a single table shared — and thus impossibly
                // FK'd — across several). `through_td.table` only holds
                // the right name here by accident, for the common case of
                // exactly one owner ever referencing that junction type
                // (the Python walker pre-renames it for that one owner).
                // A non-junction "through" type, in contrast, *is* a real,
                // independently-queryable object with its own genuine
                // table — `through_td.table` is correct for that case and
                // must stay as-is.
                let (junction_table, junction_module) = if through_td.junction {
                    (format!("{}.{}", td.table, name), td.module.clone())
                } else {
                    (through_td.table.clone(), through_td.module.clone())
                };
                Ok((
                    junction_table,
                    junction_module,
                    source_col,
                    target_col,
                    Some(through_td),
                ))
            }
        }
    }

    fn multilink_junction_info(
        &mut self,
        td: &TypeDescriptor,
        ml: &MultiLinkDescriptor,
    ) -> Result<(String, String, String, String, Option<&'a TypeDescriptor>), PyQLError> {
        self.junction_info_for(td, &ml.name, &ml.target, &ml.through)
    }

    /// Same as `multilink_junction_info`, for a junction-backed single link.
    fn link_junction_info(
        &mut self,
        td: &TypeDescriptor,
        l: &LinkDescriptor,
    ) -> Result<(String, String, String, String, Option<&'a TypeDescriptor>), PyQLError> {
        self.junction_info_for(td, &l.name, &l.target, &l.through)
    }

    /// Build the `IrMultiLinkJoin` a multi-link or junction-backed single
    /// link's forward read-side join uses (`IrPathJoin::Multi`, a shape's
    /// `IrMultiLinkPointer`, or — for a junction-backed single link — the
    /// junction variant of `IrSingleLinkCorrelation`).
    fn build_multilink_join(
        &mut self,
        td: &TypeDescriptor,
        name: &str,
        target: &str,
        through: &Option<String>,
    ) -> Result<IrMultiLinkJoin, PyQLError> {
        if let Some(through_qname) = through {
            let through_td = self.resolve_type(through_qname)?;
            if through_td.junction {
                // See `junction_info_for`'s doc comment: a dedicated
                // junction type's physical table is always owner-derived
                // (one per concrete implementor), never `through_td.table`
                // itself.
                Ok(IrMultiLinkJoin::Standard {
                    junction_table: format!("{}.{}", td.table, name),
                    module: td.module.clone(),
                })
            } else {
                let source_qname = format!("{}::{}", td.module, td.name);
                let source_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to {source_qname}"),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
                    .name
                    .clone();
                let target_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == target && l.name != source_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == target))
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to target {target}"),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
                    .name
                    .clone();
                Ok(IrMultiLinkJoin::Through {
                    junction_table: through_td.table.clone(),
                    module: through_td.module.clone(),
                    source_col,
                    target_col,
                })
            }
        } else {
            Ok(IrMultiLinkJoin::Standard {
                junction_table: format!("{}.{}", td.table, name),
                module: td.module.clone(),
            })
        }
    }

    /// Compile the RHS of a multilink `+=`, `-=`, or `:= expr` into an
    /// `IrMultiLinkValues`. `td`/`alias` are the record being updated (link-
    /// property value expressions like `@weight := <float64>$w` compile
    /// against this scope, same as any other UPDATE SET assignment — they
    /// cannot reference the linked target's own properties, only the outer
    /// record's or bound params/literals). `through_td` is the junction
    /// type's own TypeDescriptor for a `Through[...]` multi-link, or `None`
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
            let inner_expr = shape
                .expr
                .as_ref()
                .ok_or_else(|| self.type_err("multilink value shape must have a base expression"))?;
            let mut inner = self.compile_multilink_values(inner_expr, td, alias, through_td)?;

            let Some(through) = through_td else {
                return Err(self.type_err(
                    "link properties (`@prop := value`) are only valid on a multi-link \
                     declared with `Through[...]`",
                ));
            };

            for el in &shape.elements {
                let prop_name = match el.path.steps.as_slice() {
                    [ast::PathStep::LinkProp(name)] => name.clone(),
                    _ => return Err(self.type_err("only `@prop := value` link-property assignments are valid here")),
                };
                let prop = Self::resolve_property(through, &prop_name)
                    .ok_or_else(|| self.field_err(&prop_name, &format!("{}::{}", through.module, through.name)))?;
                if prop.is_readonly {
                    return Err(self.type_err(&format!(
                        "cannot set link property '{prop_name}': it is declared as read-only"
                    )));
                }
                let value_expr = el
                    .compexpr
                    .as_ref()
                    .ok_or_else(|| self.type_err(&format!("link property '{prop_name}' must be assigned a value")))?;
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

        // `{ a, b }` — a set literal of targets, each contributing its own rows.
        if let Expr::Set(elements) = expr
            && !elements.is_empty()
        {
            let mut combined: Option<IrMultiLinkValues> = None;
            for element in elements {
                let one = self.compile_multilink_values(element, td, alias, through_td)?;
                combined = Some(match combined {
                    None => one,
                    Some(previous) => IrMultiLinkValues {
                        source: IrMultiLinkValueSource::Union(Box::new(previous), Box::new(one)),
                        link_props: vec![],
                    },
                });
            }
            return Ok(combined.expect("elements is non-empty"));
        }

        // `emails := (insert Email { … })` — a nested insert whose rows become
        // the link's targets, read back from the CTE it is hoisted into.
        if let Expr::SubQuery(inner) = expr
            && matches!(inner.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_))
        {
            let cte_name = self.hoist_nested_dml(inner)?;
            return Ok(IrMultiLinkValues {
                source: IrMultiLinkValueSource::CteRef(cte_name),
                link_props: vec![],
            });
        }

        // `emails := (select .emails filter .primary)` — a relative path here
        // is relative to the row being written, so it is rooted at that type
        // and correlated back to it; compiled bare it has no object to
        // resolve against and reads as a free select.
        if let Expr::SubQuery(inner) = expr
            && let Stmt::Select(sel) = inner.as_ref()
            && let Expr::Path(path) = &sel.result
            && path.partial
        {
            let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
            steps.extend(path.steps.iter().cloned());
            let rooted = ast::Path { steps, partial: false };
            let synthetic = ast::SelectStmt {
                result: Expr::Path(rooted.clone()),
                filter: sel.filter.clone(),
                order_by: sel.order_by.clone(),
                offset: sel.offset.clone(),
                limit: sel.limit.clone(),
                lock: None,
            };
            let mut ps = self.compile_path_select(&synthetic, &rooted, &[], false)?;
            Self::correlate_path_select(&mut ps, alias);
            return Ok(IrMultiLinkValues {
                source: IrMultiLinkValueSource::PathSelect(Box::new(ps)),
                link_props: vec![],
            });
        }

        // Parenthesised subquery
        if let Expr::SubQuery(inner) = expr {
            return match self.compile_stmt(inner)? {
                IrStmt::Select(s) if matches!(s.rows.as_slice(), [IrRowSource::Bound { .. }]) => {
                    Ok(IrMultiLinkValues {
                        source: IrMultiLinkValueSource::Select(Box::new(s)),
                        link_props: vec![],
                    })
                }
                IrStmt::PathSelect(ps) => Ok(IrMultiLinkValues {
                    source: IrMultiLinkValueSource::PathSelect(Box::new(ps)),
                    link_props: vec![],
                }),
                _ => Err(self.type_err("multilink value must resolve to a SELECT or path query")),
            };
        }

        // Absolute path expression (type reference or path traversal)
        if let Expr::Path(p) = expr
            && !p.partial
        {
            let fake_sel = ast::SelectStmt {
                result: expr.clone(),
                filter: None,
                order_by: vec![],
                offset: None,
                limit: None,
                lock: None,
            };
            return match self.compile_stmt(&Stmt::Select(fake_sel))? {
                IrStmt::PathSelect(ps) => Ok(IrMultiLinkValues {
                    source: IrMultiLinkValueSource::PathSelect(Box::new(ps)),
                    link_props: vec![],
                }),
                IrStmt::Select(s) if matches!(s.rows.as_slice(), [IrRowSource::Bound { .. }]) => {
                    Ok(IrMultiLinkValues {
                        source: IrMultiLinkValueSource::Select(Box::new(s)),
                        link_props: vec![],
                    })
                }
                _ => Err(self.type_err("expected a path expression for multilink value")),
            };
        }

        Err(self.type_err("multilink value must be a CTE reference, parenthesised subquery, or type path"))
    }

    // ── DELETE ────────────────────────────────────────────────────────────────────

    fn compile_delete(&mut self, del: &ast::DeleteStmt) -> Result<IrDelete, PyQLError> {
        let type_name = self.expr_as_type_name(&del.subject)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();
        let target = IrSource {
            poly: None,
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

        let (poly_implementors, poly_columns) = if td.abstract_ && td.materialized {
            (
                self.find_poly_implementors(&format!("{}::{}", td.module, td.name)),
                Self::poly_dml_columns(td),
            )
        } else {
            (vec![], vec![])
        };
        let qname = format!("{}::{}", td.module, td.name);
        let enqueue_search = collect_search_enqueue(td, &qname, "delete");

        Ok(IrDelete {
            target,
            filter,
            returning,
            poly_implementors,
            poly_columns,
            enqueue_search,
        })
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
            // No explicit shape: implicit { id } only.
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
            .map(|p| {
                IrShapePointer::Scalar(IrScalarPointer {
                    marker_offset: None,
                    alias: p.name.clone(),
                    column: p.name.clone(),
                    pg_type: p.pg_type.clone(),
                    tuple_shape: self.resolve_property_tuple_shape(p),
                })
            })
            .collect();

        for cd in &td.computed.clone() {
            // `*` is properties; links arrive only with `**`. A computed
            // standing for objects is a link however it is written, so it
            // waits for the deep form too.
            if matches!(splat, ast::Splat::Shallow) && self.computed_is_object_valued(cd, td) {
                continue;
            }
            pointers.push(self.compile_declared_computed(cd, td, alias, module, None, &[])?);
        }

        if matches!(splat, ast::Splat::Deep) {
            for l in &td.links {
                let target_td = self.resolve_type(&l.target)?;
                let sub_alias = self.fresh_alias();
                // `**` fetches every property (not further nested links) of
                // a linked object — a `Shallow` (`*`) expansion of the
                // target type, one level deep. Recursing with `Deep` here
                // instead would walk the target's own links too, which for
                // a two-way or cyclic link graph never terminates.
                let target_module = target_td.module.clone();
                let sub_shape = self.compile_splat(&ast::Splat::Shallow, target_td, &sub_alias, &target_module)?;
                let subquery = IrSelect::schema_bound(
                    IrSource {
                        poly: None,
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: sub_alias.clone(),
                    },
                    sub_shape,
                    None,
                );
                let correlation = if l.is_junction_backed() {
                    let join = self.build_multilink_join(td, &l.name, &l.target, &l.through)?;
                    IrSingleLinkCorrelation::Junction {
                        join,
                        target_pk: "id".to_string(),
                    }
                } else {
                    IrSingleLinkCorrelation::Fk {
                        fk_column: format!("{}_id", l.name),
                        target_pk: "id".to_string(),
                    }
                };
                pointers.push(IrShapePointer::SingleLink(IrSingleLinkPointer {
                    marker_offset: None,
                    alias: l.name.clone(),
                    correlation,
                    subquery,
                    link_properties: vec![],
                }));
            }

            for ml in &td.multilinks {
                let sub_alias = self.fresh_alias();
                let target_td = self.resolve_type(&ml.target)?;
                // See the single-link loop above — same Shallow-not-Deep
                // reasoning applies to multilink targets.
                let target_module = target_td.module.clone();
                let sub_shape = self.compile_splat(&ast::Splat::Shallow, target_td, &sub_alias, &target_module)?;

                let join = if let Some(through_qname) = &ml.through {
                    let through_td = self.resolve_type(through_qname)?;
                    if through_td.junction {
                        // See `junction_info_for`'s doc comment: owner-derived.
                        IrMultiLinkJoin::Standard {
                            junction_table: format!("{}.{}", td.table, ml.name),
                            module: td.module.clone(),
                        }
                    } else {
                        let source_qname = format!("{}::{}", td.module, td.name);
                        let source_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == source_qname)
                            .ok_or_else(|| {
                                PyQLError::Type(PyQLTypeError {
                                    message: format!(
                                        "through type {through_qname} has no link to source type {source_qname}"
                                    ),
                                    position: Position { line: 0, col: 0 },
                                })
                            })?
                            .name
                            .clone();
                        let target_col = through_td
                            .links
                            .iter()
                            .find(|l| l.target == ml.target && l.name != source_col)
                            .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                            .ok_or_else(|| {
                                PyQLError::Type(PyQLTypeError {
                                    message: format!(
                                        "through type {through_qname} has no link to target type {}",
                                        ml.target
                                    ),
                                    position: Position { line: 0, col: 0 },
                                })
                            })?
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
                    // See `junction_info_for`: the junction table is named
                    // after the owner and lives in the owner's schema, which a
                    // splat reaching in from another module is not.
                    IrMultiLinkJoin::Standard {
                        junction_table: format!("{}.{}", td.table, ml.name),
                        module: td.module.clone(),
                    }
                };

                let subquery = self.link_target_select(
                    target_td,
                    IrSource {
                        poly: None,
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: sub_alias.clone(),
                    },
                    sub_shape,
                );

                pointers.push(IrShapePointer::MultiLink(IrMultiLinkPointer {
                    marker_offset: None,
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
        let interface_props: std::collections::HashSet<String> =
            parent_td.properties.iter().map(|p| p.name.clone()).collect();

        // Emit scalar subquery for each property not already in the interface
        let props: Vec<_> = concrete_td
            .properties
            .iter()
            .filter(|p| !interface_props.contains(&p.name))
            .cloned()
            .collect();

        // Same for computed pointers not already defined on the interface —
        // `[is Concrete].*` must include the concrete type's own computed
        // properties (e.g. `full_name`), not just its stored properties.
        let interface_computed: std::collections::HashSet<String> =
            parent_td.computed.iter().map(|c| c.name.clone()).collect();
        let computed: Vec<_> = concrete_td
            .computed
            .iter()
            .filter(|c| !interface_computed.contains(&c.name))
            .filter(|c| !matches!(splat, ast::Splat::Shallow) || !self.computed_is_object_valued(c, concrete_td))
            .cloned()
            .collect();

        // For deep splat, also include links
        let links: Vec<_> = if matches!(splat, ast::Splat::Deep) {
            concrete_td.links.to_vec()
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
            let subquery = IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: concrete_qname.clone(),
                    table: concrete_table.clone(),
                    alias: sub_alias,
                },
                vec![IrShapePointer::Scalar(IrScalarPointer {
                    marker_offset: None,
                    alias: prop.name.clone(),
                    column: prop.name.clone(),
                    pg_type: prop.pg_type.clone(),
                    tuple_shape: self.resolve_property_tuple_shape(&prop),
                })],
                Some(filter),
            );
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: None,
                alias: prop.name.clone(),
                expr: IrExpr::Subquery(Box::new(subquery)),
            }));
        }

        for cd in computed {
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
            let expr_ast = crate::parse::parse_pointer_expr(&cd.expression).map_err(PyQLError::Syntax)?;
            let inner_ir = self.compile_expr(&expr_ast, concrete_td, &sub_alias)?;
            let subquery = IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: concrete_qname.clone(),
                    table: concrete_table.clone(),
                    alias: sub_alias,
                },
                vec![IrShapePointer::Computed(IrComputedPointer {
                    marker_offset: None,
                    alias: cd.name.clone(),
                    expr: inner_ir,
                })],
                Some(filter),
            );
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: None,
                alias: cd.name.clone(),
                expr: IrExpr::Subquery(Box::new(subquery)),
            }));
        }

        // For deep splat, include single-link pointers as subqueries
        for link in links {
            let target_td = self.resolve_type(&link.target)?;
            let sub_alias = self.fresh_alias();
            let filter = if link.is_junction_backed() {
                let (jt_table, jt_module, jt_src_col, jt_tgt_col, _) =
                    self.junction_info_for(concrete_td, &link.name, &link.target, &link.through)?;
                let jt_alias = self.fresh_alias();
                let jt_filter = IrExpr::BinOp(Box::new(IrBinOp {
                    left: IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef {
                            alias: jt_alias.clone(),
                            column: jt_src_col,
                            pg_type: "uuid".to_string(),
                        },
                        op: ast::BinOpKind::Eq,
                        right: IrExpr::ColumnRef {
                            alias: parent_alias.to_string(),
                            column: "id".to_string(),
                            pg_type: "uuid".to_string(),
                        },
                    })),
                    op: ast::BinOpKind::And,
                    right: IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef {
                            alias: jt_alias.clone(),
                            column: jt_tgt_col,
                            pg_type: "uuid".to_string(),
                        },
                        op: ast::BinOpKind::Eq,
                        right: IrExpr::ColumnRef {
                            alias: sub_alias.clone(),
                            column: "id".to_string(),
                            pg_type: "uuid".to_string(),
                        },
                    })),
                }));
                let exists_select = IrSelect::schema_bound(
                    IrSource {
                        poly: None,
                        type_name: format!("{}::__jt__", jt_module),
                        table: jt_table,
                        alias: jt_alias,
                    },
                    vec![],
                    Some(jt_filter),
                );
                IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: IrExpr::Subquery(Box::new(exists_select)),
                }))
            } else {
                IrExpr::BinOp(Box::new(IrBinOp {
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
                }))
            };
            let sub_shape = Self::pk_returning(target_td);
            let subquery = IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias,
                },
                sub_shape,
                Some(filter),
            );
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: None,
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
        marker_offset: Option<usize>,
    ) -> Result<IrShapePointer, PyQLError> {
        let expr = self.compile_type_intersection_expr_steps(type_ref, tail_steps, parent_alias)?;
        // Alias is the last Name step
        let alias = match tail_steps.last() {
            Some(ast::PathStep::Name(n)) => n.clone(),
            _ => return Err(self.type_err("type intersection must end with a pointer name")),
        };
        Ok(IrShapePointer::Computed(IrComputedPointer {
            alias,
            expr,
            marker_offset,
        }))
    }

    /// Compile `[is Type].name` as an `IrExpr` (for computed pointer / expression context).
    fn compile_type_intersection_expr(
        &mut self,
        steps: &[ast::PathStep],
        td: &TypeDescriptor,
        parent_alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        use ast::PathStep;
        let type_ref = match steps.first() {
            Some(PathStep::TypeIntersection(tr)) => tr.clone(),
            _ => return Err(self.type_err("expected type intersection")),
        };
        // The fast path reads one stored column off the narrowed type.
        // Anything else — a computed, a link, further traversal — falls
        // through to the general builder, rooted at the narrowed type
        // (whose rows share the interface row's id).
        match self.compile_type_intersection_expr_steps(&type_ref, &steps[1..], parent_alias) {
            Ok(ir) => Ok(ir),
            Err(fast_path_err) => {
                let p = ast::Path {
                    steps: steps.to_vec(),
                    partial: true,
                };
                self.compile_partial_path_as_subquery(&p, td, parent_alias)
                    .map_err(|_| fast_path_err)
            }
        }
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
            _ => return Err(self.type_err("type intersection must be followed by a pointer name, e.g. [is Type].name")),
        };

        let prop = concrete_td
            .properties
            .iter()
            .find(|p| p.name == pointer_name)
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

        Ok(IrExpr::Subquery(Box::new(IrSelect::schema_bound(
            IrSource {
                poly: None,
                type_name: concrete_qname,
                table: concrete_table,
                alias: sub_alias,
            },
            vec![IrShapePointer::Scalar(IrScalarPointer {
                marker_offset: None,
                alias: prop_name.clone(),
                column: prop_name,
                pg_type: prop_type,
                tuple_shape: self.resolve_property_tuple_shape(prop),
            })],
            Some(filter),
        ))))
    }

    fn compile_shape_element(
        &mut self,
        el: &ShapeElement,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
    ) -> Result<IrShapePointer, PyQLError> {
        // Type intersection pointer: [is Type].pointer_name (without compexpr)
        if let Some(ast::PathStep::TypeIntersection(type_ref)) = el.path.steps.first()
            && el.compexpr.is_none()
            && el.path.steps.len() >= 2
        {
            let type_ref = type_ref.clone();
            return self.compile_type_intersection_pointer(&type_ref, &el.path.steps[1..], alias, el.marker_offset);
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
                marker_offset: el.marker_offset,
                alias: "__type__".to_string(),
                expr,
            }));
        }

        // Computed override: `pointer := expr`
        if let Some(compexpr) = &el.compexpr {
            // A link-valued RHS (`.multilink`, `.<backlink[is T] { … }`,
            // `(select .link filter … limit 1)`) is a pointer in its own
            // right, not an expression — see `try_compile_pointer_expr`.
            if let Some(ptr) = self.try_compile_pointer_expr(
                pointer_name,
                compexpr,
                td,
                alias,
                module,
                el.marker_offset,
                el.nested.as_deref().unwrap_or(&[]),
            )? {
                return Ok(ptr);
            }
            let ir = self.compile_expr(compexpr, td, alias)?;
            // Cross-scope TypeIs: promote to set-valued shape pointer.
            if let IrExpr::ArrayFromSelect(src) = ir {
                if let IrArraySource::RawExpr {
                    source,
                    poly_implementors,
                    poly_columns,
                    expr,
                } = *src
                {
                    return Ok(IrShapePointer::ScalarSet(IrScalarSetPointer {
                        alias: pointer_name.to_string(),
                        source,
                        poly_implementors,
                        poly_columns,
                        bool_expr: expr,
                    }));
                }
                return Ok(IrShapePointer::Computed(IrComputedPointer {
                    marker_offset: el.marker_offset,
                    alias: pointer_name.to_string(),
                    expr: IrExpr::ArrayFromSelect(src),
                }));
            }
            return Ok(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: el.marker_offset,
                alias: pointer_name.to_string(),
                expr: ir,
            }));
        }

        // Scalar property
        if let Some(p) = Self::resolve_property(td, pointer_name) {
            return Ok(IrShapePointer::Scalar(IrScalarPointer {
                marker_offset: el.marker_offset,
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

            // A junction-backed link can carry `@prop` read references
            // (e.g. `spouse: { name, @since }`), the same as a multi-link's
            // own nested shape (`compile_multilink_pointer`) — partition
            // those out before compiling the rest as a regular shape.
            let (regular_els, link_properties): (Vec<ShapeElement>, Vec<IrLinkProp>) = if l.is_junction_backed() {
                let mut regular = Vec::new();
                let mut props = Vec::new();
                for nel in nested_elements {
                    if let [ast::PathStep::LinkProp(name)] = nel.path.steps.as_slice() {
                        props.push(IrLinkProp { name: name.clone() });
                    } else {
                        regular.push(nel.clone());
                    }
                }
                (regular, props)
            } else {
                (nested_elements.to_vec(), vec![])
            };

            let sub_shape = self.compile_shape(&regular_els, target_td, &sub_alias, &target_td.module.clone())?;
            let subquery = self.link_target_select(
                target_td,
                IrSource {
                    poly: None,
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias,
                },
                sub_shape,
            );
            let correlation = if l.is_junction_backed() {
                let join = self.build_multilink_join(td, &l.name, &l.target, &l.through)?;
                IrSingleLinkCorrelation::Junction {
                    join,
                    target_pk: "id".to_string(),
                }
            } else {
                IrSingleLinkCorrelation::Fk {
                    fk_column: format!("{}_id", l.name),
                    target_pk: "id".to_string(),
                }
            };
            return Ok(IrShapePointer::SingleLink(IrSingleLinkPointer {
                marker_offset: el.marker_offset,
                alias: pointer_name.to_string(),
                correlation,
                subquery,
                link_properties,
            }));
        }

        // Multi-link
        if Self::resolve_multilink(td, pointer_name).is_some() {
            return self.compile_multilink_pointer(pointer_name, pointer_name, td, alias, module, el);
        }

        // Schema-defined computed pointer
        if let Some(cd) = self.resolve_computed(td, pointer_name) {
            return self.compile_declared_computed(
                &cd,
                td,
                alias,
                module,
                el.marker_offset,
                el.nested.as_deref().unwrap_or(&[]),
            );
        }

        // Declared by the binding this shape is read off, rather than by the
        // type — compiled against the binding's own row, as it was written.
        if let Some(declared) = self
            .active_declared_pointers
            .iter()
            .find(|d| path_leaf(&d.path).is_ok_and(|n| n == pointer_name))
            .cloned()
            && let Some(expr) = declared.compexpr.clone()
        {
            let nested = if el.nested.as_deref().unwrap_or(&[]).is_empty() {
                declared.nested.clone().unwrap_or_default()
            } else {
                el.nested.clone().unwrap_or_default()
            };
            return self.compile_computed_expr(pointer_name, &expr, td, alias, module, el.marker_offset, &nested);
        }

        Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)))
    }

    /// A computed pointer whose path walks *through* a multi-valued step before
    /// landing on objects — `members := .memberships.member`.
    ///
    /// Rooted at the enclosing type and correlated back to the enclosing row,
    /// exactly as `compile_partial_path_as_subquery` does for a value, then
    /// aggregated so the rows survive as rows.
    #[allow(clippy::too_many_arguments)]
    fn compile_chained_link_pointer(
        &mut self,
        pointer_name: &str,
        path: &ast::Path,
        td: &TypeDescriptor,
        alias: &str,
        nested: &[ShapeElement],
        modifiers: Option<&ast::SelectStmt>,
        multi: bool,
    ) -> Result<IrShapePointer, PyQLError> {
        let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
        steps.extend(path.steps.iter().cloned());
        let full_path = ast::Path { steps, partial: false };
        let synthetic = ast::SelectStmt {
            result: Expr::Path(full_path.clone()),
            filter: modifiers.and_then(|m| m.filter.clone()),
            order_by: modifiers.map(|m| m.order_by.clone()).unwrap_or_default(),
            offset: modifiers.and_then(|m| m.offset.clone()),
            limit: modifiers.and_then(|m| m.limit.clone()),
            lock: None,
        };
        let mut path_select = self.compile_path_select(&synthetic, &full_path, nested, false)?;
        Self::correlate_path_select(&mut path_select, alias);
        // A walk that crosses a multi step stands for a set, so it is
        // aggregated; one that cannot is a single object and is read as one,
        // rather than an array of length one.
        let expr = if multi {
            IrExpr::ArrayFromSelect(Box::new(IrArraySource::PathSelect(Box::new(path_select))))
        } else {
            IrExpr::ObjectPathSubquery(Box::new(path_select))
        };
        Ok(IrShapePointer::Computed(IrComputedPointer {
            marker_offset: None,
            alias: pointer_name.to_string(),
            expr,
        }))
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

        let sub_shape = self.compile_shape(&regular_els, target_td, &sub_alias, &target_td.module.clone())?;

        let join = if let Some(through_qname) = &ml.through {
            let through_td = self.resolve_type(through_qname)?;
            if through_td.junction {
                // Junction type: columns are always named `source` and
                // `target`. See `junction_info_for`'s doc comment: the
                // physical table is owner-derived, never `through_td.table`.
                IrMultiLinkJoin::Standard {
                    junction_table: format!("{}.{}", td.table, ml_name),
                    module: td.module.clone(),
                }
            } else {
                let source_qname = format!("{}::{}", td.module, td.name);
                let source_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to source type {source_qname}"),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
                    .name
                    .clone();
                let target_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == ml.target && l.name != source_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == ml.target))
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to target type {}", ml.target),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
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

        // `@prop` in this link's own modifiers reads the junction row, so
        // the through type has to be in scope while they compile.
        self.link_prop_scope
            .push(ml.through.clone().map(|t| (t, "jt".to_string())));
        let modifiers = (|c: &mut Self| -> Result<SelectModifiers, PyQLError> {
            Ok((
                el.filter
                    .as_ref()
                    .map(|f| c.compile_expr(f, target_td, &sub_alias))
                    .transpose()?,
                el.order_by
                    .iter()
                    .map(|s| c.compile_sort(s, target_td, &sub_alias))
                    .collect::<Result<_, _>>()?,
                el.offset
                    .as_ref()
                    .map(|e| c.compile_expr(e, target_td, &sub_alias))
                    .transpose()?,
                el.limit
                    .as_ref()
                    .map(|e| c.compile_expr(e, target_td, &sub_alias))
                    .transpose()?,
            ))
        })(self);
        self.link_prop_scope.pop();
        let (filter, order_by, offset, limit) = modifiers?;
        let subquery = IrSelect {
            rows: vec![IrRowSource::Bound {
                source: IrSource {
                    poly: self.link_target_fanout(target_td),
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias.clone(),
                },
                shape: sub_shape,
            }],
            filter,
            order_by,
            offset,
            limit,
            distinct: false,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        };

        Ok(IrShapePointer::MultiLink(IrMultiLinkPointer {
            marker_offset: el.marker_offset,
            alias: output_alias.to_string(),
            join,
            subquery,
            link_properties,
        }))
    }

    /// Compile `pointer := .<backlink_name[is OwnerType] { shape }` (a
    /// backlink used as a computed pointer inside another type's shape —
    /// the missing piece that made backlinks unusable for anything beyond
    /// `filter exists .<...>`). Mirrors `compile_multilink_pointer`'s
    /// shape/subquery construction, but the owner type's rows are
    /// correlated in reverse: via their own FK column for a single-link
    /// backlink source (`IrMultiLinkJoin::BacklinkFk`), or via the same
    /// junction table a forward multi-link would use with the owner/current
    /// column roles swapped (`IrMultiLinkJoin::BacklinkJunction`).
    fn compile_backlink_pointer(
        &mut self,
        output_alias: &str,
        path: &ast::Path,
        current_qname: &str,
        nested_elements: &[ShapeElement],
        marker_offset: Option<usize>,
        modifiers: Option<&ast::SelectStmt>,
    ) -> Result<IrShapePointer, PyQLError> {
        use ast::PathStep;

        let backlink_name = match path.steps.first() {
            Some(PathStep::Backlink(n)) => n.clone(),
            _ => return Err(self.type_err("internal: expected backlink step")),
        };
        let type_ref = match path.steps.get(1) {
            Some(PathStep::TypeIntersection(tr)) => tr,
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: format!(
                        "backlink '.< {backlink_name}' requires a type intersection, \
                     e.g.: .< {backlink_name}[is SomeType]"
                    ),
                    position: Position { line: 0, col: 0 },
                }));
            }
        };
        if path.steps.len() > 2 {
            return Err(self.type_err(
                "further path traversal after a backlink shape is not yet supported \
                 (e.g. '.<link[is Type].property') — attach a nested shape instead: \
                 '.<link[is Type] { property }'",
            ));
        }

        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let owner_td = self.resolve_type(&type_name)?;
        // `.<passkeys[is account::Account]` — the intersection narrows what
        // comes back, and the link itself may be declared further down: here
        // `passkeys` is Individual's, and an Individual is an Account. So the
        // owner is the concrete type that actually declares it, the same way
        // `backlink_exists_over_owners` resolves one.
        let owner_td = if self.declares_backlink(owner_td, &backlink_name, current_qname) {
            owner_td
        } else {
            // Qualified, because that is how a type records the interfaces it
            // implements; the query may well have written the bare name.
            let narrowed = format!("{}::{}", owner_td.module, owner_td.name);
            let declaring: Vec<&'a TypeDescriptor> = self
                .schema
                .types
                .iter()
                .filter(|t| !t.abstract_ && Self::is_or_implements(t, &narrowed))
                .filter(|t| self.declares_backlink(t, &backlink_name, current_qname))
                .collect();
            match declaring.as_slice() {
                [only] => only,
                [] => owner_td,
                several => {
                    return Err(self.type_err(&format!(
                        "'{backlink_name}' pointing to {current_qname} is declared by {} types under \
                         {narrowed} ({}), so a backlink narrowed to it has no single source to read \
                         — narrow to one of them instead",
                        several.len(),
                        several
                            .iter()
                            .map(|t| format!("{}::{}", t.module, t.name))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )));
                }
            }
        };
        let owner_qname = format!("{}::{}", owner_td.module, owner_td.name);

        let join = if let Some(l) = owner_td
            .links
            .iter()
            .find(|l| l.name == backlink_name && self.link_target_reaches(&l.target, current_qname))
        {
            if l.is_junction_backed() {
                let (junction_table, module, owner_col, current_col, _) = self.link_junction_info(owner_td, l)?;
                IrMultiLinkJoin::BacklinkJunction {
                    junction_table,
                    module,
                    owner_col,
                    current_col,
                }
            } else {
                IrMultiLinkJoin::BacklinkFk {
                    fk_col: format!("{}_id", backlink_name),
                }
            }
        } else if let Some(ml) = owner_td
            .multilinks
            .iter()
            .find(|ml| ml.name == backlink_name && self.link_target_reaches(&ml.target, current_qname))
            .cloned()
        {
            let (junction_table, module, _, _, _) = self.multilink_junction_info(owner_td, &ml)?;
            IrMultiLinkJoin::BacklinkJunction {
                junction_table,
                module,
                owner_col: "source".to_string(),
                current_col: "target".to_string(),
            }
        } else {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "type {} has no link or multi-link '{}' pointing to {}",
                    owner_qname, backlink_name, current_qname,
                ),
                position: Position { line: 0, col: 0 },
            }));
        };

        let sub_alias = self.fresh_alias();
        let sub_shape = self.compile_shape(nested_elements, owner_td, &sub_alias, &owner_td.module.clone())?;

        // The `pointer := expr` grammar (`parse_shape_element`'s `:=`
        // branch) never parses trailing FILTER/ORDER BY/OFFSET/LIMIT after
        // the RHS expression — those per-link modifiers only exist on the
        // separate no-`:=` "bare inclusion with nested shape" parse path
        // that `compile_multilink_pointer`'s other call site reads
        // `el.filter` etc. from. Writing the RHS as a sub-select
        // (`pointer := (select .<link[is T] { … } filter … limit 1)`) is the
        // way to get them here, and `modifiers` carries that inner select.
        let (filter, order_by, offset, limit) = match modifiers {
            Some(sel) => self.compile_path_modifiers(sel, owner_td, &sub_alias)?,
            None => (None, vec![], None, None),
        };
        let subquery = IrSelect {
            rows: vec![IrRowSource::Bound {
                source: IrSource {
                    poly: None,
                    type_name: owner_qname,
                    table: owner_td.table.clone(),
                    alias: sub_alias.clone(),
                },
                shape: sub_shape,
            }],
            filter,
            order_by,
            offset,
            limit,
            distinct: false,
            dml_source: None,
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            lock: None,
        };

        Ok(IrShapePointer::MultiLink(IrMultiLinkPointer {
            alias: output_alias.to_string(),
            join,
            subquery,
            link_properties: vec![],
            marker_offset,
        }))
    }

    /// Compile a pointer the *schema* declares as computed (as opposed to one
    /// written inline in the query). A declared computed whose expression is
    /// a sub-select over a link — `(select .emails filter .primary limit 1)`
    /// — is an object pointer, exactly as if it had been written inline, so
    /// it takes the same route; everything else is a scalar expression.
    /// An object-returning function call as a row source, with the shape the
    /// pointer asked for (its target's own pk when none was written).
    fn compile_fn_object_source(
        &mut self,
        fc: &ast::FunctionCall,
        nested: &[ShapeElement],
    ) -> Result<Option<IrFunctionSelect>, PyQLError> {
        let Some(fd) = self.schema.functions.iter().find(|f| {
            let module_matches = fc.module.as_deref().map(|m| m == f.module.as_str()).unwrap_or(true);
            module_matches && f.name == fc.name && f.return_is_object
        }) else {
            return Ok(None);
        };
        let (fn_module, fn_name, return_type_name) = (fd.module.clone(), fd.name.clone(), fd.return_pg_type.clone());
        let return_td = self.resolve_type(&return_type_name)?;
        let alias = self.fresh_alias();
        let shape = if nested.is_empty() {
            Self::pk_returning(return_td)
        } else {
            self.compile_shape(nested, return_td, &alias, &return_td.module)?
        };
        let args = fc
            .args
            .iter()
            .map(|a| self.compile_free_expr(a))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(IrFunctionSelect {
            fn_module,
            fn_name,
            fn_args: args,
            alias,
            type_name: format!("{}::{}", return_td.module, return_td.name),
            polymorphic: false,
            poly_implementors: vec![],
            poly_columns: vec![],
            shape,
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
        }))
    }

    fn compile_declared_computed(
        &mut self,
        cd: &crate::schema::ComputedDescriptor,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
        marker_offset: Option<usize>,
        nested: &[ShapeElement],
    ) -> Result<IrShapePointer, PyQLError> {
        let expr_ast = crate::parse::parse_pointer_expr(&cd.expression).map_err(PyQLError::Syntax)?;
        self.compile_computed_expr(&cd.name, &expr_ast, td, alias, module, marker_offset, nested)
    }

    /// The body of `compile_declared_computed`, for a pointer whose expression
    /// is already parsed — a `with` binding's own shape declares one that way.
    #[allow(clippy::too_many_arguments)]
    fn compile_computed_expr(
        &mut self,
        name: &str,
        expr_ast: &Expr,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
        marker_offset: Option<usize>,
        nested: &[ShapeElement],
    ) -> Result<IrShapePointer, PyQLError> {
        // The object the pointer is computed on stays in scope for the whole
        // expression, including any part of it compiled without a type in
        // hand — a `with` binding's right-hand side, say.
        self.anchors.push(SelectAnchor {
            type_name: td.name.clone(),
            qualified: format!("{}::{}", td.module, td.name),
            alias: alias.to_string(),
            detached: false,
        });
        let result = (|compiler: &mut Self| -> Result<IrShapePointer, PyQLError> {
            if let Some(ptr) =
                compiler.try_compile_pointer_expr(name, expr_ast, td, alias, module, marker_offset, nested)?
            {
                return Ok(ptr);
            }
            // `manifest := retrieve_connector_manifest(.id)` — a computed whose
            // expression is an object-returning call. The rows it yields are
            // the pointer's objects; read as a plain expression the call is
            // "part of a larger expression", which such a function refuses.
            if let Expr::FunctionCall(fc) = expr_ast
                && let Some(fs) = compiler.compile_fn_object_source(fc, nested)?
            {
                return Ok(IrShapePointer::Computed(IrComputedPointer {
                    marker_offset,
                    alias: name.to_string(),
                    expr: IrExpr::ArrayFromSelect(Box::new(IrArraySource::ObjectFunction(Box::new(fs)))),
                }));
            }
            let ir = compiler.compile_expr(expr_ast, td, alias)?;
            Ok(IrShapePointer::Computed(IrComputedPointer {
                marker_offset,
                alias: name.to_string(),
                expr: ir,
            }))
        })(self);
        self.anchors.pop();
        result
    }

    /// Split a sub-select's result into the path it traverses and the nested
    /// shape attached to it: `select .posts { title }` → `.posts` + `{ title }`,
    /// `select .posts` → `.posts` + no shape.
    fn split_path_result(result: &Expr) -> Option<(&ast::Path, &[ShapeElement])> {
        match result {
            Expr::Path(p) => Some((p, &[])),
            Expr::Shape(sh) => match &sh.expr {
                Some(Expr::Path(p)) => Some((p, sh.elements.as_slice())),
                _ => None,
            },
            _ => None,
        }
    }

    /// Peel a computed pointer's right-hand side down to the function call
    /// it names and the sub-select's modifiers, if that is its shape:
    /// `latest(.id)` / `(select latest(.id) filter …)`.
    fn function_subject(e: &Expr) -> Option<(&ast::FunctionCall, Option<&ast::SelectStmt>)> {
        match e {
            Expr::FunctionCall(fc) => Some((fc, None)),
            Expr::SubQuery(stmt) => match stmt.as_ref() {
                Stmt::Select(sel) => match &sel.result {
                    Expr::FunctionCall(fc) => Some((fc, Some(sel))),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// The object-returning user function `fc` names, if any.
    fn resolve_object_fn(&self, fc: &ast::FunctionCall) -> Option<&'a FunctionDescriptor> {
        self.schema.functions.iter().find(|f| {
            let module_matches = fc.module.as_deref().map(|m| m == f.module.as_str()).unwrap_or(true);
            module_matches && f.name == fc.name && f.return_is_object
        })
    }

    /// Peel a computed pointer's right-hand side down to the path it names,
    /// the shape attached to it, and the sub-select carrying its modifiers
    /// (if any): `.posts` / `.posts { title }` / `(select .posts limit 1)` /
    /// `(select .posts { title } limit 1)` / `(select .posts limit 1) { title }`.
    fn pointer_subject(e: &Expr) -> Option<(&ast::Path, &[ShapeElement], Option<&ast::SelectStmt>)> {
        match e {
            Expr::Path(p) => Some((p, &[], None)),
            Expr::SubQuery(stmt) => match stmt.as_ref() {
                Stmt::Select(sel) => {
                    let (p, inner) = Self::split_path_result(&sel.result)?;
                    Some((p, inner, Some(sel)))
                }
                _ => None,
            },
            Expr::Shape(sh) => {
                let (path, inner, modifiers) = Self::pointer_subject(sh.expr.as_ref()?)?;
                let nested = if sh.elements.is_empty() {
                    inner
                } else {
                    sh.elements.as_slice()
                };
                Some((path, nested, modifiers))
            }
            _ => None,
        }
    }

    /// A computed pointer whose right-hand side names a *link* rather than
    /// computing a value — `pointer := .multilink`, `:= .multilink { … }`,
    /// `:= .<backlink[is T] { … }`, and any of those wrapped in a sub-select
    /// carrying FILTER/ORDER BY/OFFSET/LIMIT.
    ///
    /// These are pointers in their own right, so they route to the same
    /// builders a bare `pointer: { … } filter … limit N` inclusion uses and
    /// come back as real object pointers (arrays of hydrated objects) rather
    /// than the bare id or EXISTS boolean that compiling them as an
    /// expression would produce. A sub-select's modifiers are exactly the
    /// per-link modifiers that inclusion form already carries, which is what
    /// makes the re-use exact — the `:=` grammar has no trailing-modifier
    /// form of its own.
    ///
    /// Shared by inline `pointer := …` shape elements and schema-declared
    /// computeds, so both kinds behave identically.
    ///
    /// `Ok(None)` means "not a link-valued RHS": the caller compiles it as
    /// an ordinary expression, which is where `(select …).field` — a scalar
    /// — is handled.
    #[allow(clippy::too_many_arguments)]
    fn try_compile_pointer_expr(
        &mut self,
        pointer_name: &str,
        compexpr: &Expr,
        td: &TypeDescriptor,
        alias: &str,
        module: &str,
        marker_offset: Option<usize>,
        nested_override: &[ShapeElement],
    ) -> Result<Option<IrShapePointer>, PyQLError> {
        // A nested shape can sit inside the parens (`(select .posts
        // { title })`) or after them (`(select .posts) { title }`, `.posts
        // { title }`) — the parser folds a trailing `{ }` into an
        // `Expr::Shape` wrapping whatever precedes it, never into
        // `el.nested` (that field is only populated by the separate no-`:=`
        // "bare inclusion with nested shape" parse path).
        let Some((path, declared_nested, modifiers)) = Self::pointer_subject(compexpr) else {
            return Ok(None);
        };
        if !path.partial {
            return Ok(None);
        }
        // A shape written at the point of use (`authors { name }` on a
        // declared computed) wins over the one the declaration itself
        // carries, which acts as the default.
        let nested = if nested_override.is_empty() {
            declared_nested
        } else {
            nested_override
        };

        // Only a link pointer of the current type becomes an object pointer;
        // a property (`x := .name`, `x := (select .name)`) is a scalar and
        // belongs on the expression path.
        let ml_name = match path.steps.as_slice() {
            [ast::PathStep::Name(n)] if Self::resolve_multilink(td, n).is_some() => n.clone(),
            // `.<link[is Owner]` — the objects on the other side of the
            // link. Traversing *past* the intersection (`.<link[is
            // Owner].name`) is a value, not an object pointer, so it goes
            // the expression route instead.
            [ast::PathStep::Backlink(_)] | [ast::PathStep::Backlink(_), ast::PathStep::TypeIntersection(_)] => {
                let current_qname = format!("{}::{}", td.module, td.name);
                let path = path.clone();
                let nested = nested.to_vec();
                return self
                    .compile_backlink_pointer(pointer_name, &path, &current_qname, &nested, marker_offset, modifiers)
                    .map(Some);
            }
            // `.memberships.member` — a chain no single-step builder can
            // express: the junction it would need belongs to no one link but
            // to the whole walk. Compiled as a path select and aggregated, so
            // it stays an object pointer instead of collapsing to the bare ids
            // an expression-position path gives — which, worse, was a scalar
            // subquery that failed outright the moment a second row matched.
            // A leading `[is T]` narrows what the walk starts from
            // (`brand := [is BrandOrderLineItem].brand { * }`); the walk
            // itself is the same one a bare name starts.
            steps
                if matches!(
                    steps.first(),
                    Some(ast::PathStep::Name(_) | ast::PathStep::TypeIntersection(_))
                ) =>
            {
                let (multi, target) = self.walk_path_types(td, steps, MAX_COMPUTED_SPLICES);
                // A single-valued walk takes this route only when a shape was
                // written on it (`account := resource.account { id }`), which
                // is what says an object was meant. Without one, a bare
                // `x := .link` still stands for the link's value, as it did
                // before there was an object route at all.
                if target.is_none() || (!multi && nested.is_empty()) {
                    return Ok(None);
                }
                let path = path.clone();
                let nested = nested.to_vec();
                return self
                    .compile_chained_link_pointer(pointer_name, &path, td, alias, &nested, modifiers, multi)
                    .map(Some);
            }
            _ => return Ok(None),
        };

        let synthetic = ShapeElement {
            path: path.clone(),
            splat: None,
            nested: Some(nested.to_vec()),
            compexpr: None,
            op: ast::ShapeOp::Assign,
            filter: modifiers.and_then(|s| s.filter.clone()),
            order_by: modifiers.map(|s| s.order_by.clone()).unwrap_or_default(),
            offset: modifiers.and_then(|s| s.offset.clone()),
            limit: modifiers.and_then(|s| s.limit.clone()),
            marker_offset,
        };
        self.compile_multilink_pointer(pointer_name, &ml_name, td, alias, module, &synthetic)
            .map(Some)
    }

    /// A sub-statement used as an expression: `(select .emails filter
    /// .primary limit 1).email`, `(select .posts order by .created desc
    /// limit 1).title`.
    ///
    /// Compiles the inner select as a flat path traversal
    /// (`compile_path_select` — the same builder a top-level `select
    /// Person.company.name` uses, so forward links, multi-links, backlinks,
    /// junction-backed links and type intersections all come along) and
    /// returns it as one correlated scalar subquery.
    ///
    /// A *partial* subject path (`.emails`) is relative to the enclosing
    /// object, so it is rooted at the enclosing type and correlated back to
    /// the enclosing row by primary key; an absolute one (`Person.name`) is
    /// independent and needs no correlation.
    ///
    /// `extra_fields` are the `.field` steps of an enclosing field-access
    /// chain. They are spliced onto the subject path rather than applied to
    /// its result, so the subquery projects the scalar column itself instead
    /// of an opaque object id.
    fn compile_subquery_expr(
        &mut self,
        stmt: &Stmt,
        extra_fields: &[String],
        ctx: Option<(&TypeDescriptor, &str)>,
        outer_shape: &[ShapeElement],
    ) -> Result<IrExpr, PyQLError> {
        // `(with x := … select …)` — the bindings have nowhere to live in
        // expression position, so they're hoisted to the enclosing
        // statement's own WITH clause and the inner statement takes over.
        if let Stmt::With(w) = stmt {
            for alias in &w.aliases {
                if self.bind_inline_if_correlated(&alias.name, &alias.expr)? {
                    continue;
                }
                let ir_stmt = compile_cte_binding(self, &alias.expr)?;
                let type_name = self.register_cte(&alias.name, &ir_stmt);
                self.hoisted_ctes.push(IrCteDef {
                    name: alias.name.clone(),
                    stmt: ir_stmt,
                    type_name,
                });
            }
            let inner = (*w.stmt).clone();
            return self.compile_subquery_expr(&inner, extra_fields, ctx, outer_shape);
        }

        // `(select (.<a[is T] union .<b[is T]) { id } limit 1)` — the union's
        // operands are correlated to the enclosing row, so the whole thing is
        // read as one set rather than hoisted branch by branch.
        if let Stmt::Select(inner) = stmt
            && let Expr::Shape(sh) = &inner.result
            && let Some(subject) = sh.expr.as_ref()
            && let Some(operands) = Self::union_of_relative_paths(subject)
            && let Some((td, alias)) = ctx
        {
            let elements = sh.elements.clone();
            let mut branches = Vec::with_capacity(operands.len());
            for path in operands {
                let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
                steps.extend(path.steps.iter().cloned());
                let rooted = ast::Path { steps, partial: false };
                let synthetic = ast::SelectStmt {
                    result: Expr::Path(rooted.clone()),
                    filter: inner.filter.clone(),
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    lock: None,
                };
                let mut ps = self.compile_path_select(&synthetic, &rooted, &elements, false)?;
                Self::correlate_path_select(&mut ps, alias);
                branches.push(ps);
            }
            let limit = inner
                .limit
                .as_ref()
                .map(|l| self.compile_free_expr(l))
                .transpose()?
                .map(Box::new);
            return Ok(IrExpr::ObjectPathUnion { branches, limit });
        }
        let Stmt::Select(sel) = stmt else {
            return Err(self.subquery_expr_err(stmt));
        };
        let has_modifiers =
            sel.filter.is_some() || !sel.order_by.is_empty() || sel.offset.is_some() || sel.limit.is_some();

        let Some((path, shape_els)) = Self::split_path_result(&sel.result) else {
            // `(select account::owner(.id) filter … limit 1).name` — the
            // function call is the row source the modifiers apply to.
            if let Expr::FunctionCall(fc) = &sel.result {
                let fc = fc.clone();
                if let Some(ir) = self.try_compile_fn_scalar_subquery(&fc, extra_fields, Some(sel), ctx)? {
                    return Ok(ir);
                }
            }
            // `(select <expression>)` with nothing to traverse is just the
            // expression itself. With modifiers to honour it is a statement,
            // not an expression — `(select count(Visit) filter …)` — so it is
            // hoisted into the enclosing WITH and read back by name.
            if has_modifiers {
                let inner = self.compile_stmt(stmt)?;
                let IrStmt::Select(select) = inner else {
                    return Err(self.subquery_expr_err(stmt));
                };
                if !select
                    .rows
                    .iter()
                    .all(|r| matches!(r, IrRowSource::Free(IrFreeExpr::Scalar(_))))
                {
                    return Err(self.subquery_expr_err(stmt));
                }
                let mut ir = IrExpr::ScalarSubquery(Box::new(select));
                for field in extra_fields {
                    ir = Self::project_free_object_field(ir, field);
                }
                return Ok(ir);
            }
            let mut ir = self.compile_expr_ctx(&sel.result, ctx)?;
            for field in extra_fields {
                ir = Self::project_free_object_field(ir, field);
            }
            return Ok(ir);
        };
        // A shape whose pointers are relative paths declares local names for
        // them, readable by the select's own FILTER/ORDER BY and by whatever
        // projects off it — `(select .locators { handle := [is Handle].handle,
        // latest := [is Handle].latest } filter .latest limit 1).handle`. Each
        // is substituted back into its readers, leaving an ordinary shapeless
        // sub-select behind.
        let rewritten_sel;
        let mut sel = sel;
        let mut shape_els = shape_els;
        let mut extra_steps: Vec<ast::PathStep> = extra_fields.iter().map(|f| ast::PathStep::Name(f.clone())).collect();
        if !shape_els.is_empty()
            && let Some(defs) = Self::shape_alias_paths(shape_els)
        {
            extra_steps = extra_fields
                .iter()
                .flat_map(|field| match defs.iter().find(|(name, _)| name == field) {
                    Some((_, definition)) => definition.steps.clone(),
                    None => vec![ast::PathStep::Name(field.clone())],
                })
                .collect();
            rewritten_sel = ast::SelectStmt {
                result: sel.result.clone(),
                filter: sel.filter.clone().map(|f| Self::substitute_shape_aliases(f, &defs)),
                order_by: sel
                    .order_by
                    .iter()
                    .map(|o| ast::SortExpr {
                        expr: Self::substitute_shape_aliases(o.expr.clone(), &defs),
                        direction: o.direction.clone(),
                        nones: o.nones.clone(),
                    })
                    .collect(),
                offset: sel.offset.clone(),
                limit: sel.limit.clone(),
                lock: sel.lock.clone(),
            };
            sel = &rewritten_sel;
            shape_els = &[];
        }

        // A shape only matters when the sub-select's own value is the
        // result. `(select .emails { address } limit 1).address` projects a
        // column straight back out of it, so the shape says nothing the
        // projection doesn't — the same reading PyQL gives it.
        if !shape_els.is_empty() && extra_fields.is_empty() {
            return Err(self.type_err(
                "a sub-select with a shape is not valid in expression context — \
                 assign it to a computed pointer instead, or project a property \
                 off it, e.g. '(select .emails limit 1).address'",
            ));
        }

        // `(with c := … select c)` — the result is the binding itself, which
        // is a value already, not something to traverse from.
        if !path.partial
            && let [ast::PathStep::Name(name)] = path.steps.as_slice()
            && self.is_value_binding(name)
        {
            let mut ir = self.compile_expr_ctx(&sel.result, ctx)?;
            for field in extra_fields {
                ir = Self::project_free_object_field(ir, field);
            }
            return Ok(ir);
        }

        // `(select Company filter .name = 'Acme' limit 1)` — a bare type
        // select has nothing to traverse; in expression position it stands
        // for the object's primary key, which is what a link's value is.
        if !path.partial
            && extra_fields.is_empty()
            && let [ast::PathStep::Name(type_name)] = path.steps.as_slice()
            && let Ok(root_td) = self.resolve_type(type_name)
        {
            let alias = self.fresh_alias();
            let (filter, order_by, offset, limit) = self.compile_path_modifiers(sel, root_td, &alias)?;
            let source = IrSource {
                poly: None,
                type_name: format!("{}::{}", root_td.module, root_td.name),
                table: root_td.table.clone(),
                alias,
            };
            // Never `pk_returning` here: an empty shape is how the emitter
            // spells an EXISTS inner, so a type with no declared pk would
            // quietly become `SELECT 1` instead of a key.
            let pk = root_td.properties.iter().find(|p| p.is_pk);
            let shape = vec![IrShapePointer::Scalar(IrScalarPointer {
                marker_offset: None,
                alias: "id".to_string(),
                column: pk.map(|p| p.name.clone()).unwrap_or_else(|| "id".to_string()),
                pg_type: pk.map(|p| p.pg_type.clone()).unwrap_or_else(|| "uuid".to_string()),
                tuple_shape: None,
            })];
            let mut select = IrSelect::schema_bound(source, shape, filter);
            select.order_by = order_by;
            select.offset = offset;
            select.limit = limit;
            return Ok(IrExpr::Subquery(Box::new(select)));
        }

        let mut steps = path.steps.clone();
        let correlate = if path.partial {
            let Some((td, alias)) = ctx else {
                return Err(self.type_err(
                    "a relative path in a sub-select needs an enclosing object — \
                     write the type name explicitly, e.g. '(select Person.name)'",
                ));
            };
            steps.insert(0, ast::PathStep::Name(format!("{}::{}", td.module, td.name)));
            Some(alias.to_string())
        } else {
            None
        };
        steps.extend(extra_steps.iter().cloned());
        let full_path = ast::Path { steps, partial: false };

        let mut ps = self.compile_path_select_with_tail(sel, &full_path, outer_shape, false, extra_steps.len())?;
        if let Some(outer_alias) = correlate {
            Self::correlate_path_select(&mut ps, &outer_alias);
        }
        // A shape written after the projection (`(select … limit 1).account
        // { id, name }`) says the object was wanted, not its id, so the walk
        // comes back as one rather than being reduced the way a bare path in
        // expression position is.
        if !outer_shape.is_empty() && matches!(ps.result, IrPathResult::Object { .. }) {
            return Ok(IrExpr::ObjectPathSubquery(Box::new(ps)));
        }
        // `limit 1` is what makes a sub-select over a multi-link single-
        // valued — that is the whole point of `(select .emails filter
        // .primary limit 1).address`. Any other limit, or none, still stands
        // for a set, so it comes back as an array rather than a subquery
        // Postgres would reject the moment a second row showed up.
        let single = matches!(&sel.limit, Some(Expr::Literal(ast::Literal::Int(1))));
        let multi = match &full_path.steps[0] {
            ast::PathStep::Name(root) => {
                let root_td = self.resolve_path_root(root)?;
                self.path_crosses_multi(root_td, &full_path.steps[1..])
            }
            _ => false,
        };
        if multi && !single && matches!(ps.result, IrPathResult::Scalar(..)) {
            return Ok(IrExpr::ArrayFromSelect(Box::new(IrArraySource::PathSelect(Box::new(
                ps,
            )))));
        }
        Ok(IrExpr::PathSubquery(Box::new(ps)))
    }

    /// `account::owner(.id).name` and `(select account::owner(.id) filter …
    /// limit 1).name` — an object-returning user function used inside a
    /// larger expression, projected down to one of its return type's
    /// columns.
    ///
    /// The call itself stays object-valued (there is no way to inline a
    /// function that returns a row set), so it becomes the FROM clause of a
    /// scalar subquery and `field` becomes what that subquery selects. Only
    /// a property of the return type can be projected — reaching further
    /// (`fn(x).company.name`) would need joins the function row source has
    /// no way to express.
    ///
    /// `Ok(None)` when `fc` doesn't name an object-returning function, so
    /// the caller can fall through to ordinary function-call resolution.
    fn try_compile_fn_scalar_subquery(
        &mut self,
        fc: &ast::FunctionCall,
        fields: &[String],
        modifiers: Option<&ast::SelectStmt>,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<Option<IrExpr>, PyQLError> {
        let fd = self.schema.functions.iter().find(|f| {
            let module_matches = fc.module.as_deref().map(|m| m == f.module.as_str()).unwrap_or(true);
            module_matches && f.name == fc.name && f.return_is_object
        });
        let Some(fd) = fd else { return Ok(None) };
        let (fn_module, fn_name, return_type_name, polymorphic, params) = (
            fd.module.clone(),
            fd.name.clone(),
            fd.return_pg_type.clone(),
            fd.return_is_polymorphic,
            fd.params.clone(),
        );

        let qualified = format!("{fn_module}::{fn_name}");
        let [field] = fields else {
            return Err(self.type_err(&format!(
                "function '{qualified}' returns objects, so using it inside an expression needs \
                 one of its properties, e.g. '{qualified}(…).name'"
            )));
        };
        if params.len() != fc.args.len() {
            return Err(self.type_err(&format!(
                "function '{qualified}' expects {} argument(s), got {}",
                params.len(),
                fc.args.len()
            )));
        }

        let mut fn_args = fc
            .args
            .iter()
            .map(|a| self.compile_expr_ctx(a, ctx))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(globals) = self.globals_arg_for_call(&qualified)? {
            fn_args.insert(0, globals);
        }

        let td = self.resolve_type(&return_type_name)?;
        let alias = self.fresh_alias();
        let Some(prop) = Self::resolve_property(td, field) else {
            return Err(self.field_err(field, &return_type_name));
        };
        let projected = IrShapePointer::Computed(IrComputedPointer {
            marker_offset: None,
            alias: field.clone(),
            expr: IrExpr::ColumnRef {
                alias: alias.clone(),
                column: prop.name.clone(),
                pg_type: prop.pg_type.clone(),
            },
        });

        let (poly_implementors, poly_columns) = if polymorphic {
            self.collect_poly_info(&return_type_name)
        } else {
            (vec![], vec![])
        };
        let (filter, order_by, offset, limit) = match modifiers {
            Some(sel) => {
                let td = self.resolve_type(&return_type_name)?;
                self.compile_path_modifiers(sel, td, &alias)?
            }
            None => (None, vec![], None, None),
        };

        Ok(Some(IrExpr::FnSubquery(Box::new(IrFunctionSelect {
            fn_module,
            fn_name,
            fn_args,
            alias,
            type_name: return_type_name,
            polymorphic,
            poly_implementors,
            poly_columns,
            shape: vec![projected],
            filter,
            order_by,
            offset,
            limit,
            distinct: false,
        }))))
    }

    /// Whether a declared computed pointer stands for *objects* rather than a
    /// value — `members := .memberships.member`, `primary_email := (select
    /// .emails filter .primary limit 1)`.
    ///
    /// `*` expands to properties and `**` adds links, so a computed belongs to
    /// whichever side its expression lands on. `ComputedDescriptor` carries no
    /// flag saying which, so the expression has to be walked.
    fn computed_is_object_valued(&self, cd: &crate::schema::ComputedDescriptor, td: &TypeDescriptor) -> bool {
        let Ok(expr) = crate::parse::parse_pointer_expr(&cd.expression) else {
            return false;
        };
        let Some((path, _, _)) = Self::pointer_subject(&expr) else {
            return false;
        };
        if !path.partial {
            return false;
        }
        match path.steps.as_slice() {
            // A backlink names the objects on the other side of the link.
            // Traversing past it (`.<author[is Post].title`) is a value again,
            // which the walk below works out for itself.
            [ast::PathStep::Backlink(_)] | [ast::PathStep::Backlink(_), ast::PathStep::TypeIntersection(_)] => true,
            steps => self.walk_path_types(td, steps, MAX_COMPUTED_SPLICES).1.is_some(),
        }
    }

    /// True when traversing `steps` from `td` crosses a multi-valued step —
    /// a multi-link or a backlink. Such a path stands for a *set*, so in
    /// expression position it has to come back as an array rather than a
    /// scalar subquery (which Postgres would reject at run time the moment a
    /// second row showed up).
    fn path_crosses_multi(&self, td: &TypeDescriptor, steps: &[ast::PathStep]) -> bool {
        self.walk_path_types(td, steps, MAX_COMPUTED_SPLICES).0
    }

    /// Walk `steps` from `td` without compiling anything, reporting whether
    /// any step is multi-valued and what type the walk ends on. A computed
    /// pointer is expanded into the path it stands for — the same splice
    /// `compile_path_select` performs — so `.published.title` is recognized
    /// as multi-valued when `published` resolves to a multi-link.
    ///
    /// Gives up (`None` target) rather than guessing on anything it can't
    /// resolve; the real compile reports the error.
    fn walk_path_types(
        &self,
        td: &'a TypeDescriptor,
        steps: &[ast::PathStep],
        depth: usize,
    ) -> (bool, Option<&'a TypeDescriptor>) {
        let mut current = td;
        let mut multi = false;
        for step in steps {
            match step {
                ast::PathStep::Backlink(_) => return (true, None),
                ast::PathStep::TypeIntersection(tr) => {
                    let name = match &tr.module {
                        Some(m) => format!("{}::{}", m, tr.name),
                        None => tr.name.clone(),
                    };
                    match self.resolve_type(&name) {
                        Ok(t) => current = t,
                        Err(_) => return (multi, None),
                    }
                }
                ast::PathStep::Name(n) => {
                    if let Some(ml) = Self::resolve_multilink(current, n) {
                        multi = true;
                        match self.resolve_type(&ml.target) {
                            Ok(t) => current = t,
                            Err(_) => return (multi, None),
                        }
                        continue;
                    }
                    if let Some(target) = Self::resolve_link(current, n).map(|l| l.target.clone()) {
                        match self.resolve_type(&target) {
                            Ok(t) => current = t,
                            Err(_) => return (multi, None),
                        }
                        continue;
                    }
                    // A computed pointer stands for its own path, or for the
                    // object-returning function it calls.
                    if depth > 0
                        && let Some(cd) = self.resolve_computed(current, n)
                        && let Ok(expr) = crate::parse::parse_pointer_expr(&cd.expression)
                    {
                        if let Some((p, _, modifiers)) = Self::pointer_subject(&expr)
                            && p.partial
                        {
                            let (m, t) = self.walk_path_types(current, &p.steps, depth - 1);
                            // `limit 1` of its own caps the computed at one
                            // row however many its path crosses.
                            let capped = modifiers
                                .is_some_and(|m| matches!(&m.limit, Some(Expr::Literal(ast::Literal::Int(1)))));
                            multi = multi || (m && !capped);
                            match t {
                                Some(t) => current = t,
                                None => return (multi, None),
                            }
                            continue;
                        }
                        if let Some((fc, _)) = Self::function_subject(&expr)
                            && let Some(fd) = self.resolve_object_fn(fc)
                        {
                            multi = multi || fd.return_is_set;
                            match self.resolve_type(&fd.return_pg_type) {
                                Ok(t) => current = t,
                                Err(_) => return (multi, None),
                            }
                            continue;
                        }
                    }
                    // A property (or something unresolvable): the walk ends.
                    return (multi, None);
                }
                _ => return (multi, None),
            }
        }
        (multi, Some(current))
    }

    /// Compile a relative path that the step-by-step expression rules can't
    /// resolve on their own — deeper than two steps, a computed pointer on a
    /// linked type, anything following a backlink — as a single correlated
    /// subquery over the whole traversal.
    ///
    /// `compile_path_select` already implements the general case (forward
    /// links, multi-links, backlinks, junction-backed links, type
    /// intersections, nested tuple fields, "did you mean" on a typo), so the
    /// work here is only to root the path at the enclosing type and
    /// correlate it back to the enclosing row by primary key.
    ///
    /// A leading type intersection is rooted at the *intersected* type
    /// instead: `[is Concrete].col` has to read from Concrete's own table,
    /// which shares the interface row's id.
    fn compile_partial_path_as_subquery(
        &mut self,
        p: &ast::Path,
        td: &TypeDescriptor,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let (root_name, rest) = match p.steps.first() {
            Some(ast::PathStep::TypeIntersection(tr)) => {
                let name = match &tr.module {
                    Some(m) => format!("{}::{}", m, tr.name),
                    None => tr.name.clone(),
                };
                (name, &p.steps[1..])
            }
            _ => (format!("{}::{}", td.module, td.name), &p.steps[..]),
        };
        let root_td = self.resolve_type(&root_name)?;
        let multi = self.path_crosses_multi(root_td, rest);

        let mut steps = vec![ast::PathStep::Name(root_name)];
        steps.extend(rest.iter().cloned());
        let full_path = ast::Path { steps, partial: false };
        let synthetic = ast::SelectStmt {
            result: Expr::Path(full_path.clone()),
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            lock: None,
        };
        let mut ps = self.compile_path_select(&synthetic, &full_path, &[], false)?;
        Self::correlate_path_select(&mut ps, alias);
        if multi && matches!(ps.result, IrPathResult::Scalar(..)) {
            Ok(IrExpr::ArrayFromSelect(Box::new(IrArraySource::PathSelect(Box::new(
                ps,
            )))))
        } else {
            Ok(IrExpr::PathSubquery(Box::new(ps)))
        }
    }

    /// A sub-select written relative to the enclosing object (`(select
    /// .emails filter .primary)`, with or without a shape), as a path select
    /// rooted at that object and correlated back to its row.
    ///
    /// Several places compile a sub-select with no context and so lose the
    /// object a relative path hangs off -- it then resolves in free context
    /// and reports the pointer as unknown. `None` when the statement is not
    /// of that shape, so the caller can carry on as before.
    fn relative_subselect(
        &mut self,
        stmt: &Stmt,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<Option<IrPathSelect>, PyQLError> {
        let (Some((td, alias)), Stmt::Select(sel)) = (ctx, stmt) else {
            return Ok(None);
        };
        let (path, shape): (&ast::Path, &[ShapeElement]) = match &sel.result {
            Expr::Path(p) if p.partial => (p, &[]),
            Expr::Shape(sh) => match sh.expr.as_ref() {
                Some(Expr::Path(p)) if p.partial => (p, sh.elements.as_slice()),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
        steps.extend(path.steps.iter().cloned());
        let rooted = ast::Path { steps, partial: false };
        let synthetic = ast::SelectStmt {
            result: Expr::Path(rooted.clone()),
            filter: sel.filter.clone(),
            order_by: sel.order_by.clone(),
            offset: sel.offset.clone(),
            limit: sel.limit.clone(),
            lock: None,
        };
        let mut ps = self.compile_path_select(&synthetic, &rooted, shape, false)?;
        Self::correlate_path_select(&mut ps, alias);
        Ok(Some(ps))
    }

    /// Tie a path select's root row to the enclosing row by primary key —
    /// what makes a relative path's subquery see only the current object's
    /// side of the graph.
    fn correlate_path_select(ps: &mut IrPathSelect, outer_alias: &str) {
        let correlation = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: ps.root.alias.clone(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef {
                alias: outer_alias.to_string(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
        }));
        ps.filter = Some(match ps.filter.take() {
            Some(existing) => IrExpr::BinOp(Box::new(IrBinOp {
                left: correlation,
                op: ast::BinOpKind::And,
                right: existing,
            })),
            None => correlation,
        });
    }

    /// Names the kind of sub-statement that can't stand in for a value, so
    /// the message points at the actual blocker instead of listing every
    /// statement keyword.
    fn subquery_expr_err(&self, stmt: &Stmt) -> PyQLError {
        let what = match stmt {
            Stmt::Insert(_) => "an insert",
            Stmt::Update(_) => "an update",
            Stmt::Delete(_) => "a delete",
            Stmt::With(_) => "a `with` block",
            Stmt::For(_) => "a `for` loop",
            Stmt::Group(_) => "a `group`",
            _ => "this sub-statement",
        };
        self.type_err(&format!(
            "{what} cannot stand in for a value — a sub-statement is only valid in expression \
             position as a select over a path, e.g. '(select .emails filter .primary limit 1).address'"
        ))
    }

    // ── Expression compilation ────────────────────────────────────────────────────

    /// Single expression compiler for both free and schema-bound contexts.
    /// `ctx = Some((td, alias))` when a schema type + SQL alias are in scope
    /// (enables `.property`/`.link` resolution via `compile_path`); `ctx =
    /// None` for free expressions (set literals, tuples, free objects,
    /// scalar function calls not touching a table), via `compile_free_path`
    /// for the `Path` variant. Most arms are identical either way and just
    /// thread `ctx` through recursive calls; the handful that genuinely
    /// diverge (`Path`, `BinOp`, `FunctionCall`, `Set`, `Detached`, `TypeIs`)
    /// branch internally on `ctx` — see each arm's own comment.
    fn compile_expr_ctx(&mut self, expr: &Expr, ctx: Option<(&TypeDescriptor, &str)>) -> Result<IrExpr, PyQLError> {
        match expr {
            // The main free/schema divergence — kept genuinely two-branched
            // (`compile_path` needs a schema type + alias throughout:
            // __type__, absolute-path rewrite, 2-step link/property
            // traversal, computed-pointer recursion; `compile_free_path`
            // only ever resolves for-vars/CTEs/fn-params/enum members).
            // Both share the bare-name lookup via `resolve_name_ref`.
            Expr::Path(p) => match ctx {
                Some((td, alias)) => self.compile_path(p, td, alias),
                None => self.compile_free_path(p),
            },

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
                let ir_expr = self.compile_expr_ctx(e, ctx)?;
                let ir_index = self.compile_expr_ctx(i, ctx)?;
                let is_array = is_array_expr(&ir_expr);
                Ok(IrExpr::Subscript {
                    expr: Box::new(ir_expr),
                    index: Box::new(ir_index),
                    is_array,
                })
            }

            Expr::Slice {
                expr: e,
                lower: lo,
                upper: hi,
            } => {
                let ir_expr = self.compile_expr_ctx(e, ctx)?;
                let is_array = is_array_expr(&ir_expr);
                let ir_lower = lo.as_ref().map(|x| self.compile_expr_ctx(x, ctx)).transpose()?;
                let ir_upper = hi.as_ref().map(|x| self.compile_expr_ctx(x, ctx)).transpose()?;
                Ok(IrExpr::Slice {
                    expr: Box::new(ir_expr),
                    lower: ir_lower.map(Box::new),
                    upper: ir_upper.map(Box::new),
                    is_array,
                })
            }

            Expr::TypeCast(tc) => {
                // `<AnyType>{}` — an empty set cast to any type, e.g. clearing
                // an optional link (`<Company>{}`) — is always just NULL,
                // regardless of what pg_type the cast target would otherwise
                // resolve to (a schema object type name isn't a scalar cast
                // target at all, so resolve_cast_pg_type couldn't handle it
                // below anyway). Generalizes the same bare-`{}`-in-assignment-
                // position special case in compile_assignments_inner to any
                // expression context.
                if matches!(&tc.expr, Expr::Set(elems) if elems.is_empty()) {
                    return Ok(IrExpr::Null);
                }
                if let ast::TypeExpr::Tuple { elements } = &tc.ty
                    && let Some(ir) = self.try_compile_tuple_literal_cast_ctx(elements, &tc.expr, ctx)?
                {
                    let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                    let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                    return Ok(IrExpr::TypeCast(Box::new(IrTypeCast {
                        expr: ir,
                        pg_type,
                        tuple_shape,
                    })));
                }
                if let ast::TypeExpr::Array { element } = &tc.ty
                    && let Some(ir) = self.try_compile_array_literal_cast_ctx(element, &tc.expr, ctx)?
                {
                    let pg_type = self.resolve_cast_pg_type(&tc.ty)?;
                    return Ok(IrExpr::TypeCast(Box::new(IrTypeCast {
                        expr: ir,
                        pg_type,
                        tuple_shape: None,
                    })));
                }
                let inner = self.compile_expr_ctx(&tc.expr, ctx)?;
                let pg_type = self.resolve_cast_pg_type(&tc.ty)?;

                // PostgreSQL has no native jsonb -> {uuid, date/time family,
                // interval, array<T>} cast (only jsonb -> {bool, numeric
                // family, text} are native as of PG17+) — there's nothing
                // generic to defer to the way `to_jsonb(x)` covers every
                // scalar in the opposite direction, so extract via `#>>'{}'`
                // (the value's raw text form) and cast that, matching what
                // `to_json`'s own emitted text already round-trips (ISO 8601
                // for datetimes, PG's native interval text, a bare UUID
                // string — see `to_jsonb`'s emission for `<json>x`).
                if infer_ir_type(&inner) == Some("jsonb") {
                    if let ast::TypeExpr::Array { element } = &tc.ty {
                        let elem_pg = self.resolve_cast_pg_type(element)?;
                        let sql_template =
                            format!("ARRAY(SELECT (elem #>> '{{}}')::{elem_pg} FROM jsonb_array_elements($1) AS elem)");
                        return Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                            schema: None,
                            name: "jsonb_array_cast".to_string(),
                            args: vec![inner],
                            sql_template: Some(sql_template),
                        }));
                    }
                    if matches!(
                        pg_type.as_str(),
                        "uuid" | "timestamptz" | "timestamp" | "date" | "time" | "interval"
                    ) {
                        let sql_template = format!("(($1 #>> '{{}}'))::{pg_type}");
                        return Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                            schema: None,
                            name: "jsonb_scalar_cast".to_string(),
                            args: vec![inner],
                            sql_template: Some(sql_template),
                        }));
                    }
                }

                let tuple_shape = self.resolve_tuple_cast_shape(&tc.ty);
                Ok(IrExpr::TypeCast(Box::new(IrTypeCast {
                    expr: inner,
                    pg_type,
                    tuple_shape,
                })))
            }

            Expr::BinOp(b) => {
                if let Some((td, alias)) = ctx {
                    if let Some(exists) = self.try_backlink_exists(b, td, alias)? {
                        return Ok(exists);
                    }
                    if let Some(exists) = self.try_multilink_exists(b, td, alias)? {
                        return Ok(exists);
                    }
                }
                // `x in {a, b, c}` / `x not in {a, b, c}`: a set *literal*
                // specifically on the right of in/not-in compiles to a
                // Postgres array, not through the generic Set-literal path
                // below (which hard-errors on any bare set literal in
                // expression position) — In/NotIn's own SQL emission
                // (`= ANY(...)`/`<> ALL(...)`, sql/mod.rs) already expects
                // an array-typed right operand, so this is the one
                // expression position a set literal is actually meaningful
                // in, in either schema-bound or free context.
                if matches!(b.op, ast::BinOpKind::In | ast::BinOpKind::NotIn)
                    && let Expr::Set(elems) = &b.right
                {
                    let left = self.compile_expr_ctx(&b.left, ctx)?;
                    let items = elems
                        .iter()
                        .map(|e| self.compile_expr_ctx(e, ctx))
                        .collect::<Result<Vec<_>, _>>()?;
                    let right = IrExpr::Array(items);
                    return Ok(IrExpr::BinOp(Box::new(IrBinOp {
                        left,
                        op: b.op.clone(),
                        right,
                    })));
                }
                let left = self.compile_expr_ctx(&b.left, ctx)?;
                let right = self.compile_expr_ctx(&b.right, ctx)?;
                // Comparing a value against a *set* — a path that crosses a
                // multi-link or a backlink, which arrives here as an array of
                // its elements — holds when any element matches, which is what
                // the equality means in PyQL and what `= ANY` says in SQL.
                if matches!(b.op, ast::BinOpKind::Eq | ast::BinOpKind::Ne) {
                    let flipped = matches!(left, IrExpr::ArrayFromSelect(_)) && !is_array_expr(&right);
                    let straight = matches!(right, IrExpr::ArrayFromSelect(_)) && !is_array_expr(&left);
                    if flipped || straight {
                        let (value, set) = if straight { (left, right) } else { (right, left) };
                        let membership = IrExpr::BinOp(Box::new(IrBinOp {
                            left: value,
                            op: ast::BinOpKind::In,
                            right: set,
                        }));
                        return Ok(if matches!(b.op, ast::BinOpKind::Ne) {
                            IrExpr::UnaryOp(Box::new(IrUnaryOp {
                                op: ast::UnaryOpKind::Not,
                                operand: membership,
                            }))
                        } else {
                            membership
                        });
                    }
                }
                if let (Some(lt), Some(rt)) = (infer_ir_type(&left), infer_ir_type(&right))
                    && !types_compatible(lt, rt)
                    && !datetime_arithmetic_compatible(&b.op, lt, rt)
                {
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
                Ok(IrExpr::BinOp(Box::new(IrBinOp {
                    left,
                    op: b.op.clone(),
                    right,
                })))
            }

            Expr::FunctionCall(f) => {
                // notify(Channel, payload) / notify_raw(name, payload) → pg_notify(...).
                // Works in both free and schema-bound context (unlike sequence_next
                // below) since a trigger handler's payload needs __new__/__old__,
                // which only ever resolves schema-bound.
                if (f.module.is_none() || f.module.as_deref() == Some("std")) && f.name == "notify" {
                    return self.compile_notify(f, ctx);
                }
                if (f.module.is_none() || f.module.as_deref() == Some("std")) && f.name == "notify_raw" {
                    return self.compile_notify_raw(f, ctx);
                }

                // sequence_next / sequence_reset: type-ref arg → nextval/setval SQL
                // (free-only: schema-bound context never special-cased this).
                if ctx.is_none()
                    && (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && (f.name == "sequence_next" || f.name == "sequence_reset")
                {
                    return self.compile_sequence_fn(f);
                }

                // assert_single(subquery) [free-only] / assert_single|assert_exists|
                // assert_distinct(subquery) [schema-bound] → _pylon.<fn>(ARRAY(subquery)).
                // Preserves the existing asymmetry: free context only ever recognized
                // "assert_single" here, not the other two.
                let assert_names: &[&str] = if ctx.is_some() {
                    &["assert_single", "assert_exists", "assert_distinct"]
                } else {
                    &["assert_single"]
                };
                if (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && assert_names.contains(&f.name.as_str())
                    && !f.args.is_empty()
                    && let Expr::SubQuery(inner_stmt) = &f.args[0]
                {
                    let inner = match self.relative_subselect(inner_stmt, ctx)? {
                        Some(ps) => IrArraySource::PathSelect(Box::new(ps)),
                        None => self.compile_subquery_to_array_source(inner_stmt)?,
                    };
                    let fn_pg = match f.name.as_str() {
                        "assert_single" => "assert_single",
                        "assert_exists" => "assert_exists",
                        _ => "assert_distinct",
                    };
                    return Ok(IrExpr::FunctionCall(IrFunctionCall {
                        schema: Some("_pylon".to_string()),
                        name: fn_pg.to_string(),
                        args: vec![IrExpr::ArrayFromSelect(Box::new(inner))],
                        sql_template: None,
                    }));
                }

                // contains(.multilink.scalar, value) → EXISTS (set-membership
                // semantics; schema-bound only, needs td/alias to resolve the
                // multilink).
                if let Some((td, alias)) = ctx
                    && (f.module.is_none() || f.module.as_deref() == Some("std"))
                    && f.name == "contains"
                    && f.args.len() == 2
                    && let Expr::Path(p) = &f.args[0]
                    && p.partial
                    && p.steps.len() >= 2
                    && let ast::PathStep::Name(ln) = &p.steps[0]
                    && Self::resolve_multilink(td, ln).is_some()
                {
                    let synthetic = ast::BinOp {
                        left: f.args[0].clone(),
                        op: ast::BinOpKind::Eq,
                        right: f.args[1].clone(),
                    };
                    if let Some(exists) = self.try_multilink_exists(&synthetic, td, alias)? {
                        return Ok(exists);
                    }
                }

                // Single-arg aggregate over a schema-shaped source →
                // AggOverQuery. Schema-bound: count(.multilink) correlates via
                // the junction/FK table (`.multilink` isn't an ordinary scalar
                // path). Free: count(TypeName) / count((select TypeName ...))
                // resolves the arg as a schema type reference or subquery.
                if f.args.len() == 1 {
                    let arg = &f.args[0];
                    if let Some((td, alias)) = ctx {
                        // `max(items.created_at)` inside a FILTER: the
                        // aggregate takes the whole set the path names, so it
                        // belongs inside a subquery over that set — a bare
                        // `max(...)` here is an aggregate in a WHERE clause,
                        // which Postgres rejects outright.
                        if let Expr::Path(p) = arg
                            && !p.partial
                            && p.steps.len() > 1
                            && let Some(root) = self.find_path_root_in_expr(arg)
                        {
                            let synthetic = ast::SelectStmt {
                                result: Expr::FunctionCall(f.clone()),
                                filter: None,
                                order_by: vec![],
                                offset: None,
                                limit: None,
                                lock: None,
                            };
                            let ps = self.compile_expr_as_path_select(&synthetic, &synthetic.result, &root, false)?;
                            return Ok(IrExpr::PathSubquery(Box::new(ps)));
                        }
                        if let Expr::Path(p) = arg
                            && p.partial
                            && p.steps.len() == 1
                            && let ast::PathStep::Name(ml_name) = &p.steps[0]
                            && Self::resolve_multilink(td, ml_name).is_some()
                        {
                            use crate::stdlib::{ImplStrategy, lookup};
                            let ns = f.module.as_deref().unwrap_or("std");
                            let overloads = lookup(ns, &f.name);
                            let best = overloads
                                .iter()
                                .find(|d| d.params.len() == 1)
                                .or_else(|| overloads.first());
                            if let Some(ImplStrategy::SqlBuiltin(sql_name)) = best.map(|d| &d.impl_strategy) {
                                let fn_name = sql_name.to_string();
                                let inner = self.multilink_correlation_select(ml_name, td, alias)?;
                                return Ok(IrExpr::AggOverQuery {
                                    fn_name,
                                    inner: Box::new(inner),
                                });
                            }
                        }
                    } else {
                        // `array_agg(a.sessions.id)` with no type in scope: the
                        // aggregate takes the whole set the path names, so it
                        // belongs in a subquery over that traversal. Compiled as
                        // an expression instead, a multi-valued path stands for
                        // the array of its elements, and aggregating *that*
                        // nests it one level deep — the same reason the
                        // schema-bound branch above routes this way.
                        if let Expr::Path(p) = arg
                            && !p.partial
                            && p.steps.len() > 1
                            && let Some(root) = self.find_path_root_in_expr(arg)
                        {
                            let synthetic = ast::SelectStmt {
                                result: Expr::FunctionCall(f.clone()),
                                filter: None,
                                order_by: vec![],
                                offset: None,
                                limit: None,
                                lock: None,
                            };
                            let ps = self.compile_expr_as_path_select(&synthetic, &synthetic.result, &root, false)?;
                            return Ok(IrExpr::PathSubquery(Box::new(ps)));
                        }
                        // `count((delete AuthLink filter .expired))` — the rows
                        // a mutation touched are countable like any other set.
                        // The mutation becomes its own data-modifying CTE, which
                        // Postgres runs regardless, and the aggregate reads it.
                        if let Expr::SubQuery(stmt) = arg
                            && matches!(stmt.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_))
                        {
                            use crate::stdlib::{ImplStrategy, lookup};
                            let ns = f.module.as_deref().unwrap_or("std");
                            let overloads = lookup(ns, &f.name);
                            let best = overloads
                                .iter()
                                .find(|d| d.params.len() == 1)
                                .or_else(|| overloads.first())
                                .cloned();
                            if let Some(d) = best
                                && let ImplStrategy::SqlBuiltin(sql_name) = &d.impl_strategy
                            {
                                let fn_name = sql_name.to_string();
                                let (cte_name, type_name) = self.hoist_dml_as_cte(stmt.as_ref())?;
                                let td = self.resolve_type(&type_name)?;
                                let source = IrSource {
                                    poly: None,
                                    type_name: format!("{}::{}", td.module, td.name),
                                    table: format!("@cte:{cte_name}"),
                                    alias: self.fresh_alias(),
                                };
                                let inner = IrSelect::schema_bound(source, Self::pk_returning(td), None);
                                return Ok(IrExpr::AggOverQuery {
                                    fn_name,
                                    inner: Box::new(inner),
                                });
                            }
                        }
                        let inner_sel: Option<ast::SelectStmt> = match arg {
                            Expr::Path(p) if !p.partial => {
                                // Resolve as a schema type if it matches a known type (not enum).
                                let qname = p
                                    .steps
                                    .iter()
                                    .filter_map(|s| {
                                        if let ast::PathStep::Name(n) = s {
                                            Some(n.as_str())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("::");
                                let is_schema_type = self
                                    .schema
                                    .types
                                    .iter()
                                    .any(|t| format!("{}::{}", t.module, t.name) == qname || t.name == qname);
                                if is_schema_type {
                                    Some(ast::SelectStmt {
                                        result: arg.clone(),
                                        filter: None,
                                        order_by: vec![],
                                        offset: None,
                                        limit: None,
                                        lock: None,
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
                            use crate::stdlib::{ImplStrategy, lookup};
                            let ns = f.module.as_deref().unwrap_or("std");
                            let overloads = lookup(ns, &f.name);
                            let best = overloads
                                .iter()
                                .find(|d| d.params.len() == 1)
                                .or_else(|| overloads.first());
                            if let Some(d) = best
                                && let ImplStrategy::SqlBuiltin(sql_name) = &d.impl_strategy
                            {
                                let fn_name = sql_name.to_string();
                                let inner_ir = self.compile_select(&sel, &sel.result, false)?;
                                return Ok(IrExpr::AggOverQuery {
                                    fn_name,
                                    inner: Box::new(inner_ir),
                                });
                            }
                        }
                    }
                }

                // Set-literal argument → AggOverSet (free-only; a schema-bound
                // Set argument would already hard-error via the Set/Shape arm
                // when compiled below, matching today's behavior — this
                // special case never existed on the schema side).
                if ctx.is_none() {
                    let set_arg_idx = f.args.iter().position(|a| matches!(a, Expr::Set(_)));
                    if let Some(idx) = set_arg_idx
                        && let Expr::Set(set_elems) = &f.args[idx]
                    {
                        use crate::stdlib::{ImplStrategy, lookup};
                        let ns = f.module.as_deref().unwrap_or("std");
                        let overloads = lookup(ns, &f.name);
                        let best = overloads
                            .iter()
                            .find(|d| d.params.len() == f.args.len())
                            .or_else(|| overloads.first());
                        let (schema, fn_name) = match best.map(|d| &d.impl_strategy) {
                            Some(ImplStrategy::SqlBuiltin(sql_name)) => (None, sql_name.to_string()),
                            Some(_) => {
                                return Err(self.type_err(&format!(
                                    "function '{}::{}' cannot be called with a set literal in this context",
                                    ns, f.name
                                )));
                            }
                            None => return Err(self.type_err(&format!("function '{}::{}' does not exist", ns, f.name))),
                        };
                        let elems = set_elems
                            .iter()
                            .map(|e| self.compile_expr_ctx(e, ctx))
                            .collect::<Result<Vec<_>, _>>()?;
                        return Ok(IrExpr::AggOverSet { fn_name, schema, elems });
                    }
                }

                // `any(...)`/`all(...)` say outright that the argument is a
                // set, so the comparison inside must not also be warned about.
                // The argument is compiled before `resolve_fn_call` ever sees
                // which function this is, which is why the guard goes here.
                let is_explicit_set = f.module.as_deref().unwrap_or("std") == "std"
                    && matches!(f.name.as_str(), "any" | "all")
                    && f.args.len() == 1;
                if is_explicit_set {
                    self.explicit_set_depth += 1;
                }
                let args = f
                    .args
                    .iter()
                    .map(|a| self.compile_expr_ctx(a, ctx))
                    .collect::<Result<Vec<_>, _>>();
                if is_explicit_set {
                    self.explicit_set_depth -= 1;
                }
                self.resolve_fn_call(f.module.as_deref(), &f.name, args?)
            }

            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Exists => self.compile_exists_ctx(&u.operand, ctx),

            // `distinct` in expression position: a set-producing operand
            // dedupes its own rows, and a single value — an array, a global, a
            // column — is already distinct, so it stands for itself. (A
            // statement-level `select distinct …` never reaches here; it is
            // unwrapped into the select's own `distinct` flag.)
            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Distinct => {
                let operand = self.compile_expr_ctx(&u.operand, ctx)?;
                Ok(match operand {
                    IrExpr::PathSubquery(mut ps) => {
                        ps.distinct = true;
                        IrExpr::PathSubquery(ps)
                    }
                    IrExpr::ArrayFromSelect(src) => IrExpr::ArrayFromSelect(Box::new(match *src {
                        IrArraySource::Select(mut sel) => {
                            sel.distinct = true;
                            IrArraySource::Select(sel)
                        }
                        IrArraySource::PathSelect(mut ps) => {
                            ps.distinct = true;
                            IrArraySource::PathSelect(ps)
                        }
                        other => other,
                    })),
                    other => other,
                })
            }

            Expr::UnaryOp(u) => {
                let operand = self.compile_expr_ctx(&u.operand, ctx)?;
                Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: u.op.clone(),
                    operand,
                })))
            }

            Expr::IfElse(ie) => {
                let condition = self.compile_expr_ctx(&ie.condition, ctx)?;
                let if_ = self.compile_expr_ctx(&ie.if_expr, ctx)?;
                let else_ = self.compile_expr_ctx(&ie.else_expr, ctx)?;
                Ok(IrExpr::IfElse(Box::new(IrIfElse { condition, if_, else_ })))
            }

            Expr::Array(elems) => {
                let items = elems
                    .iter()
                    .map(|e| self.compile_expr_ctx(e, ctx))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::Array(items))
            }

            Expr::NamedTuple(fields) => {
                let ir = fields
                    .iter()
                    .map(|(name, e)| Ok((name.clone(), self.compile_expr_ctx(e, ctx)?)))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::NamedTuple {
                    fields: ir,
                    is_free_object: false,
                })
            }

            Expr::Tuple(elems) => {
                let ir = elems
                    .iter()
                    .map(|e| self.compile_expr_ctx(e, ctx))
                    .collect::<Result<Vec<_>, PyQLError>>()?;
                Ok(IrExpr::Tuple(ir))
            }

            Expr::FieldAccess { expr: inner, field } => {
                if let Expr::NamedTuple(fields) = inner.as_ref() {
                    let (_, val) = fields.iter().find(|(k, _)| k == field).ok_or_else(|| {
                        self.type_err(&format!("{field} is not a member of {}", named_tuple_type_str(fields)))
                    })?;
                    return self.compile_expr_ctx(val, ctx);
                }
                // `(select .emails filter .primary limit 1).address` — the
                // field chain is spliced onto the sub-select's own path so
                // the subquery projects that column, rather than being read
                // as jsonb extraction off an object id.
                let (base, fields) = Self::peel_field_access_chain(expr);
                if let Expr::SubQuery(stmt) = base {
                    let stmt = stmt.as_ref().clone();
                    return self.compile_subquery_expr(&stmt, &fields, ctx, &[]);
                }
                // `account::owner(.id).name` — an object-returning function
                // projected to one of its columns.
                if let Expr::FunctionCall(fc) = base {
                    let fc = fc.clone();
                    if let Some(ir) = self.try_compile_fn_scalar_subquery(&fc, &fields, None, ctx)? {
                        return Ok(ir);
                    }
                }
                let ir = self.compile_expr_ctx(inner, ctx)?;
                Ok(Self::project_free_object_field(ir, field))
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
                        self.compile_expr_ctx(elem, ctx)
                    }
                    Expr::NamedTuple(fields) => {
                        let (_, val) = fields.get(*index).ok_or_else(|| {
                            self.type_err(&format!("{index} is not a member of {}", named_tuple_type_str(fields)))
                        })?;
                        self.compile_expr_ctx(val, ctx)
                    }
                    // Not a literal to constant-fold — emit a generic runtime
                    // jsonb positional access (`$param.1`, `(<tuple<...>>expr).1`, …).
                    // When the source is a cast to a statically-known tuple type,
                    // bounds-check the index against its arity at compile time
                    // (e.g. `2 is not a member of tuple<std::int64, std::str>`).
                    _ => {
                        if let Expr::TypeCast(tc) = inner.as_ref()
                            && let Some(shape) = self.resolve_tuple_cast_shape(&tc.ty)
                            && *index >= shape.members.len()
                        {
                            return Err(self.type_err(&format!(
                                "{index} is not a member of {}",
                                self.type_expr_to_display_str(&tc.ty)
                            )));
                        }
                        let ir = self.compile_expr_ctx(inner, ctx)?;
                        Ok(IrExpr::JsonbIndex {
                            expr: Box::new(ir),
                            index: *index,
                        })
                    }
                }
            }

            // `detached` bypasses the implicit root-matches-td correlation
            // rewrite. Schema-bound: if `inner` contains a type-rooted path,
            // compile it as an independent PathSubquery; otherwise (or when
            // already free) fall back to compiling `inner` with NO schema
            // binding — note this fallback is deliberately `None`, not
            // `ctx`, since `detached` means "evaluate independently of the
            // enclosing scope" and free context has no rooted path to find
            // in the first place (`find_path_root_in_expr` only ever matches
            // against a resolvable schema type name).
            Expr::Detached(inner) => {
                if ctx.is_some()
                    && let Some(root) = self.find_path_root_in_expr(inner)
                {
                    let synthetic = ast::SelectStmt {
                        result: (**inner).clone(),
                        filter: None,
                        order_by: vec![],
                        offset: None,
                        limit: None,
                        lock: None,
                    };
                    let ps = self.compile_expr_as_path_select(&synthetic, inner, &root, false)?;
                    return Ok(IrExpr::PathSubquery(Box::new(ps)));
                }
                self.compile_expr_ctx(inner, None)
            }

            // Free context tolerates an empty set (-> Null) or a singleton
            // set (-> its one element) as a convenience; a schema-bound
            // expression position hard-errors on ANY set literal instead
            // (see the generic Shape|Set arm below) — a plausibly
            // intentional semantic difference, preserved exactly as-is.
            Expr::Set(elems) if ctx.is_none() && elems.is_empty() => Ok(IrExpr::Null),

            Expr::Set(elems) if ctx.is_none() => {
                let compiled: Result<Vec<_>, _> = elems.iter().map(|e| self.compile_expr_ctx(e, ctx)).collect();
                let mut compiled = compiled?;
                if compiled.len() == 1 {
                    Ok(compiled.remove(0))
                } else {
                    Err(self.type_err("multi-element set literal is not supported in free SELECT context"))
                }
            }

            // A free object literal (`{ foo := 'bar' }`, no subject type) is
            // valid anywhere an expression is, not just as a whole SELECT's
            // result — e.g. nested inside a computed shape element. Compiled
            // the same way `compile_free_select` treats it at the top level
            // (each field compiled independently, ctx propagated so a
            // schema-bound nested free object can still reference `.name`
            // etc.), just wrapped as `IrExpr::NamedTuple` (jsonb) instead of
            // a whole result row, since here it's a value, not a row source.
            // is_free_object: true — this came from curly-brace shape syntax,
            // not a paren tuple literal, so the value-shape-tag tree
            // (pylon/query.py's shape_value_tags) can tell the frontend to
            // render it as an expandable "Object {...}", not a `(...)`
            // tuple literal (see ShapeNode::NamedTuple's own doc comment).
            Expr::Shape(s) if s.expr.is_none() => {
                let fields = s
                    .elements
                    .iter()
                    .map(|el| -> Result<(String, IrExpr), PyQLError> {
                        let name = path_leaf(&el.path)?.to_string();
                        let expr = el.compexpr.as_ref().ok_or_else(|| {
                            self.type_err("free object field must have a value expression (':= expr')")
                        })?;
                        let compiled = match expr {
                            Expr::SubQuery(stmt)
                                if matches!(stmt.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) =>
                            {
                                self.dml_as_value(stmt.as_ref())?
                            }
                            other => match self.free_object_link_field(other)? {
                                Some(object) => object,
                                None => self.compile_expr_ctx(other, ctx)?,
                            },
                        };
                        Ok((name, compiled))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::NamedTuple {
                    fields,
                    is_free_object: true,
                })
            }

            // A shape applied to a WITH-bound free object (`test := test {
            // test2 }` where `test` isn't a schema object, just a free
            // binding) projects the named fields out of it — each element
            // either reads the underlying field (by name, via the same
            // per-field CTE column `.field` access uses) or, with a `:=`
            // override, compiles a brand new value in the current context,
            // exactly like a fresh free-object-literal field would.
            Expr::Shape(s) if matches!(&s.expr, Some(inner) if self.is_free_cte_ref(inner)) => {
                let Some(Expr::Path(root_path)) = &s.expr else {
                    unreachable!()
                };
                let ast::PathStep::Name(root) = &root_path.steps[0] else {
                    unreachable!()
                };
                let fields = s
                    .elements
                    .iter()
                    .map(|el| -> Result<(String, IrExpr), PyQLError> {
                        let name = path_leaf(&el.path)?.to_string();
                        let expr = match &el.compexpr {
                            Some(over) => self.compile_expr_ctx(over, ctx)?,
                            None => match self.resolve_cte_field_chain(root, &[name.as_str()]) {
                                Some(result) => result?,
                                None => {
                                    return Err(self.type_err(&format!("free object '{root}' has no field '{name}'")));
                                }
                            },
                        };
                        Ok((name, expr))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrExpr::NamedTuple {
                    fields,
                    is_free_object: true,
                })
            }

            // `(.<prices[is Listing] union .<sale_prices[is Listing]) { id }`
            // — each operand hangs off the enclosing row, so none can be
            // hoisted into a CTE the way a standalone union's operands are.
            Expr::Shape(sh)
                if ctx.is_some()
                    && matches!(sh.expr.as_ref(), Some(Expr::Union(_, _)))
                    && Self::union_of_relative_paths(sh.expr.as_ref().expect("checked")).is_some() =>
            {
                let (td, alias) = ctx.expect("checked by the guard");
                let operands =
                    Self::union_of_relative_paths(sh.expr.as_ref().expect("checked")).expect("checked by the guard");
                let elements = sh.elements.clone();
                let mut branches = Vec::with_capacity(operands.len());
                for path in operands {
                    let mut steps = vec![ast::PathStep::Name(format!("{}::{}", td.module, td.name))];
                    steps.extend(path.steps.iter().cloned());
                    let rooted = ast::Path { steps, partial: false };
                    let synthetic = ast::SelectStmt {
                        result: Expr::Path(rooted.clone()),
                        filter: None,
                        order_by: vec![],
                        offset: None,
                        limit: None,
                        lock: None,
                    };
                    let mut ps = self.compile_path_select(&synthetic, &rooted, &elements, false)?;
                    Self::correlate_path_select(&mut ps, alias);
                    branches.push(ps);
                }
                Ok(IrExpr::ObjectPathUnion { branches, limit: None })
            }

            Expr::Shape(sh) if matches!(sh.expr.as_ref(), Some(Expr::SubQuery(_))) => {
                let sh = sh.clone();
                match self.shape_over_subquery(&sh)? {
                    Some(ir) => Ok(ir),
                    None => Err(PyQLError::Type(PyQLTypeError {
                        message: "shapes and set literals are not valid in expression context".into(),
                        position: Position { line: 0, col: 0 },
                    })),
                }
            }

            Expr::Shape(sh) if Self::shape_over_subquery_projection(sh).is_some() => {
                let (stmt, fields) = Self::shape_over_subquery_projection(sh).expect("checked by the guard");
                let stmt = stmt.clone();
                let elements = sh.elements.clone();
                self.compile_subquery_expr(&stmt, &fields, ctx, &elements)
            }

            // A bare shape or set literal is never valid in expression
            // position, in either context — preserved exactly as the
            // schema-bound side always enforced (the free side's more
            // permissive empty/singleton-set handling above is the one
            // deliberate exception, handled before this arm).
            Expr::Shape(_) | Expr::Set(_) => Err(PyQLError::Type(PyQLTypeError {
                message: "shapes and set literals are not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            // A `select` over a path compiles to a correlated subquery; DML
            // and everything else still has no expression-position meaning.
            // Union/Except previously fell through free's generic "not valid
            // in free SELECT context" catch-all — these explicit,
            // purpose-written messages (already used schema-bound) apply
            // equally well with no schema in scope, so they're unconditional
            // here rather than ctx-gated.
            Expr::SubQuery(stmt) => {
                let stmt = stmt.as_ref().clone();
                self.compile_subquery_expr(&stmt, &[], ctx, &[])
            }

            Expr::Union(_, _) => Err(PyQLError::Type(PyQLTypeError {
                message: "union is not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            Expr::Except(_, _) => Err(PyQLError::Type(PyQLTypeError {
                message: "except is not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            // TypeIs (`expr is Type`) is schema-exclusive — compile_type_is
            // deeply needs td/alias throughout (interface checks, __type__
            // column, alias-scoped bool expr). No free-context equivalent
            // existed before the merge (fell to the generic catch-all); this
            // is a new, clearer explicit error for that case.
            Expr::TypeIs { expr, ty } => match ctx {
                Some((td, alias)) => self.compile_type_is(expr, ty, td, alias),
                None => Err(self.type_err("'is' type check is not valid in free SELECT context")),
            },
        }
    }

    fn compile_expr(&mut self, expr: &Expr, td: &TypeDescriptor, alias: &str) -> Result<IrExpr, PyQLError> {
        self.compile_expr_ctx(expr, Some((td, alias)))
    }

    fn compile_free_expr(&mut self, expr: &Expr) -> Result<IrExpr, PyQLError> {
        self.compile_expr_ctx(expr, None)
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

        let (ty_module, ty_name) = ty
            .as_named()
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
        let (source_table, source_abstract, source_materialized, poly_implementors, poly_columns) = {
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
                src_td
                    .properties
                    .iter()
                    .map(|p| p.name.clone())
                    .chain(
                        src_td
                            .links
                            .iter()
                            .filter(|l| !l.is_junction_backed())
                            .map(|l| format!("{}_id", l.name)),
                    )
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
            poly: None,
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
    fn type_check_bool_expr(&self, source_qname: &str, check_qname: &str, td: &TypeDescriptor, alias: &str) -> IrExpr {
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

    /// Shared lookup for a bare 1-step name: for-loop variable, CTE binding,
    /// or function parameter. Used by both `compile_path`'s schema-bound
    /// prefix and `compile_free_path`.
    ///
    /// `allow_fn_param` used to be `false` from `compile_path`, so a parameter
    /// resolved only in free context. An object-returning body like
    /// `select Item filter .rank = v` compiles its filter against a schema
    /// anchor, so it never consulted `fn_params` and rejected the bare `v` as
    /// "absolute paths are not valid in expression context" — while the same
    /// parameter in a scalar-returning body worked, which is why the gap went
    /// unnoticed. A bare one-step name can only be a variable, CTE binding or
    /// parameter in either context (a property is always written `.name`), so
    /// there is nothing here for a parameter to shadow.
    fn resolve_name_ref(&self, name: &str, allow_fn_param: bool) -> Option<IrExpr> {
        if self.for_vars.contains_key(name) {
            return Some(IrExpr::ForVar { name: name.to_string() });
        }
        // A free-object-bound CTE (`with x := { a := 1 } select ... x ...`),
        // referenced bare with no shape to project through, has nothing to
        // expose — a free *object* needs an explicit shape to know what to
        // return (unlike a tuple/named tuple, it has no "default"
        // projection), so this collapses to an empty free object, the same
        // as `Expr::Shape`'s `is_free_cte_ref` case for `x { ... }`.
        // It also sidesteps a real problem: this CTE has no "v" column
        // (only `IrFreeExpr::Scalar` CTEs get one), so falling through to
        // the generic `IrExpr::CteRef` below would reference a column that
        // doesn't exist.
        if let Some(IrFreeExpr::FreeObject(_)) = self.cte_free_items.get(name) {
            return Some(IrExpr::NamedTuple {
                fields: vec![],
                is_free_object: true,
            });
        }
        if let Some(ir) = self.inline_bindings.get(name) {
            return Some(ir.clone());
        }
        if let Some(t) = self.cte_types.get(name) {
            // A scalar binding records its pg type here (see `cte_stmt_type`);
            // an object binding records a `module::Type` name, which is not a
            // pg type and must not be handed to type inference.
            let scalar = !t.contains("::");
            return Some(IrExpr::CteRef {
                name: name.to_string(),
                scalar,
                pg_type: (scalar && !t.is_empty()).then(|| literal_sentinel_to_pg(t).to_string()),
            });
        }
        if allow_fn_param && let Some(pg_type) = self.fn_params.get(name) {
            return Some(IrExpr::FnParam {
                name: name.to_string(),
                pg_type: pg_type.clone(),
            });
        }
        None
    }

    /// The `(qualified type, alias)` an absolute `root.prop` should resolve
    /// against when the innermost select is `detached`.
    ///
    /// `None` in the ordinary case — the innermost select is not detached, or
    /// nothing outside it binds that type, in which case naming the type still
    /// means the current row.
    fn enclosing_anchor(&self, root: &str) -> Option<(String, String)> {
        let innermost = self.anchors.last()?;
        if !innermost.detached || (innermost.type_name != root && innermost.qualified != root) {
            return None;
        }
        self.anchors
            .iter()
            .rev()
            .skip(1)
            .find(|a| a.type_name == root || a.qualified == root)
            .map(|a| (a.qualified.clone(), a.alias.clone()))
    }

    fn compile_path(&mut self, p: &ast::Path, td: &TypeDescriptor, alias: &str) -> Result<IrExpr, PyQLError> {
        if !p.partial {
            if p.steps.len() == 1
                && let ast::PathStep::Name(n) = &p.steps[0]
            {
                if let Some(ir) = self.resolve_name_ref(n, true) {
                    return Ok(ir);
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
            // Enum member access: `default::Gender.Female`
            if p.steps.len() == 2
                && let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice()
                && self.resolve_enum(type_ref).is_some()
            {
                return self.compile_enum_access(type_ref, variant);
            }
            // `root.field1.field2...` where `root` is a WITH-bound free object.
            if let Some(resolved) = self.resolve_cte_path(p) {
                return resolved;
            }
            // `root.field1.field2...` where `root` is a WITH-bound *schema
            // object* (`with person := (select detached Person filter ...)
            // select ... person.company.name ...`). `compile_path_select`
            // already treats a CTE-bound root exactly like a real type name
            // (it checks `cte_types` before falling back to `resolve_type`,
            // via the `@cte:` source-table sentinel — see its own doc
            // comment and `compile_expr_as_path_select`'s sibling case for
            // `Detached`), so it already implements the *entire* general
            // path-traversal feature set here — forward links, multilinks,
            // backlinks, junction-backed links, nested tuple field access,
            // "did you mean" on a typo'd name — not just a one-property
            // special case. Wrapping its result as `IrExpr::PathSubquery`
            // (the same vehicle `Detached` uses for a type-rooted path in
            // expression position) turns the whole traversal into one
            // correlated scalar/id expression.
            if p.steps.len() > 1
                && let ast::PathStep::Name(root) = &p.steps[0]
                && self
                    .cte_types
                    .get(root.as_str())
                    .map(|t| t.contains("::"))
                    .unwrap_or(false)
            {
                let full_path = ast::Path {
                    steps: p.steps.clone(),
                    partial: false,
                };
                let synthetic = ast::SelectStmt {
                    result: Expr::Path(full_path.clone()),
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    lock: None,
                };
                let ps = self.compile_path_select(&synthetic, &full_path, &[], false)?;
                return Ok(IrExpr::PathSubquery(Box::new(ps)));
            }
            // `__new__.prop` / `__old__.prop` — the inserted/updated/deleted
            // row a trigger handler is compiled against (see
            // `compile_trigger_handler`, which populates `special_anchors`
            // per the trigger's declared `on` events). Same rewrite-and-
            // recurse trick as the `TypeName.prop` case below, just handing
            // `compile_path` the anchor's own alias ("NEW"/"OLD") instead
            // of `td`'s. Outside trigger-handler compilation (or for the
            // anchor the current trigger's events don't legally bind —
            // e.g. `__old__` in an Insert-only trigger) `special_anchors`
            // is empty/missing that entry, so this falls through to the
            // explicit error below rather than the generic "absolute
            // paths" message: `__old__`/`__new__` cannot be used in
            // this expression.
            if p.steps.len() > 1
                && let ast::PathStep::Name(root) = &p.steps[0]
                && (root == "__new__" || root == "__old__")
            {
                if let Some((anchor_td, anchor_alias)) = self.special_anchors.get(root).cloned() {
                    let relative = ast::Path {
                        steps: p.steps[1..].to_vec(),
                        partial: true,
                    };
                    return self.compile_path(&relative, anchor_td, &anchor_alias);
                }
                return Err(PyQLError::Resolution(PyQLResolutionError::UnknownField(
                    PyQLUnknownFieldError {
                        message: format!("{root} cannot be used in this expression"),
                        position: Position { line: 0, col: 0 },
                    },
                )));
            }
            // `__subject__` is the row a constraint is checked against, which
            // is the same row a relative path reads.
            if p.steps.len() > 1 && matches!(&p.steps[0], ast::PathStep::Name(root) if root == "__subject__") {
                let relative = ast::Path {
                    steps: p.steps[1..].to_vec(),
                    partial: true,
                };
                return self.compile_path(&relative, td, alias);
            }
            // Absolute path rooted at the current td: `TypeName.prop` inside a schema-bound
            // expression (e.g. the value side of a BinOp in compile_expr_as_path_select).
            // Rewrite to a relative path and compile normally.
            if p.steps.len() > 1
                && let ast::PathStep::Name(root) = &p.steps[0]
            {
                let qualified = format!("{}::{}", td.module, td.name);
                if *root == td.name || *root == qualified {
                    let relative = ast::Path {
                        steps: p.steps[1..].to_vec(),
                        partial: true,
                    };
                    // Inside a `detached` select, naming its own type means the
                    // enclosing select's row — that is what makes an anti-join
                    // compare two different rows.
                    if let Some((outer_qualified, outer_alias)) = self.enclosing_anchor(root) {
                        let outer_td = self.resolve_type(&outer_qualified)?.clone();
                        return self.compile_path(&relative, &outer_td, &outer_alias);
                    }
                    return self.compile_path(&relative, td, alias);
                }
            }
            // `membership.account.id` — a walk off a for-loop variable in
            // expression position. `compile_path_select` already knows to
            // start such a walk from the row the variable holds, so this is
            // that walk read as one subquery.
            if let Some(ast::PathStep::Name(var)) = p.steps.first()
                && p.steps.len() > 1
                && self.for_var_types.contains_key(var)
                && !matches!(p.steps[1], ast::PathStep::TypeIntersection(_))
            {
                let synthetic = ast::SelectStmt {
                    result: Expr::Path(p.clone()),
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    lock: None,
                };
                let ps = self.compile_path_select(&synthetic, p, &[], false)?;
                return Ok(IrExpr::PathSubquery(Box::new(ps)));
            }
            // `o[is Organization]` on a for-loop variable — the narrowing is
            // a filter, not a traversal: the row is read from the narrowed
            // type's own table by the key the variable holds, so a variable
            // bound to something else yields nothing, as it should.
            if let [ast::PathStep::Name(var), ast::PathStep::TypeIntersection(type_ref)] = p.steps.as_slice()
                && self.for_var_types.contains_key(var)
            {
                let type_name = match &type_ref.module {
                    Some(m) => format!("{}::{}", m, type_ref.name),
                    None => type_ref.name.clone(),
                };
                let narrowed = self.resolve_type(&type_name)?;
                let narrowed_alias = self.fresh_alias();
                let source = IrSource {
                    poly: self.poly_fanout_for(&format!("{}::{}", narrowed.module, narrowed.name)),
                    type_name: format!("{}::{}", narrowed.module, narrowed.name),
                    table: narrowed.table.clone(),
                    alias: narrowed_alias.clone(),
                };
                let filter = IrExpr::BinOp(Box::new(IrBinOp {
                    left: IrExpr::ColumnRef {
                        alias: narrowed_alias,
                        column: "id".to_string(),
                        pg_type: "uuid".to_string(),
                    },
                    op: ast::BinOpKind::Eq,
                    right: IrExpr::ForVar { name: var.clone() },
                }));
                return Ok(IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                    source,
                    Self::pk_returning(narrowed),
                    Some(filter),
                ))));
            }
            return Err(PyQLError::Type(PyQLTypeError {
                message: "absolute paths are not valid in expression context; use .name".into(),
                position: Position { line: 0, col: 0 },
            }));
        }

        // A bare `@prop` — the junction row of the multi-link whose own
        // modifiers are being compiled. Read from the junction alias the
        // emitter puts in scope there ("jt"), the same one a `@prop` in the
        // nested shape reads.
        if p.partial
            && let [ast::PathStep::LinkProp(prop_name)] = p.steps.as_slice()
        {
            return self.compile_link_prop_ref(prop_name);
        }

        // Type intersection in expression: [is Type].name — scalar subquery
        if p.partial && matches!(p.steps.first(), Some(ast::PathStep::TypeIntersection(_))) {
            return self.compile_type_intersection_expr(&p.steps, td, alias);
        }

        if p.steps.len() == 2 {
            return self.compile_path_2step(p, td, alias);
        }

        // Three or more steps: no single column to read, so the whole
        // traversal becomes one correlated subquery.
        if p.steps.len() != 1 {
            return self.compile_partial_path_as_subquery(p, td, alias);
        }

        let pointer_name = match &p.steps[0] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "type intersections are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }));
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
            if link.is_junction_backed() {
                return self.junction_target_id_expr(td, link, alias);
            }
            // FK column reference (uuid) — e.g. `.company` → `t0."company_id"`
            return Ok(IrExpr::ColumnRef {
                alias: alias.to_string(),
                column: format!("{}_id", link.name),
                pg_type: "uuid".to_string(),
            });
        }

        // Schema-defined computed pointer: inline the expression in place.
        if let Some(cd) = self.resolve_computed(td, pointer_name) {
            let expr_ast = crate::parse::parse_pointer_expr(&cd.expression).map_err(PyQLError::Syntax)?;
            return self.compile_expr(&expr_ast, td, alias);
        }

        Err(self.field_err(pointer_name, &format!("{}::{}", td.module, td.name)))
    }

    /// Free-context counterpart of `compile_path` — no schema type/alias in
    /// scope, so only for-loop variables, CTE bindings, function parameters,
    /// and enum member access (`default::Gender.Female`) are resolvable;
    /// anything property/link-shaped is a hard error.
    fn compile_free_path(&mut self, p: &ast::Path) -> Result<IrExpr, PyQLError> {
        if p.partial {
            // A WITH binding inside a computed pointer or a nested SELECT is
            // compiled without a type in scope, but `.name` there still means
            // the innermost enclosing set — the one the anchor stack holds.
            if let Some((qualified, alias)) = self.anchors.last().map(|a| (a.qualified.clone(), a.alias.clone())) {
                let td = self.resolve_type(&qualified)?;
                return self.compile_path(p, td, &alias);
            }
            return Err(self.type_err(
                "property reference (.name) is not valid in free SELECT; \
                 use a schema-bound SELECT instead",
            ));
        }
        if p.steps.len() == 2
            && let [ast::PathStep::Name(type_ref), ast::PathStep::Name(variant)] = p.steps.as_slice()
            && self.resolve_enum(type_ref).is_some()
        {
            return self.compile_enum_access(type_ref, variant);
        }
        // `root.field1.field2...` where `root` is a WITH-bound free object
        // (any length >= 2, including chains through nested free objects).
        if let Some(resolved) = self.resolve_cte_path(p) {
            return resolved;
        }
        if p.steps.len() == 1
            && let ast::PathStep::Name(n) = &p.steps[0]
            && let Some(ir) = self.resolve_name_ref(n, true)
        {
            return Ok(ir);
        }
        Err(self.type_err("expression is not valid in free SELECT context"))
    }

    fn compile_path_2step(&mut self, p: &ast::Path, td: &TypeDescriptor, alias: &str) -> Result<IrExpr, PyQLError> {
        let link_name = match &p.steps[0] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "type intersections are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }));
            }
        };
        let pointer_name = match &p.steps[1] {
            ast::PathStep::Name(n) => n.as_str(),
            _ => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: "type intersections are not valid in expression context".into(),
                    position: Position { line: 0, col: 0 },
                }));
            }
        };

        if let Some(link) = Self::resolve_link(td, link_name) {
            if pointer_name == "id" {
                if link.is_junction_backed() {
                    return self.junction_target_id_expr(td, link, alias);
                }
                return Ok(IrExpr::ColumnRef {
                    alias: alias.to_string(),
                    column: format!("{}_id", link_name),
                    pg_type: "uuid".to_string(),
                });
            }
            let target_td = self.resolve_type(&link.target)?;
            if let Some(prop) = Self::resolve_property(target_td, pointer_name) {
                let ft_alias = self.fresh_alias();
                let target_id_expr = if link.is_junction_backed() {
                    self.junction_target_id_expr(td, link, alias)?
                } else {
                    IrExpr::ColumnRef {
                        alias: alias.to_string(),
                        column: format!("{}_id", link_name),
                        pg_type: "uuid".to_string(),
                    }
                };
                return Ok(IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                    IrSource {
                        poly: None,
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: ft_alias.clone(),
                    },
                    vec![IrShapePointer::Scalar(IrScalarPointer {
                        marker_offset: None,
                        alias: prop.name.clone(),
                        column: prop.name.clone(),
                        pg_type: prop.pg_type.clone(),
                        tuple_shape: self.resolve_property_tuple_shape(prop),
                    })],
                    Some(IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::ColumnRef {
                            alias: ft_alias.clone(),
                            column: "id".to_string(),
                            pg_type: "uuid".to_string(),
                        },
                        op: ast::BinOpKind::Eq,
                        right: target_id_expr,
                    }))),
                ))));
            }
            // Not a stored column on the target — a computed pointer, or a
            // further link. The general traversal builder resolves both.
            return self.compile_partial_path_as_subquery(p, td, alias);
        }

        // A multi-link path stands for a set of values. Inside a comparison
        // it has already been rewritten to an EXISTS (`try_multilink_exists`,
        // which runs first); everywhere else it is an array.
        if Self::resolve_multilink(td, link_name).is_some() {
            return self.compile_partial_path_as_subquery(p, td, alias);
        }

        // Not a stored pointer at all — a computed one, most likely, which
        // the general builder can traverse through by splicing in the path
        // it stands for. It raises the same "no link or property" error this
        // used to when the name really is unknown (which is what made a
        // computed head read as `has no link or property 'x'. Did you mean
        // 'x'?` — the suggester could see it, the resolver couldn't).
        self.compile_partial_path_as_subquery(p, td, alias)
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
            if is_backlink(p) {
                (p.steps.as_slice(), &b.right, false)
            } else {
                return Ok(None);
            }
        } else if let Expr::Path(p) = &b.right {
            if is_backlink(p) {
                (p.steps.as_slice(), &b.left, true)
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };
        let value_expr = self.compile_expr(value_ast, td, alias)?;
        let current_qname = format!("{}::{}", td.module, td.name);
        let exists = self.compile_backlink_as_exists(
            path_steps,
            Some((b.op.clone(), value_expr, flip)),
            &current_qname,
            alias,
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
        // Without a type intersection the backlink spans every type that
        // declares the link at this target, so the row qualifies if any one of
        // them points at it.
        let Some(PathStep::TypeIntersection(type_ref)) = steps.get(1) else {
            // Without a type intersection the backlink spans every type that
            // declares the link at this target.
            return self.backlink_exists_over_owners(
                None,
                &backlink_name,
                &steps[1..],
                comparison,
                current_qname,
                alias,
            );
        };

        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let target_td = self.resolve_type(&type_name)?;
        if self.declares_backlink(target_td, &backlink_name, current_qname) {
            return self.backlink_exists_for_owner(
                target_td,
                &backlink_name,
                &steps[2..],
                comparison,
                current_qname,
                alias,
            );
        }
        // The intersection narrows to an interface or mixin that does not
        // declare the link itself — its implementors do, and a row of one of
        // them satisfies `[is ThatType]` all the same. Qualified, because that
        // is how an implementor names what it implements.
        let narrow_to = format!("{}::{}", target_td.module, target_td.name);
        self.backlink_exists_over_owners(
            Some(&narrow_to),
            &backlink_name,
            &steps[2..],
            comparison,
            current_qname,
            alias,
        )
    }

    /// Does `td` declare the link a backlink names, pointing at the type the
    /// traversal is standing on?
    fn declares_backlink(&self, td: &TypeDescriptor, backlink_name: &str, current_qname: &str) -> bool {
        td.links
            .iter()
            .any(|l| l.name == backlink_name && self.link_target_reaches(&l.target, current_qname))
            || td
                .multilinks
                .iter()
                .any(|ml| ml.name == backlink_name && self.link_target_reaches(&ml.target, current_qname))
    }

    /// EXISTS over every type that declares the backlink's link, OR-ed
    /// together: a row qualifies if any one of them points at it. `narrow_to`
    /// keeps only the types that satisfy an `[is …]` the path asked for.
    #[allow(clippy::too_many_arguments)]
    fn backlink_exists_over_owners(
        &mut self,
        narrow_to: Option<&str>,
        backlink_name: &str,
        rest: &[ast::PathStep],
        comparison: Option<(ast::BinOpKind, IrExpr, bool)>,
        current_qname: &str,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let schema = self.schema;
        let owners: Vec<&'a TypeDescriptor> = schema
            .types
            .iter()
            .filter(|t| !t.abstract_)
            .filter(|t| narrow_to.is_none_or(|q| Self::is_or_implements(t, q)))
            .filter(|t| self.declares_backlink(t, backlink_name, current_qname))
            .collect();
        if owners.is_empty() {
            return Err(self.type_err(&match narrow_to {
                Some(q) => format!("type {q} has no link or multi-link '{backlink_name}' pointing to {current_qname}"),
                None => format!("no type has a link or multi-link '{backlink_name}' pointing to {current_qname}"),
            }));
        }
        let mut combined: Option<IrExpr> = None;
        for owner_td in owners {
            let one = self.backlink_exists_for_owner(
                owner_td,
                backlink_name,
                rest,
                comparison.clone(),
                current_qname,
                alias,
            )?;
            combined = Some(match combined {
                None => one,
                Some(previous) => IrExpr::BinOp(Box::new(IrBinOp {
                    left: previous,
                    op: ast::BinOpKind::Or,
                    right: one,
                })),
            });
        }
        Ok(combined.expect("owners is non-empty"))
    }

    /// Is `td` the type `qname` names, or one that implements/extends it?
    fn is_or_implements(td: &TypeDescriptor, qname: &str) -> bool {
        format!("{}::{}", td.module, td.name) == qname
            || td.interfaces.iter().any(|i| i == qname)
            || td.parents.iter().any(|p| p == qname)
    }

    /// One owner type's half of `compile_backlink_as_exists`: EXISTS over the
    /// rows of `target_td` that link back to `alias`, plus whatever the
    /// remaining path steps and comparison require of them.
    #[allow(clippy::too_many_arguments)]
    fn backlink_exists_for_owner(
        &mut self,
        target_td: &'a TypeDescriptor,
        backlink_name: &str,
        rest: &[ast::PathStep],
        comparison: Option<(ast::BinOpKind, IrExpr, bool)>,
        current_qname: &str,
        alias: &str,
    ) -> Result<IrExpr, PyQLError> {
        let backlink_name = backlink_name.to_string();
        let target_qname = format!("{}::{}", target_td.module, target_td.name);
        let target_table = target_td.table.clone();
        let t_alias = self.fresh_alias();

        // Backlink source is either a single (FK) link or a multi-link
        // (junction table) on the target type — mirrors the equivalent
        // link-vs-multilink resolution the general shape-position backlink
        // code already does (see the `PathStep::Backlink` handling above in
        // `compile_path_expr`/similar), which this filter/exists-specific
        // path previously didn't: it only ever checked `target_td.links`,
        // so a self-referential-multilink backlink like `Person.friends`
        // failed to compile here even though it worked in a shape position.
        let join_cond = if let Some(l) = target_td
            .links
            .iter()
            .find(|l| l.name == backlink_name && self.link_target_reaches(&l.target, current_qname))
        {
            if l.is_junction_backed() {
                // Same junction-table EXISTS shape the multi-link branch
                // below uses — no direct FK column, since this link is
                // itself junction-backed.
                let (jt_table, jt_module, jt_owner_col, jt_current_col, _) = self.link_junction_info(target_td, l)?;
                let jt_alias = self.fresh_alias();
                IrExpr::UnaryOp(Box::new(IrUnaryOp {
                    op: ast::UnaryOpKind::Exists,
                    operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                        IrSource {
                            poly: None,
                            type_name: format!("{}::__jt__", jt_module),
                            table: jt_table,
                            alias: jt_alias.clone(),
                        },
                        vec![],
                        Some(IrExpr::BinOp(Box::new(IrBinOp {
                            left: IrExpr::BinOp(Box::new(IrBinOp {
                                left: IrExpr::ColumnRef {
                                    alias: jt_alias.clone(),
                                    column: jt_owner_col,
                                    pg_type: "uuid".to_string(),
                                },
                                op: ast::BinOpKind::Eq,
                                right: IrExpr::ColumnRef {
                                    alias: t_alias.clone(),
                                    column: "id".to_string(),
                                    pg_type: "uuid".to_string(),
                                },
                            })),
                            op: ast::BinOpKind::And,
                            right: IrExpr::BinOp(Box::new(IrBinOp {
                                left: IrExpr::ColumnRef {
                                    alias: jt_alias,
                                    column: jt_current_col,
                                    pg_type: "uuid".to_string(),
                                },
                                op: ast::BinOpKind::Eq,
                                right: IrExpr::ColumnRef {
                                    alias: alias.to_string(),
                                    column: "id".to_string(),
                                    pg_type: "uuid".to_string(),
                                },
                            })),
                        }))),
                    ))),
                }))
            } else {
                let fk_col = format!("{}_id", backlink_name);
                // Join condition: target.fk_col = current.id
                IrExpr::BinOp(Box::new(IrBinOp {
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
                }))
            }
        } else if let Some(ml) = target_td
            .multilinks
            .iter()
            .find(|ml| ml.name == backlink_name && self.link_target_reaches(&ml.target, current_qname))
            .cloned()
        {
            // Junction row connects t_alias (as the multi-link's owner/
            // "source") to the current row (as its "target") — no direct FK
            // column on either table, so this is a nested EXISTS over the
            // junction table rather than a simple column comparison.
            let (jt_table, jt_module, _, _, _) = self.multilink_junction_info(target_td, &ml)?;
            let jt_alias = self.fresh_alias();
            IrExpr::UnaryOp(Box::new(IrUnaryOp {
                op: ast::UnaryOpKind::Exists,
                operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                    IrSource {
                        poly: None,
                        type_name: format!("{}::__jt__", jt_module),
                        table: jt_table,
                        alias: jt_alias.clone(),
                    },
                    vec![],
                    Some(IrExpr::BinOp(Box::new(IrBinOp {
                        left: IrExpr::BinOp(Box::new(IrBinOp {
                            left: IrExpr::ColumnRef {
                                alias: jt_alias.clone(),
                                column: "source".to_string(),
                                pg_type: "uuid".to_string(),
                            },
                            op: ast::BinOpKind::Eq,
                            right: IrExpr::ColumnRef {
                                alias: t_alias.clone(),
                                column: "id".to_string(),
                                pg_type: "uuid".to_string(),
                            },
                        })),
                        op: ast::BinOpKind::And,
                        right: IrExpr::BinOp(Box::new(IrBinOp {
                            left: IrExpr::ColumnRef {
                                alias: jt_alias,
                                column: "target".to_string(),
                                pg_type: "uuid".to_string(),
                            },
                            op: ast::BinOpKind::Eq,
                            right: IrExpr::ColumnRef {
                                alias: alias.to_string(),
                                column: "id".to_string(),
                                pg_type: "uuid".to_string(),
                            },
                        })),
                    }))),
                ))),
            }))
        } else {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!(
                    "type {} has no link or multi-link '{}' pointing to {}",
                    target_qname, backlink_name, current_qname,
                ),
                position: Position { line: 0, col: 0 },
            }));
        };

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
            operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: target_qname,
                    table: target_table,
                    alias: t_alias,
                },
                vec![],
                Some(filter),
            ))),
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
                let col = if link.is_junction_backed() {
                    self.junction_target_id_expr(target_td, link, t_alias)?
                } else {
                    IrExpr::ColumnRef {
                        alias: t_alias.to_string(),
                        column: format!("{}_id", link.name),
                        pg_type: "uuid".to_string(),
                    }
                };
                return Ok(Some(Self::apply_comparison(col, comparison)));
            }
            return Err(self.field_err(pointer_name, target_qname));
        }

        // Two forward steps: link then property (single FK join)
        if let [PathStep::Name(link_name), PathStep::Name(prop_name)] = steps
            && let Some(link) = Self::resolve_link(target_td, link_name)
        {
            let link_target = link.target.clone();
            let target_id_expr = if link.is_junction_backed() {
                self.junction_target_id_expr(target_td, link, t_alias)?
            } else {
                IrExpr::ColumnRef {
                    alias: t_alias.to_string(),
                    column: format!("{}_id", link_name),
                    pg_type: "uuid".to_string(),
                }
            };
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
                    right: target_id_expr,
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
                    operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                        IrSource {
                            poly: None,
                            type_name: link_target_qname,
                            table: link_target_table,
                            alias: l_alias,
                        },
                        vec![],
                        Some(full),
                    ))),
                }))));
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
        // A bare `.multilink` counts: comparing the link itself to an object
        // (`filter any(.emails = email)`) is the same set-membership question
        // as comparing something reached through it, just with no tail.
        fn ml_first_name(steps: &[ast::PathStep]) -> Option<&str> {
            match steps.first()? {
                ast::PathStep::Name(n) => Some(n.as_str()),
                _ => None,
            }
        }

        let (path_steps, value_ast, flip) = if let Expr::Path(p) = &b.left {
            if p.partial {
                if let Some(ln) = ml_first_name(&p.steps) {
                    if Self::resolve_multilink(td, ln).is_some() {
                        (p.steps.as_slice(), &b.right, false)
                    } else {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        } else if let Expr::Path(p) = &b.right {
            if p.partial {
                if let Some(ln) = ml_first_name(&p.steps) {
                    if Self::resolve_multilink(td, ln).is_some() {
                        (p.steps.as_slice(), &b.left, true)
                    } else {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };

        let value_expr = self.compile_expr(value_ast, td, alias)?;
        let ml_name = match &path_steps[0] {
            ast::PathStep::Name(n) => n.clone(),
            _ => return Ok(None),
        };
        let ml = Self::resolve_multilink(td, &ml_name).unwrap();

        // Warn: multi-link traversal in a comparison returns a set, not a single boolean.
        // The query works (compiled as EXISTS), but `any()` makes the intent explicit.
        if self.explicit_set_depth == 0 {
            let pointer_path: Vec<_> = path_steps
                .iter()
                .map(|s| match s {
                    ast::PathStep::Name(n) => n.as_str(),
                    _ => "?",
                })
                .collect();
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
                // See `junction_info_for`'s doc comment: owner-derived,
                // never `through_td.table` itself.
                (
                    format!("{}.{}", td_table, ml_name),
                    td_module.clone(),
                    "source".to_string(),
                    "target".to_string(),
                )
            } else {
                let source_qname = format!("{}::{}", td_module, td_name);
                let src_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == source_qname)
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to {source_qname}"),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
                    .name
                    .clone();
                let tgt_col = through_td
                    .links
                    .iter()
                    .find(|l| l.target == ml_target && l.name != src_col)
                    .or_else(|| through_td.links.iter().find(|l| l.target == ml_target))
                    .ok_or_else(|| {
                        PyQLError::Type(PyQLTypeError {
                            message: format!("through type {through_qname} has no link to {ml_target}"),
                            position: Position { line: 0, col: 0 },
                        })
                    })?
                    .name
                    .clone();
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
            poly: None,
            type_name: format!("{}::__jt__", jt_module),
            table: jt_table,
            alias: jt_alias,
        };
        let inner = IrExpr::Subquery(Box::new(IrSelect::schema_bound(jt_source, vec![], Some(full_filter))));

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
        // No tail at all — the multi-link itself is what is being compared,
        // so the junction row's target id is the whole answer.
        if steps.is_empty() {
            let col_ref = IrExpr::ColumnRef {
                alias: jt_alias.to_string(),
                column: jt_tgt_col.to_string(),
                pg_type: "uuid".to_string(),
            };
            let (left, right) = if flip {
                (value_expr, col_ref)
            } else {
                (col_ref, value_expr)
            };
            return Ok(IrExpr::BinOp(Box::new(IrBinOp { left, op, right })));
        }

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
                let (l, r) = if flip {
                    (value_expr, col_ref)
                } else {
                    (col_ref, value_expr)
                };
                return Ok(IrExpr::BinOp(Box::new(IrBinOp { left: l, op, right: r })));
            }
            // Check if it's a link (object) rather than a scalar
            // A single link is compared by the foreign key it stores, the
            // same way `.link = obj` is one step higher up
            // (`any(.access_grants.account = account)`). A multi-link has no
            // column to compare and would need a junction of its own.
            let single_link = target_td
                .links
                .iter()
                .find(|l| l.name == first_name && !l.is_junction_backed());
            if single_link.is_none() && target_td.multilinks.iter().any(|l| l.name == first_name) {
                let target_display = target_type.replace("::", ".");
                return Err(PyQLError::Type(PyQLTypeError {
                    message: format!(
                        "operator '{op}' cannot be applied to operands of type '{target_display}' and the value type",
                        op = op,
                    ),
                    position: Position { line: 0, col: 0 },
                }));
            }
            let (prop_name, prop_pg) = match single_link {
                Some(link) => (format!("{}_id", link.name), "uuid".to_string()),
                None => {
                    let prop = target_td
                        .properties
                        .iter()
                        .find(|p| p.name == first_name)
                        .ok_or_else(|| self.field_err(&first_name, target_type))?;
                    (prop.name.clone(), prop.pg_type.clone())
                }
            };
            let tgt_alias = self.fresh_alias();
            // Build EXISTS(SELECT 1 FROM target WHERE target.id = jt.target AND target.prop op value)
            let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: tgt_alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ColumnRef {
                    alias: jt_alias.to_string(),
                    column: jt_tgt_col.to_string(),
                    pg_type: "uuid".to_string(),
                },
            }));
            let prop_col = IrExpr::ColumnRef {
                alias: tgt_alias.clone(),
                column: prop_name,
                pg_type: prop_pg,
            };
            let (pl, pr) = if flip {
                (value_expr, prop_col)
            } else {
                (prop_col, value_expr)
            };
            let prop_filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: pl,
                op,
                right: pr,
            }));
            let full = IrExpr::BinOp(Box::new(IrBinOp {
                left: id_filter,
                op: ast::BinOpKind::And,
                right: prop_filter,
            }));
            let inner = IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: target_type.to_string(),
                    table: target_table,
                    alias: tgt_alias,
                },
                vec![],
                Some(full),
            )));
            return Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                op: ast::UnaryOpKind::Exists,
                operand: inner,
            })));
        }

        // steps.len() >= 2: first_name must be a single link (not multi)
        if target_td.multilinks.iter().any(|l| l.name == first_name) {
            return Err(self.type_err("nested multi-link traversal in comparison is not yet supported"));
        }
        let link = target_td
            .links
            .iter()
            .find(|l| l.name == first_name)
            .ok_or_else(|| self.field_err(&first_name, target_type))?;
        if link.is_junction_backed() {
            // A junction-backed link's correlation is a subquery, not a
            // plain FK column, and this recursive path-tail resolver is
            // built entirely around passing a (alias, column) pair down to
            // the next level — supporting it here would need a broader
            // signature change. Fail loudly rather than silently reference
            // a `{name}_id` column that doesn't exist for this link.
            return Err(self.type_err(&format!(
                "filtering through a junction-backed single link ('{first_name}') nested inside \
                 a multi-link path comparison is not yet supported — filter on '.{first_name}' \
                 directly instead"
            )));
        }
        let next_target = link.target.clone();
        let fk_col = format!("{}_id", first_name);
        let tgt_alias = self.fresh_alias();

        // FK optimisation for [single_link, "id"]:
        if steps.len() == 2
            && let Some(ast::PathStep::Name(n)) = steps.get(1)
            && n == "id"
        {
            // Compare tgt.{fk_col} (the FK in current target) directly
            // We need an EXISTS over the target to access fk_col
            // Actually: EXISTS(target WHERE target.id = jt.target AND target.{fk_col} op value)
            let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: IrExpr::ColumnRef {
                    alias: tgt_alias.clone(),
                    column: "id".to_string(),
                    pg_type: "uuid".to_string(),
                },
                op: ast::BinOpKind::Eq,
                right: IrExpr::ColumnRef {
                    alias: jt_alias.to_string(),
                    column: jt_tgt_col.to_string(),
                    pg_type: "uuid".to_string(),
                },
            }));
            let fk_ref = IrExpr::ColumnRef {
                alias: tgt_alias.clone(),
                column: fk_col,
                pg_type: "uuid".to_string(),
            };
            let (fl, fr) = if flip {
                (value_expr, fk_ref)
            } else {
                (fk_ref, value_expr)
            };
            let fk_filter = IrExpr::BinOp(Box::new(IrBinOp {
                left: fl,
                op,
                right: fr,
            }));
            let full = IrExpr::BinOp(Box::new(IrBinOp {
                left: id_filter,
                op: ast::BinOpKind::And,
                right: fk_filter,
            }));
            let inner = IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                IrSource {
                    poly: None,
                    type_name: target_type.to_string(),
                    table: target_table,
                    alias: tgt_alias,
                },
                vec![],
                Some(full),
            )));
            return Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
                op: ast::UnaryOpKind::Exists,
                operand: inner,
            })));
        }

        // General case: EXISTS(target WHERE target.id = jt.jt_tgt_col AND <tail_filter for steps[1..]>)
        // Recursive call uses tgt_alias.fk_col as the "pointer to the next type's id"
        let id_filter = IrExpr::BinOp(Box::new(IrBinOp {
            left: IrExpr::ColumnRef {
                alias: tgt_alias.clone(),
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            },
            op: ast::BinOpKind::Eq,
            right: IrExpr::ColumnRef {
                alias: jt_alias.to_string(),
                column: jt_tgt_col.to_string(),
                pg_type: "uuid".to_string(),
            },
        }));
        let nested_filter =
            self.compile_path_tail_filter(&steps[1..], op, value_expr, flip, &next_target, &tgt_alias, &fk_col)?;
        let full = IrExpr::BinOp(Box::new(IrBinOp {
            left: id_filter,
            op: ast::BinOpKind::And,
            right: nested_filter,
        }));
        let inner = IrExpr::Subquery(Box::new(IrSelect::schema_bound(
            IrSource {
                poly: None,
                type_name: target_type.to_string(),
                table: target_table,
                alias: tgt_alias,
            },
            vec![],
            Some(full),
        )));
        Ok(IrExpr::UnaryOp(Box::new(IrUnaryOp {
            op: ast::UnaryOpKind::Exists,
            operand: inner,
        })))
    }

    /// Compile `UNLESS CONFLICT [ON expr] [ELSE (UPDATE …)]` into `IrConflict`.
    fn compile_conflict(
        &mut self,
        uc: &ast::UnlessConflict,
        td: &TypeDescriptor,
    ) -> Result<(IrConflict, Vec<IrMultiLinkMutation>), PyQLError> {
        // ON clause: compile with empty alias → bare column name (`"col"` not `"t0"."col"`)
        // so the emitter produces `ON CONFLICT ("name")` not `ON CONFLICT ("t0"."name")`.
        let on = uc.on.as_ref().map(|e| self.compile_expr(e, td, "")).transpose()?;
        let mut appends = vec![];
        let do_update = match uc.else_.as_ref() {
            Some(e) => Some(self.compile_conflict_else(e, &mut appends)?),
            None => None,
        };
        Ok((IrConflict { on, do_update }, appends))
    }

    /// Compile the ELSE clause of UNLESS CONFLICT, which must be `(UPDATE Type SET { … })`.
    ///
    /// Assignments are compiled with the target table's own bare name (not
    /// schema-qualified, and not the usual `t0`-style fresh alias) as the
    /// qualifying "alias" so a self-referencing RHS (`.stock` in `stock :=
    /// .stock + 1`) resolves unambiguously to the *existing* conflicting
    /// row. A genuinely bare, unqualified column reference here is
    /// ambiguous in Postgres between the existing row and the `excluded`
    /// pseudo-row (confirmed live: "column reference ... is ambiguous")
    /// even though only one of the two is ever actually reachable this way
    /// (nothing here ever compiles a reference to `excluded`) — Postgres's
    /// own docs describe exactly this qualification: "the existing row
    /// using the table's name (or an alias)," no explicit `AS` needed on
    /// the INSERT target for that self-reference to work.
    fn compile_conflict_else(
        &mut self,
        expr: &Expr,
        appends: &mut Vec<IrMultiLinkMutation>,
    ) -> Result<Vec<(String, IrExpr)>, PyQLError> {
        let Expr::SubQuery(stmt) = expr else {
            return Err(self.type_err("UNLESS CONFLICT ELSE must be an UPDATE expression, e.g. ELSE (UPDATE …)"));
        };
        let Stmt::Update(upd) = stmt.as_ref() else {
            return Err(self.type_err("UNLESS CONFLICT ELSE must be an UPDATE expression"));
        };
        let type_name = self.expr_as_type_name(&upd.subject)?;
        let upd_td = self.resolve_type(&type_name)?;
        // Filter on the ELSE UPDATE is ignored — PostgreSQL infers the conflicting
        // row from the ON CONFLICT target automatically.
        let table = upd_td.table.clone();
        // A multi-link cannot be written by `DO UPDATE SET` — junction rows
        // are separate DML. Appending them to the enclosing insert instead
        // applies them to whichever row comes back, inserted or conflicting,
        // and the junction insert is `ON CONFLICT DO NOTHING`, so the
        // inserted branch (which already wrote the same rows from its own
        // shape) is unaffected.
        let mut scalar_shape = vec![];
        for el in &upd.shape {
            let pointer_name = path_leaf(&el.path)?;
            let Some(ml) = Self::resolve_multilink(upd_td, pointer_name) else {
                scalar_shape.push(el.clone());
                continue;
            };
            if el.op == ShapeOp::Remove {
                return Err(self.type_err(&format!(
                    "cannot use `-=` for multi-link '{pointer_name}' inside an UNLESS CONFLICT \
                     ELSE clause; the rows to remove are not known until the conflict resolves"
                )));
            }
            let Some(value) = &el.compexpr else { continue };
            let (jt, module, src_col, tgt_col, through_td) = self.multilink_junction_info(upd_td, ml)?;
            let values = self.compile_multilink_values(value, upd_td, &table, through_td)?;
            appends.push(IrMultiLinkMutation {
                junction_table: jt,
                module,
                source_col: src_col,
                target_col: tgt_col,
                values,
                single: false,
            });
        }
        self.compile_assignments_for_update(&scalar_shape, upd_td, &table)
    }

    /// Compile a nested INSERT/UPDATE/DELETE into its own CTE and return that
    /// CTE's name. Postgres cannot run DML inside another statement's value
    /// list, so it is hoisted into the enclosing statement's `WITH` and read
    /// back from there — see `pending_nested_ctes`.
    /// Compile a DML statement into a CTE of the query's own `WITH`.
    ///
    /// Unlike `hoist_nested_dml`, whose CTE belongs to the enclosing
    /// INSERT/UPDATE it was nested inside, this one has no enclosing statement
    /// to attach to — the select that names it *is* the top level.
    fn hoist_dml_as_cte(&mut self, stmt: &Stmt) -> Result<(String, String), PyQLError> {
        let type_name = self.dml_subject_type(stmt)?;
        let inner = self.compile_stmt(stmt)?;
        let cte_name = self.fresh_nested_cte_name();
        self.hoisted_ctes.push(IrCteDef {
            name: cte_name.clone(),
            stmt: inner,
            type_name: type_name.clone(),
        });
        Ok((cte_name, type_name))
    }

    fn hoist_nested_dml(&mut self, stmt: &Stmt) -> Result<String, PyQLError> {
        let type_name = self.dml_subject_type(stmt)?;
        let inner = self.compile_stmt(stmt)?;
        let cte_name = self.fresh_nested_cte_name();
        self.pending_nested_ctes.push(IrCteDef {
            name: cte_name.clone(),
            stmt: inner,
            type_name,
        });
        Ok(cte_name)
    }

    /// Compile `(SELECT TargetType FILTER …)` as a scalar subquery for use in a
    /// link assignment (`company := (SELECT Company FILTER .name = $co)`).
    /// Returns `IrExpr::Subquery` whose shape is the target pk — the SQL emitter
    /// renders this as `(SELECT "alias"."id" FROM … WHERE …)`.
    fn compile_link_subquery(&mut self, stmt: &Stmt) -> Result<IrExpr, PyQLError> {
        // `preferences := (insert Preferences { … })` — the nested DML used
        // directly as the value, which is the same hoist the wrapped
        // `select (insert …) { id }` form below goes through.
        if matches!(stmt, Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) {
            let cte_name = self.hoist_nested_dml(stmt)?;
            return Ok(IrExpr::ColumnRef {
                alias: cte_name,
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            });
        }
        let Stmt::Select(sel) = stmt else {
            return Err(self.type_err(
                "only SELECT is valid as a link assignment value; \
                 use SELECT (INSERT …) { id } to assign from a DML result",
            ));
        };

        // `SELECT (INSERT …) { id }` / `SELECT (UPDATE …) { id }` /
        // `SELECT (DELETE …) { id }` — the exact workaround this function's
        // own error message above recommends. Postgres has no way to run a
        // nested INSERT/UPDATE/DELETE inside another statement's value list
        // without hoisting it into a `WITH` CTE first, so that's what this
        // does: compile the inner DML as its own statement, stash it in
        // `self.pending_nested_ctes` under a fresh CTE name (drained by
        // whichever `compile_insert`/`compile_update` is compiling the
        // assignment this value belongs to — see those functions' own doc
        // comments — and prepended as a `WITH` CTE by the emitter, which
        // also switches the outer statement's own row source from
        // `VALUES (...)` / a bare `SET` to something that can actually
        // reference it), and return a plain reference to that CTE's `id`
        // column in place of the subquery.
        //
        // A prior attempt to handle this by delegating to `compile_select`
        // (which does know how to chain a `dml_source`, but only for a
        // *top-level* `SELECT (INSERT …) { ... }` statement) compiled
        // without error but silently emitted a subquery that dropped the
        // nested INSERT and selected an unrelated, arbitrary pre-existing
        // row instead — do not repeat that approach.
        let nested_dml = match &sel.result {
            Expr::SubQuery(inner) if matches!(inner.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) => {
                Some(inner.as_ref())
            }
            Expr::Shape(s) => match s.expr.as_ref() {
                Some(Expr::SubQuery(inner))
                    if matches!(inner.as_ref(), Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_)) =>
                {
                    Some(inner.as_ref())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(inner_stmt) = nested_dml {
            let cte_name = self.hoist_nested_dml(inner_stmt)?;
            return Ok(IrExpr::ColumnRef {
                alias: cte_name,
                column: "id".to_string(),
                pg_type: "uuid".to_string(),
            });
        }

        let type_name = self.expr_as_type_name(&sel.result)?;
        let td = self.resolve_type(&type_name)?;
        let alias = self.fresh_alias();

        let filter = sel
            .filter
            .as_ref()
            .map(|f| self.compile_expr(f, td, &alias))
            .transpose()?;

        Ok(IrExpr::Subquery(Box::new(IrSelect::schema_bound(
            IrSource {
                poly: None,
                type_name: format!("{}::{}", td.module, td.name),
                table: td.table.clone(),
                alias,
            },
            Self::pk_returning(td),
            filter,
        ))))
    }

    /// Shared `IrSort` builder for both schema-bound (`compile_sort`) and
    /// free (`compile_free_select`'s order-by) contexts — direction/nulls
    /// translation is identical either way, only the expr compiler ctx differs.
    fn compile_sort_ctx(
        &mut self,
        s: &ast::SortExpr,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<IrSort, PyQLError> {
        Ok(IrSort {
            expr: self.compile_expr_ctx(&s.expr, ctx)?,
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

    fn compile_sort(&mut self, s: &ast::SortExpr, td: &TypeDescriptor, alias: &str) -> Result<IrSort, PyQLError> {
        self.compile_sort_ctx(s, Some((td, alias)))
    }

    // ── Stdlib function resolution ────────────────────────────────────────────────

    /// Look up `name` in the stdlib (namespace = `module` or `"std"`) and produce
    /// the correct `IrExpr::FunctionCall` based on the matching `ImplStrategy`.
    /// Falls through to a plain call if no overload is found (unknown / PG built-in).
    fn resolve_fn_call(&mut self, module: Option<&str>, name: &str, args: Vec<IrExpr>) -> Result<IrExpr, PyQLError> {
        use crate::stdlib::{ImplStrategy, lookup};

        let ns = module.unwrap_or("std");
        // `any`/`all` aggregate a *set* of booleans. Everything that reaches
        // here is a single value — a multi-link comparison has already become
        // its own EXISTS — and a single boolean is its own any()/all();
        // `bool_or` over it would be an aggregate where SQL allows none.
        if ns == "std" && matches!(name, "any" | "all") && args.len() == 1 && !is_array_expr(&args[0]) {
            return Ok(args.into_iter().next().expect("checked by the guard"));
        }
        let overloads = lookup(ns, name);

        // Pick the overload whose parameter types best match the argument types.
        // Fall back to the first registered overload when no type info is available.
        let best = overloads
            .iter()
            .find(|d| {
                d.params.len() == args.len() && d.params.iter().zip(&args).all(|(p, a)| pylon_type_matches(a, &p.ty))
            })
            .or_else(|| overloads.first());

        let (schema, resolved_name, sql_template) = if let Some(desc) = best {
            match &desc.impl_strategy {
                ImplStrategy::SqlBuiltin(sql_name) => (None, sql_name.to_string(), None),
                ImplStrategy::SqlExpression(tmpl) => (None, name.to_string(), Some(tmpl.to_string())),
                ImplStrategy::PylonFunction(def) => (Some("_pylon".to_string()), def.name.to_string(), None),
                // Binary infix operator: emit as a template so sql/mod.rs's
                // generic FunctionCall path (which only ever calls
                // `schema.name(args)`) doesn't swallow the operator symbol —
                // previously unreachable in practice (only `std::overlaps`
                // used this strategy, untested, and would have silently
                // resolved to a nonexistent `"std".overlaps(...)` call).
                ImplStrategy::SqlOperator(op) if args.len() == 2 => {
                    (None, name.to_string(), Some(format!("($1 {op} $2)")))
                }
                ImplStrategy::TranspilerIntrinsic(intrinsic) => {
                    return self.compile_range_intrinsic(intrinsic, name, args);
                }
                // TranspilerIntrinsic: pass through; handled elsewhere
                _ => (module.map(str::to_string), name.to_string(), None),
            }
        } else {
            // Fall back to user-defined scalar functions. Overload
            // resolution is by (module, name, argument count) only — true
            // Postgres-style resolution by argument *type* isn't
            // implemented (a call whose args happen to have the right
            // count for the wrong-typed overload still picks that one,
            // relying on the forced TypeCast below rather than erroring).
            // Candidates are searched for one whose param count actually
            // matches the call — taking just the first (module, name)
            // match regardless of arg count used to silently pick the
            // wrong overload's signature (and then error on arg count)
            // whenever an earlier-declared overload happened to have a
            // different arity than the one actually being called
            // (confirmed live: a 2-arg call to an overload set whose
            // first-declared member takes 1 arg reported "expects 1
            // argument(s), got 2" even though a 2-arg overload existed).
            let candidates: Vec<&FunctionDescriptor> = self
                .schema
                .functions
                .iter()
                .filter(|f| {
                    let module_matches = module.map(|m| m == f.module.as_str()).unwrap_or(true);
                    module_matches && f.name == name && !f.return_is_object
                })
                .collect();
            let user_fn = candidates
                .iter()
                .find(|f| f.params.len() == args.len())
                .copied()
                .or_else(|| candidates.first().copied());
            if let Some(fd) = user_fn {
                if fd.params.len() != args.len() {
                    return Err(self.type_err(&format!(
                        "function '{}::{}' expects {} argument(s), got {}",
                        fd.module,
                        fd.name,
                        fd.params.len(),
                        args.len()
                    )));
                }
                let cast_args = fd
                    .params
                    .iter()
                    .zip(args)
                    .map(|(p, a)| {
                        IrExpr::TypeCast(Box::new(super::IrTypeCast {
                            expr: a,
                            pg_type: p.pg_type.clone(),
                            tuple_shape: None,
                        }))
                    })
                    .collect();
                let qualified = format!("{}::{}", fd.module, fd.name);
                let fn_module = fd.module.clone();
                let fn_name = fd.name.clone();
                let mut call_args: Vec<IrExpr> = cast_args;
                if let Some(globals) = self.globals_arg_for_call(&qualified)? {
                    call_args.insert(0, globals);
                }
                return Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: Some(fn_module),
                    name: fn_name,
                    args: call_args,
                    sql_template: None,
                }));
            }
            // An object-returning function is deliberately not a candidate
            // above — it compiles through `try_compile_fn_object_select`, which
            // only runs when the call is a select's subject. Saying "does not
            // exist" for one that plainly does (and naming `default::` for a
            // call the user wrote unqualified) sent people looking for a typo
            // instead of at the restriction.
            if let Some(fd) = self.schema.functions.iter().find(|f| {
                let module_matches = module.map(|m| m == f.module.as_str()).unwrap_or(true);
                module_matches && f.name == name && f.return_is_object
            }) {
                return Err(self.type_err(&format!(
                    "function '{}::{}' returns objects, so it can only be the subject of a \
                     select (`select {}::{}(…) {{ … }}`), not part of a larger expression",
                    fd.module, fd.name, fd.module, fd.name
                )));
            }
            let qualified = match module {
                Some(m) => format!("{m}::{name}"),
                None => name.to_string(),
            };
            return Err(self.type_err(&format!("function '{qualified}' does not exist")));
        };

        Ok(IrExpr::FunctionCall(super::IrFunctionCall {
            schema,
            name: resolved_name,
            args,
            sql_template,
        }))
    }

    /// Resolve `std::range(...)`/`std::multirange(...)` — `TranspilerIntrinsic`
    /// entries with no real backing function anywhere (no `_pylon` function,
    /// no bare PostgreSQL builtin of that literal name). PostgreSQL has no
    /// single polymorphic range constructor — the concrete constructor
    /// (`int8range`, `numrange`, `tsrange`, `tstzrange`, `daterange`, and
    /// their multirange counterparts) is chosen here from the resolved
    /// element type of the arguments, since there's nothing generic to defer
    /// to at the SQL level the way `to_jsonb(x)` covers "cast to json".
    fn compile_range_intrinsic(&self, intrinsic: &str, name: &str, args: Vec<IrExpr>) -> Result<IrExpr, PyQLError> {
        match intrinsic {
            "range" => {
                let point_ty = args.first().and_then(infer_ir_type).ok_or_else(|| {
                    self.type_err(
                        "range(): cannot infer the element type of the first argument — \
                     use an explicit cast, e.g. range(<int64>$lower, <int64>$upper)",
                    )
                })?;
                let ctor = range_ctor_for_pg_type(point_ty).ok_or_else(|| {
                    self.type_err(&format!(
                        "range(): unsupported element type '{point_ty}' — PostgreSQL only has native \
                     ranges over int64, decimal, datetime, cal::local_datetime, and cal::local_date"
                    ))
                })?;
                let sql_template = match args.len() {
                    1 => {
                        return Err(self.type_err(
                            "range(empty) has no inferable element type in this context — not \
                         currently supported; use range(lower, upper) instead",
                        ));
                    }
                    2 => format!("{ctor}($1, $2)"),
                    4 => format!(
                        "{ctor}($1, $2, \
                         (CASE WHEN $3 THEN '[' ELSE '(' END) || (CASE WHEN $4 THEN ']' ELSE ')' END))"
                    ),
                    n => return Err(self.type_err(&format!("range(): unexpected argument count {n}"))),
                };
                // `.name` carries the *resolved* constructor (not the
                // original `range`) so a wrapping `multirange([range(...)])`
                // call can identify the element family — emission always
                // goes through `sql_template` above, so this doesn't change
                // the SQL text.
                Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: None,
                    name: ctor.to_string(),
                    args,
                    sql_template: Some(sql_template),
                }))
            }
            "multirange" => {
                let Some(IrExpr::Array(elems)) = args.first() else {
                    return Err(self.type_err("multirange(): argument must be an array literal of ranges"));
                };
                let first_ctor = elems
                    .first()
                    .and_then(|e| match e {
                        IrExpr::FunctionCall(fc) => Some(fc.name.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        self.type_err(
                            "multirange(): cannot infer the element range type from an empty or non-range \
                     array — pass at least one range(...) call, e.g. multirange([range(1, 3)])",
                        )
                    })?;
                let ctor = multirange_ctor_for_range_ctor(first_ctor).ok_or_else(|| {
                    self.type_err(&format!("multirange(): unrecognized range constructor '{first_ctor}'"))
                })?;
                // PostgreSQL's multirange constructors (int8multirange, etc.)
                // are VARIADIC — they don't accept a plain array argument
                // without the VARIADIC keyword.
                Ok(IrExpr::FunctionCall(super::IrFunctionCall {
                    schema: None,
                    name: name.to_string(),
                    args,
                    sql_template: Some(format!("{ctor}(VARIADIC $1)")),
                }))
            }
            other => Err(self.type_err(&format!("internal error: unhandled TranspilerIntrinsic '{other}'"))),
        }
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

        let arg = fc.args.first().ok_or_else(|| {
            self.type_err(&format!(
                "{}() requires a sequence scalar type as its first argument",
                fc.name
            ))
        })?;

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
            s.is_sequence && s.name == arg_name && arg_module.map(|m| m == s.module.as_str()).unwrap_or(true)
        });

        match scalar {
            Some(s) => Ok((s.module.clone(), s.name.clone())),
            None => Err(self.type_err(&format!(
                "{}(): '{}' is not a known sequence scalar type",
                fc.name, arg_name
            ))),
        }
    }

    // ── Channel notify() helpers ────────────────────────────────────────────────

    /// PostgreSQL's hard per-NOTIFY-payload limit (`NOTIFY_PAYLOAD_MAX_LENGTH`
    /// in the Postgres source) — a payload at or over this is rejected by the
    /// server at runtime, always, for every session. Checked here only when
    /// the payload is a literal string (the one case actually decidable at
    /// compile time — see `static_min_payload_bytes` for how much of an
    /// arbitrary payload expression can be sized without running it.
    const NOTIFY_PAYLOAD_MAX_BYTES: usize = 8000;

    /// The rejection every non-object payload for a Type channel shares.
    fn notify_type_payload_err(&self, channel: &str, qname: &str) -> PyQLError {
        self.type_err(&format!(
            "notify(): payload for Channel '{channel}' (a '{qname}' object channel) must name an object of that \
             type — either a with-block binding, or __new__/__old__ inside a trigger handler"
        ))
    }

    /// Compile `notify(Channel, payload)` → `pg_notify('<wire_name>', (<payload>)::text)`.
    /// The payload's required shape depends on the Channel's declared kind:
    /// - `Type(qname)`: payload must be the bare `__new__`/`__old__` trigger
    ///   anchor for that exact type — sends its `.id`, not the whole row (see
    ///   this function's own restriction: a general object-typed expression
    ///   isn't supported yet, only the trigger-anchor case).
    /// - `Scalar(pg_type)`: payload is any expression, best-effort type-checked.
    /// - `Object(fields)`: payload must be a free object literal (`{ a := .., b := .. }`)
    ///   whose field names exactly match the declared shape.
    fn compile_notify(
        &mut self,
        fc: &ast::FunctionCall,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<IrExpr, PyQLError> {
        use crate::parse::ast::{Expr, Path, PathStep};

        if fc.args.len() != 2 {
            return Err(self.type_err("notify() takes exactly 2 arguments: (Channel, payload)"));
        }

        let (channel_module, channel_name): (Option<&str>, &str) = match &fc.args[0] {
            Expr::Path(Path { steps, partial: false }) => match steps.as_slice() {
                [PathStep::Name(s)] => {
                    if let Some((m, n)) = s.split_once("::") {
                        (Some(m), n)
                    } else {
                        (None, s.as_str())
                    }
                }
                _ => {
                    return Err(self.type_err(
                        "notify(): first argument must be a Channel name (e.g. OrderEvents or orders::OrderEvents)",
                    ));
                }
            },
            _ => {
                return Err(self.type_err(
                    "notify(): first argument must be a Channel name (e.g. OrderEvents or orders::OrderEvents)",
                ));
            }
        };
        let full_channel_name = match channel_module {
            Some(m) => format!("{m}::{channel_name}"),
            None => channel_name.to_string(),
        };
        let channel = self
            .resolve_channel(&full_channel_name)
            .ok_or_else(|| self.type_err(&format!("notify(): '{channel_name}' is not a known Channel")))?;
        let wire_name = channel.wire_name.clone();
        let payload_arg = &fc.args[1];

        // A bare `select notify(...)` trigger handler has no type at its
        // root, so the top-level statement compiles as a *free* select
        // (ctx=None) even though `special_anchors` is populated — `__new__`/
        // `__old__` only resolve through `compile_path`, which is only ever
        // reached when ctx is `Some(..)` (see `Expr::Path`'s arm above).
        // Fall back to any bound anchor's (td, alias) as a stand-in ctx so a
        // Scalar/Object payload like `__new__.name` or `{ a := __new__.x }`
        // still resolves — `compile_path`'s own `__new__`/`__old__` branch
        // ignores whatever td/alias ctx carries for those two names anyway,
        // it only matters for a real property access on some other root.
        // Cloned out of `special_anchors` (rather than borrowed) so it
        // doesn't hold an immutable borrow of `self` across the `&mut self`
        // `compile_expr_ctx` calls below.
        let anchor_fallback: Option<(&'a TypeDescriptor, String)> = self.special_anchors.values().next().cloned();
        let ctx = ctx.or_else(|| anchor_fallback.as_ref().map(|(td, alias)| (*td, alias.as_str())));

        let payload_ir = match &channel.payload {
            crate::schema::ChannelPayload::Type(qname) => {
                // The payload for an object channel is the object's `id`, in
                // either of the two places one can be named: the `__new__`/
                // `__old__` anchor a trigger handler runs against, or a
                // with-block binding in an ordinary query — which is what
                // lets a notify compose with the mutation that caused it:
                //
                //   with updated := (update User filter ... set { ... }),
                //   select (notify(UserUpdates, updated), updated)
                let Expr::Path(Path { steps, partial: false }) = payload_arg else {
                    return Err(self.notify_type_payload_err(&full_channel_name, qname));
                };
                let [PathStep::Name(name)] = steps.as_slice() else {
                    return Err(self.notify_type_payload_err(&full_channel_name, qname));
                };

                if name == "__new__" || name == "__old__" {
                    let (anchor_td, alias) = self.special_anchors.get(name.as_str()).cloned().ok_or_else(|| {
                        self.type_err(&format!(
                            "notify(): '{name}' cannot be used here — it's only bound inside a trigger handler"
                        ))
                    })?;
                    let anchor_qname = format!("{}::{}", anchor_td.module, anchor_td.name);
                    if &anchor_qname != qname {
                        return Err(self.type_err(&format!(
                            "notify(): Channel '{full_channel_name}' expects a payload of type '{qname}', got '{anchor_qname}'"
                        )));
                    }
                    IrExpr::ColumnRef {
                        alias,
                        column: "id".to_string(),
                        pg_type: "uuid".to_string(),
                    }
                } else if let Some(cte_type) = self.cte_types.get(name.as_str()).cloned() {
                    if &cte_type != qname {
                        return Err(self.type_err(&format!(
                            "notify(): Channel '{full_channel_name}' expects a payload of type '{qname}', got '{cte_type}'"
                        )));
                    }
                    // `CteRef { scalar: false }` emits `(SELECT "id" FROM
                    // "<cte>")` — the same id the trigger path sends.
                    IrExpr::CteRef {
                        name: name.clone(),
                        scalar: false,
                        pg_type: None,
                    }
                } else {
                    return Err(self.notify_type_payload_err(&full_channel_name, qname));
                }
            }
            crate::schema::ChannelPayload::Scalar(pg_type) => {
                let ir = self.compile_expr_ctx(payload_arg, ctx)?;
                if let Some(actual) = infer_ir_type(&ir)
                    && !types_compatible(actual, pg_type)
                {
                    return Err(self.type_err(&format!(
                            "notify(): Channel '{full_channel_name}' expects a payload of type '{expected}', got '{actual_pyql}'",
                            expected = pg_type_to_pyql(pg_type),
                            actual_pyql = pg_type_to_pyql(actual),
                        )));
                }
                ir
            }
            crate::schema::ChannelPayload::Object(declared_fields) => {
                let Expr::Shape(sh) = payload_arg else {
                    return Err(self.type_err(&format!(
                        "notify(): payload for Channel '{full_channel_name}' (an Object channel) must be a free \
                         object literal, e.g. {{ {} }}",
                        declared_fields
                            .iter()
                            .map(|(n, _)| format!("{n} := .."))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                };
                if sh.expr.is_some() {
                    return Err(self.type_err(&format!(
                        "notify(): payload for Channel '{full_channel_name}' (an Object channel) must be a free \
                         object literal, not a shape over a type"
                    )));
                }
                let ir = self.compile_expr_ctx(payload_arg, ctx)?;
                let IrExpr::NamedTuple {
                    fields,
                    is_free_object: true,
                } = &ir
                else {
                    return Err(self.type_err(&format!(
                        "notify(): payload for Channel '{full_channel_name}' (an Object channel) must be a free object literal"
                    )));
                };
                let declared_names: std::collections::HashSet<&str> =
                    declared_fields.iter().map(|(n, _)| n.as_str()).collect();
                let actual_names: std::collections::HashSet<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
                if declared_names != actual_names {
                    let mut expected: Vec<&str> = declared_names.iter().copied().collect();
                    expected.sort();
                    let mut actual: Vec<&str> = actual_names.iter().copied().collect();
                    actual.sort();
                    return Err(self.type_err(&format!(
                        "notify(): payload fields for Channel '{full_channel_name}' don't match — expected {{{}}}, got {{{}}}",
                        expected.join(", "), actual.join(", ")
                    )));
                }
                for (name, expr) in fields {
                    let Some((_, declared_pg_type)) = declared_fields.iter().find(|(n, _)| n == name) else {
                        continue;
                    };
                    if let Some(actual) = infer_ir_type(expr)
                        && !types_compatible(actual, declared_pg_type)
                    {
                        return Err(self.type_err(&format!(
                                "notify(): field '{name}' of Channel '{full_channel_name}' expects type '{expected}', got '{actual_pyql}'",
                                expected = pg_type_to_pyql(declared_pg_type),
                                actual_pyql = pg_type_to_pyql(actual),
                            )));
                    }
                }
                ir
            }
        };

        let floor = static_min_payload_bytes(payload_arg);
        if floor >= Self::NOTIFY_PAYLOAD_MAX_BYTES {
            return Err(self.type_err(&format!(
                "notify(): this payload is at least {floor} bytes, which is at or over PostgreSQL's {}-byte NOTIFY \
                 payload limit — the notification would fail at runtime, aborting the transaction that sent it",
                Self::NOTIFY_PAYLOAD_MAX_BYTES
            )));
        }

        let sql = format!("pg_notify('{}', ($1)::text)", wire_name.replace('\'', "''"));
        Ok(IrExpr::FunctionCall(super::IrFunctionCall {
            schema: None,
            name: "pg_notify".to_string(),
            args: vec![payload_ir],
            sql_template: Some(sql),
        }))
    }

    /// Compile `notify_raw(channel_name, payload)` → `pg_notify($1, $2)` — the
    /// escape hatch that bypasses Channel resolution and payload-shape
    /// checking entirely: both arguments are arbitrary text expressions.
    fn compile_notify_raw(
        &mut self,
        fc: &ast::FunctionCall,
        ctx: Option<(&TypeDescriptor, &str)>,
    ) -> Result<IrExpr, PyQLError> {
        if fc.args.len() != 2 {
            return Err(self.type_err("notify_raw() takes exactly 2 arguments: (channel_name, payload)"));
        }
        let channel_ir = self.compile_expr_ctx(&fc.args[0], ctx)?;
        let payload_ir = self.compile_expr_ctx(&fc.args[1], ctx)?;

        let floor = static_min_payload_bytes(&fc.args[1]);
        if floor >= Self::NOTIFY_PAYLOAD_MAX_BYTES {
            return Err(self.type_err(&format!(
                "notify_raw(): this payload is at least {floor} bytes, which is at or over PostgreSQL's {}-byte \
                 NOTIFY payload limit",
                Self::NOTIFY_PAYLOAD_MAX_BYTES
            )));
        }

        Ok(IrExpr::FunctionCall(super::IrFunctionCall {
            schema: None,
            name: "pg_notify".to_string(),
            args: vec![channel_ir, payload_ir],
            sql_template: None,
        }))
    }

    /// The `GLOBALS_ARG` value to pass when calling `qualified`, or `None` if
    /// that function does not take one.
    ///
    /// Inside a function body the caller's own argument is forwarded verbatim;
    /// at the top level every session global is packed, rather than just the
    /// ones the callee happens to read. Packing the whole set costs one small
    /// jsonb per call and keeps the caller from having to know anything about
    /// the callee's body.
    fn globals_arg_for_call(&mut self, qualified: &str) -> Result<Option<IrExpr>, PyQLError> {
        if self.fns_needing_globals.is_none() {
            self.fns_needing_globals = Some(functions_needing_globals(self.schema));
        }
        if !self.fns_needing_globals.as_ref().is_some_and(|s| s.contains(qualified)) {
            return Ok(None);
        }
        if self.in_fn_body {
            self.used_globals_arg = true;
            return Ok(Some(IrExpr::RawSql(GLOBALS_ARG.to_string())));
        }
        let session_globals: Vec<String> = self
            .schema
            .globals
            .iter()
            .filter(|g| g.computed_expr.is_none())
            .map(|g| format!("{}::{}", g.module, g.name))
            .collect();
        let mut args = Vec::with_capacity(session_globals.len() * 2);
        for name in session_globals {
            let value = self.compile_global(&name)?;
            args.push(IrExpr::Literal(IrLiteral::Str(name)));
            args.push(value);
        }
        Ok(Some(IrExpr::FunctionCall(super::IrFunctionCall {
            schema: None,
            name: "jsonb_build_object".to_string(),
            args,
            sql_template: None,
        })))
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
                fd.module,
                fd.name,
                fd.params.len(),
                fc.args.len()
            )));
        }

        let fn_module = fd.module.clone();
        let fn_name = fd.name.clone();
        let return_type_name = fd.return_pg_type.clone(); // qualified type name for object returns
        let polymorphic = fd.return_is_polymorphic;

        let mut fn_args = fc
            .args
            .iter()
            .map(|a| self.compile_free_expr(a))
            .collect::<Result<Vec<_>, _>>()?;
        let qualified = format!("{}::{}", fn_module, fn_name);
        if let Some(globals) = self.globals_arg_for_call(&qualified)? {
            fn_args.insert(0, globals);
        }

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

    /// The interface's own physical columns (properties + `{link}_id`) — the
    /// subset every concrete implementor table is guaranteed to share, safe
    /// to RETURNING/SELECT uniformly across a poly_implementors fan-out.
    /// Shared by compile_select (read path) and the IrUpdate/IrDelete
    /// builders (write path) so a DML-as-CTE fan-out (sql/mod.rs) can expose
    /// the same columns a polymorphic select would.
    /// Point a shape's single-link sub-selects at the nested-DML CTE that
    /// supplied their foreign key.
    ///
    /// `insert Account { credentials := (insert Credentials { … }) } { ** }`
    /// runs the nested insert as a data-modifying CTE, and Postgres does not
    /// show one statement's CTE writes to the rest of that statement: reading
    /// `access."Credentials"` back finds nothing, so the link hydrated as
    /// `None` even though the row was there on the next statement (confirmed
    /// live). The CTE itself is in scope, and `RETURNING *` gives it every
    /// column the shape asks for, so the sub-select reads that instead.
    fn read_nested_links_from_their_ctes(dml: &IrStmt, dml_cte: Option<&str>, shape: &mut [IrShapePointer]) {
        let (assignments, nested_ctes, appends) = match dml {
            IrStmt::Insert(ins) => (&ins.assignments, &ins.nested_ctes, &ins.multi_link_appends),
            IrStmt::Update(upd) => (&upd.assignments, &upd.nested_ctes, &upd.multi_link_appends),
            _ => return,
        };
        // A multi-link's junction rows are written by this statement too, in
        // a CTE the emitter names after the DML's own — see
        // `emit_insert_multilink_ctes`. The targets themselves come from the
        // append's value source, which for a nested insert is one of
        // `nested_ctes`.
        if let Some(dml_cte) = dml_cte {
            for pointer in shape.iter_mut() {
                let IrShapePointer::MultiLink(link) = pointer else {
                    continue;
                };
                let IrMultiLinkJoin::Standard { junction_table, .. } = &mut link.join else {
                    continue;
                };
                let Some(index) = appends.iter().position(|a| a.junction_table == *junction_table) else {
                    continue;
                };
                *junction_table = format!("@cte:{dml_cte}__ml_add_{index}");
                if let IrMultiLinkValueSource::CteRef(target_cte) = &appends[index].values.source {
                    for row in &mut link.subquery.rows {
                        if let IrRowSource::Bound { source, .. } = row {
                            source.table = format!("@cte:{target_cte}");
                            source.poly = None;
                        }
                    }
                }
            }
        }
        if nested_ctes.is_empty() {
            return;
        }
        let from_cte: HashMap<&str, &str> = assignments
            .iter()
            .filter_map(|(column, expr)| match expr {
                IrExpr::ColumnRef { alias, column: c, .. }
                    if c == "id" && nested_ctes.iter().any(|cte| cte.name == *alias) =>
                {
                    Some((column.as_str(), alias.as_str()))
                }
                _ => None,
            })
            .collect();
        if from_cte.is_empty() {
            return;
        }
        for pointer in shape {
            let IrShapePointer::SingleLink(link) = pointer else {
                continue;
            };
            let IrSingleLinkCorrelation::Fk { fk_column, .. } = &link.correlation else {
                continue;
            };
            let Some(cte_name) = from_cte.get(fk_column.as_str()) else {
                continue;
            };
            for row in &mut link.subquery.rows {
                if let IrRowSource::Bound { source, .. } = row {
                    source.table = format!("@cte:{cte_name}");
                    source.poly = None;
                }
            }
        }
    }

    fn poly_dml_columns(td: &TypeDescriptor) -> Vec<String> {
        td.properties
            .iter()
            .map(|p| p.name.clone())
            .chain(
                td.links
                    .iter()
                    .filter(|l| !l.is_junction_backed())
                    .map(|l| format!("{}_id", l.name)),
            )
            .collect()
    }

    /// A nested SELECT over a link's target, expanded inline over the
    /// interface's implementors when the target is one.
    ///
    /// An interface is materialised as a view that carries only the
    /// interface's own columns -- no discriminator -- so reading a link
    /// through it tags every row as the interface itself and hydrates the
    /// interface class rather than the concrete one. Expanding the
    /// implementors inline is what the root of a query already does, and each
    /// branch supplies its own `__type__`.
    fn link_target_select(
        &self,
        target_td: &TypeDescriptor,
        mut source: IrSource,
        shape: Vec<IrShapePointer>,
    ) -> IrSelect {
        source.poly = self.link_target_fanout(target_td);
        IrSelect::schema_bound(source, shape, None)
    }

    /// The fan-out a link's target needs, for the builders that assemble their
    /// own source rather than going through `link_target_select`.
    fn link_target_fanout(&self, target_td: &TypeDescriptor) -> Option<IrPolyFanout> {
        self.poly_fanout_for(&format!("{}::{}", target_td.module, target_td.name))
    }

    /// Give every interface-typed join target its fan-out, so a traversal that
    /// lands on one reads the implementors and carries the real type rather
    /// than the interface's view, which has no discriminator to carry.
    ///
    /// The root is left alone: a path select fans its own root out through
    /// `poly_implementors` already, and doing it twice would nest the union.
    fn resolve_join_fanouts(&self, path_select: &mut IrPathSelect) {
        for join in &mut path_select.joins {
            let target = match join {
                IrPathJoin::Single { target, .. }
                | IrPathJoin::Multi { target, .. }
                | IrPathJoin::BacklinkSingle { target, .. }
                | IrPathJoin::BacklinkMulti { target, .. }
                | IrPathJoin::Function { target, .. }
                | IrPathJoin::Lateral { target, .. } => target,
            };
            if target.poly.is_none() && !target.table.starts_with("@cte:") {
                target.poly = self.poly_fanout_for(&target.type_name.clone());
            }
        }
    }

    /// The fan-out a source of `type_name` needs, or `None` when it is not an
    /// interface and so reads from a table of its own. See `IrSource::poly`.
    fn poly_fanout_for(&self, type_name: &str) -> Option<IrPolyFanout> {
        let td = self
            .schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == type_name)?;
        if !(td.abstract_ && td.materialized) {
            return None;
        }
        let (implementors, columns) = self.collect_poly_info(type_name);
        Some(IrPolyFanout { implementors, columns })
    }

    /// Collect poly_implementors and poly_columns for a polymorphic return type.
    fn collect_poly_info(&self, type_name: &str) -> (Vec<IrPolyImplementor>, Vec<String>) {
        let implementors = self.find_poly_implementors(type_name);
        let columns = if let Some(td) = self
            .schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == type_name)
        {
            // The same set the DML path fans out (`poly_dml_columns`): the
            // interface's single-link FKs belong in it too, or reading
            // `.profile` off an interface-typed source finds no
            // `profile_id` column in the union it was fanned out into.
            Self::poly_dml_columns(td)
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
        let text_query_arg = fc.kwargs.iter().find(|(k, _)| k == "query").map(|(_, v)| v);
        let is_text_overload = text_query_arg.is_some();

        if !is_text_overload && fc.args.len() < 2 {
            return Err(
                self.type_err("vector::search requires either a positional vector argument or `query := $text`")
            );
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
        let index_name: Option<String> = fc.kwargs.iter().find(|(k, _)| k == "index_name").and_then(|(_, v)| {
            if let ast::Expr::Literal(ast::Literal::Str(s)) = v {
                Some(s.clone())
            } else {
                None
            }
        });

        // Resolve the type and find the VectorIndex.
        let td = self.resolve_type(&type_qname)?.clone();
        let vi = td
            .vector_indexes
            .iter()
            .find(|vi| vi.index_name.as_deref() == index_name.as_deref())
            .ok_or_else(|| {
                let key = index_name.as_deref().unwrap_or("<default>");
                self.type_err(&format!("type '{}' has no vector index '{}'", type_qname, key))
            })?;

        let vector_col = vi.column_name();
        let distance_op = match vi.metric.as_str() {
            "euclidean" => "<->",
            "inner_product" => "<#>",
            _ => "<=>", // cosine (default)
        };

        // Build query expression and inference fields.
        let (
            query_expr,
            inference_query_param_name,
            inference_query_literal,
            inference_model,
            inference_type_name,
            inference_index_name,
        );

        if is_text_overload {
            // Text overload: register __deferred_vec__ as the SQL param; Python injects
            // the embedding result into it before executing the query.
            let vec_idx = self.param_index("__deferred_vec__");
            let vec_param = IrExpr::Param { index: vec_idx };
            // Cast float8[] → vector so a Python list[float] encodes natively.
            let inner_cast = IrExpr::TypeCast(Box::new(IrTypeCast {
                expr: vec_param,
                pg_type: "float8[]".to_string(),
                tuple_shape: None,
            }));
            query_expr = IrExpr::TypeCast(Box::new(IrTypeCast {
                expr: inner_cast,
                pg_type: "vector".to_string(),
                tuple_shape: None,
            }));
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
                tuple_shape: None,
            }));
            inference_query_param_name = None;
            inference_query_literal = None;
            inference_model = None;
            inference_type_name = None;
            inference_index_name = None;
        }

        let alias = self.fresh_alias();
        let source = IrSource {
            poly: None,
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
            if el.splat.is_some() {
                continue;
            } // ignore splat in outer shape
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
    fn compile_vs_modifiers(&mut self, s: &ast::SelectStmt) -> Result<SearchModifiers, PyQLError> {
        let mut order_by_distance: Option<IrSortDir> = None;
        for sort in &s.order_by {
            let is_distance = matches!(&sort.expr,
                ast::Expr::Path(p) if p.partial && p.steps.len() == 1
                    && matches!(&p.steps[0], ast::PathStep::Name(n) if n == "distance")
            );
            if is_distance {
                let dir = match sort.direction {
                    ast::SortDirection::Desc => IrSortDir::Desc,
                    ast::SortDirection::Asc => IrSortDir::Asc,
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
                    let td = self
                        .resolve_type(n)
                        .map_err(|_| self.type_err(&format!("fts::search: '{}' is not a known type", n)))?;
                    format!("{}::{}", td.module, td.name)
                } else {
                    return Err(self.type_err("fts::search: first argument must be a type name"));
                }
            }
            _ => return Err(self.type_err("fts::search: first argument must be a bare type name")),
        };

        // Optional named arguments.
        let index_name: Option<String> = fc.kwargs.iter().find(|(k, _)| k == "index_name").and_then(|(_, v)| {
            if let ast::Expr::Literal(ast::Literal::Str(s)) = v {
                Some(s.clone())
            } else {
                None
            }
        });

        let mode_str = fc
            .kwargs
            .iter()
            .find(|(k, _)| k == "mode")
            .and_then(|(_, v)| {
                if let ast::Expr::Literal(ast::Literal::Str(s)) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("BestFields");

        let tsquery_fn: &'static str = match mode_str {
            "Phrase" => "phraseto_tsquery",
            _ => "websearch_to_tsquery", // BestFields and PhrasePrefix both use websearch
        };

        // Resolve the type and find the SearchIndex.
        let td = self.resolve_type(&type_qname)?.clone();
        let si = td
            .search_indexes
            .iter()
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
            poly: None,
            type_name: type_qname.clone(),
            table: td.table.clone(),
            alias: alias.clone(),
        };

        // Compile the object shape from `object { … }` in the outer shape.
        let mut object_shape: Vec<IrShapePointer> = vec![];
        for el in elements {
            if el.splat.is_some() {
                continue;
            }
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
    fn compile_fts_modifiers(&mut self, s: &ast::SelectStmt) -> Result<SearchModifiers, PyQLError> {
        let mut order_by_rank: Option<IrSortDir> = None;
        for sort in &s.order_by {
            let is_rank = matches!(&sort.expr,
                ast::Expr::Path(p) if p.partial && p.steps.len() == 1
                    && matches!(&p.steps[0], ast::PathStep::Name(n) if n == "score")
            );
            if is_rank {
                let dir = match sort.direction {
                    ast::SortDirection::Desc => IrSortDir::Desc,
                    ast::SortDirection::Asc => IrSortDir::Asc,
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
        let suggestion = self
            .schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == type_name)
            .and_then(|td| Self::suggest_pointer_name(td, field));
        let message = match suggestion {
            Some(s) => format!("object type '{type_name}' has no link or property '{field}'. Did you mean '{s}'?"),
            None => format!("object type '{type_name}' has no link or property '{field}'"),
        };
        PyQLError::Resolution(PyQLResolutionError::UnknownField(PyQLUnknownFieldError {
            message,
            position: Position { line: 0, col: 0 },
        }))
    }

    /// Fuzzy-matches `name` against every pointer (property/link/multilink/
    /// computed — Pylon's term for a type's own attributes; "field" is a
    /// Postgres-level term that doesn't apply here) on `td`, returning the
    /// closest candidate when it's plausibly a typo — powers a
    /// "Did you mean X?" suggestion for an unknown property/link.
    /// Jaro-Winkler (favors a shared prefix, which is where most real typos
    /// preserve the most characters, e.g. `nam` -> `name`) with a
    /// conservative similarity floor, so an unrelated pointer never gets
    /// suggested just because it happens to be the "closest" among an
    /// otherwise-dissimilar set of candidates.
    fn suggest_pointer_name(td: &TypeDescriptor, name: &str) -> Option<String> {
        const MIN_SIMILARITY: f64 = 0.7;
        td.properties
            .iter()
            .map(|p| p.name.as_str())
            .chain(td.links.iter().map(|l| l.name.as_str()))
            .chain(td.multilinks.iter().map(|m| m.name.as_str()))
            .chain(td.computed.iter().map(|c| c.name.as_str()))
            .map(|candidate| (candidate, strsim::jaro_winkler(name, candidate)))
            .filter(|(_, score)| *score >= MIN_SIMILARITY)
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(name, _)| name.to_string())
    }

    // ── Default returning (pk only) ────────────────────────────────────────────

    /// For bare DML (not wrapped in SELECT) return only primary-key properties:
    /// `INSERT … ` returns `{ id }`, same for UPDATE/DELETE.
    fn pk_returning(td: &TypeDescriptor) -> Vec<IrShapePointer> {
        td.properties
            .iter()
            .filter(|p| p.is_pk)
            .map(|p| {
                IrShapePointer::Scalar(IrScalarPointer {
                    marker_offset: None,
                    alias: p.name.clone(),
                    column: p.name.clone(),
                    pg_type: p.pg_type.clone(),
                    tuple_shape: None,
                })
            })
            .collect()
    }

    // ── Mutation rewrite compilation ─────────────────────────────────────────────

    /// Compile all active rewrites on `td`'s properties for the given event mask
    /// (1 = INSERT, 2 = UPDATE). Uses `self` so the alias counter and param list
    /// are shared with the surrounding statement.
    fn compile_rewrites(&mut self, td: &TypeDescriptor, alias: &str, on_mask: u8) -> Result<Vec<IrRewrite>, PyQLError> {
        let mut out = Vec::new();
        for prop in &td.properties {
            for rw in &prop.rewrites {
                if rw.on & on_mask == 0 {
                    continue;
                }
                let expr_ast = crate::parse::parse_expr(&rw.handler).map_err(PyQLError::Syntax)?;
                let ir_expr = self.compile_expr(&expr_ast, td, alias)?;
                out.push(IrRewrite {
                    column: prop.name.clone(),
                    expr: ir_expr,
                });
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

/// True for a bare `{}` or a `<AnyType>{}` cast of one — the empty-set
/// literal a caller uses to clear an optional pointer, in either form. A
/// frontend generating PyQL typically emits the cast form (e.g.
/// `<Company>{}`, matching `compile_expr`'s own `Expr::TypeCast` handling of
/// this exact case), while hand-written PyQL more often uses the bare form.
fn is_empty_set_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Set(elems) => elems.is_empty(),
        Expr::TypeCast(tc) => matches!(&tc.expr, Expr::Set(elems) if elems.is_empty()),
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

    // postgis:: types map directly to PostgreSQL types.
    if module == Some("postgis") {
        return match bare_name {
            "geometry" => Ok("geometry".to_string()),
            "geography" => Ok("geography".to_string()),
            "box2d" => Ok("box2d".to_string()),
            "box3d" => Ok("box3d".to_string()),
            other => Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown postgis type '{other}'; valid types are: geometry, geography, box2d, box3d"),
                position: Position { line: 0, col: 0 },
            })),
        };
    }

    // cal:: types map directly to PostgreSQL types.
    if module == Some("cal") {
        let pg = match bare_name {
            "local_datetime" => "timestamp",
            "local_date" => "date",
            "local_time" => "time",
            "relative_duration" | "date_duration" => "interval",
            other => {
                return Err(PyQLError::Type(PyQLTypeError {
                    message: format!(
                        "unknown cal type '{other}'; \
                     valid types are: local_datetime, local_date, local_time, \
                     relative_duration, date_duration"
                    ),
                    position: Position { line: 0, col: 0 },
                }));
            }
        };
        return Ok(pg.to_string());
    }

    let name = match module {
        Some("std") | None => bare_name,
        Some(m) => {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown type '{}::{}'", m, bare_name),
                position: Position { line: 0, col: 0 },
            }));
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
        // bigint and decimal are both PostgreSQL `numeric` — the distinction
        // is a Pylon-level scale/precision convention, not a separate PG type.
        "bigint" => "numeric",
        "datetime" => "timestamptz",
        "date" => "date",
        "time" => "time",
        "duration" => "interval",
        other => {
            return Err(PyQLError::Type(PyQLTypeError {
                message: format!("unknown type '{other}'"),
                position: Position { line: 0, col: 0 },
            }));
        }
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
        PT::Int16 | PT::Int32 | PT::Int64 | PT::BigInt => {
            matches!(infer_ir_type(expr), Some(t) if INT_TYPES.contains(&t))
        }
        PT::Float32 | PT::Float64 => matches!(infer_ir_type(expr), Some(t) if FLOAT_TYPES.contains(&t)),
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
        // Everything else an array can arrive as — a column, a cast, a
        // global, a `with` binding, a concatenation of any of those — is
        // known by the type it carries.
        other => matches!(infer_ir_type(other), Some(t) if t.ends_with("[]")),
    }
}

/// A lower bound, computable without running the query, on how many bytes a
/// `notify()` payload will serialize to.
///
/// Only the parts that are already known contribute: string literals count
/// their own length, a concatenation sums its sides, a free object counts the
/// JSON envelope it will always emit (`{}`, the quoted field names, and the
/// `:`/`,` separators) plus whatever its field expressions themselves floor
/// at. Anything runtime-valued — a column, a parameter, a function call —
/// contributes 0, so this never over-estimates and never rejects a payload
/// that could actually have fit.
///
/// Exists because Postgres's 8000-byte NOTIFY cap is enforced at *runtime*,
/// and blowing it aborts the transaction that sent the notification — which,
/// for the composed `with update ... select (notify(...), updated)` shape, is
/// the write itself.
fn static_min_payload_bytes(expr: &ast::Expr) -> usize {
    use crate::parse::ast::{BinOpKind, Expr, Literal};
    match expr {
        Expr::Literal(Literal::Str(s)) => s.len(),
        Expr::BinOp(op) if matches!(op.op, BinOpKind::Concat) => {
            static_min_payload_bytes(&op.left) + static_min_payload_bytes(&op.right)
        }
        Expr::Shape(sh) if sh.expr.is_none() => {
            // `{"a":,"b":}` — braces, one quoted name and colon per field,
            // and a comma between them. Every one of those bytes is emitted
            // regardless of what the values turn out to be.
            let mut total = 2;
            for (i, el) in sh.elements.iter().enumerate() {
                if i > 0 {
                    total += 1;
                }
                let name_len = match el.path.steps.last() {
                    Some(crate::parse::ast::PathStep::Name(n)) => n.len(),
                    _ => 0,
                };
                total += name_len + 3;
                if let Some(value) = &el.compexpr {
                    total += static_min_payload_bytes(value);
                }
            }
            total
        }
        _ => 0,
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

pub(crate) fn infer_ir_type(expr: &IrExpr) -> Option<&str> {
    match expr {
        IrExpr::ColumnRef { pg_type, .. } => Some(pg_type.as_str()),
        IrExpr::TypeCast(tc) => Some(tc.pg_type.as_str()),
        IrExpr::FnParam { pg_type, .. } => Some(pg_type.as_str()),
        IrExpr::Literal(lit) => Some(match lit {
            IrLiteral::Str(_) => "text",
            IrLiteral::Int(_) => "__int_literal",
            IrLiteral::Float(_) => "__float_literal",
            IrLiteral::Bool(_) => "boolean",
        }),
        IrExpr::EnumLiteral { pg_type, .. } => Some(pg_type.as_str()),
        IrExpr::NamedTuple { .. } => Some("jsonb"),
        IrExpr::GlobalParam { pg_type, .. } => Some(pg_type.as_str()),
        // A `with`-bound scalar is typed by what it binds, so a call over one
        // resolves to the same overload the bare value would.
        IrExpr::CteRef { pg_type, .. } => pg_type.as_deref(),
        // A correlated path subquery is typed by whatever it projects — the
        // scalar column at the end of the path. An object-valued one yields
        // an id, which no operator should silently compare against.
        IrExpr::PathSubquery(ps) => match &ps.result {
            IrPathResult::Scalar(e, _) => infer_ir_type(e),
            IrPathResult::Object { .. } => None,
        },
        // `a ++ b` and `distinct a` both yield whatever they were given, so
        // an array stays recognisable as one through either.
        IrExpr::BinOp(b) if b.op == crate::parse::ast::BinOpKind::Concat => {
            infer_ir_type(&b.left).or_else(|| infer_ir_type(&b.right))
        }
        IrExpr::UnaryOp(u) if u.op == crate::parse::ast::UnaryOpKind::Distinct => infer_ir_type(&u.operand),
        _ => None,
    }
}

/// Maps a resolved element `pg_type` (as `infer_ir_type` reports it) to the
/// PostgreSQL native range constructor over that type. PG has no float4/
/// float8/int2/int4-native range type — only int8range, numrange, tsrange,
/// tstzrange, and daterange exist — so untyped int/float literals default
/// to the widest native family (int8/numeric) rather than erroring, matching
/// how those literals already default elsewhere in Pylon.
fn range_ctor_for_pg_type(pg_type: &str) -> Option<&'static str> {
    match pg_type {
        "int2" | "int4" | "int8" | "__int_literal" => Some("int8range"),
        "numeric" | "__float_literal" => Some("numrange"),
        "timestamp" => Some("tsrange"),
        "timestamptz" => Some("tstzrange"),
        "date" => Some("daterange"),
        _ => None,
    }
}

/// The multirange counterpart of a range constructor name resolved by
/// `range_ctor_for_pg_type`.
fn multirange_ctor_for_range_ctor(range_ctor: &str) -> Option<&'static str> {
    match range_ctor {
        "int8range" => Some("int8multirange"),
        "numrange" => Some("nummultirange"),
        "tsrange" => Some("tsmultirange"),
        "tstzrange" => Some("tstzmultirange"),
        "daterange" => Some("datemultirange"),
        _ => None,
    }
}

const INT_TYPES: &[&str] = &["int2", "int4", "int8", "__int_literal"];
const FLOAT_TYPES: &[&str] = &["float4", "float8", "__float_literal"];
/// Postgres `numeric` backs both Pylon's `bigint` and `decimal` (see
/// `pg_type_for_scalar_name`) — Pylon doesn't distinguish them at the
/// pg_type level, so this bucket covers both.
const NUMERIC_TYPES: &[&str] = &["numeric"];

/// Pylon's implicit-cast graph for numeric operands:
/// `int16 → int32 → int64 → float32 → float64` on one branch and
/// `int64 → bigint → decimal` on another — every int width casts to every
/// float width and to numeric/decimal, but float and numeric/decimal don't
/// cast to each other (they're separate branches past `int64`). Postgres's
/// own operator resolution handles the actual mixed-type arithmetic once
/// the compile-time gate lets it through (e.g. `int2 + float4` is a native
/// Postgres operator) — this only needs to match which operand-type
/// combinations are meant to be allowed.
pub(crate) fn types_compatible(a: &str, b: &str) -> bool {
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
    if a_float && b_float {
        return true;
    }
    let a_numeric = NUMERIC_TYPES.contains(&a);
    let b_numeric = NUMERIC_TYPES.contains(&b);
    if a_numeric && b_numeric {
        return true;
    }
    (a_int && b_float) || (a_float && b_int) || (a_int && b_numeric) || (a_numeric && b_int)
}

/// Datetime/duration `+`/`-` pairs PostgreSQL supports natively (e.g.
/// `timestamptz + interval`) that `types_compatible`'s bucket-matching
/// (same type, or both-int, or both-float) doesn't cover. Deliberately a
/// separate, narrower check from `types_compatible` (also used for UNION-branch
/// compatibility, where "a datetime and a duration are interchangeable"
/// would be nonsensical) rather than folded into it, so this can't leak
/// into a context where "arithmetic-compatible" isn't the same relation as
/// "interchangeable." Postgres's own operator resolution is the final
/// authority on any (op, operand-order) combination that doesn't actually
/// exist (e.g. `interval - timestamptz`) — this only needs to widen the
/// compile-time gate far enough to let the legitimate combinations through.
fn datetime_arithmetic_compatible(op: &ast::BinOpKind, a: &str, b: &str) -> bool {
    if !matches!(op, ast::BinOpKind::Add | ast::BinOpKind::Sub) {
        return false;
    }
    matches!(
        (a, b),
        ("timestamptz", "interval")
            | ("interval", "timestamptz")
            | ("timestamp", "interval")
            | ("interval", "timestamp")
            | ("date", "interval")
            | ("interval", "date")
            | ("time", "interval")
            | ("interval", "time")
    )
}

/// Collect `SearchEnqueueInfo` for every OpenSearch- or Meilisearch-backed
/// search index on a type — `Postgres`-backed indexes are excluded since
/// they're maintained synchronously by a trigger-updated tsvector column,
/// not an async outbox worker.
fn collect_search_enqueue(td: &TypeDescriptor, type_name: &str, operation: &'static str) -> Vec<SearchEnqueueInfo> {
    td.search_indexes
        .iter()
        .filter(|si| si.backend == SearchBackend::OpenSearch || si.backend == SearchBackend::Meilisearch)
        .map(|si| SearchEnqueueInfo {
            type_name: type_name.to_string(),
            index_name: si.index_name.clone(),
            operation,
            backend: si.backend.clone(),
        })
        .collect()
}

/// Reverse of `type_expr_to_pg`'s plain-scalar branch — renders a base
/// Postgres type back to its canonical PyQL display name. `pub` (not just
/// crate-local): reused by `pylon-server`'s `/api/schema` port
/// (`_scalar_type_name`'s equivalent) so that display-name logic isn't
/// duplicated across crates.
///
/// `"interval"`/`"date"`/`"time"`/`"timestamp"` are inherently ambiguous
/// from the bare pg_type alone: `interval` backs both `std::duration` and
/// `cal::relative_duration` (defaults to the former — the more common
/// case), while `date`/`time`/`timestamp` (no tz) are unambiguous (only
/// `cal::local_date`/`cal::local_time`/`cal::local_datetime` use them, as
/// opposed to `timestamptz` for `std::datetime`).
pub fn pg_type_to_pyql(pg: &str) -> &str {
    match pg {
        "text" | "varchar" => "std::str",
        "uuid" => "std::uuid",
        "int2" => "std::int16",
        "int4" => "std::int32",
        "int8" => "std::int64",
        "float4" => "std::float32",
        "float8" => "std::float64",
        "boolean" => "std::bool",
        "numeric" => "std::decimal",
        "timestamptz" => "std::datetime",
        "timestamp" => "cal::local_datetime",
        "date" => "cal::local_date",
        "time" => "cal::local_time",
        "interval" => "std::duration",
        "bytea" => "std::bytes",
        "jsonb" => "std::json",
        "__int_literal" => "std::int64",
        "__float_literal" => "std::float64",
        other => other,
    }
}

/// Walk an `IrExpr` tree and replace every `ColumnRef` whose column name appears
/// in `bindings` with the bound expression.
///
/// Used for INSERT rewrites: the handler `lower(.name)` compiles to
/// `FunctionCall(lower, [ColumnRef("name")])`. If `name := $1` in the INSERT,
/// substituting produces `FunctionCall(lower, [Param(0)])`, which is valid in a
/// VALUES clause.
pub(super) fn substitute_col_refs(expr: IrExpr, bindings: &HashMap<String, IrExpr>) -> IrExpr {
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
            tuple_shape: None,
        })),
        IrExpr::IfElse(ie) => IrExpr::IfElse(Box::new(IrIfElse {
            condition: substitute_col_refs(ie.condition, bindings),
            if_: substitute_col_refs(ie.if_, bindings),
            else_: substitute_col_refs(ie.else_, bindings),
        })),
        IrExpr::Array(elems) => IrExpr::Array(elems.into_iter().map(|e| substitute_col_refs(e, bindings)).collect()),
        IrExpr::NamedTuple { fields, is_free_object } => IrExpr::NamedTuple {
            fields: fields
                .into_iter()
                .map(|(k, v)| (k, substitute_col_refs(v, bindings)))
                .collect(),
            is_free_object,
        },
        // Literals, Params, Subqueries — no column refs to substitute
        other => other,
    }
}
