from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Any

from pylon.config import Config
from pylon.exceptions import (
    ClientConnectionClosedError,
    InterfaceError,
    InternalServerError,
    NoDataError,
    PylonError,
    ResultCardinalityError,
    TransactionDeadlockError,
    TransactionSerializationError,
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
    """

    def __init__(self, tx: "PgconTransaction") -> None:
        self._tx = tx
        self._retry_exc: Exception | None = None

    async def __aenter__(self) -> "AsyncTransaction":
        return self

    async def __aexit__(
        self, exc_type: type | None, exc: BaseException | None, tb: object
    ) -> bool:
        if exc_type is None:
            # Happy path — attempt commit. `self._tx.commit()` already
            # raises the correctly-mapped `pylon.exceptions.*` instance
            # (see `pgcon_err` in `pgcon.rs`) — no further translation
            # needed here, unlike the old asyncpg-native exception mapping.
            try:
                await self._tx.commit()
            except (TransactionSerializationError, TransactionDeadlockError) as e:
                self._retry_exc = e
                raise
        else:
            # Always roll back on any error.
            await self._tx.rollback()
            if isinstance(exc, (TransactionSerializationError, TransactionDeadlockError)):
                self._retry_exc = exc  # type: ignore[assignment]

        return False

    # ------------------------------------------------------------------
    # Query helpers — identical signatures to Client
    # ------------------------------------------------------------------

    async def query(self, pyql: str, *args: Any, **kwargs: Any) -> list[Any]:
        """Execute *pyql* and return all results as a list."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        return _hydrate([{"result": row} for row in rows], compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Return at most one result, or ``None``."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        return _hydrate([{"result": row} for row in rows], compiled)[0]

    async def query_required_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any:
        """Return exactly one result; raise if the set is empty or has >1 row."""
        result = await self.query_single(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError("query_required_single returned an empty result set.")
        return result

    async def execute(self, pyql: str, *args: Any, **kwargs: Any) -> None:
        """Execute a mutation (INSERT / UPDATE / DELETE); discard the result."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        await self._tx.execute_compiled(compiled, params)

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled_json_agg(compiled, params)
        return rows[0] if rows else "[]"

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Return at most one result as a JSON string, or ``None``."""
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs))
        rows = await self._tx.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single_json expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        json_rows = await self._tx.query_compiled_row_to_json(compiled, params)
        return json_rows[0] if json_rows else None

    async def query_required_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Return exactly one result as a JSON string; raise if the set is empty."""
        result = await self.query_single_json(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError(
                "query_required_single_json returned an empty result set."
            )
        return result


# ---------------------------------------------------------------------------
# RetryingTransaction
# ---------------------------------------------------------------------------


class RetryingTransaction:
    """Async iterator returned by :meth:`Client.transaction`.

    Each call to ``__anext__`` acquires a fresh pool connection and yields
    an :class:`AsyncTransaction`.  After ``async with tx:`` exits, the
    iterator inspects ``tx._retry_exc``:

    - ``None``  → committed successfully → ``StopAsyncIteration``
    - retriable exception → back-off and yield a new transaction
    - budget exhausted → re-raise the last retriable exception

    Do not construct directly — use ``client.transaction()``.
    """

    def __init__(self, pool: "PgconPool", *, attempts: int, isolation: str) -> None:
        self._pool = pool
        self._attempts = attempts
        self._isolation = isolation
        self._attempt = 0
        self._prev_tx: AsyncTransaction | None = None

    def __aiter__(self) -> "RetryingTransaction":
        return self

    async def __anext__(self) -> AsyncTransaction:
        # Inspect the outcome of the previous attempt. Unlike the old
        # asyncpg-based pool, there's no separate release step: `commit`/
        # `rollback` on the pgcon transaction handle already consume the
        # underlying connection and return it to the pool themselves.
        if self._prev_tx is not None:
            if self._prev_tx._retry_exc is None:
                raise StopAsyncIteration

            if self._attempt >= self._attempts:
                raise self._prev_tx._retry_exc

            # Exponential back-off: attempt 1 → 0 ms, 2 → 100 ms, 3 → 200 ms, …
            await asyncio.sleep((self._attempt - 1) * 0.1)

        pgcon_tx = await self._pool.transaction(self._isolation)
        tx = AsyncTransaction(pgcon_tx)
        self._prev_tx = tx
        self._attempt += 1
        return tx


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------


class _PoolRef:
    """Shared mutable pool holder so Client.with_globals() siblings stay in sync."""
    __slots__ = ("pool", "lock")

    def __init__(self) -> None:
        self.pool: "PgconPool | None" = None
        self.lock = asyncio.Lock()


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
            dsn = self._config.database.dsn or _build_dsn(self._config.database)
            # Swap the pylon:// scheme for postgresql:// if present.
            dsn = dsn.replace("pylon://", "postgresql://", 1)
            # `pgcon_connect` already raises the correctly-mapped
            # `ConnectionFailedError`/`ConnectionTimeoutError` itself (see
            # `pgcon_connect_err` in `pgcon.rs`) — no try/except needed here
            # anymore. Note: `pool_min_size` isn't passed through — deadpool
            # (the pgcon pool implementation) has no eager pre-warm concept
            # the way asyncpg's pool did; connections are created lazily on
            # demand up to `pool_max_size` instead. The field is still
            # accepted/validated on `DatabaseConfig` for config-surface
            # compatibility, just not enforced at the connection layer.
            from pylon._core import pgcon_connect
            self._ref.pool = await pgcon_connect(dsn, self._config.database.pool_max_size)
            from pylon import cache as _cache
            _cache.init(self._config.cache)

    async def aclose(self) -> None:
        """Close the connection pool and release all resources."""
        async with self._ref.lock:
            if self._ref.pool is not None:
                self._ref.pool = None

    # Support ``async with Client(config) as client:``
    async def __aenter__(self) -> "Client":
        await self.ensure_connected()
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.aclose()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _require_pool(self) -> "PgconPool":
        if self._ref.pool is None:
            raise ClientConnectionClosedError(
                "Client is not connected. Call await client.ensure_connected() first."
            )
        return self._ref.pool

    # ------------------------------------------------------------------
    # Query interface
    # ------------------------------------------------------------------

    def with_globals(self, globals_: dict[str, Any]) -> "Client":
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

    def with_config(self, options: dict[str, Any]) -> "Client":
        """Return a client view that applies session config *options* to every query.

        The returned client shares the same connection pool. Mirrors Gel's
        session config (``configure session set ...``) — see
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

    async def query(self, pyql: str, *args: Any, **kwargs: Any) -> list[Any]:
        """Execute *pyql* and return all matching objects as a list."""
        pool = self._require_pool()
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
        records = [{"result": row} for row in rows]
        _cache.put(compiled, params, records, self._config.cache)
        return _hydrate(records, compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Execute *pyql* and return at most one result, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more
        than one object matches.
        """
        pool = self._require_pool()
        compiled, params = await _compile_and_resolve(
            pyql, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        if self._warnings:
            _emit_warnings(compiled)

        from pylon import cache as _cache
        cached = _cache.get(compiled, params, self._config.cache)
        if cached is not None:
            if len(cached) > 1:
                raise ResultCardinalityError(
                    f"query_single expected at most one result, got {len(cached)}."
                )
            if not cached:
                return None
            return _hydrate(cached, compiled)[0]

        rows = await pool.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single expected at most one result, got {len(rows)}."
            )
        records = [{"result": row} for row in rows]
        _cache.put(compiled, params, records, self._config.cache)
        if not records:
            return None
        return _hydrate(records, compiled)[0]

    async def query_required_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any:
        """Execute *pyql* and return exactly one result.

        Raises :class:`~pylon.exceptions.NoDataError` if the set is empty.
        Raises :class:`~pylon.exceptions.ResultCardinalityError` if >1 row.
        """
        result = await self.query_single(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError("query_required_single returned an empty result set.")
        return result

    async def execute(self, pyql: str, *args: Any, **kwargs: Any) -> None:
        """Execute a mutation (INSERT / UPDATE / DELETE); discard the result."""
        pool = self._require_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)
        await pool.execute_compiled(compiled, params)

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        pool = self._require_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)

        from pylon import cache as _cache
        hit, cached = _cache.get_json(compiled, params, self._config.cache, kind="json_all")
        if hit:
            return cached if cached is not None else "[]"

        rows = await pool.query_compiled_json_agg(compiled, params)
        value = rows[0] if rows else "[]"
        _cache.put_json(compiled, params, value, self._config.cache, kind="json_all")
        return value

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Execute *pyql* and return at most one result as a JSON string, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more than one
        object matches.
        """
        pool = self._require_pool()
        compiled, params = _compile_and_bind(pyql, _merge_args(args, kwargs), self._globals, self._config_options)

        from pylon import cache as _cache
        hit, cached = _cache.get_json(compiled, params, self._config.cache, kind="json_single")
        if hit:
            return cached

        rows = await pool.query_compiled(compiled, params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single_json expected at most one result, got {len(rows)}."
            )
        if not rows:
            _cache.put_json(compiled, params, None, self._config.cache, kind="json_single")
            return None
        json_rows = await pool.query_compiled_row_to_json(compiled, params)
        value = json_rows[0] if json_rows else None
        _cache.put_json(compiled, params, value, self._config.cache, kind="json_single")
        return value

    async def query_required_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return exactly one result as a JSON string.

        Raises :class:`~pylon.exceptions.NoDataError` if the set is empty.
        Raises :class:`~pylon.exceptions.ResultCardinalityError` if >1 row.
        """
        result = await self.query_single_json(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError(
                "query_required_single_json returned an empty result set."
            )
        return result

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
        async for tx in self.transaction():
            async with tx:
                for obj in objs:
                    prepared = modelquery.prepare_save(obj)
                    if prepared is None:
                        continue
                    pyql, params = prepared
                    is_new = "__pylon_saved__" not in obj.__dict__
                    if is_new:
                        result = await tx.query_single(pyql, **params)
                        obj.id = result.id
                    else:
                        await tx.execute(pyql, **params)
        for obj in objs:
            cfg = type(obj).__pylon_config__
            obj.__dict__["__pylon_saved__"] = {
                name: obj.__dict__[name]
                for name, meta in cfg.pointers.items()
                if meta.kind == "property" and name in obj.__dict__
            }

    # ------------------------------------------------------------------
    # Transaction
    # ------------------------------------------------------------------

    def transaction(
        self,
        *,
        attempts: int = 3,
        isolation: str = "serializable",
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
        """
        if attempts < 1:
            raise InterfaceError("attempts must be >= 1.")
        return RetryingTransaction(
            self._require_pool(), attempts=attempts, isolation=isolation
        )

    # ------------------------------------------------------------------
    # Raw access (escape hatch)
    # ------------------------------------------------------------------

    @asynccontextmanager
    async def raw_connection(self) -> AsyncGenerator["PgconPool", None]:
        """Yield the underlying pgcon pool handle for queries outside PyQL.

        Use sparingly — this bypasses the transpiler entirely. Exposes
        ``query(sql, params)``/``execute(sql, params)`` (positional ``$1,
        $2, ...`` params) — ``query`` decodes column 0 of every row
        regardless of its name, so a bare scalar expression (e.g.
        ``SELECT count(*) AS n``) works the same as PyQL's own ``result``
        column convention.
        """
        pool = self._require_pool()
        yield pool

    # ------------------------------------------------------------------
    # Repr
    # ------------------------------------------------------------------

    def __repr__(self) -> str:
        state = "connected" if self._ref.pool is not None else "disconnected"
        db = self._config.database
        target = db.dsn or f"{db.host}:{db.port}/{db.name}"
        return f"<Client [{state}] {target}>"


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


def _emit_warnings(compiled: "CompiledQuery") -> None:
    import warnings as _warnings
    for msg in compiled.warnings():
        _warnings.warn(msg, stacklevel=4)


async def _compile_and_resolve(
    pyql: str,
    kwargs: dict[str, Any],
    config: "Config",
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple["CompiledQuery", list[Any]]:
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
            allow_user_specified_id=bool((config_options or {}).get("allow_user_specified_id", False)),
        )
    except PylonError:
        # Already a real pylon.exceptions.* class (InvalidQueryError,
        # UnknownLinkError, ...) with position/query attached by
        # pyql_err/_from_transpiler on the Rust side — let it propagate
        # as-is instead of relabeling every compile error InternalServerError.
        raise
    except BaseException as exc:
        raise InternalServerError(str(exc)) from exc

    plan = compiled.inference_plan
    if plan is None:
        # Normal path — bind params from kwargs/globals the standard way.
        try:
            params: list[Any] = []
            for name in compiled.param_names:
                if name.startswith("__global__"):
                    qname = name[len("__global__"):]
                    params.append((globals_ or {}).get(qname))
                else:
                    params.append(kwargs[name])
        except KeyError as exc:
            raise InterfaceError(f"Missing query parameter: {exc}") from exc
        return compiled, params

    query_text = _resolve_query_text(plan, kwargs)

    if plan["kind"] == "search":
        # fts::search deferred path — fetch (id, score) pairs, inject as array params.
        search_cfg = config.search_registry.get("default")
        if search_cfg is None:
            raise InterfaceError(
                "fts::search requires [search] config in pylon.toml"
            )
        base_url = f"http://{search_cfg.host}:{search_cfg.port}"
        size = plan["size"] or 100
        if plan["backend"] == "meilisearch":
            from pylon.search.meilisearch import MeilisearchClient
            async with MeilisearchClient(base_url, api_key=search_cfg.api_key) as client:
                hits = await client.search(plan["index_name"], query_text or "", size=size)
        else:
            from pylon.search.opensearch import OpenSearchClient
            auth = (search_cfg.user, search_cfg.password) if search_cfg.user else None
            async with OpenSearchClient(base_url, auth=auth) as client:
                hits = await client.search(plan["index_name"], query_text or "", size=size)
        ids = [h[0] for h in hits]
        scores = [h[1] for h in hits]
        extra = {"__deferred_ids__": ids, "__deferred_scores__": scores}

    else:
        # vector::search text overload — embed the query text (via Rust's
        # pylon-providers HTTP client), inject as __deferred_vec__.
        model_cfg = _resolve_model_config(plan["model_name"], config)
        from pylon._core import embed_text
        vector = await embed_text(
            model_cfg.api_style, model_cfg.api_url, model_cfg.model, query_text or "",
            api_key=model_cfg.secret,
        )
        extra = {"__deferred_vec__": vector}

    params = [extra[name] for name in compiled.param_names]
    return compiled, params


def _resolve_query_text(plan: dict, kwargs: dict) -> str | None:
    query_text: str | None = plan["query_literal"]
    if query_text is None:
        param_name = plan["query_param_name"]
        query_text = kwargs.get(param_name) if param_name else None
        if query_text is None and param_name:
            raise InterfaceError(f"Missing query parameter '{param_name}'")
    return query_text


def _resolve_model_config(model_name: str, config: "Config"):
    from pylon.config import ModelConfig
    models = config.models
    if models is None:
        raise InterfaceError(
            "vector::search with text query requires [models] config in pylon.toml"
        )
    if isinstance(models, ModelConfig):
        return models
    cfg = models.get(model_name)
    if cfg is None:
        raise InterfaceError(
            f"vector::search: no model config found for '{model_name}' in pylon.toml"
        )
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
        raise InterfaceError(f"PyQL query must be a str, got {type(pyql).__name__!r}.")
    text, extra_params = rendered
    return text, {**kwargs, **extra_params}


def _compile_and_bind(
    pyql: str,
    kwargs: dict[str, Any],
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple["CompiledQuery", list[Any]]:
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
            allow_user_specified_id=bool((config_options or {}).get("allow_user_specified_id", False)),
        )
    except PylonError:
        # See the matching comment in _compile_and_resolve above.
        raise
    except BaseException as exc:
        raise InternalServerError(str(exc)) from exc
    try:
        params: list[Any] = []
        for name in compiled.param_names:
            if name.startswith("__global__"):
                qname = name[len("__global__"):]
                params.append((globals_ or {}).get(qname))
            else:
                params.append(kwargs[name])
    except KeyError as exc:
        raise InterfaceError(f"Missing query parameter: {exc}") from exc
    return compiled, params


def _transpile(
    pyql: str,
    kwargs: dict[str, Any],
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple[str, list[Any], "CompiledQuery"]:
    """Compile PyQL to SQL text — only for the JSON-wrapping paths
    (``query_json``/``query_single_json``), which still string-wrap raw SQL
    in Python until a later phase moves that wrapping into Rust too.

    Returns ``(sql, positional_params, compiled)`` ready for ``pool.query``/
    ``pool.execute``.
    """
    compiled, params = _compile_and_bind(pyql, kwargs, globals_, config_options)
    return compiled.sql, params, compiled


def _hydrate(records: list[Any], compiled: "CompiledQuery") -> list[Any]:
    """Decode ``{"result": ...}``-wrapped rows into Python dataclass instances."""
    from pylon.query import _get_schema, deserialize
    from pylon.schema import schema_snapshot
    from pylon.schema._registry import named_tuples_snapshot

    try:
        _get_schema()
    except RuntimeError:
        return records
    types, enums, _ = schema_snapshot()
    registry: dict[str, type] = {t.__name__: t for t in types}
    for nt in named_tuples_snapshot():
        mod = getattr(nt, "__pylon_module__", "default")
        registry[f"{mod}::{nt.__name__}"] = nt
    for en in enums:
        mod = getattr(en, "__pylon_module__", None) or \
            (en.__module__ or "default").rpartition(".")[-1] or "default"
        registry[en.__name__] = en
        registry[f"{mod}::{en.__name__}"] = en
    return deserialize(records, compiled, registry)


def _build_dsn(db: Any) -> str:
    """Construct a DSN string from discrete :class:`~pylon.config.DatabaseConfig` fields."""
    password_part = f":{db.password}" if db.password else ""
    return f"postgresql://{db.user}{password_part}@{db.host}:{db.port}/{db.name}"
