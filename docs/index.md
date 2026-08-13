# Pylon documentation

Pylon is an async PostgreSQL data layer for Python: a typed schema definition DSL (plain Python classes and decorators — no separate declarative text format), a query language (**PyQL**) that compiles to native SQL, a migration/diff engine that generates DDL from schema changes, and both a Python ASGI-friendly client library and a standalone Rust server binary for serving queries over HTTP.

Pylon's data model centers on object types, links between them, and computed pointers, queried through an expression-based query language — all declared as ordinary Python code rather than a separate declarative text format, and compiled straight to plain PostgreSQL with no separate database server process of its own.

## Start here

- **[Getting started](getting-started.md)** — install Pylon, write your first schema module, generate and apply a migration, run your first query.

## Schema

The schema DSL — Python classes and decorators that define your data model. Read [`schema/index.md`](schema/index.md) first for the module system and the `pylon.finalize()` pipeline, then:

- [Modules](schema/modules.md) — how `module=` maps onto PostgreSQL schemas
- [Object types](schema/types.md) — `@pylon.type`, `@pylon.abstract`, `@pylon.interface`, inheritance, junctions
- [Properties and scalars](schema/properties-and-scalars.md) — `Property[]`, built-in and custom scalars, enums, named tuples, structural tuples/arrays
- [Links](schema/links.md) — `Link`, `MultiLink`, junction tables, deletion policies, link properties
- [Computed pointers](schema/computed.md) — `Computed[]`
- [Constraints](schema/constraints.md) — `Default`, `Exclusive`, `Expression`, value/length bounds, `Readonly`
- [Indexes](schema/indexes.md) — `Index`, `VectorIndex`, `SearchIndex`
- [Functions](schema/functions.md) — `@pylon.function`
- [Triggers and rewrites](schema/triggers-and-rewrites.md) — `Trigger`, `Rewrite`
- [Globals and aliases](schema/globals-and-aliases.md) — `Global`, `Alias`
- [Channels](schema/channels.md) — `Channel`, PostgreSQL pub/sub (`NOTIFY`/`LISTEN`)
- [Signals](schema/signals.md) — `@pylon.signal`, post-commit Python callbacks
- [Schema validation](schema/validation.md) — everything `pylon.finalize()` checks, and why

## PyQL

The query language. Read [`pyql/index.md`](pyql/index.md) first, then:

- [SELECT](pyql/select.md)
- [INSERT / UPDATE / DELETE](pyql/insert-update-delete.md)
- [FOR, GROUP, WITH](pyql/for-group-with.md)
- [Paths and shapes](pyql/paths-and-shapes.md)
- [Literals and types](pyql/literals-and-types.md)
- [Operators](pyql/operators.md)
- [Parameters](pyql/parameters.md)
- [Globals and functions](pyql/globals-and-functions.md)

## Standard library

Built-in functions callable from PyQL, grouped by namespace. See [`stdlib/index.md`](stdlib/index.md), then the per-namespace reference: [`std`](stdlib/std.md), [`math`](stdlib/math.md), [`cal`](stdlib/cal.md), [`sys`](stdlib/sys.md), [`pgvector`](stdlib/pgvector.md), [`crypto`](stdlib/crypto.md), [`postgis`](stdlib/postgis.md).

The same functions are reachable from Python as `from pylon import std` — see [Model API § The `std` namespace](client/model-api.md#the-std-namespace).

## Operating Pylon

- **[`pylon.toml` configuration](config.md)** — every config key, CLI/env/file precedence
- **[CLI reference](cli.md)** — every `pylon` subcommand and flag
- **[Migrations](migrations.md)** — the diff-engine workflow in depth
- **[Client libraries](client/index.md)** — Python `Client`/`AsyncTransaction`, and the native Rust `pylon-client`
- **[Server](server.md)** — `pylon-server`, the standalone Rust HTTP server, background workers, deployment
