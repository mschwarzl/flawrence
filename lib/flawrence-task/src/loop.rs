use anyhow::Result;
use flawrence_chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatRole, ContentPart, MessageContent,
    StructuredOutput, TaskOutput, Tool, ToolCall, ToolResponse, Usage, UsageByModel,
};
use futures::future::join_all;
use std::io::Write;
use std::sync::Arc;
use tracing::info;

use flawrence_tool::event_log::{TaskEvent, ToolCallSummary};
use flawrence_tool::runtime::RuntimeContext;
use flawrence_tool::{TaskSpawner, ToolContext, registry};

use super::task::{Task, ToolCallRecord};
use crate::checkpoint::{self, ContextHistory};
use crate::enum_hints::{dictionary_suggestion, is_vetoed, negation_forbids, negation_suggestion};
use flawrence_agent::FieldGrounding;

#[derive(Debug)]
pub(crate) struct BudgetError(String);

impl std::fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BudgetError {}

/// What a bash-capable agent may do to its environment, when the environment is
/// ours to change.
///
/// Only inside the harness image. On a developer machine the workspace is the
/// operator's own installation, and an agent installing packages into it is a
/// side effect nobody asked for and nothing undoes. The image is disposable,
/// so there the same action is cheap and often the difference between
/// establishing a reproducer and reporting that a toolchain was missing.
const CONTAINER_CAPABILITIES_PROMPT: &str = r#"<environment>
This is a disposable container, not the operator's machine. If a toolchain you need is missing --
a compiler, a test runner, a language runtime -- you may install it:
`apt-get update && apt-get install -y --no-install-recommends <package>`.

Install only what a specific, blocked step needs, and say in your output what you installed and
why. A missing tool is a reason to install one package, not a reason to abandon the task or to
report the work as impossible. It is also not a licence to reconfigure the machine: leave the
target's own toolchain, its build files and any pinned versions alone -- installing a second
compiler beside the project's is fine, replacing the project's is not.
</environment>"#;

const MAVEN_SYSTEM_PROMPT: &str = r#"<maven>
This workspace uses Maven. Every Maven invocation MUST use the project `mvnw` wrapper when one
exists, otherwise `mvn`, and include `-B` so the output stays machine-readable. Select a single
module with `-pl <module> -am`: `-pl` alone fails on a sibling the reactor has not built yet.

This policy governs actual Maven invocations; it does NOT require every reproducer to run through
Maven. Maven re-resolves the whole reactor on every invocation, so a reproducer that could be a
direct `javac`/`java` run should be one. When a `test_idiom` artifact is available, prefer its
verified standalone compiler and runtime commands within their recorded scope. Reach for Maven for
its recorded fallback cases: annotation processors, a packaging step the code depends on, resources
resolved from the classpath at runtime, changed production source, and final repository regression
tests.

Export the classpath ONCE rather than per reproducer:
`<mvn> -q -B -pl <module> dependency:build-classpath -Dmdep.outputFile=<file>`
then compile and run directly against `<file>`. That is one Maven invocation instead of one per
attempt, and `test_idiom` records the file it already wrote.

The harness runs Maven in persistent verifier lanes capped across all agents. Your source changes
are snapshotted from your private worktree into one exclusive lane and `target/surefire-reports`
XML is copied back afterward. A queued bash call may wait for a long build; this is expected, does
not consume that command's timeout, and must not trigger cancellation, retries, memory polling,
sleeps, or a fallback command. Put one Maven invocation in each bash call.

A `<build>` section below carries workspace-wide startup facts: the module list, the test-target
hint, and the test source roots. Its reported facts are authoritative, so do NOT rediscover them
with `help:evaluate`, `-Dexec` probes, or a throwaway build. Module-specific profiles in
`test_idiom` outrank the workspace default.

Read results from `target/surefire-reports/TEST-*.xml`, not from console output: an undiscovered
test looks exactly like a passing one, so confirm the case you expect is actually listed there
before treating `BUILD SUCCESS` as evidence.
</maven>"#;

const GRADLE_SYSTEM_PROMPT: &str = r#"<gradle>
This workspace uses Gradle. Every Gradle invocation, including task discovery, MUST use the project
`gradlew` wrapper and include `--max-workers=1`. The complete shape is:
`./gradlew --max-workers=1 <tasks and options>`.
Do not pass `--no-daemon`: persistent verifier lanes keep Gradle and Kotlin daemons, project-local
incremental state, and build outputs warm between agents while bounding concurrent builds.

This policy governs actual Gradle invocations; it does NOT require every Kotlin reproducer to run
through Gradle. When a `test_idiom` artifact is available, prefer its verified standalone compiler
and runtime commands within their recorded scope. Use a Gradle verifier lane for its recorded
fallback cases, such as unsupported compiler-plugin wiring, internal/friend access, platform
runtime, changed production source, and final repository regression tests.

The harness runs Gradle in persistent verifier lanes capped across all agents. Your source changes
are snapshotted from your private worktree into one exclusive lane and XML test results are copied
back afterward. A queued bash call may wait
for a long build or resource-exhaustion test; this is expected, does not consume that command's
timeout, and must not trigger cancellation, retries, memory polling, sleeps, or a fallback command.
Put one Gradle invocation in each bash call. Never run `gradle` directly or call `gradlew --stop`.

A `<build>` section below carries workspace-wide startup facts: a default test-task hint, source-set
hints, and the module list. Its reported facts are authoritative, so do NOT re-probe them with
`tasks --all`, but a mixed build may legitimately use different tasks in different modules.
Module-specific profiles in `test_idiom` outrank the workspace default. Its one init-script
classpath export is expected work, not redundant task discovery.

Only when neither `<build>` nor `test_idiom` establishes the relevant module's test shape, discover
that module once with `:<module>:tasks --all` through the mandatory command above, and read it as:
- `jvmTest`, `compileKotlinJvm`, `compileTestKotlinJvm`, or `allTests` on that module means Kotlin
  Multiplatform with a JVM target. Shared tests belong in `src/commonTest/kotlin`, JVM-only tests in
  `src/jvmTest/kotlin`, and the focused test task is `:<module>:jvmTest`.
- Only `test`, `classes`, or `testClasses`, with none of the KMP markers above, means plain JVM.
  Tests belong in `src/test/kotlin` or `src/test/java`, and the focused task is `:<module>:test`.
- KMP can expose `classes`/`testClasses` through `withJava()` or an unrelated JVM module, so those
  names do not outrank KMP markers.
- For the focused Gradle fallback, put a reproducer beside a comparable existing test, using its
  source set, package, imports, and test framework. Give it a distinctive name and select only it
  with `--tests '*Name*'`.
- Never verify with `allTests` or `assemble` unless the defect is genuinely target-specific; those
  tasks build every declared native target and can consume hours and substantial memory.

Let Gradle resolve dependency classpaths once. Reuse the exact exported classpath from `test_idiom`
with its verified standalone compiler command; never reconstruct the dependency graph or guess jars
independently. Do not edit `build.gradle`, `build.gradle.kts`, `settings.gradle*`, or any
`gradle.properties` to make a reproducer run. If a Gradle fallback needs temporary build logic, put
it in a temp-file init script and pass it with `--init-script`; do not leave an entry point in the
target.

Keep configuration command-local instead of editing any project or user `gradle.properties`:
- Use native CLI options such as `--offline`, `--rerun-tasks`, and `--stacktrace` directly.
- Pass project properties as `-Pname=value`.
- Put temporary build logic in a temp file and pass `--init-script <file>`.
- Do not use `GRADLE_OPTS` or `ORG_GRADLE_PROJECT_*`; hidden environment configuration makes the
  recorded reproduction command incomplete.
- Never put secrets on the command line because the harness records commands.
- Never increase a Gradle or Kotlin heap by command line, environment, init script, or another
  mechanism. A resource-exhaustion reproducer may temporarily lower it on the command line with
  `-Dorg.gradle.jvmargs=-Xmx<N>m`; this replaces the project's JVM argument string, so use it only
  when the lower heap is the point of the run.
- `local.properties` may be edited only when the target deliberately uses it for local SDK or target
  configuration; it is not a substitute for `gradle.properties`.
</gradle>"#;

/// The build-system policy for this workspace, if it has one worth stating.
///
/// One arm per supported system, so adding a build system is a match arm and a
/// prompt constant rather than a change to how the policy is injected.
/// The container-capabilities section, when this is the harness image and the
/// agent can act on it.
fn container_capabilities_prompt(has_bash: bool) -> Option<&'static str> {
    let in_container = std::env::var_os("FLAWRENCE_IN_CONTAINER")
        .is_some_and(|flag| !flag.is_empty() && flag != "0");
    (has_bash && in_container).then_some(CONTAINER_CAPABILITIES_PROMPT)
}

fn build_system_prompt(
    specs: &[flawrence_tool::build_systems::BuildSystemSpec],
    root: &std::path::Path,
    has_bash: bool,
) -> Option<&'static str> {
    if !has_bash {
        return None;
    }
    // Named rather than matched on a type: every system is detected the same
    // way, and a policy prompt is simply something two of them happen to need.
    match flawrence_tool::build_systems::detect(specs, root)?
        .name
        .as_str()
    {
        "Gradle" => Some(GRADLE_SYSTEM_PROMPT),
        "Maven" => Some(MAVEN_SYSTEM_PROMPT),
        // A system declared purely by its skill carries its guidance there, so
        // there is nothing extra to say in the system prompt.
        _ => None,
    }
}

/// The `<build>` section, when the pre-warm established facts and the agent can
/// act on them.
///
/// Placed after the Gradle policy on purpose: these are the answers to the
/// discovery step that policy describes, so an agent reading top to bottom
/// meets the facts before it can decide to go probing for them. Gated on
/// `bash` like the `<workspace>` block, because an agent that cannot run a
/// build only pays the tokens. `None` whenever the pre-warm was disabled,
/// failed, or could not read the build reliably: the agents then discover the
/// build themselves, which is the behaviour that predates this block.
fn build_facts_section(
    facts: Option<&flawrence_tool::prewarm::BuildFacts>,
    specs: &[flawrence_tool::build_systems::BuildSystemSpec],
    root: &std::path::Path,
    has_bash: bool,
) -> Option<String> {
    if !has_bash {
        return None;
    }
    if let Some(facts) = facts {
        return Some(format!("\n\n{}\n", facts.render()));
    }
    // No facts because there is nothing to build is a different thing from no
    // facts because a probe failed, and only the first can be stated with
    // certainty. Saying it stops an agent hunting for a build that is not
    // there, and lifts a requirement that cannot be met.
    if flawrence_tool::build_systems::detect(specs, root).is_none() {
        return Some(format!(
            "\n\n{}\n",
            flawrence_tool::prewarm::no_build_system_block(root)
        ));
    }
    None
}

/// Max grounding-mismatch nudges (G) before the harness forces the grounded
/// truth into the submitted result.
const MAX_GROUNDING_NUDGES: u32 = 2;

/// Navigate a dot-path into a JSON value (`compiles`, `errors`, `errors.0`).
fn navigate_dot_path(value: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        if let Ok(idx) = segment.parse::<usize>() {
            current = current.get(idx)?;
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current.clone())
}

/// Extract one grounded field from the task's tool-call history per a
/// [`FieldGrounding`] binding. Filters by tool, applies the optional `filter`
/// (keep calls whose `result.<filter.path>` equals `filter.value` — the path
/// is relative to the result object, e.g. `compiles` means `result.compiles`),
/// takes the
/// `when: "last"` matching call, and navigates `source.<path>`.
fn extract_grounded(
    history: &[ToolCallRecord],
    binding: &FieldGrounding,
) -> Option<serde_json::Value> {
    let mut matches: Vec<&ToolCallRecord> =
        history.iter().filter(|r| r.tool == binding.tool).collect();
    if let Some(ref filter) = binding.filter {
        matches.retain(|r| {
            navigate_dot_path(&r.result, &filter.path)
                .map(|v| v == filter.value.clone())
                .unwrap_or(false)
        });
    }
    let record = match binding.when.as_str() {
        "last" => matches.last().copied()?,
        "first" => matches.first().copied()?,
        _ => return None,
    };
    let source = match binding.source.as_str() {
        "args" => &record.args,
        "result" => &record.result,
        _ => return None,
    };
    navigate_dot_path(source, &binding.path)
}

fn make_task_spawner() -> TaskSpawner {
    Arc::new(
        |runtime,
         agent_name,
         query,
         ancestors,
         title,
         artifact_store,
         event_log,
         run_usage,
         task_id,
         parent_task_id| {
            Box::pin(async move {
                let agent = runtime.agents.get(&agent_name)?.clone();
                crate::run_task(
                    runtime,
                    &agent,
                    query,
                    ancestors,
                    title,
                    artifact_store,
                    event_log,
                    run_usage,
                    task_id,
                    parent_task_id,
                )
                .await
            })
        },
    )
}

pub struct TaskLoop {
    runtime: Arc<RuntimeContext>,
    task: Task,
    /// Number of consecutive times the todo exit-guard has blocked completion.
    /// After [`TODO_BLOCK_LIMIT`] attempts, the guard force-completes any
    /// remaining todos and releases the agent, so a subagent that cannot clear
    /// its todos (e.g. a tooling quirk) still returns a result to its parent
    /// instead of looping until `MAX_ITERATIONS` and hard-failing.
    todo_block_count: u32,
}

/// Max consecutive todo exit-guard blocks before the guard releases the agent.
const TODO_BLOCK_LIMIT: u32 = 3;

impl TaskLoop {
    pub fn new(runtime: Arc<RuntimeContext>, task: Task) -> Self {
        // Announce the task before it can run a tool or spawn a child: worktree
        // isolation keys a checkout to the root of the write-capable subtree, so
        // a child must be able to find its parent even if the parent has not
        // asked for a workspace yet.
        runtime.worktrees.register_task(
            &task.task_id,
            task.parent_task_id.as_deref(),
            &task.config,
            task.title.as_deref(),
        );
        Self {
            runtime,
            task,
            todo_block_count: 0,
        }
    }

    pub async fn run(&mut self) -> Result<TaskOutput> {
        let memory = self.runtime.agent_memory.clone();
        let root_memory_scope = self.task.parent_task_id.is_none();
        let hierarchy = self.task.hierarchy().join(" > ");
        let ctx = crate::logging::TaskContext::new(
            hierarchy,
            self.task.usage.clone(),
            self.runtime.global_usage.clone(),
        );
        let execution = flawrence_api::streaming::SESSION_ID.scope(
            self.task.session_id.clone(),
            crate::logging::TASK_CONTEXT.scope(ctx, self.run_with_context()),
        );
        if let Some(memory) = memory {
            memory.scope(root_memory_scope, execution).await
        } else {
            execution.await
        }
    }

