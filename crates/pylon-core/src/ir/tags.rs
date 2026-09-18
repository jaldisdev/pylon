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

//! Cache-invalidation tag extraction: walks a compiled `IrOutput` and
//! collects every schema-qualified Postgres table a statement reads from or
//! writes to — including joins, nested subqueries, polymorphic fan-out, and
//! multi-link junction tables. Used both to *tag* a SELECT's cache entry
//! (any write to one of these tables must invalidate it) and to compute
//! which tags an INSERT/UPDATE/DELETE must invalidate — the caller already
//! knows which of the two it's doing from the compiled statement kind, so
//! one collector serves both purposes.
//!
//! No generic visitor/fold abstraction exists in this crate (confirmed: the
//! closest precedent, `substitute_col_refs`, is itself a hand-written
//! recursive match over every `IrExpr` variant) — this follows that same
//! style rather than introducing one just for this.

use super::*;

/// Collect every distinct schema-qualified table name (`"schema.table"`,
/// matching the exact unquoted form a `TG_TABLE_SCHEMA || '.' ||
/// TG_TABLE_NAME` trigger payload produces) touched anywhere in `output` —
/// the main statement, every WITH-block CTE, and every computed global CTE.
pub fn collect_tags(output: &IrOutput) -> Vec<String> {
    let mut tags = Vec::new();
    collect_stmt(&output.stmt, &mut tags);
    for cte in &output.ctes {
        collect_stmt(&cte.stmt, &mut tags);
    }
    for global in &output.global_ctes {
        if let IrGlobalCte::Computed(c) = global {
            collect_stmt(&c.stmt, &mut tags);
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

fn pg_schema(module: &str) -> &str {
    if module == "default" { "public" } else { module }
}

fn qualify(module: &str, table: &str) -> String {
    format!("{}.{}", pg_schema(module), table)
}

fn tag_for(source: &IrSource) -> String {
    let module = source.type_name.split("::").next().unwrap_or("");
    qualify(module, &source.table)
}

fn tag_for_implementor(imp: &IrPolyImplementor) -> String {
    qualify(&imp.module, &imp.table)
}

fn collect_stmt(stmt: &IrStmt, tags: &mut Vec<String>) {
    match stmt {
        IrStmt::Select(sel) => collect_select(sel, tags),
        IrStmt::PathSelect(ps) => collect_path_select(ps, tags),
        IrStmt::Insert(ins) => {
            tags.push(tag_for(&ins.target));
            for (_, e) in &ins.assignments {
                collect_expr(e, tags);
            }
            if let Some(c) = &ins.unless_conflict {
                if let Some(e) = &c.on {
                    collect_expr(e, tags);
                }
                if let Some(upd) = &c.do_update {
                    for (_, e) in upd {
                        collect_expr(e, tags);
                    }
                }
            }
            for r in &ins.rewrites {
                collect_expr(&r.expr, tags);
            }
            for p in &ins.returning {
                collect_shape_pointer(p, tags);
            }
            for m in &ins.multi_link_appends {
                tags.push(qualify(&m.module, &m.junction_table));
                collect_multi_link_values(&m.values, tags);
            }
        }
        IrStmt::Update(upd) => {
            tags.push(tag_for(&upd.target));
            for imp in &upd.poly_implementors {
                tags.push(tag_for_implementor(imp));
            }
            if let Some(f) = &upd.filter {
                collect_expr(f, tags);
            }
            for (_, e) in &upd.assignments {
                collect_expr(e, tags);
            }
            for r in &upd.rewrites {
                collect_expr(&r.expr, tags);
            }
            for p in &upd.returning {
                collect_shape_pointer(p, tags);
            }
            for c in &upd.multi_link_clears {
                tags.push(qualify(&c.module, &c.junction_table));
            }
            for m in upd
                .multi_link_replaces
                .iter()
                .chain(&upd.multi_link_appends)
                .chain(&upd.multi_link_removals)
            {
                tags.push(qualify(&m.module, &m.junction_table));
                collect_multi_link_values(&m.values, tags);
            }
        }
        IrStmt::Delete(del) => {
            tags.push(tag_for(&del.target));
            for imp in &del.poly_implementors {
                tags.push(tag_for_implementor(imp));
            }
            if let Some(f) = &del.filter {
                collect_expr(f, tags);
            }
            for p in &del.returning {
                collect_shape_pointer(p, tags);
            }
        }
        IrStmt::For(f) => {
            match &f.iterator {
                IrForIterator::Values { exprs, .. } => {
                    for e in exprs {
                        collect_expr(e, tags);
                    }
                }
                IrForIterator::Query { stmt, .. } => collect_stmt(stmt, tags),
            }
            collect_stmt(&f.body, tags);
        }
        IrStmt::Group(g) => {
            tags.push(tag_for(&g.source));
            for p in &g.shape {
                collect_shape_pointer(p, tags);
            }
            for (_, e) in &g.keys {
                collect_expr(e, tags);
            }
        }
        IrStmt::FunctionSelect(fs) => collect_function_select(fs, tags),
        IrStmt::VectorSearch(vs) => {
            tags.push(tag_for(&vs.source));
            collect_expr(&vs.query_expr, tags);
            for p in &vs.object_shape {
                collect_shape_pointer(p, tags);
            }
            if let Some(f) = &vs.filter {
                collect_expr(f, tags);
            }
            if let Some(e) = &vs.offset {
                collect_expr(e, tags);
            }
            if let Some(e) = &vs.limit {
                collect_expr(e, tags);
            }
        }
        IrStmt::FtsSearch(fs) => {
            tags.push(tag_for(&fs.source));
            collect_expr(&fs.query_expr, tags);
            for p in &fs.object_shape {
                collect_shape_pointer(p, tags);
            }
            if let Some(f) = &fs.filter {
                collect_expr(f, tags);
            }
            if let Some(e) = &fs.offset {
                collect_expr(e, tags);
            }
            if let Some(e) = &fs.limit {
                collect_expr(e, tags);
            }
        }
    }
}

fn collect_select(sel: &IrSelect, tags: &mut Vec<String>) {
    for row in &sel.rows {
        match row {
            IrRowSource::Bound { source, shape } => {
                tags.push(tag_for(source));
                for p in shape {
                    collect_shape_pointer(p, tags);
                }
            }
            IrRowSource::Free(item) => collect_free_expr(item, tags),
        }
    }
    for imp in &sel.poly_implementors {
        tags.push(tag_for_implementor(imp));
    }
    if let Some(f) = &sel.filter {
        collect_expr(f, tags);
    }
    for s in &sel.order_by {
        collect_expr(&s.expr, tags);
    }
    if let Some(e) = &sel.offset {
        collect_expr(e, tags);
    }
    if let Some(e) = &sel.limit {
        collect_expr(e, tags);
    }
    if let Some(dml) = &sel.dml_source {
        collect_stmt(dml, tags);
    }
}

fn collect_path_select(ps: &IrPathSelect, tags: &mut Vec<String>) {
    tags.push(tag_for(&ps.root));
    for imp in &ps.poly_implementors {
        tags.push(tag_for_implementor(imp));
    }
    for j in &ps.joins {
        match j {
            IrPathJoin::Single { target, .. } | IrPathJoin::BacklinkSingle { target, .. } => tags.push(tag_for(target)),
            // A junction is involved — a write to it (e.g. re-linking a
            // junction-backed single link, or appending/removing a
            // multi-link target) must also invalidate this query's cache
            // entry, not just a write to the target's own table.
            IrPathJoin::Multi { join, target, .. } => {
                tags.push(tag_for(target));
                tag_junction(join, tags);
            }
            IrPathJoin::BacklinkMulti {
                junction_table,
                module,
                target,
                ..
            } => {
                tags.push(tag_for(target));
                tags.push(qualify(module, junction_table));
            }
        }
    }
    match &ps.result {
        IrPathResult::Scalar(e, _) => collect_expr(e, tags),
        IrPathResult::Object { shape, .. } => {
            for p in shape {
                collect_shape_pointer(p, tags);
            }
        }
    }
    if let Some(f) = &ps.filter {
        collect_expr(f, tags);
    }
    for s in &ps.order_by {
        collect_expr(&s.expr, tags);
    }
    if let Some(e) = &ps.offset {
        collect_expr(e, tags);
    }
    if let Some(e) = &ps.limit {
        collect_expr(e, tags);
    }
}

/// Push a junction table's own tag, if `join` involves one — shared by a
/// multi-link's `IrShapePointer`/`IrPathJoin` and a junction-backed single
/// link's `IrSingleLinkCorrelation::Junction` (D1: same join shape either
/// way, so the same tag-collection applies).
fn tag_junction(join: &IrMultiLinkJoin, tags: &mut Vec<String>) {
    match join {
        IrMultiLinkJoin::Standard { junction_table, module }
        | IrMultiLinkJoin::Through {
            junction_table, module, ..
        }
        | IrMultiLinkJoin::BacklinkJunction {
            junction_table, module, ..
        } => {
            tags.push(qualify(module, junction_table));
        }
        // No junction table involved — the owner table's own tag comes from
        // the caller's own `collect_select`/`tag_for` instead.
        IrMultiLinkJoin::BacklinkFk { .. } => {}
    }
}

fn collect_shape_pointer(p: &IrShapePointer, tags: &mut Vec<String>) {
    match p {
        IrShapePointer::Scalar(_) => {}
        IrShapePointer::SingleLink(sl) => {
            collect_select(&sl.subquery, tags);
            if let IrSingleLinkCorrelation::Junction { join, .. } = &sl.correlation {
                tag_junction(join, tags);
            }
        }
        IrShapePointer::MultiLink(ml) => {
            collect_select(&ml.subquery, tags);
            tag_junction(&ml.join, tags);
        }
        IrShapePointer::Computed(c) => collect_expr(&c.expr, tags),
        IrShapePointer::ScalarSet(ss) => {
            tags.push(tag_for(&ss.source));
            for imp in &ss.poly_implementors {
                tags.push(tag_for_implementor(imp));
            }
            collect_expr(&ss.bool_expr, tags);
        }
    }
}

fn collect_free_expr(item: &IrFreeExpr, tags: &mut Vec<String>) {
    match item {
        IrFreeExpr::Scalar(e) => collect_expr(e, tags),
        IrFreeExpr::FreeObject(fields) => {
            for (_, e) in fields {
                collect_expr(e, tags);
            }
        }
        IrFreeExpr::Tuple(exprs) => {
            for e in exprs {
                collect_expr(e, tags);
            }
        }
        IrFreeExpr::AssertSet { inner, .. } => collect_array_source(inner, tags),
        IrFreeExpr::CtePassthrough(_) => {}
    }
}

fn collect_array_source(src: &IrArraySource, tags: &mut Vec<String>) {
    match src {
        IrArraySource::Select(sel) => collect_select(sel, tags),
        IrArraySource::PathSelect(ps) => collect_path_select(ps, tags),
        IrArraySource::RawExpr {
            source,
            poly_implementors,
            expr,
            ..
        } => {
            tags.push(tag_for(source));
            for imp in poly_implementors {
                tags.push(tag_for_implementor(imp));
            }
            collect_expr(expr, tags);
        }
    }
}

fn collect_multi_link_values(v: &IrMultiLinkValues, tags: &mut Vec<String>) {
    collect_multi_link_value_source(&v.source, tags);
    for (_, e) in &v.link_props {
        collect_expr(e, tags);
    }
}

fn collect_multi_link_value_source(src: &IrMultiLinkValueSource, tags: &mut Vec<String>) {
    match src {
        IrMultiLinkValueSource::CteRef(_) => {}
        IrMultiLinkValueSource::Select(sel) => collect_select(sel, tags),
        IrMultiLinkValueSource::PathSelect(ps) => collect_path_select(ps, tags),
        IrMultiLinkValueSource::Union(a, b) => {
            collect_multi_link_values(a, tags);
            collect_multi_link_values(b, tags);
        }
    }
}

fn collect_expr(expr: &IrExpr, tags: &mut Vec<String>) {
    match expr {
        IrExpr::ColumnRef { .. }
        | IrExpr::Param { .. }
        | IrExpr::Literal(_)
        | IrExpr::Null
        | IrExpr::CteRef { .. }
        | IrExpr::CteFieldRef { .. }
        | IrExpr::ForVar { .. }
        | IrExpr::EnumLiteral { .. }
        | IrExpr::GlobalParam { .. }
        | IrExpr::GlobalRef { .. }
        | IrExpr::FnParam { .. }
        | IrExpr::RawSql(_) => {}
        IrExpr::BinOp(b) => {
            collect_expr(&b.left, tags);
            collect_expr(&b.right, tags);
        }
        IrExpr::UnaryOp(u) => collect_expr(&u.operand, tags),
        IrExpr::FunctionCall(f) => {
            for a in &f.args {
                collect_expr(a, tags);
            }
        }
        IrExpr::TypeCast(c) => collect_expr(&c.expr, tags),
        IrExpr::IfElse(ie) => {
            collect_expr(&ie.condition, tags);
            collect_expr(&ie.if_, tags);
            collect_expr(&ie.else_, tags);
        }
        IrExpr::Subquery(sel) => collect_select(sel, tags),
        IrExpr::Array(elems) | IrExpr::Tuple(elems) => {
            for e in elems {
                collect_expr(e, tags);
            }
        }
        IrExpr::AggOverSet { elems, .. } => {
            for e in elems {
                collect_expr(e, tags);
            }
        }
        IrExpr::AggOverQuery { inner, .. } => collect_select(inner, tags),
        IrExpr::ArrayFromSelect(src) => collect_array_source(src, tags),
        IrExpr::NamedTuple { fields, .. } => {
            for (_, e) in fields {
                collect_expr(e, tags);
            }
        }
        IrExpr::Subscript { expr, index, .. } => {
            collect_expr(expr, tags);
            collect_expr(index, tags);
        }
        IrExpr::JsonbField { expr, .. } | IrExpr::JsonbIndex { expr, .. } => collect_expr(expr, tags),
        IrExpr::Slice { expr, lower, upper, .. } => {
            collect_expr(expr, tags);
            if let Some(l) = lower {
                collect_expr(l, tags);
            }
            if let Some(u) = upper {
                collect_expr(u, tags);
            }
        }
        IrExpr::PathSubquery(ps) => collect_path_select(ps, tags),
        IrExpr::FnSubquery(fs) => collect_function_select(fs, tags),
    }
}

fn collect_function_select(fs: &IrFunctionSelect, tags: &mut Vec<String>) {
    for imp in &fs.poly_implementors {
        tags.push(tag_for_implementor(imp));
    }
    for a in &fs.fn_args {
        collect_expr(a, tags);
    }
    for p in &fs.shape {
        collect_shape_pointer(p, tags);
    }
    if let Some(f) = &fs.filter {
        collect_expr(f, tags);
    }
    for s in &fs.order_by {
        collect_expr(&s.expr, tags);
    }
    if let Some(e) = &fs.offset {
        collect_expr(e, tags);
    }
    if let Some(e) = &fs.limit {
        collect_expr(e, tags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(type_name: &str, table: &str, alias: &str) -> IrSource {
        IrSource {
            type_name: type_name.into(),
            table: table.into(),
            alias: alias.into(),
        }
    }

    fn output(stmt: IrStmt) -> IrOutput {
        IrOutput {
            stmt,
            params: vec![],
            ctes: vec![],
            global_ctes: vec![],
            warnings: vec![],
            uses_globals_arg: false,
        }
    }

    fn scalar_pointer(alias: &str) -> IrShapePointer {
        IrShapePointer::Scalar(IrScalarPointer {
            marker_offset: None,
            alias: alias.into(),
            column: alias.into(),
            pg_type: "text".into(),
            tuple_shape: None,
        })
    }

    #[test]
    fn bound_select_tags_its_own_table_under_public_for_default_module() {
        let sel = IrSelect::schema_bound(
            src("default::Person", "person", "t0"),
            vec![scalar_pointer("name")],
            None,
        );
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["public.person"]);
    }

    #[test]
    fn non_default_module_keeps_its_own_schema_name() {
        let sel = IrSelect::schema_bound(src("catalog::Product", "product", "t0"), vec![], None);
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["catalog.product"]);
    }

    #[test]
    fn single_link_shape_pointer_tags_the_target_table_too() {
        let inner = IrSelect::schema_bound(src("default::Company", "company", "t1"), vec![], None);
        let link = IrShapePointer::SingleLink(IrSingleLinkPointer {
            marker_offset: None,
            alias: "company".into(),
            correlation: IrSingleLinkCorrelation::Fk {
                fk_column: "company_id".into(),
                target_pk: "id".into(),
            },
            subquery: inner,
            link_properties: vec![],
        });
        let sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![link], None);
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["public.company", "public.person"]);
    }

    #[test]
    fn junction_backed_single_link_shape_pointer_tags_the_junction_table_too() {
        // Regression: a write to the junction table (e.g. re-linking
        // Person.spouse) must invalidate a cached SELECT that reads it — the
        // `SingleLink` arm here originally reused the FK-only logic
        // unconditionally, never tagging the junction table for the new
        // `Junction` correlation variant, so the cache never invalidated
        // (confirmed live: the junction row updated correctly, but the
        // Data Explorer kept showing stale data after commit).
        let inner = IrSelect::schema_bound(src("default::Org", "org", "t1"), vec![], None);
        let link = IrShapePointer::SingleLink(IrSingleLinkPointer {
            marker_offset: None,
            alias: "spouse".into(),
            correlation: IrSingleLinkCorrelation::Junction {
                join: IrMultiLinkJoin::Standard {
                    junction_table: "person.spouse".into(),
                    module: "default".into(),
                },
                target_pk: "id".into(),
            },
            subquery: inner,
            link_properties: vec![],
        });
        let sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![link], None);
        let out = output(IrStmt::Select(sel));
        assert_eq!(
            collect_tags(&out),
            vec!["public.org", "public.person", "public.person.spouse"]
        );
    }

    #[test]
    fn path_select_over_junction_backed_single_link_tags_the_junction_table_too() {
        // Same regression as above, for the top-level `select
        // Person.spouse` path-select form (`IrPathJoin::Multi`), which
        // previously only tagged the join's target table.
        let ps = IrPathSelect {
            root: src("default::Person", "person", "t0"),
            joins: vec![IrPathJoin::Multi {
                source_alias: "t0".into(),
                junction_alias: "jt".into(),
                join: IrMultiLinkJoin::Standard {
                    junction_table: "person.spouse".into(),
                    module: "default".into(),
                },
                target: src("default::Org", "org", "t1"),
            }],
            result: IrPathResult::Object {
                alias: "t1".into(),
                type_name: "default::Org".into(),
                shape: vec![],
            },
            filter: None,
            order_by: vec![],
            offset: None,
            limit: None,
            distinct: false,
            poly_implementors: vec![],
        };
        let out = output(IrStmt::PathSelect(ps));
        assert_eq!(
            collect_tags(&out),
            vec!["public.org", "public.person", "public.person.spouse"]
        );
    }

    #[test]
    fn multi_link_tags_target_table_and_junction_table() {
        let inner = IrSelect::schema_bound(src("default::Post", "post", "t1"), vec![], None);
        let ml = IrShapePointer::MultiLink(IrMultiLinkPointer {
            marker_offset: None,
            alias: "posts".into(),
            join: IrMultiLinkJoin::Standard {
                junction_table: "person.posts".into(),
                module: "default".into(),
            },
            subquery: inner,
            link_properties: vec![],
        });
        let sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![ml], None);
        let out = output(IrStmt::Select(sel));
        assert_eq!(
            collect_tags(&out),
            vec!["public.person", "public.person.posts", "public.post"]
        );
    }

    #[test]
    fn polymorphic_select_tags_every_implementor() {
        let mut sel = IrSelect::schema_bound(src("default::Animal", "animal", "t0"), vec![], None);
        sel.polymorphic = true;
        sel.poly_implementors = vec![
            IrPolyImplementor {
                type_name: "default::Dog".into(),
                table: "dog".into(),
                module: "default".into(),
            },
            IrPolyImplementor {
                type_name: "default::Cat".into(),
                table: "cat".into(),
                module: "default".into(),
            },
        ];
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["public.animal", "public.cat", "public.dog"]);
    }

    #[test]
    fn insert_tags_target_and_junction_tables() {
        let ins = IrInsert {
            target: src("default::Person", "person", "t0"),
            assignments: vec![],
            unless_conflict: None,
            rewrites: vec![],
            returning: vec![],
            enqueue_vector: vec![],
            enqueue_search: vec![],
            multi_link_appends: vec![IrMultiLinkMutation {
                junction_table: "person.posts".into(),
                module: "default".into(),
                source_col: "source".into(),
                target_col: "target".into(),
                values: IrMultiLinkValues {
                    source: IrMultiLinkValueSource::Select(Box::new(IrSelect::schema_bound(
                        src("default::Post", "post", "t1"),
                        vec![],
                        None,
                    ))),
                    link_props: vec![],
                },
                single: false,
            }],
            nested_ctes: vec![],
        };
        let out = output(IrStmt::Insert(ins));
        assert_eq!(
            collect_tags(&out),
            vec!["public.person", "public.person.posts", "public.post"]
        );
    }

    #[test]
    fn update_tags_target_and_all_multi_link_mutation_junctions() {
        let upd = IrUpdate {
            target: src("default::Person", "person", "t0"),
            filter: None,
            assignments: vec![],
            rewrites: vec![],
            returning: vec![],
            enqueue_vector: vec![],
            enqueue_search: vec![],
            poly_implementors: vec![],
            poly_columns: vec![],
            multi_link_clears: vec![IrMultiLinkClear {
                junction_table: "person.posts".into(),
                module: "default".into(),
                source_col: "source".into(),
            }],
            multi_link_replaces: vec![],
            multi_link_appends: vec![],
            multi_link_removals: vec![],
            nested_ctes: vec![],
        };
        let out = output(IrStmt::Update(upd));
        assert_eq!(collect_tags(&out), vec!["public.person", "public.person.posts"]);
    }

    #[test]
    fn delete_tags_target_table() {
        let del = IrDelete {
            target: src("default::Person", "person", "t0"),
            filter: None,
            returning: vec![],
            poly_implementors: vec![],
            poly_columns: vec![],
            enqueue_search: vec![],
        };
        let out = output(IrStmt::Delete(del));
        assert_eq!(collect_tags(&out), vec!["public.person"]);
    }

    #[test]
    fn nested_subquery_in_filter_expression_is_tagged() {
        let inner = IrSelect::schema_bound(src("default::Company", "company", "t1"), vec![], None);
        let filter = IrExpr::Subquery(Box::new(inner));
        let sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![], Some(filter));
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["public.company", "public.person"]);
    }

    #[test]
    fn with_cte_and_computed_global_cte_are_both_tagged() {
        let cte_sel = IrSelect::schema_bound(src("default::Company", "company", "t1"), vec![], None);
        let main_sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![], None);
        let mut out = output(IrStmt::Select(main_sel));
        out.ctes.push(IrCteDef {
            name: "c".into(),
            stmt: IrStmt::Select(cte_sel),
            type_name: "default::Company".into(),
        });
        out.global_ctes
            .push(IrGlobalCte::Computed(Box::new(IrComputedGlobalCte {
                cte_name: "g".into(),
                qualified_name: "default::current_user".into(),
                stmt: IrStmt::Select(IrSelect::schema_bound(src("default::Post", "post", "t2"), vec![], None)),
            })));
        assert_eq!(
            collect_tags(&out),
            vec!["public.company", "public.person", "public.post"]
        );
    }

    #[test]
    fn duplicate_tags_are_deduped() {
        let inner1 = IrSelect::schema_bound(src("default::Company", "company", "t1"), vec![], None);
        let inner2 = IrSelect::schema_bound(src("default::Company", "company", "t2"), vec![], None);
        let link1 = IrShapePointer::SingleLink(IrSingleLinkPointer {
            marker_offset: None,
            alias: "a".into(),
            correlation: IrSingleLinkCorrelation::Fk {
                fk_column: "a_id".into(),
                target_pk: "id".into(),
            },
            subquery: inner1,
            link_properties: vec![],
        });
        let link2 = IrShapePointer::SingleLink(IrSingleLinkPointer {
            marker_offset: None,
            alias: "b".into(),
            correlation: IrSingleLinkCorrelation::Fk {
                fk_column: "b_id".into(),
                target_pk: "id".into(),
            },
            subquery: inner2,
            link_properties: vec![],
        });
        let sel = IrSelect::schema_bound(src("default::Person", "person", "t0"), vec![link1, link2], None);
        let out = output(IrStmt::Select(sel));
        assert_eq!(collect_tags(&out), vec!["public.company", "public.person"]);
    }
}
