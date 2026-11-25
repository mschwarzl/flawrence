use anyhow::{Context, Result};
use flawrence_chat::UsageByModel;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokio::process::Command;

use crate::command::run_command_bounded;
use crate::{ToolContext, ToolDef};

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_GRAPH_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRYPOINT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 32_000;
const MAX_QUERY_RESULTS: usize = 100;
const MAX_AMBIGUOUS_RESULTS: usize = 20;
const MAX_GRAPH_NODES: usize = 500_000;
const MAX_GRAPH_EDGES: usize = 1_000_000;
const MAX_SOURCE_ENTRIES: usize = 1_000_000;
const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(not(target_os = "macos"))]
const TRAILMARK_MEMORY_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy)]
struct GraphRecord {
    graph_hash: u64,
    source_hash: u64,
}

#[derive(Default)]
struct GraphRegistry {
    records: HashMap<PathBuf, GraphRecord>,
}

static GENERATED_GRAPHS: OnceLock<Mutex<GraphRegistry>> = OnceLock::new();
static HASH_BUILDER: OnceLock<RandomState> = OnceLock::new();
static ANALYZE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static QUERY_SEMAPHORE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrailmarkOp {
    /// Build the code graph for the working directory. Run this first.
    Analyze,
    /// List entry points for the working directory.
    Entrypoints,
    /// List callers of `target` from the cached graph.
    Callers,
    /// List callees of `target` from the cached graph.
    Callees,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrailmarkArgs {
    pub operation: TrailmarkOp,
    /// Exact symbol name or graph id for callers/callees queries. Null means "no target",
    /// which is correct for analyze and entrypoints and an error for callers/callees.
    pub target: Option<String>,
    /// Leave this null to auto-detect every language in the repository. That is
    /// the recommended choice and the more accurate one: trailmark's polyglot
    /// detection covers mixed and multiplatform projects, whereas naming a
    /// single language silently excludes the rest (e.g. `"kotlin"` on a
    /// Kotlin/Java project analyses no Java). Set a language only to
    /// deliberately restrict the analysis.
    pub language: Option<String>,
    /// Subdirectory to analyse, relative to the workspace root. Omit to analyse
    /// the whole repository, which is right for most projects. Narrow it to the
    /// package under review when the repository is large enough that the whole
    /// graph will not build -- analysing fourteen modules to review one package
    /// is slower and no more accurate. Changing it rebuilds the graph, so pick
    /// one scope for the task rather than alternating.
    pub path: Option<String>,
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n... [truncated {} bytes]",
        &value[..end],
        value.len() - end
    )
}

fn trailmark_bin() -> Result<PathBuf> {
    let bin = std::env::var("TRAILMARK_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_home("~/.venv-trailmark/bin/trailmark"));
    anyhow::ensure!(
        bin.is_file(),
        "trailmark binary not found at {}. Install it in a dedicated environment or set \
         TRAILMARK_BIN.",
        bin.display()
    );
    Ok(bin)
}

fn apply_memory_limit(command: &mut Command) {
    // Darwin exposes memory rlimits but rejects attempts to lower them with EINVAL.
    #[cfg(target_os = "macos")]
    let _ = command;

    #[cfg(not(target_os = "macos"))]
    unsafe {
        use std::os::unix::process::CommandExt;

        command.as_std_mut().pre_exec(|| {
            let resource = libc::RLIMIT_AS;
            let mut existing = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(resource, &mut existing) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let requested = TRAILMARK_MEMORY_LIMIT_BYTES as libc::rlim_t;
            let limit = libc::rlimit {
                rlim_cur: if existing.rlim_max == libc::RLIM_INFINITY {
                    requested
                } else {
                    requested.min(existing.rlim_max)
                },
                rlim_max: existing.rlim_max,
            };
            if libc::setrlimit(resource, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "c" | "cairo"
                | "cc"
                | "circom"
                | "cjs"
                | "cpp"
                | "cs"
                | "cxx"
                | "dart"
                | "erl"
                | "fc"
                | "func"
                | "go"
                | "gql"
                | "graphql"
                | "h"
                | "hh"
                | "hpp"
                | "hs"
                | "hxx"
                | "java"
                | "js"
                | "jsx"
                | "kt"
                | "kts"
                | "m"
                | "masm"
                | "mjs"
                | "mm"
                | "move"
                | "php"
                | "proto"
                | "py"
                | "rb"
                | "rego"
                | "rs"
                | "sol"
                | "sql"
                | "sw"
                | "swift"
                | "tact"
                | "thrift"
                | "ts"
                | "tsx"
        )
    )
}

/// Hash of every source file under `root`. Used to detect the tree changing
/// *while* trailmark runs.
fn source_manifest(root: &Path) -> Result<u64> {
    let (root, files) = source_files(root)?;
    hash_files(&root, files)
}

/// Hash of a specific set of files, relative to `root`.
fn hash_files(root: &Path, files: Vec<PathBuf>) -> Result<u64> {
    let mut files = files;
    files.sort();
    files.dedup();
    let mut total_bytes = 0_u64;
    let mut hasher = HASH_BUILDER.get_or_init(RandomState::new).build_hasher();
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        relative.hash(&mut hasher);
        let bytes = read_source_file(&path)?;
        anyhow::ensure!(
            bytes.len() <= MAX_SOURCE_FILE_BYTES,
            "source file exceeds {MAX_SOURCE_FILE_BYTES} bytes: {}",
            path.display()
        );
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        anyhow::ensure!(
            total_bytes <= MAX_SOURCE_BYTES,
            "source tree exceeds {MAX_SOURCE_BYTES} source bytes"
        );
        bytes.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

/// The files a cached graph was built from, as recorded in its own nodes.
///
/// Scoping staleness to these — rather than to the whole tree — is what lets a
/// sibling agent write a PoC into the workspace without invalidating every
/// other agent's graph. A file the graph never covered cannot make the edges it
/// does contain wrong; it only makes the graph incomplete, which is reported
/// separately.
fn graph_source_files(graph: &Value, root: &Path) -> Vec<PathBuf> {
    let Some(nodes) = graph.get("nodes").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = nodes
        .values()
        .filter_map(|node| node.get("location")?.get("file_path")?.as_str())
        .map(PathBuf::from)
        .filter(|path| path.starts_with(root))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Walk the tree once and return the canonical root plus every source file
/// under it, sorted. Content is read only by [`hash_files`].
fn source_files(root: &Path) -> Result<(PathBuf, Vec<PathBuf>)> {
    const SKIP_DIRECTORIES: &[&str] = &[
        ".git",
        ".hg",
        ".svn",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        ".tox",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        "build",
        "dist",
        ".eggs",
        "vendor",
    ];
    let root = root
        .canonicalize()
        .context("failed to resolve source root")?;
    let mut directories = vec![root.clone()];
    let mut source_files = Vec::new();
    let mut entry_count = 0_usize;
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            entry_count += 1;
            anyhow::ensure!(
                entry_count <= MAX_SOURCE_ENTRIES,
                "source tree exceeds {MAX_SOURCE_ENTRIES} entries"
            );
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') && !SKIP_DIRECTORIES.contains(&name.as_ref()) {
                    directories.push(path);
                }
            } else if file_type.is_symlink() && is_source_file(&path) {
                anyhow::bail!(
                    "Trailmark refuses symlinked source files: {}",
                    path.display()
                );
            } else if file_type.is_file() && is_source_file(&path) {
                source_files.push(path);
            }
        }
    }

    let trailmark_configuration_directory = root.join(".trailmark");
    if let Ok(metadata) = std::fs::symlink_metadata(&trailmark_configuration_directory) {
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "Trailmark configuration directory cannot be a symlink: {}",
            trailmark_configuration_directory.display()
        );
    }
    for configuration in [
        root.join("pyproject.toml"),
        trailmark_configuration_directory.join("links.toml"),
        trailmark_configuration_directory.join("entrypoints.toml"),
    ] {
        if std::fs::symlink_metadata(&configuration).is_ok() {
            source_files.push(configuration);
        }
    }

    source_files.sort();
    Ok((root, source_files))
}

