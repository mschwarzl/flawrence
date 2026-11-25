//! Anthropic chat client using `anthropic-async`.
//!
//! Supports API key authentication only.
//! Uses adaptive thinking with effort mapped from `ReasoningEffort`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use anthropic_async::types::common::CacheControl;
use anthropic_async::types::content::{
    ContentBlock, ContentBlockParam, MessageContentParam, MessageParam, MessageRole, SystemParam,
};
use anthropic_async::types::messages::{MessagesCreateRequest, OutputConfig, ThinkingConfig};
use anthropic_async::types::tools::Tool as AnthropicTool;
use anthropic_async::{AnthropicConfig, BetaFeature, Client as AnthropicSdkClient};

use flawrence_chat::{
    CacheDetails, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole, ContentPart,
    MessageContent, ReasoningEffort, Tool, ToolCall, Usage,
};

use crate::chat_client::ChatClient;

// ============================================================================
// AnthropicClient
// ============================================================================

/// Client for Anthropic models (Claude).
pub struct AnthropicClient {
    api_key: String,
    base_url_override: Option<String>,
}

impl AnthropicClient {
    pub fn new(api_key: String, base_url_override: Option<String>) -> Self {
        Self {
            api_key,
            base_url_override,
        }
    }

    /// Build an `anthropic_async::Client` for the current auth mode.
    ///
    /// The structured-outputs beta is the only beta feature this client needs,
    /// and only requests that constrain the output to a JSON schema use it.
    /// Adaptive thinking, effort and tool use are all GA. Enabling the beta puts
    /// the request under a stricter JSON-schema subset that rejects the
    /// `minimum` our unsigned tool-argument types generate, so it is switched on
    /// only when a request actually asks for structured output.
    async fn build_sdk_client(
        &self,
        structured_outputs: bool,
    ) -> Result<AnthropicSdkClient<AnthropicConfig>> {
        let mut config = AnthropicConfig::new().with_api_key(&self.api_key);
        if structured_outputs {
            config = config.with_beta_features([BetaFeature::StructuredOutputsLatest]);
        }
        if let Some(url) = &self.base_url_override {
            config = config.with_api_base(url.clone());
        }
        Ok(AnthropicSdkClient::with_config(config))
    }

    /// Single attempt at executing a request.
    async fn execute_once(
        &self,
        model: &str,
        request: &ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        // Strip "anthropic/" prefix from model name
        let api_model = model.strip_prefix("anthropic/").unwrap_or(model);

        let default_options = ChatOptions::default();
        let opts = options.unwrap_or(&default_options);
        let client = self
            .build_sdk_client(opts.structured_output.is_some())
            .await?;
        let sdk_request = build_request(api_model, request, opts)?;

        let response = client
            .messages()
            .create(sdk_request)
            .await
            .map_err(|e| anyhow::anyhow!("Anthropic API error: {e}"))?;

        parse_response(model, response)
    }
}

#[async_trait]
impl ChatClient for AnthropicClient {
    async fn chat_completion(
        &self,
        model: &str,
        request: ChatRequest,
        options: Option<&ChatOptions>,
    ) -> Result<ChatResponse> {
        self.execute_once(model, &request, options).await
    }
}

// ============================================================================
// Request building
// ============================================================================

fn build_request(
    model: &str,
    request: &ChatRequest,
    options: &ChatOptions,
) -> Result<MessagesCreateRequest> {
    let (system, messages) = serialize_messages(&request.messages)?;

    let tools = request.tools.as_ref().map(|t| serialize_tools(t));

    let effort_str = match &options.reasoning_effort {
        Some(ReasoningEffort::Low) => "low",
        Some(ReasoningEffort::Medium) => "medium",
        Some(ReasoningEffort::High) | None => "high",
        Some(ReasoningEffort::Max) => "max",
    };

    let mut req = MessagesCreateRequest {
        model: model.to_string(),
        max_tokens: options.max_tokens.unwrap_or(65536),
        system,
        messages,
        thinking: Some(ThinkingConfig::Adaptive),
        output_config: Some(OutputConfig {
            format: None,
            effort: Some(effort_str.to_string()),
        }),
        tools,
        temperature: options.temperature,
        ..Default::default()
    };

    // Structured output → JSON schema output format
    if let Some(ref structured) = options.structured_output {
        match structured {
            flawrence_chat::StructuredOutput::JsonSchema { schema, .. } => {
                req.output_config = Some(OutputConfig {
                    format: Some(anthropic_async::types::messages::OutputFormat::JsonSchema {
                        schema: schema.clone(),
                    }),
                    effort: Some(effort_str.to_string()),
                });
            }
        }
    }

    Ok(req)
}

