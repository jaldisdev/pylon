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

import asyncio
import json
import re
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Any

from pylon.config import Config
from pylon.exceptions import (
    ClientConnectionClosedError,
    InterfaceError,
    InternalServerError,
    MissingParameterError,
    NoDataError,
    PylonError,
    ResultCardinalityError,
    Rollback,
    TransactionDeadlockError,
    TransactionSerializationError,
    UnknownParameterError,
)

if TYPE_CHECKING:
    from pylon._core import CompiledQuery, PgconPool, PgconTransaction


# ---------------------------------------------------------------------------
# Transaction
# ---------------------------------------------------------------------------


class AsyncTransaction:
    """Wraps a `pgcon` transaction handle.

    Obtain one via :meth:`Client.transaction`, never construct directly.
    The underlying connection is already `BEGIN`-ed by the time this wraps
    it (`PgconPool.transaction()` does both in one call), so `__aenter__`
    has nothing left to start.

    ``_retry_exc`` is set by ``__aexit__`` when the failure is retriable
    (serialisation failure or deadlock).  :class:`RetryingTransaction`
    inspects this flag in ``__anext__`` to decide whether to loop again.

    Raising :class:`~pylon.exceptions.Rollback` inside the block rolls back
    and exits quietly — the exception is suppressed and the loop ends.
    """

    def __init__(self, tx: PgconTransaction) -> None:
        self._tx = tx
        self._retry_exc: Exception | None = None

    async def __aenter__(self) -> AsyncTransaction:
        return self

    async def __aexit__(self, exc_type: type | None, exc: BaseException | None, tb: object) -> bool:
        if exc_type is None:
            # Happy path — attempt commit. `self._tx.commit()` already
            # raises the correctly-mapped `pylon.exceptions.*` instance
            # (see `pgcon_err` in `pgcon.rs`) — no further translation
            # needed here — the driver already raises the mapped class.
            try:
                await self._tx.commit()
            except (TransactionSerializationError, TransactionDeadlockError) as e:
                self._retry_exc = e
                raise
        else:
            # Always roll back on any error.
            await self._tx.rollback()
            if isinstance(exc, Rollback):
                # A deliberate abort, not a failure: swallow it so the
                # caller's code continues past the block, and leave
                # `_retry_exc` unset so the loop stops instead of re-running
                # a body that asked not to be committed.
                return True
            if isinstance(exc, (TransactionSerializationError, TransactionDeadlockError)):
                self._retry_exc = exc  # type: ignore[assignment]

        return False

    # ------------------------------------------------------------------
    # Query helpers — identical signatures to Client
    # ------------------------------------------------------------------

    @staticmethod
    def _evict(compiled: Any) -> None:
        """Drop cache entries a write invalidated.

        Applies to *every* method, not just `execute`: `query("insert ...")`
        is a normal way to insert and read the new row back, and
        `Client.save` uses `query_single` for exactly that. A write reaching
        the database through any of them has to evict, or a later identical
        read is served the pre-write result forever.

        A transaction never *populates* the cache — those rows aren't
        committed yet — so an aborted attempt can only over-evict, which
        costs a re-read and never serves stale data.
        """
        from pylon import cache as _cache

        _cache.invalidate_for(compiled)

    async def _run_compiled(self, compiled: CompiledQuery, params: list[Any]) -> list[Any]:
        """Run an already-compiled statement — what a script's statements are."""
        rows = await self._tx.query_compiled(compiled, params)
        self._evict(compiled)
        return rows

    async def query(self, pyql: str, *args: Any, **kwargs: Any) -> list[Any]:
        """Execute *pyql* and return all results as a list."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        self._evict(compiled)
        return _hydrate(rows, compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Return at most one result, or ``None``."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        self._evict(compiled)
        if len(rows) > 1:
            raise ResultCardinalityError(f'query_single expected at most one result, got {len(rows)}.')
        if not rows:
            return None
        return _hydrate(rows, compiled)[0]

    async def query_required_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any:
        """Return exactly one result; raise if the set is empty or has >1 row."""
        result = await self.query_single(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError('query_required_single returned an empty result set.')
        return result

    async def execute(self, pyql: str, *args: Any, **kwargs: Any) -> None:
        """Execute a mutation (INSERT / UPDATE / DELETE); discard the result."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        await self._tx.execute_compiled(compiled, params)
        self._evict(compiled)

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled_json_agg(compiled, params)
        self._evict(compiled)
        return rows[0] if rows else '[]'

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Return at most one result as a JSON string, or ``None``."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        self._evict(compiled)
        if len(rows) > 1:
            raise ResultCardinalityError(f'query_single_json expected at most one result, got {len(rows)}.')
        if not rows:
            return None
        json_rows = await self._tx.query_compiled_row_to_json(compiled, params)
        return json_rows[0] if json_rows else None

    async def query_required_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Return exactly one result as a JSON string; raise if the set is empty."""
        result = await self.query_single_json(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError('query_required_single_json returned an empty result set.')
        return result


# ---------------------------------------------------------------------------
# RetryingTransaction
# ---------------------------------------------------------------------------


class RetryingTransaction:
    """Async iterator returned by :meth:`Client.transaction`.

    Each call to ``__anext__`` acquires a fresh pool connection and yields
    an :class:`AsyncTransaction`.  After ``async with tx:`` exits, the
    iterator inspects ``tx._retry_exc``:

    - ``None``  → committed successfully (or deliberately rolled back with
      :class:`~pylon.exceptions.Rollback`) → ``StopAsyncIteration``
    - retriable exception → back-off and yield a new transaction
    - budget exhausted → re-raise the last retriable exception

    Do not construct directly — use ``client.transaction()``.
    """

    def __init__(self, client: Client, *, attempts: int, isolation: str) -> None:
        # The client rather than its pool: `Client.transaction()` is sync, so
        # a pool that is not open yet can only be opened once the first
        # attempt is awaited.
        self._client = client
        self._attempts = attempts
        self._isolation = isolation
        self._attempt = 0
        self._prev_tx: AsyncTransaction | None = None

    def __aiter__(self) -> RetryingTransaction:
        return self

    async def __anext__(self) -> AsyncTransaction:
        # Inspect the outcome of the previous attempt. Unlike the old
        # pool, there's no separate release step: `commit`/
        # `rollback` on the pgcon transaction handle already consume the
        # underlying connection and return it to the pool themselves.
        if self._prev_tx is not None:
            if self._prev_tx._retry_exc is None:
                raise StopAsyncIteration

            if self._attempt >= self._attempts:
                raise self._prev_tx._retry_exc

            # Exponential back-off: attempt 1 → 0 ms, 2 → 100 ms, 3 → 200 ms, …
            await asyncio.sleep((self._attempt - 1) * 0.1)

        pool = await self._client._connected_pool()
        pgcon_tx = await pool.transaction(self._isolation)
        tx = AsyncTransaction(pgcon_tx)
        self._prev_tx = tx
        self._attempt += 1
        return tx


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------


# `analyze` is a soft keyword (see pylon-core's parser) — legal as a leading
# statement token only; `Client.analyze()` mirrors that by accepting a query
# with or without it already written, rather than requiring callers to
# remember to type it themselves.
_ANALYZE_PREFIX_RE = re.compile(r'(?is)^analyze\b')


class _PoolRef:
    """Shared mutable pool holder so Client.with_globals() siblings stay in sync."""

    __slots__ = ('closed', 'lock', 'pool')

    def __init__(self) -> None:
        self.pool: PgconPool | None = None
        self.lock = asyncio.Lock()
        # Tells a client that was closed from one that has never connected:
        # without it, a query after `aclose()` would quietly open a new pool.
        self.closed = False


class Client:
    """Async Pylon client — pgcon (Rust driver) pool wrapper with PyQL transpilation.

    Args:
        config: A :class:`~pylon.config.Config` instance.  When omitted the
                client attempts to load ``pylon.toml`` from the working tree.
    """

    def __init__(self, config: Config | None = None, *, warnings: bool = True) -> None:
        if config is None:
            from pylon.config import load_config

            config = load_config()
        self._config = config
        self._ref = _PoolRef()
        self._warnings = warnings
        self._globals: dict[str, Any] = {}
        self._config_options: dict[str, Any] = {}

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def ensure_connected(self) -> None:
        """Initialise the connection pool if it has not been created yet.

        Safe to call multiple times; subsequent calls are no-ops.
        """
        async with self._ref.lock:
            if self._ref.pool is not None:
                return
            self._ref.closed = False
            dsn = self._config.database.dsn or _build_dsn(self._config.database)
            # Swap the pylon:// scheme for postgresql:// if present.
            dsn = dsn.replace('pylon://', 'postgresql://', 1)
            # `pgcon_connect` already raises the correctly-mapped
            # `ConnectionFailedError`/`ConnectionTimeoutError` itself (see
            # `pgcon_connect_err` in `pgcon.rs`) — no try/except needed here
            # anymore. Note: `pool_min_size` isn't passed through — deadpool
            # (the pgcon pool implementation) has no eager pre-warm concept
            # up front; connections are created lazily on
            # demand up to `pool_max_size` instead. The field is still
            # accepted/validated on `DatabaseConfig` for config-surface
            # compatibility, just not enforced at the connection layer.
            from pylon._core import pgcon_connect

            self._ref.pool = await pgcon_connect(dsn, self._config.database.pool_max_size)
            await _check_internal_schema(self._ref.pool)
            from pylon import cache as _cache

            _cache.init(self._config.cache)
            await _install_migrated_schema(self._ref.pool)

    async def aclose(self) -> None:
        """Close the connection pool and release all resources."""
        async with self._ref.lock:
            self._ref.closed = True
            if self._ref.pool is not None:
                self._ref.pool = None

    # Support ``async with Client(config) as client:``
    async def __aenter__(self) -> Client:
        await self.ensure_connected()
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.aclose()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _require_pool(self) -> PgconPool:
        if self._ref.pool is None:
            raise ClientConnectionClosedError('Client is not connected. Call await client.ensure_connected() first.')
        return self._ref.pool

    async def _connected_pool(self) -> PgconPool:
        """The pool, opening it on first use.

        A client is reached from request handlers, background workers and CLI
        commands alike, which share no startup between them to connect from,
        so requiring an explicit `ensure_connected()` means every one of those
        entry points has to remember — and the ones that forget fail on their
        first query rather than at startup. `ensure_connected` is idempotent
        and takes the lock itself, so once connected this costs one check.

        A client that has been closed stays closed; only one that was never
        connected opens here.
        """
        if self._ref.pool is None:
            if self._ref.closed:
                raise ClientConnectionClosedError('Client is closed.')
            await self.ensure_connected()
        return self._require_pool()

    # ------------------------------------------------------------------
    # Query interface
    # ------------------------------------------------------------------

    def with_globals(self, globals_: dict[str, Any]) -> Client:
        """Return a client view that injects *globals_* into every query.

        The returned client shares the same connection pool.  Globals are
        keyed by their qualified name (``"module::name"``).

        Usage::

            authed = client.with_globals({"default::current_user_id": user_id})
            posts = await authed.query("select Post { title }")
        """
        c = Client.__new__(Client)
        c._config = self._config
        c._ref = self._ref
        c._warnings = self._warnings
        c._globals = {**self._globals, **globals_}
        c._config_options = self._config_options
        return c

    def with_config(self, options: dict[str, Any]) -> Client:
        """Return a client view that applies session config *options* to every query.

        The returned client shares the same connection pool. See
        ``pylon.config_options`` for the registry of known option names/
        defaults. An unrecognized option name is stored but has no effect
        (only names ``pylon.query.compile()`` actually consumes change
        compilation), matching ``with_globals()``'s own unvalidated-merge
        behavior.

        Usage::

            unsafe = client.with_config({"allow_user_specified_id": True})
            await unsafe.query("insert Person { id := <uuid>$id, name := $name }", id=..., name=...)
        """
        c = Client.__new__(Client)
        c._config = self._config
        c._ref = self._ref
        c._warnings = self._warnings
        c._globals = self._globals
        c._config_options = {**self._config_options, **options}
        return c

    async def _run_script(self, pyql: str, kwargs: dict[str, Any]) -> list[Any]:
        """Run a script's statements in order and return the last one's rows.

        Postgres cannot take several statements with parameters in one round
        trip, so each is sent on its own — inside a transaction, so a script
        either lands whole or not at all, which is how a reader of the source
        would expect several statements written together to behave.
        """
        from pylon._core import compile_script as _compile_script
        from pylon.query import _get_schema

        statements = _compile_script(
            pyql,
            _get_schema(),
            allow_user_specified_id=bool(self._config_options.get('allow_user_specified_id', False)),
        )
        rows: list[Any] = []
        last = statements[-1]
        # The arguments are the script's, not any one statement's — checked
        # once against every parameter it declares, so a statement does not
        # report its neighbour's parameter as an extra argument.
        script_params: set[str] = set()
        for compiled in statements:
            script_params |= _declared_params(compiled)
        _check_arguments(script_params, kwargs)
        async for tx in self.transaction():
            async with tx:
                for compiled in statements:
                    if self._warnings:
                        _emit_warnings(compiled)
                    rows = await tx._run_compiled(
                        compiled, _bind_positional(compiled, kwargs, self._globals, declared=script_params)
                    )
        return _hydrate(rows, last)

    async def query(self, pyql: str, *args: Any, **kwargs: Any) -> list[Any]:
        """Execute *pyql* and return all matching objects as a list."""
        merged = _merge_args(args, kwargs)
        if _looks_like_a_script(pyql):
            return await self._run_script(pyql, merged)
        pool = await self._connected_pool()
        compiled, params = await _compile_and_resolve(
            pyql, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        if self._warnings:
            _emit_warnings(compiled)

        from pylon import cache as _cache

        cached = _cache.get(compiled, params, self._config.cache)
        if cached is not None:
            return _hydrate(cached, compiled)

        # `pool.query_compiled` already raises the correctly-mapped
        # `pylon.exceptions.*` instance on failure (see `pgcon_err` in
        # `pgcon.rs`) — no exception translation needed here.
        rows = await pool.query_compiled(compiled, params)
        _cache.put(compiled, params, rows, self._config.cache)
        # `client.query("insert ... ")` is a normal way to insert and read the
        # row back, so a read path can be a write path too.
        _cache.invalidate_for(compiled)
        return _hydrate(rows, compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Execute *pyql* and return at most one result, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more
        than one object matches.
        """
        pool = await self._connected_pool()
        compiled, params = await _compile_and_resolve(
            pyql, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        if self._warnings:
            _emit_warnings(compiled)

        from pylon import cache as _cache

        cached = _cache.get(compiled, params, self._config.cache)
        if cached is not None:
            if len(cached) > 1:
                raise ResultCardinalityError(f'query_single expected at most one result, got {len(cached)}.')
            if not cached:
                return None
            return _hydrate(cached, compiled)[0]

        rows = await pool.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(f'query_single expected at most one result, got {len(rows)}.')
        _cache.put(compiled, params, rows, self._config.cache)
        _cache.invalidate_for(compiled)
        if not rows:
            return None
        return _hydrate(rows, compiled)[0]

    async def query_required_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any:
        """Execute *pyql* and return exactly one result.

        Raises :class:`~pylon.exceptions.NoDataError` if the set is empty.
        Raises :class:`~pylon.exceptions.ResultCardinalityError` if >1 row.
        """
        result = await self.query_single(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError('query_required_single returned an empty result set.')
        return result

    async def execute(self, pyql: str, *args: Any, **kwargs: Any) -> None:
        """Execute a mutation (INSERT / UPDATE / DELETE); discard the result."""
        pool = await self._connected_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)
        await pool.execute_compiled(compiled, params)
        from pylon import cache as _cache

        _cache.invalidate_for(compiled)

    async def listen(self, channel: str) -> AsyncGenerator[Any]:
        """Listen for `NOTIFY` payloads on a schema-declared `Channel`.

        Opens a dedicated (non-pooled) connection for the lifetime of the
        returned async generator — a `LISTEN` registration is per-session,
        so running it on a pooled connection would leak the subscription
        onto whatever unrelated query later borrows that same connection
        back out of the pool. The dedicated connection (and the server-side
        subscription with it) closes automatically once iteration stops —
        breaking out of the loop, an unhandled exception, or the generator
        being garbage-collected all drop the last reference to it.

        Yields decoded payloads matching the Channel's own declared shape
        (see `pylon.schema._channels.decode_channel_payload`): a bare
        `uuid.UUID` for a Type-shaped channel (the changed row's `id`, not
        a fetched object — see `docs/schema/channels.md`), the declared
        scalar's native Python value for a Scalar-shaped channel, or a
        `pylon.Object` for an Object-shaped channel. A payload that doesn't
        actually match the declared shape raises
        :class:`~pylon.exceptions.QueryError` — the loop ends there rather
        than silently skipping the bad payload.

        *channel* is a bare or `module::name` reference, matching how a
        schema author already writes it inside a `notify(...)` call.

        Usage::

            async for payload in client.listen("UserUpdates"):
                print(payload)
        """
        from pylon._core import pgcon_listen
        from pylon.query import _get_schema
        from pylon.schema._channels import decode_channel_payload, resolve_channel

        schema = _get_schema()
        ch = resolve_channel(schema, channel)

        dsn = self._config.database.dsn or _build_dsn(self._config.database)
        dsn = dsn.replace('pylon://', 'postgresql://', 1)
        conn = await pgcon_listen(dsn)

        queue: asyncio.Queue[str] = asyncio.Queue()
        await conn.add_listener(ch.wire_name, lambda *args: queue.put_nowait(args[-1]))

        while True:
            raw_payload = await queue.get()
            yield decode_channel_payload(ch, raw_payload)

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        pool = await self._connected_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)

        from pylon import cache as _cache

        hit, cached = _cache.get_json(compiled, params, self._config.cache, kind='json_all')
        if hit:
            return cached if cached is not None else '[]'

        rows = await pool.query_compiled_json_agg(compiled, params)
        value = rows[0] if rows else '[]'
        _cache.put_json(compiled, params, value, self._config.cache, kind='json_all')
        _cache.invalidate_for(compiled)
        return value

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Execute *pyql* and return at most one result as a JSON string, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more than one
        object matches.
        """
        pool = await self._connected_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)

        from pylon import cache as _cache

        hit, cached = _cache.get_json(compiled, params, self._config.cache, kind='json_single')
        if hit:
            return cached

        rows = await pool.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(f'query_single_json expected at most one result, got {len(rows)}.')
        if not rows:
            _cache.put_json(compiled, params, None, self._config.cache, kind='json_single')
            _cache.invalidate_for(compiled)
            return None
        json_rows = await pool.query_compiled_row_to_json(compiled, params)
        value = json_rows[0] if json_rows else None
        _cache.put_json(compiled, params, value, self._config.cache, kind='json_single')
        _cache.invalidate_for(compiled)
        return value

    async def query_required_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return exactly one result as a JSON string.

        Raises :class:`~pylon.exceptions.NoDataError` if the set is empty.
        Raises :class:`~pylon.exceptions.ResultCardinalityError` if >1 row.
        """
        result = await self.query_single_json(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError('query_required_single_json returned an empty result set.')
        return result

    async def analyze(self, pyql: str, *args: Any, **kwargs: Any) -> dict[str, Any]:
        """Run *pyql* through Postgres's `EXPLAIN (ANALYZE, FORMAT JSON)` and
        return a query plan grouped by the query's own shape (its root
        select, each nested link, etc.) instead of raw SQL relation names.

        *pyql* doesn't need the leading ``analyze`` keyword already written —
        it's added automatically if missing, so ``client.analyze("select
        Person { name }")`` and ``client.analyze("analyze select Person {
        name }")`` behave identically. The returned dict is the
        coarse-grained tree (``pylon_core::analyze::CoarseGrainedNode``,
        Rust-side): ``path``, ``marker_offset``, ``relations``, ``cost``,
        ``children`` (each a ``{"name": ..., "node": {...}}`` entry).
        """
        pool = await self._connected_pool()
        normalized = pyql if _ANALYZE_PREFIX_RE.match(pyql.lstrip()) else f'analyze {pyql}'
        compiled, params = await _compile_and_resolve(
            normalized, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        raw_json = await pool.analyze_compiled(compiled, params)
        return json.loads(raw_json)

    async def save(self, *objs: Any) -> None:
        """Insert or update each of *objs* — instances of `@pylon.type`
        classes — in a single transaction.

        A "new" instance (never hydrated from a query result, i.e. no
        `__pylon_saved__` shadow — see `pylon.query._decode`) is INSERTed
        and has its generated ``id`` assigned back. A hydrated instance is
        diffed against the values it was loaded with and UPDATEd only if
        something actually changed (a no-op is skipped entirely). See
        `pylon.modelquery.prepare_save` for the diffing/rendering logic.
        """
        from pylon import modelquery

        # The `__pylon_saved__` shadow is only refreshed once the whole
        # transaction has actually committed (after the retry loop below
        # exits normally) — refreshing it per-statement, inside the loop,
        # would make a *retried* attempt (serialization failure/deadlock —
        # a rolled-back attempt that reruns this same body) see a diff of
        # zero for objects it already "saved" on the failed attempt, and
        # silently skip re-issuing their INSERT/UPDATE.
        # Unsaved link targets are written before the objects that reference
        # them, so constructing a graph and saving the root writes the whole
        # graph. See `modelquery.save_order` for why these stay separate
        # statements rather than one composed statement.
        ordered = modelquery.save_order(objs)

        # Generated ids *are* assigned inside the transaction, because a
        # later statement in the same attempt needs them to reference the
        # rows earlier statements just wrote. That makes them attempt-local
        # state: an id from a rolled-back attempt names a row that doesn't
        # exist, and leaving it in place would make the next attempt render
        # `insert T { id := <that id>, ... }` — which is rejected outright
        # unless `allow_user_specified_id` is on, turning a retryable
        # serialization failure into a hard error. So they're cleared at the
        # start of every attempt, and again if the whole save gives up.
        pending_inserts = [obj for obj in ordered if obj.__dict__.get('id') is None]

        def _discard_generated_ids() -> None:
            for obj in pending_inserts:
                obj.__dict__['id'] = None

        try:
            async for tx in self.transaction():
                _discard_generated_ids()
                async with tx:
                    for obj in ordered:
                        prepared = modelquery.prepare_save(obj)
                        if prepared is None:
                            continue
                        pyql, params = prepared
                        is_new = '__pylon_saved__' not in obj.__dict__
                        if is_new:
                            result = await tx.query_single(pyql, **params)
                            obj.id = result.id
                        else:
                            await tx.execute(pyql, **params)
        except BaseException:
            # Nothing committed, so no object should claim a database id.
            _discard_generated_ids()
            raise

        for obj in ordered:
            modelquery.mark_saved(obj)

    # ------------------------------------------------------------------
    # Transaction
    # ------------------------------------------------------------------

    def transaction(
        self,
        *,
        attempts: int = 3,
        isolation: str = 'serializable',
    ) -> RetryingTransaction:
        """Return an async iterator that drives a retrying transaction loop.

        Each iteration yields a fresh :class:`AsyncTransaction`.  Wrap it in
        ``async with tx:`` to commit or roll back.  On a serialisation failure
        or deadlock the iterator retries automatically up to *attempts* times,
        with exponential back-off between attempts (0 ms, 100 ms, 200 ms, …).

        Args:
            attempts:  Maximum number of attempts before re-raising (default 3).
            isolation: PostgreSQL isolation level — ``"serializable"`` (default),
                       ``"repeatable_read"``, or ``"read_committed"``.

        Usage::

            async for tx in client.transaction():
                async with tx:
                    obj = await tx.query_single('SELECT ...')
                    await tx.execute('INSERT ...')

        With custom retry budget::

            async for tx in client.transaction(attempts=5, isolation="repeatable_read"):
                async with tx:
                    ...

        Raise :class:`~pylon.exceptions.Rollback` to discard the work
        instead of committing it — useful for a test that wants to write,
        read its own writes, and leave nothing behind::

            async for tx in client.transaction():
                async with tx:
                    await tx.execute('insert Person { name := "Ada" }')
                    raise Rollback
        """
        if attempts < 1:
            raise InterfaceError('attempts must be >= 1.')
        return RetryingTransaction(self, attempts=attempts, isolation=isolation)

    # ------------------------------------------------------------------
    # Raw access (escape hatch)
    # ------------------------------------------------------------------

    @asynccontextmanager
    async def raw_connection(self) -> AsyncGenerator[PgconPool]:
        """Yield the underlying pgcon pool handle for queries outside PyQL.

        Use sparingly — this bypasses the transpiler entirely. Exposes
        ``query(sql, params)``/``execute(sql, params)`` (positional ``$1,
        $2, ...`` params) — ``query`` decodes column 0 of every row
        regardless of its name, so a bare scalar expression (e.g.
        ``SELECT count(*) AS n``) works the same as PyQL's own ``result``
        column convention.
        """
        pool = await self._connected_pool()
        yield pool

    # ------------------------------------------------------------------
    # Repr
    # ------------------------------------------------------------------

    def __repr__(self) -> str:
        state = 'connected' if self._ref.pool is not None else 'disconnected'
        db = self._config.database
        target = db.dsn or f'{db.host}:{db.port}/{db.name}'
        return f'<Client [{state}] {target}>'


# ---------------------------------------------------------------------------
# Module-level convenience
# ---------------------------------------------------------------------------


def create_async_client(config: Config | None = None) -> Client:
    """Return a new :class:`Client` instance.

    The pool is *not* initialised here; call
    ``await client.ensure_connected()`` (or use the client as an async
    context manager) before issuing queries.

    Args:
        config: Optional :class:`~pylon.config.Config`.  Omit to auto-load
                ``pylon.toml`` from the working tree.
    """
    return Client(config)


# ---------------------------------------------------------------------------
# Private helpers
# ---------------------------------------------------------------------------


def _emit_warnings(compiled: CompiledQuery) -> None:
    import warnings as _warnings

    for msg in compiled.warnings():
        _warnings.warn(msg, stacklevel=4)


def _record_compile(success: bool) -> None:
    """Records a compile-stage outcome in `pylon_queries_total{stage="compile"}`
    (see `pylon-workers::metrics`) — called from `_compile_and_resolve`/
    `_compile_and_bind`, the actual query-serving compile step, not every
    `pylon.query.compile()` caller (the LSP compiles too, but that isn't a
    served query)."""
    from pylon._core import record_query_compile_result

    record_query_compile_result(success)


async def _compile_and_resolve(
    pyql: str,
    kwargs: dict[str, Any],
    config: Config,
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple[CompiledQuery, list[Any]]:
    """Compile PyQL and, for OpenSearch-backed queries, perform the HTTP phase first.

    Returns ``(compiled, params)`` ready for ``pool.query_compiled``/
    ``pool.execute_compiled`` — callers never need to read ``compiled.sql``
    themselves; the fused pgcon methods read it directly out of ``compiled``.
    ``config_options`` mirrors ``Client.with_config()`` — see ``pylon.config_options``.
    """
    pyql, kwargs = _normalize_pyql_source(pyql, kwargs)
    from pylon.query import compile as _pyql_compile

    try:
        compiled = _pyql_compile(
            pyql,
            allow_user_specified_id=bool((config_options or {}).get('allow_user_specified_id', False)),
        )
    except PylonError:
        _record_compile(False)
        # Already a real pylon.exceptions.* class (InvalidQueryError,
        # UnknownLinkError, ...) with position/query attached by
        # pyql_err/_from_transpiler on the Rust side — let it propagate
        # as-is instead of relabeling every compile error InternalServerError.
        raise
    except BaseException as exc:
        _record_compile(False)
        raise InternalServerError(str(exc)) from exc
    _record_compile(True)

    plan = compiled.inference_plan
    if plan is None:
        # Normal path — bind params from kwargs/globals the standard way.
        return compiled, _bind_positional(compiled, kwargs, globals_)

    query_text = _resolve_query_text(plan, kwargs)

    if plan['kind'] == 'search':
        # fts::search deferred path — fetch (id, score) pairs, inject as array params.
        search_cfg = config.search_registry.get('default')
        if search_cfg is None:
            raise InterfaceError('fts::search requires [search] config in pylon.toml')
        base_url = f'http://{search_cfg.host}:{search_cfg.port}'
        size = plan['size'] or 100
        if plan['backend'] == 'meilisearch':
            from pylon.search.meilisearch import MeilisearchClient

            async with MeilisearchClient(base_url, api_key=search_cfg.api_key) as client:
                hits = await client.search(plan['index_name'], query_text or '', size=size)
        else:
            from pylon.search.opensearch import OpenSearchClient

            auth = (search_cfg.user, search_cfg.password) if search_cfg.user else None
            async with OpenSearchClient(base_url, auth=auth) as client:
                hits = await client.search(plan['index_name'], query_text or '', size=size)
        ids = [h[0] for h in hits]
        scores = [h[1] for h in hits]
        extra = {'__deferred_ids__': ids, '__deferred_scores__': scores}

    else:
        # vector::search text overload — embed the query text (via Rust's
        # pylon-providers HTTP client), inject as __deferred_vec__.
        model_cfg = _resolve_model_config(plan['model_name'], config)
        from pylon._core import embed_text

        vector = await embed_text(
            model_cfg.api_style,
            model_cfg.api_url,
            model_cfg.model,
            query_text or '',
            api_key=model_cfg.secret,
        )
        extra = {'__deferred_vec__': vector}

    params = [extra[name] for name in compiled.param_names]
    return compiled, params


def _resolve_query_text(plan: dict, kwargs: dict) -> str | None:
    query_text: str | None = plan['query_literal']
    if query_text is None:
        param_name = plan['query_param_name']
        query_text = kwargs.get(param_name) if param_name else None
        if query_text is None and param_name:
            raise InterfaceError(f"Missing query parameter '{param_name}'")
    return query_text


def _resolve_model_config(model_name: str, config: Config):
    from pylon.config import ModelConfig

    models = config.models
    if models is None:
        raise InterfaceError('vector::search with text query requires [models] config in pylon.toml')
    if isinstance(models, ModelConfig):
        return models
    cfg = models.get(model_name)
    if cfg is None:
        raise InterfaceError(f"vector::search: no model config found for '{model_name}' in pylon.toml")
    return cfg


def _merge_args(args: tuple, kwargs: dict[str, Any]) -> dict[str, Any]:
    if not args:
        return kwargs
    return {str(i): v for i, v in enumerate(args)} | kwargs


def _normalize_pyql_source(pyql: Any, kwargs: dict[str, Any]) -> tuple[str, dict[str, Any]]:
    """Accepts a raw PyQL string unchanged, or a `@pylon.type` class /
    `pylon.modelquery.ModelSet` (from `client.query(Model)`,
    `Model.filter(...)`, `Model.filter(...).delete()`) — rendered to PyQL
    text via `pylon.modelquery.render()`, with its generated `$__mq_pN`
    params merged into the caller's own kwargs. Raises the same
    `InterfaceError` as before for anything else.
    """
    if isinstance(pyql, str):
        return pyql, kwargs
    from pylon import modelquery

    rendered = modelquery.render(pyql)
    if rendered is None:
        raise InterfaceError(f'PyQL query must be a str, got {type(pyql).__name__!r}.')
    text, extra_params = rendered
    return text, {**kwargs, **extra_params}


def _check_arguments(expected: set[str], kwargs: dict[str, Any]) -> None:
    """Refuse arguments the query does not declare, and vice versa.

    the upstream engine rejects a stray argument rather than ignoring it, and a silently
    dropped one hides what it usually is: a condition that was edited out, or
    a name that no longer matches. The wording is the upstream engine's own, from
    `_make_missing_args_error_message` in its `object.pyx` codec, so a message
    carried over from an older log or test still reads the same.
    """
    passed = set(kwargs)
    if not expected:
        if passed:
            raise UnknownParameterError('expected no named arguments')
    elif expected != passed:
        missed = expected - passed
        extra = passed - expected
        message = f'expected {expected} arguments, got {passed if passed else "nothing"}'
        if missed:
            message += f', missed {missed}'
        if extra:
            message += f', extra {extra}'
        raise MissingParameterError(message) if missed else UnknownParameterError(message)


def _declared_params(compiled: CompiledQuery) -> set[str]:
    """The names `compiled` expects from the caller — globals come from the
    session, so they are not the caller's to supply."""
    return {name for name in compiled.param_names if not name.startswith('__global__')}


def _bind_positional(
    compiled: CompiledQuery,
    kwargs: dict[str, Any],
    globals_: dict[str, Any] | None = None,
    declared: set[str] | None = None,
) -> list[Any]:
    """Positional params for `compiled`, drawn from `kwargs` and `globals_`.

    `declared` overrides what the arguments are checked against, for a script:
    its arguments belong to the script as a whole, so a statement must not
    call another statement's parameter an extra one.
    """
    _check_arguments(_declared_params(compiled) if declared is None else declared, kwargs)
    return [
        (globals_ or {}).get(name[len('__global__') :]) if name.startswith('__global__') else kwargs[name]
        for name in compiled.param_names
    ]


def _compile_and_bind(
    pyql: str,
    kwargs: dict[str, Any],
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple[CompiledQuery, list[Any]]:
    """Compile PyQL and resolve positional params — never reads ``compiled.sql``.

    Returns ``(compiled, params)`` ready for ``pool.query_compiled``/
    ``execute_compiled``. ``param_names`` entries prefixed with ``__global__``
    are filled from ``globals_``; all others from ``kwargs``.
    ``config_options`` mirrors ``Client.with_config()`` — see
    ``pylon.config_options``. Doesn't handle inference-plan queries — those
    still go through ``_compile_and_resolve``.
    """
    pyql, kwargs = _normalize_pyql_source(pyql, kwargs)
    from pylon.query import compile as _pyql_compile

    try:
        compiled = _pyql_compile(
            pyql,
            allow_user_specified_id=bool((config_options or {}).get('allow_user_specified_id', False)),
        )
    except PylonError:
        _record_compile(False)
        # See the matching comment in _compile_and_resolve above.
        raise
    except BaseException as exc:
        _record_compile(False)
        raise InternalServerError(str(exc)) from exc
    _record_compile(True)
    return compiled, _bind_positional(compiled, kwargs, globals_)


def _looks_like_a_script(pyql: str) -> bool:
    """Whether the source may hold more than one statement.

    Only a hint, to keep the ordinary path off the script route: a semicolon
    inside a string literal makes this say yes, and compiling the script then
    reports one statement anyway. A trailing semicolon on a single statement
    says no.
    """
    return ';' in pyql.strip().rstrip(';')


def _hydrate(rows: list[Any], compiled: CompiledQuery) -> list[Any]:
    """Decode raw result rows into Python dataclass instances.

    Runs the native walk (`pylon._core.hydrate`). `pylon.query.deserialize`
    remains the readable reference implementation of the same contract, and
    ``tests/test_hydrate_parity.py`` holds the two to it.
    """
    from pylon._core import hydrate as _native_hydrate
    from pylon.query import _get_schema, hydration_registry

    try:
        _get_schema()
    except RuntimeError:
        # Without a schema there is nothing to decode into, so hand back
        # plain values — converting the RowSet, since callers in this state
        # expect an ordinary list they can index and iterate.
        return rows.to_list() if hasattr(rows, 'to_list') else rows
    return _native_hydrate(rows, compiled, hydration_registry())


def _build_dsn(db: Any) -> str:
    """Construct a DSN string from discrete :class:`~pylon.config.DatabaseConfig` fields."""
    password_part = f':{db.password}' if db.password else ''
    return f'postgresql://{db.user}{password_part}@{db.host}:{db.port}/{db.name}'


async def _check_internal_schema(pool: PgconPool) -> None:
    """Refuse to use a database whose internal `_pylon` schema this build
    cannot work against.

    Raised rather than warned, and raised *here* rather than left to
    surface later: a client that connects happily and then fails somewhere
    deep in a query gives no hint that the real problem is a database one
    upgrade behind. That is exactly how a missing internal column presents
    — as an unrelated-looking error, far from its cause. A warning in an
    application's logs would not be read until someone was already
    debugging that.

    Deliberately does *not* repair anything. A client holds an application
    role that may well lack DDL rights, and even where it doesn't, having
    every process that happens to connect mutate shared internal structures
    is how a rolling deploy turns into an outage. `pylon migration apply`
    stays the only writer.

    A database no migration has ever run against reports nothing, matching
    `_install_migrated_schema`'s own tolerance for that state.
    """
    import logging

    from pylon._core import migration_check_internal_schema
    from pylon.exceptions import ConnectionFailedError

    fatal, message = await migration_check_internal_schema(pool)
    if message is None:
        return
    if fatal:
        raise ConnectionFailedError(message)
    logging.getLogger(__name__).warning('%s', message)


async def _install_migrated_schema(pool: PgconPool) -> None:
    """Installs the *migrated* schema (`_pylon."Schema"`, written by
    `migration apply`/`watch`) as the process-level singleton every query
    on this connection compiles against — overriding whatever
    `pylon.finalize()` built from the current `.py` files.

    This is what makes a parser-only, no-DDL-footprint schema change (e.g.
    a property's `readonly` flag — enforced only by the Rust compiler
    consulting `SchemaDescriptor`, never a real Postgres constraint) have
    no effect on a running app until a migration is actually applied: even
    though `pylon.finalize()` already installed the *new* declaration at
    process startup, connecting overwrites it with whatever the database
    itself was last migrated to. Without this, editing `schema.py` and
    restarting the process would be enough to change enforcement, with no
    migration required at all — the same gap `pylon-client` (Rust),
    `pylon-server`, and `pylon-lsp` were already built not to have.

    A no-op if no migration has ever been applied to this database (a
    brand-new/unmigrated target — `_pylon."Schema"` doesn't exist yet):
    whatever `pylon.finalize()` installed is left in place, matching
    `pylon-lsp`'s own graceful-degradation behavior for the same case.
    """
    from pylon._core import SchemaDescriptor, migration_read_schema_snapshot
    from pylon.exceptions import QueryError, SchemaError
    from pylon.query import _set_schema

    try:
        snapshot_json = await migration_read_schema_snapshot(pool)
    except QueryError as exc:
        if getattr(exc, 'sqlstate', None) == '42P01':  # undefined_table
            return
        raise
    if snapshot_json is None:
        return

    try:
        _set_schema(SchemaDescriptor.from_json(snapshot_json))
    except ValueError as exc:
        # The stored snapshot is written by whichever Pylon last ran
        # `migration apply`/`watch`. If that was an older version, the JSON
        # can be missing fields this one requires, and serde reports it as
        # `missing field 'x' at line 1 column N` — a byte offset into a
        # blob the reader never sees, with no hint that the fix is to
        # re-run a migration.
        raise SchemaError(
            f'the schema snapshot stored in this database cannot be read by this version of Pylon '
            f'({exc}). It was written by an older version whose format differs. Re-run '
            f'`pylon migration apply` (or `pylon migration watch` in development) against this '
            f'database to rewrite it.'
        ) from exc
