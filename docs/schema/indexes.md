# Indexes

## `Index` — plain B-tree

```python
@pylon.type
class Product:
    name: str
    last_name: str
    archived_at: pylon.DateTime | None

    pylon.Index("name")                                  # single pointer
    pylon.Index(("last_name", "name"))                    # composite
    pylon.Index("str_lower(.name)")                        # PyQL expression index
    pylon.Index("name", unless=".archived_at")             # partial index
```

A non-unique index declared as a class-body expression — `Index(pointer, *, unless=None)`. `pointer` is either a bare pointer name, a tuple for a composite index, or a PyQL expression string (detected automatically — anything containing `(`, `.`, a space, or an operator character is treated as an expression rather than a bare name). `unless=` makes it partial, only indexing objects where the given expression is false/null. **Uniqueness is always expressed via [`Exclusive`](constraints.md#exclusive), never via `Index`** — `Index` never enforces anything, only accelerates lookups.

## `VectorIndex` — embeddings

```python
@pylon.type
class Product:
    name: str
    description: str

    pylon.VectorIndex(
        pointers=[pylon.VectorPointer("Product.name"), pylon.VectorPointer("Product.description")],
        model="mistral-embed",
    )

    # Named (so it can be targeted specifically in a query or by index name):
    summary_index = pylon.VectorIndex(
        pointers=[pylon.VectorPointer("Product.name")],
        model="mistral-embed",
    )
```

A deferred embedding index — writes to the indexed pointers enqueue a job; a background worker (the vector-index worker, run via [`pylon worker start`](../cli.md#pylon-worker) or in-process by [`pylon-server`](../server.md)) generates the embedding via the model configured under `[models.*]` in `pylon.toml` and writes it back. `VectorPointer('TypeName.pointer_name')` — the type prefix is validated against the enclosing type at `finalize()` time. `metric` (`"cosine"` (default), `"euclidean"`, or `"inner_product"`) and `dimensions` (default `1024`) are keyword-only.

Query it via [`vector::search`](../pyql/globals-and-functions.md) in PyQL.

## `SearchIndex` — full-text search

```python
@pylon.type
class Product:
    name: str
    description: str | None

    pylon.SearchIndex(
        backend=pylon.SearchBackend.Postgres,
        pointers=[
            pylon.SearchPointer("Product.name", weight_category=pylon.SearchWeight.A),
            pylon.SearchPointer("Product.description", weight_category=pylon.SearchWeight.B),
        ],
    )
```

Two backends behind one PyQL query interface (`fts::search` — see [Globals and functions](../pyql/globals-and-functions.md)):

| `SearchBackend` | Write path | Requires |
|---|---|---|
| `Postgres` | Synchronous — a generated `tsvector` column + GIN index. | Nothing extra. |
| `OpenSearch` | Deferred — outbox-driven, asynchronous, drained by the search-index worker. | `[search]` in `pylon.toml`. |
| `Meilisearch` | Same deferred shape as OpenSearch. | `[search]` in `pylon.toml`. |

`SearchPointer('TypeName.pointer_name', weight_category=...)` — `weight_category` is one of `SearchWeight.A`/`B`/`C`/`D` (Postgres's own `tsvector` ranking weight tiers, `A` highest). Like `VectorIndex`, assign to a class attribute (`typeahead = pylon.SearchIndex(...)`) to give it a name instead of leaving it anonymous.
