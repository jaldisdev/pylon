//! Junction-table cleanup and `on_delete` cascade-policy live-execution
//! tests. See `live_execution_smoke.rs` for the harness's purpose and how
//! to run these (same pattern, this binary is `--test live_execution_on_delete`).
//!
//! This whole area had **zero** prior test coverage — not even a fast
//! SQL-text snapshot test — despite genuinely intricate logic spanning two
//! independently-duplicated code paths (`export::export_schema`'s
//! `target_fk_suffix`/`source_jt_fk_suffix`/`target_jt_fk_suffix` plus
//! `emit_link_source_triggers`/`emit_multilink_deletion_triggers`, and a
//! separate mirror in `diff/mod.rs` for the incremental-migration path).
//! Every case here inserts real rows, deletes the triggering one, and
//! asserts on real post-delete state — the only way to actually verify a
//! `BEFORE DELETE` trigger or an FK cascade did what it was supposed to.
//!
//! **Baseline finding (addresses the original suspicion that prompted this
//! whole test group): junction-row cleanup on owner deletion is NOT
//! configurable and always works** — `source_jt_fk_suffix` unconditionally
//! returns `ON DELETE CASCADE` for the junction table's owner-side FK,
//! regardless of any `on_delete` policy. Confirmed live in
//! `deleting_the_owner_always_cleans_up_its_own_junction_rows`.

mod common;

use common::*;
use pylon_core::diff::{diff_schema, DbState};
use pylon_core::export::export_schema;
use pylon_core::query;
use pylon_core::schema::{DeleteAction, DeleteSide, MultiLinkDescriptor, OnDeletePolicy, PropertyDescriptor, SchemaDescriptor, TypeDescriptor};
use pylon_pgcon::ExtensionOids;
use pylon_value::CachedValue;

fn ty(
    name: &str,
    module: &str,
    properties: Vec<PropertyDescriptor>,
    links: Vec<pylon_core::schema::LinkDescriptor>,
    multilinks: Vec<MultiLinkDescriptor>,
) -> TypeDescriptor {
    TypeDescriptor {
        name: name.into(),
        module: module.into(),
        table: name.into(),
        abstract_: false,
        materialized: true,
        description: None,
        parents: vec![],
        interfaces: vec![],
        properties,
        links,
        multilinks,
        computed: vec![],
        constraints: vec![],
        indexes: vec![],
        vector_indexes: vec![],
        search_indexes: vec![],
        triggers: vec![],
        junction: false,
        signals: vec![],
    }
}

fn nullable_link(name: &str, target_qname: &str, on_delete: Vec<OnDeletePolicy>) -> pylon_core::schema::LinkDescriptor {
    pylon_core::schema::LinkDescriptor {
        name: name.into(),
        target: target_qname.into(),
        nullable: true,
        through: None,
        description: None,
        default_pyql: None,
        is_exclusive: false,
        is_readonly: false,
        rewrites: vec![],
        on_delete,
    }
}

fn ml(name: &str, target_qname: &str, on_delete: Vec<OnDeletePolicy>) -> MultiLinkDescriptor {
    MultiLinkDescriptor {
        name: name.into(),
        target: target_qname.into(),
        through: None,
        nullable: false,
        description: None,
        default_pyql: None,
        on_delete,
    }
}

/// One `Org` type, five `Team*` types each with a single link to `Org`
/// configured with a different `on_delete` policy, and one `Tag` type with
/// five `Product*` types each with a multilink to `Tag` configured with a
/// different policy — enough to exercise every `DeleteAction` at least
/// once, on both a single link and a multilink.
fn on_delete_schema(module: &str) -> SchemaDescriptor {
    let org_q = format!("{module}::Org");
    let tag_q = format!("{module}::Tag");
    SchemaDescriptor {
        types: vec![
            ty("Org", module, vec![id_prop(), text_prop("name")], vec![], vec![]),
            ty("Tag", module, vec![id_prop(), text_prop("name")], vec![], vec![]),
            // Single-link on_delete variants (policy is Target-side unless noted).
            ty(
                "TeamRestrictDefault",
                module,
                vec![id_prop(), text_prop("name")],
                vec![nullable_link("org", &org_q, vec![])], // unspecified -> Restrict
                vec![],
            ),
            ty(
                "TeamSetNull",
                module,
                vec![id_prop(), text_prop("name")],
                vec![nullable_link("org", &org_q, vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::Allow }])],
                vec![],
            ),
            ty(
                "TeamCascadeOnTargetDelete",
                module,
                vec![id_prop(), text_prop("name")],
                vec![nullable_link("org", &org_q, vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::DeleteSource }])],
                vec![],
            ),
            ty(
                "TeamDeleteTarget",
                module,
                vec![id_prop(), text_prop("name")],
                vec![nullable_link("org", &org_q, vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTarget }])],
                vec![],
            ),
            ty(
                "TeamDeleteTargetIfOrphan",
                module,
                vec![id_prop(), text_prop("name")],
                vec![nullable_link("org", &org_q, vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTargetIfOrphan }])],
                vec![],
            ),
            // Multilink on_delete variants.
            ty(
                "ProductRestrictDefault",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![ml("tags", &tag_q, vec![])], // unspecified -> Restrict
            ),
            ty(
                "ProductAllow",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![ml("tags", &tag_q, vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::Allow }])],
            ),
            ty(
                "ProductCascadeOnTargetDelete",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![ml("tags", &tag_q, vec![OnDeletePolicy { side: DeleteSide::Target, action: DeleteAction::DeleteSource }])],
            ),
            ty(
                "ProductDeleteTarget",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![ml("tags", &tag_q, vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTarget }])],
            ),
            ty(
                "ProductDeleteTargetIfOrphan",
                module,
                vec![id_prop(), text_prop("name")],
                vec![],
                vec![ml("tags", &tag_q, vec![OnDeletePolicy { side: DeleteSide::Source, action: DeleteAction::DeleteTargetIfOrphan }])],
            ),
        ],
        ..Default::default()
    }
}

