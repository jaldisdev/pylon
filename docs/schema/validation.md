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

### Best-effort, not exhaustive

Type inference over a compiled PyQL expression only recognizes a specific set of shapes: a bare column/property reference, an explicit cast, a literal, an enum member reference, a named-tuple construction, a function parameter, and a global parameter. Anything more complex — a binary operation, a function call, an `if/else` — is **silently skipped, not rejected**, since the checker can't determine what type it produces:

```python
@pylon.function
def helper() -> pylon.Str:
    """str_lower('X')"""
# not rejected even though nothing here actually confirms str_lower's return
# type against Str — the checker can't see through a FunctionCall today
```

This means the checks catch every simple, common case (which is most real schemas) without risking a false-positive rejection on a legitimate but more complex expression. It's a strictly additive floor, not a guarantee that every mismatch is caught.

## Compile-only checks

A few constructs have no single declared scalar type to compare against — a trigger handler is void, an alias or computed global can select any shape, not just a scalar — but their PyQL bodies are still compiled eagerly at `finalize()` time (syntax/resolution errors surface immediately) rather than staying uncompiled until something happens to invoke them:

- [`Trigger`](triggers-and-rewrites.md#trigger) handlers
- [`Alias`](globals-and-aliases.md#alias) bodies
- Computed [`Global`](globals-and-aliases.md#global) expressions

Before this, each of these only ever got compiled the first time something actually exercised it — a query referencing the alias, an actual mutation firing the trigger, `export_schema` emitting the trigger's DDL — so a broken one could sit undetected in an otherwise-valid schema indefinitely.

## What isn't checked

- A **computed global**'s declared type isn't compared against its expression's actual output — only its compile-validity is checked (see above). Its declared type is a PyQL type-name string for client-side typing, not a Postgres type the compiler's `infer_ir_type`/`types_compatible` machinery (built around Postgres type strings) can currently bridge to safely.
- A **link-level `Rewrite`** isn't checked at all — nothing compiles it, because nothing in the compiler reads `LinkDescriptor.rewrites` yet regardless of validation. See the caveat in [Triggers and rewrites](triggers-and-rewrites.md#rewrite).
- Anything the best-effort type inference (above) can't classify is skipped, not flagged — a wrong return type hidden behind a function call or binary operation won't be caught by this pass.
