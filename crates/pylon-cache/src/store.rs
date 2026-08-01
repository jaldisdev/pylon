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

//! LMDB-backed storage for `CachedEntry` values, keyed by query hash, with a
//! reverse tag index for invalidation.
//!
//! Two databases in one `heed` environment:
//! - `entries`: cache key (`Str`) -> `CachedEntry` bytes (`Bytes`, rkyv).
//! - `tags`: tag (`Str`) -> cache key (`Str`), `DUP_SORT` so one tag maps to
//!   every cache key it was recorded against — this is what makes
//!   `invalidate` an O(matching entries) reverse lookup instead of a full
//!   `entries` scan.

use std::path::Path;

use heed::types::{Bytes, Str};
use heed::{Database, DatabaseFlags, Env, EnvOpenOptions};
use rkyv::rancor::Error as RkyvError;
use sha2::{Digest, Sha256};

use pylon_value::{ArchivedCachedEntry, CachedEntry, CachedValue};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Bump this whenever `CachedValue`'s rkyv binary layout changes in a way
/// that isn't safely re-readable under the new layout — adding, removing,
/// or reordering an enum variant (rkyv discriminants are positional by
/// default), changing a field's type, etc. `Cache::open` wipes the whole
/// environment on a version mismatch rather than risk silently misdecoding
/// bytes written under an older layout.
///
/// This constant exists because of a real incident: `CachedValue::Interval`/
/// `Date`/`Time`/`Timestamp`/`Timestamptz` were inserted *between*
/// `Decimal` and `Array` (rather than appended at the end), which shifted
/// every later variant's discriminant — an `Array` entry written by an
/// older build got silently misread as the new `Interval` variant by a
/// rebuilt binary, surfacing as a `ValueError` about decoding a
/// `cal::relative_duration` on a completely unrelated query. Reordering
/// mid-enum should be avoided going forward (append new variants at the
/// end instead) — but bumping this version is the safety net for whenever
/// that isn't possible or gets missed.
const CACHE_FORMAT_VERSION: &[u8] = b"2";

/// `sha256(sql) + bound parameter values`, hex-encoded — see the cache
/// layer plan's key-simplification note: `compiled.sql` is already a
/// canonical, whitespace-insensitive form, so hashing it directly (rather
/// than a separately normalized AST) is both simpler and precise.
pub fn cache_key(sql: &str, params: &[CachedValue]) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(sql.as_bytes());
    for param in params {
        let bytes = rkyv::to_bytes::<RkyvError>(param)?;
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Snapshot of a cache's current size — see `Cache::stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of cached entries (one per distinct query+params key).
    pub entry_count: u64,
    /// Bytes actually used by the environment's databases, excluding
    /// LMDB's free (reclaimable) pages — i.e. real usage, not the
    /// fixed virtual `map_size` reservation the environment was opened with.
    pub used_bytes: u64,
}

pub struct Cache {
    env: Env,
    entries: Database<Str, Bytes>,
    tags: Database<Str, Str>,
}