async fn setup() -> (String, SchemaDescriptor, pylon_pgcon::PgPool) {
    let module = unique_module("live_on_delete");
    let schema = on_delete_schema(&module);
    let ddl = export_schema(&schema).unwrap();
    let pool = test_pool().await;
    pool.batch_execute(&ddl).await.unwrap();
    (module, schema, pool)
}

async fn exec(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.execute_typed(&compiled.sql, &[]).await.unwrap();
}

/// Compiles and runs a `select`, returning the raw decoded rows for direct
/// inspection (e.g. checking a multilink shape's array length to confirm
/// junction-row cleanup, without needing to query the junction table
/// directly via raw SQL).
async fn rows_of(pool: &pylon_pgcon::PgPool, schema: &SchemaDescriptor, pyql: &str) -> Vec<CachedValue> {
    let compiled = query::compile(pyql, schema).unwrap();
    pool.query_typed(&compiled.sql, &[], &ExtensionOids::default()).await.unwrap()
}

// ── Single-link on_delete policies ──────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn single_link_default_restrict_blocks_delete_while_referenced() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Acme' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::TeamRestrictDefault {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
    ).await;

    let delete = query::compile(&format!("delete {module}::Org filter .name = 'Acme'"), &schema).unwrap();
    let result = pool.execute_typed(&delete.sql, &[]).await;
    assert!(result.is_err(), "deleting an Org still referenced by a Team (default Restrict) should fail");

    let orgs = rows_of(&pool, &schema, &format!("select {module}::Org filter .name = 'Acme'")).await;
    assert_eq!(orgs.len(), 1, "the Org must still exist after the restricted delete failed");
}

#[tokio::test]
#[ignore]
async fn single_link_allow_sets_the_fk_null_on_target_delete() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Acme' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::TeamSetNull {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::Org filter .name = 'Acme'")).await;

    let teams = rows_of(&pool, &schema, &format!("select {module}::TeamSetNull {{ name, org: {{ name }} }} filter .name = 'Alpha'")).await;
    assert_eq!(teams.len(), 1, "the Team itself must survive an Allow (SET NULL) target delete");
    let CachedValue::Composite(fields) = &teams[0] else { panic!("expected Composite, got {:?}", teams[0]) };
    // [0] = __type__, [1] = name, [2] = org (nested Composite, or Null since the FK is now NULL)
    assert_eq!(fields.get(2), Some(&CachedValue::Null), "org link should have been set NULL, got {:?}", fields.get(2));
}

#[tokio::test]
#[ignore]
async fn single_link_target_delete_source_cascades_to_the_team() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Acme' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::TeamCascadeOnTargetDelete {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::Org filter .name = 'Acme'")).await;

    let teams = rows_of(&pool, &schema, &format!("select {module}::TeamCascadeOnTargetDelete filter .name = 'Alpha'")).await;
    assert_eq!(teams.len(), 0, "DeleteSource (Target-side) must cascade-delete the Team when its Org is deleted");
}

#[tokio::test]
#[ignore]
async fn single_link_source_delete_target_always_deletes_the_org() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Acme' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::TeamDeleteTarget {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Acme') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::TeamDeleteTarget filter .name = 'Alpha'")).await;

    let orgs = rows_of(&pool, &schema, &format!("select {module}::Org filter .name = 'Acme'")).await;
    assert_eq!(orgs.len(), 0, "DeleteTarget (Source-side) must delete the Org when the Team is deleted, regardless of other references");
}

