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

"""Tests for pylon.client and pylon.config pool-size fields."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from pylon.config import DatabaseConfig
from pylon.exceptions import (
    InterfaceError,
    InternalServerError,
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
        db = DatabaseConfig(host='h', port=5432, name='db', user='u')
        assert db.pool_min_size == 2
        assert db.pool_max_size == 10

    def test_custom_values(self):
        db = DatabaseConfig(
            host='h',
            port=5432,
            name='db',
            user='u',
            pool_min_size=5,
            pool_max_size=20,
        )
        assert db.pool_min_size == 5
        assert db.pool_max_size == 20

    def test_min_size_zero_raises(self):
        with pytest.raises(ValueError, match='pool_min_size'):
            DatabaseConfig(host='h', port=5432, name='db', user='u', pool_min_size=0)

    def test_max_less_than_min_raises(self):
        with pytest.raises(ValueError, match='pool_max_size'):
            DatabaseConfig(
                host='h',
                port=5432,
                name='db',
                user='u',
                pool_min_size=5,
                pool_max_size=3,
            )

    def test_dsn_only_uses_defaults(self):
        db = DatabaseConfig(dsn='pylon://u:p@h:5432/db')
        assert db.pool_min_size == 2
        assert db.pool_max_size == 10


# ---------------------------------------------------------------------------
# Helpers — _compile_and_bind and _hydrate
# ---------------------------------------------------------------------------


class TestCompileAndBind:
    def _make_compiled(self, sql: str = 'SELECT 1', param_names: list[str] | None = None):
        compiled = MagicMock()
        compiled.sql = sql
        compiled.param_names = param_names if param_names is not None else []
        return compiled

    def test_non_str_raises_interface_error(self):
        from pylon.client import _compile_and_bind

        with pytest.raises(InterfaceError):
            _compile_and_bind(123, {})  # type: ignore[arg-type]

    def test_calls_pyql_compile(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled('SELECT 42')
        with patch('pylon.query.compile', return_value=compiled):
            returned, params = _compile_and_bind('select 42', {})

        assert returned is compiled
        assert params == []

    def test_kwargs_become_positional_params(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled('SELECT $1', param_names=['x'])
        with patch('pylon.query.compile', return_value=compiled):
            _, params = _compile_and_bind('select $x', {'x': 99})

        assert params == [99]

    def test_multiple_kwargs_order_by_param_names(self):
        from pylon.client import _compile_and_bind

        # param_names determines order, not kwargs insertion order
        compiled = self._make_compiled('SELECT $1, $2', param_names=['a', 'b'])
        with patch('pylon.query.compile', return_value=compiled):
            _, params = _compile_and_bind('q', {'b': 2, 'a': 1})

        assert params == [1, 2]

    def test_param_names_reorders_kwargs(self):
        from pylon.client import _compile_and_bind

        # $1=age, $2=name — even though name comes first in kwargs
        compiled = self._make_compiled('SELECT $1, $2', param_names=['age', 'name'])
        with patch('pylon.query.compile', return_value=compiled):
            _, params = _compile_and_bind('q', {'name': 'Alice', 'age': 30})

        assert params == [30, 'Alice']

    def test_missing_param_raises_interface_error(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled('SELECT $1', param_names=['name'])
        with (
            patch('pylon.query.compile', return_value=compiled),
            pytest.raises(InterfaceError, match='Missing query parameter'),
        ):
            _compile_and_bind('select $name', {})

    def test_config_options_thread_allow_user_specified_id(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled('INSERT ...')
        with patch('pylon.query.compile', return_value=compiled) as mock_compile:
            _compile_and_bind('insert ...', {}, config_options={'allow_user_specified_id': True})
        assert mock_compile.call_args.kwargs['allow_user_specified_id'] is True

    def test_missing_config_options_defaults_to_false(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled('INSERT ...')
        with patch('pylon.query.compile', return_value=compiled) as mock_compile:
            _compile_and_bind('insert ...', {})
        assert mock_compile.call_args.kwargs['allow_user_specified_id'] is False

    def test_compile_failure_raises_internal_error(self):
        from pylon.client import _compile_and_bind

        with (
            patch('pylon.query.compile', side_effect=RuntimeError('todo')),
            pytest.raises(InternalServerError, match='todo'),
        ):
            _compile_and_bind('select 1', {})


class TestMergeArgs:
    def test_no_args_returns_kwargs_unchanged(self):
        from pylon.client import _merge_args

        kw = {'name': 'Alice'}
        result = _merge_args((), kw)
        assert result is kw

    def test_args_mapped_to_string_indices(self):
        from pylon.client import _merge_args

        result = _merge_args(('Alice', 30), {})
        assert result == {'0': 'Alice', '1': 30}

    def test_args_and_kwargs_merged(self):
        from pylon.client import _merge_args

        result = _merge_args(('Alice',), {'age': 30})
        assert result == {'0': 'Alice', 'age': 30}

    def test_kwargs_win_over_args_on_collision(self):
        from pylon.client import _merge_args

        result = _merge_args(('original',), {'0': 'override'})
        assert result['0'] == 'override'


class TestCompileAndBindPositional:
    def _make_compiled(self, param_names):
        c = MagicMock()
        c.sql = 'SELECT $1'
        c.param_names = param_names
        return c

    def test_positional_arg_bound_by_index_name(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled(['0'])
        with patch('pylon.query.compile', return_value=compiled):
            _, params = _compile_and_bind('select $0', {'0': 'Alice'})
        assert params == ['Alice']

    def test_two_positional_args_in_order(self):
        from pylon.client import _compile_and_bind

        compiled = self._make_compiled(['0', '1'])
        with patch('pylon.query.compile', return_value=compiled):
            _, params = _compile_and_bind('select $0, $1', {'0': 'Alice', '1': 30})
        assert params == ['Alice', 30]


class TestClientQueryPositional:
    def test_positional_args_forwarded_to_the_compile_step(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            received_kwargs: dict = {}

            async def fake_resolve(pyql, kwargs, config, globals_=None, config_options=None):
                received_kwargs.update(kwargs)
                compiled = _fake_compiled()
                compiled.param_names = []
                compiled.inference_plan = None
                return compiled, []

            with (
                patch('pylon.client._compile_and_resolve', side_effect=fake_resolve),
                patch('pylon.client._hydrate', return_value=[]),
            ):
                await client.query('select Person filter .name = $0', 'Alice')

            assert received_kwargs == {'0': 'Alice'}

        run(_run())


class TestHydrate:
    def _make_compiled(self):
        return MagicMock()

    def test_returns_records_when_no_schema(self):
        from pylon.client import _hydrate

        records = [object(), object()]
        compiled = self._make_compiled()
        with patch('pylon.query._get_schema', side_effect=RuntimeError('no schema')):
            result = _hydrate(records, compiled)

        assert result is records

    def test_delegates_to_the_native_hydrator(self):
        """`_hydrate` runs `pylon._core.hydrate`, not the Python reference
        `deserialize` — see `tests/test_hydrate_parity.py`, which is what
        holds the two implementations to the same contract."""
        from pylon.client import _hydrate

        rows = ['row1']
        compiled = self._make_compiled()
        registry = MagicMock()
        hydrated = [object()]
        with (
            patch('pylon.query._get_schema', return_value=MagicMock()),
            patch('pylon.query.hydration_registry', return_value=registry),
            patch('pylon._core.hydrate', return_value=hydrated) as native,
        ):
            result = _hydrate(rows, compiled)

        assert result is hydrated
        native.assert_called_once_with(rows, compiled, registry)


# ---------------------------------------------------------------------------
# Client query methods — mock pgcon pool
# ---------------------------------------------------------------------------


def _fake_compiled(sql: str = 'SELECT 1', tags: list[str] | None = None, mutates: bool = False):
    c = MagicMock()
    c.sql = sql
    c.inference_plan = None
    c.tags = tags if tags is not None else []
    # Must be set explicitly: a bare MagicMock attribute is truthy, which
    # would make every fake query look like a mutation and so uncacheable.
    c.mutates = mutates
    return c


def _client_with_pool(pool: MagicMock, cache_config=None):
    """Return a Client whose internal pool is already set to *pool*.

    *cache_config* defaults to a `MagicMock` (matching every pre-existing
    caller — its truthy `.enabled` is never actually consulted, since
    `pylon.cache`'s global `_enabled` flag short-circuits first while the
    cache is untouched by these tests). Pass a real `CacheConfig` to
    exercise the read-through cache wiring itself.
    """
    from pylon.client import Client, _PoolRef

    cfg_db = DatabaseConfig(host='h', port=5432, name='db', user='u')
    cfg = MagicMock()
    cfg.database = cfg_db
    cfg.cache = cache_config if cache_config is not None else MagicMock()
    client = Client.__new__(Client)
    client._config = cfg
    ref = _PoolRef()
    ref.pool = pool
    client._ref = ref
    client._warnings = True
    client._globals = {}
    client._config_options = {}
    return client


def _make_pool(query_result=None, execute_result=None):
    """Return a MagicMock standing in for a `pylon._core.PgconPool` handle.

    `query`/`execute` take positional `(sql, params)`, matching the real
    `PgconPool` — no more separate `pool.acquire()`-yielded connection;
    `pgcon`'s pool methods are called directly on the pool handle itself.
    `query_result` defaults to `[]`; pass a list to stand in for the
    (already-decoded, unwrapped) rows a real `pool.query()` would return —
    `Client.query()` itself does the `{"result": row}` wrapping now.
    `query_compiled`/`execute_compiled` are the fused entrypoints
    (`compiled` object + params, no SQL string) used by the non-JSON,
    non-inference-plan `Client`/`AsyncTransaction` methods — mirror the
    same return values as `query`/`execute` since they're the same
    operation, just reading SQL out of `compiled` on the Rust side instead
    of taking it as a Python string.
    """
    pool = MagicMock()
    pool.query = AsyncMock(return_value=query_result if query_result is not None else [])
    pool.execute = AsyncMock(return_value=execute_result)
    pool.query_compiled = AsyncMock(return_value=query_result if query_result is not None else [])
    pool.execute_compiled = AsyncMock(return_value=execute_result)
    pool.query_compiled_json_agg = AsyncMock(return_value=query_result if query_result is not None else [])
    pool.query_compiled_row_to_json = AsyncMock(return_value=query_result if query_result is not None else [])
    return pool


class TestClientQuery:
    def _patch_compile(self, sql='SELECT 1'):
        compiled = _fake_compiled(sql)

        async def fake_resolve(pyql, kwargs, config, globals_=None, config_options=None):
            return compiled, list(kwargs.values())

        def fake_bind(pyql, kwargs, globals_=None, config_options=None):
            return compiled, list(kwargs.values())

        p1 = patch('pylon.client._compile_and_resolve', side_effect=fake_resolve)
        p3 = patch('pylon.client._compile_and_bind', side_effect=fake_bind)

        class _Both:
            def __enter__(self):
                p1.__enter__()
                p3.__enter__()
                return self

            def __exit__(self, *a):
                p3.__exit__(*a)
                p1.__exit__(*a)

        return _Both(), compiled

    def _patch_hydrate(self, result=None):
        out = result if result is not None else []
        return patch('pylon.client._hydrate', return_value=out)

    def test_query_returns_hydrated_list(self):
        async def _run():
            pool = _make_pool(query_result=['r1', 'r2'])
            client = _client_with_pool(pool)
            hydrated = [object(), object()]

            compile_patch, _ = self._patch_compile()
            with compile_patch, self._patch_hydrate(hydrated):
                result = await client.query('select User')

            assert result is hydrated

        run(_run())

    def test_query_single_empty_returns_none(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch, self._patch_hydrate([]):
                result = await client.query_single('select User')

            assert result is None

        run(_run())

    def test_query_single_one_row_returns_first(self):
        async def _run():
            row = object()
            pool = _make_pool(query_result=[row])
            client = _client_with_pool(pool)

            hydrated_row = object()
            compile_patch, _ = self._patch_compile()
            with compile_patch, patch('pylon.client._hydrate', return_value=[hydrated_row]):
                result = await client.query_single('select User')

            assert result is hydrated_row

        run(_run())

    def test_query_single_multiple_rows_raises(self):
        async def _run():
            pool = _make_pool(query_result=['r1', 'r2'])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch, pytest.raises(ResultCardinalityError):
                await client.query_single('select User')

        run(_run())

    def test_query_required_single_empty_raises(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch, self._patch_hydrate([]), pytest.raises(NoDataError):
                await client.query_required_single('select User')

        run(_run())

    def test_execute_discards_result(self):
        async def _run():
            pool = _make_pool()
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch:
                result = await client.execute("insert User { name := 'x' }")

            assert result is None
            pool.execute_compiled.assert_awaited_once()

        run(_run())

    def test_query_json_returns_string(self):
        async def _run():
            pool = _make_pool(query_result=['[{"id": 1}]'])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch:
                result = await client.query_json('select User')

            assert result == '[{"id": 1}]'

        run(_run())

    def test_query_json_empty_returns_empty_array(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch:
                result = await client.query_json('select User')

            assert result == '[]'

        run(_run())

    def test_query_single_json_empty_returns_none(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch:
                result = await client.query_single_json('select User')

            assert result is None

        run(_run())

    def test_query_single_json_multiple_rows_raises(self):
        async def _run():
            pool = _make_pool(query_result=['r1', 'r2'])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch, pytest.raises(ResultCardinalityError):
                await client.query_single_json('select User')

        run(_run())

    def test_query_required_single_json_empty_raises(self):
        async def _run():
            pool = _make_pool(query_result=[])
            client = _client_with_pool(pool)

            compile_patch, _ = self._patch_compile()
            with compile_patch, pytest.raises(NoDataError):
                await client.query_required_single_json('select User')

        run(_run())


# ---------------------------------------------------------------------------
# Client caching — real CacheConfig, proving a second call skips the DB
# ---------------------------------------------------------------------------


class TestClientCaching:
    def _patch_compile(self, sql='SELECT 1', tags=None):
        compiled = _fake_compiled(sql, tags=tags)

        async def fake_resolve(pyql, kwargs, config, globals_=None, config_options=None):
            return compiled, list(kwargs.values())

        def fake_bind(pyql, kwargs, globals_=None, config_options=None):
            return compiled, list(kwargs.values())

        p1 = patch('pylon.client._compile_and_resolve', side_effect=fake_resolve)
        p3 = patch('pylon.client._compile_and_bind', side_effect=fake_bind)

        class _Both:
            def __enter__(self):
                p1.__enter__()
                p3.__enter__()
                return self

            def __exit__(self, *a):
                p3.__exit__(*a)
                p1.__exit__(*a)

        return _Both(), compiled

    def _cache_config(self, tmp_path):
        from pylon.config import CacheConfig

        return CacheConfig(enabled=True, path=tmp_path / 'cache')

    def test_query_cache_hit_skips_db_fetch(self, tmp_path):
        async def _run():
            pool = _make_pool(query_result=['row1'])
            client = _client_with_pool(pool, cache_config=self._cache_config(tmp_path))
            from pylon import cache as _cache

            _cache.init(client._config.cache)

            compile_patch, _ = self._patch_compile(tags=['public.person'])
            with (
                compile_patch,
                patch('pylon.client._hydrate', side_effect=lambda rows, compiled: list(rows)),
            ):
                first = await client.query('select Person')
                second = await client.query('select Person')

            assert first == ['row1']
            assert second == ['row1']
            pool.query_compiled.assert_awaited_once()

        run(_run())

    def test_query_single_cache_hit_skips_db_fetch(self, tmp_path):
        async def _run():
            pool = _make_pool(query_result=['row1'])
            client = _client_with_pool(pool, cache_config=self._cache_config(tmp_path))
            from pylon import cache as _cache

            _cache.init(client._config.cache)

            compile_patch, _ = self._patch_compile(tags=['public.person'])
            with (
                compile_patch,
                patch('pylon.client._hydrate', side_effect=lambda rows, compiled: list(rows)),
            ):
                first = await client.query_single('select Person')
                second = await client.query_single('select Person')

            assert first == 'row1'
            assert second == 'row1'
            pool.query_compiled.assert_awaited_once()

        run(_run())

    def test_query_json_cache_hit_skips_db_fetchval(self, tmp_path):
        async def _run():
            pool = _make_pool(query_result=['[{"id": 1}]'])
            client = _client_with_pool(pool, cache_config=self._cache_config(tmp_path))
            from pylon import cache as _cache

            _cache.init(client._config.cache)

            compile_patch, _ = self._patch_compile(tags=['public.person'])
            with compile_patch:
                first = await client.query_json('select Person')
                second = await client.query_json('select Person')

            assert first == '[{"id": 1}]'
            assert second == '[{"id": 1}]'
            pool.query_compiled_json_agg.assert_awaited_once()

        run(_run())

    def test_query_single_json_cache_hit_skips_db_round_trip(self, tmp_path):
        async def _run():
            pool = _make_pool()
            # query_single_json issues two fused calls on a cache miss:
            # `query_compiled` first (to check emptiness/cardinality), then
            # `query_compiled_row_to_json` for the actual JSON text.
            pool.query_compiled = AsyncMock(return_value=['row1'])
            pool.query_compiled_row_to_json = AsyncMock(return_value=['{"id": 1}'])
            client = _client_with_pool(pool, cache_config=self._cache_config(tmp_path))
            from pylon import cache as _cache

            _cache.init(client._config.cache)

            compile_patch, _ = self._patch_compile(tags=['public.person'])
            with compile_patch:
                first = await client.query_single_json('select Person')
                second = await client.query_single_json('select Person')

            assert first == '{"id": 1}'
            assert second == '{"id": 1}'
            pool.query_compiled.assert_awaited_once()
            pool.query_compiled_row_to_json.assert_awaited_once()

        run(_run())

    def test_query_with_no_tags_is_never_cached(self, tmp_path):
        async def _run():
            pool = _make_pool(query_result=['row1'])
            client = _client_with_pool(pool, cache_config=self._cache_config(tmp_path))
            from pylon import cache as _cache

            _cache.init(client._config.cache)

            compile_patch, _ = self._patch_compile(tags=[])
            with (
                compile_patch,
                patch('pylon.client._hydrate', side_effect=lambda rows, compiled: list(rows)),
            ):
                await client.query('select 1')
                await client.query('select 1')

            assert pool.query_compiled.await_count == 2

        run(_run())

    def test_cache_disabled_never_short_circuits_db(self, tmp_path):
        async def _run():
            pool = _make_pool(query_result=['row1'])
            from pylon.config import CacheConfig

            disabled = CacheConfig(enabled=False, path=tmp_path / 'cache')
            client = _client_with_pool(pool, cache_config=disabled)

            compile_patch, _ = self._patch_compile(tags=['public.person'])
            with (
                compile_patch,
                patch('pylon.client._hydrate', side_effect=lambda rows, compiled: list(rows)),
            ):
                await client.query('select Person')
                await client.query('select Person')

            assert pool.query_compiled.await_count == 2

        run(_run())


# ---------------------------------------------------------------------------
# with_globals — returns a Client sharing the same pool
# ---------------------------------------------------------------------------


class TestWithGlobals:
    def test_returns_client_instance(self):
        from pylon.client import Client

        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_globals({'default::x': 1})
        assert isinstance(view, Client)

    def test_shares_pool_ref(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_globals({'default::x': 1})
        assert view._ref is client._ref

    def test_globals_merged(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        client._globals = {'default::a': 1}
        view = client.with_globals({'default::b': 2})
        assert view._globals == {'default::a': 1, 'default::b': 2}

    def test_chained_with_globals_merges(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_globals({'default::a': 1}).with_globals({'default::b': 2})
        assert view._globals == {'default::a': 1, 'default::b': 2}

    def test_later_connection_visible_to_view(self):
        """Pool connected after with_globals() must be visible via the shared ref."""
        pool = _make_pool()
        client = _client_with_pool(None)  # not yet connected
        client._ref.pool = None
        view = client.with_globals({'default::x': 1})
        # Simulate connection on original
        client._ref.pool = pool
        assert view._require_pool() is pool


class TestWithConfig:
    def test_returns_client_instance(self):
        from pylon.client import Client

        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_config({'allow_user_specified_id': True})
        assert isinstance(view, Client)

    def test_shares_pool_ref(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_config({'allow_user_specified_id': True})
        assert view._ref is client._ref

    def test_options_merged(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        client._config_options = {'allow_user_specified_id': False}
        view = client.with_config({'some_future_option': True})
        assert view._config_options == {'allow_user_specified_id': False, 'some_future_option': True}

    def test_chained_with_config_merges(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_config({'a': 1}).with_config({'b': 2})
        assert view._config_options == {'a': 1, 'b': 2}

    def test_with_config_preserves_globals(self):
        pool = _make_pool()
        client = _client_with_pool(pool)
        view = client.with_globals({'default::x': 1}).with_config({'allow_user_specified_id': True})
        assert view._globals == {'default::x': 1}


# ---------------------------------------------------------------------------
# ensure_connected — pool config forwarding
# ---------------------------------------------------------------------------


class TestEnsureConnected:
    def test_passes_max_pool_size_to_pgcon_connect(self):
        async def _run():
            from pylon.client import Client
            from pylon.config import CacheConfig

            db_cfg = DatabaseConfig(
                host='localhost',
                port=5432,
                name='db',
                user='u',
                pool_min_size=3,
                pool_max_size=15,
            )
            cfg = MagicMock()
            cfg.database = db_cfg
            cfg.cache = CacheConfig(enabled=False)

            mock_pool = MagicMock()
            with (
                patch('pylon._core.pgcon_connect', new=AsyncMock(return_value=mock_pool)) as mock_connect,
                patch('pylon._core.migration_read_schema_snapshot', new=AsyncMock(return_value=None)),
            ):
                client = Client(cfg)
                await client.ensure_connected()

            mock_connect.assert_awaited_once()
            call_args, _ = mock_connect.call_args
            assert call_args[1] == 15

        run(_run())

    def test_second_call_is_noop(self):
        async def _run():
            from pylon.client import Client
            from pylon.config import CacheConfig

            db_cfg = DatabaseConfig(host='h', port=5432, name='db', user='u')
            cfg = MagicMock()
            cfg.database = db_cfg
            cfg.cache = CacheConfig(enabled=False)

            mock_pool = MagicMock()
            with (
                patch('pylon._core.pgcon_connect', new=AsyncMock(return_value=mock_pool)) as mock_connect,
                patch('pylon._core.migration_read_schema_snapshot', new=AsyncMock(return_value=None)),
            ):
                client = Client(cfg)
                await client.ensure_connected()
                await client.ensure_connected()

            assert mock_connect.await_count == 1

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

            pool = MagicMock()
            pool.transaction = AsyncMock(return_value=MagicMock())

            with patch('pylon.client.AsyncTransaction', return_value=tx_obj):
                iterator = RetryingTransaction(pool, attempts=3, isolation='serializable')
                iterations = 0
                async for tx in iterator:
                    async with tx:
                        iterations += 1

            assert iterations == 1
            pool.transaction.assert_awaited_once_with('serializable')

        run(_run())

    def test_exhausted_retries_re_raises(self):
        async def _run():
            from pylon.client import RetryingTransaction
            from pylon.exceptions import TransactionSerializationError

            exc = TransactionSerializationError('serialization failure')

            call_count = 0

            class FakeTx:
                def __init__(self, pgcon_tx):
                    self._tx = pgcon_tx

                async def __aenter__(self):
                    return self

                async def __aexit__(self, *_):
                    nonlocal call_count
                    call_count += 1
                    self._retry_exc = exc
                    return False

                _retry_exc = exc

            pool = MagicMock()
            pool.transaction = AsyncMock(return_value=MagicMock())

            with patch('pylon.client.AsyncTransaction', FakeTx):
                iterator = RetryingTransaction(pool, attempts=2, isolation='serializable')
                with pytest.raises(TransactionSerializationError):
                    async for tx in iterator:
                        async with tx:
                            pass

        run(_run())


class TestInstallMigratedSchema:
    """The schema snapshot stored in the database is written by whichever
    Pylon last ran a migration. An older writer's format can be unreadable
    here, and serde's own message (`missing field 'x' at line 1 column N`)
    points at a byte offset in a blob the reader never sees."""

    def _run_with_snapshot(self, snapshot_json):
        async def _run():
            from pylon.client import _install_migrated_schema

            async def fake_read(_pool):
                return snapshot_json

            with patch('pylon._core.migration_read_schema_snapshot', fake_read):
                await _install_migrated_schema(MagicMock())

        run(_run())

    def test_an_unreadable_snapshot_names_the_cause_and_the_fix(self):
        from pylon.exceptions import SchemaError

        # Valid JSON, but missing a field this version requires.
        stale = '{"types":[],"scalars":[],"enums":[],"named_tuples":[],"globals":[],"functions":[],"aliases":[]}'
        with pytest.raises(SchemaError) as excinfo:
            self._run_with_snapshot(stale)

        message = str(excinfo.value)
        assert 'older version' in message
        assert 'pylon migration apply' in message
        # The underlying serde detail is kept, just no longer the whole story.
        assert 'missing field' in message

    def test_the_original_error_is_chained(self):
        from pylon.exceptions import SchemaError

        stale = '{"types":[]}'
        with pytest.raises(SchemaError) as excinfo:
            self._run_with_snapshot(stale)
        assert isinstance(excinfo.value.__cause__, ValueError)

    def test_no_snapshot_is_a_no_op(self):
        # An unmigrated database leaves whatever finalize() installed.
        self._run_with_snapshot(None)
