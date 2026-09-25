# pylon-db-client

Native Rust client for [Pylon](https://github.com/jaldisdev/pylon) — runs PyQL queries against a Pylon-managed PostgreSQL database with no Python interpreter and no PyO3 involved.

The crate is published as `pylon-db-client` and imported as `pylon_client`.

```toml
[dependencies]
pylon-db-client = "0.1"
```

```rust
use pylon_client::{Client, Value};

let client = Client::builder(dsn).max_pool_size(10).build()?;

let people = client.query("select Person { name, age }", &[]).await?;
let person = client
    .query_single("select Person filter .id = <uuid>$id", &[("id", some_id.into())])
    .await?;
```

`Builder::build()` reaches nothing over the network; the pool and the schema snapshot open together on first use. Call `ensure_connected()` to fail at startup instead.

Results decode either into a generic, dynamically-typed `Value`/`Object`, or into your own row struct via `#[derive(Queryable)]` — there is no generated per-schema-type code either way. Also here: retrying `transaction()` closures with an explicit `Error::Rollback`, `listen()` on schema-declared channels, optional read-through LMDB caching, and `raw_connection()` as the escape hatch to plain SQL.

Requires a database that `pylon migration apply` (or `pylon migration watch`) has run against — this crate reads the schema snapshot it writes, and never parses `pylon.toml` itself.

Full documentation: [docs/client/rust.md](https://github.com/jaldisdev/pylon/blob/canary/docs/client/rust.md).

## Licence

MIT OR Apache-2.0.
