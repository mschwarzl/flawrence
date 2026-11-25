use crate::command::{run_command, run_command_bounded};
use crate::{ToolContext, ToolDef};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Component, Path};
use std::{fs, path::PathBuf};
use tokio::process::Command;

/// Canonicalize a path and verify it's within the given base directory.
/// Returns an error if the path escapes the base directory or doesn't exist.
pub fn safe_canonicalize_path(base_dir: &Path, path: &str) -> Result<PathBuf> {
    let canonical = base_dir.join(path).canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "Path not accessible '{}': {}{}",
            path,
            e,
            nearest_existing_hint(base_dir, path)
        )
    })?;

    if canonical.starts_with(base_dir) {
        Ok(canonical)
    } else {
        Err(anyhow::anyhow!(
            "Path '{}' is outside the working directory '{}'. Use a path relative to it, \
             or $WORKSPACE for the root in bash commands.",
            path,
            base_dir.display()
        ))
    }
}

/// Append "did you mean" to a path error when the code index knows a file with
/// that name. Paths reach the tools copied out of prose — abbreviated with
/// `...`, or with a guessed package directory — and the file name is usually
/// the part that survived.
fn suggest_indexed_path(ctx: &ToolContext, path: &str, error: anyhow::Error) -> anyhow::Error {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
        return error;
    };
    let matches = ctx.runtime.codebase_index.files_named(name);
    match matches.as_slice() {
        [] => error,
        [only] => anyhow::anyhow!("{error} Did you mean \"{}\"?", only.display()),
        many => anyhow::anyhow!(
            "{error} Indexed files named \"{name}\": {}",
            many.iter()
                .take(5)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve `.` and `..` without touching the filesystem, so a path that does
/// not exist can still be described sensibly. A `..` that would climb above the
/// root is dropped rather than escaping.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Point at the deepest part of a bad path that does exist. A model that
/// invents `…/awesn1/io/` learns more from "…/at/asitplus exists" than from
/// a bare ENOENT, and can glob or list from there instead of guessing again.
fn nearest_existing_hint(base_dir: &Path, path: &str) -> String {
    // Normalise `.` and `..` first: walking the raw join produces hints like
    // `a/b/c/../../..`, which is worse than saying nothing.
    let requested = lexically_normalize(&base_dir.join(path));
    let mut ancestors = requested.ancestors().skip(1);
    let existing =
        ancestors.find(|candidate| candidate.exists() && candidate.starts_with(base_dir));

    let Some(existing) = existing else {
        return String::new();
    };
    let relative = existing.strip_prefix(base_dir).unwrap_or(existing);
    let shown = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative.display().to_string()
    };
    format!(
        " (deepest existing directory: '{shown}' — use list_directory or glob_files from there)"
    )
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    /// Regex pattern to search for in file contents (e.g., "fn main", "TODO.*fix").
    pub pattern: String,
    /// Glob to filter which files are searched (e.g., "*.rs", "*.toml"). Complementary to `path` — use glob for file-type filtering and path for directory scoping.
    pub glob: Option<String>,
    /// Directory to search in, relative to the project root (e.g., "src/tools", "tests"). Omit to search the entire project.
    pub path: Option<String>,
    /// Set to true for case-insensitive matching. Default is false (case-sensitive).
    #[serde(default)]
    pub case_insensitive: bool,
}

impl ToolDef for GrepArgs {
    const NAME: &'static str = "grep_files";
    const DESCRIPTION: &'static str = "Search for patterns in files using ripgrep with glob filtering. The glob parameter filters which files to search (e.g., '*.rs', 'src/**/*.toml'). Returns matching lines with line numbers. Use when: you need to find where a string, pattern, or regex appears across the codebase — e.g., locating usages/references of a function, finding TODO comments, or searching for error messages. Prefer this over read_file when you don't know which file contains the code. For symbol definitions and file outlines, prefer code_index (instant, zero-cost) over grep.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        // Long alternations of symbol names are the single most useful query a
        // security agent makes ("find any of these twelve functions"), and
        // ripgrep matches them with a finite automaton — no backtracking, so
        // length costs almost nothing. The old 200-character cap rejected them
        // and advised read_file instead, which cannot answer the question.
        // The remaining bound just stops a pathological regex reaching rg.
        const MAX_PATTERN_CHARS: usize = 2_000;
        if args.pattern.len() > MAX_PATTERN_CHARS {
            anyhow::bail!(
                "Pattern too long ({} chars, max {MAX_PATTERN_CHARS}). Split it into several \
                 searches, or narrow with `glob`/`path` and read the files instead.",
                args.pattern.len()
            );
        }

        if let Some(glob) = &args.glob {
            if glob.starts_with('/') {
                anyhow::bail!("Absolute paths not allowed in glob pattern");
            }
            if glob.starts_with("..") {
                anyhow::bail!("Glob pattern cannot start with '..'");
            }
        }

        let workspace = ctx.workspace();
        let base_dir = workspace.as_path();
        let search_dir = resolve_search_dir(base_dir, args.path.as_deref())?;

        let rg = resolve_rg_bin();
        let mut cmd = Command::new(&rg);
        cmd.current_dir(base_dir)
            .stdin(std::process::Stdio::null())
            .arg("-n")
            .arg("--heading")
            .arg("--max-filesize=1M")
            .arg("--no-binary")
            .arg("--max-columns=500")
            .arg("--max-columns-preview")
            .arg("--sort=path");
        if args.case_insensitive {
            cmd.arg("-i");
        }
        if let Some(glob) = &args.glob {
            cmd.arg("-g").arg(glob);
        }
        cmd.arg("--").arg(&args.pattern);
        if let Some(dir) = &search_dir {
            cmd.arg(dir);
        }

        let output = run_command_bounded(
            &mut cmd,
            std::time::Duration::from_secs(30),
            crate::MAX_TOOL_RESPONSE_CHARS,
            crate::MAX_TOOL_RESPONSE_CHARS,
        )
        .await?;
        if output.timed_out {
            anyhow::bail!("Ripgrep command timed out after 30 seconds");
        }
        let stdout = output.stdout;
        let stderr = output.stderr;

        if stdout.is_empty() {
            if !stderr.is_empty() {
                return Ok((format!("No matches found\n{}", stderr.trim()), None));
            }
            return Ok(("No matches found".to_string(), None));
        }

        let lines: Vec<&str> = stdout.lines().collect();
        let (shown, truncated) = take_within_limits(&lines, MAX_GREP_LINES);
        let mut result = shown.join("\n");
        if truncated || output.stdout_truncated {
            result.push_str(&format!(
                "\n\n[Showing {} of {} matching lines. Use `glob` or `path` to narrow the \
                 search, or code_index for symbol definitions.]",
                shown.len(),
                lines.len(),
            ));
        }

        Ok((result, None))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    /// File path relative to working directory (e.g., "src/main.rs").
    pub path: String,
    /// 0-indexed line offset to start reading from. Use 0 for the beginning.
    pub offset: usize,
    /// Maximum number of lines to return.
    pub limit: usize,
}

impl ToolDef for ReadFileArgs {
    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str = "Read raw file contents with optional offset/limit for line ranges. Use when: (1) you already know the file and line range you need (e.g., after grep_files located a match), (2) you need the full content of a small file like a config or manifest, or (3) you need to inspect a specific code region in detail. Avoid for exploratory searches — use grep_files instead. For symbol definitions and file outlines, prefer code_index (instant, zero-cost) over reading entire files.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let safe_path = safe_canonicalize_path(&ctx.workspace(), &args.path)
            .map_err(|error| suggest_indexed_path(&ctx, &args.path, error))?;

        let content = fs::read_to_string(&safe_path)
            .map_err(|e| anyhow::anyhow!("Failed to read '{}': {}", args.path, e))?;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let offset = args.offset;
        let limit = args.limit;

        let requested: Vec<&str> = lines.iter().skip(offset).take(limit).copied().collect();
        let (selected, truncated) = fit_within_budget(&requested);

        let start_line = offset + 1;
        let end_line = offset + selected.len();
        let header = format!("Lines {} to {} of {}:", start_line, end_line, total_lines);
        let mut result = format!("{}\n{}", header, selected.join("\n"));
        if truncated {
            // Returning nothing and asking the model to guess a smaller range
            // wastes a turn; naming the resume point does not.
            result.push_str(&format!(
                "\n\n[Truncated after {} of {} requested lines to stay within the \
                 {}-character response budget. Continue with offset={}.]",
                selected.len(),
                requested.len(),
                crate::MAX_TOOL_RESPONSE_CHARS,
                end_line,
            ));
        }

        Ok((result, None))
    }
}

