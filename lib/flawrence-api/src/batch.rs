//! Workers AI Batch API client.
//!
//! Sends chat completion requests via the Cloudflare Workers AI
//! `/ai/run/<model>?queueRequest=true` REST endpoint and polls for results
//! with gradual backoff (7.5s -> 15s -> 30s -> 60s, capped at 60s).

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::chat_client::ChatClient;
use flawrence_chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole, ContentPart, MessageContent,
    ReasoningEffort, StructuredOutput, Tool, ToolCall, Usage,
};

// ============================================================================
// Workers AI request types (serialized to JSON)
// ============================================================================

/// Batch submit envelope: wraps one or more requests.
#[derive(Serialize)]
struct BatchSubmitRequest {
    requests: Vec<BatchRequestItem>,
}

/// A single inference request within the batch.
#[derive(Serialize)]
struct BatchRequestItem {
    messages: Vec<WaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WaiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

/// A chat message in OpenAI-compatible format for the Workers AI native endpoint.
#[derive(Serialize)]
struct WaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WaiToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// Tool call in an assistant message (outbound serialization).
#[derive(Serialize)]
struct WaiToolCallOut {
    id: String,
    r#type: String,
    function: WaiFunctionOut,
}

#[derive(Serialize)]
struct WaiFunctionOut {
    name: String,
    arguments: String,
}

/// Tool definition in OpenAI format.
#[derive(Serialize)]
struct WaiTool {
    r#type: String,
    function: WaiToolFunction,
}

#[derive(Serialize)]
struct WaiToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

/// Poll request: send the request_id to retrieve results.
#[derive(Serialize)]
struct PollRequest {
    request_id: String,
}

// ============================================================================
// Workers AI response types (deserialized from JSON)
// ============================================================================

/// Top-level Cloudflare API envelope.
#[derive(Deserialize)]
struct CfApiResponse {
    result: serde_json::Value,
    success: bool,
    #[serde(default)]
    errors: Vec<CfApiError>,
}

#[derive(Deserialize)]
struct CfApiError {
    message: String,
    #[allow(dead_code)]
    #[serde(default)]
    code: Option<u32>,
}

/// Queued/running status returned before the batch completes.
#[derive(Deserialize)]
struct QueuedResult {
    #[allow(dead_code)]
    status: String,
    request_id: String,
}

/// Completed batch result: contains one response per submitted request.
#[derive(Deserialize)]
struct BatchCompletedResult {
    responses: Vec<BatchResponseItem>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
struct BatchResponseItem {
    result: OaiChatCompletion,
    success: bool,
}

/// OpenAI-compatible ChatCompletion object (nested inside batch response).
#[derive(Deserialize)]
struct OaiChatCompletion {
    #[serde(default)]
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OaiChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct OaiToolCall {
    id: String,
    function: OaiFunction,
}

#[derive(Deserialize)]
struct OaiFunction {
    name: String,
    /// Stringified JSON — we parse into serde_json::Value.
    arguments: String,
}

#[derive(Deserialize)]
struct OaiUsage {
    #[serde(default)]
    prompt_tokens: Option<i32>,
    #[serde(default)]
    completion_tokens: Option<i32>,
    #[allow(dead_code)]
    #[serde(default)]
    total_tokens: Option<i32>,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptTokensDetails>,
}

#[derive(Deserialize)]
struct OaiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i32>,
}

// ============================================================================
// BatchClient
// ============================================================================

/// Maximum total polling time before we give up.
const MAX_POLL_DURATION: Duration = Duration::from_secs(48 * 3600); // 48 hours

/// Polling interval for the first 5 minutes.
const FAST_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Polling interval after 5 minutes.
const SLOW_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Threshold at which polling switches from fast to slow.
const SLOW_POLL_THRESHOLD: Duration = Duration::from_secs(300); // 5 minutes

/// Client for the Workers AI asynchronous batch REST API.
pub struct BatchClient {
    http: reqwest::Client,
    api_token: String,
    account_id: String,
}

impl BatchClient {
    pub fn new(api_token: String, account_id: String, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10));

        if let Some(proxy) = proxy_url {
            builder = builder.proxy(reqwest::Proxy::all(proxy).context("Invalid HTTP_PROXY URL")?);
        }

        let http = builder.build().context("Failed to build HTTP client")?;
        Ok(Self {
            http,
            api_token,
            account_id,
        })
    }

