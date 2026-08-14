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

//! Read-through query-result caching — thin helpers over `pylon_cache::Cache`,
//! mirroring `pylon/cache.py`'s own `_cache_key`/`get`/`put`/`get_json`/
//! `put_json`/`invalidate_for`. Deliberately smaller than the Python
//! surface: a single global on/off (whether `Client` was built with
//! `Builder::cache(..)`) and no per-type (`[cache.sets.<Name>]`) overrides.
//! See `exec.rs` for where these are actually called from.
//!
//! A client's own writes evict their tags (`invalidate_for`); cross-process
//! invalidation is still `NOTIFY`-driven and needs a listener.

use pylon_core::query::CompiledQuery;
use pylon_value::DecodedValue;

use crate::error::{Error, Result};

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Cache(e.to_string())
}

/// How one statement may use the process cache.
///
/// Reading-through and evicting are separate permissions, because a
/// transaction has one and not the other: rows read inside it aren't
/// committed, so they must never populate the cache — but a write inside it
/// still has to evict, or a later identical read is served the pre-write
/// result forever.
#[derive(Clone, Copy, Default)]
pub(crate) struct CacheAccess<'a> {
    cache: Option<&'a pylon_cache::Cache>,
    read_through: bool,
}

impl<'a> CacheAccess<'a> {
    /// Outside a transaction: results may be both served and stored.
    pub(crate) fn read_write(cache: Option<&'a pylon_cache::Cache>) -> Self {
        Self {
            cache,
            read_through: true,
        }
    }

    /// Inside a transaction: writes evict, but nothing is served or stored.
    pub(crate) fn evict_only(cache: Option<&'a pylon_cache::Cache>) -> Self {
        Self {
            cache,
            read_through: false,
        }
    }

    fn readable(&self) -> Option<&'a pylon_cache::Cache> {
        if self.read_through { self.cache } else { None }
    }
}

/// `kind` namespaces the key so `query`/`query_single` (kind `"rows"`, a
/// decoded row list) and the `_json` methods (kinds `"json_all"`/
/// `"json_single"`, a raw JSON string) never collide on the same
/// underlying SQL+params — mirrors `pylon/cache.py::_cache_key`'s
/// `f"{kind}\x00{compiled.sql}"` prefix.
fn cache_key(kind: &str, sql: &str, params: &[DecodedValue]) -> Result<String> {
    pylon_cache::cache_key(&format!("{kind}\0{sql}"), params).map_err(map_err)
}

/// Whether a query's result may be served from, or written to, the cache.
///
/// A *mutating* statement never may. `client.query("insert Person {...}")`
/// is a normal way to insert and read the new row back, but its result is
/// not a function of its inputs: serving a cached one returns a stale id
/// *and skips the write entirely*, so running the same insert twice
/// silently produced one row instead of two. Tags alone don't cover this —
/// a write has tags (the tables it touches), it just must not be cached
/// under them. Mirrors `pylon/cache.py::_is_cacheable`.
fn is_cacheable(compiled: &CompiledQuery) -> bool {
    !compiled.mutates && !compiled.tags.is_empty()
}

/// Evicts every cached entry tagged with a set this statement wrote.
///
/// Cross-process invalidation is `NOTIFY`-driven and needs a listener, but a
/// client's *own* writes must not: without this, a process that writes and
/// then re-runs an identical read gets its own pre-write result back, and
/// nothing in a plain program (no worker, no server) ever corrects it.
/// Mirrors `pylon/cache.py::invalidate_for`.
///
/// Eviction only ever *removes* entries, so it's safe whenever the cache is
/// open — correctness shouldn't depend on the write and the read that
/// populated the entry agreeing about any per-set enable flag.
pub(crate) fn invalidate_for(access: CacheAccess<'_>, compiled: &CompiledQuery) -> Result<()> {
    // Deliberately `access.cache`, not `access.readable()`: eviction applies
    // inside a transaction too, where read-through does not.
    let Some(cache) = access.cache else { return Ok(()) };
    if !compiled.mutates || compiled.tags.is_empty() {
        return Ok(());
    }
    cache.invalidate(&compiled.tags).map_err(map_err)
}

/// Returns cached rows for `compiled`+`params`, or `None` on a cache miss.
pub(crate) fn get_rows(
    access: CacheAccess<'_>,
    compiled: &CompiledQuery,
    params: &[DecodedValue],
) -> Result<Option<Vec<DecodedValue>>> {
    let Some(cache) = access.readable() else {
        return Ok(None);
    };
    if !is_cacheable(compiled) {
        return Ok(None);
    }
    let key = cache_key("rows", &compiled.sql, params)?;
    Ok(cache.get(&key).map_err(map_err)?.map(|entry| entry.rows))
}

