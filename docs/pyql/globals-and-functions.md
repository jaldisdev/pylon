# Globals and functions

## `global name`

```pyql
select Post filter .author.id = global current_user_id
```

References a schema-declared [`Global`](../schema/globals-and-aliases.md#global). A session global's value comes from the client (`with_globals()`) or, in the REPL, `set global name := expr;`; a computed global re-evaluates its declared expression every time it's referenced. `global module::name` disambiguates if the bare name is ambiguous across modules.

## Calling functions

```pyql
select count(Person)
select str_lower('HELLO')
select mysum(1, 2)
```

Any [stdlib](../stdlib/index.md) or [user-defined](../schema/functions.md) function — positional and keyword arguments both work (`f(a, b)`, `f(a, b := 2)`), and an unqualified call resolves against every namespace (`std`, `math`, `cal`, ...) plus your own schema's functions, the same way an unqualified type name resolves across modules. Module-qualify a call when needed: `math::sqrt(2.0)`.

## Cardinality assertions

```pyql
select assert_exists((select Person filter .id = <uuid>$id))
select assert_distinct((select Person filter .company = <uuid>$company_id))
```

`assert_exists(subquery)` raises a database-level error if the wrapped subquery's result is empty — turns "silently got an empty set" into a hard failure at the point that's actually wrong, rather than downstream. `assert_distinct(subquery)` raises if any element appears more than once.

## `notify` / `notify_raw`

```pyql
select notify(UserUpdates, __new__)
select notify(Pings, .name)
select notify(SearchReady, { doc_id := .id, score := .relevance })
select notify_raw('any_channel_name', 'raw text payload')
```

Sends a PostgreSQL `NOTIFY` on a schema-declared [`Channel`](../schema/channels.md)'s wire name. The payload's required shape depends on the Channel's own declared kind:

- **Type-shaped** — payload must be `__new__` or `__old__` (only valid inside a [`Trigger`](../schema/triggers-and-rewrites.md) handler in this phase); sends that row's `id`, not the whole object.
- **Scalar-shaped** — payload is any expression of the matching type, e.g. `.name` on the object a trigger handler is firing for.
- **Object-shaped** — payload must be a free object literal (`{ field := expr, ... }`) whose field names match the declared shape exactly.

`notify_raw(channel_name, payload)` bypasses Channel resolution and payload-shape checking entirely — both arguments are arbitrary text expressions, an escape hatch for a channel Pylon doesn't know about.

Both raise a compile error if a literal string payload is at or over PostgreSQL's 8000-byte `NOTIFY` payload limit; a payload that isn't a literal (a column, a parameter, an expression) can't be checked at compile time and is left as-is.

## `vector::search`

```pyql
select vector::search(Product, <array<float32>>$vec) { object { name, price }, distance }
select vector::search(Product, query := $text) { object { name }, distance }
select vector::search((select Product filter .price < 100), <array<float32>>$vec) { object { name }, distance }
```

Similarity search over a [`VectorIndex`](../schema/indexes.md#vectorindex-embeddings). First argument is a type name (searches every indexed row of that type) or a filtered subquery (narrows the candidate set first). Second argument is either a raw embedding vector, or `query := $text` — the *text-overload* form, which has the client embed the text via the configured `[models.*]` provider before the query even reaches Postgres (see `_compile_and_resolve` in [Python client](../client/python.md)). The result shape is fixed: `{ object { ...fields... }, distance }` — `object`'s sub-shape is an ordinary shape over the target type; `distance` is always emitted, never itself shaped.

## `fts::search`

```pyql
select fts::search(Product, $query) { object { name }, score }
```

Full-text search over a [`SearchIndex`](../schema/indexes.md#searchindex-full-text-search). Same fixed-shape convention as `vector::search`, with `score` instead of `distance`. For a Postgres-backed index this compiles to a synchronous `tsvector`/GIN query; for an OpenSearch/Meilisearch-backed index, the client performs the HTTP search first and injects the matching IDs/scores as query parameters before the SQL runs.
