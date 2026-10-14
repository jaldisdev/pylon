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

"""Live-Postgres tests for search/vector index backfill and amendment.

The unit side lives in `crates/pylon-core/src/diff/indexes.rs`; what needs a
real database is that the SQL those functions emit actually runs — an outbox
enqueue that Postgres rejects, or a regenerated tsvector column that doesn't
compile, would pass every pure test and fail on the first migration.

Run with: `.venv/bin/pytest tests/test_index_rebuild_live.py -m live_db`
"""

from __future__ import annotations

import asyncio

import pytest

import pylon.schema as pylon
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk

pytestmark = [pytest.mark.live_db]


def _build_schema():
    types, enums, scalars = snapshot()
    return walk(types, enums, scalars, [])


async def _outbox_rows(pool, type_name: str):
    return await pool.query_named(
        'SELECT object_id::text AS object_id, index_kind::text AS index_kind, '
        'index_name, operation, status::text AS status '
        'FROM _pylon."IndexOutbox" WHERE type_name = $1 ORDER BY object_id',
        [type_name],
    )


async def _run_ops(pool, ops: list[str]) -> None:
    for sql in ops:
        await pool.batch_execute(sql)


def test_adding_a_deferred_index_enqueues_every_existing_object(live_pool, unique_module):
    """The gap this whole feature closes: an index added to a populated type
    must pick up the objects that were already there, not just later writes."""
    from pylon._core import changed_index_backfills, export_schema

    module = unique_module('live_idx_backfill')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str

        before = _build_schema()
        await live_pool.batch_execute(export_schema(before))
        for title in ('alpha', 'beta', 'gamma'):
            await live_pool.execute(f'INSERT INTO "{module}"."Doc" (title) VALUES ($1)', [title])

        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc2:
            title: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Meilisearch,
                pointers=[pylon.SearchPointer('Doc.title')],
            )

        after = _build_schema()

        backfills = changed_index_backfills(after, before)
        assert len(backfills) == 1, [repr(b) for b in backfills]
        idx = backfills[0]
        assert idx.index_kind == 'Meilisearch'
        assert idx.index_name is None
        assert idx.table == f'"{module}"."Doc"'

        await live_pool.batch_execute(idx.backfill_sql)

        rows = await _outbox_rows(live_pool, f'{module}::Doc')
        assert len(rows) == 3, rows
        assert {r['operation'] for r in rows} == {'index'}
        assert {r['status'] for r in rows} == {'Pending'}
        assert {r['index_kind'] for r in rows} == {'Meilisearch'}
        assert {r['index_name'] for r in rows} == {None}

        # Running it again must coalesce onto the same jobs rather than
        # trip the outbox's UNIQUE NULLS NOT DISTINCT constraint.
        await live_pool.batch_execute(idx.backfill_sql)
        assert len(await _outbox_rows(live_pool, f'{module}::Doc')) == 3

    asyncio.run(run())


def test_a_backfill_revives_a_job_that_had_burned_through_its_retries(live_pool, unique_module):
    from pylon._core import deferred_index_backfills, export_schema

    module = unique_module('live_idx_revive')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Meilisearch,
                pointers=[pylon.SearchPointer('Doc.title')],
            )

        schema = _build_schema()
        await live_pool.batch_execute(export_schema(schema))
        await live_pool.execute(f'INSERT INTO "{module}"."Doc" (title) VALUES ($1)', ['alpha'])

        # The write itself enqueues; drive that job into the terminal state a
        # worker leaves behind after exhausting its attempts.
        await live_pool.batch_execute(
            f"""UPDATE _pylon."IndexOutbox"
                SET status = 'Failed', attempts = 5, next_attempt = now() + interval '1 day'
                WHERE type_name = '{module}::Doc'"""
        )

        (idx,) = deferred_index_backfills(schema)
        await live_pool.batch_execute(idx.backfill_sql)

        rows = await live_pool.query_named(
            'SELECT status::text AS status, attempts, next_attempt FROM _pylon."IndexOutbox" WHERE type_name = $1',
            [f'{module}::Doc'],
        )
        assert len(rows) == 1, rows
        assert rows[0]['status'] == 'Pending'
        assert rows[0]['attempts'] == 0
        assert rows[0]['next_attempt'] is None

    asyncio.run(run())


