use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

use crate::{ToolContext, ToolDef};
use flawrence_index::types::SymbolSource;
use flawrence_index::{Symbol, SymbolKind, fmt};

const MAX_FIND_RESULTS: usize = 100;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeIndexOperation {
    /// Look up symbol definitions by name. Requires: symbol.
    Find,
    /// Get full source of a symbol. Requires: symbol + path (from find results).
    Source,
    /// Find a symbol and return its source in one step. Requires: symbol.
    FindSource,
    /// List all definitions in a file. Requires: path. Optional: kind filter.
    Outline,
    /// Codebase stats: file count, definitions, language breakdown.
    Summarize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeIndexArgs {
    /// The operation to perform.
    pub operation: CodeIndexOperation,
    /// Symbol name or qualified name (e.g. "MyClass::method"). Required for: find, source,
    /// find_source. Omit entirely for outline and summarize.
    pub symbol: Option<String>,
    /// File path relative to project root. Required for: source, outline. Omit entirely to
    /// search the whole index, which is what find and find_source want.
    pub path: Option<String>,
    /// Restrict an outline to one kind of definition. OMIT THIS unless you have already seen
    /// the full outline and want to re-read one kind from a file too large to show whole.
    /// Never supply it on a first look: it hides every other kind, and a class-only outline of
    /// a Java or Kotlin file is one line that costs a turn to discover was not what you wanted.
    pub kind: Option<SymbolKind>,
}

impl ToolDef for CodeIndexArgs {
    const NAME: &'static str = "code_index";
    const DESCRIPTION: &'static str = "Look up symbol definitions, source code, and file structure from the code index. \
        Prefer this over read_file when you need specific definitions rather than the whole file. \
        Use grep_files for references/usages or free-text search.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let index = &ctx.runtime.codebase_index;
        let root = index.root();

        let result = match args.operation {
            CodeIndexOperation::Find => {
                let symbol = require_symbol(&args.symbol)?;
                let results = index.find_symbol(symbol);
                if results.is_empty() {
                    format!("No definitions found for \"{}\".", symbol)
                } else {
                    let total = results.len();
                    let truncated = total > MAX_FIND_RESULTS;
                    let shown = &results[..total.min(MAX_FIND_RESULTS)];
                    format!(
                        "Definitions matching \"{}\" ({} results{}):\n{}",
                        symbol,
                        total,
                        if truncated {
                            format!(", showing first {}", MAX_FIND_RESULTS)
                        } else {
                            String::new()
                        },
                        fmt::format_find_results(shown, root),
                    )
                }
            }
            CodeIndexOperation::Source => {
                let symbol = require_symbol(&args.symbol)?;
                let path = require_path(&args.path)?;
                if !index.has_file(Path::new(path)) {
                    anyhow::bail!("{}", not_in_index(index, path));
                }
                match index.get_body(symbol, Path::new(path)) {
                    Some(src) => bounded_source(&src, root),
                    None => format!("Could not extract body for \"{}\" in \"{}\".", symbol, path),
                }
            }
            CodeIndexOperation::FindSource => {
                let symbol = require_symbol(&args.symbol)?;
                let results = index.find_symbol(symbol);
                if results.is_empty() {
                    format!("No definitions found for \"{}\".", symbol)
                } else if results.len() == 1 {
                    let s = results[0];
                    match index.get_body(symbol, &s.file) {
                        Some(src) => bounded_source(&src, root),
                        None => format!(
                            "Found \"{}\" at {}:{} but could not extract body.",
                            s.qualified_name,
                            s.file.strip_prefix(root).unwrap_or(&s.file).display(),
                            s.line_start,
                        ),
                    }
                } else {
                    format!(
                        "Multiple definitions for \"{}\". Use source with a specific path:\n{}",
                        symbol,
                        fmt::format_find_results(
                            &results[..results.len().min(MAX_FIND_RESULTS)],
                            root
                        ),
                    )
                }
            }
            CodeIndexOperation::Outline => {
                let path = require_path(&args.path)?;
                if !index.has_file(Path::new(path)) {
                    anyhow::bail!("{}", not_in_index(index, path));
                }
                // The unfiltered count is what makes a filtered outline honest. Without it a
                // `kind` filter reports "(4 definitions)" for a file holding 55, and the caller
                // has no way to tell a small file from a narrow view of a large one.
                let total = index.list_symbols(Path::new(path), None).len();
                let results = index.list_symbols(Path::new(path), args.kind.as_ref());
                bound_response(
                    format_outline(path, &results, total, args.kind.as_ref()),
                    "definitions",
                    "Narrow it with `kind` -- a filtered outline reports how many it hides.",
                )
            }
            CodeIndexOperation::Summarize => {
                let summary = index.summarize();
                fmt::format_summary(&summary)
            }
        };

        Ok((result, None))
    }
}

