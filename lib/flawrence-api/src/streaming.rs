//! OpenAI-compatible streaming clients using `async-openai` BYOT.
//!
//! Calls the OpenAI-compatible `POST /v1/chat/completions` endpoint with
//! `"stream": true` to avoid upstream HTTP timeouts on long-running inference.
//! SSE chunks are consumed internally and assembled into a single `ChatResponse`.

use crate::strict::strict_schema_supported;
use anyhow::{Context, Result};
use async_openai::Client;
use async_openai::config::{Config as OpenAIClientConfig, OpenAIConfig};
use async_openai::error::OpenAIError;
use async_openai::traits::RequestOptionsBuilder;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_compat::header::{HeaderMap, HeaderValue, USER_AGENT};
use secrecy::SecretString;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const OPENAI_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OPENAI_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

tokio::task_local! {
    /// Per-task session ID for GPU affinity routing.
    /// Scoped by the task loop; read by `StreamingClient` to set
    /// the `x-session-affinity` header on every request.
    pub static SESSION_ID: String;
}

/// Idle timeout for the SSE stream: if no chunk arrives within this window,
/// the stream is treated as stalled and the call returns an error (which the
/// task loop retries with backoff). Without this, a stalled upstream hangs the
/// agent forever with no error and no progress. The timer resets only on chunks
/// that carry real progress (content, reasoning, tool-call arguments, usage,
/// or a finish reason); heartbeat/keepalive frames that yield chunks with no
/// progress do not reset it, so a provider that streams heartbeats while the
/// completion never advances still triggers the timeout. Legitimately long
/// streaming responses are unaffected as long as they emit tokens within the
/// window. Configurable via `FLAWRENCE_STREAM_IDLE_TIMEOUT_SECS`.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
fn stream_idle_timeout() -> Duration {
    std::env::var("FLAWRENCE_STREAM_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT)
}

use super::chat_client::ChatClient;
use flawrence_chat::{
    CacheDetails, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole, ContentPart,
    MessageContent, ReasoningEffort, StructuredOutput, Tool, ToolCall, Usage,
};

/// Terminal Responses API failure that still carries billable usage.
#[derive(Debug)]
pub struct OpenAiResponseError {
    message: String,
    usage: Usage,
    retryable: bool,
    billing_unknown: bool,
}

impl OpenAiResponseError {
    #[must_use]
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    #[must_use]
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    #[must_use]
    pub fn billing_unknown(&self) -> bool {
        self.billing_unknown
    }

    #[must_use]
    pub fn retryable_with_budget(&self, budget_is_set: bool) -> bool {
        self.retryable && !(budget_is_set && self.billing_unknown)
    }
}

impl std::fmt::Display for OpenAiResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "OpenAI Responses API error: {}", self.message)
    }
}

impl std::error::Error for OpenAiResponseError {}

#[derive(Debug)]
pub struct StreamingRequestError {
    message: String,
    retryable: bool,
    billing_unknown: bool,
}

impl StreamingRequestError {
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    #[must_use]
    pub fn billing_unknown(&self) -> bool {
        self.billing_unknown
    }

    #[must_use]
    pub fn retryable_with_budget(&self, budget_is_set: bool) -> bool {
        self.retryable && !(budget_is_set && self.billing_unknown)
    }
}

impl std::fmt::Display for StreamingRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StreamingRequestError {}

impl crate::retry::ClassifiedError for OpenAiResponseError {
    fn is_retryable(&self) -> bool {
        self.retryable
    }
    fn billing_unknown(&self) -> bool {
        self.billing_unknown
    }
    fn usage(&self) -> Option<&Usage> {
        Some(&self.usage)
    }
}

impl crate::retry::ClassifiedError for StreamingRequestError {
    fn is_retryable(&self) -> bool {
        self.retryable
    }
    fn billing_unknown(&self) -> bool {
        self.billing_unknown
    }
}

// ============================================================================
// StreamingClient
// ============================================================================

/// Default endpoint of the `openai/` Responses client.
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Credentials for the `openai/` Responses client.
pub enum OpenAiAuth {
    /// A standard OpenAI API key, sent as a bearer token.
    ApiKey(String),
    /// An opencode gateway access token, sent as the `cf-access-token` header.
    OpencodeToken(String),
}

/// Chat client for Workers AI chat completions, OpenAI Responses and
/// OpenAI-compatible providers, using SSE streaming to avoid upstream HTTP timeouts.
pub struct StreamingClient {
    client: Client<Box<dyn OpenAIClientConfig>>,
    provider: StreamingProvider,
    /// Model ids whose provider permanently rejected `reasoning_effort`. The
    /// field is omitted for them from the first request on, so a known-bad
    /// model does not pay a failing request on every call.
    effort_rejected: Mutex<HashSet<String>>,
}

impl StreamingClient {
    /// Whether the provider already permanently rejected `reasoning_effort`
    /// for this model.
    fn effort_rejected(&self, model: &str) -> bool {
        self.effort_rejected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(model)
    }

    /// Record a model whose provider permanently rejected `reasoning_effort`,
    /// so every later call omits the field from the start.
    fn remember_effort_rejected(&self, model: &str) {
        self.effort_rejected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(model.to_string());
    }