/// Convert harness messages to Anthropic format.
/// Returns (system_param, message_params).
/// Handles:
/// - System messages → extracted to `system` field
/// - Tool messages → converted to User messages with ToolResult content blocks
/// - Consecutive same-role messages → merged
fn serialize_messages(
    messages: &[ChatMessage],
) -> Result<(Option<SystemParam>, Vec<MessageParam>)> {
    let mut system_text = String::new();
    let mut out: Vec<MessageParam> = Vec::new();

    for msg in messages {
        match msg.role {
            ChatRole::System => {
                if !system_text.is_empty() {
                    system_text.push('\n');
                }
                let text = extract_text(&msg.content);
                system_text.push_str(&text);
            }
            ChatRole::User => {
                let blocks = user_content_blocks(&msg.content);
                push_or_merge(&mut out, MessageRole::User, blocks);
            }
            ChatRole::Assistant => {
                let blocks = assistant_content_blocks(msg);
                push_or_merge(&mut out, MessageRole::Assistant, blocks);
            }
            ChatRole::Tool => {
                // Tool results become User messages with ToolResult content blocks
                let blocks = tool_result_blocks(&msg.content);
                push_or_merge(&mut out, MessageRole::User, blocks);
            }
        }
    }

    let system = if system_text.is_empty() {
        None
    } else {
        Some(SystemParam::String(system_text))
    };

    // Set cache_control on the last content block of the last message
    if let Some(last_msg) = out.last_mut()
        && let MessageContentParam::Blocks(blocks) = &mut last_msg.content
        && let Some(last_block) = blocks.last_mut()
    {
        set_cache_control(last_block);
    }

    Ok((system, out))
}

/// Push blocks to the output, merging with the last message if same role.
fn push_or_merge(out: &mut Vec<MessageParam>, role: MessageRole, blocks: Vec<ContentBlockParam>) {
    if blocks.is_empty() {
        return;
    }

    if let Some(last) = out.last_mut()
        && last.role == role
    {
        // Merge into existing message
        match &mut last.content {
            MessageContentParam::Blocks(existing) => {
                existing.extend(blocks);
            }
            MessageContentParam::String(s) => {
                let mut merged = vec![ContentBlockParam::Text {
                    text: std::mem::take(s),
                    citations: None,
                    cache_control: None,
                }];
                merged.extend(blocks);
                last.content = MessageContentParam::Blocks(merged);
            }
        }
        return;
    }

    out.push(MessageParam {
        role,
        content: MessageContentParam::Blocks(blocks),
    });
}

fn user_content_blocks(content: &MessageContent) -> Vec<ContentBlockParam> {
    content
        .into_iter()
        .filter_map(|part| match part {
            ContentPart::Text(t) => Some(ContentBlockParam::Text {
                text: t.clone(),
                citations: None,
                cache_control: None,
            }),
            _ => None,
        })
        .collect()
}

fn assistant_content_blocks(msg: &ChatMessage) -> Vec<ContentBlockParam> {
    let mut blocks = Vec::new();

    // Prepend thinking block if we have a reasoning signature
    if let Some(ref signature) = msg.reasoning_signature {
        blocks.push(ContentBlockParam::Thinking {
            thinking: msg.reasoning_content.clone().unwrap_or_default(),
            signature: signature.clone(),
        });
    }

    for part in &msg.content {
        match part {
            ContentPart::Text(t) => {
                blocks.push(ContentBlockParam::Text {
                    text: t.clone(),
                    citations: None,
                    cache_control: None,
                });
            }
            ContentPart::ToolCall(tc) => {
                blocks.push(ContentBlockParam::ToolUse {
                    id: tc.call_id.clone(),
                    name: tc.fn_name.clone(),
                    input: tc.fn_arguments.clone(),
                    cache_control: None,
                });
            }
            ContentPart::ToolResponse(_) => {} // Handled separately as User messages
        }
    }

    blocks
}

