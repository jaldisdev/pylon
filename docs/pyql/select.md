# SELECT

```pyql
select Person
select Person { name, age }
select Person { name, age } filter .name = $name
select Person { name, posts { title, body } }
select distinct Person { name }
select Person { name } order by .name asc offset 10 limit 5
```

`select <expr> [{ shape }] [filter <bool-expr>] [order by ...] [offset <expr>] [limit <expr>]`. A bare `select Person` with no shape returns each matching row's `id` only — a shape is what pulls specific properties/links into the result (see [Paths and shapes](paths-and-shapes.md)).

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
