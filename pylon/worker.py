from __future__ import annotations

import asyncio
import logging
from enum import Enum
from typing import Any

log = logging.getLogger(__name__)

CLAIM_BATCH_SQL = """
UPDATE _pylon."IndexOutbox"
SET status = 'Processing'
WHERE id IN (
    SELECT id FROM _pylon."IndexOutbox"
    WHERE index_kind = $1::_pylon."IndexKind"
      AND status = 'Pending'
      AND (next_attempt IS NULL OR next_attempt <= now())
    ORDER BY enqueued_at
    LIMIT $2
    FOR UPDATE SKIP LOCKED
)
RETURNING id, object_id, type_name, index_name, attempts
"""

MARK_DONE_SQL = """
DELETE FROM _pylon."IndexOutbox" WHERE id = ANY($1::uuid[])
"""

MARK_FAILED_SQL = """
UPDATE _pylon."IndexOutbox"
SET status = CASE WHEN attempts >= 5
                  THEN 'Failed'::_pylon."IndexOutboxStatus"
                  ELSE 'Pending'::_pylon."IndexOutboxStatus"
             END,
    attempts = attempts + 1,
    next_attempt = now() + (30 * 2^LEAST(attempts, 4) || ' seconds')::interval
WHERE id = ANY($1::uuid[])
"""

_BACKOFF_CAP = 5


class IndexKind(str, Enum):
    VECTOR = "Vector"
    OPEN_SEARCH = "OpenSearch"
    MEILISEARCH = "Meilisearch"


class IndexWorker:
    """Base class for deferred index workers.

    Subclasses set ``index_kind`` and implement ``process_batch``.
    """

    index_kind: IndexKind
    batch_size: int = 50
    poll_interval: float = 30.0

    def __init__(self, conn: Any) -> None:
        self._conn = conn
        self._drain_lock = asyncio.Lock()

    async def run(self) -> None:
        await self._conn.add_listener("pylon_index_queue", self._on_notify)
        try:
            await self._drain()
            while True:
                await asyncio.sleep(self.poll_interval)
                await self._drain()
        finally:
            await self._conn.remove_listener("pylon_index_queue", self._on_notify)

    def _on_notify(self, _conn: Any, _pid: int, _channel: str, _payload: str) -> None:
        asyncio.ensure_future(self._drain())

    async def _drain(self) -> None:
        if self._drain_lock.locked():
            return
        async with self._drain_lock:
            while True:
                rows = await self.claim_batch(self.batch_size)
                if not rows:
                    break
                await self._process_safe(rows)
                if len(rows) < self.batch_size:
                    break

    async def _process_safe(self, rows: list[Any]) -> None:
        try:
            await self.process_batch(rows)
            ids = [r["id"] for r in rows]
            await self._conn.execute(MARK_DONE_SQL, ids)
        except Exception:
            log.exception("IndexWorker: batch failed, scheduling retry")
            ids = [r["id"] for r in rows]
            await self._conn.execute(MARK_FAILED_SQL, ids)

    async def claim_batch(self, limit: int) -> list[Any]:
        return await self._conn.fetch(CLAIM_BATCH_SQL, self.index_kind.value, limit)

    async def process_batch(self, rows: list[Any]) -> None:
        raise NotImplementedError
