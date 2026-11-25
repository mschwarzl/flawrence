use crate::content::MessageContent;
use crate::usage::Usage;

/// Chat completion response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Message content returned by the assistant.
    pub content: MessageContent,
    /// Optional reasoning content (e.g. from Kimi K2.5, DeepSeek, Anthropic).
    pub reasoning_content: Option<String>,
    /// Encrypted reasoning signature for round-tripping (Anthropic extended thinking).
    pub reasoning_signature: Option<String>,
    /// Model identifier string (e.g. "@cf/moonshotai/kimi-k2.5").
    pub model: String,
    /// Token usage.
    pub usage: Usage,
    /// Whether the provider stopped generating because it reached the output
    /// token limit, rather than because the model finished.
    ///
    /// A truncated response looks like a complete one: the text simply ends,
    /// and a tool call's arguments arrive missing whatever came after the cut.
    /// Without this, "the model omitted these fields" and "the model never got
    /// to write these fields" are the same observation, and only the first has
    /// a remedy the model can act on.
    pub truncated: bool,
}
