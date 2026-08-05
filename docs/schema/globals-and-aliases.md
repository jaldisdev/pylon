# Globals and aliases

Both are module-level declarations (not inside a class body) — annotate a module-level name in a schema file, the same way you'd annotate a variable with a type.

## `Global`

```python
# dbschema/shop.py
import pylon

current_user_id: pylon.Global[pylon.UUID]                    # required session global
current_user_id: pylon.Global[pylon.UUID | None]             # optional session global
current_user: pylon.Global[
    pylon.UUID | None,
    "select User.id filter User.email = global current_user_email",
]
```

Two kinds, distinguished by whether a PyQL expression is given as the second type-subscript argument:

- **Session global** — no expression. Its value is injected per-request from the client (`client.with_globals({"module::name": value})` — see [Python client § with_globals](../client/python.md#with_globals)) or, in the REPL, via `set global name := expr;`. Referenced in PyQL as `global name`.
- **Computed global** — a PyQL expression evaluated at query time whenever `global name` is referenced; can select anything, not just a scalar (the example above selects a `User`'s `id`). Not injected by the client at all — there's nothing to inject, the expression *is* the value.

`Global[T]`/`Global[T | None]` follow the same optional-via-union convention as `Property`. A computed global's declared type isn't checked against its expression's actual output today — see the note in [Validation](validation.md#what-isnt-checked).

Compiled eagerly at `finalize()` time (syntax/resolution errors surface immediately, the same as a trigger handler) regardless of whether any query has referenced it yet — see [Validation](validation.md).

## `Alias`

```python
# dbschema/shop.py
import pylon

published_posts: pylon.Alias["select Post filter .is_published = true"]
```

A named, reusable PyQL query fragment — referencing `published_posts` in a query is equivalent to inlining the aliased expression, with the outer query's own shape/filter/modifiers merged on top:

```pyql
select published_posts { title }
select published_posts filter .author.name = "Ada"
```

An alias has no declared return type (unlike a computed global) — it can select any shape a plain `select` could. Like a computed global, its body is compiled eagerly at `finalize()` time so a broken alias is caught before anything queries it, not on first use.
