from __future__ import annotations

Message = dict[str, str]  # {"role": "system" | "user" | "assistant", "content": "..."}


class ModelProvider:
    """Base model provider.  Each concrete subclass targets one API and
    implements whichever capability that API actually supports — e.g.
    Anthropic has no embeddings endpoint, so AnthropicProvider only
    implements *chat*; OpenAI-compatible endpoints (OpenAI, Mistral, ...)
    implement both.  Neither capability is required of the other.
    """

    async def embed_batch(self, texts: list[str]) -> list[list[float]]:
        """Return one embedding vector per input text, in the same order."""
        raise NotImplementedError(f"{type(self).__name__} does not support embeddings")

    async def embed(self, text: str) -> list[float]:
        results = await self.embed_batch([text])
        return results[0]

    async def chat(self, messages: list[Message]) -> str:
        """Send the conversation so far, return the assistant's reply text."""
        raise NotImplementedError(f"{type(self).__name__} does not support chat")
