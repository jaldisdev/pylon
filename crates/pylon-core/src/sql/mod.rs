use crate::ir::{
    IrArraySource, IrCteDef, IrDelete, IrExpr, IrFor, IrForIterator, IrFreeExpr,
    IrFunctionSelect, IrGlobalCte, IrGroup, IrInsert, IrLiteral, IrMultiLinkPointer,
    IrMultiLinkJoin, IrMultiLinkMutation, IrMultiLinkValues, IrMultiLinkValueSource, IrNulls,
    IrOutput, IrPathJoin, IrPathResult, IrPathSelect, IrPolyImplementor, IrRowSource, IrScalarPointer,
    IrScalarSetPointer, IrSelect, IrShapePointer, IrSingleLinkPointer, IrFtsSearch, IrSort, IrSortDir,
    IrSource, IrStmt, IrUpdate, IrVectorSearch, VectorEnqueueInfo, SearchEnqueueInfo,
};
use crate::parse::ast::{BinOpKind, UnaryOpKind};
use crate::query::{Cardinality, InferencePlan, ShapeDescriptor, ShapeNode};

pub struct SqlOutput {
    pub sql: String,
    pub shape: ShapeDescriptor,
    pub inference_plan: Option<InferencePlan>,
}

/// Emit the SQL body for a computed global CTE — a plain scalar query with a `value` column.
fn emit_for_global_cte(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::Select(sel) => match sel.rows.as_slice() {
            [IrRowSource::Bound { source, .. }] => {
                let alias = &source.alias;
                let mut sql = format!(
                    "SELECT {}.\"id\" AS \"value\"\nFROM {} AS {}",
                    qi(alias),
                    source_ref(source),
                    qi(alias)
                );
                append_filter(&mut sql, &sel.filter);
                sql
            }
            rows => match rows.first() {
                Some(IrRowSource::Free(IrFreeExpr::Scalar(e))) => format!("SELECT {} AS \"value\"", emit_expr(e)),
                _ => "SELECT NULL AS \"value\"".to_string(),
            },
        },
        IrStmt::PathSelect(sel) => {
            let from_sql = emit_path_joins(&sel.root, &sel.joins);
            let scalar_expr = match &sel.result {
                IrPathResult::Scalar(e, _) => emit_expr(e),
                IrPathResult::Object { alias, .. } => format!("{}.\"id\"", qi(alias)),
            };
            let mut sql = format!("SELECT {} AS \"value\"\nFROM {}", scalar_expr, from_sql);
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql
        }
        _ => "SELECT NULL AS \"value\"".to_string(),
    }
}

fn emit_global_cte_parts(global_ctes: &[IrGlobalCte]) -> Vec<String> {
    global_ctes.iter().map(|g| match g {
        IrGlobalCte::Session(s) => format!(
            "\"{}\" AS (SELECT ${}::{} AS \"value\")",
            s.cte_name, s.param_index + 1, s.pg_type
        ),
        IrGlobalCte::Computed(c) => {
            let body = emit_for_global_cte(&c.stmt);
            format!("\"{}\" AS (\n{}\n)", c.cte_name, body)
        }
    }).collect()
}

pub fn emit(ir: &IrOutput) -> SqlOutput {
    let mut out = match &ir.stmt {
        IrStmt::Update(upd) => emit_update_stmt(upd, &ir.ctes),
        IrStmt::For(f) => emit_for_stmt(f, &ir.ctes),
        stmt => {
            let mut o = match stmt {
                IrStmt::Select(sel) => emit_select_stmt(sel, &ir.ctes),
                IrStmt::PathSelect(sel) => emit_path_select(sel),
                IrStmt::Insert(ins) => emit_insert_stmt(ins),
                IrStmt::Delete(del) => emit_delete_stmt(del),
                IrStmt::Group(grp) => emit_group(grp),
                IrStmt::FunctionSelect(sel) => emit_function_select(sel),
                IrStmt::VectorSearch(vs) => emit_vector_search(vs),
                IrStmt::FtsSearch(fs) => emit_fts_search(fs),
                IrStmt::Update(_) | IrStmt::For(_) => unreachable!(),
            };
            if !ir.ctes.is_empty() {
                let prefix = emit_cte_prefix(&ir.ctes);
                o.sql = format!("{}{}", prefix, o.sql);
            }
            o
        }
    };

    // Prepend global CTEs — merged into the existing WITH clause if present.
    // Two conventions produce a top-level WITH prefix: `emit_cte_prefix`'s
    // "WITH\n<parts>\n" (used whenever `ir.ctes` is non-empty) and the
    // inline "WITH <parts>\n" (space, no newline) built directly by a few
    // statement emitters (e.g. emit_update_stmt's enqueue branch). Both are
    // exactly 5 bytes before the CTE parts start, so the same slice works
    // for either — but checking only one (as this used to) means a query
    // with both user-defined CTEs *and* a global CTE gets a second, stray
    // `WITH` keyword prepended instead of being merged into the first.
    if !ir.global_ctes.is_empty() {
        let global_parts = emit_global_cte_parts(&ir.global_ctes);
        let global_str = global_parts.join(",\n     ");
        if out.sql.starts_with("WITH\n") || out.sql.starts_with("WITH ") {
            out.sql = format!("WITH {},\n     {}", global_str, &out.sql[5..]);
        } else {
            out.sql = format!("WITH {}\n{}", global_str, out.sql);
        }
    }

    out
}

// ── Identifier / literal helpers ────────────────────────────────────────────

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn pg_schema(module: &str) -> String {
    if module == "default" { "\"public\"".into() } else { qi(module) }
}

pub fn pg_schema_str(module: &str) -> String {
    pg_schema(module)
}

fn qn(module: &str, name: &str) -> String {
    format!("{}.{}", pg_schema(module), qi(name))
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `'module::Type'::text` — always position 0 in every non-free-type tuple.
fn type_disc(type_name: &str) -> String {
    format!("{}::text", sql_str(type_name))
}

/// Extract the PostgreSQL schema name (module) from `"module::TypeName"`.
fn module_of(type_name: &str) -> &str {
    type_name.split("::").next().unwrap_or("public")
}

fn source_ref(src: &IrSource) -> String {
    // "@cte:name" sentinel: the source is a WITH-clause CTE, not a real table.
    if let Some(cte_name) = src.table.strip_prefix("@cte:") {
        qi(cte_name)
    } else {
        qn(module_of(&src.type_name), &src.table)
    }
}

// ── Polymorphic UNION ALL ───────────────────────────────────────────────────

fn emit_poly_union(implementors: &[IrPolyImplementor], columns: &[String]) -> String {
    let col_list = columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");
    implementors.iter().map(|imp| {
        format!(
            "    SELECT {}::text AS \"__type__\", {} FROM {}",
            sql_str(&imp.type_name),
            col_list,
            qn(&imp.module, &imp.table),
        )
    }).collect::<Vec<_>>().join("\n    UNION ALL\n")
}

// ── SELECT statement ────────────────────────────────────────────────────────

fn emit_select_stmt(sel: &IrSelect, ctes: &[IrCteDef]) -> SqlOutput {
    match sel.rows.as_slice() {
        [IrRowSource::Bound { source, shape }] => emit_bound_select(sel, source, shape),
        rows => emit_free_rows(sel, rows, ctes),
    }
}

/// Schema-bound SELECT — has a FROM clause, projected column shape, and
/// (optionally) a DML source / polymorphic fan-out. `source`/`shape` are the
/// single `IrRowSource::Bound` entry destructured by `emit_select_stmt`;
/// everything else (filter/order_by/offset/limit/distinct/dml_source/
/// polymorphic/poly_*) stays on the outer `IrSelect`.
fn emit_bound_select(sel: &IrSelect, source: &IrSource, shape: &[IrShapePointer]) -> SqlOutput {
    let alias = &source.alias;
    let (pointer_exprs, shape_pointers) = build_shape(shape, alias);

    let type_expr = if sel.polymorphic {
        format!("{}.\"__type__\"", qi(alias))
    } else {
        type_disc(&source.type_name)
    };
    let mut parts = vec![type_expr];
    parts.extend(pointer_exprs);
    let tuple = parts.join(",\n    ");

    let distinct = if sel.distinct { "DISTINCT " } else { "" };

    // SELECT-over-DML: wrap inner statement in a CTE, select from it.
    let from_clause = if let Some(dml) = &sel.dml_source {
        let mut cte_parts = match dml.as_ref() {
            IrStmt::Update(upd) if update_has_any_multilink(upd) => emit_update_multilink_ctes(upd, "_dml"),
            IrStmt::Insert(ins) if insert_has_any_multilink(ins) => emit_insert_multilink_ctes(ins, "_dml"),
            IrStmt::Update(upd) if !upd.poly_implementors.is_empty() => emit_poly_update_dml_ctes(upd, "_dml"),
            IrStmt::Delete(del) if !del.poly_implementors.is_empty() => emit_poly_delete_dml_ctes(del, "_dml"),
            _ => vec![format!("\"_dml\" AS (\n{}\n)", emit_dml_as_cte_source(dml))],
        };
        let (enqueue_v, enqueue_s) = match dml.as_ref() {
            IrStmt::Insert(ins) => (ins.enqueue_vector.as_slice(), ins.enqueue_search.as_slice()),
            IrStmt::Update(upd) => (upd.enqueue_vector.as_slice(), upd.enqueue_search.as_slice()),
            _ => (&[][..], &[][..]),
        };
        cte_parts.extend(enqueue_ctes(enqueue_v, "_dml"));
        cte_parts.extend(enqueue_search_ctes(enqueue_s, "_dml", enqueue_v.len()));
        format!("WITH\n{}\nSELECT {}(\n    {}\n) AS result\nFROM \"_dml\" AS {}",
            cte_parts.join(",\n"), distinct, tuple, qi(alias))
    } else if sel.polymorphic && !source.table.starts_with("@cte:") {
        // Polymorphic interface with no CTE indirection: fan out to implementor tables.
        let union_sql = emit_poly_union(&sel.poly_implementors, &sel.poly_columns);
        format!("SELECT {}(\n    {}\n) AS result\nFROM (\n{}\n) AS {}",
            distinct, tuple, union_sql, qi(alias))
    } else {
        // Concrete table or CTE (pre-filtered): query directly.
        // For CTE-backed polymorphic sources, __type__ is already present in the CTE result.
        format!("SELECT {}(\n    {}\n) AS result\nFROM {} AS {}",
            distinct, tuple, source_ref(source), qi(alias))
    };

    let mut sql = from_clause;
    append_filter(&mut sql, &sel.filter);
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    let root_pointers = prepend_type(shape_pointers);
    SqlOutput {
        sql,
        shape: ShapeDescriptor {
            root: ShapeNode::Object {
                name: String::new(),
                type_name: Some(source.type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                pointers: root_pointers,
            },
        },
        inference_plan: None,
    }
}

/// Emit a DML statement for use as a CTE source, using `RETURNING *` to expose
/// all columns to the outer SELECT.  The DML's own returning shape is ignored.
fn emit_dml_as_cte_source(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::Insert(ins) => {
            let rewrite_cols: std::collections::HashSet<&str> =
                ins.rewrites.iter().map(|r| r.column.as_str()).collect();
            let cols: Vec<String> = ins.assignments.iter()
                .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
                .map(|(c, _)| format!("    {}", qi(c)))
                .chain(ins.rewrites.iter().map(|r| format!("    {}", qi(&r.column))))
                .collect();
            let vals: Vec<String> = ins.assignments.iter()
                .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
                .map(|(_, e)| format!("    {}", emit_expr(e)))
                .chain(ins.rewrites.iter().map(|r| format!("    {}", emit_expr(&r.expr))))
                .collect();
            let mut sql = format!(
                "    INSERT INTO {} (\n{}\n    ) VALUES (\n{}\n    )",
                source_ref(&ins.target),
                cols.join(",\n"),
                vals.join(",\n"),
            );
            if let Some(conflict) = &ins.unless_conflict {
                emit_conflict(&mut sql, conflict);
            }
            sql.push_str("\n    RETURNING *");
            sql
        }
        IrStmt::Update(upd) => {
            // Callers must route an update with any multi-link (junction
            // table) mutation through emit_update_multilink_ctes instead —
            // junction INSERT/DELETE CTEs are data-modifying and Postgres
            // requires those to sit at the *top level* of the query, so they
            // can't be nested inside this function's single-CTE-body return
            // value. See emit_cte_prefix and emit_select_stmt's dml_source
            // branch, both of which check has_any_multilink before calling
            // this function at all.
            let alias = &upd.target.alias;
            let mut sets: Vec<String> = upd.assignments.iter()
                .map(|(col, expr)| format!("    {} = {}", qi(col), emit_expr(expr)))
                .collect();
            for rw in &upd.rewrites {
                sets.push(format!("    {} = {}", qi(&rw.column), emit_expr(&rw.expr)));
            }
            let mut sql = format!(
                "    UPDATE {} AS {}\n    SET\n{}",
                source_ref(&upd.target), qi(alias), sets.join(",\n"),
            );
            append_filter(&mut sql, &upd.filter);
            sql.push_str("\n    RETURNING *");
            sql
        }
        IrStmt::Delete(del) => {
            let alias = &del.target.alias;
            let mut sql = format!(
                "    DELETE FROM {} AS {}",
                source_ref(&del.target), qi(alias),
            );
            append_filter(&mut sql, &del.filter);
            sql.push_str("\n    RETURNING *");
            sql
        }
        IrStmt::Select(inner) => match inner.rows.as_slice() {
            [IrRowSource::Bound { source, .. }] => {
                // SELECT-over-SELECT: expose raw columns so the outer SELECT can
                // project its own shape from them, mirroring DML's RETURNING *.
                let from = if inner.polymorphic && !source.table.starts_with("@cte:") {
                    format!(
                        "(\n{}\n    ) AS {}",
                        emit_poly_union(&inner.poly_implementors, &inner.poly_columns),
                        qi(&source.alias),
                    )
                } else {
                    format!("{} AS {}", source_ref(source), qi(&source.alias))
                };
                let mut sql = format!("    SELECT * FROM {}", from);
                append_filter(&mut sql, &inner.filter);
                append_order_by(&mut sql, &inner.order_by);
                append_offset_limit(&mut sql, &inner.offset, &inner.limit);
                sql
            }
            // Free rows: full UNION-ALL emission, same as before the merge
            // (this arm used to be a separate IrStmt::FreeSelect match). Only
            // `.sql` is used here, and CtePassthrough's SQL text doesn't
            // depend on the ctes list (only its *shape* resolution does), so
            // an empty slice is fine.
            _ => emit_select_stmt(inner, &[]).sql,
        },
        IrStmt::FunctionSelect(sel) => {
            // Expose raw columns so the outer SELECT can project its own shape,
            // mirroring how IrStmt::Select works as a CTE source.
            let args_sql = sel.fn_args.iter().map(emit_expr).collect::<Vec<_>>().join(", ");
            let fn_call = format!("{}.{}({})", pg_schema(&sel.fn_module), qi(&sel.fn_name), args_sql);
            let mut sql = format!("    SELECT * FROM {} AS {}", fn_call, qi(&sel.alias));
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql
        }
        IrStmt::For(_) | IrStmt::Group(_) | IrStmt::VectorSearch(_) | IrStmt::FtsSearch(_) => unreachable!("cannot appear as a CTE source"),
        IrStmt::PathSelect(ps) => {
            // Path select as CTE: emit a flat SELECT that exposes an `id` column.
            let mut sql = emit_path_joins(&ps.root, &ps.joins);
            append_filter(&mut sql, &ps.filter);
            sql
        }
    }
}

// ── Polymorphic DML-as-CTE fan-out ──────────────────────────────────────────
//
// A DML statement targeting an interface/abstract type can't be a single
// `UPDATE`/`DELETE ... RETURNING *` the way emit_dml_as_cte_source handles a
// concrete-type target — Postgres has no single physical relation backing
// the interface (each concrete implementor has its own table), and a bare
// `RETURNING *` off whichever one table `source_ref` happened to resolve
// would only ever see that one implementor's rows and never carry a
// `__type__` discriminator column at all (confirmed live: querying such a
// CTE from an outer polymorphic SELECT raised "column t1.__type__ does not
// exist", since the outer SELECT — see emit_select_stmt and the "CTE-backed
// polymorphic sources" comment there — assumes any CTE tied to a polymorphic
// type already exposes one).
//
// Mirrors emit_poly_update_stmt/emit_poly_delete_stmt's per-implementor
// UNION ALL fan-out (one CTE per concrete table, each RETURNING-ing rows,
// unioned together with a compile-time-known type literal per branch — the
// type is known per branch even though it can vary per row across the whole
// statement, since each branch only ever touches one implementor's table).
// The difference here: those two functions only need `id` back (a
// standalone DML's own default RETURNING shape), while a DML-as-CTE source
// must expose whatever columns the *outer* SELECT might project — so this
// returns the interface's full common column set (poly_columns: every
// property + link FK the type declares, guaranteed present on every
// implementor table) instead of just id.
//
// Not handled: a multi-link mutation (+=/-=/:=) on the interface's own
// pointer combined with poly_implementors on the same statement — that
// would need per-implementor junction-table CTEs too, which
// emit_update_multilink_ctes doesn't do either today. Callers check
// update_has_any_multilink first and keep routing that (rarer) combination
// through the existing (equally not-poly-aware) path rather than silently
// mishandling it here.

fn emit_poly_update_dml_ctes(upd: &IrUpdate, name: &str) -> Vec<String> {
    let alias = &upd.target.alias;
    let sets: Vec<String> = upd.assignments.iter()
        .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
        .chain(upd.rewrites.iter().map(|rw| format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr))))
        .collect();
    let col_list = upd.poly_columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");

    let mut cte_parts = vec![];
    let mut union_parts = vec![];
    for (i, imp) in upd.poly_implementors.iter().enumerate() {
        let cte_name = format!("{}__u{}", name, i);
        let mut upd_sql = format!(
            "UPDATE {} AS {}\nSET {}",
            qn(&imp.module, &imp.table), qi(alias), sets.join(", "),
        );
        append_filter(&mut upd_sql, &upd.filter);
        upd_sql.push_str(&format!("\nRETURNING {}", col_list));
        cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte_name, upd_sql));

        union_parts.push(format!(
            "SELECT {}::text AS \"__type__\", {} FROM \"{}\"",
            sql_str(&imp.type_name), col_list, cte_name,
        ));
    }
    cte_parts.push(format!("\"{}\" AS (\n{}\n)", name, union_parts.join("\nUNION ALL\n")));
    cte_parts
}

fn emit_poly_delete_dml_ctes(del: &IrDelete, name: &str) -> Vec<String> {
    let alias = &del.target.alias;
    let col_list = del.poly_columns.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");

    let mut cte_parts = vec![];
    let mut union_parts = vec![];
    for (i, imp) in del.poly_implementors.iter().enumerate() {
        let cte_name = format!("{}__d{}", name, i);
        let mut del_sql = format!(
            "DELETE FROM {} AS {}",
            qn(&imp.module, &imp.table), qi(alias),
        );
        append_filter(&mut del_sql, &del.filter);
        del_sql.push_str(&format!("\nRETURNING {}", col_list));
        cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte_name, del_sql));

        union_parts.push(format!(
            "SELECT {}::text AS \"__type__\", {} FROM \"{}\"",
            sql_str(&imp.type_name), col_list, cte_name,
        ));
    }
    cte_parts.push(format!("\"{}\" AS (\n{}\n)", name, union_parts.join("\nUNION ALL\n")));
    cte_parts
}

// ── User CTE helpers ────────────────────────────────────────────────────────

fn update_has_any_multilink(upd: &IrUpdate) -> bool {
    !upd.multi_link_clears.is_empty()
        || !upd.multi_link_replaces.is_empty()
        || !upd.multi_link_appends.is_empty()
        || !upd.multi_link_removals.is_empty()
}

/// Builds the CTE chain for an UPDATE with multi-link (junction table)
/// mutations, bound to a single external `name` (a user WITH-binding, or the
/// implicit "_dml" wrapper for `SELECT (UPDATE ...)`). Junction INSERT/DELETE
/// statements are themselves data-modifying, and Postgres requires
/// data-modifying CTEs to sit at the *top level* of the query — they can't
/// be nested inside another CTE's own body (confirmed live: nesting them
/// raises "WITH clause containing a data-modifying statement must be at the
/// top level"). So this returns a flat list of sibling CTE parts, prefixed
/// by `name` to stay collision-free alongside any other bound statements in
/// the same WITH block, ending in a `"{name}" AS (SELECT * FROM
/// "{name}__ids")` passthrough — every other reference to `name` keeps
/// seeing the updated row's full columns exactly as `RETURNING *` would have
/// exposed them.
fn emit_update_multilink_ctes(upd: &IrUpdate, name: &str) -> Vec<String> {
    let alias = &upd.target.alias;
    let has_scalar_changes = !upd.assignments.is_empty() || !upd.rewrites.is_empty();
    let ids_name = format!("{}__ids", name);
    let mut parts: Vec<String> = vec![];

    if has_scalar_changes {
        let mut sets: Vec<String> = upd.assignments.iter()
            .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
            .collect();
        for rw in &upd.rewrites {
            sets.push(format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr)));
        }
        let mut upd_sql = format!(
            "UPDATE {} AS {}\nSET {}",
            source_ref(&upd.target), qi(alias), sets.join(", "),
        );
        append_filter(&mut upd_sql, &upd.filter);
        upd_sql.push_str("\nRETURNING *");
        parts.push(format!("\"{}\" AS (\n{}\n)", ids_name, upd_sql));
    } else {
        let mut sel = format!(
            "SELECT {}.* FROM {} AS {}",
            qi(alias), source_ref(&upd.target), qi(alias),
        );
        append_filter(&mut sel, &upd.filter);
        parts.push(format!("\"{}\" AS (\n{}\n)", ids_name, sel));
    }

    for (i, clr) in upd.multi_link_clears.iter().enumerate() {
        let del = format!(
            "DELETE FROM {} WHERE {} IN (SELECT id FROM \"{}\")",
            qn(&clr.module, &clr.junction_table), qi(&clr.source_col), ids_name,
        );
        parts.push(format!("\"{}__clr_{}\" AS (\n{}\n)", name, i, del));
    }
    for (i, app) in upd.multi_link_appends.iter().enumerate() {
        parts.push(emit_ml_append_cte(app, &ids_name, &format!("{}__ml_add_{}", name, i)));
    }
    for (i, rem) in upd.multi_link_removals.iter().enumerate() {
        parts.push(emit_ml_remove_cte(rem, &ids_name, &format!("{}__ml_rm_{}", name, i)));
    }
    for (i, rep) in upd.multi_link_replaces.iter().enumerate() {
        parts.push(emit_ml_append_cte(rep, &ids_name, &format!("{}__ml_rep_{}", name, i)));
    }

    parts.push(format!("\"{}\" AS (\n    SELECT * FROM \"{}\"\n)", name, ids_name));
    parts
}

fn insert_has_any_multilink(ins: &IrInsert) -> bool {
    !ins.multi_link_appends.is_empty()
}

/// Builds the CTE chain for an INSERT with multi-link (junction table)
/// assignments at creation time — same top-level-CTE constraint as
/// `emit_update_multilink_ctes` (junction INSERTs are themselves
/// data-modifying). The row itself is inserted first (`{name}__ids`, via
/// `RETURNING *`) so the junction CTEs can reference its freshly-generated id.
fn emit_insert_multilink_ctes(ins: &IrInsert, name: &str) -> Vec<String> {
    let ids_name = format!("{}__ids", name);
    let mut parts: Vec<String> = vec![];

    let rewrite_cols: std::collections::HashSet<&str> =
        ins.rewrites.iter().map(|r| r.column.as_str()).collect();
    let cols: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(c, _)| qi(c))
        .chain(ins.rewrites.iter().map(|r| qi(&r.column)))
        .collect();
    let vals: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(_, e)| emit_expr(e))
        .chain(ins.rewrites.iter().map(|r| emit_expr(&r.expr)))
        .collect();
    let mut insert_sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        source_ref(&ins.target), cols.join(", "), vals.join(", "),
    );
    if let Some(conflict) = &ins.unless_conflict { emit_conflict(&mut insert_sql, conflict); }
    insert_sql.push_str("\nRETURNING *");
    parts.push(format!("\"{}\" AS (\n{}\n)", ids_name, insert_sql));

    for (i, app) in ins.multi_link_appends.iter().enumerate() {
        parts.push(emit_ml_append_cte(app, &ids_name, &format!("{}__ml_add_{}", name, i)));
    }

    parts.push(format!("\"{}\" AS (\n    SELECT * FROM \"{}\"\n)", name, ids_name));
    parts
}

/// Expands a list of user-bound WITH names into their top-level CTE parts —
/// usually one part per name, except an UPDATE or INSERT with any multi-link
/// mutation expands into several sibling parts (see
/// emit_update_multilink_ctes / emit_insert_multilink_ctes), and an UPDATE or
/// DELETE targeting an interface/abstract type similarly fans out into one
/// part per concrete implementor (see emit_poly_update_dml_ctes /
/// emit_poly_delete_dml_ctes) — neither can be nested inside a single name's
/// own CTE body.
fn emit_user_cte_parts(ctes: &[IrCteDef]) -> Vec<String> {
    let mut parts: Vec<String> = vec![];
    for c in ctes {
        if let IrStmt::Update(upd) = &c.stmt {
            if update_has_any_multilink(upd) {
                parts.extend(emit_update_multilink_ctes(upd, &c.name));
                continue;
            }
            if !upd.poly_implementors.is_empty() {
                parts.extend(emit_poly_update_dml_ctes(upd, &c.name));
                continue;
            }
        }
        if let IrStmt::Insert(ins) = &c.stmt {
            if insert_has_any_multilink(ins) {
                parts.extend(emit_insert_multilink_ctes(ins, &c.name));
                continue;
            }
        }
        if let IrStmt::Delete(del) = &c.stmt {
            if !del.poly_implementors.is_empty() {
                parts.extend(emit_poly_delete_dml_ctes(del, &c.name));
                continue;
            }
        }
        parts.push(format!("\"{}\" AS (\n{}\n)", c.name, emit_dml_as_cte_source(&c.stmt)));
    }
    parts
}