/// Trim a rendered response to the budget, naming what was cut and how to get
/// the rest.
///
/// The task loop truncates an oversized response anyway; the difference is that
/// it does so silently. A caller told only "definitions" has no idea the list
/// stopped early, and a disassembled monolith -- one class, thousands of
/// members -- is exactly the shape that reaches this.
fn bound_response(rendered: String, unit: &str, remedy: &str) -> String {
    let lines: Vec<&str> = rendered.lines().collect();
    let (kept, truncated) = crate::filesystem::fit_within_budget(&lines);
    if !truncated {
        return rendered;
    }
    format!(
        "{}\n\n[Showing {} of {} lines of {unit} to stay within the {}-character response \
         budget. {remedy}]",
        kept.join("\n"),
        kept.len(),
        lines.len(),
        crate::MAX_TOOL_RESPONSE_CHARS,
    )
}

/// A symbol's source, trimmed to the budget with its own line range as the way
/// back in.
///
/// Paging the source here would need a new argument; `read_file` already takes
/// an offset and a limit, and the symbol knows exactly which lines it spans --
/// so point at the tool that can already do it, with the numbers filled in.
fn bounded_source(src: &SymbolSource, root: &Path) -> String {
    let rendered = fmt::format_source(src, root);
    let lines: Vec<&str> = rendered.lines().collect();
    let (kept, truncated) = crate::filesystem::fit_within_budget(&lines);
    if !truncated {
        return rendered;
    }
    let relative = src.file.strip_prefix(root).unwrap_or(&src.file);
    let span = src.line_end.saturating_sub(src.line_start) + 1;
    format!(
        "{}\n```\n\n[Truncated: \"{}\" spans {} lines ({}:{}-{}) and does not fit the \
         {}-character response budget. Page it with read_file offset={} limit=<n>.]",
        kept.join("\n"),
        src.qualified_name,
        span,
        relative.display(),
        src.line_start,
        src.line_end,
        crate::MAX_TOOL_RESPONSE_CHARS,
        src.line_start.saturating_sub(1),
    )
}

/// Render an outline, stating plainly when a `kind` filter is hiding definitions.
///
/// An agent orienting itself in an unfamiliar file has no independent way to know a listing is
/// partial, so a filtered outline must say so in the same breath as the count -- including when
/// the filter leaves nothing at all and the file is anything but empty.
fn format_outline(
    path: &str,
    results: &[&Symbol],
    total: usize,
    kind: Option<&SymbolKind>,
) -> String {
    let Some(kind) = kind else {
        return if results.is_empty() {
            format!("No definitions found in \"{}\".", path)
        } else {
            format!(
                "Outline of {} ({} definitions):\n{}",
                path,
                results.len(),
                fmt::format_outline(results),
            )
        };
    };

    if results.is_empty() {
        return if total == 0 {
            format!("No definitions found in \"{}\".", path)
        } else {
            format!(
                "No {kind} definitions in \"{path}\", but the file has {total} definition(s) \
                 of other kinds. Re-run outline without \"kind\" to see them."
            )
        };
    }

    let hidden = total - results.len();
    if hidden == 0 {
        format!(
            "Outline of {} (all {} definitions are {kind}):\n{}",
            path,
            results.len(),
            fmt::format_outline(results),
        )
    } else {
        format!(
            "Outline of {} ({} of {} definitions; filtered to kind={kind}, {hidden} hidden -- \
             re-run without \"kind\" to see them):\n{}",
            path,
            results.len(),
            total,
            fmt::format_outline(results),
        )
    }
}

