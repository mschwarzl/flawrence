use anyhow::{Context, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use flawrence_agent::{AgentRegistry, SkillRegistry};
use flawrence_api::{
    AnthropicClient, BatchClient, ChatClient, OpenAiAuth, RoutingClient, StreamingClient,
};
use flawrence_index::CodebaseIndex;
use flawrence_task::logging::CustomFormat;
use flawrence_task::pool::TaskPool;
use flawrence_tool::config::{ClientMode, Config};
use flawrence_tool::registry;
use flawrence_tool::runtime::RuntimeContext;
use flawrence_tool::scheduling::Schedule;
use flawrence_tool::shutdown::{ABORT, ShutdownSignal};

mod tui;

#[derive(Parser)]
#[command(name = "flawrence")]
#[command(about = "An agentic AI system for security research")]
struct Cli {
    /// Use only the supplied environment, without loading a local `.env` file.
    #[arg(long)]
    no_dotenv: bool,

    #[arg(short, long, default_value = "default")]
    agent: String,

    #[arg(short, long)]
    directory: Option<PathBuf>,

    #[arg(long)]
    artifacts: Option<PathBuf>,

    #[arg(long)]
    transcripts: Option<PathBuf>,

    #[arg(short = 'c', long)]
    concurrent: Option<usize>,

    #[arg(short = 'n', long)]
    runs: Option<usize>,

    #[arg(short = 's', long)]
    schedule: Option<String>,

    #[arg(short, long)]
    model: Option<String>,

    #[arg(long)]
    plain: bool,

    /// Print detailed step-by-step agent execution (reasoning, tool calls, results)
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Show a full-screen live dashboard instead of streaming logs (Unix only).
    /// JSON/log output stays the default.
    #[arg(long)]
    tui: bool,

    /// Enable the web status UI server.
    #[arg(long)]
    web_ui: bool,

    /// Port for the web UI server.
    #[arg(long, default_value = "3000")]
    web_ui_port: u16,

    /// Host address for the web UI server.
    #[arg(long, default_value = "127.0.0.1")]
    web_ui_host: String,

    /// Load all agents from the agents directory (eager load).
    /// By default, only agents reachable from --agent are loaded.
    #[arg(long)]
    load_all: bool,

    /// Pre-load an input artifact (can be repeated). Format: key=/path/to/file
    /// Input artifacts are read-only files loaded into the artifact store before the agent starts.
    #[arg(long = "input-artifact", value_parser = parse_input_artifact)]
    input_artifacts: Vec<(String, PathBuf)>,

    /// Override agent parameters (can be repeated). Format: key=value
    /// Parameters are substituted into the agent prompt before execution.
    #[arg(long = "arg", value_parser = parse_key_value)]
    args: Vec<(String, String)>,

    query: Option<String>,
}

/// Parse --input-artifact key=/path/to/file into (key, resolved_path).
fn parse_input_artifact(s: &str) -> Result<(String, PathBuf), String> {
    let (key, path_str) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid format '{}'. Expected key=/path/to/file", s))?;
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "Invalid artifact key '{}'. Keys must match [a-z0-9_]+",
            key
        ));
    }
    Ok((key.to_string(), PathBuf::from(path_str)))
}

