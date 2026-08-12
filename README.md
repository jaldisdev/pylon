# Pylon

Async PostgreSQL mapper for the JALDIS platform. Provides a schema definition DSL, an PyQL query language, OpenSearch integration, and vector index support.

## Requirements

- Python 3.13+
- PostgreSQL

## Installation

```bash
pip install pylon
```

## Configuration

Pylon is configured programmatically via `Config` and its child dataclasses.

```python
import os
from pylon import Client, Config, DatabaseConfig, SearchConfig, ModelConfig

client = Client(
    config=Config(
        database=DatabaseConfig(
            host="localhost",
            port=5432,
            name="mydb",
            user="myuser",
            password=os.environ["PYLON_DB_PASSWORD"],
        ),
        search=SearchConfig(
            host="localhost",
            port=9200,
            password=os.environ["PYLON_SEARCH_PASSWORD"],
        ),
        models=ModelConfig(
            api_style="openai",
            api_url="https://api.openai.com",
            model="text-embedding-3-small",
            secret=os.environ["OPENAI_API_KEY"],
        ),
    )
)
```

A `pylon.toml` file can be used instead via `load_config()`:

```python
from pylon import Client
from pylon.config import load_config

client = Client(config=load_config())
```

See the [configuration reference](docs/config.md) for the full `pylon.toml` schema.

## Development

Pylon is a Rust workspace plus a Python package; the two are joined by a
PyO3 extension module (`pylon._core`) built with maturin.

### Building

```bash
maturin develop      # builds pylon._core into the active virtualenv
pip install -e ./    # maturin develop alone can leave a stale .so behind
```

The test suite fails fast with a "rebuild it" message if `pylon._core` is
older than the Python code being tested against it, rather than silently
skipping the affected tests.

### Testing

```bash
python -m pytest                        # Python suite (live-DB tests excluded)
cargo test --workspace --exclude pylon-py   # Rust suite
```

`--exclude pylon-py` is required, and is not the same caveat as excluding it
from linting. PyO3 extension modules link against the interpreter through
build configuration that only maturin supplies, so a plain `cargo build` or
`cargo test` on that crate fails at the link step with undefined symbols
(`_PyList_New`, `_Py_NoneStruct`, …). It still *type-checks* fine, which is
why CI runs `cargo clippy -p pylon-py --lib` separately — that crate is the
only consumer of several `pylon-core`/`pylon-pgcon` APIs, so skipping it
entirely would let a signature change break the extension unnoticed.

Live-Postgres tests are opt-in and need a throwaway database:

```bash
export PYLON_PGCON_TEST_DSN=postgresql://postgres:postgres@localhost:5432/pylon_live_test
cargo test --workspace --exclude pylon-py -- --ignored --test-threads=1
python -m pytest -m live_db
```

These create and drop schemas freely — never point them at a real database.

### Linting

```bash
cargo clippy --workspace --exclude pylon-py --all-targets -- -D warnings
cargo clippy -p pylon-py --lib -- -D warnings
cargo fmt --check
ruff check pylon tests
ruff format --check pylon tests
```