fn require_symbol(symbol: &Option<String>) -> Result<&str> {
    symbol.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "'symbol' is required for find, source and find_source. \
             For outline use 'path' with symbol=null; for summarize both are null."
        )
    })
}

fn require_path(path: &Option<String>) -> Result<&str> {
    path.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "'path' is required for source and outline. Run find first to get the path of a \
             symbol, or find_source to skip straight to the body."
        )
    })
}

/// A file the index does not know is usually one of three things, and the
/// caller can only act if it is told which: a typo, a real file whose language
/// is not indexed, or a file with no definitions in it.
fn not_in_index(index: &flawrence_index::CodebaseIndex, path: &str) -> String {
    let on_disk = index.root().join(path).is_file();
    let summary = index.summarize();
    let mut languages: Vec<(&String, &usize)> = summary.languages.iter().collect();
    languages.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let languages: Vec<&str> = languages.iter().map(|(name, _)| name.as_str()).collect();

    if on_disk {
        format!(
            "File \"{path}\" exists but is not in the code index — its language is not indexed, \
             or it contains no definitions. Indexed languages: {}. Use read_file or grep_files \
             for this file.",
            if languages.is_empty() {
                "(none)".to_string()
            } else {
                languages.join(", ")
            }
        )
    } else {
        format!(
            "File \"{path}\" does not exist.{}",
            suggest_paths(index, path)
        )
    }
}

/// Turn a wrong path into the right one when the file name is unambiguous.
fn suggest_paths(index: &flawrence_index::CodebaseIndex, path: &str) -> String {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
        return " Use glob_files or list_directory to find the real path, then retry.".to_string();
    };
    let matches = index.files_named(name);
    match matches.as_slice() {
        [] => " Use glob_files or list_directory to find the real path, then retry.".to_string(),
        [only] => format!(" Did you mean \"{}\"?", only.display()),
        many => format!(
            " Indexed files named \"{name}\":\n{}",
            many.iter()
                .take(5)
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sym(name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: name.to_string(),
            scope: None,
            kind,
            file: PathBuf::from("DerDecoder.kt"),
            line_start: 1,
            line_end: 2,
            signature: format!("fun {name}()"),
        }
    }

    /// The run-15 regression: three agents asked for an outline of a 55-definition file with
    /// `kind=class`, were told "(4 definitions)", and re-read ~900 lines apiece to recover what
    /// the filter had hidden. A filtered outline must name both the true total and the filter.
    #[test]
    fn a_filtered_outline_reports_the_unfiltered_total_and_the_filter() {
        let classes: Vec<Symbol> = (0..4)
            .map(|i| sym(&format!("C{i}"), SymbolKind::Class))
            .collect();
        let results: Vec<&Symbol> = classes.iter().collect();

        let out = format_outline("DerDecoder.kt", &results, 55, Some(&SymbolKind::Class));

        assert!(out.contains("4 of 55"), "must state the true total: {out}");
        assert!(
            out.contains("kind=class"),
            "must name the active filter: {out}"
        );
        assert!(
            out.contains("51 hidden"),
            "must state what was suppressed: {out}"
        );
        assert!(
            !out.contains("(4 definitions)"),
            "must not read as a complete listing: {out}"
        );
    }

    /// The sharper form of the same bug: a filter that matches nothing must not claim the file
    /// is empty, or an agent concludes the symbol it wants does not exist.
    #[test]
    fn a_filter_matching_nothing_does_not_claim_the_file_is_empty() {
        let out = format_outline("DerDecoder.kt", &[], 55, Some(&SymbolKind::Interface));

        assert!(
            out.contains("55"),
            "must reveal the file is populated: {out}"
        );
        assert!(
            !out.starts_with("No definitions found"),
            "must not read as an empty file: {out}"
        );
        assert!(out.contains("without"), "must point at the way out: {out}");
    }

    #[test]
    fn an_unfiltered_outline_keeps_its_plain_wording() {
        let syms: Vec<Symbol> = (0..3)
            .map(|i| sym(&format!("f{i}"), SymbolKind::Function))
            .collect();
        let results: Vec<&Symbol> = syms.iter().collect();

        let out = format_outline("DerDecoder.kt", &results, 3, None);

        assert!(out.contains("(3 definitions)"), "{out}");
        assert!(!out.contains("hidden"), "nothing is hidden: {out}");
    }

    #[test]
    fn a_filter_hiding_nothing_says_so_without_a_misleading_hidden_count() {
        let syms: Vec<Symbol> = (0..2)
            .map(|i| sym(&format!("C{i}"), SymbolKind::Class))
            .collect();
        let results: Vec<&Symbol> = syms.iter().collect();

        let out = format_outline("DerDecoder.kt", &results, 2, Some(&SymbolKind::Class));

        assert!(out.contains("all 2 definitions are class"), "{out}");
        assert!(!out.contains("hidden"), "{out}");
    }

    #[test]
    fn a_genuinely_empty_file_still_reports_empty_under_a_filter() {
        let out = format_outline("Empty.kt", &[], 0, Some(&SymbolKind::Class));
        assert!(out.starts_with("No definitions found"), "{out}");
    }
}

