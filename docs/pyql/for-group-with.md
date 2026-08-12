# FOR, GROUP, WITH

## `FOR`

```pyql
for name in {'Alice', 'Bob', 'Carol'} union (
    insert Person { name := name }
)
```

`for [optional] var in <iterator> [union] <body>` — evaluates `body` once per element of `iterator`, collecting the results into one set. `union` is a readability keyword; `for var in iter (stmt)` (no `union`) parses identically. `optional` allows the loop to run once with `var` bound to nothing if `iterator` is empty, instead of producing an empty set.

The body is most often a parenthesized `insert`/`update`/`delete`/`select` — a bulk mutation driven by a set of inputs, the PyQL equivalent of looping over objects client-side and issuing one statement per object, but compiled as a single query.

## `GROUP`

```pyql
group Person using decade := .age // 10 by decade
group Person { name } by .age
```

`group <subject> [{ shape }] [using alias := expr, ...] by expr, ...`. `using` binds one or more named expressions computed per row; `by` names the grouping key(s) — either a `using` alias or a bare property path. An optional shape restricts which fields appear on each grouped element (same shape grammar as `select`).

Each result row decodes to `{"key": {...}, "grouping": [...], "elements": [...]}`:

- `key` — a dict keyed by each `by` name (a `using` alias, or the bare property name when grouping directly by a property), holding that group's key value(s).
- `elements` — the set of shaped objects belonging to that group.
- `grouping` — which key names actually apply to this row (relevant only for advanced multi-level grouping).

## `WITH`

```pyql
with recent := (select Order filter .created_at > <datetime>$since)
select recent { id, total }

with
  insert0 := (insert Post { title := $title }),
  update0 := (update Person filter .id = <uuid>$id set { posts += (select insert0) })
select { insert0, update0 }
```

`with alias := (expr), alias := (expr), ... <main-stmt>` — binds one or more named sub-expressions (each itself a full statement, in parens) usable by name in the main statement and in each other, evaluated in declaration order. This is Pylon's mechanism for composing multiple mutations into one query — the classic pattern above (`insert0` feeds into `update0`, which appends the just-inserted post onto an author's `posts`) does the whole "insert a related row, then link it" sequence as a single round trip instead of two.

A `with`-bound alias behaves like a CTE: referencing it more than once in the main statement reuses the same evaluated result, not a re-run of the sub-expression.
