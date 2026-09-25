# pylon-db-pgcon

The PostgreSQL connection layer behind [Pylon](https://github.com/jaldisdev/pylon): pooling, binary wire-format decoding, and LISTEN/NOTIFY. Depends on no other Pylon crate.

The crate is published as `pylon-db-pgcon` and imported as `pylon_pgcon`.

It is published so that [`pylon-db-client`](https://crates.io/crates/pylon-db-client) can be. Depend on that instead: this crate's API is internal to Pylon and may change in any release.

## Licence

MIT OR Apache-2.0.
