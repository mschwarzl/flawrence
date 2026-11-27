use std::path::PathBuf;
use std::sync::Arc;

use termimad::MadSkin;
use tokio::task::JoinSet;
use tracing::info;

use flawrence_agent::AgentConfig;
use flawrence_chat::UsageByModel;
use flawrence_tool::artifact_store::{ArtifactStore, SharedArtifactStore};
use flawrence_tool::event_log::{EventLog, SharedEventLog};
use flawrence_tool::runtime::RuntimeContext;

use super::r#loop::BudgetError;
use super::run_task;
use crate::ui::JobRegistry;

/// Result from a single spawned run, including artifact metadata.
struct RunResult {
    run_id: usize,
    outcome: anyhow::Result<flawrence_chat::TaskOutput>,
    session_id: String,
    artifact_count: usize,
    /// Per-run transcript log, populated during execution.
    event_log: SharedEventLog,
}

/// Everything the per-run reporters need that is not part of the run itself.
#[derive(Clone, Copy)]
struct RunReport<'a> {
    has_artifacts: bool,
    plain_output: bool,
    transcript_dir: Option<&'a std::path::Path>,
    /// Where finding artifacts land, so a failed run can be salvaged from disk.
    artifacts_dir: Option<&'a std::path::Path>,
    run_configuration: &'a str,
}

