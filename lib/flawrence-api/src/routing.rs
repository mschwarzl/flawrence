use crate::chat_client::ChatClient;
use anyhow::{Result, bail};
use async_trait::async_trait;
use flawrence_chat::{ChatOptions, ChatRequest, ChatResponse};
use std::collections::HashMap;
use std::sync::Arc;

/// Model-id prefixes that always route to a built-in client. A custom
/// `FLAWRENCE_PROVIDER_*` provider must not use one of these names: the
/// built-in credentials would silently serve its requests instead.
pub const BUILTIN_MODEL_PREFIXES: &[&str] = &["anthropic/", "openai/", "workers-ai/"];

pub struct RoutingClient {
    pub anthropic: Option<Arc<dyn ChatClient>>,
    pub openai: Option<Arc<dyn ChatClient>>,
    pub workers_ai: Option<Arc<dyn ChatClient>>,
    pub providers: HashMap<String, Arc<dyn ChatClient>>,
}

impl RoutingClient {
    pub fn new() -> Self {
        Self {
            anthropic: None,
            openai: None,
            workers_ai: None,
            providers: HashMap::new(),
        }
    }

    pub fn with_anthropic(mut self, client: Arc<dyn ChatClient>) -> Self {
        self.anthropic = Some(client);
        self
    }

    pub fn with_workers_ai(mut self, client: Arc<dyn ChatClient>) -> Self {
        self.workers_ai = Some(client);
        self
    }

    pub fn with_openai(mut self, client: Arc<dyn ChatClient>) -> Self {
        self.openai = Some(client);
        self
    }

    pub fn with_provider(mut self, name: impl Into<String>, client: Arc<dyn ChatClient>) -> Self {
        self.providers.insert(name.into(), client);
        self
    }

    fn route(&self, model: &str) -> Result<&Arc<dyn ChatClient>> {
        for prefix in BUILTIN_MODEL_PREFIXES {
            if let Some(remainder) = model.strip_prefix(prefix)
                && (remainder.is_empty()
                    || BUILTIN_MODEL_PREFIXES
                        .iter()
                        .any(|nested| remainder.starts_with(nested)))
            {
                bail!("Model must have exactly one provider prefix and a non-empty id: {model}");
            }
        }
        if model.starts_with("anthropic/") {
            self.anthropic
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No Anthropic client configured for model: {model}"))
        } else if model.starts_with("openai/") {
            self.openai
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No OpenAI client configured for model: {model}"))
        } else if model.starts_with("workers-ai/") || !model.contains('/') {
            self.workers_ai.as_ref().ok_or_else(|| {
                anyhow::anyhow!("No Workers AI client configured for model: {model}")
            })
        } else if let Some((prefix, _)) = model.split_once('/') {
            self.providers
                .get(prefix)
                .ok_or_else(|| anyhow::anyhow!("Unknown model prefix: {model}"))
        } else {
            bail!("Unknown model prefix: {model}")
        }
    }

    pub fn available_providers(&self) -> Vec<String> {
        let mut providers = Vec::new();
        if self.anthropic.is_some() {
            providers.push("anthropic".to_string());
        }
        if self.openai.is_some() {
            providers.push("openai".to_string());
        }
        if self.workers_ai.is_some() {
            providers.push("workers-ai".to_string());
        }
        providers.extend(self.providers.keys().cloned());
        providers
    }
}

impl Default for RoutingClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChatClient for RoutingClient {
    async fn chat_completion(
        &self,
        model: &str,
        request: ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        let client = self.route(model)?;
        // Strip provider prefix so the underlying client receives a bare model ID.
        let bare_model = model.split_once('/').map(|(_, m)| m).unwrap_or(model);
        client.chat_completion(bare_model, request, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicated_provider_prefix() {
        let router = RoutingClient::new();
        let error = router.route("openai/openai/gpt-5.6").err().unwrap();
        assert!(error.to_string().contains("exactly one provider prefix"));
        assert!(router.route("openai/anthropic/model").is_err());
        assert!(router.route("openai/").is_err());
    }
}
