use crate::ir::{
    IrArraySource, IrCteDef, IrDelete, IrExpr, IrFor, IrForIterator, IrFreeExpr, IrFreeSelect,
    IrFunctionSelect, IrGlobalCte, IrGroup, IrInsert, IrLinkProp, IrLiteral, IrMultiLinkField, IrMultiLinkJoin,
    IrMultiLinkMutation, IrMultiLinkValues, IrNulls, IrOutput, IrPathJoin, IrPathResult,
    IrPathSelect, IrPolyImplementor, IrScalarField, IrScalarSetField, IrSelect, IrShapeField, IrSingleLinkField,
    IrSort, IrSortDir, IrSource, IrStmt, IrUpdate, VectorEnqueueInfo,
};
use crate::parse::ast::{BinOpKind, UnaryOpKind};
use crate::query::{Cardinality, ShapeDescriptor, ShapeNode};

pub struct SqlOutput {
    pub sql: String,
    pub shape: ShapeDescriptor,
}

/// Emit the SQL body for a computed global CTE — a plain scalar query with a `value` column.
fn emit_for_global_cte(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::FreeSelect(sel) => match sel.items.first() {
            Some(IrFreeExpr::Scalar(e)) => format!("SELECT {} AS \"value\"", emit_expr(e)),
            _ => "SELECT NULL AS \"value\"".to_string(),
        },
        IrStmt::PathSelect(sel) => {
            let from_sql = emit_path_joins(&sel.root, &sel.joins);
            let scalar_expr = match &sel.result {
                IrPathResult::Scalar(e) => emit_expr(e),
                IrPathResult::Object { alias, .. } => format!("{}.\"id\"", qi(alias)),
            };
            let mut sql = format!("SELECT {} AS \"value\"\nFROM {}", scalar_expr, from_sql);
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql
        }
        IrStmt::Select(sel) => {
            let alias = &sel.source.alias;
            let mut sql = format!(
                "SELECT {}.\"id\" AS \"value\"\nFROM {} AS {}",
                qi(alias),
                source_ref(&sel.source),
                qi(alias)
            );
            append_filter(&mut sql, &sel.filter);
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
                IrStmt::Select(sel) => emit_select_stmt(sel),
                IrStmt::FreeSelect(sel) => emit_free_select(sel),
                IrStmt::PathSelect(sel) => emit_path_select(sel),
                IrStmt::Insert(ins) => emit_insert_stmt(ins),
                IrStmt::Delete(del) => emit_delete_stmt(del),
                IrStmt::Group(grp) => emit_group(grp),
                IrStmt::FunctionSelect(sel) => emit_function_select(sel),
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
    if !ir.global_ctes.is_empty() {
        let global_parts = emit_global_cte_parts(&ir.global_ctes);
        let global_str = global_parts.join(",\n     ");
        if out.sql.starts_with("WITH ") {
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

fn qn(module: &str, name: &str) -> String {
    format!("{}.{}", qi(module), qi(name))
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

fn emit_select_stmt(sel: &IrSelect) -> SqlOutput {
    let alias = &sel.source.alias;
    let (field_exprs, shape_fields) = build_shape(&sel.shape, alias);

    let type_expr = if sel.polymorphic {
        format!("{}.\"__type__\"", qi(alias))
    } else {
        type_disc(&sel.source.type_name)
    };
    let mut parts = vec![type_expr];
    parts.extend(field_exprs);
    let tuple = parts.join(",\n    ");

    let distinct = if sel.distinct { "DISTINCT " } else { "" };

    // SELECT-over-DML: wrap inner statement in a CTE, select from it.
    let from_clause = if let Some(dml) = &sel.dml_source {
        let cte_sql = emit_dml_as_cte_source(dml);
        let enqueue = match dml.as_ref() {
            IrStmt::Insert(ins) => ins.enqueue_vector.as_slice(),
            IrStmt::Update(upd) => upd.enqueue_vector.as_slice(),
            _ => &[],
        };
        let mut cte_parts = vec![format!("\"_dml\" AS (\n{}\n)", cte_sql)];
        cte_parts.extend(enqueue_ctes(enqueue, "_dml"));
        format!("WITH\n{}\nSELECT {}(\n    {}\n) AS result\nFROM \"_dml\" AS {}",
            cte_parts.join(",\n"), distinct, tuple, qi(alias))
    } else if sel.polymorphic && !sel.source.table.starts_with("@cte:") {
        // Polymorphic interface with no CTE indirection: fan out to implementor tables.
        let union_sql = emit_poly_union(&sel.poly_implementors, &sel.poly_columns);
        format!("SELECT {}(\n    {}\n) AS result\nFROM (\n{}\n) AS {}",
            distinct, tuple, union_sql, qi(alias))
    } else {
        // Concrete table or CTE (pre-filtered): query directly.
        // For CTE-backed polymorphic sources, __type__ is already present in the CTE result.
        format!("SELECT {}(\n    {}\n) AS result\nFROM {} AS {}",
            distinct, tuple, source_ref(&sel.source), qi(alias))
    };

    let mut sql = from_clause;
    append_filter(&mut sql, &sel.filter);
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    let root_fields = prepend_type(shape_fields);
    SqlOutput {
        sql,
        shape: ShapeDescriptor {
            root: ShapeNode::Object {
                name: String::new(),
                type_name: Some(sel.source.type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                fields: root_fields,
            },
        },
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
        IrStmt::Select(inner) => {
            // SELECT-over-SELECT: expose raw columns so the outer SELECT can
            // project its own shape from them, mirroring DML's RETURNING *.
            let from = if inner.polymorphic && !inner.source.table.starts_with("@cte:") {
                format!(
                    "(\n{}\n    ) AS {}",
                    emit_poly_union(&inner.poly_implementors, &inner.poly_columns),
                    qi(&inner.source.alias),
                )
            } else {
                format!("{} AS {}", source_ref(&inner.source), qi(&inner.source.alias))
            };
            let mut sql = format!("    SELECT * FROM {}", from);
            append_filter(&mut sql, &inner.filter);
            append_order_by(&mut sql, &inner.order_by);
            append_offset_limit(&mut sql, &inner.offset, &inner.limit);
            sql
        }
        IrStmt::FreeSelect(sel) => emit_free_select(sel).sql,
        IrStmt::FunctionSelect(sel) => {
            // Expose raw columns so the outer SELECT can project its own shape,
            // mirroring how IrStmt::Select works as a CTE source.
            let args_sql = sel.fn_args.iter().map(emit_expr).collect::<Vec<_>>().join(", ");
            let fn_call = format!("{}.{}({})", qi(&sel.fn_module), qi(&sel.fn_name), args_sql);
            let mut sql = format!("    SELECT * FROM {} AS {}", fn_call, qi(&sel.alias));
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql
        }
        IrStmt::For(_) | IrStmt::Group(_) => unreachable!("cannot appear as a CTE source"),
        IrStmt::PathSelect(ps) => {
            // Path select as CTE: emit a flat SELECT that exposes an `id` column.
            let mut sql = emit_path_joins(&ps.root, &ps.joins);
            append_filter(&mut sql, &ps.filter);
            sql
        }
    }
}

// ── User CTE helpers ────────────────────────────────────────────────────────

/// Emit `WITH "name" AS (...), ...` prefix (WITH keyword + trailing newline included).
fn emit_cte_prefix(ctes: &[IrCteDef]) -> String {
    let parts: Vec<String> = ctes.iter()
        .map(|c| format!("\"{}\" AS (\n{}\n)", c.name, emit_dml_as_cte_source(&c.stmt)))
        .collect();
    format!("WITH\n{}\n", parts.join(",\n"))
}

/// Emit `(SELECT id FROM ...)` sub-expression for a multi-link values source.
fn emit_multilink_values_subquery(vals: &IrMultiLinkValues) -> String {
    match vals {
        IrMultiLinkValues::CteRef(name) => {
            // Just the CTE name; will be aliased at the call site.
            format!("\"{}\"", name)
        }
        IrMultiLinkValues::Select(s) => {
            let alias = &s.source.alias;
            let mut sql = format!(
                "(SELECT {}.\"id\" FROM {} AS {}",
                qi(alias), source_ref(&s.source), qi(alias)
            );
            append_filter(&mut sql, &s.filter);
            sql.push(')');
            sql
        }
        IrMultiLinkValues::PathSelect(ps) => {
            let root_alias = &ps.root.alias;
            let mut sql = format!(
                "(SELECT {}.\"id\" FROM {} AS {}",
                qi(root_alias), source_ref(&ps.root), qi(root_alias)
            );
            for join in &ps.joins {
                sql.push_str(&emit_path_join_sql(join));
            }
            append_filter(&mut sql, &ps.filter);
            sql.push(')');
            sql
        }
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
fn emit_ml_append_cte(mutation: &IrMultiLinkMutation, idx: usize, cte_name: &str) -> String {
    let vals_ref = emit_multilink_values_subquery(&mutation.values);
    let ins = format!(
        "INSERT INTO {} ({}, {})\nSELECT \"_ids\".\"id\", \"_v\".\"id\" FROM \"_ids\" CROSS JOIN {} AS \"_v\"\nON CONFLICT DO NOTHING\nRETURNING {}, {}",
        qn(&mutation.module, &mutation.junction_table),
        qi(&mutation.source_col),
        qi(&mutation.target_col),
        vals_ref,
        qi(&mutation.source_col),
        qi(&mutation.target_col),
    );
    format!("\"{}\" AS (\n{}\n)", cte_name, ins)
}

/// Emit the CTE clause for a junction table DELETE (remove).
fn emit_ml_remove_cte(mutation: &IrMultiLinkMutation, idx: usize, cte_name: &str) -> String {
    let vals_ref = emit_multilink_values_subquery(&mutation.values);
    let del = format!(
        "DELETE FROM {}\nWHERE {} IN (SELECT \"id\" FROM \"_ids\")\n  AND {} IN (SELECT \"id\" FROM {})\nRETURNING {}, {}",
        qn(&mutation.module, &mutation.junction_table),
        qi(&mutation.source_col),
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
        || matches!(expr, IrExpr::NamedTuple(_))
}

fn emit_free_select(sel: &IrFreeSelect) -> SqlOutput {
    use crate::query::{Cardinality, ShapeNode};

    if sel.items.is_empty() {
        return SqlOutput {
            sql: "SELECT NULL AS result WHERE FALSE".to_string(),
            shape: ShapeDescriptor {
                root: ShapeNode::Scalar { name: String::new(), position: 0 },
            },
        };
    }

    // assert_exists / assert_distinct: set-returning — emit as unnest, not UNION ALL
    if sel.items.len() == 1 {
        if let IrFreeExpr::AssertSet { fn_name, inner } = &sel.items[0] {
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
            };
        }
    }

    let shape_root = free_item_shape(sel.items.first().unwrap());

    let branches: Vec<String> = sel.items.iter().map(|item| match item {
        IrFreeExpr::Scalar(expr) => {
            // Arrays (OID 1007) and jsonb (OID 3802) can't be decoded inside anonymous
            // ROW() composites by asyncpg. Return them as plain top-level columns instead.
            if is_raw_scalar(expr) {
                format!("SELECT {} AS result", emit_expr(expr))
            } else {
                // `result` is a ROW() composite for top-level asyncpg decoding.
                // `v` is the unwrapped scalar for use in CteRef expression context.
                let e = emit_expr(expr);
                format!("SELECT ROW({e}) AS result, {e} AS v")
            }
        }
        IrFreeExpr::FreeObject(fields) => {
            if fields.len() == 1 {
                format!("SELECT ROW({}) AS result", emit_expr(&fields[0].1))
            } else {
                let exprs: Vec<String> = fields.iter().map(|(_, e)| emit_expr(e)).collect();
                format!("SELECT ({}) AS result", exprs.join(", "))
            }
        }
        IrFreeExpr::Tuple(exprs) => {
            if exprs.len() == 1 {
                format!("SELECT ROW({}) AS result", emit_expr(&exprs[0]))
            } else {
                let parts: Vec<String> = exprs.iter().map(emit_expr).collect();
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

    SqlOutput { sql, shape: ShapeDescriptor { root: shape_root } }
}

fn free_item_shape(item: &IrFreeExpr) -> crate::query::ShapeNode {
    use crate::query::{Cardinality, ShapeNode};
    match item {
        IrFreeExpr::Scalar(IrExpr::TypeCast(c)) if c.pg_type == "jsonb" => ShapeNode::JsonScalar,
        IrFreeExpr::Scalar(IrExpr::NamedTuple(_)) => ShapeNode::NamedTuple {
            name: String::new(),
            position: 0,
            type_name: None,
        },
        IrFreeExpr::Scalar(e) if is_raw_scalar(e) => ShapeNode::RawScalar,
        IrFreeExpr::Scalar(_) => ShapeNode::Scalar { name: String::new(), position: 0 },
        IrFreeExpr::FreeObject(fields) => ShapeNode::Object {
            name: String::new(),
            type_name: None,
            position: 0,
            cardinality: Cardinality::Many,
            fields: fields
                .iter()
                .enumerate()
                .map(|(i, (name, _))| ShapeNode::Scalar { name: name.clone(), position: i })
                .collect(),
        },
        IrFreeExpr::Tuple(exprs) => ShapeNode::Tuple {
            position: 0,
            elements: (0..exprs.len())
                .map(|i| ShapeNode::Scalar { name: String::new(), position: i })
                .collect(),
        },
        IrFreeExpr::AssertSet { .. } => ShapeNode::Scalar { name: String::new(), position: 0 },
        IrFreeExpr::CtePassthrough(_) => ShapeNode::Scalar { name: String::new(), position: 0 },
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
            let scalar = match s.shape.first() {
                Some(IrShapeField::Scalar(sf)) =>
                    format!("{}.{}", qi(&s.source.alias), qi(&sf.column)),
                _ => format!("{}.\"id\"", qi(&s.source.alias)),
            };
            let mut sql = format!("SELECT {} FROM {} AS {}",
                scalar, source_ref(&s.source), qi(&s.source.alias));
            append_filter(&mut sql, &s.filter);
            format!("ARRAY({})", sql)
        }
        IrArraySource::PathSelect(ps) => {
            let scalar = match &ps.result {
                IrPathResult::Scalar(e) => emit_expr(e),
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

    // Build the elements ROW: type discriminator at pos 0, then shape fields.
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

    // ShapeNode for each element (Object with the selected fields).
    let element_node = ShapeNode::Object {
        name: String::new(),
        type_name: Some(grp.source.type_name.clone()),
        position: 0,
        cardinality: Cardinality::Many,
        fields: prepend_type(shape_nodes),
    };

    let root = ShapeNode::Group {
        key_nodes,
        grouping_position: grouping_pos,
        elements_position: elements_pos,
        element: Box::new(element_node),
    };

    SqlOutput { sql, shape: ShapeDescriptor { root } }
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
        IrPathResult::Scalar(ir_expr) => {
            // Named tuples / jsonb field accesses can't be decoded inside ROW() — emit raw.
            let is_nt = matches!(ir_expr, IrExpr::NamedTuple(_))
                || matches!(ir_expr, IrExpr::JsonbField { .. })
                || matches!(ir_expr, IrExpr::ColumnRef { pg_type, .. } if pg_type.starts_with("__nt__:"));
            if is_nt {
                let expr_sql = format!("{} AS result", emit_expr(ir_expr));
                let shape = if matches!(ir_expr, IrExpr::JsonbField { .. }) {
                    ShapeNode::RawScalar
                } else {
                    let type_name = match ir_expr {
                        IrExpr::ColumnRef { pg_type, .. } =>
                            pg_type.strip_prefix("__nt__:").map(|s| s.to_string()),
                        _ => None,
                    };
                    ShapeNode::NamedTuple { name: String::new(), position: 0, type_name }
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
            let (field_exprs, field_nodes) = build_shape(shape, alias);
            let mut parts = vec![type_disc(type_name)];
            parts.extend(field_exprs);
            let expr = format!("(\n    {}\n) AS result", parts.join(",\n    "));
            let shape_root = ShapeNode::Object {
                name: String::new(),
                type_name: Some(type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                fields: prepend_type(field_nodes),
            };
            (expr, shape_root)
        }
    };

    let mut sql = format!("SELECT {}{}\nFROM {}", distinct, result_expr, from_sql);
    append_filter(&mut sql, &sel.filter);
    append_order_by(&mut sql, &sel.order_by);
    append_offset_limit(&mut sql, &sel.offset, &sel.limit);

    SqlOutput { sql, shape: ShapeDescriptor { root: shape_root } }
}

// ── FOR LOOP ─────────────────────────────────────────────────────────────────

fn emit_for_stmt(f: &IrFor, user_ctes: &[IrCteDef]) -> SqlOutput {
    let IrForIterator::Values { exprs, pg_type } = &f.iterator;
    let iter_alias = format!("_for_{}", f.var_name);

    if exprs.is_empty() {
        let empty = SqlOutput {
            sql: "SELECT NULL AS result WHERE FALSE".to_string(),
            shape: ShapeDescriptor { root: ShapeNode::Scalar { name: String::new(), position: 0 } },
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
                IrStmt::Select(sel) => emit_select_stmt(sel),
                IrStmt::FreeSelect(sel) => emit_free_select(sel),
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
            SqlOutput { sql, shape: body_out.shape }
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

    let mut cte_parts: Vec<String> = user_ctes.iter().map(|cte| {
        format!("{} AS (\n{}\n)", qi(&cte.name), emit_dml_as_cte_source(&cte.stmt))
    }).collect();
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
    SqlOutput { sql, shape }
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

/// Build `SELECT (...) AS result FROM "cte_name"` plus its ShapeDescriptor,
/// mirroring `emit_returning_shape` but for the CTE-wrapper SELECT path.
fn shape_select_from_cte(
    target: &IrSource,
    returning: &[IrShapeField],
    cte_name: &str,
) -> (ShapeDescriptor, Option<String>) {
    if returning.is_empty() {
        return (
            ShapeDescriptor { root: ShapeNode::Scalar { name: String::new(), position: 0 } },
            None,
        );
    }
    let (field_exprs, shape_fields) = build_shape(returning, "");
    let mut parts = vec![type_disc(&target.type_name)];
    parts.extend(field_exprs);
    let tuple = parts.join(",\n    ");
    let sql = format!("SELECT (\n    {}\n) AS result\nFROM {}", tuple, qi(cte_name));
    let root_fields = prepend_type(shape_fields);
    let shape = ShapeDescriptor {
        root: ShapeNode::Object {
            name: String::new(),
            type_name: Some(target.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            fields: root_fields,
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

    if ins.enqueue_vector.is_empty() {
        let mut sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            source_ref(&ins.target), cols.join(", "), vals.join(", "),
        );
        if let Some(conflict) = &ins.unless_conflict { emit_conflict(&mut sql, conflict); }
        let (shape, returning_sql) = emit_returning_shape(&ins.target, &ins.returning, false);
        if let Some(r) = returning_sql { sql.push_str(&r); }
        return SqlOutput { sql, shape };
    }

    // Enqueue path: wrap INSERT in a CTE so we can append the outbox inserts.
    let mut insert_sql = format!(
        "    INSERT INTO {} ({}) VALUES ({})",
        source_ref(&ins.target), cols.join(", "), vals.join(", "),
    );
    if let Some(conflict) = &ins.unless_conflict { emit_conflict(&mut insert_sql, conflict); }
    insert_sql.push_str("\n    RETURNING \"id\"");

    let mut cte_parts = vec![format!("\"_w\" AS (\n{}\n)", insert_sql)];
    cte_parts.extend(enqueue_ctes(&ins.enqueue_vector, "_w"));

    let (shape, select_sql) = shape_select_from_cte(&ins.target, &ins.returning, "_w");
    let sql = format!(
        "WITH\n{}\n{}",
        cte_parts.join(",\n"),
        select_sql.unwrap_or_else(|| "SELECT * FROM \"_w\"".to_string()),
    );
    SqlOutput { sql, shape }
}

// ── UPDATE ──────────────────────────────────────────────────────────────────

fn emit_poly_update_stmt(upd: &IrUpdate, user_ctes: &[IrCteDef]) -> SqlOutput {
    let alias = &upd.target.alias;
    let sets: Vec<String> = upd.assignments.iter()
        .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
        .chain(upd.rewrites.iter().map(|rw| format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr))))
        .collect();

    let mut cte_parts: Vec<String> = user_ctes.iter()
        .map(|c| format!("\"{}\" AS (\n{}\n)", c.name, emit_dml_as_cte_source(&c.stmt)))
        .collect();
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
    SqlOutput { sql, shape }
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

    if !has_any_multilink && upd.enqueue_vector.is_empty() {
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
        return SqlOutput { sql, shape };
    }

    if !has_any_multilink && !upd.enqueue_vector.is_empty() {
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

        let mut cte_parts: Vec<String> = vec![];
        for cte in user_ctes {
            cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte.name, emit_dml_as_cte_source(&cte.stmt)));
        }
        cte_parts.push(format!("\"_w\" AS (\n{}\n)", upd_sql));
        cte_parts.extend(enqueue_ctes(&upd.enqueue_vector, "_w"));

        let (shape2, select_sql) = shape_select_from_cte(&upd.target, &upd.returning, "_w");
        let sql = format!(
            "WITH\n{}\n{}",
            cte_parts.join(",\n"),
            select_sql.unwrap_or_else(|| "SELECT * FROM \"_w\"".to_string()),
        );
        return SqlOutput { sql, shape: shape2 };
    }

    // CTE-based UPDATE for junction table mutations.
    let result_expr = if !upd.returning.is_empty() {
        let (field_exprs, _) = build_shape(&upd.returning, alias);
        let mut parts = vec![type_disc(&upd.target.type_name)];
        parts.extend(field_exprs);
        parts.join(",\n    ")
    } else {
        format!("{}.id", qi(alias))
    };

    let has_scalar_changes = !upd.assignments.is_empty() || !upd.rewrites.is_empty();
    let mut cte_parts: Vec<String> = vec![];

    // User CTEs first.
    for cte in user_ctes {
        cte_parts.push(format!("\"{}\" AS (\n{}\n)", cte.name, emit_dml_as_cte_source(&cte.stmt)));
    }

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
        cte_parts.push(emit_ml_append_cte(app, i, &format!("_ml_add_{}", i)));
    }

    // Junction removals (`-=`).
    for (i, rem) in upd.multi_link_removals.iter().enumerate() {
        cte_parts.push(emit_ml_remove_cte(rem, i, &format!("_ml_rm_{}", i)));
    }

    // Junction inserts for replace (`:= expr` — insert after the clear).
    for (i, rep) in upd.multi_link_replaces.iter().enumerate() {
        cte_parts.push(emit_ml_append_cte(rep, i, &format!("_ml_rep_{}", i)));
    }

    // Enqueue CTEs (source is _ids which has all columns including id).
    cte_parts.extend(enqueue_ctes(&upd.enqueue_vector, "_ids"));

    let sql = format!(
        "WITH\n{}\nSELECT (\n    {}\n) AS result\nFROM \"_ids\" AS {}",
        cte_parts.join(",\n"),
        result_expr,
        qi(alias),
    );
    SqlOutput { sql, shape }
}

// ── DELETE ──────────────────────────────────────────────────────────────────

fn emit_delete_stmt(del: &IrDelete) -> SqlOutput {
    if !del.poly_implementors.is_empty() {
        return emit_poly_delete_stmt(del);
    }
    let alias = &del.target.alias;
    let mut sql = format!(
        "DELETE FROM {} AS {}",
        source_ref(&del.target),
        qi(alias),
    );

    append_filter(&mut sql, &del.filter);

    let (shape, returning_sql) = emit_returning_shape(&del.target, &del.returning, true);
    if let Some(r) = returning_sql {
        sql.push_str(&r);
    }
    SqlOutput { sql, shape }
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
    SqlOutput { sql, shape }
}

// ── RETURNING helper ─────────────────────────────────────────────────────────

/// Builds the RETURNING clause and ShapeDescriptor for DML.
/// `with_alias`: UPDATE/DELETE can use the table alias; INSERT cannot.
fn emit_returning_shape(
    target: &IrSource,
    returning: &[IrShapeField],
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
    let (field_exprs, shape_fields) = build_shape(returning, alias);

    let mut parts = vec![type_disc(&target.type_name)];
    parts.extend(field_exprs);
    let tuple = parts.join(",\n    ");
    let sql = format!("\nRETURNING (\n    {}\n) AS result", tuple);

    let root_fields = prepend_type(shape_fields);
    let shape = ShapeDescriptor {
        root: ShapeNode::Object {
            name: String::new(),
            type_name: Some(target.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            fields: root_fields,
        },
    };
    (shape, Some(sql))
}

// ── Shape emission ───────────────────────────────────────────────────────────

fn emit_scalar_set(f: &IrScalarSetField, pos: usize) -> (String, ShapeNode) {
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

/// Build SQL expressions and ShapeNodes for `fields`, starting at position 1
/// (position 0 is always the type discriminator, added by the caller).
fn build_shape(
    fields: &[IrShapeField],
    table_alias: &str,
) -> (Vec<String>, Vec<ShapeNode>) {
    let mut exprs = Vec::new();
    let mut nodes = Vec::new();

    for (i, field) in fields.iter().enumerate() {
        let pos = i + 1;
        match field {
            IrShapeField::Scalar(f) => {
                let (sql, node) = emit_scalar(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapeField::SingleLink(f) => {
                let (sql, node) = emit_single_link(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapeField::MultiLink(f) => {
                let (sql, node) = emit_multi_link(f, table_alias, pos);
                exprs.push(sql);
                nodes.push(node);
            }
            IrShapeField::Computed(f) => {
                exprs.push(emit_expr(&f.expr));
                nodes.push(ShapeNode::Scalar { name: f.alias.clone(), position: pos });
            }
            IrShapeField::ScalarSet(f) => {
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

fn emit_scalar(f: &IrScalarField, table_alias: &str, pos: usize) -> (String, ShapeNode) {
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
    let sql = if table_alias.is_empty() {
        format!("{}::{}", qi(&f.column), f.pg_type)
    } else {
        format!("{}.{}::{}", qi(table_alias), qi(&f.column), f.pg_type)
    };
    (sql, ShapeNode::Scalar { name: f.alias.clone(), position: pos })
}

fn emit_single_link(
    f: &IrSingleLinkField,
    parent_alias: &str,
    pos: usize,
) -> (String, ShapeNode) {
    let sub = &f.subquery;
    let sub_alias = &sub.source.alias;

    let (sub_exprs, sub_nodes) = build_shape(&sub.shape, sub_alias);
    let mut parts = vec![type_disc(&sub.source.type_name)];
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
        source_ref(&sub.source),
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
        type_name: Some(sub.source.type_name.clone()),
        position: pos,
        cardinality: Cardinality::Optional,
        fields: prepend_type(sub_nodes),
    };
    (sql, node)
}

fn emit_multi_link(
    f: &IrMultiLinkField,
    parent_alias: &str,
    pos: usize,
) -> (String, ShapeNode) {
    let sub = &f.subquery;
    let sub_alias = &sub.source.alias;

    let (sub_exprs, mut sub_nodes) = build_shape(&sub.shape, sub_alias);
    let mut row_parts = vec![type_disc(&sub.source.type_name)];
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
                source_ref(&sub.source),
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
                source_ref(&sub.source),
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
            type_name: Some(sub.source.type_name.clone()),
            position: 0,
            cardinality: Cardinality::Required,
            fields: prepend_type(sub_nodes),
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
                Some(s) => format!("{}.{}", qi(s), qi(&f.name)),
                None => f.name.clone(),
            };
            format!("{}({})", name, args.join(", "))
        }
        IrExpr::TypeCast(c) => {
            // PostgreSQL doesn't support arbitrary_type::jsonb; to_jsonb() accepts any input.
            // String literals have type "unknown" in PG, so cast to text first.
            if c.pg_type == "jsonb" {
                let inner = match &c.expr {
                    IrExpr::Literal(IrLiteral::Str(_)) => {
                        format!("{}::text", emit_expr(&c.expr))
                    }
                    _ => emit_expr(&c.expr),
                };
                format!("to_jsonb({})", inner)
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
        IrExpr::ArrayFromSelect(src) => emit_array_source(src),

        IrExpr::CteRef { name, scalar } => {
            // scalar CTEs emit `ROW(expr) AS result, expr AS v`; use `v` for
            // expression context so we get the plain scalar type, not record.
            let col = if *scalar { "v" } else { "id" };
            format!("(SELECT \"{}\" FROM \"{}\")", col, name)
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

        IrExpr::NamedTuple(fields) => {
            let pairs: Vec<String> = fields.iter()
                .flat_map(|(k, v)| [format!("'{}'", k.replace('\'', "''")), emit_expr(v)])
                .collect();
            format!("jsonb_build_object({})", pairs.join(", "))
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

        IrExpr::FnParam { name, .. } => qi(name),

        IrExpr::PathSubquery(ps) => {
            let scalar = match &ps.result {
                IrPathResult::Scalar(e) => emit_expr(e),
                IrPathResult::Object { alias, .. } => format!("{}.\"id\"", qi(alias)),
            };
            let from_sql = emit_path_joins(&ps.root, &ps.joins);
            let mut sql = format!("(SELECT {}\nFROM {}", scalar, from_sql);
            append_filter(&mut sql, &ps.filter);
            sql.push(')');
            sql
        }

        IrExpr::Subquery(sel) => {
            let alias = &sel.source.alias;
            let mut sql = if sel.shape.is_empty() {
                // EXISTS inner: SELECT 1 FROM …
                format!("(SELECT 1\nFROM {} AS {}", source_ref(&sel.source), qi(alias))
            } else {
                // Scalar subquery: SELECT alias.col FROM …
                let pk_col = sel
                    .shape
                    .iter()
                    .find_map(|f| if let IrShapeField::Scalar(s) = f { Some(s.column.as_str()) } else { None })
                    .unwrap_or("id");
                format!(
                    "(SELECT {}.{}\nFROM {} AS {}",
                    qi(alias), qi(pk_col), source_ref(&sel.source), qi(alias),
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

// ── Function select ──────────────────────────────────────────────────────────

fn emit_function_select(sel: &IrFunctionSelect) -> SqlOutput {
    let alias = &sel.alias;
    let (field_exprs, shape_fields) = build_shape(&sel.shape, alias);

    let type_expr = if sel.polymorphic {
        format!("{}.\"__type__\"", qi(alias))
    } else {
        type_disc(&sel.type_name)
    };
    let mut parts = vec![type_expr];
    parts.extend(field_exprs);
    let tuple = parts.join(",\n    ");
    let distinct = if sel.distinct { "DISTINCT " } else { "" };

    let args_sql = sel.fn_args.iter().map(emit_expr).collect::<Vec<_>>().join(", ");
    let fn_call = format!("{}.{}({})", qi(&sel.fn_module), qi(&sel.fn_name), args_sql);

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

    let root_fields = prepend_type(shape_fields);
    SqlOutput {
        sql,
        shape: ShapeDescriptor {
            root: ShapeNode::Object {
                name: String::new(),
                type_name: Some(sel.type_name.clone()),
                position: 0,
                cardinality: Cardinality::Many,
                fields: root_fields,
            },
        },
    }
}

/// Emit the SQL body expression for a user-defined function DDL.
///
/// For scalar functions (`FreeSelect`) emits just the expression — e.g. `"a" + "b"`.
/// For object functions (`Select`, `FunctionSelect`) emits a full `SELECT … FROM …`.
pub fn emit_fn_body(stmt: &IrStmt) -> String {
    match stmt {
        IrStmt::FreeSelect(sel) => {
            if let Some(IrFreeExpr::Scalar(e)) = sel.items.first() {
                // Wrap in a complete SELECT statement for PostgreSQL LANGUAGE SQL body.
                format!("SELECT {}", emit_expr(e))
            } else {
                emit_free_select(sel).sql
            }
        }
        other => emit_dml_as_cte_source(other),
    }
}

fn emit_literal(lit: &IrLiteral) -> String {
    match lit {
        IrLiteral::Str(s) => sql_str(s),
        IrLiteral::Int(i) => i.to_string(),
        IrLiteral::Float(f) => {
            let s = f.to_string();
            if s.contains('.') || s.contains('e') { s } else { format!("{}.0", s) }
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
        FunctionDescriptor, FunctionParamDescriptor, LinkDescriptor, MultiLinkDescriptor,
        PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
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
                    vector_indexes: vec![],
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
                    vector_indexes: vec![],
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
                    vector_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                },
            ],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
            functions: vec![],
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
        assert!(out.sql.contains("ROW(1)"));
        assert!(out.sql.contains("ROW(2)"));
        assert!(out.sql.contains("ROW(3)"));
        assert!(out.sql.contains("AS result"));
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
        // Shape should describe an object with fields foo and n
        let crate::query::ShapeNode::Object { fields, type_name, .. } = &out.shape.root
            else { panic!("expected Object shape") };
        assert!(type_name.is_none());
        assert_eq!(fields.len(), 2);
        assert!(matches!(&fields[0], crate::query::ShapeNode::Scalar { name, position: 0 } if name == "foo"));
        assert!(matches!(&fields[1], crate::query::ShapeNode::Scalar { name, position: 1 } if name == "n"));
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
        assert!(out.sql.contains("ROW('hello')"));
        assert!(out.sql.contains("AS result"));
        assert!(matches!(out.shape.root, crate::query::ShapeNode::Scalar { .. }));
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
        assert!(out.sql.contains("FROM \"default\".\"Person\""));
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
        assert!(out.sql.contains("FROM \"default\".\"Person\""));
        assert!(out.sql.contains("WHERE"));
        assert!(out.sql.contains("'019ef1bb-0d42-7a9f-8f6b-b38d028a49ba'"));
    }

    #[test]
    fn test_select_single_link() {
        let out = compile_and_emit("SELECT Person { name, company { name } }");
        assert!(out.sql.contains("'default::Company'::text"));
        assert!(out.sql.contains("FROM \"default\".\"Company\""));
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
            check_constraints: vec![], is_exclusive: true, is_pk: true,
            is_readonly: true, rewrites: vec![],
        };
        let name_prop = || PropertyDescriptor {
            name: "name".into(), pg_type: "text".into(), nullable: false,
            default_sql: None, description: None, check_constraints: vec![],
            is_exclusive: false, is_pk: false, is_readonly: false, rewrites: vec![],
        };
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
                        nullable: false, description: None, on_delete: vec![],
                    }],
                    computed: vec![], constraints: vec![], indexes: vec![], vector_indexes: vec![], triggers: vec![], junction: false,
                },
                TypeDescriptor {
                    name: "PersonFriend".into(), module: "default".into(), table: "PersonFriend".into(),
                    abstract_: false, materialized: false, description: None,
                    parents: vec![], interfaces: vec![],
                    properties: vec![id_prop()],
                    links: vec![
                        LinkDescriptor {
                            name: "person".into(), target: "default::Person".into(),
                            nullable: false, description: None, is_exclusive: false,
                            is_readonly: false, rewrites: vec![], on_delete: vec![],
                        },
                        LinkDescriptor {
                            name: "friend".into(), target: "default::Person".into(),
                            nullable: false, description: None, is_exclusive: false,
                            is_readonly: false, rewrites: vec![], on_delete: vec![],
                        },
                    ],
                    multilinks: vec![], computed: vec![], constraints: vec![],
                    indexes: vec![], vector_indexes: vec![], triggers: vec![], junction: false,
                },
            ],
            scalars: vec![], enums: vec![], globals: vec![], functions: vec![],
        }
    }

    #[test]
    fn test_select_through_multi_link() {
        let schema = make_schema_with_through();
        let ast = crate::parse::parse("SELECT Person { name, friends { name } }").unwrap();
        let ir = crate::ir::compile(&ast, &schema).unwrap();
        let out = emit(&ir);
        // Junction table is the PersonFriend table, not the standard dotted name
        assert!(out.sql.contains("\"default\".\"PersonFriend\""));
        // Source FK column (person → Person) and target FK column (friend → Person)
        assert!(out.sql.contains("\"friend\""));
        assert!(out.sql.contains("\"person\""));
        // Still emits array_agg pattern
        assert!(out.sql.contains("array_agg(ROW("));
    }

    #[test]
    fn test_shape_descriptor_scalars() {
        let out = compile_and_emit("SELECT Person { name, age }");
        let ShapeNode::Object { fields, .. } = &out.shape.root else { panic!() };
        assert_eq!(fields.len(), 3); // __type__, name, age
        assert!(matches!(&fields[0], ShapeNode::Scalar { name, position: 0 } if name == "__type__"));
        assert!(matches!(&fields[1], ShapeNode::Scalar { name, position: 1 } if name == "name"));
        assert!(matches!(&fields[2], ShapeNode::Scalar { name, position: 2 } if name == "age"));
    }

    #[test]
    fn test_shape_descriptor_multi_link() {
        let out = compile_and_emit("SELECT Person { name, posts { title } }");
        let ShapeNode::Object { fields, .. } = &out.shape.root else { panic!() };
        // fields: [__type__, name, posts]
        assert_eq!(fields.len(), 3);
        let ShapeNode::Array { name, position, element } = &fields[2] else { panic!() };
        assert_eq!(name, "posts");
        assert_eq!(*position, 2);
        let ShapeNode::Object { fields: elem_fields, .. } = element.as_ref() else { panic!() };
        // element fields: [__type__, title]
        assert_eq!(elem_fields.len(), 2);
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
        assert!(out.sql.contains("INSERT INTO \"default\".\"Person\""));
        assert!(out.sql.contains("RETURNING"));
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(out.sql.contains(") AS result"));
        // Bare INSERT returns pk only (Gel behaviour)
        let ShapeNode::Object { cardinality, fields, .. } = &out.shape.root else { panic!() };
        assert_eq!(*cardinality, Cardinality::Required);
        // Only __type__ and id — not name or age
        assert!(fields.iter().any(|f| matches!(f, ShapeNode::Scalar { name, .. } if name == "id")));
        assert!(!fields.iter().any(|f| matches!(f, ShapeNode::Scalar { name, .. } if name == "name")));
    }

    #[test]
    fn test_update_returning() {
        let out = compile_and_emit("UPDATE Person FILTER .name = $name SET { age := 31 }");
        assert!(out.sql.contains("UPDATE \"default\".\"Person\""));
        assert!(out.sql.contains("SET"));
        // Bare UPDATE returns pk only
        assert!(out.sql.contains("RETURNING"));
        assert!(out.sql.contains("'default::Person'::text"));
        assert!(!out.sql.contains("\"name\"::text"), "bare UPDATE must not return name");
    }

    #[test]
    fn test_delete_returning() {
        let out = compile_and_emit("DELETE Person FILTER .id = $id");
        assert!(out.sql.contains("DELETE FROM \"default\".\"Person\""));
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
        assert!(out.sql.contains("WITH \"_dml\" AS ("));
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
        assert!(out.sql.contains("WITH \"_dml\" AS ("));
        assert!(out.sql.contains("UPDATE"));
        assert!(out.sql.contains("RETURNING *"));
        assert!(out.sql.contains("\"name\"::text"));
    }

    #[test]
    fn test_select_over_delete() {
        let out = compile_and_emit(
            "SELECT (DELETE Person FILTER .id = $id) { id, name }",
        );
        assert!(out.sql.contains("WITH \"_dml\" AS ("));
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
            description: None,
            check_constraints: vec![],
            is_exclusive: false,
            is_pk: false,
            is_readonly: false,
            rewrites: vec![
                RewriteEntry { on: 1, handler: "str_lower(.name)".into() },  // INSERT
                RewriteEntry { on: 2, handler: "str_lower(.name)".into() },  // UPDATE
            ],
        });
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
        assert!(out.sql.contains("WITH \"_dml\" AS ("));
        // CTE exposes raw columns via SELECT *
        assert!(out.sql.contains("SELECT *"));
        assert!(out.sql.contains("FROM \"default\".\"Person\""));
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
        assert!(out.sql.contains("WITH \"_dml\" AS ("));
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
        assert!(out.sql.contains("FROM \"default\".\"Company\""));
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
        assert!(out.sql.contains("FROM \"default\".\"Company\""));
    }

    #[test]
    fn test_computed_field_in_shape_emits_expression() {
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
        assert!(out.sql.contains("\"default\".\"mysum\"(1, 2)"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_user_fn_object_select_no_shape() {
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with("SELECT adults()", &schema);
        assert!(out.sql.contains("\"default\".\"adults\"()"), "got:\n{}", out.sql);
        assert!(out.sql.contains("FROM"), "got:\n{}", out.sql);
    }

    #[test]
    fn test_user_fn_object_select_with_shape() {
        let schema = make_schema_with_fns();
        let out = compile_and_emit_with("SELECT adults() { name }", &schema);
        assert!(out.sql.contains("\"default\".\"adults\"()"), "got:\n{}", out.sql);
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
            out.sql.contains("SELECT * FROM \"default\".\"adults\"()"),
            "CTE source must be SELECT * FROM fn(), got:\n{}",
            out.sql,
        );
        assert!(out.sql.contains("\"age\""), "outer filter must reference raw column, got:\n{}", out.sql);
    }
}
