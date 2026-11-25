# Flawrence

Instructions for people and coding agents who work on this repository.
User documentation is in `README.md` and `docs/`.

## Architecture

Flawrence is a Rust workspace (edition 2024, resolver 3) with two binary crates and seven library crates.

| Crate | Role |
| --- | --- |
| `bin/flawrence-runner` | The `flawrence-runner` CLI and the `flawrence-web` launcher. |
| `bin/flawrence-index` | `flawrence-index-cli`, a debug CLI for the code index. |
| `lib/flawrence-chat` | Messages, tool calls, usage and cost types. |
| `lib/flawrence-api` | Provider clients behind the `ChatClient` trait. Anthropic, Workers AI, OpenAI Responses and OpenAI-compatible chat. |
| `lib/flawrence-agent` | Loads `agents/*.md` and `skills/`. Permissions and prompt parameters. |
| `lib/flawrence-tool` | The `ToolDef` trait, every tool, the artifact store, worktrees and verifier lanes. |
| `lib/flawrence-index` | Tree-sitter code index. |
| `lib/flawrence-task` | The agent loop, the task pool, checkpoints and logging. |
| `lib/flawrence-status` | The `--web-ui` status server. |

Agents are Markdown files with YAML front matter. The body is the system prompt.
Permissions are closed by default. An agent can only touch the artifacts and spawn the subagents its front matter grants.
Agents talk to each other only through the artifact store.

The main pipeline is `security-review`.
It runs `threat-model`, `attack-surface` and `test-idiom`, then one `vulnerability-research` agent per attack surface entry.
Research agents spawn `vulnerability-validation`. Confirmed findings go to `vulnerability-poc`.
`vulnerability-patch` and the `ghsa-*` agents handle fixes and disclosure afterwards.

## Building and running

```sh
cargo build --release
flawrence-runner --agent <name> --directory <path> "<query>"
```

Every environment variable is documented in `docs/configuration.md`.
Update that file when you add or change one.

## Provider API compatibility

Notes for anyone touching `flawrence-api` (the provider clients). We are responsible for adapting to each provider's API, so the guiding rule is to depend on the smallest, most stable surface that does the job.

### Beta vs GA features (Anthropic)
- The only beta feature Flawrence uses is structured outputs (`output_config.format`, sent with the `anthropic-beta: structured-outputs-*` header). It is enabled only on requests that set a wire-level JSON-schema output, not on every request. See `AnthropicClient::build_sdk_client`.
- Adaptive thinking (`thinking: {type: "adaptive"}`), the `output_config.effort` control, and strict tool use (`strict: true`) are all GA. They need no beta header. Do not turn on a beta to get them.
- Before adding a beta, confirm the feature genuinely needs one and scope it to the requests that use it. A blanket beta header puts every request under that beta's stricter validation, which is how a tool schema that is fine on the normal API starts returning 400.

### Strict tool schemas
- Every tool is sent `strict: true`, which makes the provider enforce a JSON-schema subset. That subset rejects numeric range bounds (`minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`) on integers, string `pattern`/`format`/length facets, and combinators like `allOf`/`if`/`not`.
- `schemars` emits `minimum: 0` for every unsigned field (`usize`/`u32`), so a plain tool argument like `offset: usize` silently carries a bound the provider rejects.
- `registry::prepare_schema` (through `fold_constraints`) folds range and length bounds into the property description and drops the rest, so emitted tool schemas stay inside the subset and keep strict validation instead of degrading or 400ing. `strict::strict_schema_supported` lists the same keywords and drops the `strict` flag for any schema that still carries one, as a fallback.
- The cross-crate test `flawrence-task/tests/tool_schemas.rs` fails the build if any tool schema falls out of the strict subset. When you add a tool argument with a constraint `schemars` encodes (a bounded integer, a regex, a min-length string), extend `fold_constraints` and `strict.rs` rather than letting the tool quietly lose strict validation.

