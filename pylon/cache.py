#
# This source file is part of the Pylon open source project.
#
# Copyright (c) 2026 Jaldis B.V.
#
# Licensed under the MIT OR Apache-2.0 license (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://opensource.org/licenses/MIT
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pylon._core import CompiledQuery
    from pylon.config import CacheConfig

log = logging.getLogger(__name__)

NOTIFY_CHANNEL = 'pylon_cache_invalidate'

_enabled = False


def init(config: CacheConfig) -> None:
    """Open the process-global LMDB cache. No-op if ``config.enabled`` is
    False — callers must still short-circuit on ``config.enabled`` before
    calling `get`/`put` themselves; this just guards the LMDB handle."""
    global _enabled
    if not config.enabled:
        return
    from pylon._core import cache_init

    config.path.mkdir(parents=True, exist_ok=True)
    cache_init(str(config.path), config.max_size_mb)
    _enabled = True


def _resolve_set_name_for_tag(tag: str) -> str | None:
    """Schema-qualified table tag (e.g. ``"public.person"``) -> short Pylon
    type name (e.g. ``"Person"``). Returns None for tags with no owning type
    (e.g. a junction table).

    Delegates to the Rust-side map rather than scanning ``schema.types``
    here: that getter rebuilds a Python object for every type in the schema
    on each access, which this was paying per tag, per query.
    """
    from pylon.query import _get_schema

    return _get_schema().type_name_for_tag(tag)


def _is_disabled_for_sets(compiled: CompiledQuery, config: CacheConfig) -> bool:
    """True if any tag on *compiled* belongs to a set with an explicit
    ``[cache.sets.<Name>] enabled = false`` override."""
    if not config.sets:
        return False
    for tag in compiled.tags:
        set_name = _resolve_set_name_for_tag(tag)
        if set_name is None:
            continue
        override = config.sets.get(set_name)
        if override is not None and not override.enabled:
            return True
    return False


def _cache_key(compiled: CompiledQuery, params: list[Any], *, kind: str) -> str:
    """*kind* namespaces the hash so different callers compiling the exact
    same PyQL text/params to different cached *value shapes* (a decoded row
    list for `query`/`query_single` vs. a raw JSON string for the
    `*_json` methods) never collide on the same key.

    Keys on ``compiled.shape_id`` rather than ``compiled.sql``: the shape id
    already identifies the statement (it is a hash of exactly that SQL plus
    its result shape) and is computed once at compile time, so this no longer
    copies the whole SQL text into a Python string and re-hashes it — twice
    per query, once for the lookup and once for the store.
    """
    from pylon._core import cache_key

    return cache_key(f'{kind}\x00{compiled.shape_id}', list(params))


def _is_cacheable(compiled: CompiledQuery, config: CacheConfig) -> bool:
    """Whether this query's result may be stored or served from the cache.

    A *mutating* statement never may. `client.query("insert Person {...}")`
    is a normal way to insert and read the new row back, but its result is
    not a function of its inputs: serving a cached one returns a stale id
    *and skips the write entirely*, so running the same insert twice
    silently produced one row instead of two.
    """
    if not _enabled or not config.enabled:
        return False
    if compiled.mutates:
        return False
    return not _is_disabled_for_sets(compiled, config)


def get(compiled: CompiledQuery, params: list[Any], config: CacheConfig) -> list[Any] | None:
    """Returns the cached rows, ready to pass straight to
    `pylon.query.deserialize` exactly like a live query result — or ``None``
    on a cache miss or when caching is disabled (globally, for a mutating
    statement, or for the sets this query touches)."""
    if not _is_cacheable(compiled, config):
        return None
    from pylon._core import cache_get

    key = _cache_key(compiled, params, kind='rows')
    return cache_get(key)


def put(compiled: CompiledQuery, params: list[Any], rows: list[Any], config: CacheConfig) -> None:
    """Caches *rows* under a key derived from *compiled* + *params*, tagged
    with ``compiled.tags`` for later invalidation. No-op if caching is
    disabled or the query has no tags to key eviction on."""
    if not compiled.tags or not _is_cacheable(compiled, config):
        return
    from pylon._core import cache_put

    key = _cache_key(compiled, params, kind='rows')
    cache_put(key, list(compiled.tags), rows)


def get_json(compiled: CompiledQuery, params: list[Any], config: CacheConfig, *, kind: str) -> tuple[bool, str | None]:
    """Returns ``(hit, value)`` for the JSON-string-returning query methods
    (`query_json`/`query_single_json`). ``hit`` distinguishes a genuine
    cache hit from a miss independently of ``value``, since
    `query_single_json` legitimately caches ``None`` for an empty result.
    *kind* must differ between `query_json` (``"json_all"``) and
    `query_single_json` (``"json_single"``) — same underlying SQL, but a
    JSON array vs. at most one JSON object are different cached values."""
    if not _is_cacheable(compiled, config):
        return False, None
    from pylon._core import cache_get

    key = _cache_key(compiled, params, kind=kind)
    rows = cache_get(key)
    if rows is None:
        return False, None
    return True, (rows[0] if rows else None)


def put_json(compiled: CompiledQuery, params: list[Any], value: str | None, config: CacheConfig, *, kind: str) -> None:
    """Counterpart to `get_json` — stores *value* (or nothing, for a
    legitimately-empty `query_single_json` result) under a key namespaced
    by *kind*. No-op if caching is disabled or the query has no tags."""
    if not compiled.tags or not _is_cacheable(compiled, config):
        return
    from pylon._core import cache_put

    key = _cache_key(compiled, params, kind=kind)
    cache_put(key, list(compiled.tags), [value] if value is not None else [])


def invalidate_for(compiled: CompiledQuery) -> None:
    """Evict this process's cached entries for the tables *compiled* writes to.

    Cross-process invalidation runs over `NOTIFY pylon_cache_invalidate` and
    needs a listener, but a client's *own* writes must not need one: without
    this, a process that writes and then re-runs an identical read gets its
    own pre-write result back, and nothing in a plain script (no worker, no
    server) ever corrects it.

    Takes no `CacheConfig`: eviction only ever *removes* entries, so it is
    safe whenever the cache is open, and correctness shouldn't depend on
    per-set enable flags matching between the write and the read that
    populated the entry.
    """
    if not _enabled or not compiled.mutates or not compiled.tags:
        return
    from pylon._core import cache_invalidate

    cache_invalidate(list(compiled.tags))


def stat() -> dict[str, int] | None:
    """Returns ``{"entry_count": int, "used_bytes": int}`` for the `pylon
    cache status` CLI command, or ``None`` if the cache isn't open (either
    ``[cache].enabled = false`` or `init` hasn't been called)."""
    if not _enabled:
        return None
    from pylon._core import cache_stat

    return cache_stat()


def clear() -> None:
    """Evicts every cache entry — for the `pylon cache purge` CLI command.
    No-op if the cache isn't open."""
    if not _enabled:
        return
    from pylon._core import cache_clear

    cache_clear()


__all__ = [
    'NOTIFY_CHANNEL',
    'clear',
    'get',
    'get_json',
    'init',
    'put',
    'put_json',
    'stat',
]
