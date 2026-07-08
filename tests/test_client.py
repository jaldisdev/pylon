"""Tests for pylon.client and pylon.config pool-size fields."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from pylon.config import DatabaseConfig
from pylon.exceptions import (
    InternalServerError,
    InterfaceError,
    NoDataError,
    ResultCardinalityError,
)


def run(coro):
    """Run an async coroutine synchronously in tests."""
    return asyncio.run(coro)


# ---------------------------------------------------------------------------
# DatabaseConfig — pool size fields
# ---------------------------------------------------------------------------


class TestDatabaseConfigPoolSize:
    def test_defaults(self):
        db = DatabaseConfig(host="h", port=5432, name="db", user="u")
        assert db.pool_min_size == 2
        assert db.pool_max_size == 10

    def test_custom_values(self):
        db = DatabaseConfig(
            host="h", port=5432, name="db", user="u",
            pool_min_size=5, pool_max_size=20,
        )
        assert db.pool_min_size == 5
        assert db.pool_max_size == 20

    def test_min_size_zero_raises(self):
        with pytest.raises(ValueError, match="pool_min_size"):
            DatabaseConfig(host="h", port=5432, name="db", user="u", pool_min_size=0)

    def test_max_less_than_min_raises(self):
        with pytest.raises(ValueError, match="pool_max_size"):
            DatabaseConfig(
                host="h", port=5432, name="db", user="u",
                pool_min_size=5, pool_max_size=3,
            )

    def test_dsn_only_uses_defaults(self):
        db = DatabaseConfig(dsn="pylon://u:p@h:5432/db")
        assert db.pool_min_size == 2
        assert db.pool_max_size == 10


# ---------------------------------------------------------------------------
# Helpers — _transpile and _hydrate
# ---------------------------------------------------------------------------


class TestTranspile:
    def _make_compiled(self, sql: str = "SELECT 1", param_names: list[str] | None = None):
        compiled = MagicMock()
        compiled.sql = sql
        compiled.param_names = param_names if param_names is not None else []
        return compiled

    def test_non_str_raises_interface_error(self):
        from pylon.client import _transpile

        with pytest.raises(InterfaceError):
            _transpile(123, {})  # type: ignore[arg-type]

    def test_calls_pyql_compile(self):
        from pylon.client import _transpile

        compiled = self._make_compiled("SELECT 42")
        with patch("pylon.query.compile", return_value=compiled):
            sql, params, returned = _transpile("select 42", {})

        assert sql == "SELECT 42"
        assert params == []
        assert returned is compiled

    def test_kwargs_become_positional_params(self):
        from pylon.client import _transpile

        compiled = self._make_compiled("SELECT $1", param_names=["x"])
        with patch("pylon.query.compile", return_value=compiled):
            sql, params, _ = _transpile("select $x", {"x": 99})

        assert params == [99]

    def test_multiple_kwargs_order_by_param_names(self):
        from pylon.client import _transpile

        # param_names determines order, not kwargs insertion order
        compiled = self._make_compiled("SELECT $1, $2", param_names=["a", "b"])
        with patch("pylon.query.compile", return_value=compiled):
            _, params, _ = _transpile("q", {"b": 2, "a": 1})

        assert params == [1, 2]

    def test_param_names_reorders_kwargs(self):
        from pylon.client import _transpile

        # $1=age, $2=name — even though name comes first in kwargs
        compiled = self._make_compiled("SELECT $1, $2", param_names=["age", "name"])
        with patch("pylon.query.compile", return_value=compiled):
            _, params, _ = _transpile("q", {"name": "Alice", "age": 30})

        assert params == [30, "Alice"]

    def test_missing_param_raises_interface_error(self):
        from pylon.client import _transpile

        compiled = self._make_compiled("SELECT $1", param_names=["name"])
        with patch("pylon.query.compile", return_value=compiled):
            with pytest.raises(InterfaceError, match="Missing query parameter"):
                _transpile("select $name", {})

    def test_compile_failure_raises_internal_error(self):
        from pylon.client import _transpile

        with patch("pylon.query.compile", side_effect=RuntimeError("todo")):
            with pytest.raises(InternalServerError, match="todo"):
                _transpile("select 1", {})


class TestMergeArgs:
    def test_no_args_returns_kwargs_unchanged(self):
        from pylon.client import _merge_args

        kw = {"name": "Alice"}
        result = _merge_args((), kw)
        assert result is kw

    def test_args_mapped_to_string_indices(self):
        from pylon.client import _merge_args

        result = _merge_args(("Alice", 30), {})
        assert result == {"0": "Alice", "1": 30}

    def test_args_and_kwargs_merged(self):
        from pylon.client import _merge_args

        result = _merge_args(("Alice",), {"age": 30})
        assert result == {"0": "Alice", "age": 30}

    def test_kwargs_win_over_args_on_collision(self):
        from pylon.client import _merge_args

        result = _merge_args(("original",), {"0": "override"})
        assert result["0"] == "override"


class TestTranspilePositional:
    def _make_compiled(self, param_names):
        c = MagicMock()
        c.sql = "SELECT $1"
        c.param_names = param_names
        return c

    def test_positional_arg_bound_by_index_name(self):
        from pylon.client import _transpile

        compiled = self._make_compiled(["0"])
        with patch("pylon.query.compile", return_value=compiled):
            _, params, _ = _transpile("select $0", {"0": "Alice"})
        assert params == ["Alice"]

    def test_two_positional_args_in_order(self):
        from pylon.client import _transpile

        compiled = self._make_compiled(["0", "1"])
        with patch("pylon.query.compile", return_value=compiled):
            _, params, _ = _transpile("select $0, $1", {"0": "Alice", "1": 30})
        assert params == ["Alice", 30]


class TestClientQueryPositional:
    def test_positional_args_forwarded_to_transpile(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=[])
            client = _client_with_pool(pool)

            received_kwargs: dict = {}

            async def fake_resolve(pyql, kwargs, config, globals_=None):
                received_kwargs.update(kwargs)
                compiled = _fake_compiled()
                compiled.param_names = []
                compiled.inference_plan = None
                return compiled, "SELECT 1", []

            with patch("pylon.client._compile_and_resolve", side_effect=fake_resolve), \
                 patch("pylon.client._hydrate", return_value=[]):
                await client.query("select Person filter .name = $0", "Alice")

            assert received_kwargs == {"0": "Alice"}

        run(_run())


class TestHydrate:
    def _make_compiled(self):
        return MagicMock()

    def test_returns_records_when_no_schema(self):
        from pylon.client import _hydrate

        records = [object(), object()]
        compiled = self._make_compiled()
        with patch("pylon.query._get_schema", side_effect=RuntimeError("no schema")):
            result = _hydrate(records, compiled)

        assert result is records

    def test_returns_deserialized_objects(self):
        from pylon.client import _hydrate

        records = ["row1"]
        compiled = self._make_compiled()
        hydrated = [object()]
        with (
            patch("pylon.query._get_schema", return_value=MagicMock()),
            patch("pylon.schema.schema_snapshot", return_value=([], [], [])),
            patch("pylon.query.deserialize", return_value=hydrated),
        ):
            result = _hydrate(records, compiled)

        assert result is hydrated


# ---------------------------------------------------------------------------
# Client query methods — mock asyncpg pool
# ---------------------------------------------------------------------------


def _fake_compiled(sql: str = "SELECT 1"):
    c = MagicMock()
    c.sql = sql
    c.inference_plan = None
    return c


def _client_with_pool(pool: MagicMock):
    """Return a Client whose internal pool is already set to *pool*."""
    from pylon.client import Client

    cfg_db = DatabaseConfig(host="h", port=5432, name="db", user="u")
    cfg = MagicMock()
    cfg.database = cfg_db
    client = Client.__new__(Client)
    client._config = cfg
    client._pool = pool
    client._lock = asyncio.Lock()
    client._warnings = True
    return client


def _make_pool(fetch_result=None, fetchval_result=None):
    row = MagicMock()
    row.__class__ = object  # asyncpg.Record-ish

    conn = AsyncMock()
    conn.fetch = AsyncMock(return_value=fetch_result or [])
    conn.fetchval = AsyncMock(return_value=fetchval_result)
    conn.execute = AsyncMock(return_value=None)

    pool = MagicMock()
    pool.acquire = MagicMock(return_value=_async_cm(conn))
    return pool, conn


class _async_cm:
    """Minimal async context manager returning a fixed value."""

    def __init__(self, value):
        self._value = value

    async def __aenter__(self):
        return self._value

    async def __aexit__(self, *_):
        pass


class TestClientQuery:
    def _patch_transpile(self, sql="SELECT 1"):
        compiled = _fake_compiled(sql)

        async def fake_resolve(pyql, kwargs, config, globals_=None):
            return compiled, sql, list(kwargs.values())

        def fake_transpile(pyql, kwargs, globals_=None):
            return sql, list(kwargs.values()), compiled

        p1 = patch("pylon.client._compile_and_resolve", side_effect=fake_resolve)
        p2 = patch("pylon.client._transpile", side_effect=fake_transpile)

        class _Both:
            def __enter__(self):
                p1.__enter__()
                p2.__enter__()
                return self
            def __exit__(self, *a):
                p2.__exit__(*a)
                p1.__exit__(*a)

        return _Both(), compiled

    def _patch_hydrate(self, result=None):
        out = result if result is not None else []
        return patch("pylon.client._hydrate", return_value=out)

    def test_query_returns_hydrated_list(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=["r1", "r2"])
            client = _client_with_pool(pool)
            hydrated = [object(), object()]

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, self._patch_hydrate(hydrated):
                result = await client.query("select User")

            assert result is hydrated

        run(_run())

    def test_query_single_empty_returns_none(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=[])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, self._patch_hydrate([]):
                result = await client.query_single("select User")

            assert result is None

        run(_run())

    def test_query_single_one_row_returns_first(self):
        async def _run():
            row = object()
            pool, conn = _make_pool(fetch_result=[row])
            client = _client_with_pool(pool)

            hydrated_row = object()
            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, patch("pylon.client._hydrate", return_value=[hydrated_row]):
                result = await client.query_single("select User")

            assert result is hydrated_row

        run(_run())

    def test_query_single_multiple_rows_raises(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=["r1", "r2"])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, pytest.raises(ResultCardinalityError):
                await client.query_single("select User")

        run(_run())

    def test_query_required_single_empty_raises(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=[])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, self._patch_hydrate([]):
                with pytest.raises(NoDataError):
                    await client.query_required_single("select User")

        run(_run())

    def test_execute_discards_result(self):
        async def _run():
            pool, conn = _make_pool()
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch:
                result = await client.execute("insert User { name := 'x' }")

            assert result is None
            conn.execute.assert_awaited_once()

        run(_run())

    def test_query_json_returns_string(self):
        async def _run():
            pool, conn = _make_pool(fetchval_result='[{"id": 1}]')
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch:
                result = await client.query_json("select User")

            assert result == '[{"id": 1}]'

        run(_run())

    def test_query_json_empty_returns_empty_array(self):
        async def _run():
            pool, conn = _make_pool(fetchval_result=None)
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch:
                result = await client.query_json("select User")

            assert result == "[]"

        run(_run())

    def test_query_single_json_empty_returns_none(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=[])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch:
                result = await client.query_single_json("select User")

            assert result is None

        run(_run())

    def test_query_single_json_multiple_rows_raises(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=["r1", "r2"])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, pytest.raises(ResultCardinalityError):
                await client.query_single_json("select User")

        run(_run())

    def test_query_required_single_json_empty_raises(self):
        async def _run():
            pool, conn = _make_pool(fetch_result=[])
            client = _client_with_pool(pool)

            transpile_patch, _ = self._patch_transpile()
            with transpile_patch, pytest.raises(NoDataError):
                await client.query_required_single_json("select User")

        run(_run())


# ---------------------------------------------------------------------------
# ensure_connected — pool config forwarding
# ---------------------------------------------------------------------------


class TestEnsureConnected:
    def test_passes_pool_sizes_to_asyncpg(self):
        async def _run():
            from pylon.client import Client

            db_cfg = DatabaseConfig(
                host="localhost", port=5432, name="db", user="u",
                pool_min_size=3, pool_max_size=15,
            )
            cfg = MagicMock()
            cfg.database = db_cfg

            mock_pool = MagicMock()
            with patch("asyncpg.create_pool", new=AsyncMock(return_value=mock_pool)) as mock_create:
                client = Client(cfg)
                await client.ensure_connected()

            mock_create.assert_awaited_once()
            _, call_kwargs = mock_create.call_args
            assert call_kwargs["min_size"] == 3
            assert call_kwargs["max_size"] == 15

        run(_run())

    def test_second_call_is_noop(self):
        async def _run():
            from pylon.client import Client

            db_cfg = DatabaseConfig(host="h", port=5432, name="db", user="u")
            cfg = MagicMock()
            cfg.database = db_cfg

            mock_pool = MagicMock()
            with patch("asyncpg.create_pool", new=AsyncMock(return_value=mock_pool)) as mock_create:
                client = Client(cfg)
                await client.ensure_connected()
                await client.ensure_connected()

            assert mock_create.await_count == 1

        run(_run())


# ---------------------------------------------------------------------------
# RetryingTransaction
# ---------------------------------------------------------------------------


class TestRetryingTransaction:
    def test_commits_on_success(self):
        async def _run():
            from pylon.client import RetryingTransaction

            tx_obj = MagicMock()
            tx_obj.__aenter__ = AsyncMock(return_value=tx_obj)
            tx_obj.__aexit__ = AsyncMock(return_value=False)
            tx_obj._retry_exc = None  # committed cleanly

            pool = AsyncMock()
            pool.acquire = AsyncMock(return_value=MagicMock())
            pool.release = AsyncMock()

            with patch("pylon.client.AsyncTransaction", return_value=tx_obj):
                iterator = RetryingTransaction(pool, attempts=3, isolation="serializable")
                iterations = 0
                async for tx in iterator:
                    async with tx:
                        iterations += 1

            assert iterations == 1

        run(_run())

    def test_exhausted_retries_re_raises(self):
        async def _run():
            from pylon.client import RetryingTransaction
            from pylon.exceptions import TransactionSerializationError

            exc = TransactionSerializationError("serialization failure")

            call_count = 0

            class FakeTx:
                def __init__(self, conn, isolation):
                    self._conn = conn

                async def __aenter__(self):
                    return self

                async def __aexit__(self, *_):
                    nonlocal call_count
                    call_count += 1
                    self._retry_exc = exc
                    return False

                _retry_exc = exc

            pool = AsyncMock()
            pool.acquire = AsyncMock(return_value=MagicMock())
            pool.release = AsyncMock()

            with patch("pylon.client.AsyncTransaction", FakeTx):
                iterator = RetryingTransaction(pool, attempts=2, isolation="serializable")
                with pytest.raises(TransactionSerializationError):
                    async for tx in iterator:
                        async with tx:
                            pass

        run(_run())
