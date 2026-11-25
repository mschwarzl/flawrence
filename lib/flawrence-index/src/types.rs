use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Module,
    Constant,
    Interface,
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Module => "module",
            SymbolKind::Constant => "constant",
            SymbolKind::Interface => "interface",
        })
    }
}

impl SymbolKind {
    /// Map tree-sitter-tags syntax_type_name strings to SymbolKind.
    /// Only definitions are indexed — references are skipped (use grep for usages).
    pub fn from_tag(syntax_type_name: &str, is_definition: bool) -> Option<Self> {
        if !is_definition {
            return None;
        }
        match syntax_type_name {
            s if s.contains("function") => Some(SymbolKind::Function),
            s if s.contains("method") => Some(SymbolKind::Method),
            s if s.contains("class") || s.contains("struct") || s.contains("type") => {
                Some(SymbolKind::Class)
            }
            s if s.contains("module") || s.contains("namespace") => Some(SymbolKind::Module),
            s if s.contains("constant") || s.contains("variable") => Some(SymbolKind::Constant),
            s if s.contains("interface") || s.contains("trait") || s.contains("protocol") => {
                Some(SymbolKind::Interface)
            }
            _ => None, // unknown definition kinds are skipped rather than misclassified
        }
    }
}

impl FromStr for SymbolKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "function" => Ok(SymbolKind::Function),
            "method" => Ok(SymbolKind::Method),
            "class" => Ok(SymbolKind::Class),
            "module" => Ok(SymbolKind::Module),
            "constant" => Ok(SymbolKind::Constant),
            "interface" => Ok(SymbolKind::Interface),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line_start: usize, // 1-indexed
    pub line_end: usize,
    pub signature: String,
}

impl Symbol {
    /// Create a Symbol for a code file.
    #[allow(clippy::too_many_arguments)]
    pub fn new_code(
        name: String,
        qualified_name: String,
        scope: Option<String>,
        kind: SymbolKind,
        file: PathBuf,
        line_start: usize,
        line_end: usize,
        signature: String,
    ) -> Self {
        Self {
            name,
            qualified_name,
            scope,
            kind,
            file,
            line_start,
            line_end,
            signature,
        }
    }
}

#[derive(Debug)]
pub struct SymbolSource {
    pub qualified_name: String,
    pub file: PathBuf,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct CodebaseSummary {
    pub file_count: usize,
    pub definition_count: usize,
    pub languages: HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_kind_from_tag_definitions() {
        assert_eq!(
            SymbolKind::from_tag("definition.function", true),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            SymbolKind::from_tag("definition.method", true),
            Some(SymbolKind::Method)
        );
        assert_eq!(
            SymbolKind::from_tag("definition.class", true),
            Some(SymbolKind::Class)
        );
        assert_eq!(
            SymbolKind::from_tag("definition.module", true),
            Some(SymbolKind::Module)
        );
        assert_eq!(
            SymbolKind::from_tag("definition.interface", true),
            Some(SymbolKind::Interface)
        );
    }

    #[test]
    fn test_symbol_kind_from_tag_reference_returns_none() {
        assert_eq!(SymbolKind::from_tag("reference.call", false), None);
        assert_eq!(
            SymbolKind::from_tag("reference.implementation", false),
            None
        );
    }

    #[test]
    fn test_from_str() {
        assert_eq!("function".parse::<SymbolKind>(), Ok(SymbolKind::Function));
        assert_eq!("Method".parse::<SymbolKind>(), Ok(SymbolKind::Method));
        assert_eq!("CLASS".parse::<SymbolKind>(), Ok(SymbolKind::Class));
        assert_eq!("interface".parse::<SymbolKind>(), Ok(SymbolKind::Interface));
        assert!("unknown".parse::<SymbolKind>().is_err());
    }
}