/// Parse --arg key=value into (key, value).
fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("Invalid format '{}'. Expected key=value", s))?;
    if key.is_empty() {
        return Err("Parameter key cannot be empty".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

/// Prefix-glob match, mirroring `ArtifactPermissions::matches_glob`: a
/// pattern `foo_*` matches any key with that prefix; everything else matches
/// exactly.
fn glob_match(pattern: &str, key: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        key.starts_with(prefix)
    } else {
        pattern == key
    }
}

/// Whether a declared input-artifact pattern is satisfied by the provided
/// keys. An exact key is mandatory; a glob pattern declares an *optional
/// family* — zero or more provided keys may match, and its absence is not an
/// error (agents that consume open-ended input families fall back gracefully
/// when none are supplied).
fn input_key_satisfied(pattern: &str, provided: &HashSet<String>) -> bool {
    if pattern.contains('*') {
        // Optional family: satisfied by zero or more matching keys — its
        // absence is never an error.
        true
    } else {
        provided.contains(pattern)
    }
}

/// Whether a provided key belongs to any glob input family declared by a
/// loaded agent (so it is a legitimate input even though no exact key
/// demanded it).
fn matches_input_family(key: &str, required: &HashSet<String>) -> bool {
    required
        .iter()
        .any(|pattern| pattern.contains('*') && glob_match(pattern, key))
}

/// Keep completed dashboards available, but never wait for another Ctrl+C after abort.
async fn finish_dashboard(
    handle: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    shutdown: &ShutdownSignal,
) {
    if !shutdown.at_least(ABORT) {
        tokio::signal::ctrl_c().await.ok();
    }
    let _ = shutdown_tx.send(());
    if shutdown.at_least(ABORT) {
        // Active SSE connections must not keep a force-quit process alive.
        handle.abort();
    } else {
        tokio::select! {
            _ = handle => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.no_dotenv {
        dotenvy::dotenv().ok();
    }

    tracing_subscriber::registry()
        .with(fmt::layer().event_format(CustomFormat::auto()))
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    let concurrent = cli.concurrent.unwrap_or(1);
    if concurrent < 1 {
        eprintln!("Error: --concurrent must be at least 1");
        std::process::exit(1);
    }
    let max_runs: Option<usize> = match (cli.concurrent, cli.runs) {
        (_, Some(n)) => Some(n),
        (Some(_), None) => None,
        (None, None) => Some(1),
    };
    if let Some(n) = max_runs
        && n < concurrent
    {
        eprintln!(
            "Error: --runs ({}) must be >= --concurrent ({})",
            n, concurrent
        );
        std::process::exit(1);
    }

    let agent_name = &cli.agent;
    let available_tools = registry().tool_names();

    let agents_dir = std::env::var("FLAWRENCE_AGENTS_DIR").unwrap_or_else(|_| "agents".to_string());

    eprintln!("Loading agents...");
    let mut agents = if cli.load_all {
        AgentRegistry::load_from_dir(&agents_dir, &available_tools)
            .context("Failed to load agents")?
    } else {
        AgentRegistry::load_reachable(&agents_dir, agent_name, &available_tools)
            .context("Failed to load agents")?
    };

    eprintln!("Loading skills...");
    let skills = SkillRegistry::load_all().context("Failed to load skills")?;

    let base_directory = match cli.directory {
        Some(ref dir) => dir
            .canonicalize()
            .with_context(|| format!("Failed to resolve directory '{}'", dir.display()))?,
        None => std::env::current_dir().context("Failed to get current directory")?,
    };

    let config = Config::from_env()
        .context("Failed to load configuration.")?
        .with_model_override(cli.model.clone());

    if let Some(model) = &config.model_override {
        agents.apply_model_override(model);
        eprintln!("Model override: {model}");
    }

    let schedule = cli
        .schedule
        .map(|s| Schedule::parse(&s))
        .transpose()
        .context("Invalid --schedule value")?
        .map(std::sync::Arc::new);

    let mut router = RoutingClient::new();

    // Workers AI client (requires CF creds)
    if let (Some(token), Some(account)) = (&config.cf_api_token, &config.cf_account_id) {
        let workers_client: std::sync::Arc<dyn ChatClient> = match config.client_mode {
            ClientMode::Streaming => {
                let c = StreamingClient::new(
                    token.clone(),
                    account.clone(),
                    config.workers_ai_base_url.clone(),
                )
                .context("Failed to create streaming API client")?;
                std::sync::Arc::new(c)
            }
            ClientMode::Batch => {
                let c =
                    BatchClient::new(token.clone(), account.clone(), config.proxy_url.as_deref())
                        .context("Failed to create batch API client")?;
                std::sync::Arc::new(c)
            }
        };
        router = router.with_workers_ai(workers_client);
    }

    // Anthropic client (API key only)
    if let Some(ref key) = config.anthropic_api_key {
        router = router.with_anthropic(std::sync::Arc::new(AnthropicClient::new(
            key.clone(),
            config.anthropic_base_url.clone(),
        )));
    }

    // `openai/` models use the Responses API. An API key wins over a gateway token.
    let openai_auth = config
        .openai_api_key
        .clone()
        .map(OpenAiAuth::ApiKey)
        .or_else(|| {
            config
                .opencode_access_token
                .clone()
                .map(OpenAiAuth::OpencodeToken)
        });
    if let Some(auth) = openai_auth {
        let openai_client = StreamingClient::new_openai(auth, config.openai_base_url.clone())
            .context("Failed to create OpenAI API client")?;
        router = router.with_openai(std::sync::Arc::new(openai_client));
    }

    // Custom OpenAI-compatible providers from FLAWRENCE_PROVIDER_* env vars
    for (name, provider_cfg) in &config.providers {
        let headers: Vec<(String, String)> = provider_cfg
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let client = StreamingClient::from_url_with_headers(
            provider_cfg.api_key.clone(),
            provider_cfg.base_url.clone(),
            &headers,
        )
        .with_context(|| format!("Failed to create client for provider: {name}"))?;
        router = router.with_provider(name.clone(), std::sync::Arc::new(client));
    }

    let client: std::sync::Arc<dyn ChatClient> = std::sync::Arc::new(router);

    let index_start = std::time::Instant::now();
    let codebase_index = CodebaseIndex::build(&base_directory);
    let index_summary = codebase_index.summarize();
    let index_elapsed = index_start.elapsed();
    eprintln!(
        "Indexed {} files ({} defs) in {:.2}s",
        index_summary.file_count,
        index_summary.definition_count,
        index_elapsed.as_secs_f64()
    );

    // Before anything else that depends on it. Detection rules live in the
    // idiom skills, so a typo there is no longer a compile error -- and a skill
    // that declares a build system and gets the shape wrong would otherwise be
    // read as one that declares nothing, leaving that system silently
    // undetectable on a real target. Fail here instead, naming the file.
    match flawrence_tool::build_systems::validate() {
        Ok(count) => tracing::info!("Build systems declared by skills: {count}"),
        Err(error) => {
            eprintln!("Build-system declarations are invalid: {error:#}");
            std::process::exit(2);
        }
    }

    // Resolve and validate input artifacts (before agents is moved into RuntimeContext)
    let mut seen_keys = HashSet::new();
    let mut input_entries: Vec<(String, PathBuf)> = Vec::new();
    for (key, path) in &cli.input_artifacts {
        if !seen_keys.insert(key.clone()) {
            anyhow::bail!("Duplicate input artifact key: {}", key);
        }
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            base_directory.join(path)
        };
        if !resolved.exists() {
            anyhow::bail!("Input artifact '{}' not found: {}", key, resolved.display());
        }
        input_entries.push((key.clone(), resolved));
    }

    // Collect required input artifacts from all reachable agents
    let required_keys: HashSet<String> = agents.iter().flat_map(|a| a.input_artifacts()).collect();

    // Validate all required are provided. An exact input key is mandatory;
    // a glob input pattern (e.g. `notes_*`) declares an optional family —
    // zero or more provided keys may match, and its absence is not an error.
    for key in &required_keys {
        if !input_key_satisfied(key, &seen_keys) {
            let agent_names: Vec<&str> = agents
                .iter()
                .filter(|a| a.input_artifacts().contains(key))
                .map(|a| a.name.as_str())
                .collect();
            let by = if agent_names.len() == 1 {
                format!("agent '{}'", agent_names[0])
            } else {
                format!("agents {}", agent_names.join(", "))
            };
            anyhow::bail!(
                "Input artifact '{}' required by {} but not provided. \
                 Provide it via: --input-artifact {}=/path/to/file",
                key,
                by,
                key
            );
        }
    }

    // Warn about unused input artifacts — except keys that match a glob input
    // family declared by some loaded agent (e.g. `notes_*`): those are
    // legitimate inputs even though no exact key demanded them.
    for key in &seen_keys {
        if !required_keys.contains(key) && !matches_input_family(key, &required_keys) {
            eprintln!(
                "Warning: Input artifact '{}' provided but not required by any loaded agent",
                key
            );
        }
    }

    // CLI `--arg` overrides are stored on the RuntimeContext and applied to
    // every agent's prompt placeholders uniformly in `run_task` (root and
    // spawned children alike), so subagents inherit the main agent's values
    // without per-level forwarding.
    let arg_overrides: HashMap<String, String> = cli.args.iter().cloned().collect();
    if !arg_overrides.is_empty() {
        eprintln!("\nArg overrides (applied to every agent at run start):");
        for (key, value) in &arg_overrides {
            eprintln!("  {} = {}", key, value);
        }
    }

    let runtime = RuntimeContext::new(
        client,
        config,
        agents,
        skills,
        base_directory.clone(),
        schedule.clone(),
        codebase_index,
        arg_overrides,
    )
    .await?;

    let parsed_agent = runtime
        .agents
        .get(agent_name)
        .with_context(|| format!("Agent '{}' not found", agent_name))?
        .clone();
    let tui_mode = cli.tui;
    let run_info = tui::RunInfo {
        agent: agent_name.clone(),
        model: parsed_agent.model.clone(),
        directory: base_directory.display().to_string(),
        max_iterations: runtime.config.max_iterations as u32,
        budget_cost: runtime.config.max_cost_usd,
        rates: flawrence_chat::model_pricing(&parsed_agent.model).map(|p| tui::Rates {
            input: p.input,
            output: p.output,
            cache_read: p.cache_read,
            cache_write: p.cache_write,
        }),
        cost_estimated: runtime.config.token_cost_override.is_none(),
        alias: if runtime.config.token_cost_override.is_none() {
            flawrence_chat::model_pricing_alias(&parsed_agent.model).map(str::to_string)
        } else {
            None
        },
    };

    // In TUI mode the dashboard renders this summary. In JSON mode (the default)
    // keep the streaming banner and warnings printed to stderr.
    if !tui_mode {
        let use_ansi = io::stderr().is_terminal()
            && std::env::var("NO_COLOR").map_or(true, |value| value.is_empty());
        let (orange, yellow, reset) = if use_ansi {
            ("\x1b[38;5;208m", "\x1b[33m", "\x1b[0m")
        } else {
            ("", "", "")
        };
        let is_custom_provider = parsed_agent
            .model
            .split_once('/')
            .is_some_and(|(provider, _)| runtime.config.providers.contains_key(provider));

        eprintln!("\nFlawrence - Agentic Security Research");
        eprintln!("--------------------------------------");
        eprintln!("Agent: {}", agent_name);
        eprintln!("Model: {}", parsed_agent.model);
        eprintln!("Directory: {}", base_directory.display());
        if runtime.config.token_cost_override.is_none()
            && let Some(alias) = flawrence_chat::model_pricing_alias(&parsed_agent.model)
        {
            eprintln!(
                "{yellow}WARNING: Cost lookup used best-effort provider alias '{}' for '{}'.{reset}",
                alias, parsed_agent.model
            );
        }
        if let Some(pricing) = flawrence_chat::model_pricing(&parsed_agent.model) {
            let label = if runtime.config.token_cost_override.is_some() {
                "Configured model cost"
            } else {
                "Estimated model cost"
            };
            eprintln!(
                "{orange}{label}: ${:.2}/MTok input, ${:.2}/MTok output, ${:.2}/MTok cache read, ${:.2}/MTok cache write{reset}",
                pricing.input, pricing.output, pricing.cache_read, pricing.cache_write
            );
        } else {
            eprintln!("Estimated model cost: could not be looked up");
        }
        if runtime.config.token_cost_override.is_none() {
            eprintln!(
                "{yellow}WARNING: Model costs are estimates and may differ from your provider's charges.{reset}"
            );
        } else {
            let cache_warning = if runtime
                .config
                .token_cost_override
                .and_then(|cost| cost.cache_read_per_mtok)
                .is_some()
            {
                "Configured costs price cache writes at $0, so totals may be understated."
            } else {
                "Configured costs price cache reads and writes at $0, so totals may be understated."
            };
            eprintln!("{yellow}WARNING: {cache_warning}{reset}");
        }
        if is_custom_provider {
            eprintln!(
                "{yellow}WARNING: Custom providers may report cache reads but omit cache writes, so actual costs may be higher.{reset}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if let Some(max_tokens) = runtime.config.max_total_tokens {
            eprintln!("Token budget: {} tokens (cumulative)", max_tokens);
        }
        if let Some(max_cost) = runtime.config.max_cost_usd {
            eprintln!("Cost budget: ${:.2} (cumulative)", max_cost);
        }
        if let Some(ref sched) = schedule {
            eprintln!("Schedule: {}", sched.display());
        }
        if concurrent > 1 || max_runs.is_none() {
            eprintln!("Concurrency: {}", concurrent);
            match max_runs {
                Some(n) => eprintln!("Total runs: {}", n),
                None => eprintln!("Total runs: unlimited (Ctrl+C to stop)"),
            }
        }
        eprintln!(
            "Ctrl+C: 1x finish in-flight work, 2x cancel subagents but still \
             submit and summarise what was found, 3x force-quit."
        );
        eprintln!();
    } // end: JSON-mode banner

    let query = match cli.query {
        Some(q) => q,
        None => {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .context("Failed to read query from stdin")?;
            input.trim().to_string()
        }
    };

    if query.is_empty() {
        eprintln!("Error: No query provided");
        std::process::exit(1);
    }

    eprintln!("Query: {}", query);
    eprintln!("--------------------------------------------");

    let artifacts_dir = {
        let dir = cli.artifacts.clone().unwrap_or_else(|| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            use std::time::SystemTime;

            // Combine wall-clock nanoseconds and PID for a unique-enough suffix.
            let mut hasher = DefaultHasher::new();
            SystemTime::now().hash(&mut hasher);
            std::process::id().hash(&mut hasher);
            let name = format!("artifacts-{:016x}", hasher.finish());
            std::env::var_os("FLAWRENCE_ARTIFACTS_BASE_DIR")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(name)
        });
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create artifacts directory '{}'", dir.display()))?;
        eprintln!("Artifacts directory: {}", dir.display());
        dir
    };

    let global_usage = runtime.global_usage.clone();
    let worktrees = runtime.worktrees.clone();
    eprintln!("Worktree isolation: {}", worktrees.describe());
    if let Some(lanes) = &runtime.verifier_lanes {
        eprintln!("Verifier lanes: {}", lanes.describe());
    }

    // Compile the target once, before the fan-out, so the ~20 agents that
    // follow share one warm dependency and build cache instead of each paying a
    // cold compile. Deliberately ahead of change tracking: the build's own
    // output is not one of the run's changes. Never fatal.
    {
        let prewarm_root = runtime
            .verifier_lanes
            .as_ref()
            .map(|lanes| lanes.prewarm_root().to_path_buf())
            .unwrap_or_else(|| base_directory.clone());
        // A fact set an earlier run derived against this same commit is reused
        // rather than derived again. The pre-warm is deterministic, so a second
        // attempt reaches the same answer -- and when it cannot (a target whose
        // build fails, like a project that cross-compiles for a toolchain this
        // image lacks) the cached facts are the difference between agents
        // reading a `<build>` block and every one of them discovering the build
        // for itself.
        if let Some(cached) =
            flawrence_tool::prewarm::load_cached_facts(&artifacts_dir, &prewarm_root)
        {
            eprintln!(
                "Build pre-warm: reusing build facts derived earlier for this commit, skipping the build"
            );
            // This run's own report, not the previous run's left in place and
            // not the "pre-warm disabled" one the skipped path would otherwise
            // write over it -- which would also take the emitted `<build>`
            // block with it, while the agents were given exactly that block.
            let record = artifacts_dir.join("build-facts.md");
            if let Err(error) =
                std::fs::write(&record, flawrence_tool::prewarm::adopted_report(&cached))
            {
                eprintln!(
                    "Build pre-warm: build facts not recorded in {}: {error}",
                    record.display()
                );
            }
            let _ = runtime.build_facts.set(cached);
        }
        let prewarm_enabled = runtime.config.prewarm_build && runtime.build_facts.get().is_none();
        let prewarm_specs = runtime.build_systems.clone();
        let prewarm_timeout_secs = runtime.config.prewarm_timeout_secs;
        let outcome = tokio::task::spawn_blocking(move || {
            flawrence_tool::prewarm::prewarm(
                &prewarm_specs,
                &prewarm_root,
                prewarm_enabled,
                Some(std::time::Duration::from_secs(prewarm_timeout_secs)),
            )
        })
        .await;
        match outcome {
            Ok(outcome) => {
                eprintln!("Build pre-warm: {}", outcome.describe());
                // Retained rather than logged and dropped: every one of the
                // ~20 agents that follow would otherwise occupy every Gradle
                // verifier lane for 30-120s rediscovering exactly this.
                if let Some(facts) = outcome.facts() {
                    let _ = runtime.build_facts.set(facts.clone());
                    // Cached for later runs against this artifacts directory,
                    // keyed to the commit these facts describe.
                    if let Err(error) =
                        flawrence_tool::prewarm::save_facts(&artifacts_dir, &base_directory, facts)
                    {
                        eprintln!("Build pre-warm: build facts not cached for reuse: {error}");
                    }
                }
                // Written before the fan-out so a run that is cancelled early
                // still leaves behind the ground truth it was operating on.
                let record = artifacts_dir.join("build-facts.md");
                if let Err(error) = std::fs::write(&record, outcome.audit_report()) {
                    eprintln!(
                        "Build pre-warm: build facts not recorded in {}: {error}",
                        record.display()
                    );
                }
            }
            Err(error) => eprintln!("Build pre-warm: pre-warm task failed: {error}"),
        }
    }

    // Record what the run changes in the workspace, whatever the agent and
    // however the changes happen. Never fatal: a missing git, or a workspace
    // that cannot be tracked, must not stop the analysis.
    // Prefixes the two patch-directory files whose names are otherwise fixed, so
    // a second invocation pointed at the same artifacts directory adds to it
    // rather than overwriting the first run's evidence.
    let patch_session = flawrence_tool::workspace_patch::new_patch_session();
    let workspace_tracker =
        match flawrence_tool::workspace_patch::WorkspaceTracker::start(&base_directory) {
            Ok(tracker) => tracker,
            Err(error) => {
                eprintln!("Change tracking unavailable: {error}");
                None
            }
        };
    if let Some(tracker) = &workspace_tracker {
        eprintln!("Change tracking: {}", tracker.describe());
        if tracker.started_dirty() {
            // A dirty start means the patch, the index, and every agent's view
            // of the code already carry someone else's changes. Say so loudly
            // and pause, so it is seen before a multi-hour run scrolls it away.
            let (red, reset) = if cli.plain {
                ("", "")
            } else {
                ("\x1b[1;31m", "\x1b[0m")
            };
            eprintln!(
                "{red}WARNING: the target workspace is a git repository with uncommitted \
                 changes.{reset}"
            );
            eprintln!(
                "{red}         Agents will read and modify that state. This run's patch \
                 excludes what was{reset}"
            );
            eprintln!(
                "{red}         already there, but a clean checkout gives cleaner \
                 results.{reset}"
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    let patch_dir = artifacts_dir.clone();
    let job_registry = if cli.web_ui {
        Some(flawrence_task::ui::JobRegistry::new())
    } else {
        None
    };
    let agents_for_server = std::sync::Arc::new(runtime.agents.clone());
    let shutdown = runtime.shutdown.clone();
    let pool = TaskPool::new(
        runtime,
        parsed_agent,
        query,
        concurrent,
        max_runs,
        Some(artifacts_dir),
        cli.plain,
        cli.transcripts,
        cli.verbose,
        input_entries,
        workspace_tracker.as_ref().map(|tracker| {
            format!(
                "{}; worktrees: {}",
                tracker.describe(),
                worktrees.describe()
            )
        }),
    )
    .with_job_registry(job_registry.clone());

    /// Write the shared checkout's patch and restore it once the pool is done.
    /// Returns the outcome so it can be listed in the change index.
    fn record_workspace_changes(
        tracker: Option<flawrence_tool::workspace_patch::WorkspaceTracker>,
        artifacts_dir: &std::path::Path,
        session: &str,
    ) -> Option<flawrence_tool::workspace_patch::PatchOutcome> {
        let tracker = tracker?;
        let result = tracker.finish(artifacts_dir, session);
        match &result {
            Ok(outcome) if outcome.files_changed == 0 => {
                eprintln!(
                    "Workspace unchanged by this run; noted at {}",
                    outcome.patch_path.display()
                );
            }
            Ok(outcome) => {
                eprintln!(
                    "Workspace changes: {} file(s), {} bytes -> {}{}",
                    outcome.files_changed,
                    outcome.bytes,
                    outcome.patch_path.display(),
                    if outcome.binary_omitted {
                        " (binary hunks omitted; patch exceeded the size limit)"
                    } else {
                        ""
                    }
                );
                // The run does not revert what it changed, so say what is left
                // behind rather than letting the next run discover it.
                if outcome.left_dirty > 0 {
                    eprintln!(
                        "Workspace left dirty: {} changed path(s) remain (nothing was reverted). \
                         Reset the checkout before the next run for a clean comparison.",
                        outcome.left_dirty
                    );
                }
            }
            Err(error) => eprintln!("Failed to record workspace changes: {error:#}"),
        }
        result.ok()
    }

    // The web status UI runs as a sidecar alongside the run; --tui wraps
    // pool.run() in a full-screen dashboard. Both are optional and independent.
    let server_handle = if let Some(registry) = job_registry.clone() {
        let addr = format!("{}:{}", cli.web_ui_host, cli.web_ui_port);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Err(e) =
                flawrence_status::run_server(registry, Some(agents_for_server), &addr, shutdown_rx)
                    .await
            {
                tracing::error!("Status server error: {e}");
            }
        });
        Some((handle, shutdown_tx))
    } else {
        None
    };

    let outcome = {
        #[cfg(unix)]
        {
            let _ = (&run_info, &global_usage);
            let dashboard = if tui_mode {
                match tui::live::Dashboard::start(run_info, global_usage)? {
                    Some(dashboard) => Some(dashboard),
                    None => {
                        eprintln!("Could not open a terminal for --tui; continuing in JSON mode.");
                        None
                    }
                }
            } else {
                None
            };
            let run = pool.run().await;
            if let Some(dashboard) = dashboard {
                dashboard.set_finished();
                std::thread::sleep(std::time::Duration::from_millis(600));
                dashboard.stop();
            }
            run
        }
        #[cfg(not(unix))]
        {
            let _ = (&run_info, &global_usage);
            if tui_mode {
                eprintln!("--tui is only supported on Unix; continuing in JSON mode.");
            }
            pool.run().await
        }
    };

    // After the pool, whether it succeeded or failed: the changes a failed run
    // made are as much evidence as the ones a successful run made.
    let mut change_records = Vec::new();
    if let Some(shared) = record_workspace_changes(workspace_tracker, &patch_dir, &patch_session) {
        change_records.push(
            flawrence_tool::workspace_patch::ChangeRecord::for_shared_root(
                base_directory.clone(),
                &shared,
            ),
        );
    }
    // Catch subtrees whose futures were cancelled before normal completion cleanup.
    for worktree in worktrees.worktrees() {
        if let Err(error) = worktrees.archive_and_remove(&worktree, &patch_dir) {
            eprintln!("Keeping worktree {}: {error:#}", worktree.path.display());
        }
    }
    change_records.extend(worktrees.completed_changes());
    if !change_records.is_empty() {
        match flawrence_tool::workspace_patch::write_change_index(
            &patch_dir,
            &change_records,
            &patch_session,
        ) {
            Ok(path) => eprintln!("Change index: {}", path.display()),
            Err(error) => eprintln!("Failed to write the change index: {error:#}"),
        }
    }

    if let Some((handle, shutdown_tx)) = server_handle {
        if !shutdown.at_least(ABORT) {
            println!(
                "All tasks complete. Web UI: http://{}:{}/",
                cli.web_ui_host, cli.web_ui_port
            );
        }
        finish_dashboard(handle, shutdown_tx, &shutdown).await;
    }

    outcome
}

#[cfg(test)]
mod input_artifact_tests {
    use super::*;

    #[tokio::test]
    async fn force_quit_closes_dashboard_without_a_fourth_interrupt() {
        let shutdown = ShutdownSignal::new();
        for _ in 0..3 {
            shutdown.escalate();
        }
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        // Simulate a server with an SSE connection that never finishes.
        let handle = tokio::spawn(std::future::pending());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            finish_dashboard(handle, shutdown_tx, &shutdown),
        )
        .await
        .unwrap();
        shutdown_rx.await.unwrap();
    }

    fn set<'a>(keys: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
        keys.into_iter().map(str::to_string).collect()
    }

    #[test]
    fn exact_key_missing_is_unsatisfied() {
        assert!(!input_key_satisfied("threat_model", &set([])));
    }

    #[test]
    fn exact_key_provided_is_satisfied() {
        assert!(input_key_satisfied("threat_model", &set(["threat_model"])));
    }

    #[test]
    fn glob_family_empty_is_satisfied() {
        // The optional-family rule: zero matching keys is not an error.
        assert!(input_key_satisfied("notes_*", &set([])));
    }

    #[test]
    fn glob_family_is_satisfied_regardless_of_other_keys() {
        // A glob pattern never fails validation — what it matches is decided
        // at seeding time, not here.
        assert!(input_key_satisfied("notes_*", &set(["notes_parser"])));
        assert!(input_key_satisfied("notes_*", &set(["threat_model"])));
    }

    #[test]
    fn provided_key_in_a_family_is_not_unused() {
        let required = set(["notes_*"]);
        assert!(matches_input_family("notes_parser", &required));
        assert!(!matches_input_family("threat_model", &required));
    }

    #[test]
    fn glob_match_mirrors_artifact_permissions() {
        assert!(glob_match("lv_candidate_*", "lv_candidate_smartcast"));
        assert!(!glob_match("lv_candidate_*", "lv_oracle_smartcast"));
        assert!(glob_match("lv_report", "lv_report"));
        assert!(!glob_match("lv_report", "lv_reportx"));
    }
}