/// Caches `rows` under a key derived from `compiled`+`params`, tagged with
/// `compiled.tags` for later invalidation by whatever else is watching
/// this cache directory. A no-op when there are no tags to key eviction
/// on, or when the statement writes — mirrors `pylon/cache.py::put`.
pub(crate) fn put_rows(
    access: CacheAccess<'_>,
    compiled: &CompiledQuery,
    params: &[DecodedValue],
    rows: &[DecodedValue],
) -> Result<()> {
    let Some(cache) = access.readable() else { return Ok(()) };
    if !is_cacheable(compiled) {
        return Ok(());
    }
    let key = cache_key("rows", &compiled.sql, params)?;
    cache.put(&key, rows.to_vec(), compiled.tags.clone()).map_err(map_err)
}

/// Returns `Some(value)` on a cache hit (`value` is `None` for a
/// legitimately-cached empty `query_single_json` result — distinguished
/// from a miss by the outer `Option`, matching `pylon/cache.py::get_json`'s
/// `(hit, value)` tuple).
pub(crate) fn get_json(
    access: CacheAccess<'_>,
    kind: &str,
    compiled: &CompiledQuery,
    params: &[DecodedValue],
) -> Result<Option<Option<String>>> {
    let Some(cache) = access.readable() else {
        return Ok(None);
    };
    if !is_cacheable(compiled) {
        return Ok(None);
    }
    let key = cache_key(kind, &compiled.sql, params)?;
    let Some(entry) = cache.get(&key).map_err(map_err)? else {
        return Ok(None);
    };
    Ok(Some(match entry.rows.into_iter().next() {
        Some(DecodedValue::Str(s)) => Some(s),
        _ => None,
    }))
}

