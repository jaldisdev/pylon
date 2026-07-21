from __future__ import annotations

import asyncio
import logging
import sys

import click

from ..config import _print_error, requires_config


def _build_providers(schema, config) -> dict:
    """Resolve the `[models.<name>]` config each schema `VectorIndex` declares.

    Returns ``{(type_name, index_name): ModelConfig}`` — the embedding HTTP
    call itself happens in Rust now (`run_vector_worker`), so this only does
    the schema × `pylon.toml` lookup, not provider construction.
    """
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
            providers[(type_name, index_name)] = model_cfg
            log.info(
                "Provider registered: %s  index=%s  →  %s  (%s)",
                type_name, index_name or "<default>", vi.model, model_cfg.api_style,
            )
    return providers


def build_worker_tasks(
    schema, config, *, batch_size: int = 50, poll_interval: float = 30.0,
    log: logging.Logger | None = None, shared_cache: bool = False,
) -> list:
    """Build the list of background-worker coroutines implied by `schema`/`config`.

    Shared between `pylon worker start` (its own process) and `pylon serve`
    (launched as `asyncio.ensure_future` tasks alongside the ASGI app) — the
    claim queries every worker below runs (`FOR UPDATE SKIP LOCKED`) are
    already safe to run from more than one process/task concurrently, so
    there's no correctness difference between the two call sites, just
    where the coroutines get awaited from.

    `shared_cache` must be True when the caller's process already has
    `pylon.cache.init()`-opened LMDB handle (as `pylon serve`'s own
    read-through cache does) — the cache-invalidation worker then attaches
    to that same handle instead of opening a second one, which LMDB refuses
    within a single process.
    """
    log = log or logging.getLogger(__name__)
    db = config.database
    dsn = db.dsn or f"postgresql://{db.user}:{db.password}@{db.host}:{db.port}/{db.name}"

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
    from pylon.schema._registry import signals_snapshot
    want_signals = bool(signals_snapshot())

    tasks = []

    if providers:
        from pylon._core import run_vector_worker
        # Runs entirely in Rust now (`pylon_workers::VectorIndexWorker`)
        # — claim/embed/write all happen natively; only the resolved
        # `[models.*]` config crosses into Rust as plain data.
        provider_list = [
            (type_name, index_name, model_cfg.api_style, model_cfg.api_url, model_cfg.model, model_cfg.secret)
            for (type_name, index_name), model_cfg in providers.items()
        ]
        log.info(
            "VectorIndexWorker started  batch_size=%d  poll_interval=%.0fs",
            batch_size, poll_interval,
        )
        tasks.append(run_vector_worker(dsn, schema, provider_list, batch_size, poll_interval))

    if want_opensearch and not config.search_registry:
        log.warning("SearchIndex(backend=OpenSearch) declared but no [search] config found; skipping")
    if want_opensearch and config.search_registry:
        from pylon._core import run_opensearch_worker
        search_cfg = config.search_registry["default"]
        base_url = f"http://{search_cfg.host}:{search_cfg.port}"
        log.info(
            "OpenSearchWorker started  base_url=%s  batch_size=%d  poll_interval=%.0fs",
            base_url, batch_size, poll_interval,
        )
        tasks.append(run_opensearch_worker(
            dsn, schema, base_url, search_cfg.user, search_cfg.password, batch_size, poll_interval,
        ))

    if want_meilisearch and not config.search_registry:
        log.warning("SearchIndex(backend=Meilisearch) declared but no [search] config found; skipping")
    if want_meilisearch and config.search_registry:
        from pylon._core import run_meilisearch_worker
        search_cfg = config.search_registry["default"]
        base_url = f"http://{search_cfg.host}:{search_cfg.port}"
        log.info(
            "MeilisearchWorker started  base_url=%s  batch_size=%d  poll_interval=%.0fs",
            base_url, batch_size, poll_interval,
        )
        tasks.append(run_meilisearch_worker(
            dsn, schema, base_url, search_cfg.api_key, batch_size, poll_interval,
        ))

    if config.cache.enabled and shared_cache:
        from pylon._core import run_cache_invalidation_worker_shared
        from pylon.cache import NOTIFY_CHANNEL
        # This process (pylon serve) already opened the LMDB handle via
        # pylon.cache.init() for its own read-through cache — attach to
        # that same handle rather than opening a second one (LMDB refuses
        # a second Env::open on the same path within one process).
        log.info("CacheInvalidationWorker started (shared cache)  channel=%s", NOTIFY_CHANNEL)
        tasks.append(run_cache_invalidation_worker_shared(dsn))
    elif config.cache.enabled:
        from pylon._core import run_cache_invalidation_worker
        from pylon.cache import NOTIFY_CHANNEL
        # Runs entirely in Rust now (`pylon_workers::CacheInvalidationWorker`)
        # — it opens its own LMDB handle onto the shared, file-backed
        # cache at config.cache.path directly, no `pylon.cache.init()`
        # needed in this process. LMDB supports safe concurrent
        # multi-process access to one file, so this worker process
        # evicting entries is immediately visible to every serving
        # process (e.g. `pylon serve`) mapping the same path.
        log.info("CacheInvalidationWorker started  channel=%s", NOTIFY_CHANNEL)
        tasks.append(run_cache_invalidation_worker(dsn, str(config.cache.path), config.cache.max_size_mb))

    if want_signals:
        from pylon.signals import run_signal_dispatcher
        # The one worker that isn't Rust-native — it needs to hold a
        # live reference to each registered `@pylon.signal` handler,
        # which only exists in this Python process.
        log.info(
            "Signal dispatcher started  batch_size=%d  poll_interval=%.0fs",
            batch_size, poll_interval,
        )
        tasks.append(run_signal_dispatcher(dsn, batch_size=batch_size, poll_interval=poll_interval))

    return tasks


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
    and processes IndexOutbox rows until interrupted. `pylon serve` already
    launches these same workers in-process (see `build_worker_tasks`) — run
    this separately only if you want workers on their own process/machine.
    """
    logging.basicConfig(
        level=getattr(logging, log_level.upper()),
        format="%(asctime)s  %(levelname)-8s  %(name)s  %(message)s",
    )
    log = logging.getLogger(__name__)

    import pylon
    import pylon.query as _q

    pylon.finalize()
    config = ctx.obj["config"]
    schema = _q._singleton

    want_search = any(si.backend in ("OpenSearch", "Meilisearch") for td in schema.types for si in td.search_indexes)
    if want_search and not config.search_registry:
        _print_error(
            "search indexes defined but no [search] config found",
            "Add [search] host/port/backend to pylon.toml.",
        )
        ctx.exit(1)
        return

    tasks = build_worker_tasks(schema, config, batch_size=batch_size, poll_interval=poll_interval, log=log)
    if not tasks:
        _print_error(
            "no index workers to start",
            "Add VectorIndex or SearchIndex(backend=...) to your schema, "
            "and configure [models.*] / [search] in pylon.toml — or set "
            "[cache].enabled = true to start the cache-invalidation worker.",
        )
        ctx.exit(1)
        return

    async def run() -> None:
        # Every Rust-native worker's connection and any HTTP client it owns
        # are dropped along with the process, matching `worker start`'s own
        # lifecycle (runs until interrupted). No Python-side cleanup needed
        # there; the signal dispatcher's own connection is likewise dropped
        # when its task is cancelled.
        await asyncio.gather(*tasks)

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        click.echo("\nWorker stopped.")
    except Exception as exc:
        _print_error("worker crashed", str(exc))
        sys.exit(1)
