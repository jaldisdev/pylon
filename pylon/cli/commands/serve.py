from __future__ import annotations

import signal
from pathlib import Path

import click

from ..config import requires_config


@click.command()
@click.option("--host", default=None, help="Override [webserver].host.")
@click.option("--port", type=int, default=None, help="Override [webserver].port.")
@click.option("--ui/--no-ui", "ui_enabled", default=None, help="Override [ui].enabled.")
@requires_config
def serve(host: str | None, port: int | None, ui_enabled: bool | None) -> None:
    """Run the Pylon web server (PyQL/schema API, and the GUI when enabled).

    Foreground only for now, logs to stdout, Ctrl-C to stop — the
    `--detach`/`stop`/`status` pidfile workflow from the observability spec
    lands in a later pass once there's a build of the SPA to actually serve.

    The server itself is native Rust now (`pylon_server::run`, via the
    `run_server` binding) — `pylon.toml` is parsed there too, so this
    command just forwards the CLI's own overrides straight through.
    """
    from pylon._core import run_server

    # The built frontend lives at `pylon/server/static/` (bundled into the
    # installed package) — `pylon-server` has no way to locate an installed
    # Python package on its own, so this is computed here, the same way
    # the old `asgi.py::STATIC_DIR` did (`Path(__file__).parent / "static"`).
    static_dir = Path(__file__).resolve().parent.parent.parent / "server" / "static"

    # `run_server` blocks inside a Tokio runtime that installs its own
    # `SIGINT` handler for graceful shutdown (`pylon_server::serve`) —
    # that alone is enough to stop cleanly on Ctrl-C. But CPython's own
    # default `SIGINT` handler is still live underneath it, and fires
    # independently the moment the GIL is reacquired once `run_server`
    # returns: Click's own context teardown then sees a `KeyboardInterrupt`
    # mid-`__exit__`, converts it to a bare, message-less `click.exceptions.
    # Abort`, and `pylon/cli/root.py::main`'s catch-all prints an empty
    # "error:" line and exits 1 — even though the server itself already
    # shut down cleanly. Suppressing Python's own handler for the duration
    # (Rust's is the one actually doing the work) avoids that race.
    previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        run_server(host=host, port=port, ui_enabled=ui_enabled, static_dir=str(static_dir))
    finally:
        signal.signal(signal.SIGINT, previous_handler)
