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

//! Search/vector index amendments, and backfill of the deferred ones.
//!
//! The main diff works from `DbState` — what Postgres introspection can
//! see — which is enough to *create* an index's physical footprint and
//! almost nothing else:
//!
//! * A deferred search index (Meilisearch/OpenSearch) has no footprint at
//!   all: no column, no catalog object, nothing. Introspection can never
//!   tell that one was added, let alone that its pointer list changed.
//! * A `Postgres`-backed search index's tsvector expression isn't
//!   introspected either — `introspect::COLUMNS_SQL` records a DEFAULT only
//!   for a *non*-generated column — so a changed pointer or weight is
//!   invisible, and `diff_inner`'s Phase 9 skips the whole index the moment
//!   its column exists.
//! * A vector index's `metric` lives in its hnsw operator class, which
//!   `schema_to_db_state` doesn't model (`DbIndex::key` is `None` for these,
//!   and the generic index pass skips them for exactly that reason), and its
//!   `dimensions` lives in a `vector(n)` typmod on a column no type-change
//!   pass looks at (that pass walks `td.properties`, and an index column is
//!   not a property).
//! * A plain `Index` *does* have an introspectable footprint, but the main
//!   diff only ever asks what Postgres is missing — it has no drop pass at
//!   all, and settles an index with a key by name alone. So a declaration
//!   that was deleted left its index alive in the database with nothing in
//!   the schema referring to it, and one whose body changed under an
//!   unchanged name was read as already there.
//! * A composite `Exclusive`'s `unless` is a partial unique index's
//!   `WHERE NOT (…)`, which `introspect` reports as Postgres's own
//!   normalization of the predicate rather than as the string Pylon
//!   compiled — so the main diff can only ask whether a predicate is
//!   *present*, and an edited one reads as unchanged.
//!
//! So everything here diffs against the *previous `SchemaDescriptor`* —
//! `_pylon."Migrations".schema_state` on the applied tip row, the same
//! baseline `schema_content_changed` already uses — rather than against
//! `DbState`. `previous = None` emits nothing: that is either a fresh
//! install, where the create path covers it and no type holds an object
//! yet, or a database predating `schema_state`, where "what changed" is
//! unknowable and guessing "everything" would re-embed every object of
//! every type on the next migration. `pylon index rebuild` is the
//! deliberate way in for that case.

use serde_json::Value;

use super::{DiffOp, qi, qn};
use crate::schema::{
    IndexDescriptor, SchemaDescriptor, SearchBackend, SearchIndexDescriptor, TypeConstraint, TypeDescriptor,
};

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ── Shared naming and expressions ────────────────────────────────────────────
//
// Every index Pylon names, it names here — the create path, the drop path,
// `schema_to_db_state`'s baseline and `export_schema`'s DDL dump all read one
// of the four functions below. Two paths naming the same index differently
// is not a cosmetic problem: the diff settles an index with a key by *name*,
// so a second opinion reads as a second index and gets created alongside the
// first. (`export_schema` used to leave plain indexes unnamed, letting
// Postgres auto-name them `t_col_idx` — so the first `migration create`
// against an exported-and-applied database proposed `t__col_idx` on top.)
//
// Each returns the *final* name, capped: Postgres truncates an identifier
// past 63 bytes, which is a third opinion again for a long one.

/// The hnsw index name for one vector index on `table`.
pub(crate) fn vector_index_name(table: &str, index_name: Option<&str>) -> String {
    super::capped_index_name(match index_name {
        None => format!("{table}__vector__"),
        Some(n) => format!("{table}__vector_{n}__"),
    })
}

/// The GIN index name for one `Postgres`-backed search index on `table`.
pub(crate) fn search_index_name(table: &str, index_name: Option<&str>) -> String {
    super::capped_index_name(match index_name {
        None => format!("{table}__search__"),
        Some(n) => format!("{table}__search_{n}__"),
    })
}

/// The name of one composite `Exclusive` on `table`. Two of them may share a
/// pointer list (differing only in their `unless`), so the constraint's
/// position in the type's declaration list goes in the name as well.
pub(crate) fn exclusive_index_name(table: &str, pointers: &[String], position: usize) -> String {
    super::capped_index_name(format!("{table}_{}_{position}_key", pointers.join("_")))
}

/// The name of one plain `Index` on `table`. An expression index has no
/// pointer names to build one from, so its position in the type's declaration
/// list stands in for them.
pub(crate) fn plain_index_name(table: &str, idx: &IndexDescriptor, position: usize) -> String {
    super::capped_index_name(match idx.expression {
        Some(_) => format!("{table}__expr{position}_idx"),
        None => format!("{table}__{}_idx", idx.pointers.join("_")),
    })
}

