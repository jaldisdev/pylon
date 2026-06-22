use crate::ir::{
    IrDelete, IrExpr, IrInsert, IrLiteral, IrMultiLinkField, IrMultiLinkJoin, IrNulls, IrOutput,
    IrScalarField, IrSelect, IrShapeField, IrSingleLinkField, IrSort, IrSortDir, IrSource,
    IrStmt, IrUpdate,
};
use crate::parse::ast::{BinOpKind, UnaryOpKind};
use crate::query::{Cardinality, ShapeDescriptor, ShapeNode};

pub struct SqlOutput {
    pub sql: String,
    pub shape: ShapeDescriptor,
}

pub fn emit(ir: &IrOutput) -> SqlOutput {
    match &ir.stmt {
        IrStmt::Select(sel) => emit_select_stmt(sel),
        IrStmt::Insert(ins) => emit_insert_stmt(ins),
        IrStmt::Update(upd) => emit_update_stmt(upd),
        IrStmt::Delete(del) => emit_delete_stmt(del),
    }
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
    qn(module_of(&src.type_name), &src.table)
}

// ── SELECT statement ────────────────────────────────────────────────────────

fn emit_select_stmt(sel: &IrSelect) -> SqlOutput {
    let alias = &sel.source.alias;
    let (field_exprs, shape_fields) = build_shape(&sel.shape, alias);

    let mut parts = vec![type_disc(&sel.source.type_name)];
    parts.extend(field_exprs);
    let tuple = parts.join(",\n    ");

    // SELECT-over-DML: wrap inner statement in a CTE, select from it.
    let from_clause = if let Some(dml) = &sel.dml_source {
        let cte_sql = emit_dml_as_cte_source(dml);
        format!("WITH \"_dml\" AS (\n{}\n)\nSELECT (\n    {}\n) AS result\nFROM \"_dml\" AS {}",
            cte_sql, tuple, qi(alias))
    } else {
        format!("SELECT (\n    {}\n) AS result\nFROM {} AS {}",
            tuple, source_ref(&sel.source), qi(alias))
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
                match (&conflict.on, &conflict.else_) {
                    (None, None) => sql.push_str(" ON CONFLICT DO NOTHING"),
                    (Some(on_expr), None) => sql.push_str(
                        &format!(" ON CONFLICT ({}) DO NOTHING", emit_expr(on_expr))),
                    _ => {}
                }
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
            let mut sql = format!(
                "    SELECT * FROM {} AS {}",
                source_ref(&inner.source),
                qi(&inner.source.alias),
            );
            append_filter(&mut sql, &inner.filter);
            append_order_by(&mut sql, &inner.order_by);
            append_offset_limit(&mut sql, &inner.offset, &inner.limit);
            sql
        }
    }
}

// ── INSERT ──────────────────────────────────────────────────────────────────

fn emit_insert_stmt(ins: &IrInsert) -> SqlOutput {
    // Rewrites override explicit assignments for the same column.
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

    let mut sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        source_ref(&ins.target),
        cols.join(", "),
        vals.join(", "),
    );

    if let Some(conflict) = &ins.unless_conflict {
        match (&conflict.on, &conflict.else_) {
            (None, None) => sql.push_str(" ON CONFLICT DO NOTHING"),
            (Some(on_expr), None) => {
                sql.push_str(&format!(" ON CONFLICT ({}) DO NOTHING", emit_expr(on_expr)))
            }
            _ => {} // ON CONFLICT ... DO UPDATE handled in a later phase
        }
    }

    let (shape, returning_sql) = emit_returning_shape(&ins.target, &ins.returning, false);
    if let Some(r) = returning_sql {
        sql.push_str(&r);
    }
    SqlOutput { sql, shape }
}

// ── UPDATE ──────────────────────────────────────────────────────────────────