    /// Build the endpoint URL for a given model.
    fn endpoint_url(&self, model: &str) -> String {
        format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/ai/run/{}?queueRequest=true",
            self.account_id, model
        )
    }

    /// Submit a batch request and poll until completion.
    async fn run_batch(
        &self,
        model: &str,
        submit_body: BatchSubmitRequest,
    ) -> Result<BatchCompletedResult> {
        let url = self.endpoint_url(model);

        // Step 1: Submit
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("Content-Type", "application/json")
            .json(&submit_body)
            .send()
            .await
            .context("Failed to submit batch request")?;

        let cf_resp: CfApiResponse = resp
            .json()
            .await
            .context("Failed to parse batch submit response")?;

        if !cf_resp.success {
            let msgs: Vec<String> = cf_resp.errors.iter().map(|e| e.message.clone()).collect();
            anyhow::bail!("Workers AI batch submit failed: {}", msgs.join("; "));
        }

        // Check if result is already complete (no queueing needed)
        if cf_resp.result.get("responses").is_some() {
            let completed: BatchCompletedResult = serde_json::from_value(cf_resp.result)
                .context("Failed to parse immediate batch result")?;
            return Ok(completed);
        }

        // Parse as queued
        let queued: QueuedResult =
            serde_json::from_value(cf_resp.result).context("Failed to parse queued response")?;

        let request_id = queued.request_id;
        tracing::info!(request_id = %request_id, model = %model, "Batch request queued, polling for result");

        // Step 2: Poll — every 5s for first 5 minutes, then every 60s
        let poll_start = std::time::Instant::now();

        loop {
            let elapsed = poll_start.elapsed();
            let delay = if elapsed < SLOW_POLL_THRESHOLD {
                FAST_POLL_INTERVAL
            } else {
                SLOW_POLL_INTERVAL
            };
            tokio::time::sleep(delay).await;

            // Check timeout
            if poll_start.elapsed() > MAX_POLL_DURATION {
                anyhow::bail!(
                    "Batch request {} timed out after {:?} of polling",
                    request_id,
                    MAX_POLL_DURATION
                );
            }

            let poll_body = PollRequest {
                request_id: request_id.clone(),
            };

            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_token))
                .header("Content-Type", "application/json")
                .json(&poll_body)
                .send()
                .await
                .context("Failed to poll batch request")?;

            let cf_resp: CfApiResponse =
                resp.json().await.context("Failed to parse poll response")?;

            if !cf_resp.success {
                let msgs: Vec<String> = cf_resp.errors.iter().map(|e| e.message.clone()).collect();
                anyhow::bail!("Workers AI batch poll failed: {}", msgs.join("; "));
            }

            // Check if completed
            if cf_resp.result.get("responses").is_some() {
                let completed: BatchCompletedResult = serde_json::from_value(cf_resp.result)
                    .context("Failed to parse completed batch result")?;
                tracing::info!(
                    request_id = %request_id,
                    elapsed_ms = %poll_start.elapsed().as_millis(),
                    "Batch request completed"
                );
                return Ok(completed);
            }

            // Still queued/running
            let status = cf_resp
                .result
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            tracing::debug!(
                request_id = %request_id,
                status = %status,
                delay_ms = %delay.as_millis(),
                "Still polling..."
            );
        }
    }
}

// ============================================================================
// Conversion helpers
// ============================================================================

