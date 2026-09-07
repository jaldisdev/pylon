# Python client

`pylon.Client` (and its module-level constructor `pylon.create_async_client`) is the Python API for running PyQL against a database — a connection-pooled wrapper around Pylon's native Rust driver (`pgcon`) that compiles PyQL to SQL and hydrates results back into real instances of your `@pylon.type` classes.

Every query method takes a PyQL string, positional args (bound to `$1`, `$2`, ... — see [Parameters](../pyql/parameters.md)) or keyword args (bound to `$name`), and is `async`.

## Constructing a client

```python
import pylon

client = pylon.Client(config=my_config)          # explicit Config
client = pylon.create_async_client()              # loads pylon.toml from the working tree
```

`Client(config=None)` also auto-loads `pylon.toml` if no `Config` is given. Pass `warnings=False` to suppress compile-time warnings the transpiler attaches to a query (`Config` object emitted via Python's `warnings` module by default).

### Lifecycle

```python
await client.ensure_connected()   # opens the pool; safe to call more than once
await client.aclose()             # closes it
```

or use it as an async context manager, which does both automatically:

```python
async with pylon.create_async_client() as client:
    await client.query("select Person { name }")
```

`repr(client)` shows connection state and target (`<Client [connected] localhost:5432/mydb>`).

## Running workers in-process

Two of Pylon's background workers have to run in a Python process — cache invalidation and the [signal dispatcher](../schema/signals.md) — and normally do so via [`pylon worker start`](../cli.md#pylon-worker-start). `pylon.workers` runs the same pair inside an application that already has an event loop, for a container whose entry point is a web framework and has no room for a second command.

Which entry point you want depends on how your framework hands you the process lifetime.

**One bracketing hook** — an ASGI lifespan, and the common case. `run_workers` is an async context manager, so the workers start before the first request and are cancelled when the block exits:

```python
from contextlib import asynccontextmanager

import pylon
from pylon.workers import run_workers

client = pylon.create_async_client()

@asynccontextmanager
async def lifespan(app):
    await client.ensure_connected()
    async with run_workers():
        yield

app = FastAPI(lifespan=lifespan)   # Starlette, Litestar, Quart: same shape
```

**Two separate callbacks** — a startup hook and a shutdown hook with no shared scope, which a context manager has nowhere to suspend inside. Hold a `BackgroundWorkers` across the pair instead:

```python
import pylon
from pylon.workers import BackgroundWorkers

client = pylon.create_async_client()
workers = BackgroundWorkers()

@app.on_startup
async def start_workers():
    await client.ensure_connected()
    await workers.start()

@app.on_shutdown
async def stop_workers():
    await workers.stop()
```

`stop()` is a no-op if `start()` never ran, so a shutdown hook doesn't have to guard against a startup that failed before reaching it. `run_workers` yields the started tasks if you want to inspect them, and `BackgroundWorkers.tasks` exposes the same. Both take the same arguments, mirroring `pylon worker start`'s flags:

| Argument | Meaning |
| --- | --- |
| `batch_size` | SignalOutbox rows claimed per polling cycle (default 50). |
| `poll_interval` | Seconds between polls when the outbox is empty (default 30). |
| `disabled` | Worker kinds to skip — either of `'cache'`, `'signals'` (`pylon.workers.WORKER_KINDS`). |
| `shared_cache` | Attach the cache-invalidation worker to this process's own cache handle. Defaults to `True`, which is what you want here. |
| `schema`, `config` | Default to the process schema singleton and the `pylon.toml` found from the working tree. |

**Connect before you start them.** Two things depend on that order. The cache handle this process evicts through is opened by `ensure_connected()`, so starting workers first raises `InterfaceError` rather than silently invalidating nothing. And connecting replaces the process schema with the one the database was last migrated to, so resolving it afterwards hands the workers the same schema your queries compile against, instead of whatever your local `.py` files currently declare.

**Count your processes first.** A server running N worker processes runs its startup hook N times, so this gives you N cache invalidators, N `LISTEN` connections, and N signal dispatchers per container — all doing the same work against the same database. They are all correct (the dispatcher claims with `FOR UPDATE SKIP LOCKED`, and the invalidators evict from copies of one file), just redundant. Above one process per container, a single [`pylon worker start`](../cli.md#pylon-worker-start) sharing the cache directory does that job once and every worker process sees it. Running them in-process is the simpler choice when the container holds one process and you scale by adding containers.

If you run them here anyway, `disabled=['signals']` is usually right: the dispatcher has no locality constraint, so one somewhere in the deployment is enough rather than one per process.

### Why the cache worker is the one that forces the decision

The signal dispatcher drains an outbox table with `FOR UPDATE SKIP LOCKED`. It can run anywhere, in any number, and the database sorts out who gets which row — running it here is a convenience.

Cache invalidation is not like that. `[cache]` is an LMDB file on local disk with no expiry, and the worker evicts from the environment it holds open. It therefore has to reach *your* cache: same process (this API), or same path on the same filesystem (a second process or container sharing the volume). A cache-invalidation worker deployed anywhere else evicts entries nobody reads, and every replica holding the real cache goes on serving the write it never saw for as long as it lives. There is no partial version of this to fall back on — a cache with no invalidator reaching it is wrong, not merely stale.

What this does *not* replace is `pylon-server`. Vector indexing, search indexing, and partition maintenance all run only there. A schema declaring a [`VectorIndex` or `SearchIndex`](../schema/indexes.md) or a [`Partition`](../schema/partitioning.md) needs a `pylon-server --no-http` container somewhere no matter what this process runs — and `run_workers` logs a line at startup when it sees one, since the alternative is a search result that silently never appears.

## Query methods

| Method | Returns | Raises on wrong cardinality |
|---|---|---|
| `query(pyql, *args, **kwargs)` | `list[Any]` — every matching object | — |
| `query_single(pyql, *args, **kwargs)` | one object or `None` | `ResultCardinalityError` if >1 |
| `query_required_single(pyql, *args, **kwargs)` | exactly one object | `NoDataError` if 0, `ResultCardinalityError` if >1 |
| `execute(pyql, *args, **kwargs)` | `None` — for INSERT/UPDATE/DELETE where you don't need the result | — |
| `query_json(pyql, *args, **kwargs)` | `str` — a JSON array (`"[]"` if empty) | — |
| `query_single_json(pyql, *args, **kwargs)` | `str \| None` — a single JSON object | `ResultCardinalityError` if >1 |
| `query_required_single_json(pyql, *args, **kwargs)` | `str` — a single JSON object | `NoDataError` if 0, `ResultCardinalityError` if >1 |

```python
people = await client.query("select Person { name, age }")
person = await client.query_single("select Person filter .id = <uuid>$id", id=some_id)
await client.execute("update Person filter .id = <uuid>$id set { age := .age + 1 }", id=some_id)
```

A result row for a shaped object comes back as a real instance of the corresponding `@pylon.type` Python class, with nested shapes hydrated recursively — not a plain dict.

`query()` and `execute()` also accept a `@pylon.type` class or a `Model.filter(...)` set in place of the PyQL string — see the [Model API](model-api.md):

```python
people = await client.query(Person)
bobs = await client.query(Person.filter(name='Bob'))
await client.execute(Person.filter(id=some_id).delete())
```

### `analyze`

```python
plan = await client.analyze("select Post { title, author: { name } }")
```

Runs the query through Postgres's `EXPLAIN (ANALYZE, FORMAT JSON)` and returns the plan re-grouped by the query's own shape (root select, each nested link) instead of raw SQL relation names — `{"path": ..., "relations": [...], "cost": ..., "children": [{"name": ..., "node": {...}}, ...]}`. You don't need to write the leading `analyze` keyword yourself; it's added if missing.

### `save`

```python
post = Post(title="Hello", body="...")
await client.save(post)          # INSERT — post.id is populated afterward
post.title = "Hello, edited"
await client.save(post)          # UPDATE — only if something actually changed
```

Accepts any number of `@pylon.type` instances and saves them all in one transaction. An instance that was never hydrated from a query result is `INSERT`ed (its generated `id` is written back onto the object); one that *was* hydrated is diffed against the values it was loaded with and only `UPDATE`d if something changed — an unmodified object is skipped entirely, not re-written as a no-op update.

Links are saved too: assign an instance to a single link, and use `+=` / `-=` on a multi-link. Unsaved link targets are written first, so saving the root of an object graph writes the whole graph in the one transaction. Multi-links are replayed from a recorded operation log rather than diffed, since `+=` on an already-linked member is a server-side no-op and so leaves no state change to diff. See [Model API § Links](model-api.md#links).

## `listen`

```python
async for payload in client.listen("UserUpdates"):
    print(payload)
```

Subscribes to a schema-declared [`Channel`](../schema/channels.md) and yields decoded `NOTIFY` payloads as an async generator, for as long as you keep iterating. *channel* is a bare or `module::name` reference — the same string you'd pass to [`notify(...)`](../pyql/globals-and-functions.md#notify--notify_raw) from PyQL.

Each yielded value matches the Channel's own declared shape: a bare `uuid.UUID` (the changed row's `id`, not a fetched object) for a Type-shaped channel, the declared scalar's native Python value for a Scalar-shaped channel, or a `pylon.Object` for an Object-shaped channel. A payload that doesn't actually match what was declared raises `QueryError` and ends the loop there, rather than being silently dropped.

Unlike every other client method, `listen()` doesn't use the connection pool — `LISTEN` is a per-session subscription, so reusing a pooled connection would leak it onto whatever unrelated query later borrows that connection back out of the pool. Each call opens its own dedicated connection, held for as long as you keep iterating; it closes automatically once you stop (`break`, an exception, or letting the generator get garbage-collected).

See the [Rust client's own `listen()`](rust.md#listen) for the equivalent from a Rust process.

## Transactions

```python
async for tx in client.transaction():
    async with tx:
        person = await tx.query_single("select Person filter .id = <uuid>$id", id=some_id)
        await tx.execute("update Person filter .id = <uuid>$id set { age := .age + 1 }", id=some_id)
```

`client.transaction(*, attempts=3, isolation="serializable")` returns an async iterator. Each iteration acquires a fresh connection and yields an `AsyncTransaction`, which exposes the exact same query methods as `Client` itself (`query`, `query_single`, `query_required_single`, `execute`, `query_json`, `query_single_json`, `query_required_single_json`) scoped to that transaction. Exiting the `async with tx:` block commits (or rolls back, on an exception).

On a serialization failure or deadlock, the loop **retries automatically** — up to `attempts` times, with exponential back-off (0ms, 100ms, 200ms, ...) — by re-running the loop body with a brand-new transaction. Write loop bodies accordingly: idempotent, no side effects outside the transaction itself, since a retried attempt reruns the whole body.

`isolation` accepts `"serializable"` (default), `"repeatable_read"`, or `"read_committed"`.

### Deliberate rollback

```python
from pylon import Rollback

async for tx in client.transaction():
    async with tx:
        await tx.execute('insert Person { name := "Ada" }')
        assert await tx.query('select Person filter .name = "Ada"')
        raise Rollback
```

Raising `Rollback` inside the block rolls the transaction back and exits quietly: the exception is suppressed at the end of the `async with`, so execution continues after the loop, and the retry loop treats the attempt as finished rather than re-running the body. Everything written inside the block is visible to the block's own queries and to nothing else — which is what makes it useful for tests and dry runs that need real writes without leaving rows behind.

`Rollback` deliberately sits outside the `PylonError` hierarchy (it subclasses `Exception` directly), so an `except PylonError` inside the block won't swallow it.

## Per-call customization

Both return a new `Client` sharing the same underlying connection pool — cheap, and safe to build per-request.

### `with_globals`

```python
authed = client.with_globals({"default::current_user_id": user_id})
posts = await authed.query("select Post { title }")   # `global current_user_id` now resolves
```

Injects values for [`Global`](../schema/globals-and-aliases.md)s referenced via `global name` in a query, keyed by qualified name (`"module::name"`). Repeated calls merge into the previous set of globals rather than replacing it.

### `with_config`

```python
unsafe = client.with_config({"allow_user_specified_id": True})
await unsafe.query("insert Person { id := <uuid>$id, name := $name }", id=some_uuid, name="Ada")
```

Applies session-level compile options. `allow_user_specified_id` is the one Pylon defines today — normally an `INSERT` that explicitly assigns `id` is a compile error (Pylon always generates it); this opts a specific client view back into allowing it, e.g. for a data-migration script seeding fixed UUIDs. An unrecognized option name is stored but has no effect.

## Escape hatch: raw SQL

```python
async with client.raw_connection() as pool:
    rows = await pool.query("SELECT count(*) AS n FROM some_table", [])
```

`client.raw_connection()` yields the underlying connection pool directly — bypasses PyQL entirely. `pool.query(sql, params)`/`pool.execute(sql, params)` take positional `$1, $2, ...` SQL and params; `query` decodes column 0 of each row regardless of its name (so a bare `SELECT count(*) AS n` works the same as PyQL's own single-`result`-column convention). Use sparingly — nothing here is validated or type-checked the way a compiled PyQL query is.

## Exceptions

Every method raises a `pylon.exceptions.PylonError` subclass — never a raw driver/network exception. The hierarchy that matters for a `Client` caller:

| Class | Raised when |
|---|---|
| `ClientConnectionClosedError` | A query method is called before `ensure_connected()` (or after `aclose()`). |
| `ConnectionFailedError` / `ConnectionTimeoutError` | The pool couldn't establish a connection. |
| `TransactionSerializationError` / `TransactionDeadlockError` | A transaction attempt failed for a retriable reason — `client.transaction()`'s loop handles these itself; they only escape once the retry budget is exhausted. |
| `InvalidQueryError` | The PyQL string failed to compile (syntax or type error) — carries the offending query text and source position for a rendered caret-style message. |
| `UnknownTypeError` / `UnknownLinkError` | The query references a type/property/link that doesn't exist in the schema. |
| `MissingParameterError` / `UnknownParameterError` / `InvalidParameterTypeError` | A `$name` param wasn't supplied, was supplied but unused, or had the wrong Python type for its declared PyQL type. |
| `ResultCardinalityError` (and its subclass `NoDataError`) | `query_single*`/`query_required_single*` got more (or, for the `required` variants, fewer) rows than expected. |
| `ConstraintViolationError` | A database constraint (exclusivity, check, etc.) was violated. |
| `InternalServerError` | Anything else — a genuine bug, not a caller mistake. |

See `pylon/exceptions.py` for the complete class hierarchy (it also includes `MigrationError`/`MigrationConflictError`, raised by the migration tooling rather than `Client`). The one exception in that module that is *not* a `PylonError` is [`Rollback`](#deliberate-rollback) — it signals a decision, not a failure.