def test_amending_a_postgres_search_index_rewrites_its_generated_column(live_pool, unique_module):
    """The amendment the main diff can't see: it skips a search index whose
    column already exists, so a changed pointer list never reached Postgres."""
    from pylon._core import export_schema, index_amendment_ops

    module = unique_module('live_idx_pgsearch')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            body: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Postgres,
                pointers=[pylon.SearchPointer('Doc.title')],
            )

        before = _build_schema()
        await live_pool.batch_execute(export_schema(before))
        await live_pool.execute(
            f'INSERT INTO "{module}"."Doc" (title, body) VALUES ($1, $2)',
            ['alpha', 'needle'],
        )

        async def matches(term: str) -> int:
            rows = await live_pool.query_named(
                f'SELECT count(*)::int8 AS n FROM "{module}"."Doc" '
                f'WHERE "__search__" @@ plainto_tsquery(\'english\', $1)',
                [term],
            )
            return rows[0]['n']

        assert await matches('alpha') == 1
        assert await matches('needle') == 0, 'body is not in the index yet'

        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc2:
            title: str
            body: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Postgres,
                pointers=[
                    pylon.SearchPointer('Doc.title'),
                    pylon.SearchPointer('Doc.body', weight_category=pylon.SearchWeight.B),
                ],
            )

        after = _build_schema()

        ops = index_amendment_ops(after, before, False)
        assert ops, 'a changed pointer list must produce DDL'
        assert all(not non_tx for _sql, non_tx in ops), 'watch mode runs everything in one batch'
        await _run_ops(live_pool, [sql for sql, _ in ops])

        assert await matches('alpha') == 1, 'the existing pointer must survive the rebuild'
        assert await matches('needle') == 1, 'the added pointer must be indexed for rows already there'

        # The GIN index has to come back with the column, or every search
        # against this type quietly falls back to a sequential scan.
        rows = await live_pool.query_named(
            'SELECT count(*)::int8 AS n FROM pg_indexes WHERE schemaname = $1 AND indexname = $2',
            [module, 'Doc__search__'],
        )
        assert rows[0]['n'] == 1

    asyncio.run(run())


def test_amending_a_vector_dimension_rebuilds_the_column_and_enqueues_a_re_embed(live_pool, unique_module):
    from pylon._core import changed_index_backfills, export_schema, index_amendment_ops

    module = unique_module('live_idx_vecdims')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Doc.title')],
                model='mistral-embed',
                dimensions=4,
            )

        before = _build_schema()
        await live_pool.batch_execute('CREATE EXTENSION IF NOT EXISTS "vector"')
        await live_pool.batch_execute(export_schema(before))
        await live_pool.execute(f'INSERT INTO "{module}"."Doc" (title) VALUES ($1)', ['alpha'])
        await live_pool.batch_execute(f'UPDATE "{module}"."Doc" SET "__vector__" = \'[1,2,3,4]\'::vector')

        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc2:
            title: str
            pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Doc.title')],
                model='mistral-embed',
                dimensions=8,
            )

        after = _build_schema()

        await _run_ops(live_pool, [sql for sql, _ in index_amendment_ops(after, before, False)])

        rows = await live_pool.query_named(
            'SELECT format_type(a.atttypid, a.atttypmod) AS t '
            'FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid '
            'JOIN pg_namespace n ON n.oid = c.relnamespace '
            'WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3',
            [module, 'Doc', '__vector__'],
        )
        assert rows[0]['t'] == 'vector(8)', rows

        # The old 4-wide embedding is necessarily gone, so the re-embed has
        # to be queued or the column stays NULL forever.
        backfills = changed_index_backfills(after, before)
        assert [b.index_kind for b in backfills] == ['Vector'], [repr(b) for b in backfills]
        await live_pool.batch_execute(backfills[0].backfill_sql)
        assert len(await _outbox_rows(live_pool, f'{module}::Doc')) == 1

    asyncio.run(run())


def test_a_metric_change_rebuilds_the_index_without_touching_the_embeddings(live_pool, unique_module):
    from pylon._core import changed_index_backfills, export_schema, index_amendment_ops

    module = unique_module('live_idx_metric')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Doc.title')],
                model='mistral-embed',
                metric='cosine',
                dimensions=4,
            )

        before = _build_schema()
        await live_pool.batch_execute('CREATE EXTENSION IF NOT EXISTS "vector"')
        await live_pool.batch_execute(export_schema(before))
        await live_pool.execute(f'INSERT INTO "{module}"."Doc" (title) VALUES ($1)', ['alpha'])
        await live_pool.batch_execute(f'UPDATE "{module}"."Doc" SET "__vector__" = \'[1,2,3,4]\'::vector')

        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc2:
            title: str
            pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Doc.title')],
                model='mistral-embed',
                metric='euclidean',
                dimensions=4,
            )

        after = _build_schema()

        await _run_ops(live_pool, [sql for sql, _ in index_amendment_ops(after, before, False)])

        rows = await live_pool.query_named(
            'SELECT indexdef FROM pg_indexes WHERE schemaname = $1 AND indexname = $2',
            [module, 'Doc__vector__'],
        )
        assert 'vector_l2_ops' in rows[0]['indexdef'], rows

        rows = await live_pool.query_named(f'SELECT ("__vector__" IS NOT NULL) AS kept FROM "{module}"."Doc"', [])
        assert rows[0]['kept'] is True, 'a metric change must not discard the embeddings'

        assert not changed_index_backfills(after, before), 'a metric change must not bill the embedding provider again'

    asyncio.run(run())


