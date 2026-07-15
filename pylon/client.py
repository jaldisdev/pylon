from __future__ import annotations

import asyncio
import decimal
import json
import struct
import uuid as _uuid_mod
from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager
from typing import TYPE_CHECKING, Any

import asyncpg

from pylon.config import Config
from pylon.exceptions import (
    ClientConnectionClosedError,
    ConnectionFailedError,
    ConnectionTimeoutError,
    InterfaceError,
    InternalServerError,
    NoDataError,
    QueryError,
    ResultCardinalityError,
    TransactionDeadlockError,
    TransactionSerializationError,
)

if TYPE_CHECKING:
    from pylon._core import CompiledQuery


# ---------------------------------------------------------------------------
# Transaction
# ---------------------------------------------------------------------------


class AsyncTransaction:
    """Wraps an asyncpg connection inside an explicit transaction.

    Obtain one via :meth:`Client.transaction`, never construct directly.

    ``_retry_exc`` is set by ``__aexit__`` when the failure is retriable
    (serialisation failure or deadlock).  :class:`RetryingTransaction`
    inspects this flag in ``__anext__`` to decide whether to loop again.
    """

    def __init__(
        self, conn: asyncpg.Connection, isolation: str = "serializable"
    ) -> None:
        self._conn = conn
        self._isolation = isolation
        self._tx: asyncpg.transaction.Transaction | None = None
        self._retry_exc: Exception | None = None

    async def __aenter__(self) -> "AsyncTransaction":
        self._tx = self._conn.transaction(isolation=self._isolation)
        await self._tx.start()
        return self

    async def __aexit__(
        self, exc_type: type | None, exc: BaseException | None, tb: object
    ) -> bool:
        if self._tx is None:
            return False

        if exc_type is None:
            # Happy path — attempt commit.
            try:
                await self._tx.commit()
            except asyncpg.SerializationError as e:
                mapped = TransactionSerializationError(str(e))
                self._retry_exc = mapped
                raise mapped from e
            except asyncpg.DeadlockDetectedError as e:
                mapped = TransactionDeadlockError(str(e))
                self._retry_exc = mapped
                raise mapped from e
        else:
            # Always roll back on any error.
            await self._tx.rollback()
            # Re-map asyncpg-native retriable errors to Pylon exceptions.
            if isinstance(exc, asyncpg.SerializationError):
                mapped = TransactionSerializationError(str(exc))
                self._retry_exc = mapped
                raise mapped from exc
            if isinstance(exc, asyncpg.DeadlockDetectedError):
                mapped = TransactionDeadlockError(str(exc))
                self._retry_exc = mapped
                raise mapped from exc
            # Already a Pylon retriable exception — record it for the iterator.
            if isinstance(
                exc, (TransactionSerializationError, TransactionDeadlockError)
            ):
                self._retry_exc = exc  # type: ignore[assignment]

        return False

    # ------------------------------------------------------------------
    # Query helpers — identical signatures to Client
    # ------------------------------------------------------------------

    async def query(self, pyql: str, *args: Any, **kwargs: Any) -> list[Any]:
        """Execute *pyql* and return all results as a list."""
        sql, params, compiled = _transpile(pyql, _merge_args(args, kwargs))
        records = list(await self._conn.fetch(sql, *params))
        return _hydrate(records, compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Return at most one result, or ``None``."""
        sql, params, compiled = _transpile(pyql, _merge_args(args, kwargs))
        rows = await self._conn.fetch(sql, *params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        return _hydrate(list(rows), compiled)[0]

    async def query_required_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any:
        """Return exactly one result; raise if the set is empty or has >1 row."""
        result = await self.query_single(pyql, *args, **kwargs)
        if result is None:
            raise NoDataError("query_required_single returned an empty result set.")
        return result

    async def execute(self, pyql: str, *args: Any, **kwargs: Any) -> None:
        """Execute a mutation (INSERT / UPDATE / DELETE); discard the result."""
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs))
        await self._conn.execute(sql, *params)

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs))
        return (
            await self._conn.fetchval(
                f"SELECT COALESCE(json_agg(q), '[]') FROM ({sql}) q", *params
            )
            or "[]"
        )

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Return at most one result as a JSON string, or ``None``."""
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs))
        rows = await self._conn.fetch(sql, *params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single_json expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        return await self._conn.fetchval(
            f"SELECT row_to_json(q) FROM ({sql} LIMIT 1) q", *params
        )

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

    def __init__(self, pool: asyncpg.Pool, *, attempts: int, isolation: str) -> None:
        self._pool = pool
        self._attempts = attempts
        self._isolation = isolation
        self._attempt = 0
        self._prev_tx: AsyncTransaction | None = None

    def __aiter__(self) -> "RetryingTransaction":
        return self

    async def __anext__(self) -> AsyncTransaction:
        # Inspect the outcome of the previous attempt.
        if self._prev_tx is not None:
            if self._prev_tx._retry_exc is None:
                # Committed cleanly — release the connection and stop.
                await self._pool.release(self._prev_tx._conn)
                raise StopAsyncIteration

            # Retriable failure — release the connection before deciding.
            await self._pool.release(self._prev_tx._conn)

            if self._attempt >= self._attempts:
                raise self._prev_tx._retry_exc

            # Exponential back-off: attempt 1 → 0 ms, 2 → 100 ms, 3 → 200 ms, …
            await asyncio.sleep((self._attempt - 1) * 0.1)

        conn: asyncpg.Connection = await self._pool.acquire()
        tx = AsyncTransaction(conn, isolation=self._isolation)
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
        self.pool: asyncpg.Pool | None = None
        self.lock = asyncio.Lock()


class Client:
    """Async Pylon client — asyncpg pool wrapper with PyQL transpilation.

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
            try:
                self._ref.pool = await asyncpg.create_pool(
                    dsn,
                    min_size=self._config.database.pool_min_size,
                    max_size=self._config.database.pool_max_size,
                    init=_setup_codecs,
                )
            except asyncpg.InvalidCatalogNameError as exc:
                raise ConnectionFailedError(str(exc)) from exc
            except (OSError, asyncpg.CannotConnectNowError) as exc:
                raise ConnectionFailedError(str(exc)) from exc
            except asyncio.TimeoutError as exc:
                raise ConnectionTimeoutError(
                    "Timed out while connecting to PostgreSQL."
                ) from exc

    async def aclose(self) -> None:
        """Close the connection pool and release all resources."""
        async with self._ref.lock:
            if self._ref.pool is not None:
                await self._ref.pool.close()
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

    def _require_pool(self) -> asyncpg.Pool:
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
        compiled, sql, params = await _compile_and_resolve(
            pyql, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        if self._warnings:
            _emit_warnings(compiled)
        try:
            async with pool.acquire() as conn:
                records = list(await conn.fetch(sql, *params))
        except asyncpg.SerializationError as exc:
            raise TransactionSerializationError(str(exc)) from exc
        except asyncpg.DeadlockDetectedError as exc:
            raise TransactionDeadlockError(str(exc)) from exc
        except asyncpg.PostgresError as exc:
            raise _fmt_pg_error(exc) from exc
        return _hydrate(records, compiled)

    async def query_single(self, pyql: str, *args: Any, **kwargs: Any) -> Any | None:
        """Execute *pyql* and return at most one result, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more
        than one object matches.
        """
        pool = self._require_pool()
        compiled, sql, params = await _compile_and_resolve(
            pyql, _merge_args(args, kwargs), self._config, self._globals, self._config_options
        )
        if self._warnings:
            _emit_warnings(compiled)
        try:
            async with pool.acquire() as conn:
                rows = await conn.fetch(sql, *params)
        except asyncpg.SerializationError as exc:
            raise TransactionSerializationError(str(exc)) from exc
        except asyncpg.DeadlockDetectedError as exc:
            raise TransactionDeadlockError(str(exc)) from exc
        except asyncpg.PostgresError as exc:
            raise _fmt_pg_error(exc) from exc
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        return _hydrate(list(rows), compiled)[0]

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
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs), self._globals, self._config_options)
        async with pool.acquire() as conn:
            try:
                await conn.execute(sql, *params)
            except asyncpg.SerializationError as exc:
                raise TransactionSerializationError(str(exc)) from exc
            except asyncpg.DeadlockDetectedError as exc:
                raise TransactionDeadlockError(str(exc)) from exc
            except asyncpg.PostgresError as exc:
                raise _fmt_pg_error(exc) from exc

    async def query_json(self, pyql: str, *args: Any, **kwargs: Any) -> str:
        """Execute *pyql* and return all results serialised as a JSON string.

        Returns ``"[]"`` when the result set is empty.
        """
        pool = self._require_pool()
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs), self._globals, self._config_options)
        async with pool.acquire() as conn:
            return (
                await conn.fetchval(
                    f"SELECT COALESCE(json_agg(q), '[]') FROM ({sql}) q", *params
                )
                or "[]"
            )

    async def query_single_json(self, pyql: str, *args: Any, **kwargs: Any) -> str | None:
        """Execute *pyql* and return at most one result as a JSON string, or ``None``.

        Raises :class:`~pylon.exceptions.ResultCardinalityError` if more than one
        object matches.
        """
        pool = self._require_pool()
        sql, params, _ = _transpile(pyql, _merge_args(args, kwargs), self._globals, self._config_options)
        async with pool.acquire() as conn:
            rows = await conn.fetch(sql, *params)
        if len(rows) > 1:
            raise ResultCardinalityError(
                f"query_single_json expected at most one result, got {len(rows)}."
            )
        if not rows:
            return None
        async with pool.acquire() as conn:
            return await conn.fetchval(
                f"SELECT row_to_json(q) FROM ({sql} LIMIT 1) q", *params
            )

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
    async def raw_connection(self) -> AsyncGenerator[asyncpg.Connection, None]:
        """Yield a raw asyncpg connection for queries outside PyQL.

        Use sparingly — this bypasses the transpiler entirely.
        """
        pool = self._require_pool()
        async with pool.acquire() as conn:
            yield conn  # type: ignore[misc]

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


def _pg_decode_value(type_oid: int, data: bytes) -> Any:
    match type_oid:
        case 25 | 1043 | 1042:  # text, varchar, bpchar
            return data.decode("utf-8")
        case 2950:  # uuid
            return str(_uuid_mod.UUID(bytes=data))
        case 20:  # int8
            return struct.unpack_from(">q", data)[0]
        case 23:  # int4
            return struct.unpack_from(">i", data)[0]
        case 21:  # int2
            return struct.unpack_from(">h", data)[0]
        case 16:  # bool
            return data[0] != 0
        case 701:  # float8
            return struct.unpack_from(">d", data)[0]
        case 700:  # float4
            return struct.unpack_from(">f", data)[0]
        case 3802:  # jsonb: 1-byte version prefix + json text
            return json.loads(data[1:].decode("utf-8"))
        case 1700:  # numeric — untyped decimal literals default to this
            return _pg_decode_numeric(data)
        case 2249:  # record (nested composite)
            return _pg_decode_record(data)
        case 2287:  # _record (record[])
            return _pg_decode_record_array(data)
        case _:  # enums, domains, and other text-compatible custom types
            return data.decode("utf-8")


def _pg_decode_numeric(data: bytes) -> decimal.Decimal:
    """Decode PostgreSQL's binary `numeric` wire format (base-10000 digit
    groups) into a `Decimal` — needed because our custom composite decoder
    bypasses asyncpg's own (correct) built-in numeric codec entirely."""
    ndigits, weight, sign, dscale = struct.unpack_from(">hhHh", data, 0)
    if sign == 0xC000:  # NUMERIC_NAN
        return decimal.Decimal("NaN")
    digits = struct.unpack_from(f">{ndigits}h", data, 8) if ndigits else ()
    result = decimal.Decimal(0)
    for i, digit in enumerate(digits):
        result += decimal.Decimal(digit) * (decimal.Decimal(10) ** ((weight - i) * 4))
    if sign == 0x4000:  # NUMERIC_NEG
        result = -result
    quant = decimal.Decimal(1).scaleb(-dscale) if dscale > 0 else decimal.Decimal(1)
    return result.quantize(quant)


