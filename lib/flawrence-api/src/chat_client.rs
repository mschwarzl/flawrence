use anyhow::Result;
use async_trait::async_trait;

use flawrence_chat::{ChatOptions, ChatRequest, ChatResponse};

#[async_trait]
pub trait ChatClient: Send + Sync {
    async fn chat_completion(
        &self,
        model: &str,
        request: ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse>;
}
