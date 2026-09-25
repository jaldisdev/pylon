# pylon-db-derive

`#[derive(Queryable)]` for the [Pylon](https://github.com/jaldisdev/pylon) Rust client — decodes a query result into a caller's own row struct, matching fields by name.

The crate is published as `pylon-db-derive` and imported as `pylon_derive`.

You do not need to depend on it directly: [`pylon-db-client`](https://crates.io/crates/pylon-db-client) re-exports the macro, so `use pylon_client::Queryable;` brings in both the trait and the derive.

## Licence

MIT OR Apache-2.0.
