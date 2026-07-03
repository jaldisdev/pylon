from __future__ import annotations

import asyncio
import logging
import sys

import asyncpg
import click

from ..config import _print_error, requires_config


def _build_provider(api_style: str, api_url: str, model: str, secret: str | None):
    if api_style == "openai":
        from pylon.vector.models.openai import OpenAIEmbeddingProvider
        return OpenAIEmbeddingProvider(api_url=api_url, model=model, api_key=secret)
    if api_style == "anthropic":
        from pylon.vector.models.anthropic import AnthropicEmbeddingProvider
        return AnthropicEmbeddingProvider(api_url=api_url, model=model, api_key=secret)
    raise click.ClickException(
        f"Unknown api_style {api_style!r} in [models] — expected 'openai' or 'anthropic'."
    )


def _build_providers(schema, config) -> dict:
    log = logging.getLogger(__name__)
    models = config.models_registry
    providers: dict = {}
    for td in schema.types:
        for vi in td.vector_indexes:
            type_name = f"{td.module}::{td.name}"
            index_name = vi.index_name
            model_cfg = models.get(vi.model) or models.get("default")
            if model_cfg is None:
                log.warning(
                    "No [models.%s] entry in pylon.toml for %s (index=%s); skipping",
                    vi.model, type_name, index_name or "<default>",
                )
                continue
            providers[(type_name, index_name)] = _build_provider(
                model_cfg.api_style, model_cfg.api_url, model_cfg.model, model_cfg.secret,
            )
            log.info(
                "Provider registered: %s  index=%s  →  %s  (%s)",
                type_name, index_name or "<default>", vi.model, model_cfg.api_style,
            )
    return providers


@click.group()
def worker() -> None:
    """Manage Pylon background index workers."""


@worker.command()
@click.option("--batch-size", default=50, show_default=True,
              help="Number of IndexOutbox rows to claim per cycle.")
@click.option("--poll-interval", default=30.0, show_default=True,
              help="Seconds between polling cycles when idle.")
@click.option("--log-level", default="INFO", show_default=True,
              type=click.Choice(["DEBUG", "INFO", "WARNING", "ERROR"], case_sensitive=False),
              help="Logging level.")
@requires_config
@click.pass_context
def start(ctx: click.Context, batch_size: int, poll_interval: float, log_level: str) -> None:
    """Start the vector index worker.

    Reads [models.*] from pylon.toml, auto-discovers all types that declare a
    VectorIndex, and processes IndexOutbox rows until interrupted.
    """
    logging.basicConfig(
        level=getattr(logging, log_level.upper()),
        format="%(asctime)s  %(levelname)-8s  %(name)s  %(message)s",
    )
    log = logging.getLogger(__name__)

    import pylon
    from pylon.vector.sync import VectorIndexWorker
    import pylon.query as _q

    pylon.finalize()
    config = ctx.obj["config"]
    schema = _q._singleton

    providers = _build_providers(schema, config)
    if not providers:
        _print_error(
            "no vector index providers configured",
            "Add [models.<model-name>] entries to pylon.toml matching the "
            "model= declared on each VectorIndex in your schema.",
        )
        ctx.exit(1)
        return

    db = config.database
    dsn = db.dsn or f"postgresql://{db.user}:{db.password}@{db.host}:{db.port}/{db.name}"

    async def run() -> None:
        conn = await asyncpg.connect(dsn)
        try:
            worker_instance = VectorIndexWorker(conn, schema=schema, providers=providers)
            worker_instance.batch_size = batch_size
            worker_instance.poll_interval = poll_interval
            log.info(
                "VectorIndexWorker started  batch_size=%d  poll_interval=%.0fs",
                batch_size, poll_interval,
            )
            await worker_instance.run()
        finally:
            await conn.close()

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        click.echo("\nWorker stopped.")
    except Exception as exc:
        _print_error("worker crashed", str(exc))
        sys.exit(1)