### Error classification
- Retryability is decided in `flawrence_api::retry` from the HTTP status first, then the provider's structured error type/code, and only then the message text. Do not add substring checks that guess from a stringified error. A transient 503 whose body reads "Authentication service temporarily unavailable" must not be read as a permanent auth failure because the word "authentication" appears in it.

## Testing

```
cargo test                   # whole workspace
cargo test -p flawrence-tool   # a single crate
```

### Where tests go

Put tests in an inline `#[cfg(test)] mod tests` block at the bottom of the source file they cover, after the implementation. The only integration tests are the cross-crate checks in `lib/flawrence-task/tests/`. Do not add on-disk fixtures.

### What to write

- Write unit tests that exercise one module's behavior through its public surface.
- Keep the suite pure and offline. The API-client tests build request bodies, parse synthetic streaming events, and classify errors instead of calling a live endpoint. Do the same, so tests need no network or credentials and stay deterministic.
- For parsers and the indexer, embed small inputs as raw string literals such as `r#"..."#` inline, rather than fixture files (see `flawrence-index`).
- Use `tempfile` for anything that touches the filesystem, so tests stay hermetic (see `flawrence-tool`).
- For terminal UI, render a frame to ratatui's `TestBackend` and assert on the buffer contents (see the `flawrence-runner` dashboard tests).
- Name each test for the behavior it asserts, in `snake_case`.

## Idiomatic Rust Patterns

Write code that reads the way the surrounding crates already read. Prefer the concise, functional style below.

### Concise control flow

Prefer the `?` operator over an explicit `match` for error propagation.

```rust
// Prefer
let result = operation()?;

// Avoid
let result = match operation() {
    Ok(value) => value,
    Err(e) => return Err(MyError::from(e)),
};
```

Use `if let` when handling a single pattern.

```rust
// Prefer
if let Some(value) = option {
    do_something(value);
}

// Avoid
match option {
    Some(value) => do_something(value),
    None => {}
}
```

Use `while let` for loop-until-none patterns.

```rust
// Prefer
while let Some(item) = iterator.next() {
    process(item);
}

// Avoid
loop {
    match iterator.next() {
        Some(item) => process(item),
        None => break,
    }
}
```

Use `let ... else` to bind on the happy path and bail early on the other, instead of nesting the rest of the function inside an `if let`. This is the dominant early-exit style in this codebase.

```rust
// Prefer
let Some(config) = load_config() else {
    return Ok(());
};
use_config(config);

// Avoid
if let Some(config) = load_config() {
    use_config(config);
}
```

Combine a pattern bind with a condition using a let-chain (edition 2024), rather than nesting.

```rust
// Prefer
if let Some(value) = option
    && value.is_valid()
{
    handle(value);
}

// Avoid
if let Some(value) = option {
    if value.is_valid() {
        handle(value);
    }
}
```

### Iterator patterns

Prefer iterator chains over manual index loops. Reach for functional combinators such as `filter_map`, `flatten`, `fold`, and `sum` instead of building intermediate collections.

```rust
// Prefer
let results: Vec<_> = items
    .iter()
    .filter(|item| item.is_valid())
    .map(|item| item.process())
    .collect();

// Avoid
let mut results = Vec::new();
for item in &items {
    if item.is_valid() {
        results.push(item.process());
    }
}
```

### Error handling

This workspace uses `anyhow` throughout: `anyhow::Result`, `bail!`, `ensure!`, and `context` / `with_context`. It does not use `thiserror`. When a caller needs to inspect or downcast a specific failure (for example to read billable usage or a retry flag off it), define a small typed error by hand, the way `OpenAiResponseError` and `BudgetError` do: a `struct` or `enum` with an `impl std::fmt::Display` and an empty `impl std::error::Error`.

Lean on `?`, which lets `anyhow` convert any `std::error::Error` automatically.

```rust
// Prefer: `?` lets anyhow convert the source error
let file = File::open("data.txt")?;

// Avoid: a manual conversion anyhow already performs
let file = File::open("data.txt").map_err(anyhow::Error::from)?;
```