fn tool_result_blocks(content: &MessageContent) -> Vec<ContentBlockParam> {
    content
        .into_iter()
        .filter_map(|part| match part {
            ContentPart::ToolResponse(tr) => Some(ContentBlockParam::ToolResult {
                tool_use_id: tr.call_id.clone(),
                content: Some(anthropic_async::types::content::ToolResultContent::String(
                    tr.content.clone(),
                )),
                is_error: None,
                cache_control: None,
            }),
            _ => None,
        })
        .collect()
}

fn extract_text(content: &MessageContent) -> String {
    content
        .into_iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Anthropic's custom-tool schema validator rejects JSON Schema's array-typed
/// `type` (e.g. `"type": ["string", "null"]`), which is how `flawrence_tool`
/// renders a nullable enum -- 400s with `Enum value 'function' does not match
/// declared type '['string', 'null']'` even outside strict mode. An earlier
/// version of this function un-flattened such nodes into `anyOf: [{"type":
/// T}, {"type": "null"}]`, which Anthropic *parses*, but enough of these
/// unions summed across one tool's schema then hit a separate "Schema is too
/// complex" 400. Every such field is already non-required (that is what
/// nullable means in this registry -- see `is_nullable`/`means_absent`), so
/// there is a much cheaper fix: drop `"null"` from `type` outright. Omitting
/// an optional key and typing it without `null` are indistinguishable to a
/// tool-calling model, so nothing is lost; `null` is likewise dropped from
/// any sibling `enum` (whitelists in this registry never otherwise include
/// the JSON `null` literal as a real argument value).
fn anthropicize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => {
            let mut object: serde_json::Map<String, Value> = object
                .iter()
                .map(|(k, v)| (k.clone(), anthropicize_schema(v)))
                .collect();
            if let Some(Value::Array(types)) = object.get("type") {
                let non_null: Vec<Value> = types
                    .iter()
                    .filter(|t| t.as_str() != Some("null"))
                    .cloned()
                    .collect();
                match non_null.len() {
                    1 => {
                        object.insert("type".to_string(), non_null[0].clone());
                    }
                    0 => {
                        object.remove("type");
                    }
                    _ => {
                        object.insert("type".to_string(), Value::Array(non_null));
                    }
                }
            }
            if let Some(Value::Array(values)) = object.get("enum")
                && values.iter().any(Value::is_null)
            {
                let non_null: Vec<Value> =
                    values.iter().filter(|v| !v.is_null()).cloned().collect();
                object.insert("enum".to_string(), Value::Array(non_null));
            }
            Value::Object(object)
        }
        Value::Array(items) => Value::Array(items.iter().map(anthropicize_schema).collect()),
        other => other.clone(),
    }
}

fn serialize_tools(tools: &[Tool]) -> Vec<AnthropicTool> {
    tools
        .iter()
        .map(|t| AnthropicTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t
                .schema
                .as_ref()
                .map(anthropicize_schema)
                .unwrap_or(serde_json::json!({"type": "object"})),
            cache_control: None,
            // Strict tool use is GA (no beta header) but enforces a JSON-schema
            // subset, so only mark a tool strict when its schema fits that
            // subset; anything else is sent unconstrained rather than 400ing.
            strict: t.strict.map(|strict| {
                strict
                    && t.schema
                        .as_ref()
                        .is_some_and(crate::strict::strict_schema_supported)
            }),
        })
        .collect()
}

// ============================================================================
// Response parsing
// ============================================================================

