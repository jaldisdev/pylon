from __future__ import annotations

import asyncio
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

from pylon import cache
from pylon.config import CacheConfig, CacheSetConfig


def compiled(sql: str = "select 1", tags: list[str] | None = None) -> SimpleNamespace:
    return SimpleNamespace(sql=sql, tags=tags if tags is not None else [])


@pytest.fixture(autouse=True)
def reset_cache_state(monkeypatch):
    """Every test gets its own LMDB dir via `init()` — reset the module-level
    `_enabled` flag first so a disabled-config test doesn't inherit `True`
    from a prior test in the same process."""
    monkeypatch.setattr(cache, "_enabled", False)
    yield


class TestInit:
    def test_disabled_config_is_a_noop(self, tmp_path):
        config = CacheConfig(enabled=False, path=tmp_path / "cache")
        cache.init(config)
        assert cache._enabled is False
        assert not (tmp_path / "cache").exists()

    def test_enabled_config_opens_lmdb(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        assert cache._enabled is True
        assert (tmp_path / "cache").is_dir()


class TestGetPutRoundTrip:
    def test_miss_then_put_then_hit(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        q = compiled(tags=["public.person"])

        assert cache.get(q, [1], config) is None

        records = [{"result": (1, "alice")}]
        cache.put(q, [1], records, config)

        hit = cache.get(q, [1], config)
        assert hit == [{"result": (1, "alice")}]

    def test_different_params_are_different_keys(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        q = compiled(tags=["public.person"])

        cache.put(q, [1], [{"result": "one"}], config)
        cache.put(q, [2], [{"result": "two"}], config)

        assert cache.get(q, [1], config) == [{"result": "one"}]
        assert cache.get(q, [2], config) == [{"result": "two"}]

    def test_put_with_no_tags_is_a_noop(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        q = compiled(tags=[])

        cache.put(q, [1], [{"result": "x"}], config)
        assert cache.get(q, [1], config) is None

    def test_get_returns_none_when_globally_disabled(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        q = compiled(tags=["public.person"])
        cache.put(q, [1], [{"result": "x"}], config)

        disabled_config = CacheConfig(enabled=False, path=config.path)
        assert cache.get(q, [1], disabled_config) is None

    def test_invalidate_via_worker_evicts_entry(self, tmp_path):
        config = CacheConfig(enabled=True, path=tmp_path / "cache")
        cache.init(config)
        q = compiled(tags=["public.person"])
        cache.put(q, [1], [{"result": "x"}], config)
        assert cache.get(q, [1], config) is not None

        from pylon._core import cache_invalidate

        cache_invalidate(["public.person"])
        assert cache.get(q, [1], config) is None


class TestSetOverrides:
    def _install_fake_schema(self, monkeypatch):
        fake_type = SimpleNamespace(name="Order", module="default", table="Order")
        fake_schema = SimpleNamespace(types=[fake_type])
        monkeypatch.setattr("pylon.query._get_schema", lambda: fake_schema)

    def test_no_sets_configured_never_disables(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig()
        q = compiled(tags=["public.Order"])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_set_disabled_short_circuits(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={"Order": CacheSetConfig(enabled=False)})
        q = compiled(tags=["public.Order"])
        assert cache._is_disabled_for_sets(q, config) is True

    def test_set_enabled_override_does_not_disable(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={"Order": CacheSetConfig(enabled=True)})
        q = compiled(tags=["public.Order"])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_unrelated_set_override_does_not_disable(self, monkeypatch):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(sets={"Invoice": CacheSetConfig(enabled=False)})
        q = compiled(tags=["public.Order"])
        assert cache._is_disabled_for_sets(q, config) is False

    def test_get_and_put_respect_disabled_set(self, monkeypatch, tmp_path):
        self._install_fake_schema(monkeypatch)
        config = CacheConfig(
            enabled=True, path=tmp_path / "cache", sets={"Order": CacheSetConfig(enabled=False)}
        )
        cache.init(config)
        q = compiled(tags=["public.Order"])

        cache.put(q, [1], [{"result": "x"}], config)
        assert cache.get(q, [1], config) is None


class TestCacheInvalidationWorker:
    def test_notify_triggers_invalidation(self, monkeypatch):
        invalidated: list[list[str]] = []

        async def fake_invalidate(self, tags):
            invalidated.append(tags)

        monkeypatch.setattr(cache.CacheInvalidationWorker, "_invalidate", fake_invalidate)

        class FakeConn:
            def __init__(self):
                self.listeners = {}

            async def add_listener(self, channel, callback):
                self.listeners[channel] = callback

            async def remove_listener(self, channel, callback):
                del self.listeners[channel]

        async def scenario():
            conn = FakeConn()
            worker = cache.CacheInvalidationWorker(conn)
            run_task = asyncio.ensure_future(worker.run())
            await asyncio.sleep(0)  # let run() reach add_listener

            worker._on_notify(conn, 1, cache.NOTIFY_CHANNEL, "public.person")
            await asyncio.sleep(0.01)  # let the scheduled _drain() run

            run_task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await run_task

        asyncio.run(scenario())
        assert invalidated == [["public.person"]]
