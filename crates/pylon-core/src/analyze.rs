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
//! Deliberately narrower than a full port of the upstream engine's `ir_analyze.py`: no
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

// ── Coarse-grained plan correlation ─────────────────────────────────────────────
//
// Postgres's own `EXPLAIN (FORMAT JSON)` output is a JSON array with one
// element (`[{"Plan": {...}, ...}]`); deliberately modeled on only the
// handful of fields every plan node kind carries (see postgres/src/backend/
// commands/explain.c) rather than the upstream engine's ~30-subclass `pg_tree.py` — a v1
// coarse-grained tree only needs cost/time/rows/width plus enough identity
// (`Alias`/`Relation Name`) to correlate against `ShapePathAlias`.

use std::collections::HashMap;

#[derive(Debug, Clone, serde::Deserialize)]
struct RawExplainRoot {
    #[serde(rename = "Plan")]
    plan: RawPlanNode,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawPlanNode {
    #[serde(rename = "Alias")]
    alias: Option<String>,
    #[serde(rename = "Relation Name")]
    relation_name: Option<String>,
    #[serde(rename = "Startup Cost", default)]
    startup_cost: f64,
    #[serde(rename = "Total Cost", default)]
    total_cost: f64,
    #[serde(rename = "Plan Rows", default)]
    plan_rows: f64,
    #[serde(rename = "Plan Width", default)]
    plan_width: i64,
    #[serde(rename = "Actual Startup Time")]
    actual_startup_time: Option<f64>,
    #[serde(rename = "Actual Total Time")]
    actual_total_time: Option<f64>,
    #[serde(rename = "Actual Rows")]
    actual_rows: Option<f64>,
    #[serde(rename = "Actual Loops")]
    actual_loops: Option<f64>,
    #[serde(rename = "Plans", default)]
    plans: Vec<RawPlanNode>,
}

/// Cost/timing figures carried by one coarse-grained node — lifted verbatim
/// from whichever raw Postgres plan node is the top of that shape path's own
/// subtree. `actual_*` fields are `None` under plain `EXPLAIN` (no
/// `ANALYZE`, so the query was never actually run) — matches Postgres's own
/// output, which omits them in that case.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCost {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub plan_rows: f64,
    pub plan_width: i64,
    pub actual_startup_time: Option<f64>,
    pub actual_total_time: Option<f64>,
    pub actual_rows: Option<f64>,
    pub actual_loops: Option<f64>,
}

impl From<&RawPlanNode> for PlanCost {
    fn from(raw: &RawPlanNode) -> Self {
        PlanCost {
            startup_cost: raw.startup_cost,
            total_cost: raw.total_cost,
            plan_rows: raw.plan_rows,
            plan_width: raw.plan_width,
            actual_startup_time: raw.actual_startup_time,
            actual_total_time: raw.actual_total_time,
            actual_rows: raw.actual_rows,
            actual_loops: raw.actual_loops,
        }
    }
}

/// One node of the coarse-grained tree — the REPL text formatter and the
/// Query Editor's visual view both render this same shape directly (see the
/// crate's `analyze` design notes).
#[derive(Debug, Clone, PartialEq)]
pub struct CoarseGrainedNode {
    pub path: String,
    /// Relation names touched by this path's own plan nodes (not those of a
    /// nested child path) — deduplicated, in first-encountered order.
    pub relations: Vec<String>,
    pub cost: PlanCost,
    /// `(pointer_name, node)` — the pointer name is `path`'s own last dotted
    /// segment (e.g. `"villains"` for `"root.villains"`).
    pub children: Vec<(String, CoarseGrainedNode)>,
}

/// Parse Postgres's `EXPLAIN (FORMAT JSON)` output text and correlate it
/// against `path_aliases` (see `collect_shape_path_aliases`) into a
/// coarse-grained tree. `path_aliases` empty (a statement kind with no
/// shape breakdown — see `collect_shape_path_aliases`) still produces a
/// single root node; it just never gains any pointer children.
pub fn build_coarse_grained(raw_json: &str, path_aliases: &[ShapePathAlias]) -> Result<CoarseGrainedNode, String> {
    let roots: Vec<RawExplainRoot> = serde_json::from_str(raw_json)
        .map_err(|e| format!("malformed EXPLAIN (FORMAT JSON) output: {e}"))?;
    let root = roots.into_iter().next().ok_or_else(|| "EXPLAIN produced no plan".to_string())?;

    let alias_to_path: HashMap<&str, &str> =
        path_aliases.iter().map(|p| (p.sql_alias.as_str(), p.path.as_str())).collect();

    Ok(build_node(&root.plan, ROOT_PATH, &alias_to_path))
}