#[tokio::test]
#[ignore]
async fn single_link_source_delete_target_if_orphan_respects_other_references() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Shared' }}")).await;
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'SoleOwned' }}")).await;
    for stmt in [
        format!("insert {module}::TeamDeleteTargetIfOrphan {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Shared') }}"),
        format!("insert {module}::TeamDeleteTargetIfOrphan {{ name := 'Beta', org := (select {module}::Org filter .name = 'Shared') }}"),
        format!("insert {module}::TeamDeleteTargetIfOrphan {{ name := 'Gamma', org := (select {module}::Org filter .name = 'SoleOwned') }}"),
    ] {
        exec(&pool, &schema, &stmt).await;
    }

    // Alpha shares its Org with Beta — deleting Alpha must NOT delete the Org.
    exec(&pool, &schema, &format!("delete {module}::TeamDeleteTargetIfOrphan filter .name = 'Alpha'")).await;
    let shared = rows_of(&pool, &schema, &format!("select {module}::Org filter .name = 'Shared'")).await;
    assert_eq!(shared.len(), 1, "Org still referenced by Beta must survive deleting Alpha");

    // Gamma is the sole owner of SoleOwned — deleting it must delete the Org too.
    exec(&pool, &schema, &format!("delete {module}::TeamDeleteTargetIfOrphan filter .name = 'Gamma'")).await;
    let sole_owned = rows_of(&pool, &schema, &format!("select {module}::Org filter .name = 'SoleOwned'")).await;
    assert_eq!(sole_owned.len(), 0, "Org with no remaining references must be deleted when its sole-owning Team is deleted");
}

// ── Junction-table cleanup and multilink on_delete policies ────────────────────

/// Addresses the original suspicion that prompted this whole test group:
/// deleting the owner of a multilink must always clean up its junction
/// rows, regardless of any `on_delete` policy — `source_jt_fk_suffix` is
/// hardcoded to `ON DELETE CASCADE` and isn't configurable.
#[tokio::test]
#[ignore]
async fn deleting_the_owner_always_cleans_up_its_own_junction_rows() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductRestrictDefault {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::ProductRestrictDefault filter .name = 'Widget'")).await;

    // The Product is gone, and — the actual point of this test — the Tag
    // it referenced must survive untouched (only the junction row, not the
    // Tag, should have been cleaned up).
    let products = rows_of(&pool, &schema, &format!("select {module}::ProductRestrictDefault filter .name = 'Widget'")).await;
    assert_eq!(products.len(), 0);
    let tags = rows_of(&pool, &schema, &format!("select {module}::Tag filter .name = 'red'")).await;
    assert_eq!(tags.len(), 1, "the Tag must survive its owning Product being deleted");
}

#[tokio::test]
#[ignore]
async fn multilink_default_restrict_blocks_target_delete_while_referenced() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductRestrictDefault {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;

    let delete = query::compile(&format!("delete {module}::Tag filter .name = 'red'"), &schema).unwrap();
    let result = pool.execute_typed(&delete.sql, &[]).await;
    assert!(result.is_err(), "deleting a Tag still referenced by a Product (default Restrict) should fail");

    let tags = rows_of(&pool, &schema, &format!("select {module}::Tag filter .name = 'red'")).await;
    assert_eq!(tags.len(), 1);
}

#[tokio::test]
#[ignore]
async fn multilink_allow_removes_the_junction_row_on_target_delete() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductAllow {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::Tag filter .name = 'red'")).await;

    let products = rows_of(&pool, &schema, &format!("select {module}::ProductAllow {{ name, tags: {{ name }} }} filter .name = 'Widget'")).await;
    assert_eq!(products.len(), 1, "the Product itself must survive an Allow target delete");
    let CachedValue::Composite(fields) = &products[0] else { panic!("expected Composite, got {:?}", products[0]) };
    let CachedValue::Array(tags) = &fields[2] else { panic!("expected tags to decode as an Array, got {:?}", fields[2]) };
    assert_eq!(tags.len(), 0, "the junction row must be gone, leaving an empty tags array");
}

#[tokio::test]
#[ignore]
async fn multilink_target_delete_source_cascades_to_the_product() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductCascadeOnTargetDelete {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::Tag filter .name = 'red'")).await;

    let products = rows_of(&pool, &schema, &format!("select {module}::ProductCascadeOnTargetDelete filter .name = 'Widget'")).await;
    assert_eq!(products.len(), 0, "DeleteSource (Target-side) must cascade-delete the Product when a referenced Tag is deleted");
}

#[tokio::test]
#[ignore]
async fn multilink_source_delete_target_always_deletes_the_tag() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductDeleteTarget {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;

    exec(&pool, &schema, &format!("delete {module}::ProductDeleteTarget filter .name = 'Widget'")).await;

    let tags = rows_of(&pool, &schema, &format!("select {module}::Tag filter .name = 'red'")).await;
    assert_eq!(tags.len(), 0, "DeleteTarget (Source-side) must delete the Tag when the Product is deleted, regardless of other references");
}