/// Emit `WITH "name" AS (...), ...` prefix (WITH keyword + trailing newline included).
fn emit_cte_prefix(ctes: &[IrCteDef]) -> String {
    format!("WITH\n{}\n", emit_user_cte_parts(ctes).join(",\n"))
}

/// Collects every link-property name assigned anywhere in a multilink value
/// tree (both sides of any nested `union`), in first-seen order — the SQL
/// layer needs one consistent column list across every unioned branch, even
/// when different targets set different (or no) properties.
fn collect_link_prop_names(vals: &IrMultiLinkValues, names: &mut Vec<String>) {
    for (name, _) in &vals.link_props {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    if let IrMultiLinkValueSource::Union(a, b) = &vals.source {
        collect_link_prop_names(a, names);
        collect_link_prop_names(b, names);
    }
}

/// `, <expr> AS "name"` for each entry in `prop_names` — the value assigned to
/// *this* node if any, else a bare `NULL` (Postgres infers its type from the
/// other union branch's typed value in the same column position; if no
/// branch ever sets it, the INSERT's target column type coerces it).
fn emit_link_prop_cols(vals: &IrMultiLinkValues, prop_names: &[String]) -> String {
    prop_names
        .iter()
        .map(|name| match vals.link_props.iter().find(|(n, _)| n == name) {
            Some((_, expr)) => format!(", {} AS {}", emit_expr(expr), qi(name)),
            None => format!(", NULL AS {}", qi(name)),
        })
        .collect()
}

/// Emit `(SELECT id[, prop, ...] FROM ...)` sub-expression for a multi-link
/// values source. `prop_names` is the full set of link-property names used
/// anywhere in the enclosing mutation (see `collect_link_prop_names`) — every
/// leaf projects all of them so a `union` of heterogeneous branches has a
/// consistent column list.
fn emit_multilink_values_subquery(vals: &IrMultiLinkValues, prop_names: &[String]) -> String {
    if let IrMultiLinkValueSource::Union(a, b) = &vals.source {
        return format!(
            "({}\nUNION ALL\n{})",
            emit_multilink_values_subquery(a, prop_names),
            emit_multilink_values_subquery(b, prop_names),
        );
    }

    let prop_cols = emit_link_prop_cols(vals, prop_names);

    match &vals.source {
        IrMultiLinkValueSource::CteRef(name) => {
            if prop_cols.is_empty() {
                // Just the CTE name; will be aliased at the call site.
                format!("\"{}\"", name)
            } else {
                format!("(SELECT \"_s\".\"id\"{} FROM \"{}\" AS \"_s\")", prop_cols, name)
            }
        }
        IrMultiLinkValueSource::Select(s) => {
            // Compiler only ever constructs this variant for a single
            // schema-bound row (compile_subquery_exists's callers reject
            // free rows before wrapping) — see compiler.rs's multilink-
            // value-source sites.
            let [IrRowSource::Bound { source, .. }] = s.rows.as_slice() else {
                unreachable!("IrMultiLinkValueSource::Select is always schema-bound")
            };
            let alias = &source.alias;
            let mut sql = format!(
                "(SELECT {}.\"id\"{} FROM {} AS {}",
                qi(alias), prop_cols, source_ref(source), qi(alias)
            );
            append_filter(&mut sql, &s.filter);
            sql.push(')');
            sql
        }
        IrMultiLinkValueSource::PathSelect(ps) => {
            let root_alias = &ps.root.alias;
            let mut sql = format!(
                "(SELECT {}.\"id\"{} FROM {} AS {}",
                qi(root_alias), prop_cols, source_ref(&ps.root), qi(root_alias)
            );
            for join in &ps.joins {
                sql.push_str(&emit_path_join_sql(join));
            }
            append_filter(&mut sql, &ps.filter);
            sql.push(')');
            sql
        }
        IrMultiLinkValueSource::Union(..) => unreachable!("handled above"),
    }
}

/// SQL fragment for a single path join (used in multilink values emission).
fn emit_path_join_sql(join: &IrPathJoin) -> String {
    match join {
        IrPathJoin::Single { source_alias, fk_col, target } => {
            format!(
                " JOIN {} AS {} ON {}.\"id\" = {}.{}",
                source_ref(target), qi(&target.alias),
                qi(&target.alias), qi(source_alias), qi(fk_col)
            )
        }
        IrPathJoin::Multi { source_alias, junction_alias, join: ml_join, target } => {
            let (jt_ref, src_col, tgt_col) = match ml_join {
                IrMultiLinkJoin::Standard { junction_table, module } =>
                    (qn(module, junction_table), "source".to_string(), "target".to_string()),
                IrMultiLinkJoin::Through { junction_table, module, source_col, target_col } =>
                    (qn(module, junction_table), source_col.clone(), target_col.clone()),
            };
            format!(
                " JOIN {} AS {} ON {}.{} = {}.\"id\" JOIN {} AS {} ON {}.{} = {}.\"id\"",
                jt_ref, qi(junction_alias),
                qi(junction_alias), qi(&src_col), qi(source_alias),
                source_ref(target), qi(&target.alias),
                qi(junction_alias), qi(&tgt_col), qi(&target.alias),
            )
        }
        IrPathJoin::BacklinkSingle { source_alias, fk_col, target } => {
            format!(
                " JOIN {} AS {} ON {}.{} = {}.\"id\"",
                source_ref(target), qi(&target.alias),
                qi(&target.alias), qi(fk_col), qi(source_alias),
            )
        }
        IrPathJoin::BacklinkMulti { source_alias, junction_alias, junction_table, module,
                                    owner_col, current_col, target } => {
            format!(
                " JOIN {} AS {} ON {}.{} = {}.\"id\" JOIN {} AS {} ON {}.\"id\" = {}.{}",
                qn(module, junction_table), qi(junction_alias),
                qi(junction_alias), qi(current_col), qi(source_alias),
                source_ref(target), qi(&target.alias),
                qi(&target.alias), qi(junction_alias), qi(owner_col),
            )
        }
    }
}

/// Emit the CTE clause for a junction table INSERT (append / replace-insert).
/// `ids_name` is the row-source CTE to join against (usually "_ids", but
/// callers hoisting this into a shared top-level WITH block alongside other
/// user-bound statements pass a name prefixed for collision-safety instead).
fn emit_ml_append_cte(mutation: &IrMultiLinkMutation, ids_name: &str, cte_name: &str) -> String {
    let mut prop_names = vec![];
    collect_link_prop_names(&mutation.values, &mut prop_names);
    let vals_ref = emit_multilink_values_subquery(&mutation.values, &prop_names);

    let extra_cols: String = prop_names.iter().map(|n| format!(", {}", qi(n))).collect();
    let extra_select: String = prop_names.iter().map(|n| format!(", \"_v\".{}", qi(n))).collect();

    // Re-checking an already-linked target with a new property value must
    // update in place rather than error — a bare `DO NOTHING` (the no-
    // properties case) would silently keep the old value instead.
    let conflict_clause = if prop_names.is_empty() {
        "ON CONFLICT DO NOTHING".to_string()
    } else {
        let sets: Vec<String> = prop_names.iter()
            .map(|n| format!("{} = EXCLUDED.{}", qi(n), qi(n)))
            .collect();
        format!(
            "ON CONFLICT ({}, {}) DO UPDATE SET {}",
            qi(&mutation.source_col), qi(&mutation.target_col), sets.join(", "),
        )
    };

    let ins = format!(
        "INSERT INTO {} ({}, {}{})\nSELECT \"{}\".\"id\", \"_v\".\"id\"{} FROM \"{}\" CROSS JOIN {} AS \"_v\"\n{}\nRETURNING {}, {}",
        qn(&mutation.module, &mutation.junction_table),
        qi(&mutation.source_col),
        qi(&mutation.target_col),
        extra_cols,
        ids_name,
        extra_select,
        ids_name,
        vals_ref,
        conflict_clause,
        qi(&mutation.source_col),
        qi(&mutation.target_col),
    );
    format!("\"{}\" AS (\n{}\n)", cte_name, ins)
}

/// Emit the CTE clause for a junction table DELETE (remove). See
/// emit_ml_append_cte re: `ids_name`.
fn emit_ml_remove_cte(mutation: &IrMultiLinkMutation, ids_name: &str, cte_name: &str) -> String {
    // Removal never carries link-property assignments (rejected at compile
    // time in ir/compiler.rs), so no extra columns are ever needed here.
    let vals_ref = emit_multilink_values_subquery(&mutation.values, &[]);
    let del = format!(
        "DELETE FROM {}\nWHERE {} IN (SELECT \"id\" FROM \"{}\")\n  AND {} IN (SELECT \"id\" FROM {})\nRETURNING {}, {}",
        qn(&mutation.module, &mutation.junction_table),
        qi(&mutation.source_col),
        ids_name,
        qi(&mutation.target_col),
        vals_ref,
        qi(&mutation.source_col),
        qi(&mutation.target_col),
    );
    format!("\"{}\" AS (\n{}\n)", cte_name, del)
}

// ── Conflict helper ─────────────────────────────────────────────────────────

use crate::ir::IrConflict;

fn emit_conflict(sql: &mut String, conflict: &IrConflict) {
    let on_sql = conflict.on.as_ref().map(|e| format!("({})", emit_expr(e)));
    match (&on_sql, &conflict.do_update) {
        (None, None) => sql.push_str(" ON CONFLICT DO NOTHING"),
        (Some(on), None) => sql.push_str(&format!(" ON CONFLICT {} DO NOTHING", on)),
        (None, Some(updates)) => {
            sql.push_str(&format!(" ON CONFLICT DO UPDATE SET {}", do_update_sets(updates)));
        }
        (Some(on), Some(updates)) => {
            sql.push_str(&format!(
                " ON CONFLICT {} DO UPDATE SET {}",
                on,
                do_update_sets(updates),
            ));
        }
    }
}

fn do_update_sets(updates: &[(String, IrExpr)]) -> String {
    updates
        .iter()
        .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
        .collect::<Vec<_>>()
        .join(", ")
}

// ── FREE SELECT ─────────────────────────────────────────────────────────────

/// Types that asyncpg cannot decode inside anonymous ROW() composites.
/// Return them as plain top-level columns instead.
fn is_integer_expr(expr: &IrExpr) -> bool {
    match expr {
        IrExpr::ColumnRef { pg_type, .. } =>
            matches!(pg_type.as_str(), "int2" | "int4" | "int8" | "integer" | "bigint" | "smallint"),
        IrExpr::Literal(crate::ir::IrLiteral::Int(_)) => true,
        IrExpr::BinOp(op) => is_integer_expr(&op.left) && is_integer_expr(&op.right),
        _ => false,
    }
}

fn is_raw_scalar(expr: &IrExpr) -> bool {
    matches!(expr, IrExpr::Array(_))
        || matches!(expr, IrExpr::TypeCast(c) if c.pg_type == "jsonb")
        || matches!(expr, IrExpr::NamedTuple { .. })
        || matches!(expr, IrExpr::Tuple(_))
        || matches!(expr, IrExpr::JsonbField { .. })
        || matches!(expr, IrExpr::JsonbIndex { .. })
}

/// Free (non-schema-bound) SELECT rows — one or more UNION ALL branches with
/// a ROW(…) wrapper. `rows` is `sel.rows` with any leading `Bound` entries
/// already ruled out by `emit_select_stmt`'s dispatch (a schema object and a
/// free literal can never appear in the same UNION — `check_union_type_compat`
/// rejects that at compile time), so every entry here is `IrRowSource::Free`.
fn emit_free_rows(sel: &IrSelect, rows: &[IrRowSource], ctes: &[IrCteDef]) -> SqlOutput {
    use crate::query::ShapeNode;

    let items: Vec<&IrFreeExpr> = rows.iter().map(|r| match r {
        IrRowSource::Free(item) => item,
        IrRowSource::Bound { .. } => unreachable!("mixed Bound/Free rows rejected at compile time"),
    }).collect();

    if items.is_empty() {
        return SqlOutput {
            sql: "SELECT NULL AS result WHERE FALSE".to_string(),
            shape: ShapeDescriptor {
                root: ShapeNode::Scalar { name: String::new(), position: 0 },
            },
            inference_plan: None,
        };
    }

    // assert_exists / assert_distinct: set-returning — emit as unnest, not UNION ALL
    if items.len() == 1 {
        if let IrFreeExpr::AssertSet { fn_name, inner } = items[0] {
            let array_sql = emit_array_source(inner);
            let mut sql = format!(
                "SELECT ROW(v) AS result FROM unnest(\"_pylon\".{}({})) AS _assert(v)",
                fn_name, array_sql,
            );
            if sel.distinct {
                sql = format!("SELECT DISTINCT * FROM ({}) AS \"_distinct\"", sql);
            }
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            return SqlOutput {
                sql,
                shape: ShapeDescriptor {
                    root: ShapeNode::Scalar { name: String::new(), position: 0 },
                },
                inference_plan: None,
            };
        }
    }

    let shape_root = free_item_shape(items.first().unwrap(), ctes);

    let branches: Vec<String> = items.iter().map(|item| match item {
        IrFreeExpr::Scalar(expr) => {
            // Arrays (OID 1007) and jsonb (OID 3802) can't be decoded inside anonymous
            // ROW() composites by asyncpg. Return them as plain top-level columns instead.
            if is_raw_scalar(expr) {
                format!("SELECT {} AS result", emit_expr(expr))
            } else {
                // `result` is a ROW() composite for top-level asyncpg decoding.
                // `v` is the unwrapped scalar for use in CteRef expression context.
                // Wrap in a subquery so volatile functions (nextval, etc.) are called once.
                // Enum values also need a ::text cast inside the ROW() — same
                // unregistered-OID problem as arrays/jsonb — but the bare `v`
                // column stays natively typed for CteRef expression use.
                let e = emit_expr(expr);
                let row_value = if enum_type_of_expr(expr).is_some() { "v::text" } else { "v" };
                format!("SELECT ROW({row_value}) AS result, v FROM (SELECT {e} AS v) AS _scalar")
            }
        }
        IrFreeExpr::FreeObject(fields) => {
            // Each field is computed once in an inner subquery (so a
            // volatile expression like nextval() isn't evaluated twice) and
            // exposed both packed into the `result` composite (for whole-
            // object passthrough/decoding) and as its own named column (for
            // `IrExpr::CteFieldRef` — `with x := {a := ...} select x.a`).
            let inner_cols: Vec<String> = fields.iter().enumerate()
                .map(|(i, (_, e))| format!("{} AS \"_f{}\"", emit_expr(e), i))
                .collect();
            let row_items: Vec<String> = fields.iter().enumerate()
                .map(|(i, (_, e))| {
                    if enum_type_of_expr(e).is_some() { format!("\"_f{}\"::text", i) } else { format!("\"_f{}\"", i) }
                })
                .collect();
            let named_cols: Vec<String> = fields.iter().enumerate()
                .map(|(i, (name, _))| format!("\"_f{}\" AS {}", i, qi(name)))
                .collect();
            format!(
                "SELECT ROW({}) AS result, {} FROM (SELECT {}) AS _obj",
                row_items.join(", "), named_cols.join(", "), inner_cols.join(", "),
            )
        }
        IrFreeExpr::Tuple(exprs) => {
            if exprs.len() == 1 {
                format!("SELECT ROW({}) AS result", emit_free_field_expr(&exprs[0]))
            } else {
                let parts: Vec<String> = exprs.iter().map(emit_free_field_expr).collect();
                format!("SELECT ({}) AS result", parts.join(", "))
            }
        }
        IrFreeExpr::AssertSet { .. } => unreachable!("AssertSet is handled by early return above"),
        IrFreeExpr::CtePassthrough(name) => format!("SELECT \"result\" FROM {}", qi(name)),
    }).collect();

    let union_sql = branches.join("\nUNION ALL\n");

    let mut sql = if sel.distinct {
        // Wrap UNION ALL in an outer SELECT DISTINCT to deduplicate.
        format!("SELECT DISTINCT * FROM (\n{}\n) AS \"_distinct\"", union_sql)
    } else {
        union_sql
    };
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    SqlOutput { sql, shape: ShapeDescriptor { root: shape_root }, inference_plan: None }
}

/// Determine whether `expr`'s runtime SQL type is a custom/enum type whose
/// OID asyncpg can't decode inside an anonymous ROW()/tuple composite — and
/// if so, the Postgres-schema-qualified enum type name for `ShapeNode::Enum`
/// tagging (same convention `pg_quoted_to_pylon` expects). Covers both a
/// column reference to an enum-typed property (quoted pg_type) and a bare
/// enum literal (`default::Gender.Male`) — the two ways an enum value can
/// appear as a free scalar/object/tuple field, which (unlike a schema
/// object's `emit_scalar`) has no dedicated per-pointer descriptor to carry
/// this, so it must be recovered from the expression itself.
fn enum_type_of_expr(expr: &IrExpr) -> Option<String> {
    match expr {
        IrExpr::ColumnRef { pg_type, .. } if pg_type.starts_with('"') => Some(pg_quoted_to_pylon(pg_type)),
        IrExpr::EnumLiteral { pg_type, .. } => Some(pg_quoted_to_pylon(pg_type)),
        _ => None,
    }
}

/// Emit `expr` for use as a free object/tuple field value — casts to
/// `::text` when it's enum-typed (see `enum_type_of_expr`) so asyncpg's
/// anonymous composite decoder doesn't choke on an unregistered type OID.
fn emit_free_field_expr(expr: &IrExpr) -> String {
    if enum_type_of_expr(expr).is_some() {
        format!("{}::text", emit_expr(expr))
    } else {
        emit_expr(expr)
    }
}

/// Shape node for one free scalar/object/tuple field — `Enum` when the
/// value is enum-typed (see `enum_type_of_expr`), else a plain `Scalar`.
fn free_field_shape_node(name: &str, position: usize, expr: &IrExpr) -> crate::query::ShapeNode {
    use crate::query::ShapeNode;
    match enum_type_of_expr(expr) {
        Some(enum_type) => ShapeNode::Enum { name: name.to_string(), position, enum_type },
        None => ShapeNode::Scalar { name: name.to_string(), position },
    }
}

/// Shape node for an arbitrary compiled expression appearing at a shape
/// position (a computed pointer, or a free scalar/tuple/object field) —
/// `NamedTuple`/`JsonScalar` when the expression produces jsonb (tuple
/// literals, nested free objects, tuple-typed casts), `RawScalar` when
/// asyncpg can't decode it inside an anonymous ROW() composite, `Enum` when
/// enum-typed, else a plain `Scalar`.
fn expr_shape_node(name: &str, position: usize, expr: &IrExpr) -> crate::query::ShapeNode {
    use crate::query::ShapeNode;
    match expr {
        IrExpr::TypeCast(c) if c.tuple_shape.is_some() => {
            let shape = c.tuple_shape.as_ref().unwrap();
            ShapeNode::NamedTuple {
                name: name.to_string(),
                position,
                type_name: shape.type_name.clone(),
                members: Some(shape.members.clone()),
                is_free_object: false,
            }
        }
        IrExpr::TypeCast(c) if c.pg_type == "jsonb" => ShapeNode::JsonScalar,
        IrExpr::NamedTuple { is_free_object, .. } => ShapeNode::NamedTuple {
            name: name.to_string(),
            position,
            type_name: None,
            members: None,
            is_free_object: *is_free_object,
        },
        e if is_raw_scalar(e) => ShapeNode::RawScalar,
        e => free_field_shape_node(name, position, e),
    }
}

fn free_item_shape(item: &IrFreeExpr, ctes: &[IrCteDef]) -> crate::query::ShapeNode {
    use crate::query::{Cardinality, ShapeNode};
    match item {
        IrFreeExpr::Scalar(e) => expr_shape_node("", 0, e),
        IrFreeExpr::FreeObject(fields) => ShapeNode::Object {
            name: String::new(),
            type_name: None,
            position: 0,
            cardinality: Cardinality::Many,
            pointers: fields
                .iter()
                .enumerate()
                .map(|(i, (name, e))| free_field_shape_node(name, i, e))
                .collect(),
        },
        IrFreeExpr::Tuple(exprs) => ShapeNode::Tuple {
            position: 0,
            elements: exprs
                .iter()
                .enumerate()
                .map(|(i, e)| free_field_shape_node("", i, e))
                .collect(),
        },
        IrFreeExpr::AssertSet { .. } => ShapeNode::Scalar { name: String::new(), position: 0 },
        // The referenced CTE's own `result` column carries whatever shape its
        // defining free row has (a bare scalar, a multi-field free object, a
        // tuple, or even another passthrough) — resolve it by looking the CTE
        // up rather than assuming Scalar, otherwise a multi-field free object
        // bound in a `WITH` clause decodes as just its first field's value.
        IrFreeExpr::CtePassthrough(name) => ctes
            .iter()
            .find(|c| &c.name == name)
            .and_then(|c| match &c.stmt {
                IrStmt::Select(sel) => match sel.rows.first() {
                    Some(IrRowSource::Free(inner)) => Some(free_item_shape(inner, ctes)),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(ShapeNode::Scalar { name: String::new(), position: 0 }),
    }
}

// ── PATH SELECT ─────────────────────────────────────────────────────────────

fn emit_path_joins(root: &IrSource, joins: &[IrPathJoin]) -> String {
    let mut parts = vec![format!("{} AS {}", source_ref(root), qi(&root.alias))];
    for join in joins {
        match join {
            IrPathJoin::Single { source_alias, fk_col, target } => {
                parts.push(format!(
                    "JOIN {} AS {} ON {}.{} = {}.\"id\"",
                    source_ref(target), qi(&target.alias),
                    qi(source_alias), qi(fk_col),
                    qi(&target.alias),
                ));
            }
            IrPathJoin::Multi { source_alias, junction_alias, join, target } => {
                match join {
                    IrMultiLinkJoin::Standard { junction_table, module } => {
                        parts.push(format!(
                            "JOIN {} AS {} ON {}.\"source\" = {}.\"id\"",
                            qn(module, junction_table), qi(junction_alias),
                            qi(junction_alias), qi(source_alias),
                        ));
                        parts.push(format!(
                            "JOIN {} AS {} ON {}.\"id\" = {}.\"target\"",
                            source_ref(target), qi(&target.alias),
                            qi(&target.alias), qi(junction_alias),
                        ));
                    }
                    IrMultiLinkJoin::Through { junction_table, module, source_col, target_col } => {
                        parts.push(format!(
                            "JOIN {} AS {} ON {}.{} = {}.\"id\"",
                            qn(module, junction_table), qi(junction_alias),
                            qi(junction_alias), qi(source_col),
                            qi(source_alias),
                        ));
                        parts.push(format!(
                            "JOIN {} AS {} ON {}.\"id\" = {}.{}",
                            source_ref(target), qi(&target.alias),
                            qi(&target.alias), qi(junction_alias), qi(target_col),
                        ));
                    }
                }
            }
            IrPathJoin::BacklinkSingle { source_alias, fk_col, target } => {
                parts.push(format!(
                    "JOIN {} AS {} ON {}.{} = {}.\"id\"",
                    source_ref(target), qi(&target.alias),
                    qi(&target.alias), qi(fk_col), qi(source_alias),
                ));
            }
            IrPathJoin::BacklinkMulti { source_alias, junction_alias, junction_table, module,
                                        owner_col, current_col, target } => {
                parts.push(format!(
                    "JOIN {} AS {} ON {}.{} = {}.\"id\"",
                    qn(module, junction_table), qi(junction_alias),
                    qi(junction_alias), qi(current_col), qi(source_alias),
                ));
                parts.push(format!(
                    "JOIN {} AS {} ON {}.\"id\" = {}.{}",
                    source_ref(target), qi(&target.alias),
                    qi(&target.alias), qi(junction_alias), qi(owner_col),
                ));
            }
        }
    }
    parts.join("\n")
}

/// Emit `ARRAY(SELECT scalar FROM source [JOINs] [WHERE filter])`.
fn emit_array_source(src: &IrArraySource) -> String {
    match src {
        IrArraySource::Select(s) => {
            // compile_subquery_to_array_source only ever constructs this
            // variant for a single schema-bound row.
            let [IrRowSource::Bound { source, shape }] = s.rows.as_slice() else {
                unreachable!("IrArraySource::Select is always schema-bound")
            };
            let scalar = match shape.first() {
                Some(IrShapePointer::Scalar(sf)) =>
                    format!("{}.{}", qi(&source.alias), qi(&sf.column)),
                _ => format!("{}.\"id\"", qi(&source.alias)),
            };
            let mut sql = format!("SELECT {} FROM {} AS {}",
                scalar, source_ref(source), qi(&source.alias));
            append_filter(&mut sql, &s.filter);
            format!("ARRAY({})", sql)
        }
        IrArraySource::PathSelect(ps) => {
            let scalar = match &ps.result {
                IrPathResult::Scalar(e, _) => emit_expr(e),
                IrPathResult::Object { alias, .. } => format!("{}.\"id\"", qi(alias)),
            };
            let from_sql = emit_path_joins(&ps.root, &ps.joins);
            let mut sql = format!("SELECT {} FROM {}", scalar, from_sql);
            append_filter(&mut sql, &ps.filter);
            format!("ARRAY({})", sql)
        }
        IrArraySource::RawExpr { source, poly_implementors, poly_columns, expr } => {
            let from_sql = if !poly_implementors.is_empty() {
                format!("(\n{}\n) AS {}",
                    emit_poly_union(poly_implementors, poly_columns),
                    qi(&source.alias))
            } else {
                format!("{} AS {}", source_ref(source), qi(&source.alias))
            };
            format!("ARRAY(SELECT {} FROM {})", emit_expr(expr), from_sql)
        }
    }
}

/// Emit a group-key expression, casting schema-qualified enum ColumnRefs to `::text`
/// so asyncpg can decode them outside of a typed composite.
fn emit_key_expr(expr: &IrExpr) -> String {
    if let IrExpr::ColumnRef { alias, column, pg_type } = expr {
        if pg_type.starts_with('"') {
            let col_ref = if alias.is_empty() {
                qi(column)
            } else {
                format!("{}.{}", qi(alias), qi(column))
            };
            return format!("{}::text", col_ref);
        }
    }
    emit_expr(expr)
}

fn emit_group(grp: &IrGroup) -> SqlOutput {
    let alias = &grp.source.alias;
    let (shape_exprs, shape_nodes) = build_shape(&grp.shape, alias);

    // Build the elements ROW: type discriminator at pos 0, then shape pointers.
    let mut elem_row_parts = vec![type_disc(&grp.source.type_name)];
    elem_row_parts.extend(shape_exprs);
    let elem_row = elem_row_parts.join(",\n            ");

    // Key positions: NULL at pos 0 (type slot), keys at 1..=N, grouping at N+1, elements at N+2.
    let n_keys = grp.keys.len();
    let grouping_pos = n_keys + 1;
    let elements_pos = n_keys + 2;

    // Build key SQL expressions and ShapeNodes.
    let mut key_exprs_sql: Vec<String> = vec![];
    let mut key_nodes: Vec<ShapeNode> = vec![];
    for (i, (key_name, key_expr)) in grp.keys.iter().enumerate() {
        let pos = i + 1;
        if let IrExpr::ColumnRef { pg_type, .. } = key_expr {
            if pg_type.starts_with('"') {
                key_exprs_sql.push(emit_key_expr(key_expr));
                let enum_type = pg_quoted_to_pylon(pg_type);
                key_nodes.push(ShapeNode::Enum {
                    name: key_name.clone(),
                    position: pos,
                    enum_type,
                });
                continue;
            }
        }
        key_exprs_sql.push(emit_expr(key_expr));
        key_nodes.push(ShapeNode::Scalar { name: key_name.clone(), position: pos });
    }

    // Build the outer SELECT tuple.
    let mut outer_parts = vec!["NULL::text".to_string()];
    outer_parts.extend(key_exprs_sql.clone());
    let key_names_sql = grp.keys.iter()
        .map(|(name, _)| format!("'{}'", name))
        .collect::<Vec<_>>()
        .join(", ");
    outer_parts.push(format!("ARRAY[{}]::text[]", key_names_sql));
    outer_parts.push(format!(
        "array_agg(ROW(\n            {}\n        )::record)",
        elem_row
    ));

    let outer_tuple = outer_parts.join(",\n    ");

    let group_by_sql = grp.keys.iter()
        .map(|(_, key_expr)| emit_expr(key_expr))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT (\n    {}\n) AS \"result\"\nFROM {} AS {}\nGROUP BY {}",
        outer_tuple,
        source_ref(&grp.source),
        qi(alias),
        group_by_sql,
    );

    // ShapeNode for each element (Object with the selected pointers).
    let element_node = ShapeNode::Object {
        name: String::new(),
        type_name: Some(grp.source.type_name.clone()),
        position: 0,
        cardinality: Cardinality::Many,
        pointers: prepend_type(shape_nodes),
    };

    let root = ShapeNode::Group {
        key_nodes,
        grouping_position: grouping_pos,
        elements_position: elements_pos,
        element: Box::new(element_node),
    };

    SqlOutput { sql, shape: ShapeDescriptor { root }, inference_plan: None }
}

fn emit_poly_union_type_only(implementors: &[IrPolyImplementor]) -> String {
    implementors.iter().map(|imp| {
        format!(
            "    SELECT {}::text AS \"__type__\" FROM {}",
            sql_str(&imp.type_name),
            qn(&imp.module, &imp.table),
        )
    }).collect::<Vec<_>>().join("\n    UNION ALL\n")
}

fn emit_path_select(sel: &IrPathSelect) -> SqlOutput {
    let distinct = if sel.distinct { "DISTINCT " } else { "" };
    let from_sql = if !sel.poly_implementors.is_empty() {
        format!(
            "(\n{}\n) AS {}",
            emit_poly_union_type_only(&sel.poly_implementors),
            qi(&sel.root.alias),
        )
    } else {
        emit_path_joins(&sel.root, &sel.joins)
    };

    let (result_expr, shape_root) = match &sel.result {
        IrPathResult::Scalar(ir_expr, tuple_shape) => {
            // Named tuples / jsonb field accesses can't be decoded inside ROW() — emit raw.
            let is_nt = matches!(ir_expr, IrExpr::NamedTuple { .. })
                || matches!(ir_expr, IrExpr::Tuple(_))
                || matches!(ir_expr, IrExpr::JsonbField { .. })
                || matches!(ir_expr, IrExpr::JsonbIndex { .. })
                || matches!(ir_expr, IrExpr::ColumnRef { pg_type, .. } if pg_type.starts_with("__nt__:"))
                || tuple_shape.is_some();
            if is_nt {
                let expr_sql = format!("{} AS result", emit_expr(ir_expr));
                let shape = if matches!(ir_expr, IrExpr::JsonbField { .. } | IrExpr::JsonbIndex { .. } | IrExpr::Tuple(_)) {
                    ShapeNode::RawScalar
                } else if let Some(shape) = tuple_shape {
                    // A bare tuple-typed property reference (nominal or
                    // structural) — real member shape already resolved at
                    // compile time (see resolve_property_tuple_shape), same
                    // as a `Type { tuple_property }` shape query gets via
                    // emit_scalar, instead of falling back to `members: None`.
                    ShapeNode::NamedTuple {
                        name: String::new(),
                        position: 0,
                        type_name: shape.type_name.clone(),
                        members: Some(shape.members.clone()),
                        is_free_object: false,
                    }
                } else {
                    let type_name = match ir_expr {
                        IrExpr::ColumnRef { pg_type, .. } =>
                            pg_type.strip_prefix("__nt__:").map(|s| s.to_string()),
                        _ => None,
                    };
                    ShapeNode::NamedTuple { name: String::new(), position: 0, type_name, members: None, is_free_object: false }
                };
                (expr_sql, shape)
            } else {
                // Schema-qualified types (enums, domains) have unknown OIDs inside ROW() —
                // cast to text so asyncpg's anonymous_record_decode can handle them.
                if let IrExpr::ColumnRef { pg_type, .. } = ir_expr {
                    if pg_type.starts_with('"') {
                        let enum_type = pg_quoted_to_pylon(pg_type);
                        let expr = format!("ROW({}::text) AS result", emit_expr(ir_expr));
                        let shape = ShapeNode::Enum { name: String::new(), position: 0, enum_type };
                        (expr, shape)
                    } else {
                        let expr = format!("ROW({}) AS result", emit_expr(ir_expr));
                        (expr, ShapeNode::Scalar { name: String::new(), position: 0 })
                    }
                } else {
                    let expr = format!("ROW({}) AS result", emit_expr(ir_expr));
                    (expr, ShapeNode::Scalar { name: String::new(), position: 0 })
                }
            }
        }
        IrPathResult::Object { alias, type_name, shape } => {
            let (pointer_exprs, pointer_nodes) = build_shape(shape, alias);
            let mut parts = vec![type_disc(type_name)];
            parts.extend(pointer_exprs);
            let expr = format!("(\n    {}\n) AS result", parts.join(",\n    "));
            let shape_root = ShapeNode::Object {
                name: String::new(),
                type_name: Some(type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                pointers: prepend_type(pointer_nodes),
            };
            (expr, shape_root)
        }
    };

    let mut sql = format!("SELECT {}{}\nFROM {}", distinct, result_expr, from_sql);
    append_filter(&mut sql, &sel.filter);
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    SqlOutput { sql, shape: ShapeDescriptor { root: shape_root }, inference_plan: None }
}

// ── FOR LOOP ─────────────────────────────────────────────────────────────────

fn emit_for_stmt(f: &IrFor, user_ctes: &[IrCteDef]) -> SqlOutput {
    let IrForIterator::Values { exprs, pg_type } = &f.iterator;
    let iter_alias = format!("_for_{}", f.var_name);

    if exprs.is_empty() {
        let empty = SqlOutput {
            sql: "SELECT NULL AS result WHERE FALSE".to_string(),
            shape: ShapeDescriptor { root: ShapeNode::Scalar { name: String::new(), position: 0 } },
            inference_plan: None,
        };
        return empty;
    }

    let rows: Vec<String> = exprs.iter()
        .map(|e| format!("({}::{})", emit_expr(e), pg_type))
        .collect();

    match f.body.as_ref() {
        IrStmt::Insert(ins) => emit_for_insert(ins, &iter_alias, &rows, user_ctes),
        body => {
            let body_out = match body {
                IrStmt::Select(sel) => emit_select_stmt(sel, user_ctes),
                IrStmt::PathSelect(sel) => emit_path_select(sel),
                other => panic!("unsupported for-loop body: {:?}", other),
            };
            let values_from = format!("(VALUES {}) AS {}(\"v\")", rows.join(", "), qi(&iter_alias));
            let indent_body = body_out.sql.replace('\n', "\n    ");
            let cte_prefix = if !user_ctes.is_empty() { emit_cte_prefix(user_ctes) } else { String::new() };
            let sql = format!(
                "{}SELECT \"_body\".result\nFROM {}\nCROSS JOIN LATERAL (\n    {}\n) AS \"_body\"",
                cte_prefix, values_from, indent_body,
            );
            SqlOutput { sql, shape: body_out.shape, inference_plan: None }
        }
    }
}

fn emit_for_insert(
    ins: &IrInsert,
    iter_alias: &str,
    rows: &[String],
    user_ctes: &[IrCteDef],
) -> SqlOutput {
    let rewrite_cols: std::collections::HashSet<&str> =
        ins.rewrites.iter().map(|r| r.column.as_str()).collect();

    let cols: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(c, _)| qi(c))
        .chain(ins.rewrites.iter().map(|r| qi(&r.column)))
        .collect();
    let sel_exprs: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(_, e)| emit_expr(e))
        .chain(ins.rewrites.iter().map(|r| emit_expr(&r.expr)))
        .collect();

    let mut cte_parts: Vec<String> = emit_user_cte_parts(user_ctes);
    cte_parts.push(format!("{}(\"v\") AS (VALUES {})", qi(iter_alias), rows.join(", ")));

    let mut sql = format!(
        "WITH {}\nINSERT INTO {} ({})\nSELECT {} FROM {}",
        cte_parts.join(",\n"),
        source_ref(&ins.target),
        cols.join(", "),
        sel_exprs.join(", "),
        qi(iter_alias),
    );
    if let Some(conflict) = &ins.unless_conflict {
        emit_conflict(&mut sql, conflict);
    }
    let (shape, returning_sql) = emit_returning_shape(&ins.target, &ins.returning, false);
    if let Some(r) = returning_sql {
        sql.push_str(&r);
    }
    SqlOutput { sql, shape, inference_plan: None }
}

// ── INSERT ──────────────────────────────────────────────────────────────────

// ── Vector index enqueue helpers ─────────────────────────────────────────────

/// Build one `"_eqN" AS (INSERT INTO _pylon."IndexOutbox" ...)` CTE string.
fn enqueue_cte_sql(eq: &VectorEnqueueInfo, source_cte: &str, cte_name: &str) -> String {
    let index_name_sql = match &eq.index_name {
        None => "NULL".to_string(),
        Some(name) => sql_str(name),
    };
    format!(
        concat!(
            "\"{}\" AS (\n",
            "    INSERT INTO _pylon.\"IndexOutbox\"\n",
            "        (object_id, type_name, index_kind, index_name)\n",
            "    SELECT \"id\", {}, 'Vector'::_pylon.\"IndexKind\", {}\n",
            "    FROM \"{}\"\n",
            "    ON CONFLICT (object_id, index_kind, index_name)\n",
            "    DO UPDATE SET status = 'Pending', enqueued_at = now()\n",
            ")",
        ),
        cte_name,
        sql_str(&eq.type_name),
        index_name_sql,
        source_cte,
    )
}

/// Build the full list of enqueue CTE strings for a set of vector indexes.
fn enqueue_ctes(enqueue: &[VectorEnqueueInfo], source_cte: &str) -> Vec<String> {
    enqueue.iter().enumerate()
        .map(|(i, eq)| enqueue_cte_sql(eq, source_cte, &format!("_eq{}", i)))
        .collect()
}

/// The `_pylon."IndexOutbox".index_kind` enum label for a search backend —
/// `Postgres`-backed indexes never reach this (see `collect_search_enqueue`),
/// so there's no corresponding `IndexKind` value for that variant.
fn search_backend_index_kind(backend: &crate::schema::SearchBackend) -> &'static str {
    match backend {
        crate::schema::SearchBackend::OpenSearch => "OpenSearch",
        crate::schema::SearchBackend::Meilisearch => "Meilisearch",
        crate::schema::SearchBackend::Postgres => {
            unreachable!("Postgres-backed search indexes are never collected into SearchEnqueueInfo")
        }
    }
}

/// Build one OpenSearch/Meilisearch outbox CTE string.
fn enqueue_search_cte_sql(eq: &SearchEnqueueInfo, source_cte: &str, cte_name: &str) -> String {
    let index_name_sql = match &eq.index_name {
        None => "NULL".to_string(),
        Some(name) => sql_str(name),
    };
    format!(
        concat!(
            "\"{}\" AS (\n",
            "    INSERT INTO _pylon.\"IndexOutbox\"\n",
            "        (object_id, type_name, index_kind, index_name, operation)\n",
            "    SELECT \"id\", {}, '{}'::_pylon.\"IndexKind\", {}, {}\n",
            "    FROM \"{}\"\n",
            "    ON CONFLICT (object_id, index_kind, index_name)\n",
            "    DO UPDATE SET status = 'Pending', operation = EXCLUDED.operation, enqueued_at = now()\n",
            ")",
        ),
        cte_name,
        sql_str(&eq.type_name),
        search_backend_index_kind(&eq.backend),
        index_name_sql,
        sql_str(eq.operation),
        source_cte,
    )
}

/// Build the full list of OpenSearch/Meilisearch enqueue CTE strings.
fn enqueue_search_ctes(enqueue: &[SearchEnqueueInfo], source_cte: &str, offset: usize) -> Vec<String> {
    enqueue.iter().enumerate()
        .map(|(i, eq)| enqueue_search_cte_sql(eq, source_cte, &format!("_es{}", offset + i)))
        .collect()
}

/// Build `SELECT (...) AS result FROM "cte_name"` plus its ShapeDescriptor,
/// mirroring `emit_returning_shape` but for the CTE-wrapper SELECT path.
fn shape_select_from_cte(
    target: &IrSource,
    returning: &[IrShapePointer],
    cte_name: &str,
) -> (ShapeDescriptor, Option<String>) {
    if returning.is_empty() {
        return (
            ShapeDescriptor { root: ShapeNode::Scalar { name: String::new(), position: 0 } },
            None,
        );
    }
    let (pointer_exprs, shape_pointers) = build_shape(returning, "");
    let mut parts = vec![type_disc(&target.type_name)];
    parts.extend(pointer_exprs);
    let tuple = parts.join(",\n    ");
    let sql = format!("SELECT (\n    {}\n) AS result\nFROM {}", tuple, qi(cte_name));
    let root_pointers = prepend_type(shape_pointers);
    let shape = ShapeDescriptor {
        root: ShapeNode::Object {
            name: String::new(),
            type_name: Some(target.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: root_pointers,
        },
    };
    (shape, Some(sql))
}

fn emit_insert_stmt(ins: &IrInsert) -> SqlOutput {
    let rewrite_cols: std::collections::HashSet<&str> =
        ins.rewrites.iter().map(|r| r.column.as_str()).collect();
    let mut cols: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(c, _)| qi(c))
        .collect();
    let mut vals: Vec<String> = ins.assignments.iter()
        .filter(|(c, _)| !rewrite_cols.contains(c.as_str()))
        .map(|(_, e)| emit_expr(e))
        .collect();
    for rw in &ins.rewrites {
        cols.push(qi(&rw.column));
        vals.push(emit_expr(&rw.expr));
    }

    if ins.enqueue_vector.is_empty() && ins.enqueue_search.is_empty() && !insert_has_any_multilink(ins) {
        let mut sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            source_ref(&ins.target), cols.join(", "), vals.join(", "),
        );
        if let Some(conflict) = &ins.unless_conflict { emit_conflict(&mut sql, conflict); }
        let (shape, returning_sql) = emit_returning_shape(&ins.target, &ins.returning, false);
        if let Some(r) = returning_sql { sql.push_str(&r); }
        return SqlOutput { sql, shape, inference_plan: None };
    }

    // Wrap path: needed for outbox enqueue CTEs and/or (see
    // emit_insert_multilink_ctes) junction-table population, both of which
    // require the row's own id, so the plain single-statement INSERT above
    // can't be used.
    let mut cte_parts = if insert_has_any_multilink(ins) {
        emit_insert_multilink_ctes(ins, "_w")
    } else {
        let mut insert_sql = format!(
            "    INSERT INTO {} ({}) VALUES ({})",
            source_ref(&ins.target), cols.join(", "), vals.join(", "),
        );
        if let Some(conflict) = &ins.unless_conflict { emit_conflict(&mut insert_sql, conflict); }
        insert_sql.push_str("\n    RETURNING \"id\"");
        vec![format!("\"_w\" AS (\n{}\n)", insert_sql)]
    };
    cte_parts.extend(enqueue_ctes(&ins.enqueue_vector, "_w"));
    cte_parts.extend(enqueue_search_ctes(&ins.enqueue_search, "_w", ins.enqueue_vector.len()));

    let (shape, select_sql) = shape_select_from_cte(&ins.target, &ins.returning, "_w");
    let sql = format!(
        "WITH\n{}\n{}",
        cte_parts.join(",\n"),
        select_sql.unwrap_or_else(|| "SELECT * FROM \"_w\"".to_string()),
    );
    SqlOutput { sql, shape, inference_plan: None }
}

