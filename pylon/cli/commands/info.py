from __future__ import annotations

import sys
from pathlib import Path

import click

from ..banner import _INFO_COLOR, _RESET

_BOLD_WHITE = "\x1b[1;37m"


def _label(s: str) -> str:
    return f"{_INFO_COLOR}{s:<12}{_RESET}"


@click.command("info")
@click.pass_context
def info_cmd(ctx: click.Context) -> None:
    """Show paths and connection info for the current Pylon project."""
    try:
        from importlib.metadata import version as _version
        pylon_version = _version("pylon")
    except Exception:
        pylon_version = "(development)"

    try:
        import pylon._core as _core
        core_version = getattr(_core, "__version__", "(unknown)")
    except Exception:
        core_version = "(unknown)"

    click.echo(f"\n{_BOLD_WHITE}Pylon {pylon_version}{_RESET}  (core {core_version})\n")

    # Python
    py = sys.version.split()[0]
    click.echo(f"  {_label('Python')} {sys.executable}  ({py})")

    # Config & schema
    config = ctx.obj.get("config") if ctx.obj else None
    if config is None:
        click.echo(f"  {_label('Config')} (no pylon.toml found in current directory tree)")
    else:
        from pylon.config import _find_toml
        try:
            toml_path = _find_toml(Path.cwd())
            click.echo(f"  {_label('Config')} {toml_path}")
        except FileNotFoundError:
            click.echo(f"  {_label('Config')} (not found)")

        if config.project:
            click.echo(f"  {_label('Schema')} {config.project.schema_dir}")
            if config.project.pyql:
                click.echo(f"  {_label('PyQL')} {config.project.pyql}")

        # Database
        db = config.database
        if db.dsn:
            display_dsn = db.dsn.replace("pylon://", "postgresql://", 1)
            # Mask password in display
            from urllib.parse import urlparse, urlunparse
            parsed = urlparse(display_dsn)
            if parsed.password:
                masked = parsed._replace(netloc=parsed.netloc.replace(
                    f":{parsed.password}@", ":***@"
                ))
                display_dsn = urlunparse(masked)
            click.echo(f"  {_label('Database')} {display_dsn}")
        else:
            click.echo(
                f"  {_label('Database')} {db.host}:{db.port}/{db.name}"
                f"  (user: {db.user or '(default)'})"
            )

    click.echo()
