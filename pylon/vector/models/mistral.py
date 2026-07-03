from __future__ import annotations

import os
from typing import Any

from .base import EmbeddingProvider

MODEL = "mistral-embed"
DIMENSIONS = 1024
MAX_BATCH = 128


class MistralProvider(EmbeddingProvider):
    """Mistral AI embedding provider (``mistral-embed``, 1024 dimensions).

    Requires ``mistralai`` package and ``MISTRAL_API_KEY`` env var (or pass
    ``api_key`` explicitly).
    """

    def __init__(self, *, api_key: str | None = None, model: str = MODEL) -> None:
        try:
            from mistralai import Mistral  # type: ignore[import-untyped]
        except ImportError as e:
            raise ImportError(
                "Install the 'mistralai' package to use MistralProvider: "
                "pip install mistralai"
            ) from e
        self._client: Any = Mistral(api_key=api_key or os.environ["MISTRAL_API_KEY"])
        self._model = model

    async def embed_batch(self, texts: list[str]) -> list[list[float]]:
        results: list[list[float]] = []
        for i in range(0, len(texts), MAX_BATCH):
            chunk = texts[i : i + MAX_BATCH]
            response = await self._client.embeddings.create_async(
                model=self._model,
                inputs=chunk,
            )
            results.extend(item.embedding for item in response.data)
        return results