    /// One attempt at the chat-completions path: build the request body, open
    /// the SSE stream and accumulate it into a [`ChatResponse`]. Split from
    /// [`StreamingClient::chat_completion`] so the `reasoning_effort` fallback
    /// can rerun the attempt with the field dropped.
    async fn chat_attempt(
        &self,
        request_model: &str,
        model: &str,
        request: &ChatRequest,
        options: Option<&ChatOptions>,
        skip_effort: bool,
    ) -> Result<ChatResponse> {
        let body = build_chat_body(request, request_model, options, self.provider, skip_effort);

        // Call the streaming endpoint via async-openai BYOT
        let mut chat = self.client.chat();
        if self.provider == StreamingProvider::WorkersAi
            && let Ok(sid) = SESSION_ID.try_with(|s| s.clone())
        {
            chat = chat
                .header("x-session-affinity", sid)
                .context("Failed to set session affinity header")?;
        }
        let mut stream = chat
            .create_stream_byot::<Value, Value>(body)
            .await
            .map_err(|e| map_stream_error("failed to create streaming chat completion", &e))?;

        // Accumulate the streamed response. Race each chunk against an idle
        // timeout so a stalled upstream becomes a retryable error instead of
        // hanging the agent forever.
        let mut text = String::new();
        let mut truncated = false;
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCallAccumulator> = Vec::new();
        let mut usage: Option<Usage> = None;

        let idle = stream_idle_timeout();
        let mut stalled = false;
        // Track the last chunk that carried real progress (content, reasoning,
        // tool-call arguments, usage, or a finish reason). A provider that
        // streams heartbeat/keepalive frames yields chunks continuously — which
        // would reset a naive per-chunk idle timer forever while the completion
        // never advances. Resetting only on meaningful progress makes such stalls
        // recoverable: after `idle` with no progress we bail and let the task
        // loop retry.
        let mut last_progress = Instant::now();
        loop {
            let chunk_result = match tokio::time::timeout(idle, stream.next()).await {
                Err(_) => {
                    stalled = true;
                    break;
                }
                Ok(None) => break,
                Ok(Some(r)) => r,
            };
            let chunk =
                chunk_result.map_err(|e| map_stream_error("error in SSE stream chunk", &e))?;
            let mut made_progress = false;

            // Parse choices[0].delta
            if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array())
                && let Some(choice) = choices.first()
            {
                // `length` is the provider saying it stopped at the output
                // limit rather than because the model was done. It arrives
                // on the final chunk, after the content it truncated.
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    truncated = reason == "length";
                }
                let delta = &choice["delta"];

                // Accumulate text content
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        made_progress = true;
                    }
                    text.push_str(content);
                } else if let Some(blocks) = delta.get("content").and_then(|c| c.as_array()) {
                    // Mistral streams content as typed blocks, with reasoning in
                    // `thinking` blocks instead of `reasoning_content`.
                    if push_content_blocks(blocks, &mut text, &mut reasoning) {
                        made_progress = true;
                    }
                }

                // Accumulate reasoning content
                if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                    if !rc.is_empty() {
                        made_progress = true;
                    }
                    reasoning.push_str(rc);
                }

                // A finish_reason signals the completion ended — progress.
                if choice.get("finish_reason").is_some() {
                    made_progress = true;
                }

                // Accumulate tool calls (streamed incrementally by index)
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    if !tcs.is_empty() {
                        made_progress = true;
                    }
                    accumulate_tool_call_deltas(&mut tool_calls, tcs);
                }
            }

            // Parse usage from final chunk (if present). With
            // `include_usage`, providers send `"usage": null` on every content
            // chunk, so a null must not clobber a real reading.
            if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                made_progress = true;
                usage = Some(parse_usage(u));
            }

            if made_progress {
                last_progress = Instant::now();
            } else if last_progress.elapsed() > idle {
                // Chunks keep arriving but carry no progress (e.g. provider
                // heartbeat/keepalive frames, or a stalled inference). The
                // completion is not advancing; bail so the task loop retries
                // instead of hanging forever.
                stalled = true;
                break;
            }
        }
        if stalled {
            return Err(StreamingRequestError {
                message: format!(
                    "{} stream idle timeout: no progress in {}s — the upstream is sending frames but the \
                     completion is not advancing (likely heartbeat/keepalive with no content, or a stalled \
                     inference). Adjust via FLAWRENCE_STREAM_IDLE_TIMEOUT_SECS.",
                    self.provider.label(),
                    idle.as_secs()
                ),
                retryable: true,
                billing_unknown: true,
            }
            .into());
        }

        // Build content parts
        let mut parts: Vec<ContentPart> = Vec::new();

        if !text.trim().is_empty() {
            parts.push(ContentPart::Text(text));
        }

        for acc in tool_calls {
            if acc.name.is_empty() {
                continue;
            }
            let arguments: Value = serde_json::from_str(&acc.arguments)
                .unwrap_or_else(|_| Value::String(acc.arguments.clone()));
            parts.push(ContentPart::ToolCall(ToolCall {
                call_id: acc.id,
                fn_name: acc.name,
                fn_arguments: arguments,
            }));
        }

        let content = if parts.is_empty() {
            MessageContent::default()
        } else {
            MessageContent::from_parts(parts)
        };

        Ok(ChatResponse {
            content,
            reasoning_content: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            reasoning_signature: None,
            model: model.to_string(),
            usage: usage.unwrap_or_default(),
            truncated,
        })
    }

    /// Create a new streaming client for Workers AI. With `base_url_override`,
    /// `account_id` is ignored and the override is used verbatim.
    pub fn new(
        api_token: String,
        account_id: String,
        base_url_override: Option<String>,
    ) -> Result<Self> {
        let base_url = base_url_override.unwrap_or_else(|| {
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1",
                account_id
            )
        });

        let config = OpenAIConfig::new()
            .with_api_base(base_url)
            .with_api_key(api_token)
            .with_header("User-Agent", crate::USER_AGENT)?;

        let http_client = reqwest_compat::Client::builder()
            .connect_timeout(OPENAI_CONNECT_TIMEOUT)
            .timeout(OPENAI_REQUEST_TIMEOUT)
            .build()
            .context("Failed to create Workers AI HTTP client")?;
        let client = Client::with_config(Box::new(config) as Box<dyn OpenAIClientConfig>)
            .with_http_client(http_client);

        Ok(Self {
            client,
            provider: StreamingProvider::WorkersAi,
            effort_rejected: Mutex::new(HashSet::new()),
        })
    }

    /// Create a generic OpenAI-compatible streaming client from an explicit
    /// base URL and API key. Used for custom/self-hosted providers routed via
    /// the standard `POST /v1/chat/completions` endpoint.
    pub fn from_url(api_key: String, base_url: String) -> Result<Self> {
        let provider = if is_mistral_base_url(&base_url) {
            StreamingProvider::Mistral
        } else {
            StreamingProvider::SelfHosted
        };
        let config = OpenAIConfig::new()
            .with_api_base(base_url)
            .with_api_key(api_key)
            .with_header("User-Agent", crate::USER_AGENT)?;

        let http_client = reqwest_compat::Client::builder()
            .connect_timeout(OPENAI_CONNECT_TIMEOUT)
            .timeout(OPENAI_REQUEST_TIMEOUT)
            .build()
            .context("Failed to create self-hosted provider HTTP client")?;
        let client = Client::with_config(Box::new(config) as Box<dyn OpenAIClientConfig>)
            .with_http_client(http_client);

        Ok(Self {
            client,
            provider,
            effort_rejected: Mutex::new(HashSet::new()),
        })
    }

    /// Create a streaming client for the OpenAI Responses API.
    ///
    /// An API key talks to `https://api.openai.com/v1` unless `base_url_override`
    /// is set. An opencode gateway token has no default endpoint, so it needs
    /// `base_url_override`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the endpoint is missing for a gateway token, is not an
    /// HTTPS URL, or carries a query or fragment.
    pub fn new_openai(auth: OpenAiAuth, base_url_override: Option<String>) -> Result<Self> {
        let raw_base_url = match (&auth, base_url_override) {
            (_, Some(url)) => url,
            (OpenAiAuth::ApiKey(_), None) => DEFAULT_OPENAI_BASE_URL.to_string(),
            (OpenAiAuth::OpencodeToken(_), None) => {
                anyhow::bail!("OPENCODE_ACCESS_TOKEN needs FLAWRENCE_OPENAI_BASE_URL")
            }
        };
        let parsed_base_url = reqwest_compat::Url::parse(&raw_base_url)
            .context("FLAWRENCE_OPENAI_BASE_URL is not a valid URL")?;
        anyhow::ensure!(
            parsed_base_url.scheme() == "https",
            "FLAWRENCE_OPENAI_BASE_URL must use HTTPS"
        );
        anyhow::ensure!(
            parsed_base_url.query().is_none() && parsed_base_url.fragment().is_none(),
            "FLAWRENCE_OPENAI_BASE_URL cannot contain a query or fragment"
        );
        let base_url = parsed_base_url.as_str().trim_end_matches('/').to_string();
        let config: Box<dyn OpenAIClientConfig> = match auth {
            OpenAiAuth::ApiKey(key) => Box::new(
                OpenAIConfig::new()
                    .with_api_base(base_url)
                    .with_api_key(key)
                    .with_header("User-Agent", crate::USER_AGENT)?,
            ),
            OpenAiAuth::OpencodeToken(token) => Box::new(OpencodeConfig::new(base_url, token)?),
        };
        let http_client = reqwest_compat::Client::builder()
            .redirect(reqwest_compat::redirect::Policy::none())
            .connect_timeout(OPENAI_CONNECT_TIMEOUT)
            .timeout(OPENAI_REQUEST_TIMEOUT)
            .build()
            .context("Failed to create OpenAI HTTP client")?;
        let client = Client::with_config(config).with_http_client(http_client);

        Ok(Self {
            client,
            provider: StreamingProvider::OpenAi,
            effort_rejected: Mutex::new(HashSet::new()),
        })
    }

    async fn responses_completion(
        &self,
        model: &str,
        request: &ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        let request_model = strip_model_prefix(model, self.provider);
        let body = build_responses_request(request_model, request, options)?;
        let mut stream = self
            .client
            .responses()
            .create_stream_byot::<Value, Value>(body)
            .await
            .map_err(|e| map_stream_error("failed to create streaming response", &e))?;
        let mut accumulator = ResponsesAccumulator::default();

        loop {
            let event_result = tokio::time::timeout(OPENAI_STREAM_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| accumulator.stream_error("Responses API stream idle timeout"))?;
            let Some(event_result) = event_result else {
                break;
            };
            let event = event_result.map_err(|error| {
                accumulator.stream_error(format!("error in Responses API stream event: {error}"))
            })?;
            accumulator.apply_event(&event);
            if accumulator.is_terminal() {
                break;
            }
        }

        accumulator.finish(model)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamingProvider {
    WorkersAi,
    OpenAi,
    SelfHosted,
    /// Mistral's chat API: a custom provider whose base URL is on
    /// `mistral.ai`. Its schema is strict (unknown fields are a 422), so it
    /// gets the standard `reasoning_effort` and no `reasoning_content` replay.
    Mistral,
}

/// Append a delta's typed content blocks: `text` blocks to `text`, and the
/// nested text of `thinking` blocks to `reasoning`. Returns whether any
/// non-empty text was appended.
fn push_content_blocks(blocks: &[Value], text: &mut String, reasoning: &mut String) -> bool {
    let before = text.len() + reasoning.len();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("thinking") => match block.get("thinking") {
                Some(Value::String(t)) => reasoning.push_str(t),
                Some(Value::Array(parts)) => {
                    for t in parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                    {
                        reasoning.push_str(t);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    text.len() + reasoning.len() > before
}

/// Whether a custom provider's base URL points at Mistral's API.
fn is_mistral_base_url(base_url: &str) -> bool {
    reqwest_compat::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "mistral.ai" || host.ends_with(".mistral.ai"))
}

impl StreamingProvider {
    /// How to name this provider in an error a human will read. A run against
    /// a self-hosted endpoint should not be told "Workers AI" timed out.
    fn label(self) -> &'static str {
        match self {
            Self::WorkersAi => "Workers AI",
            Self::OpenAi => "OpenAI",
            Self::SelfHosted => "the configured endpoint",
            Self::Mistral => "Mistral",
        }
    }
}

/// async-openai's default config always emits an Authorization header, which
/// the opencode gateway rejects. The gateway authenticates only with this
/// Cloudflare Access header.
struct OpencodeConfig {
    api_base: String,
    access_token: SecretString,
    headers: HeaderMap,
}

impl OpencodeConfig {
    fn new(api_base: String, access_token: String) -> Result<Self> {
        let mut token_header = HeaderValue::from_bytes(access_token.as_bytes())
            .context("OPENCODE_ACCESS_TOKEN is not a valid HTTP header value")?;
        token_header.set_sensitive(true);

        let mut headers = HeaderMap::with_capacity(2);
        headers.insert("cf-access-token", token_header);
        headers.insert(USER_AGENT, HeaderValue::from_static(crate::USER_AGENT));

        Ok(Self {
            api_base,
            access_token: access_token.into(),
            headers,
        })
    }
}

impl OpenAIClientConfig for OpencodeConfig {
    fn headers(&self) -> HeaderMap {
        self.headers.clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base, path)
    }

    fn query(&self) -> Vec<(&str, &str)> {
        Vec::new()
    }

    fn api_base(&self) -> &str {
        &self.api_base
    }

    fn api_key(&self) -> &SecretString {
        &self.access_token
    }
}

// ============================================================================
// ChatClient impl
// ============================================================================

#[async_trait]
impl ChatClient for StreamingClient {
    async fn chat_completion(
        &self,
        model: &str,
        request: ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        if self.provider == StreamingProvider::OpenAi {
            return self.responses_completion(model, &request, options).await;
        }

        let request_model = strip_model_prefix(model, self.provider);
        let skip_effort = self.effort_rejected(request_model);
        let response = self
            .chat_attempt(request_model, model, &request, options, skip_effort)
            .await;

        // A permanent error that names the field means this model does not
        // take `reasoning_effort` at all (Mistral 3051: "reasoning_effort is
        // not enabled for this model"). A retryable failure must not disable
        // the field — the task loop retries those with the same options, so
        // disabling on a hiccup would silently downgrade a healthy model.
        // Remember the model so later calls omit the field from the start,
        // and retry this call once without it.
        match response {
            Err(e)
                if !skip_effort
                    && options.is_some_and(|o| o.reasoning_effort.is_some())
                    && effort_rejected_error(&e) =>
            {
                self.remember_effort_rejected(request_model);
                self.chat_attempt(request_model, model, &request, options, true)
                    .await
            }
            other => other,
        }
    }
}
/// Serialize a chat-completions request body. `skip_reasoning_effort` omits
/// the effort fields for a model the provider has already rejected the field
/// for (see [`StreamingClient::remember_effort_rejected`]).
fn build_chat_body(
    request: &ChatRequest,
    model: &str,
    options: Option<&ChatOptions>,
    provider: StreamingProvider,
    skip_reasoning_effort: bool,
) -> Value {
    // `stream_options.include_usage` is what makes an OpenAI-compatible
    // provider emit the final usage-only chunk. Without it, spec-faithful
    // providers stream no usage at all and every call bills as unknown.
    let mut body = json!({
        "model": model,
        "messages": serialize_messages(
            &request.messages,
            provider != StreamingProvider::Mistral,
        ),
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    // Add tools if present
    if let Some(tools) = request.tools.as_ref().filter(|t| !t.is_empty()) {
        body["tools"] = serialize_tools(tools);
    }

    // Add options
    if let Some(opts) = options {
        if let Some(temp) = opts.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = opts.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(ref output) = opts.structured_output {
            body["response_format"] = serialize_structured_output(output);
        }
        if let Some(ref effort) = opts.reasoning_effort
            && !skip_reasoning_effort
        {
            apply_reasoning_effort(&mut body, effort, provider);
        }
    }
    body
}

/// A permanent error naming the field means the model does not take
/// `reasoning_effort` at all (Mistral 3051: "reasoning_effort is not enabled
/// for this model"). Matching on the field name instead of one provider's
/// full sentence keeps the fallback working when a provider rewords its
/// error; the cost of a false positive is one silently dropped capability.
fn effort_rejection(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("reasoning_effort") || lowered.contains("reasoning effort")
}

/// Whether `e` is a permanent provider rejection of `reasoning_effort`.
fn effort_rejected_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<StreamingRequestError>()
        .is_some_and(|classified| !classified.retryable())
        && effort_rejection(&format!("{e:#}"))
}

fn strip_model_prefix(model: &str, provider: StreamingProvider) -> &str {
    match provider {
        StreamingProvider::WorkersAi => model.strip_prefix("workers-ai/").unwrap_or(model),
        StreamingProvider::OpenAi => model.strip_prefix("openai/").unwrap_or(model),
        // Self-hosted providers receive a bare model id (the router already
        // stripped the provider-name prefix), so pass it through untouched.
        StreamingProvider::SelfHosted | StreamingProvider::Mistral => model,
    }
}

fn build_responses_request(
    model: &str,
    request: &ChatRequest,
    options: Option<&ChatOptions>,
) -> Result<Value> {
    let mut body = json!({
        "include": ["reasoning.encrypted_content"],
        "model": model,
        "input": serialize_responses_input(&request.messages)?,
        "store": false,
        "stream": true,
    });

    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body["tools"] = serialize_responses_tools(tools);
    }

    if let Some(options) = options {
        if options.reasoning_effort.is_none()
            && let Some(temperature) = options.temperature
        {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = options.max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }
        if let Some(output) = &options.structured_output {
            body["text"] = serialize_responses_structured_output(output);
        }
        if let Some(effort) = &options.reasoning_effort {
            body["reasoning"] = json!({ "effort": responses_reasoning_effort_str(effort) });
        }
    }

    Ok(body)
}

/// Turn an async-openai transport/stream error into a concise, classified error.
///
/// Cloudflare Workers AI reports failures with its own envelope
/// `{"errors":[{"message":..,"code":..}],"success":false,...}`, which async-openai
/// cannot deserialize into its OpenAI-shaped error type and surfaces as an opaque
/// "missing field `error`" message. We extract the real message/code so logs are
/// legible and the upstream retry classifier can distinguish a transient timeout
/// (e.g. code 3046) from a rate limit.
/// Turn an async-openai error into a classified [`StreamingRequestError`].
///
/// The typed error already carries the signal that matters -- an HTTP status on
/// the reqwest variant, a structured `type`/`code` on an API error -- so read
/// that first and fall back to parsing the streamed JSON body only for what the
/// type did not preserve. Classification then keys off the status and type,
/// with the free-text message as a last resort. This is what stops a transient
/// 503 whose body mentions "authentication" from being read as permanent.
fn map_stream_error(context: &str, error: &OpenAIError) -> anyhow::Error {
    classify_stream_error(context, error).into()
}

/// The classified error, before conversion into an `anyhow::Error`, so call
/// sites can read `retryable` without downcasting.
fn classify_stream_error(context: &str, error: &OpenAIError) -> StreamingRequestError {
    let raw = error.to_string();

    let mut status = None;
    let mut error_type: Option<String> = None;
    let mut error_code: Option<String> = None;
    match error {
        OpenAIError::Reqwest(inner) => status = inner.status().map(|code| code.as_u16()),
        OpenAIError::ApiError(api) => {
            status = api.code.as_deref().and_then(http_error_status);
            error_type = api.r#type.clone();
            error_code = api.code.clone();
        }
        _ => {}
    }

    // Providers also stream the failure as a JSON body; recover from it whatever
    // the typed error did not already give us, plus a cleaner message.
    let embedded = parse_embedded_error(&raw);
    if let Some(embedded) = &embedded {
        status = status.or(embedded.status);
        if error_type.is_none() {
            error_type = embedded.error_type.clone();
        }
        if error_code.is_none() {
            error_code = embedded.error_code.clone();
        }
    }
    let message = embedded
        .as_ref()
        .and_then(|embedded| embedded.message.clone())
        .map_or_else(
            || format!("{context}: {raw}"),
            |m| format!("{m} [{context}]"),
        );

    let retryable =
        crate::retry::classify(status, error_type.as_deref(), error_code.as_deref(), &raw)
            .is_retryable(true);

    StreamingRequestError {
        retryable,
        billing_unknown: retryable && billing_is_uncertain(&raw),
        message,
    }
}

/// A provider error object recovered from a streamed JSON body.
struct EmbeddedError {
    status: Option<u16>,
    error_type: Option<String>,
    error_code: Option<String>,
    message: Option<String>,
}

fn json_u16(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| value.as_str().and_then(http_error_status))
        .filter(|status| (400..=599).contains(status))
}

