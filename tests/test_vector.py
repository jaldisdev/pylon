"""Tests for the vector index pipeline.

Covers:
  - VectorPointer / VectorIndex construction
  - Walker pointer resolution (_resolve_vector_pointer, _make_vector_index_desc)
  - OpenAIProvider and AnthropicProvider (mocked httpx)
  - IndexWorker drain lock
  - VectorIndexWorker.process_batch (mocked asyncpg + compile_index_fetch)
"""
import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

import pylon.schema as pylon
from pylon.schema._indexes import VectorPointer, VectorIndex
from pylon.schema._registry import clear as clear_registry, snapshot
from pylon.schema._walker import (
    SchemaError,
    _make_vector_index_desc,
    _resolve_vector_pointer,
)


# ── helpers ───────────────────────────────────────────────────────────────────


def run(coro):
    return asyncio.run(coro)


@pytest.fixture(autouse=True)
def _isolated_registry():
    before = snapshot()
    yield
    clear_registry()
    types, enums, scalars = before
    from pylon.schema._registry import register_type, register_enum, register_scalar
    for t in types:
        register_type(t)
    for e in enums:
        register_enum(e)
    for s in scalars:
        register_scalar(s)


# ── VectorPointer ───────────────────────────────────────────────────────────────


class TestVectorPointer:
    def test_stores_ref(self):
        assert VectorPointer("Product.name").ref == "Product.name"

    def test_plain_pointer_name(self):
        assert VectorPointer("name").ref == "name"

    def test_repr(self):
        assert repr(VectorPointer("Product.name")) == "VectorPointer('Product.name')"


# ── VectorIndex ───────────────────────────────────────────────────────────────


class TestVectorIndex:
    def test_stores_vector_pointers(self):
        vi = VectorIndex(
            pointers=[VectorPointer("Product.name"), VectorPointer("Product.description")],
            model="mistral-embed",
        )
        assert len(vi._vector_pointers) == 2
        assert vi._vector_pointers[0].ref == "Product.name"
        assert vi._vector_pointers[1].ref == "Product.description"

    def test_defaults(self):
        vi = VectorIndex(pointers=[VectorPointer("Product.name")], model="mistral-embed")
        assert vi.metric == "cosine"
        assert vi.dimensions == 1024
        assert vi.index_name is None

    def test_named_via_set_name(self):
        vi = VectorIndex(pointers=[VectorPointer("Product.name")], model="mistral-embed")
        vi.__set_name__(None, "summary_index")
        assert vi.index_name == "summary_index"

    def test_custom_metric_and_dimensions(self):
        vi = VectorIndex(
            pointers=[VectorPointer("Product.name")],
            model="text-embedding-3-small",
            metric="euclidean",
            dimensions=512,
        )
        assert vi.metric == "euclidean"
        assert vi.dimensions == 512


# ── Pointer resolution ──────────────────────────────────────────────────────────


class TestResolveVectorPointer:
    def test_qualified_ref(self):
        result = _resolve_vector_pointer("Product.name", "Product", {"name", "description"})
        assert result == "name"

    def test_plain_ref(self):
        result = _resolve_vector_pointer("name", "Product", {"name", "description"})
        assert result == "name"

    def test_wrong_type_prefix_raises(self):
        with pytest.raises(SchemaError, match="does not match"):
            _resolve_vector_pointer("Order.name", "Product", {"name"})

    def test_unknown_qualified_pointer_raises(self):
        with pytest.raises(SchemaError, match="not found"):
            _resolve_vector_pointer("Product.missing", "Product", {"name", "description"})

    def test_unknown_plain_pointer_raises(self):
        with pytest.raises(SchemaError, match="not found"):
            _resolve_vector_pointer("missing", "Product", {"name"})


