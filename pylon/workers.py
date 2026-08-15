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

"""The background workers that have to run in a Python process.

Two do: the signal dispatcher, because a `@pylon.signal` handler is a live
Python callable that only exists here, and cache invalidation, because it
evicts from an LMDB environment on local disk and so has to reach the cache
some nearby process actually reads. Everything else — vector and search
indexing — claims outbox rows the database arbitrates, can therefore run
anywhere, and runs in `pylon-server`.

`pylon worker start` runs both of these in a process of its own. This module
is the same pair as an importable API, for an application that already has
an event loop and would rather run them beside it than deploy a second
process. `run_workers` is the entry point:

    @asynccontextmanager
    async def lifespan(app):
        await client.ensure_connected()
        async with run_workers():
            yield

Placement is the whole point for the cache worker. `[cache]` is a file with
no expiry, so an invalidator only keeps a cache correct if it is attached to
*that* cache — by sharing either the process (`shared_cache` below) or the
filesystem path (a separate process on the same machine or volume, which
LMDB supports). A worker with neither is not a partial safeguard: it evicts
from a cache nobody reads, while every reader of the real one keeps serving
the write it never saw, for as long as the process lives.
"""

from __future__ import annotations

import asyncio
import logging
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Collection, Coroutine

    from pylon._core import SchemaDescriptor
    from pylon.config import Config

__all__ = ['WORKER_KINDS', 'build_worker_tasks', 'run_workers']

#: Every worker this module can start, as accepted by the `disabled`
#: argument of `build_worker_tasks`/`run_workers` (and mirrored by
#: `pylon worker start`'s `--disable-*` flags).
WORKER_KINDS = ('cache', 'signals')


def _check_disabled(disabled: Collection[str]) -> frozenset[str]:
    """Normalize and validate a `disabled` argument.

    Rejects an unknown kind rather than ignoring it: a silently-misspelled
    `'caches'` would leave the worker it was meant to suppress running, and
    the whole reason to disable one is that something else is meant to be
    covering it.
    """
    from pylon.exceptions import InterfaceError

    kinds = frozenset(disabled)
    unknown = sorted(kinds - frozenset(WORKER_KINDS))
    if unknown:
        raise InterfaceError(
            f'unknown worker kind(s) {", ".join(repr(k) for k in unknown)}; '
            f'expected any of {", ".join(repr(k) for k in WORKER_KINDS)}'
        )
    return kinds


def _resolve(schema: SchemaDescriptor | None, config: Config | None) -> tuple[SchemaDescriptor, Config]:
    """Fill in whichever of schema/config the caller left out.

    Note the ordering this implies for an application that also holds a
    `Client`: connecting replaces the process-level schema singleton with
    the one the database was last migrated to (see
    `client._install_migrated_schema`). Resolving the schema here *after*
    `ensure_connected()` therefore hands the workers the same migrated
    schema the application's own queries compile against, rather than
    whatever the local `.py` files currently declare.
    """
    if config is None:
        from pylon.config import load_config

        config = load_config()
    if schema is None:
        import pylon
        from pylon.query import _get_schema

        pylon.finalize()
        schema = _get_schema()
    return schema, config


def _warn_unclaimed_indexes(schema: SchemaDescriptor, log: logging.Logger) -> None:
    """Note any index whose outbox rows nothing in this process will drain.

    An unindexed row is invisible: the write succeeds, the outbox row lands,
    and the only symptom is a search result that never appears. Since the
    worker that would drain it lives in a different binary entirely, say so
    once at startup rather than leaving it to be discovered from a query
    returning less than it should.
    """
    kinds = set()
    for td in schema.types:
        if td.vector_indexes:
            kinds.add('VectorIndex')
        if td.search_indexes:
            kinds.add('SearchIndex')
    if kinds:
        log.info(
            '%s declared in the schema — those outboxes are drained by pylon-server, not here',
            '/'.join(sorted(kinds)),
        )


