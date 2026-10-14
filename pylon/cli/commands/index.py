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

"""Search and vector index commands — pylon index <subcommand>."""

from __future__ import annotations

import asyncio

import click

from ..config import requires_config

# Objects already queued for this index. Counted after the enqueue rather
# than returned by it, so the enqueue statement stays byte-identical to the
# one a migration carries.
_QUEUE_DEPTH_SQL = """
SELECT count(*)::int8 AS depth
FROM _pylon."IndexOutbox"
WHERE type_name = $1 AND index_kind = $2::_pylon."IndexKind"
  AND index_name IS NOT DISTINCT FROM $3
"""


def _pg_dsn(config) -> str:
    db = config.database
    if db.dsn is not None:
        return db.dsn.replace('pylon://', 'postgresql://', 1)
    pw = f':{db.password}' if db.password else ''
    return f'postgresql://{db.user}{pw}@{db.host}:{db.port}/{db.name}'


def _label(idx) -> str:
    suffix = '' if idx.index_name is None else f' {idx.index_name!r}'
    return f'{idx.type_name} {idx.index_kind}{suffix}'


def _matches(idx, type_filter: str | None, index_filter: str | None, kind_filter: str | None) -> bool:
    if type_filter is not None:
        # Accept both the qualified name and the bare one, since a schema
        # author writes `Product` and the outbox stores `default::Product`.
        bare = idx.type_name.rsplit('::', 1)[-1]
        if type_filter not in (idx.type_name, bare):
            return False
    if index_filter is not None:
        # A type's default index has no name of its own; `default` is the
        # only way to ask for it on a command line.
        wanted = None if index_filter == 'default' else index_filter
        if idx.index_name != wanted:
            return False
    if kind_filter is not None:
        is_vector = idx.index_kind == 'Vector'
        if (kind_filter.lower() == 'vector') != is_vector:
            return False
    return True


@click.group()
def index() -> None:
    """Inspect and rebuild Pylon's deferred search and vector indexes.

    "Deferred" means worker-driven: an OpenSearch or Meilisearch index, or a
    vector (embedding) index. A Postgres-backed search index is a generated
    tsvector column Postgres keeps current itself, so it never appears here
    and never needs rebuilding.
    """


async def _load_applied_schema(config):
    """The schema as actually applied to the database, plus a live pool.

    Deliberately *not* the compiled schema files: those may declare an index
    no migration has created yet, and enqueuing jobs for one would hand the
    workers objects to index against a definition the database doesn't have.
    """
    from pylon._core import (
        SchemaDescriptor,
        migration_ensure_internal_schema,
        migration_read_schema_snapshot,
        pgcon_connect,
    )

    pool = await pgcon_connect(_pg_dsn(config), 2)
    await migration_ensure_internal_schema(pool)
    snapshot = await migration_read_schema_snapshot(pool)
    if snapshot is None:
        raise click.ClickException("This database has no applied schema yet. Run 'pylon migration apply' first.")
    return pool, SchemaDescriptor.from_json(snapshot)


@index.command('list')
@requires_config
@click.pass_context
def list_cmd(ctx: click.Context) -> None:
    """List every deferred index and how many jobs are queued for it."""

    async def run() -> None:
        from pylon._core import deferred_index_backfills

        pool, schema = await _load_applied_schema(ctx.obj['config'])
        indexes = deferred_index_backfills(schema)
        if not indexes:
            click.echo('No deferred search or vector indexes in the applied schema.')
            return

        click.echo(f'{"TYPE":<36}{"KIND":<14}{"INDEX":<16}QUEUED')
        for idx in indexes:
            rows = await pool.query_named(_QUEUE_DEPTH_SQL, [idx.type_name, idx.index_kind, idx.index_name])
            depth = rows[0]['depth'] if rows else 0
            click.echo(f'{idx.type_name:<36}{idx.index_kind:<14}{idx.index_name or "(default)":<16}{depth}')

    asyncio.run(run())


@index.command()
@click.option(
    '--type',
    'type_filter',
    default=None,
    metavar='NAME',
    help='Only this object type, qualified or bare (e.g. default::Product or Product).',
)
@click.option(
    '--index',
    'index_filter',
    default=None,
    metavar='NAME',
    help="Only this named index; pass 'default' for a type's unnamed one.",
)
@click.option(
    '--kind',
    'kind_filter',
    type=click.Choice(['vector', 'search'], case_sensitive=False),
    default=None,
    help='Only vector (embedding) indexes, or only full-text search indexes.',
)
@click.option(
    '--yes', is_flag=True, default=False, help='Skip the confirmation prompt (and the object counts it needs).'
)
@requires_config
@click.pass_context
def rebuild(
    ctx: click.Context,
    type_filter: str | None,
    index_filter: str | None,
    kind_filter: str | None,
    yes: bool,
) -> None:
    """Re-index every existing object into its deferred search/vector indexes.

    Enqueues one `_pylon."IndexOutbox"` job per object per matching index;
    the index workers do the actual indexing, at their own pace.

    `pylon migration create` already writes this backfill into the migration
    for an index it sees added or amended, so this command is for what it
    cannot know about: a search engine that was wiped, moved, or replaced
    out from under the database, jobs dropped by hand, or a migration created
    with --no-index-backfill.

    Rebuilding a vector index re-embeds every matching object, which bills
    the embedding provider for all of them.
    """

    async def run() -> None:
        from pylon._core import deferred_index_backfills

        pool, schema = await _load_applied_schema(ctx.obj['config'])
        indexes = [
            idx for idx in deferred_index_backfills(schema) if _matches(idx, type_filter, index_filter, kind_filter)
        ]
        if not indexes:
            click.echo('No deferred search or vector indexes match.')
            return

        if not yes:
            click.echo(f'Rebuilding {len(indexes)} index(es):')
            total = 0
            for idx in indexes:
                rows = await pool.query_named(f'SELECT count(*)::int8 AS n FROM {idx.table}', [])
                n = rows[0]['n'] if rows else 0
                total += n
                click.echo(f'  {_label(idx)}  — {n} object(s)')
            if any(idx.index_kind == 'Vector' for idx in indexes):
                click.echo('\nVector indexes re-embed every object; the embedding provider bills for all of them.')
            click.confirm(f'\nEnqueue {total} indexing job(s)?', abort=True)

        for idx in indexes:
            await pool.execute(idx.backfill_sql, [])
            rows = await pool.query_named(_QUEUE_DEPTH_SQL, [idx.type_name, idx.index_kind, idx.index_name])
            depth = rows[0]['depth'] if rows else 0
            click.echo(f'  {_label(idx)}  — {depth} job(s) queued')

        click.echo("\nThe index workers drain these. Watch progress with 'pylon index list'.")

    asyncio.run(run())