fn http_error_status(code: &str) -> Option<u16> {
    code.parse()
        .ok()
        .filter(|status| (400..=599).contains(status))
}

fn json_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// A `code` may be a string ("rate_limit") or a number (a Workers AI status).
fn json_code(value: &Value) -> Option<String> {
    json_string(value).or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// Recover a provider error object from a streamed body, across the shapes seen
/// in practice: `{"error":{...}}` (OpenAI/Anthropic), `{"errors":[{...}]}`
/// (Workers AI), `{"object":"error",...}` (vLLM), and `{"detail":"..."}`.
fn parse_embedded_error(raw: &str) -> Option<EmbeddedError> {
    let start = raw.find('{')?;
    // SDK Display implementations can append a code after the JSON object.
    let value = serde_json::Deserializer::from_str(raw.get(start..)?)
        .into_iter::<Value>()
        .next()?
        .ok()?;

    if let Some(error) = value
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| {
            (value.get("object").and_then(Value::as_str) == Some("error")).then_some(&value)
        })
    {
        return Some(EmbeddedError {
            status: error
                .get("status")
                .and_then(json_u16)
                .or_else(|| error.get("code").and_then(json_u16)),
            error_type: error.get("type").and_then(json_string),
            error_code: error.get("code").and_then(json_code),
            message: error.get("message").and_then(json_string),
        });
    }
    if let Some(first) = value
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
    {
        return Some(EmbeddedError {
            status: None,
            error_type: first.get("type").and_then(json_string),
            error_code: first.get("code").and_then(json_code),
            message: first.get("message").and_then(json_string),
        });
    }
    value
        .get("detail")
        .and_then(json_string)
        .map(|detail| EmbeddedError {
            status: None,
            error_type: None,
            error_code: None,
            message: Some(detail),
        })
}