/// The generation expression of a `Postgres`-backed search index's tsvector
/// column. Shared with `diff_inner`'s search pass for the same reason as
/// the names above: an amendment has to re-add the column with byte-for-byte
/// the expression a fresh install would have given it, or the two diverge
/// the first time anything else is compared.
pub(crate) fn tsvector_expr(si: &SearchIndexDescriptor) -> String {
    si.pointers
        .iter()
        .map(|sf| {
            format!(
                "setweight(to_tsvector('english', coalesce({}, '')), '{}')",
                qi(&sf.name),
                sf.weight.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(" || ")
}

// ── Deferred index identity ──────────────────────────────────────────────────

/// A deferred index's identity in `_pylon."IndexOutbox"` — everything both
/// the backfill enqueue and `pylon index rebuild` need in order to name one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredIndex {
    /// `_pylon."IndexOutbox".type_name`, i.e. `"module::Type"`.
    pub type_name: String,
    pub module: String,
    pub table: String,
    /// The `_pylon."IndexKind"` label: `"Vector"`, `"OpenSearch"`, or
    /// `"Meilisearch"`.
    pub index_kind: String,
    /// `None` is the type's default (unnamed) index, stored as SQL NULL —
    /// which the outbox's `UNIQUE NULLS NOT DISTINCT` treats as a value
    /// rather than as "no constraint".
    pub index_name: Option<String>,
}

/// The `_pylon."IndexKind"` label a search backend's jobs carry, or `None`
/// for a backend that has no queue: a `Postgres`-backed index is a generated
/// tsvector column Postgres itself keeps current, so there is no worker to
/// hand anything to and nothing to backfill.
fn search_index_kind(backend: &SearchBackend) -> Option<&'static str> {
    match backend {
        SearchBackend::OpenSearch => Some("OpenSearch"),
        SearchBackend::Meilisearch => Some("Meilisearch"),
        SearchBackend::Postgres => None,
    }
}

/// Every deferred (worker-driven) index `schema` declares, in schema order.
///
/// Abstract and interface types are skipped: they have no table of their own
/// to read objects out of, matching which types `diff_inner`'s vector and
/// search passes emit a footprint for at all.
pub fn deferred_indexes(schema: &SchemaDescriptor) -> Vec<DeferredIndex> {
    let mut out = Vec::new();
    for td in &schema.types {
        if td.abstract_ {
            continue;
        }
        let type_name = format!("{}::{}", td.module, td.name);
        let at = |kind: &str, index_name: Option<String>| DeferredIndex {
            type_name: type_name.clone(),
            module: td.module.clone(),
            table: td.table.clone(),
            index_kind: kind.to_string(),
            index_name,
        };
        for vi in &td.vector_indexes {
            out.push(at("Vector", vi.index_name.clone()));
        }
        for si in &td.search_indexes {
            if let Some(kind) = search_index_kind(&si.backend) {
                out.push(at(kind, si.index_name.clone()));
            }
        }
    }
    out
}

/// The outbox enqueue that rebuilds one deferred index from the type it
/// indexes: one `Pending` job per object already there, which the index
/// worker then drains exactly as it drains a job some write enqueued.
///
/// `ON CONFLICT ... DO UPDATE` matches the write path's own enqueue CTE
/// (`sql::enqueue_search_cte_sql`), so a backfill coalesces with whatever
/// job is already outstanding for an object instead of failing on the
/// outbox's `UNIQUE NULLS NOT DISTINCT (object_id, index_kind, index_name)`.
/// Unlike that one it also resets `attempts`/`next_attempt`: a rebuild is a
/// deliberate fresh start, so a job that had already burned through its
/// retries should be tried again now rather than stay parked as `Failed`.
pub fn index_backfill_sql(idx: &DeferredIndex) -> String {
    format!(
        concat!(
            "INSERT INTO _pylon.\"IndexOutbox\"\n",
            "    (object_id, type_name, index_kind, index_name, operation)\n",
            "SELECT \"id\", {}, {}::_pylon.\"IndexKind\", {}, 'index'\n",
            "FROM {}\n",
            "ON CONFLICT (object_id, index_kind, index_name)\n",
            "DO UPDATE SET status = 'Pending', operation = EXCLUDED.operation,\n",
            "              attempts = 0, next_attempt = NULL, enqueued_at = now();",
        ),
        sql_str(&idx.type_name),
        sql_str(&idx.index_kind),
        idx.index_name
            .as_deref()
            .map(sql_str)
            .unwrap_or_else(|| "NULL".to_string()),
        qn(&idx.module, &idx.table),
    )
}

// ── Change detection ─────────────────────────────────────────────────────────

/// Fields of a `VectorIndexDescriptor` whose change leaves the embeddings
/// already stored for it valid.
///
/// `metric` only picks the hnsw operator class — it decides which index
/// Postgres builds *over* the vectors, not what the vectors are. Re-embedding
/// on a metric change would bill the embedding provider for every object of
/// the type to arrive at byte-identical results.
///
/// Everything *not* listed here counts as invalidating, so a field added to
/// the descriptor later triggers a rebuild by default instead of being
/// silently ignored — the same bias `schema_content_changed` takes, for the
/// same reason.
const VECTOR_EMBEDDING_IRRELEVANT: &[&str] = &["metric"];

fn find_type<'a>(schema: &'a SchemaDescriptor, type_name: &str) -> Option<&'a TypeDescriptor> {
    schema
        .types
        .iter()
        .find(|t| format!("{}::{}", t.module, t.name) == type_name)
}

fn without_keys(mut value: Value, keys: &[&str]) -> Value {
    if let Some(map) = value.as_object_mut() {
        for k in keys {
            map.remove(*k);
        }
    }
    value
}

