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

"""Microbenchmarks for the Rust/Python boundary on the query hot path.

Each measurement isolates one crossing so its cost can be compared against
the end-to-end query it sits inside. Run with::

    PYLON_PGCON_TEST_DSN=postgresql://... .venv/bin/python bench_boundary.py

Requires a live PostgreSQL 18 database (the DSN must point at a scratch
database — this installs a throwaway module into it).
"""

from __future__ import annotations

import asyncio
import gc
import os
import pathlib
import statistics
import sys
import tempfile
import time
import uuid
from datetime import datetime, timezone

import pylon.schema as pylon_schema
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk

ITERATIONS = 2000
DB_ITERATIONS = 300


def dsn() -> str:
    value = os.environ.get('PYLON_PGCON_TEST_DSN')
    if not value:
        raise SystemExit('PYLON_PGCON_TEST_DSN must be set')
    return value


def bench(label: str, fn, iterations: int = ITERATIONS) -> float:
    """Median µs per call over `iterations`, after a warmup pass."""
    for _ in range(min(50, iterations)):
        fn()
    gc.collect()
    gc.disable()
    samples = []
    try:
        for _ in range(7):
            start = time.perf_counter()
            for _ in range(iterations):
                fn()
            samples.append((time.perf_counter() - start) / iterations * 1e6)
    finally:
        gc.enable()
    median = statistics.median(samples)
    print(f'  {label:<52} {median:9.2f} µs')
    return median


async def bench_async(label: str, fn, iterations: int = DB_ITERATIONS) -> float:
    """Minimum µs per call across samples.

    Minimum, not median: a database round trip over a container's loopback
    has a long right tail (scheduler, TCP, autovacuum) that is not a property
    of the code under test, so the median moves by hundreds of µs between
    runs. The floor is the stable statistic.
    """
    for _ in range(20):
        await fn()
    gc.collect()
    gc.disable()
    samples = []
    try:
        for _ in range(5):
            start = time.perf_counter()
            for _ in range(iterations):
                await fn()
            samples.append((time.perf_counter() - start) / iterations * 1e6)
    finally:
        gc.enable()
    best = min(samples)
    print(f'  {label:<52} {best:9.2f} µs')
    return best


async def bench_async_ab(label_a: str, fn_a, label_b: str, fn_b,
                         iterations: int = DB_ITERATIONS) -> tuple[float, float]:
    """Interleaved A/B, so drift in database latency hits both arms equally.

    Round-trip noise is far larger than the per-call difference being looked
    for, so running A to completion and then B compares two different noise
    regimes. Alternating and taking each arm's floor does not.
    """
    for _ in range(20):
        await fn_a()
        await fn_b()
    gc.collect()
    gc.disable()
    best_a = best_b = float('inf')
    try:
        for _ in range(5):
            start = time.perf_counter()
            for _ in range(iterations):
                await fn_a()
            best_a = min(best_a, (time.perf_counter() - start) / iterations * 1e6)
            start = time.perf_counter()
            for _ in range(iterations):
                await fn_b()
            best_b = min(best_b, (time.perf_counter() - start) / iterations * 1e6)
    finally:
        gc.enable()
    print(f'  {label_a:<52} {best_a:9.2f} µs')
    print(f'  {label_b:<52} {best_b:9.2f} µs')
    return best_a, best_b


# A deliberately realistic type: 15 properties across several scalar kinds,
# plus a link and a multilink — wide enough that per-query O(shape) work is
# visible, not so wide it is unrepresentative. Declared at module scope (not
# inside a function) so `from __future__ import annotations` can resolve the
# `Link[Author]`/`MultiLink[Tag]` forward references against real globals.
MODULE = f'bench_{time.time_ns()}'

clear_registry()


@pylon_schema.type(module=MODULE, name='Tag')
class Tag:
    label: str


@pylon_schema.type(module=MODULE, name='Author')
class Author:
    name: str
    email: str


@pylon_schema.type(module=MODULE, name='Article')
class Article:
    title: str
    slug: str
    body: str
    summary: str
    views: int
    rating: float
    published: bool
    created_at: datetime
    updated_at: datetime
    locale: str
    source: str
    checksum: str
    word_count: int
    read_minutes: int
    external_id: uuid.UUID
    author: pylon_schema.Link[Author] | None
    tags: pylon_schema.MultiLink[Tag]