fn billing_is_uncertain(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    const DEFINITE_PROVIDER_REJECTIONS: &[&str] = &[
        "400",
        "401",
        "403",
        "404",
        "405",
        "422",
        "429",
        "rate limit",
        "too many requests",
        "invalid_request",
        "authentication",
        "unauthorized",
        "forbidden",
        "unknown model",
        "unsupported",
    ];
    !DEFINITE_PROVIDER_REJECTIONS
        .iter()
        .any(|marker| message.contains(marker))
}

/// Message-only retry classification, re-exported so existing callers keep the
/// `streaming::` path. The logic now lives in [`crate::retry`], which also reads
/// status codes and structured error types when they are available.
pub use crate::retry::retryable_error_message;

// ============================================================================
// Accumulator for streaming tool calls
// ============================================================================

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// Fold one chunk's `tool_calls` deltas into the per-index accumulators. Later
/// chunks may repeat `id` or `name` as an empty string (Mistral sends
/// `"name": ""` with every argument fragment); those must not clobber the
/// value from the first chunk, or the call is dropped for having no name.
fn accumulate_tool_call_deltas(tool_calls: &mut Vec<ToolCallAccumulator>, deltas: &[Value]) {
    for tc in deltas {
        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

        // Grow the accumulator vec if needed
        while tool_calls.len() <= index {
            tool_calls.push(ToolCallAccumulator::default());
        }

        let acc = &mut tool_calls[index];

        if let Some(id) = tc
            .get("id")
            .and_then(|i| i.as_str())
            .filter(|i| !i.is_empty())
        {
            acc.id = id.to_string();
        }
        if let Some(func) = tc.get("function") {
            if let Some(name) = func
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|n| !n.is_empty())
            {
                acc.name = name.to_string();
            }
            if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                acc.arguments.push_str(args);
            }
        }
    }
}

#[derive(Default)]
struct ResponsesAccumulator {
    text: String,
    reasoning: String,
    response_items: Vec<Value>,
    refusal: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
    error: Option<String>,
    error_retryable: bool,
    completed: bool,
}

impl ResponsesAccumulator {
    fn is_terminal(&self) -> bool {
        self.completed || self.error.is_some()
    }

    fn stream_error(&self, message: impl Into<String>) -> anyhow::Error {
        OpenAiResponseError {
            message: message.into(),
            usage: self.usage.clone().unwrap_or_default(),
            retryable: true,
            billing_unknown: true,
        }
        .into()
    }