// ── UPDATE ──────────────────────────────────────────────────────────────────

fn emit_poly_update_stmt(upd: &IrUpdate, user_ctes: &[IrCteDef]) -> SqlOutput {
    let alias = &upd.target.alias;
    let sets: Vec<String> = upd.assignments.iter()
        .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
        .chain(upd.rewrites.iter().map(|rw| format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr))))
        .collect();

    let mut cte_parts: Vec<String> = emit_user_cte_parts(user_ctes);
    let mut union_parts = vec![];

    for (i, imp) in upd.poly_implementors.iter().enumerate() {
        let cte_name = format!("_u{}", i);
        let mut upd_sql = format!(
            "UPDATE {} AS {}\nSET {}",
            qn(&imp.module, &imp.table),
            qi(alias),
            sets.join(", "),
        );
        append_filter(&mut upd_sql, &upd.filter);
        upd_sql.push_str(&format!("\nRETURNING {}.\"id\"", qi(alias)));
        cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte_name, upd_sql));

        let r_alias = format!("_r{}", i);
        union_parts.push(format!(
            "SELECT ROW({}::text, {}.\"id\") AS result FROM \"{}\" AS {}",
            sql_str(&imp.type_name),
            qi(&r_alias),
            cte_name,
            qi(&r_alias),
        ));
    }

    let sql = format!(
        "WITH\n{}\n{}",
        cte_parts.join(",\n"),
        union_parts.join("\nUNION ALL\n"),
    );

    let (shape, _) = emit_returning_shape(&upd.target, &upd.returning, true);
    SqlOutput { sql, shape, inference_plan: None }
}

fn emit_update_stmt(upd: &IrUpdate, user_ctes: &[IrCteDef]) -> SqlOutput {
    if !upd.poly_implementors.is_empty() {
        return emit_poly_update_stmt(upd, user_ctes);
    }
    let alias = &upd.target.alias;
    let (shape, returning_sql) = emit_returning_shape(&upd.target, &upd.returning, true);

    let has_any_multilink = !upd.multi_link_clears.is_empty()
        || !upd.multi_link_replaces.is_empty()
        || !upd.multi_link_appends.is_empty()
        || !upd.multi_link_removals.is_empty();

    if !has_any_multilink && upd.enqueue_vector.is_empty() && upd.enqueue_search.is_empty() {
        // No junction changes, no enqueue — plain UPDATE (possibly with user CTE prefix).
        let mut sets: Vec<String> = upd.assignments.iter()
            .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
            .collect();
        for rw in &upd.rewrites {
            sets.push(format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr)));
        }
        let mut sql = format!(
            "UPDATE {} AS {}\nSET {}",
            source_ref(&upd.target), qi(alias), sets.join(", "),
        );
        append_filter(&mut sql, &upd.filter);
        if let Some(r) = returning_sql { sql.push_str(&r); }
        if !user_ctes.is_empty() {
            sql = format!("{}{}", emit_cte_prefix(user_ctes), sql);
        }
        return SqlOutput { sql, shape, inference_plan: None };
    }

    if !has_any_multilink && (!upd.enqueue_vector.is_empty() || !upd.enqueue_search.is_empty()) {
        // No junction changes but need to enqueue — wrap UPDATE in a CTE.
        let mut sets: Vec<String> = upd.assignments.iter()
            .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
            .collect();
        for rw in &upd.rewrites {
            sets.push(format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr)));
        }
        let mut upd_sql = format!(
            "    UPDATE {} AS {}\n    SET {}",
            source_ref(&upd.target), qi(alias), sets.join(", "),
        );
        append_filter(&mut upd_sql, &upd.filter);
        upd_sql.push_str("\n    RETURNING \"id\"");

        let mut cte_parts: Vec<String> = emit_user_cte_parts(user_ctes);
        cte_parts.push(format!("\"_w\" AS (\n{}\n)", upd_sql));
        cte_parts.extend(enqueue_ctes(&upd.enqueue_vector, "_w"));
        cte_parts.extend(enqueue_search_ctes(&upd.enqueue_search, "_w", upd.enqueue_vector.len()));

        let (shape2, select_sql) = shape_select_from_cte(&upd.target, &upd.returning, "_w");
        let sql = format!(
            "WITH\n{}\n{}",
            cte_parts.join(",\n"),
            select_sql.unwrap_or_else(|| "SELECT * FROM \"_w\"".to_string()),
        );
        return SqlOutput { sql, shape: shape2, inference_plan: None };
    }

    // CTE-based UPDATE for junction table mutations.
    let result_expr = if !upd.returning.is_empty() {
        let (pointer_exprs, _) = build_shape(&upd.returning, alias);
        let mut parts = vec![type_disc(&upd.target.type_name)];
        parts.extend(pointer_exprs);
        parts.join(",\n    ")
    } else {
        format!("{}.id", qi(alias))
    };

    let has_scalar_changes = !upd.assignments.is_empty() || !upd.rewrites.is_empty();

    // User CTEs first.
    let mut cte_parts: Vec<String> = emit_user_cte_parts(user_ctes);

    // _ids: the target rows (updated or selected).
    if has_scalar_changes {
        let mut sets: Vec<String> = upd.assignments.iter()
            .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
            .collect();
        for rw in &upd.rewrites {
            sets.push(format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr)));
        }
        let mut upd_sql = format!(
            "UPDATE {} AS {}\nSET {}",
            source_ref(&upd.target), qi(alias), sets.join(", "),
        );
        append_filter(&mut upd_sql, &upd.filter);
        upd_sql.push_str("\nRETURNING *");
        cte_parts.push(format!("\"_ids\" AS (\n{}\n)", upd_sql));
    } else {
        let mut sel = format!(
            "SELECT {}.* FROM {} AS {}",
            qi(alias), source_ref(&upd.target), qi(alias),
        );
        append_filter(&mut sel, &upd.filter);
        cte_parts.push(format!("\"_ids\" AS (\n{}\n)", sel));
    }

    // Junction clears (`:= {}` and `:= expr` — the clear part of replace).
    for (i, clr) in upd.multi_link_clears.iter().enumerate() {
        let del = format!(
            "DELETE FROM {} WHERE {} IN (SELECT id FROM \"_ids\")",
            qn(&clr.module, &clr.junction_table), qi(&clr.source_col),
        );
        cte_parts.push(format!("\"_clr_{}\" AS (\n{}\n)", i, del));
    }

    // Junction appends (`+=`).
    for (i, app) in upd.multi_link_appends.iter().enumerate() {
        cte_parts.push(emit_ml_append_cte(app, "_ids", &format!("_ml_add_{}", i)));
    }

    // Junction removals (`-=`).
    for (i, rem) in upd.multi_link_removals.iter().enumerate() {
        cte_parts.push(emit_ml_remove_cte(rem, "_ids", &format!("_ml_rm_{}", i)));
    }

    // Junction inserts for replace (`:= expr` — insert after the clear).
    for (i, rep) in upd.multi_link_replaces.iter().enumerate() {
        cte_parts.push(emit_ml_append_cte(rep, "_ids", &format!("_ml_rep_{}", i)));
    }

    // Enqueue CTEs (source is _ids which has all columns including id).
    cte_parts.extend(enqueue_ctes(&upd.enqueue_vector, "_ids"));
    cte_parts.extend(enqueue_search_ctes(&upd.enqueue_search, "_ids", upd.enqueue_vector.len()));

    let sql = format!(
        "WITH\n{}\nSELECT (\n    {}\n) AS result\nFROM \"_ids\" AS {}",
        cte_parts.join(",\n"),
        result_expr,
        qi(alias),
    );
    SqlOutput { sql, shape, inference_plan: None }
}

// ── DELETE ──────────────────────────────────────────────────────────────────