fn read_source_file(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open source file {}", path.display()))?;
    let size = file.metadata()?.len();
    anyhow::ensure!(
        size <= MAX_SOURCE_FILE_BYTES as u64,
        "source file exceeds {MAX_SOURCE_FILE_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_SOURCE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_SOURCE_FILE_BYTES,
        "source file grew while reading: {}",
        path.display()
    );
    Ok(bytes)
}

fn parse_graph(graph_json: &str, expected_root: &Path) -> Result<ParsedGraph> {
    let graph: Value =
        serde_json::from_str(graph_json).context("trailmark emitted invalid JSON")?;
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_object)
        .context("trailmark graph has no nodes object")?
        .clone();
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .context("trailmark graph has no edges array")?
        .clone();
    anyhow::ensure!(
        nodes.len() <= MAX_GRAPH_NODES,
        "trailmark graph has too many nodes: {} > {MAX_GRAPH_NODES}",
        nodes.len()
    );
    anyhow::ensure!(
        edges.len() <= MAX_GRAPH_EDGES,
        "trailmark graph has too many edges: {} > {MAX_GRAPH_EDGES}",
        edges.len()
    );
    // Trailmark synthesises an id for a symbol it cannot resolve — most often an
    // `inherits`/`implements` target that lives in a third-party jar — without
    // emitting a node for it. Those edges are normal output, not corruption, so
    // they are dropped rather than allowed to discard a whole analysis. Queries
    // only ever resolve ids that exist in the node table, so nothing answerable
    // is lost; the count is reported so the gap stays visible.
    let resolvable = |edge: &Value| {
        let endpoint = |field: &str| {
            edge.get(field)
                .and_then(Value::as_str)
                .is_some_and(|id| nodes.contains_key(id))
        };
        endpoint("source") && endpoint("target")
    };
    let dangling = edges.iter().filter(|edge| !resolvable(edge)).count();

    let reported_root = graph
        .get("root_path")
        .and_then(Value::as_str)
        .context("trailmark graph has no root_path")?;
    let reported_root = Path::new(reported_root)
        .canonicalize()
        .context("trailmark graph root_path does not exist")?;
    let expected_root = expected_root
        .canonicalize()
        .context("failed to resolve analysis root")?;
    anyhow::ensure!(
        reported_root == expected_root,
        "trailmark graph root mismatch: expected {}, got {}",
        expected_root.display(),
        reported_root.display()
    );

    let mut graph = graph;
    if dangling > 0 {
        tracing::warn!(
            dangling,
            "dropping trailmark edges whose endpoints are not in the graph"
        );
        if let Some(Value::Array(edges)) = graph.get_mut("edges") {
            edges.retain(resolvable);
        }
    }
    Ok(ParsedGraph { graph, dangling })
}

/// A validated graph plus the number of edges dropped for pointing at symbols
/// the graph does not define.
struct ParsedGraph {
    graph: Value,
    dangling: usize,
}

/// Summarise an existing cached graph, if it is still valid for this
/// workspace. `None` means "no usable cache" for any reason, so the caller
/// simply rebuilds.
fn reuse_cached_graph(path: &Path, directory: &Path) -> Option<String> {
    let graph_json = read_limited_file(path, MAX_GRAPH_BYTES).ok()?;
    let parsed = parse_graph(&graph_json, directory).ok()?;
    let (canonical_root, _) = source_files(directory).ok()?;
    let graph_files = graph_source_files(&parsed.graph, &canonical_root);
    let source_hash = hash_files(&canonical_root, graph_files).ok()?;
    verify_graph_generated(path, &graph_json, source_hash).ok()?;

    let dropped = if parsed.dangling > 0 {
        format!(
            "\nunresolved edges dropped: {} (endpoints outside the analysed sources)",
            parsed.dangling
        )
    } else {
        String::new()
    };
    Some(format!(
        "reused the graph already built for this workspace ({})\n{}{}",
        path.display(),
        summarize_graph(&parsed.graph),
        dropped
    ))
}

