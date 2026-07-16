from __future__ import annotations

import asyncio
import logging
import sys

import asyncpg
import click

from ..config import _print_error, requires_config


def _build_provider(api_style: str, api_url: str, model: str, secret: str | None):
    if api_style == "openai":
        from pylon.vector.models.openai import OpenAIProvider
        return OpenAIProvider(api_url=api_url, model=model, api_key=secret)
    if api_style == "anthropic":
        from pylon.vector.models.anthropic import AnthropicProvider
        return AnthropicProvider(api_url=api_url, model=model, api_key=secret)
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
    """Start index workers for all configured indexes.

    Auto-discovers VectorIndex and SearchIndex declarations from the schema
    and processes IndexOutbox rows until interrupted.
    """
    logging.basicConfig(
        level=getattr(logging, log_level.upper()),
        format="%(asctime)s  %(levelname)-8s  %(name)s  %(message)s",
    )
    log = logging.getLogger(__name__)

    import pylon
    from pylon.vector.sync import VectorIndexWorker
    from pylon.search import OpenSearchClient, OpenSearchWorker, MeilisearchClient, MeilisearchWorker
    import pylon.query as _q

    pylon.finalize()
    config = ctx.obj["config"]
    schema = _q._singleton

    providers = _build_providers(schema, config)
    want_opensearch = any(
        si.backend == "OpenSearch"
        for td in schema.types
        for si in td.search_indexes
    )
    want_meilisearch = any(
        si.backend == "Meilisearch"
        for td in schema.types
        for si in td.search_indexes
    )

    if not providers and not want_opensearch and not want_meilisearch and not config.cache.enabled:
        _print_error(
            "no index workers to start",
            "Add VectorIndex or SearchIndex(backend=...) to your schema, "
            "and configure [models.*] / [search] in pylon.toml — or set "
            "[cache].enabled = true to start the cache-invalidation worker.",
        )
        ctx.exit(1)
        return

    if (want_opensearch or want_meilisearch) and not config.search_registry:
        _print_error(
            "search indexes defined but no [search] config found",
            "Add [search] host/port/backend to pylon.toml.",
        )
        ctx.exit(1)
        return

    db = config.database
    dsn = db.dsn or f"postgresql://{db.user}:{db.password}@{db.host}:{db.port}/{db.name}"

    async def run() -> None:
        tasks = []
        conns = []
        clients = []

        if providers:
            conn = await asyncpg.connect(dsn)
            conns.append(conn)
            w = VectorIndexWorker(conn, schema=schema, providers=providers)
            w.batch_size = batch_size
            w.poll_interval = poll_interval
            log.info(
                "VectorIndexWorker started  batch_size=%d  poll_interval=%.0fs",
                batch_size, poll_interval,
            )
            tasks.append(w.run())

        if want_opensearch:
            search_cfg = config.search_registry["default"]
            base_url = f"http://{search_cfg.host}:{search_cfg.port}"
            auth = (search_cfg.user, search_cfg.password) if search_cfg.user else None
            client = OpenSearchClient(base_url, auth=auth)
            await client.__aenter__()
            clients.append(client)
            conn = await asyncpg.connect(dsn)
            conns.append(conn)
            w = OpenSearchWorker(conn, schema=schema, client=client)
            w.batch_size = batch_size
            w.poll_interval = poll_interval
            log.info(
                "OpenSearchWorker started  base_url=%s  batch_size=%d  poll_interval=%.0fs",
                base_url, batch_size, poll_interval,
            )
            tasks.append(w.run())

        if want_meilisearch:
            search_cfg = config.search_registry["default"]
            base_url = f"http://{search_cfg.host}:{search_cfg.port}"
            client = MeilisearchClient(base_url, api_key=search_cfg.api_key)
            await client.__aenter__()
            clients.append(client)
            conn = await asyncpg.connect(dsn)
            conns.append(conn)
            w = MeilisearchWorker(conn, schema=schema, client=client)
            w.batch_size = batch_size
            w.poll_interval = poll_interval
            log.info(
                "MeilisearchWorker started  base_url=%s  batch_size=%d  poll_interval=%.0fs",
                base_url, batch_size, poll_interval,
            )
            tasks.append(w.run())

        if config.cache.enabled:
            from pylon import cache as pylon_cache
            from pylon.cache import CacheInvalidationWorker, NOTIFY_CHANNEL
            # This process's own LMDB handle onto the shared, file-backed
            # cache at config.cache.path — LMDB supports safe concurrent
            # multi-process access to one file, so this worker process
            # evicting entries is immediately visible to every serving
            # process (e.g. `pylon serve`) mapping the same path.
            pylon_cache.init(config.cache)
            conn = await asyncpg.connect(dsn)
            conns.append(conn)
            w = CacheInvalidationWorker(conn)
            log.info("CacheInvalidationWorker started  channel=%s", NOTIFY_CHANNEL)
            tasks.append(w.run())

        try:
            await asyncio.gather(*tasks)
        finally:
            for conn in conns:
                await conn.close()
            for client in clients:
                await client.__aexit__(None, None, None)

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        click.echo("\nWorker stopped.")
    except Exception as exc:
        _print_error("worker crashed", str(exc))
        sys.exit(1)
