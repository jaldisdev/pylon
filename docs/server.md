# Server

`pylon-server` is a standalone Rust binary that serves PyQL queries over HTTP — a native alternative to embedding `Client` directly in a Python process. It has no Python dependency of its own: schema compilation, query transpilation, the connection pool, background workers, and the optional read-through cache are all native Rust (`pylon-core`, `pylon-client`, `pylon-workers`, `pylon-cache`).

## Running it

```bash
pylon-server
```

Discovers `pylon.toml` the same way the `pylon` CLI does — walking up from the current directory — unless `--config` points at one explicitly. Blocks until Ctrl-C.

| Flag | Description |
|---|---|
| `--config PATH` | Path to `pylon.toml` (skips the upward search). |
| `--host HOST` | Override `[webserver].host`. |
| `--port PORT` | Override `[webserver].port`. |
| `--ui` / `--no-ui` | Override `[ui].enabled`. |
| `--static-dir PATH` | Serve the frontend from this directory instead of the build embedded into the binary at compile time. |
| `--vector-worker` / `--disable-vector-worker` | Force the vector-index worker on/off, regardless of whether the schema declares `VectorIndex`es. |
| `--search-worker` / `--disable-search-worker` | Force the OpenSearch/Meilisearch index worker(s) on/off, regardless of `[search]` config. |
| `--cache-worker` / `--disable-cache-worker` | Force the cache-invalidation worker on/off, regardless of `[cache].enabled` (forcing it on has no effect if `[cache].enabled = false` — no cache handle was ever opened to attach it to). |
| `--http` / `--no-http` | Bind a port and serve at all, or don't. `--no-http` runs background workers only and blocks until Ctrl-C — a pure worker container with no API/UI. |
| `-h`, `--help` | Show usage and exit. |
| `-v`, `--version` | Show the `pylon-server` version and exit. |

Giving both forms of the same flag pair (e.g. `--ui` and `--no-ui` together) is an error. See [`[webserver]`/`[ui]`/`[metrics]`/`[cache]`](config.md#webserver) in the configuration reference for the `pylon.toml` keys these flags override.

The `--disable-*-worker` flags exist for deployments that run a given worker in its own process (`pylon worker start`) instead of in-process here, so it isn't double-spawned; the positive form (`--vector-worker`, etc.) opts a worker back in even if schema/config would otherwise cause it to be skipped for lack of a matching declaration.

## Background workers

On startup, `pylon-server` spawns whatever background workers your schema and `pylon.toml` imply as detached Tokio tasks — mirroring what `pylon worker start` runs as its own process (see [`cli.md`](cli.md#pylon-worker)):

- **Vector-index worker** — if the schema declares any `VectorIndex` with a matching `[models.*]` entry.
- **Search-index worker(s)** — OpenSearch and/or Meilisearch, if the schema declares a matching `SearchIndex(backend=...)` and `[search]` is configured.
- **Cache-invalidation worker** — if `[cache].enabled = true`. Shares the same LMDB handle `pylon-server`'s own read-through query cache already opened, rather than opening a second one (LMDB refuses a second `Env::open` on the same path within one process).

A worker that fails to start (missing config, bad client construction) logs a message and is skipped — not a startup-fatal error, unlike the database connection itself.

### Signals are the one worker never spawned here

**`@pylon.signal` handlers never run inside `pylon-server`, under any flag combination.** A registered signal handler is a live Python callable (see [Signals](schema/signals.md)) — `pylon-server` is pure Rust and has nothing to invoke it with. There's no `--signals-worker` flag because there's nothing here for it to toggle.

If your schema has any `@pylon.signal` registrations, you must also run `pylon worker start` (Python) as a separate process — it's the only thing that dispatches signals, in this deployment or any other. This is easy to miss: `pylon-server` doesn't warn you if signals are registered but nothing is consuming `_pylon."SignalOutbox"` — the rows will simply accumulate, unprocessed, until something drains them. (Verified end-to-end: a mutation through `pylon-server` still fires the database-level capture trigger and lands a row in the outbox correctly — the gap is purely "is anything polling it," not the capture mechanism itself.)

## HTTP API

| Method | Path | Description |
|---|---|---|
| `GET` | `/metrics` | Prometheus text exposition (only mounted if `[metrics].enabled = true`). Worker/backend counters only — no per-HTTP-request metrics. |
| `GET` | `/api/schema` | The compiled schema, as JSON. |
| `GET` | `/api/globals` | Declared `Global`s, as JSON. |
| `GET` | `/api/connections` | Named connections available (from `[database.<name>]` entries in `pylon.toml`). |
| `GET` | `/api/models` | Configured `[models.*]` entries. |
| `GET` | `/api/config-options` | Known `with_config()` session option names. |
| `GET` | `/api/<connection>/stats` | Pool stats for that connection. |
| `POST` | `/api/<connection>/query` | Compile and run a PyQL query. |
| `POST` | `/api/<connection>/analyze` | `EXPLAIN (ANALYZE)` a PyQL query — same shape as `Client.analyze()`. |
| `POST` | `/api/<connection>/ai/chat` | AI chat endpoint (see the AI/chat extension, if configured). |

`<connection>` is a named `[database.<name>]` entry — the same concept as the CLI's `-d/--database NAME` (a bare `default` connection always exists, from the base `[database]` block).

`POST /api/<connection>/query` body:

```json
{
  "pyql": "select Person filter .id = <uuid>$id",
  "params": {"id": "..."},
  "globals": {"default::current_user_id": "..."},
  "config": {"allow_user_specified_id": false}
}
```

`params`/`globals`/`config` mirror `Client.query()`'s positional/keyword args, `with_globals()`, and `with_config()` respectively (see [Client libraries](client/index.md)).

If `[ui].enabled = true` (the default), anything not matching an API route falls through to serving the frontend SPA — embedded into the binary at compile time via `include_dir!`, or from `--static-dir` if given.

## Deployment shape

A typical production deployment is two units:

1. `pylon-server` — HTTP API + UI + every worker except signals.
2. `pylon worker start` — only needed if the schema has `@pylon.signal` registrations; otherwise optional (`pylon-server` already covers vector/search/cache indexing on its own).

Both connect to the same PostgreSQL database and read the same `pylon.toml`. Neither needs the other running to function for its own responsibilities — a mutation through `pylon-server` still writes signal-outbox rows correctly even with no dispatcher running; they just won't be drained until one is.

Before either can serve anything, the database itself needs `pylon database initialize` (installs `_pylon` and the standard library) and at least one applied migration (`pylon migration apply`) — see [Migrations](migrations.md).