fn emit_delete_stmt(del: &IrDelete) -> SqlOutput {
    if !del.poly_implementors.is_empty() {
        return emit_poly_delete_stmt(del);
    }
    let alias = &del.target.alias;

    if del.enqueue_search.is_empty() {
        let mut sql = format!(
            "DELETE FROM {} AS {}",
            source_ref(&del.target),
            qi(alias),
        );
        append_filter(&mut sql, &del.filter);
        let (shape, returning_sql) = emit_returning_shape(&del.target, &del.returning, true);
        if let Some(r) = returning_sql { sql.push_str(&r); }
        return SqlOutput { sql, shape, inference_plan: None };
    }

    // Wrap DELETE in a CTE to enqueue OpenSearch delete jobs.
    let mut del_sql = format!(
        "    DELETE FROM {} AS {}",
        source_ref(&del.target),
        qi(alias),
    );
    append_filter(&mut del_sql, &del.filter);
    del_sql.push_str("\n    RETURNING \"id\"");

    let mut cte_parts = vec![format!("\"_del\" AS (\n{}\n)", del_sql)];
    cte_parts.extend(enqueue_search_ctes(&del.enqueue_search, "_del", 0));

    let (shape, select_sql) = shape_select_from_cte(&del.target, &del.returning, "_del");
    let sql = format!(
        "WITH\n{}\n{}",
        cte_parts.join(",\n"),
        select_sql.unwrap_or_else(|| "SELECT * FROM \"_del\"".to_string()),
    );
    SqlOutput { sql, shape, inference_plan: None }
}

fn emit_poly_delete_stmt(del: &IrDelete) -> SqlOutput {
    let alias = &del.target.alias;
    let mut cte_parts = vec![];
    let mut union_parts = vec![];

    for (i, imp) in del.poly_implementors.iter().enumerate() {
        let cte_name = format!("_d{}", i);
        let mut del_sql = format!(
            "DELETE FROM {} AS {}",
            qn(&imp.module, &imp.table),
            qi(alias),
        );
        append_filter(&mut del_sql, &del.filter);
        del_sql.push_str(&format!("\nRETURNING {}.\"id\"", qi(alias)));
        cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte_name, del_sql));

        let r_alias = format!("_r{}", i);
        union_parts.push(format!(
            "SELECT ROW({}::text, {}.\"id\") AS result FROM \"{}\" AS {}",
            sql_str(&imp.type_name),
            qi(&r_alias),
            cte_name,
            qi(&r_alias),
        ));
    }

    let sql = format!(
        "WITH\n{}\n{}",
        cte_parts.join(",\n"),
        union_parts.join("\nUNION ALL\n"),
    );

    let (shape, _) = emit_returning_shape(&del.target, &del.returning, true);
    SqlOutput { sql, shape, inference_plan: None }
}

// ── RETURNING helper ─────────────────────────────────────────────────────────

