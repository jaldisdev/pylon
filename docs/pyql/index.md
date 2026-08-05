# PyQL

PyQL is Pylon's query language — compiled to native SQL by `pylon-core`, never executed as an interpreted layer on top of Postgres. A query string goes through `Client.query()`/`execute()`/etc. (see [Client libraries](../client/index.md)), or the [`pylon` REPL](../cli.md#pylon-repl)/[`pylon query`](../cli.md#pylon-query) command.

## Statements

- [`select`](select.md) — read data, optionally shaping nested links into the result
- [`insert` / `update` / `delete`](insert-update-delete.md) — mutations
- [`for` / `group` / `with`](for-group-with.md) — looping, grouping, and named sub-expressions
- `analyze <stmt>` — run any of the above through `EXPLAIN (ANALYZE, FORMAT JSON)` instead of executing it normally (see [Python client § analyze](../client/python.md#analyze))

## The rest of this reference

- [Paths and shapes](paths-and-shapes.md) — how `.name`/`.posts.title` traversal and `{ ... }` shapes work
- [Literals and types](literals-and-types.md) — literals, casts, tuples, arrays, enum access
- [Operators](operators.md) — comparison, logical, arithmetic, `is`, `union`/`except`
- [Parameters](parameters.md) — `$1`/`$name`, client-side argument binding
- [Globals and functions](globals-and-functions.md) — `global name`, calling functions, `vector::search`/`fts::search`

## Syntax notes

- Statement keywords (`select`, `insert`, `update`, `delete`, `for`, `group`, `with`, `filter`, `order by`, `by`, `union`, `using`, and the `for update`/`share`/`no key update`/`key share`/`nowait`/`skip locked` row-locking clause's own words) are case-insensitive — `SELECT` and `select` are identical.
- A statement is a single expression tree; there's no statement-separator token needed for a query passed to `Client` (the REPL uses a trailing `;` purely as its own "run now" signal, not part of the language).
- `#`-prefixed line comments are supported.

## A quick tour

```pyql
select Person { name, age } filter .age >= 18 order by .name limit 10

insert Post {
    title := 'Hello',
    author := (select Person filter .id = <uuid>$author_id),
}

update Person filter .id = <uuid>$id set { age := .age + 1 }

delete Person filter .name = 'unused test row'

with recent := (select Order filter .created_at > <datetime>$since)
select recent { id, total }
```
