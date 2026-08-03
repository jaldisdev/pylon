# SELECT

```pyql
select Person
select Person { name, age }
select Person { name, age } filter .name = $name
select Person { name, posts { title, body } }
select distinct Person { name }
select Person { name } order by .name asc offset 10 limit 5
```

`select <expr> [{ shape }] [filter <bool-expr>] [order by ...] [offset <expr>] [limit <expr>] [for update|share|no key update|key share [nowait|skip locked]]`. A bare `select Person` with no shape returns each matching row's `id` only — a shape is what pulls specific properties/links into the result (see [Paths and shapes](paths-and-shapes.md)).

## `filter`

A boolean PyQL expression, evaluated once per candidate row — see [Operators](operators.md) for what's available (`=`, `and`/`or`, `like`/`ilike`, `in`, etc.):

```pyql
select Person { name } filter .age >= 18 and .active = true
```

## `order by`

```pyql
select Person { name } order by .name asc
select Person { name } order by .age desc, .name asc
```

Each clause is `<expr> [asc|desc]` (default `asc`); multiple clauses are comma-separated, applied in order.

## `offset` / `limit`

```pyql
select Person { name } order by .name offset 10 limit 5
```

Both take any scalar expression, not just a literal — including a [parameter](parameters.md) (`limit $page_size`).

## Row locking (`for update` / `for share`)

```pyql
select default::Job filter .status = 'pending' order by .priority asc limit 1 for update skip locked
select default::Job filter .id = <uuid>$id for update nowait
select default::Job filter .id = <uuid>$id for share
select default::Job filter .id = <uuid>$id for no key update
select default::Job filter .id = <uuid>$id for key share
```

A trailing row-locking clause, placed last (after `order by`/`offset`/`limit`, matching Postgres's own grammar — not right after `filter`). Compiles straight through to Postgres's `SELECT ... FOR UPDATE|SHARE|NO KEY UPDATE|KEY SHARE [NOWAIT|SKIP LOCKED]`, with the same semantics:

- **Lock strength** — `for update` (exclusive) and `for no key update` (exclusive, but doesn't block a foreign key referencing the row) block concurrent writers; `for share` and `for key share` (weaker: doesn't block a concurrent `for no key update`) block concurrent writers but allow concurrent readers.
- **Wait behavior** — with no modifier, a transaction that can't acquire the lock blocks until it can. `nowait` fails immediately instead (Postgres's `55P03`/`lock_not_available` error) rather than waiting. `skip locked` silently excludes any row it can't lock instead of blocking or failing — the classic job-queue dequeue pattern:

  ```pyql
  select default::Job filter .status = 'pending' order by .priority asc limit 1 for update skip locked
  ```

  Each worker's transaction claims the next unclaimed row and moves on; a row another worker already has locked is simply skipped, not waited for.

Not allowed — same restriction Postgres itself enforces, since a locking clause only makes sense when every output row maps 1:1 to a physical table row:

- Combined with `distinct`.
- On an interface (polymorphic) type — its rows span more than one underlying table.
- On `select (insert/update/delete ...) { ... }` — there's nothing left to lock once the DML has already run.

## `distinct`

```pyql
select distinct Person { name }
```

A prefix modifier right after `select`, not a shape-level or per-column thing — deduplicates entire result rows.

## Free selects

`select` doesn't require a schema type at all — a *free* select evaluates any expression directly:

```pyql
select 1 + 2
select { 1, 2, 3 }          # a set literal — three rows
select (1, 'hello')          # a positional tuple — one row
select { foo := 'bar', n := 42 }   # a free (named-field) object
select str_lower('HELLO')
select <str>$value
```

## Nested shapes and modifiers

A shape element that's itself a link can carry its own `filter`/`order by`/`offset`/`limit`, scoped to just that nested set:

```pyql
select Author {
    name,
    posts: { title, body } filter .published = true order by .created_at desc limit 5,
}
```

See [Paths and shapes](paths-and-shapes.md) for the full shape grammar (splats, computed overrides, backlinks, link properties).