#[tokio::test]
#[ignore]
async fn multilink_source_delete_target_if_orphan_respects_other_references() {
    let (module, schema, pool) = setup().await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'shared' }}")).await;
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'sole' }}")).await;
    for stmt in [
        format!("insert {module}::ProductDeleteTargetIfOrphan {{ name := 'Alpha', tags := (select {module}::Tag filter .name = 'shared') }}"),
        format!("insert {module}::ProductDeleteTargetIfOrphan {{ name := 'Beta', tags := (select {module}::Tag filter .name = 'shared') }}"),
        format!("insert {module}::ProductDeleteTargetIfOrphan {{ name := 'Gamma', tags := (select {module}::Tag filter .name = 'sole') }}"),
    ] {
        exec(&pool, &schema, &stmt).await;
    }

    // Alpha shares its Tag with Beta — deleting Alpha must NOT delete the Tag.
    exec(&pool, &schema, &format!("delete {module}::ProductDeleteTargetIfOrphan filter .name = 'Alpha'")).await;
    let shared = rows_of(&pool, &schema, &format!("select {module}::Tag filter .name = 'shared'")).await;
    assert_eq!(shared.len(), 1, "Tag still referenced by Beta must survive deleting Alpha");

    // Gamma is the sole user of 'sole' — deleting it must delete the Tag too.
    exec(&pool, &schema, &format!("delete {module}::ProductDeleteTargetIfOrphan filter .name = 'Gamma'")).await;
    let sole = rows_of(&pool, &schema, &format!("select {module}::Tag filter .name = 'sole'")).await;
    assert_eq!(sole.len(), 0, "Tag with no remaining references must be deleted when its sole-using Product is deleted");
}

// ── Migration (diff) path parity ────────────────────────────────────────────────

/// Regression test for the gap this whole test group's investigation
/// surfaced: `diff/mod.rs` (the incremental-migration DDL path used by
/// `pylon migration create`/`watch`) never emitted deletion-policy triggers
/// at all — only `export_schema`'s fresh-install path did. Builds the exact
/// same schema via `diff_schema(target, &DbState::default())` (a
/// from-scratch migration against an empty database, not `export_schema`)
/// and re-runs one single-link and one multilink on_delete scenario against
/// it, proving the two DDL-generation paths now agree.
#[tokio::test]
#[ignore]
async fn migration_path_emits_working_deletion_policy_triggers() {
    let module = unique_module("live_on_delete_migration");
    let schema = on_delete_schema(&module);
    let ddl_ops = diff_schema(&schema, &DbState::default()).unwrap();
    assert!(
        ddl_ops.iter().any(|op| op.contains("AFTER DELETE") || op.contains("BEFORE DELETE")),
        "expected at least one deletion-policy trigger in the migration-path DDL"
    );

    let pool = test_pool().await;
    for op in &ddl_ops {
        pool.batch_execute(op).await.unwrap();
    }

    // Single-link DeleteTargetIfOrphan (Source-side) — same scenario as
    // `single_link_source_delete_target_if_orphan_respects_other_references`.
    exec(&pool, &schema, &format!("insert {module}::Org {{ name := 'Shared' }}")).await;
    for stmt in [
        format!("insert {module}::TeamDeleteTargetIfOrphan {{ name := 'Alpha', org := (select {module}::Org filter .name = 'Shared') }}"),
        format!("insert {module}::TeamDeleteTargetIfOrphan {{ name := 'Beta', org := (select {module}::Org filter .name = 'Shared') }}"),
    ] {
        exec(&pool, &schema, &stmt).await;
    }
    exec(&pool, &schema, &format!("delete {module}::TeamDeleteTargetIfOrphan filter .name = 'Alpha'")).await;
    let orgs = rows_of(&pool, &schema, &format!("select {module}::Org filter .name = 'Shared'")).await;
    assert_eq!(orgs.len(), 1, "Org still referenced by Beta must survive deleting Alpha (migration-built schema)");

    // Multilink target-side DeleteSource — same scenario as
    // `multilink_target_delete_source_cascades_to_the_product`.
    exec(&pool, &schema, &format!("insert {module}::Tag {{ name := 'red' }}")).await;
    exec(
        &pool, &schema,
        &format!("insert {module}::ProductCascadeOnTargetDelete {{ name := 'Widget', tags := (select {module}::Tag filter .name = 'red') }}"),
    ).await;
    exec(&pool, &schema, &format!("delete {module}::Tag filter .name = 'red'")).await;
    let products = rows_of(&pool, &schema, &format!("select {module}::ProductCascadeOnTargetDelete filter .name = 'Widget'")).await;
    assert_eq!(products.len(), 0, "DeleteSource (Target-side) must cascade-delete the Product (migration-built schema)");
}