    fn build_system_prompt(&self) -> String {
        let mut prompt = self.task.config.prompt.clone();

        let skills_section = self.runtime.skills.format_for_prompt();
        if !skills_section.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&skills_section);
        }

        if !self.task.config.subagents.is_empty() {
            prompt.push_str("\n\n<available_agents>\n");
            for name in &self.task.config.subagents {
                if let Ok(cfg) = self.runtime.agents.get(name) {
                    prompt.push_str(&format!(
                        "  <agent name=\"{}\">{}</agent>\n",
                        name, &cfg.description
                    ));
                }
            }
            prompt.push_str("</available_agents>\n");
        }

        // Agents reach for absolute paths they never saw — `/workspace` when the
        // root is `/workspace/awesn1` — and pay a probe or a rejected call to
        // find out. Naming the variable costs one line and is correct whether or
        // not worktree isolation moved the root out from under them, which
        // hardcoding the path here would not be.
        let has_bash = self.task.config.tools.iter().any(|tool| tool == "bash");
        if has_bash {
            prompt.push_str(
                "\n\n<workspace>\nEvery tool resolves relative paths from the workspace root, and \
                 paths outside it are rejected. `$WORKSPACE` holds its absolute path in bash \
                 commands: prefer `cd \"$WORKSPACE/sub\"` over an absolute path you guessed or \
                 probed for, and pass `$WORKSPACE` rather than a literal path when a command needs \
                 the root.\n</workspace>\n",
            );
        }
        if let Some(environment) = container_capabilities_prompt(has_bash) {
            prompt.push_str("\n\n");
            prompt.push_str(environment);
            prompt.push('\n');
        }
        if let Some(gradle) = build_system_prompt(
            &self.runtime.build_systems,
            &self.runtime.base_directory,
            has_bash,
        ) {
            prompt.push_str("\n\n");
            prompt.push_str(gradle);
            prompt.push('\n');
        }
        if let Some(section) = build_facts_section(
            self.runtime.build_facts.get(),
            &self.runtime.build_systems,
            &self.runtime.base_directory,
            has_bash,
        ) {
            prompt.push_str(&section);
        }
        if let Some(limit) = self.runtime.config.agent_memory_mb {
            prompt.push_str(&format!(
                "\n\n<agent_memory>\nYour direct subagent tree shares a {limit} MiB sampled RSS \
                 watchdog threshold across all its command subprocesses and deeper agents. A deeper \
                 child does not get an extra allowance. Root-agent commands have a separate budget. \
                 The watchdog samples about every 100 ms and can overshoot; it is not a hard RAM \
                 ceiling and does not measure the harness's shared heap or web UI. A \
                 memory_limit_exceeded tool error means the watchdog observed the tree above its \
                 threshold and killed tracked subprocesses. Agent chat loops remain alive: reduce \
                 inputs, allocations, or batches before retrying. This does not diagnose a kernel \
                 OOM or prove a target vulnerability. Do not bypass the watchdog or retry the same \
                 oversized command unchanged. Keep reproducers self-contained. For Gradle, this \
                 overrides the daemon policy above: always use --no-daemon \
                 -Pkotlin.compiler.execution.strategy=in-process. Cap each test JVM heap explicitly \
                 and leave native-memory headroom; -Xmx alone is not a process RAM cap.\n\
                 </agent_memory>\n"
            ));
        }

        // Static todo awareness (cached in system prompt)
        if !self.task.config.todos.is_empty() {
            prompt.push_str(&format!(
                "\n\n<todos>\nYou have {} pending todo(s). You MUST complete all todos before finishing. Use the `todo` tool to view and manage them. You cannot submit results or end the task while todos remain pending.\n</todos>\n",
                self.task.config.todos.len()
            ));
        }

        // Static artifact permission awareness (cached in system prompt)
        let artifact_perms = self.task.config.artifact_permissions();
        if artifact_perms.has_any() {
            prompt.push_str("\n\n<artifacts>\n");
            if !artifact_perms.read.is_empty() {
                prompt.push_str(&format!(
                    "You can read artifacts matching: {}\n",
                    artifact_perms
                        .read
                        .iter()
                        .map(|e| e.key())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !artifact_perms.write.is_empty() {
                prompt.push_str(&format!(
                    "You can write artifacts matching: {}\n",
                    artifact_perms.write.join(", ")
                ));
            }
            prompt.push_str("Use the `artifact` tool to read/write artifacts shared across the task tree.\n</artifacts>\n");
        }

        prompt.push_str("\n\n<use_parallel_tool_calls>\nFor maximum efficiency, whenever you perform multiple independent operations, invoke all relevant tools simultaneously rather than sequentially. Prioritize calling tools in parallel whenever possible. For example, when spawning multiple subagents, emit all spawn_task calls in a single response. When adding multiple todos, emit all todo calls in a single response. Err on the side of maximizing parallel tool calls rather than running too many tools sequentially.\n</use_parallel_tool_calls>\n");

        if self.task.ancestors.is_empty() {
            prompt.push_str(&format!(
                "\n\nRoot subagent concurrency: FLAWRENCE_MAX_PARALLEL_AGENTS={}. \
                 This runtime limit overrides any fixed batch size in your instructions. \
                 Extra direct children wait for a slot; every assigned row still needs its own agent.\n",
                self.runtime.config.max_parallel_agents
            ));
        }

        prompt
    }

    fn build_tools(&self) -> Vec<Tool> {
        let mut allowed = self.task.config.tools.clone();

        // Auto-add spawn_task if subagents are defined
        if !self.task.config.subagents.is_empty() && !allowed.contains(&"spawn_task".to_string()) {
            allowed.push("spawn_task".to_string());
        }

        if self.task.config.output_schema.is_some() && !allowed.is_empty() {
            allowed.push("submit_result".to_string());
        }

        let mut tools = registry().tools_filtered(&allowed);

        // Constrain the arguments whose valid values are only known at
        // runtime. Both are names the model would otherwise have to recall
        // from the system prompt, and inventing one costs a wasted turn.
        restrict_to_known_values(
            &mut tools,
            "load_skill",
            "skill_name",
            &self.runtime.skills.names(),
        );
        restrict_to_known_values(
            &mut tools,
            "spawn_task",
            "agent_name",
            &self.task.config.subagents,
        );

        if let Some(ref schema) = self.task.config.output_schema
            && let Some(submit_tool) = tools.iter_mut().find(|t| t.name == "submit_result")
        {
            let mut s = schema.clone();
            if let Some(obj) = s.as_object_mut() {
                obj.entry("type").or_insert(serde_json::json!("object"));
                obj.insert("additionalProperties".to_string(), serde_json::json!(false));
            }
            submit_tool.schema = Some(s);
            submit_tool.strict = Some(true);
        }

        tools
    }

    fn build_chat_options(&self) -> ChatOptions {
        let mut opts = ChatOptions::default()
            .with_temperature(1.0)
            .with_max_tokens(self.runtime.config.max_tokens)
            .with_reasoning_effort(self.task.config.reasoning_effort.clone());

        // Only set wire-level response_format when the agent has NO tools.
        // When tools are present, structured output is enforced via the
        // submit_result tool whose schema is patched in build_tools().
        // Sending both response_format and tools causes models to emit raw
        // JSON immediately instead of making tool calls first.
        if self.task.config.tools.is_empty()
            && let Some(ref schema) = self.task.config.output_schema
        {
            opts = opts.with_structured_output(StructuredOutput::JsonSchema {
                name: format!("{}_response", self.task.config.name),
                schema: schema.clone(),
            });
        }

        opts
    }

    async fn track_usage(&mut self, response: &ChatResponse, model: &str) {
        self.track_usage_values(&response.usage, model).await;
    }

    async fn track_usage_values(&mut self, usage: &Usage, model: &str) {
        let prompt_tokens = usage
            .prompt_tokens
            .and_then(|tokens| u32::try_from(tokens).ok())
            .unwrap_or(0);
        let completion_tokens = usage
            .completion_tokens
            .and_then(|tokens| u32::try_from(tokens).ok())
            .unwrap_or(0);

        let (cache_read, cache_write) = usage
            .cache_details
            .as_ref()
            .map(|d| {
                (
                    d.read_tokens
                        .and_then(|tokens| u32::try_from(tokens).ok())
                        .unwrap_or(0),
                    d.write_tokens
                        .and_then(|tokens| u32::try_from(tokens).ok())
                        .unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));

        self.task.update_context_size(prompt_tokens);
        let uncached_input = prompt_tokens.saturating_sub(cache_read + cache_write);
        // Update per-task usage (also visible to log formatter via shared Arc)
        self.task.usage.lock().unwrap().add(
            model,
            uncached_input,
            cache_write,
            cache_read,
            completion_tokens,
        );
        // Update global usage (real-time cross-session total)
        self.runtime.global_usage.lock().unwrap().add(
            model,
            uncached_input,
            cache_write,
            cache_read,
            completion_tokens,
        );
        self.task.run_usage.lock().unwrap().add(
            model,
            uncached_input,
            cache_write,
            cache_read,
            completion_tokens,
        );
        crate::logging::TaskContext::update_context_size(self.task.context_size);

        // Skip noise events when the provider omitted usage entirely.
        // Some upstreams return `usage: null` or all-zero counts on
        // non-terminal stream chunks; emitting those just clutters the
        // transcript without telling the renderer anything new.
        if prompt_tokens > 0 || completion_tokens > 0 {
            // Cumulative cost of this task (not global), so consumers can
            // attribute spend to a specific agent loop.
            let cumulative_cost_usd = self.task.usage.lock().unwrap().total_cost();
            self.task
                .event_log
                .emit(
                    &self.task.task_id,
                    TaskEvent::TokenUsage {
                        model: model.to_string(),
                        prompt_tokens,
                        completion_tokens,
                        cumulative_cost_usd,
                    },
                )
                .await;
        }
    }

    /// A stream that stalls mid-flight leaves the bill unknown, so a run with a
    /// budget refuses to retry it: the retry might double-charge an amount we
    /// cannot see. Taken literally that rule throws away completed work — one
    /// dropped SSE stream cost a run its only validated finding for a code path,
    /// reproducer and all, while $5 of a $13 budget went unspent.
    ///
    /// So allow a few such retries while the budget still has clear headroom,
    /// and hold the original line once it does not. The worst case is a small
    /// unmetered overshoot; the alternative is losing the analysis outright.
    fn billing_unknown_retry_allowed(&self, already_retried: u32) -> bool {
        const MAX_BILLING_UNKNOWN_RETRIES: u32 = 2;
        /// Retry only while this much of each budget is still unspent.
        const REQUIRED_HEADROOM: f64 = 0.15;

        if already_retried >= MAX_BILLING_UNKNOWN_RETRIES {
            return false;
        }
        let (total_tokens, total_cost) = {
            let usage = self.runtime.global_usage.lock().unwrap();
            (usage.total_u64(), usage.total_cost())
        };
        budget_has_headroom(
            total_cost,
            self.runtime.config.max_cost_usd,
            total_tokens,
            self.runtime.config.max_total_tokens,
            REQUIRED_HEADROOM,
        )
    }

    /// Enforce the optional cumulative budget thresholds.
    ///
    /// Checked against *global* usage so the ceiling bounds the entire runner
    /// invocation — every agent and subagent — rather than a single loop.
    /// Returning an error here unwinds the whole task tree. The check runs
    /// immediately before each paid call, after any schedule wait.
    /// A completed request, plus concurrently in-flight requests, can cross a
    /// threshold because providers do not expose an exact preflight cost.
    fn check_budget(&self, model: &str) -> Result<()> {
        let (total_tokens, total_cost) = {
            let usage = self.runtime.global_usage.lock().unwrap();
            (usage.total_u64(), usage.total_cost())
        };
        if let Some(max_tokens) = self.runtime.config.max_total_tokens
            && total_tokens >= max_tokens
        {
            return Err(BudgetError(format!(
                "Token budget exhausted: {total_tokens} tokens used \
                 (>= MAX_TOTAL_TOKENS = {max_tokens}); halting run"
            ))
            .into());
        }
        if let Some(max_cost) = self.runtime.config.max_cost_usd {
            if flawrence_chat::model_pricing(model).is_none() {
                return Err(BudgetError(format!(
                    "No pricing is available for model '{model}', so the MAX_COST_USD cap \
                     cannot be enforced. Set TOKEN_COST_INPUT and TOKEN_COST_OUTPUT to price \
                     this model, or unset MAX_COST_USD to run without a cost cap."
                ))
                .into());
            }
            if total_cost >= max_cost {
                return Err(BudgetError(format!(
                    "Cost budget exhausted: ${total_cost:.2} spent \
                     (>= MAX_COST_USD = ${max_cost:.2}); halting run"
                ))
                .into());
            }
        }
        Ok(())
    }

    fn log_thinking(response: &ChatResponse) {
        if let Some(thinking) = response.reasoning_content.as_deref() {
            let first_line = thinking.lines().next().unwrap_or("").trim();
            if !first_line.is_empty() {
                let truncated: String = first_line.chars().take(300).collect();
                if truncated.len() < first_line.len() {
                    info!("{}...", truncated);
                } else {
                    info!("{}", truncated);
                }
            }
        }
    }

    fn log_invalid_response(messages: &[ChatMessage], response: &ChatResponse) -> Result<()> {
        let log_entry = serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "messages": messages,
            "response": {
                "content": response.content,
                "reasoning_content": response.reasoning_content,
                "model": response.model,
            }
        });

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("errors.log")?;

        writeln!(file, "{}", serde_json::to_string(&log_entry)?)?;
        Ok(())
    }

    fn extract_tool_calls(response: &ChatResponse) -> Vec<ToolCall> {
        (&response.content)
            .into_iter()
            .filter_map(|part| {
                if let ContentPart::ToolCall(tc) = part {
                    Some(tc.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    async fn execute_tool_calls(&mut self, tool_calls: &[ToolCall]) {
        if tool_calls.len() > 25 {
            tracing::warn!(
                count = tool_calls.len(),
                "Too many tool calls in single request, refusing all"
            );
            let parts: Vec<_> = tool_calls.iter().map(|tc| {
                ContentPart::ToolResponse(ToolResponse {
                    call_id: tc.call_id.clone(),
                    content: format!("Error: Too many tool calls ({}) in a single request. Maximum is 25. Please reduce the number of concurrent calls.", tool_calls.len()),
                })
            }).collect();
            self.task
                .messages
                .push(ChatMessage::tool(MessageContent::from_parts(parts)));
            return;
        }

        let ancestors = self.task.hierarchy();
        let agent_config = self.task.config.clone();
        let mut allowed_tools = self.task.config.tools.clone();
        // Auto-add spawn_task if subagents are defined
        if !self.task.config.subagents.is_empty()
            && !allowed_tools.contains(&"spawn_task".to_string())
        {
            allowed_tools.push("spawn_task".to_string());
        }
        if self.task.config.output_schema.is_some() && !allowed_tools.is_empty() {
            allowed_tools.push("submit_result".to_string());
        }

        let event_log = self.task.event_log.clone();
        let parent_task_id = self.task.task_id.clone();

        let futures: Vec<_> = tool_calls.iter().map(|tc| {
            let name = tc.fn_name.clone();
            let args = tc.fn_arguments.clone();
            let call_id = tc.call_id.clone();
            let ctx = ToolContext::new(
                self.runtime.clone(),
                agent_config.clone(),
                ancestors.clone(),
                self.task.todo_list.clone(),
                self.task.artifact_store.clone(),
                self.task.event_log.clone(),
                self.task.run_usage.clone(),
                self.task.task_id.clone(),
                make_task_spawner(),
            );
            let allowed = allowed_tools.clone();
            let event_log = event_log.clone();
            let parent_task_id = parent_task_id.clone();

            async move {
                // Always emit Started first so renderers can pair every
                // AssistantMessage tool-call entry with a Started/Completed.
                // The auth-deny path below would otherwise produce an
                // orphan Completed.
                event_log
                    .emit(
                        &parent_task_id,
                        TaskEvent::ToolCallStarted {
                            call_id: call_id.clone(),
                            tool: name.clone(),
                            args: args.clone(),
                        },
                    )
                    .await;

                // Fail-closed: empty tools list = no tools allowed.
                // A hallucinated tool name and a real-but-forbidden one need
                // different messages: telling a model it "is not allowed" to
                // call something that does not exist invites it to retry.
                if !allowed.contains(&name) {
                    let msg = rejected_tool_message(
                        ctx.agent_name(),
                        &name,
                        &allowed,
                        registry().contains(&name),
                    );
                    tracing::warn!(%msg);
                    event_log
                        .emit(
                            &parent_task_id,
                            TaskEvent::ToolCallCompleted {
                                call_id: call_id.clone(),
                                tool: name.clone(),
                                ok: false,
                                output: serde_json::Value::String(msg.clone()),
                                duration_ms: 0,
                            },
                        )
                        .await;
                    return (call_id, msg, None::<UsageByModel>);
                }

                let started = std::time::Instant::now();
                let execution = with_heartbeat(
                    &name,
                    &call_id,
                    &args,
                    started,
                    registry().execute(&name, args.clone(), &ctx),
                );
                let (call_id, result, usage, ok) = match execution.await {
                    Ok((result, usage)) => (call_id, result, usage, true),
                    Err(e) => {
                        tracing::warn!(tool = %name, error = %e, "Tool execution failed");
                        (call_id, e.to_string(), None, false)
                    }
                };
                let duration_ms = started.elapsed().as_millis() as u64;
                info!(tool = %name, args = %args, output_chars = result.len(), output_lines = result.lines().count(), "Tool call");

                // Tool result is opaque text; try to parse as JSON for
                // pretty serialisation, fall back to a string value.
                // Skip from_str entirely for plain-text outputs.
                let trimmed = result.trim_start();
                let output_value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    serde_json::from_str::<serde_json::Value>(&result)
                        .unwrap_or_else(|_| serde_json::Value::String(result.clone()))
                } else {
                    serde_json::Value::String(result.clone())
                };
                event_log
                    .emit(
                        &parent_task_id,
                        TaskEvent::ToolCallCompleted {
                            call_id: call_id.clone(),
                            tool: name.clone(),
                            ok,
                            output: output_value,
                            duration_ms,
                        },
                    )
                    .await;

                (call_id, result, usage)
            }
        }).collect();

        let results = join_all(futures).await;

        // G: record the (tool, args, result) transcript so submit_result can
        // verify grounded fields against real tool calls. Only accumulated when
        // this agent has output_grounding configured (memory hygiene for
        // long-running orchestrators that never ground).
        let collect_history = self.task.config.output_grounding.is_some();
        let mut parts = Vec::new();
        for (tc, (call_id, content, usage)) in tool_calls.iter().zip(results.into_iter()) {
            let content = cap_tool_response(content, &call_id);
            let content = if content.is_empty() {
                "(empty)".to_string()
            } else {
                content
            };
            if collect_history {
                let parsed = serde_json::from_str(&content)
                    .unwrap_or_else(|_| serde_json::Value::String(content.clone()));
                self.task.tool_call_history.push(ToolCallRecord {
                    tool: tc.fn_name.clone(),
                    args: tc.fn_arguments.clone(),
                    result: parsed,
                });
            }
            parts.push(ContentPart::ToolResponse(ToolResponse { call_id, content }));
            if let Some(usage) = usage {
                // Merge subtask usage into per-task only (global was already
                // updated in real-time when the subtask made its API calls)
                self.task.usage.lock().unwrap().merge(&usage);
            }
        }

        self.task
            .messages
            .push(ChatMessage::tool(MessageContent::from_parts(parts)));
    }

    /// Check if all todos are complete. If not, push a nudge message and return false.
    ///
    /// After [`TODO_BLOCK_LIMIT`] consecutive blocks, force-completes the
    /// remaining todos (recording a warning in the transcript) and returns
    /// true, so a stuck agent still terminates and returns a result.
    async fn check_todos_complete(&mut self) -> bool {
        let mut todo_list = self.task.todo_list.lock().await;
        if todo_list.all_complete() {
            self.todo_block_count = 0;
            return true;
        }
        let pending = todo_list.format_pending();

        // Bounded escape: if the agent keeps hitting the guard without making
        // progress, release it rather than looping until MAX_ITERATIONS. This
        // protects subagents (e.g. patch-verifier) from hard-failing and
        // dropping their result when a todo becomes un-completable.
        self.todo_block_count = self.todo_block_count.saturating_add(1);
        if self.todo_block_count > TODO_BLOCK_LIMIT {
            let count = todo_list.complete_all();
            drop(todo_list); // release lock before pushing message
            let warn = format!(
                "Release: {count} pending todo(s) could not be completed after \
                 {TODO_BLOCK_LIMIT} attempts and were force-completed to avoid an \
                 infinite loop. Proceeding with task completion."
            );
            tracing::warn!("Exit guard force-releasing stuck todos: {warn}");
            self.task.messages.push(ChatMessage::user(warn));
            self.todo_block_count = 0;
            return true;
        }

        drop(todo_list); // release lock before pushing message

        info!("Exit blocked: pending todos remain");
        self.task.messages.push(ChatMessage::user(format!(
            "You cannot complete this task yet. The following todos are still pending:\n{}\n\nComplete all todos before finishing.",
            pending
        )));
        false
    }

    async fn run_with_context(&mut self) -> Result<TaskOutput> {
        let outcome = self.run_with_context_inner().await;
        // Emit terminal event. `TaskCompleted` is the canonical successful
        // ending; `TaskFailed` covers every error bubble (max-iters, API
        // exhaustion, etc.). The transcript readers rely on exactly one
        // of these closing every task's stream.
        match &outcome {
            Ok(out) => {
                let parsed_output = serde_json::from_str::<serde_json::Value>(&out.response)
                    .unwrap_or_else(|_| serde_json::Value::String(out.response.clone()));
                self.task
                    .event_log
                    .emit(
                        &self.task.task_id,
                        TaskEvent::TaskCompleted {
                            output: parsed_output,
                            elapsed_secs: out.elapsed_secs,
                            usage: out.usage.to_json(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                self.task
                    .event_log
                    .emit(
                        &self.task.task_id,
                        TaskEvent::TaskFailed {
                            error: format!("{e:#}"),
                        },
                    )
                    .await;
            }
        }
        // Save source patches and remove build caches now, rather than retaining
        // gigabytes per finished subtree until the whole review ends. Validation
        // descendants share the checkout, so only the finished subtree root releases it.
        let worktrees = self.runtime.worktrees.clone();
        let task_id = self.task.task_id.clone();
        let artifacts_dir = self.task.artifact_store.read().await.output_dir().cloned();
        match tokio::task::spawn_blocking(move || {
            worktrees.release_task(&task_id, artifacts_dir.as_deref())
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!("Keeping agent worktree: {error:#}"),
            Err(error) => tracing::warn!("Worktree pruning task failed: {error}"),
        }
        outcome
    }

    async fn run_with_context_inner(&mut self) -> Result<TaskOutput> {
        let start_time = std::time::Instant::now();

        // Boot-strap event for this loop. Carries linkage info (parent_task_id)
        // and config the consumer needs to render the run's header.
        self.task
            .event_log
            .emit(
                &self.task.task_id,
                TaskEvent::TaskStarted {
                    parent_task_id: self.task.parent_task_id.clone(),
                    agent: self.task.config.name.clone(),
                    query: self.task.query.clone(),
                    title: self.task.title.clone().unwrap_or_default(),
                    iter_cap: self.runtime.config.max_iterations as u32,
                },
            )
            .await;

        let system_prompt = self.build_system_prompt();
        self.task.messages.push(ChatMessage::system(system_prompt));
        self.task
            .messages
            .push(ChatMessage::user(self.task.query.clone()));

        let tools = self.build_tools();
        let model = self.task.config.model.clone();
        let chat_options = self.build_chat_options();
        let mut context_history = ContextHistory::default();
        let checkpoints_enabled = checkpoint::permitted(&self.task);
        let mut iteration = 0;
        // Set from the provider's own finish reason, so a cut-off response is
        // known rather than inferred from a repeated failure. Assigned on every
        // iteration before the submit_result check reads it.
        #[allow(unused_assignments)]
        let mut response_truncated = false;
        let is_root = self.task.parent_task_id.is_none();
        // The root is nudged once toward consolidation; subagents never reach
        // this loop under shutdown because their futures are dropped by the
        // parent's spawner as soon as the level reaches CONSOLIDATE.
        let mut shutdown_nudged = false;
        let global_max = self.runtime.config.max_iterations;
        let mut memory_notice_generation = 0;
        loop {
            if let Some(message) =
                flawrence_tool::memory::notice_since(&mut memory_notice_generation)
            {
                self.task
                    .event_log
                    .emit(
                        &self.task.task_id,
                        TaskEvent::Warning {
                            message: message.clone(),
                        },
                    )
                    .await;
                self.task.messages.push(ChatMessage::user(format!(
                    "Resource notification: {message}"
                )));
            }
            iteration += 1;
            // B/F: a per-agent `max_iterations` (if set) overrides the global
            // cap. On cap-exceed we do NOT bail — we nudge the model once to
            // submit a best-effort result, give it a short grace window, and
            // synthesize a bounded result if it still doesn't submit.
            let max_iters = self.task.config.max_iterations.unwrap_or(global_max);
            if iteration > max_iters {
                if !self.task.depth_limit_hit {
                    self.task.depth_limit_hit = true;
                    self.task.messages.push(ChatMessage::user(
                        "You have reached the iteration limit for this task. \
                         Call `submit_result` now with your best-effort verdict. \
                         Set any unfinished fields to `null`/`unknown` and include \
                         `depth_limit_reached: true`. Do not start new tool calls.",
                    ));
                }
                let grace_iters = iteration.saturating_sub(max_iters + 1);
                if grace_iters >= 2 {
                    let mut synth = serde_json::json!({
                        "verdict": "unknown",
                        "confidence": "low",
                        "evidence": [],
                        "reasoning": "Subagent hit its iteration depth limit and did not submit within the grace window. Grounded fields below (e.g. passed/verified_command) are populated by the harness from the real tool calls made before the limit.",
                        "depth_limit_reached": true
                    });
                    // Preserve real work: populate grounded fields from the
                    // tool-call transcript so a budget-exhausted-but-verified
                    // subagent still hands its parent passed:true +
                    // verified_command instead of a bare verdict:unknown.
                    if let Some(ref grounding) = self.task.config.output_grounding
                        && let Some(obj) = synth.as_object_mut()
                    {
                        for (field, binding) in &grounding.0 {
                            let g = extract_grounded(&self.task.tool_call_history, binding)
                                .unwrap_or(serde_json::Value::Null);
                            obj.insert(field.clone(), g);
                        }
                    }
                    let synth = synth.to_string();
                    let _ = self
                        .task
                        .event_log
                        .emit(
                            &self.task.task_id,
                            TaskEvent::Warning {
                                message:
                                    "iteration depth limit reached; synthesizing best-effort result"
                                        .to_string(),
                            },
                        )
                        .await;
                    return Ok(TaskOutput {
                        response: synth,
                        usage: self.task.usage.lock().unwrap().clone(),
                        attributed_usage: self.task.attributed_usage(),
                        elapsed_secs: start_time.elapsed().as_secs_f64(),
                    });
                }
                // else: fall through and run this iteration (the nudge above is
                // now in message history) so the model gets a chance to submit.
            }

            // Staged shutdown. ABORT stops even the root; CONSOLIDATE lets the
            // root keep going but tells it, once, to stop fanning out and to
            // submit and summarise what the finished agents already produced.
            let shutdown = self.runtime.shutdown.level();
            if shutdown >= flawrence_tool::shutdown::ABORT {
                anyhow::bail!("Run aborted by the operator");
            }
            if is_root && shutdown >= flawrence_tool::shutdown::CONSOLIDATE && !shutdown_nudged {
                shutdown_nudged = true;
                self.task.messages.push(ChatMessage::user(
                    "The operator has stopped the run. In-flight subagents were cancelled and no \
                     new ones can be started. Do not spawn any further subtasks. Using only the \
                     findings gathered so far, deduplicate them, submit the genuine \
                     vulnerabilities, and write your final summary now.",
                ));
            }
            self.task
                .event_log
                .emit(
                    &self.task.task_id,
                    TaskEvent::IterationStarted {
                        iteration: iteration as u32,
                    },
                )
                .await;

            let request_bytes = ContextHistory::request_size(&self.task.messages, &tools)?;
            let pressure = context_history.under_pressure(
                request_bytes,
                self.runtime.config.max_tokens,
                self.runtime.config.max_context_tokens,
            );
            if !context_history.pending
                && !self.task.depth_limit_hit
                && (pressure || (checkpoints_enabled && context_history.after_children))
            {
                anyhow::ensure!(
                    checkpoints_enabled,
                    "Context pressure for '{}'; grant artifact read/write checkpoint_* to enable checkpoints (MAX_CONTEXT_TOKENS={:?})",
                    self.task.config.name,
                    self.runtime.config.max_context_tokens
                );
                context_history.pending = true;
                self.task
                    .messages
                    .push(ChatMessage::user(checkpoint::CHECKPOINT_REQUEST));
            }
            let request_options = if context_history.pending {
                checkpoint::options(&chat_options)
            } else {
                chat_options.clone()
            };
            let request_tools = if context_history.pending {
                Vec::new()
            } else {
                tools.clone()
            };
            let mut messages = self.task.messages.clone();

            // Chat completion with retry and exponential backoff. The cap is
            // configurable via FLAWRENCE_MAX_API_RETRIES. Default is conservative:
            // if the upstream hasn't recovered in 50 attempts, it likely won't,
            // so fail rather than loop forever. Raise it to ride out a long
            // outage; lower it to fail faster.
            let max_attempts: u64 = std::env::var("FLAWRENCE_MAX_API_RETRIES")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(50);
            let retry_started = std::time::Instant::now();
            let mut attempt = 0;
            let mut billing_unknown_retries = 0u32;
            let mut empty_response_count = 0;
            let mut unbilled_empty_count = 0u32;
            let (response, request_bytes) = loop {
                // Enforce schedule: pause if outside the allowed UTC time window
                flawrence_tool::scheduling::enforce_schedule(&self.runtime.schedule).await;
                self.check_budget(&model)?;

                attempt += 1;

                let request_bytes = ContextHistory::request_size(&messages, &request_tools)?;
                let req = ChatRequest::new(messages.clone()).with_tools(request_tools.clone());

                match self
                    .runtime
                    .client
                    .chat_completion(&model, req, Some(&request_options))
                    .await
                {
                    Ok(res) => {
                        let has_meaningful_text = res
                            .content
                            .first_text()
                            .map(|t| !t.trim().is_empty())
                            .unwrap_or(false);
                        let has_tools = (&res.content)
                            .into_iter()
                            .any(|p| matches!(p, ContentPart::ToolCall(_)));

                        // Always store non-empty parts (filtering whitespace-only text).
                        let filtered_parts: Vec<ContentPart> = (&res.content)
                            .into_iter()
                            .filter(|p| match p {
                                ContentPart::Text(t) => !t.trim().is_empty(),
                                _ => true,
                            })
                            .cloned()
                            .collect();
                        if !filtered_parts.is_empty() {
                            // Build the event from the filtered parts before
                            // wrapping into a ChatMessage so we keep the
                            // tool-call summaries cheap to read out.
                            let event_text: String = filtered_parts
                                .iter()
                                .filter_map(|p| match p {
                                    ContentPart::Text(t) => Some(t.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let event_tool_calls: Vec<ToolCallSummary> = filtered_parts
                                .iter()
                                .filter_map(|p| match p {
                                    ContentPart::ToolCall(tc) => Some(ToolCallSummary::from(tc)),
                                    _ => None,
                                })
                                .collect();
                            self.task
                                .event_log
                                .emit(
                                    &self.task.task_id,
                                    TaskEvent::AssistantMessage {
                                        content: event_text,
                                        reasoning: res.reasoning_content.clone(),
                                        tool_calls: event_tool_calls,
                                    },
                                )
                                .await;

                            let mut assistant_msg =
                                ChatMessage::assistant(MessageContent::from_parts(filtered_parts));
                            assistant_msg.reasoning_content = res.reasoning_content.clone();
                            assistant_msg.reasoning_signature = res.reasoning_signature.clone();
                            self.task.messages.push(assistant_msg.clone());
                            messages.push(assistant_msg);
                        }

                        if has_meaningful_text || has_tools {
                            // Carried to the submit_result check below: a
                            // tool call assembled from a cut-off response is
                            // missing whatever came after the cut, and the
                            // difference between that and a model omitting
                            // fields decides which advice can be acted on.
                            response_truncated = res.truncated;
                            break (res, request_bytes);
                        }

                        // Response has no meaningful text and no tool calls.
                        empty_response_count += 1;
                        // A response that billed nothing was never generated:
                        // the provider handed back an empty body. Nudging is
                        // pointless — the model never saw the previous turn —
                        // and ten such attempts in a row cost a validation
                        // agent its verdict while reporting "empty response",
                        // which reads like the model misbehaving.
                        // A generated turn always bills a prompt: the model
                        // had to read the conversation. A zero prompt count is
                        // therefore proof the request never reached the model,
                        // regardless of whether the provider attached a token
                        // or two of whitespace to the empty body.
                        let billed_nothing = res.usage.prompt_tokens.unwrap_or(0) == 0;
                        if billed_nothing {
                            unbilled_empty_count += 1;
                        }
                        self.track_usage(&res, &model).await;

                        // `billed_nothing` is the whole diagnosis: a billed
                        // empty means the model read the conversation and
                        // still produced nothing -- a prompt or tool-schema
                        // problem. An unbilled one means the request never
                        // arrived, which no amount of prompt work will fix.
                        // Without it on this line both read as "empty
                        // response" and the two get chased interchangeably.
                        tracing::warn!(
                            attempt,
                            empty_count = empty_response_count,
                            billed_nothing,
                            unbilled_count = unbilled_empty_count,
                            prompt_tokens = res.usage.prompt_tokens.unwrap_or(0),
                            has_text = res.content.first_text().is_some(),
                            has_thinking = res.reasoning_content.is_some(),
                            "Empty response (whitespace-only text), retrying"
                        );

                        const MAX_UNBILLED_EMPTY: u32 = 3;
                        if unbilled_empty_count >= MAX_UNBILLED_EMPTY {
                            anyhow::bail!(
                                "The provider returned {unbilled_empty_count} empty responses in a \
                                 row, none of which consumed any tokens. That is an upstream \
                                 failure, not model output: retrying cannot help because the \
                                 request is not reaching the model. The task's work up to this \
                                 point is lost; re-run it."
                            );
                        }
                        if empty_response_count >= 10 {
                            anyhow::bail!(
                                "Model produced 10 consecutive empty responses without valid output"
                            );
                        }

                        // Log invalid response with full message history
                        if let Err(e) = Self::log_invalid_response(&messages, &res) {
                            tracing::error!(error = ?e, "Failed to log invalid response");
                        }

                        // Replace the previous nudge instead of stacking another
                        // copy: ten identical reminders crowd the context that
                        // the model needs in order to answer at all.
                        let nudge = ChatMessage::user(empty_response_nudge(
                            &messages,
                            self.task.config.output_schema.as_ref(),
                        ));
                        if empty_response_count > 1
                            && messages.last().is_some_and(is_empty_response_nudge)
                        {
                            messages.pop();
                        }
                        messages.push(nudge);

                        // A provider that just returned nothing is often busy or
                        // rate-limited; hammering it immediately turned one stuck
                        // agent into ten wasted calls in as many seconds.
                        tokio::time::sleep(std::time::Duration::from_millis(
                            250 * empty_response_count.min(8) as u64,
                        ))
                        .await;
                        continue;
                    }
                    Err(e) => {
                        // One retry decision, from whichever classified error
                        // this is. A provider error already carries the verdict,
                        // decided from its status and type at the client
                        // boundary; anything else falls back to a message scan.
                        use flawrence_api::retry::ClassifiedError;
                        let classified: Option<&dyn ClassifiedError> = e
                            .downcast_ref::<flawrence_api::streaming::OpenAiResponseError>()
                            .map(|err| err as &dyn ClassifiedError)
                            .or_else(|| {
                                e.downcast_ref::<flawrence_api::streaming::StreamingRequestError>()
                                    .map(|err| err as &dyn ClassifiedError)
                            });
                        if let Some(classified) = classified {
                            // Read everything off the borrow up front (cloning
                            // the usage) so nothing tied to `e` is held across
                            // the await below; the spawned run future must stay
                            // Send.
                            let budget_is_set = self.runtime.config.max_total_tokens.is_some()
                                || self.runtime.config.max_cost_usd.is_some();
                            let usage = classified.usage().cloned();
                            let is_retryable = classified.is_retryable();
                            let billing_unknown = classified.billing_unknown();
                            let retryable_with_budget =
                                classified.retryable_with_budget(budget_is_set);

                            if let Some(usage) = &usage {
                                self.track_usage_values(usage, &model).await;
                            }
                            if !retryable_with_budget {
                                if !(billing_unknown
                                    && is_retryable
                                    && self.billing_unknown_retry_allowed(billing_unknown_retries))
                                {
                                    return Err(e);
                                }
                                billing_unknown_retries += 1;
                                tracing::warn!(
                                    billing_unknown_retries,
                                    "Retrying a request whose billing is unknown; the budget still \
                                     has headroom and the alternative is discarding this agent's work"
                                );
                            }
                        } else if !flawrence_api::retry::retryable_error_message(
                            &format!("{e:#}"),
                            true,
                        ) {
                            return Err(e);
                        }
                        // Counting attempts is the wrong control when a single
                        // failure can cost minutes: a provider that idles for
                        // its full 300s timeout turns 100 attempts into ~8.8
                        // hours on one task. Bound the wall clock instead and
                        // keep the attempt cap only as a guard against a fast
                        // error looping tightly.
                        let spent = retry_started.elapsed();
                        if spent >= RETRY_WALL_CLOCK_LIMIT {
                            anyhow::bail!(
                                "API call failed after {attempt} attempts over {}s (limit {}s): {e}",
                                spent.as_secs(),
                                RETRY_WALL_CLOCK_LIMIT.as_secs()
                            );
                        }
                        if attempt as u64 >= max_attempts {
                            anyhow::bail!("API call failed after {attempt} attempts: {e}");
                        }
                        // Pick backoff by error class. Rate limits need a long,
                        // conservative wait to avoid hammering the API. Transient
                        // upstream errors (inference timeouts, dropped SSE streams)
                        // recover in seconds, so retry quickly to avoid wasting
                        // minutes of the iteration budget on 30s+ sleeps.
                        let msg = format!("{e:#}").to_lowercase();
                        let is_rate_limit = msg.contains("rate limit")
                            || msg.contains("429")
                            || msg.contains("too many requests");
                        let backoff_secs = if is_rate_limit {
                            // 30s, 60s, 120s (cap).
                            std::cmp::min(30 * 2_u64.pow((attempt - 1).min(2)), 120)
                        } else {
                            // 2s, 4s, 8s, 16s (cap).
                            std::cmp::min(2 * 2_u64.pow((attempt - 1).min(3)), 16)
                        };
                        // Cheap, dependency-free jitter (0-999ms) so parallel tasks
                        // do not retry in lockstep after a shared upstream blip.
                        let jitter_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_millis() as u64)
                            .unwrap_or(0)
                            % 1000;
                        // Logged only once the delay is known: a bare "retrying"
                        // gives no way to tell a backoff from a hot loop, which
                        // is exactly the question an operator asks when a run
                        // stalls on a provider that hangs for minutes.
                        tracing::warn!(
                            attempt,
                            backoff_secs,
                            error = ?e,
                            "API call failed, backing off before retry"
                        );
                        // Surface the failure live in the web UI transcript so
                        // stalls and timeouts are visible there, not only on
                        // stderr.
                        let _ = self
                            .task
                            .event_log
                            .emit(
                                &self.task.task_id,
                                TaskEvent::Warning {
                                    message: format!(
                                        "API call failed (attempt {attempt}), retrying in {backoff_secs}s: {e}"
                                    ),
                                },
                            )
                            .await;
                        tokio::time::sleep(std::time::Duration::from_millis(
                            backoff_secs * 1000 + jitter_ms,
                        ))
                        .await;
                    }
                }
            };

            self.track_usage(&response, &model).await;
            context_history.record(request_bytes, response.usage.prompt_tokens);
            crate::logging::TaskContext::update_iteration(iteration as u32);
            Self::log_thinking(&response);

            let tool_calls = Self::extract_tool_calls(&response);

            if context_history.pending {
                let text = response.content.first_text().unwrap_or_default();
                let valid = checkpoint::validate(text).and_then(|()| {
                    anyhow::ensure!(
                        !response.truncated && tool_calls.is_empty(),
                        "checkpoint must be complete JSON without tool calls"
                    );
                    Ok(())
                });
                if let Err(error) = valid {
                    context_history.failures = context_history.failures.saturating_add(1);
                    anyhow::ensure!(
                        context_history.failures < 3,
                        "Unable to checkpoint after 3 responses; original history preserved: {error}"
                    );
                    // No tools executed; discard only the unusable checkpoint reply.
                    if self
                        .task
                        .messages
                        .last()
                        .is_some_and(|message| message.role == ChatRole::Assistant)
                    {
                        self.task.messages.pop();
                    }
                    self.task.messages.push(ChatMessage::user(format!(
                        "Checkpoint rejected: {error}. Return complete JSON only; shorten the text and use evidence references.")));
                } else {
                    let key = checkpoint::save_and_reset(
                        &mut self.task,
                        text,
                        context_history.generation,
                    )
                    .await?;
                    context_history.reset();
                    crate::logging::TaskContext::update_context_size(0);
                    shutdown_nudged = false;
                    tracing::info!(artifact = %key, "Saved context checkpoint and reset working messages");
                    self.task
                        .event_log
                        .emit(
                            &self.task.task_id,
                            TaskEvent::Warning {
                                message: format!("Context reset from saved checkpoint '{key}'"),
                            },
                        )
                        .await;
                }
                // A checkpoint is runtime bookkeeping, not another investigation iteration.
                iteration = iteration.saturating_sub(1);
                continue;
            }

            if tool_calls.is_empty() {
                // Block plain-text termination when output_schema requires submit_result.
                // Only applies when the agent has tools — tool-less agents use
                // response_format for structured output and never call submit_result.
                if self.task.config.output_schema.is_some() && !self.task.config.tools.is_empty() {
                    self.task.messages.push(ChatMessage::user(
                        "You must call `submit_result` with the structured output. Do not respond with plain text."
                    ));
                    continue;
                }

                // Exit guard: check todos before allowing completion
                if !self.check_todos_complete().await {
                    continue;
                }
                let content = response
                    .content
                    .first_text()
                    .expect("retry loop guarantees content");
                return Ok(TaskOutput {
                    response: content.to_string(),
                    usage: self.task.output_usage(),
                    attributed_usage: self.task.attributed_usage(),
                    elapsed_secs: start_time.elapsed().as_secs_f64(),
                });
            }

            // Check for submit_result — ends the task immediately
            if let Some(submit) = tool_calls.iter().find(|tc| tc.fn_name == "submit_result") {
                // Wall-clock starts ticking when the agent invokes
                // submit_result; every Completed emit below uses this to
                // populate `duration_ms`. submit_result is treated as a
                // regular tool call in the event stream (Started + one
                // Completed per attempt) so renderers can use the same
                // ToolCall affordance for it.
                let submit_started = std::time::Instant::now();

                // submit_result must be the only tool call in the batch.
                // Every entry in tool_calls is rejected here: emit a full
                // Started/Completed pair for each so the conversation
                // renderer can pair AssistantMessage tool-calls 1:1 with
                // lifecycle events.
                if tool_calls.len() > 1 {
                    tracing::warn!(
                        "submit_result batched with {} other tool calls — rejecting",
                        tool_calls.len() - 1
                    );
                    for tc in tool_calls.iter() {
                        self.task
                            .event_log
                            .emit(
                                &self.task.task_id,
                                TaskEvent::ToolCallStarted {
                                    call_id: tc.call_id.clone(),
                                    tool: tc.fn_name.clone(),
                                    args: tc.fn_arguments.clone(),
                                },
                            )
                            .await;
                    }
                    let mut parts: Vec<ContentPart> = Vec::with_capacity(tool_calls.len());
                    let elapsed_ms = submit_started.elapsed().as_millis() as u64;
                    for tc in tool_calls.iter() {
                        let content = if tc.fn_name == "submit_result" {
                            "submit_result must be called alone, not batched with other tools. Call your other tools first, then call submit_result separately.".to_string()
                        } else {
                            "Skipped — submit_result was in this batch. Call this tool separately, then call submit_result alone.".to_string()
                        };
                        self.task
                            .event_log
                            .emit(
                                &self.task.task_id,
                                TaskEvent::ToolCallCompleted {
                                    call_id: tc.call_id.clone(),
                                    tool: tc.fn_name.clone(),
                                    ok: false,
                                    output: serde_json::Value::String(content.clone()),
                                    duration_ms: elapsed_ms,
                                },
                            )
                            .await;
                        parts.push(ContentPart::ToolResponse(ToolResponse {
                            call_id: tc.call_id.clone(),
                            content,
                        }));
                    }
                    self.task
                        .messages
                        .push(ChatMessage::tool(MessageContent::from_parts(parts)));
                    continue;
                }

                // Single-submit case: emit Started once. Each rejection
                // path below emits a matching Completed before its
                // `continue`; the acceptance path emits Completed before
                // the final Ok return.
                self.task
                    .event_log
                    .emit(
                        &self.task.task_id,
                        TaskEvent::ToolCallStarted {
                            call_id: submit.call_id.clone(),
                            tool: "submit_result".to_string(),
                            args: submit.fn_arguments.clone(),
                        },
                    )
                    .await;

                let mut result = submit.fn_arguments.clone();

                // Check if submit_result should be rejected
                let rejection_message = if let Some(ref schema) = self.task.config.output_schema {
                    if let Ok(validator) = jsonschema::validator_for(schema) {
                        let errors: Vec<String> = validator
                            .iter_errors(&result)
                            .map(|e| format!("- {}: {}{}", e.instance_path, e, enum_suggestion(&e)))
                            .collect();
                        if !errors.is_empty() {
                            tracing::warn!(
                                error_count = errors.len(),
                                "submit_result failed schema validation"
                            );
                            // A conditional schema ("these 14 fields once the
                            // verdict is confirmed") produces a wall of
                            // per-field errors. Leading with the missing names
                            // turns it into a checklist the retry can work
                            // through, instead of prose to re-read.
                            let missing = missing_properties(&errors);
                            let headline = if missing.is_empty() {
                                String::new()
                            } else {
                                format!("\nMissing required fields: {}.", missing.join(", "))
                            };
                            Some(format!(
                                "Schema validation failed. Fix these errors and call submit_result again:{headline}\n{}",
                                errors.join("\n")
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let rejection_message = rejection_message.map(|msg| {
                    if response_truncated {
                        truncated_submit_advice(&msg, &result)
                    } else {
                        msg
                    }
                });

                if let Some(msg) = rejection_message {
                    self.task
                        .event_log
                        .emit(
                            &self.task.task_id,
                            TaskEvent::ToolCallCompleted {
                                call_id: submit.call_id.clone(),
                                tool: "submit_result".to_string(),
                                ok: false,
                                output: serde_json::Value::String(msg.clone()),
                                duration_ms: submit_started.elapsed().as_millis() as u64,
                            },
                        )
                        .await;
                    self.task
                        .messages
                        .push(ChatMessage::tool(MessageContent::from_parts(vec![
                            ContentPart::ToolResponse(ToolResponse {
                                call_id: submit.call_id.clone(),
                                content: msg,
                            }),
                        ])));
                    continue;
                }

                // Exit guard: check todos before allowing submit
                if !self.check_todos_complete().await {
                    let msg = "Cannot submit yet — complete all pending todos first.".to_string();
                    self.task
                        .event_log
                        .emit(
                            &self.task.task_id,
                            TaskEvent::ToolCallCompleted {
                                call_id: submit.call_id.clone(),
                                tool: "submit_result".to_string(),
                                ok: false,
                                output: serde_json::Value::String(msg.clone()),
                                duration_ms: submit_started.elapsed().as_millis() as u64,
                            },
                        )
                        .await;
                    self.task
                        .messages
                        .push(ChatMessage::tool(MessageContent::from_parts(vec![
                            ContentPart::ToolResponse(ToolResponse {
                                call_id: submit.call_id.clone(),
                                content: msg,
                            }),
                        ])));
                    continue;
                }

                // G: verify grounded fields against actual tool calls (Option 2 —
                // verify + nudge). The harness checks the model's submitted
                // `passed`/`verified_command`/etc. against the real
                // tool calls in the transcript. On mismatch it rejects
                // the submit with a teaching nudge (bounded); after the cap it
                // forces the grounded truth. Honest omissions are silently
                // filled. Cheat-proof: the model cannot report a value its tool
                // calls did not produce.
                if let Some(ref grounding) = self.task.config.output_grounding {
                    let (mismatches, inserts) = {
                        let history = &self.task.tool_call_history;
                        let mut mm: Vec<(
                            String,
                            Option<serde_json::Value>,
                            Option<serde_json::Value>,
                        )> = Vec::new();
                        let mut ins: Vec<(String, serde_json::Value)> = Vec::new();
                        for (field, binding) in &grounding.0 {
                            let grounded = extract_grounded(history, binding);
                            let submitted = result.get(field).cloned();
                            match (grounded, submitted) {
                                (Some(g), Some(s)) if g != s => {
                                    mm.push((field.clone(), Some(s), Some(g)))
                                }
                                (Some(g), None) => ins.push((field.clone(), g)),
                                (None, Some(s)) => mm.push((field.clone(), Some(s), None)),
                                (Some(_), Some(_)) | (None, None) => {}
                            }
                        }
                        (mm, ins)
                    };
                    if !mismatches.is_empty() && self.task.nudge_count < MAX_GROUNDING_NUDGES {
                        self.task.nudge_count += 1;
                        let lines: Vec<String> = mismatches
                            .iter()
                            .map(|(field, submitted, grounded)| {
                                let tool = grounding
                                    .0
                                    .get(field)
                                    .map(|b| b.tool.as_str())
                                    .unwrap_or("the tool");
                                let s = submitted
                                    .as_ref()
                                    .map(|v| {
                                        serde_json::to_string(v)
                                            .unwrap_or_else(|_| "???".into())
                                    })
                                    .unwrap_or_else(|| "omitted".to_string());
                                let g = grounded
                                    .as_ref()
                                    .map(|v| {
                                        serde_json::to_string(v)
                                            .unwrap_or_else(|_| "???".into())
                                    })
                                    .unwrap_or_else(|| "no matching tool call".to_string());
                                format!(
                                    "  `{field}`: you submitted {s}, but your real `{tool}` call(s) produced {g}"
                                )
                            })
                            .collect();
                        let msg = format!(
                            "submit_result rejected: the following fields are verified by the harness against your actual tool calls and do not match:\n{}\nFix the underlying issue (call the tool, or fix your input and re-run it) and call submit_result again with values that match your real tool output. Correction nudge {}/{}.",
                            lines.join("\n"),
                            self.task.nudge_count,
                            MAX_GROUNDING_NUDGES
                        );
                        self.task
                            .event_log
                            .emit(
                                &self.task.task_id,
                                TaskEvent::ToolCallCompleted {
                                    call_id: submit.call_id.clone(),
                                    tool: "submit_result".to_string(),
                                    ok: false,
                                    output: serde_json::Value::String(msg.clone()),
                                    duration_ms: submit_started.elapsed().as_millis() as u64,
                                },
                            )
                            .await;
                        self.task
                            .messages
                            .push(ChatMessage::tool(MessageContent::from_parts(vec![
                                ContentPart::ToolResponse(ToolResponse {
                                    call_id: submit.call_id.clone(),
                                    content: msg,
                                }),
                            ])));
                        continue;
                    }
                    // Either no mismatches, or nudges exhausted → enforce truth.
                    if let Some(obj) = result.as_object_mut() {
                        // Honest omissions: silently fill from real calls.
                        for (field, g) in inserts {
                            obj.insert(field, g);
                        }
                        // Nudges exhausted: force every grounded field to the
                        // real value (no more lies possible).
                        if !mismatches.is_empty() {
                            for (field, binding) in &grounding.0 {
                                let g = extract_grounded(&self.task.tool_call_history, binding)
                                    .unwrap_or(serde_json::Value::Null);
                                obj.insert(field.clone(), g);
                            }
                            let _ = self.task.event_log.emit(
                                &self.task.task_id,
                                TaskEvent::Warning {
                                    message: format!(
                                        "grounding forced after {} nudges; grounded fields overwritten with real tool-call values",
                                        self.task.nudge_count
                                    ),
                                },
                            ).await;
                        }
                    }
                }

                // B/F: tag the result if this subagent hit its depth limit.
                if self.task.depth_limit_hit
                    && let Some(obj) = result.as_object_mut()
                {
                    obj.insert(
                        "depth_limit_reached".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }

                let result_str =
                    serde_json::to_string(&result).unwrap_or_else(|_| result.to_string());
                info!(tool = "submit_result", args = %result_str, output_chars = result_str.len(), "Tool call");
                // Acceptance: close out the submit_result tool call with
                // ok=true before TaskCompleted is emitted by the outer
                // wrapper. `output` carries the accepted payload as JSON
                // so a renderer doesn't have to reach back into
                // AssistantMessage.tool_calls to display the result.
                self.task
                    .event_log
                    .emit(
                        &self.task.task_id,
                        TaskEvent::ToolCallCompleted {
                            call_id: submit.call_id.clone(),
                            tool: "submit_result".to_string(),
                            ok: true,
                            output: result.clone(),
                            duration_ms: submit_started.elapsed().as_millis() as u64,
                        },
                    )
                    .await;
                return Ok(TaskOutput {
                    response: result_str,
                    usage: self.task.output_usage(),
                    attributed_usage: self.task.attributed_usage(),
                    elapsed_secs: start_time.elapsed().as_secs_f64(),
                });
            }

            self.execute_tool_calls(&tool_calls).await;
            context_history.after_children = tool_calls
                .iter()
                .any(|call| matches!(call.fn_name.as_str(), "spawn_task" | "report_finding"));
        }
    }
}

/// Backstop for the shared response budget. Tools that can predict their own
/// size truncate themselves with something actionable (`read_file` names the
/// offset to resume from); this catches the ones that cannot — a broad
/// `grep_files`, say — so a single call can never flood the context. Warning
/// about an oversized response while forwarding it in full, as this used to,
/// left the log honest and the context blown.
fn cap_tool_response(content: String, call_id: &str) -> String {
    let budget = flawrence_tool::MAX_TOOL_RESPONSE_CHARS;
    if content.chars().count() <= budget {
        return content;
    }
    let original = content.chars().count();
    tracing::warn!(
        chars = original,
        budget,
        call_id = %call_id,
        "Truncating oversized tool response"
    );
    let mut truncated: String = content.chars().take(budget).collect();
    truncated.push_str(&format!(
        "\n\n[Truncated: {original} characters exceeded the {budget}-character response budget. \
         Narrow the request — a path, a line range, or a tighter pattern — and call again.]"
    ));
    truncated
}

/// Pull the property names out of "\"x\" is a required property" messages.
/// A "did you mean" for an enum violation. Every enum slip observed in practice
/// is a near-miss synonym — `moderate` for `medium`, `reproducer` for
/// `local_reproducer`, `very high` for `high` — which the model cannot fix by
/// re-reading its own output; it has to be told the exact token. Suggest only
/// when the match is unambiguous, because a confidently wrong hint
/// (`source` -> `sink`) costs more than no hint at all.
fn enum_suggestion(error: &jsonschema::ValidationError<'_>) -> String {
    let jsonschema::error::ValidationErrorKind::Enum { options } = &error.kind else {
        return String::new();
    };
    let (Some(submitted), Some(options)) = (error.instance.as_str(), options.as_array()) else {
        return String::new();
    };
    let permitted: Vec<&str> = options.iter().filter_map(|o| o.as_str()).collect();
    match nearest_enum_value(submitted, &permitted) {
        Some(hit) => format!(" -- did you mean \"{hit}\"?"),
        None => String::new(),
    }
}

/// The one permitted value that unambiguously matches `submitted`, or None.
/// The curated dictionary is consulted first, because the words it knows are
/// exactly the ones lexical scoring gets wrong or backwards (`moderate` is 6
/// edits from `medium`; `unconfirmed` *contains* `confirmed`). Only when the
/// dictionary is silent does containment apply (`reproducer` inside
/// `local_reproducer`), then a typo-sized edit distance. Ties are rejected
/// rather than guessed, and any candidate the antonym table vetoes is dropped
/// however well it scores.
/// Test-only alias so the cross-enum fuzz in `enum_hints` can exercise the
/// real matcher rather than a replica of it.
#[cfg(test)]
pub(crate) fn nearest_enum_value_for_test<'a>(
    submitted: &str,
    permitted: &[&'a str],
) -> Option<&'a str> {
    nearest_enum_value(submitted, permitted)
}

fn nearest_enum_value<'a>(submitted: &str, permitted: &[&'a str]) -> Option<&'a str> {
    if let Some(meant) = dictionary_suggestion(submitted, permitted) {
        // Hand the caller its own slice entry so the hint quotes the schema's
        // exact spelling.
        return permitted
            .iter()
            .copied()
            .find(|p| p.eq_ignore_ascii_case(meant));
    }

    // A negation is resolved before any lexical stage, because every stage
    // below would otherwise return the value being negated.
    if let Some(meant) = negation_suggestion(submitted, permitted) {
        return Some(meant);
    }

    let needle = submitted.trim().to_ascii_lowercase();

    // Containment is only sound when no permitted value nests inside another.
    // When one does, substring evidence cannot say which value the needle came
    // from -- `bypass_found` sits inside `no_bypass_found`, so `not_bypass_found`
    // matches the affirmative verdict and `ypass_found` matches the negative
    // one, each the opposite of what was meant. Edit distance measures the
    // actual corruption and gets all four directions right, so defer to it.
    let nests = permitted.iter().any(|value| {
        let v = value.to_ascii_lowercase();
        permitted.iter().any(|other| {
            !other.eq_ignore_ascii_case(value) && other.to_ascii_lowercase().contains(&v)
        })
    });
    if !nests {
        let contained: Vec<&str> = permitted
            .iter()
            .copied()
            .filter(|candidate| !is_vetoed(&needle, candidate))
            .filter(|candidate| !negation_forbids(&needle, candidate, permitted))
            .filter(|candidate| {
                let c = candidate.to_ascii_lowercase();
                c.contains(&needle) || needle.contains(&c)
            })
            .collect();
        if let [only] = contained[..] {
            return Some(only);
        }
        if !contained.is_empty() {
            return None; // ambiguous containment
        }
    }

    let mut scored: Vec<(usize, &str)> = permitted
        .iter()
        .copied()
        .filter(|candidate| !is_vetoed(&needle, candidate))
        .filter(|candidate| !negation_forbids(&needle, candidate, permitted))
        .map(|candidate| {
            (
                edit_distance(&needle, &candidate.to_ascii_lowercase()),
                candidate,
            )
        })
        .filter(|(distance, _)| *distance <= 2)
        .collect();
    scored.sort_by_key(|(distance, _)| *distance);
    match scored[..] {
        [(_, only)] => Some(only),
        [(best, only), (runner_up, _), ..] if best < runner_up => Some(only),
        _ => None,
    }
}

/// Levenshtein distance, row-at-a-time. Enum tokens are short, so the naive
/// implementation is well inside any budget that matters here.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ac) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ac != *bc);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn missing_properties(errors: &[String]) -> Vec<String> {
    let mut names: Vec<String> = errors
        .iter()
        .filter(|error| error.contains("is a required property"))
        .filter_map(|error| {
            let start = error.find('"')?;
            let rest = &error[start + 1..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether both budgets still have the given fraction unspent. An unset budget
/// never blocks.
fn budget_has_headroom(
    spent: f64,
    cost_limit: Option<f64>,
    tokens: u64,
    token_limit: Option<u64>,
    required: f64,
) -> bool {
    cost_limit.is_none_or(|limit| spent < limit * (1.0 - required))
        && token_limit.is_none_or(|limit| (tokens as f64) < (limit as f64) * (1.0 - required))
}

/// What to say when the provider truncated the response that carried a
/// `submit_result`.
///
/// The plain checklist assumes the fields are missing because the model did not
/// write them, and tells it to add them. When the response was cut off at the
/// output limit that advice cannot work: the fields it lists are the ones the
/// model never reached, and re-sending the same content reproduces the same
/// cut, forever.
///
/// Called only when the provider said so -- `finish_reason: length` on the
/// OpenAI-compatible paths, `stop_reason: max_tokens` on Anthropic -- so this
/// is a fact about the response, not a guess from a repeated failure.
fn truncated_submit_advice(original: &str, submitted: &serde_json::Value) -> String {
    let largest = submitted.as_object().and_then(|fields| {
        fields
            .iter()
            .filter_map(|(name, value)| value.as_str().map(|text| (name, text.chars().count())))
            .max_by_key(|(_, size)| *size)
    });
    let culprit = match largest {
        Some((name, size)) => format!(
            " `{name}` is the largest field you sent, at {size} characters; cut it down first \
             -- keep the decisive evidence and drop the rest."
        ),
        None => String::new(),
    };
    format!(
        "{original}\n\nThe provider cut this response off at its output limit. Those fields are \
         not missing because you forgot them -- generation stopped before reaching them -- so \
         re-sending the same content will fail identically. Do not retry unchanged.{culprit} Then \
         call submit_result again with every required field, shorter."
    )
}

/// What to say to a model that returned nothing at all.
///
/// The generic "check the format of your tool calls" is wrong advice when the
/// previous turn ended in a rejected `submit_result`: the model made no tool
/// call to fix. Repeating the actual blocker — the validation errors and the
/// fields the schema requires — gives the retry something to act on.
/// Whether a message is one of our own empty-response nudges, so a repeat can
/// replace it rather than pile up behind it.
fn is_empty_response_nudge(message: &ChatMessage) -> bool {
    (&message.content).into_iter().any(|part| match part {
        ContentPart::Text(text) => text.starts_with(EMPTY_NUDGE_PREFIX),
        _ => false,
    })
}

/// Opening words shared by every empty-response nudge, including the variant
/// that also repeats a submit_result rejection.
/// How long one turn may spend retrying the provider before the task gives up.
///
/// A stalled stream costs its full idle timeout (300s) per attempt, so an
/// attempt count alone cannot bound the damage. Thirty minutes is generous
/// enough to ride out a provider incident and short enough that a wedged task
/// does not silently consume a whole run's wall clock.
/// How long a tool call runs before it starts announcing itself, and how often
/// it repeats after that.
const TOOL_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Emit a periodic "still running" line while `work` is in flight.
///
/// Every other line this loop logs appears only once a tool has *returned*, so
/// from outside the process a Gradle build queued behind the serialization
/// permit, a multi-minute call-graph walk, and a provider that accepted the
/// connection and then sent nothing are indistinguishable: all three are
/// silence. Three separate stalls were investigated as hangs before anyone
/// noticed the work was merely slow, and the reading hardest to reach from an
/// idle terminal is the correct one -- that nothing is wrong. The heartbeat
/// exists to make "running" and "wedged" tell themselves apart.
/// Tools that spawn children which log their own tool calls. Their progress is
/// already visible line by line, so a heartbeat on top of it restates what the
/// children are saying: at a twenty-way fan-out that is forty lines a minute
/// burying the silent leaf calls the heartbeat exists to surface.
const SELF_REPORTING_TOOLS: &[&str] = &["spawn_task"];

/// Argument keys that identify *which* call this is, most specific first.
/// Concurrent calls to one tool are otherwise indistinguishable: twenty agents
/// queued on the Gradle permit all log `tool=bash` and nothing else.
const SUBJECT_KEYS: &[&str] = &[
    "command",
    "query",
    "finding_key",
    "symbol",
    "path",
    "file_path",
    "key",
];

const SUBJECT_MAX_CHARS: usize = 60;

/// A short single-line excerpt of the arguments, naming what this call is on.
///
/// Flattened deliberately: a `bash` command carries heredocs and newlines, and
/// a heartbeat that spans twenty lines is worse than no heartbeat at all.
fn heartbeat_subject(args: &serde_json::Value) -> Option<String> {
    let raw = SUBJECT_KEYS
        .iter()
        .find_map(|key| args.get(key).and_then(serde_json::Value::as_str))
        .filter(|text| !text.trim().is_empty())?;

    let flattened = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() > SUBJECT_MAX_CHARS {
        return Some(
            flattened
                .chars()
                .take(SUBJECT_MAX_CHARS)
                .chain("...".chars())
                .collect(),
        );
    }
    Some(flattened)
}

/// Whether a tool's progress is invisible enough to be worth announcing.
fn beats_while_running(tool: &str) -> bool {
    !SELF_REPORTING_TOOLS.contains(&tool)
}

async fn with_heartbeat<F: std::future::Future>(
    tool: &str,
    call_id: &str,
    args: &serde_json::Value,
    started: std::time::Instant,
    work: F,
) -> F::Output {
    if !beats_while_running(tool) {
        return work.await;
    }
    let subject = heartbeat_subject(args).unwrap_or_default();
    beat_while(TOOL_HEARTBEAT_INTERVAL, work, |beat| {
        info!(
            tool = %tool,
            call_id = %call_id,
            subject = %subject,
            elapsed_secs = started.elapsed().as_secs(),
            beat,
            "Tool call still running"
        );
    })
    .await
}

/// Drive `work` to completion, invoking `on_beat` every `interval` until it
/// finishes. A tool that returns before the first interval never beats at all,
/// which keeps the common fast call as quiet as it is today.
async fn beat_while<F: std::future::Future>(
    interval: std::time::Duration,
    work: F,
    mut on_beat: impl FnMut(u64),
) -> F::Output {
    tokio::pin!(work);
    let mut ticker = tokio::time::interval(interval);
    // `interval` yields its first tick immediately; that one is not a heartbeat.
    ticker.tick().await;
    let mut beats = 0;
    loop {
        tokio::select! {
            result = &mut work => return result,
            _ = ticker.tick() => {
                beats += 1;
                on_beat(beats);
            }
        }
    }
}

const RETRY_WALL_CLOCK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

const EMPTY_NUDGE_PREFIX: &str = "Your last response was empty";

fn empty_response_nudge(
    messages: &[ChatMessage],
    output_schema: Option<&serde_json::Value>,
) -> String {
    const GENERIC: &str = "Your last response was empty. Continue the task: call a tool, or if you are done, produce your final answer as text.";

    let last_tool_text = messages.last().and_then(|message| {
        (&message.content).into_iter().find_map(|part| match part {
            ContentPart::ToolResponse(response) => Some(response.content.as_str()),
            _ => None,
        })
    });

    let Some(rejection) =
        last_tool_text.filter(|text| text.starts_with("Schema validation failed"))
    else {
        return GENERIC.to_string();
    };

    let required = output_schema
        .and_then(|schema| schema.get("required"))
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let mut nudge = format!(
        "Your last response was empty, and your previous submit_result call was rejected:\n{rejection}\n\n         Call submit_result again with the corrected arguments."
    );
    if !required.is_empty() {
        nudge.push_str(&format!(" Required fields: {required}."));
    }
    nudge
}

/// Replace a string argument's schema with an enum of the values that exist
/// right now, so an invented name is rejected by the provider's own validation
/// rather than by the tool. No-op when nothing is available, which would
/// otherwise produce an empty — unsatisfiable — enum.
fn restrict_to_known_values(tools: &mut [Tool], tool: &str, argument: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let Some(tool) = tools.iter_mut().find(|candidate| candidate.name == tool) else {
        return;
    };
    let Some(property) = tool
        .schema
        .as_mut()
        .and_then(|schema| schema.get_mut("properties"))
        .and_then(|properties| properties.get_mut(argument))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    property.insert(
        "enum".to_string(),
        serde_json::Value::Array(
            values
                .iter()
                .cloned()
                .map(serde_json::Value::from)
                .collect(),
        ),
    );
    property.insert(
        "description".to_string(),
        serde_json::Value::String(format!("One of: {}.", values.join(", "))),
    );
}

/// Explain a tool call the agent may not make. A hallucinated name and a real
/// but forbidden one need different wording: telling a model it "is not
/// allowed" to call something that does not exist reads as a permissions
/// problem and invites a retry.
fn rejected_tool_message(agent: &str, tool: &str, allowed: &[String], exists: bool) -> String {
    let available = if allowed.is_empty() {
        "(none)".to_string()
    } else {
        allowed.join(", ")
    };
    if exists {
        format!(
            "Authorization error: Agent '{agent}' is not allowed to execute tool '{tool}'. \
             Do not call it again. Allowed tools: {available}"
        )
    } else {
        format!(
            "Unknown tool '{tool}': no such tool exists. Do not call it again. \
             Tools available to you: {available}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{edit_distance, nearest_enum_value};

    /// The exact enum slips observed in run 9's rejected submission. Each is a
    /// near-miss the model cannot fix from its own output alone.
    #[test]
    fn suggests_the_permitted_value_for_observed_near_misses() {
        assert_eq!(
            nearest_enum_value(
                "reproducer",
                &[
                    "http_cross_tenant",
                    "local_reproducer",
                    "static_proof",
                    "source_only"
                ]
            ),
            Some("local_reproducer")
        );
        assert_eq!(
            nearest_enum_value("very high", &["low", "medium", "high"]),
            Some("high")
        );
    }

    /// `moderate` for `medium` is the most frequent slip and is now fixed by
    /// the synonym table, not by a looser threshold: at 6 edits it is further
    /// from `medium` than the wrong-but-tempting `source` -> `sink` is at 5,
    /// so any distance limit generous enough to catch it would also start
    /// emitting that misleading hint. The dictionary names the meaning
    /// outright and leaves the threshold alone, which is why the
    /// `source`/`sink` protection below still holds.
    #[test]
    fn suggests_semantic_synonyms_that_are_lexically_distant() {
        assert_eq!(
            nearest_enum_value(
                "moderate",
                &["informational", "low", "medium", "high", "critical"]
            ),
            Some("medium")
        );
    }

    /// The pair the dictionary must never break: `source` means `entrypoint`,
    /// and the antonym veto keeps its 5-edit neighbour `sink` off the table
    /// even though nothing lexical rules it out.
    #[test]
    fn source_never_resolves_to_sink() {
        assert_eq!(
            nearest_enum_value("source", &["entrypoint", "propagation", "sink"]),
            Some("entrypoint")
        );
        // Without `entrypoint` on offer there is no answer at all — the veto
        // outranks any remaining candidate.
        assert_eq!(nearest_enum_value("source", &["propagation", "sink"]), None);
    }

    /// A value with no lexical relation and no dictionary entry must produce
    /// no hint at all.
    #[test]
    fn declines_to_guess_when_nothing_matches() {
        assert_eq!(
            nearest_enum_value(
                "code_access",
                &["authentication_level", "authorization_role", "data_state"]
            ),
            None
        );
    }

    /// A negated permitted value is where containment is actively harmful:
    /// `unconfirmed` contains `confirmed`, so the lexical path alone would
    /// suggest the exact opposite verdict.
    #[test]
    fn negations_resolve_to_the_opposite_verdict_not_the_word_they_contain() {
        assert_eq!(
            nearest_enum_value("not confirmed", &["confirmed", "rejected"]),
            Some("rejected")
        );
        // `unconfirmed` sits 2 edits from `confirmed`, too close to carry a
        // synonym entry safely. The structural negation rule reaches it
        // anyway, without putting a word inside the matcher's typo radius.
        assert_eq!(
            nearest_enum_value("unconfirmed", &["confirmed", "rejected"]),
            Some("rejected")
        );
        assert_eq!(
            nearest_enum_value("not duplicate", &["duplicate", "distinct"]),
            Some("distinct")
        );
    }

    /// The 164-case flip family found by exhaustive single-edit search: every
    /// corruption of the `no_` prefix in `no_bypass_found` left `bypass_found`
    /// as the sole containment hit, so a red-team agent writing the negative
    /// verdict was told it meant the affirmative one. That enum has since been
    /// renamed to remove the nesting, so these values are now a synthetic
    /// fixture -- kept because the guard has to hold for any schema, including
    /// one nobody has written yet.
    #[test]
    fn a_value_nested_in_another_never_wins_by_containment() {
        let redteam = &["no_bypass_found", "bypass_found", "blocked"];
        // Skipping containment does better than abstaining: edit distance
        // reaches the intended verdict in BOTH directions, which containment
        // got backwards in both.
        for typed in [
            "not_bypass_found",
            "no bypass_found",
            "nobypass_found",
            "non_bypass_found",
        ] {
            assert_eq!(
                nearest_enum_value(typed, redteam),
                Some("no_bypass_found"),
                "{typed} resolved to the wrong verdict"
            );
        }
        for typed in ["ypass_found", "bypass_foun", "_bypass_found"] {
            assert_eq!(
                nearest_enum_value(typed, redteam),
                Some("bypass_found"),
                "{typed} resolved to the wrong verdict"
            );
        }
        // The rule is scoped to nesting: an ordinary containment hit still works.
        assert_eq!(
            nearest_enum_value("reproducer", &["local_reproducer", "static_proof"]),
            Some("local_reproducer")
        );
    }

    /// Negation is structural, not a list of spellings. A model can negate a
    /// permitted value a dozen ways (`no_`, `non`, `un`, `isnt_`, `not_a_`),
    /// and every one of them CONTAINS the value it negates -- so the lexical
    /// path confidently returns the opposite verdict. Enumerating spellings in
    /// the dictionary is a losing game; the prefix has to be recognised.
    #[test]
    fn any_spelling_of_a_negation_resolves_away_from_the_word_it_contains() {
        let dedup = &["duplicate", "distinct"];
        for typed in [
            "no_duplicate",
            "no duplicate",
            "non_duplicate",
            "nonduplicate",
            "isnt_duplicate",
            "not_a_duplicate",
            "un-duplicate",
        ] {
            assert_eq!(
                nearest_enum_value(typed, dedup),
                Some("distinct"),
                "{typed} must not resolve to the value it negates"
            );
        }

        let verdict = &["confirmed", "rejected"];
        for typed in [
            "no_confirmed",
            "non_confirmed",
            "unconfirmed",
            "isnt_confirmed",
        ] {
            assert_eq!(
                nearest_enum_value(typed, verdict),
                Some("rejected"),
                "{typed} must not resolve to the value it negates"
            );
        }
    }

    /// The complement is only inferable when the domain is binary: `not A`
    /// means B only if the enum is {A, B}. With a third option present the
    /// negation vetoes the affirmed value and stops there.
    #[test]
    fn negation_infers_the_complement_only_for_binary_enums() {
        let three = &["bypass_found", "blocked", "inconclusive"];
        assert_eq!(nearest_enum_value("no_bypass_found", three), None);
    }

    /// Two options matching equally well is not a suggestion.
    #[test]
    fn declines_on_ambiguity() {
        assert_eq!(
            nearest_enum_value("read", &["read_only", "read_write"]),
            None
        );
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("high", "high"), 0);
        assert_eq!(edit_distance("hgh", "high"), 1);
        assert_eq!(edit_distance("", "high"), 4);
    }

    use super::*;

    /// Installing packages is cheap in a disposable image and is a side effect
    /// nobody asked for on the operator's own machine. The hint must therefore
    /// depend on where the harness is running, not on what the agent can do.
    #[test]
    fn the_install_hint_reaches_bash_agents_only_inside_the_image() {
        let restore = std::env::var("FLAWRENCE_IN_CONTAINER").ok();

        unsafe { std::env::set_var("FLAWRENCE_IN_CONTAINER", "1") };
        let prompt = container_capabilities_prompt(true).expect("in the image");
        assert!(prompt.contains("apt-get install"), "{prompt}");
        assert!(prompt.contains("disposable container"), "{prompt}");
        // Bounded: one package for a blocked step, and hands off the target's
        // own toolchain.
        assert!(
            prompt.contains("only what a specific, blocked step needs"),
            "{prompt}"
        );
        assert!(
            prompt.contains("replacing the project's is not"),
            "{prompt}"
        );
        // An agent that cannot run commands has no use for it.
        assert!(container_capabilities_prompt(false).is_none());

        // Off the image, an agent must not be told to install anything.
        unsafe { std::env::remove_var("FLAWRENCE_IN_CONTAINER") };
        assert!(container_capabilities_prompt(true).is_none());
        unsafe { std::env::set_var("FLAWRENCE_IN_CONTAINER", "0") };
        assert!(container_capabilities_prompt(true).is_none());

        unsafe {
            match restore {
                Some(value) => std::env::set_var("FLAWRENCE_IN_CONTAINER", value),
                None => std::env::remove_var("FLAWRENCE_IN_CONTAINER"),
            }
        }
    }

    /// A target with nothing to build must still get a `<build>` section --
    /// silence would leave an agent assuming a build exists and hunting for it,
    /// while a requirement it cannot meet stands unmentioned.
    #[test]
    fn a_target_with_no_build_system_is_told_so_rather_than_told_nothing() {
        let bare = tempfile::tempdir().unwrap();
        let section = build_facts_section(None, &specs(), bare.path(), true)
            .expect("no build system is still something to say");

        assert!(
            section.contains("No build system was detected"),
            "{section}"
        );
        assert!(section.contains("lifted"), "{section}");

        // A workspace that does build, whose probe simply produced no facts,
        // must not be told there is nothing to build -- that would be false.
        let gradle = tempfile::tempdir().unwrap();
        std::fs::write(gradle.path().join("settings.gradle.kts"), "").unwrap();
        assert!(build_facts_section(None, &specs(), gradle.path(), true).is_none());

        // And an agent that cannot run commands has no use for either.
        assert!(build_facts_section(None, &specs(), bare.path(), false).is_none());
    }

    /// The real shipped declarations, so these tests fail if the skills that
    /// carry them stop parsing or stop covering a system.
    fn specs() -> Vec<flawrence_tool::build_systems::BuildSystemSpec> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        flawrence_tool::build_systems::load_specs(&dir).expect("shipped skills parse")
    }

    /// A Maven workspace gets Maven's policy, not Gradle's. An agent told to
    /// avoid `tasks --all` in a Maven reactor learns nothing, and one told to
    /// pass `--max-workers=1` to `mvn` gets an error.
    #[test]
    fn each_build_system_gets_its_own_policy() {
        let maven = tempfile::tempdir().unwrap();
        std::fs::write(maven.path().join("pom.xml"), "<project/>").unwrap();

        let policy = build_system_prompt(&specs(), maven.path(), true).expect("Maven policy");
        assert!(policy.contains("<maven>"), "{policy}");
        assert!(!policy.contains("gradlew"), "{policy}");
        assert!(!policy.contains("tasks --all"), "{policy}");
        // The two facts that make a Maven reproducer work at all.
        assert!(policy.contains("-pl <module> -am"), "{policy}");
        assert!(policy.contains("dependency:build-classpath"), "{policy}");
        // Same escape hatch as Gradle: the policy governs builds, not every
        // reproducer.
        assert!(
            policy.contains("does NOT require every reproducer"),
            "{policy}"
        );
        // Reading console output instead of the XML is how a review concludes a
        // test passed when it never ran.
        assert!(policy.contains("surefire-reports"), "{policy}");

        assert!(build_system_prompt(&specs(), maven.path(), false).is_none());

        // Cargo is cheap enough not to earn context.
        let cargo = tempfile::tempdir().unwrap();
        std::fs::write(cargo.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(build_system_prompt(&specs(), cargo.path(), true).is_none());
    }

    #[test]
    fn gradle_policy_is_injected_only_for_bash_agents_in_gradle_workspaces() {
        let gradle = tempfile::tempdir().unwrap();
        std::fs::write(
            gradle.path().join("settings.gradle.kts"),
            "rootProject.name = \"x\"",
        )
        .unwrap();
        let plain = tempfile::tempdir().unwrap();

        let policy = build_system_prompt(&specs(), gradle.path(), true).expect("Gradle policy");
        assert!(policy.contains("--max-workers=1"), "{policy}");
        // The daemon is what keeps the JVM and Kotlin compiler warm between builds. With
        // concurrency bounded by the harness its memory cost is capped, and banning it made
        // every agent pay cold startup inside a queue -- the run's dominant cost.
        assert!(
            !policy.contains("--no-daemon --max-workers=1"),
            "the mandated command must not ban the daemon: {policy}"
        );
        assert!(
            policy.contains("./gradlew --max-workers=1 <tasks and options>"),
            "{policy}"
        );
        assert!(
            !policy.contains("kotlin.compiler.execution.strategy"),
            "{policy}"
        );
        assert!(
            policy.contains("does NOT require every Kotlin reproducer")
                && policy.contains("prefer its verified standalone compiler"),
            "{policy}"
        );
        assert!(
            policy.contains("Do not pass `--no-daemon`"),
            "the prompt must say so explicitly, or models copy it from habit: {policy}"
        );
        // The fallback spelling must survive: it is the only discovery route left when the
        // pre-warm could not derive facts, and `<build>` is then absent.
        assert!(policy.contains(":<module>:tasks --all"), "{policy}");
        // ... but it must be reachable only as a fallback, never as the default step.
        assert!(policy.contains("do NOT re-probe"), "{policy}");
        assert!(
            policy.contains("Only when neither `<build>` nor `test_idiom`"),
            "{policy}"
        );
        assert!(policy.contains(":<module>:jvmTest"), "{policy}");
        assert!(
            policy.contains("Never verify with `allTests` or `assemble`"),
            "{policy}"
        );
        assert!(policy.contains("-Pname=value"), "{policy}");
        assert!(policy.contains("--init-script <file>"), "{policy}");
        assert!(policy.contains("local.properties"), "{policy}");
        assert!(build_system_prompt(&specs(), gradle.path(), false).is_none());
        assert!(build_system_prompt(&specs(), plain.path(), true).is_none());
    }

    /// The block is for agents that can build, and only when the pre-warm
    /// actually established something. A half-populated or speculative block
    /// would be worse than the discovery it replaces.
    #[test]
    fn build_facts_reach_only_bash_agents_and_only_when_the_pre_warm_ran() {
        let facts = flawrence_tool::prewarm::BuildFacts {
            program: "./gradlew".to_string(),
            system: "Gradle".to_string(),
            flavour: "Kotlin Multiplatform with a JVM target".to_string(),
            test_target_hint: ":<module>:jvmTest".to_string(),
            modules: vec!["kxs".to_string()],
            test_source_dirs: vec!["src/jvmTest/kotlin".to_string()],
            verified_command: "./gradlew --no-daemon compileKotlinJvm".to_string(),
            toolchain_verified: true,
            commit: None,
            failure_excerpt: None,
        };

        // A workspace that does build, so the "nothing to build" path cannot
        // fire and only the facts themselves are in question.
        let gradle = tempfile::tempdir().unwrap();
        std::fs::write(gradle.path().join("settings.gradle.kts"), "").unwrap();

        let section = build_facts_section(Some(&facts), &specs(), gradle.path(), true)
            .expect("bash agents get the facts");
        assert!(
            section.contains("<build>") && section.contains("</build>"),
            "{section}"
        );
        assert!(section.contains("`:<module>:jvmTest`"), "{section}");

        assert!(build_facts_section(Some(&facts), &specs(), gradle.path(), false).is_none());
        // Pre-warm disabled, failed, or unreadable on a target that does build:
        // nothing at all, rather than a false claim that it has no build system.
        assert!(build_facts_section(None, &specs(), gradle.path(), true).is_none());
    }

    #[test]
    fn forbidden_tools_and_invented_tools_read_differently() {
        let allowed = vec!["read_file".to_string(), "todo".to_string()];

        let forbidden = rejected_tool_message("threat-model", "bash", &allowed, true);
        assert!(
            forbidden.contains("not allowed to execute tool 'bash'"),
            "{forbidden}"
        );
        assert!(forbidden.contains("read_file, todo"), "{forbidden}");

        // `choose_step` is not a tool this harness has ever had.
        let invented = rejected_tool_message("threat-model", "choose_step", &allowed, false);
        assert!(
            invented.contains("Unknown tool 'choose_step'"),
            "{invented}"
        );
        assert!(!invented.contains("not allowed"), "{invented}");
        assert!(invented.contains("read_file, todo"), "{invented}");
    }

    #[test]
    fn both_nudge_variants_are_recognised_so_they_do_not_stack() {
        let generic = ChatMessage::user(empty_response_nudge(&[], None));
        assert!(is_empty_response_nudge(&generic));

        // The variant that repeats a submit_result rejection must match too,
        // otherwise ten copies of it accumulate in a validation agent's context.
        let rejected = vec![ChatMessage::user(
            "Schema validation failed. Fix these errors and call submit_result again:".to_string(),
        )];
        let repeat = ChatMessage::user(empty_response_nudge(&rejected, None));
        assert!(is_empty_response_nudge(&repeat), "{repeat:?}");

        // An ordinary message is not a nudge.
        assert!(!is_empty_response_nudge(&ChatMessage::user(
            "read this file".to_string()
        )));
    }

    #[test]
    fn a_budget_with_room_left_permits_one_more_paid_attempt() {
        // Run 7's numbers: $7.84 spent of a $13.00 cap when the stream stalled.
        assert!(budget_has_headroom(7.84, Some(13.00), 0, None, 0.15));
        // Near the ceiling, hold the original no-retry line.
        assert!(!budget_has_headroom(12.50, Some(13.00), 0, None, 0.15));
        // Either budget can veto.
        assert!(!budget_has_headroom(
            1.00,
            Some(13.00),
            99_000,
            Some(100_000),
            0.15
        ));
        // No budget at all: the caller already retries, so never block here.
        assert!(budget_has_headroom(999.0, None, u64::MAX, None, 0.15));
    }

    /// A validation agent on GLM retried submit_result with the same content
    /// and got the same twelve missing fields every time -- because the response
    /// was being cut off after `description`, not because it had forgotten
    /// them. The checklist told it to add fields it never reached.
    #[test]
    fn a_truncated_submit_names_the_cut_and_the_field_to_shorten() {
        let submitted = serde_json::json!({
            "conditions": "short",
            "confidence": "high",
            "description": "x".repeat(40_000),
        });
        let checklist = "Schema validation failed.\nMissing required fields: execution, trace.";

        let advice = truncated_submit_advice(checklist, &submitted);

        // The original checklist survives -- it is still what must end up sent.
        assert!(advice.contains("Missing required fields"), "{advice}");
        // But the diagnosis changes from "you forgot" to "you were cut off",
        // stated as the provider's doing rather than the model's.
        assert!(advice.contains("cut this response off"), "{advice}");
        assert!(advice.contains("output limit"), "{advice}");
        assert!(advice.contains("Do not retry unchanged"), "{advice}");
        // And it names the field to shorten, with its size, rather than leaving
        // the model to guess which of thirteen fields is the problem.
        assert!(advice.contains("`description`"), "{advice}");
        assert!(advice.contains("40000 characters"), "{advice}");
    }

    /// Nothing to name is not a reason to say nothing useful.
    #[test]
    fn truncation_advice_survives_a_payload_with_no_string_fields() {
        let advice = truncated_submit_advice("failed", &serde_json::json!({"count": 3}));
        assert!(advice.contains("cut this response off"), "{advice}");
        assert!(!advice.contains("largest field"), "{advice}");
    }

    #[test]
    fn schema_errors_are_summarised_as_a_field_checklist() {
        let errors = vec![
            r#"- : "title" is a required property"#.to_string(),
            r#"- : "trace" is a required property"#.to_string(),
            r#"- : "execution" is a required property"#.to_string(),
            r#"- /severity: "critical" is not one of the allowed values"#.to_string(),
        ];
        assert_eq!(
            missing_properties(&errors),
            vec!["execution", "title", "trace"]
        );
    }

    #[test]
    fn an_empty_response_after_a_rejected_submit_repeats_the_blocker() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "verdict": {"type": "string"}, "reason": {"type": "string"} },
            "required": ["verdict", "reason"]
        });
        let messages = vec![ChatMessage::tool(MessageContent::from_parts(vec![
            ContentPart::ToolResponse(ToolResponse {
                call_id: "call_1".into(),
                content: "Schema validation failed. Fix these errors and call submit_result                           again:\n- /verdict: \"maybe\" is not one of the allowed values"
                    .into(),
            }),
        ]))];

        let nudge = empty_response_nudge(&messages, Some(&schema));

        assert!(nudge.contains("submit_result call was rejected"), "{nudge}");
        assert!(nudge.contains("not one of the allowed values"), "{nudge}");
        assert!(
            nudge.contains("Required fields: verdict, reason."),
            "{nudge}"
        );
        // The old advice pointed at tool-call formatting, which was never the problem.
        assert!(!nudge.contains("check the format"), "{nudge}");
    }

    #[test]
    fn an_empty_response_with_no_pending_rejection_gets_generic_advice() {
        let messages = vec![ChatMessage::tool(MessageContent::from_parts(vec![
            ContentPart::ToolResponse(ToolResponse {
                call_id: "call_1".into(),
                content: "Lines 1 to 40 of 400: …".into(),
            }),
        ]))];

        let nudge = empty_response_nudge(&messages, None);
        assert!(nudge.contains("call a tool"), "{nudge}");
    }

    #[test]
    fn oversized_tool_responses_are_capped_not_just_logged() {
        let budget = flawrence_tool::MAX_TOOL_RESPONSE_CHARS;
        let capped = cap_tool_response("x".repeat(budget + 5_000), "call_1");

        assert!(capped.starts_with(&"x".repeat(100)));
        assert!(
            capped.contains("exceeded the"),
            "{}",
            &capped[capped.len() - 200..]
        );
        assert!(capped.contains("Narrow the request"));
        // The payload itself is cut to the budget; only the explanation is extra.
        assert_eq!(capped.chars().take_while(|c| *c == 'x').count(), budget);
    }

    #[test]
    fn responses_within_budget_are_untouched() {
        let content = "fits comfortably".to_string();
        assert_eq!(cap_tool_response(content.clone(), "call_1"), content);
    }

    #[test]
    fn runtime_names_replace_free_text_arguments() {
        let mut tools = registry().tools_filtered(&["load_skill".to_string()]);
        let skills = vec!["trailmark".to_string(), "finding-dedup".to_string()];
        restrict_to_known_values(&mut tools, "load_skill", "skill_name", &skills);

        let property = &tools[0].schema.as_ref().unwrap()["properties"]["skill_name"];
        assert_eq!(
            property["enum"],
            serde_json::json!(["trailmark", "finding-dedup"])
        );
        // The stock description names example skills that may not be installed;
        // the injected one names the ones that are.
        assert_eq!(property["description"], "One of: trailmark, finding-dedup.");
    }

    #[test]
    fn nothing_is_injected_when_nothing_is_available() {
        let mut tools = registry().tools_filtered(&["load_skill".to_string()]);
        let before = tools[0].schema.clone();
        restrict_to_known_values(&mut tools, "load_skill", "skill_name", &[]);
        // An empty enum would be unsatisfiable, so the schema is left alone.
        assert_eq!(tools[0].schema, before);
    }

    #[test]
    fn injecting_into_a_tool_the_agent_lacks_is_a_no_op() {
        let mut tools = registry().tools_filtered(&["read_file".to_string()]);
        restrict_to_known_values(&mut tools, "load_skill", "skill_name", &["x".to_string()]);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
    }

    #[test]
    fn an_agent_with_no_tools_is_told_so() {
        let message = rejected_tool_message("reviewer", "todo", &[], true);
        assert!(message.contains("Allowed tools: (none)"), "{message}");
    }

    /// A long tool call must announce itself while it runs. Every other line the
    /// loop logs waits for the tool to return, which is why a finished run, a
    /// cold provider and a six-minute call-graph walk all read as "hung".
    #[tokio::test(start_paused = true)]
    async fn a_slow_tool_call_beats_once_per_interval_while_it_runs() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let beats = AtomicU64::new(0);

        let result = beat_while(
            std::time::Duration::from_secs(30),
            tokio::time::sleep(std::time::Duration::from_secs(155)),
            |_| {
                beats.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        let _: () = result;
        // 155s of work at a 30s interval: beats at 30, 60, 90, 120, 150.
        assert_eq!(beats.load(Ordering::SeqCst), 5);
    }

    /// The common case is a call that returns in milliseconds. Those must stay
    /// silent, or the heartbeat drowns the log it was added to clarify.
    #[tokio::test(start_paused = true)]
    async fn a_fast_tool_call_stays_silent() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let beats = AtomicU64::new(0);

        beat_while(
            std::time::Duration::from_secs(30),
            tokio::time::sleep(std::time::Duration::from_millis(20)),
            |_| {
                beats.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;

        assert_eq!(beats.load(Ordering::SeqCst), 0);
    }

    /// The heartbeat must not change what the tool call returns.
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_passes_the_wrapped_value_through_untouched() {
        let value = beat_while(
            std::time::Duration::from_secs(30),
            async {
                tokio::time::sleep(std::time::Duration::from_secs(70)).await;
                "tool output"
            },
            |_| {},
        )
        .await;

        assert_eq!(value, "tool output");
    }

    /// A subagent logs its own tool calls, so the parent's `spawn_task` is the
    /// one wait that was never opaque. Beating on it buries the leaf calls that
    /// are genuinely silent -- at a twenty-way fan-out, under forty lines a
    /// minute of them.
    #[test]
    fn only_tools_whose_progress_is_invisible_beat() {
        assert!(!beats_while_running("spawn_task"));

        for leaf in ["bash", "code_index", "read_file", "artifact", "grep_files"] {
            assert!(beats_while_running(leaf), "{leaf} is silent while it runs");
        }
    }

    /// Skipping the heartbeat must not change what the call returns.
    #[tokio::test(start_paused = true)]
    async fn a_skipped_heartbeat_still_yields_the_tool_result() {
        let value = with_heartbeat(
            "spawn_task",
            "call-1",
            &serde_json::json!({"agent_name": "threat-model"}),
            std::time::Instant::now(),
            async {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                "child result"
            },
        )
        .await;

        assert_eq!(value, "child result");
    }

    /// Twenty agents queued on the Gradle permit all log `tool=bash`; without a
    /// subject the heartbeat cannot say which build is the slow one.
    #[test]
    fn the_subject_names_which_call_is_running() {
        let subject = heartbeat_subject(&serde_json::json!({
            "command": "./gradlew :kxs:jvmTest --tests '*Probe*'",
            "cwd": "$WORKSPACE"
        }));
        assert_eq!(
            subject.as_deref(),
            Some("./gradlew :kxs:jvmTest --tests '*Probe*'")
        );
    }

    /// A heredoc-bearing command must not turn one heartbeat into twenty lines.
    #[test]
    fn a_multi_line_command_is_flattened_and_truncated_to_one_line() {
        let subject = heartbeat_subject(&serde_json::json!({
            "command": "cat > Probe.kt <<'EOF'\npackage at.asitplus\n\nval x = 1\nEOF\n./gradlew :kxs:jvmTest"
        }))
        .expect("a command is a subject");

        assert!(!subject.contains('\n'), "must stay one line: {subject}");
        assert!(
            subject.chars().count() <= SUBJECT_MAX_CHARS + 3,
            "must be bounded: {subject}"
        );
        assert!(subject.starts_with("cat > Probe.kt"), "{subject}");
        assert!(subject.ends_with("..."), "truncation must show: {subject}");
    }

    #[test]
    fn a_call_with_no_identifying_argument_yields_no_subject() {
        assert_eq!(
            heartbeat_subject(&serde_json::json!({"operation": "list"})),
            None
        );
        assert_eq!(heartbeat_subject(&serde_json::json!({"key": "   "})), None);
        assert_eq!(heartbeat_subject(&serde_json::Value::Null), None);
    }

    /// `code_index` and `artifact` calls are identified too, not just bash.
    #[test]
    fn the_subject_falls_back_through_the_key_list() {
        assert_eq!(
            heartbeat_subject(&serde_json::json!({"operation": "find", "symbol": "DerDecoder"}))
                .as_deref(),
            Some("DerDecoder")
        );
        assert_eq!(
            heartbeat_subject(&serde_json::json!({"operation": "read", "key": "test_idiom"}))
                .as_deref(),
            Some("test_idiom")
        );
    }
}
#[cfg(test)]
mod grounding_tests {
    use super::*;
    use flawrence_agent::{FieldGrounding, FilterExpr};

    fn record(tool: &str, args: serde_json::Value, result: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            tool: tool.to_string(),
            args,
            result,
        }
    }

    /// `passed` grounding (no filter, source=result, when=last) returns the
    /// last run_check call's pass flag.
    #[test]
    fn passed_grounding_uses_last_call_result() {
        let history = vec![
            record(
                "run_check",
                serde_json::json!({"command": "old"}),
                serde_json::json!({"available": true, "passed": false, "errors": ["e"]}),
            ),
            record(
                "run_check",
                serde_json::json!({"command": "fixed"}),
                serde_json::json!({"available": true, "passed": true, "errors": []}),
            ),
        ];
        let binding = FieldGrounding {
            tool: "run_check".to_string(),
            source: "result".to_string(),
            path: "passed".to_string(),
            when: "last".to_string(),
            filter: None,
        };
        assert_eq!(
            extract_grounded(&history, &binding),
            Some(serde_json::Value::Bool(true))
        );
    }

    /// `verified_command` grounding (filter path `passed`, source=args,
    /// path `command`, when=last) returns the command of the last *passing* call.
    ///
    #[test]
    fn verified_command_filter_path_is_relative_to_result() {
        let history = vec![
            record(
                "run_check",
                serde_json::json!({"command": "broken version"}),
                serde_json::json!({"available": true, "passed": false, "errors": ["x"]}),
            ),
            record(
                "run_check",
                serde_json::json!({"command": "passing version"}),
                serde_json::json!({"available": true, "passed": true, "errors": []}),
            ),
        ];
        let binding = FieldGrounding {
            tool: "run_check".to_string(),
            source: "args".to_string(),
            path: "command".to_string(),
            when: "last".to_string(),
            filter: Some(FilterExpr {
                path: "passed".to_string(),
                value: serde_json::Value::Bool(true),
            }),
        };
        assert_eq!(
            extract_grounded(&history, &binding),
            Some(serde_json::Value::String("passing version".to_string()))
        );
    }

    /// The bug: `path: "result.passed"` navigates result.result.passed,
    /// which never exists, so the filter never matches and grounding returns
    /// None. This test documents that the wrong path silently fails.
    #[test]
    fn wrong_filter_path_result_passed_never_matches() {
        let history = vec![record(
            "run_check",
            serde_json::json!({"command": "r"}),
            serde_json::json!({"available": true, "passed": true, "errors": []}),
        )];
        let binding = FieldGrounding {
            tool: "run_check".to_string(),
            source: "args".to_string(),
            path: "command".to_string(),
            when: "last".to_string(),
            filter: Some(FilterExpr {
                path: "result.passed".to_string(),
                value: serde_json::Value::Bool(true),
            }),
        };
        assert_eq!(extract_grounded(&history, &binding), None);
    }

    /// navigate_dot_path supports object keys and array indices.
    #[test]
    fn navigate_dot_path_object_and_index() {
        let v = serde_json::json!({"a": {"b": [10, 20, 30]}});
        assert_eq!(navigate_dot_path(&v, "a.b.1"), Some(serde_json::json!(20)));
        assert_eq!(navigate_dot_path(&v, "a.b.0"), Some(serde_json::json!(10)));
        assert_eq!(navigate_dot_path(&v, "a.missing"), None);
    }

    /// Honest-omission + mismatch classification mirrors the run behavior: a
    /// grounded Some with a submitted None is an insert (not a mismatch); a
    /// grounded None with a submitted Some is a mismatch (claimed, no backing).
    #[test]
    fn grounding_classifies_inserts_and_mismatches() {
        // grounded Some(true), submitted None -> insert (no nudge)
        let g: Option<serde_json::Value> = Some(serde_json::Value::Bool(true));
        let s: Option<serde_json::Value> = None;
        let mut result = serde_json::json!({});
        let mut mm: Vec<(&str, Option<serde_json::Value>, Option<serde_json::Value>)> = Vec::new();
        let mut ins: Vec<(&str, serde_json::Value)> = Vec::new();
        match (g.clone(), s) {
            (Some(gv), Some(sv)) if gv != sv => mm.push(("passed", Some(sv), Some(gv))),
            (Some(gv), None) => ins.push(("passed", gv)),
            (None, Some(sv)) => mm.push(("passed", Some(sv), None)),
            (Some(_), Some(_)) | (None, None) => {}
        }
        assert!(mm.is_empty(), "honest omission is not a mismatch");
        assert_eq!(ins.len(), 1);
        if let Some(obj) = result.as_object_mut() {
            for (f, v) in ins {
                obj.insert(f.to_string(), v);
            }
        }
        assert_eq!(result, serde_json::json!({"passed": true}));
    }
}
