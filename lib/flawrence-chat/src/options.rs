use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Chat completion options.
#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub structured_output: Option<StructuredOutput>,
}

impl ChatOptions {
    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    pub fn with_structured_output(mut self, output: StructuredOutput) -> Self {
        self.structured_output = Some(output);
        self
    }
}

/// Provider-specific hint for reasoning intensity/budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Max,
}

/// Provider-agnostic structured output specification.
///
/// Each `ChatClient` adapter is responsible for converting this to the
/// provider's wire format (e.g. OpenAI's `response_format`, Anthropic's
/// tool-based structured output, Google's `response_schema`).
#[derive(Debug, Clone)]
pub enum StructuredOutput {
    /// Constrain model output to match a JSON Schema.
    JsonSchema { name: String, schema: Value },
}