/// The part of a deferred index's declaration that decides whether what is
/// already stored for it is still valid — `None` when `schema` doesn't
/// declare that index at all.
fn stored_content_fingerprint(schema: &SchemaDescriptor, idx: &DeferredIndex) -> Option<Value> {
    let td = find_type(schema, &idx.type_name)?;
    if idx.index_kind == "Vector" {
        let vi = td.vector_indexes.iter().find(|v| v.index_name == idx.index_name)?;
        Some(without_keys(
            serde_json::to_value(vi).ok()?,
            VECTOR_EMBEDDING_IRRELEVANT,
        ))
    } else {
        // Matching the backend too (not just the name) is what makes a
        // backend flip register: an index that moved Postgres → Meilisearch,
        // or Meilisearch → OpenSearch, has nothing stored under its new
        // kind and so reads here as brand new.
        let si = td.search_indexes.iter().find(|s| {
            s.index_name == idx.index_name && search_index_kind(&s.backend) == Some(idx.index_kind.as_str())
        })?;
        serde_json::to_value(si).ok()
    }
}

/// Deferred indexes whose whole contents need rebuilding: every one `target`
/// declares that `previous` either didn't, or declared differently enough
/// that the documents/embeddings already stored for it are stale.
///
/// This is what makes adding a search or vector index to a populated type
/// self-sufficient — without it the index exists but holds only objects
/// written *after* the migration, and someone has to remember to go rebuild
/// it by hand.
pub fn changed_deferred_indexes(target: &SchemaDescriptor, previous: Option<&SchemaDescriptor>) -> Vec<DeferredIndex> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    deferred_indexes(target)
        .into_iter()
        .filter(|idx| {
            let before = stored_content_fingerprint(previous, idx);
            before.is_none() || before != stored_content_fingerprint(target, idx)
        })
        .collect()
}

/// `index_backfill_sql` for every index `changed_deferred_indexes` reports.
pub fn changed_index_backfill_ops(target: &SchemaDescriptor, previous: Option<&SchemaDescriptor>) -> Vec<DiffOp> {
    changed_deferred_indexes(target, previous)
        .iter()
        .map(|idx| DiffOp {
            sql: index_backfill_sql(idx),
            non_transactional: false,
        })
        .collect()
}

// ── Footprint amendments ─────────────────────────────────────────────────────

/// What Postgres ends up holding for one plain `Index`: whether it is
/// UNIQUE, its key, and its partial predicate.
type PlainIndexShape = (bool, String, String);

/// One type's plain `Index` declarations as `(name, shape)` — the name the
/// database holds each under, paired with the shape of the index itself, so
/// two revisions of a declaration can be compared by what Postgres would
/// actually have built from each.
///
/// `None` for a declaration whose key doesn't compile, matching
/// `schema_to_db_state`, which skips those: an index the create path never
/// emitted is not one to drop or amend either.
fn plain_indexes(schema: &SchemaDescriptor, td: &TypeDescriptor) -> Vec<(String, Option<PlainIndexShape>)> {
    td.indexes
        .iter()
        .enumerate()
        .map(|(i, idx)| {
            let shape = crate::export::index_body_and_predicate(
                td,
                &idx.pointers,
                idx.expression.as_deref(),
                idx.unless.as_deref(),
                schema,
            )
            .ok()
            .map(|(body, predicate)| (idx.unique, body, predicate));
            (plain_index_name(&td.table, idx, i), shape)
        })
        .collect()
}

/// One composite `Exclusive`: the pointer list that identifies it across
/// revisions, the name the database holds its index under, and the `(key,
/// predicate)` Postgres would have built from it.
type ExclusiveIndex = (Vec<String>, String, Option<(String, String)>);

/// One type's composite `Exclusive` declarations as `(pointers, name, shape)`
/// — the pointer list that identifies one across revisions, the name the
/// database holds it under, and the key and predicate Postgres would have
/// built from it.
///
/// Paired across revisions by the *pointers*, not the name or the position:
/// those are what `schema_to_db_state` settles the index's existence by, and
/// an `Exclusive` removed from in front of another would otherwise renumber
/// it into a stranger.
///
/// `None` for a declaration whose `unless` doesn't compile, matching
/// `emit_unique_indexes` and `schema_to_db_state`, which both skip those
/// rather than emit an index missing the predicate that narrows it.
fn exclusive_indexes(schema: &SchemaDescriptor, td: &TypeDescriptor) -> Vec<ExclusiveIndex> {
    if td.junction {
        return Vec::new();
    }
    td.constraints
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match c {
            TypeConstraint::Exclusive { pointers, unless } => {
                let shape = crate::export::exclusive_body_and_predicate(td, pointers, unless.as_deref(), schema).ok();
                Some((pointers.clone(), exclusive_index_name(&td.table, pointers, i), shape))
            }
            TypeConstraint::Expression { .. } => None,
        })
        .collect()
}

fn tx(sql: String) -> DiffOp {
    DiffOp {
        sql,
        non_transactional: false,
    }
}

