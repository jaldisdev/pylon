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
    IrPathResult, IrPathSelect, IrPolyImplementor, IrRewrite, IrRowSource, IrScalarPointer, IrScalarSetPointer,
    IrSelect, IrSessionGlobalCte, IrShapePointer, IrSingleLinkCorrelation, IrSingleLinkPointer, IrSort, IrSortDir,
    IrSource, IrStmt, IrTypeCast, IrUnaryOp, IrUpdate, IrVectorSearch, SearchEnqueueInfo, TupleCastShape,
    VectorEnqueueInfo,
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
            Some(IrRowSource::Free(IrFreeExpr::Scalar(expr))) => {
                infer_ir_type(expr).map(|t| t.to_string()).unwrap_or_default()
            }
            _ => String::new(),
        },
        IrStmt::PathSelect(ps) => ps.root.type_name.clone(),
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
    let body = if body.starts_with("select")
        || body.starts_with("SELECT")
        || body.starts_with("with")
        || body.starts_with("WITH")
    {
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
    /// The schema-bound selects currently being compiled, innermost last.
    ///
    /// Only consulted for `detached`: an absolute `TypeName.prop` inside a
    /// detached select means the *enclosing* select's row, so the innermost
    /// entry is skipped and the next matching one used.
    anchors: Vec<SelectAnchor>,
    /// Set when `compile_stmt` strips a select-level `detached`, and taken by
    /// the `compile_path_modifiers` that compiles that select's own clauses.
    pending_detached: bool,
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
/// body cannot have — it would emit a bare `$1` nothing binds. Following Gel's
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
            fn_params: HashMap::new(),
            special_anchors: HashMap::new(),
            global_ctes: vec![],
            pending_nested_ctes: vec![],
            nested_cte_counter: 0,
            warnings: vec![],
            config,
            anchors: Vec::new(),
            pending_detached: false,
            in_fn_body: false,
            fns_needing_globals: None,
            used_globals_arg: false,
        }
    }

    /// Register a compiled WITH binding under `name`: records its type (or
    /// empty string for a free binding) in `cte_types`, and — when it's a
    /// single free row — its `IrFreeExpr` in `cte_free_items` so a later
    /// `name.field` reference can resolve to `IrExpr::CteFieldRef`.
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
            // caller packs. Mirrors Gel's `__edb_json_globals__`.
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
                    let inner = self.compile_subquery_to_array_source(inner_stmt)?;
                    let offset = s.offset.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
                    let limit = s.limit.as_ref().map(|e| self.compile_free_expr(e)).transpose()?;
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
                    let ir_inner = compile_cte_binding(self, &alias.expr)?;
                    self.register_cte(&alias.name, &ir_inner);
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
        let root_td = match &cte_object_type {
            Some(t) => self.resolve_type(t)?,
            None => self.resolve_type(root_name)?,
        };
        let root_alias = self.fresh_alias();
        let root = IrSource {
            type_name: format!("{}::{}", root_td.module, root_td.name),
            table: match &cte_object_type {
                Some(_) => format!("@cte:{}", root_name),
                None => root_td.table.clone(),
            },
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
                    let link_targets_current = td
                        .links
                        .iter()
                        .any(|l| l.name == *link_name && l.target == current_qname)
                        || td
                            .multilinks
                            .iter()
                            .any(|ml| ml.name == *link_name && ml.target == current_qname);
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
                                .any(|l| l.name == *link_name && l.target == current_qname)
                                || t.multilinks
                                    .iter()
                                    .any(|ml| ml.name == *link_name && ml.target == current_qname)
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
                        self.compile_path_modifiers(sel, owner_td, &target_alias)?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter,
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
                let result = IrPathResult::Scalar(
                    IrExpr::ColumnRef {
                        alias: current_alias.clone(),
                        column: p.name.clone(),
                        pg_type: p.pg_type.clone(),
                    },
                    tuple_shape,
                );
                let (filter, order_by, offset, limit) = self.compile_path_modifiers(sel, current_td, &current_alias)?;
                return Ok(IrPathSelect {
                    root,
                    joins,
                    result,
                    filter,
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
                        self.compile_path_modifiers(sel, target_td, &target_alias)?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter,
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
                        self.compile_path_modifiers(sel, target_td, &target_alias)?;
                    return Ok(IrPathSelect {
                        root,
                        joins,
                        result,
                        filter,
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

            return Err(self.field_err(step_name, &format!("{}::{}", current_td.module, current_td.name)));
        }

        // Should be unreachable: steps is non-empty (we checked len > 1 before dispatch).
        Err(self.type_err("empty path traversal"))
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
                    && self.resolve_type(root).is_ok()
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
        let td = self.resolve_type(root_type_name)?;
        let alias = self.fresh_alias();
        let root = IrSource {
            type_name: format!("{}::{}", td.module, td.name),
            table: td.table.clone(),
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
            IrStmt::PathSelect(ps) => Ok(IrArraySource::PathSelect(ps)),
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
                    // For-loop variable is a scalar, not a schema type reference.
                    if self.for_vars.contains_key(n.as_str()) {
                        return true;
                    }
                    // Scalar CTE: type string has no "::" (object types always do).
                    if self
                        .cte_types
                        .get(n.as_str())
                        .map(|t| !t.contains("::"))
                        .unwrap_or(false)
                    {
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
        self.cte_types
            .get(n.as_str())
            .map(|t| !t.contains("::"))
            .unwrap_or(false)
    }

    // ── FREE SELECT ───────────────────────────────────────────────────────────────

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
            other => {
                items.push(IrFreeExpr::Scalar(self.compile_free_expr(other)?));
            }
        }
        Ok(())
    }

    fn compile_free_select(
        &mut self,
        sel: &ast::SelectStmt,
        result_expr: &Expr,
        distinct: bool,
    ) -> Result<IrSelect, PyQLError> {
        if sel.filter.is_some() {
            return Err(self.type_err("FILTER is not supported on free SELECT expressions"));
        }

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
                vec![IrFreeExpr::Scalar(IrExpr::NamedTuple {
                    fields: ir,
                    is_free_object: false,
                })]
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

        Ok(IrSelect {
            rows: items.into_iter().map(IrRowSource::Free).collect(),
            filter: None,
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

    // ── SELECT ────────────────────────────────────────────────────────────────────

    fn compile_select(
        &mut self,
        sel: &ast::SelectStmt,
        result_expr: &Expr,
        distinct: bool,
    ) -> Result<IrSelect, PyQLError> {
        let (type_name, shape_elements, inner_stmt, cte_name) = self.extract_type_and_shape(result_expr)?;
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

        // On the stack for as long as this select's own clauses are being
        // compiled, so a `detached` select nested in one of them can find the
        // row it is being compared against. See `enclosing_anchor`.
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
        let (shape, filter, order_by, offset, limit) = clauses?;

        // Compile the inner DML if this is a SELECT-over-DML / SELECT-over-SELECT.
        let dml_source = inner_stmt.map(|s| self.compile_stmt(s).map(Box::new)).transpose()?;

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
                        }));
                    }
                };
                Ok((type_name, &s.elements, inner, cte_name))
            }
            // Bare `SELECT (DML)` without an outer shape
            Expr::SubQuery(stmt) => Ok((self.dml_subject_type(stmt)?, &[], Some(stmt.as_ref()), None)),
            // Bare CTE object reference: `select cte_name`
            Expr::Path(p) if !p.partial && p.steps.len() == 1 => {
                if let ast::PathStep::Name(n) = &p.steps[0]
                    && let Some(t) = self.cte_types.get(n.as_str())
                    && t.contains("::")
                {
                    return Ok((t.clone(), &[], None, Some(n.clone())));
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
                let compiled: Result<Vec<_>, _> = elems.iter().map(|e| self.compile_free_expr(e)).collect();
                let compiled = compiled?;
                let raw = compiled.first().and_then(|e| infer_ir_type(e)).unwrap_or("text");
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
            Some(old) => {
                self.for_vars.insert(f.var.clone(), old);
            }
            None => {
                self.for_vars.remove(&f.var);
            }
        }

        // Checked here rather than left to the SQL emitter: `emit_for_stmt`
        // only implements these three body kinds, and reaching it with any
        // other one used to abort the process instead of reporting a PyQL
        // error the caller could act on.
        let body_kind = match &body {
            IrStmt::Insert(_) | IrStmt::Select(_) | IrStmt::PathSelect(_) => None,
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
            iterator: IrForIterator::Values { exprs, pg_type },
            body: Box::new(body),
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
        let unless_conflict = ins
            .unless_conflict
            .as_ref()
            .map(|uc| self.compile_conflict(uc, td))
            .transpose()?;
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

        Ok(IrInsert {
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
        // See `compile_insert`'s identical save/restore of `pending_nested_ctes`.
        let outer_pending_nested_ctes = std::mem::take(&mut self.pending_nested_ctes);
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
            let expr_ast = crate::parse::parse_expr(&cd.expression).map_err(PyQLError::Syntax)?;
            let ir = self.compile_expr(&expr_ast, td, alias)?;
            pointers.push(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: None,
                alias: cd.name.clone(),
                expr: ir,
            }));
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
                let sub_shape = self.compile_splat(&ast::Splat::Shallow, target_td, &sub_alias, module)?;
                let subquery = IrSelect::schema_bound(
                    IrSource {
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
                let sub_shape = self.compile_splat(&ast::Splat::Shallow, target_td, &sub_alias, module)?;

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
                    IrMultiLinkJoin::Standard {
                        junction_table: format!("{}.{}", td.table, ml.name),
                        module: module.to_string(),
                    }
                };

                let subquery = IrSelect::schema_bound(
                    IrSource {
                        type_name: format!("{}::{}", target_td.module, target_td.name),
                        table: target_td.table.clone(),
                        alias: sub_alias.clone(),
                    },
                    sub_shape,
                    None,
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
            let expr_ast = crate::parse::parse_expr(&cd.expression).map_err(PyQLError::Syntax)?;
            let inner_ir = self.compile_expr(&expr_ast, concrete_td, &sub_alias)?;
            let subquery = IrSelect::schema_bound(
                IrSource {
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
            // `alias := .multilink` → rename a multilink, same semantics as a regular pointer
            if let Expr::Path(p) = compexpr {
                if p.partial
                    && p.steps.len() == 1
                    && let ast::PathStep::Name(ml_name) = &p.steps[0]
                    && Self::resolve_multilink(td, ml_name).is_some()
                {
                    let ml_name = ml_name.clone();
                    return self.compile_multilink_pointer(pointer_name, &ml_name, td, alias, module, el);
                }
                // `alias := .<backlink[is Type]` (no shape) → a backlink
                // used as a computed pointer.
                if p.partial && matches!(p.steps.first(), Some(ast::PathStep::Backlink(_))) {
                    let current_qname = format!("{}::{}", td.module, td.name);
                    return self.compile_backlink_pointer(pointer_name, p, &current_qname, &[], el.marker_offset);
                }
            }
            // `alias := .<backlink[is Type] { shape }` — the parser's `:=`
            // grammar always parses the RHS as a single expression
            // (`parse_expr`), which greedily folds a trailing `{ }` into
            // the expression itself as `Expr::Shape` rather than into
            // `el.nested` (that field is only ever populated by the
            // separate no-`:=` "bare inclusion with nested shape" parse
            // path — see `parse_shape_element`). So the shape case has to
            // be unwrapped here rather than read off `el.nested` the way
            // `compile_multilink_pointer`'s bare (non-computed) call site does.
            if let Expr::Shape(sh) = compexpr
                && let Some(Expr::Path(p)) = &sh.expr
                && p.partial
                && matches!(p.steps.first(), Some(ast::PathStep::Backlink(_)))
            {
                let current_qname = format!("{}::{}", td.module, td.name);
                return self.compile_backlink_pointer(pointer_name, p, &current_qname, &sh.elements, el.marker_offset);
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
            let subquery = IrSelect::schema_bound(
                IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias,
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
        if let Some(cd) = td.computed.iter().find(|c| c.name == pointer_name) {
            let expr_ast = crate::parse::parse_expr(&cd.expression).map_err(PyQLError::Syntax)?;
            let ir = self.compile_expr(&expr_ast, td, alias)?;
            return Ok(IrShapePointer::Computed(IrComputedPointer {
                marker_offset: el.marker_offset,
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

        let subquery = IrSelect {
            rows: vec![IrRowSource::Bound {
                source: IrSource {
                    type_name: format!("{}::{}", target_td.module, target_td.name),
                    table: target_td.table.clone(),
                    alias: sub_alias.clone(),
                },
                shape: sub_shape,
            }],
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
        let owner_qname = format!("{}::{}", owner_td.module, owner_td.name);

        let join = if let Some(l) = owner_td
            .links
            .iter()
            .find(|l| l.name == backlink_name && l.target == current_qname)
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
            .find(|ml| ml.name == backlink_name && ml.target == current_qname)
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

        // No filter/order_by/offset/limit here: the `pointer := expr`
        // grammar (`parse_shape_element`'s `:=` branch) never parses
        // trailing FILTER/ORDER BY/OFFSET/LIMIT after the RHS expression —
        // those per-link modifiers only exist on the separate no-`:=`
        // "bare inclusion with nested shape" parse path that
        // `compile_multilink_pointer`'s other call site reads `el.filter`
        // etc. from.
        let subquery = IrSelect {
            rows: vec![IrRowSource::Bound {
                source: IrSource {
                    type_name: owner_qname,
                    table: owner_td.table.clone(),
                    alias: sub_alias.clone(),
                },
                shape: sub_shape,
            }],
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
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
                    let inner = self.compile_subquery_to_array_source(inner_stmt)?;
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

                let args = f
                    .args
                    .iter()
                    .map(|a| self.compile_expr_ctx(a, ctx))
                    .collect::<Result<Vec<_>, _>>()?;
                self.resolve_fn_call(f.module.as_deref(), &f.name, args)
            }

            Expr::UnaryOp(u) if u.op == ast::UnaryOpKind::Exists => self.compile_exists_ctx(&u.operand, ctx),

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
                let ir = self.compile_expr_ctx(inner, ctx)?;
                Ok(IrExpr::JsonbField {
                    expr: Box::new(ir),
                    field: field.clone(),
                })
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
                        Ok((name, self.compile_expr_ctx(expr, ctx)?))
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

            // A bare shape or set literal is never valid in expression
            // position, in either context — preserved exactly as the
            // schema-bound side always enforced (the free side's more
            // permissive empty/singleton-set handling above is the one
            // deliberate exception, handled before this arm).
            Expr::Shape(_) | Expr::Set(_) => Err(PyQLError::Type(PyQLTypeError {
                message: "shapes and set literals are not valid in expression context".into(),
                position: Position { line: 0, col: 0 },
            })),

            // Strict improvement over the pre-merge free side: SubQuery/
            // Union/Except previously fell through free's generic "not
            // valid in free SELECT context" catch-all. These explicit,
            // purpose-written messages (already used schema-bound) apply
            // equally well with no schema in scope, so they're unconditional
            // here rather than ctx-gated.
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
        if let Some(t) = self.cte_types.get(name) {
            let scalar = !t.contains("::");
            return Some(IrExpr::CteRef {
                name: name.to_string(),
                scalar,
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
        if let Some(cd) = td.computed.iter().find(|c| c.name == pointer_name) {
            let expr_ast = crate::parse::parse_expr(&cd.expression).map_err(PyQLError::Syntax)?;
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
        let type_ref = match steps.get(1) {
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

        let type_name = match &type_ref.module {
            Some(m) => format!("{}::{}", m, type_ref.name),
            None => type_ref.name.clone(),
        };
        let target_td = self.resolve_type(&type_name)?;
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
            .find(|l| l.name == backlink_name && l.target == current_qname)
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
            .find(|ml| ml.name == backlink_name && ml.target == current_qname)
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
            operand: IrExpr::Subquery(Box::new(IrSelect::schema_bound(
                IrSource {
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
        fn ml_first_name(steps: &[ast::PathStep]) -> Option<&str> {
            if steps.len() < 2 {
                return None;
            }
            match &steps[0] {
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
        {
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
                let (l, r) = if flip {
                    (value_expr, col_ref)
                } else {
                    (col_ref, value_expr)
                };
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
                        op = op,
                    ),
                    position: Position { line: 0, col: 0 },
                }));
            }
            let prop = target_td
                .properties
                .iter()
                .find(|p| p.name == first_name)
                .ok_or_else(|| self.field_err(&first_name, target_type))?;
            let prop_name = prop.name.clone();
            let prop_pg = prop.pg_type.clone();
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
    fn compile_conflict(&mut self, uc: &ast::UnlessConflict, td: &TypeDescriptor) -> Result<IrConflict, PyQLError> {
        // ON clause: compile with empty alias → bare column name (`"col"` not `"t0"."col"`)
        // so the emitter produces `ON CONFLICT ("name")` not `ON CONFLICT ("t0"."name")`.
        let on = uc.on.as_ref().map(|e| self.compile_expr(e, td, "")).transpose()?;
        let do_update = uc.else_.as_ref().map(|e| self.compile_conflict_else(e)).transpose()?;
        Ok(IrConflict { on, do_update })
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
    fn compile_conflict_else(&mut self, expr: &Expr) -> Result<Vec<(String, IrExpr)>, PyQLError> {
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
        self.compile_assignments_for_update(&upd.shape, upd_td, &table)
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
            let inner_type_name = self.dml_subject_type(inner_stmt)?;
            let inner_ir = self.compile_stmt(inner_stmt)?;
            let cte_name = self.fresh_nested_cte_name();
            self.pending_nested_ctes.push(IrCteDef {
                name: cte_name.clone(),
                stmt: inner_ir,
                type_name: inner_type_name,
            });
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

    /// Collect poly_implementors and poly_columns for a polymorphic return type.
    fn collect_poly_info(&self, type_name: &str) -> (Vec<IrPolyImplementor>, Vec<String>) {
        let implementors = self.find_poly_implementors(type_name);
        let columns = if let Some(td) = self
            .schema
            .types
            .iter()
            .find(|t| format!("{}::{}", t.module, t.name) == type_name)
        {
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
        IrExpr::ColumnRef { pg_type, .. } => pg_type.ends_with("[]"),
        IrExpr::TypeCast(tc) => tc.pg_type.ends_with("[]"),
        _ => false,
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