fn summarize_graph(graph: &Value) -> String {
    let summary = graph.get("summary").unwrap_or(&Value::Null);
    let count = |key: &str| summary.get(key).and_then(Value::as_u64).unwrap_or(0);
    let dependencies: Vec<String> = summary
        .get("dependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(MAX_QUERY_RESULTS)
        .map(|dependency| truncate(dependency, 160))
        .collect();
    format!(
        "language: {}\nnodes: {} (functions: {}, classes: {}, proxies: {})\n\
         call_edges: {}\nentrypoints: {}\ndependencies (showing at most {}): {}",
        graph.get("language").and_then(Value::as_str).unwrap_or("?"),
        count("total_nodes"),
        count("functions"),
        count("classes"),
        count("proxies"),
        count("call_edges"),
        count("entrypoints"),
        MAX_QUERY_RESULTS,
        dependencies.join(", "),
    )
}

fn graph_key(graph_path: &Path) -> Result<PathBuf> {
    graph_path
        .canonicalize()
        .with_context(|| format!("failed to resolve graph cache {}", graph_path.display()))
}

fn graph_hash(graph_json: &str) -> u64 {
    HASH_BUILDER
        .get_or_init(RandomState::new)
        .hash_one(graph_json)
}

fn mark_graph_generated(graph_path: &Path, graph_json: &str, source_hash: u64) -> Result<()> {
    let mut registry = GENERATED_GRAPHS
        .get_or_init(|| Mutex::new(GraphRegistry::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("trailmark graph registry lock is poisoned"))?;
    registry.records.insert(
        graph_key(graph_path)?,
        GraphRecord {
            graph_hash: graph_hash(graph_json),
            source_hash,
        },
    );
    Ok(())
}

fn verify_graph_generated(graph_path: &Path, graph_json: &str, source_hash: u64) -> Result<()> {
    let key = graph_key(graph_path)?;
    let registry = GENERATED_GRAPHS
        .get_or_init(|| Mutex::new(GraphRegistry::default()))
        .lock()
        .map_err(|_| anyhow::anyhow!("trailmark graph registry lock is poisoned"))?;
    let record = registry.records.get(&key).context(
        "cached graph was not generated by this harness process; call operation=analyze first",
    )?;
    anyhow::ensure!(
        record.graph_hash == graph_hash(graph_json),
        "cached graph changed after operation=analyze; rebuild it before querying"
    );
    anyhow::ensure!(
        record.source_hash == source_hash,
        "a source file this graph was built from changed after operation=analyze; \
         re-run operation=analyze before querying"
    );
    Ok(())
}

fn read_limited_file(path: &Path, max_bytes: usize) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let size = file.metadata()?.len();
    anyhow::ensure!(
        size <= max_bytes as u64,
        "{} is too large: {size} bytes > {max_bytes}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= max_bytes,
        "{} grew while reading",
        path.display()
    );
    String::from_utf8(bytes).context("trailmark graph is not valid UTF-8")
}

/// The entry-point listing, or an explanation when there is nothing to list.
///
/// `[]` is three characters that look like a broken tool. For a library it is
/// usually the correct answer -- entry-point detection looks for handlers,
/// routes and `main` functions, and a codec has none of those -- but an empty
/// result cannot say so for itself, and an agent has no way to tell "none
/// exist" from "this did not work". Say which it is, and name what serves the
/// same purpose instead.
fn describe_entrypoints(stdout: &str, directory: &Path) -> String {
    let trimmed = stdout.trim();
    let empty = trimmed.is_empty() || trimmed == "[]" || trimmed == "{}";
    if !empty {
        return truncate(stdout, MAX_OUTPUT_BYTES);
    }
    format!(
        "No entry points found in {}.\n\n\
         This is not a failure and re-running will not change it. Entry-point detection looks for \
         request handlers, routes, CLI `main` functions and similar process boundaries. A library, \
         codec or parser legitimately has none: its entry surface is its public API, reached \
         through whatever calls it.\n\n\
         Treat the public API as the entry surface -- the exported types and their public methods \
         that accept caller-supplied data. `code_index` with operation=outline lists what a file \
         exposes, and trailmark callers/callees traces inward from one of them.",
        directory.display()
    )
}

/// Every cached graph in the artifact directory, as `scope -> file` lines.
///
/// Scopes are per-analysis, so an agent that queries with a different `path`
/// than the one that was analysed must be told which scopes exist rather than
/// that no analysis has run.
fn cached_graphs(graph_path: &Path) -> Vec<(PathBuf, String)> {
    let Some(dir) = graph_path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("trailmark-graph") || !name.ends_with(".json") {
                return None;
            }
            // root_path is the scope, recorded by trailmark itself, so it does
            // not have to be guessed back out of the sanitised file name.
            let scope = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|graph| {
                    graph
                        .get("root_path")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })?;
            Some((entry.path(), scope))
        })
        .collect();
    found.sort();
    found
}

fn available_graphs(graph_path: &Path) -> Vec<String> {
    cached_graphs(graph_path)
        .into_iter()
        .map(|(_, scope)| format!("  path={scope}"))
        .collect()
}

/// The cache file for one analysis scope.
///
/// The graph used to live at a single fixed name, which was fine while every
/// analysis covered the whole workspace. Once a scope can be narrowed, agents
/// analysing different modules would each overwrite the others' graph and then
/// fail their own follow-up queries on a root mismatch -- a shared mutable file
/// with no owner. One file per scope instead. The unscoped name is unchanged,
/// so a whole-workspace analysis keeps its existing cache.
fn graph_file_name(workspace: &Path, directory: &Path) -> String {
    let Ok(relative) = directory.strip_prefix(workspace) else {
        return "trailmark-graph.json".to_string();
    };
    let relative = relative.to_string_lossy();
    if relative.is_empty() {
        return "trailmark-graph.json".to_string();
    }
    // Hashed as well as sanitised: two different scopes must never collide on
    // one file, and a deep path would otherwise outrun the filename limit.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in relative.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let slug: String = relative
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Generous, because the point is that an operator can tell the graphs apart
    // by eye; the hash is what guarantees uniqueness when a path is cut.
    let slug: String = slug.trim_matches('-').chars().take(120).collect();
    format!("trailmark-graph-{slug}-{hash:016x}.json")
}

fn write_graph_cache(path: &Path, graph_json: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .context("trailmark graph path has no parent")?;
    let output_root = parent
        .parent()
        .context("trailmark graph path is outside an artifact root")?;
    anyhow::ensure!(
        parent.file_name().and_then(|name| name.to_str()) == Some("attack-surface")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == "trailmark-graph.json"
                        || (name.starts_with("trailmark-graph-") && name.ends_with(".json"))
                }),
        "unexpected trailmark cache path: {}",
        path.display()
    );
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(output_root)
        .with_context(|| format!("failed to open artifact root {}", output_root.display()))?;
    let cache_directory_name = CString::new("attack-surface")?;
    let mkdir_result =
        unsafe { libc::mkdirat(root.as_raw_fd(), cache_directory_name.as_ptr(), 0o700) };
    if mkdir_result != 0 {
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.raw_os_error() == Some(libc::EEXIST),
            "failed to create trailmark cache directory: {error}"
        );
    }
    let cache_directory_fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            cache_directory_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if cache_directory_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to safely open trailmark cache directory");
    }
    let cache_directory = unsafe { std::fs::File::from_raw_fd(cache_directory_fd) };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary_name = CString::new(format!(
        ".trailmark-graph-{}-{nonce}.tmp",
        std::process::id()
    ))?;
    let temporary_fd = unsafe {
        libc::openat(
            cache_directory.as_raw_fd(),
            temporary_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if temporary_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to create trailmark cache file");
    }
    let mut temporary = unsafe { std::fs::File::from_raw_fd(temporary_fd) };
    if let Err(error) = temporary
        .write_all(graph_json.as_bytes())
        .and_then(|()| temporary.sync_all())
    {
        unsafe {
            libc::unlinkat(cache_directory.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
        return Err(error.into());
    }
    // The scoped name, not a fixed one: the guard above has already checked its
    // shape. Publishing every graph as `trailmark-graph.json` would leave each
    // scope's cache under a name nothing looks for.
    let final_name = CString::new(
        path.file_name()
            .and_then(|name| name.to_str())
            .context("trailmark graph path has no file name")?,
    )?;
    let rename_result = unsafe {
        libc::renameat(
            cache_directory.as_raw_fd(),
            temporary_name.as_ptr(),
            cache_directory.as_raw_fd(),
            final_name.as_ptr(),
        )
    };
    if rename_result != 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::unlinkat(cache_directory.as_raw_fd(), temporary_name.as_ptr(), 0);
        }
        return Err(error).context("failed to publish trailmark cache file");
    }
    Ok(())
}

