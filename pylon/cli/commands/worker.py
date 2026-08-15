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

import click

from ..config import _print_error, requires_config


@click.group()
def worker() -> None:
    """Run Pylon's Python-side background workers and manage the index outbox.

    The outbox subcommands (`failed`, `retry`) administer rows regardless of
    which process drains them — `pylon-server` does, for the vector and
    search indexes those rows belong to.
    """


@worker.command()
@click.option('--batch-size', default=50, show_default=True, help='Number of SignalOutbox rows to claim per cycle.')
@click.option('--poll-interval', default=30.0, show_default=True, help='Seconds between polling cycles when idle.')
@click.option('--disable-cache-worker', is_flag=True, help='Skip the cache-invalidation worker.')
@click.option('--disable-signal-dispatcher', is_flag=True, help='Skip the signal dispatcher.')
@click.option(
    '--log-level',
    default='INFO',
    show_default=True,
    type=click.Choice(['DEBUG', 'INFO', 'WARNING', 'ERROR'], case_sensitive=False),
    help='Logging level.',
)
@requires_config
@click.pass_context
def start(
    ctx: click.Context,
    batch_size: int,
    poll_interval: float,
    disable_cache_worker: bool,
    disable_signal_dispatcher: bool,
    log_level: str,
) -> None:
    """Start the background workers that have to run in a Python process.

    Two do, and they are the two this command runs: the signal dispatcher,
    because a `@pylon.signal` handler is a live Python callable that exists
    nowhere else, and cache invalidation, because it evicts from an LMDB
    file on local disk and so has to reach the cache a nearby process
    actually reads. Vector and search indexing claim outbox rows the
    database arbitrates, can run anywhere, and run in `pylon-server`.

    The two `--disable-*` flags split even this pair across processes, for a
    deployment running the cache invalidator next to each application (which
    has to share that application's cache directory to evict anything it
    will ever read) and the signal dispatcher once, somewhere central.

    See `pylon.workers.run_workers` for running either inside an application
    process instead of this one.
    """
    logging.basicConfig(
        level=getattr(logging, log_level.upper()),
        format='%(asctime)s  %(levelname)-8s  %(name)s  %(message)s',
    )
    log = logging.getLogger(__name__)

    import pylon
    import pylon.query as _q
    from pylon.workers import build_worker_tasks

    pylon.finalize()
    config = ctx.obj['config']
    schema = _q._singleton

    disabled = [kind for kind, off in (('cache', disable_cache_worker), ('signals', disable_signal_dispatcher)) if off]

    tasks = build_worker_tasks(
        schema,
        config,
        batch_size=batch_size,
        poll_interval=poll_interval,
        log=log,
        disabled=disabled,
    )
    if not tasks:
        hint = (
            'Set [cache].enabled = true in pylon.toml to start the cache-invalidation '
            'worker, or register a @pylon.signal handler to start the dispatcher.'
        )
        if disabled:
            hint = f'Disabled by flag: {", ".join(disabled)}. {hint}'
        _print_error('no workers to start', hint)
        ctx.exit(1)
        return

    async def run() -> None:
        # The cache worker's connection is dropped along with the process,
        # matching this command's own lifecycle (runs until interrupted), so
        # there's no Python-side cleanup to do for it; the signal
        # dispatcher's connection is likewise dropped when its task is
        # cancelled.
        await asyncio.gather(*tasks)

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        click.echo('\nWorker stopped.')
    except Exception as exc:
        _print_error('worker crashed', str(exc))
        sys.exit(1)


# ── Failed-row inspection and requeue ─────────────────────────────────────────
#
# A row that exhausts `MAX_ATTEMPTS` is parked as `Failed` and no worker will
# ever claim it again. Without these two commands the only way to see or
# recover that work is hand-written SQL against `_pylon."IndexOutbox"`.

