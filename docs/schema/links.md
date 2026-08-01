# Links

## `Link` — single reference

```python
@pylon.type
class Product:
    category: pylon.Link[Category]
    supplier: pylon.Link[Supplier] | None            # optional (nullable FK)
```

Backed by a plain `<name>_id uuid` foreign-key column on the source table. Required by default (`NOT NULL`); make it optional with `| None`.

## `MultiLink` — one-to-many / many-to-many

```python
@pylon.type
class Product:
    tags: pylon.MultiLink[Tag]
```

With no `Through[...]`, Pylon generates an implicit junction table for you (two-column, source/target FK pair). To attach extra properties to the relationship itself, back it with an explicit [`@pylon.junction`](types.md#junctions-pylonjunction) type instead:

```python
@pylon.junction
class ProductTag:
    weight: pylon.Property[pylon.Float64, pylon.MinValue(0)]

@pylon.type
class Product:
    tags: pylon.MultiLink[Tag, pylon.Through[ProductTag]]
```

`Through[...]` works on a single `Link` too, for a one-to-one relationship that itself needs properties:

```python
spouse: pylon.Link[Person, pylon.Through[Marriage]] | None
```

See [Object types § Junctions](types.md#junctions-pylonjunction) for the junction-type rules (scalar properties only, `source`/`target` reserved, exactly one referencing link).

### Link properties in PyQL

A junction-backed link's own properties are reachable in a query shape via `@propname`:

```pyql
select Product { name, tags: { name, @weight } }
```

## Deletion policies: `OnDelete`

```python
from pylon.schema import OnDelete, Target, Source, Restrict, DeferredRestrict, Allow, DeleteSource, DeleteTarget, DeleteTargetIfOrphan

chat: pylon.Link[MessageThread, pylon.OnDelete(Target, DeleteSource)]
messages: pylon.MultiLink[Message, pylon.OnDelete(Source, DeleteTargetIfOrphan)]
```

`OnDelete(side, action)` — `side` is `Target` (what happens to *this* link/pointer when the referenced row is deleted) or `Source` (what happens when the owning row itself is deleted). A link/multilink can declare a policy for either side, both, or neither.

| Action | Meaning |
|---|---|
| `Allow` | Deletion proceeds; the reference is simply cleared/removed. |
| `Restrict` | Block the delete outright while a reference exists (an immediate, non-deferred check). |
| `DeferredRestrict` | Same block, checked at transaction commit instead of immediately — allows a same-transaction reorder (e.g. delete both sides of a pair together) that an immediate `Restrict` would reject mid-transaction. |
| `DeleteSource` | Deleting the target cascades to delete the row(s) referencing it. |
| `DeleteTarget` | Deleting the source cascades to delete the row(s) it references. |
| `DeleteTargetIfOrphan` | Like `DeleteTarget`, but only if no other row still references that target. |

With no `OnDelete` declared at all, a `Link`'s target defaults to a plain foreign key with Postgres's own default behavior — deleting a still-referenced row is blocked. Declare `OnDelete(Target, Allow)` explicitly if you want the reference silently cleared instead.

## Required-link cycles are rejected

A cycle of *required* (non-nullable) links — `A.b` required-links to `B`, `B.a` required-links back to `A` — makes a first `INSERT` impossible (each side needs the other to already exist) and is rejected at `pylon.finalize()` time. Break the cycle by making at least one side of it nullable, or restructure through a linking type.

## Cross-module links

A link can target a type in a different [module](modules.md) freely — Pylon doesn't require the source and target to share a PostgreSQL schema.
