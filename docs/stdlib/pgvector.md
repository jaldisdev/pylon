# `pgvector`

Requires the `vector` PostgreSQL extension (`CREATE EXTENSION IF NOT EXISTS vector;`) on the target database — Pylon doesn't auto-provision it; a schema with a [`VectorIndex`](../schema/indexes.md#vectorindex-embeddings) declared gets it enabled automatically by the migration diff engine, but calling these functions directly on a database with no vector column doesn't.

| Function | Signature | Returns | Description |
|---|---|---|---|
| `euclidean_distance` | `(a: vector, b: vector)` | `float64` | L2 (Euclidean) distance — pgvector's `<->` operator. |
| `cosine_distance` | `(a: vector, b: vector)` | `float64` | Cosine distance — pgvector's `<=>` operator. `0` = identical direction. |
| `neg_inner_product` | `(a: vector, b: vector)` | `float64` | Negative inner product — pgvector's `<#>` operator (negated so that, like the other two, a smaller value means "more similar"). |
| `inner_product` | `(a: vector, b: vector)` | `float64` | Plain (non-negated) inner product. |

For an actual similarity search over an indexed property, use [`vector::search`](../pyql/globals-and-functions.md#vectorsearch) rather than calling these directly — they're the raw distance primitives it compiles down to.
