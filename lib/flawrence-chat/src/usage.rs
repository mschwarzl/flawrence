use serde::{Deserialize, Serialize};

/// Token usage reported by the provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub cache_details: Option<CacheDetails>,
}

/// Prompt-token cache breakdown (provider-agnostic).
///
/// - `read_tokens`: tokens served from cache (OpenAI's `cached_tokens`)
/// - `write_tokens`: tokens written to cache (Anthropic's `cache_creation_input_tokens`)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheDetails {
    pub read_tokens: Option<i32>,
    pub write_tokens: Option<i32>,
}
