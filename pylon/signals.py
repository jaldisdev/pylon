"""Post-commit signal dispatch — drains `_pylon."SignalOutbox"` and invokes
every registered `@pylon.signal` handler for each row's (type, operation).

Deliberately a plain Python `asyncio` loop, not one of the Rust-native
`pylon.worker` loops (cache invalidation, vector/search indexing): those
moved entirely into Rust specifically to avoid GIL/asyncio entanglement,
but a registered signal handler is a live Python callable (see
`pylon.schema._signal_registry`) with no equivalent on the Rust side —
there's nothing to hand off to. It still reuses the same low-level
connection primitive those Rust workers use internally
(`pylon._core.pgcon_listen`/`PgconListener`), so no new Rust surface is
needed here.
"""

from __future__ import annotations

import asyncio
import uuid
from typing import Any

from pylon._core import pgcon_listen
from pylon.schema._signal_registry import handlers_for
from pylon.schema._triggers import On

_CLAIM_SQL = """
UPDATE _pylon."SignalOutbox"
SET status = 'Processing'
WHERE id IN (
    SELECT id FROM _pylon."SignalOutbox"
    WHERE status = 'Pending'
      AND (next_attempt IS NULL OR next_attempt <= now())
    ORDER BY enqueued_at
    LIMIT $1
    FOR UPDATE SKIP LOCKED
)
RETURNING id, type_name, operation, old_row, new_row, attempts
"""

_MARK_DONE_SQL = 'DELETE FROM _pylon."SignalOutbox" WHERE id = $1'

_MARK_FAILED_SQL = """
UPDATE _pylon."SignalOutbox"
SET status = CASE WHEN attempts >= 5 THEN 'Failed' ELSE 'Pending' END,
    attempts = attempts + 1,
    next_attempt = now() + (30 * 2 ^ LEAST(attempts, 4) || ' seconds')::interval
WHERE id = $1
"""

_OPERATION_TO_ON = {"INSERT": On.Insert, "UPDATE": On.Update, "DELETE": On.Delete}


def _qualified_type_registry() -> dict[str, type]:
    """`{"module::Name": cls, ...}` for every `@pylon.type`-decorated class
    currently registered — built fresh each call (registration doesn't
    change after `pylon.finalize()` has run), mirroring how
    `pylon.client._hydrate` builds its own short-name registry."""
    from pylon.schema._registry import snapshot

    types, _enums, _custom_scalars = snapshot()
    return {f"{t.__pylon_config__.module}::{t.__name__}": t for t in types}


def _hydrate(cls: type, row: dict[str, Any] | None) -> Any:
    """Build a real instance of `cls` from a raw `old_row`/`new_row`
    snapshot (JSONB-decoded: column name -> value). Not
    `pylon.query._decode()` — that needs a compiled `ShapeNode`, which
    doesn't exist at a trigger capture site. Bypasses `__init__` the same
    way `_decode()` does. `id` and every single-link `<name>_id` column
    are coerced from their JSON string form back to `uuid.UUID` (JSON has
    no native UUID type); every other column is passed through as-is —
    computed pointers and multilinks are never present in `row` at all
    (no backing column to have captured), matching a database trigger's
    own `OLD`/`NEW` scope exactly.
    """
    if row is None:
        return None
    cfg = cls.__pylon_config__
    uuid_keys = {"id"} | {f"{name}_id" for name in cfg.pointers if cfg.pointers[name].kind == "link"}
    obj = object.__new__(cls)
    obj.__dict__.update({
        key: uuid.UUID(value) if key in uuid_keys and isinstance(value, str) else value
        for key, value in row.items()
    })
    return obj


async def _process_row(row: dict[str, Any], type_registry: dict[str, type]) -> None:
    type_name = row["type_name"]
    on = _OPERATION_TO_ON.get(row["operation"])
    cls = type_registry.get(type_name)
    if on is None or cls is None:
        # A signal was removed (or its target type renamed/dropped) after
        # this row was already queued but before the DDL/trigger caught
        # up — nothing to dispatch to; drop it rather than retry forever.
        return
    old = _hydrate(cls, row["old_row"])
    new = _hydrate(cls, row["new_row"])
    for handler in handlers_for(type_name, on):
        await handler(old, new)


async def run_signal_dispatcher(dsn: str, *, batch_size: int = 50, poll_interval: float = 5.0) -> None:
    """Drain `_pylon."SignalOutbox"` forever, dispatching each row to every
    registered `@pylon.signal` handler matching its `(type, operation)`.

    Structurally mirrors the Rust index/search/vector workers' claim loop
    (`FOR UPDATE SKIP LOCKED` + `LISTEN`/`NOTIFY` plus a poll-interval
    fallback), just in Python — see the module docstring for why.
    """
    conn = await pgcon_listen(dsn)
    woken = asyncio.Event()
    await conn.add_listener("pylon_signal_queue", lambda *_args: woken.set())

    while True:
        drained_any = False
        while True:
            claimed = await conn.query_named(_CLAIM_SQL, [batch_size])
            if not claimed:
                break
            drained_any = True
            type_registry = _qualified_type_registry()
            for row in claimed:
                try:
                    await _process_row(row, type_registry)
                except Exception as exc:  # noqa: BLE001 - one bad handler must not kill the loop
                    print(f"pylon.signals: handler failed for {row['type_name']} {row['operation']}: {exc}")
                    await conn.execute(_MARK_FAILED_SQL, [row["id"]])
                else:
                    await conn.execute(_MARK_DONE_SQL, [row["id"]])
            if len(claimed) < batch_size:
                break

        if not drained_any:
            woken.clear()
            try:
                await asyncio.wait_for(woken.wait(), timeout=poll_interval)
            except asyncio.TimeoutError:
                pass
