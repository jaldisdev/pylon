# Object types

Three decorators declare an object type. All three take the same `module=`/`name=` keywords (see [Modules](modules.md)) and can be used bare (`@pylon.type`) or with arguments (`@pylon.type(module="shop")`).

| Decorator | Backing table? | Fields inherited via Python subclassing? | Use for |
|---|---|---|---|
| `@pylon.type` | Yes | Yes | An ordinary, queryable object type. |
| `@pylon.abstract` | No | Yes (flattened into every subtype) | Shared fields/constraints with no identity of its own — a mixin. |
| `@pylon.interface` | A PostgreSQL **view**, not a table | Yes (flattened, same as abstract) | A shared, polymorphically-queryable surface across otherwise-unrelated concrete types. |

> **Don't assume "abstract" means polymorphically queryable** — in Pylon it doesn't. `@pylon.abstract` is a plain mixin: its fields flatten into subtypes at schema-build time, and it has no identity, no table, and no query surface of its own — you can never `select` an abstract type directly. `@pylon.interface` is the one with a polymorphic query surface: still no table of its own, but backed by a generated view so you can query it directly and get every concrete implementor's matching objects back, tagged by type. If a shared field or constraint should also be independently queryable across its implementors, it needs `@pylon.interface`, not `@pylon.abstract`.

## `@pylon.type`

```python
import pylon

@pylon.type
class Product:
    name: str
    price: pylon.Property[pylon.Decimal, pylon.MinValue(0)]
```

Every concrete type gets an `id: uuid` primary key automatically (generated via `uuidv7()` server-side unless you supply your own `Default(...)`) — never declare `id` yourself. See [Properties and scalars](properties-and-scalars.md), [Links](links.md), [Constraints](constraints.md) for what else goes in the class body.

## `@pylon.abstract` — mixin

```python
@pylon.abstract
class Auditable:
    created_at: pylon.Property[pylon.DateTime, pylon.Default(pylon.Now)]
    updated_at: pylon.Property[pylon.DateTime, pylon.Default(pylon.Now)]

@pylon.type
class Post(Auditable):
    title: str
```

`Post` gets `created_at`/`updated_at` flattened directly onto its own table — `Auditable` produces no table, no view, no queryable identity at all. Constraints, indexes, and triggers declared on an abstract parent propagate the same way. An own field on the subtype shadows an inherited one of the same name.

## `@pylon.interface` — polymorphic view

```python
@pylon.interface
class Publishable:
    published_at: pylon.Property[pylon.DateTime] | None

@pylon.type
class Post(Publishable):
    title: str

@pylon.type
class Video(Publishable):
    url: str
```

`Publishable` is materialized as a PostgreSQL view unioning every concrete type that inherits from it — `select Publishable` returns objects from both `Post` and `Video`, each still carrying its own concrete type tag (readable in PyQL via the [`is`](../pyql/operators.md) operator: `select Publishable filter Publishable is Post`).

**Conformance is checked, not assumed**: every concrete subtype must actually declare (or inherit) a pointer matching each of the interface's own pointers, by name and by *kind* — a property can't satisfy a link-shaped interface pointer, and vice versa. In practice this falls out naturally from ordinary Python inheritance (subclassing `Publishable` already gives `Post` a `published_at` property via the same field-flattening abstract types use) — the check exists to catch a mismatch, e.g. a subtype that shadows `published_at` with an incompatible kind.

## Inheriting from more than one parent

Python's own multiple inheritance works — a type can combine several abstract mixins and/or interfaces:

```python
@pylon.type
class Post(Auditable, Publishable):
    title: str
```

Field/constraint/index/trigger flattening walks the full MRO, most-distant ancestor first, so a more-specific declaration always shadows a less-specific one of the same name.

## Junctions: `@pylon.junction`

A junction type holds extra properties on a many-to-many relationship (see [Links](links.md#multilink-one-to-many-many-to-many) for the full `MultiLink[..., Through[...]]` picture):

```python
@pylon.junction
class ProductTag:
    weight: pylon.Property[pylon.Float64, pylon.MinValue(0)]
    added_at: pylon.Property[pylon.DateTime, pylon.Default(pylon.Now)]

@pylon.type
class Product:
    tags: pylon.MultiLink[Tag, pylon.Through[ProductTag]]
```

Rules, enforced at `finalize()` time:

- A junction type may only declare scalar properties — no links, multilinks, or nested junctions.
- `source` and `target` are reserved names on a junction type (the generated table's own FK columns).
- Each junction type must be referenced by **exactly one** link or multi-link's `Through[...]` — reusing one junction type across two different relationships is rejected.
- The junction table's actual name derives from the `MultiLink` that references it, not from the junction class's own name.

## Table naming

The generated table name is the type's own name (PascalCase preserved) inside the schema its module maps to — `shop::Product` → `"shop"."Product"`. Override with `table="..."` on the decorator if you need a specific name (e.g. matching a pre-existing table during a migration off another system).