#[cfg(test)]
mod budget {
    use super::*;
    use std::path::PathBuf;

    fn huge_source(lines: usize) -> SymbolSource {
        SymbolSource {
            qualified_name: "Disassembled::main".to_string(),
            file: PathBuf::from("/repo/out/Disassembled.j"),
            line_start: 212,
            line_end: 212 + lines - 1,
            language: "java".to_string(),
            body: (0..lines)
                .map(|i| format!("    invokevirtual #{i} // a disassembled instruction line"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// A disassembled monolith is one symbol with thousands of members. The task
    /// loop would cut it silently; the caller must instead learn it was cut and
    /// how to read the rest.
    #[test]
    fn an_oversized_symbol_names_its_line_range_and_the_tool_that_can_page_it() {
        let src = huge_source(2_000);
        let out = bounded_source(&src, Path::new("/repo"));

        assert!(out.len() <= crate::MAX_TOOL_RESPONSE_CHARS, "{}", out.len());
        assert!(out.contains("Truncated"), "{}", tail(&out));
        // The exact span, so the caller does not have to discover it.
        assert!(
            out.contains("out/Disassembled.j:212-2211"),
            "{}",
            tail(&out)
        );
        assert!(out.contains("2000 lines"), "{}", tail(&out));
        // read_file already pages; point at it with the number filled in.
        assert!(out.contains("read_file offset=211"), "{}", tail(&out));
        // The fence is closed, or everything after reads as code.
        assert!(out.contains("```"), "{}", tail(&out));
    }

    /// The common case must be byte-identical to what it was, or every caller
    /// gains framing to parse for nothing.
    #[test]
    fn a_symbol_that_fits_is_returned_verbatim() {
        let src = huge_source(3);
        assert_eq!(
            bounded_source(&src, Path::new("/repo")),
            fmt::format_source(&src, Path::new("/repo"))
        );
    }

    /// An outline's remedy is the `kind` filter, which already reports what it
    /// hides -- so the two compose into a complete answer.
    #[test]
    fn an_oversized_outline_points_at_the_kind_filter() {
        let rendered = (0..4_000)
            .map(|i| format!("method someRatherLongMethodName{i}(int a, int b) -> void"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = bound_response(rendered, "definitions", "Narrow it with `kind`.");

        assert!(out.len() <= crate::MAX_TOOL_RESPONSE_CHARS, "{}", out.len());
        assert!(
            out.contains("of 4000 lines of definitions"),
            "{}",
            tail(&out)
        );
        assert!(out.contains("Narrow it with `kind`"), "{}", tail(&out));
    }

    #[test]
    fn an_outline_that_fits_keeps_its_exact_text() {
        let rendered = "Outline of A.java (2 definitions):\nclass A\nmethod A::b".to_string();
        assert_eq!(
            bound_response(rendered.clone(), "definitions", "remedy"),
            rendered
        );
    }

    fn tail(s: &str) -> String {
        s.chars()
            .skip(s.chars().count().saturating_sub(260))
            .collect()
    }
}
