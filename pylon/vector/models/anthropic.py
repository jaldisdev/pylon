from __future__ import annotations

from .base import Message, ModelProvider


class AnthropicProvider(ModelProvider):
    """Anthropic Messages API provider — chat completions only.

    Anthropic has no embeddings endpoint, so this only implements *chat*
    (inherits the base class's default "not supported" *embed_batch*).

    POST {api_url}/messages
    x-api-key: {api_key}
    anthropic-version: 2023-06-01
    {"model": "...", "max_tokens": ..., "system": "...", "messages": [...]}

    Anthropic takes the system prompt as a separate top-level field, not a
    "system"-role message — split out here so callers can build a uniform
    [{"role": "system", ...}, {"role": "user", ...}, ...] list regardless of
    which provider ends up handling it.
    """

    ANTHROPIC_VERSION = "2023-06-01"
    MAX_TOKENS = 1024

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

    async def chat(self, messages: list[Message]) -> str:
        system = next((m["content"] for m in messages if m["role"] == "system"), None)
        turns = [m for m in messages if m["role"] != "system"]
        body: dict[str, object] = {
            "model": self._model,
            "max_tokens": self.MAX_TOKENS,
            "messages": turns,
        }
        if system:
            body["system"] = system
        response = await self._client.post("/messages", json=body)
        response.raise_for_status()
        data = response.json()
        return "".join(block["text"] for block in data["content"] if block["type"] == "text")