/// Trailmark is a Python program, so a bad argument comes back as twenty lines
/// of interpreter frames followed by the one line that matters. Surface the
/// exception itself; the frames tell the model nothing it can act on.
fn concise_failure(stderr: &str) -> String {
    let full = truncate(stderr, MAX_OUTPUT_BYTES);
    if !full.contains("Traceback (most recent call last)") {
        return full;
    }
    match full.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(exception) => format!("{} [Python traceback omitted]", exception.trim()),
        None => full,
    }
}

fn terminal_name(id: &str) -> &str {
    id.rsplit([':', '.']).next().unwrap_or(id)
}

fn resolve_target(nodes: &Map<String, Value>, target: &str) -> Result<String> {
    if nodes.contains_key(target) {
        return Ok(target.to_string());
    }
    // Trailmark spells qualified names with dots (`pkg.File:Class.method`),
    // while `code_index` — and therefore this harness's own documentation —
    // uses `::`. A model that learned `Der::decodeFromByteArray` from one tool
    // must not be told "no graph symbol matches" by the other. Accept either,
    // and fall back to the query's own terminal name.
    let dotted = target.replace("::", ".");
    if nodes.contains_key(&dotted) {
        return Ok(dotted);
    }
    // A qualified query is a suffix of the graph id: the id
    // `kxs.…internal.DerEncoder:DerEncoder.encodeValue` ends with
    // `DerEncoder.encodeValue`. Trying that before the bare terminal name keeps
    // the caller's scope qualifier meaningful, which is the difference between
    // one hit and "ambiguous (2 matches)".
    let suffix_matches: Vec<&str> = nodes
        .keys()
        .filter(|id| id.ends_with(&dotted) && dotted.contains('.'))
        .map(String::as_str)
        .collect();
    if let [only] = prefer_resolved(suffix_matches).as_slice() {
        return Ok((*only).to_string());
    }

    let wanted = terminal_name(&dotted).to_string();
    let mut matches: Vec<&str> = nodes
        .keys()
        .filter(|id| {
            let name = nodes
                .get(*id)
                .and_then(|node| node.get("name"))
                .and_then(Value::as_str);
            terminal_name(id) == target
                || terminal_name(id) == wanted
                || name == Some(target)
                || name == Some(dotted.as_str())
        })
        .map(String::as_str)
        .collect();
    matches = prefer_resolved(matches);
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [] => anyhow::bail!(
            "No graph symbol matches '{target}'.{}",
            near_misses(nodes, &wanted)
        ),
        [only] => Ok((*only).to_string()),
        many => {
            let displayed = many
                .iter()
                .take(MAX_AMBIGUOUS_RESULTS)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "Symbol '{target}' is ambiguous ({} matches). Use an exact graph id:\n{}",
                many.len(),
                displayed
            )
        }
    }
}