/// Counterpart to `get_json` — `value = None` caches a legitimately-empty
/// `query_single_json` result rather than skipping the cache entry
/// entirely. A no-op when there are no tags to key eviction on.
pub(crate) fn put_json(
    access: CacheAccess<'_>,
    kind: &str,
    compiled: &CompiledQuery,
    params: &[DecodedValue],
    value: Option<&str>,
) -> Result<()> {
    let Some(cache) = access.readable() else { return Ok(()) };
    if !is_cacheable(compiled) {
        return Ok(());
    }
    let key = cache_key(kind, &compiled.sql, params)?;
    let rows = value
        .map(|v| vec![DecodedValue::Str(v.to_string())])
        .unwrap_or_default();
    cache.put(&key, rows, compiled.tags.clone()).map_err(map_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-write access, as `Client` uses outside a transaction.
    fn rw(cache: &pylon_cache::Cache) -> CacheAccess<'_> {
        CacheAccess::read_write(Some(cache))
    }

    fn open_temp() -> (tempfile::TempDir, pylon_cache::Cache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = pylon_cache::Cache::open(dir.path(), 10).unwrap();
        (dir, cache)
    }

    fn compiled_with_tags(sql: &str, tags: &[&str]) -> CompiledQuery {
        compiled(sql, tags, false)
    }

    fn compiled(sql: &str, tags: &[&str], mutates: bool) -> CompiledQuery {
        CompiledQuery {
            sql: sql.to_string(),
            param_names: vec![],
            params: vec![],
            shape: pylon_core::query::ShapeDescriptor {
                root: pylon_core::query::ShapeNode::RawScalar,
            },
            warnings: vec![],
            inference_plan: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            mutates,
            analyze_paths: None,
        }
    }

    /// A write must never be cached, even though it carries tags. Serving a
    /// cached INSERT result returns a stale id *and skips the write*, so the
    /// same insert run twice would produce one row instead of two.
    #[test]
    fn a_mutating_statement_is_never_cached() {
        let (_dir, cache) = open_temp();
        let insert = compiled("insert person", &["public.person"], true);

        put_rows(rw(&cache), &insert, &[], &[DecodedValue::Str("row".into())]).unwrap();
        assert_eq!(
            get_rows(rw(&cache), &insert, &[]).unwrap(),
            None,
            "a write must not be served from cache"
        );

        put_json(rw(&cache), "json_all", &insert, &[], Some("[]")).unwrap();
        assert_eq!(get_json(rw(&cache), "json_all", &insert, &[]).unwrap(), None);
    }

    /// The read/write distinction is `mutates`, not the tag list: both carry
    /// the same tags, and only the read is cacheable.
    #[test]
    fn the_same_tags_are_still_cacheable_for_a_read() {
        let (_dir, cache) = open_temp();
        let read = compiled("select person", &["public.person"], false);
        put_rows(rw(&cache), &read, &[], &[DecodedValue::Str("row".into())]).unwrap();
        assert!(get_rows(rw(&cache), &read, &[]).unwrap().is_some());
    }

    /// Cross-process invalidation needs a `NOTIFY` listener, but a client's
    /// own writes must not: without this, a program that reads, writes, then
    /// repeats the read gets its own pre-write result back forever.
    #[test]
    fn a_write_evicts_a_read_sharing_its_tag() {
        let (_dir, cache) = open_temp();
        let read = compiled("select person", &["public.person"], false);
        put_rows(rw(&cache), &read, &[], &[DecodedValue::Str("before".into())]).unwrap();
        assert!(get_rows(rw(&cache), &read, &[]).unwrap().is_some());

        let write = compiled("update person", &["public.person"], true);
        invalidate_for(rw(&cache), &write).unwrap();

        assert_eq!(
            get_rows(rw(&cache), &read, &[]).unwrap(),
            None,
            "the read must be evicted by a write to the same set"
        );
    }

    #[test]
    fn a_write_leaves_an_unrelated_tag_alone() {
        let (_dir, cache) = open_temp();
        let other = compiled("select company", &["public.company"], false);
        put_rows(rw(&cache), &other, &[], &[DecodedValue::Str("row".into())]).unwrap();

        invalidate_for(rw(&cache), &compiled("update person", &["public.person"], true)).unwrap();
        assert!(get_rows(rw(&cache), &other, &[]).unwrap().is_some());
    }

    /// Inside a transaction the two permissions split: a write evicts, but
    /// nothing is served or stored, because those rows aren't committed yet.
    #[test]
    fn transaction_access_evicts_without_populating() {
        let (_dir, cache) = open_temp();
        let tx = CacheAccess::evict_only(Some(&cache));
        let read = compiled("select person", &["public.person"], false);

        // A read inside the transaction must not populate...
        put_rows(tx, &read, &[], &[DecodedValue::Str("uncommitted".into())]).unwrap();
        assert_eq!(get_rows(rw(&cache), &read, &[]).unwrap(), None);

        // ...nor be served from an entry an earlier committed read left.
        put_rows(rw(&cache), &read, &[], &[DecodedValue::Str("committed".into())]).unwrap();
        assert_eq!(get_rows(tx, &read, &[]).unwrap(), None);

        // But a write inside it still evicts that entry — including one that
        // arrives as `query("insert ...")` rather than `execute`.
        invalidate_for(tx, &compiled("insert person", &["public.person"], true)).unwrap();
        assert_eq!(
            get_rows(rw(&cache), &read, &[]).unwrap(),
            None,
            "a transactional write must evict"
        );
    }

    #[test]
    fn a_read_never_evicts() {
        let (_dir, cache) = open_temp();
        let read = compiled("select person", &["public.person"], false);
        put_rows(rw(&cache), &read, &[], &[DecodedValue::Str("row".into())]).unwrap();

        invalidate_for(rw(&cache), &read).unwrap();
        assert!(get_rows(rw(&cache), &read, &[]).unwrap().is_some());
    }

    #[test]
    fn rows_miss_then_hit() {
        let (_dir, cache) = open_temp();
        let compiled = compiled_with_tags("select 1", &["public.person"]);
        assert_eq!(get_rows(rw(&cache), &compiled, &[]).unwrap(), None);

        put_rows(rw(&cache), &compiled, &[], &[DecodedValue::I64(1)]).unwrap();
        assert_eq!(
            get_rows(rw(&cache), &compiled, &[]).unwrap(),
            Some(vec![DecodedValue::I64(1)])
        );
    }

    #[test]
    fn no_tags_means_put_is_a_no_op() {
        let (_dir, cache) = open_temp();
        let compiled = compiled_with_tags("select 1", &[]);
        put_rows(rw(&cache), &compiled, &[], &[DecodedValue::I64(1)]).unwrap();
        assert_eq!(get_rows(rw(&cache), &compiled, &[]).unwrap(), None);
    }

    #[test]
    fn rows_and_json_kinds_do_not_collide_on_the_same_sql() {
        let (_dir, cache) = open_temp();
        let compiled = compiled_with_tags("select 1", &["public.person"]);
        put_rows(rw(&cache), &compiled, &[], &[DecodedValue::I64(1)]).unwrap();
        put_json(rw(&cache), "json_all", &compiled, &[], Some("[1]")).unwrap();

        assert_eq!(
            get_rows(rw(&cache), &compiled, &[]).unwrap(),
            Some(vec![DecodedValue::I64(1)])
        );
        assert_eq!(
            get_json(rw(&cache), "json_all", &compiled, &[]).unwrap(),
            Some(Some("[1]".to_string()))
        );
        // A different kind namespace for the same SQL is a genuine miss.
        assert_eq!(get_json(rw(&cache), "json_single", &compiled, &[]).unwrap(), None);
    }

    #[test]
    fn json_single_caches_a_legitimately_empty_result_distinct_from_a_miss() {
        let (_dir, cache) = open_temp();
        let compiled = compiled_with_tags("select Person filter false", &["public.person"]);
        assert_eq!(get_json(rw(&cache), "json_single", &compiled, &[]).unwrap(), None);

        put_json(rw(&cache), "json_single", &compiled, &[], None).unwrap();
        assert_eq!(get_json(rw(&cache), "json_single", &compiled, &[]).unwrap(), Some(None));
    }
}