/// Convert our ChatMessage list into Workers AI native message format.
fn serialize_messages(messages: &[ChatMessage]) -> Vec<WaiMessage> {
    let mut out = Vec::with_capacity(messages.len());

    for msg in messages {
        match msg.role {
            ChatRole::System | ChatRole::User => {
                let text = extract_text_content(&msg.content);
                out.push(WaiMessage {
                    role: role_str(&msg.role).to_string(),
                    content: Some(text),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            ChatRole::Assistant => {
                let tool_calls: Vec<ToolCall> = (&msg.content)
                    .into_iter()
                    .filter_map(|p| match p {
                        ContentPart::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();

                let text = (&msg.content).into_iter().find_map(|p| match p {
                    ContentPart::Text(t) => Some(t.clone()),
                    _ => None,
                });

                if tool_calls.is_empty() {
                    out.push(WaiMessage {
                        role: "assistant".to_string(),
                        content: text.or(Some(String::new())),
                        reasoning_content: msg.reasoning_content.clone(),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                } else {
                    let oai_calls: Vec<WaiToolCallOut> = tool_calls
                        .iter()
                        .map(|tc| WaiToolCallOut {
                            id: tc.call_id.clone(),
                            r#type: "function".to_string(),
                            function: WaiFunctionOut {
                                name: tc.fn_name.clone(),
                                arguments: tc.fn_arguments.to_string(),
                            },
                        })
                        .collect();

                    out.push(WaiMessage {
                        role: "assistant".to_string(),
                        content: text,
                        reasoning_content: msg.reasoning_content.clone(),
                        tool_calls: Some(oai_calls),
                        tool_call_id: None,
                    });
                }
            }
            ChatRole::Tool => {
                // Each ToolResponse in the content becomes a separate tool message
                for part in &msg.content {
                    if let ContentPart::ToolResponse(tr) = part {
                        out.push(WaiMessage {
                            role: "tool".to_string(),
                            content: Some(tr.content.clone()),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: Some(tr.call_id.clone()),
                        });
                    }
                }
            }
        }
    }

    out
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

/// Convert a StructuredOutput into OpenAI-compatible `response_format` JSON.
fn serialize_structured_output(output: &StructuredOutput) -> serde_json::Value {
    match output {
        StructuredOutput::JsonSchema { name, schema } => {
            serde_json::json!({
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

/// Convert our Tool definitions into OpenAI-compatible format.
fn serialize_tools(tools: &[Tool]) -> Vec<WaiTool> {
    tools
        .iter()
        .map(|t| WaiTool {
            r#type: "function".to_string(),
            function: WaiToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.schema.clone(),
                strict: t.strict.map(|strict| {
                    strict
                        && t.schema
                            .as_ref()
                            .is_some_and(crate::strict::strict_schema_supported)
                }),
            },
        })
        .collect()
}

/// Convert a ReasoningEffort into an OpenAI-compatible string.
fn serialize_reasoning_effort(effort: &ReasoningEffort) -> Option<String> {
    let level = match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
    };
    Some(level.into())
}

/// Convert a completed batch response into our ChatResponse.
fn parse_batch_result(completed: BatchCompletedResult, model: &str) -> Result<ChatResponse> {
    let item = completed
        .responses
        .into_iter()
        .next()
        .context("Batch response contained no items")?;

    if !item.success {
        anyhow::bail!("Batch response item reported failure");
    }

    let choice = item
        .result
        .choices
        .into_iter()
        .next()
        .context("Batch response contained no choices")?;

    let truncated = choice.finish_reason.as_deref() == Some("length");
    let msg = choice.message;

    // Build content parts
    let mut parts: Vec<ContentPart> = Vec::new();

    if let Some(text) = msg.content
        && !text.trim().is_empty()
    {
        parts.push(ContentPart::Text(text));
    }

    if let Some(tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            let arguments: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| {
                    // If arguments aren't valid JSON, wrap as a string
                    serde_json::Value::String(tc.function.arguments.clone())
                });
            parts.push(ContentPart::ToolCall(ToolCall {
                call_id: tc.id,
                fn_name: tc.function.name,
                fn_arguments: arguments,
            }));
        }
    }

    let content = if parts.is_empty() {
        MessageContent::default()
    } else {
        MessageContent::from_parts(parts)
    };

    // Use the inner per-item usage if available, fall back to batch-level usage
    let usage_src = item.result.usage.or(completed.usage);
    let usage = match usage_src {
        Some(u) => Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cache_details: u
                .prompt_tokens_details
                .map(|d| flawrence_chat::CacheDetails {
                    read_tokens: d.cached_tokens,
                    write_tokens: None,
                }),
        },
        None => Usage::default(),
    };

    Ok(ChatResponse {
        content,
        reasoning_content: msg.reasoning_content,
        reasoning_signature: None,
        model: model.to_string(),
        usage,
        truncated,
    })
}

// ============================================================================
// ChatClient impl
// ============================================================================

#[async_trait]
impl ChatClient for BatchClient {
    async fn chat_completion(
        &self,
        model: &str,
        request: ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        // Strip "workers-ai/" prefix to get the CF model name
        let cf_model = model.strip_prefix("workers-ai/").unwrap_or(model);

        let messages = serialize_messages(&request.messages);
        let tools = request
            .tools
            .as_ref()
            .filter(|t| !t.is_empty())
            .map(|t| serialize_tools(t));

        let (max_tokens, temperature) = match options {
            Some(opts) => (opts.max_tokens, opts.temperature),
            None => (None, None),
        };

        let response_format = options
            .and_then(|opts| opts.structured_output.as_ref())
            .map(serialize_structured_output);

        let reasoning_effort = options
            .and_then(|opts| opts.reasoning_effort.as_ref())
            .and_then(serialize_reasoning_effort);

        let submit_body = BatchSubmitRequest {
            requests: vec![BatchRequestItem {
                messages,
                tools,
                max_tokens,
                temperature,
                response_format,
                reasoning_effort,
            }],
        };

        let completed = self.run_batch(cf_model, submit_body).await?;
        parse_batch_result(completed, model)
    }
}
