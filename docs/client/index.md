# Client libraries

Two ways to run PyQL against a Pylon-managed database:

- **[Python](python.md)** (`pylon.Client`) — the primary, most complete client. Used by the CLI, the REPL, and everywhere else in this project that talks to a database from Python. Query results hydrate into real instances of your `@pylon.type` classes.
- **[Model API](model-api.md)** — build queries and mutations from those same `@pylon.type` classes instead of writing PyQL text: `client.query(Person)`, `Person.filter(...)`, `client.save(obj)`, `std.*` expressions, and link assignment including `+=` / `-=` on multi-links. Renders to ordinary PyQL and runs through the Python client unchanged.
- **[Rust](rust.md)** (`pylon_client::Client`) — a native client for a Rust process that doesn't want a Python/pyo3 dependency at all (e.g. `pylon-server`). A separate implementation, not a wrapper around the Python one — query results decode into a generic `Value`/`Object` instead, since Rust has no per-schema-type codegen to hydrate into.

Both compile the same PyQL through `pylon-core` and connect through the same underlying driver (`pgcon`), so a query that works against one works identically against the other — see [Rust client § Differences from the Python client](rust.md#differences-from-the-python-client) for exactly what varies.
