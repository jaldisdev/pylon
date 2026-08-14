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

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

from pylon import cache
from pylon.config import CacheConfig, CacheSetConfig


def compiled(sql: str = 'select 1', tags: list[str] | None = None, mutates: bool = False) -> SimpleNamespace:
    # `shape_id` stands in for what the real `CompiledQuery` computes once at
    # compile time; `pylon.cache` keys on it rather than on `sql`, so a
    # double with two different `sql` values must produce two different ids
    # for these tests to keep distinguishing them.
    return SimpleNamespace(
        sql=sql,
        shape_id=f'shape-of-{sql}',
        tags=tags if tags is not None else [],
        mutates=mutates,
    )


@pytest.fixture(autouse=True)
def reset_cache_state(monkeypatch):
    """Every test gets its own LMDB dir via `init()` — reset the module-level
    `_enabled` flag first so a disabled-config test doesn't inherit `True`
    from a prior test in the same process."""
    monkeypatch.setattr(cache, '_enabled', False)
    yield


class TestInit:
    def test_disabled_config_is_a_noop(self, tmp_path):
        config = CacheConfig(enabled=False, path=tmp_path / 'cache')
        cache.init(config)
        assert cache._enabled is False
        assert not (tmp_path / 'cache').exists()

    def test_enabled_config_opens_lmdb(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        assert cache._enabled is True
        assert (tmp_path / 'cache').is_dir()


class TestGetPutRoundTrip:
    def test_miss_then_put_then_hit(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        assert cache.get(q, [1], config) is None

        records = [(1, 'alice')]
        cache.put(q, [1], records, config)

        hit = cache.get(q, [1], config)
        assert hit == [(1, 'alice')]

    def test_different_params_are_different_keys(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        cache.put(q, [1], ['one'], config)
        cache.put(q, [2], ['two'], config)

        assert cache.get(q, [1], config) == ['one']
        assert cache.get(q, [2], config) == ['two']

    def test_put_with_no_tags_is_a_noop(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=[])

        cache.put(q, [1], ['x'], config)
        assert cache.get(q, [1], config) is None

    def test_get_returns_none_when_globally_disabled(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])
        cache.put(q, [1], ['x'], config)

        disabled_config = CacheConfig(enabled=False, path=config.path)
        assert cache.get(q, [1], disabled_config) is None

    def test_invalidate_via_worker_evicts_entry(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])
        cache.put(q, [1], ['x'], config)
        assert cache.get(q, [1], config) is not None

        from pylon._core import cache_invalidate

        cache_invalidate(['public.person'])
        assert cache.get(q, [1], config) is None


class TestJsonCache:
    def test_miss_then_put_then_hit(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        hit, value = cache.get_json(q, [1], config, kind='json_all')
        assert hit is False
        assert value is None

        cache.put_json(q, [1], '[{"name": "alice"}]', config, kind='json_all')

        hit, value = cache.get_json(q, [1], config, kind='json_all')
        assert hit is True
        assert value == '[{"name": "alice"}]'

    def test_caches_a_genuine_none_distinct_from_a_miss(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        cache.put_json(q, [1], None, config, kind='json_single')

        hit, value = cache.get_json(q, [1], config, kind='json_single')
        assert hit is True
        assert value is None

    def test_different_kinds_do_not_collide(self, tmp_path):
        """Same compiled.sql/params, different `kind` (json_all vs json_single)
        must not share a cache entry — they cache different value shapes for
        the same underlying query."""
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        cache.put_json(q, [1], '[{"name": "alice"}]', config, kind='json_all')
        cache.put_json(q, [1], '{"name": "alice"}', config, kind='json_single')

        _, all_value = cache.get_json(q, [1], config, kind='json_all')
        _, single_value = cache.get_json(q, [1], config, kind='json_single')
        assert all_value == '[{"name": "alice"}]'
        assert single_value == '{"name": "alice"}'

    def test_rows_cache_and_json_cache_do_not_collide(self, tmp_path):
        """`get`/`put` (kind="rows") and `get_json`/`put_json` must not share
        a key even for the identical compiled.sql/params — one caches a row
        list for `deserialize()`, the other a raw JSON string."""
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        cache.put(q, [1], ['row-value'], config)
        cache.put_json(q, [1], '"json-value"', config, kind='json_all')

        assert cache.get(q, [1], config) == ['row-value']
        hit, value = cache.get_json(q, [1], config, kind='json_all')
        assert hit is True
        assert value == '"json-value"'

    def test_put_json_with_no_tags_is_a_noop(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=[])

        cache.put_json(q, [1], '"x"', config, kind='json_all')
        hit, _ = cache.get_json(q, [1], config, kind='json_all')
        assert hit is False


class TestStatAndClear:
    def test_stat_returns_none_when_not_initialized(self, monkeypatch):
        monkeypatch.setattr(cache, '_enabled', False)
        assert cache.stat() is None

    def test_clear_is_a_noop_when_not_initialized(self, monkeypatch):
        monkeypatch.setattr(cache, '_enabled', False)
        cache.clear()  # must not raise

    def test_stat_reports_entry_count(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])

        assert cache.stat()['entry_count'] == 0

        cache.put(q, [1], ['x'], config)
        cache.put(q, [2], ['y'], config)
        assert cache.stat()['entry_count'] == 2

    def test_clear_evicts_everything(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        q = compiled(tags=['public.person'])
        cache.put(q, [1], ['x'], config)
        cache.put(q, [2], ['y'], config)

        cache.clear()

        assert cache.stat()['entry_count'] == 0
        assert cache.get(q, [1], config) is None
        assert cache.get(q, [2], config) is None


class TestSetOverrides:
    def _install_fake_schema(self, monkeypatch):
        # Mirrors the real `SchemaDescriptor.type_name_for_tag` (Rust-side):
        # tag -> short type name, with the "default" module mapping to the
        # "public" Postgres schema.
        fake_schema = SimpleNamespace(type_name_for_tag=lambda tag: 'Order' if tag == 'public.Order' else None)
        monkeypatch.setattr('pylon.query._get_schema', lambda: fake_schema)

    def test_no_sets_configured_never_disables(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig()
        q = compiled(tags=['public.Order'])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_set_disabled_short_circuits(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={'Order': CacheSetConfig(enabled=False)})
        q = compiled(tags=['public.Order'])
        assert cache._is_disabled_for_sets(q, config) is True

    def test_set_enabled_override_does_not_disable(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={'Order': CacheSetConfig(enabled=True)})
        q = compiled(tags=['public.Order'])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_unrelated_set_override_does_not_disable(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={'Invoice': CacheSetConfig(enabled=False)})
        q = compiled(tags=['public.Order'])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_get_and_put_respect_disabled_set(self, monkeypatch, tmp_path):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(enabled=True, path=tmp_path / 'cache', sets={'Order': CacheSetConfig(enabled=False)})
        cache.init(config)
        q = compiled(tags=['public.Order'])

        cache.put(q, [1], ['x'], config)
        assert cache.get(q, [1], config) is None


class TestMutatingQueries:
    """A mutating statement must never be cached, and must evict what it
    invalidates.

    Caching one was actively destructive rather than merely stale:
    `client.query("insert Person { name := $n }")` run twice served the
    first call's cached row the second time — returning a stale id *and
    skipping the write*, so two requested inserts produced one row.
    """

    def _config(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / 'cache')
        cache.init(config)
        return config

    def test_a_mutating_query_is_never_stored(self, tmp_path):
        config = self._config(tmp_path)
        q = compiled(sql='insert ...', tags=['public.person'], mutates=True)
        cache.put(q, [1], ['x'], config)
        assert cache.get(q, [1], config) is None

    def test_a_mutating_query_is_never_served(self, tmp_path):
        # Even an entry written before this rule existed must not be served.
        config = self._config(tmp_path)
        readonly = compiled(sql='shared', tags=['public.person'])
        cache.put(readonly, [1], ['stale'], config)
        assert cache.get(readonly, [1], config) is not None

        mutating = compiled(sql='shared', tags=['public.person'], mutates=True)
        assert cache.get(mutating, [1], config) is None

    def test_json_paths_are_guarded_too(self, tmp_path):
        config = self._config(tmp_path)
        q = compiled(sql='insert ...', tags=['public.person'], mutates=True)
        cache.put_json(q, [1], '{"a":1}', config, kind='json_all')
        assert cache.get_json(q, [1], config, kind='json_all') == (False, None)

    def test_a_write_evicts_the_tables_it_touches(self, tmp_path):
        config = self._config(tmp_path)
        read = compiled(sql='select ...', tags=['public.person'])
        cache.put(read, [1], ['before'], config)
        assert cache.get(read, [1], config) is not None

        write = compiled(sql='update ...', tags=['public.person'], mutates=True)
        cache.invalidate_for(write)
        assert cache.get(read, [1], config) is None

    def test_a_write_leaves_unrelated_tables_alone(self, tmp_path):
        config = self._config(tmp_path)
        other = compiled(sql='select ...', tags=['public.company'])
        cache.put(other, [1], ['keep'], config)

        write = compiled(sql='update ...', tags=['public.person'], mutates=True)
        cache.invalidate_for(write)
        assert cache.get(other, [1], config) is not None

    def test_a_read_does_not_evict(self, tmp_path):
        config = self._config(tmp_path)
        read = compiled(sql='select ...', tags=['public.person'])
        cache.put(read, [1], ['keep'], config)
        cache.invalidate_for(read)
        assert cache.get(read, [1], config) is not None