def _pg_decode_record(data: bytes) -> tuple:
    offset = 0
    (nfields,) = struct.unpack_from(">i", data, offset)
    offset += 4
    fields: list[Any] = []
    for _ in range(nfields):
        (type_oid,) = struct.unpack_from(">I", data, offset)
        offset += 4
        (field_len,) = struct.unpack_from(">i", data, offset)
        offset += 4
        if field_len == -1:
            fields.append(None)
        else:
            fields.append(_pg_decode_value(type_oid, data[offset : offset + field_len]))
            offset += field_len
    return tuple(fields)


def _pg_decode_record_array(data: bytes) -> list:
    offset = 0
    (ndims,) = struct.unpack_from(">i", data, offset)
    offset += 4
    offset += 4  # flags (has-nulls)
    offset += 4  # element OID (always 2249 for record[])
    if ndims == 0:
        return []
    (dim,) = struct.unpack_from(">i", data, offset)
    offset += 4
    offset += 4  # lbound (usually 1)
    result: list[Any] = []
    for _ in range(dim):
        (elem_len,) = struct.unpack_from(">i", data, offset)
        offset += 4
        if elem_len == -1:
            result.append(None)
        else:
            result.append(_pg_decode_record(data[offset : offset + elem_len]))
            offset += elem_len
    return result


