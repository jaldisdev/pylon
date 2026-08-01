# Schema

Pylon's schema is ordinary Python: classes decorated with `@pylon.type` (and friends), pointer annotations like `Property[...]`/`Link[...]`, and module-level declarations for functions, globals, and aliases. There is no separate declarative text format to keep in sync — the Python *is* the schema.

## The pieces

- [Modules](modules.md) — how a schema file maps onto a PostgreSQL schema
- [Object types](types.md) — `@pylon.type`, `@pylon.abstract`, `@pylon.interface`, inheritance, junctions
- [Properties and scalars](properties-and-scalars.md) — `Property[]`, built-in and custom scalars, enums, named tuples, structural tuples/arrays
- [Links](links.md) — `Link`, `MultiLink`, junction tables, deletion policies, link properties
- [Computed pointers](computed.md) — `Computed[]`
- [Constraints](constraints.md) — `Default`, `Exclusive`, `Expression`, value/length bounds, `Readonly`
- [Indexes](indexes.md) — `Index`, `VectorIndex`, `SearchIndex`
- [Functions](functions.md) — `@pylon.function`
- [Triggers and rewrites](triggers-and-rewrites.md) — `Trigger`, `Rewrite`
- [Globals and aliases](globals-and-aliases.md) — `Global`, `Alias`
- [Signals](signals.md) — `@pylon.signal`
- [Validation](validation.md) — everything `pylon.finalize()` checks before your schema is considered valid

## How a schema becomes real

```python
import pylon
pylon.finalize()
```

`pylon.finalize()` (`pylon/_finalize.py`) is the one call that turns Python source into a validated, queryable schema:

1. Reads `pylon.toml`, walking up from the current directory unless `config=` is given.
2. Imports every `.py` file directly under `[project] schema-dir` that doesn't start with `_` — each becomes one Pylon module (see [Modules](modules.md)). Decorators (`@pylon.type`, `@pylon.function`, ...) register themselves into a process-wide registry as each file executes; nothing is built yet at this point.
3. Walks the registry (`pylon.schema._walker.walk()`): resolves forward references, flattens `@pylon.abstract` inheritance, checks for duplicate names/cycles/dangling links/interface conformance, and builds the low-level descriptor objects the Rust compiler operates on.
4. Runs a second pass (`crates/pylon-core/src/validate.rs`) that actually *compiles* every function body, computed-pointer expression, default, rewrite handler, trigger handler, alias, and computed global — checking that each one's declared type (where it has one) matches what its PyQL body actually produces. See [Validation](validation.md) for the full list of what this catches.
5. Installs the result as the process-level schema singleton (what `Client` queries compile against) and returns it.

Any failure at any of these steps raises `pylon.exceptions.SchemaError` (or a real `PylonError` subclass with a rendered, positioned message for a genuine PyQL syntax/type error inside a schema body) — nothing partially-broken is ever installed as the active schema.

`pylon migration create`/`apply`/`watch` all call an equivalent reload internally each time they run, so schema errors surface immediately on the next CLI invocation too, not just at application startup.
