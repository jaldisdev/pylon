/// A single turn in a chat conversation — mirrors
/// `pylon.vector.models.base.Message` (`{"role": "system"|"user"|"assistant",
/// "content": "..."}`). Callers build a uniform list regardless of which
/// provider ends up handling it; `AnthropicProvider::chat` splits the
/// system-role message out itself, the same way the Python version does.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}