    fn apply_event(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                }
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.reasoning.push_str(delta);
                }
            }
            Some("response.refusal.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.refusal.push_str(delta);
                }
            }
            Some("response.output_item.done") => self.apply_output_item(&event["item"]),
            Some("response.completed") => {
                self.completed = true;
                self.usage = responses_event_usage(event);
            }
            Some("response.failed") => {
                self.usage = responses_event_usage(event);
                let error = event.pointer("/response/error").unwrap_or(&Value::Null);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("OpenAI response failed");
                let error_type = error.get("type").and_then(Value::as_str);
                let error_code = error.get("code").and_then(Value::as_str);
                self.error_retryable =
                    crate::retry::classify(None, error_type, error_code, message)
                        .is_retryable(false);
                self.error = Some(message.to_string());
            }
            Some("response.incomplete") => {
                self.usage = responses_event_usage(event);
                self.error = Some(
                    event
                        .pointer("/response/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("OpenAI response was incomplete")
                        .to_string(),
                );
            }
            Some("error") => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("OpenAI response stream failed");
                let error_code = event.get("code").and_then(Value::as_str);
                self.error_retryable =
                    crate::retry::classify(None, None, error_code, message).is_retryable(false);
                self.error = Some(message.to_string());
            }
            _ => {}
        }
    }

    fn apply_output_item(&mut self, item: &Value) {
        self.response_items.push(item.clone());
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            return;
        }
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            return;
        };
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let raw_arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let fn_arguments = serde_json::from_str(raw_arguments)
            .unwrap_or_else(|_| Value::String(raw_arguments.to_string()));
        self.tool_calls.push(ToolCall {
            call_id,
            fn_name: name.to_string(),
            fn_arguments,
        });
    }

    fn finish(self, model: &str) -> Result<ChatResponse> {
        if let Some(error) = self.error {
            let billing_unknown = self.error_retryable && self.usage.is_none();
            return Err(OpenAiResponseError {
                message: error,
                usage: self.usage.unwrap_or_default(),
                retryable: self.error_retryable,
                billing_unknown,
            }
            .into());
        }
        if !self.completed {
            return Err(
                self.stream_error("OpenAI Responses API stream ended before response.completed")
            );
        }

        let text_capacity = if self.text.is_empty() { 0 } else { 1 };
        let mut parts = Vec::with_capacity(self.tool_calls.len() + text_capacity);
        if !self.refusal.trim().is_empty() {
            parts.push(ContentPart::Text(format!(
                "Model refusal: {}",
                self.refusal
            )));
        } else if !self.text.trim().is_empty() {
            parts.push(ContentPart::Text(self.text));
        }
        parts.extend(self.tool_calls.into_iter().map(ContentPart::ToolCall));

        let content = if parts.is_empty() {
            MessageContent::default()
        } else {
            MessageContent::from_parts(parts)
        };

        Ok(ChatResponse {
            content,
            reasoning_content: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            reasoning_signature: if self.response_items.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&self.response_items)?)
            },
            model: model.to_string(),
            usage: self.usage.unwrap_or_default(),
            // The Responses API reports running out of output budget as a
            // `response.incomplete` event, which this path turns into an error
            // before reaching here -- so a response that gets built is one that
            // finished.
            truncated: false,
        })
    }
}

// ============================================================================
// Serialization helpers (request → JSON Value)
// ============================================================================

/// Convert ChatMessage list to OpenAI-compatible JSON array. With
/// `replay_reasoning`, assistant turns carry their `reasoning_content` back to
/// the provider; strict servers reject that field, so they pass `false`.
fn serialize_messages(messages: &[ChatMessage], replay_reasoning: bool) -> Value {
    let mut out = Vec::with_capacity(messages.len());

    for msg in messages {
        match msg.role {
            ChatRole::System | ChatRole::User => {
                let text = extract_text_content(&msg.content);
                out.push(json!({
                    "role": role_str(&msg.role),
                    "content": text,
                }));
            }
            ChatRole::Assistant => {
                let tool_calls: Vec<Value> = (&msg.content)
                    .into_iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolCall(tc) => Some(json!({
                            "id": tc.call_id,
                            "type": "function",
                            "function": {
                                "name": tc.fn_name,
                                "arguments": tc.fn_arguments.to_string(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                let text = (&msg.content).into_iter().find_map(|p| match p {
                    ContentPart::Text(t) => Some(t.clone()),
                    _ => None,
                });

                let mut m = json!({
                    "role": "assistant",
                    "content": null,
                });

                if tool_calls.is_empty() {
                    m["content"] = json!(text.unwrap_or_default());
                } else {
                    if let Some(t) = text {
                        m["content"] = json!(t);
                    }
                    m["tool_calls"] = json!(tool_calls);
                }

                if replay_reasoning && let Some(ref rc) = msg.reasoning_content {
                    m["reasoning_content"] = json!(rc);
                }

                out.push(m);
            }
            ChatRole::Tool => {
                // Each ToolResponse becomes a separate tool message
                for part in &msg.content {
                    if let ContentPart::ToolResponse(tr) = part {
                        out.push(json!({
                            "role": "tool",
                            "content": tr.content,
                            "tool_call_id": tr.call_id,
                        }));
                    }
                }
            }
        }
    }

    Value::Array(out)
}

fn serialize_responses_input(messages: &[ChatMessage]) -> Result<Value> {
    let mut input = Vec::with_capacity(messages.len());

    for message in messages {
        match message.role {
            ChatRole::System | ChatRole::User => input.push(json!({
                "role": role_str(&message.role),
                "content": extract_text_content(&message.content),
            })),
            ChatRole::Assistant => {
                let mut replayed_items = false;
                if let Some(signature) = &message.reasoning_signature {
                    let items = serde_json::from_str::<Vec<Value>>(signature)
                        .context("invalid OpenAI reasoning signature")?;
                    replayed_items = !items.is_empty();
                    input.extend(items);
                }
                if replayed_items {
                    continue;
                }
                let text = extract_text_content(&message.content);
                if !text.is_empty() {
                    input.push(json!({ "role": "assistant", "content": text }));
                }
                for part in &message.content {
                    if let ContentPart::ToolCall(tool_call) = part {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": tool_call.call_id,
                            "name": tool_call.fn_name,
                            "arguments": tool_call.fn_arguments.to_string(),
                        }));
                    }
                }
            }
            ChatRole::Tool => {
                for part in &message.content {
                    if let ContentPart::ToolResponse(tool_response) = part {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_response.call_id,
                            "output": tool_response.content,
                        }));
                    }
                }
            }
        }
    }

    Ok(Value::Array(input))
}

/// Extract all text parts from a MessageContent, concatenated.
fn extract_text_content(content: &MessageContent) -> String {
    let texts: Vec<&str> = content
        .into_iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    texts.join("\n\n")
}

fn role_str(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

/// Convert Tool definitions to OpenAI-compatible JSON array.
fn serialize_tools(tools: &[Tool]) -> Value {
    let out: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut func = json!({ "name": t.name });
            if let Some(ref desc) = t.description {
                func["description"] = json!(desc);
            }
            if let Some(ref schema) = t.schema {
                func["parameters"] = schema.clone();
            }
            if let Some(strict) = t.strict {
                func["strict"] =
                    json!(strict && t.schema.as_ref().is_some_and(strict_schema_supported));
            }
            json!({
                "type": "function",
                "function": func,
            })
        })
        .collect();
    Value::Array(out)
}

fn serialize_responses_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let mut value = json!({ "type": "function", "name": tool.name });
                if let Some(description) = &tool.description {
                    value["description"] = json!(description);
                }
                if let Some(schema) = &tool.schema {
                    value["parameters"] = schema.clone();
                }
                if let Some(strict) = tool.strict {
                    let strict =
                        strict && tool.schema.as_ref().is_some_and(strict_schema_supported);
                    value["strict"] = json!(strict);
                }
                value
            })
            .collect(),
    )
}

/// Apply reasoning effort settings to the request body.
fn apply_reasoning_effort(body: &mut Value, effort: &ReasoningEffort, provider: StreamingProvider) {
    let level = match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
    };
    match provider {
        // Aqueduct rejects `reasoning_effort`, so retain `enable_thinking` on
        // the custom-provider path. `preserve_thinking` is a separate extension
        // that custom endpoints may reject even when they accept `enable_thinking`.
        StreamingProvider::WorkersAi | StreamingProvider::SelfHosted => {
            body["enable_thinking"] = json!(level);
            if provider == StreamingProvider::WorkersAi {
                body["preserve_thinking"] = "true".into();
            }
        }
        StreamingProvider::OpenAi => {
            body["reasoning_effort"] = json!(level);
        }
        // Mistral rejects `enable_thinking` and validates `reasoning_effort`
        // per model. None of its reasoning models accept "medium"; "high" is
        // the one value they all take, so Medium rounds up to it.
        StreamingProvider::Mistral => {
            let level = match effort {
                ReasoningEffort::Medium => "high",
                _ => level,
            };
            body["reasoning_effort"] = json!(level);
        }
    }
}

fn responses_reasoning_effort_str(effort: &ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
    }
}