def test_removing_a_postgres_search_index_drops_its_generated_column(live_pool, unique_module):
    from pylon._core import export_schema, index_amendment_ops

    module = unique_module('live_idx_drop')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Postgres,
                pointers=[pylon.SearchPointer('Doc.title')],
            )

        before = _build_schema()
        await live_pool.batch_execute(export_schema(before))

        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc2:
            title: str

        after = _build_schema()

        await _run_ops(live_pool, [sql for sql, _ in index_amendment_ops(after, before, False)])

        rows = await live_pool.query_named(
            'SELECT count(*)::int8 AS n FROM pg_attribute a '
            'JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace '
            'WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 AND NOT a.attisdropped',
            [module, 'Doc', '__search__'],
        )
        assert rows[0]['n'] == 0, 'a generated column nothing declares is still recomputed on every write'

    asyncio.run(run())


def test_deleting_through_an_interface_enqueues_a_job_per_implementor(live_pool, unique_module):
    """The orphan this closes: a search index is declared on the concrete type
    that holds the rows, so a delete written against the interface enqueued
    nothing and the documents outlived their rows. Live because the fan-out
    emits an outbox CTE beside each implementor's delete, reading that
    delete's own returned ids — only Postgres can confirm that runs."""
    from pylon._core import compile as pycompile
    from pylon._core import export_schema

    module = unique_module('live_idx_poly_del')

    async def run():
        clear_registry()

        @pylon.interface(module=module, name='Contact')
        class Contact:
            name: str

        @pylon.type(module=module, name='Individual')
        class Individual(Contact):
            name: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Meilisearch,
                pointers=[pylon.SearchPointer('Individual.name')],
            )

        @pylon.type(module=module, name='Business')
        class Business(Contact):
            name: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Meilisearch,
                pointers=[pylon.SearchPointer('Business.name')],
            )

        schema = _build_schema()
        await live_pool.batch_execute(export_schema(schema))
        await live_pool.execute(f'INSERT INTO "{module}"."Individual" (name) VALUES ($1)', ['alpha'])
        await live_pool.execute(f'INSERT INTO "{module}"."Business" (name) VALUES ($1)', ['beta'])
        # The inserts enqueue index jobs of their own; this is about the delete.
        await live_pool.batch_execute('DELETE FROM _pylon."IndexOutbox"')

        await live_pool.batch_execute(pycompile(f'delete {module}::Contact', schema).sql)

        for type_name in (f'{module}::Individual', f'{module}::Business'):
            rows = await _outbox_rows(live_pool, type_name)
            assert len(rows) == 1, (type_name, rows)
            assert rows[0]['operation'] == 'delete', rows
            assert rows[0]['index_kind'] == 'Meilisearch', rows
        assert await _outbox_rows(live_pool, f'{module}::Contact') == [], (
            'the interface declares no index and is indexed under no name of its own'
        )

        counts = await live_pool.query_named(
            f'SELECT (SELECT count(*) FROM "{module}"."Individual")::int8 AS individuals, '
            f'(SELECT count(*) FROM "{module}"."Business")::int8 AS businesses',
            [],
        )
        assert counts[0]['individuals'] == 0, 'the rows themselves still have to go'
        assert counts[0]['businesses'] == 0

    asyncio.run(run())


def test_a_bulk_insert_in_a_loop_enqueues_a_job_for_every_row(live_pool, unique_module):
    """`for … union (insert …)` is the bulk-write idiom and enqueued nothing at
    all, so a loop's rows reached the table and no index. Live because the fix
    turns the statement into a CTE plus a projection: Postgres has to accept
    the enqueue beside the insert, and the rows still have to come back."""
    from pylon._core import compile as pycompile
    from pylon._core import export_schema

    module = unique_module('live_idx_for_insert')

    async def run():
        clear_registry()

        @pylon.type(module=module, name='Doc')
        class Doc:
            title: str
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Meilisearch,
                pointers=[pylon.SearchPointer('Doc.title')],
            )

        schema = _build_schema()
        await live_pool.batch_execute(export_schema(schema))

        compiled = pycompile(
            f"for t in {{'alpha', 'beta', 'gamma'}} union (insert {module}::Doc {{ title := t }})",
            schema,
        )
        written = await live_pool.query_compiled(compiled, [], None)
        assert len(written) == 3, written

        rows = await _outbox_rows(live_pool, f'{module}::Doc')
        assert len(rows) == 3, rows
        assert {r['operation'] for r in rows} == {'index'}
        assert {r['status'] for r in rows} == {'Pending'}
        written_ids = await live_pool.query_named(f'SELECT id::text AS id FROM "{module}"."Doc"', [])
        assert {r['object_id'] for r in rows} == {r['id'] for r in written_ids}, (
            'every job has to name a row the loop actually wrote'
        )

    asyncio.run(run())