class TestMakeVectorIndexDesc:
    def test_resolves_pointers_and_passes_to_core(self):
        core = MagicMock()
        vi = VectorIndex(
            pointers=[VectorPointer("Product.name"), VectorPointer("Product.description")],
            model="mistral-embed",
        )
        _make_vector_index_desc(vi, core, "Product", {"name", "description"})
        core.VectorIndexDescriptor.assert_called_once_with(
            pointers=["name", "description"],
            model="mistral-embed",
            metric="cosine",
            dimensions=1024,
            index_name=None,
        )

    def test_plain_refs_resolved(self):
        core = MagicMock()
        vi = VectorIndex(pointers=[VectorPointer("name")], model="mistral-embed")
        _make_vector_index_desc(vi, core, "Product", {"name"})
        core.VectorIndexDescriptor.assert_called_once()
        assert core.VectorIndexDescriptor.call_args.kwargs["pointers"] == ["name"]

    def test_bad_ref_raises_schema_error(self):
        core = MagicMock()
        vi = VectorIndex(pointers=[VectorPointer("Product.nonexistent")], model="mistral-embed")
        with pytest.raises(SchemaError):
            _make_vector_index_desc(vi, core, "Product", {"name"})

    def test_named_index_passed_through(self):
        core = MagicMock()
        vi = VectorIndex(pointers=[VectorPointer("Product.name")], model="mistral-embed")
        vi.__set_name__(None, "my_index")
        _make_vector_index_desc(vi, core, "Product", {"name"})
        assert core.VectorIndexDescriptor.call_args.kwargs["index_name"] == "my_index"


# ── OpenAIProvider ───────────────────────────────────────────────────


class TestOpenAIProvider:
    def _make_provider(self, json_responses: list):
        from pylon.vector.models.openai import OpenAIProvider
        provider = OpenAIProvider(
            api_url="https://api.example.com/v1",
            model="test-model",
            api_key="test-key",
        )
        mock_response = MagicMock()
        mock_response.raise_for_status = MagicMock()
        mock_response.json = MagicMock(side_effect=json_responses)
        mock_client = AsyncMock()
        mock_client.post.return_value = mock_response
        provider._client = mock_client
        return provider, mock_client

    def test_embed_batch_returns_embeddings(self):
        resp = {"data": [{"index": 0, "embedding": [0.1, 0.2]}, {"index": 1, "embedding": [0.3, 0.4]}]}
        provider, mock_client = self._make_provider([resp])
        result = run(provider.embed_batch(["hello", "world"]))
        assert result == [[0.1, 0.2], [0.3, 0.4]]
        mock_client.post.assert_awaited_once_with(
            "/embeddings",
            json={"model": "test-model", "input": ["hello", "world"]},
        )

    def test_embed_batch_sorts_by_index(self):
        resp = {"data": [{"index": 1, "embedding": [0.3, 0.4]}, {"index": 0, "embedding": [0.1, 0.2]}]}
        provider, _ = self._make_provider([resp])
        result = run(provider.embed_batch(["a", "b"]))
        assert result == [[0.1, 0.2], [0.3, 0.4]]

    def test_embed_batch_splits_into_chunks(self):
        from pylon.vector.models.openai import MAX_BATCH
        texts = [f"t{i}" for i in range(MAX_BATCH + 1)]
        resp1 = {"data": [{"index": i, "embedding": [float(i)]} for i in range(MAX_BATCH)]}
        resp2 = {"data": [{"index": 0, "embedding": [float(MAX_BATCH)]}]}
        provider, mock_client = self._make_provider([resp1, resp2])
        result = run(provider.embed_batch(texts))
        assert len(result) == MAX_BATCH + 1
        assert mock_client.post.await_count == 2

    def test_uses_bearer_auth(self):
        from pylon.vector.models.openai import OpenAIProvider
        with patch("httpx.AsyncClient") as mock_cls:
            OpenAIProvider(api_url="https://api.example.com/v1", model="m", api_key="sk-123")
            headers = mock_cls.call_args.kwargs["headers"]
            assert headers["Authorization"] == "Bearer sk-123"

    def test_http_error_propagates(self):
        import httpx
        from pylon.vector.models.openai import OpenAIProvider
        provider = OpenAIProvider(
            api_url="https://api.example.com/v1", model="m", api_key="k"
        )
        mock_resp = MagicMock()
        mock_resp.raise_for_status.side_effect = httpx.HTTPStatusError(
            "401", request=MagicMock(), response=MagicMock()
        )
        mock_client = AsyncMock()
        mock_client.post.return_value = mock_resp
        provider._client = mock_client
        with pytest.raises(httpx.HTTPStatusError):
            run(provider.embed_batch(["text"]))


# ── AnthropicProvider ────────────────────────────────────────────────


