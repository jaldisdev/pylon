# Standard library

Every function below is callable from PyQL, either unqualified (Pylon resolves an unqualified call across every namespace) or module-qualified (`math::sqrt(2.0)`). See [Globals and functions](../pyql/globals-and-functions.md#calling-functions) for general call syntax.

The `std`, `math`, `cal`, and `sys` namespaces are also importable from Python — `from pylon import std` — for use in query-builder expressions and pointer defaults. Names and argument counts are validated against the same registry documented here. See [Model API § The `std` namespace](../client/model-api.md#the-std-namespace).

## Namespaces

| Namespace | Contents |
|---|---|
| [`std`](std.md) | Aggregates, assertions, strings, numerics, generics, UUIDs, JSON, bitwise ops, bytes, arrays, ranges, datetimes, type conversions, sequences. |
| [`math`](math.md) | Trigonometric and other mathematical functions. |
| [`cal`](cal.md) | Calendar/local-date arithmetic (as opposed to `std`'s timezone-aware `datetime` functions). |
| [`sys`](sys.md) | System/session introspection. |
| [`pgvector`](pgvector.md) | Thin wrapper over the `pgvector` Postgres extension — vector distance operators. |
| [`crypto`](crypto.md) | Thin wrapper over `pgcrypto` — hashing and digest functions. |
| [`postgis`](postgis.md) | Thin wrapper over the PostGIS extension — geometry/geography functions. |

## How to read a function table

Each function name gets one entry, covering every overload together (the same name with different parameter types) — matching how the standard library itself is organized (`crates/pylon-core/src/stdlib/registry/*_ns.rs`, one `FnDescriptor` per overload, grouped by name). A table row's **Signature** column lists every overload's parameter types; **Returns** is that overload's return type; where every overload shares one behavior, the description covers all of them at once.

- `set of T` means the function is an aggregate or set-producing function — it consumes/produces a *set*, not a single scalar.
- `T?` marks an optional/nullable return.
- `array<T>` / `range<T>` / `multirange<T>` follow the same structural-type spelling used in [casts](../pyql/literals-and-types.md).

Three namespaces (`pgvector`, `crypto`, `postgis`) are near-mechanical passthroughs to their matching Postgres extension — their own upstream documentation ([pgvector](https://github.com/pgvector/pgvector), [pgcrypto](https://www.postgresql.org/docs/current/pgcrypto.html), [PostGIS](https://postgis.net/docs/)) is the authoritative reference for the underlying behavior; the tables here exist so you don't have to guess the PyQL-side name/signature mapping.
