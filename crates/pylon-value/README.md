# pylon-db-value

The decoded-value type shared across every [Pylon](https://github.com/jaldisdev/pylon) crate — one enum covering the PostgreSQL types Pylon reads off the wire, in the representation it keeps them in.

The crate is published as `pylon-db-value` and imported as `pylon_value`.

It is published so that [`pylon-db-client`](https://crates.io/crates/pylon-db-client) can be. Depend on that instead: this crate's API is internal to Pylon and may change in any release.

## Licence

MIT OR Apache-2.0.
