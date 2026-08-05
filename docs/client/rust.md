# Rust client

`pylon-client` is a native Rust crate for querying a Pylon-managed Postgres database directly — no Python interpreter, no pyo3 involved. It's a separate implementation from the Python client, not a wrapper around it: query compilation reuses `pylon-core` as-is, but results decode into a generic, dynamically-typed `Value`/`Object` rather than per-`@pylon.type` generated structs, since Rust has no runtime codegen to build those from a schema the way `pylon-py` does.

## Constructing a client

```rust
let client = pylon_client::Client::builder(dsn)
    .max_pool_size(10)
    .build()
    .await?;
```

`Builder::build()` connects eagerly (a bad DSN/host/credentials fails right here) and fetches the schema snapshot from `_pylon."Schema"` — fails with `Error::NoSchemaSnapshot` if neither `pylon migration apply` nor `pylon migration watch` has ever run against this database. A bare schema-file edit has no effect here until one of those does — same rule as the Python client's `ensure_connected()`. This crate never parses `pylon.toml` itself; the DSN is passed in directly.

Optional: `.cache(path, max_size_mb)` (or `.cache_handle(existing)` to attach to an already-open handle) opts into read-through LMDB caching — mirrors `pylon.toml`'s `[cache]` section (a single global on/off, no per-type overrides). Nothing evicts entries automatically; pair it with something that invalidates the same directory when data changes (e.g. `pylon worker start`).

## Query methods

| Method | Returns |
|---|---|
| `query(pyql, params)` | `Result<Vec<Value>>` — every matching object |
| `query_single(pyql, params)` | `Result<Option<Value>>` |
| `query_required_single(pyql, params)` | `Result<Value>` |
| `execute(pyql, params)` | `Result<()>` |
| `query_json(pyql, params)` | `Result<String>` — a JSON array |
| `query_single_json(pyql, params)` | `Result<Option<String>>` |
| `query_required_single_json(pyql, params)` | `Result<String>` |
| `analyze(pyql, params)` | `Result<String>` — same `EXPLAIN`-based plan the Python client's `analyze()` returns |

```rust
let some_id: uuid::Uuid = /* ... */;

let people = client.query("select Person { name, age }", &[]).await?;
let person = client.query_single(
    "select Person filter .id = <uuid>$id",
    &[("id", some_id.into())],
).await?;
client.execute(
    "update Person filter .id = <uuid>$id set { age := .age + 1, name := $name }",
    &[("id", some_id.into()), ("name", "Ada".into())],
).await?;
```

`params` is `&[(&str, DecodedValue)]` — named, bound the same way `$name` args work on the Python side (see [Parameters](../pyql/parameters.md)). The second element of each tuple accepts `.into()` for every native type `DecodedValue` has a `From` impl for — `String`/`&str`, `bool`, `i16`/`i32`/`i64`, `f32`/`f64`, `Vec<u8>`, `uuid::Uuid` — so a call site rarely has to spell out the variant name explicitly. Fall back to the explicit `DecodedValue::Variant(...)` form for anything without one (`Decimal`, `Date`/`Time`/`Timestamp`, `Range`, ...).

### `Value`/`Object` — the generic result type

Every result decodes into `pylon_client::Value`, a single enum with one variant per shape: `Null`, `Bool`, `Int64`, `Float64`, `Str`, `Bytes`, `Uuid`, `Decimal` (kept as its canonical string form — no single obviously-correct native Rust decimal type to commit this client to), `Duration`/`Date`/`Time`/`Timestamp`/`Timestamptz` (PG-epoch-relative wire representations, matching `pylon_value::DecodedValue`'s own), `Range`, `Array`, `Tuple`, `Object`, `Enum { type_name, value }`, `Group` (a `group` statement's result), `VectorSearch`/`FtsSearch` (`{ object, distance }`/`{ object, score }`).

`Value::Object` wraps a field-name-indexed `Object` — a schema object, a free object literal (`select { a := 1 }`), and a named tuple all decode into this same shape:

```rust
let Value::Object(person) = &people[0] else { panic!() };
let name: &Value = person.get("name").unwrap();
// or: &person["name"]  (Index<&str>, panics if the field doesn't exist)
person.type_name();   // Some("default::Person") — None for a free object/unregistered named tuple
```

## Transactions

```rust
client.transaction(pylon_client::Isolation::Serializable, |tx| Box::pin(async move {
    tx.execute("insert Person { name := <str>$name }", &[("name", "Bob".into())]).await
})).await?;
```