/// Convert StructuredOutput to OpenAI-compatible response_format JSON.
fn serialize_structured_output(output: &StructuredOutput) -> Value {
    match output {
        StructuredOutput::JsonSchema { name, schema } => {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": name,
                    "strict": true,
                    "schema": schema,
                }
            })
        }
    }
}

fn serialize_responses_structured_output(output: &StructuredOutput) -> Value {
    match output {
        StructuredOutput::JsonSchema { name, schema } => json!({
            "format": {
                "type": "json_schema",
                "name": name,
                "strict": strict_schema_supported(schema),
                "schema": schema,
            }
        }),
    }
}

// ============================================================================
// Response parsing helpers
// ============================================================================

/// Parse a usage JSON object into our Usage type.
fn parse_usage(u: &Value) -> Usage {
    let prompt_tokens = u
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .and_then(|tokens| i32::try_from(tokens).ok());
    let completion_tokens = u
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .and_then(|tokens| i32::try_from(tokens).ok());

    let cache_details = u.get("prompt_tokens_details").map(|d| CacheDetails {
        read_tokens: d
            .get("cached_tokens")
            .and_then(|v| v.as_i64())
            .and_then(|tokens| i32::try_from(tokens).ok()),
        write_tokens: None,
    });

    Usage {
        prompt_tokens,
        completion_tokens,
        cache_details,
    }
}

fn parse_responses_usage(usage: &Value) -> Usage {
    Usage {
        prompt_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .and_then(|tokens| i32::try_from(tokens).ok()),
        completion_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .and_then(|tokens| i32::try_from(tokens).ok()),
        cache_details: usage
            .get("input_tokens_details")
            .map(|details| CacheDetails {
                read_tokens: details
                    .get("cached_tokens")
                    .and_then(Value::as_i64)
                    .and_then(|tokens| i32::try_from(tokens).ok()),
                write_tokens: None,
            }),
    }
}

