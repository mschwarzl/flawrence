use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResponse};

/// A single content segment in a chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    ToolCall(ToolCall),
    ToolResponse(ToolResponse),
}

/// Message content container: a list of content parts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageContent {
    parts: Vec<ContentPart>,
}

// -- Constructors --

impl MessageContent {
    pub fn from_text(content: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::Text(content.into())],
        }
    }

    pub fn from_parts(parts: impl Into<Vec<ContentPart>>) -> Self {
        Self {
            parts: parts.into(),
        }
    }

    pub fn from_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            parts: tool_calls.into_iter().map(ContentPart::ToolCall).collect(),
        }
    }
}

// -- Mutation --

impl MessageContent {
    pub fn push(&mut self, part: impl Into<ContentPart>) {
        self.parts.push(part.into());
    }
}

// -- Accessors --

impl MessageContent {
    pub fn first_text(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

// -- Iterator support --

impl IntoIterator for MessageContent {
    type Item = ContentPart;
    type IntoIter = std::vec::IntoIter<ContentPart>;
    fn into_iter(self) -> Self::IntoIter {
        self.parts.into_iter()
    }
}

impl<'a> IntoIterator for &'a MessageContent {
    type Item = &'a ContentPart;
    type IntoIter = std::slice::Iter<'a, ContentPart>;
    fn into_iter(self) -> Self::IntoIter {
        self.parts.iter()
    }
}

// -- From impls --

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        Self::from_text(s)
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        Self::from_text(s)
    }
}

impl From<Vec<ContentPart>> for MessageContent {
    fn from(parts: Vec<ContentPart>) -> Self {
        Self { parts }
    }
}

impl From<ToolResponse> for MessageContent {
    fn from(tr: ToolResponse) -> Self {
        Self {
            parts: vec![ContentPart::ToolResponse(tr)],
        }
    }
}

impl From<Vec<ToolCall>> for MessageContent {
    fn from(tool_calls: Vec<ToolCall>) -> Self {
        Self::from_tool_calls(tool_calls)
    }
}