def build_worker_tasks(
    schema: SchemaDescriptor | None = None,
    config: Config | None = None,
    *,
    batch_size: int = 50,
    poll_interval: float = 30.0,
    log: logging.Logger | None = None,
    shared_cache: bool = False,
    disabled: Collection[str] = (),
) -> list[Coroutine[Any, Any, None]]:
    """Build the worker coroutines `schema`/`config` imply, without running them.

    Prefer `run_workers` unless you need to own the tasks yourself — these
    coroutines must be scheduled by the caller, and a worker that stops
    being polled is indistinguishable from one that was never started.

    Args:
        schema: Defaults to the process schema singleton (see `_resolve`).
        config: Defaults to the `pylon.toml` found from the working tree.
        batch_size: Signal-outbox rows claimed per polling cycle.
        poll_interval: Seconds between polls when the outbox is empty.
        log: Where the per-worker startup lines go.
        shared_cache: True when this process has already opened the LMDB
            cache (any process holding a connected `Client` has). The
            cache-invalidation worker then attaches to that handle instead
            of opening a second one, which LMDB refuses within a single
            process. Leave False in a process with no cache of its own —
            the worker opens `[cache].path` itself, and its evictions are
            visible to every other process mapping the same file.
        disabled: Worker kinds to skip, from `WORKER_KINDS` — for a
            deployment where something else already runs them.
    """
    log = log or logging.getLogger(__name__)
    skip = _check_disabled(disabled)
    schema, config = _resolve(schema, config)

    want_cache = config.cache.enabled and 'cache' not in skip
    if want_cache and shared_cache:
        # Checked up front, before a single coroutine exists: raising from
        # the middle of the build would leave any worker already
        # constructed unawaited, which surfaces as a "coroutine was never
        # awaited" warning pointing at this module rather than at the
        # calling-order mistake that caused it.
        from pylon.cache import is_initialized

        if not is_initialized():
            from pylon.exceptions import InterfaceError

            raise InterfaceError(
                'shared_cache=True requires this process to have opened the cache already, '
                'which happens when a Client connects — await Client.ensure_connected() '
                'before starting the workers, or pass shared_cache=False to have the worker '
                'open [cache].path itself.'
            )

    if 'signals' not in skip:
        from pylon.schema._registry import signals_snapshot

        want_signals = bool(signals_snapshot())
    else:
        want_signals = False

    db = config.database
    dsn = db.dsn or f'postgresql://{db.user}:{db.password}@{db.host}:{db.port}/{db.name}'

    _warn_unclaimed_indexes(schema, log)

    tasks: list = []

    if want_cache and shared_cache:
        from pylon._core import run_cache_invalidation_worker_shared
        from pylon.cache import NOTIFY_CHANNEL

        # This process already opened the LMDB handle for its own
        # read-through cache — attach to that same handle rather than
        # opening a second one (LMDB refuses a second Env::open on the same
        # path within one process).
        log.info('CacheInvalidationWorker started (shared cache)  channel=%s', NOTIFY_CHANNEL)
        tasks.append(run_cache_invalidation_worker_shared(dsn))
    elif want_cache:
        from pylon._core import run_cache_invalidation_worker
        from pylon.cache import NOTIFY_CHANNEL

        # Opens its own LMDB handle onto the file-backed cache at
        # config.cache.path. LMDB supports safe concurrent multi-process
        # access to one file, so this process evicting entries is
        # immediately visible to every process mapping the same path.
        log.info('CacheInvalidationWorker started  channel=%s', NOTIFY_CHANNEL)
        tasks.append(run_cache_invalidation_worker(dsn, str(config.cache.path), config.cache.max_size_mb))

    if want_signals:
        from pylon.signals import run_signal_dispatcher

        # Python, unlike every other worker, because it has to hold a live
        # reference to each registered `@pylon.signal` handler — which only
        # exists in this process.
        log.info(
            'Signal dispatcher started  batch_size=%d  poll_interval=%.0fs',
            batch_size,
            poll_interval,
        )
        tasks.append(run_signal_dispatcher(dsn, batch_size=batch_size, poll_interval=poll_interval))

    return tasks


@asynccontextmanager
async def run_workers(
    schema: SchemaDescriptor | None = None,
    config: Config | None = None,
    *,
    batch_size: int = 50,
    poll_interval: float = 30.0,
    log: logging.Logger | None = None,
    shared_cache: bool = True,
    disabled: Collection[str] = (),
) -> AsyncIterator[list[asyncio.Task]]:
    """Run the background workers for the duration of the `async with` block.

    Built for an ASGI lifespan, where the framework owns the entry point and
    the workers have to live beside it:

        @asynccontextmanager
        async def lifespan(app):
            await client.ensure_connected()
            async with run_workers():
                yield

    Arguments are `build_worker_tasks`'s, except that `shared_cache`
    defaults to True here — a process running its workers inline is one that
    holds a `Client`, and therefore one that already has the cache open.

    Yields the started tasks, for a caller that wants to inspect them. They
    are cancelled and awaited on the way out; a worker that raised rather
    than being cancelled re-raises here, since a worker that died silently
    at startup is a cache that quietly stops being invalidated.
    """
    coros = build_worker_tasks(
        schema,
        config,
        batch_size=batch_size,
        poll_interval=poll_interval,
        log=log,
        shared_cache=shared_cache,
        disabled=disabled,
    )
    tasks = [asyncio.ensure_future(c) for c in coros]
    try:
        yield tasks
    finally:
        for task in tasks:
            task.cancel()
        results = await asyncio.gather(*tasks, return_exceptions=True)
        failures = [r for r in results if isinstance(r, BaseException) and not isinstance(r, asyncio.CancelledError)]
        if failures:
            raise failures[0]