Add a message with `context` or `with_context` rather than `map_err` when all you need is context.

```rust
// Prefer anyhow context over a map_err closure
let file = File::open(&path).with_context(|| format!("failed to open {path:?}"))?;
```

Reserve `map_err` for a real transformation, such as building a typed error that carries data. Use `ok_or`, `and_then`, and `or_else` for combinator-style flows.

### Prefer combinators and `matches!`

Reach for `Option` and `Result` combinators instead of a verbose `match` when transforming or testing a value: `map`, `map_or`, `unwrap_or_default`, `is_some_and`, `is_none_or`, `ok_or`.

```rust
// Prefer
let is_custom = model
    .split_once('/')
    .is_some_and(|(provider, _)| providers.contains_key(provider));

// Avoid
let is_custom = match model.split_once('/') {
    Some((provider, _)) => providers.contains_key(provider),
    None => false,
};
```

Use the `matches!` macro for a boolean pattern test.

```rust
// Prefer
let has_tool = parts.iter().any(|p| matches!(p, ContentPart::ToolCall(_)));

// Avoid
let has_tool = parts.iter().any(|p| match p {
    ContentPart::ToolCall(_) => true,
    _ => false,
});
```

### Builder-style configuration

For optional configuration, prefer consuming `with_*` methods that return `Self`, the pattern used by `RoutingClient`, `ChatRequest`, and `ChatOptions`. This reads better than long positional constructors or many `Option` arguments.

```rust
let router = RoutingClient::new()
    .with_anthropic(anthropic)
    .with_workers_ai(workers);

let options = ChatOptions::default()
    .with_temperature(1.0)
    .with_max_tokens(4096);
```

### Guard arithmetic against overflow

For sizes, indices, token counts, and other values derived from external input, prefer `saturating_*` and `checked_*` arithmetic over bare `+` and `-`, as the token accounting and TUI layout code already do. This avoids panics in debug and silent wraparound in release.

```rust
// Prefer
let remaining = budget.saturating_sub(spent);
let start = logs.len().saturating_sub(visible);

// Avoid
let remaining = budget - spent; // panics or wraps when spent > budget
```

### Signal intent with `#[must_use]`

Annotate a function or type whose result is meaningless to discard, such as a computed price, a retryability flag, or a builder, with `#[must_use]`, so dropping the value is a compiler warning.

```rust
#[must_use]
pub fn retryable(&self) -> bool {
    self.retryable
}
```

### Other idiomatic patterns

- Destructure in `let` bindings and function arguments where it clarifies intent.
- Use the struct-update spread for partial updates, for example `Config { retries: 3, ..base }`.
- Implement and rely on `From` and `Into` for conversions rather than ad hoc functions.
- Accept `impl AsRef<Path>`, `AsRef<str>`, or `AsMut<_>` for flexible parameters instead of forcing an owned type.
- Define extensible behavior behind a trait with `#[async_trait]`, the way `ChatClient` and `ToolDef` do, so new providers and tools plug in without changing call sites.
- Choose the lock for the job. Use `std::sync::Mutex` or `RwLock` for short synchronous critical sections such as the worktree map and the lane free-list. Reach for `tokio::sync::{Mutex, RwLock, Semaphore}` only when the guard is held across an `.await`, the way the artifact store (`RwLock`) and the Gradle lanes (`Semaphore`) do. Do not add `parking_lot`. Since Rust 1.62 the standard `Mutex` is futex-based and small, so the extra dependency is not worth it, and std poisoning is a useful signal.
- Handle a poisoned lock deliberately. `.lock().unwrap()` is fine when corrupted shared state should abort the process. Use `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` to recover and keep going where losing one agent must not kill the whole run, as the worktree manager does. Everywhere else avoid `.unwrap()` and `.expect()` on fallible values and propagate with `?` or `let ... else`.

### Import grouping

Group items from the same crate or module with braces, and prefer imports over fully qualified paths in signatures.

