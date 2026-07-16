from __future__ import annotations

import click

from ..config import requires_config


@click.group()
def cache() -> None:
    """Inspect and manage the Pylon query-result cache."""


def _open_cache(config) -> None:
    """Open this process's own LMDB handle at `[cache].path`, regardless of
    `[cache].enabled` — CLI inspection/maintenance should work on whatever
    is actually on disk even while caching is toggled off."""
    from pylon._core import cache_init

    config.cache.path.mkdir(parents=True, exist_ok=True)
    cache_init(str(config.cache.path), config.cache.max_size_mb)


def _format_bytes(n: int) -> str:
    size = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if unit == "B":
            if size < 1024:
                return f"{int(size)} {unit}"
        elif size < 1024 or unit == "GB":
            return f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} GB"


@cache.command()
@requires_config
@click.pass_context
def status(ctx: click.Context) -> None:
    """Show the current cache size (entry count and bytes used)."""
    config = ctx.obj["config"]
    _open_cache(config)
    from pylon._core import cache_stat

    stats = cache_stat()
    click.echo(f"path:    {config.cache.path}")
    click.echo(f"enabled: {config.cache.enabled}")
    click.echo(f"entries: {stats['entry_count']}")
    click.echo(f"used:    {_format_bytes(stats['used_bytes'])}")


@cache.command()
@click.option("--yes", is_flag=True, default=False, help="Skip the confirmation prompt.")
@requires_config
@click.pass_context
def purge(ctx: click.Context, yes: bool) -> None:
    """Evict every entry from the cache."""
    config = ctx.obj["config"]
    if not yes:
        click.confirm(f"Purge all entries from the cache at {config.cache.path}?", abort=True)

    _open_cache(config)
    from pylon._core import cache_clear

    cache_clear()
    click.echo("Cache purged.")