fn build_node(raw: &RawPlanNode, path: &str, alias_to_path: &HashMap<&str, &str>) -> CoarseGrainedNode {
    let mut relations = Vec::new();
    let mut seen_relations = std::collections::HashSet::new();
    let mut children = Vec::new();
    collect_plan_nodes(raw, path, alias_to_path, &mut relations, &mut seen_relations, &mut children);
    CoarseGrainedNode { path: path.to_string(), relations, cost: PlanCost::from(raw), children }
}

/// Walk `raw`'s own subtree, folding nodes into `relations`/this level's
/// implicit cost — until a child's *subtree* (not just the child node
/// itself) resolves to a *different* shape path, at which point that whole
/// child subtree roots its own nested `CoarseGrainedNode` instead (mirrors
/// the upstream engine's `coarse_grained.py` `_build_shape`/`_scan_relations`, simplified: no
/// context-hoisting, no fine-grained squash pass first).
///
/// Checking the whole subtree (not just `child.alias`) matters for a
/// junction-backed link's own correlated subquery: its join always has (at
/// least) two scan children — the junction table (alias always the literal
/// `"jt"`, see `sql::mod`'s `emit_multi_link`/`emit_single_link`, never
/// present in `alias_to_path`) and the target table (whose alias *is* in the
/// map). Resolving only `child.alias` would attribute whichever scan happens
/// to come first in Postgres's own child order to the *parent* path instead
/// of the pointer's own — resolving the whole subtree first, before
/// deciding, sidesteps that ordering dependency entirely.
fn collect_plan_nodes(
    raw: &RawPlanNode,
    path: &str,
    alias_to_path: &HashMap<&str, &str>,
    relations: &mut Vec<String>,
    seen_relations: &mut std::collections::HashSet<String>,
    children: &mut Vec<(String, CoarseGrainedNode)>,
) {
    if let Some(rel) = &raw.relation_name {
        if seen_relations.insert(rel.clone()) {
            relations.push(rel.clone());
        }
    }
    for child in &raw.plans {
        match resolve_subtree_path(child, alias_to_path) {
            Some(child_path) if child_path != path => {
                let pointer_name = child_path.rsplit('.').next().unwrap_or(child_path).to_string();
                children.push((pointer_name, build_node(child, child_path, alias_to_path)));
            }
            _ => collect_plan_nodes(child, path, alias_to_path, relations, seen_relations, children),
        }
    }
}