def _decode_vector_binary(data: bytes) -> list:
    ndim = struct.unpack_from(">H", data, 0)[0]
    return list(struct.unpack_from(f">{ndim}f", data, 4))


def _encode_vector_binary(v: list) -> bytes:
    floats = [float(x) for x in v]
    return struct.pack(f">HH{len(floats)}f", len(floats), 0, *floats)


async def _setup_codecs(conn: asyncpg.Connection) -> None:
    row = await conn.fetchrow(
        "SELECT t.oid, n.nspname "
        "FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace "
        "WHERE t.typname = 'vector'"
    )
    if row is not None:
        conn._protocol.get_settings().add_python_codec(
            row["oid"], "vector", row["nspname"], [], "scalar",
            _encode_vector_binary,
            _decode_vector_binary,
            "binary",
        )

    await conn.set_type_codec(
        "jsonb",
        encoder=json.dumps,
        decoder=json.loads,
        schema="pg_catalog",
        format="text",
    )
    # Override jsonb with a binary-format codec so it decodes correctly inside
    # anonymous record composites (asyncpg passes binary data there, not text).
    conn._protocol.get_settings().add_python_codec(
        3802, "jsonb", "pg_catalog", [], "scalar",
        lambda v: b"\x01" + json.dumps(v).encode(),
        lambda data: json.loads(data[1:].decode("utf-8")),
        "binary",
    )
    # Monkey-patch: register a binary decoder for record[] (OID 2287) directly,
    # bypassing asyncpg's scalar/composite validation in set_type_codec.
    conn._protocol.get_settings().add_python_codec(
        2287, "_record", "pg_catalog", [], "scalar",
        lambda v: v,
        _pg_decode_record_array,
        "binary",
    )


