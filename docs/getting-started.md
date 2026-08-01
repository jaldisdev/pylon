# Getting started

This walks through installing Pylon, defining a small schema, applying it to a real PostgreSQL database, and running your first query.

## Requirements

- Python 3.13+
- PostgreSQL (any recent version reachable over the network — Pylon connects directly, there's no separate Pylon-specific server process for the database itself)

## Install

```bash
pip install pylon
```

If you're developing Pylon itself (working in this repository rather than consuming it as a package), the Rust extension is built with [maturin](https://www.maturin.rs/) instead:

```bash
maturin develop -m crates/pylon-py/Cargo.toml
pip install -e ./ --no-deps
```

Both steps are required after any change to the Rust side — `maturin develop` alone can leave a stale compiled extension behind.

## Project layout

A Pylon project needs a `pylon.toml` at its root and a schema directory (any name — `pylon.toml` points at it):

```
myproject/
  pylon.toml
  dbschema/
    blog.py
```

`pylon.toml`:

```toml
[project]
schema-dir = "dbschema"

[database]
host = "localhost"
port = 5432
name = "myproject"
user = "myproject"
password_env = "PYLON_DB_PASSWORD"
```

See the [configuration reference](config.md) for every available key.

## Write a schema module

Every `.py` file directly under `schema-dir` (that doesn't start with `_`) becomes a Pylon **module** — see [Modules](schema/modules.md) for how that name maps onto a PostgreSQL schema. `dbschema/blog.py`:

```python
import pylon


@pylon.type
class Author:
    name: str
    email: pylon.Property[str, pylon.Exclusive]


@pylon.type
class Post:
    title: str
    body: str
    published: pylon.Property[bool, pylon.Default(False)]
    created_at: pylon.Property[pylon.DateTime, pylon.Default(pylon.Now)]
    author: pylon.Link[Author]
```

This declares two object types (`blog::Author`, `blog::Post` — module name `blog` comes from the filename), a unique `email` property, a boolean with a literal default, a timestamp defaulting to the current time, and a required link from `Post` to `Author`. See [Object types](schema/types.md), [Properties and scalars](schema/properties-and-scalars.md), [Links](schema/links.md), and [Constraints](schema/constraints.md) for the full picture.

## Install the standard library

Once, per database — installs `_pylon`, the internal schema Pylon's own generated SQL functions live in:

```bash
pylon database initialize
```

Safe to re-run; every statement it generates is idempotent.

## Generate and apply a migration

```bash
pylon migration create
```

This diffs your schema module(s) against the live database and writes a migration file under `dbschema/migrations/`, prompting interactively if anything is ambiguous (a rename vs. a drop+create, a cast for a changed property type). Then:

```bash
pylon migration apply
```

See [Migrations](migrations.md) for the full workflow, including `--dev` mode, `status`, `log`, `watch`, `squash`, and `rehash`.

## Run your first query

```python
import asyncio
import pylon
from pylon.config import load_config


async def main() -> None:
    client = pylon.Client(config=load_config())
    await client.ensure_connected()

    author = await client.query_required_single(
        "insert Author { name := 'Ada', email := 'ada@example.com' }"
    )
    post = await client.query_required_single(
        """
        insert Post {
            title := 'Hello, Pylon',
            body := 'First post.',
            author := (select Author filter .id = <uuid>$author_id),
        }
        """,
        author_id=author.id,
    )
    print(post.title, post.author.name)

    await client.aclose()


asyncio.run(main())
```

`client.query_required_single` compiles the PyQL string to SQL, executes it, and hydrates the result into a real Python object matching your schema class. See [PyQL](pyql/index.md) for the query language itself and [Client library](client.md) for every method `Client` exposes (`query`, `query_single`, `execute`, transactions, `with_config`, `save`).

## Where to next

- [Schema validation](schema/validation.md) — what `pylon.finalize()` catches before your schema ever reaches the database
- [Server](server.md) — running `pylon-server` to serve queries over HTTP instead of embedding `Client` directly
- [Standard library](stdlib/index.md) — every built-in function callable from PyQL