/// Item caps for listing tools. Both are relevance limits rather than safety
/// ones — the response budget is what actually bounds size — so exceeding them
/// truncates with a count rather than failing the call.
const MAX_GLOB_MATCHES: usize = 200;
const MAX_GREP_LINES: usize = 250;

/// Stop walking a glob after this many hits. Purely a memory guard: the result
/// is truncated long before this.
const MAX_GLOB_SCAN: usize = 5_000;

/// Apply the item cap and the response budget, whichever bites first.
fn take_within_limits<'a>(items: &[&'a str], max_items: usize) -> (Vec<&'a str>, bool) {
    let capped: Vec<&str> = items.iter().take(max_items).copied().collect();
    let (fitted, over_budget) = fit_within_budget(&capped);
    let truncated = over_budget || items.len() > fitted.len();
    (fitted, truncated)
}

/// Take as many whole lines as fit in the response budget, leaving room for
/// the header and the continuation note. Always yields at least one line, so a
/// single enormous line still makes progress rather than looping forever.
pub(crate) fn fit_within_budget<'a>(lines: &[&'a str]) -> (Vec<&'a str>, bool) {
    const RESERVED_FOR_NOTES: usize = 256;
    let budget = crate::MAX_TOOL_RESPONSE_CHARS.saturating_sub(RESERVED_FOR_NOTES);

    let mut used = 0;
    let mut taken = Vec::with_capacity(lines.len());
    for line in lines {
        let cost = line.len() + 1;
        if used + cost > budget && !taken.is_empty() {
            return (taken, true);
        }
        used += cost;
        taken.push(*line);
    }
    (taken, false)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDirectoryArgs {
    /// Directory path relative to working directory (e.g., "src/tools"). Use "." for the root.
    pub path: String,
}

impl ToolDef for ListDirectoryArgs {
    const NAME: &'static str = "list_directory";
    const DESCRIPTION: &'static str = "List files and directories in a path. Use when: you need to explore the project structure, discover what files exist in a directory, or orient yourself in an unfamiliar codebase. Prefer glob_files when you need to find files matching a specific naming pattern across multiple directories. Prefer grep_files when you need to search file contents rather than just file names.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let safe_path = safe_canonicalize_path(&ctx.workspace(), &args.path)?;

        if !safe_path.is_dir() {
            anyhow::bail!(
                "'{}' is not a directory — use read_file to read it, or list_directory on its parent.",
                args.path
            );
        }

        let mut files = Vec::new();
        let mut directories = Vec::new();

        for entry in
            fs::read_dir(&safe_path).map_err(|e| anyhow::anyhow!("Failed to list dir: {}", e))?
        {
            let entry = entry.map_err(|e| anyhow::anyhow!("Entry error: {}", e))?;
            let name = entry.file_name().to_string_lossy().to_string();

            if entry.path().is_dir() {
                directories.push(name);
            } else {
                files.push(name);
            }
        }

        directories.sort();
        files.sort();

        Ok((
            format!(
                "Listing of {}:\nDirectories ({}):\n{}\n\nFiles ({}):\n{}",
                args.path,
                directories.len(),
                directories.join("\n"),
                files.len(),
                files.join("\n")
            ),
            None,
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFileArgs {
    /// File path relative to working directory (e.g., "src/new_module.rs"). Parent directories are created automatically.
    pub path: String,
    /// Complete file content to write. This replaces the entire file.
    #[schemars(length(max = 100000))]
    pub content: String,
}

impl ToolDef for WriteFileArgs {
    const NAME: &'static str = "write_file";
    const DESCRIPTION: &'static str = "Write content to a file. Creates parent directories if needed. Path must be relative to working directory. Use when: you need to create a new file (e.g., a vulnerability report, analysis output) or overwrite an existing file with updated content. Note: this replaces the entire file — there is no partial/patch edit mode.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        for component in std::path::Path::new(&args.path).components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                _ => anyhow::bail!(
                    "Path must be relative with no parent traversal: {}",
                    args.path
                ),
            }
        }

        let workspace = ctx.workspace();
        let base_dir = workspace.as_path();
        let target = base_dir.join(&args.path);

        // Security: canonicalize before dir creation to resolve symlinks, then verify still under base dir
        // For new files, canonicalize the parent and append the filename
        let target_canonical = target.canonicalize().or_else(|_| {
            // File doesn't exist yet - canonicalize parent (or as much as exists) and append filename
            if let (Some(parent), Some(filename)) = (target.parent(), target.file_name()) {
                // Parent may not exist yet either, so walk up to find the
                // deepest existing ancestor and verify *that* is under base_dir.
                let mut ancestor = parent.to_path_buf();
                let mut trailing = vec![filename.to_os_string()];
                while !ancestor.exists() {
                    if let Some(fname) = ancestor.file_name() {
                        trailing.push(fname.to_os_string());
                    }
                    if !ancestor.pop() {
                        break;
                    }
                }
                let ancestor_canonical = ancestor.canonicalize()?;
                let mut result = ancestor_canonical;
                for part in trailing.into_iter().rev() {
                    result = result.join(part);
                }
                Ok(result)
            } else {
                Err(anyhow::anyhow!("Invalid target path"))
            }
        })?;

        if !target_canonical.starts_with(base_dir) {
            anyhow::bail!("Path escapes working directory");
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create directories: {}", e))?;
        }

        fs::write(&target_canonical, &args.content)
            .map_err(|e| anyhow::anyhow!("Failed to write '{}': {}", args.path, e))?;

        Ok((
            format!("Wrote {} bytes to '{}'", args.content.len(), args.path),
            None,
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArgs {
    /// Unified diff to apply with `git apply`. Paths must be relative to the working directory.
    #[schemars(length(max = 200000))]
    pub patch: String,
}

impl ToolDef for ApplyPatchArgs {
    const NAME: &'static str = "apply_patch";
    const DESCRIPTION: &'static str = "Apply a unified diff to the working directory using `git apply --check` followed by `git apply`. Use this for code edits because it is reviewable, multi-file, and fails before modifying files if the patch does not apply cleanly. Paths must be relative and stay inside the working directory; binary patches and absolute/parent-directory paths are rejected.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        if args.patch.trim().is_empty() {
            anyhow::bail!("patch must not be empty");
        }
        validate_patch_paths(&args.patch)?;
        let workspace = ctx.workspace();

        let mut patch_file = tempfile::NamedTempFile::new()
            .map_err(|e| anyhow::anyhow!("Failed to create temp patch file: {}", e))?;
        std::io::Write::write_all(&mut patch_file, args.patch.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write temp patch file: {}", e))?;

        let mut check = Command::new("git");
        check
            .current_dir(&workspace)
            .arg("apply")
            .arg("--check")
            .arg(patch_file.path());
        let check_output = run_command(&mut check, std::time::Duration::from_secs(30)).await?;
        if check_output.exit_code != 0 {
            anyhow::bail!(
                "Patch did not apply cleanly:\nstdout: {}\nstderr: {}",
                check_output.stdout,
                check_output.stderr
            );
        }

        let mut apply = Command::new("git");
        apply
            .current_dir(&workspace)
            .arg("apply")
            .arg(patch_file.path());
        let apply_output = run_command(&mut apply, std::time::Duration::from_secs(30)).await?;
        if apply_output.exit_code != 0 {
            anyhow::bail!(
                "Patch check passed but apply failed:\nstdout: {}\nstderr: {}",
                apply_output.stdout,
                apply_output.stderr
            );
        }

        Ok(("Patch applied cleanly.".to_string(), None))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobFilesArgs {
    /// Glob pattern relative to working directory (e.g., "src/**/*.rs", "*.toml", "docs/**/*.md"). Only files within the working directory are returned.
    pub pattern: String,
}

impl ToolDef for GlobFilesArgs {
    const NAME: &'static str = "glob_files";
    const DESCRIPTION: &'static str = "Find files matching a glob pattern. Only returns files within the working directory. Use when: you need to discover files by name or extension (e.g., 'src/**/*.rs' to find all Rust files, '*.toml' to find config files). Prefer this over list_directory when you need recursive file discovery or filtering by extension. Prefer grep_files when you need to search inside file contents rather than match file names.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        // DoS prevention: reject patterns that start outside base dir
        if args.pattern.starts_with('/') {
            anyhow::bail!("Absolute paths not allowed");
        }
        if args.pattern.starts_with("..") {
            anyhow::bail!("Pattern cannot start with '..'");
        }

        let workspace = ctx.workspace();
        let base_dir = workspace.as_path();

        // Build pattern relative to base dir
        let full_pattern = base_dir.join(&args.pattern).to_string_lossy().to_string();

        let entries = glob::glob(&full_pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;

        let mut matches = Vec::new();
        let mut scan_stopped = false;
        for entry in entries {
            // Skip glob errors (permission denied, etc.)
            let Ok(path) = entry else { continue };

            // Security gate: canonicalize and verify within base dir
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            if !canonical.starts_with(base_dir) {
                continue; // Silently filter - no info leak
            }

            // Return path relative to base dir
            let relative = canonical.strip_prefix(base_dir).unwrap_or(&canonical);
            matches.push(relative.display().to_string());

            if matches.len() >= MAX_GLOB_SCAN {
                scan_stopped = true;
                break;
            }
        }

        if matches.is_empty() {
            return Ok(("No matches found".to_string(), None));
        }

        // Returning nothing because there was one match too many costs a turn
        // and tells the model nothing; the first page plus a total lets it
        // either use what it has or narrow deliberately.
        let all: Vec<&str> = matches.iter().map(String::as_str).collect();
        let (shown, truncated) = take_within_limits(&all, MAX_GLOB_MATCHES);
        let mut result = shown.join("\n");
        if truncated {
            result.push_str(&format!(
                "\n\n[Showing {} of {}{} matches. Narrow the pattern — a subdirectory, \
                 or a more specific name — or use list_directory to explore incrementally.]",
                shown.len(),
                matches.len(),
                if scan_stopped { "+" } else { "" },
            ));
        }
        Ok((result, None))
    }
}

/// Resolve an optional user-supplied path to a clean relative directory within base_dir.
/// Returns None if the path resolves to base_dir itself (e.g. ".", "./", "/").
pub fn resolve_search_dir(base_dir: &Path, path: Option<&str>) -> Result<Option<PathBuf>> {
    let Some(p) = path else { return Ok(None) };
    let resolved = safe_canonicalize_path(base_dir, p)?;
    let rel = resolved.strip_prefix(base_dir).map_err(|_| {
        anyhow::anyhow!(
            "Path '{}' is outside the working directory '{}'. Use a path relative to it, \
             or $WORKSPACE for the root in bash commands.",
            p,
            base_dir.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(rel.to_path_buf()))
    }
}

/// Resolve the ripgrep binary path. Precedence: `FLAWRENCE_RG_BIN` env, then a
/// PATH search for an executable named `rg`, then a small list of common
/// fallback locations. Falls back to the bare name `rg` (letting the spawn
/// fail with a clear error) only if nothing is found — `grep_files` otherwise
/// silently no-ops when the harness process PATH omits the `rg` install dir.
fn resolve_rg_bin() -> String {
    if let Ok(p) = std::env::var("FLAWRENCE_RG_BIN")
        && !p.is_empty()
    {
        return p;
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("rg");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }) {
        return found.to_string_lossy().into_owned();
    }
    for fb in [
        "/usr/local/bin/rg",
        "/usr/bin/rg",
        "/home/ubuntu/.pi/agent/bin/rg",
    ] {
        if std::path::Path::new(fb).is_file() {
            return fb.to_string();
        }
    }
    "rg".to_string()
}

fn validate_patch_path(path: &str) -> Result<()> {
    if path == "/dev/null" {
        return Ok(());
    }
    if path.starts_with('"') {
        anyhow::bail!("Quoted patch paths are not supported: {path}");
    }

    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        anyhow::bail!("Patch contains an empty path");
    }

    let candidate = Path::new(path);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => anyhow::bail!("Patch path escapes working directory: {path}"),
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("Patch path must be relative: {path}")
            }
        }
    }
    Ok(())
}

fn validate_patch_paths(patch: &str) -> Result<()> {
    if patch.contains("GIT binary patch") || patch.contains("Binary files ") {
        anyhow::bail!(
            "Binary patches are not supported. Use write_file for binary content, or produce a \
             text-only diff."
        );
    }

    let mut saw_path = false;
    let mut in_file_header = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_file_header = true;
            let mut parts = rest.split_whitespace();
            let old_path = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("Malformed diff --git line"))?;
            let new_path = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("Malformed diff --git line"))?;
            validate_patch_path(old_path)?;
            validate_patch_path(new_path)?;
            saw_path = true;
        } else if line.starts_with("@@") {
            in_file_header = false;
        } else if in_file_header
            && let Some(path) = line
                .strip_prefix("rename from ")
                .or_else(|| line.strip_prefix("rename to "))
                .or_else(|| line.strip_prefix("copy from "))
                .or_else(|| line.strip_prefix("copy to "))
        {
            validate_patch_path(path)?;
            saw_path = true;
        } else if in_file_header
            && let Some(rest) = line
                .strip_prefix("--- ")
                .or_else(|| line.strip_prefix("+++ "))
        {
            let path = rest.split_whitespace().next().unwrap_or(rest);
            validate_patch_path(path)?;
            saw_path = true;
        }
    }

    if !saw_path {
        anyhow::bail!(
            "Patch contains no file paths — it must be a unified diff with `diff --git a/... b/...` \
             or `--- a/...` / `+++ b/...` headers, as produced by `git diff`."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_missing_path_points_at_the_deepest_directory_that_exists() {
        // Shape from a live run: the model invented `…/awesn1/io/` under a real
        // package directory and got a bare ENOENT back.
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("kxs/src/commonMain/kotlin/at/asitplus");
        std::fs::create_dir_all(&real).unwrap();

        let error = safe_canonicalize_path(
            &base.path().canonicalize().unwrap(),
            "kxs/src/commonMain/kotlin/at/asitplus/awesn1/io/",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Path not accessible"), "{error}");
        assert!(
            error.contains("kxs/src/commonMain/kotlin/at/asitplus"),
            "{error}"
        );

        assert!(error.contains("list_directory"), "{error}");
    }

    #[test]
    fn a_wrong_path_with_the_right_file_name_gets_a_suggestion() {
        // The exact shape agents copy out of the attack-surface artifact:
        // an abbreviated or mis-packaged path whose file name is correct.
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        let real = base.join("kxs/src/commonMain/kotlin/at/asitplus/internal");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("DerDecoder.kt"), "class DerDecoder\n").unwrap();
        let index = flawrence_index::CodebaseIndex::build(&base);

        let matches = index.files_named("DerDecoder.kt");
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0].ends_with("at/asitplus/internal/DerDecoder.kt"),
            "{matches:?}"
        );
        // A name the index does not know yields nothing to suggest.
        assert!(index.files_named("Nope.kt").is_empty());
    }

    #[test]
    fn a_relative_walk_is_normalised_before_being_reported() {
        // Observed live: the model wrote
        // `kxs/src/commonTest/kotlin/at/asitplus/awesn1/serialization/../../../../core/...`
        // and the hint echoed the `../../..` back at it.
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("kxs/src/commonTest/kotlin")).unwrap();

        let error = safe_canonicalize_path(
            &base,
            "kxs/src/commonTest/kotlin/at/asitplus/../../../../core/src/Missing.kt",
        )
        .unwrap_err()
        .to_string();

        let hint = error
            .split("deepest existing directory: ")
            .nth(1)
            .expect("hint present");
        assert!(!hint.contains(".."), "hint still contains ..: {hint}");
    }

    #[test]
    fn a_path_outside_the_working_directory_is_still_refused() {
        let base = tempfile::tempdir().unwrap();
        let error = safe_canonicalize_path(&base.path().canonicalize().unwrap(), "../etc")
            .unwrap_err()
            .to_string();
        // Either not accessible or escaping — never silently resolved.
        assert!(
            error.contains("Path not accessible") || error.contains("escapes working directory"),
            "{error}"
        );
    }

    #[test]
    fn a_listing_one_item_over_the_cap_still_returns_results() {
        // The failure in the wild: 101 matches against a limit of 100 returned
        // nothing at all, and the model widened the pattern in response.
        let paths: Vec<String> = (0..101).map(|i| format!("kxs/src/File{i}.kt")).collect();
        let items: Vec<&str> = paths.iter().map(String::as_str).collect();

        let (shown, truncated) = take_within_limits(&items, MAX_GLOB_MATCHES);
        assert!(!truncated, "101 matches fit under the 200-item cap");
        assert_eq!(shown.len(), 101);
    }

    #[test]
    fn a_listing_over_the_cap_is_truncated_with_a_count() {
        let paths: Vec<String> = (0..600).map(|i| format!("src/File{i}.kt")).collect();
        let items: Vec<&str> = paths.iter().map(String::as_str).collect();

        let (shown, truncated) = take_within_limits(&items, MAX_GLOB_MATCHES);
        assert!(truncated);
        assert_eq!(shown.len(), MAX_GLOB_MATCHES);
    }

    #[test]
    fn the_response_budget_bites_before_the_item_cap_for_long_lines() {
        // 250 grep lines of 500 columns each is well past the budget.
        let line = "src/Very/Long/Path.kt:42:".to_owned() + &"m".repeat(500);
        let lines: Vec<&str> = std::iter::repeat_n(line.as_str(), MAX_GREP_LINES).collect();

        let (shown, truncated) = take_within_limits(&lines, MAX_GREP_LINES);
        assert!(truncated);
        assert!(shown.len() < MAX_GREP_LINES);
        let rendered: usize = shown.iter().map(|l| l.len() + 1).sum();
        assert!(rendered <= crate::MAX_TOOL_RESPONSE_CHARS);
    }

    #[test]
    fn a_long_read_is_truncated_at_a_line_boundary() {
        // Lines dense enough that 700 of them blow the budget — the shape of
        // the 37871-character AttestationVerifier.kt read.
        let line = "x".repeat(200);
        let lines: Vec<&str> = std::iter::repeat_n(line.as_str(), 700).collect();

        let (selected, truncated) = fit_within_budget(&lines);

        assert!(truncated);
        assert!(!selected.is_empty());
        assert!(selected.len() < lines.len());
        let rendered: usize = selected.iter().map(|l| l.len() + 1).sum();
        assert!(
            rendered <= crate::MAX_TOOL_RESPONSE_CHARS,
            "{rendered} exceeds the budget"
        );
    }

    #[test]
    fn a_short_read_is_returned_whole() {
        let lines = vec!["fn main() {", "    println!(\"hi\");", "}"];
        let (selected, truncated) = fit_within_budget(&lines);

        assert!(!truncated);
        assert_eq!(selected, lines);
    }

    #[test]
    fn one_enormous_line_still_makes_progress() {
        // Otherwise the model would resume at the same offset forever.
        let line = "y".repeat(crate::MAX_TOOL_RESPONSE_CHARS * 2);
        let lines = vec![line.as_str(), "next"];
        let (selected, truncated) = fit_within_budget(&lines);

        assert_eq!(selected.len(), 1);
        assert!(truncated);
    }
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_search_dir_none_when_no_path() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        assert!(resolve_search_dir(&base, None).unwrap().is_none());
    }

    #[test]
    fn resolve_search_dir_returns_relative_subdir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("src/tools")).unwrap();

        let result = resolve_search_dir(&base, Some("src/tools")).unwrap();
        assert_eq!(result.unwrap(), PathBuf::from("src/tools"));
    }

    #[test]
    fn resolve_search_dir_dot_resolves_to_none() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        assert!(resolve_search_dir(&base, Some(".")).unwrap().is_none());
        assert!(resolve_search_dir(&base, Some("./")).unwrap().is_none());
    }

    #[test]
    fn resolve_search_dir_rejects_parent_traversal() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        let result = resolve_search_dir(&base, Some("../"));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_search_dir_rejects_nonexistent_path() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        let result = resolve_search_dir(&base, Some("nonexistent"));
        assert!(result.is_err());
    }

    #[test]
    fn validate_patch_paths_accepts_relative_git_diff() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        validate_patch_paths(patch).unwrap();
    }

    #[test]
    fn validate_patch_paths_rejects_parent_traversal() {
        let patch = "diff --git a/src/lib.rs b/../outside\n--- a/src/lib.rs\n+++ b/../outside\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(validate_patch_paths(patch).is_err());
    }

    #[test]
    fn validate_patch_paths_rejects_binary_patch() {
        let patch = "diff --git a/a.png b/a.png\nGIT binary patch\n";
        assert!(validate_patch_paths(patch).is_err());
    }

    #[test]
    fn validate_patch_paths_rejects_unsafe_rename_target() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\nsimilarity index 100%\nrename from src/lib.rs\nrename to ../outside\n";
        assert!(validate_patch_paths(patch).is_err());
    }
}
