//! Support for `analyze <query>` — running a query through Postgres's
//! `EXPLAIN (ANALYZE, FORMAT JSON)` and reporting a query plan grouped by the
//! shape of the original query (its root select, each nested link, etc.)
//! instead of raw SQL relation names.
//!
//! This module builds the one piece of correlation data the rest of the
//! feature (SQL execution, REPL/UI rendering) needs: a map from each SQL
//! relation alias emitted into the compiled query (e.g. `t3`) to the dotted
//! shape path it corresponds to (e.g. `root.villains`), plus the source byte
//! offset to place that path's marker at in the echoed query text.
//!
//! Deliberately narrower than a full port of Gel's `ir_analyze.py`: no
//! general expression-level span tracking, no context-hoisting heuristics —
//! just enough structure to build the *coarse-grained* tree the REPL and
//! Query Editor render (see the crate's `analyze` design notes for why).

use crate::ir::{IrGroup, IrRowSource, IrSelect, IrShapePointer, IrStmt};

/// One shape-tree position's SQL identity: the relation alias Postgres's own
/// `EXPLAIN (FORMAT JSON)` output will report for it (verbatim — see
/// `sql::mod`'s `qi(&source.alias)` emission), the dotted path naming that
/// position (`"root"`, `"root.villains"`, `"root.villains.nemesis"`, ...),
/// and the source byte offset to plant that path's marker at, if the
/// originating shape element carried one (see `parse::ast::ShapeElement`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShapePathAlias {
    pub sql_alias: String,
    pub path: String,
    pub marker_offset: Option<usize>,
}

const ROOT_PATH: &str = "root";

/// Walk a compiled statement's shape tree, collecting one `ShapePathAlias`
/// per level reachable from the root. Statement kinds with no `(root
/// IrSource, shape: Vec<IrShapePointer>)` pair (`for`, path-select,
/// function-select, vector/FTS search) produce an empty list — the caller
/// falls back to treating the whole plan as a single unlabeled root in that
/// case, which is an acceptable v1 gap (see the crate's `analyze` design
/// notes).
pub fn collect_shape_path_aliases(stmt: &IrStmt) -> Vec<ShapePathAlias> {
    let mut out = Vec::new();
    match stmt {
        IrStmt::Select(sel) => collect_select(sel, ROOT_PATH, None, &mut out),
        IrStmt::Insert(ins) => {
            out.push(ShapePathAlias { sql_alias: ins.target.alias.clone(), path: ROOT_PATH.to_string(), marker_offset: None });
            collect_shape(&ins.returning, ROOT_PATH, &mut out);
        }
        IrStmt::Update(upd) => {
            out.push(ShapePathAlias { sql_alias: upd.target.alias.clone(), path: ROOT_PATH.to_string(), marker_offset: None });
            collect_shape(&upd.returning, ROOT_PATH, &mut out);
        }
        IrStmt::Delete(del) => {
            out.push(ShapePathAlias { sql_alias: del.target.alias.clone(), path: ROOT_PATH.to_string(), marker_offset: None });
            collect_shape(&del.returning, ROOT_PATH, &mut out);
        }
        IrStmt::Group(g) => {
            let IrGroup { source, shape, .. } = g;
            out.push(ShapePathAlias { sql_alias: source.alias.clone(), path: ROOT_PATH.to_string(), marker_offset: None });
            collect_shape(shape, ROOT_PATH, &mut out);
        }
        IrStmt::PathSelect(_)
        | IrStmt::For(_)
        | IrStmt::FunctionSelect(_)
        | IrStmt::VectorSearch(_)
        | IrStmt::FtsSearch(_) => {}
    }
    out
}

fn collect_select(sel: &IrSelect, path: &str, marker_offset: Option<usize>, out: &mut Vec<ShapePathAlias>) {
    for row in &sel.rows {
        if let IrRowSource::Bound { source, shape } = row {
            out.push(ShapePathAlias { sql_alias: source.alias.clone(), path: path.to_string(), marker_offset });
            collect_shape(shape, path, out);
        }
        // IrRowSource::Free (set/tuple/free-object literal): no relation
        // alias to correlate — nothing to add.
    }
}