_FAILED_LIST_SQL = """
SELECT (id::text, index_kind::text, type_name, coalesce(index_name, ''), operation,
        attempts, enqueued_at::text) AS result
FROM _pylon."IndexOutbox"
WHERE status = 'Failed'
ORDER BY enqueued_at
LIMIT $1
"""

_FAILED_COUNT_SQL = """
SELECT (index_kind::text, count(*)) AS result
FROM _pylon."IndexOutbox"
WHERE status = 'Failed'
GROUP BY index_kind
ORDER BY index_kind
"""

_REQUEUE_SQL = """
UPDATE _pylon."IndexOutbox"
SET status = 'Pending', attempts = 0, next_attempt = NULL, claimed_at = NULL
WHERE status = 'Failed'
  AND ($1 = '' OR index_kind::text = $1)
"""


def _dsn_from(config) -> str:
    db = config.database
    if db.dsn:
        return db.dsn.replace('pylon://', 'postgresql://', 1)
    pw = f':{db.password}' if db.password else ''
    return f'postgresql://{db.user}{pw}@{db.host}:{db.port}/{db.name}'


@worker.command()
@click.option('--limit', default=50, show_default=True, help='Maximum number of rows to list.')
@requires_config
@click.pass_context
def failed(ctx: click.Context, limit: int) -> None:
    """List IndexOutbox rows that exhausted every retry.

    These are terminal: no worker will claim them again until they are
    requeued with `pylon worker retry`.
    """
    config = ctx.obj['config']

    async def run() -> None:
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_dsn_from(config), 2)
        counts = await pool.query(_FAILED_COUNT_SQL, [])
        if not counts:
            click.echo('No failed rows.')
            return

        total = sum(row[1] for row in counts)
        summary = ', '.join(f'{row[0]}: {row[1]}' for row in counts)
        click.echo(f'{total} failed row(s) — {summary}\n')

        rows = await pool.query(_FAILED_LIST_SQL, [limit])
        header = f'{"INDEX KIND":<14}{"TYPE":<28}{"INDEX":<16}{"OP":<8}{"TRIES":<7}ENQUEUED'
        click.echo(header)
        click.echo('-' * len(header))
        for row in rows:
            _id, kind, type_name, index_name, operation, attempts, enqueued = row
            click.echo(f'{kind:<14}{type_name:<28}{index_name or "-":<16}{operation:<8}{attempts:<7}{enqueued}')
        if total > limit:
            click.echo(f'\n... {total - limit} more (raise --limit to see them)')
        click.echo("\nRequeue with 'pylon worker retry'.")

    asyncio.run(run())


@worker.command()
@click.option('--index-kind', default=None, help='Only requeue this kind (Vector, OpenSearch, Meilisearch).')
@click.option('--yes', is_flag=True, default=False, help='Skip the confirmation prompt.')
@requires_config
@click.pass_context
def retry(ctx: click.Context, index_kind: str | None, yes: bool) -> None:
    """Requeue failed IndexOutbox rows so `pylon-server` picks them up again.

    Resets `attempts` and clears the backoff, putting each row back to
    Pending. Fix whatever made them fail first — otherwise they will simply
    burn through their retries again.
    """
    config = ctx.obj['config']

    async def run() -> None:
        from pylon._core import pgcon_connect

        pool = await pgcon_connect(_dsn_from(config), 2)
        counts = await pool.query(_FAILED_COUNT_SQL, [])
        if index_kind:
            counts = [row for row in counts if row[0] == index_kind]
        if not counts:
            click.echo('No failed rows to requeue.')
            return

        total = sum(row[1] for row in counts)
        scope = f' for {index_kind}' if index_kind else ''
        if not yes:
            click.confirm(f'Requeue {total} failed row(s){scope}?', abort=True)

        await pool.execute(_REQUEUE_SQL, [index_kind or ''])
        click.echo(f'Requeued {total} row(s){scope}.')

    asyncio.run(run())