fn emit_update_stmt(upd: &IrUpdate) -> SqlOutput {
    let alias = &upd.target.alias;
    let mut sets: Vec<String> = upd
        .assignments
        .iter()
        .map(|(col, expr)| format!("{} = {}", qi(col), emit_expr(expr)))
        .collect();
    for rw in &upd.rewrites {
        sets.push(format!("{} = {}", qi(&rw.column), emit_expr(&rw.expr)));
    }

    let mut sql = format!(
        "UPDATE {} AS {}\nSET {}",
        source_ref(&upd.target),
        qi(alias),
        sets.join(", "),
    );

    append_filter(&mut sql, &upd.filter);

    let (shape, returning_sql) = emit_returning_shape(&upd.target, &upd.returning, true);
    if let Some(r) = returning_sql {
        sql.push_str(&r);
    }
    SqlOutput { sql, shape }
}

// ── DELETE ──────────────────────────────────────────────────────────────────

fn emit_delete_stmt(del: &IrDelete) -> SqlOutput {
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
        }
    }

    (exprs, nodes)
}

fn emit_scalar(f: &IrScalarField, table_alias: &str, pos: usize) -> (String, ShapeNode) {
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

    let (sub_exprs, sub_nodes) = build_shape(&sub.shape, sub_alias);
    let mut row_parts = vec![type_disc(&sub.source.type_name)];
    row_parts.extend(sub_exprs);
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
        IrMultiLinkJoin::Through { .. } => {
            todo!("Through link join not yet supported in SQL emitter")
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
                BinOpKind::FloorDiv => format!("floor(({}) / ({}))", l, r),
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
            let name = match &f.schema {
                Some(s) => format!("{}.{}", qi(s), qi(&f.name)),
                None => f.name.clone(),
            };
            format!("{}({})", name, args.join(", "))
        }
        IrExpr::TypeCast(c) => format!("({})::{}", emit_expr(&c.expr), c.pg_type),
        IrExpr::IfElse(ie) => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            emit_expr(&ie.condition),
            emit_expr(&ie.if_),
            emit_expr(&ie.else_),
        ),
        IrExpr::Subquery(sel) => {
            // Scalar subquery for link FK assignments: returns the target pk column.
            // Emitted as `(SELECT "alias"."id" FROM … WHERE …)`.
            let alias = &sel.source.alias;
            let pk_col = sel
                .shape
                .iter()
                .find_map(|f| if let IrShapeField::Scalar(s) = f { Some(s.column.as_str()) } else { None })
                .unwrap_or("id");
            let mut sql = format!(
                "(SELECT {}.{}\nFROM {} AS {}",
                qi(alias),
                qi(pk_col),
                source_ref(&sel.source),
                qi(alias),
            );
            append_filter(&mut sql, &sel.filter);
            append_order_by(&mut sql, &sel.order_by);
            append_offset_limit(&mut sql, &sel.offset, &sel.limit);
            sql.push(')');
            sql
        }
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
        LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor,
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
                    triggers: vec![],
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
                    triggers: vec![],
                },
            ],
            scalars: vec![],
            enums: vec![],
            globals: vec![],
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
    fn test_select_single_link() {
        let out = compile_and_emit("SELECT Person { name, company { name } }");
        assert!(out.sql.contains("'default::Company'::text"));
        assert!(out.sql.contains("FROM \"default\".\"Company\""));
        // join condition: parent FK = target PK
        assert!(out.sql.contains("\"company\" = "));
    }

    #[test]
    fn test_select_multi_link() {
        let out = compile_and_emit("SELECT Person { name, posts { title } }");
        assert!(out.sql.contains("array_agg(ROW("));
        assert!(out.sql.contains("ARRAY[]::record[]"));
        assert!(out.sql.contains("'default::Post'::text"));
        assert!(out.sql.contains("\"Person.posts\""));
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
        assert!(out.sql.contains("str_lower("));
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
        assert!(out.sql.contains("str_lower("), "rewrite expression must be present");
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
        assert!(out.sql.contains("str_lower("));
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
        // The company column should be assigned via a scalar subquery
        assert!(out.sql.contains("\"company\""));
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
        assert!(out.sql.contains("\"company\""));
        assert!(out.sql.contains("SELECT"));
        assert!(out.sql.contains("FROM \"default\".\"Company\""));
    }
}
