# Channels

A `Channel` declares a PostgreSQL pub/sub channel (`NOTIFY`/`LISTEN`) as a schema-level construct, giving it a declared payload shape instead of the raw untyped channel-name-plus-text-string Postgres itself offers. Like `Global`/`Alias`, it's a module-level declaration — but unlike them, it's a real bound value, not an annotation:

```python
# dbschema/shop.py
import uuid
import pylon

@pylon.type(module="shop", name="User")
class User:
    name: str

UserUpdates = pylon.Channel(User)
Pings = pylon.Channel(str, name="custom_ping")
SearchReady = pylon.Channel(pylon.Object(doc_id=uuid.UUID, score=float))
```

## Payload kinds

`Channel(payload_type, *, name=None, description=None)` accepts three shapes for `payload_type`:

- **A registered `@pylon.type`/`@pylon.interface`** — e.g. `pylon.Channel(User)`. `notify()` sends that row's `id`, not the whole object (see [`notify`](../pyql/globals-and-functions.md#notify--notify_raw)); `payload_type` isn't itself constructed, just referenced.
- **A plain scalar** — e.g. `pylon.Channel(str)`, `pylon.Channel(uuid.UUID)`, or a registered custom scalar. The payload is sent as text.
- **An ad hoc named-field shape**, via `pylon.Object(...)` — e.g. `pylon.Object(doc_id=uuid.UUID, score=float)`. This reuses the same `pylon.Object` class query results use for free-form shapes (`pylon.Object(name='hello', count=42)`, see [Client library](../client.md)) — here called with *types* as the keyword values instead of data, which the schema builder reads back to derive the payload's field names and types. Every field must be a scalar (same restriction as `Tuple`/`NamedTuple`). `notify()`'s payload for one of these must be a free object literal matching the declared fields exactly: `{ doc_id := .id, score := .relevance }`.

## Wire name

The actual PostgreSQL `NOTIFY`/`LISTEN` identifier is derived as `{module}__{snake_case(variable_name)}` — e.g. `UserUpdates` declared in the `shop` module becomes `shop__user_updates`. `name=` overrides this entirely, used verbatim with no casing applied (`Pings` above becomes `custom_ping`, not `shop__pings`).

This is the one schema construct whose module gets folded directly into its own externally-visible name, rather than kept as separate DDL-level namespacing (compare a type's module, which maps onto a PostgreSQL schema and never appears in the type's own identifier) — Postgres channels have no schema-namespacing concept at all, they're a single flat, database-wide identifier space. Consequently, wire names must be unique across the *entire* schema, not just within one module — two `Channel`s in different modules that happen to resolve to the same wire name (whether by coincidence or an explicit `name=` collision) is a `SchemaError`, caught at `finalize()` time.

Wire names starting with `pylon_` are rejected outright — that prefix is reserved for Pylon's own internal channels (`pylon_index_queue`, `pylon_signal_queue`, `pylon_cache_invalidate`).

## Zero DDL footprint

A `Channel` has no backing table, column, or any other catalog object — `NOTIFY`/`LISTEN` needs nothing created in Postgres ahead of time. This means adding, removing, or renaming a `Channel` produces no DDL at all, but it still needs to be recorded: `pylon migration create` detects this kind of schema-only change (the same mechanism that also covers a bare `Readonly` flip or a new `Rewrite`) and still produces a migration, so the change is properly tracked and reflected once applied — see [Migrations](../migrations.md).

## What's not built yet

Sending a payload (`notify()`) is covered in [PyQL: globals and functions](../pyql/globals-and-functions.md#notify--notify_raw), including using it from a `Trigger` handler. Receiving payloads (a PyQL `listen`/`unlisten` statement, and a typed `client.listen()` on the Python side) doesn't exist yet.
