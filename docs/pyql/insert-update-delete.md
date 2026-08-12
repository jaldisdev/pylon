# INSERT, UPDATE, DELETE

## INSERT

```pyql
insert Person { name := 'Alice', age := 30 }
```

`insert Type { field := expr, ... }`. Every required, non-defaulted property/link needs an assignment; `id` is never assigned (Pylon always generates it) unless the client explicitly opts into `allow_user_specified_id` (see [Python client § with_config](../client/python.md#with_config)).

Assign a link by subquery:

```pyql
insert Post {
    title := 'Hello',
    author := (select Person filter .id = <uuid>$author_id),
}
```

Assign a multi-link with a set of subqueries, and set link properties (for a `Through[...]`-junction link) with `@propname`:

```pyql
insert Product {
    name := 'Widget',
    tags := (select Tag filter .id in array_unpack($tag_ids)) { @weight := 1.0 },
}
```

### `unless conflict`

```pyql
insert Product { sku := 'ABC-123', name := 'Widget' }
unless conflict on .sku
else (update Product set { name := 'Widget' })
```

Maps to PostgreSQL's `INSERT ... ON CONFLICT`. `on <expr>` names the conflicting column (or expression) — typically a property with an [`Exclusive`](../schema/constraints.md#exclusive) constraint; `else (update Type set { ... })` becomes the `DO UPDATE SET` clause, evaluated against the existing conflicting row (its own `filter`, if given, is ignored — Postgres already knows which row conflicted from the `on` target). Omit `else` entirely for a plain "insert, or silently do nothing on conflict."

## UPDATE

```pyql
update Person filter .id = <uuid>$id set { age := .age + 1 }
```

`update <expr> [filter <bool-expr>] set { field := expr, ... }`. `.age` on the right-hand side refers to the object's *current* value, so `.age := .age + 1` is a genuine read-modify-write, not a self-referential error.

Multi-link updates support three operators instead of just replace:

```pyql
update Product filter .id = <uuid>$id set {
    tags := (select Tag filter .name = 'featured'),   # := replace the whole set
}
update Product filter .id = <uuid>$id set {
    tags += (select Tag filter .name = 'sale'),        # += append
}
update Product filter .id = <uuid>$id set {
    tags -= (select Tag filter .name = 'sale'),        # -= remove
}
```

A property/link with a schema-declared [`Rewrite(On.Update, ...)`](../schema/triggers-and-rewrites.md#rewrite) is transformed by that rewrite after your `set { }` assignment, before the value reaches the database — and a property marked [`Readonly`](../schema/constraints.md#readonly) can't appear in `set { }` at all; the transpiler rejects it at compile time.

## DELETE

```pyql
delete Person filter .name = 'unused test row'
```

`delete <expr> [filter <bool-expr>]` — no shape. Deleting without a `filter` deletes every row of that type; there's no separate "confirm you meant that" step at the language level (the responsibility sits with the caller, same as a bare `DELETE FROM table` in SQL).

## Using a mutation's result

Every mutation is itself a valid subquery expression — wrap it in parens and `select` it to shape the result:

```pyql
select (insert Person { name := 'Alice', age := 30 }) { id, name }
select (update Person filter .id = <uuid>$id set { age := .age + 1 }) { age }
select (delete Person filter .id = <uuid>$id) { name }
```

See [Client libraries](../client/index.md) for `execute()` (discard the result entirely) vs. `query`/`query_single` (get it back shaped).