/// Offer the closest terminal names in the graph. A bare "no match" leaves the
/// caller guessing at spelling; three real ids let it retry once and move on.
fn near_misses(nodes: &Map<String, Value>, wanted: &str) -> String {
    const MAX_SUGGESTIONS: usize = 5;
    let needle = wanted.to_lowercase();
    let mut candidates: Vec<&str> = nodes
        .keys()
        .filter(|id| !id.starts_with("proxy.unresolved:"))
        .filter(|id| {
            let name = terminal_name(id).to_lowercase();
            name.contains(&needle) || needle.contains(&name) && name.len() > 3
        })
        .map(String::as_str)
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    candidates.truncate(MAX_SUGGESTIONS);

    if candidates.is_empty() {
        " Run operation=entrypoints to see what the graph contains, or operation=analyze if the \
         graph is stale."
            .to_string()
    } else {
        format!(
            " Closest graph ids:\n{}",
            candidates
                .iter()
                .map(|id| format!("  {id}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// Drop trailmark's `proxy.unresolved:` placeholders when a real definition
/// also matched. They are stand-ins for symbols outside the analysed sources,
/// so they turn an otherwise exact hit into a spurious ambiguity — the reason
/// `encodeValue` reported "ambiguous (2 matches)" against a real method and
/// `proxy.unresolved:super.encodeValue`.
fn prefer_resolved(matches: Vec<&str>) -> Vec<&str> {
    let resolved: Vec<&str> = matches
        .iter()
        .copied()
        .filter(|id| !id.starts_with("proxy.unresolved:"))
        .collect();
    if resolved.is_empty() {
        matches
    } else {
        resolved
    }
}

fn node_display(nodes: &Map<String, Value>, id: &str) -> String {
    if let Some(location) = nodes.get(id).and_then(|node| node.get("location")) {
        let file = location
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let line = location
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return format!("{id}  ({file}:{line})");
    }
    id.to_string()
}

fn query_graph(graph: &Value, callers: bool, target: &str) -> Result<String> {
    let nodes = graph["nodes"]
        .as_object()
        .context("trailmark graph has no nodes object")?;
    let edges = graph["edges"]
        .as_array()
        .context("trailmark graph has no edges array")?;
    let resolved_target = resolve_target(nodes, target)?;
    let mut hits = Vec::new();
    for edge in edges {
        if edge.get("kind").and_then(Value::as_str) != Some("calls") {
            continue;
        }
        let source = edge.get("source").and_then(Value::as_str).unwrap_or("");
        let destination = edge.get("target").and_then(Value::as_str).unwrap_or("");
        let (matched, reported) = if callers {
            (destination, source)
        } else {
            (source, destination)
        };
        if matched == resolved_target {
            hits.push(node_display(nodes, reported));
        }
    }
    hits.sort();
    hits.dedup();
    if hits.is_empty() {
        return Ok(format!(
            "No {} found for '{}'.",
            if callers { "callers" } else { "callees" },
            resolved_target
        ));
    }
    let total = hits.len();
    hits.truncate(MAX_QUERY_RESULTS);
    let mut output = format!(
        "{} {} of '{}':\n{}",
        total,
        if callers { "callers" } else { "callees" },
        resolved_target,
        hits.join("\n")
    );
    if total > MAX_QUERY_RESULTS {
        output.push_str(&format!(
            "\n... [{} more omitted]",
            total - MAX_QUERY_RESULTS
        ));
    }
    Ok(output)
}

impl ToolDef for TrailmarkArgs {
    const NAME: &'static str = "trailmark";
    const DESCRIPTION: &'static str = "Build and query a trailmark code graph for the working directory. operation=analyze rebuilds and caches the graph; entrypoints invokes trailmark directly; callers/callees query an exact symbol from a graph generated during this process. Run analyze before callers/callees. Requires --artifacts for graph queries. \
        The graph is always built from the shared checkout, never from your own worktree, so files YOU add or edit are never in it -- your reproducers and probes will not appear as callers or callees, and that is expected, not a stale graph. Use it to map the target's own code. \
        Use `path` to narrow the analysis to a subdirectory when the repository is too large to graph whole; one graph is cached per scope, so pass the same `path` to callers/callees that you passed to analyze.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let bin = trailmark_bin()?;
        // Absence *and* blank both mean "let trailmark decide"; a model that
        // fills a required field with "" must not end up overriding the default.
        let language = args
            .language
            .as_deref()
            .map(str::trim)
            .filter(|language| !language.is_empty())
            .unwrap_or("auto");
        // Always the shared source, never the calling agent's worktree. A call
        // graph is a map of the target, the same map for every agent, so one
        // cached graph serves the whole fan-out instead of each agent
        // rebuilding an identical one under a path only it matches --
        // serialised behind the analyze lock the entire time. The cost is that
        // an agent's own edits are never in the graph, which the description
        // states outright: reproducers are not part of the target's call graph
        // and nobody should be querying them.
        let workspace = ctx.runtime.base_directory.clone();
        let directory = match args
            .path
            .as_deref()
            .map(str::trim)
            .filter(|scope| !scope.is_empty() && *scope != ".")
        {
            // Confined the same way the file tools are: a scope is an argument
            // from a model, not a trusted path.
            Some(scope) => crate::filesystem::safe_canonicalize_path(&workspace, scope)?,
            None => workspace.clone(),
        };
        let graph_path = {
            let store = ctx.artifact_store.read().await;
            let file = graph_file_name(&workspace, &directory);
            store
                .output_dir()
                .map(|output| output.join("attack-surface").join(file))
        };

        match args.operation {
            TrailmarkOp::Analyze => {
                let _analyze_guard = ANALYZE_LOCK.lock().await;
                // Every research agent runs analyze at the start of its task.
                // They are serialised on this lock, so without a check they
                // re-parse the whole repository one after another for the same
                // answer. Reuse the graph if it is still valid — the same
                // verification a callers/callees query would do.
                if let Some(path) = &graph_path
                    && path.is_file()
                    && let Some(summary) = reuse_cached_graph(path, &directory)
                {
                    return Ok((summary, None));
                }
                let manifest_root = directory.clone();
                let source_hash =
                    tokio::task::spawn_blocking(move || source_manifest(&manifest_root)).await??;
                let mut command = Command::new(&bin);
                command
                    .arg("analyze")
                    .arg("-l")
                    .arg(language)
                    .arg(&directory);
                apply_memory_limit(&mut command);
                let result =
                    run_command_bounded(&mut command, TIMEOUT, MAX_GRAPH_BYTES, MAX_STDERR_BYTES)
                        .await?;
                anyhow::ensure!(!result.timed_out, "trailmark analyze timed out after 300s");
                anyhow::ensure!(
                    !result.stdout_truncated,
                    "trailmark graph exceeds the {MAX_GRAPH_BYTES}-byte limit for {}. Narrow the \
                     analysis with the `path` argument -- pass the subdirectory the review actually \
                     covers rather than the whole repository. Retrying unchanged will fail the same \
                     way.",
                    directory.display()
                );
                anyhow::ensure!(
                    result.exit_code == 0,
                    "trailmark analyze failed (exit {}): {}{}",
                    result.exit_code,
                    concise_failure(&result.stderr),
                    if result.stderr_truncated {
                        "\n[stderr truncated]"
                    } else {
                        ""
                    }
                );
                anyhow::ensure!(
                    !result.stdout.trim().is_empty(),
                    "trailmark emitted no graph"
                );
                let manifest_root = directory.clone();
                let post_source_hash =
                    tokio::task::spawn_blocking(move || source_manifest(&manifest_root)).await??;
                anyhow::ensure!(
                    source_hash == post_source_hash,
                    "source files changed while Trailmark was running; retry analysis"
                );
                let parsed = parse_graph(&result.stdout, &directory)?;
                // Scope the staleness record to the files this graph actually
                // covers, so a sibling agent writing a PoC elsewhere in the
                // workspace does not invalidate it.
                let (canonical_root, _) = source_files(&directory)?;
                let graph_files = graph_source_files(&parsed.graph, &canonical_root);
                let graph_source_hash = hash_files(&canonical_root, graph_files)?;
                let summary = summarize_graph(&parsed.graph);
                let unresolved = if parsed.dangling > 0 {
                    format!(
                        "\nunresolved edges dropped: {} (endpoints outside the analysed sources, \
                         e.g. supertypes from dependencies)",
                        parsed.dangling
                    )
                } else {
                    String::new()
                };
                let cache_note = if let Some(path) = graph_path {
                    write_graph_cache(&path, &result.stdout)?;
                    mark_graph_generated(&path, &result.stdout, graph_source_hash)?;
                    format!("graph cached at {}", path.display())
                } else {
                    "graph not cached; run with --artifacts to enable callers/callees".to_string()
                };
                Ok((format!("{cache_note}\n{summary}{unresolved}"), None))
            }
            TrailmarkOp::Entrypoints => {
                let manifest_root = directory.clone();
                tokio::task::spawn_blocking(move || source_manifest(&manifest_root)).await??;
                let mut command = Command::new(&bin);
                command
                    .arg("entrypoints")
                    .arg("-l")
                    .arg(language)
                    .arg("--json")
                    .arg(&directory);
                apply_memory_limit(&mut command);
                let result = run_command_bounded(
                    &mut command,
                    TIMEOUT,
                    MAX_ENTRYPOINT_BYTES,
                    MAX_STDERR_BYTES,
                )
                .await?;
                anyhow::ensure!(
                    !result.timed_out,
                    "trailmark entrypoints timed out after 300s"
                );
                anyhow::ensure!(
                    !result.stdout_truncated,
                    "trailmark entrypoint output exceeds the {MAX_ENTRYPOINT_BYTES}-byte limit"
                );
                anyhow::ensure!(
                    result.exit_code == 0,
                    "trailmark entrypoints failed (exit {}): {}",
                    result.exit_code,
                    concise_failure(&result.stderr)
                );
                Ok((describe_entrypoints(&result.stdout, &directory), None))
            }
            TrailmarkOp::Callers | TrailmarkOp::Callees => {
                let path = graph_path
                    .context("callers/callees require --artifacts and operation=analyze first")?;
                let mut path = path;
                let mut directory = directory;
                // No scope given and no whole-workspace graph: if exactly one
                // graph was built, that is unambiguously the one meant. Asking
                // an agent to name a scope it never chose is friction for no
                // information -- but with several, guessing would silently
                // answer from the wrong module.
                if args.path.is_none() && !path.is_file() {
                    let cached = cached_graphs(&path);
                    if let [(only, scope)] = cached.as_slice() {
                        path = only.clone();
                        // The graph's own root, or parse_graph rejects it.
                        directory = PathBuf::from(scope);
                    }
                }
                if !path.is_file() {
                    let existing = available_graphs(&path);
                    anyhow::bail!(
                        "no cached graph for this scope. A graph is cached per `path`, so a query \
                         must use the same `path` the analysis used.{}",
                        if existing.is_empty() {
                            " No graph has been built yet: run operation=analyze first.".to_string()
                        } else {
                            format!(
                                "\nGraphs already built, callable by passing one of these as \
                                 `path`:\n{}",
                                existing.join("\n")
                            )
                        }
                    );
                }
                let target = args
                    .target
                    .context("callers/callees require a target symbol")?;
                let callers = matches!(args.operation, TrailmarkOp::Callers);
                let _permit = QUERY_SEMAPHORE.acquire().await?;
                let output = tokio::task::spawn_blocking(move || {
                    let graph_json = read_limited_file(&path, MAX_GRAPH_BYTES)?;
                    let parsed = parse_graph(&graph_json, &directory)?;
                    let (canonical_root, present) = source_files(&directory)?;
                    let graph_files = graph_source_files(&parsed.graph, &canonical_root);
                    let covered: std::collections::HashSet<&PathBuf> = graph_files.iter().collect();
                    let added = present
                        .iter()
                        .filter(|path| !covered.contains(path))
                        .count();
                    let graph_source_hash = hash_files(&canonical_root, graph_files)?;
                    verify_graph_generated(&path, &graph_json, graph_source_hash)?;

                    let mut answer = query_graph(&parsed.graph, callers, &target)?;
                    if added > 0 {
                        // Honest about coverage without refusing to answer: the
                        // edges below are still accurate for the files the graph
                        // was built from.
                        answer.push_str(&format!(
                            "\n\n[{added} source file(s) present now were not in this graph. \
                             Results are accurate for the analysed files but may be incomplete; \
                             re-run operation=analyze to include the rest.]"
                        ));
                    }
                    Ok::<String, anyhow::Error>(answer)
                })
                .await??;
                Ok((output, None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn target_resolution_rejects_ambiguous_short_names() {
        let nodes = json!({
            "rust.a:Parser.parse": {},
            "rust.b:Parser.parse": {}
        });
        assert!(resolve_target(nodes.as_object().unwrap(), "parse").is_err());
        assert_eq!(
            resolve_target(nodes.as_object().unwrap(), "rust.a:Parser.parse").unwrap(),
            "rust.a:Parser.parse"
        );
    }

    #[test]
    fn double_colon_targets_resolve_against_dotted_graph_ids() {
        // Observed live: the model asked for `Der::decodeFromByteArray`, the
        // spelling `code_index` taught it, and trailmark rejected it outright.
        let nodes = json!({
            "kxs.src.commonMain.Der:Der.decodeFromByteArray": {},
            "kxs.src.commonMain.Der:Der.encodeToByteArray": {}
        });
        let nodes = nodes.as_object().unwrap();

        assert_eq!(
            resolve_target(nodes, "Der::decodeFromByteArray").unwrap(),
            "kxs.src.commonMain.Der:Der.decodeFromByteArray"
        );
        // The dotted spelling and the bare terminal name still work.
        assert_eq!(
            resolve_target(nodes, "Der.decodeFromByteArray").unwrap(),
            "kxs.src.commonMain.Der:Der.decodeFromByteArray"
        );
        assert_eq!(
            resolve_target(nodes, "decodeFromByteArray").unwrap(),
            "kxs.src.commonMain.Der:Der.decodeFromByteArray"
        );
    }

    #[test]
    fn a_scoped_query_beats_an_unresolved_proxy() {
        // Observed live: `DerEncoder.encodeValue` found nothing, and the bare
        // `encodeValue` was rejected as ambiguous against a proxy placeholder.
        let nodes = json!({
            "kxs.src.commonMain.internal.DerEncoder:DerEncoder.encodeValue": {},
            "proxy.unresolved:super.encodeValue": {}
        });
        let nodes = nodes.as_object().unwrap();

        assert_eq!(
            resolve_target(nodes, "DerEncoder.encodeValue").unwrap(),
            "kxs.src.commonMain.internal.DerEncoder:DerEncoder.encodeValue"
        );
        assert_eq!(
            resolve_target(nodes, "DerEncoder::encodeValue").unwrap(),
            "kxs.src.commonMain.internal.DerEncoder:DerEncoder.encodeValue"
        );
        // Even the bare name resolves now: the proxy is not a real rival.
        assert_eq!(
            resolve_target(nodes, "encodeValue").unwrap(),
            "kxs.src.commonMain.internal.DerEncoder:DerEncoder.encodeValue"
        );
    }

    #[test]
    fn a_missing_target_suggests_real_graph_ids() {
        let nodes = json!({
            "kxs.Der:Der.decodeFromByteArray": {},
            "kxs.Der:Der.encodeToByteArray": {},
            "proxy.unresolved:super.decodeFromByteArray": {}
        });
        let error = resolve_target(nodes.as_object().unwrap(), "decodeFromByteArrayy")
            .unwrap_err()
            .to_string();

        assert!(error.contains("No graph symbol matches"), "{error}");
        assert!(error.contains("kxs.Der:Der.decodeFromByteArray"), "{error}");
        // Placeholders are not useful suggestions.
        assert!(!error.contains("proxy.unresolved"), "{error}");
    }

    #[test]
    fn a_target_with_no_relatives_points_at_entrypoints() {
        let nodes = json!({ "kxs.Der:Der.encodeToByteArray": {} });
        let error = resolve_target(nodes.as_object().unwrap(), "zzzznothing")
            .unwrap_err()
            .to_string();
        assert!(error.contains("operation=entrypoints"), "{error}");
    }

    #[test]
    fn genuinely_ambiguous_names_are_still_rejected() {
        let nodes = json!({
            "a.File:Parser.parse": {},
            "b.File:Parser.parse": {}
        });
        assert!(resolve_target(nodes.as_object().unwrap(), "parse").is_err());
    }

    #[test]
    fn query_uses_one_resolved_symbol() {
        let graph = json!({
            "nodes": {
                "rust.a:Parser.parse": {},
                "rust.a:Parser.run": {},
                "rust.b:Parser.parse": {}
            },
            "edges": [
                { "kind": "calls", "source": "rust.a:Parser.run", "target": "rust.a:Parser.parse" }
            ]
        });
        let output = query_graph(&graph, true, "rust.a:Parser.parse").unwrap();
        assert!(output.contains("rust.a:Parser.run"));
    }

    #[test]
    fn a_python_traceback_is_reduced_to_its_exception() {
        let stderr = "Traceback (most recent call last):\n                        File \"/opt/trailmark/bin/trailmark\", line 10, in <module>\n                          sys.exit(main())\n                        File \"/opt/trailmark/lib/python3.13/site-packages/trailmark/parse.py\",                       line 195, in _resolve_directory_languages\n    raise ValueError(msg)\n                      ValueError: Unsupported language: null\n";

        let concise = concise_failure(stderr);
        assert_eq!(
            concise,
            "ValueError: Unsupported language: null [Python traceback omitted]"
        );
    }

    #[test]
    fn a_plain_error_is_passed_through() {
        assert_eq!(
            concise_failure("no such directory\n"),
            "no such directory\n"
        );
    }

    #[test]
    fn a_sibling_agents_scratch_file_does_not_invalidate_the_graph() {
        // Parallel agents write PoCs into the workspace while others query the
        // graph. Scoping staleness to the graph's own files is what keeps those
        // queries working.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let analysed = root.join("Decoder.kt");
        std::fs::write(&analysed, "fun decode() {}\n").unwrap();

        let graph = json!({
            "root_path": root.to_str().unwrap(),
            "nodes": {
                "app.Decoder:decode": {
                    "location": { "file_path": analysed.to_str().unwrap(), "start_line": 1 }
                }
            },
            "edges": []
        });

        let files = graph_source_files(&graph, &root);
        assert_eq!(files, vec![analysed.clone()]);
        let before = hash_files(&root, files.clone()).unwrap();

        // A PoC written by another agent: not in the graph, must not matter.
        std::fs::write(root.join("PocScratch.kt"), "fun poc() {}\n").unwrap();
        assert_eq!(
            hash_files(&root, graph_source_files(&graph, &root)).unwrap(),
            before,
            "an unrelated new file invalidated the graph"
        );
        // The whole-tree manifest does change — which is what used to fail queries.
        assert_ne!(source_manifest(&root).unwrap(), before);

        // Editing a file the graph *was* built from must still invalidate it.
        std::fs::write(&analysed, "fun decode(x: Int) {}\n").unwrap();
        assert_ne!(
            hash_files(&root, graph_source_files(&graph, &root)).unwrap(),
            before
        );
    }

    #[test]
    fn unresolved_supertypes_are_dropped_not_fatal() {
        // Shape taken from a real polyglot run: a Java class inheriting from a
        // Netty base class that lives in a jar, so trailmark synthesises the
        // target id without emitting a node for it.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let graph = json!({
            "root_path": root.to_str().unwrap(),
            "nodes": {
                "app.Handler": {},
                "app.Handler:Handler": {},
                "app.Handler:Handler.channelRead": {}
            },
            "edges": [
                { "kind": "calls", "source": "app.Handler:Handler.channelRead", "target": "app.Handler:Handler" },
                { "kind": "inherits", "source": "app.Handler:Handler", "target": "app.Handler:ChannelInboundHandlerAdapter" },
                { "kind": "implements", "source": "app.Handler:Handler", "target": "app.Handler:AttestationExtension<AuthorizationList>" }
            ]
        })
        .to_string();

        let parsed = parse_graph(&graph, directory.path()).expect("graph must survive");
        assert_eq!(parsed.dangling, 2);

        let edges = parsed.graph["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "only the resolvable edge is kept");
        assert_eq!(edges[0]["kind"], "calls");

        // The surviving graph still answers queries.
        let answer = query_graph(&parsed.graph, true, "app.Handler:Handler").unwrap();
        assert!(answer.contains("channelRead"), "{answer}");
    }

    /// The write guard gates which file the cache may create. It had to widen
    /// for scoped names without becoming a general write primitive.
    #[test]
    fn the_cache_guard_admits_scoped_names_and_nothing_else() {
        let artifacts = tempfile::tempdir().unwrap();
        let dir = artifacts.path().join("attack-surface");
        std::fs::create_dir_all(&dir).unwrap();

        for allowed in [
            "trailmark-graph.json",
            "trailmark-graph-core-0123456789abcdef.json",
        ] {
            let target = dir.join(allowed);
            write_graph_cache(&target, "{}").expect("should be writable");
            // Where it landed, not merely that the call succeeded: publishing
            // every scope under one fixed name is exactly the bug this missed.
            assert!(
                target.is_file(),
                "{allowed} was not published at its own name"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
        }
        for refused in ["graph.json", "trailmark-graph.txt", "notes.md"] {
            assert!(
                write_graph_cache(&dir.join(refused), "{}").is_err(),
                "{refused} must not be writable"
            );
        }
        // Still confined to an attack-surface directory.
        assert!(write_graph_cache(&artifacts.path().join("trailmark-graph.json"), "{}").is_err());
    }

    /// `[]` is three characters that read as a broken tool. bc-java's threat
    /// model agent got exactly that and moved on none the wiser -- for an ASN.1
    /// codec, "no entry points" is the right answer, but nothing said so.
    #[test]
    fn an_empty_entrypoint_listing_explains_itself() {
        let dir = Path::new("/w/core/src/main/java/org/bouncycastle/asn1");
        for empty in ["[]", "  ", "{}", ""] {
            let out = describe_entrypoints(empty, dir);
            assert!(out.contains("No entry points found"), "{empty:?}: {out}");
            // Not a failure, and retrying is wasted.
            assert!(out.contains("not a failure"), "{out}");
            assert!(out.contains("re-running will not change it"), "{out}");
            // Name what serves the same purpose instead.
            assert!(out.contains("public API"), "{out}");
            assert!(out.contains("outline"), "{out}");
            assert!(
                out.contains("bouncycastle/asn1"),
                "the scope matters: {out}"
            );
        }
    }

    /// A real listing must pass through untouched -- no framing to parse, and
    /// no explanation competing with the answer.
    #[test]
    fn a_real_entrypoint_listing_is_returned_as_is() {
        let listing = r#"[{"symbol":"Server::handle","file":"src/http.rs"}]"#;
        assert_eq!(describe_entrypoints(listing, Path::new("/w")), listing);
    }

    /// A query with no `path` must resolve to the one graph that exists. Making
    /// an agent name a scope it never chose is friction for no information --
    /// but with several graphs, guessing would answer from the wrong module.
    #[test]
    fn a_lone_graph_is_found_without_naming_its_scope() {
        let dir = tempfile::tempdir().unwrap();
        let attack = dir.path().join("attack-surface");
        std::fs::create_dir_all(&attack).unwrap();
        let missing = attack.join("trailmark-graph.json");

        let core = dir.path().join("core");
        let prov = dir.path().join("prov");
        for root in [&core, &prov] {
            std::fs::create_dir_all(root).unwrap();
        }
        let graph = |root: &Path| json!({"root_path": root, "nodes": {}, "edges": []}).to_string();

        // Nothing built yet.
        assert!(cached_graphs(&missing).is_empty());

        std::fs::write(
            attack.join("trailmark-graph-core-0000000000000000.json"),
            graph(&core),
        )
        .unwrap();
        let one = cached_graphs(&missing);
        assert_eq!(one.len(), 1);
        // The scope drives the root the graph is then verified against.
        assert_eq!(one[0].1, core.to_string_lossy());

        // A second graph makes it ambiguous, so the caller must choose.
        std::fs::write(
            attack.join("trailmark-graph-prov-1111111111111111.json"),
            graph(&prov),
        )
        .unwrap();
        assert_eq!(cached_graphs(&missing).len(), 2);

        // A file that is not a graph is never a candidate, however lone.
        std::fs::write(
            attack.join("trailmark-graph-broken-2222222222222222.json"),
            "{}",
        )
        .unwrap();
        assert_eq!(
            cached_graphs(&missing).len(),
            2,
            "a graph with no root_path cannot be resolved against"
        );
    }

    /// An agent that queries with a different scope than was analysed must be
    /// told which scopes exist, not that no analysis has run.
    #[test]
    fn available_graphs_reports_each_cached_scope_by_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let attack = dir.path().join("attack-surface");
        std::fs::create_dir_all(&attack).unwrap();

        let core_root = dir.path().join("core/src/main/java");
        let prov_root = dir.path().join("prov");
        std::fs::create_dir_all(&core_root).unwrap();
        std::fs::create_dir_all(&prov_root).unwrap();
        std::fs::write(
            attack.join("trailmark-graph-core-0000000000000000.json"),
            json!({"root_path": core_root, "nodes": {}, "edges": []}).to_string(),
        )
        .unwrap();
        std::fs::write(
            attack.join("trailmark-graph-prov-1111111111111111.json"),
            json!({"root_path": prov_root, "nodes": {}, "edges": []}).to_string(),
        )
        .unwrap();
        // Not a graph; must not be advertised as one.
        std::fs::write(attack.join("attack_surface.md"), "| a | b |").unwrap();

        let listed = available_graphs(&attack.join("trailmark-graph-missing.json"));
        assert_eq!(listed.len(), 2, "{listed:?}");
        // The reported scope is the real path, not the sanitised file name, so
        // it can be pasted straight back as `path`.
        assert!(
            listed.iter().any(|l| l.contains("core/src/main/java")),
            "{listed:?}"
        );
        assert!(listed.iter().any(|l| l.contains("prov")), "{listed:?}");
        assert!(
            !listed.iter().any(|l| l.contains("attack_surface")),
            "{listed:?}"
        );

        assert!(available_graphs(Path::new("/nonexistent/x.json")).is_empty());
    }

    /// Forty agents each scoping to their own module would otherwise share one
    /// mutable graph file: each overwrites the last, and every follow-up query
    /// then fails on a root mismatch against whoever wrote most recently.
    #[test]
    fn each_scope_caches_to_its_own_graph_file() {
        let workspace = Path::new("/w");

        // The unscoped name is unchanged, so an existing cache stays valid.
        assert_eq!(
            graph_file_name(workspace, Path::new("/w")),
            "trailmark-graph.json"
        );

        let core = graph_file_name(workspace, Path::new("/w/core"));
        let prov = graph_file_name(workspace, Path::new("/w/prov"));
        assert_ne!(core, prov, "different modules must not share a file");
        assert!(
            core.starts_with("trailmark-graph-") && core.ends_with(".json"),
            "{core}"
        );
        assert!(
            core.contains("core"),
            "the scope should stay legible: {core}"
        );

        // Same scope, same file -- otherwise the cache never hits.
        assert_eq!(core, graph_file_name(workspace, Path::new("/w/core")));

        // Nested scopes are distinct, and a path with separators stays a single
        // legal filename.
        let nested = graph_file_name(workspace, Path::new("/w/core/src/main/java"));
        assert_ne!(nested, core);
        assert!(!nested.contains('/'), "{nested}");

        // A long path must not outrun the filename limit.
        let deep = graph_file_name(
            workspace,
            Path::new(
                "/w/a/very/deeply/nested/module/path/that/keeps/going/for/quite/a/while/indeed",
            ),
        );
        assert!(deep.len() < 190, "{} chars: {deep}", deep.len());
    }

    /// bc-java blew the 64MB graph cap: fourteen modules analysed to review one
    /// package. The scope has to reach the model, and it has to be confined --
    /// it is an argument from a model, not a trusted path.
    #[test]
    fn the_analysis_scope_is_advertised_and_confined() {
        let schema = serde_json::to_value(schemars::schema_for!(TrailmarkArgs)).unwrap();
        let path = &schema["properties"]["path"];
        assert!(!path.is_null(), "scope must reach the model: {schema}");
        assert!(
            path["description"]
                .as_str()
                .is_some_and(|d| d.contains("Omit")),
            "omitting it must stay the default: {path}"
        );

        let temp = tempfile::tempdir().unwrap();
        // Canonicalised because macOS resolves /var through a symlink, and the
        // helper compares against the base it is given.
        let workspace = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(workspace.join("core/src/main/java")).unwrap();

        let resolved = crate::filesystem::safe_canonicalize_path(&workspace, "core/src/main/java")
            .expect("a subdirectory of the workspace resolves");
        assert!(resolved.starts_with(&workspace), "{resolved:?}");

        // Escaping the workspace must not be analysable.
        assert!(
            crate::filesystem::safe_canonicalize_path(&workspace, "../../etc").is_err(),
            "a scope must not escape the workspace"
        );
    }

    #[test]
    fn a_graph_for_another_directory_is_still_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let graph = json!({
            "root_path": other.path().canonicalize().unwrap().to_str().unwrap(),
            "nodes": {},
            "edges": []
        })
        .to_string();
        assert!(parse_graph(&graph, directory.path()).is_err());
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert!(truncate("abédef", 3).starts_with("ab"));
    }

    #[test]
    fn source_manifest_changes_with_source_content() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("main.rs");
        std::fs::write(&source, "fn first() {}\n").unwrap();
        let first = source_manifest(directory.path()).unwrap();
        std::fs::write(&source, "fn second() {}\n").unwrap();
        let second = source_manifest(directory.path()).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn source_manifest_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        symlink(external.path(), directory.path().join("linked.rs")).unwrap();
        assert!(source_manifest(directory.path()).is_err());
    }

    #[test]
    fn source_manifest_rejects_large_files_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let source = std::fs::File::create(directory.path().join("large.rs")).unwrap();
        source.set_len(MAX_SOURCE_FILE_BYTES as u64 + 1).unwrap();
        assert!(source_manifest(directory.path()).is_err());
    }

    #[test]
    fn graph_cache_rejects_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let artifacts = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        symlink(external.path(), artifacts.path().join("attack-surface")).unwrap();
        let graph_path = artifacts.path().join("attack-surface/trailmark-graph.json");
        assert!(write_graph_cache(&graph_path, "{}").is_err());
    }

    #[tokio::test]
    async fn memory_limited_subprocess_starts_on_this_platform() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("true");
        apply_memory_limit(&mut command);
        let result = run_command_bounded(&mut command, std::time::Duration::from_secs(5), 100, 100)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
    }
}