def build_schema():
    return walk(*snapshot(), [])


QUERY = """
select Article {
    id, title, slug, body, summary, views, rating, published,
    created_at, updated_at, locale, source, checksum,
    word_count, read_minutes, external_id,
    author: { id, name, email },
    tags: { id, label },
}
filter .views > $min_views
"""


async def main() -> None:
    from pylon._core import (
        cache_get,
        cache_init,
        cache_key,
        cache_put,
        export_schema,
        migration_ensure_tracking_tables,
        migration_write_schema_snapshot,
        pgcon_connect,
    )
    from pylon.client import Client
    from pylon.config import CacheConfig, Config, DatabaseConfig
    from pylon.query import _set_schema
    from pylon.query import compile as pyql_compile

    module = MODULE
    schema = build_schema()

    pool = await pgcon_connect(dsn(), 4)
    await pool.batch_execute(export_schema(schema))
    # `Client.ensure_connected()` reinstalls the singleton from the *migrated*
    # schema in `_pylon."Schema"`, so the benchmark schema has to be written
    # there too — otherwise the client compiles against whatever a previous
    # run left behind.
    await migration_ensure_tracking_tables(pool)
    await migration_write_schema_snapshot(pool, schema.to_json())
    _set_schema(schema)

    print(f'\nSchema installed in module {module!r} '
          f'({schema.type_count} types)\n')

    # ── Seed data ────────────────────────────────────────────────────────
    cfg = Config(database=DatabaseConfig(dsn=dsn()))
    client = Client(cfg, warnings=False)
    await client.ensure_connected()

    # Seeded with literals/stdlib calls rather than bound params — parameter
    # binding is measured separately below, and keeping casts out of the seed
    # avoids conflating seed setup with what is being benchmarked.
    for i in range(50):
        await client.execute(
            f'insert {module}::Article {{'
            f" title := 'Title {i}', slug := 'slug-{i}',"
            f" body := '{'body text ' * 20}', summary := 'summary',"
            f' views := {i * 10}, rating := 4.5, published := true,'
            f' created_at := std::datetime_current(),'
            f' updated_at := std::datetime_current(),'
            f" locale := 'en', source := 'seed', checksum := 'abc',"
            f' word_count := 500, read_minutes := 3,'
            f' external_id := std::uuid_generate_v7(),'
            f' author := (select (insert {module}::Author {{'
            f" name := 'Author {i}', email := 'a{i}@example.com' }}) {{ id }}) }}"
        )

    compiled = pyql_compile(QUERY.replace('Article', f'{module}::Article'))
    params = [0]

    # ── 1. Compile-cache hit (finding #5) ────────────────────────────────
    print('COMPILE PATH')
    query_text = QUERY.replace('Article', f'{module}::Article')
    t_compile = bench('compile() — cache hit', lambda: pyql_compile(query_text))

    # ── 2. Getters that rebuild Python objects (findings #7, #6-getters) ─
    print('\nCOMPILEDQUERY GETTERS (per query execution)')
    t_shape = bench('compiled.shape (rebuilds full dict tree)', lambda: compiled.shape)
    t_sql = bench('compiled.sql (copies SQL into a Python str)', lambda: compiled.sql)
    t_tags = bench('compiled.tags (fresh list, x5 per query)', lambda: compiled.tags)
    t_params = bench('compiled.param_names (fresh list)', lambda: compiled.param_names)
    t_warn = bench('compiled.warnings() (fresh list)', lambda: compiled.warnings())
    t_mut = bench('compiled.mutates (bool, control)', lambda: compiled.mutates)
    t_plan = bench('compiled.inference_plan', lambda: compiled.inference_plan)

    # ── 3. Cache-key derivation (finding #3) ─────────────────────────────
    print('\nCACHE KEY DERIVATION (x2 per query: get + put)')
    sql_text = compiled.sql
    t_key_full = bench(
        'cache_key(shape_id, params) — as shipped',
        lambda: cache_key(f'rows\x00{compiled.shape_id}', list(params)),
    )
    t_key_pre = bench(
        'cache_key(f"...{compiled.sql}") — the old SQL-text key',
        lambda: cache_key(f'rows\x00{compiled.sql}', list(params)),
    )

    # ── 4. Value conversion both ways (findings #2, #4) ──────────────────
    print('\nVALUE CONVERSION')
    # One LMDB environment for the whole run — LMDB refuses a second
    # `Env::open` on any path within one process, so the cached-client
    # section at the end reuses this same directory.
    cache_dir = tempfile.mkdtemp()
    cache_init(cache_dir, 64)
    rows = await pool.query_compiled(compiled, params)
    print(f'  (result set: {len(rows)} rows)')
    t_put = bench(
        'cache_put — 50 rows PyObject -> DecodedValue -> rkyv',
        lambda: cache_put('k', ['t'], rows),
        iterations=200,
    )
    t_get = bench(
        'cache_get — 50 rows rkyv -> DecodedValue -> PyObject',
        lambda: cache_get('k'),
        iterations=200,
    )
    # `cache_put` bundles three costs: py_to_cached, rkyv serialization, and
    # an LMDB write transaction. `cache_key` runs py_to_cached over its
    # params and then hashes — so passing the rows as params isolates the
    # conversion from the storage write.
    t_conv_only = bench(
        '  ...of which py_to_cached alone (via cache_key)',
        lambda: cache_key('x', rows.to_list()),
        iterations=200,
    )
    # Isolate the temporal/uuid import cost specifically.
    ts_row = [
        datetime.now(timezone.utc), uuid.uuid4(), datetime.now(timezone.utc),
        uuid.uuid4(), datetime.now(timezone.utc), uuid.uuid4(),
    ]
    cache_put('temporal', ['t'], [ts_row])
    t_temporal = bench(
        'cache_get — 6 temporal/uuid values (py.import per value)',
        lambda: cache_get('temporal'),
    )
    plain_row = ['a', 'b', 'c', 1, 2, 3]
    cache_put('plain', ['t'], [plain_row])
    t_plain = bench(
        'cache_get — 6 str/int values (control, no py.import)',
        lambda: cache_get('plain'),
    )

    # ── 5. Registry rebuild (finding #6) ─────────────────────────────────
    print('\nHYDRATION')
    from pylon._core import hydrate as native_hydrate
    from pylon.query import deserialize
    from pylon.schema import schema_snapshot
    from pylon.schema._registry import named_tuples_snapshot
    from pylon.client import _hydrate

    def build_registry():
        types, enums, _ = schema_snapshot()
        registry = {t.__name__: t for t in types}
        for nt in named_tuples_snapshot():
            mod = getattr(nt, '__pylon_module__', 'default')
            registry[f'{mod}::{nt.__name__}'] = nt
        for en in enums:
            mod = getattr(en, '__pylon_module__', None) or (en.__module__ or 'default').rpartition('.')[-1] or 'default'
            registry[en.__name__] = en
            registry[f'{mod}::{en.__name__}'] = en
        return registry

    from pylon.query import hydration_registry

    prebuilt = build_registry()
    t_registry = bench('registry rebuild alone (per query)', build_registry, iterations=500)
    plain_rows = rows.to_list()
    t_deser = bench('deserialize() — Python reference walk, 50 rows',
                    lambda: deserialize(plain_rows, compiled, prebuilt), iterations=200)
    native_reg = hydration_registry()
    t_native = bench('_core.hydrate() — native walk, 50 rows',
                     lambda: native_hydrate(rows, compiled, native_reg), iterations=200)
    t_hydrate = bench('_hydrate() — as shipped (registry + shape + decode)',
                      lambda: _hydrate(rows, compiled), iterations=200)
    # Finding #8's wrapper is gone; kept as a measurement of what it cost.
    t_wrap = bench('(removed) per-row dict wrapper, for reference',
                   lambda: [{'result': r} for r in rows], iterations=2000)

    # ── 6. shape_id A/B (finding #1) ─────────────────────────────────────
    print('\nDATABASE ROUND TRIP')
    print('  (query_compiled calls shape_id(); query() does not — the delta')
    print('   is the per-execution Debug-format + SHA-256 of the shape tree)')
    t_qc, t_q = await bench_async_ab(
        'pool.query_compiled(compiled, params)',
        lambda: pool.query_compiled(compiled, params),
        'pool.query(sql_text, params) — no shape_id',
        lambda: pool.query(sql_text, params),
    )

    # ── 7. prepare vs prepare_cached estimate (adjacent finding) ─────────
    trivial = 'SELECT 1 AS result'
    t_prep, t_batch = await bench_async_ab(
        'pool.query("SELECT 1") — prepare + execute (2 round trips)',
        lambda: pool.query(trivial, []),
        'pool.batch_execute("SELECT 1") — simple protocol (1 round trip)',
        lambda: pool.batch_execute(trivial),
    )

    # ── 8. End to end ────────────────────────────────────────────────────
    print('\nEND TO END')
    t_e2e = await bench_async('client.query(...) — cache off',
                              lambda: client.query(query_text, min_views=0))

    cached_cfg = Config(
        database=DatabaseConfig(dsn=dsn()),
        cache=CacheConfig(enabled=True, path=pathlib.Path(cache_dir)),
    )
    import pylon.cache as pylon_cache

    # `cache_init` already ran above on this same directory. LMDB refuses a
    # second open of it, and `Client.ensure_connected()` calls
    # `pylon.cache.init()` unconditionally — so neutralize that one call and
    # flip the module flag directly instead.
    pylon_cache._enabled = True
    pylon_cache.init = lambda _cfg: None
    cached_client = Client(cached_cfg, warnings=False)
    await cached_client.ensure_connected()
    t_e2e_cached = await bench_async('client.query(...) — cache on, warm hit',
                                     lambda: cached_client.query(query_text, min_views=0))

    # ── 9. Compile concurrency (Phase 4) ─────────────────────────────────
    # Compilation holds no Python object, so `compile()` releases the GIL for
    # its duration. That is invisible to a single-threaded measurement — the
    # only way to see it is whether threads scale. Each thread compiles
    # *distinct* query texts, so every call is a genuine cache miss and does
    # real parse/IR/typecheck/emit work; on a cache hit there is nothing to
    # overlap.
    print('\nCOMPILE CONCURRENCY (distinct queries, forced cache misses)')
    import concurrent.futures

    from pylon._core import clear_query_cache

    def compile_batch(worker: int, count: int) -> None:
        for i in range(count):
            pyql_compile(f'select {module}::Article {{ title }} filter .views > {worker * 100_000 + i}')

    def run_threaded(threads: int, per_thread: int = 150) -> float:
        clear_query_cache()
        with concurrent.futures.ThreadPoolExecutor(max_workers=threads) as pool_exec:
            start = time.perf_counter()
            list(pool_exec.map(lambda w: compile_batch(w, per_thread), range(threads)))
            elapsed = time.perf_counter() - start
        return threads * per_thread / elapsed

    one = run_threaded(1)
    four = run_threaded(4)
    print(f'  {"1 thread":<52} {one:9.0f} compiles/s')
    print(f'  {"4 threads":<52} {four:9.0f} compiles/s')
    print(f'  {"scaling":<52} {four / one:9.2f}x')

    # ── Summary ──────────────────────────────────────────────────────────
    print('\n' + '=' * 72)
    print('PER-QUERY BOUNDARY OVERHEAD (cache-off path)')
    print('=' * 72)
    breakdown = [
        ('compile() cache hit', t_compile),
        ('compiled.shape', t_shape),
        ('compiled.sql x2 (cache key)', t_sql * 2),
        ('compiled.tags x5', t_tags * 5),
        ('compiled.param_names', t_params),
        ('compiled.warnings()', t_warn),
        ('compiled.inference_plan', t_plan),
        ('cache_key x2', t_key_full * 2),
        ('registry rebuild', t_registry),
        ('{"result": r} wrapper', t_wrap),
        ('shape_id (query_compiled - query)', max(0.0, t_qc - t_q)),
    ]
    total = sum(v for _, v in breakdown)
    for name, value in breakdown:
        print(f'  {name:<44} {value:9.2f} µs  {value / t_e2e * 100:5.1f}% of e2e')
    print(f'  {"-" * 44} {"-" * 9}')
    print(f'  {"TOTAL avoidable-ish overhead":<44} {total:9.2f} µs  '
          f'{total / t_e2e * 100:5.1f}% of e2e')
    print(f'  {"end-to-end client.query()":<44} {t_e2e:9.2f} µs')
    print(f'  {"prepare round trip (est.)":<44} '
          f'{max(0.0, t_prep - t_batch):9.2f} µs')

    await pool.batch_execute(f'DROP SCHEMA IF EXISTS "{module}" CASCADE')
    await client.aclose()
    await cached_client.aclose()


if __name__ == '__main__':
    sys.exit(asyncio.run(main()))