```rust
// Prefer
use flawrence_chat::{ChatMessage, ChatResponse, ChatRole};
use flawrence_tool::runtime::RuntimeContext;

// Avoid
use flawrence_chat::ChatMessage;
use flawrence_chat::ChatResponse;
use flawrence_chat::ChatRole;
```

```rust
// Prefer imported types in signatures
pub async fn run_task(runtime: Arc<RuntimeContext>, query: String) -> Result<()> {
    // ...
}

// Avoid fully qualified paths in signatures
pub async fn run_task(
    runtime: std::sync::Arc<flawrence_tool::runtime::RuntimeContext>,
    query: String,
) -> anyhow::Result<()> {
    // ...
}
```

## Project Philosophy

### Avoid overengineering

- Implement exactly what the task requires. Cover the specified edge cases, but do not add functionality that was not asked for.
- Keep changes minimal and targeted. Modify only what is necessary to fulfill the requirement.
- Resist bells and whistles. When a feature is removed, delete the fields, methods, and imports that only it used.
- Refactor only when it is essential to the task or explicitly requested.

## Quality Assurance

After any change to Rust code, and before marking a task done or committing, run:

```
cargo fmt --all
cargo clippy
cargo test
```

The code must be rustfmt-clean and the build warning-free. CI runs `cargo fmt --all --check` and fails on any formatting drift, so run `cargo fmt --all` before committing rather than leaving it for review. Fix every issue `clippy` reports (dead code, unused imports, needless clones), re-run the checks, and confirm the fixes do not break tests. `cargo clippy` catches common mistakes and non-idiomatic code.

## Documentation

API documentation comes from rustdoc. Run `cargo doc --no-deps --workspace --open` to read it.
User documentation is Markdown in `README.md`, `FINDINGS.md` and `docs/`.
Write it in plain language with short sentences.

Keep `docs/` for permanent documentation.
Plans and reviews (`PLAN_*.md`, `REVIEW_*.md`, `SPEC_*.md`) go in `plans/`, which is gitignored.

### What rustdoc generates automatically

- The crate and module tree, plus the index page per module.
- Full signatures for every type, function, trait, method, and constant.
- Cross-reference links and "view source".
- Inherited `impl` documentation.
- A search index.

You do **not** doc-comment these. Agents only add what rustdoc cannot infer: intent, contracts, examples, and warnings.

### What agents must add

1. **Crate-level module docs (`//!`)** at the top of every `lib.rs` and `main.rs`: one paragraph stating the crate's purpose and its role in the architecture, then a short bullet list of the public modules.
2. **Item-level doc comments (`///`)** on every public item (`pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub type`, `pub const`, and public fields where non-obvious). Private items only need docs when intent is non-obvious.
3. **Doc-tests** (runnable examples in `///` code fences) for the public API of library crates. They compile and run under `cargo test`, so they double as regression tests and catch rotting examples.
4. **Standard sections** where they apply: `# Errors`, `# Panics`, `# Safety`, `# Examples`.
5. **Cross-references** with intra-doc links (`` [`ChatClient`] ``, `` [`Task::run`] ``) instead of plain text, so the generated HTML links correctly.

### Doc-comment style guide (for generated and hand-written code)

Follow the existing Tiger Style "Comments" principle: document the **why** and the **contract**, not the mechanics.

- **First line is the summary.** One line, a noun phrase or imperative sentence, ending with a period. This becomes the one-line description in module indexes and search results.
- **Blank line, then details.** Keep the summary separate from the body so rustdoc renders them distinctly.
- **Document the contract, not the implementation.** State preconditions, postconditions, ownership/lifetime semantics, and units. Do not restate what the code obviously does.
- **No bare `TODO`/`FIXME` in doc comments.** Use a `# TODO` task or open an issue; comments that rot silently violate Zero Technical Debt.
- **Runnable examples over prose.** A compiling doc-test is worth a paragraph of explanation. Prefer:

