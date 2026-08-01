# Modules

Every Pylon object type, function, scalar, enum, etc. belongs to a **module** — a namespace, unrelated to Python's own module system except that it's usually inferred from one. A module maps directly onto a PostgreSQL schema:

| Pylon module | PostgreSQL schema |
|---|---|
| `default` | `public` |
| anything else | a schema of that same name |

> **Terminology**: always "module" here, never "schema" — "schema" is reserved for the PostgreSQL-level concept a module maps onto, or for "the schema" meaning your whole compiled data model. Calling a module a schema is a real source of confusion once you're also talking about the PostgreSQL schema it produces.

## How a module name is chosen

For a class decorated `@pylon.type` (or `@pylon.abstract`, `@pylon.interface`, `@pylon.enum`, `@pylon.scalar`, `@pylon.function`, ...), the module comes from, in order:

1. An explicit `module=` keyword on the decorator: `@pylon.type(module="shop")`.
2. A `__pylon_module__` variable set at the top of the defining Python file: `__pylon_module__ = "shop"`.
3. The last component of the file's own dotted Python module path — which, for a normal schema file imported by `pylon.finalize()`'s directory scan, is just the filename stem. `dbschema/shop.py` → module `shop`.

So in the common case you never write `module=` at all — one schema file per module, named after it:

```python
# dbschema/shop.py — every type here defaults to module "shop"
import pylon

@pylon.type
class Product:
    name: str
```

```python
# dbschema/blog.py — module "blog", a completely separate PostgreSQL schema
import pylon

@pylon.type
class Post:
    title: str
```

A [`Link`](links.md)/[`MultiLink`](links.md) can point across modules freely — Pylon doesn't restrict foreign keys to staying within one PostgreSQL schema.

## The `default` module

Types declared with no module information at all (no `module=`, no `__pylon_module__`, and a defining file whose stem happens to be `default`, or more commonly just relying on the fallback) land in PostgreSQL's `public` schema — this is the one module name with special treatment, matching how `public` is already the schema every fresh Postgres database starts with.

## Reserved names

A module can't be named `public`, `pg_catalog`, `information_schema`, or `pg_toast` — these collide with PostgreSQL's own reserved schema names and are rejected at `pylon.finalize()` time (`'{stem}.py' is not a valid module name: ... is a reserved PostgreSQL schema name`).

## Cross-module references

Every qualified name in error messages, DDL, and PyQL casts is `module::Name` — e.g. `shop::Product`, `blog::Post`. Within PyQL, an unqualified `Product` resolves against the whole schema (there's no PyQL notion of "the current module" the way a Python file has `__pylon_module__`) as long as the name isn't ambiguous across modules; a genuinely ambiguous bare name needs the `module::Name` qualified form.
