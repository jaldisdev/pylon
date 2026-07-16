from __future__ import annotations

import asyncio
import logging
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pylon._core import CompiledQuery
    from pylon.config import CacheConfig

log = logging.getLogger(__name__)

NOTIFY_CHANNEL = "pylon_cache_invalidate"

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
    type name (e.g. ``"Person"``), by scanning the schema singleton's types.
    Returns None for tags with no owning type (e.g. a junction table)."""
    from pylon.query import _get_schema

    for t in _get_schema().types:
        pg_schema = "public" if t.module == "default" else t.module
        if f"{pg_schema}.{t.table}" == tag:
            return t.name
    return None


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


def _cache_key(compiled: CompiledQuery, params: list[Any]) -> str:
    from pylon._core import cache_key

    return cache_key(compiled.sql, list(params))


def get(compiled: CompiledQuery, params: list[Any], config: CacheConfig) -> list[Any] | None:
    """Returns cached rows, each already wrapped as ``{"result": row}`` so
    the list can be passed straight to `pylon.query.deserialize` exactly
    like a live asyncpg result — or ``None`` on a cache miss or when caching
    is disabled (globally or for the sets this query touches)."""
    if not _enabled or not config.enabled:
        return None
    if _is_disabled_for_sets(compiled, config):
        return None
    from pylon._core import cache_get

    key = _cache_key(compiled, params)
    rows = cache_get(key)
    if rows is None:
        return None
    return [{"result": row} for row in rows]


def put(compiled: CompiledQuery, params: list[Any], records: list[Any], config: CacheConfig) -> None:
    """Caches *records* (each an asyncpg Record with a ``result`` column)
    under a key derived from *compiled* + *params*, tagged with
    ``compiled.tags`` for later invalidation. No-op if caching is disabled
    or the query has no tags to key eviction on."""
    if not _enabled or not config.enabled or not compiled.tags:
        return
    if _is_disabled_for_sets(compiled, config):
        return
    from pylon._core import cache_put

    key = _cache_key(compiled, params)
    rows = [record["result"] for record in records]
    cache_put(key, list(compiled.tags), rows)


class CacheInvalidationWorker:
    """Listens on `NOTIFY_CHANNEL` and evicts matching cache entries.

    Unlike `pylon.worker.IndexWorker` there's no outbox table to drain from
    — the NOTIFY payload *is* the tag to invalidate (a schema-qualified
    table name, written by the `_pylon.notify_cache_invalidate()` trigger),
    so eviction happens directly from the listener callback. Best-effort:
    if a NOTIFY is ever dropped, the affected cache entries persist until
    naturally evicted or overwritten — there is no durable outbox to
    reconcile against, unlike the index queue.
    """

    def __init__(self, conn: Any) -> None:
        self._conn = conn
        self._pending: set[str] = set()
        self._drain_lock = asyncio.Lock()

    async def run(self) -> None:
        await self._conn.add_listener(NOTIFY_CHANNEL, self._on_notify)
        try:
            await asyncio.Event().wait()
        finally:
            await self._conn.remove_listener(NOTIFY_CHANNEL, self._on_notify)

    def _on_notify(self, _conn: Any, _pid: int, _channel: str, payload: str) -> None:
        self._pending.add(payload)
        asyncio.ensure_future(self._drain())

    async def _drain(self) -> None:
        if self._drain_lock.locked():
            return
        async with self._drain_lock:
            while self._pending:
                tags = list(self._pending)
                self._pending.clear()
                try:
                    await self._invalidate(tags)
                except Exception:
                    log.exception("CacheInvalidationWorker: eviction failed for tags %r", tags)

    async def _invalidate(self, tags: list[str]) -> None:
        from pylon._core import cache_invalidate

        cache_invalidate(tags)


__all__ = ["NOTIFY_CHANNEL", "init", "get", "put", "CacheInvalidationWorker"]