````rust
/// Parse a chat request from raw JSON.
///
/// Returns an error if the payload is not a valid request object.
///
/// # Errors
///
/// Returns `Err` if `bytes` is not valid JSON or not a request object.
///
/// # Examples
///
/// ```
/// # use flawrence_chat::ChatRequest;
/// let req = ChatRequest::from_json(r#"{ \"model\": \"claude\" }"#)?;
/// assert_eq!(req.model(), "claude");
/// # Ok::<(), anyhow::Error>(())
/// ```
fn from_json(bytes: &str) -> anyhow::Result<ChatRequest> { /* ... */ }
````

The `# ` prefix hides boilerplate (`use`, the trailing `Ok::<()>`) from the rendered docs while keeping the example compilable.

### Doc-test conventions

- Write doc-tests for the **public API of library crates** only (`flawrence-chat`, `flawrence-api`, `flawrence-agent`, `flawrence-tool`, `flawrence-index`, `flawrence-task`, `flawrence-status`). Binary crates do not need doc-tests.
- Keep examples deterministic: no real network calls, no time-of-day, no filesystem outside `std::env::temp_dir()`. Mock external clients.
- Use `no_run` when the example cannot execute standalone (needs a live API key, server, etc.):

````text
/// ```no_run
/// # use flawrence_api::ChatClient;
/// let client = ChatClient::anthropic(key)?;
/// client.complete(req).await?;
/// ```
````
- Use `ignore` for snippets that are illustrative pseudo-code, not compilable.
- Prefer the `# Ok::<(), anyhow::Error>(())` trailer so `fn main` has a unit return and the example type-checks with `?`.

### Browsing the docs

```bash
cargo doc --no-deps --workspace --open      # build all crate docs and open in a browser
```

The output is static HTML under `target/doc/`. For a specific crate, open `target/doc/<crate>/index.html` (e.g. `flawrence_chat`). From any page, use the crate-switcher dropdown (top-left) to move between the crates and the search box to find any item.

On a remote or headless machine, build the docs and serve them over HTTP, then browse from your local machine:

```bash
cargo doc --no-deps --workspace             # build only
python3 -m http.server 8000 --directory target/doc   # open http://<host>:8000/<crate>/index.html
```

### Common pitfalls (from fixing real rustdoc warnings)

- **Angle brackets in prose are parsed as HTML tags.** A doc comment containing `Arc<RwLock>` or a path like `<directory>/x` triggers an "unclosed HTML tag" warning, because rustdoc reads `<RwLock>` / `<directory>` as an opening HTML tag. Wrap the whole token in inline backticks so it is treated as code.
- **Intra-doc links must resolve in scope.** A bare `` [`new_task_id`] `` fails to resolve if the item is not in scope at the comment site — qualify it: `` [`crate::new_task_id`] ``. For methods, use the type-qualified form `` [`AgentConfig::apply_params`] ``.
- **Escaping in links.** If a link target itself contains `[` or `]` (e.g. a slice or array type), escape those brackets with a backslash so they are not treated as link delimiters.

### Commands

```bash
cargo doc --no-deps --workspace --open     # build & open API docs in a browser
cargo test --doc                           # compile + run all doc-tests
cargo doc --no-deps --workspace 2>&1 | grep -E "rustdoc::|unresolved link|unclosed|invalid_html"
                                          # doc-only warnings; output must be empty
```

Treat rustdoc warnings (broken intra-doc links, invalid HTML tags, missing docs) like compiler warnings: fix them before committing. Once a crate's public API is fully documented, add `#![deny(missing_docs)]` to its `lib.rs` to prevent regressions.

## Tiger Style for Rust (Coding Guidelines)

Guidelines for coding agents writing Rust code, based on Tiger Style principles.

### Core Principles

- **Safety** — Write code that works in all situations and catches problems early
- **Performance** — Use resources efficiently from the start
- **Developer Experience** — Produce readable, maintainable code

### 1. Safety

#### Control and Limits

- Simple control flow: favor straightforward control structures. Avoid recursion unless necessary; prefer iterative solutions to keep execution bounded.