import re as _re

_PG_QUOTED_IDENT_RE = _re.compile(r'"([^"]+)"\."([^"]+)"')


def _fmt_pg_error(exc: asyncpg.PostgresError) -> QueryError:
    """Re-raise a PostgresError with Pylon-style type names in the message."""
    msg = _PG_QUOTED_IDENT_RE.sub(lambda m: f"'{m.group(1)}::{m.group(2)}'", exc.args[0])
    return QueryError(msg)


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
) -> tuple["CompiledQuery", str, list[Any]]:
    """Compile PyQL and, for OpenSearch-backed queries, perform the HTTP phase first.

    Returns ``(compiled, sql, params)`` ready for asyncpg. ``config_options``
    mirrors ``Client.with_config()`` — see ``pylon.config_options``.
    """
    if not isinstance(pyql, str):
        raise InterfaceError(f"PyQL query must be a str, got {type(pyql).__name__!r}.")
    from pylon.query import compile as _pyql_compile
    try:
        compiled = _pyql_compile(
            pyql,
            allow_user_specified_id=bool((config_options or {}).get("allow_user_specified_id", False)),
        )
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
        return compiled, compiled.sql, params

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
        # vector::search text overload — embed the query text, inject as __deferred_vec__.
        model_cfg = _resolve_model_config(plan["model_name"], config)
        provider = _make_provider(model_cfg)
        vector = await provider.embed(query_text or "")
        extra = {"__deferred_vec__": vector}

    params = [extra[name] for name in compiled.param_names]
    return compiled, compiled.sql, params


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


def _make_provider(model_cfg):
    from pylon.vector.models import OpenAIProvider, AnthropicProvider
    if model_cfg.api_style == "anthropic":
        return AnthropicProvider(
            api_url=model_cfg.api_url,
            model=model_cfg.model,
            api_key=model_cfg.secret,
        )
    return OpenAIProvider(
        api_url=model_cfg.api_url,
        model=model_cfg.model,
        api_key=model_cfg.secret,
    )


def _merge_args(args: tuple, kwargs: dict[str, Any]) -> dict[str, Any]:
    if not args:
        return kwargs
    return {str(i): v for i, v in enumerate(args)} | kwargs


def _transpile(
    pyql: str,
    kwargs: dict[str, Any],
    globals_: dict[str, Any] | None = None,
    config_options: dict[str, Any] | None = None,
) -> tuple[str, list[Any], "CompiledQuery"]:
    """Compile PyQL to SQL via the pylon-core Rust extension.

    Returns ``(sql, positional_params, compiled)`` ready for asyncpg.
    ``param_names`` entries prefixed with ``__global__`` are filled from
    ``globals_``; all others from ``kwargs``. ``config_options`` mirrors
    ``Client.with_config()`` — see ``pylon.config_options``.
    """
    if not isinstance(pyql, str):
        raise InterfaceError(f"PyQL query must be a str, got {type(pyql).__name__!r}.")
    from pylon.query import compile as _pyql_compile

    try:
        compiled = _pyql_compile(
            pyql,
            allow_user_specified_id=bool((config_options or {}).get("allow_user_specified_id", False)),
        )
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
    return compiled.sql, params, compiled


def _hydrate(records: list[Any], compiled: "CompiledQuery") -> list[Any]:
    """Decode asyncpg Records into Python dataclass instances."""
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