fn responses_event_usage(event: &Value) -> Option<Usage> {
    let usage = event.pointer("/response/usage")?;
    if !usage.is_object() {
        return None;
    }
    let usage = parse_responses_usage(usage);
    if usage.prompt_tokens.is_none() && usage.completion_tokens.is_none() {
        return None;
    }
    Some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_provider_reasoning_omits_preserve_thinking() {
        let mut body = json!({});

        apply_reasoning_effort(
            &mut body,
            &ReasoningEffort::High,
            StreamingProvider::SelfHosted,
        );

        assert_eq!(body, json!({ "enable_thinking": "high" }));
    }

    #[test]
    fn workers_ai_reasoning_keeps_preserve_thinking() {
        let mut body = json!({});

        apply_reasoning_effort(
            &mut body,
            &ReasoningEffort::High,
            StreamingProvider::WorkersAi,
        );

        assert_eq!(
            body,
            json!({ "enable_thinking": "high", "preserve_thinking": "true" })
        );
    }

    #[test]
    fn mistral_reasoning_uses_reasoning_effort_and_rounds_medium_up() {
        let mut body = json!({});
        apply_reasoning_effort(
            &mut body,
            &ReasoningEffort::Medium,
            StreamingProvider::Mistral,
        );
        assert_eq!(body, json!({ "reasoning_effort": "high" }));

        let mut body = json!({});
        apply_reasoning_effort(&mut body, &ReasoningEffort::Low, StreamingProvider::Mistral);
        assert_eq!(body, json!({ "reasoning_effort": "low" }));
    }

    #[test]
    fn detects_mistral_base_url() {
        assert!(is_mistral_base_url("https://api.mistral.ai/v1"));
        assert!(is_mistral_base_url("https://API.Mistral.ai/v1/"));
        assert!(!is_mistral_base_url("https://api.lyceum.technology/v2"));
        assert!(!is_mistral_base_url("https://notmistral.ai/v1"));
        assert!(!is_mistral_base_url("https://mistral.ai.example.com/v1"));
    }

    #[test]
    fn strict_providers_drop_replayed_reasoning() {
        let mut message = ChatMessage::assistant(MessageContent::from_text("done"));
        message.reasoning_content = Some("because".to_string());

        let replayed = serialize_messages(std::slice::from_ref(&message), true);
        assert_eq!(replayed[0]["reasoning_content"], "because");

        let stripped = serialize_messages(&[message], false);
        assert!(stripped[0].get("reasoning_content").is_none());
        assert_eq!(stripped[0]["content"], "done");
    }

    #[test]
    fn content_blocks_split_text_and_thinking() {
        let blocks = json!([
            { "type": "thinking", "thinking": [{ "type": "text", "text": "Let me" }], "closed": true },
            { "type": "thinking", "thinking": " think" },
            { "type": "text", "text": "Hi" },
        ]);
        let (mut text, mut reasoning) = (String::new(), String::new());

        assert!(push_content_blocks(
            blocks.as_array().unwrap(),
            &mut text,
            &mut reasoning
        ));
        assert_eq!(text, "Hi");
        assert_eq!(reasoning, "Let me think");
        assert!(!push_content_blocks(&[], &mut text, &mut reasoning));
    }

    #[test]
    fn empty_repeated_tool_call_name_does_not_clobber_the_first() {
        // Shape Mistral streams: the name and id arrive once, then every
        // argument fragment repeats `"name": ""`.
        let chunks = [
            json!([{ "id": "call-1", "type": "function", "index": 0,
                     "function": { "name": "todo", "arguments": "{\"items\": " } }]),
            json!([{ "index": 0, "function": { "name": "", "arguments": "[\"a\"]}" } }]),
        ];
        let mut tool_calls = Vec::new();
        for chunk in &chunks {
            accumulate_tool_call_deltas(&mut tool_calls, chunk.as_array().unwrap());
        }

        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call-1");
        assert_eq!(tool_calls[0].name, "todo");
        assert_eq!(tool_calls[0].arguments, "{\"items\": [\"a\"]}");
    }

    #[test]
    fn assistant_tool_calls_preserve_null_content() {
        let message = ChatMessage::assistant(MessageContent::from_tool_calls(vec![ToolCall {
            call_id: "call-1".to_string(),
            fn_name: "read_file".to_string(),
            fn_arguments: json!({ "path": "README.md" }),
        }]));

        let messages = serialize_messages(&[message], true);

        assert_eq!(messages[0]["content"], Value::Null);
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call-1");
    }

    #[test]
    fn strips_only_the_selected_provider_prefix() {
        assert_eq!(
            strip_model_prefix("openai/gpt-5.6", StreamingProvider::OpenAi),
            "gpt-5.6"
        );
        assert_eq!(
            strip_model_prefix("workers-ai/model", StreamingProvider::OpenAi),
            "workers-ai/model"
        );
    }

    #[test]
    fn responses_request_preserves_reasoning_and_tools() {
        let request = ChatRequest::new(vec![ChatMessage::user("probe")]).with_tools(vec![
            Tool::new("probe")
                .with_schema(json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"],
                    "additionalProperties": false,
                }))
                .with_strict(true),
        ]);
        let options = ChatOptions::default()
            .with_temperature(1.0)
            .with_max_tokens(4096)
            .with_reasoning_effort(ReasoningEffort::High);

        let body = build_responses_request("gpt-5.6", &request, Some(&options)).unwrap();

        assert_eq!(body["model"], "gpt-5.6");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "probe");
        assert_eq!(body["tools"][0]["strict"], true);
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn responses_disables_strict_mode_for_conditional_tool_schema() {
        let tools = vec![
            Tool::new("submit_result")
                .with_schema(json!({
                    "type": "object",
                    "properties": {
                        "verdict": { "type": "string" },
                        "title": { "type": "string" },
                    },
                    "required": ["verdict"],
                    "additionalProperties": false,
                    "if": { "properties": { "verdict": { "const": "confirmed" } } },
                    "then": { "required": ["verdict", "title"] },
                }))
                .with_strict(true),
        ];

        let serialized = serialize_responses_tools(&tools);

        assert_eq!(serialized[0]["strict"], false);
        assert!(serialized[0]["parameters"].get("if").is_some());
    }

    #[test]
    fn openai_api_key_defaults_to_the_public_endpoint() {
        assert!(StreamingClient::new_openai(OpenAiAuth::ApiKey("sk-test".into()), None).is_ok());
    }

    #[test]
    fn opencode_token_requires_an_explicit_endpoint() {
        let error = StreamingClient::new_openai(OpenAiAuth::OpencodeToken("token".into()), None)
            .err()
            .expect("a gateway token without an endpoint must be rejected");
        assert!(error.to_string().contains("FLAWRENCE_OPENAI_BASE_URL"));
        assert!(
            StreamingClient::new_openai(
                OpenAiAuth::OpencodeToken("token".into()),
                Some("https://gateway.example/v1".into())
            )
            .is_ok()
        );
    }

    #[test]
    fn opencode_config_omits_authorization_header() {
        let config =
            OpencodeConfig::new("https://example.com/v1".to_string(), "token".to_string()).unwrap();
        let headers = config.headers();

        assert_eq!(headers["cf-access-token"], "token");
        assert!(!headers.contains_key(reqwest_compat::header::AUTHORIZATION));
    }

    #[test]
    fn responses_input_replays_output_items_without_reconstruction() {
        let reasoning_item = json!({
            "type": "reasoning",
            "encrypted_content": "opaque",
            "summary": [],
        });
        let function_item = json!({
            "type": "function_call",
            "call_id": "call-1",
            "name": "probe",
            "arguments": "{\"value\":\"pong\"}",
            "phase": "commentary"
        });
        let mut assistant =
            ChatMessage::assistant(MessageContent::from_parts(vec![ContentPart::ToolCall(
                ToolCall {
                    call_id: "call-1".to_string(),
                    fn_name: "probe".to_string(),
                    fn_arguments: json!({ "value": "pong" }),
                },
            )]));
        assistant.reasoning_signature = Some(json!([reasoning_item, function_item]).to_string());

        let input = serialize_responses_input(&[assistant]).unwrap();

        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input.as_array().unwrap().len(), 2);
    }

    #[test]
    fn responses_accumulator_rejects_truncated_stream() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply_event(&json!({
            "type": "response.output_text.delta",
            "delta": "partial",
        }));

        let error = accumulator.finish("openai/gpt-5.6").unwrap_err();
        let response_error = error.downcast_ref::<OpenAiResponseError>().unwrap();
        assert!(response_error.retryable());
        assert!(response_error.retryable_with_budget(false));
        assert!(!response_error.retryable_with_budget(true));
    }

    /// An async-openai stream error whose display is exactly `message`, standing
    /// in for a provider that streamed the failure with no typed status.
    fn stream_error(message: &str) -> OpenAIError {
        OpenAIError::StreamError(Box::new(async_openai::error::StreamError::EventStream(
            message.to_string(),
        )))
    }

    #[test]
    fn bare_vllm_context_rejection_is_not_retried_after_sdk_deserialization_fails() {
        let body = r#"{"object":"error","message":"Requested token count exceeds the model's maximum context length of 524288 tokens. You requested a total of 528140 tokens: 462604 tokens from the input messages and 65536 tokens for the completion.","type":"BadRequestError","param":null,"code":400}"#;
        let parse_error =
            serde_json::from_str::<async_openai::error::WrappedError>(body).unwrap_err();
        let sdk_error = OpenAIError::JSONDeserialize(parse_error, body.to_string());
        let error = map_stream_error("error in SSE stream chunk", &sdk_error);
        let classified = error.downcast_ref::<StreamingRequestError>().unwrap();
        assert!(!classified.retryable());
        assert!(!classified.billing_unknown());
        assert!(error.to_string().contains("462604 tokens"));
        assert!(!error.to_string().contains("missing field"));
    }

    #[test]
    fn embedded_http_codes_are_structural_and_sdk_suffixes_are_ignored() {
        for (body, retryable) in [
            (
                r#"{"object":"error","message":"bad request","code":"400"} (code: 400)"#,
                false,
            ),
            (
                r#"{"error":{"message":"Authentication service temporarily unavailable","code":503}}"#,
                true,
            ),
            (
                r#"{"object":"error","message":"slow down","code":429}"#,
                true,
            ),
        ] {
            let error = map_stream_error("stream", &stream_error(body));
            assert_eq!(
                error
                    .downcast_ref::<StreamingRequestError>()
                    .unwrap()
                    .retryable(),
                retryable
            );
        }
        let embedded =
            parse_embedded_error(r#"{"errors":[{"message":"timeout","code":3046}]}"#).unwrap();
        assert_eq!(embedded.status, None);
        assert_eq!(embedded.error_code.as_deref(), Some("3046"));
    }

    #[test]
    fn ambiguous_transport_errors_are_marked_as_billing_unknown() {
        let error = map_stream_error("request failed", &stream_error("connection reset by peer"));
        let request_error = error.downcast_ref::<StreamingRequestError>().unwrap();
        assert!(request_error.retryable());
        assert!(request_error.billing_unknown());
        assert!(!request_error.retryable_with_budget(true));
        assert!(request_error.retryable_with_budget(false));

        let error = map_stream_error("request failed", &stream_error("429 too many requests"));
        let request_error = error.downcast_ref::<StreamingRequestError>().unwrap();
        assert!(request_error.retryable());
        assert!(!request_error.billing_unknown());

        for error_message in [
            "503 service unavailable",
            r#"provider error: {"errors":[{"message":"Request timeout","code":3046}]}"#,
        ] {
            let error = map_stream_error("request failed", &stream_error(error_message));
            let request_error = error.downcast_ref::<StreamingRequestError>().unwrap();
            assert!(request_error.retryable());
            assert!(request_error.billing_unknown());
            assert!(!request_error.retryable_with_budget(true));
        }
    }

    #[test]
    fn a_status_code_on_the_transport_error_beats_a_misleading_body() {
        // A 503 whose body mentions "authentication": the structured status
        // must win, so this retries rather than dying as a permanent auth error.
        let api = OpenAIError::ApiError(async_openai::error::ApiError {
            message: r#"{"detail":"Authentication service temporarily unavailable"}"#.to_string(),
            r#type: None,
            param: None,
            code: None,
        });
        let error = map_stream_error("stream", &api);
        let request_error = error.downcast_ref::<StreamingRequestError>().unwrap();
        assert!(request_error.retryable());

        // A structured invalid_request_error stays permanent.
        let api = OpenAIError::ApiError(async_openai::error::ApiError {
            message: "Model does not support tools".to_string(),
            r#type: Some("invalid_request_error".to_string()),
            param: None,
            code: None,
        });
        let error = map_stream_error("stream", &api);
        assert!(
            !error
                .downcast_ref::<StreamingRequestError>()
                .unwrap()
                .retryable()
        );
    }

    #[test]
    fn incomplete_response_preserves_usage_and_is_not_retryable() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply_event(&json!({
            "type": "response.incomplete",
            "response": {
                "usage": { "input_tokens": 12, "output_tokens": 34 },
                "incomplete_details": { "reason": "max_output_tokens" }
            }
        }));

        let error = accumulator.finish("openai/gpt-5.6").unwrap_err();
        let response_error = error.downcast_ref::<OpenAiResponseError>().unwrap();
        assert_eq!(response_error.usage().prompt_tokens, Some(12));
        assert_eq!(response_error.usage().completion_tokens, Some(34));
        assert!(!response_error.retryable());
    }

    #[test]
    fn failed_response_with_null_usage_is_billing_unknown() {
        let mut accumulator = ResponsesAccumulator::default();
        accumulator.apply_event(&json!({
            "type": "response.failed",
            "response": {
                "usage": null,
                "error": {
                    "type": "server_error",
                    "message": "temporary upstream failure"
                }
            }
        }));

        let error = accumulator.finish("openai/gpt-5.6").unwrap_err();
        let response_error = error.downcast_ref::<OpenAiResponseError>().unwrap();
        assert!(response_error.retryable());
        assert!(response_error.retryable_with_budget(false));
        assert!(!response_error.retryable_with_budget(true));
    }

    #[test]
    fn strict_schema_requires_object_root_and_exact_required_set() {
        assert!(!strict_schema_supported(&json!({
            "type": "array",
            "items": { "type": "string" }
        })));
        assert!(!strict_schema_supported(&json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value", "value"],
            "additionalProperties": false
        })));
    }

    #[test]
    fn invalid_reasoning_signature_is_rejected() {
        let mut assistant = ChatMessage::assistant("result");
        assistant.reasoning_signature = Some("not-json".to_string());

        assert!(serialize_responses_input(&[assistant]).is_err());
    }

    #[test]
    fn responses_maps_max_reasoning_to_max() {
        assert_eq!(responses_reasoning_effort_str(&ReasoningEffort::Max), "max");
    }

    #[test]
    fn permanent_stream_errors_are_not_retryable() {
        assert!(!retryable_error_message("401 unauthorized", true));
        assert!(!retryable_error_message("invalid_request_error", true));
        assert!(retryable_error_message("429 rate limit", false));
        assert!(retryable_error_message("upstream timeout", false));
    }

    #[test]
    fn a_transiently_down_auth_service_is_retryable_despite_the_word_authentication() {
        // The 503 body that aborted a consolidation step: transient, but its
        // "authentication" used to match the permanent list first.
        assert!(retryable_error_message(
            "error in SSE stream chunk: {\"detail\":\"Authentication service temporarily unavailable\"}",
            false,
        ));
        assert!(retryable_error_message("503 Service Unavailable", false));
        assert!(retryable_error_message(
            "model overloaded, try again",
            false
        ));
    }

    #[test]
    fn a_real_auth_failure_stays_permanent() {
        // A genuine bad-key rejection has no transient phrase, so it must not
        // be retried into the wall-clock limit.
        assert!(!retryable_error_message(
            "401 Unauthorized: authentication failed",
            true,
        ));
    }
}