```rust
// Prefer
for item in items.iter().take(MAX_ITEMS) {
    process(item);
}
```

- Set fixed limits: use explicit upper bounds on loops and data structures.

```rust
const MAX_RETRIES: usize = 5;
const MAX_BUFFER_SIZE: usize = 4096;
```

- Limit function length: keep functions under 70 lines. Extract helper functions for complex logic.
- Centralize control flow: keep match/if statements in parent functions. Move non-branching logic to pure helper functions.

#### Memory and Types

- Use explicitly sized types: prefer `u32`, `i64`, `u8` over `usize`/`isize` for data that crosses boundaries.

```rust
struct Header {
    version: u8,
    flags: u16,
    length: u32,
    checksum: u64,
}
```

- Static memory allocation: allocate during initialization. Pre-allocate with `Vec::with_capacity`. Consider fixed-size arrays.
- Minimize variable scope: declare variables in the smallest scope possible.

#### Error Handling

- Use assertions: `debug_assert!` for invariants, `assert!` for conditions that must hold in release.

```rust
fn process_chunk(data: &[u8], offset: usize) {
    debug_assert!(offset < data.len(), "offset out of bounds");
    debug_assert!(data.len() % CHUNK_SIZE == 0, "data not aligned");
}
```

- Validate function arguments and return values.
- Handle all errors: never use `.unwrap()` or `.expect()` in library code without justification. Propagate errors with `?`.
- Treat warnings as errors: use `#![deny(warnings)]` or configure in Cargo.toml.
- Avoid implicit defaults: be explicit with function options.

### 2. Performance

- Think early: consider data layout, allocation patterns, and I/O during initial design.
- Napkin math: estimate before implementing (1 GB memory ~250ms, SSD ~1s, network ~8s).
- Batch operations: amortize s by processing multiple items together.
- Optimize in order: Network > Disk > Memory > CPU.
- Write predictable code: avoid patterns that cause cache misses or branch mispredictions.
- Don't rely on compiler optimizations: write clear, efficient code. Be explicit in hot paths.

### 3. Developer Experience

#### Naming

- Clear and consistent: `snake_case` for functions/variables, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for constants.
- Avoid abbreviations: use full words unless universal (id, url, tcp).
- Include units in names: `timeout_ms`, `latency_us_max`, `buffer_size_bytes`.

#### Comments

- Document the 'why': explain intent, not mechanics.
- Don't forget to say why and how.
- Avoid: stating the obvious, outdated comments, redundant TODOs, commented-out code, misleading intent.

#### Organization

- Logical structure: public API first, then private implementation, then tests.
- Simple function signatures: limit parameters. Use builder pattern for complex configuration.
- Construct in-place: initialize structures where declared.

#### Consistency

- Avoid duplicates: maintain single source of truth.
- Pass large types (>16 bytes) by reference.

#### Avoiding Off-by-One Errors

- Distinguish indexes (0-based position), counts (1-based quantity), and sizes (bytes in memory).
- Handle division explicitly: be explicit about rounding.

#### Code Style

- 4 spaces indentation, 100 character line length.
- One statement per line, clear blocks.
- Minimal dependencies: evaluate each crate for necessity and maintenance status.
- Use rustfmt, clippy, cargo test, cargo doc.

#### Zero Technical Debt

- Do it right the first time: design and implement correctly from the start.
- Be proactive: anticipate issues and address them early.
- Build momentum: quality code enables faster future development.

### Quick Reference

| Principle | Action |
|-----------|--------|
| Fixed limits | Use const bounds, bounded collections |
| Explicit types | u32/i64 over usize for data types |
| Static allocation | Pre-allocate, avoid runtime allocation |
| Handle all errors | Use Result, propagate with ? |
| No warnings | #![deny(warnings)] |
| Clear names | Full words, include units |
| Document why | Explain intent, not mechanics |
| Pass by reference | For types > 16 bytes |
| Batch operations | Group I/O and expensive operations |
| Profile first | Measure before optimizing |