`Client::transaction`/`transaction_with_attempts` re-run the closure once per attempt against a fresh `Transaction` (exposing the same query methods as `Client`, minus `analyze`); commits automatically on `Ok`, rolls back and retries (0ms, 100ms, 200ms, ... back-off) on a serialization failure/deadlock, up to the attempt budget (default 3 via `transaction`; `transaction_with_attempts` takes an explicit count) — rolls back and propagates immediately on anything else. Write closures accordingly: idempotent, no side effects outside the transaction itself, since a retried attempt reruns the whole body.

## `listen`

```rust
let mut listener = client.listen("UserUpdates").await?;
while let Some(payload) = listener.recv().await {
    println!("{:?}", payload?);
}
```

Subscribes to a schema-declared [`Channel`](../schema/channels.md) — *channel* is a bare or `module::name` reference, the same string [`notify(...)`](../pyql/globals-and-functions.md#notify--notify_raw) takes from PyQL. Unlike the [Python client's `listen()`](python.md#listen) (a typed async generator), this returns a `ChannelListener` whose `recv()` you call in a loop — this crate has no `Stream`/async-generator precedent to build one on top of, and `recv()` matches `tokio::sync::mpsc::Receiver`'s own idiom closely enough not to need one. `recv()` returns `None` once the connection closes.

Each decoded payload matches the Channel's own declared shape: `Value::Uuid` for a Type-shaped channel (the changed row's `id`, not a fetched object — see [Channels](../schema/channels.md)), the matching `Value` variant for a Scalar-shaped channel, or `Value::Object` for an Object-shaped channel. A payload that doesn't match its declared shape comes back as `Err(Error::MalformedPayload(_))` from that one `recv()` call rather than being silently dropped — the subscription itself keeps running; the next `recv()` call still waits for the next notification.

`recv()`'s scalar decode covers `uuid`/`int2`/`int4`/`int8`/`float4`/`float8`/`numeric`/`boolean`; `date`/`time`/`timestamp`/`timestamptz`/`interval`/`bytea` fall back to the raw NOTIFY text as `Value::Str` — this crate has no date/time dependency to convert them into `Value::Date`/`Time`/`Timestamp`'s PG-epoch-relative integer form correctly, and returning the raw text is safer than getting that silently wrong.

Like the Python client, `listen()` opens its own dedicated (non-pooled) connection — `LISTEN` is per-session, so running it on a pooled connection would leak the subscription onto whatever unrelated query later borrows that connection back out of the pool. The connection (and the server-side subscription with it) closes once the returned `ChannelListener` is dropped.

## Escape hatch: raw SQL

```rust
let pool: &pylon_pgcon::PgPool = client.raw_connection();
let rows = pool.query_typed("SELECT count(*) FROM some_table", &[], &pylon_pgcon::ExtensionOids::default()).await?;
```

`client.raw_connection()` returns the underlying `pylon_pgcon::PgPool` directly, bypassing PyQL entirely — nothing here is validated or type-checked the way a compiled PyQL query is.

## Errors

Every method returns `pylon_client::Result<T>` (`= std::result::Result<T, Error>`) — a real `Error` enum, not a boxed `dyn Error`, so the transaction retry loop can inspect the Postgres SQLSTATE directly:

| Variant | When |
|---|---|
| `Error::Db` | A connection/pool/decode/server-response failure from the driver. |
| `Error::Compile` | The PyQL string failed to compile — syntax, type, resolution, or cardinality error. |
| `Error::MissingParam` | A required query parameter had no matching entry in `params`. |
| `Error::ResultCardinality { got }` | `query_single`/`query_required_single` (or their `_json` siblings) got more than one row. |
| `Error::NoData` | `query_required_single` (or its `_json` sibling) got zero rows. |
| `Error::NoSchemaSnapshot` | Neither `pylon migration apply` nor `pylon migration watch` has ever run against this database. |
| `Error::UnknownChannel` | `listen(name)` — no `Channel` in the schema matches *name*. |
| `Error::MalformedPayload` | A `listen()` NOTIFY payload didn't match its Channel's declared shape. |
| `Error::Cache` | An LMDB cache open/get/put failure. |

`Error::is_serialization_error()` / `is_deadlock()` / `is_retriable()` identify the two conditions `transaction()`'s retry loop handles automatically.

## Differences from the Python client

|  | [Python](python.md) (`pylon.Client`) | Rust (`pylon_client::Client`) |
|---|---|---|
| Query results | Real `@pylon.type`-generated dataclasses | Generic `Value`/`Object` (no per-schema-type codegen) |
| `save()` (diff-and-upsert a hydrated object) | Yes | Not yet — hand-write the `insert`/`update` |
| `with_globals`/`with_config` | Per-call view, same pool | Same (`Client::with_globals`/`Client::with_config`) |
| `listen()` | Typed async generator | `recv()`-based `ChannelListener` handle |
| Config source | Auto-loads `pylon.toml` | DSN passed explicitly — never parses `pylon.toml` |
