# Validation

Everything on this page runs at `pylon.finalize()` time — before your schema is ever installed as the active singleton, and well before any migration or query would otherwise be the first thing to notice a problem. A failure anywhere raises `pylon.exceptions.SchemaError` (or a real, positioned `PylonError` subclass for a genuine PyQL syntax/type error found inside a schema body).

## Structural checks

Run first, by `pylon.schema._walker.walk()`:

- **Duplicate names** — two types, or two functions with an identical `(module, name, param_types)` signature, sharing a qualified name.
- **Dangling references** — a `Link`/`MultiLink` target, an interface, a `Through[...]` junction, that doesn't resolve to a real declared type.
- **Required-link cycles** — a cycle of non-nullable links, which would make `INSERT` impossible on either side. See [Links](links.md#required-link-cycles-are-rejected).
- **Interface conformance** — every concrete subtype of an [`@pylon.interface`](types.md) actually declares (or inherits) a matching pointer, by name and kind, for each of the interface's own pointers.
- **Junction rules** — a `@pylon.junction` type has only scalar properties, doesn't use `source`/`target` as property names, and is referenced by exactly one link/multi-link. See [Object types § Junctions](types.md#junctions-pylonjunction).
- **Array/tuple element types must be scalar** — an object type used inside `Array[...]`/`Tuple[...]`, at any nesting depth, is rejected rather than silently degrading to an opaque, unreferenceable `jsonb`/`text[]` column. See [Properties and scalars](properties-and-scalars.md#structural-tuples-and-arrays).
- **Function declaration hygiene** — `volatility=` must be a real `Volatility` member; `language=` must be `Language.PyQL` (the only one currently supported); every parameter and the return type need a real annotation. See [Functions](functions.md).

## Type-consistency checks

Run second, by `crates/pylon-core/src/validate.rs` — each of these actually *compiles* the relevant PyQL body and compares its inferred return type against what's declared:

| Construct | Declared type compared against |
|---|---|
| [Function](functions.md) body | The function's own return-type annotation |
| [Computed pointer](computed.md) expression | `Computed[return_type, ...]`'s `return_type` |
| Property/link [`Default`](constraints.md#default) expression | The property's own type (a link's default must produce `uuid`) |
| [`Rewrite`](triggers-and-rewrites.md#rewrite) handler | The owning property's own type |

All mismatches across the whole schema are collected and reported together in one `SchemaError`, not one at a time — you don't get a fix-one-rerun loop.

A body that doesn't **compile** is an error here too, not a skip. That matters most for defaults: nothing downstream re-reports one, so a `Default` naming a function that doesn't exist used to reach Postgres as a column with no default at all, failing on the first insert a long way from the declaration that caused it.

```python
uuid: Property[pylon.UUID, Default('std::uuid_generate_v7j()')]
# SchemaError: default for 'default::T.uuid (default)': function
# 'std::uuid_generate_v7j' does not exist — did you mean std::uuid_generate_v7()?
```

### Cardinality

A pointer declared with a single scalar type promises one value per row. Two expressions break that promise and are rejected:

- a path that crosses a [multi-link](links.md), which yields an array;
- a call to a `set[...]`-returning [function](functions.md), which Postgres expands into rows.

```python
tag_names: Computed[pylon.Str, '.tags.label']         # rejected — yields many
tag_names: Computed[pylon.Array[pylon.Str], '.tags.label']   # fine
```

Declaring the array type is the fix when you want the many values; `limit 1`, `std::assert_single()`, or an aggregate is the fix when you want one.

Only the top of the expression is examined (looking through a cast or a `coalesce`), which covers a pointer whose whole body is the offending expression. One buried inside a larger expression isn't caught.

### Function calls are resolved, not guessed

A call inside any of these bodies has to name a real overload. Previously a call that matched none of them resolved to whichever overload happened to be registered first, and only failed once Postgres saw a signature nobody wrote:

```python
Computed[pylon.Str, 'std::str_lower(.a, .b)']
# SchemaError: function 'std::str_lower' takes 1 argument(s), got 2

Computed[pylon.Str, 'std::str_lower(.count)']   # .count is an int64
# SchemaError: function 'std::str_lower' has no overload accepting (int8)
#              — it accepts (str)
```

Argument types are judged by the same implicit-cast graph the compiler resolves operators against, so an int reaches a `float64` parameter as it always has. An overload is only chosen on evidence, though: if an argument's type can't be inferred, the call is reported rather than resolved to a guess.

### Type inference is still not exhaustive

Return-type comparison only fires when the checker can actually type the expression. It recognizes a bare column/property reference, an explicit cast, a literal, an enum member, a named-tuple construction, a function parameter, a global parameter, a slice, arithmetic, and any function call it resolved to a known scalar return type. Anything else — an `if/else`, for instance — is **silently skipped, not rejected**.

This keeps the pass from false-positive rejections on legitimate but more complex expressions. It's a floor, not a guarantee that every mismatch is caught.

## Compile-only checks

A few constructs have no single declared scalar type to compare against — a trigger handler is void, an alias or computed global can select any shape, not just a scalar — but their PyQL bodies are still compiled eagerly at `finalize()` time (syntax/resolution errors surface immediately) rather than staying uncompiled until something happens to invoke them:

- [`Trigger`](triggers-and-rewrites.md#trigger) handlers
- [`Alias`](globals-and-aliases.md#alias) bodies
- Computed [`Global`](globals-and-aliases.md#global) expressions

Before this, each of these only ever got compiled the first time something actually exercised it — a query referencing the alias, an actual mutation firing the trigger, `export_schema` emitting the trigger's DDL — so a broken one could sit undetected in an otherwise-valid schema indefinitely.

## What isn't checked

- A **computed global**'s declared type isn't compared against its expression's actual output — only its compile-validity is checked (see above). Its declared type is a PyQL type-name string for client-side typing, not a Postgres type the compiler's `infer_ir_type`/`types_compatible` machinery (built around Postgres type strings) can currently bridge to safely.
- An **object-valued computed pointer** — one whose expression is a [sub-select over a link](computed.md#sub-selects), like `"(select .orders limit 5)"` — has no scalar type to compare its declared `MultiLink[...]`/`Link[...]` against, so only its compile-validity is checked. A computed that *projects* a property off such a sub-select (`"(select .orders limit 1).total"`) is scalar and is checked normally.
- A **link-level `Rewrite`** isn't checked at all — nothing compiles it, because nothing in the compiler reads `LinkDescriptor.rewrites` yet regardless of validation. See the caveat in [Triggers and rewrites](triggers-and-rewrites.md#rewrite).
- Anything the type inference (above) can't classify is skipped, not flagged — a wrong return type hidden behind an `if/else` won't be caught by this pass.
