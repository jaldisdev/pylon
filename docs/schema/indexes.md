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

A deferred embedding index — writes to the indexed pointers enqueue a job; a background worker (the vector-index worker, run by [`pylon-server`](../server.md) — including under `--no-http`, for a worker-only container) generates the embedding via the model configured under `[models.*]` in `pylon.toml` and writes it back. `VectorPointer('TypeName.pointer_name')` — the type prefix is validated against the enclosing type at `finalize()` time. `metric` (`"cosine"` (default), `"euclidean"`, or `"inner_product"`) and `dimensions` (default `1024`) are keyword-only.

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

## Rebuilding after a schema change

Adding a `VectorIndex`, or a `SearchIndex` on a deferred backend, to a type
that already holds objects raises an obvious question: what happens to the
objects that were already there? Only writes enqueue indexing jobs, so without
help an index added today would hold only the objects written tomorrow.

`pylon migration create` handles this. It compares the new schema against the
one recorded on the applied migration tip and, for every deferred index that is
new or whose definition changed, writes an outbox backfill into the migration
file — one indexing job per existing object, drained by the same worker that
drains ordinary writes:

```sql
INSERT INTO _pylon."IndexOutbox"
    (object_id, type_name, index_kind, index_name, operation)
SELECT "id", 'catalog::Product', 'Meilisearch'::_pylon."IndexKind", NULL, 'index'
FROM "catalog"."Product"
ON CONFLICT (object_id, index_kind, index_name)
DO UPDATE SET status = 'Pending', operation = EXCLUDED.operation,
              attempts = 0, next_attempt = NULL, enqueued_at = now();
```

Because it lives in the migration file, every database the migration reaches —
CI, staging, production — rebuilds the same way, and the backfill is visible in
review rather than being an operator's step to remember. `pylon migration watch`
does the same against a dev database.

Changing a `VectorIndex`'s `metric` is the one amendment that does **not**
trigger a rebuild: it only changes the hnsw operator class Postgres builds over
the vectors, not the vectors themselves, so re-embedding would bill the
embedding provider for identical results. Everything else about an index —
pointers, model, dimensions, backend — counts as invalidating.

Rebuilding a vector index re-embeds every object. Pass
`pylon migration create --no-index-backfill` to leave the backfill out and run
it yourself later with `pylon index rebuild`.

### `pylon index rebuild`

For what a migration cannot know about — a search engine wiped, moved, or
replaced out from under the database, jobs dropped by hand, or a migration
created with `--no-index-backfill`:

```console
$ pylon index list
TYPE                                KIND          INDEX           QUEUED
catalog::Product                    Meilisearch   (default)       0

$ pylon index rebuild --type Product --kind search
```

`rebuild` reads the schema as actually applied to the database (not the schema
files, which may declare an index no migration has created yet), shows how many
objects each matching index covers, and asks before enqueueing. `--type`,
`--index` (`default` selects a type's unnamed index), and `--kind`
(`vector`/`search`) narrow the set; `--yes` skips the prompt and the object
counts it needs.

Postgres-backed `SearchIndex`es never appear here. Their `tsvector` column is
generated, so it is recomputed for every object the type holds the moment the
column's definition changes — the migration's `ALTER TABLE` is the whole
rebuild.
