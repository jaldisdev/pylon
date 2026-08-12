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
import itertools
import os
import time

import pytest

#: Attributes the suite needs `pylon._core` to expose. A missing one means
#: the compiled extension predates the Python code being tested against it.
_REQUIRED_CORE_API = ('PropertyDescriptor', 'Guidance', 'SchemaDescriptor', 'pgcon_connect')


def pytest_configure(config: pytest.Config) -> None:
    """Fail the whole run on a stale `pylon._core`, rather than skipping.

    Whole test classes used to be `skipif`'d on `hasattr(_core, ...)`, which
    meant an out-of-date compiled extension produced a green run with those
    classes silently absent — the exact failure mode of `maturin develop`
    leaving a stale `.so` behind. A build problem should look like a build
    problem.
    """
    try:
        from pylon import _core
    except ImportError as exc:  # pragma: no cover - environment error
        raise pytest.UsageError(
            f'pylon._core is not importable ({exc}). Build it with: maturin develop && pip install -e ./'
        ) from exc

    missing = [name for name in _REQUIRED_CORE_API if not hasattr(_core, name)]
    if missing:  # pragma: no cover - environment error
        raise pytest.UsageError(
            'pylon._core is out of date — missing '
            + ', '.join(missing)
            + '. Rebuild it with: maturin develop && pip install -e ./'
        )


def live_db_dsn() -> str:
    """Same env var as crates/pylon-core/tests/common/mod.rs's test_dsn() — no
    hardcoded fallback, must be set explicitly.
    """
    dsn = os.environ.get('PYLON_PGCON_TEST_DSN')
    if not dsn:
        raise RuntimeError('PYLON_PGCON_TEST_DSN must be set to run live-Postgres tests')
    return dsn


@pytest.fixture
def live_pool():
    from pylon._core import export_stdlib, pgcon_connect

    async def _connect():
        # `pgcon_connect(...)` must be *called* while an event loop is
        # already running (pyo3-asyncio's `future_into_py` grabs the
        # current running loop at call time, not at await time) — calling
        # it directly as `asyncio.run(pgcon_connect(...))`'s argument
        # evaluates it too early, before the loop exists.
        pool = await pgcon_connect(live_db_dsn(), 2)
        # Every concrete table gets an unconditional `pylon_cache_invalidate`
        # trigger, which references `_pylon.notify_cache_invalidate()` — that
        # function (and the `_pylon` schema itself) only exist once
        # `export_stdlib()`'s DDL has run, normally done once via `pylon
        # database install`. Idempotent (CREATE ... IF NOT EXISTS throughout),
        # safe to run before every test.
        await pool.batch_execute(export_stdlib())
        return pool

    return asyncio.run(_connect())


_module_counter = itertools.count()


@pytest.fixture
def unique_module():
    """Returns a `prefix -> unique module name` factory — nanos + a counter,
    mirroring tests/common/mod.rs's unique_module() (nanos alone can collide
    if the OS timer resolution is coarser than 1ns). A fixture (not a plain
    importable function) since tests/ has no __init__.py, so a relative
    import between test modules isn't available — pytest fixture injection
    is the sharing mechanism that works without one.
    """

    def _make(prefix: str) -> str:
        return f'{prefix}_{time.time_ns()}_{next(_module_counter)}'

    return _make