/// Render an optional path, falling back to a marker when unset.
fn display_path(path: Option<&std::path::Path>, fallback: &str) -> String {
    path.map(|p| p.display().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// Pick a fence long enough to survive backticks inside `content`.
fn fenced(content: &str) -> String {
    let longest_run = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}text\n{}\n{fence}", content.trim_end())
}

/// Runs multiple copies of the same agent+prompt concurrently, replenishing
/// as each completes. Prints results per-completion and an aggregate summary
/// on exit (including Ctrl+C).
pub struct TaskPool {
    runtime: Arc<RuntimeContext>,
    agent: AgentConfig,
    query: String,
    concurrency: usize,
    max_runs: Option<usize>,
    artifacts_dir: Option<PathBuf>,
    plain_output: bool,
    /// Optional directory where each run's transcript is dumped on
    /// completion as `run-<n>.json`. `None` disables transcript writing.
    transcript_dir: Option<PathBuf>,
    /// When true, subscribe to the event log and print each event in real-time.
    verbose: bool,
    /// How the run's workspace changes are being recorded, if at all.
    workspace_tracking: Option<String>,
    /// Pre-rendered snapshot of the query and harness settings for this run.
    run_configuration: String,
    /// Optional registry for web UI tracking.
    job_registry: Option<Arc<JobRegistry>>,
    /// Input artifacts pre-loaded into each run's artifact store.
    /// Each tuple is (key, content, extension).
    input_artifacts: Vec<(String, String, String)>,
}

impl TaskPool {
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the existing pool configuration API"
    )]
    pub fn new(
        runtime: Arc<RuntimeContext>,
        agent: AgentConfig,
        query: String,
        concurrency: usize,
        max_runs: Option<usize>,
        artifacts_dir: Option<PathBuf>,
        plain_output: bool,
        transcript_dir: Option<PathBuf>,
        verbose: bool,
        input_artifact_paths: Vec<(String, PathBuf)>,
        workspace_tracking: Option<String>,
    ) -> Self {
        let input_artifacts = input_artifact_paths
            .into_iter()
            .map(|(key, path)| {
                let content = std::fs::read_to_string(&path)
                    .expect("input artifact file validated at startup");
                let extension = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("md")
                    .to_string();
                (key, content, extension)
            })
            .collect();
        let mut pool = Self {
            runtime,
            agent,
            query,
            concurrency,
            max_runs,
            artifacts_dir,
            plain_output,
            transcript_dir,
            verbose,
            workspace_tracking,
            run_configuration: String::new(),
            job_registry: None,
            input_artifacts,
        };
        pool.run_configuration = pool.format_run_configuration();
        pool
    }

    pub fn with_job_registry(mut self, registry: Option<Arc<JobRegistry>>) -> Self {
        self.job_registry = registry;
        self
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let has_artifacts = self.artifacts_dir.is_some();
        let transcript_dir = self.transcript_dir.clone();
        let report = RunReport {
            has_artifacts,
            plain_output: self.plain_output,
            transcript_dir: transcript_dir.as_deref(),
            artifacts_dir: self.artifacts_dir.as_deref(),
            run_configuration: &self.run_configuration,
        };
        let mut join_set: JoinSet<RunResult> = JoinSet::new();
        let mut next_run: usize = 0;
        let mut completed: usize = 0;
        let mut failed: usize = 0;
        let mut shutting_down = false;
        let mut last_error: Option<String> = None;

        // Seed the initial batch
        while next_run < self.concurrency && !self.run_limit_reached(next_run) {
            self.spawn_run(&mut join_set, next_run);
            next_run += 1;
        }

        loop {
            tokio::select! {
                biased;

                _ = tokio::signal::ctrl_c() => {
                    // Each Ctrl+C escalates the shared shutdown level by one.
                    use flawrence_tool::shutdown::{CONSOLIDATE, DRAINING};
                    let in_flight = join_set.len();
                    match self.runtime.shutdown.escalate() {
                        DRAINING => {
                            // First: start nothing new, let in-flight runs finish.
                            shutting_down = true;
                            eprintln!(
                                "\nShutting down, waiting for {in_flight} in-flight run(s) to finish... \
                                 (Ctrl+C again to cancel them and consolidate what was found)"
                            );
                        }
                        CONSOLIDATE => {
                            // Second: cancel in-flight subagents (their futures are
                            // dropped by the spawner) but let each root loop keep
                            // running so it can submit findings and write a summary.
                            // This still calls the model, so it costs some tokens.
                            shutting_down = true;
                            eprintln!(
                                "\nCancelling in-flight subagents and consolidating findings from \
                                 {in_flight} run(s) (this still uses the model)... \
                                 (Ctrl+C again to force-quit without a summary)"
                            );
                        }
                        _ => {
                            // Third (ABORT): stop everything, summary included.
                            eprintln!("\nForce-quit — aborting all runs...");
                            join_set.abort_all();
                            // Collect anything that completed just before the abort.
                            while let Some(result) = join_set.join_next().await {
                                if let Ok(run) = result {
                                    Self::handle_run_result(
                                        run, report,
                                        &mut completed, &mut failed, &mut last_error,
                                    ).await;
                                }
                            }
                            break;
                        }
                    }
                    // Don't spawn new runs; let the loop drain naturally.
                }

                Some(result) = join_set.join_next() => {
                    match result {
                        Ok(run) => {
                            let terminal_budget_error = run
                                .outcome
                                .as_ref()
                                .err()
                                .is_some_and(|error| {
                                    error.downcast_ref::<BudgetError>().is_some()
                                });
                            Self::handle_run_result(
                                run, report,
                                &mut completed, &mut failed, &mut last_error,
                            ).await;
                            shutting_down |= terminal_budget_error;
                        }
                        Err(e) => {
                            // JoinError (panic or cancellation)
                            if !e.is_cancelled() {
                                eprintln!("Task panicked: {e}");
                            }
                        }
                    }

                    if self.budget_exhausted() {
                        shutting_down = true;
                        eprintln!("Cumulative budget reached; no new runs will be started.");
                    }

                    // Replenish unless shutting down or limit reached
                    if !shutting_down && !self.run_limit_reached(next_run) {
                        self.spawn_run(&mut join_set, next_run);
                        next_run += 1;
                    }

                    // All done?
                    if join_set.is_empty() {
                        break;
                    }
                }

                else => break,
            }
        }

        if completed > 1 || failed > 0 {
            let global_usage = self.runtime.global_usage.lock().unwrap().clone();
            Self::print_aggregate_summary(completed, &global_usage);
        }

        // Propagate error for single-run mode so the process exits non-zero
        if self.max_runs == Some(1)
            && let Some(err) = last_error
        {
            anyhow::bail!("{}", err);
        }

        Ok(())
    }

    async fn handle_run_result(
        run: RunResult,
        report: RunReport<'_>,
        completed: &mut usize,
        failed: &mut usize,
        last_error: &mut Option<String>,
    ) {
        // Persist transcript before printing the summary so the file is on
        // disk even if printing panics.
        if let Some(dir) = report.transcript_dir
            && let Err(e) = Self::write_transcript(&run.event_log, dir, run.run_id).await
        {
            tracing::warn!(error = %e, run_id = run.run_id + 1, "Failed to write transcript");
        }

        match run.outcome {
            Ok(output) => {
                Self::print_run_result(
                    run.run_id,
                    &output,
                    &run.session_id,
                    run.artifact_count,
                    report,
                );
                *completed += 1;
            }
            Err(e) => {
                eprintln!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                eprintln!("Run #{} failed", run.run_id + 1);
                eprintln!("Error: {e}");
                eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                // The run never wrote its own summary, so recover one from the
                // findings already on disk. This is what makes aborting a run on
                // a flaky provider salvage rather than discard the work.
                if let Some(dir) = report.artifacts_dir {
                    let dir = dir.to_path_buf();
                    let session = run.session_id.clone();
                    let salvage = tokio::task::spawn_blocking(move || {
                        flawrence_tool::salvage::salvage_summary(&dir, &session)
                    })
                    .await;
                    match salvage {
                        Ok(salvage) if salvage.found_anything() => match &salvage.summary_path {
                            Some(path) => eprintln!(
                                "Salvaged summary ({}): {}",
                                salvage.describe(),
                                path.display()
                            ),
                            None => eprintln!(
                                "Findings on disk ({}) but the salvaged summary could not be written.",
                                salvage.describe()
                            ),
                        },
                        Ok(_) => {}
                        Err(_) => tracing::warn!("Findings salvage did not complete"),
                    }
                }
                *last_error = Some(format!("{e}"));
                *completed += 1;
                *failed += 1;
            }
        }
    }

    fn budget_exhausted(&self) -> bool {
        let usage = self.runtime.global_usage.lock().unwrap();
        budget_reached(
            &usage,
            self.runtime.config.max_total_tokens,
            self.runtime.config.max_cost_usd,
        )
    }

    async fn write_transcript(
        event_log: &SharedEventLog,
        dir: &std::path::Path,
        run_id: usize,
    ) -> anyhow::Result<()> {
        let path = dir.join(format!("run-{}.json", run_id + 1));
        event_log.finish_transcript().await?;
        eprintln!("Transcript: {}", path.display());
        Ok(())
    }

    fn run_limit_reached(&self, next_run: usize) -> bool {
        self.max_runs.is_some_and(|max| next_run >= max)
    }

    fn spawn_run(&self, join_set: &mut JoinSet<RunResult>, run_id: usize) {
        let runtime = self.runtime.clone();
        let agent = self.agent.clone();
        let query = self.query.clone();
        let artifacts_dir = self.artifacts_dir.clone();
        let verbose = self.verbose;
        let run_configuration = self.run_configuration.clone();
        let job_registry = self.job_registry.clone();
        let transcript_dir = self.transcript_dir.clone();
        let agent_name = self.agent.name.clone();
        let target = self.runtime.base_directory.to_string_lossy().to_string();

        info!(run = run_id + 1, "Starting run");

        let input_artifacts = self.input_artifacts.clone();

        join_set.spawn(async move {
            let event_log = EventLog::new();
            info!(
                run = run_id + 1,
                limit = event_log.max_events(),
                "Dashboard event retention"
            );
            if let Some(directory) = transcript_dir {
                let job_id = format!("run-{}", run_id + 1);
                let path = directory.join(format!("{job_id}.json"));
                if let Err(error) = event_log.enable_transcript(&path, &job_id).await {
                    tracing::warn!(%error, "Failed to create transcript");
                }
            }
            let artifact_store = ArtifactStore::new_shared(artifacts_dir);
            artifact_store.write().await.seed_shared_claims();
            {
                let mut store = artifact_store.write().await;
                for (key, content, extension) in &input_artifacts {
                    if let Err(e) =
                        store.write_with_extension(key.clone(), content.clone(), extension)
                    {
                        return RunResult {
                            run_id,
                            outcome: Err(anyhow::anyhow!(
                                "Failed to load input artifact '{}': {}",
                                key,
                                e
                            )),
                            session_id: "error".to_string(),
                            artifact_count: 0,
                            event_log,
                        };
                    }
                }
            }
            let run_usage = UsageByModel::new_shared();
            let session_id = {
                let store = artifact_store.read().await;
                store.session_id().to_string()
            };
            let task_id = crate::new_task_id();
            if let Some(ref registry) = job_registry {
                registry
                    .insert(
                        run_id,
                        task_id.clone(),
                        agent_name.clone(),
                        query.clone(),
                        Some(target.clone()),
                        event_log.clone(),
                        artifact_store.clone(),
                    )
                    .await;
            }
            if verbose {
                Self::spawn_verbose_printer(event_log.clone(), task_id.clone());
            }
            let title = format!("run#{} ({})", run_id + 1, session_id);
            let outcome = run_task(
                runtime,
                &agent,
                query,
                Vec::new(),
                Some(title),
                artifact_store.clone(),
                event_log.clone(),
                run_usage,
                task_id.clone(),
                None,
            )
            .await;
            if let Some(ref registry) = job_registry {
                registry.finalize(&task_id, &outcome).await;
            }
            if let Ok(output) = &outcome {
                let mut store = artifact_store.write().await;
                let artifact_count = store.keys().len() + 1;
                let statistics = Self::format_session_statistics(
                    output,
                    &session_id,
                    artifact_count,
                    store.output_dir().is_some(),
                );
                let summary = format!(
                    "{}\n\n---\n\n## Run configuration\n\n{}\n\n## Session statistics\n\n{}\n",
                    output.response,
                    fenced(&run_configuration),
                    fenced(&statistics),
                );
                if let Err(error) = store.write("summary".into(), summary) {
                    tracing::warn!(%error, "Failed to persist final summary artifact");
                }
            }
            let artifact_count = Self::count_artifacts(&artifact_store).await;
            RunResult {
                run_id,
                outcome,
                session_id,
                artifact_count,
                event_log,
            }
        });
    }

    fn spawn_verbose_printer(event_log: SharedEventLog, task_id: String) {
        use futures::StreamExt;
        tokio::spawn(async move {
            let stream = event_log.subscribe(None, Some(task_id));
            tokio::pin!(stream);
            while let Some(ev) = stream.next().await {
                Self::print_event(&ev.event);
            }
        });
    }

    fn print_event(event: &flawrence_tool::event_log::TaskEvent) {
        use flawrence_tool::event_log::TaskEvent;
        match event {
            TaskEvent::TaskStarted { agent, query, .. } => {
                eprintln!("\n\x1b[1;36m=== TASK STARTED: agent={agent} ===\x1b[0m");
                eprintln!("    query: {}", &query[..query.len().min(120)]);
            }
            TaskEvent::IterationStarted { iteration } => {
                eprintln!("\n\x1b[1;33m--- Iteration #{iteration} ---\x1b[0m");
            }
            TaskEvent::AssistantMessage {
                content,
                reasoning,
                tool_calls,
            } => {
                if let Some(r) = reasoning
                    && !r.is_empty()
                {
                    let truncated: String = r.chars().take(300).collect();
                    eprintln!("  \x1b[2m[reasoning]\x1b[0m {truncated}");
                }
                if !content.is_empty() {
                    let truncated: String = content.chars().take(200).collect();
                    eprintln!("  \x1b[1m[response]\x1b[0m  {truncated}");
                }
                for tc in tool_calls {
                    let args = serde_json::to_string(&tc.arguments).unwrap_or_default();
                    let args_short: String = args.chars().take(80).collect();
                    eprintln!("  \x1b[32m->\x1b[0m {}({})", tc.name, args_short);
                }
            }
            TaskEvent::ToolCallCompleted {
                tool,
                ok,
                duration_ms,
                output,
                ..
            } => {
                let status = if *ok {
                    "\x1b[32mOK\x1b[0m"
                } else {
                    "\x1b[31mFAIL\x1b[0m"
                };
                let out_str = output
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| serde_json::to_string(output).unwrap_or_default());
                let out_len = out_str.len();
                eprintln!("  \x1b[34m<-\x1b[0m {tool} [{status} {duration_ms}ms] {out_len}ch");
            }
            TaskEvent::TokenUsage {
                model,
                prompt_tokens,
                completion_tokens,
                cumulative_cost_usd,
            } => {
                eprintln!(
                    "  \x1b[2mtokens: {model} p={prompt_tokens} c={completion_tokens} cost=${cumulative_cost_usd:.4}\x1b[0m"
                );
            }
            TaskEvent::SubagentSpawned { agent, title, .. } => {
                eprintln!("  \x1b[35m>> Spawning subagent: {agent} \"{title}\"\x1b[0m");
            }
            TaskEvent::SubagentCompleted { .. } => {
                eprintln!("  \x1b[35m<< Subagent completed\x1b[0m");
            }
            TaskEvent::TaskCompleted { elapsed_secs, .. } => {
                eprintln!("\n\x1b[1;32m=== TASK COMPLETED in {elapsed_secs:.1}s ===\x1b[0m");
            }
            TaskEvent::TaskFailed { error } => {
                eprintln!("\n\x1b[1;31m=== TASK FAILED: {error} ===\x1b[0m");
            }
            _ => {}
        }
    }

    async fn count_artifacts(store: &SharedArtifactStore) -> usize {
        store.read().await.keys().len()
    }

    fn print_run_result(
        run_id: usize,
        output: &flawrence_chat::TaskOutput,
        session_id: &str,
        artifact_count: usize,
        report: RunReport<'_>,
    ) {
        eprintln!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("Run #{} — Final Answer:\n", run_id + 1);

        if report.plain_output {
            eprintln!("{}", &output.response);
        } else {
            let skin = MadSkin::default();
            skin.print_text(&output.response);
        }

        eprintln!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        eprintln!("{}", report.run_configuration);
        eprintln!(
            "{}",
            Self::format_session_statistics(
                output,
                session_id,
                artifact_count,
                report.has_artifacts
            )
        );
    }

    /// A reproducible snapshot of what this run was asked to do and under
    /// which settings. Rendered into both the printed summary and the
    /// `summary` artifact so a finished run can be re-created from its own
    /// output. Credentials are reported as `set`/`unset` only — see
    /// [`flawrence_tool::config::Config::describe`].
    fn format_run_configuration(&self) -> String {
        let runtime = &self.runtime;
        let mut out = String::from("Run configuration:\n");
        let mut row = |name: &str, value: String| {
            out.push_str(&format!("   {name:<18} {value}\n"));
        };

        row("Agent:", self.agent.name.clone());
        row("Model:", self.agent.model.clone());
        row(
            "Reasoning effort:",
            format!("{:?}", self.agent.reasoning_effort),
        );
        row("Directory:", runtime.base_directory.display().to_string());
        // Reported unconditionally: a blank line here would be ambiguous
        // between "not a repository" and "we forgot to look".
        match (
            &runtime.git_info.repository_url,
            &runtime.git_info.commit_hash,
        ) {
            (repository, Some(commit)) => {
                row(
                    "Repository:",
                    repository
                        .as_deref()
                        .map(flawrence_tool::config::redact_url)
                        .unwrap_or_else(|| "(no origin remote)".to_string()),
                );
                row("Commit:", commit.clone());
            }
            (Some(repository), None) => {
                row(
                    "Repository:",
                    flawrence_tool::config::redact_url(repository),
                );
                row("Commit:", "(no commits yet)".to_string());
            }
            // `git` reports neither a remote nor a commit for a non-repository
            // and for a freshly `git init`ed one, so the two are not
            // distinguishable here.
            (None, None) => row(
                "Repository:",
                "(not a git repository, or no commits yet)".to_string(),
            ),
        }
        row(
            "Artifacts:",
            display_path(self.artifacts_dir.as_deref(), "(none)"),
        );
        row(
            "Transcripts:",
            display_path(self.transcript_dir.as_deref(), "(disabled)"),
        );
        row("Concurrency:", self.concurrency.to_string());
        row(
            "Total runs:",
            match self.max_runs {
                Some(runs) => runs.to_string(),
                None => "unlimited".to_string(),
            },
        );
        if let Some(schedule) = &runtime.schedule {
            row("Schedule:", schedule.display());
        }
        if let Some(tracking) = &self.workspace_tracking {
            row("Change tracking:", tracking.clone());
        }
        let index = runtime.codebase_index.summarize();
        let mut languages: Vec<(&String, &usize)> = index.languages.iter().collect();
        languages.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let languages = if languages.is_empty() {
            "no indexed languages".to_string()
        } else {
            languages
                .iter()
                .map(|(name, count)| format!("{name}: {count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        row(
            "Code index:",
            format!(
                "{} files, {} definitions ({languages})",
                index.file_count, index.definition_count
            ),
        );

        out.push_str("\nHarness settings:\n");
        for (name, value) in runtime.config.describe() {
            out.push_str(&format!("   {name}={value}\n"));
        }

        out.push_str("\nQuery:\n");
        for line in self.query.lines() {
            out.push_str(&format!("   {line}\n"));
        }
        out
    }

    fn format_session_statistics(
        output: &flawrence_chat::TaskOutput,
        session_id: &str,
        artifact_count: usize,
        has_artifacts: bool,
    ) -> String {
        let billed_tokens = output.usage.total();
        let attributed_tokens = output.attributed_usage.total();
        let overhead_tokens = billed_tokens.saturating_sub(attributed_tokens);
        let overhead_percent = if billed_tokens == 0 {
            0.0
        } else {
            overhead_tokens as f64 * 100.0 / billed_tokens as f64
        };
        let billed_cost = output.usage.total_cost().abs();
        let attributed_cost = output.attributed_usage.total_cost().abs();
        let overhead_cost = (billed_cost - attributed_cost).max(0.0);
        let mut statistics = format!(
            "Session: {session_id}\nElapsed time: {:.2}s\n\nToken accounting:\n   Total billed:                {billed_tokens} tokens\n   Attributed to completed paths: {attributed_tokens} tokens\n   Failed/unattributed overhead:  {overhead_tokens} tokens ({overhead_percent:.1}%)\n\nCost accounting:\n   Total billed:                ${billed_cost:.2}\n   Attributed to completed paths: ${attributed_cost:.2}\n   Failed/unattributed overhead:  ${overhead_cost:.2}\n\nCost breakdown by model (total billed):\n",
            output.elapsed_secs,
        );
        for (model, usage) in &output.usage.by_model {
            let total_input = usage.input + usage.cache_write_input + usage.cache_read_input;
            statistics.push_str(&format!(
                "   {model}:\n      Input:  {total_input} ({} uncached, {} cache read, {} cache write)\n      Output: {}\n      Cost:   ${:.2}\n",
                usage.input,
                usage.cache_read_input,
                usage.cache_write_input,
                usage.output,
                usage.cost_usd.abs()
            ));
        }
        if has_artifacts {
            if artifact_count == 0 {
                statistics.push_str("\n\nNo artifacts written.");
            } else {
                statistics.push_str(&format!(
                    "\n\n{artifact_count} artifact(s) written to disk."
                ));
            }
        }
        statistics
    }

    fn print_aggregate_summary(completed: usize, usage: &UsageByModel) {
        eprintln!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("Aggregate Summary");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("Completed: {} run(s)", completed);
        eprintln!();

        eprintln!("Cost breakdown by model:");
        for (model, usage) in &usage.by_model {
            let total_input = usage.input + usage.cache_write_input + usage.cache_read_input;
            eprintln!("   {}:", model);
            eprintln!(
                "      Input:  {} ({} uncached, {} cache read, {} cache write)",
                total_input, usage.input, usage.cache_read_input, usage.cache_write_input
            );
            eprintln!("      Output: {}", usage.output);
            eprintln!("      Cost:   ${:.4}", usage.cost_usd);
        }

        eprintln!();
        eprintln!("   Total tokens: {}", usage.total());
        eprintln!("   Total cost:   ${:.2}", usage.total_cost().abs());
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    }
}

fn budget_reached(
    usage: &UsageByModel,
    max_total_tokens: Option<u64>,
    max_cost_usd: Option<f64>,
) -> bool {
    max_total_tokens.is_some_and(|limit| usage.total_u64() >= limit)
        || max_cost_usd.is_some_and(|limit| usage.total_cost() >= limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_stops_pool_replenishment_at_threshold() {
        let mut usage = UsageByModel::default();
        usage.add("model", 50, 0, 0, 50);
        assert!(budget_reached(&usage, Some(100), None));
        assert!(!budget_reached(&usage, Some(101), None));
    }

    #[test]
    fn persisted_statistics_include_cli_session_fields() {
        let mut output = flawrence_chat::TaskOutput {
            response: "done".into(),
            elapsed_secs: 12.5,
            ..Default::default()
        };
        output.usage.add("model", 10, 2, 3, 4);
        output.attributed_usage.add("model", 5, 1, 1, 2);

        let statistics = TaskPool::format_session_statistics(&output, "deadbeef", 7, true);

        assert!(statistics.contains("Session: deadbeef"));
        assert!(statistics.contains("Elapsed time: 12.50s"));
        assert!(statistics.contains("Total billed:                19 tokens"));
        assert!(statistics.contains("Failed/unattributed overhead:  10 tokens (52.6%)"));
        assert!(statistics.contains("7 artifact(s) written to disk."));
    }
}

#[cfg(test)]
mod run_configuration_tests {
    use super::*;
    use flawrence_chat::{ChatOptions, ChatRequest, ChatResponse};
    use flawrence_tool::config::{ClientMode, Config, TokenCostOverride};

    struct StubClient;

    struct CheckpointClient {
        calls: std::sync::atomic::AtomicUsize,
        child_mode: bool,
    }

    #[async_trait::async_trait]
    impl flawrence_api::ChatClient for CheckpointClient {
        async fn chat_completion(
            &self,
            model: &str,
            request: ChatRequest,
            options: Option<&ChatOptions>,
        ) -> anyhow::Result<ChatResponse> {
            use flawrence_chat::{ContentPart, MessageContent, ToolCall, Usage};
            use serde_json::json;
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let checkpoint_call = if self.child_mode { 2 } else { 1 };
            let (content, prompt_tokens) = if call == 0 {
                let (name, args) = if self.child_mode {
                    (
                        "spawn_task",
                        json!({"agent_name":"threat-model", "task_title":"Map parser", "query":"Map parser"}),
                    )
                } else {
                    (
                        "artifact",
                        json!({"operation":"write", "key":"checkpoint_task_root_evidence", "content":"original evidence", "offset":null}),
                    )
                };
                (
                    MessageContent::from_tool_calls(vec![ToolCall {
                        call_id: "work".to_string(),
                        fn_name: name.to_string(),
                        fn_arguments: args,
                    }]),
                    90_000,
                )
            } else if self.child_mode && call == 1 {
                (
                    MessageContent::from_text("Child finished; no findings"),
                    100,
                )
            } else if call == checkpoint_call {
                assert!(request.tools.as_ref().is_none_or(Vec::is_empty));
                assert!(options.unwrap().structured_output.is_some());
                assert!(
                    request
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .first_text()
                        .unwrap()
                        .contains("Runtime checkpoint")
                );
                (
                    MessageContent::from_text(
                        json!({"completed":["previous work"],
                    "facts":[], "rejected":[], "pending":[], "artifacts":[],
                    "next_action":"Return the final report"})
                        .to_string(),
                    ),
                    90_000,
                )
            } else {
                assert_eq!(call, checkpoint_call + 1, "unexpected checkpoint loop");
                assert_eq!(request.messages.len(), 3);
                assert!(request.messages.iter().all(|message| {
                    (&message.content)
                        .into_iter()
                        .all(|part| matches!(part, ContentPart::Text(_)))
                }));
                assert!(
                    request
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .first_text()
                        .unwrap()
                        .contains("previous work")
                );
                assert!(
                    request
                        .tools
                        .as_ref()
                        .is_some_and(|tools| !tools.is_empty())
                );
                (MessageContent::from_text("final report"), 1000)
            };
            Ok(ChatResponse {
                content,
                model: model.to_string(),
                reasoning_content: None,
                reasoning_signature: None,
                truncated: false,
                usage: Usage {
                    prompt_tokens: Some(prompt_tokens),
                    completion_tokens: Some(10),
                    cache_details: None,
                },
            })
        }
    }

    #[tokio::test]
    async fn agent_continues_after_child_batch_and_pressure_checkpoints() {
        for child_mode in [false, true] {
            let mut pool = test_pool("Review parser").await;
            let client = Arc::new(CheckpointClient {
                calls: std::sync::atomic::AtomicUsize::new(0),
                child_mode,
            });
            let runtime = Arc::get_mut(&mut pool.runtime).unwrap();
            runtime.client = client.clone();
            runtime.config.max_total_tokens = None;
            runtime.config.max_context_tokens = if child_mode { None } else { Some(100_000) };
            let mut agent = flawrence_agent::AgentConfig::parse(
                "research",
                r#"---
model: test/model
description: Checkpoint loop test
reasoning_effort: Low
tools: [artifact]
tool_configs:
  artifact:
    read: [checkpoint_*]
    write: [checkpoint_*]
---
Review parser and return a final report.
"#,
            )
            .unwrap();
            if child_mode {
                agent.subagents.push("threat-model".to_string());
            }
            let dir = tempfile::tempdir().unwrap();
            let artifacts = ArtifactStore::new_shared(Some(dir.path().to_path_buf()));
            let output = run_task(
                pool.runtime,
                &agent,
                "Review parser".to_string(),
                Vec::new(),
                None,
                artifacts.clone(),
                EventLog::new(),
                UsageByModel::new_shared(),
                "task_root".to_string(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(output.response, "final report");
            let calls = client.calls.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(calls, if child_mode { 4 } else { 3 });
            let store = artifacts.read().await;
            assert!(store.read("checkpoint_task_root_0_history").is_ok());
            let state: serde_json::Value =
                serde_json::from_str(store.read("checkpoint_task_root_0").unwrap()).unwrap();
            assert_eq!(
                state["children"].as_object().unwrap().len(),
                usize::from(child_mode)
            );
        }
    }

    #[async_trait::async_trait]
    impl flawrence_api::ChatClient for StubClient {
        async fn chat_completion(
            &self,
            _model: &str,
            _request: ChatRequest,
            _options: Option<&ChatOptions>,
        ) -> anyhow::Result<ChatResponse> {
            anyhow::bail!("stub client is never called")
        }
    }

    fn test_config() -> Config {
        Config {
            cf_api_token: None,
            cf_account_id: None,
            max_iterations: 42,
            max_agent_depth: 3,
            max_parallel_agents: 40,
            agent_memory_mb: None,
            max_tokens: 4096,
            max_total_tokens: Some(1_000),
            max_context_tokens: None,
            max_cost_usd: None,
            // Set so `RuntimeContext::new` skips the live pricing fetch.
            token_cost_override: Some(TokenCostOverride {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
            }),
            proxy_url: None,
            client_mode: ClientMode::Streaming,
            anthropic_api_key: Some("sk-ant-do-not-print".to_string()),
            openai_api_key: None,
            opencode_access_token: None,
            model_override: None,
            anthropic_base_url: None,
            openai_base_url: None,
            workers_ai_base_url: None,
            vuln_ingest_endpoint: String::new(),
            vuln_ingest_token_path: None,
            worktree_isolation: false,
            prewarm_build: false,
            prewarm_timeout_secs: 5400,
            max_rate_limit_retries: 10,
            rate_limit_backoff_cap_secs: 120,
            providers: Default::default(),
        }
    }

    async fn test_pool(query: &str) -> TaskPool {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("main.rs"), "fn main() {}\n").unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            agents_dir.path().join("threat-model.md"),
            "---\ndescription: test agent\nmodel: anthropic/claude-sonnet-4-20250514\nreasoning_effort: High\ntools: []\n---\nBody.\n",
        )
        .unwrap();

        let agents = flawrence_agent::AgentRegistry::load_from_dir(agents_dir.path(), &[]).unwrap();
        let agent = agents.get("threat-model").unwrap().clone();
        let skills = flawrence_agent::SkillRegistry::load_from_dir(agents_dir.path()).unwrap();
        let index = flawrence_index::CodebaseIndex::build(directory.path());
        let runtime = RuntimeContext::new(
            Arc::new(StubClient),
            test_config(),
            agents,
            skills,
            directory.path().to_path_buf(),
            None,
            index,
            std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        TaskPool::new(
            runtime,
            agent,
            query.to_string(),
            2,
            Some(3),
            Some(PathBuf::from("/artifacts/run")),
            false,
            None,
            false,
            Vec::new(),
            Some("git repository at deadbeefcafe".to_string()),
        )
    }

    #[tokio::test]
    async fn root_children_queue_but_validators_and_shutdown_do_not_wait_for_slots() {
        use flawrence_tool::{TaskSpawner, ToolContext, todo_list::TodoList};
        use std::time::Duration;
        use tokio::sync::{Semaphore, mpsc};

        let mut pool = test_pool("Review").await;
        let runtime = Arc::get_mut(&mut pool.runtime).unwrap();
        runtime.config.max_parallel_agents = 2;
        runtime.root_agent_slots = Semaphore::new(2);
        let release = Arc::new(Semaphore::new(0));
        let (started, mut receiver) = mpsc::unbounded_channel();
        let task_spawner: TaskSpawner = Arc::new({
            let release = release.clone();
            move |_, _, _, ancestors, _, _, _, _, _, _| {
                let release = release.clone();
                let started = started.clone();
                Box::pin(async move {
                    started.send(ancestors.len()).unwrap();
                    if ancestors.len() == 1 {
                        release.acquire().await.unwrap().forget();
                    }
                    Ok(flawrence_chat::TaskOutput::cancelled("finished"))
                })
            }
        });
        let context = ToolContext::new(
            pool.runtime.clone(),
            pool.agent.clone(),
            vec!["root".into()],
            TodoList::new_shared(&[]),
            ArtifactStore::new_shared(None),
            EventLog::new(),
            UsageByModel::new_shared(),
            "root".into(),
            task_spawner,
        );
        let mut children = JoinSet::new();
        for _ in 0..6 {
            let context = context.clone();
            children.spawn(async move {
                context
                    .spawn_child("threat-model", "row".into(), "review".into())
                    .await
            });
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            assert_eq!(receiver.recv().await, Some(1));
            assert_eq!(receiver.recv().await, Some(1));
            tokio::task::yield_now().await;
            assert!(receiver.try_recv().is_err());
            assert_eq!(pool.runtime.root_agent_slots.available_permits(), 0);

            let mut nested = context.clone();
            nested.ancestors.push("research".into());
            nested
                .spawn_child("threat-model", "validate".into(), "proof".into())
                .await
                .unwrap();
            assert_eq!(receiver.recv().await, Some(2));

            release.add_permits(1);
            assert_eq!(receiver.recv().await, Some(1));
            assert_eq!(
                children
                    .join_next()
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .response,
                "finished"
            );
            pool.runtime.shutdown.escalate();
            pool.runtime.shutdown.escalate();
            while let Some(child) = children.join_next().await {
                assert!(child.unwrap().unwrap().response.contains("cancelled"));
            }
        })
        .await
        .unwrap();
        assert_eq!(pool.runtime.root_agent_slots.available_permits(), 2);
    }

    #[tokio::test]
    async fn run_configuration_records_query_and_settings() {
        let pool = test_pool("Threat model for kxs serialization integration").await;
        // Column padding is cosmetic; compare on collapsed whitespace.
        let rendered: String = pool
            .run_configuration
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = &rendered;

        assert!(rendered.contains("Threat model for kxs serialization integration"));
        assert!(rendered.contains("Agent: threat-model"));
        assert!(rendered.contains("MAX_ITERATIONS=42"));
        assert!(rendered.contains("MAX_AGENT_DEPTH=3"));
        assert!(rendered.contains("MAX_TOKENS=4096"));
        assert!(rendered.contains("MAX_TOTAL_TOKENS=1000"));
        assert!(rendered.contains("MAX_COST_USD=unlimited"));
        assert!(rendered.contains("TRAILMARK_BIN="));
        assert!(rendered.contains("Concurrency: 2"));
        assert!(rendered.contains("Total runs: 3"));
        assert!(rendered.contains("Code index:"));
        assert!(rendered.contains("Rust: 1"));
    }

    #[tokio::test]
    async fn run_configuration_reports_a_missing_git_repository() {
        // The fixture directory is a bare tempdir, never `git init`ed.
        let pool = test_pool("anything").await;
        assert!(
            pool.run_configuration
                .contains("Repository:        (not a git repository, or no commits yet)"),
            "{}",
            pool.run_configuration
        );
        assert!(!pool.run_configuration.contains("Commit:"));
    }

    #[tokio::test]
    async fn run_configuration_never_prints_secret_values() {
        let pool = test_pool("anything").await;
        assert!(!pool.run_configuration.contains("sk-ant-do-not-print"));
        assert!(
            pool.run_configuration
                .contains("ANTHROPIC_API_KEY=set (redacted)")
        );
    }

    #[test]
    fn fenced_survives_backticks_in_the_query() {
        let block = fenced("run ```cargo test``` first");
        assert!(block.starts_with("````text\n"));
        assert!(block.ends_with("\n````"));

        let plain = fenced("no fences here");
        assert!(plain.starts_with("```text\n"));
    }
}
