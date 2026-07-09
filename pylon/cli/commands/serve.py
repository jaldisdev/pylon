from __future__ import annotations

import dataclasses

import click
import uvicorn

from pylon.server import create_app

from ..config import requires_config


@click.command()
@click.option("--host", default=None, help="Override [webserver].host.")
@click.option("--port", type=int, default=None, help="Override [webserver].port.")
@click.option("--ui/--no-ui", "ui_enabled", default=None, help="Override [ui].enabled.")
@requires_config
@click.pass_context
def serve(ctx: click.Context, host: str | None, port: int | None, ui_enabled: bool | None) -> None:
    """Run the Pylon web server (PyQL/schema API, and the GUI when enabled).

    Foreground only for now, logs to stdout, Ctrl-C to stop — the
    `--detach`/`stop`/`status` pidfile workflow from the observability spec
    lands in a later pass once there's a build of the SPA to actually serve.
    """
    config = ctx.obj["config"]

    webserver = config.webserver
    if host is not None:
        webserver = dataclasses.replace(webserver, host=host)
    if port is not None:
        webserver = dataclasses.replace(webserver, port=port)

    ui = config.ui
    if ui_enabled is not None:
        ui = dataclasses.replace(ui, enabled=ui_enabled)

    config = dataclasses.replace(config, webserver=webserver, ui=ui)

    uvicorn.run(create_app(config), host=webserver.host, port=webserver.port)
