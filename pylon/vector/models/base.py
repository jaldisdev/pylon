from __future__ import annotations

from abc import ABC, abstractmethod


class EmbeddingProvider(ABC):
    """Abstract embedding provider.  Each concrete subclass targets one API."""

    @abstractmethod
    async def embed_batch(self, texts: list[str]) -> list[list[float]]:
        """Return one embedding vector per input text, in the same order."""
        ...

    async def embed(self, text: str) -> list[float]:
        results = await self.embed_batch([text])
        return results[0]
