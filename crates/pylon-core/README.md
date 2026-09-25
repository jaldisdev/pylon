# pylon-db-core

The [Pylon](https://github.com/jaldisdev/pylon) compiler: PyQL parsing, IR, SQL emission, schema export and migration diffing. Contains no PyO3 — it is what lets the Rust client and the Python extension share one implementation.

The crate is published as `pylon-db-core` and imported as `pylon_core`.

It is published so that [`pylon-db-client`](https://crates.io/crates/pylon-db-client) can be. Depend on that instead: this crate's API is internal to Pylon and may change in any release.

## Licence

MIT OR Apache-2.0.