class TestAnthropicProvider:
    def test_sets_x_api_key_header(self):
        from pylon.vector.models.anthropic import AnthropicProvider
        with patch("httpx.AsyncClient") as mock_cls:
            AnthropicProvider(api_url="https://api.example.com", model="m", api_key="sk-ant")
            headers = mock_cls.call_args.kwargs["headers"]
            assert headers["x-api-key"] == "sk-ant"

    def test_sets_anthropic_version_header(self):
        from pylon.vector.models.anthropic import AnthropicProvider
        with patch("httpx.AsyncClient") as mock_cls:
            AnthropicProvider(api_url="https://api.example.com", model="m", api_key="k")
            headers = mock_cls.call_args.kwargs["headers"]
            assert "anthropic-version" in headers

    def test_no_bearer_auth(self):
        from pylon.vector.models.anthropic import AnthropicProvider
        with patch("httpx.AsyncClient") as mock_cls:
            AnthropicProvider(api_url="https://api.example.com", model="m", api_key="k")
            headers = mock_cls.call_args.kwargs["headers"]
            assert "Authorization" not in headers


# ── IndexWorker drain lock ────────────────────────────────────────────────────


class TestIndexWorkerDrainLock:
    def _make_worker(self):
        from pylon.worker import IndexKind, IndexWorker

        class CountingWorker(IndexWorker):
            index_kind = IndexKind.VECTOR
            claim_calls = 0

            async def claim_batch(self, limit):
                self.claim_calls += 1
                return []

            async def process_batch(self, rows):
                pass

        return CountingWorker(AsyncMock())

    def test_skips_when_lock_held(self):
        worker = self._make_worker()

        async def go():
            async with worker._drain_lock:
                await worker._drain()
            return worker.claim_calls

        assert run(go()) == 0

    def test_runs_when_unlocked(self):
        worker = self._make_worker()
        run(worker._drain())
        assert worker.claim_calls == 1


# ── VectorIndexWorker ─────────────────────────────────────────────────────────


def _make_schema_mock(type_name: str, index_name, col_name: str, table: str):
    module, name = type_name.split("::")
    vi = MagicMock()
    vi.index_name = index_name
    vi.column_name = col_name
    td = MagicMock()
    td.module = module
    td.name = name
    td.table = table
    td.vector_indexes = [vi]
    schema = MagicMock()
    schema.types = [td]
    return schema


class TestVectorIndexWorker:
    def _make_worker(self, type_name="default::Product", index_name=None):
        from pylon.vector.sync import VectorIndexWorker
        schema = _make_schema_mock(type_name, index_name, "__vector__", "Product")
        provider = AsyncMock()
        provider.embed_batch.return_value = [[0.1, 0.2, 0.3]]
        conn = AsyncMock()
        conn.fetch.return_value = [{"id": "uuid-1", "source_text": "Wireless Headphones"}]
        with patch("pylon.vector.sync.compile_index_fetch", return_value="SELECT ..."):
            worker = VectorIndexWorker(
                conn,
                schema=schema,
                providers={(type_name, index_name): provider},
            )
        return worker, conn, provider

    def test_process_batch_embeds_and_writes(self):
        worker, conn, provider = self._make_worker()
        rows = [{"type_name": "default::Product", "index_name": None, "object_id": "uuid-1"}]
        run(worker.process_batch(rows))
        provider.embed_batch.assert_awaited_once_with(["Wireless Headphones"])
        conn.executemany.assert_awaited_once()
        _, batch = conn.executemany.call_args[0]
        assert batch[0][0] == "uuid-1"
        assert batch[0][1] == "[0.1,0.2,0.3]"

    def test_missing_provider_skips_fetch(self):
        worker, conn, _ = self._make_worker()
        rows = [{"type_name": "default::Other", "index_name": None, "object_id": "uuid-1"}]
        run(worker.process_batch(rows))
        conn.fetch.assert_not_awaited()

    def test_empty_fetch_result_skips_embed(self):
        worker, conn, provider = self._make_worker()
        conn.fetch.return_value = []
        rows = [{"type_name": "default::Product", "index_name": None, "object_id": "uuid-1"}]
        run(worker.process_batch(rows))
        provider.embed_batch.assert_not_awaited()
