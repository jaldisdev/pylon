from __future__ import annotations

from .base import ModelProvider

MAX_BATCH = 2048


class AnthropicProvider(ModelProvider):
    """Generic Anthropic-style embedding provider.

    For endpoints that use Anthropic's authentication conventions
    (``x-api-key`` header, ``anthropic-version`` header) rather than
    the OpenAI Bearer token style.
    """

    ANTHROPIC_VERSION = "2023-06-01"

    def __init__(self, *, api_url: str, model: str, api_key: str | None = None) -> None:
        try:
            import httpx  # type: ignore[import-untyped]
        except ImportError as e:
            raise ImportError(
                "Install the 'httpx' package to use AnthropicProvider: "
                "pip install httpx"
            ) from e
        headers: dict[str, str] = {
            "Content-Type": "application/json",
            "anthropic-version": self.ANTHROPIC_VERSION,
        }
        if api_key:
            headers["x-api-key"] = api_key
        self._client = httpx.AsyncClient(
            base_url=api_url.rstrip("/"),
            headers=headers,
            timeout=60.0,
        )
        self._model = model

    async def embed_batch(self, texts: list[str]) -> list[list[float]]:
        results: list[list[float]] = []
        for i in range(0, len(texts), MAX_BATCH):
            chunk = texts[i : i + MAX_BATCH]
            response = await self._client.post(
                "/embeddings",
                json={"model": self._model, "input": chunk},
            )
            response.raise_for_status()
            data = response.json()
            items = sorted(data["data"], key=lambda x: x["index"])
            results.extend(item["embedding"] for item in items)
        return results