impl Cache {
    pub fn open(path: &Path, max_size_mb: usize) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        // SAFETY: LMDB requires the caller to ensure no other process opens
        // this environment with incompatible flags/map size concurrently —
        // this path is exclusively owned by one pylon node's cache dir.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(max_size_mb * 1024 * 1024)
                .max_dbs(3)
                .open(path)?
        };

        let mut wtxn = env.write_txn()?;
        let entries = env.create_database(&mut wtxn, Some("entries"))?;
        let tags = env
            .database_options()
            .types::<Str, Str>()
            .flags(DatabaseFlags::DUP_SORT)
            .name("tags")
            .create(&mut wtxn)?;
        let meta: Database<Str, Bytes> = env.create_database(&mut wtxn, Some("meta"))?;

        // Self-healing format check — see `CACHE_FORMAT_VERSION`'s doc
        // comment. A missing/mismatched version (including a cache
        // directory from before this check existed at all) wipes
        // everything rather than risk misdecoding stale bytes.
        if meta.get(&wtxn, "format_version")?.map(<[u8]>::to_vec) != Some(CACHE_FORMAT_VERSION.to_vec()) {
            entries.clear(&mut wtxn)?;
            tags.clear(&mut wtxn)?;
            meta.put(&mut wtxn, "format_version", CACHE_FORMAT_VERSION)?;
        }
        wtxn.commit()?;

        Ok(Self { env, entries, tags })
    }

    pub fn get(&self, key: &str) -> Result<Option<CachedEntry>> {
        let rtxn = self.env.read_txn()?;
        let Some(bytes) = self.entries.get(&rtxn, key)? else {
            return Ok(None);
        };
        // LMDB's mmap'd page data isn't guaranteed to land on an aligned
        // offset, but rkyv's archived views require it — copy into an
        // aligned buffer before the unchecked access below.
        let mut aligned = rkyv::util::AlignedVec::<16>::new();
        aligned.extend_from_slice(bytes);
        // SAFETY: bytes were produced by `put` on this same type, only ever
        // read back from our own LMDB env — never untrusted external input.
        let archived = unsafe { rkyv::access_unchecked::<ArchivedCachedEntry>(&aligned) };
        let entry: CachedEntry = rkyv::deserialize::<CachedEntry, RkyvError>(archived)?;
        Ok(Some(entry))
    }

    pub fn put(&self, key: &str, rows: Vec<CachedValue>, tags: Vec<String>) -> Result<()> {
        let entry = CachedEntry { rows, tags: tags.clone() };
        let bytes = rkyv::to_bytes::<RkyvError>(&entry)?;

        let mut wtxn = self.env.write_txn()?;
        self.entries.put(&mut wtxn, key, &bytes)?;
        for tag in &tags {
            self.tags.put(&mut wtxn, tag, key)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    /// Evicts every cache entry tagged with any of `tags` — called from the
    /// LISTEN/NOTIFY invalidation path after a write commits.
    pub fn invalidate(&self, tags: &[String]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;

        let mut keys_to_remove: Vec<String> = Vec::new();
        for tag in tags {
            if let Some(iter) = self.tags.get_duplicates(&wtxn, tag.as_str())? {
                for result in iter {
                    let (_, cache_key) = result?;
                    keys_to_remove.push(cache_key.to_owned());
                }
            }
        }

        for tag in tags {
            // Deleting by key alone on a DUP_SORT database removes every
            // duplicate value recorded for that key (LMDB's own semantics
            // for `mdb_del` with no data pointer).
            self.tags.delete(&mut wtxn, tag.as_str())?;
        }
        for key in &keys_to_remove {
            self.entries.delete(&mut wtxn, key.as_str())?;
        }

        wtxn.commit()?;
        Ok(())
    }

    /// Current size of the cache — see `CacheStats`.
    pub fn stat(&self) -> Result<CacheStats> {
        let rtxn = self.env.read_txn()?;
        let entry_count = self.entries.len(&rtxn)?;
        drop(rtxn);
        let used_bytes = self.env.non_free_pages_size()?;
        Ok(CacheStats { entry_count, used_bytes })
    }

    /// Evicts every cache entry, unconditionally — used by the `pylon cache
    /// purge` CLI command. Unlike `invalidate`, this doesn't require
    /// knowing any tags up front.
    pub fn clear(&self) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.entries.clear(&mut wtxn)?;
        self.tags.clear(&mut wtxn)?;
        wtxn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 10).unwrap();
        (dir, cache)
    }

    #[test]
    fn reopening_with_a_different_format_version_wipes_stale_entries() {
        // Regression: CachedValue variants inserted mid-enum shift every
        // later variant's rkyv discriminant, so a stale cache directory
        // written under an older layout must never be trusted as-is — it
        // needs to be wiped, not silently misdecoded.
        let dir = tempfile::tempdir().unwrap();
        {
            let cache = Cache::open(dir.path(), 10).unwrap();
            cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
            assert!(cache.get("key1").unwrap().is_some());
        }
        // Simulate a cache directory written under a different format
        // version by overwriting the version marker directly.
        {
            let env = unsafe {
                EnvOpenOptions::new().map_size(10 * 1024 * 1024).max_dbs(3).open(dir.path()).unwrap()
            };
            let mut wtxn = env.write_txn().unwrap();
            let meta: Database<Str, Bytes> = env.create_database(&mut wtxn, Some("meta")).unwrap();
            meta.put(&mut wtxn, "format_version", b"a-different-version").unwrap();
            wtxn.commit().unwrap();
        }
        let cache = Cache::open(dir.path(), 10).unwrap();
        assert!(cache.get("key1").unwrap().is_none());
    }

    #[test]
    fn reopening_with_the_same_format_version_preserves_entries() {
        let dir = tempfile::tempdir().unwrap();
        {
            let cache = Cache::open(dir.path(), 10).unwrap();
            cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
        }
        let cache = Cache::open(dir.path(), 10).unwrap();
        assert!(cache.get("key1").unwrap().is_some());
    }

    #[test]
    fn put_then_get_round_trips() {
        let (_dir, cache) = open_temp();
        let rows = vec![CachedValue::I64(1), CachedValue::Str("hi".into())];
        cache.put("key1", rows.clone(), vec!["public.person".into()]).unwrap();

        let entry = cache.get("key1").unwrap().expect("entry present");
        assert_eq!(entry.rows, rows);
        assert_eq!(entry.tags, vec!["public.person".to_string()]);
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (_dir, cache) = open_temp();
        assert!(cache.get("nope").unwrap().is_none());
    }

    #[test]
    fn invalidate_evicts_all_entries_sharing_a_tag() {
        let (_dir, cache) = open_temp();
        cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
        cache.put("key2", vec![CachedValue::I64(2)], vec!["public.person".into(), "public.pet".into()]).unwrap();
        cache.put("key3", vec![CachedValue::I64(3)], vec!["public.pet".into()]).unwrap();

        cache.invalidate(&["public.person".to_string()]).unwrap();

        assert!(cache.get("key1").unwrap().is_none());
        assert!(cache.get("key2").unwrap().is_none());
        assert!(cache.get("key3").unwrap().is_some());
    }

    #[test]
    fn invalidate_unknown_tag_is_a_no_op() {
        let (_dir, cache) = open_temp();
        cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
        cache.invalidate(&["public.nonexistent".to_string()]).unwrap();
        assert!(cache.get("key1").unwrap().is_some());
    }

    #[test]
    fn cache_key_is_stable_and_sensitive_to_params() {
        let k1 = cache_key("select 1", &[CachedValue::I64(1)]).unwrap();
        let k2 = cache_key("select 1", &[CachedValue::I64(1)]).unwrap();
        let k3 = cache_key("select 1", &[CachedValue::I64(2)]).unwrap();
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn stat_on_empty_cache_reports_zero_entries() {
        let (_dir, cache) = open_temp();
        let stats = cache.stat().unwrap();
        assert_eq!(stats.entry_count, 0);
    }

    #[test]
    fn stat_reports_entry_count_and_nonzero_used_bytes_after_put() {
        let (_dir, cache) = open_temp();
        cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
        cache.put("key2", vec![CachedValue::I64(2)], vec!["public.pet".into()]).unwrap();

        let stats = cache.stat().unwrap();
        assert_eq!(stats.entry_count, 2);
        assert!(stats.used_bytes > 0);
    }

    #[test]
    fn clear_removes_all_entries_and_tags() {
        let (_dir, cache) = open_temp();
        cache.put("key1", vec![CachedValue::I64(1)], vec!["public.person".into()]).unwrap();
        cache.put("key2", vec![CachedValue::I64(2)], vec!["public.pet".into()]).unwrap();

        cache.clear().unwrap();

        assert!(cache.get("key1").unwrap().is_none());
        assert!(cache.get("key2").unwrap().is_none());
        assert_eq!(cache.stat().unwrap().entry_count, 0);

        // Tags were cleared too — re-invalidating a formerly-present tag
        // must be a no-op, not find stale reverse-index entries.
        cache.invalidate(&["public.person".to_string()]).unwrap();
    }

    #[test]
    fn clear_on_empty_cache_is_a_no_op() {
        let (_dir, cache) = open_temp();
        cache.clear().unwrap();
        assert_eq!(cache.stat().unwrap().entry_count, 0);
    }
}
