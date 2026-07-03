from __future__ import annotations

from .base import ModelProvider

MAX_BATCH = 2048


class OpenAIProvider(ModelProvider):
    """Generic OpenAI-compatible embedding provider.

    Works with any endpoint that follows the OpenAI embeddings API:
      POST {api_url}/embeddings
      Authorization: Bearer {api_key}
      {"model": "...", "input": ["text", ...]}

    Compatible providers: OpenAI, Mistral, any OpenAI-compatible endpoint.
    """

    def __init__(self, *, api_url: str, model: str, api_key: str | None = None) -> None:
        try:
            import httpx  # type: ignore[import-untyped]
        except ImportError as e:
            raise ImportError(
                "Install the 'httpx' package to use OpenAIProvider: pip install httpx"
            ) from e
        headers: dict[str, str] = {"Content-Type": "application/json"}
        if api_key:
            headers["Authorization"] = f"Bearer {api_key}"
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
