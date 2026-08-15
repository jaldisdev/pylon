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
import logging
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

from pylon import cache, workers
from pylon.config import CacheConfig
from pylon.exceptions import InterfaceError

# Every test here builds task *lists* and asserts on their length rather than
# awaiting anything. The cache worker is a pyo3 coroutine that connects on
# first poll, so the cases that would actually construct one are the cases
# these tests deliberately arrange not to reach.


def config_double(tmp_path, *, cache_enabled: bool) -> SimpleNamespace:
    return SimpleNamespace(
        cache=CacheConfig(enabled=cache_enabled, path=tmp_path / 'cache'),
        database=SimpleNamespace(dsn='postgresql://user@localhost:5432/db'),
    )


def schema_double(*, vector: bool = False, search: bool = False) -> SimpleNamespace:
    td = SimpleNamespace(
        module='default',
        name='Doc',
        vector_indexes=['vi'] if vector else [],
        search_indexes=['si'] if search else [],
    )
    return SimpleNamespace(types=[td])


@pytest.fixture(autouse=True)
def no_signal_handlers(monkeypatch):
    """Default every test to "no `@pylon.signal` registered".

    The registry is process-global and populated by importing a schema, so
    without this a test's task count would depend on which other test module
    ran first.
    """
    from pylon.schema import _registry

    monkeypatch.setattr(_registry, 'signals_snapshot', lambda: {})
    monkeypatch.setattr(cache, '_enabled', False)
    yield


def with_signal_handler(monkeypatch):
    """Make the build produce exactly one worker, and make it inert.

    The registry patch is what puts a dispatcher in the list; stubbing the
    dispatcher itself keeps these tests from depending on a task being
    cancelled before it reaches its first `await` — a real one would try to
    connect there.
    """
    from pylon import signals
    from pylon.schema import _registry

    monkeypatch.setattr(_registry, 'signals_snapshot', lambda: {('default::Doc', 'insert'): [object()]})

    async def stub_dispatcher(dsn, *, batch_size=50, poll_interval=30.0):
        await asyncio.Event().wait()

    monkeypatch.setattr(signals, 'run_signal_dispatcher', stub_dispatcher)


class TestCheckDisabled:
    def test_accepts_known_kinds(self):
        assert workers._check_disabled(['cache', 'signals']) == frozenset({'cache', 'signals'})
        assert workers._check_disabled([]) == frozenset()

    def test_rejects_unknown_kind(self):
        # Misspellings fail loudly: silently ignoring one would leave the
        # worker it was meant to suppress running.
        with pytest.raises(InterfaceError, match="unknown worker kind\\(s\\) 'caches'"):
            workers._check_disabled(['caches'])

    def test_rejects_a_kind_that_moved_to_the_server(self):
        with pytest.raises(InterfaceError, match='vector'):
            workers._check_disabled(['vector'])


class TestDisabling:
    def test_signal_dispatcher_runs_when_handlers_are_registered(self, tmp_path, monkeypatch):
        with_signal_handler(monkeypatch)
        tasks = workers.build_worker_tasks(schema_double(), config_double(tmp_path, cache_enabled=False))
        assert len(tasks) == 1
        tasks[0].close()  # never scheduled; closing avoids an un-awaited warning

    def test_disabled_signals_skips_the_dispatcher(self, tmp_path, monkeypatch):
        with_signal_handler(monkeypatch)
        tasks = workers.build_worker_tasks(
            schema_double(),
            config_double(tmp_path, cache_enabled=False),
            disabled=['signals'],
        )
        assert tasks == []

    def test_disabled_cache_skips_the_invalidation_worker(self, tmp_path):
        # Cache enabled in config, so without the flag this would build a
        # worker (and, with shared_cache, raise below).
        tasks = workers.build_worker_tasks(
            schema_double(),
            config_double(tmp_path, cache_enabled=True),
            shared_cache=True,
            disabled=['cache'],
        )
        assert tasks == []


class TestSharedCacheOrdering:
    def test_raises_when_the_cache_was_never_opened(self, tmp_path):
        with pytest.raises(InterfaceError, match='ensure_connected'):
            workers.build_worker_tasks(
                schema_double(),
                config_double(tmp_path, cache_enabled=True),
                shared_cache=True,
            )

    def test_no_error_when_the_cache_is_disabled_entirely(self, tmp_path):
        # shared_cache is about *how* to attach, not whether to: with no
        # cache configured there is nothing to attach to and nothing to warn
        # about.
        tasks = workers.build_worker_tasks(
            schema_double(),
            config_double(tmp_path, cache_enabled=False),
            shared_cache=True,
        )
        assert tasks == []


class TestBackgroundWorkers:
    """The start/stop handle, for frameworks whose lifetime is two callbacks.

    Every case here configures no workers at all, so `start()` schedules an
    empty list — enough to exercise the handle's own bookkeeping without a
    worker that would want a database on its first poll.
    """

    def handle(self, tmp_path) -> workers.BackgroundWorkers:
        return workers.BackgroundWorkers(schema_double(), config_double(tmp_path, cache_enabled=False))

    def test_start_then_stop(self, tmp_path):
        async def scenario():
            handle = self.handle(tmp_path)
            assert handle.tasks == []
            await handle.start()
            await handle.stop()
            assert handle.tasks == []

        asyncio.run(scenario())

    def test_stop_without_start_is_a_noop(self, tmp_path):
        # A startup hook that raised before reaching start() still leaves the
        # shutdown hook to run; it shouldn't have to guard against that.
        asyncio.run(self.handle(tmp_path).stop())

    def test_double_stop_is_a_noop(self, tmp_path):
        async def scenario():
            handle = self.handle(tmp_path)
            await handle.start()
            await handle.stop()
            await handle.stop()

        asyncio.run(scenario())

    def test_start_twice_raises(self, tmp_path, monkeypatch):
        with_signal_handler(monkeypatch)  # so start() actually schedules something

        async def scenario():
            handle = self.handle(tmp_path)
            await handle.start()
            try:
                with pytest.raises(InterfaceError, match='already started'):
                    await handle.start()
            finally:
                await handle.stop()

        asyncio.run(scenario())

    def test_run_workers_yields_its_tasks(self, tmp_path, monkeypatch):
        with_signal_handler(monkeypatch)

        async def scenario():
            async with workers.run_workers(
                schema_double(),
                config_double(tmp_path, cache_enabled=False),
            ) as tasks:
                assert len(tasks) == 1
                started = tasks[0]
            assert started.cancelled()

        asyncio.run(scenario())


class TestUnclaimedIndexWarning:
    def test_notes_indexes_this_process_will_not_drain(self, tmp_path, caplog):
        with caplog.at_level(logging.INFO):
            workers.build_worker_tasks(
                schema_double(vector=True, search=True),
                config_double(tmp_path, cache_enabled=False),
            )
        assert 'SearchIndex/VectorIndex' in caplog.text
        assert 'pylon-server' in caplog.text

    def test_silent_when_the_schema_declares_none(self, tmp_path, caplog):
        with caplog.at_level(logging.INFO):
            workers.build_worker_tasks(schema_double(), config_double(tmp_path, cache_enabled=False))
        assert 'pylon-server' not in caplog.text