/// The first shape path resolvable anywhere in `node`'s own subtree (`node`
/// itself, then its descendants), or `None` if nothing in it correlates to a
/// known shape path at all. Safe to treat as unambiguous here: a single
/// pointer's correlated subquery is always planned as its own independent
/// subtree (a `SubPlan`/`InitPlan` in Postgres's own terms) — it never
/// shares a join with a sibling pointer's subquery, so a subtree can only
/// ever resolve to one path, never a mix of two.
fn resolve_subtree_path<'a>(node: &RawPlanNode, alias_to_path: &HashMap<&str, &'a str>) -> Option<&'a str> {
    if let Some(path) = node.alias.as_deref().and_then(|a| alias_to_path.get(a).copied()) {
        return Some(path);
    }
    node.plans.iter().find_map(|child| resolve_subtree_path(child, alias_to_path))
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

    // ── build_coarse_grained ─────────────────────────────────────────────────

    /// No live Postgres in this test environment (the crate's own
    /// `live_execution_*` integration tests are all `#[ignore]`d for the
    /// same reason), so this fixture's plan-node *shape* (Seq Scan / Nested
    /// Loop nesting) is hand-built to match real `EXPLAIN (FORMAT JSON)`
    /// output structurally — it's not a literal capture. The `Alias` values
    /// inside it are NOT invented, though: they're read straight off a real
    /// `collect_shape_path_aliases` call, so the correlation step under test
    /// is matched against genuine compiler-assigned aliases either way.
    fn explain_json_fixture(root_alias: &str, villain_alias: &str) -> String {
        format!(
            r#"[
              {{
                "Plan": {{
                  "Node Type": "Seq Scan",
                  "Alias": "{root_alias}",
                  "Relation Name": "hero",
                  "Startup Cost": 0.0,
                  "Total Cost": 12.5,
                  "Plan Rows": 100,
                  "Plan Width": 40,
                  "Actual Startup Time": 0.01,
                  "Actual Total Time": 0.05,
                  "Actual Rows": 100,
                  "Actual Loops": 1,
                  "Plans": [
                    {{
                      "Node Type": "Nested Loop",
                      "Startup Cost": 0.0,
                      "Total Cost": 8.2,
                      "Plan Rows": 3,
                      "Plan Width": 32,
                      "Actual Startup Time": 0.0,
                      "Actual Total Time": 0.01,
                      "Actual Rows": 3,
                      "Actual Loops": 100,
                      "Plans": [
                        {{
                          "Node Type": "Seq Scan",
                          "Alias": "hv",
                          "Relation Name": "hero.villains",
                          "Startup Cost": 0.0,
                          "Total Cost": 2.0,
                          "Plan Rows": 3,
                          "Plan Width": 16,
                          "Actual Startup Time": 0.0,
                          "Actual Total Time": 0.0,
                          "Actual Rows": 3,
                          "Actual Loops": 100,
                          "Plans": []
                        }},
                        {{
                          "Node Type": "Seq Scan",
                          "Alias": "{villain_alias}",
                          "Relation Name": "villain",
                          "Startup Cost": 0.0,
                          "Total Cost": 2.0,
                          "Plan Rows": 1,
                          "Plan Width": 32,
                          "Actual Startup Time": 0.0,
                          "Actual Total Time": 0.0,
                          "Actual Rows": 1,
                          "Actual Loops": 3,
                          "Plans": []
                        }}
                      ]
                    }}
                  ]
                }},
                "Planning Time": 0.1,
                "Execution Time": 0.2
              }}
            ]"#
        )
    }

    #[test]
    fn test_build_coarse_grained_groups_nodes_by_shape_path() {
        let ir = compile("select Hero { name, villains: { name } }");
        let paths = collect_shape_path_aliases(&ir.stmt);
        let root_alias = paths.iter().find(|p| p.path == "root").unwrap().sql_alias.clone();
        let villains_alias = paths.iter().find(|p| p.path == "root.villains").unwrap().sql_alias.clone();

        let raw_json = explain_json_fixture(&root_alias, &villains_alias);
        let tree = build_coarse_grained(&raw_json, &paths).unwrap();

        assert_eq!(tree.path, "root");
        assert_eq!(tree.relations, vec!["hero".to_string()]);
        assert_eq!(tree.cost.total_cost, 12.5);
        assert_eq!(tree.children.len(), 1);

        let (pointer_name, villains_node) = &tree.children[0];
        assert_eq!(pointer_name, "villains");
        assert_eq!(villains_node.path, "root.villains");
        // Both the junction table ("hero.villains", alias "hv" — never in
        // the alias map, see resolve_subtree_path's doc comment) and the
        // target table ("villain") roll up into this same path's relations,
        // since the whole Nested Loop subtree resolves to "root.villains".
        assert_eq!(villains_node.relations, vec!["hero.villains".to_string(), "villain".to_string()]);
        // Cost is the Nested Loop's own (the top of this path's subtree),
        // not the inner villain scan's — see resolve_subtree_path.
        assert_eq!(villains_node.cost.total_cost, 8.2);
        assert!(villains_node.children.is_empty());
    }

    #[test]
    fn test_build_coarse_grained_with_no_path_aliases_still_produces_a_root_node() {
        // A statement kind `collect_shape_path_aliases` can't break down
        // (see its own doc comment) still gets a single root node — no
        // pointer children, but not an error either.
        let raw_json = explain_json_fixture("t0", "t1");
        let tree = build_coarse_grained(&raw_json, &[]).unwrap();
        assert_eq!(tree.path, "root");
        assert!(tree.children.is_empty());
    }

    #[test]
    fn test_build_coarse_grained_rejects_malformed_json() {
        assert!(build_coarse_grained("not json", &[]).is_err());
    }
}