/// Builds the RETURNING clause and ShapeDescriptor for DML.
/// `with_alias`: UPDATE/DELETE can use the table alias; INSERT cannot.
fn emit_returning_shape(
    target: &IrSource,
    returning: &[IrShapePointer],
    with_alias: bool,
) -> (ShapeDescriptor, Option<String>) {
    if returning.is_empty() {
        return (
            ShapeDescriptor {
                root: ShapeNode::Scalar { name: String::new(), position: 0 },
            },
            None,
        );
    }

    let alias = if with_alias { target.alias.as_str() } else { "" };
    let (pointer_exprs, shape_pointers) = build_shape(returning, alias);

    let mut parts = vec![type_disc(&target.type_name)];
    parts.extend(pointer_exprs);
    let tuple = parts.join(",\n    ");
    let sql = format!("\nRETURNING (\n    {}\n) AS result", tuple);

    let root_pointers = prepend_type(shape_pointers);
    let shape = ShapeDescriptor {
        root: ShapeNode::Object {
            name: String::new(),
            type_name: Some(target.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: root_pointers,
        },
    };
    (shape, Some(sql))
}

// ── Shape emission ───────────────────────────────────────────────────────────

fn emit_scalar_set(f: &IrScalarSetPointer, pos: usize) -> (String, ShapeNode) {
    let from_sql = if !f.poly_implementors.is_empty() {
        format!("(\n{}\n) AS {}",
            emit_poly_union(&f.poly_implementors, &f.poly_columns),
            qi(&f.source.alias))
    } else {
        format!("{} AS {}", source_ref(&f.source), qi(&f.source.alias))
    };
    let sql = format!(
        "(SELECT COALESCE(array_agg(ROW({})::record), ARRAY[]::record[]) FROM {})",
        emit_expr(&f.bool_expr),
        from_sql,
    );
    let node = ShapeNode::Array {
        name: f.alias.clone(),
        position: pos,
        element: Box::new(ShapeNode::Scalar { name: String::new(), position: 0 }),
    };
    (sql, node)
}

/// Build SQL expressions and ShapeNodes for `pointers`, starting at position 1
/// (position 0 is always the type discriminator, added by the caller).
fn build_shape(
    pointers: &[IrShapePointer],
    table_alias: &str,
) -> (Vec<String>, Vec<ShapeNode>) {
    let mut exprs = Vec::new();
    let mut nodes = Vec::new();

    for (i, pointer) in pointers.iter().enumerate() {
        let pos = i + 1;
        match pointer {
            IrShapePointer::Scalar(f) => {
                let (sql, node) = emit_scalar(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapePointer::SingleLink(f) => {
                let (sql, node) = emit_single_link(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapePointer::MultiLink(f) => {
                let (sql, node) = emit_multi_link(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapePointer::Computed(f) => {
                exprs.push(emit_expr(&f.expr));
                nodes.push(expr_shape_node(&f.alias, pos, &f.expr));
            }
            IrShapePointer::ScalarSet(f) => {
                let (sql, node) = emit_scalar_set(f, pos);
                exprs.push(sql);
                nodes.push(node);
            }
        }
    }

    (exprs, nodes)
}

/// Convert a PostgreSQL schema-qualified type name (`"module"."TypeName"`) to
/// a Pylon-qualified name (`module::TypeName`).
fn pg_quoted_to_pylon(pg_type: &str) -> String {
    let inner = pg_type.trim_start_matches('"');
    if let Some(idx) = inner.find(r#""."#) {
        let module = &inner[..idx];
        let type_name = inner[idx + 3..].trim_end_matches('"');
        format!("{}::{}", module, type_name)
    } else {
        pg_type.to_string()
    }
}

fn emit_scalar(f: &IrScalarPointer, table_alias: &str, pos: usize) -> (String, ShapeNode) {
    if let Some(nt_name) = f.pg_type.strip_prefix("__nt__:") {
        let sql = if table_alias.is_empty() {
            format!("{}::jsonb", qi(&f.column))
        } else {
            format!("{}.{}::jsonb", qi(table_alias), qi(&f.column))
        };
        return (sql, ShapeNode::NamedTuple {
            name: f.alias.clone(),
            position: pos,
            type_name: Some(nt_name.to_string()),
            members: f.tuple_shape.as_ref().map(|s| s.members.clone()),
            is_free_object: false,
        });
    }
    // Schema-qualified custom types (enums, domains) have runtime OIDs unknown to asyncpg's
    // anonymous_record_decode. Cast to text — the string label is all the decoder needs.
    if f.pg_type.starts_with('"') {
        let enum_type = pg_quoted_to_pylon(&f.pg_type);
        let sql = if table_alias.is_empty() {
            format!("{}::text", qi(&f.column))
        } else {
            format!("{}.{}::text", qi(table_alias), qi(&f.column))
        };
        return (sql, ShapeNode::Enum { name: f.alias.clone(), position: pos, enum_type });
    }
    // A structural pylon.Tuple[...]-typed property — same jsonb column shape
    // as the nominal `__nt__:` case above, just with no registered dataclass
    // to hydrate (type_name stays None).
    if let Some(shape) = &f.tuple_shape {
        let sql = if table_alias.is_empty() {
            format!("{}::jsonb", qi(&f.column))
        } else {
            format!("{}.{}::jsonb", qi(table_alias), qi(&f.column))
        };
        return (sql, ShapeNode::NamedTuple {
            name: f.alias.clone(),
            position: pos,
            type_name: shape.type_name.clone(),
            members: Some(shape.members.clone()),
            is_free_object: false,
        });
    }
    let sql = if table_alias.is_empty() {
        format!("{}::{}", qi(&f.column), f.pg_type)
    } else {
        format!("{}.{}::{}", qi(table_alias), qi(&f.column), f.pg_type)
    };
    (sql, ShapeNode::Scalar { name: f.alias.clone(), position: pos })
}

fn emit_single_link(
    f: &IrSingleLinkPointer,
    parent_alias: &str,
    pos: usize,
) -> (String, ShapeNode) {
    let sub = &f.subquery;
    let [IrRowSource::Bound { source, shape }] = sub.rows.as_slice() else {
        unreachable!("single-link subquery is always schema-bound")
    };
    let sub_alias = &source.alias;

    let (sub_exprs, sub_nodes) = build_shape(shape, sub_alias);
    let mut parts = vec![type_disc(&source.type_name)];
    parts.extend(sub_exprs);
    let tuple = parts.join(",\n        ");

    // join condition: parent FK column = target PK column
    let mut where_parts = vec![format!(
        "{}.{} = {}.{}",
        qi(parent_alias),
        qi(&f.fk_column),
        qi(sub_alias),
        qi(&f.target_pk),
    )];
    if let Some(filter) = &sub.filter {
        where_parts.push(emit_expr(filter));
    }

    let mut sql = format!(
        "(SELECT (\n        {}\n    )\n    FROM {} AS {}\n    WHERE {}",
        tuple,
        source_ref(source),
        qi(sub_alias),
        where_parts.join(" AND "),
    );
    if !sub.order_by.is_empty() {
        let s: Vec<_> = sub.order_by.iter().map(emit_sort_clause).collect();
        sql.push_str(&format!("\n    ORDER BY {}", s.join(", ")));
    }
    sql.push(')');

    let node = ShapeNode::Object {
        name: f.alias.clone(),
        type_name: Some(source.type_name.clone()),
        position: pos,
        cardinality: Cardinality::Optional,
        pointers: prepend_type(sub_nodes),
    };
    (sql, node)
}

fn emit_multi_link(
    f: &IrMultiLinkPointer,
    parent_alias: &str,
    pos: usize,
) -> (String, ShapeNode) {
    let sub = &f.subquery;
    let [IrRowSource::Bound { source, shape }] = sub.rows.as_slice() else {
        unreachable!("multi-link subquery is always schema-bound")
    };
    let sub_alias = &source.alias;

    let (sub_exprs, mut sub_nodes) = build_shape(shape, sub_alias);
    let mut row_parts = vec![type_disc(&source.type_name)];
    row_parts.extend(sub_exprs);

    // Link properties: read from the junction table alias "jt".
    // The ShapeNode name carries the `@` prefix so hydration stores it as
    // `@prop` in the object's __dict__ and the REPL displays it with `@`.
    for lp in &f.link_properties {
        row_parts.push(format!("\"jt\".{}", qi(&lp.name)));
        // Positions are 1-based (0 = type discriminator). sub_nodes.len() gives
        // the count of already-assigned positions, so the next position is len+1.
        let pos = sub_nodes.len() + 1;
        sub_nodes.push(ShapeNode::Scalar { name: format!("@{}", lp.name), position: pos });
    }

    let row = row_parts.join(",\n            ");

    // ORDER BY inside array_agg
    let order_sql = if !sub.order_by.is_empty() {
        let s: Vec<_> = sub.order_by.iter().map(emit_sort_clause).collect();
        format!(" ORDER BY {}", s.join(", "))
    } else {
        String::new()
    };

    let (from_sql, source_cond) = match &f.join {
        IrMultiLinkJoin::Standard { junction_table, module } => {
            let from = format!(
                "FROM {} AS \"jt\"\n    INNER JOIN {} AS {}\n    ON {}.id = \"jt\".target",
                qn(module, junction_table),
                source_ref(source),
                qi(sub_alias),
                qi(sub_alias),
            );
            let cond = format!("\"jt\".source = {}.id", qi(parent_alias));
            (from, cond)
        }
        IrMultiLinkJoin::Through { junction_table, module, source_col, target_col } => {
            let from = format!(
                "FROM {} AS \"jt\"\n    INNER JOIN {} AS {}\n    ON {}.id = \"jt\".{}",
                qn(module, junction_table),
                source_ref(source),
                qi(sub_alias),
                qi(sub_alias),
                qi(target_col),
            );
            let cond = format!("\"jt\".{} = {}.id", qi(source_col), qi(parent_alias));
            (from, cond)
        }
    };

    let mut where_parts = vec![source_cond];
    if let Some(filter) = &sub.filter {
        where_parts.push(emit_expr(filter));
    }

    let sql = format!(
        "(SELECT COALESCE(\n        array_agg(ROW(\n            {}\n        )::record{}),\n        ARRAY[]::record[]\n    )\n    {}\n    WHERE {})",
        row,
        order_sql,
        from_sql,
        where_parts.join(" AND "),
    );

    let node = ShapeNode::Array {
        name: f.alias.clone(),
        position: pos,
        element: Box::new(ShapeNode::Object {
            name: String::new(),
            type_name: Some(source.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: prepend_type(sub_nodes),
        }),
    };
    (sql, node)
}

/// Prepend `ShapeNode::Scalar { name: "__type__", position: 0 }` and shift
/// existing nodes' positions by 1.
fn prepend_type(nodes: Vec<ShapeNode>) -> Vec<ShapeNode> {
    let mut out = vec![ShapeNode::Scalar { name: "__type__".into(), position: 0 }];
    out.extend(nodes);
    out
}

// ── SQL clause helpers ──────────────────────────────────────────────────────

fn append_filter(sql: &mut String, filter: &Option<IrExpr>) {
    if let Some(f) = filter {
        sql.push_str(&format!("\nWHERE {}", emit_expr(f)));
    }
}

fn append_order_by(sql: &mut String, order_by: &[IrSort]) {
    if !order_by.is_empty() {
        let s: Vec<_> = order_by.iter().map(emit_sort_clause).collect();
        sql.push_str(&format!("\nORDER BY {}", s.join(", ")));
    }
}

fn append_offset_limit(sql: &mut String, offset: &Option<IrExpr>, limit: &Option<IrExpr>) {
    if let Some(o) = offset {
        sql.push_str(&format!("\nOFFSET {}", emit_expr(o)));
    }
    if let Some(l) = limit {
        sql.push_str(&format!("\nLIMIT {}", emit_expr(l)));
    }
}

fn emit_sort_clause(s: &IrSort) -> String {
    let dir = match s.direction {
        IrSortDir::Asc => "ASC",
        IrSortDir::Desc => "DESC",
    };
    let nulls = match s.nulls {
        IrNulls::First => "NULLS FIRST",
        IrNulls::Last => "NULLS LAST",
    };
    format!("{} {} {}", emit_expr(&s.expr), dir, nulls)
}

// ── Expression emission ─────────────────────────────────────────────────────

pub fn emit_expr(expr: &IrExpr) -> String {
    match expr {
        IrExpr::ColumnRef { alias, column, .. } => {
            if alias.is_empty() {
                qi(column)
            } else {
                format!("{}.{}", qi(alias), qi(column))
            }
        }
        IrExpr::Param { index } => format!("${}", index + 1),
        IrExpr::Literal(lit) => emit_literal(lit),
        IrExpr::BinOp(op) => {
            let l = emit_expr(&op.left);
            let r = emit_expr(&op.right);
            match op.op {
                BinOpKind::Add => format!("({} + {})", l, r),
                BinOpKind::Sub => format!("({} - {})", l, r),
                BinOpKind::Mul => format!("({} * {})", l, r),
                BinOpKind::Div => format!("({} / {})", l, r),
                BinOpKind::FloorDiv => {
                    if is_integer_expr(&op.left) && is_integer_expr(&op.right) {
                        format!("({} / {})", l, r)
                    } else {
                        format!("floor(({}) / ({}))", l, r)
                    }
                }
                BinOpKind::Mod => format!("({} % {})", l, r),
                BinOpKind::Pow => format!("power({}, {})", l, r),
                BinOpKind::Eq => format!("({} = {})", l, r),
                BinOpKind::Ne => format!("({} <> {})", l, r),
                BinOpKind::Lt => format!("({} < {})", l, r),
                BinOpKind::Le => format!("({} <= {})", l, r),
                BinOpKind::Gt => format!("({} > {})", l, r),
                BinOpKind::Ge => format!("({} >= {})", l, r),
                BinOpKind::And => format!("({} AND {})", l, r),
                BinOpKind::Or => format!("({} OR {})", l, r),
                BinOpKind::Like => format!("({} LIKE {})", l, r),
                BinOpKind::Ilike => format!("({} ILIKE {})", l, r),
                BinOpKind::NotLike => format!("({} NOT LIKE {})", l, r),
                BinOpKind::NotIlike => format!("({} NOT ILIKE {})", l, r),
                BinOpKind::In => format!("({} = ANY({}))", l, r),
                BinOpKind::NotIn => format!("({} <> ALL({}))", l, r),
                BinOpKind::Coalesce => format!("COALESCE({}, {})", l, r),
                BinOpKind::Concat => format!("({} || {})", l, r),
            }
        }
        IrExpr::UnaryOp(op) => {
            let inner = emit_expr(&op.operand);
            match op.op {
                UnaryOpKind::Not => format!("(NOT {})", inner),
                UnaryOpKind::Minus => format!("(-{})", inner),
                UnaryOpKind::Exists => format!("EXISTS({})", inner),
                UnaryOpKind::Distinct => format!("DISTINCT {}", inner),
            }
        }
        IrExpr::FunctionCall(f) => {
            let args: Vec<_> = f.args.iter().map(emit_expr).collect();
            if let Some(tmpl) = &f.sql_template {
                // SqlExpression impl: substitute $1, $2, … with emitted arg SQL
                let mut sql = tmpl.to_string();
                for (i, arg) in args.iter().enumerate() {
                    sql = sql.replace(&format!("${}", i + 1), arg);
                }
                return sql;
            }
            let name = match &f.schema {
                Some(s) => format!("{}.{}", pg_schema(s), qi(&f.name)),
                None => f.name.clone(),
            };
            format!("{}({})", name, args.join(", "))
        }
        IrExpr::TypeCast(c) => {
            // PostgreSQL doesn't support arbitrary_type::jsonb; to_jsonb() accepts any input.
            // String literals have type "unknown" in PG, so cast to text first.
            if c.pg_type == "jsonb" {
                match &c.expr {
                    // A bare `$N` parameter has no PG type at all until told
                    // otherwise. to_jsonb($N) can't resolve it — it's a
                    // polymorphic function, which needs a *concrete* input
                    // type to dispatch on, and an unknown-typed placeholder
                    // gives it nothing to work with ("could not determine
                    // polymorphic type because input has type unknown").
                    // A direct ($N)::jsonb cast works because PG specially
                    // resolves an unknown-typed parameter against an
                    // explicit cast — the same reason `$1::uuid` works fine
                    // elsewhere in this file.
                    IrExpr::Param { .. } => format!("({})::jsonb", emit_expr(&c.expr)),
                    IrExpr::Literal(IrLiteral::Str(_)) => {
                        format!("to_jsonb({}::text)", emit_expr(&c.expr))
                    }
                    _ => format!("to_jsonb({})", emit_expr(&c.expr)),
                }
            } else {
                format!("({})::{}", emit_expr(&c.expr), c.pg_type)
            }
        }
        IrExpr::IfElse(ie) => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            emit_expr(&ie.condition),
            emit_expr(&ie.if_),
            emit_expr(&ie.else_),
        ),
        IrExpr::Array(elems) => {
            if elems.is_empty() {
                "ARRAY[]::text[]".to_string()
            } else {
                let parts: Vec<String> = elems.iter().map(emit_expr).collect();
                format!("ARRAY[{}]", parts.join(", "))
            }
        }
        IrExpr::Null => "NULL".to_string(),
        IrExpr::AggOverSet { fn_name, schema: _, elems } => {
            let union_all = elems
                .iter()
                .map(|e| format!("SELECT {}", emit_expr(e)))
                .collect::<Vec<_>>()
                .join(" UNION ALL ");
            format!("(SELECT {}(v) FROM ({}) AS _set(v))", fn_name, union_all)
        }
        IrExpr::AggOverQuery { fn_name, inner } => {
            let inner_sql = emit_select_stmt(inner, &[]).sql;
            format!("(SELECT {}(*) FROM ({}) _agg)", fn_name, inner_sql)
        }
        IrExpr::ArrayFromSelect(src) => emit_array_source(src),

        IrExpr::CteRef { name, scalar } => {
            // scalar CTEs emit `ROW(expr) AS result, expr AS v`; use `v` for
            // expression context so we get the plain scalar type, not record.
            let col = if *scalar { "v" } else { "id" };
            format!("(SELECT \"{}\" FROM \"{}\")", col, name)
        }

        IrExpr::CteFieldRef { name, field } => {
            format!("(SELECT {} FROM {})", qi(field), qi(name))
        }

        IrExpr::ForVar { name } => format!("\"_for_{}\".\"v\"", name),

        IrExpr::EnumLiteral { pg_type, variant } => {
            format!("'{}'::{}", variant.replace('\'', "''"), pg_type)
        }

        IrExpr::GlobalParam { index, pg_type } => {
            format!("(${}::{})", index + 1, pg_type)
        }

        IrExpr::GlobalRef { cte_name } => {
            format!("(SELECT \"value\" FROM \"{}\")", cte_name)
        }

        IrExpr::NamedTuple { fields, .. } => {
            let pairs: Vec<String> = fields.iter()
                .flat_map(|(k, v)| [format!("'{}'", k.replace('\'', "''")), emit_expr(v)])
                .collect();
            format!("jsonb_build_object({})", pairs.join(", "))
        }

        IrExpr::Tuple(elems) => {
            let items: Vec<String> = elems.iter().map(emit_expr).collect();
            format!("jsonb_build_array({})", items.join(", "))
        }

        IrExpr::Subscript { expr, index, is_array } => {
            let e = emit_expr(expr);
            let i = emit_expr(index);
            if *is_array {
                format!("_pylon.array_subscript({}, ({})::bigint)", e, i)
            } else {
                format!("_pylon.str_subscript({}, ({})::bigint)", e, i)
            }
        }

        IrExpr::Slice { expr, lower, upper, is_array } => {
            let e = emit_expr(expr);
            if *is_array {
                let lo = lower.as_deref().map(|x| format!("({}) + 1", emit_expr(x)))
                    .unwrap_or_else(|| "1".to_string());
                let hi = upper.as_deref().map(|x| emit_expr(x))
                    .unwrap_or_default();
                if hi.is_empty() {
                    format!("({})[{}:]", e, lo)
                } else {
                    format!("({})[{}:{}]", e, lo, hi)
                }
            } else {
                // substr(expr, start, length) for text/bytea.
                let start = lower.as_deref().map(|x| format!("({}) + 1", emit_expr(x)))
                    .unwrap_or_else(|| "1".to_string());
                match upper.as_deref() {
                    Some(hi_expr) => {
                        let lo_val = lower.as_deref().map(emit_expr).unwrap_or_else(|| "0".to_string());
                        // GREATEST(0, ...) so reversed bounds yield '' instead of a PG error.
                        format!("substr({}, {}, GREATEST(0, ({}) - ({})))", e, start, emit_expr(hi_expr), lo_val)
                    }
                    None => format!("substr({}, {})", e, start),
                }
            }
        }

        IrExpr::JsonbField { expr, field } => {
            format!("({}->{})", emit_expr(expr), sql_str(field))
        }

        IrExpr::JsonbIndex { expr, index } => {
            format!("({}->{})", emit_expr(expr), index)
        }

        IrExpr::FnParam { name, .. } => qi(name),

        IrExpr::PathSubquery(ps) => {
            let scalar = match &ps.result {
                IrPathResult::Scalar(e, _) => emit_expr(e),
                IrPathResult::Object { alias, .. } => format!("{}.\"id\"", qi(alias)),
            };
            let from_sql = emit_path_joins(&ps.root, &ps.joins);
            let mut sql = format!("(SELECT {}\nFROM {}", scalar, from_sql);
            append_filter(&mut sql, &ps.filter);
            sql.push(')');
            sql
        }

        IrExpr::Subquery(sel) => {
            let [IrRowSource::Bound { source, shape }] = sel.rows.as_slice() else {
                unreachable!("scalar/exists subquery is always schema-bound")
            };
            let alias = &source.alias;
            let mut sql = if shape.is_empty() {
                // EXISTS inner: SELECT 1 FROM …
                format!("(SELECT 1\nFROM {} AS {}", source_ref(source), qi(alias))
            } else {
                // Scalar subquery: SELECT alias.col FROM …
                let pk_col = shape
                    .iter()
                    .find_map(|f| if let IrShapePointer::Scalar(s) = f { Some(s.column.as_str()) } else { None })
                    .unwrap_or("id");
                format!(
                    "(SELECT {}.{}\nFROM {} AS {}",
                    qi(alias), qi(pk_col), source_ref(source), qi(alias),
                )
            };
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql.push(')');
            sql
        }
    }
}

// ── Vector search ────────────────────────────────────────────────────────────

fn emit_vector_search(vs: &IrVectorSearch) -> SqlOutput {
    let alias = &vs.source.alias;
    let dist_sql = format!(
        "{}.{} {} {}",
        qi(alias),
        qi(&vs.vector_col),
        vs.distance_op,
        emit_expr(&vs.query_expr),
    );

    // Build the object sub-tuple.  If object_shape is empty, include all properties
    // (type disc + id implicitly come from build_shape when no elements provided;
    //  with no shape elements the shape is empty, so we fall back to "just id").
    // Use build_shape when there are explicit shape pointers; otherwise emit a minimal tuple.
    let (obj_tuple, object_shape_nodes) = if vs.object_shape.is_empty() {
        // No explicit shape: produce (type_disc, id) as minimum.
        let type_expr = type_disc(&vs.source.type_name);
        let id_expr = format!("{}.\"id\"", qi(alias));
        let tuple = format!("{},\n    {}", type_expr, id_expr);
        let id_node = ShapeNode::Scalar { name: "id".to_string(), position: 1 };
        (tuple, vec![id_node])
    } else {
        let (pointer_exprs, shape_pointers) = build_shape(&vs.object_shape, alias);
        let mut parts = vec![type_disc(&vs.source.type_name)];
        parts.extend(pointer_exprs);
        (parts.join(",\n    "), prepend_type(shape_pointers))
    };

    // Outer tuple: NULL (type slot), object sub-tuple at pos 1, distance at pos 2.
    let outer = format!(
        "NULL::text,\n    ROW(\n    {}\n    )::record,\n    {}",
        obj_tuple, dist_sql,
    );
    let mut sql = format!(
        "SELECT (\n    {}\n) AS result\nFROM {} AS {}",
        outer,
        source_ref(&vs.source),
        qi(alias),
    );
    append_filter(&mut sql, &vs.filter);

    // ORDER BY distance if requested.
    if let Some(dir) = &vs.order_by_distance {
        let dir_sql = match dir { IrSortDir::Asc => "ASC", IrSortDir::Desc => "DESC" };
        sql.push_str(&format!("\nORDER BY {} {}", dist_sql, dir_sql));
    }
    append_offset_limit(&mut sql, &vs.offset, &vs.limit);

    let object_node = ShapeNode::Object {
        name: "object".to_string(),
        type_name: Some(vs.source.type_name.clone()),
        position: 1,
        cardinality: Cardinality::Many,
        pointers: object_shape_nodes,
    };
    let shape = ShapeDescriptor {
        root: ShapeNode::VectorSearch {
            object_position: 1,
            distance_position: 2,
            object_node: Box::new(object_node),
        },
    };
    let inference_plan = vs.inference_model.as_ref().map(|model_name| {
        InferencePlan::Embedding {
            model_name: model_name.clone(),
            type_name: vs.inference_type_name.clone().unwrap_or_default(),
            index_name: vs.inference_index_name.clone().unwrap_or(None),
            query_param_name: vs.inference_query_param_name.clone().unwrap_or_default(),
            query_literal: vs.inference_query_literal.clone(),
        }
    });
    SqlOutput { sql, shape, inference_plan }
}

// ── FTS search ───────────────────────────────────────────────────────────────

fn emit_fts_search(fs: &IrFtsSearch) -> SqlOutput {
    use crate::schema::SearchBackend;
    if fs.backend != SearchBackend::Postgres {
        return emit_fts_search_deferred(fs);
    }

    let alias = &fs.source.alias;
    let search_col = format!("{}.{}", qi(alias), qi(&fs.search_col));
    let query_sql = emit_expr(&fs.query_expr);
    let tsquery = format!("{}('english', {})", fs.tsquery_fn, query_sql);
    let rank_sql = format!("ts_rank({}, {})", search_col, tsquery);

    let (obj_tuple, object_shape_nodes) = if fs.object_shape.is_empty() {
        let type_expr = type_disc(&fs.source.type_name);
        let id_expr = format!("{}.\"id\"", qi(alias));
        let tuple = format!("{},\n    {}", type_expr, id_expr);
        let id_node = ShapeNode::Scalar { name: "id".to_string(), position: 1 };
        (tuple, vec![id_node])
    } else {
        let (pointer_exprs, shape_pointers) = build_shape(&fs.object_shape, alias);
        let mut parts = vec![type_disc(&fs.source.type_name)];
        parts.extend(pointer_exprs);
        (parts.join(",\n    "), prepend_type(shape_pointers))
    };

    let outer = format!(
        "NULL::text,\n    ROW(\n    {}\n    )::record,\n    {}",
        obj_tuple, rank_sql,
    );
    let mut sql = format!(
        "SELECT (\n    {}\n) AS result\nFROM {} AS {}\nWHERE {} @@ {}",
        outer,
        source_ref(&fs.source),
        qi(alias),
        search_col,
        tsquery,
    );
    if let Some(f) = &fs.filter {
        sql.push_str(&format!(" AND ({})", emit_expr(f)));
    }
    if let Some(dir) = &fs.order_by_rank {
        let dir_sql = match dir { IrSortDir::Asc => "ASC", IrSortDir::Desc => "DESC" };
        sql.push_str(&format!("\nORDER BY {} {}", rank_sql, dir_sql));
    }
    append_offset_limit(&mut sql, &fs.offset, &fs.limit);

    let object_node = ShapeNode::Object {
        name: "object".to_string(),
        type_name: Some(fs.source.type_name.clone()),
        position: 1,
        cardinality: Cardinality::Many,
        pointers: object_shape_nodes,
    };
    let shape = ShapeDescriptor {
        root: ShapeNode::FtsSearch {
            object_position: 1,
            rank_position: 2,
            object_node: Box::new(object_node),
        },
    };
    SqlOutput { sql, shape, inference_plan: None }
}

fn emit_fts_search_deferred(fs: &IrFtsSearch) -> SqlOutput {
    let alias = &fs.source.alias;
    let ids_idx = fs.deferred_ids_param.expect("deferred_ids_param must be set for deferred backend");
    let scores_idx = fs.deferred_scores_param.expect("deferred_scores_param must be set for deferred backend");
    let ids_param = format!("${}", ids_idx + 1);
    let scores_param = format!("${}", scores_idx + 1);

    let (obj_tuple, object_shape_nodes) = if fs.object_shape.is_empty() {
        let type_expr = type_disc(&fs.source.type_name);
        let id_expr = format!("{}.\"id\"", qi(alias));
        let tuple = format!("{},\n    {}", type_expr, id_expr);
        let id_node = ShapeNode::Scalar { name: "id".to_string(), position: 1 };
        (tuple, vec![id_node])
    } else {
        let (pointer_exprs, shape_pointers) = build_shape(&fs.object_shape, alias);
        let mut parts = vec![type_disc(&fs.source.type_name)];
        parts.extend(pointer_exprs);
        (parts.join(",\n    "), prepend_type(shape_pointers))
    };

    let outer = format!(
        "NULL::text,\n    ROW(\n    {}\n    )::record,\n    \"_os\".\"score\"",
        obj_tuple,
    );
    let mut sql = format!(
        concat!(
            "SELECT (\n    {}\n) AS result\n",
            "FROM {} AS {}\n",
            "JOIN UNNEST({}::uuid[], {}::float8[]) AS \"_os\"(\"id\", \"score\")\n",
            "    ON \"_os\".\"id\" = {}.\"id\"",
        ),
        outer,
        source_ref(&fs.source),
        qi(alias),
        ids_param,
        scores_param,
        qi(alias),
    );
    if let Some(f) = &fs.filter {
        sql.push_str(&format!("\nWHERE ({})", emit_expr(f)));
    }
    if let Some(dir) = &fs.order_by_rank {
        let dir_sql = match dir { IrSortDir::Asc => "ASC", IrSortDir::Desc => "DESC" };
        sql.push_str(&format!("\nORDER BY \"_os\".\"score\" {}", dir_sql));
    }
    // limit/offset are passed to OpenSearch as size/from, not emitted in Postgres SQL.
    let size = fs.limit.as_ref().and_then(|lim| {
        if let IrExpr::Literal(IrLiteral::Int(n)) = lim { Some(*n as usize) } else { None }
    });

    let object_node = ShapeNode::Object {
        name: "object".to_string(),
        type_name: Some(fs.source.type_name.clone()),
        position: 1,
        cardinality: Cardinality::Many,
        pointers: object_shape_nodes,
    };
    let shape = ShapeDescriptor {
        root: ShapeNode::FtsSearch {
            object_position: 1,
            rank_position: 2,
            object_node: Box::new(object_node),
        },
    };
    let backend_str = match fs.backend {
        crate::schema::SearchBackend::Meilisearch => "meilisearch",
        _ => "opensearch",
    };
    let inference_plan = Some(InferencePlan::Search {
        backend: backend_str.to_string(),
        index_name: fs.deferred_index_name.clone().unwrap_or_default(),
        query_param_name: fs.deferred_query_param_name.clone().unwrap_or_default(),
        query_literal: fs.deferred_query_literal.clone(),
        size,
    });
    SqlOutput { sql, shape, inference_plan }
}

// ── Function select ──────────────────────────────────────────────────────────

fn emit_function_select(sel: &IrFunctionSelect) -> SqlOutput {
    let alias = &sel.alias;
    let (pointer_exprs, shape_pointers) = build_shape(&sel.shape, alias);

    let type_expr = if sel.polymorphic {
        format!("{}.\"__type__\"", qi(alias))
    } else {
        type_disc(&sel.type_name)
    };
    let mut parts = vec![type_expr];
    parts.extend(pointer_exprs);
    let tuple = parts.join(",\n    ");
    let distinct = if sel.distinct { "DISTINCT " } else { "" };

    let args_sql = sel.fn_args.iter().map(emit_expr).collect::<Vec<_>>().join(", ");
    let fn_call = format!("{}.{}({})", pg_schema(&sel.fn_module), qi(&sel.fn_name), args_sql);

    let from_clause = if sel.polymorphic {
        // Polymorphic: we can't peek inside the function — assume it returns
        // a __type__ column since we control the DDL.  Use the fn call directly.
        format!("{} AS {}", fn_call, qi(alias))
    } else {
        format!("{} AS {}", fn_call, qi(alias))
    };

    let mut sql = format!(
        "SELECT {}(\n    {}\n) AS result\nFROM {}",
        distinct, tuple, from_clause,
    );
    append_filter(&mut sql, &sel.filter);
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    let root_pointers = prepend_type(shape_pointers);
    SqlOutput {
        sql,
        shape: ShapeDescriptor {
            root: ShapeNode::Object {
                name: String::new(),
                type_name: Some(sel.type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                pointers: root_pointers,
            },
        },
        inference_plan: None,
    }
}

/// Emit the SQL body expression for a user-defined function DDL.
///
/// For a bare scalar free select emits just the expression — e.g. `"a" + "b"`.
/// For everything else (object functions, non-scalar free selects) emits a
/// full `SELECT … FROM …` via `emit_dml_as_cte_source`.
pub fn emit_fn_body(ir: &crate::ir::IrOutput) -> String {
    let body = match &ir.stmt {
        IrStmt::Select(sel) if matches!(sel.rows.as_slice(), [IrRowSource::Free(IrFreeExpr::Scalar(_))]) => {
            let IrRowSource::Free(IrFreeExpr::Scalar(e)) = &sel.rows[0] else { unreachable!() };
            format!("SELECT {}", emit_expr(e))
        }
        other => emit_dml_as_cte_source(other),
    };
    if ir.ctes.is_empty() {
        body
    } else {
        let cte_prefix = emit_cte_prefix(&ir.ctes);
        format!("{}{}", cte_prefix, body)
    }
}

fn emit_literal(lit: &IrLiteral) -> String {
    match lit {
        IrLiteral::Str(s) => sql_str(s),
        IrLiteral::Int(i) => i.to_string(),
        IrLiteral::Float(f) => {
            // Explicit ::float8 cast — a bare untyped numeral like `1.0`
            // defaults to Postgres `numeric`, but an un-cast PyQL float
            // literal means `float64` (matches the upstream engine/PyQL semantics; only
            // an explicit `<decimal>`/`123n` literal should ever produce a
            // real decimal). Without this, `numeric`'s different wire OID
            // also broke decoding inside ROW() composites (see
            // _pg_decode_numeric in pylon/client.py, still needed for
            // genuine decimal casts).
            let s = f.to_string();
            let s = if s.contains('.') || s.contains('e') { s } else { format!("{}.0", s) };
            format!("({}::float8)", s)
        }
        IrLiteral::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir;
    use crate::parse;
    use crate::schema::{
        FunctionDescriptor, FunctionParamDescriptor, GlobalDescriptor, LinkDescriptor,
        MultiLinkDescriptor, NamedTupleDescriptor, PropertyDescriptor, SchemaDescriptor,
        TypeDescriptor,
    };

    fn make_schema() -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![
                TypeDescriptor {
                    name: "Person".into(),
                    module: "default".into(),
                    table: "Person".into(),
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
                            default_sql: Some("uuidv7()".into()),
                        default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: true,
                            is_pk: true,
                            is_readonly: true,
                            rewrites: vec![],
                        tuple_members: None, },
                        PropertyDescriptor {
                            name: "name".into(),
                            pg_type: "text".into(),
                            nullable: false,
                            default_sql: None,
                        default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                        tuple_members: None, },
                        PropertyDescriptor {
                            name: "age".into(),
                            pg_type: "int8".into(),
                            nullable: true,
                            default_sql: None,
                        default_pyql: None,
                            description: None,
                            check_constraints: vec![],
                            is_exclusive: false,
                            is_pk: false,
                            is_readonly: false,
                            rewrites: vec![],
                        tuple_members: None, },
                    ],
                    links: vec![LinkDescriptor {
                        name: "company".into(),
                        target: "default::Company".into(),
                        nullable: true,
                        description: None,
                        default_pyql: None,
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
                        default_pyql: None,
                        on_delete: vec![],
                    }],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
                TypeDescriptor {
                    name: "Company".into(),
                    module: "default".into(),
                    table: "Company".into(),
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
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    tuple_members: None, }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
                TypeDescriptor {
                    name: "Post".into(),
                    module: "default".into(),
                    table: "Post".into(),
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
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    tuple_members: None, }],
                    links: vec![],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
            ],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![], aliases: vec![],
        }
    }

    fn compile_and_emit(query: &str) -> SqlOutput {
        let schema = make_schema();
        compile_and_emit_with(query, &schema)
    }

    fn compile_and_emit_with(query: &str, schema: &SchemaDescriptor) -> SqlOutput {
        let ast = parse::parse(query).expect("parse failed");
        let ir = ir::compile(&ast, schema).expect("IR compile failed");
        emit(&ir)
    }

    #[test]
    fn test_free_select_set_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT {1, 2, 3}").unwrap();
        let ir = ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        // Three UNION ALL branches
        assert_eq!(out.sql.matches("UNION ALL").count(), 2);
        assert!(out.sql.contains("1 AS v"));
        assert!(out.sql.contains("2 AS v"));
        assert!(out.sql.contains("3 AS v"));
        assert!(out.sql.contains("ROW(v) AS result"));
        assert!(matches!(out.shape.root, crate::query::ShapeNode::Scalar { .. }));
    }

    #[test]
    fn test_free_select_free_object() {
        let schema = make_schema();
        let ast = parse::parse("SELECT { foo := 'bar', n := 42 }").unwrap();
        let ir = ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        assert!(out.sql.contains("'bar'"));
        assert!(out.sql.contains("42"));
        assert!(out.sql.contains("AS result"));
        // Shape should describe an object with pointers foo and n
        let crate::query::ShapeNode::Object { pointers, type_name, .. } = &out.shape.root
            else { panic!("expected Object shape") };
        assert!(type_name.is_none());
        assert_eq!(pointers.len(), 2);
        assert!(matches!(&pointers[0], crate::query::ShapeNode::Scalar { name, position: 0 } if name == "foo"));
        assert!(matches!(&pointers[1], crate::query::ShapeNode::Scalar { name, position: 1 } if name == "n"));
    }

    #[test]
    fn test_free_select_object_with_enum_field_casts_to_text_and_tags_shape() {
        // Regression: `select { gender := default::Gender.Male }` failed at
        // runtime with "no decoder for composite type element ... " —
        // asyncpg can't decode a custom enum OID inside an anonymous ROW()
        // composite. free_item_shape/emit_free_select always treated every
        // free-object field as a plain untyped Scalar, never casting an
        // enum-valued field to ::text (unlike a schema object's emit_scalar,
        // which already does this for a real enum-typed property column).
        let mut schema = make_schema();
        schema.enums.push(crate::schema::EnumDescriptor {
            name: "Gender".into(),
            module: "default".into(),
            members: vec!["Male".into(), "Female".into()],
        });
        let out = compile_and_emit_with(
            "select { gender := default::Gender.Male }",
            &schema,
        );
        // The enum value is computed once (`'Male'::"public"."Gender" AS
        // "_f0"`) and reused for both the `result` composite (cast to
        // `::text` there, since asyncpg can't decode an enum OID inside an
        // anonymous ROW()) and the plain per-field column exposed for
        // `IrExpr::CteFieldRef` access — so the `::text` cast now applies
        // to that computed column, not inline on the enum literal itself.
        assert!(
            out.sql.contains("'Male'::\"public\".\"Gender\""),
            "expected the enum literal, got:\n{}", out.sql
        );
        assert!(
            out.sql.contains("ROW(\"_f0\"::text) AS result"),
            "expected the ROW composite to cast the enum field to text, got:\n{}", out.sql
        );
        let crate::query::ShapeNode::Object { pointers, .. } = &out.shape.root
            else { panic!("expected Object shape") };
        assert_eq!(pointers.len(), 1);
        assert!(matches!(
            &pointers[0],
            crate::query::ShapeNode::Enum { name, position: 0, enum_type }
                if name == "gender" && enum_type == "public::Gender"
        ), "expected Enum-tagged shape, got: {:?}", pointers[0]);
    }

    #[test]
    fn test_free_select_bare_enum_literal_casts_to_text_inside_row() {
        // Same bug, bare scalar form: `select default::Gender.Male;` (no
        // shape/object wrapper) also wraps the value in ROW() for top-level
        // asyncpg decoding.
        let mut schema = make_schema();
        schema.enums.push(crate::schema::EnumDescriptor {
            name: "Gender".into(),
            module: "default".into(),
            members: vec!["Male".into(), "Female".into()],
        });
        let out = compile_and_emit_with("select default::Gender.Male", &schema);
        assert!(
            out.sql.contains("ROW(v::text) AS result"),
            "expected ROW(v::text), got:\n{}", out.sql
        );
        assert!(matches!(
            &out.shape.root,
            crate::query::ShapeNode::Enum { enum_type, .. } if enum_type == "public::Gender"
        ), "expected Enum-tagged shape, got: {:?}", out.shape.root);
    }

    #[test]
    fn test_schema_select_distinct_emits_distinct_keyword() {
        // Regression: compile_stmt's Distinct/Detached unwrap and
        // compile_select's own re-derivation of the same unwrap were merged
        // into a single pass-through (Phase 2 of the compile_expr merge) —
        // confirm `select distinct` still compiles and emits DISTINCT.
        let out = compile_and_emit("SELECT DISTINCT Person { name }");
        assert!(out.sql.contains("DISTINCT"), "expected DISTINCT in SQL:\n{}", out.sql);
    }

    #[test]
    fn test_schema_select_detached_compiles_as_ordinary_select() {
        // Same merge — `detached` at the top level of a schema select is a
        // no-op (already independent); confirm it still compiles cleanly
        // instead of erroring or double-unwrapping.
        let out = compile_and_emit("SELECT DETACHED Person { name }");
        assert!(out.sql.contains("\"name\""), "expected name column in SQL:\n{}", out.sql);
    }

    #[test]
    fn test_free_select_tuple() {
        let schema = make_schema();
        let ast = parse::parse("SELECT (1, 2)").unwrap();
        let ir = ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        assert!(out.sql.contains("1"));
        assert!(out.sql.contains("2"));
        assert!(out.sql.contains("AS result"));
        assert!(matches!(out.shape.root, crate::query::ShapeNode::Tuple { .. }));
    }

    #[test]
    fn test_free_select_scalar_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT 'hello'").unwrap();
        let ir = ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        assert!(out.sql.contains("SELECT 'hello' AS v"));
        assert!(out.sql.contains("ROW(v) AS result"));
        assert!(matches!(out.shape.root, crate::query::ShapeNode::Scalar { .. }));
    }

    #[test]
    fn test_float_literal_casts_to_float8() {
        // Regression: an un-cast float literal like `1.0` is untyped in
        // Postgres and defaults to `numeric`, not `float64` — silently
        // changing PyQL's semantics (only an explicit `<decimal>`/`123n`
        // literal should ever produce a real decimal) and breaking decode
        // inside ROW() composites (numeric's wire format differs from
        // float8's). An explicit ::float8 cast keeps it a real float.
        let out = compile_and_emit("SELECT 1.0");
        assert!(
            out.sql.contains("(1.0::float8)"),
            "expected explicit float8 cast, got:\n{}", out.sql
        );
    }

    #[test]
    fn test_free_select_array_literal() {
        let schema = make_schema();
        let ast = parse::parse("SELECT [1, 2, 3]").unwrap();
        let ir = ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        assert!(out.sql.contains("SELECT ARRAY[1, 2, 3] AS result"));
        assert!(matches!(out.shape.root, crate::query::ShapeNode::RawScalar));
    }

    #[test]
    fn test_select_scalars() {
        let out = compile_and_emit("SELECT Person { name, age }");
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(out.sql.contains("\"name\"::text"));
        assert!(out.sql.contains("\"age\"::int8"));
        assert!(out.sql.contains("FROM \"public\".\"Person\""));
        assert!(out.sql.contains(") AS result"));
    }

    #[test]
    fn test_select_filter_param() {
        let out = compile_and_emit("SELECT Person { name } FILTER .name = $name");
        assert!(out.sql.contains("WHERE"));
        assert!(out.sql.contains("$1"));
    }

    #[test]
    fn test_schema_type_cast_select() {
        let out = compile_and_emit(
            "SELECT <default::Person><uuid>'019ef1bb-0d42-7a9f-8f6b-b38d028a49ba'",
        );
        assert!(out.sql.contains("FROM \"public\".\"Person\""));
        assert!(out.sql.contains("WHERE"));
        assert!(out.sql.contains("'019ef1bb-0d42-7a9f-8f6b-b38d028a49ba'"));
    }

    #[test]
    fn test_select_single_link() {
        let out = compile_and_emit("SELECT Person { name, company { name } }");
        assert!(out.sql.contains("'default::Company'::text"));
        assert!(out.sql.contains("FROM \"public\".\"Company\""));
        // join condition: parent FK column = target PK
        assert!(out.sql.contains("\"company_id\" = "));
    }

    #[test]
    fn test_select_multi_link() {
        let out = compile_and_emit("SELECT Person { name, posts { title } }");
        assert!(out.sql.contains("array_agg(ROW("));
        assert!(out.sql.contains("ARRAY[]::record[]"));
        assert!(out.sql.contains("'default::Post'::text"));
        assert!(out.sql.contains("\"Person.posts\""));
    }

    fn make_schema_with_through() -> SchemaDescriptor {
        let id_prop = || PropertyDescriptor {
            name: "id".into(), pg_type: "uuid".into(), nullable: false,
            default_sql: Some("gen_random_uuid()".into()), description: None,
                        default_pyql: None,
            check_constraints: vec![], is_exclusive: true, is_pk: true,
            is_readonly: true, rewrites: vec![],
        tuple_members: None, };
        let name_prop = || PropertyDescriptor {
            name: "name".into(), pg_type: "text".into(), nullable: false,
            default_sql: None, description: None, check_constraints: vec![],
                        default_pyql: None,
            is_exclusive: false, is_pk: false, is_readonly: false, rewrites: vec![],
        tuple_members: None, };
        SchemaDescriptor {
            types: vec![
                TypeDescriptor {
                    name: "Person".into(), module: "default".into(), table: "Person".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop(), name_prop()],
                    links: vec![],
                    multilinks: vec![MultiLinkDescriptor {
                        name: "friends".into(),
                        target: "default::Person".into(),
                        through: Some("default::PersonFriend".into()),
                        nullable: false, description: None, default_pyql: None, on_delete: vec![],
                    }],
                    computed: vec![], constraints: vec![], indexes: vec![], vector_indexes: vec![], search_indexes: vec![], triggers: vec![], junction: false,
                },
                TypeDescriptor {
                    name: "PersonFriend".into(), module: "default".into(), table: "PersonFriend".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop()],
                    links: vec![
                        LinkDescriptor {
                            name: "person".into(), target: "default::Person".into(),
                            nullable: false, description: None, default_pyql: None,
                            is_exclusive: false, is_readonly: false, rewrites: vec![], on_delete: vec![],
                        },
                        LinkDescriptor {
                            name: "friend".into(), target: "default::Person".into(),
                            nullable: false, description: None, default_pyql: None,
                            is_exclusive: false, is_readonly: false, rewrites: vec![], on_delete: vec![],
                        },
                    ],
                    multilinks: vec![], computed: vec![], constraints: vec![],
                    indexes: vec![], vector_indexes: vec![], search_indexes: vec![], triggers: vec![], junction: false,
                },
            ],
            scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        }
    }

    #[test]
    fn test_select_through_multi_link() {
        let schema = make_schema_with_through();
        let ast = crate::parse::parse("SELECT Person { name, friends { name } }").unwrap();
        let ir = crate::ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        // Junction table is the PersonFriend table, not the standard dotted name
        assert!(out.sql.contains("\"public\".\"PersonFriend\""));
        // Source FK column (person → Person) and target FK column (friend → Person)
        assert!(out.sql.contains("\"friend\""));
        assert!(out.sql.contains("\"person\""));
        // Still emits array_agg pattern
        assert!(out.sql.contains("array_agg(ROW("));
    }

    /// Product/Tag/ProductTag — the actual real-world shape link properties
    /// were built for (mirrors pylon-demo). Deliberately *not* built off
    /// `make_schema_with_through` (Person self-linking to Person): a
    /// same-type through-link hits a pre-existing, unrelated column-
    /// resolution bug in `multilink_junction_info` (both source_col and
    /// target_col resolve to the same link name when source type == target
    /// type) — tracked separately, not fixed here.
    fn make_schema_with_through_and_prop() -> SchemaDescriptor {
        let id_prop = || PropertyDescriptor {
            name: "id".into(), pg_type: "uuid".into(), nullable: false,
            default_sql: Some("gen_random_uuid()".into()), description: None,
            default_pyql: None, check_constraints: vec![], is_exclusive: true,
            is_pk: true, is_readonly: true, rewrites: vec![],
        tuple_members: None, };
        let name_prop = || PropertyDescriptor {
            name: "name".into(), pg_type: "text".into(), nullable: false,
            default_sql: None, description: None, check_constraints: vec![],
            default_pyql: None, is_exclusive: false, is_pk: false,
            is_readonly: false, rewrites: vec![],
        tuple_members: None, };
        SchemaDescriptor {
            types: vec![
                TypeDescriptor {
                    name: "Product".into(), module: "default".into(), table: "Product".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop(), name_prop()],
                    links: vec![],
                    multilinks: vec![MultiLinkDescriptor {
                        name: "tags".into(),
                        target: "default::Tag".into(),
                        through: Some("default::ProductTag".into()),
                        nullable: false, description: None, default_pyql: None, on_delete: vec![],
                    }],
                    computed: vec![], constraints: vec![], indexes: vec![], vector_indexes: vec![],
                    search_indexes: vec![], triggers: vec![], junction: false,
                },
                TypeDescriptor {
                    name: "Tag".into(), module: "default".into(), table: "Tag".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop(), name_prop()],
                    links: vec![], multilinks: vec![], computed: vec![], constraints: vec![],
                    indexes: vec![], vector_indexes: vec![], search_indexes: vec![], triggers: vec![],
                    junction: false,
                },
                TypeDescriptor {
                    name: "ProductTag".into(), module: "default".into(), table: "Product.tags".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop(), PropertyDescriptor {
                        name: "weight".into(), pg_type: "float8".into(), nullable: false,
                        default_sql: None, default_pyql: None, description: None,
                        check_constraints: vec![], is_exclusive: false, is_pk: false,
                        is_readonly: false, rewrites: vec![],
                    tuple_members: None, }],
                    // No declared Link pointers — matches pylon-demo's actual
                    // ProductTag, which relies on multilink_junction_info's
                    // "source"/"target" defaults.
                    links: vec![], multilinks: vec![], computed: vec![], constraints: vec![],
                    indexes: vec![], vector_indexes: vec![], search_indexes: vec![], triggers: vec![],
                    junction: true,
                },
            ],
            scalars: vec![], enums: vec![], named_tuples: vec![], globals: vec![], functions: vec![], aliases: vec![],
        }
    }

    #[test]
    fn test_multilink_append_with_link_property() {
        let schema = make_schema_with_through_and_prop();
        let out = compile_and_emit_with(
            "UPDATE Product FILTER .id = $id SET { tags += (SELECT Tag FILTER .id = $tid) { @weight := <float64>$w } }",
            &schema,
        );
        // Extra junction column present in both the INSERT column list and the SELECT list.
        assert!(out.sql.contains("\"weight\""), "missing weight column:\n{}", out.sql);
        // Re-linking an existing pair with a new weight must update in place,
        // not silently keep the old value (a bare `DO NOTHING` would).
        assert!(
            out.sql.contains("ON CONFLICT (\"source\", \"target\") DO UPDATE SET \"weight\" = EXCLUDED.\"weight\""),
            "missing upsert conflict clause:\n{}", out.sql
        );
    }

    #[test]
    fn test_multilink_append_union_with_different_link_property_values() {
        // The realistic Data Explorer scenario: multiple checked targets in
        // one `+=`, each with its own distinct property value — expressed as
        // a `union` of individually-shaped target selects.
        let schema = make_schema_with_through_and_prop();
        let out = compile_and_emit_with(
            "UPDATE Product FILTER .id = $id SET { \
                tags += (SELECT Tag FILTER .id = $aid) { @weight := <float64>$w1 } \
                    union (SELECT Tag FILTER .id = $bid) { @weight := <float64>$w2 } \
            }",
            &schema,
        );
        assert!(out.sql.contains("UNION ALL"), "expected a UNION ALL between the two shaped targets:\n{}", out.sql);
        // Both branches must select the same "weight" column (with their own
        // value) so the union has a consistent column list.
        assert_eq!(out.sql.matches("AS \"weight\"").count(), 2, "each union branch must project its own weight:\n{}", out.sql);
    }

    #[test]
    fn test_multilink_append_without_link_property_keeps_do_nothing() {
        // No `@prop := ...` anywhere — must keep the original DO NOTHING
        // behavior (no spurious upsert/extra columns for the common case).
        let schema = make_schema_with_through_and_prop();
        let out = compile_and_emit_with(
            "UPDATE Product FILTER .id = $id SET { tags += (SELECT Tag FILTER .id = $tid) }",
            &schema,
        );
        assert!(out.sql.contains("ON CONFLICT DO NOTHING"), "expected plain DO NOTHING when no link properties are set:\n{}", out.sql);
        assert!(!out.sql.contains("\"weight\""), "unexpected weight column with no link property assignment:\n{}", out.sql);
    }

    #[test]
    fn test_multilink_link_property_rejected_on_standard_junction() {
        // Person.posts is a Standard (implicit) junction — no user-declared
        // properties, so `@prop := ...` must be rejected at compile time.
        let ast = crate::parse::parse(
            "UPDATE Person FILTER .id = $id SET { posts += (SELECT Post FILTER .title = $t) { @weight := <float64>$w } }",
        ).unwrap();
        match crate::ir::compile(&ast, &make_schema()) {
            Ok(_) => panic!("expected a compile error for link property on a Standard junction"),
            Err(e) => assert!(e.to_string().contains("through"), "expected a through(...)-related error, got: {e}"),
        }
    }

    #[test]
    fn test_multilink_link_property_rejected_on_remove() {
        let schema = make_schema_with_through_and_prop();
        let ast = crate::parse::parse(
            "UPDATE Product FILTER .id = $id SET { tags -= (SELECT Tag FILTER .id = $tid) { @weight := <float64>$w } }",
        ).unwrap();
        match crate::ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected a compile error for link property on a remove (-=)"),
            Err(e) => assert!(e.to_string().contains("removing"), "expected a remove-related error, got: {e}"),
        }
    }

    #[test]
    fn test_insert_with_multilink_assignment() {
        // Regression test: `insert Product { tags := ... }` used to fail with
        // "object type 'default::Product' has no link or property 'tags'" —
        // compile_assignments_inner never checked resolve_multilink, and
        // IrInsert had no multi-link handling at all.
        let schema = make_schema_with_through_and_prop();
        let out = compile_and_emit_with(
            "INSERT Product { name := $name, tags := (SELECT Tag FILTER .id = $tid) { @weight := <float64>$w } }",
            &schema,
        );
        // The row insert must happen before the junction insert references its id.
        assert!(out.sql.contains("\"_w__ids\" AS (\nINSERT INTO"), "missing row-insert CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"_w__ml_add_0\" AS ("), "missing junction-append CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"weight\""), "missing weight column:\n{}", out.sql);
        assert!(out.sql.contains("\"_w\" AS (\n    SELECT * FROM \"_w__ids\"\n)"), "missing _w passthrough:\n{}", out.sql);
        assert_eq!(out.sql.matches("WITH\n").count(), 1, "must be a single flat top-level WITH block:\n{}", out.sql);
    }

    #[test]
    fn test_with_bound_insert_with_multilink_assignment() {
        // Same as above, but bound via WITH (the Data Explorer's actual
        // shape) — exercises emit_user_cte_parts's insert_has_any_multilink
        // branch instead of emit_insert_stmt's bare-statement path.
        let schema = make_schema_with_through_and_prop();
        let out = compile_and_emit_with(
            "with insert0 := (insert Product { name := $name, tags := (select Tag filter .id = $tid) }) select insert0",
            &schema,
        );
        assert!(out.sql.contains("\"insert0__ids\" AS (\nINSERT INTO"), "missing row-insert CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"insert0__ml_add_0\" AS ("), "missing junction-append CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"insert0\" AS (\n    SELECT * FROM \"insert0__ids\"\n)"), "missing insert0 passthrough:\n{}", out.sql);
        assert_eq!(out.sql.matches("WITH\n").count(), 1, "must be a single flat top-level WITH block:\n{}", out.sql);
    }

    #[test]
    fn test_with_block_cte_over_computed_global_merges_into_single_with_clause() {
        // Regression: `with user := (select global current_user) select user;`
        // produced two separate top-level `WITH` keywords ("syntax error at
        // or near WITH" from Postgres) whenever the computed global's own
        // expression referenced a session global (registering a global CTE
        // in addition to the user-defined "user" CTE). The merge check in
        // emit() only recognized the "WITH " (space) prefix convention, not
        // emit_cte_prefix's "WITH\n" (newline) convention used here, so it
        // prepended a second WITH block instead of merging into the first.
        let mut schema = make_schema();
        schema.globals.push(GlobalDescriptor {
            name: "current_user_id".into(),
            module: "default".into(),
            scalar_type: "UUID".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });
        schema.globals.push(GlobalDescriptor {
            name: "current_user".into(),
            module: "default".into(),
            scalar_type: "Person".into(),
            required: false,
            default_expr: None,
            computed_expr: Some(
                "select default::Person filter .id = global current_user_id".into(),
            ),
        });
        let out = compile_and_emit_with(
            "with\n  user := (select global current_user)\nselect user;",
            &schema,
        );
        assert_eq!(
            out.sql.matches("WITH").count(), 1,
            "must be a single WITH clause, got:\n{}", out.sql
        );
    }

    #[test]
    fn test_path_traversal_into_with_bound_cte_of_object_type() {
        // Regression: `with user := (select global current_user) select
        // user.name;` failed with "unknown type 'user'" — compile_path_select
        // (used for multi-step absolute paths like `user.name`) only ever
        // tried resolve_type(root_name), never checking whether the root
        // name is a WITH-block CTE bound to an object type.
        let mut schema = make_schema();
        schema.globals.push(GlobalDescriptor {
            name: "current_user_id".into(),
            module: "default".into(),
            scalar_type: "UUID".into(),
            required: false,
            default_expr: None,
            computed_expr: None,
        });
        schema.globals.push(GlobalDescriptor {
            name: "current_user".into(),
            module: "default".into(),
            scalar_type: "Person".into(),
            required: false,
            default_expr: None,
            computed_expr: Some(
                "select default::Person filter .id = global current_user_id".into(),
            ),
        });
        let out = compile_and_emit_with(
            "with\n  user := (select global current_user)\nselect user.name;",
            &schema,
        );
        assert!(out.sql.contains("FROM \"user\""), "expected path traversal from the CTE, got:\n{}", out.sql);
        assert_eq!(out.sql.matches("WITH").count(), 1, "must be a single WITH clause, got:\n{}", out.sql);
    }

    #[test]
    fn test_with_bound_free_object_passthrough_preserves_all_fields() {
        // Regression: `with test := { test2 := 1.0, test3 := 'str' } select
        // test;` decoded as just `1.0` (the CTE's first field) — a bare
        // reference to a free-object CTE (`IrFreeExpr::CtePassthrough`) had
        // its shape hardcoded to `ShapeNode::Scalar` regardless of what the
        // CTE actually held.
        let out = compile_and_emit(
            "with\n  test := { test2 := 1.0, test3 := 'str' }\nselect test;",
        );
        let ShapeNode::Object { pointers, .. } = &out.shape.root else {
            panic!("expected Object shape, got {:?}", out.shape.root)
        };
        assert_eq!(pointers.len(), 2);
        assert!(matches!(&pointers[0], ShapeNode::Scalar { name, .. } if name == "test2"));
        assert!(matches!(&pointers[1], ShapeNode::Scalar { name, .. } if name == "test3"));
    }

    #[test]
    fn test_with_bound_free_object_field_access() {
        // Regression: `with test := {...} select test.test2;` failed with
        // "unknown type 'test'" — any absolute path with 2+ steps
        // unconditionally routed to compile_path_select, which only knows
        // how to resolve real schema types, never a free-value CTE.
        let out = compile_and_emit(
            "with\n  test := { test2 := 1.0, test3 := 'str' }\nselect test.test2;",
        );
        assert!(out.sql.contains("\"test2\" FROM \"test\""), "got:\n{}", out.sql);
    }

    #[test]
    fn test_with_bound_free_object_nested_field_access_chain() {
        // A field-access chain through a *nested* free object literal
        // (`test.test3.foo`) must resolve the first hop via the CTE's own
        // materialized column, then extract `foo` as jsonb from that value
        // — the fix must not be hardcoded to exactly 2 path steps.
        let out = compile_and_emit(
            "with\n  test := { test2 := 1.0, test3 := { foo := 'bar' } }\nselect test.test3.foo;",
        );
        assert!(out.sql.contains("\"test3\" FROM \"test\""), "got:\n{}", out.sql);
        assert!(out.sql.contains("->'foo'"), "expected jsonb field extraction, got:\n{}", out.sql);
    }

    #[test]
    fn test_with_bound_free_object_nested_field_access_wrong_field_errors() {
        // A typo'd field name anywhere in the chain must still be a compile
        // error, not silently emit SQL that returns NULL at runtime.
        let schema = make_schema();
        let ast = parse::parse(
            "with\n  test := { test2 := 1.0, test3 := { foo := 'bar' } }\nselect test.test3.nope;",
        ).unwrap();
        assert!(ir::compile(&ast, &schema).is_err());
    }

    #[test]
    fn test_nested_free_object_literal_in_computed_shape_element() {
        // Regression: a free object literal (`{ foo := 'bar' }`) nested
        // inside a computed shape element — not the top-level SELECT
        // result — hit "shapes and set literals are not valid in
        // expression context"; free object literals were only ever handled
        // at the statement level.
        let out = compile_and_emit("select default::Person { id, test := { foo := 'bar' } };");
        let ShapeNode::Object { pointers, .. } = &out.shape.root else {
            panic!("expected Object shape")
        };
        let test_node = pointers.iter()
            .find(|p| matches!(p, ShapeNode::NamedTuple { name, .. } if name == "test"))
            .unwrap_or_else(|| panic!("expected a NamedTuple shape node for 'test', got {:?}", pointers));
        assert!(matches!(test_node, ShapeNode::NamedTuple { is_free_object: true, .. }));
    }

    #[test]
    fn test_bare_free_cte_reference_in_computed_shape_collapses_to_empty() {
        // A free-object CTE referenced bare (no shape) as a computed shape
        // element's value has nothing to project — matches the upstream engine, which
        // needs an explicit shape to know what to expose from a free
        // object (unlike a tuple, which is a plain value with no such
        // requirement).
        let out = compile_and_emit(
            "with\n  test := { test2 := 1.0, test3 := 'str' }\n\
             select default::Person { id, test := test };",
        );
        assert!(out.sql.contains("jsonb_build_object()"), "expected an empty free object, got:\n{}", out.sql);
    }

    #[test]
    fn test_shaped_free_cte_reference_projects_fields() {
        // `test := test { test2 }` must project just the named field out
        // of the underlying free object — neither collapsing to empty nor
        // returning every field.
        let out = compile_and_emit(
            "with\n  test := { test2 := 1.0, test3 := 'str' }\n\
             select default::Person { id, test := test { test2 } };",
        );
        assert!(out.sql.contains("jsonb_build_object('test2'"), "got:\n{}", out.sql);
        assert!(!out.sql.contains("'test3'"), "test3 should not be projected, got:\n{}", out.sql);
    }

    #[test]
    fn test_insert_multilink_remove_rejected() {
        let schema = make_schema_with_through_and_prop();
        let ast = crate::parse::parse(
            "INSERT Product { name := $name, tags -= (SELECT Tag FILTER .id = $tid) }",
        ).unwrap();
        match crate::ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected a compile error for `-=` on a multi-link at insert time"),
            Err(e) => assert!(
                e.to_string().contains("nothing to remove"),
                "expected a 'nothing to remove yet' error, got: {e}"
            ),
        }
    }

    #[test]
    fn test_multilink_junction_info_disambiguates_self_referencing_through_type() {
        // Regression test for a self-referencing through-link (Person.friends
        // via PersonFriend, which declares two Person-typed links: "person"
        // and "friend"). Naively matching purely by target type resolves
        // *both* source_col and target_col to the same first-matching link
        // ("person"), silently collapsing the junction to a single column
        // and losing the other side entirely.
        let schema = make_schema_with_through();
        let out = compile_and_emit_with(
            "UPDATE Person FILTER .id = $id SET { friends += (SELECT Person FILTER .id = $fid) }",
            &schema,
        );
        assert!(out.sql.contains("(\"person\", \"friend\")"), "expected two distinct FK columns:\n{}", out.sql);
        assert!(!out.sql.contains("(\"person\", \"person\")"), "source/target collapsed to the same column:\n{}", out.sql);
    }

    #[test]
    fn test_shape_descriptor_scalars() {
        let out = compile_and_emit("SELECT Person { name, age }");
        let ShapeNode::Object { pointers, .. } = &out.shape.root else { panic!() };
        assert_eq!(pointers.len(), 3); // __type__, name, age
        assert!(matches!(&pointers[0], ShapeNode::Scalar { name, position: 0 } if name == "__type__"));
        assert!(matches!(&pointers[1], ShapeNode::Scalar { name, position: 1 } if name == "name"));
        assert!(matches!(&pointers[2], ShapeNode::Scalar { name, position: 2 } if name == "age"));
    }

    #[test]
    fn test_shape_descriptor_multi_link() {
        let out = compile_and_emit("SELECT Person { name, posts { title } }");
        let ShapeNode::Object { pointers, .. } = &out.shape.root else { panic!() };
        // pointers: [__type__, name, posts]
        assert_eq!(pointers.len(), 3);
        let ShapeNode::Array { name, position, element } = &pointers[2] else { panic!() };
        assert_eq!(name, "posts");
        assert_eq!(*position, 2);
        let ShapeNode::Object { pointers: elem_pointers, .. } = element.as_ref() else { panic!() };
        // element pointers: [__type__, title]
        assert_eq!(elem_pointers.len(), 2);
    }

    #[test]
    fn test_select_order_by_limit() {
        let out = compile_and_emit("SELECT Person { name } ORDER BY .name ASC LIMIT 10");
        assert!(out.sql.contains("ORDER BY"));
        assert!(out.sql.contains("LIMIT 10"));
    }

    #[test]
    fn test_insert_returning() {
        let out = compile_and_emit("INSERT Person { name := 'Alice', age := 30 }");
        assert!(out.sql.contains("INSERT INTO \"public\".\"Person\""));
        assert!(out.sql.contains("RETURNING"));
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(out.sql.contains(") AS result"));
        // Bare INSERT returns pk only (the upstream engine behaviour)
        let ShapeNode::Object { cardinality, pointers, .. } = &out.shape.root else { panic!() };
        assert_eq!(*cardinality, Cardinality::Required);
        // Only __type__ and id — not name or age
        assert!(pointers.iter().any(|f| matches!(f, ShapeNode::Scalar { name, .. } if name == "id")));
        assert!(!pointers.iter().any(|f| matches!(f, ShapeNode::Scalar { name, .. } if name == "name")));
    }

    #[test]
    fn test_update_returning() {
        let out = compile_and_emit("UPDATE Person FILTER .name = $name SET { age := 31 }");
        assert!(out.sql.contains("UPDATE \"public\".\"Person\""));
        assert!(out.sql.contains("SET"));
        // Bare UPDATE returns pk only
        assert!(out.sql.contains("RETURNING"));
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(!out.sql.contains("\"name\"::text"), "bare UPDATE must not return name");
    }

    #[test]
    fn test_update_set_tuple_param_cast_uses_direct_jsonb_cast_not_to_jsonb() {
        // Regression: to_jsonb($N) on a bare, still-untyped parameter fails at
        // execution time with "could not determine polymorphic type because
        // input has type unknown" — Postgres can't dispatch a polymorphic
        // function against an unknown-typed placeholder. A direct
        // ($N)::jsonb cast resolves the parameter's type from the cast
        // itself instead, matching how e.g. `$1::uuid` already works.
        let out = compile_and_emit("UPDATE Person FILTER .id = $id SET { age := <tuple<x: float64>>$val }");
        assert!(out.sql.contains(")::jsonb"), "expected a direct ::jsonb cast, got:\n{}", out.sql);
        assert!(!out.sql.contains("to_jsonb($"), "must not pass a bare param straight into to_jsonb(): got:\n{}", out.sql);
    }

    #[test]
    fn test_empty_set_cast_to_object_type_clears_optional_link() {
        // Regression: unsetting an optional single-link generates
        // `<TargetType>{}` (a cast of the empty set, not the bare `{}` the
        // assignment-position special case in compile_assignments_inner
        // already handled) — that TypeCast wraps the empty set, so it fell
        // through to the generic Set/Shape rejection instead ("shapes and
        // set literals are not valid in expression context"). Must compile
        // to a plain NULL for the link's FK column, matching real PyQL:
        // `<AnyType>{}` is always NULL regardless of context.
        let out = compile_and_emit("UPDATE Person FILTER .id = $id SET { company := <default::Company>{} }");
        assert!(out.sql.contains("\"company_id\" = NULL"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_delete_returning() {
        let out = compile_and_emit("DELETE Person FILTER .id = $id");
        assert!(out.sql.contains("DELETE FROM \"public\".\"Person\""));
        // Bare DELETE returns pk only
        assert!(out.sql.contains("RETURNING"));
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(!out.sql.contains("\"name\"::text"), "bare DELETE must not return name");
    }

    #[test]
    fn test_select_over_insert() {
        let out = compile_and_emit(
            "SELECT (INSERT Person { name := $name, age := $age }) { id, name }",
        );
        // Must use a CTE
        assert!(out.sql.contains("WITH\n\"_dml\" AS ("));
        assert!(out.sql.contains("INSERT INTO"));
        assert!(out.sql.contains("RETURNING *"));
        // Outer SELECT shapes the result
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(out.sql.contains("\"name\"::text"));
    }

    #[test]
    fn test_select_over_update() {
        let out = compile_and_emit(
            "SELECT (UPDATE Person FILTER .id = $id SET { name := $name }) { id, name }",
        );
        assert!(out.sql.contains("WITH\n\"_dml\" AS ("));
        assert!(out.sql.contains("UPDATE"));
        assert!(out.sql.contains("RETURNING *"));
        assert!(out.sql.contains("\"name\"::text"));
    }

    #[test]
    fn test_insert_user_specified_id_denied_by_default() {
        let schema = make_schema();
        let ast = parse::parse("INSERT Person { id := <uuid>$id, name := $name, age := $age }").unwrap();
        match ir::compile_with_config(&ast, &schema, &ir::SessionConfig::default()) {
            Err(err) => assert!(err.to_string().contains("cannot assign to property 'id'"), "got: {err}"),
            Ok(_) => panic!("expected id assignment to be denied by default"),
        }
    }

    #[test]
    fn test_insert_user_specified_id_allowed_when_configured() {
        let schema = make_schema();
        let ast = parse::parse("INSERT Person { id := <uuid>$id, name := $name, age := $age }").unwrap();
        let config = ir::SessionConfig { allow_user_specified_id: true };
        let ir_out = ir::compile_with_config(&ast, &schema, &config)
            .expect("expected id assignment to be allowed with allow_user_specified_id");
        let out = emit(&ir_out);
        assert!(out.sql.contains("INSERT INTO"));
    }

    #[test]
    fn test_update_user_specified_id_denied_even_when_configured() {
        let schema = make_schema();
        let ast = parse::parse("UPDATE Person FILTER .name = $name SET { id := <uuid>$id }").unwrap();
        let config = ir::SessionConfig { allow_user_specified_id: true };
        match ir::compile_with_config(&ast, &schema, &config) {
            Err(err) => assert!(err.to_string().contains("cannot assign to property 'id'"), "got: {err}"),
            Ok(_) => panic!("expected UPDATE to always deny reassigning id"),
        }
    }

    #[test]
    fn test_select_over_update_multilink_only() {
        // Regression test: an UPDATE bound to a single external name (here,
        // the implicit "_dml" wrapper for `SELECT (UPDATE ...)`) whose SET
        // clause is *only* a multi-link mutation, with no scalar/single-link
        // assignment. This used to either emit an empty `SET` clause
        // ("UPDATE ... SET  WHERE ..." — invalid SQL) or, in an earlier fix
        // attempt, nest the junction INSERT inside "_dml"'s own CTE body —
        // which Postgres rejects outright ("WITH clause containing a
        // data-modifying statement must be at the top level"). The junction
        // CTEs must instead be hoisted out as top-level siblings of "_dml".
        let out = compile_and_emit(
            "SELECT (UPDATE Person FILTER .id = $id SET { posts += (SELECT Post FILTER .title = $title) }) { id, name }",
        );
        assert!(out.sql.contains("\"_dml__ml_add_0\""), "missing junction-append CTE:\n{}", out.sql);
        assert!(out.sql.contains("INSERT INTO"), "missing junction INSERT:\n{}", out.sql);
        // No scalar changes -> the row-source CTE must be a SELECT, not an UPDATE
        // with an empty SET clause.
        assert!(out.sql.contains("\"_dml__ids\" AS (\nSELECT"), "expected SELECT-based _ids CTE:\n{}", out.sql);
        assert!(!out.sql.contains("SET\n\nWHERE") && !out.sql.contains("SET \nWHERE"), "empty SET clause regression:\n{}", out.sql);
        // The junction CTE must be a *sibling* at the top-level WITH, not
        // nested inside another CTE's body — only one "WITH" keyword total.
        assert_eq!(out.sql.matches("WITH\n").count(), 1, "junction CTE must not be nested in a second WITH:\n{}", out.sql);
        // "_dml" itself must still resolve (as a passthrough) for the outer SELECT.
        assert!(out.sql.contains("\"_dml\" AS (\n    SELECT * FROM \"_dml__ids\"\n)"), "missing _dml passthrough:\n{}", out.sql);
    }

    #[test]
    fn test_select_over_update_scalar_and_multilink() {
        // Mixed case: a scalar assignment alongside a multi-link mutation, both
        // bound to the same external name — the scalar SET must survive *and*
        // the junction mutation must still be emitted as its own top-level CTE.
        let out = compile_and_emit(
            "SELECT (UPDATE Person FILTER .id = $id SET { name := $name, posts += (SELECT Post FILTER .title = $title) }) { id, name }",
        );
        assert!(out.sql.contains("\"_dml__ml_add_0\""), "missing junction-append CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"_dml__ids\" AS (\nUPDATE"), "expected UPDATE-based _ids CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"name\" = "), "missing scalar SET assignment:\n{}", out.sql);
        assert_eq!(out.sql.matches("WITH\n").count(), 1, "junction CTE must not be nested in a second WITH:\n{}", out.sql);
    }

    #[test]
    fn test_with_bound_insert_and_multilink_update_forward_ref() {
        // The actual shape pylon-ui's Data Explorer generates for its
        // flagship "insert + link in one batch" scenario: a same-batch
        // forward-reference (Person.posts += a not-yet-existing Post, bound
        // to insert0) inside a multi-statement WITH. This is the exact query
        // that failed live against a real Postgres before this fix ("WITH
        // clause containing a data-modifying statement must be at the top
        // level"), so it's the most important regression to pin down.
        let out = compile_and_emit(
            "with insert0 := (insert Post { title := $title }), update0 := (update Person filter .id = $id set { posts += (select insert0) }) select { insert0, update0 }",
        );
        assert!(out.sql.contains("\"insert0\" AS (\n    INSERT INTO"), "missing insert0 CTE:\n{}", out.sql);
        assert!(out.sql.contains("\"update0__ml_add_0\""), "missing junction-append CTE for update0:\n{}", out.sql);
        assert!(out.sql.contains("\"update0__ids\" AS (\nSELECT"), "expected SELECT-based update0 ids CTE (no scalar changes):\n{}", out.sql);
        assert!(out.sql.contains("\"update0\" AS (\n    SELECT * FROM \"update0__ids\"\n)"), "missing update0 passthrough:\n{}", out.sql);
        // Exactly one WITH keyword — every CTE (insert0, update0__ids,
        // update0__ml_add_0, update0) must be a top-level sibling.
        assert_eq!(out.sql.matches("WITH\n").count(), 1, "must be a single flat top-level WITH block:\n{}", out.sql);
    }

    #[test]
    fn test_select_over_delete() {
        let out = compile_and_emit(
            "SELECT (DELETE Person FILTER .id = $id) { id, name }",
        );
        assert!(out.sql.contains("WITH\n\"_dml\" AS ("));
        assert!(out.sql.contains("DELETE FROM"));
        assert!(out.sql.contains("RETURNING *"));
        assert!(out.sql.contains("\"name\"::text"));
    }

    fn make_schema_with_rewrite() -> SchemaDescriptor {
        use crate::schema::RewriteEntry;
        let mut schema = make_schema();
        // Add a `slug` property to Person with an INSERT+UPDATE rewrite: lower(.name)
        let person = schema.types.iter_mut().find(|t| t.name == "Person").unwrap();
        person.properties.push(PropertyDescriptor {
            name: "slug".into(),
            pg_type: "text".into(),
            nullable: true,
            default_sql: None,
                        default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: false,
            is_pk: false,
            is_readonly: false,
            rewrites: vec![
                RewriteEntry { on: 1, handler: "str_lower(.name)".into() },  // INSERT
                RewriteEntry { on: 2, handler: "str_lower(.name)".into() },  // UPDATE
            ],
        tuple_members: None, });
        schema
    }

    #[test]
    fn test_insert_rewrite_injected() {
        let schema = make_schema_with_rewrite();
        let out = compile_and_emit_with("INSERT Person { name := $name, age := 30 }", &schema);
        // slug should appear in the INSERT column list via the rewrite
        assert!(out.sql.contains("\"slug\""));
        // The rewrite expression str_lower($1) should appear in VALUES
        assert!(out.sql.contains("lower("));
        // $1 (name param) should be the arg
        assert!(out.sql.contains("$1"));
    }

    #[test]
    fn test_insert_rewrite_overrides_explicit_assignment() {
        let schema = make_schema_with_rewrite();
        // User explicitly assigns slug — rewrite should win (user assignment dropped)
        let out = compile_and_emit_with(
            "INSERT Person { name := $name, age := 30, slug := 'manual' }",
            &schema,
        );
        // The literal 'manual' must NOT appear — rewrite wins
        assert!(!out.sql.contains("'manual'"), "rewrite must override explicit slug assignment");
        // The rewrite expression must appear
        assert!(out.sql.contains("lower("), "rewrite expression must be present");
    }

    #[test]
    fn test_update_rewrite_in_set_clause() {
        let schema = make_schema_with_rewrite();
        let out = compile_and_emit_with(
            "UPDATE Person FILTER .id = $id SET { name := $name }",
            &schema,
        );
        assert!(out.sql.contains("SET"));
        assert!(out.sql.contains("\"slug\""));
        // Rewrite references .name which is also being SET to $name ($2).
        // After substitute_col_refs, the rewrite should use $2, not the pre-update column.
        // $1 = id (filter), $2 = name (assignment)
        assert!(out.sql.contains("lower($2)"),
            "rewrite must use new name value ($2), got:\n{}", out.sql);
        assert!(!out.sql.contains("lower(\"t0\".\"name\")"),
            "rewrite must not use pre-update column ref");
    }

    #[test]
    fn test_update_rewrite_unrelated_property_uses_row_value() {
        // If the rewrite references a property NOT being SET, it should read
        // the current row value (ColumnRef), not a parameter.
        let schema = make_schema_with_rewrite();
        // SET age only — slug rewrite references .name which is NOT being SET.
        let out = compile_and_emit_with(
            "UPDATE Person FILTER .id = $id SET { age := $age }",
            &schema,
        );
        assert!(out.sql.contains("\"slug\""));
        // .name is not being SET, so rewrite sees the current row value.
        assert!(out.sql.contains("lower(\"t0\".\"name\")"),
            "rewrite must use current row value when name is not being SET, got:\n{}", out.sql);
    }

    #[test]
    fn test_unless_conflict_do_nothing() {
        let out = compile_and_emit("INSERT Person { name := $name } UNLESS CONFLICT");
        assert!(out.sql.contains("ON CONFLICT DO NOTHING"));
    }

    #[test]
    fn test_unless_conflict_on_do_nothing() {
        let out = compile_and_emit("INSERT Person { name := $name } UNLESS CONFLICT ON .name");
        assert!(out.sql.contains("ON CONFLICT (\"name\") DO NOTHING"));
    }

    #[test]
    fn test_unless_conflict_do_update() {
        let out = compile_and_emit(
            "INSERT Person { name := $name, age := $age } \
             UNLESS CONFLICT ON .name \
             ELSE (UPDATE Person SET { age := $age })",
        );
        assert!(out.sql.contains("ON CONFLICT (\"name\") DO UPDATE SET"));
        assert!(out.sql.contains("\"age\" = $2"));
        // The param $age is shared — same index as in the INSERT VALUES
        assert!(!out.sql.contains("DO NOTHING"));
    }

    #[test]
    fn test_unless_conflict_do_update_no_on() {
        let out = compile_and_emit(
            "INSERT Person { name := $name } \
             UNLESS CONFLICT \
             ELSE (UPDATE Person SET { age := 0 })",
        );
        assert!(out.sql.contains("ON CONFLICT DO UPDATE SET"));
        assert!(out.sql.contains("\"age\" = 0"));
    }

    #[test]
    fn test_select_over_select() {
        let out = compile_and_emit(
            "SELECT (SELECT Person FILTER .age > 18) { name }",
        );
        // Must use a CTE
        assert!(out.sql.contains("WITH\n\"_dml\" AS ("));
        // CTE exposes raw columns via SELECT *
        assert!(out.sql.contains("SELECT *"));
        assert!(out.sql.contains("FROM \"public\".\"Person\""));
        // CTE carries the inner filter
        assert!(out.sql.contains("WHERE"));
        // Outer SELECT projects its own shape
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(out.sql.contains("\"name\"::text"));
    }

    #[test]
    fn test_select_over_select_with_outer_filter() {
        let out = compile_and_emit(
            "SELECT (SELECT Person FILTER .age > 18) { name } FILTER .name = $name",
        );
        assert!(out.sql.contains("WITH\n\"_dml\" AS ("));
        assert!(out.sql.contains("SELECT *"));
        // Both filters present: one inside CTE, one in outer SELECT
        assert_eq!(out.sql.matches("WHERE").count(), 2);
        assert!(out.sql.contains("$1"));
    }

    #[test]
    fn test_insert_link_subquery() {
        let out = compile_and_emit(
            "INSERT Person { name := $name, company := (SELECT Company FILTER .name = $co) }",
        );
        // The company FK column should be assigned via a scalar subquery
        assert!(out.sql.contains("\"company_id\""));
        assert!(out.sql.contains("SELECT"));
        // The subquery must select the pk (id) of Company
        assert!(out.sql.contains("\"id\""));
        assert!(out.sql.contains("FROM \"public\".\"Company\""));
        // Filter param must appear
        assert!(out.sql.contains("$2")); // $1 = name, $2 = co
    }

    #[test]
    fn test_update_link_subquery() {
        let out = compile_and_emit(
            "UPDATE Person FILTER .id = $id SET { company := (SELECT Company FILTER .name = $co) }",
        );
        assert!(out.sql.contains("\"company_id\""));
        assert!(out.sql.contains("SELECT"));
        assert!(out.sql.contains("FROM \"public\".\"Company\""));
    }

    #[test]
    fn test_computed_pointer_in_shape_emits_expression() {
        let mut schema = make_schema();
        schema.types[0].computed.push(crate::schema::ComputedDescriptor {
            name: "upper_name".into(),
            expression: "str_upper(.name)".into(),
            return_type: Some("text".into()),
        });
        let out = compile_and_emit_with("SELECT Person { upper_name }", &schema);
        assert!(out.sql.to_lowercase().contains("upper"), "expected upper() in SQL, got:\n{}", out.sql);
    }

    #[test]
    fn test_multi_sort_with_then_emits_two_order_keys() {
        let out = compile_and_emit("SELECT Person { name } ORDER BY .name THEN .age DESC");
        assert!(out.sql.contains("ORDER BY"), "expected ORDER BY");
        // Both columns should appear in the ORDER BY clause
        assert!(out.sql.contains("\"name\""));
        assert!(out.sql.contains("\"age\""));
        assert!(out.sql.contains("DESC"));
    }

    #[test]
    fn test_string_index_emits_str_subscript() {
        let out = compile_and_emit("SELECT 'hello'[1]");
        assert!(out.sql.contains("_pylon.str_subscript"), "expected _pylon.str_subscript() for string index, got:\n{}", out.sql);
    }

    #[test]
    fn test_string_slice_emits_substr() {
        let out = compile_and_emit("SELECT 'hello'[1:3]");
        assert!(out.sql.contains("substr"), "expected substr() for string slice, got:\n{}", out.sql);
    }

    #[test]
    fn test_array_index_emits_subscript() {
        let out = compile_and_emit("SELECT [1, 2, 3][1]");
        assert!(out.sql.contains("_pylon.array_subscript"), "expected _pylon.array_subscript() for array index, got:\n{}", out.sql);
    }

    #[test]
    fn test_array_slice_emits_subscript() {
        let out = compile_and_emit("SELECT [1, 2, 3][0:2]");
        assert!(!out.sql.contains("substr"), "should not use substr for array, got:\n{}", out.sql);
        assert!(out.sql.contains(")["), "expected array slice syntax, got:\n{}", out.sql);
    }

    #[test]
    fn test_open_ended_string_slice_emits_substr_no_length() {
        let out = compile_and_emit("SELECT 'hello'[2:]");
        // substr(expr, start) without length argument
        assert!(out.sql.contains("substr"), "expected substr(), got:\n{}", out.sql);
        // Should NOT have a third argument (length)
        let substr_idx = out.sql.find("substr").unwrap();
        let after = &out.sql[substr_idx..];
        let commas = after.chars().take_while(|&c| c != ')').filter(|&c| c == ',').count();
        assert_eq!(commas, 1, "open-ended slice should use 2-arg substr, got:\n{}", out.sql);
    }

    #[test]
    fn test_group_by_single_key() {
        let out = compile_and_emit("group Person { name } by .age");
        // GROUP BY clause present
        assert!(out.sql.contains("GROUP BY"), "expected GROUP BY, got:\n{}", out.sql);
        // Key column referenced
        assert!(out.sql.contains("\"age\""), "expected age column, got:\n{}", out.sql);
        // array_agg for elements
        assert!(out.sql.contains("array_agg(ROW("), "expected array_agg, got:\n{}", out.sql);
        // grouping names array
        assert!(out.sql.contains("ARRAY['age']"), "expected grouping array, got:\n{}", out.sql);
        // shape node is Group
        assert!(matches!(out.shape.root, crate::query::ShapeNode::Group { .. }));
        if let crate::query::ShapeNode::Group { key_nodes, grouping_position, elements_position, .. } = &out.shape.root {
            assert_eq!(key_nodes.len(), 1);
            assert!(matches!(&key_nodes[0], crate::query::ShapeNode::Scalar { name, position: 1 } if name == "age"));
            assert_eq!(*grouping_position, 2);
            assert_eq!(*elements_position, 3);
        }
    }

    #[test]
    fn test_group_using_alias() {
        let out = compile_and_emit("group Person using decade := .age // 10 by decade");
        assert!(out.sql.contains("GROUP BY"), "expected GROUP BY, got:\n{}", out.sql);
        assert!(out.sql.contains("ARRAY['decade']"), "expected grouping array, got:\n{}", out.sql);
        if let crate::query::ShapeNode::Group { key_nodes, .. } = &out.shape.root {
            assert_eq!(key_nodes.len(), 1);
            assert!(matches!(&key_nodes[0], crate::query::ShapeNode::Scalar { name, .. } if name == "decade"));
        }
    }

    #[test]
    fn test_abs_path_concat_same_type() {
        let out = compile_and_emit("SELECT Person.name ++ ' ' ++ Person.name");
        assert!(out.sql.contains("\"name\""), "expected name column, got:\n{}", out.sql);
        assert!(out.sql.contains("||"), "expected concat operator, got:\n{}", out.sql);
        assert!(out.sql.contains("FROM"), "expected FROM clause, got:\n{}", out.sql);
    }

    #[test]
    fn test_abs_path_single_property() {
        let out = compile_and_emit("SELECT Person.name");
        assert!(out.sql.contains("\"name\""), "expected name column, got:\n{}", out.sql);
        assert!(out.sql.contains("FROM"), "expected FROM clause, got:\n{}", out.sql);
    }

    #[test]
    fn test_pgvector_cast_emits_vector_type() {
        let out = compile_and_emit("SELECT <pgvector::vector>[1.0, 2.0, 3.0]");
        assert!(out.sql.contains("::vector"), "expected ::vector cast, got:\n{}", out.sql);
        assert!(out.sql.contains("ARRAY["), "expected ARRAY literal, got:\n{}", out.sql);
    }

    #[test]
    fn test_pgvector_euclidean_distance_emits_l2_operator() {
        let out = compile_and_emit(
            "SELECT pgvector::euclidean_distance(<pgvector::vector>[1.0, 2.0], <pgvector::vector>[3.0, 4.0])",
        );
        assert!(out.sql.contains("<->"), "expected <-> operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_pgvector_cosine_distance_emits_cosine_operator() {
        let out = compile_and_emit(
            "SELECT pgvector::cosine_distance(<pgvector::vector>[1.0, 2.0], <pgvector::vector>[3.0, 4.0])",
        );
        assert!(out.sql.contains("<=>"), "expected <=> operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_pgvector_neg_inner_product_emits_ip_operator() {
        let out = compile_and_emit(
            "SELECT pgvector::neg_inner_product(<pgvector::vector>[1.0, 2.0], <pgvector::vector>[3.0, 4.0])",
        );
        assert!(out.sql.contains("<#>"), "expected <#> operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_pgvector_inner_product_negates_ip_operator() {
        let out = compile_and_emit(
            "SELECT pgvector::inner_product(<pgvector::vector>[1.0, 2.0], <pgvector::vector>[3.0, 4.0])",
        );
        assert!(out.sql.contains("<#>"), "expected <#> operator, got:\n{}", out.sql);
        assert!(out.sql.contains("0.0"), "expected negation of <#>, got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_digest_str_and_bytes_overloads_both_use_pgcrypto_digest() {
        let out = compile_and_emit("SELECT crypto::digest('hello', 'sha256')");
        assert!(out.sql.contains("digest("), "expected pgcrypto's digest(), got:\n{}", out.sql);

        let out = compile_and_emit("SELECT crypto::digest(std::from_hex('68656c6c6f'), 'sha256')");
        assert!(out.sql.contains("digest("), "expected pgcrypto's digest(), got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_hmac_str_and_bytes_overloads_both_use_pgcrypto_hmac() {
        let out = compile_and_emit("SELECT crypto::hmac('hello', 'key', 'sha256')");
        assert!(out.sql.contains("hmac("), "expected pgcrypto's hmac(), got:\n{}", out.sql);

        let out = compile_and_emit(
            "SELECT crypto::hmac(std::from_hex('68656c6c6f'), std::from_hex('6b6579'), 'sha256')",
        );
        assert!(out.sql.contains("hmac("), "expected pgcrypto's hmac(), got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_gen_salt_zero_arg_defaults_to_blowfish() {
        let out = compile_and_emit("SELECT crypto::gen_salt()");
        assert!(out.sql.contains("gen_salt('bf')"), "expected default 'bf' salt type, got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_gen_salt_one_arg_passes_type_through() {
        let out = compile_and_emit("SELECT crypto::gen_salt('xdes')");
        assert!(out.sql.contains("gen_salt("), "expected gen_salt() call, got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_gen_salt_iter_count_casts_to_int4() {
        let out = compile_and_emit("SELECT crypto::gen_salt('xdes', 5)");
        assert!(out.sql.contains("::int4"), "expected int8 -> int4 narrowing cast, got:\n{}", out.sql);
    }

    #[test]
    fn test_crypto_crypt_uses_pgcrypto_crypt() {
        let out = compile_and_emit("SELECT crypto::crypt('hunter2', crypto::gen_salt())");
        assert!(out.sql.contains("crypt("), "expected pgcrypto's crypt(), got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_cast_emits_geometry_type() {
        let out = compile_and_emit("SELECT <postgis::geometry>'POINT(1 2)'");
        assert!(out.sql.contains("::geometry"), "expected ::geometry cast, got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_x_uses_st_x_builtin() {
        let out = compile_and_emit("SELECT postgis::x(<postgis::geometry>'POINT(1 2)')");
        assert!(out.sql.contains("st_x("), "expected st_x() call, got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_area_geometry_and_geography_overloads() {
        let out = compile_and_emit("SELECT postgis::area(<postgis::geometry>'POINT(1 2)')");
        assert!(out.sql.contains("st_area("), "expected st_area() call, got:\n{}", out.sql);

        let out = compile_and_emit(
            "SELECT postgis::area(<postgis::geography>'POINT(1 2)', true)",
        );
        assert!(out.sql.contains("st_area("), "expected st_area() call, got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_setsrid_casts_int64_arg_to_int4() {
        let out = compile_and_emit("SELECT postgis::setsrid(<postgis::geometry>'POINT(1 2)', 4326)");
        assert!(out.sql.contains("st_setsrid("), "expected st_setsrid() call, got:\n{}", out.sql);
        assert!(out.sql.contains("::int4"), "expected int8 -> int4 narrowing cast, got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_quantizecoordinates_default_arity_variants_compile() {
        // The upstream engine documents this with 3 trailing optional params; Pylon has no
        // notion of default args, so each arity is its own registered
        // overload — confirm both the 2-arg and 4-arg forms resolve.
        let out = compile_and_emit(
            "SELECT postgis::quantizecoordinates(<postgis::geometry>'POINT(1 2)', 5)",
        );
        assert!(out.sql.contains("st_quantizecoordinates("), "got:\n{}", out.sql);

        let out = compile_and_emit(
            "SELECT postgis::quantizecoordinates(<postgis::geometry>'POINT(1 2)', 5, 5, 5)",
        );
        assert!(out.sql.contains("st_quantizecoordinates("), "got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_op_contains_emits_infix_operator_not_function_call() {
        // Regression: ImplStrategy::SqlOperator was never actually consulted
        // by resolve_fn_call — it fell through to the generic "schema.name(args)"
        // FunctionCall path, which would have emitted a nonexistent
        // `"postgis".op_contains(...)` call instead of the `~` operator.
        let out = compile_and_emit(
            "SELECT postgis::op_contains(<postgis::geometry>'POINT(1 2)', <postgis::geometry>'POINT(3 4)')",
        );
        assert!(out.sql.contains(" ~ "), "expected infix ~ operator, got:\n{}", out.sql);
        assert!(!out.sql.contains("op_contains("), "must not call a literal op_contains function, got:\n{}", out.sql);
    }

    #[test]
    fn test_postgis_op_overlaps_geometry_and_geography_overloads() {
        let out = compile_and_emit(
            "SELECT postgis::op_overlaps(<postgis::geometry>'POINT(1 2)', <postgis::geometry>'POINT(3 4)')",
        );
        assert!(out.sql.contains(" && "), "expected infix && operator, got:\n{}", out.sql);

        let out = compile_and_emit(
            "SELECT postgis::op_overlaps(<postgis::geography>'POINT(1 2)', <postgis::geography>'POINT(3 4)')",
        );
        assert!(out.sql.contains(" && "), "expected infix && operator, got:\n{}", out.sql);
    }

    // ── User-defined function tests ───────────────────────────────────────────

    fn make_schema_with_fns() -> SchemaDescriptor {
        let mut s = make_schema();
        s.functions = vec![
            FunctionDescriptor {
                name: "mysum".into(),
                module: "default".into(),
                params: vec![
                    FunctionParamDescriptor { name: "a".into(), pg_type: "int8".into() },
                    FunctionParamDescriptor { name: "b".into(), pg_type: "int8".into() },
                ],
                return_pg_type: "int8".into(),
                return_is_object: false,
                return_is_set: false,
                return_is_polymorphic: false,
                volatility: "immutable".into(),
                body: "a + b".into(),
            },
            FunctionDescriptor {
                name: "adults".into(),
                module: "default".into(),
                params: vec![],
                return_pg_type: "default::Person".into(),
                return_is_object: true,
                return_is_set: true,
                return_is_polymorphic: false,
                volatility: "stable".into(),
                body: "select Person filter .age > 18".into(),
            },
        ];
        s
    }

    #[test]
    fn test_user_fn_scalar_call() {
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with("SELECT mysum(1, 2)", &schema);
        assert!(out.sql.contains("\"public\".\"mysum\""), "got:\n{}", out.sql);
    }

    #[test]
    fn test_user_fn_object_select_no_shape() {
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with("SELECT adults()", &schema);
        assert!(out.sql.contains("\"public\".\"adults\"()"), "got:\n{}", out.sql);
        assert!(out.sql.contains("FROM"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_user_fn_object_select_with_shape() {
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with("SELECT adults() { name }", &schema);
        assert!(out.sql.contains("\"public\".\"adults\"()"), "got:\n{}", out.sql);
        assert!(out.sql.contains("\"name\""), "got:\n{}", out.sql);
    }

    #[test]
    fn test_user_fn_in_cte_exposes_raw_columns() {
        // Regression: FunctionSelect as a CTE source must emit SELECT * FROM fn()
        // so the outer query can reference raw columns like t1.age.
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with(
            "WITH persons := adults() SELECT persons FILTER .age > 25",
            &schema,
        );
        assert!(
            out.sql.contains("SELECT * FROM \"public\".\"adults\"()"),
            "CTE source must be SELECT * FROM fn(), got:\n{}",
            out.sql,
        );
        assert!(out.sql.contains("\"age\""), "outer filter must reference raw column, got:\n{}", out.sql);
    }

    // ── vector::search tests ──────────────────────────────────────────────────

    fn make_schema_with_vector() -> SchemaDescriptor {
        use crate::schema::VectorIndexDescriptor;
        let mut s = make_schema();
        if let Some(td) = s.types.iter_mut().find(|t| t.name == "Person") {
            td.vector_indexes.push(VectorIndexDescriptor {
                index_name: None,
                pointers: vec!["name".into()],
                model: "test-embed".into(),
                metric: "cosine".into(),
                dimensions: 4,
            });
        }
        s
    }

    #[test]
    fn test_vector_search_bare_type_name() {
        let schema = make_schema_with_vector();
        let out = compile_and_emit_with(
            "WITH search := vector::search(Person, <pgvector::vector>[1.0, 2.0, 3.0, 4.0]) \
             SELECT search { object { name }, distance }",
            &schema,
        );
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
        assert!(out.sql.contains("<=>"), "expected cosine operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_vector_search_qualified_type_name() {
        let schema = make_schema_with_vector();
        let out = compile_and_emit_with(
            "WITH search := vector::search(default::Person, <pgvector::vector>[1.0, 2.0, 3.0, 4.0]) \
             SELECT search { object { name }, distance }",
            &schema,
        );
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
        assert!(out.sql.contains("<=>"), "expected cosine operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_vector_search_subquery_filter_included_in_where() {
        let schema = make_schema_with_vector();
        let out = compile_and_emit_with(
            "WITH search := vector::search((select Person filter .name = 'Alice'), <pgvector::vector>[1.0, 2.0, 3.0, 4.0]) \
             SELECT search { object { name }, distance }",
            &schema,
        );
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
        assert!(out.sql.contains("\"name\""), "expected name filter, got:\n{}", out.sql);
        assert!(out.sql.contains("Alice"), "expected filter value, got:\n{}", out.sql);
        assert!(out.sql.contains("<=>"), "expected cosine operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_vector_search_subquery_filter_combined_with_outer_property_filter() {
        let schema = make_schema_with_vector();
        let out = compile_and_emit_with(
            "WITH search := vector::search((select Person filter .age > 18), <pgvector::vector>[1.0, 2.0, 3.0, 4.0]) \
             SELECT search { object { name }, distance }",
            &schema,
        );
        assert!(out.sql.contains("\"age\""), "expected age pre-filter, got:\n{}", out.sql);
        assert!(out.sql.contains("18"), "expected filter value 18, got:\n{}", out.sql);
        assert!(out.sql.contains("<=>"), "expected cosine operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_vector_search_text_overload_with_subquery_filter() {
        let schema = make_schema_with_vector();
        let out = compile_and_emit_with(
            "WITH search := vector::search((select Person filter .name = 'Alice'), query := $q) \
             SELECT search { object { name }, distance }",
            &schema,
        );
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
        assert!(out.sql.contains("Alice"), "expected pre-filter value, got:\n{}", out.sql);
        assert!(out.sql.contains("float8[]"), "expected float8[] cast for deferred vec param, got:\n{}", out.sql);
        assert!(out.sql.contains("<=>"), "expected cosine operator, got:\n{}", out.sql);
    }

    #[test]
    fn test_count_type_ref_compiles_to_agg_over_query() {
        let out = compile_and_emit("SELECT count(Person)");
        assert!(out.sql.contains("count(*)"), "expected count(*), got:\n{}", out.sql);
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
    }

    #[test]
    fn test_count_qualified_type_ref_compiles_to_agg_over_query() {
        let out = compile_and_emit("SELECT count(default::Person)");
        assert!(out.sql.contains("count(*)"), "expected count(*), got:\n{}", out.sql);
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
    }

    #[test]
    fn test_count_subquery_compiles_to_agg_over_query() {
        let out = compile_and_emit("SELECT count((select Person))");
        assert!(out.sql.contains("count(*)"), "expected count(*), got:\n{}", out.sql);
        assert!(out.sql.contains("\"Person\""), "expected Person table, got:\n{}", out.sql);
    }

    #[test]
    fn test_count_subquery_with_filter() {
        let out = compile_and_emit("SELECT count((select Person filter .name = 'Alice'))");
        assert!(out.sql.contains("count(*)"), "expected count(*), got:\n{}", out.sql);
        assert!(out.sql.contains("\"name\""), "expected filter on name, got:\n{}", out.sql);
    }

    #[test]
    fn test_positional_param_compiles_to_dollar_n() {
        let out = compile_and_emit("SELECT Person FILTER .name = $0");
        assert!(out.sql.contains("$1"), "expected $1 placeholder, got:\n{}", out.sql);
    }

    #[test]
    fn test_multiple_positional_params_compile_in_order() {
        let out = compile_and_emit("SELECT Person FILTER .name = $0 AND .age > $1");
        assert!(out.sql.contains("$1"), "expected $1, got:\n{}", out.sql);
        assert!(out.sql.contains("$2"), "expected $2, got:\n{}", out.sql);
    }

    #[test]
    fn test_repeated_positional_param_reuses_slot() {
        let out = compile_and_emit("SELECT Person FILTER .name = $0 OR .name = $0");
        assert_eq!(out.sql.matches("$1").count(), 2, "both uses must reference $1, got:\n{}", out.sql);
    }

    #[test]
    fn test_cast_to_nonexistent_type_names_full_type() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = <default::Ghost>$name").unwrap();
        match ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected compile error for unknown type"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("unknown type 'default::Ghost'"),
                    "expected full type name in error, got: {msg}",
                );
            }
        }
    }

    #[test]
    fn test_cast_to_nonexistent_unqualified_type_names_type() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER .name = <Ghost>$name").unwrap();
        match ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected compile error for unknown type"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("unknown type 'Ghost'"),
                    "expected type name in error, got: {msg}",
                );
            }
        }
    }

    #[test]
    fn test_structural_tuple_cast_unnamed_resolves_to_jsonb() {
        let out = compile_and_emit("SELECT <tuple<str, bool>>$p");
        assert!(out.sql.contains("($1)::jsonb"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_jsonb_to_uuid_cast_extracts_via_text() {
        // PostgreSQL has no native jsonb -> uuid cast; extract raw text then cast.
        let out = compile_and_emit("SELECT <uuid>(<json>$p)");
        assert!(out.sql.contains("#>> '{}'"), "got:\n{}", out.sql);
        assert!(out.sql.contains("::uuid"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_jsonb_to_datetime_cast_extracts_via_text() {
        let out = compile_and_emit("SELECT <datetime>(<json>$p)");
        assert!(out.sql.contains("#>> '{}'"), "got:\n{}", out.sql);
        assert!(out.sql.contains("::timestamptz"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_jsonb_to_duration_cast_extracts_via_text() {
        let out = compile_and_emit("SELECT <duration>(<json>$p)");
        assert!(out.sql.contains("#>> '{}'"), "got:\n{}", out.sql);
        assert!(out.sql.contains("::interval"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_jsonb_to_array_cast_unpacks_each_element() {
        let out = compile_and_emit("SELECT <array<int64>>(<json>$p)");
        assert!(out.sql.contains("jsonb_array_elements("), "got:\n{}", out.sql);
        assert!(out.sql.contains("#>> '{}'"), "got:\n{}", out.sql);
        assert!(out.sql.contains("::int8"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_non_jsonb_cast_is_unaffected_by_jsonb_extraction() {
        // A plain str -> uuid cast (source isn't jsonb) must still use the
        // ordinary `::pg_type` path, not the jsonb-extraction template.
        let out = compile_and_emit("SELECT <uuid>$p");
        assert!(!out.sql.contains("#>>"), "got:\n{}", out.sql);
        assert!(out.sql.contains("::uuid"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_structural_tuple_cast_named_resolves_to_jsonb() {
        let out = compile_and_emit("SELECT <tuple<x: float64, y: float64>>$p");
        assert!(out.sql.contains("($1)::jsonb"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_structural_tuple_cast_nested_resolves_to_jsonb() {
        let out = compile_and_emit(
            "SELECT <tuple<point: tuple<x: float64, y: float64>, label: str>>$p",
        );
        assert!(out.sql.contains("($1)::jsonb"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_nominal_named_tuple_cast_resolves_to_jsonb() {
        let mut schema = make_schema();
        schema.named_tuples.push(NamedTupleDescriptor {
            name: "Point".into(),
            module: "default".into(),
            members: vec![],
        });
        let out = compile_and_emit_with("SELECT <default::Point>$p", &schema);
        assert!(out.sql.contains("($1)::jsonb"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_array_literal_cast_resolves_to_native_pg_array_not_jsonb() {
        // The exact query reported as failing — must compile now, and must
        // resolve to a real Postgres array (text[]), never jsonb (arrays
        // decode natively via asyncpg, unlike tuples).
        let out = compile_and_emit("SELECT <array<str>>['foo', 'bar']");
        assert!(out.sql.contains("::text[]") || out.sql.contains("ARRAY["), "got:\n{}", out.sql);
        assert!(!out.sql.contains("jsonb"), "arrays must not use jsonb, got:\n{}", out.sql);
    }

    #[test]
    fn test_array_literal_cast_applies_per_element_cast() {
        // Each element gets its own real cast, not a raw untyped ARRAY[...] —
        // '1' and '2' must actually coerce to int8, matching real PyQL
        // per-element cast semantics (mirrors the equivalent tuple test).
        let out = compile_and_emit("SELECT <array<int64>>['1', '2']");
        assert!(out.sql.contains("ARRAY[('1')::int8, ('2')::int8]"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_array_param_cast_uses_direct_suffix_cast() {
        let out = compile_and_emit("SELECT <array<int64>>$p");
        assert!(out.sql.contains("::int8[]"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_array_of_named_tuple_element_casts_to_jsonb_array() {
        // An array's element type can be anything except another array —
        // including a structural tuple, which still resolves that one
        // element to jsonb while the array itself stays a native pg array.
        let out = compile_and_emit("SELECT <array<tuple<x: float64, y: float64>>>$p");
        assert!(out.sql.contains("::jsonb[]"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_contains_on_array_literal_cast_uses_array_overload_not_strpos() {
        // Regression: contains()'s array<Any> overload only matched a bare
        // IrExpr::Array node, never a TypeCast-wrapped one — which is what
        // every `<array<T>>[...]` cast actually compiles to. Overload
        // resolution silently fell back to the *first* registered `contains`
        // overload (str, str), emitting a bogus `strpos(text[], text)` call.
        let out = compile_and_emit("SELECT contains(<array<str>>[1, 2], '2')");
        assert!(out.sql.contains("@> ARRAY["), "got:\n{}", out.sql);
        assert!(!out.sql.contains("strpos"), "must not fall back to the str/str overload, got:\n{}", out.sql);
    }

    #[test]
    fn test_nested_array_type_rejected_at_parse_time() {
        match parse::parse("SELECT <array<array<str>>>$p") {
            Ok(_) => panic!("expected parse error for nested array type"),
            Err(e) => assert!(
                e.to_string().contains("nested arrays are not supported"),
                "got: {}",
                e
            ),
        }
    }

    #[test]
    fn test_array_cast_in_computed_shape_field_schema_bound_context() {
        // Schema-bound counterpart (compile_expr, not compile_free_expr) — a
        // computed shape field casting an array literal.
        let out = compile_and_emit("SELECT Person { name, tags := <array<str>>['a', 'b'] }");
        assert!(out.sql.contains("ARRAY[('a')::text, ('b')::text]"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_nominal_named_tuple_cast_shape_carries_real_members() {
        use crate::schema::{TupleMemberDescriptor, TupleMemberKind};
        let mut schema = make_schema();
        schema.named_tuples.push(NamedTupleDescriptor {
            name: "Point".into(),
            module: "default".into(),
            members: vec![
                TupleMemberDescriptor { name: Some("x".into()), kind: TupleMemberKind::Scalar { pg_type: "float8".into() } },
                TupleMemberDescriptor { name: Some("y".into()), kind: TupleMemberKind::Scalar { pg_type: "float8".into() } },
            ],
        });
        let out = compile_and_emit_with("SELECT <default::Point>$p", &schema);
        match &out.shape.root {
            crate::query::ShapeNode::NamedTuple { type_name, members, .. } => {
                assert_eq!(type_name.as_deref(), Some("default::Point"));
                let members = members.as_ref().expect("expected resolved members");
                assert_eq!(members.len(), 2);
                assert_eq!(members[0].key.as_deref(), Some("x"));
                assert_eq!(members[1].key.as_deref(), Some("y"));
            }
            other => panic!("expected ShapeNode::NamedTuple, got {other:?}"),
        }
    }

    #[test]
    fn test_structural_tuple_property_read_shape_carries_real_members() {
        use crate::schema::{TupleMemberDescriptor, TupleMemberKind};
        let schema = SchemaDescriptor {
            types: vec![TypeDescriptor {
                name: "Person".into(),
                module: "default".into(),
                table: "Person".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
                    name: "address".into(),
                    pg_type: "jsonb".into(),
                    nullable: true,
                    default_sql: None,
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                    tuple_members: Some(vec![
                        TupleMemberDescriptor {
                            name: Some("street".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                        TupleMemberDescriptor {
                            name: Some("zip".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                    ]),
                }],
                links: vec![],
                multilinks: vec![],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            }],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
        };
        let out = compile_and_emit_with("SELECT Person { address }", &schema);
        assert!(out.sql.contains("::jsonb"), "got:\n{}", out.sql);
        match &out.shape.root {
            crate::query::ShapeNode::Object { pointers, .. } => {
                let address = pointers
                    .iter()
                    .find(|p| matches!(p, crate::query::ShapeNode::NamedTuple { name, .. } if name == "address"))
                    .expect("expected address pointer in shape");
                match address {
                    crate::query::ShapeNode::NamedTuple { type_name, members, .. } => {
                        assert_eq!(*type_name, None);
                        let members = members.as_ref().expect("expected resolved members");
                        assert_eq!(members.len(), 2);
                        assert_eq!(members[0].key.as_deref(), Some("street"));
                        assert_eq!(members[1].key.as_deref(), Some("zip"));
                    }
                    other => panic!("expected NamedTuple, got {other:?}"),
                }
            }
            other => panic!("expected ShapeNode::Object, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_path_select_structural_tuple_property_shape_carries_real_members() {
        // A bare `select Type.property` path traversal (no `{ }` shape) used
        // to fall back to `ShapeNode::Scalar` for a *structural* tuple
        // property — `emit_path_select`'s IrPathResult::Scalar branch only
        // detected a *nominal* named tuple (via the "__nt__:" pg_type
        // marker), never consulting the property's own `tuple_members`. This
        // decoded every row as an opaque blob instead of a proper tuple
        // literal (reported as `select default::Person.address` rendering
        // wrong in both pylon-ui and the CLI REPL, while a `{ address }`
        // shape query — the test above — already worked).
        use crate::schema::{TupleMemberDescriptor, TupleMemberKind};
        let schema = SchemaDescriptor {
            types: vec![TypeDescriptor {
                name: "Person".into(),
                module: "default".into(),
                table: "Person".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
                    name: "address".into(),
                    pg_type: "jsonb".into(),
                    nullable: true,
                    default_sql: None,
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                    tuple_members: Some(vec![
                        TupleMemberDescriptor {
                            name: Some("street".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                        TupleMemberDescriptor {
                            name: Some("zip".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                    ]),
                }],
                links: vec![],
                multilinks: vec![],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            }],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
        };
        let out = compile_and_emit_with("SELECT Person.address", &schema);
        match &out.shape.root {
            crate::query::ShapeNode::NamedTuple { type_name, members, .. } => {
                assert_eq!(*type_name, None);
                let members = members.as_ref().expect("expected resolved members");
                assert_eq!(members.len(), 2);
                assert_eq!(members[0].key.as_deref(), Some("street"));
                assert_eq!(members[1].key.as_deref(), Some("zip"));
            }
            other => panic!("expected ShapeNode::NamedTuple, got {other:?}"),
        }
    }

    #[test]
    fn test_path_traversal_into_structural_tuple_property() {
        use crate::schema::{TupleMemberDescriptor, TupleMemberKind};
        let schema = SchemaDescriptor {
            types: vec![TypeDescriptor {
                name: "Person".into(),
                module: "default".into(),
                table: "Person".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
                    name: "address".into(),
                    pg_type: "jsonb".into(),
                    nullable: true,
                    default_sql: None,
                    default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                    tuple_members: Some(vec![
                        TupleMemberDescriptor {
                            name: Some("street".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                        TupleMemberDescriptor {
                            name: Some("zip".into()),
                            kind: TupleMemberKind::Scalar { pg_type: "text".into() },
                        },
                    ]),
                }],
                links: vec![],
                multilinks: vec![],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            }],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
        };
        // Regression: a structural (unnamed) tuple property previously failed
        // path traversal with "'address' is a scalar property, not a link —
        // cannot traverse further" because compile_path_select only allowed
        // further traversal for the nominal `__nt__:` named-tuple marker,
        // ignoring `tuple_members` on plain jsonb-typed properties.
        let out = compile_and_emit_with("SELECT default::Person.address.street", &schema);
        assert!(out.sql.contains("\"address\"->'street'"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_structural_tuple_cast_shape_carries_real_members() {
        let out = compile_and_emit("SELECT <tuple<street: str, zip: str>>$p");
        match &out.shape.root {
            crate::query::ShapeNode::NamedTuple { type_name, members, .. } => {
                assert_eq!(*type_name, None);
                let members = members.as_ref().expect("expected resolved members");
                assert_eq!(members.len(), 2);
                assert_eq!(members[0].key.as_deref(), Some("street"));
                assert_eq!(members[1].key.as_deref(), Some("zip"));
            }
            other => panic!("expected ShapeNode::NamedTuple, got {other:?}"),
        }
    }

    #[test]
    fn test_tuple_cast_mixed_named_and_unnamed_elements_rejected() {
        match parse::parse("SELECT <tuple<x: float64, bool>>$p") {
            Ok(_) => panic!("expected parse error for mixed named/unnamed tuple elements"),
            Err(e) => assert!(
                e.to_string().contains("all named or all unnamed"),
                "got: {}",
                e
            ),
        }
    }

    #[test]
    fn test_is_with_tuple_type_rejected() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER Person is tuple<x: float64, y: float64>")
            .unwrap();
        match ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected error for IS with a tuple type"),
            Err(e) => {
                assert!(
                    e.to_string().contains("cannot use IS with a tuple or array type"),
                    "got: {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_is_with_array_type_rejected() {
        let schema = make_schema();
        let ast = parse::parse("SELECT Person FILTER Person is array<str>").unwrap();
        match ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected error for IS with an array type"),
            Err(e) => {
                assert!(
                    e.to_string().contains("cannot use IS with a tuple or array type"),
                    "got: {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_tuple_index_on_non_literal_falls_back_to_jsonb_index() {
        // `.1` only constant-folds against a literal tuple; anything else
        // (a $param, a cast result, …) needs a genuine runtime jsonb index
        // instead of erroring "only supported on tuple literals".
        let out = compile_and_emit("SELECT (<tuple<int64, str>>('1', 3)).1");
        assert!(out.sql.contains("->1"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_tuple_index_out_of_bounds_on_cast_target_errors_at_compile_time() {
        // Regression: `.2` on a 2-element tuple cast silently fell back to a
        // runtime jsonb index (returning null) instead of failing, because
        // the source is a TypeCast, not a literal Tuple/NamedTuple. The cast's
        // target type is statically known here, so this must bounds-check
        // and error the same way the upstream engine does.
        let ast = parse::parse("SELECT (<tuple<int64, str>>('1', 3)).2").unwrap();
        let schema = make_schema();
        match ir::compile(&ast, &schema) {
            Ok(_) => panic!("expected out-of-bounds tuple index error"),
            Err(e) => {
                assert!(
                    e.to_string().contains("2 is not a member of tuple<std::int64, std::str>"),
                    "got: {}",
                    e
                );
            }
        }
    }

    #[test]
    fn test_positional_tuple_literal_cast_to_tuple_type_compiles() {
        // Each element must be cast to its own declared type — '1' isn't
        // silently jsonb-wrapped unchanged as a string; it's coerced to
        // int8, matching real PyQL per-element cast semantics.
        let out = compile_and_emit("SELECT <tuple<int64, str>>(1, 'x')");
        assert!(out.sql.contains("jsonb_build_array((1)::int8, ('x')::text)"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_positional_tuple_literal_cast_coerces_mismatched_literal_types() {
        // The exact case that was silently wrong: casting a string literal to
        // int64 and an int literal to str must actually coerce each one, not
        // just pass the raw literal through untouched.
        let out = compile_and_emit("SELECT <tuple<int64, str>>('1', 3)");
        assert!(out.sql.contains("jsonb_build_array(('1')::int8, (3)::text)"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_nested_tuple_literal_cast_applies_casts_recursively() {
        let out = compile_and_emit(
            "SELECT <tuple<point: tuple<x: float64, y: float64>, label: str>>(point := ('1', 2), label := 5)",
        );
        assert!(
            out.sql.contains(
                "jsonb_build_object('point', jsonb_build_object('x', ('1')::float8, 'y', (2)::float8), 'label', (5)::text)"
            ),
            "got:\n{}",
            out.sql
        );
    }

    #[test]
    fn test_positional_tuple_literal_nested_inside_named_tuple_compiles() {
        let out = compile_and_emit("SELECT (point := (1, 2), label := 'origin')");
        assert!(out.sql.contains("jsonb_build_array(1, 2)"), "got:\n{}", out.sql);
        assert!(out.sql.contains("jsonb_build_object("), "got:\n{}", out.sql);
    }

    #[test]
    fn test_positional_tuple_literal_in_schema_bound_shape_field_compiles() {
        let out = compile_and_emit("SELECT Person { name, pair := (1, 2) }");
        assert!(out.sql.contains("jsonb_build_array(1, 2)"), "got:\n{}", out.sql);
    }

    // ── search index outbox enqueue tests ─────────────────────────────────────
    //
    // Regression coverage for a real bug: `collect_search_enqueue` used to
    // filter for `SearchBackend::OpenSearch` only, so a Meilisearch-backed
    // SearchIndex never got an outbox row on insert/update/delete — the
    // Meilisearch worker (Rust or the old Python one) never received real
    // traffic from normal DML, only from a manually-inserted outbox row.

    fn make_schema_with_search_index(backend: crate::schema::SearchBackend) -> SchemaDescriptor {
        use crate::schema::{SearchIndexDescriptor, SearchPointerDescriptor, SearchWeight};
        let mut s = make_schema();
        if let Some(td) = s.types.iter_mut().find(|t| t.name == "Person") {
            td.search_indexes.push(SearchIndexDescriptor {
                index_name: None,
                backend,
                pointers: vec![SearchPointerDescriptor { name: "name".into(), weight: SearchWeight::A }],
            });
        }
        s
    }

    #[test]
    fn test_insert_enqueues_a_meilisearch_outbox_row() {
        let schema = make_schema_with_search_index(crate::schema::SearchBackend::Meilisearch);
        let out = compile_and_emit_with("INSERT Person { name := 'Alice', age := 30 }", &schema);
        assert!(
            out.sql.contains("'Meilisearch'::_pylon.\"IndexKind\""),
            "expected a Meilisearch outbox enqueue CTE, got:\n{}",
            out.sql,
        );
        assert!(out.sql.contains("INSERT INTO _pylon.\"IndexOutbox\""), "got:\n{}", out.sql);
    }

    #[test]
    fn test_insert_enqueues_an_opensearch_outbox_row() {
        let schema = make_schema_with_search_index(crate::schema::SearchBackend::OpenSearch);
        let out = compile_and_emit_with("INSERT Person { name := 'Alice', age := 30 }", &schema);
        assert!(
            out.sql.contains("'OpenSearch'::_pylon.\"IndexKind\""),
            "expected an OpenSearch outbox enqueue CTE, got:\n{}",
            out.sql,
        );
    }

    #[test]
    fn test_insert_does_not_enqueue_an_outbox_row_for_a_postgres_backed_search_index() {
        // Postgres-backed search indexes are maintained synchronously by a
        // trigger-updated tsvector column — no async worker involved.
        let schema = make_schema_with_search_index(crate::schema::SearchBackend::Postgres);
        let out = compile_and_emit_with("INSERT Person { name := 'Alice', age := 30 }", &schema);
        assert!(!out.sql.contains("_pylon.\"IndexOutbox\""), "did not expect an outbox enqueue, got:\n{}", out.sql);
    }

    #[test]
    fn test_update_enqueues_a_meilisearch_outbox_row() {
        let schema = make_schema_with_search_index(crate::schema::SearchBackend::Meilisearch);
        let out = compile_and_emit_with("UPDATE Person FILTER .name = 'Alice' SET { age := 31 }", &schema);
        assert!(
            out.sql.contains("'Meilisearch'::_pylon.\"IndexKind\""),
            "expected a Meilisearch outbox enqueue CTE, got:\n{}",
            out.sql,
        );
    }

    #[test]
    fn test_delete_enqueues_a_meilisearch_outbox_delete_job() {
        let schema = make_schema_with_search_index(crate::schema::SearchBackend::Meilisearch);
        let out = compile_and_emit_with("DELETE Person FILTER .name = 'Alice'", &schema);
        assert!(
            out.sql.contains("'Meilisearch'::_pylon.\"IndexKind\""),
            "expected a Meilisearch outbox enqueue CTE, got:\n{}",
            out.sql,
        );
        assert!(out.sql.contains("'delete'"), "expected the delete operation literal, got:\n{}", out.sql);
    }

    #[test]
    fn test_range_intrinsic_resolves_int_literals_to_int8range() {
        // Regression: std::range's ImplStrategy::TranspilerIntrinsic had no
        // actual substitution anywhere — it fell through resolve_fn_call's
        // catch-all, which emitted a literal (nonexistent) `"std"."range"(...)`
        // call instead of a real PostgreSQL range constructor.
        let out = compile_and_emit("SELECT std::overlaps(std::range(1, 3), std::range(2, 5))");
        assert!(out.sql.contains("int8range(1, 3)"), "got:\n{}", out.sql);
        assert!(out.sql.contains("int8range(2, 5)"), "got:\n{}", out.sql);
        assert!(out.sql.contains(" && "), "expected infix && for overlaps, got:\n{}", out.sql);
        assert!(!out.sql.contains("\"std\""), "must not emit a literal std schema call, got:\n{}", out.sql);
    }

    #[test]
    fn test_range_intrinsic_resolves_datetime_to_tstzrange() {
        let out = compile_and_emit(
            "SELECT std::range(<datetime>'2024-01-01T00:00:00Z', <datetime>'2024-06-01T00:00:00Z')",
        );
        assert!(out.sql.contains("tstzrange("), "got:\n{}", out.sql);
    }

    #[test]
    fn test_range_intrinsic_four_arg_form_computes_bounds_string() {
        let out = compile_and_emit("SELECT std::range(1, 3, true, false)");
        assert!(out.sql.contains("int8range(1, 3,"), "got:\n{}", out.sql);
        assert!(out.sql.contains("CASE WHEN"), "expected a dynamic bounds-string CASE, got:\n{}", out.sql);
    }

    #[test]
    fn test_multirange_intrinsic_resolves_from_range_element() {
        let out = compile_and_emit("SELECT std::multirange([std::range(1, 3), std::range(5, 7)])");
        assert!(out.sql.contains("int8multirange(VARIADIC "), "got:\n{}", out.sql);
        assert!(!out.sql.contains("\"std\""), "must not emit a literal std schema call, got:\n{}", out.sql);
    }
}

