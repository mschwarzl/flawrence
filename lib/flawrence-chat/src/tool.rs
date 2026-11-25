use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool definition for function calling by LLMs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema for the tool parameters.
    pub schema: Option<Value>,
    /// Enable grammar-constrained sampling for tool inputs (provider-dependent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl Tool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            schema: None,
            strict: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_schema(mut self, parameters: Value) -> Self {
        self.schema = Some(parameters);
        self
    }

    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }
}

/// A tool call returned by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call ID (e.g. "functions.getWeather:0").
    pub call_id: String,
    /// Function name.
    pub fn_name: String,
    /// Parsed arguments as JSON value.
    pub fn_arguments: Value,
}

/// A tool response sent back to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResponse {
    pub call_id: String,
    pub content: String,
}
