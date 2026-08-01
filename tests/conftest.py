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


def live_db_dsn() -> str:
    """Same env var + default as crates/pylon-core/tests/common/mod.rs's test_dsn()."""
    return os.environ.get("PYLON_PGCON_TEST_DSN", "postgresql://postgres:postgres@localhost:5418/app")


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
        return f"{prefix}_{time.time_ns()}_{next(_module_counter)}"

    return _make
