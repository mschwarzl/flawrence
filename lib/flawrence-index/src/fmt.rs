use std::fmt::Write;
use std::path::Path;

use crate::types::{CodebaseSummary, Symbol, SymbolSource};

pub fn fence_language(lang: &str) -> &'static str {
    match lang {
        "Python" => "python",
        "Rust" => "rust",
        "JavaScript" => "javascript",
        "TypeScript" | "TSX" => "typescript",
        "C/C++" => "cpp",
        "Go" => "go",
        "Java" => "java",
        "Ruby" => "ruby",
        _ => "",
    }
}

/// Format find results: `  file:line [kind] qualified_name — signature`
pub fn format_find_results(symbols: &[&Symbol], root: &Path) -> String {
    if symbols.is_empty() {
        return String::from("No definitions found.");
    }
    let mut out = String::new();
    for s in symbols {
        let rel = s.file.strip_prefix(root).unwrap_or(&s.file);
        let sig = if s.signature.is_empty() {
            s.qualified_name.as_str()
        } else {
            s.signature.as_str()
        };
        if s.qualified_name != s.name && sig != s.qualified_name.as_str() {
            writeln!(
                out,
                "  {}:{} [{}] {} — {}",
                rel.display(),
                s.line_start,
                s.kind,
                s.qualified_name,
                sig
            )
        } else {
            writeln!(
                out,
                "  {}:{} [{}] {}",
                rel.display(),
                s.line_start,
                s.kind,
                sig
            )
        }
        .unwrap();
    }
    out
}

/// Format outline: `  line: [kind] qualified_name — signature`
/// Suppresses signature when it doesn't add info beyond the name.
pub fn format_outline(symbols: &[&Symbol]) -> String {
    if symbols.is_empty() {
        return String::from("No definitions found.");
    }
    let mut out = String::new();
    for s in symbols {
        let sig =
            if s.signature.is_empty() || s.signature == s.qualified_name || s.signature == s.name {
                None
            } else {
                Some(s.signature.as_str())
            };
        match sig {
            Some(sig) => writeln!(
                out,
                "  {}:[{}] {} — {}",
                s.line_start, s.kind, s.qualified_name, sig
            ),
            None => writeln!(out, "  {}:[{}] {}", s.line_start, s.kind, s.qualified_name),
        }
        .unwrap();
    }
    out
}

/// Format source body as a fenced code block.
pub fn format_source(src: &SymbolSource, root: &Path) -> String {
    let rel = src.file.strip_prefix(root).unwrap_or(&src.file);
    let fence = fence_language(&src.language);
    format!(
        "Source of \"{}\" in {} (lines {}-{}):\n```{}\n{}\n```",
        src.qualified_name,
        rel.display(),
        src.line_start,
        src.line_end,
        fence,
        src.body,
    )
}

/// Format codebase summary.
pub fn format_summary(summary: &CodebaseSummary) -> String {
    let mut out = String::from("Codebase summary:\n");
    writeln!(out, "  Files: {}", summary.file_count).unwrap();
    writeln!(out, "  Definitions: {}", summary.definition_count).unwrap();
    if !summary.languages.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "  Languages:").unwrap();
        let mut langs: Vec<_> = summary.languages.iter().collect();
        langs.sort_by(|a, b| b.1.cmp(a.1));
        for (lang, count) in langs {
            writeln!(out, "    {}: {} files", lang, count).unwrap();
        }
    }
    out
}