fn parse_response(
    full_model: &str,
    response: anthropic_async::types::messages::MessagesCreateResponse,
) -> Result<ChatResponse> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut reasoning_content: Option<String> = None;
    let mut reasoning_signature: Option<String> = None;

    for block in &response.content {
        match block {
            ContentBlock::Text { text, .. } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    call_id: id.clone(),
                    fn_name: name.clone(),
                    fn_arguments: input.clone(),
                });
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                reasoning_content = Some(thinking.clone());
                reasoning_signature = Some(signature.clone());
            }
            ContentBlock::RedactedThinking { data } => {
                // Redacted thinking: no visible content, only signature for round-tripping
                reasoning_signature = Some(data.clone());
            }
            _ => {
                // ServerToolUse, WebSearchToolResult, Unknown — skip
            }
        }
    }

    // Build content parts
    let mut parts: Vec<ContentPart> = Vec::new();
    for text in text_parts {
        if !text.is_empty() {
            parts.push(ContentPart::Text(text));
        }
    }
    for tc in tool_calls {
        parts.push(ContentPart::ToolCall(tc));
    }

    let content = if parts.is_empty() {
        MessageContent::default()
    } else {
        MessageContent::from_parts(parts)
    };

    // Map usage
    let usage = response
        .usage
        .map(|u| Usage {
            prompt_tokens: {
                let base = u.input_tokens.unwrap_or(0) as i32;
                let cache_read = u.cache_read_input_tokens.unwrap_or(0) as i32;
                let cache_write = u.cache_creation_input_tokens.unwrap_or(0) as i32;
                Some(base + cache_read + cache_write)
            },
            completion_tokens: u.output_tokens.map(|v| v as i32),
            cache_details: {
                let has_cache =
                    u.cache_creation_input_tokens.is_some() || u.cache_read_input_tokens.is_some();
                if has_cache {
                    Some(CacheDetails {
                        write_tokens: u.cache_creation_input_tokens.map(|v| v as i32),
                        read_tokens: u.cache_read_input_tokens.map(|v| v as i32),
                    })
                } else {
                    None
                }
            },
        })
        .unwrap_or_default();

    Ok(ChatResponse {
        // Anthropic names the same condition `max_tokens`.
        truncated: response.stop_reason.as_deref() == Some("max_tokens"),
        content,
        reasoning_content,
        reasoning_signature,
        model: full_model.to_string(),
        usage,
    })
}

// ============================================================================
// Helpers
// ============================================================================

/// Set ephemeral cache_control on a content block param.
fn set_cache_control(block: &mut ContentBlockParam) {
    let cc = Some(CacheControl::ephemeral());
    match block {
        ContentBlockParam::Text { cache_control, .. } => *cache_control = cc,
        ContentBlockParam::ToolUse { cache_control, .. } => *cache_control = cc,
        ContentBlockParam::ToolResult { cache_control, .. } => *cache_control = cc,
        // Thinking/RedactedThinking don't have cache_control
        _ => {}
    }
}

#[cfg(test)]
mod anthropicize_schema_tests {
    use super::anthropicize_schema;
    use serde_json::json;

    /// The exact shape `flawrence_tool::registry` renders for `Option<SomeEnum>`
    /// (a nullable enum) -- flat `type: ["string", "null"]` alongside `enum`.
    /// Anthropic's `input_schema` validator rejects this outright, 400ing with
    /// "Enum value 'function' does not match declared type '['string', 'null']'".
    /// The field is already non-required, so dropping `"null"` from `type`
    /// (rather than exploding into `anyOf`, which trips a separate "Schema is
    /// too complex" 400 once enough of these appear in one tool) is sufficient.
    #[test]
    fn a_nullable_enum_has_null_dropped_from_its_type_and_enum() {
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": ["string", "null"],
                    "enum": ["function", "method", "class"]
                }
            }
        });

        let converted = anthropicize_schema(&schema);
        let kind = &converted["properties"]["kind"];

        assert_eq!(kind["type"], "string", "{kind}");
        assert_eq!(
            kind["enum"],
            json!(["function", "method", "class"]),
            "{kind}"
        );
        assert!(
            kind.get("anyOf").is_none(),
            "no union should be introduced: {kind}"
        );
    }

    /// A plain (non-array) `type` -- the overwhelming majority of properties --
    /// must pass through unchanged, recursing into nested objects/arrays.
    #[test]
    fn an_ordinary_schema_is_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["path"]
        });

        assert_eq!(anthropicize_schema(&schema), schema);
    }

    /// A nullable non-enum field (e.g. `Option<String>`) collapses to the
    /// single remaining type, with no `enum` to touch.
    #[test]
    fn a_nullable_field_without_an_enum_collapses_to_its_bare_type() {
        let schema = json!({"type": ["integer", "null"], "description": "byte offset"});
        let converted = anthropicize_schema(&schema);
        assert_eq!(converted["type"], "integer");
        assert_eq!(converted["description"], "byte offset");
        assert!(converted.get("anyOf").is_none());
    }

    /// An enum whose whitelist happens to include the JSON `null` literal
    /// itself (not merely a nullable-typed property) has it stripped too.
    #[test]
    fn a_literal_null_inside_an_enum_whitelist_is_stripped() {
        let schema = json!({"type": ["string", "null"], "enum": ["on", "off", null]});
        let converted = anthropicize_schema(&schema);
        assert_eq!(converted["type"], "string");
        assert_eq!(converted["enum"], json!(["on", "off"]));
    }
}