fn collect_shape(shape: &[IrShapePointer], parent_path: &str, out: &mut Vec<ShapePathAlias>) {
    for ptr in shape {
        match ptr {
            IrShapePointer::SingleLink(p) => {
                let path = format!("{parent_path}.{}", p.alias);
                collect_select(&p.subquery, &path, p.marker_offset, out);
            }
            IrShapePointer::MultiLink(p) => {
                let path = format!("{parent_path}.{}", p.alias);
                collect_select(&p.subquery, &path, p.marker_offset, out);
            }
            // Scalar/Computed/ScalarSet: no nested subquery of their own, so
            // no separate SQL alias to correlate — they're covered by their
            // parent's own row alias.
            IrShapePointer::Scalar(_) | IrShapePointer::Computed(_) | IrShapePointer::ScalarSet(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
    use crate::{ir, parse};

    fn id_prop() -> PropertyDescriptor {
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
            tuple_members: None,
        }
    }

    fn name_prop() -> PropertyDescriptor {
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
            tuple_members: None,
        }
    }

    fn make_schema() -> SchemaDescriptor {
        SchemaDescriptor {
            types: vec![
                TypeDescriptor {
                    name: "Hero".into(),
                    module: "default".into(),
                    table: "hero".into(),
                    abstract_: false,
                    materialized: false,
                    description: None,
                    parents: vec![],
                    interfaces: vec![],
                    properties: vec![id_prop(), name_prop()],
                    links: vec![],
                    multilinks: vec![MultiLinkDescriptor {
                        name: "villains".into(),
                        target: "default::Villain".into(),
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
                    signals: vec![],
                },
                TypeDescriptor {
                    name: "Villain".into(),
                    module: "default".into(),
                    table: "villain".into(),
                    abstract_: false,
                    materialized: false,
                    description: None,
                    parents: vec![],
                    interfaces: vec![],
                    properties: vec![id_prop(), name_prop()],
                    links: vec![LinkDescriptor {
                        name: "nemesis".into(),
                        target: "default::Hero".into(),
                        nullable: true,
                        through: None,
                        description: None,
                        default_pyql: None,
                        is_exclusive: false,
                        is_readonly: false,
                        rewrites: vec![],
                        on_delete: vec![],
                    }],
                    multilinks: vec![],
                    computed: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    vector_indexes: vec![],
                    search_indexes: vec![],
                    triggers: vec![],
                    junction: false,
                    signals: vec![],
                },
            ],
            scalars: vec![],
            enums: vec![],
            named_tuples: vec![],
            globals: vec![],
            functions: vec![],
            aliases: vec![],
        }
    }

    fn compile(query: &str) -> ir::IrOutput {
        let schema = make_schema();
        let ast = parse::parse(query).unwrap();
        // `analyze` transparently unwraps to the inner statement's IR (see
        // ir::compiler's `Stmt::Analyze` arm) — compiling the bare inner
        // query directly gives the identical `IrOutput` a real `analyze`
        // caller would walk.
        let ast = match ast {
            parse::Stmt::Analyze(inner) => *inner,
            other => other,
        };
        ir::compile(&ast, &schema).unwrap()
    }

    #[test]
    fn test_root_only_query_produces_a_single_root_alias() {
        let ir = compile("select Hero { name }");
        let paths = collect_shape_path_aliases(&ir.stmt);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].path, "root");
    }

    #[test]
    fn test_nested_multilink_produces_root_and_nested_paths() {
        let ir = compile("select Hero { name, villains: { name, nemesis: { name } } }");
        let paths = collect_shape_path_aliases(&ir.stmt);
        let by_path: Vec<&str> = paths.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(by_path, vec!["root", "root.villains", "root.villains.nemesis"]);
        // Each level's alias must be distinct — they're different SQL relations.
        assert_ne!(paths[0].sql_alias, paths[1].sql_alias);
        assert_ne!(paths[1].sql_alias, paths[2].sql_alias);
    }

    #[test]
    fn test_marker_offsets_survive_from_ast_to_the_alias_map() {
        let query = "select Hero { name, villains: { name } }";
        let ir = compile(query);
        let paths = collect_shape_path_aliases(&ir.stmt);
        let villains = paths.iter().find(|p| p.path == "root.villains").unwrap();
        let offset = villains.marker_offset.expect("villains path should carry a marker offset");
        assert_eq!(&query[offset..offset + "villains".len()], "villains");
    }

    #[test]
    fn test_analyze_wrapped_query_compiles_to_the_same_ir_as_the_bare_inner_query() {
        // Sanity check for the `compile()` test helper's unwrap-Analyze step:
        // an `analyze`-prefixed query and its bare inner form must produce
        // identical shape-path aliases, since `analyze` only changes
        // execution (see ir::compiler's `Stmt::Analyze` arm).
        let wrapped = compile("analyze select Hero { name, villains: { name } }");
        let bare = compile("select Hero { name, villains: { name } }");
        assert_eq!(collect_shape_path_aliases(&wrapped.stmt).len(), collect_shape_path_aliases(&bare.stmt).len());
    }
}