#[cfg(test)]
mod truncation_signal {
    use super::*;

    /// `length` is the provider stating it stopped at the output limit. Without
    /// reading it, a tool call missing its last twelve fields is
    /// indistinguishable from a model that chose not to write them -- and only
    /// one of those has advice the model can act on.
    #[test]
    fn a_finish_reason_of_length_is_what_marks_a_response_truncated() {
        let cut = serde_json::json!({"choices":[{"delta":{},"finish_reason":"length"}]});
        let done = serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        let mid = serde_json::json!({"choices":[{"delta":{"content":"x"}}]});

        let reason = |chunk: &Value| {
            chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str)
                .map(|r| r == "length")
        };

        assert_eq!(reason(&cut), Some(true));
        assert_eq!(reason(&done), Some(false));
        // Mid-stream chunks carry no finish reason and must not clear it.
        assert_eq!(reason(&mid), None);
    }

    #[test]
    fn chat_body_omits_reasoning_effort_for_a_rejected_model() {
        let request = ChatRequest::new(vec![ChatMessage::user("hi")]);
        let options = ChatOptions::default().with_reasoning_effort(ReasoningEffort::High);

        let with = build_chat_body(
            &request,
            "ministral-3b",
            Some(&options),
            StreamingProvider::Mistral,
            false,
        );
        assert_eq!(with["reasoning_effort"], json!("high"));

        let without = build_chat_body(
            &request,
            "ministral-3b",
            Some(&options),
            StreamingProvider::Mistral,
            true,
        );
        assert!(without.get("reasoning_effort").is_none());
        // Unrelated options still pass through.
        assert_eq!(without["model"], json!("ministral-3b"));
    }

    #[test]
    fn effort_rejection_matches_the_field_not_one_sentence() {
        assert!(effort_rejection(
            "reasoning_effort is not enabled for this model"
        ));
        assert!(effort_rejection(
            "invalid_request_error: unknown field reasoning_effort (code: 3051)"
        ));
        assert!(effort_rejection(
            "Reasoning effort is disabled for this model"
        ));
        assert!(!effort_rejection("rate limit exceeded"));
        assert!(!effort_rejection("model not found"));
    }

    /// A minimal successful chat-completions SSE response.
    const MOCK_SUCCESS_SSE: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "content-type: text/event-stream\r\n",
        "connection: close\r\n\r\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    /// Read one HTTP request off `listener`, record its body, and answer with
    /// Mistral's effort-rejection on the first request and `MOCK_SUCCESS_SSE`
    /// afterwards — exactly the sequence the fallback is supposed to handle.
    fn mock_provider_body(listener: &std::net::TcpListener, bodies: &Mutex<Vec<String>>) {
        use std::io::{Read, Write};
        let (mut socket, _) = listener.accept().expect("mock provider: accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let request_body = loop {
            if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if buf.len() >= body_start + content_length {
                    break String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                        .into_owned();
                }
            }
            let read = socket.read(&mut chunk).expect("mock provider: read");
            assert!(read > 0, "mock provider: connection closed mid-request");
            buf.extend_from_slice(&chunk[..read]);
        };
        let count = {
            let mut all = bodies.lock().unwrap();
            all.push(request_body);
            all.len()
        };
        let (status_line, content_type, payload) = if count == 1 {
            let error = r#"{"error":{"message":"reasoning_effort is not enabled for this model","type":"invalid_request_error","code":"3051"}}"#;
            (
                "HTTP/1.1 400 Bad Request",
                "application/json",
                error.to_string(),
            )
        } else {
            (
                "HTTP/1.1 200 OK",
                "text/event-stream",
                MOCK_SUCCESS_SSE.into(),
            )
        };
        let response = format!(
            "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        socket
            .write_all(response.as_bytes())
            .expect("mock provider: write");
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }

    /// The fallback runs against the real client path: the first request
    /// carries `reasoning_effort` and fails with Mistral error 3051, the retry
    /// omits the field, and the model is remembered so later calls skip the
    /// failing request entirely. Delete the fallback and this fails.
    #[tokio::test]
    async fn effort_fallback_retries_once_then_skips_the_field() {
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let server_bodies = Arc::clone(&bodies);
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                mock_provider_body(&listener, &server_bodies);
            }
        });

        let http = reqwest_compat::Client::builder().build().unwrap();
        let config = OpenAIConfig::new()
            .with_api_base(base_url)
            .with_api_key("test-key".to_string())
            .with_header("User-Agent", crate::USER_AGENT)
            .unwrap();
        let streaming = StreamingClient {
            client: Client::with_config(Box::new(config) as Box<dyn OpenAIClientConfig>)
                .with_http_client(http),
            provider: StreamingProvider::Mistral,
            effort_rejected: Mutex::new(HashSet::new()),
        };

        let options = ChatOptions::default().with_reasoning_effort(ReasoningEffort::High);
        let request = ChatRequest::new(vec![ChatMessage::user("hi")]);

        let response = streaming
            .chat_completion("ministral-3b", request.clone(), Some(&options))
            .await
            .unwrap();
        assert_eq!(response.content.first_text(), Some("ok"));

        {
            let all = bodies.lock().unwrap();
            assert_eq!(all.len(), 2, "expected the failed request plus one retry");
            assert!(
                all[0].contains("reasoning_effort"),
                "first request should carry the field: {}",
                all[0]
            );
            assert!(
                !all[1].contains("reasoning_effort"),
                "retry should omit the field: {}",
                all[1]
            );
        }

        // The model is remembered: the next call sends a single request,
        // already without the field.
        let response = streaming
            .chat_completion("ministral-3b", request, Some(&options))
            .await
            .unwrap();
        assert_eq!(response.content.first_text(), Some("ok"));
        let all = bodies.lock().unwrap();
        assert_eq!(all.len(), 3);
        assert!(
            !all[2].contains("reasoning_effort"),
            "remembered model should skip the field: {}",
            all[2]
        );
        drop(all);
        server.join().unwrap();
    }
}
