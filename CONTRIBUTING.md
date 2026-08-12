# Contributing to Pylon

Thanks for your interest in contributing to Pylon! This document covers how
to get set up, the expectations for pull requests, and where to look first if
you're new to the codebase.

## Code of Conduct

This project follows a [Code of Conduct](CODE_OF_CONDUCT.md). By
participating, you agree to abide by its terms.

## Project layout

```
crates/pylon-core/       PyQL parser, IR, SQL emitter, schema export, migration diffing
crates/pylon-pgcon/      PostgreSQL driver: pooling, wire decode, LISTEN/NOTIFY
crates/pylon-py/         PyO3 bindings exposing the Rust crates to Python
crates/pylon-client/     Native Rust query client (no PyO3)
crates/pylon-server/     HTTP server for the API and Query Editor
crates/pylon-workers/    Background workers: index outbox, cache invalidation
crates/pylon-cache/      LMDB read-through cache
crates/pylon-config/     pylon.toml parsing
crates/pylon-value/      The decoded-value type shared across every crate
crates/pylon-lsp/        Language server for in-editor PyQL diagnostics
crates/pylon-providers/  Embedding-provider HTTP clients
pylon/                   Python package: schema DSL, client, CLI
tests/                   Python test suite (pytest)
docs/                    User-facing documentation
```

Two boundaries are load-bearing; please keep them intact in new code:

* **`pylon-core` and `pylon-pgcon` contain no PyO3.** Anything touching the
  Python C API belongs in `pylon-py`. This is what lets `pylon-client` and
  `pylon-server` use the same engine without an interpreter in the process.
* **`pylon-pgcon` doesn't depend on `pylon-core`.** It's usable on its own as
  a plain Rust PostgreSQL client.

## Getting started

### Prerequisites

* Rust (stable toolchain)
* Python 3.13+
* PostgreSQL (only needed for the live test suites)

### Setup

```bash
git clone https://github.com/<org>/pylon.git
cd pylon
python -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"
maturin develop      # builds the pylon._core extension module
pip install -e ./    # maturin develop alone can leave a stale .so behind
```

### Running tests

```bash
# Python
python -m pytest

# Rust — see below for why pylon-py is excluded
cargo test --workspace --exclude pylon-py

# Lint / format (CI enforces all of these, so run them before opening a PR)
cargo clippy --workspace --exclude pylon-py --all-targets -- -D warnings
cargo clippy -p pylon-py --lib -- -D warnings
cargo fmt --check
ruff check pylon tests
ruff format --check pylon tests
```

**Why `--exclude pylon-py`:** PyO3 extension modules link against the
interpreter through build configuration only maturin supplies, so a plain
`cargo build`/`cargo test` on that crate fails at the link step with
undefined symbols. It still type-checks, which is why it gets its own
`cargo clippy -p pylon-py --lib` step — that crate is the sole consumer of
several `pylon-core`/`pylon-pgcon` APIs, and skipping it entirely lets a
signature change break the extension with nothing noticing.

### Live-Postgres tests

Both suites have an opt-in half that needs a real database. These create and
drop schemas freely, so point them at a throwaway:

```bash
export PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test
cargo test --workspace --exclude pylon-py -- --ignored --test-threads=1
python -m pytest -m live_db
```

Rust live tests are gated behind `#[ignore]`; Python's behind the `live_db`
marker. Neither has a hardcoded DSN fallback — the environment variable is
required, deliberately, so a live test can never silently run against
whatever database happened to be configured.

## Making a change

1. Open an issue first for anything non-trivial (new public API, behavior
   change, a new schema construct) so we can agree on the approach before you
   invest time in an implementation.
2. Keep PRs focused — one logical change per PR is much easier to review and
   bisect later.
3. Add tests for new behavior. Rust logic should have Rust unit tests; anything
   that only shows up against a real database should have a live test too.
   Python-facing behavior should have a `pytest` test that exercises the full
   path (Python → Rust → Python) rather than mocking the boundary.
4. Update `docs/` when you change or add public API surface.
5. Run the full test suite and linters locally before opening the PR.

## Terminology

Pylon has its own vocabulary, and user-facing text should use it consistently:

* A query returns a **set** of **objects** — not a table of rows. "Row" and
  "table" are correct only when genuinely discussing the PostgreSQL layer.
* A PostgreSQL *schema* is a Pylon **module**. Don't call a module a schema in
  user-facing text; "schema" in Pylon means the whole type declaration set.
* `@pylon.abstract` is a **mixin** with no query surface;
  `@pylon.interface` is the one that's polymorphically queryable. They are not
  interchangeable.

## Migration files

Migration IDs are content-addressed over the migration's body *and* its
position in the chain, prefixed with a format version (`m2`). If you change
what goes into that hash, bump the prefix and keep the previous format
verifying — existing migration files on disk must not stop validating. See
`crates/pylon-core/src/migration/mod.rs`.

## Commit messages and PR descriptions

Write commit messages that explain *why*, not just *what* — the diff already
shows what changed. In the PR description, note any behavior changes,
migration considerations, and what you tested.

## License

By contributing, you agree that your contributions will be dual-licensed
under the [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE) licenses, the
same as the rest of the project.
