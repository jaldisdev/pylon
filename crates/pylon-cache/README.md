# pylon-db-cache

The LMDB read-through query cache behind [Pylon](https://github.com/jaldisdev/pylon) — the store `[cache]` in `pylon.toml` turns on, shared by the clients and the worker that invalidates it.

The crate is published as `pylon-db-cache` and imported as `pylon_cache`.

It is published so that [`pylon-db-client`](https://crates.io/crates/pylon-db-client) can be. Depend on that instead: this crate's API is internal to Pylon and may change in any release.

## Licence

MIT OR Apache-2.0.
