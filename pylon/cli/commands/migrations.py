import hashlib
import subprocess
import tempfile
from pathlib import Path

import click

from ..config import _print_error, requires_config


def _require_migrations_dir(ctx: click.Context, migrations_dir: Path) -> None:
    if not migrations_dir.is_dir():
        _print_error(
            'migrations directory not found',
            f'Expected at {migrations_dir}',
        )
        ctx.exit(1)


def _next_seq(migrations_dir: Path) -> int:
    existing = sorted(migrations_dir.glob('[0-9][0-9][0-9][0-9][0-9]_*.sql'))
    if not existing:
        return 1
    return int(existing[-1].name[:5]) + 1


def _short_hash(content: bytes) -> str:
    return 'm1' + hashlib.sha256(content).hexdigest()[:6]


def _pg_url(config) -> str:
    db = config.database
    if db.dsn is not None:
        return db.dsn.replace('pylon://', 'postgres://', 1)
    pw_part = f':{db.password}@' if db.password else '@'
    return f'postgres://{db.user}{pw_part}{db.host}:{db.port}/{db.name}'


@click.group()
def migration() -> None:
    """Manage Pylon schema migrations."""


@migration.command()
@requires_config
@click.pass_context
def create(ctx: click.Context) -> None:
    """Diff the current schema against the database and create a new migration file."""
    import pylon
    from pylon.schema._export import export

    config = ctx.obj['config']
    migrations_dir = config.project.schema_dir / 'migrations'
    _require_migrations_dir(ctx, migrations_dir)

    pylon.finalize()
    desired_sql = export()

    with tempfile.TemporaryDirectory() as tmp:
        schema_file = Path(tmp) / 'desired.sql'
        schema_file.write_text(desired_sql)

        result = subprocess.run(
            [
                'atlas', 'schema', 'diff',
                '--from', _pg_url(config),
                '--to', f'file://{schema_file}',
                '--format', '{{ sql . }}',
            ],
            capture_output=True,
            text=True,
            check=True,
        )

    diff_sql = result.stdout.strip()
    if not diff_sql:
        click.echo('No schema changes detected.')
        return

    content = diff_sql.encode()
    seq = _next_seq(migrations_dir)
    dest = migrations_dir / f'{seq:05d}_{_short_hash(content)}.sql'
    dest.write_text(diff_sql)
    click.echo(f'Created {dest}')

    subprocess.run(
        ['atlas', 'migrate', 'hash', '--dir', f'file://{migrations_dir}'],
        check=True,
    )


@migration.command('list')
@requires_config
@click.pass_context
def list_migrations(ctx: click.Context) -> None:
    """List all migration files."""
    config = ctx.obj['config']
    migrations_dir = config.project.schema_dir / 'migrations'
    _require_migrations_dir(ctx, migrations_dir)
    files = sorted(migrations_dir.glob('[0-9][0-9][0-9][0-9][0-9]_*.sql'))
    if not files:
        click.echo('No migrations found.')
        return
    for f in files:
        click.echo(f.name)


@migration.command()
@click.argument('migration_name', required=False)
@click.option('--dry-run', is_flag=True, default=False, help='Preview SQL without applying.')
@requires_config
@click.pass_context
def migrate(ctx: click.Context, migration_name: str | None, dry_run: bool) -> None:
    """Apply pending migrations to the database.

    When MIGRATION_NAME is given, that single file is executed directly.
    Otherwise all pending migrations are applied via Atlas.
    """
    import asyncio
    import asyncpg

    config = ctx.obj['config']
    migrations_dir = config.project.schema_dir / 'migrations'
    _require_migrations_dir(ctx, migrations_dir)

    if migration_name:
        target = migrations_dir / migration_name
        if not target.is_file():
            _print_error(
                f'migration file not found: {migration_name}',
                f'File must exist in {migrations_dir}',
            )
            ctx.exit(1)

        sql = target.read_text()

        if dry_run:
            click.echo(sql)
            return

        async def _apply() -> None:
            conn = await asyncpg.connect(_pg_url(config))
            try:
                async with conn.transaction():
                    await conn.execute(sql)
            finally:
                await conn.close()

        asyncio.run(_apply())
        click.echo(f'Applied {migration_name}.')
        return

    cmd = [
        'atlas', 'migrate', 'apply',
        '--url', _pg_url(config),
        '--dir', f'file://{migrations_dir}',
    ]

    if dry_run:
        cmd += ['--dry-run']

    subprocess.run(cmd, check=True)