/// DDL bringing an index's Postgres footprint back in line with its
/// declaration, covering the changes the main diff does not see (see this
/// module's own header for why it does not see them).
///
/// Six cases, all keyed off `previous` rather than `DbState`:
///
/// * A plain `Index` that went away, or whose key, uniqueness or predicate
///   changed under the name the database already holds it under. Dropped,
///   and re-made from the new declaration in the second case — the create
///   pass would read the stale index as the declared one.
///
/// * A composite `Exclusive` whose `unless` changed over unchanged
///   pointers. The main diff asks of a partial index only whether it *has* a
///   predicate, so the index kept the one it was built with and went on
///   enforcing the old rule.
///
/// * A `Postgres`-backed search index whose pointers or weights changed.
///   Its column is `GENERATED ALWAYS AS (…) STORED` and Postgres offers no
///   way to alter a generation expression, so the column is dropped and
///   re-added — which recomputes it for every object the type already holds
///   — and its GIN index rebuilt.
/// * A vector index whose `dimensions` changed. `vector(n)` is a typmod and
///   every stored embedding is the wrong width for the new one, so the
///   column is dropped and re-added empty. What refills it is the backfill
///   enqueue from `changed_deferred_indexes`, since `dimensions` counts as
///   embedding-relevant there.
/// * A vector index whose `metric` changed. The embeddings stay valid and
///   the column is untouched; only the hnsw operator class is wrong, so just
///   that index is rebuilt.
/// * An index that went away — removed from the type, or moved off the
///   `Postgres` backend. Its column is left behind by the main diff forever
///   (the drop-column pass skips every `__…__` name by design, since that is
///   also how it avoids dropping the columns it doesn't manage), and a
///   generated one keeps being recomputed on every write.
///
/// `for_migration` selects `CONCURRENTLY` for index work, same contract as
/// `diff_schema_ops` — the table is never new here (an amendment means the
/// index already had a footprint), so there is no fresh-table case to
/// exempt.
pub fn index_amendment_ops(
    target: &SchemaDescriptor,
    previous: Option<&SchemaDescriptor>,
    for_migration: bool,
) -> Vec<DiffOp> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let concurrently = if for_migration { "CONCURRENTLY " } else { "" };
    let mut ops = Vec::new();

    for td in &target.types {
        if td.abstract_ {
            continue;
        }
        let type_name = format!("{}::{}", td.module, td.name);
        let Some(prev_td) = find_type(previous, &type_name) else {
            continue;
        };
        let table = qn(&td.module, &td.table);

        let drop_index = |name: &str| DiffOp {
            sql: format!(
                "DROP INDEX {concurrently}IF EXISTS {};",
                qn(&td.module, &super::capped_index_name(name.to_string()))
            ),
            non_transactional: for_migration,
        };

        // ── Postgres-backed search indexes ───────────────────────────────
        for si in td
            .search_indexes
            .iter()
            .filter(|s| s.backend == SearchBackend::Postgres)
        {
            let Some(prev_si) = prev_td
                .search_indexes
                .iter()
                .find(|s| s.index_name == si.index_name && s.backend == SearchBackend::Postgres)
            else {
                // Either brand new or newly moved onto Postgres: no column
                // exists yet, so the main diff's own pass creates it.
                continue;
            };
            if serde_json::to_value(prev_si).ok() == serde_json::to_value(si).ok() {
                continue;
            }
            let col = si.column_name();
            let idx_name = search_index_name(&td.table, si.index_name.as_deref());
            // Dropping the column takes its GIN index with it, so the
            // explicit drop is only for the index that outlives a column
            // Postgres would otherwise keep — here, none. The two statements
            // below have to commit together: landing the drop without the
            // re-add would leave the type with no search column at all.
            ops.push(tx(format!("ALTER TABLE {table} DROP COLUMN IF EXISTS {};", qi(&col))));
            ops.push(tx(format!(
                "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED;",
                qi(&col),
                tsvector_expr(si)
            )));
            ops.push(DiffOp {
                sql: format!(
                    "CREATE INDEX {concurrently}IF NOT EXISTS {} ON {table} USING gin ({});",
                    qi(&super::capped_index_name(idx_name)),
                    qi(&col)
                ),
                non_transactional: for_migration,
            });
        }

        // A search index that is gone, or no longer Postgres-backed, still
        // has its generated column sitting on the table.
        for prev_si in prev_td
            .search_indexes
            .iter()
            .filter(|s| s.backend == SearchBackend::Postgres)
        {
            let still_there = td
                .search_indexes
                .iter()
                .any(|s| s.index_name == prev_si.index_name && s.backend == SearchBackend::Postgres);
            if still_there {
                continue;
            }
            ops.push(tx(format!(
                "ALTER TABLE {table} DROP COLUMN IF EXISTS {};",
                qi(&prev_si.column_name())
            )));
        }

        // ── Plain indexes ────────────────────────────────────────────────
        // Keyed by the generated name, because that is what the database
        // carries. A declaration whose name is gone leaves its index behind
        // forever otherwise: the main diff only ever asks what is *missing*
        // from Postgres. One whose name survives a changed body is dropped
        // and re-made here too, since that pass settles an index with a key
        // by name alone and so reads the stale one as already there.
        let prev_plain = plain_indexes(previous, prev_td);
        let now_plain = plain_indexes(target, td);
        for (name, _) in &prev_plain {
            if !now_plain.iter().any(|(n, _)| n == name) {
                ops.push(drop_index(name));
            }
        }
        for (name, now) in &now_plain {
            let Some((_, before)) = prev_plain.iter().find(|(n, _)| n == name) else {
                // Nothing of this name before, so no footprint to amend: the
                // main diff's own pass creates it.
                continue;
            };
            let (Some(now), Some(before)) = (now, before) else {
                continue;
            };
            if before == now {
                continue;
            }
            let (unique, body, predicate) = now;
            ops.push(drop_index(name));
            ops.push(DiffOp {
                sql: format!(
                    "CREATE {}INDEX {concurrently}IF NOT EXISTS {} ON {table} {body}{predicate};",
                    if *unique { "UNIQUE " } else { "" },
                    qi(&super::capped_index_name(name.clone())),
                ),
                non_transactional: for_migration,
            });
        }

        // ── Composite exclusives ─────────────────────────────────────────
        // A `pylon.Exclusive(("account", "package"), unless="exists(.ended_at)")`
        // is a partial unique index, and its `WHERE NOT (<unless>)` is the
        // whole of what makes the constraint narrower than the pointers
        // alone. The main diff settles one of these by its columns and
        // uniqueness — Postgres is free to have auto-named it, so the name is
        // no guide — and asked of the predicate only whether there *was* one.
        // So a changed `unless` over unchanged pointers read as the index
        // already being there: nothing was proposed, and the index went on
        // enforcing the predicate it was built with.
        //
        // An exclusive that is gone, or one that is new, needs nothing here:
        // the drop is the main diff's business (it has the table's real
        // indexes to work from) and the create is the pass that already
        // covers a missing one.
        let prev_exclusives = exclusive_indexes(previous, prev_td);
        for (pointers, name, now) in exclusive_indexes(target, td) {
            let Some((_, prev_name, before)) = prev_exclusives.iter().find(|(p, _, _)| *p == pointers) else {
                continue;
            };
            let (Some((body, predicate)), Some(before)) = (&now, before) else {
                continue;
            };
            if before == &(body.clone(), predicate.clone()) {
                continue;
            }
            // Dropped under the name the *previous* revision gave it, which
            // is the one the database holds — renumbering can have moved the
            // declaration since.
            ops.push(drop_index(prev_name));
            ops.push(DiffOp {
                sql: format!(
                    "CREATE UNIQUE INDEX {concurrently}IF NOT EXISTS {} ON {table} {body}{predicate};",
                    qi(&super::capped_index_name(name)),
                ),
                non_transactional: for_migration,
            });
        }

        // ── Vector indexes ───────────────────────────────────────────────
        for vi in &td.vector_indexes {
            let Some(prev_vi) = prev_td.vector_indexes.iter().find(|v| v.index_name == vi.index_name) else {
                continue;
            };
            let idx_name = vector_index_name(&td.table, vi.index_name.as_deref());
            let col = vi.column_name();
            if prev_vi.dimensions != vi.dimensions {
                ops.push(tx(format!("ALTER TABLE {table} DROP COLUMN IF EXISTS {};", qi(&col))));
                ops.push(tx(format!(
                    "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {} vector({});",
                    qi(&col),
                    vi.dimensions
                )));
                ops.push(DiffOp {
                    sql: format!(
                        "CREATE INDEX {concurrently}IF NOT EXISTS {} ON {table} USING hnsw ({} {});",
                        qi(&super::capped_index_name(idx_name)),
                        qi(&col),
                        vi.ops_class()
                    ),
                    non_transactional: for_migration,
                });
            } else if prev_vi.metric != vi.metric {
                ops.push(drop_index(&idx_name));
                ops.push(DiffOp {
                    sql: format!(
                        "CREATE INDEX {concurrently}IF NOT EXISTS {} ON {table} USING hnsw ({} {});",
                        qi(&super::capped_index_name(idx_name)),
                        qi(&col),
                        vi.ops_class()
                    ),
                    non_transactional: for_migration,
                });
            }
        }

        for prev_vi in &prev_td.vector_indexes {
            if td.vector_indexes.iter().any(|v| v.index_name == prev_vi.index_name) {
                continue;
            }
            ops.push(drop_index(&vector_index_name(&td.table, prev_vi.index_name.as_deref())));
            ops.push(tx(format!(
                "ALTER TABLE {table} DROP COLUMN IF EXISTS {};",
                qi(&prev_vi.column_name())
            )));
        }
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        PropertyDescriptor, SearchIndexDescriptor, SearchPointerDescriptor, SearchWeight, TypeDescriptor,
        VectorIndexDescriptor,
    };

    /// A bare concrete type in `default`. No properties: nothing in this
    /// module reads them — an index's own pointer names are carried by its
    /// descriptor, and the backfill selects `id` by name.
    fn td(name: &str) -> TypeDescriptor {
        TypeDescriptor {
            name: name.into(),
            module: "default".into(),
            table: name.into(),
            abstract_: false,
            materialized: false,
            description: None,
            parents: vec![],
            interfaces: vec![],
            bases: vec![],
            properties: vec![],
            links: vec![],
            multilinks: vec![],
            computed: vec![],
            constraints: vec![],
            indexes: vec![],
            partition: None,
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![],
        }
    }

    fn schema(types: Vec<TypeDescriptor>) -> SchemaDescriptor {
        SchemaDescriptor {
            types,
            ..Default::default()
        }
    }

    fn vector(index_name: Option<&str>, pointers: &[&str], dimensions: u32, metric: &str) -> VectorIndexDescriptor {
        VectorIndexDescriptor {
            index_name: index_name.map(str::to_string),
            pointers: pointers.iter().map(|p| p.to_string()).collect(),
            model: "mistral-embed".into(),
            metric: metric.into(),
            dimensions,
        }
    }

    fn search(index_name: Option<&str>, backend: SearchBackend, pointers: &[&str]) -> SearchIndexDescriptor {
        SearchIndexDescriptor {
            index_name: index_name.map(str::to_string),
            backend,
            pointers: pointers
                .iter()
                .map(|p| SearchPointerDescriptor {
                    name: p.to_string(),
                    weight: SearchWeight::A,
                })
                .collect(),
        }
    }

    fn joined(ops: &[DiffOp]) -> String {
        ops.iter().map(|o| o.sql.as_str()).collect::<Vec<_>>().join("\n")
    }

    // ── deferred index enumeration ───────────────────────────────────────

    #[test]
    fn a_postgres_backed_search_index_is_not_a_deferred_index() {
        let mut t = td("Product");
        t.search_indexes = vec![
            search(None, SearchBackend::Postgres, &["name"]),
            search(Some("fuzzy"), SearchBackend::Meilisearch, &["name"]),
        ];
        let found = deferred_indexes(&schema(vec![t]));
        assert_eq!(found.len(), 1, "got: {found:?}");
        assert_eq!(found[0].index_kind, "Meilisearch");
        assert_eq!(found[0].index_name.as_deref(), Some("fuzzy"));
    }

    #[test]
    fn an_abstract_type_contributes_no_deferred_index() {
        let mut t = td("Searchable");
        t.abstract_ = true;
        t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        assert!(deferred_indexes(&schema(vec![t])).is_empty());
    }

    // ── backfill detection ───────────────────────────────────────────────

    #[test]
    fn a_new_deferred_search_index_is_backfilled() {
        let before = schema(vec![td("Product")]);
        let mut t = td("Product");
        t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        let after = schema(vec![t]);

        let changed = changed_deferred_indexes(&after, Some(&before));
        assert_eq!(changed.len(), 1, "got: {changed:?}");
        let sql = index_backfill_sql(&changed[0]);
        assert!(
            sql.contains(r#"'Meilisearch'::_pylon."IndexKind""#) && sql.contains(r#"FROM "public"."Product""#),
            "got: {sql}"
        );
    }

    #[test]
    fn an_unchanged_deferred_index_is_not_backfilled() {
        let mut t = td("Product");
        t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        let before = schema(vec![t.clone()]);
        assert!(changed_deferred_indexes(&schema(vec![t]), Some(&before)).is_empty());
    }

    #[test]
    fn adding_a_pointer_to_a_deferred_search_index_backfills_it() {
        let mut before_t = td("Product");
        before_t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        let mut after_t = td("Product");
        after_t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name", "description"])];

        // The worker splits one `source_text` by pointer order into the
        // document's fields, so an added pointer changes every document.
        assert_eq!(
            changed_deferred_indexes(&schema(vec![after_t]), Some(&schema(vec![before_t]))).len(),
            1
        );
    }

    #[test]
    fn switching_a_search_index_to_a_different_engine_backfills_it() {
        let mut before_t = td("Product");
        before_t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        let mut after_t = td("Product");
        after_t.search_indexes = vec![search(None, SearchBackend::OpenSearch, &["name"])];

        let changed = changed_deferred_indexes(&schema(vec![after_t]), Some(&schema(vec![before_t])));
        assert_eq!(changed.len(), 1, "got: {changed:?}");
        assert_eq!(changed[0].index_kind, "OpenSearch");
    }

    #[test]
    fn a_changed_embedding_model_backfills_the_vector_index() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        let mut after_t = td("Product");
        let mut vi = vector(None, &["name"], 1024, "cosine");
        vi.model = "text-embedding-3-small".into();
        after_t.vector_indexes = vec![vi];

        assert_eq!(
            changed_deferred_indexes(&schema(vec![after_t]), Some(&schema(vec![before_t]))).len(),
            1
        );
    }

    #[test]
    fn a_metric_only_change_does_not_re_embed() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        let mut after_t = td("Product");
        after_t.vector_indexes = vec![vector(None, &["name"], 1024, "euclidean")];

        assert!(
            changed_deferred_indexes(&schema(vec![after_t]), Some(&schema(vec![before_t]))).is_empty(),
            "a metric change rebuilds the hnsw index, not the embeddings — \
             re-embedding would bill the provider for identical vectors"
        );
    }

    #[test]
    fn no_baseline_backfills_nothing() {
        let mut t = td("Product");
        t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];
        assert!(
            changed_deferred_indexes(&schema(vec![t]), None).is_empty(),
            "without a baseline every index reads as new, which on a database \
             predating schema_state would re-index the entire dataset"
        );
    }

    // ── footprint amendments ─────────────────────────────────────────────

    #[test]
    fn a_changed_postgres_search_index_rebuilds_its_generated_column() {
        let mut before_t = td("Product");
        before_t.search_indexes = vec![search(None, SearchBackend::Postgres, &["name"])];
        let mut after_t = td("Product");
        after_t.search_indexes = vec![search(None, SearchBackend::Postgres, &["name", "description"])];

        let ops = index_amendment_ops(&schema(vec![after_t]), Some(&schema(vec![before_t])), true);
        let sql = joined(&ops);
        assert!(sql.contains(r#"DROP COLUMN IF EXISTS "__search__""#), "got: {sql}");
        assert!(
            sql.contains(r#"coalesce("description", '')"#),
            "the re-added column must carry the new expression; got: {sql}"
        );
        assert!(sql.contains("CREATE INDEX CONCURRENTLY"), "got: {sql}");
        assert!(
            !ops[0].non_transactional && !ops[1].non_transactional,
            "the drop and re-add must commit together or the type is left with no search column"
        );
    }

    #[test]
    fn an_unchanged_postgres_search_index_is_left_alone() {
        let mut t = td("Product");
        t.search_indexes = vec![search(None, SearchBackend::Postgres, &["name"])];
        let ops = index_amendment_ops(&schema(vec![t.clone()]), Some(&schema(vec![t])), true);
        assert!(ops.is_empty(), "got: {}", joined(&ops));
    }

    #[test]
    fn a_changed_vector_dimension_recreates_the_column_and_index() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        let mut after_t = td("Product");
        after_t.vector_indexes = vec![vector(None, &["name"], 1536, "cosine")];

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(sql.contains(r#"DROP COLUMN IF EXISTS "__vector__""#), "got: {sql}");
        assert!(sql.contains(r#""__vector__" vector(1536)"#), "got: {sql}");
        assert!(sql.contains("USING hnsw"), "got: {sql}");
    }

    #[test]
    fn a_changed_metric_rebuilds_only_the_hnsw_index() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        let mut after_t = td("Product");
        after_t.vector_indexes = vec![vector(None, &["name"], 1024, "euclidean")];

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(
            !sql.contains("DROP COLUMN"),
            "the embeddings are still valid; got: {sql}"
        );
        assert!(
            sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__vector__""#),
            "got: {sql}"
        );
        assert!(sql.contains("vector_l2_ops"), "got: {sql}");
    }

    #[test]
    fn a_removed_postgres_search_index_drops_its_column() {
        let mut before_t = td("Product");
        before_t.search_indexes = vec![search(None, SearchBackend::Postgres, &["name"])];
        let after_t = td("Product");

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(
            sql.contains(r#"DROP COLUMN IF EXISTS "__search__""#),
            "a generated column nothing declares is still recomputed on every write; got: {sql}"
        );
    }

    #[test]
    fn moving_a_search_index_off_postgres_drops_the_tsvector_column() {
        let mut before_t = td("Product");
        before_t.search_indexes = vec![search(None, SearchBackend::Postgres, &["name"])];
        let mut after_t = td("Product");
        after_t.search_indexes = vec![search(None, SearchBackend::Meilisearch, &["name"])];

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(sql.contains(r#"DROP COLUMN IF EXISTS "__search__""#), "got: {sql}");
    }

    #[test]
    fn a_removed_vector_index_drops_its_column_and_index() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(Some("summary"), &["name"], 1024, "cosine")];
        let after_t = td("Product");

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(sql.contains(r#""Product__vector_summary__""#), "got: {sql}");
        assert!(
            sql.contains(r#"DROP COLUMN IF EXISTS "__vector_summary__""#),
            "got: {sql}"
        );
    }

    /// A property for a plain index to be declared over. `td` carries none,
    /// because nothing else in this module reads them — an index key does.
    fn indexed_td(name: &str, props: &[&str]) -> TypeDescriptor {
        let mut t = td(name);
        t.properties = props
            .iter()
            .map(|p| PropertyDescriptor {
                name: (*p).into(),
                pg_type: "text".into(),
                nullable: true,
                default_sql: None,
                default_pyql: None,
                description: None,
                check_constraints: vec![],
                is_exclusive: false,
                is_pk: false,
                is_readonly: false,
                rewrites: vec![],
                tuple_members: None,
                column_type: None,
            })
            .collect();
        t
    }

    fn plain(pointers: &[&str], unique: bool, unless: Option<&str>) -> IndexDescriptor {
        IndexDescriptor {
            pointers: pointers.iter().map(|p| p.to_string()).collect(),
            expression: None,
            unique,
            unless: unless.map(str::to_string),
        }
    }

    /// Postgres truncates an identifier past 63 bytes, so a name longer than
    /// that is a *third* opinion about what the index is called, on top of
    /// whichever path generated it. Every caller gets the capped name.
    #[test]
    fn a_long_index_name_is_capped_at_the_source() {
        let long = "T".repeat(80);
        let names = [
            plain_index_name(&long, &plain(&["name"], false, None), 0),
            vector_index_name(&long, Some("summary")),
            search_index_name(&long, None),
        ];
        for name in names {
            assert_eq!(name.len(), 63, "{name}");
        }
        // And stable: a hashed tail that moved between runs would make every
        // migration propose the index again.
        assert_eq!(
            plain_index_name(&long, &plain(&["name"], false, None), 0),
            plain_index_name(&long, &plain(&["name"], false, None), 0)
        );
    }

    /// Deleting an `Index` declaration left its index alive in the database
    /// with nothing in the schema referring to it — the main diff only ever
    /// asks what Postgres is *missing*. Worst case, replacing one with a
    /// `VectorIndex` on the same pointer kept a B-tree over an unbounded
    /// text column that nothing would ever use again.
    #[test]
    fn a_removed_plain_index_is_dropped() {
        let mut before_t = indexed_td("Product", &["name", "created_at"]);
        before_t.indexes = vec![plain(&["name"], false, None), plain(&["created_at"], false, None)];
        let after_t = indexed_td("Product", &["name", "created_at"]);

        let ops = index_amendment_ops(&schema(vec![after_t]), Some(&schema(vec![before_t])), true);
        let sql = joined(&ops);
        assert!(
            sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__name_idx""#),
            "got: {sql}"
        );
        assert!(
            sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__created_at_idx""#),
            "got: {sql}"
        );
        assert!(
            ops.iter().all(|op| op.non_transactional),
            "a concurrent drop cannot run inside a transaction: {ops:?}"
        );
    }

    /// Replacing an `Index` with a `VectorIndex` on the same pointer is the
    /// real-world shape of the above: two separate declarations, so the
    /// B-tree is removed rather than amended.
    #[test]
    fn swapping_a_plain_index_for_a_vector_one_drops_the_btree() {
        let mut before_t = indexed_td("Product", &["data"]);
        before_t.indexes = vec![plain(&["data"], false, None)];
        let mut after_t = indexed_td("Product", &["data"]);
        after_t.vector_indexes = vec![vector(None, &["data"], 1024, "cosine")];

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(sql.contains(r#"IF EXISTS "public"."Product__data_idx""#), "got: {sql}");
    }

    /// An index the declaration still names is left alone.
    #[test]
    fn an_unchanged_plain_index_is_left_alone() {
        let mut before_t = indexed_td("Product", &["name"]);
        before_t.indexes = vec![plain(&["name"], false, None)];
        let after_t = before_t.clone();

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(sql.is_empty(), "got: {sql}");
    }

    /// Uniqueness and the partial predicate both live in the index itself,
    /// under a name that does not change with them — so the create pass
    /// reads the stale index as the declared one and the amendment has to
    /// replace it outright.
    #[test]
    fn a_plain_index_changed_under_its_own_name_is_replaced() {
        for (before, after, expected) in [
            (
                plain(&["name"], false, None),
                plain(&["name"], true, None),
                "UNIQUE INDEX",
            ),
            (
                plain(&["name"], false, None),
                plain(&["name"], false, Some(".name = 'x'")),
                "WHERE NOT",
            ),
        ] {
            let mut before_t = indexed_td("Product", &["name"]);
            before_t.indexes = vec![before];
            let mut after_t = indexed_td("Product", &["name"]);
            after_t.indexes = vec![after];

            let sql = joined(&index_amendment_ops(
                &schema(vec![after_t]),
                Some(&schema(vec![before_t])),
                true,
            ));
            assert!(
                sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__name_idx""#),
                "got: {sql}"
            );
            assert!(sql.contains(expected), "expected {expected} in: {sql}");
        }
    }

    /// An expression index is named by its position in the declaration list,
    /// so removing one ahead of it moves the name it answers to. Keying the
    /// diff on the name rather than on the declaration gets both halves
    /// right: the vacated name is replaced, the trailing one dropped.
    #[test]
    fn removing_one_of_two_expression_indexes_leaves_the_other_in_place() {
        let mut before_t = indexed_td("Product", &["name", "created_at"]);
        before_t.indexes = vec![
            IndexDescriptor {
                pointers: vec![],
                expression: Some("str_lower(.name)".into()),
                unique: false,
                unless: None,
            },
            IndexDescriptor {
                pointers: vec![],
                expression: Some("str_lower(.created_at)".into()),
                unique: false,
                unless: None,
            },
        ];
        let mut after_t = indexed_td("Product", &["name", "created_at"]);
        after_t.indexes = vec![before_t.indexes[1].clone()];

        let sql = joined(&index_amendment_ops(
            &schema(vec![after_t]),
            Some(&schema(vec![before_t])),
            true,
        ));
        assert!(
            sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__expr1_idx""#),
            "the trailing name no declaration answers to: {sql}"
        );
        assert!(
            sql.contains(r#"DROP INDEX CONCURRENTLY IF EXISTS "public"."Product__expr0_idx""#)
                && sql.contains(r#"CREATE INDEX CONCURRENTLY IF NOT EXISTS "Product__expr0_idx""#),
            "the surviving index moved into the vacated name: {sql}"
        );
        assert!(sql.contains("created_at"), "with its own key: {sql}");
    }

    #[test]
    fn watch_mode_never_uses_concurrently() {
        let mut before_t = td("Product");
        before_t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        let mut after_t = td("Product");
        after_t.vector_indexes = vec![vector(None, &["name"], 1024, "inner_product")];

        let ops = index_amendment_ops(&schema(vec![after_t]), Some(&schema(vec![before_t])), false);
        assert!(!joined(&ops).contains("CONCURRENTLY"), "got: {}", joined(&ops));
        assert!(
            ops.iter().all(|o| !o.non_transactional),
            "watch applies everything in one batch_execute"
        );
    }

    #[test]
    fn no_baseline_amends_nothing() {
        let mut t = td("Product");
        t.vector_indexes = vec![vector(None, &["name"], 1024, "cosine")];
        assert!(index_amendment_ops(&schema(vec![t]), None, true).is_empty());
    }
}
