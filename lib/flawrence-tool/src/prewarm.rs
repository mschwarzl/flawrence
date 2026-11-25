//! Compile the target once before the agents fan out.
//!
//! A cold Gradle build costs minutes and downloads a full dependency graph.
//! Paying that once in the shared root, before ~20 agents start, is the
//! difference between one cold compile and twenty: the dependency cache and the
//! build cache live under `GRADLE_USER_HOME`, which every worktree shares, so
//! per-agent builds hit a warm cache instead of recompiling from scratch. That
//! reuse is not automatic — Gradle's build cache is opt-in per invocation — so
//! the pre-warm also enables it for the user rather than trusting the agents to
//! pass a flag (see `ensure_gradle_build_cache_enabled`).
//!
//! Everything here is best-effort. A target with no recognised build system, a
//! build that fails, a missing wrapper — all of them log and let the run
//! continue. The analysis does not depend on the target compiling.
//!
//! A pre-warm is minutes of silence unless it says what it is doing, which
//! reads as a frozen harness. Every step announces itself on stderr — the
//! stream the operator is already watching for the run banner — and the child's
//! own output is forwarded line by line while it runs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Property that makes Gradle's build cache opt-in-by-default for a user.
const GRADLE_CACHING_KEY: &str = "org.gradle.caching";

/// A cold multi-module Kotlin build can legitimately take this long; beyond it,
/// something is wrong and the run should get on with its actual work.
/// Fallback ceiling when a caller does not supply one. The operator-facing
/// default lives in `Config` (`FLAWRENCE_PREWARM_TIMEOUT_SECS`); this only
/// guards direct callers such as tests.
const PREWARM_TIMEOUT_FALLBACK: Duration = Duration::from_secs(90 * 60);

/// What the pre-warm did, for the run log.
#[derive(Debug)]
pub enum PrewarmOutcome {
    /// Switched off by configuration.
    Disabled,
    /// Nothing recognisable to build.
    NoBuildSystem,
    /// The build ran; `success` says whether it compiled.
    Ran {
        system: String,
        command: String,
        success: bool,
        elapsed: Duration,
        detail: String,
        /// What Gradle reported it actually did, when the target is Gradle.
        work: Option<GradleWork>,
        /// The tail of a failed build's output. Recorded here as well as on the
        /// facts, because the case that needs it most -- a build that produced
        /// nothing, so there are no facts -- has nowhere else to put it.
        failure: Option<String>,
        /// What the probe learned about the build, when enough of it could be
        /// read to be worth telling the agents. `None` whenever any part of it
        /// would be a guess. Boxed to keep this variant from dwarfing the two
        /// unit variants every non-building run returns.
        facts: Option<Box<BuildFacts>>,
    },
}

impl PrewarmOutcome {
    /// One line for the run log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => "disabled".to_string(),
            Self::NoBuildSystem => "no recognised build system; skipped".to_string(),
            Self::Ran {
                command,
                success,
                elapsed,
                detail,
                work,
                ..
            } => {
                let verdict = if *success { "ok" } else { "failed" };
                let mut line = format!("{command} -> {verdict} in {:.1}s", elapsed.as_secs_f64());
                // A wall-clock duration alone cannot be trusted: a few seconds
                // is either a warm `build/` dir or a build that did nothing.
                // The task outcomes are what settles it, so they travel with
                // the duration.
                if let Some(work) = work.as_ref().filter(|work| work.observed()) {
                    line.push_str(&format!(" [{}]", work.describe()));
                }
                if !success && !detail.is_empty() {
                    line.push_str(&format!(" ({detail})"));
                }
                line
            }
        }
    }

    /// The facts to hand the agents, when the pre-warm established any.
    #[must_use]
    pub fn facts(&self) -> Option<&BuildFacts> {
        match self {
            Self::Ran { facts, .. } => facts.as_deref(),
            _ => None,
        }
    }

    /// The audit record written next to the run's artifacts.
    ///
    /// Always renders something, including for a run that never built: "no
    /// facts, because X" is the answer an audit needs, whereas a missing file
    /// is indistinguishable from a crashed harness.
    #[must_use]
    pub fn audit_report(&self) -> String {
        let mut out = String::from("# Build facts\n\n");
        match self {
            Self::Disabled => {
                out.push_str(
                    "No build facts were derived: the build pre-warm was disabled by \
                     configuration.\n",
                );
            }
            Self::NoBuildSystem => {
                out.push_str(
                    "**No build system was detected**, so nothing was compiled and no build facts \
                     exist.\n\n\
                     This is a legitimate state -- a decompiled artifact, a vendored source drop, \
                     a documentation tree -- and the run continues. Agents are told that the \
                     requirement for a compiled, executed reproducer is lifted, and that findings \
                     must instead rest on source, exact locations and reasoning, stating plainly \
                     that no reproducer was possible.\n\n\
                     If you expected this target to build, that is worth intervening over: check \
                     that the workspace root is the directory you meant, and that its manifest is \
                     one of the build systems declared in the idiom skills.\n",
                );
            }
            Self::Ran {
                system,
                command,
                success,
                failure,
                elapsed,
                detail,
                work,
                facts,
            } => {
                out.push_str(&format!(
                    "- Build system detected: {}\n- Pre-warm command: `{command}`\n- Result: \
                     {}\n- Elapsed: {:.1}s\n",
                    system,
                    if *success { "success" } else { "failed" },
                    elapsed.as_secs_f64()
                ));
                if let Some(work) = work.as_ref().filter(|work| work.observed()) {
                    out.push_str(&format!("- Gradle work: {}\n", work.describe()));
                }
                if !detail.is_empty() {
                    out.push_str(&format!("- Detail: {detail}\n"));
                }
                if let Some(failure) = failure {
                    out.push_str(&format!(
                        "\n## How the build failed\n\nTail of the pre-warm output. A build that \
                         fails one target while compiling the rest is normal for a project that \
                         cross-compiles; the facts above are still usable.\n\n```\n{failure}\n```\n"
                    ));
                }
                match facts {
                    None => out.push_str(
                        "\nNo build facts were derived: the task list could not be read, the \
                         project reported no test task, or the build failed. Agents fall back to \
                         discovering the build themselves.\n",
                    ),
                    Some(facts) => {
                        out.push_str(&format!(
                            "- Layout probe: read successfully\n\n## Derived facts\n\n- \
                             Invocation: `{}`\n- Project flavour: {}\n- Workspace default test-target hint: \
                             `{}` (mixed modules may differ)\n- Modules ({}): {}\n- Test source directories: {}\n",
                            facts.program,
                            facts.flavour,
                            facts.test_target_hint,
                            facts.modules.len(),
                            if facts.modules.is_empty() {
                                "none found".to_string()
                            } else {
                                facts.modules.join(", ")
                            },
                            if facts.test_source_dirs.is_empty() {
                                "none found on disk".to_string()
                            } else {
                                facts.test_source_dirs.join(", ")
                            },
                        ));
                        out.push_str("\n## Emitted to every bash-capable agent\n\n```\n");
                        out.push_str(&facts.render());
                        out.push_str("\n```\n");
                    }
                }
            }
        }
        out
    }
}

/// Gradle's own account of a build: how many tasks ran, and how many were
/// already done.
///
/// This is the only honest answer to "did that three-second build compile
/// anything?", which is why it is parsed rather than inferred from timing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GradleWork {
    /// Tasks Gradle actually executed.
    pub executed: usize,
    /// Tasks whose outputs were already current.
    pub up_to_date: usize,
    /// Tasks satisfied from the build cache.
    pub from_cache: usize,
    /// Tasks with nothing to work on.
    pub no_source: usize,
    /// Tasks skipped for any other reason.
    pub skipped: usize,
    /// Gradle's own `BUILD SUCCESSFUL in 3s` / `BUILD FAILED in 3s` line.
    pub result_line: Option<String>,
}

impl GradleWork {
    /// Whether anything at all was parsed, so an empty summary is not printed.
    #[must_use]
    pub fn observed(&self) -> bool {
        self.result_line.is_some() || self.total_tasks() > 0
    }

    fn total_tasks(&self) -> usize {
        self.executed + self.up_to_date + self.from_cache + self.no_source + self.skipped
    }

    /// One bracketed phrase for the outcome line.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(result) = &self.result_line {
            parts.push(result.clone());
        }
        parts.push(format!(
            "{} executed, {} up-to-date, {} from-cache, {} no-source, {} skipped",
            self.executed, self.up_to_date, self.from_cache, self.no_source, self.skipped
        ));
        // Spelling out the no-op case, because that is the reading the operator
        // cannot make from the duration and would otherwise mistake for a fast
        // compile.
        if self.total_tasks() > 0 && self.executed == 0 && self.from_cache == 0 {
            parts.push("nothing compiled: the build was already warm".to_string());
        }
        parts.join("; ")
    }
}

/// Count task outcomes and the build result out of `--console=plain` output.
///
/// Gradle marks each task as `> Task :path SUFFIX`, with no suffix when it
/// really ran. Anything that is not a task line or the build result is ignored,
/// so compiler chatter cannot inflate the counts.
impl GradleWork {
    /// Whether Gradle got far enough to build something usable.
    ///
    /// A build that fails after compiling most of a reactor is not the same
    /// event as one that never started: the first leaves compiled outputs and
    /// a task graph that was really read, the second leaves nothing. Projects
    /// that cross-compile for several toolchains routinely fail one target
    /// while the parts an analysis needs are already built.
    #[must_use]
    pub fn produced_artifacts(&self) -> bool {
        self.executed + self.up_to_date + self.from_cache > 0
    }
}

#[must_use]
pub fn parse_gradle_work(output: &str) -> GradleWork {
    let mut work = GradleWork::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("> Task ") {
            match rest.split_whitespace().nth(1) {
                None => work.executed += 1,
                Some("UP-TO-DATE") => work.up_to_date += 1,
                Some("FROM-CACHE") => work.from_cache += 1,
                Some("NO-SOURCE") => work.no_source += 1,
                Some(_) => work.skipped += 1,
            }
        } else if line.starts_with("BUILD SUCCESSFUL")
            || line.starts_with("BUILD FAILED")
            || line.starts_with("CONFIGURE SUCCESSFUL")
        {
            work.result_line = Some(line.chars().take(120).collect());
        }
    }
    work
}

/// What the pre-warm learned about the target's build, kept so that no agent
/// has to learn it again.
///
/// Every field here was read from a probe that already ran, never guessed:
/// ~20 agents each running `tasks --all` to rediscover these same facts
/// consume every Gradle verifier lane at 30-120s apiece, and
/// that is the largest avoidable cost in a run. A field that could not be read
/// reliably keeps the whole struct out of the prompt, because a confidently
/// wrong build fact sends an agent down a path that cannot work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildFacts {
    /// Which build system produced these facts. Decides only how they are
    /// phrased -- every field below means the same thing for all of them.
    /// The declared name of the build system, as its skill gives it. A plain
    /// name rather than a variant: every system is detected, pre-warmed and
    /// routed the same way, and the only thing that differs is whether one has
    /// an optional facts-derivation pass written for it.
    pub system: String,
    /// How to invoke the build, as the agents should type it (`./gradlew`,
    /// `./mvnw`, `mvn`).
    pub program: String,
    /// Human phrase for the project shape, e.g. "Kotlin Multiplatform with a
    /// JVM target" or "Maven multi-module (JVM)".
    pub flavour: String,
    /// How to name one module's tests on a command line, with `<module>` left
    /// as a placeholder: `:<module>:jvmTest`, `-pl <module> test`.
    pub test_target_hint: String,
    /// Module paths in the build system's own notation (`kxs`, `a:b`,
    /// `core/api`).
    pub modules: Vec<String>,
    /// Test source directories that exist on disk, relative to a module.
    pub test_source_dirs: Vec<String>,
    /// The pre-warm invocation these facts came from.
    pub verified_command: String,
    /// Whether that invocation completed. False when the build failed after
    /// producing usable outputs, which leaves the facts sound but the toolchain
    /// unproven.
    pub toolchain_verified: bool,
    /// The target's HEAD commit when these facts were derived, when it is a git
    /// repository. Compared before reusing a cached set: facts from another
    /// commit describe a module layout that may no longer exist.
    #[serde(default)]
    pub commit: Option<String>,
    /// The tail of a failed build's output. `None` when the build succeeded.
    ///
    /// "BUILD FAILED in 4s" tells an agent nothing it can act on. The lines
    /// before it name the module and the reason, which is what decides whether
    /// a reproducer can route around the break or has to avoid that module.
    pub failure_excerpt: Option<String>,
}

/// Beyond this many modules the list stops being a fact an agent reads and
/// starts being a wall of text in every bash agent's context, on every task.
const MODULE_LIST_LIMIT: usize = 40;

impl BuildFacts {
    /// The `<build>` section for an agent system prompt.
    ///
    /// Compact by construction: this is charged to every bash-capable agent on
    /// every task, so each line has to earn its place.
    #[must_use]
    pub fn render(&self) -> String {
        let flavour = &self.flavour;
        let name = self.system.as_str();
        let discovery = match name {
            "Gradle" => {
                "do NOT run `tasks --all` or any other discovery build. The harness bounds \
                 Gradle in persistent verifier lanes, so rediscovering this still costs the whole \
                 run minutes."
            }
            "Maven" => {
                "do NOT run `help:evaluate`, `-Dexec` probes or any other discovery build. \
                 Maven resolves the whole reactor on every invocation, so rediscovering this costs \
                 the whole run minutes."
            }
            _ => "do NOT re-probe the workspace layout.",
        };
        let mut block = String::from("<build>\n");
        block.push_str(&format!(
            "Probed once at startup, already correct: {discovery}\n\
             Build system: {name}, invoked as `{}` ({flavour}).\n\
             Workspace default test-target hint: `{}`. Mixed modules may differ; exact module \
             profiles in `test_idiom` override this hint.\n",
            self.program, self.test_target_hint
        ));
        if !self.modules.is_empty() {
            let shown = self.modules.len().min(MODULE_LIST_LIMIT);
            block.push_str(&format!("Modules: {}", self.modules[..shown].join(", ")));
            if shown < self.modules.len() {
                block.push_str(&format!(" (+{} more)", self.modules.len() - shown));
            }
            block.push_str(".\n");
        }
        if !self.test_source_dirs.is_empty() {
            block.push_str(&format!(
                "Test sources (relative to a module): {}.\n",
                self.test_source_dirs.join(", ")
            ));
        }
        block.push_str(&if self.toolchain_verified {
            format!(
                "Toolchain verified at startup by `{}`.\n</build>",
                self.verified_command
            )
        } else {
            let mut note = format!(
                "Derived from `{}`, which compiled part of the project and then failed. The facts \
                 above come from a task graph that was really read and the outputs that did build \
                 are present, so use them; a target this project cannot compile is not yours to \
                 fix. Expect some modules to be unbuilt.\n",
                self.verified_command
            );
            if let Some(excerpt) = &self.failure_excerpt {
                note.push_str(&format!(
                    "How it failed, so you can tell which modules are affected without \
                     rebuilding (full output in build-facts.md):\n{excerpt}\n"
                ));
            }
            note.push_str("</build>");
            note
        });
        block
    }
}

/// Whether the reported task list belongs to a Kotlin Multiplatform project
/// with a JVM target.
///
/// `jvmTest` and `compileKotlinJvm` exist only when a multiplatform module
/// declares a `jvm()` target, so either one identifies KMP reproducibly.
#[must_use]
pub fn is_multiplatform(available: &[String]) -> bool {
    let has = |task: &str| available.iter().any(|known| known == task);
    has("compileKotlinJvm") || has("compileTestKotlinJvm") || has("jvmTest")
}

/// The task that runs a module's JVM tests, out of the reported task list.
///
/// KMP outranks `test`: a multiplatform module's `test` task is an aggregate
/// that may run nothing on the JVM, while `jvmTest` is what actually executes a
/// reproducer. `None` when neither name was reported, so nothing is claimed.
#[must_use]
pub fn select_test_task(available: &[String]) -> Option<String> {
    let has = |task: &str| available.iter().any(|known| known == task);
    if is_multiplatform(available) && has("jvmTest") {
        return Some("jvmTest".to_string());
    }
    if has("test") {
        return Some("test".to_string());
    }
    None
}

/// Module paths out of the qualified task names in `gradle tasks --all` output.
///
/// Qualified lines read `:kxs:jvmTest - description`; everything before the
/// last colon is the module path, which is exactly how an agent addresses it.
/// Root-project tasks carry no path and contribute nothing.
#[must_use]
pub fn parse_gradle_module_names(output: &str) -> Vec<String> {
    let mut modules: Vec<String> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        let candidate = line.split(" - ").next().unwrap_or(line).trim();
        let Some(path) = candidate.strip_prefix(':') else {
            continue;
        };
        let Some((module, task)) = path.rsplit_once(':') else {
            continue;
        };
        if module.is_empty() || task.is_empty() {
            continue;
        }
        // Anything that is not a plain Gradle path is prose that happened to
        // start with a colon, and a bogus module name is worse than none.
        if !module
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':')
        {
            continue;
        }
        if !modules.iter().any(|known| known == module) {
            modules.push(module.to_string());
        }
    }
    modules.sort();
    modules
}

/// Module paths out of a `settings.gradle` / `settings.gradle.kts` file.
///
/// Covers both dialects (`include(":a")` and `include ":a", ":b"`) by reading
/// the quoted arguments of any `include` line rather than parsing the script.
#[must_use]
pub fn parse_settings_gradle_modules(contents: &str) -> Vec<String> {
    let mut modules: Vec<String> = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with("//") || !line.contains("include") {
            continue;
        }
        for (index, piece) in line.split(['"', '\'']).enumerate() {
            // Odd indices are the insides of quotes; even ones are the code
            // between them.
            if index % 2 == 0 {
                continue;
            }
            let module = piece.trim().trim_start_matches(':');
            if module.is_empty()
                || !module.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':'
                })
            {
                continue;
            }
            if !modules.iter().any(|known| known == module) {
                modules.push(module.to_string());
            }
        }
    }
    modules.sort();
    modules
}

/// The conventional test source directories for the project flavour, keeping
/// only those `exists` confirms.
///
/// The filesystem lookup is the caller's, so the choice itself is testable
/// without a checkout. Conventions are not guarantees — a KMP project may have
/// no `commonTest` at all — which is why nothing is emitted unchecked.
#[must_use]
pub fn select_test_source_dirs(multiplatform: bool, exists: impl Fn(&str) -> bool) -> Vec<String> {
    let candidates: &[&str] = if multiplatform {
        &["src/commonTest/kotlin", "src/jvmTest/kotlin"]
    } else {
        &["src/test/kotlin", "src/test/java"]
    };
    candidates
        .iter()
        .filter(|candidate| exists(candidate))
        .map(|candidate| (*candidate).to_string())
        .collect()
}

/// Detect the build system of `root` by the files it ships.
///
/// Probed in the order that a polyglot repository should be read: a Gradle
/// wrapper or build script wins over a `pom.xml` sitting next to it, because
/// the wrapper is what a Gradle project's own CI runs.
#[must_use]
pub fn detect_build_system(root: &Path) -> Option<&'static crate::build_systems::BuildSystemSpec> {
    crate::build_systems::detect(crate::build_systems::loaded(), root)
}

/// Pick the Gradle tasks that compile main and test sources.
///
/// Which tasks exist depends on the plugins applied: a JVM project has
/// `classes`/`testClasses`, a Kotlin Multiplatform project has
/// `compileKotlinJvm`/`compileTestKotlinJvm` and may have neither of the
/// former. Hardcoding one pair compiles nothing on half the targets, so the
/// choice is made from the task list the project actually reports.
#[must_use]
pub fn select_gradle_tasks(available: &[String]) -> Vec<String> {
    const KMP: &[&str] = &["compileKotlinJvm", "compileTestKotlinJvm"];
    const JVM: &[&str] = &["classes", "testClasses"];
    const JAVA: &[&str] = &["compileJava", "compileTestJava"];
    const ASSEMBLE: &[&str] = &["assemble"];

    let multiplatform = is_multiplatform(available);

    // KMP must outrank `classes`/`testClasses`. A multiplatform module gets
    // those tasks only if it calls `withJava()`, which most do not -- so when
    // they are present at all they usually belong to some *other*, JVM-only
    // module (a benchmark or a build tool), and warming it compiles nothing the
    // agents use while still reporting success. `compileTestKotlinJvm` is
    // exactly what `:module:jvmTest` needs, which is what the agents run.
    //
    // `assemble` is deliberately unreachable for KMP: it drags in every
    // declared target, so a project with Apple or Linux native targets burns an
    // hour producing artifacts no agent will ever execute.
    let candidates: Vec<&[&str]> = if multiplatform {
        vec![KMP, JVM, JAVA]
    } else {
        vec![JVM, JAVA, ASSEMBLE]
    };

    for group in candidates {
        let chosen: Vec<String> = group
            .iter()
            .filter(|task| available.iter().any(|known| known == *task))
            .map(|task| (*task).to_string())
            .collect();
        if !chosen.is_empty() {
            return chosen;
        }
    }
    // A project whose task list could not be read still deserves an attempt;
    // `classes testClasses` is what the overwhelming majority offers, and a
    // wrong guess costs a failed pre-warm, not a failed run.
    if available.is_empty() {
        return vec!["classes".to_string(), "testClasses".to_string()];
    }
    Vec::new()
}

/// Raw `gradle tasks --all` output for `root`, as far as it can be read.
///
/// Returns an empty string when the probe fails; the caller then falls back to
/// the conventional task names rather than skipping the pre-warm. The raw text
/// is returned rather than parsed names because the qualified names carry the
/// module list, and this is the only place in the run where that listing is
/// Run a declared probe and return its stdout.
///
/// Not forwarded to the operator: a task listing is hundreds of lines of no
/// interest, and it is the compile that is worth watching. An unreadable probe
/// is not fatal -- the candidate match simply finds nothing and the pre-warm is
/// skipped, which is better than guessing at a command.
fn run_probe(root: &Path, rendered: &str) -> String {
    let mut words = rendered.split_whitespace();
    let Some(program) = words.next() else {
        return String::new();
    };
    let mut command = Command::new(program);
    command.current_dir(root).args(words);
    report("probing the build to pick a compile task");
    let Ok(output) = run_command(command, Duration::from_secs(300), false) else {
        report("the probe could not be read; skipping the pre-warm");
        return String::new();
    };
    output.stdout
}

/// Task names out of `gradle tasks --all` output.
///
/// Lines are `name - description`, or bare names for tasks without one, with
/// section headers and rules interleaved. Qualified names (`:sub:classes`) are
/// reduced to their task name because that is how they are invoked from the
/// root.
#[must_use]
pub fn parse_gradle_task_names(output: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('-') || line.ends_with(':') {
            continue;
        }
        let candidate = line.split(" - ").next().unwrap_or(line).trim();
        let candidate = candidate.rsplit(':').next().unwrap_or(candidate);
        if candidate.is_empty()
            || candidate.contains(char::is_whitespace)
            || !candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let candidate = candidate.to_string();
        if !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

/// Compile the target's main and test sources in `root`.
///
/// Never fails the caller: every problem is reported through the returned
/// outcome.
pub fn prewarm(
    specs: &[crate::build_systems::BuildSystemSpec],
    root: &Path,
    enabled: bool,
    timeout: Option<Duration>,
) -> PrewarmOutcome {
    let timeout = timeout.unwrap_or(PREWARM_TIMEOUT_FALLBACK);
    if !enabled {
        return PrewarmOutcome::Disabled;
    }
    let Some(system) = crate::build_systems::detect(specs, root) else {
        report("no recognised build system; skipping");
        return PrewarmOutcome::NoBuildSystem;
    };
    let gradle = system.name == "Gradle";
    report(&format!(
        "{} detected in {}",
        describe_system(system, root),
        root.display()
    ));
    if gradle {
        // Only worth doing when there is a Gradle project to warm, and only
        // together with the pre-warm that fills the cache in the first place.
        match ensure_gradle_build_cache_enabled() {
            Ok(message) => report(&message),
            Err(error) => report(&format!("build cache not enabled: {error}")),
        }
        match ensure_gradle_java_installations() {
            Ok(message) => report(&message),
            Err(error) => report(&format!("JDK installations not registered: {error}")),
        }
    }
    let Some((command, rendered, facts)) = build_invocation(root, system) else {
        report("no compile task the project offers; skipping");
        return PrewarmOutcome::NoBuildSystem;
    };
    report(&format!("running `{rendered}` in {}", root.display()));
    let started = Instant::now();
    let mut result = run_command(command, timeout, true);

    // Gradle must run on a JDK it recognises, and its support lags each JDK
    // release by months. An image tracking the newest JDK therefore fails on
    // any project whose wrapper predates it -- before compiling a line, while
    // Groovy is still parsing the build script. Pin the older JDK for the whole
    // run rather than per invocation: `org.gradle.java.home` in the harness's
    // own Gradle home reaches every later agent without any of them knowing.
    if gradle
        && result
            .as_ref()
            .is_ok_and(|output| !output.success && needs_older_jdk(&output.combined()))
        && let Some(java_home) = fallback_java_home()
    {
        match pin_gradle_java_home(&java_home) {
            Ok(message) => {
                report(&message);
                if let Some((retry, _, _)) = build_invocation(root, system) {
                    report(&format!("retrying `{rendered}` on the fallback JDK"));
                    result = run_command(retry, timeout, true);
                }
            }
            Err(error) => report(&format!("could not pin a fallback JDK: {error}")),
        }
    }
    let elapsed = started.elapsed();
    let mut combined = String::new();
    let (success, detail, work) = match result {
        Ok(output) => {
            combined = output.combined();
            let work = gradle.then(|| parse_gradle_work(&combined));
            let detail = if output.success {
                String::new()
            } else {
                last_meaningful_line(&output.stderr, &output.stdout)
            };
            (output.success, detail, work)
        }
        Err(error) => (false, error, None),
    };
    // Read before `work` is moved into the outcome below.
    let produced = work.as_ref().is_some_and(GradleWork::produced_artifacts);
    PrewarmOutcome::Ran {
        system: system.name.clone(),
        command: rendered,
        success,
        elapsed,
        detail,
        work,
        failure: (!success && !combined.trim().is_empty()).then(|| failure_excerpt(&combined)),
        // A build that failed after compiling most of a reactor still leaves a
        // task graph that was really read and outputs on disk. Dropping those
        // facts sends every agent to rediscover them, which is what the derive-
        // once pipeline exists to prevent -- and cross-compiling projects fail
        // one toolchain routinely. Keep them, marked unverified. Only a build
        // that produced nothing at all is worth nothing.
        facts: facts.filter(|_| success || produced).map(|mut facts| {
            facts.toolchain_verified = success;
            facts.failure_excerpt = (!success).then(|| failure_excerpt(&combined));
            Box::new(facts)
        }),
    }
}

/// The report for a run that reused an earlier run's facts.
///
/// Written because the alternative is worse than writing nothing: the pre-warm
/// is skipped on this path, so the ordinary report would say the pre-warm was
/// "disabled by configuration" -- the opposite of what happened -- and would
/// replace the previous run's report, taking the emitted `<build>` block with
/// it, while the agents were in fact given exactly that block.
#[must_use]
pub fn adopted_report(facts: &BuildFacts) -> String {
    format!(
        "# Build facts\n\n\
         - Source: reused from a previous run against this artifacts directory\n\
         - Commit: {}\n\
         - Pre-warm: skipped, because these facts already describe this commit\n\
         - Originally derived by: `{}`\n\n\
         ## Derived facts\n\n\
         - Invocation: `{}`\n\
         - Project flavour: {}\n\
         - Workspace default test-target hint: `{}` (mixed modules may differ)\n\
         - Modules ({}): {}\n\
         - Test source directories: {}\n\n\
         ## Emitted to every bash-capable agent\n\n```\n{}\n```\n",
        facts.commit.as_deref().unwrap_or("unknown"),
        facts.verified_command,
        facts.program,
        facts.flavour,
        facts.test_target_hint,
        facts.modules.len(),
        if facts.modules.is_empty() {
            "none found".to_string()
        } else {
            facts.modules.join(", ")
        },
        if facts.test_source_dirs.is_empty() {
            "none found on disk".to_string()
        } else {
            facts.test_source_dirs.join(", ")
        },
        facts.render(),
    )
}

/// The file a derived fact set is cached in, beside the human-readable report.
///
/// Machine-readable rather than parsed back out of `build-facts.md`: the report
/// is written for an operator, and reconstructing facts from prose is the kind
/// of thing that works until the prose changes.
fn cached_facts_path(artifacts_dir: &Path) -> PathBuf {
    artifacts_dir.join("build-facts.json")
}

/// The target's HEAD, or `None` when it is not a git repository.
fn target_commit(root: &Path) -> Option<String> {
    crate::workspace_patch::git(root, &["rev-parse", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|head| !head.is_empty())
}

/// Record a derived fact set so a later run against the same artifacts
/// directory does not have to derive it again.
pub fn save_facts(artifacts_dir: &Path, root: &Path, facts: &BuildFacts) -> Result<(), String> {
    let mut facts = facts.clone();
    facts.commit = target_commit(root);
    let json = serde_json::to_string_pretty(&facts).map_err(|error| error.to_string())?;
    std::fs::write(cached_facts_path(artifacts_dir), json).map_err(|error| error.to_string())
}

/// A fact set an earlier run derived, if it still describes this checkout.
///
/// The pre-warm is deterministic, so re-deriving reaches the same answer -- but
/// only for the same source. Facts from another commit would describe a module
/// layout that may have moved, and a wrong build fact is worse than none: it
/// sends an agent at a task that does not exist and the failure looks like the
/// agent's own mistake. Refuse rather than guess.
#[must_use]
pub fn load_cached_facts(artifacts_dir: &Path, root: &Path) -> Option<BuildFacts> {
    let text = std::fs::read_to_string(cached_facts_path(artifacts_dir)).ok()?;
    let facts: BuildFacts = serde_json::from_str(&text).ok()?;
    let current = target_commit(root);
    // Only reusable when both sides agree on the commit. An unknown commit on
    // either side is not agreement.
    match (&facts.commit, &current) {
        (Some(recorded), Some(current)) if recorded == current => Some(facts),
        _ => None,
    }
}

/// The `<build>` section for a workspace with no build system.
///
/// Nothing to build is a legitimate state, not a failure: a decompiled APK, a
/// vendored source drop, a documentation tree. But an agent told nothing will
/// assume a build exists and spend its turns hunting for one, and the standing
/// requirement that a reproducer must compile and run has no meaning here. Say
/// both things plainly, and say what evidence looks like instead.
#[must_use]
pub fn no_build_system_block(root: &Path) -> String {
    format!(
        "<build>\nNo build system was detected in {}. This is a fact about the target, not a \
         failure, and re-probing will not change it: there is no manifest for any build system \
         the harness knows.\n\
         Do NOT go looking for one, and do NOT treat this as a blocker.\n\
         The usual requirement that a finding carry a compiled, executed reproducer is lifted \
         here, because there is nothing to compile it against. Evidence is instead the source \
         itself: exact file and line, the values that reach the code path, and the reasoning from \
         input to consequence. Say explicitly in every finding that no reproducer was possible and \
         why, so the claim is judged on what it actually rests on.\n\
         If part of the tree does build -- a vendored module with its own manifest, a script that \
         compiles one file -- say so and use it; the absence is about the workspace root.\n\
         </build>",
        root.display()
    )
}

/// The tail of a build's output, bounded for a system prompt.
///
/// The tail, because a build reports its failure last: the compiler error and
/// the failing task are at the end, while the head is task-graph chatter. Kept
/// small because this is charged to every bash-capable agent on every task --
/// enough to name the module and the reason, not enough to be a build log.
#[must_use]
pub fn failure_excerpt(output: &str) -> String {
    const MAX_LINES: usize = 20;
    const MAX_CHARS: usize = 1_200;

    let meaningful: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let skipped = meaningful.len().saturating_sub(MAX_LINES);
    let mut excerpt = String::new();
    if skipped > 0 {
        excerpt.push_str(&format!("[... {skipped} earlier lines ...]\n"));
    }
    for line in meaningful.iter().skip(skipped) {
        if excerpt.chars().count() + line.chars().count() + 1 > MAX_CHARS {
            break;
        }
        excerpt.push_str(line);
        excerpt.push('\n');
    }
    excerpt.trim_end().to_string()
}

/// Whether a failed build failed because the JDK is newer than the build
/// tooling supports.
///
/// Deliberately narrow: this decides whether to silently change the JDK the
/// whole run builds on, so it must not fire on an ordinary compile error.
#[must_use]
pub fn needs_older_jdk(output: &str) -> bool {
    output.contains("Unsupported class file major version")
        || output.contains("Unsupported major.minor version")
        || (output.contains("Unsupported Java") && output.contains("Gradle"))
        || output.contains("no longer supports Java")
}

/// Every JDK the image provides, oldest first, as announced by
/// `FLAWRENCE_JAVA_INSTALLATIONS`.
///
/// Absent on a developer machine, where none of this happens and a build
/// failure is reported as-is.
fn java_installations() -> Vec<PathBuf> {
    let Some(raw) = std::env::var_os("FLAWRENCE_JAVA_INSTALLATIONS") else {
        return Vec::new();
    };
    raw.to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.join("bin").join("java").is_file())
        .collect()
}

/// The oldest JDK available, which is the one to retry on when the build
/// tooling rejects whatever it was given.
fn fallback_java_home() -> Option<PathBuf> {
    java_installations().into_iter().next()
}

/// Tell Gradle where every installed JDK lives.
///
/// The default JDK is the oldest one, because Gradle support lags each JDK
/// release and broad compatibility matters more than newness. That alone would
/// strand a project whose toolchain genuinely requires a newer JDK, so register
/// all of them: Gradle then resolves a declared `languageVersion` per project
/// instead of everything running on whatever happens to be on PATH.
fn ensure_gradle_java_installations() -> Result<String, String> {
    let installations = java_installations();
    if installations.is_empty() {
        return Ok("no JDK installations announced; leaving toolchain detection alone".to_string());
    }
    let home = gradle_user_home().ok_or("GRADLE_USER_HOME and HOME are both unset")?;
    std::fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    let file = home.join("gradle.properties");
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    if existing.lines().any(|line| {
        line.trim_start()
            .starts_with("org.gradle.java.installations.paths")
    }) {
        return Ok("JDK installations already registered with Gradle".to_string());
    }
    let joined = installations
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("org.gradle.java.installations.paths={joined}\n"));
    std::fs::write(&file, updated).map_err(|error| error.to_string())?;
    Ok(format!(
        "registered JDK installations with Gradle: {joined}"
    ))
}

/// Point Gradle at a specific JDK for every later invocation.
///
/// Goes in `$GRADLE_USER_HOME/gradle.properties`, never the target's own: the
/// repository under review is evidence, and editing it would show up in the
/// run's patch as a change an agent appeared to make. A project that already
/// pins its own `org.gradle.java.home` outranks this and is left alone.
fn pin_gradle_java_home(java_home: &Path) -> Result<String, String> {
    let home = gradle_user_home().ok_or("GRADLE_USER_HOME and HOME are both unset")?;
    std::fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    let file = home.join("gradle.properties");
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim_start().starts_with("org.gradle.java.home"))
    {
        return Ok("a Gradle JDK is already pinned; leaving it alone".to_string());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("org.gradle.java.home={}\n", java_home.display()));
    std::fs::write(&file, updated).map_err(|error| error.to_string())?;
    Ok(format!(
        "build tooling rejected the default JDK; pinned org.gradle.java.home={} for this run",
        java_home.display()
    ))
}

/// One human-facing line, in the same stream and with the same prefix as the
/// run banner's `Build pre-warm:` outcome, plus a machine-readable copy.
fn report(message: &str) {
    eprintln!("Build pre-warm: {message}");
    tracing::info!("Build pre-warm: {message}");
}

fn describe_system(system: &crate::build_systems::BuildSystemSpec, root: &Path) -> String {
    match gradle_wrapper(root) {
        Some(path) if system.name == "Gradle" => format!("Gradle (wrapper {})", path.display()),
        _ => system.name.clone(),
    }
}

/// Turn on Gradle's build cache for every invocation by this user.
///
/// Gradle's build cache is opt-in per invocation unless `org.gradle.caching` is
/// set in a properties file, so a `--build-cache` flag on the pre-warm alone
/// fills a cache that nothing reads. Lane one keeps the pre-warm's local state;
/// the other persistent lanes initially have no `build/` directory, so the
/// shared build cache primes them before each lane retains its own incremental
/// state.
///
/// The property goes in `$GRADLE_USER_HOME/gradle.properties`, never the
/// target's own `gradle.properties`: the target repository is the evidence under
/// analysis, and an edit there would show up in the run's patch and in every
/// agent's view of the code. `GRADLE_USER_HOME` is flawrence-owned, outside the
/// checkout, and shared by the root build and all worktrees.
///
/// Returns the line to log, or an error explaining why nothing was written.
/// Never fails the run.
fn ensure_gradle_build_cache_enabled() -> Result<String, String> {
    let home = gradle_user_home().ok_or("GRADLE_USER_HOME and HOME are both unset")?;
    ensure_gradle_build_cache_enabled_in(&home)
}

/// The file half of [`ensure_gradle_build_cache_enabled`], separated so it can
/// be exercised against a temporary home instead of the operator's own.
fn ensure_gradle_build_cache_enabled_in(home: &Path) -> Result<String, String> {
    let file = home.join("gradle.properties");
    let existing = match std::fs::read_to_string(&file) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", file.display())),
    };
    match gradle_properties_with_caching(existing.as_deref()) {
        None => Ok(format!(
            "{GRADLE_CACHING_KEY} already set in {}",
            file.display()
        )),
        Some(updated) => {
            std::fs::create_dir_all(home)
                .map_err(|error| format!("{}: {error}", home.display()))?;
            std::fs::write(&file, updated)
                .map_err(|error| format!("{}: {error}", file.display()))?;
            Ok(format!(
                "{GRADLE_CACHING_KEY}=true written to {}; agent builds reuse this compile",
                file.display()
            ))
        }
    }
}

/// `GRADLE_USER_HOME`, falling back to the conventional `~/.gradle`.
fn gradle_user_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GRADLE_USER_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".gradle"))
}

/// The contents `gradle.properties` should have, or `None` when it already
/// decides `org.gradle.caching` itself.
///
/// An operator's existing settings — including a deliberate
/// `org.gradle.caching=false` — outrank the harness's preference, and the rest
/// of the file is appended to rather than replaced.
#[must_use]
pub fn gradle_properties_with_caching(existing: Option<&str>) -> Option<String> {
    let existing = existing.unwrap_or_default();
    let already_set = existing.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#')
            && !line.starts_with('!')
            && line
                .split_once(['=', ':'])
                .is_some_and(|(key, _)| key.trim() == GRADLE_CACHING_KEY)
    });
    if already_set {
        return None;
    }
    let mut updated = existing.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!(
        "# Written by the harness pre-warm: makes the build cache filled before the\n\
         # fan-out readable by every agent invocation, which does not pass --build-cache.\n\
         {GRADLE_CACHING_KEY}=true\n"
    ));
    Some(updated)
}

/// The command that compiles main plus test sources, its rendering for the
/// log, and the facts the Gradle probe happened to reveal along the way.
/// The pre-warm command, its rendering for the log, and whatever facts the
/// probe happened to reveal.
///
/// Nothing here branches on which build system it is. The command comes from
/// the system's own declaration -- fixed, or derived by probing what the
/// project offers -- and facts derivation is an optional pass looked up by
/// name, because parsing a task listing or a POM is real per-system logic that
/// no schema can express.
fn build_invocation(
    root: &Path,
    system: &crate::build_systems::BuildSystemSpec,
) -> Option<(Command, String, Option<BuildFacts>)> {
    use crate::build_systems as bs;

    let program = bs::resolve_program(system, root)?;

    let (rendered, probe_output, flavour) = match &system.prewarm_derive {
        Some(derive) => {
            // One probe. Its output decides the command and is handed to facts
            // derivation, so the listing is paid for once for both.
            let listing = run_probe(root, &bs::fill(&derive.probe, &program, ""));
            let available = parse_gradle_task_names(&listing);
            let candidate = bs::choose_candidate(derive, &available)?;
            let rendered = bs::fill(&derive.command, &program, &candidate.tasks.join(" "));
            (rendered, Some(listing), candidate.flavour.clone())
        }
        None => (
            bs::fill(system.prewarm.as_deref()?, &program, ""),
            None,
            None,
        ),
    };

    let mut words = rendered.split_whitespace();
    let mut command = Command::new(words.next()?);
    command.current_dir(root).args(words);

    let facts = derive_facts(
        &system.name,
        root,
        &program,
        probe_output.as_deref(),
        &rendered,
        flavour,
    );
    Some((command, rendered, facts))
}

/// The optional per-system enrichment: modules, test task, source roots.
///
/// Keyed by name rather than by type. A system without an entry here is not
/// unsupported -- it is pre-warmed and named like any other, and its `<build>`
/// block simply carries less.
fn derive_facts(
    name: &str,
    root: &Path,
    program: &str,
    probe_output: Option<&str>,
    rendered: &str,
    flavour: Option<String>,
) -> Option<BuildFacts> {
    match name {
        "Gradle" => {
            let listing = probe_output?;
            let available = parse_gradle_task_names(listing);
            gradle_build_facts(root, program, &available, listing, rendered, flavour)
        }
        "Maven" => maven_build_facts(root, program, rendered),
        _ => None,
    }
}

/// Assemble the facts worth telling the agents, or `None` when the probe did
/// not yield enough to be sure of.
///
/// Bails on an unreadable task list rather than falling back to conventions:
/// the fallback is fine for choosing a compile task (a wrong guess costs one
/// failed pre-warm) but not for a prompt that ~20 agents will act on.
fn gradle_build_facts(
    root: &Path,
    program: &str,
    available: &[String],
    listing: &str,
    verified_command: &str,
    flavour: Option<String>,
) -> Option<BuildFacts> {
    if available.is_empty() {
        return None;
    }
    let test_task = select_test_task(available)?;
    let multiplatform = is_multiplatform(available);

    let mut modules = parse_gradle_module_names(listing);
    // `settings.gradle*` catches modules whose tasks the listing reported
    // unqualified, and costs a file read rather than another Gradle launch.
    for name in ["settings.gradle.kts", "settings.gradle"] {
        if let Ok(contents) = std::fs::read_to_string(root.join(name)) {
            for module in parse_settings_gradle_modules(&contents) {
                if !modules.contains(&module) {
                    modules.push(module);
                }
            }
        }
    }
    modules.sort();

    // A source directory is claimed only where one exists: the root project of
    // a multi-module build usually has no sources of its own, so every module
    // is a candidate before the convention is reported as real.
    let module_dirs: Vec<PathBuf> = std::iter::once(root.to_path_buf())
        .chain(
            modules
                .iter()
                .map(|module| root.join(module.replace(':', "/"))),
        )
        .collect();
    let test_source_dirs = select_test_source_dirs(multiplatform, |candidate| {
        module_dirs.iter().any(|dir| dir.join(candidate).is_dir())
    });

    Some(BuildFacts {
        system: "Gradle".to_string(),
        program: program.to_string(),
        flavour: flavour.unwrap_or_else(|| {
            if multiplatform {
                "Kotlin Multiplatform with a JVM target".to_string()
            } else {
                "plain JVM".to_string()
            }
        }),
        test_target_hint: format!(":<module>:{test_task}"),
        modules,
        test_source_dirs,
        verified_command: verified_command.to_string(),
        toolchain_verified: true,
        commit: None,
        failure_excerpt: None,
    })
}

/// How to invoke Maven, preferring a wrapper the project ships.
///
/// A project with `mvnw` pins its own Maven version; using the ambient `mvn`
/// against it can resolve different plugin versions than the build expects.
#[must_use]
pub fn maven_program(root: &Path) -> String {
    if root.join("mvnw").is_file() {
        "./mvnw".to_string()
    } else {
        "mvn".to_string()
    }
}

/// Module directories declared by a reactor POM, depth-first through nested
/// aggregators.
///
/// Deliberately a scan rather than an XML parse: the only thing needed is the
/// `<module>` entries of `<modules>`, and a dependency-free reader that handles
/// the shape Maven actually writes beats pulling in a parser for one element.
/// Comments are stripped first so a commented-out module is not reported as
/// real.
#[must_use]
pub fn parse_pom_modules(pom: &str) -> Vec<String> {
    let mut without_comments = String::with_capacity(pom.len());
    let mut rest = pom;
    while let Some(start) = rest.find("<!--") {
        without_comments.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    without_comments.push_str(rest);

    let Some(block_start) = without_comments.find("<modules>") else {
        return Vec::new();
    };
    let after = &without_comments[block_start + "<modules>".len()..];
    let block = match after.find("</modules>") {
        Some(end) => &after[..end],
        None => after,
    };

    let mut modules = Vec::new();
    let mut cursor = block;
    while let Some(start) = cursor.find("<module>") {
        let tail = &cursor[start + "<module>".len()..];
        let Some(end) = tail.find("</module>") else {
            break;
        };
        let name = tail[..end].trim();
        if !name.is_empty() {
            modules.push(name.trim_matches('/').to_string());
        }
        cursor = &tail[end..];
    }
    modules
}

/// Every module in the reactor, as paths relative to the root, nested
/// aggregators included.
fn maven_modules(root: &Path) -> Vec<String> {
    fn walk(root: &Path, prefix: &str, out: &mut Vec<String>, depth: usize) {
        // A reactor deep enough to hit this is pathological, and the recursion
        // must terminate even if a POM cycles back on itself.
        if depth > 8 {
            return;
        }
        let pom = if prefix.is_empty() {
            root.join("pom.xml")
        } else {
            root.join(prefix).join("pom.xml")
        };
        let Ok(text) = std::fs::read_to_string(&pom) else {
            return;
        };
        for module in parse_pom_modules(&text) {
            let path = if prefix.is_empty() {
                module.clone()
            } else {
                format!("{prefix}/{module}")
            };
            if out.contains(&path) {
                continue;
            }
            out.push(path.clone());
            walk(root, &path, out, depth + 1);
        }
    }
    let mut modules = Vec::new();
    walk(root, "", &mut modules, 0);
    modules.sort();
    modules
}

/// Whether any POM in the reactor wires the Kotlin compiler, which decides
/// whether reproducers reach for `kotlinc` or `javac`.
fn maven_uses_kotlin(root: &Path, modules: &[String]) -> bool {
    std::iter::once(root.join("pom.xml"))
        .chain(modules.iter().map(|m| root.join(m).join("pom.xml")))
        .filter_map(|pom| std::fs::read_to_string(pom).ok())
        .any(|text| text.contains("kotlin-maven-plugin") || text.contains("kotlin-stdlib"))
}

/// Test source roots Maven projects actually use, kept to those on disk.
fn maven_test_source_dirs(root: &Path, modules: &[String], kotlin: bool) -> Vec<String> {
    let module_dirs: Vec<PathBuf> = std::iter::once(root.to_path_buf())
        .chain(modules.iter().map(|m| root.join(m)))
        .collect();
    let mut candidates = vec!["src/test/java"];
    if kotlin {
        candidates.push("src/test/kotlin");
    }
    candidates
        .into_iter()
        .filter(|candidate| module_dirs.iter().any(|dir| dir.join(candidate).is_dir()))
        .map(str::to_string)
        .collect()
}

/// Assemble Maven's answers to the same questions the Gradle probe answers.
///
/// Everything here comes from the reactor POMs on disk rather than from a
/// second build: Maven re-resolves the whole reactor on every invocation, so a
/// discovery run is the expensive thing the derive-once pipeline exists to
/// avoid.
#[must_use]
pub fn maven_build_facts(root: &Path, program: &str, verified_command: &str) -> Option<BuildFacts> {
    if !root.join("pom.xml").is_file() {
        return None;
    }
    let modules = maven_modules(root);
    let kotlin = maven_uses_kotlin(root, &modules);
    let test_source_dirs = maven_test_source_dirs(root, &modules, kotlin);

    let flavour = match (modules.is_empty(), kotlin) {
        (true, false) => "single-module (JVM)",
        (true, true) => "single-module with Kotlin",
        (false, false) => "multi-module reactor (JVM)",
        (false, true) => "multi-module reactor with Kotlin",
    };

    Some(BuildFacts {
        system: "Maven".to_string(),
        program: program.to_string(),
        flavour: flavour.to_string(),
        // `-pl` selects one module and `-am` builds what it needs; without the
        // latter a module test fails on a sibling the reactor has not built.
        test_target_hint: "-pl <module> -am test".to_string(),
        modules,
        test_source_dirs,
        verified_command: verified_command.to_string(),
        toolchain_verified: true,
        commit: None,
        failure_excerpt: None,
    })
}

/// Kept in step with the mandated agent command in `GRADLE_SYSTEM_PROMPT`.
///
/// The daemon is deliberately left enabled: lane one receives this pre-warm,
/// so its JVM, Kotlin daemon, and project-local incremental state are ready for
/// The Gradle executable as it should appear in the log, so the operator can
/// paste the reported command and get the same build.
/// The project's Gradle wrapper, if it ships one.
fn gradle_wrapper(root: &Path) -> Option<PathBuf> {
    ["gradlew", "gradlew.bat"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
}

/// The Gradle executable as it should appear in the log, so the operator can
struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandOutput {
    /// Both streams together, because Gradle splits task lines and warnings
    /// across them depending on the logger.
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// Run a command, killing it if it outlives `limit`, optionally echoing its
/// output as it arrives.
///
/// A hung build must not hold the whole run hostage, and `std::process` has no
/// timeout of its own. Both pipes are drained by their own thread: a build that
/// fills the pipe buffer would otherwise block until someone reads it, and
/// holding the output back until exit is what made a multi-minute pre-warm look
/// like a frozen harness.
fn run_command(
    mut command: Command,
    limit: Duration,
    forward: bool,
) -> Result<CommandOutput, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().map(|pipe| drain(pipe, forward));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, forward));

    let deadline = Instant::now() + limit;
    let mut status = None;
    let mut failure = None;
    loop {
        match child.try_wait() {
            Ok(Some(code)) => {
                status = Some(code);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Distinguish "the harness killed it" from "the build
                    // failed": a large Kotlin Multiplatform target can need an
                    // hour, and the operator needs to know to raise the cap
                    // rather than go hunting a compile error.
                    failure = Some(format!(
                        "killed by the harness after {}s (raise FLAWRENCE_PREWARM_TIMEOUT_SECS)",
                        limit.as_secs()
                    ));
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    // Joined even on the failure paths, so the reader threads end with the
    // process instead of outliving it.
    let stdout = stdout.map(collect).unwrap_or_default();
    let stderr = stderr.map(collect).unwrap_or_default();
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(CommandOutput {
        success: status.is_some_and(|status| status.success()),
        stdout,
        stderr,
    })
}

/// Read a child pipe to exhaustion on its own thread, forwarding each line.
fn drain<R: Read + Send + 'static>(pipe: R, forward: bool) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut collected = String::new();
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            if forward {
                eprintln!("  [build] {line}");
            }
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    })
}

/// A reader thread's text, or nothing if it panicked — losing log text must not
/// turn into a lost run.
fn collect(handle: std::thread::JoinHandle<String>) -> String {
    handle.join().unwrap_or_default()
}

/// Last non-empty line of a failed build, for a one-line log entry.
fn last_meaningful_line(stderr: &str, stdout: &str) -> String {
    for stream in [stderr, stdout] {
        if let Some(line) = stream.lines().rev().find(|line| !line.trim().is_empty()) {
            return line.trim().chars().take(200).collect();
        }
    }
    "no output".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shipped declarations. Loaded by path rather than through the
    /// global, which resolves `skills` relative to the working directory and so
    /// finds nothing under `cargo test -p`.
    pub(super) fn shipped_specs() -> Vec<crate::build_systems::BuildSystemSpec> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        crate::build_systems::load_specs(&dir).expect("shipped skills parse")
    }

    fn touch(root: &Path, name: &str) {
        std::fs::write(root.join(name), "").unwrap();
    }

    /// Detection order comes from the skills, and the more specific manifest
    /// must win: a project with several markers is built by one of them, and
    /// picking the wrong one sends the pre-warm and every agent at the wrong
    /// tool.
    #[test]
    fn build_systems_are_detected_from_their_own_files() {
        let specs = shipped_specs();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let detected =
            |root: &Path| crate::build_systems::detect(&specs, root).map(|spec| spec.name.clone());

        // Nothing to build is a legitimate state, not a failure.
        assert_eq!(detected(root), None);

        touch(root, "Cargo.toml");
        assert_eq!(detected(root).as_deref(), Some("Cargo"));

        touch(root, "pom.xml");
        assert_eq!(detected(root).as_deref(), Some("Maven"));

        touch(root, "settings.gradle.kts");
        assert_eq!(detected(root).as_deref(), Some("Gradle"));

        // A wrapper does not change which system it is, only how it is invoked.
        touch(root, "gradlew");
        assert_eq!(detected(root).as_deref(), Some("Gradle"));
    }

    /// awesn1's shape, and the case that made a pre-warm silently useless: a
    /// multiplatform build whose modules never call `withJava()`, plus one
    /// JVM-only module (`:benchmarks`) that does contribute `classes`. Choosing
    /// `classes` there compiles the benchmark module and reports BUILD
    /// SUCCESSFUL while warming nothing the agents run.
    #[test]
    fn multiplatform_outranks_a_stray_jvm_module_offering_classes() {
        let mixed = vec![
            "assemble".to_string(),
            "classes".to_string(),
            "testClasses".to_string(),
            "compileKotlinJvm".to_string(),
            "compileTestKotlinJvm".to_string(),
            "jvmTest".to_string(),
            "allTests".to_string(),
        ];
        assert_eq!(
            select_gradle_tasks(&mixed),
            vec!["compileKotlinJvm", "compileTestKotlinJvm"]
        );
    }

    /// `assemble` on a project with Apple or Linux targets compiles native
    /// artifacts no agent executes -- an hour spent to warm nothing. It must
    /// never be reachable once KMP is detected, even with no compile task.
    #[test]
    fn assemble_is_never_chosen_for_a_multiplatform_project() {
        // `jvmTest` alone is enough to identify KMP.
        let sparse = vec!["assemble".to_string(), "jvmTest".to_string()];
        let chosen = select_gradle_tasks(&sparse);
        assert!(
            !chosen.iter().any(|task| task == "assemble"),
            "assemble reachable for KMP: {chosen:?}"
        );
    }

    #[test]
    fn gradle_tasks_are_chosen_from_what_the_project_offers() {
        let jvm = vec!["classes".into(), "testClasses".into(), "jar".into()];
        assert_eq!(select_gradle_tasks(&jvm), vec!["classes", "testClasses"]);

        // KMP: no `classes` task at all.
        let kmp = vec![
            "compileKotlinJvm".into(),
            "compileTestKotlinJvm".into(),
            "linkDebugTestIosX64".into(),
        ];
        assert_eq!(
            select_gradle_tasks(&kmp),
            vec!["compileKotlinJvm", "compileTestKotlinJvm"]
        );

        // Nothing compiling: fall back to whatever produces artefacts.
        assert_eq!(select_gradle_tasks(&["assemble".into()]), vec!["assemble"]);
        // Unreadable task list is not a reason to skip the pre-warm.
        assert_eq!(select_gradle_tasks(&[]), vec!["classes", "testClasses"]);
        // A project offering none of them gets no invocation instead of a wrong one.
        assert!(select_gradle_tasks(&["publish".into()]).is_empty());
    }

    #[test]
    fn task_names_survive_gradle_s_listing_format() {
        let listing = "\
Build tasks
-----------
assemble - Assembles the outputs.
classes - Assembles main classes.
:core:testClasses

Rules
-----
compileKotlinJvm
";
        let names = parse_gradle_task_names(listing);
        assert!(names.contains(&"assemble".to_string()), "{names:?}");
        assert!(names.contains(&"classes".to_string()), "{names:?}");
        assert!(names.contains(&"testClasses".to_string()), "{names:?}");
        assert!(names.contains(&"compileKotlinJvm".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(' ')), "{names:?}");
    }

    #[test]
    fn a_target_without_a_build_system_is_skipped_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            prewarm(&shipped_specs(), dir.path(), true, None),
            PrewarmOutcome::NoBuildSystem
        ));
        assert!(matches!(
            prewarm(&shipped_specs(), dir.path(), false, None),
            PrewarmOutcome::Disabled
        ));
    }

    #[test]
    fn a_real_compile_is_distinguishable_from_an_up_to_date_no_op() {
        let compiled = parse_gradle_work(
            "\
> Task :core:compileKotlin
> Task :core:compileJava NO-SOURCE
> Task :core:classes
> Task :core:compileTestKotlin FROM-CACHE
w: warning: something about > Task :not:a:task
BUILD SUCCESSFUL in 1m 12s
4 actionable tasks: 3 executed, 1 from cache
",
        );
        assert_eq!(compiled.executed, 2);
        assert_eq!(compiled.from_cache, 1);
        assert_eq!(compiled.no_source, 1);
        assert_eq!(
            compiled.result_line.as_deref(),
            Some("BUILD SUCCESSFUL in 1m 12s")
        );
        assert!(!compiled.describe().contains("nothing compiled"));

        // The case the operator could not tell apart: seconds of wall clock and
        // not one task executed.
        let noop = parse_gradle_work(
            "\
> Task :core:compileKotlin UP-TO-DATE
> Task :core:classes UP-TO-DATE
> Task :core:compileTestKotlin UP-TO-DATE
BUILD SUCCESSFUL in 3s
",
        );
        assert_eq!(noop.executed, 0);
        assert_eq!(noop.up_to_date, 3);
        assert!(noop.describe().contains("nothing compiled"), "{noop:?}");

        let quiet = parse_gradle_work("nothing that looks like gradle output\n");
        assert!(!quiet.observed());
    }

    #[test]
    fn the_outcome_line_carries_the_task_counts() {
        let outcome = PrewarmOutcome::Ran {
            system: "Gradle".to_string(),
            command: "gradle classes testClasses".to_string(),
            success: true,
            elapsed: Duration::from_millis(3800),
            detail: String::new(),
            work: Some(parse_gradle_work(
                "> Task :a:classes UP-TO-DATE\nBUILD SUCCESSFUL in 3s\n",
            )),
            failure: None,
            facts: None,
        };
        let line = outcome.describe();
        assert!(line.contains("ok in 3.8s"), "{line}");
        assert!(line.contains("BUILD SUCCESSFUL in 3s"), "{line}");
        assert!(line.contains("nothing compiled"), "{line}");
    }

    #[test]
    fn caching_is_added_without_disturbing_existing_properties() {
        // Absent file: the property is all there is to write.
        let fresh = gradle_properties_with_caching(None).expect("a missing file needs writing");
        assert!(fresh.contains("org.gradle.caching=true"), "{fresh}");

        // Unrelated keys survive, and a file without a trailing newline does not
        // get the property glued onto its last line.
        let merged = gradle_properties_with_caching(Some("org.gradle.jvmargs=-Xmx4g"))
            .expect("a file without the key needs writing");
        assert!(merged.contains("org.gradle.jvmargs=-Xmx4g"), "{merged}");
        assert!(merged.contains("\norg.gradle.caching=true\n"), "{merged}");

        // An operator's own decision is left alone, in either direction.
        assert!(gradle_properties_with_caching(Some("org.gradle.caching=true\n")).is_none());
        assert!(gradle_properties_with_caching(Some("org.gradle.caching = false\n")).is_none());
        assert!(gradle_properties_with_caching(Some("org.gradle.caching:true\n")).is_none());
        // A commented-out key is not a setting.
        assert!(gradle_properties_with_caching(Some("# org.gradle.caching=true\n")).is_some());
        // Nor is a longer key that merely starts the same way.
        assert!(gradle_properties_with_caching(Some("org.gradle.caching.debug=true\n")).is_some());
    }

    #[test]
    fn the_caching_property_lands_in_gradle_user_home_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // A home that does not exist yet, as in a fresh container.
        let home = dir.path().join("gradle-home");
        let file = home.join("gradle.properties");

        let first = ensure_gradle_build_cache_enabled_in(&home).expect("a writable home succeeds");
        assert!(first.contains("written to"), "{first}");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("org.gradle.caching=true")
        );

        let second = ensure_gradle_build_cache_enabled_in(&home).expect("a second pass succeeds");
        assert!(second.contains("already set"), "{second}");
    }

    /// The listing the module list is read from: qualified names carry the
    /// module path, and prose that happens to start with a colon must not.
    #[test]
    fn module_names_come_from_qualified_task_names() {
        let listing = "\
Build tasks
-----------
classes - Assembles main classes.
:kxs:jvmTest - Runs the JVM tests.
:kxs:compileKotlinJvm
:indispensable:asn1:jvmTest - Nested modules keep their path.
:kxs:jvmTest - A repeat contributes nothing.
: not a task at all
:trailing:
";
        assert_eq!(
            parse_gradle_module_names(listing),
            vec!["indispensable:asn1".to_string(), "kxs".to_string()]
        );
    }

    /// Both settings dialects, because a project reporting only unqualified
    /// tasks would otherwise look like it had no modules.
    #[test]
    fn module_names_come_from_both_settings_dialects() {
        let kts = "rootProject.name = \"x\"\ninclude(\":kxs\")\ninclude(\":a:b\", \":c\")\n";
        assert_eq!(
            parse_settings_gradle_modules(kts),
            vec!["a:b".to_string(), "c".to_string(), "kxs".to_string()]
        );

        let groovy = "include ':kxs', ':benchmarks'\n// include ':commented' is still quoted\n";
        let mut found = parse_settings_gradle_modules(groovy);
        found.retain(|module| module != "commented");
        assert_eq!(found, vec!["benchmarks".to_string(), "kxs".to_string()]);
    }

    /// `jvmTest` outranks `test` on a multiplatform project: a KMP `test` task
    /// is an aggregate that may execute nothing on the JVM.
    #[test]
    fn the_test_task_follows_the_reported_task_list() {
        let names = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_string()).collect()
        };

        assert_eq!(
            select_test_task(&names(&["compileKotlinJvm", "jvmTest", "test", "classes"])),
            Some("jvmTest".to_string())
        );
        assert_eq!(
            select_test_task(&names(&["classes", "testClasses", "test"])),
            Some("test".to_string())
        );
        // A KMP project without a `jvm()` target has no JVM test task, and
        // claiming one would send every agent at a task that does not exist.
        assert_eq!(select_test_task(&names(&["compileKotlinNative"])), None);
        assert_eq!(select_test_task(&[]), None);
    }

    /// Conventions are candidates, not facts: only what is on disk is emitted.
    #[test]
    fn test_source_dirs_are_only_the_ones_that_exist() {
        let present = |dir: &str| dir == "src/jvmTest/kotlin" || dir == "src/test/java";

        assert_eq!(
            select_test_source_dirs(true, present),
            vec!["src/jvmTest/kotlin".to_string()]
        );
        assert_eq!(
            select_test_source_dirs(false, present),
            vec!["src/test/java".to_string()]
        );
        assert!(select_test_source_dirs(true, |_| false).is_empty());
    }

    /// The exact text every bash-capable agent is charged for.
    #[test]
    fn the_build_block_states_the_facts_and_forbids_re_probing() {
        let kmp = BuildFacts {
            program: "./gradlew".to_string(),
            system: "Gradle".to_string(),
            flavour: "Kotlin Multiplatform with a JVM target".to_string(),
            test_target_hint: ":<module>:jvmTest".to_string(),
            modules: vec!["indispensable".to_string(), "kxs".to_string()],
            test_source_dirs: vec![
                "src/commonTest/kotlin".to_string(),
                "src/jvmTest/kotlin".to_string(),
            ],
            verified_command: "./gradlew --no-daemon compileKotlinJvm".to_string(),
            toolchain_verified: true,
            commit: None,
            failure_excerpt: None,
        };
        let block = kmp.render();
        assert!(
            block.starts_with("<build>\n") && block.ends_with("</build>"),
            "{block}"
        );
        assert!(block.contains("do NOT run `tasks --all`"), "{block}");
        assert!(
            block.contains("Kotlin Multiplatform with a JVM target"),
            "{block}"
        );
        assert!(block.contains("`:<module>:jvmTest`"), "{block}");
        assert!(block.contains("Mixed modules may differ"), "{block}");
        assert!(block.contains("Modules: indispensable, kxs"), "{block}");
        assert!(
            block.contains("src/commonTest/kotlin, src/jvmTest/kotlin."),
            "{block}"
        );

        let plain = BuildFacts {
            system: "Gradle".to_string(),
            flavour: "plain JVM".to_string(),
            test_target_hint: ":<module>:test".to_string(),
            test_source_dirs: vec!["src/test/kotlin".to_string()],
            ..kmp.clone()
        };
        let block = plain.render();
        assert!(block.contains("plain JVM"), "{block}");
        assert!(block.contains("`:<module>:test`"), "{block}");

        // A module list long enough to be a wall of text is truncated rather
        // than repeated into every agent's context in full.
        let many = BuildFacts {
            modules: (0..MODULE_LIST_LIMIT + 5)
                .map(|n| format!("m{n}"))
                .collect(),
            ..kmp
        };
        assert!(many.render().contains("(+5 more)"), "{}", many.render());
    }

    /// A run that never built still has to say so, in the prompt by omission
    /// and in the audit file in words.
    #[test]
    fn a_pre_warm_that_did_not_run_yields_no_facts_but_still_an_audit_record() {
        // Switched off by the operator: nothing was attempted, and nothing about
        // the target has been learned either way.
        let disabled = PrewarmOutcome::Disabled;
        assert!(disabled.facts().is_none());
        let report = disabled.audit_report();
        assert!(report.contains("No build facts were derived"), "{report}");
        assert!(report.contains("disabled by configuration"), "{report}");
        assert!(!report.contains("<build>"), "{report}");

        // Nothing to build, which is a finding about the target rather than a
        // gap in the record, and reads differently for that reason.
        let none = PrewarmOutcome::NoBuildSystem;
        assert!(none.facts().is_none());
        let report = none.audit_report();
        assert!(report.contains("No build system was detected"), "{report}");
        assert!(report.contains("legitimate state"), "{report}");
        assert!(!report.contains("<build>"), "{report}");

        let failed = PrewarmOutcome::Ran {
            system: "Gradle".to_string(),
            command: "gradle classes".to_string(),
            success: false,
            elapsed: Duration::from_secs(3),
            detail: "compilation error".to_string(),
            work: None,
            failure: None,
            facts: None,
        };
        assert!(failed.facts().is_none());
        let report = failed.audit_report();
        assert!(report.contains("Result: failed"), "{report}");
        assert!(report.contains("compilation error"), "{report}");
        assert!(report.contains("No build facts were derived"), "{report}");
    }

    /// The audit file carries both the run context and the block the agents
    /// were given, so a finished run can be read back.
    #[test]
    fn the_audit_record_carries_the_context_and_the_emitted_block() {
        let outcome = PrewarmOutcome::Ran {
            system: "Gradle".to_string(),
            command: "gradle compileKotlinJvm".to_string(),
            success: true,
            elapsed: Duration::from_secs(42),
            detail: String::new(),
            work: Some(parse_gradle_work(
                "> Task :kxs:compileKotlinJvm\nBUILD SUCCESSFUL in 42s",
            )),
            failure: None,
            facts: Some(Box::new(BuildFacts {
                program: "./gradlew".to_string(),
                system: "Gradle".to_string(),
                flavour: "Kotlin Multiplatform with a JVM target".to_string(),
                test_target_hint: ":<module>:jvmTest".to_string(),
                modules: vec!["kxs".to_string()],
                test_source_dirs: vec!["src/jvmTest/kotlin".to_string()],
                verified_command: "gradle compileKotlinJvm".to_string(),
                toolchain_verified: true,
                commit: None,
                failure_excerpt: None,
            })),
        };
        let report = outcome.audit_report();
        assert!(
            report.contains("Pre-warm command: `gradle compileKotlinJvm`"),
            "{report}"
        );
        assert!(report.contains("Result: success"), "{report}");
        assert!(report.contains("Elapsed: 42.0s"), "{report}");
        assert!(report.contains("BUILD SUCCESSFUL in 42s"), "{report}");
        assert!(report.contains("Modules (1): kxs"), "{report}");
        assert!(report.contains("<build>"), "{report}");
    }

    #[test]
    fn a_failing_build_is_reported_and_not_fatal() {
        // A manifest cargo rejects before it touches the network, so the test
        // exercises the failure path without becoming a real build.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "this is not toml").unwrap();
        match prewarm(&shipped_specs(), dir.path(), true, None) {
            PrewarmOutcome::Ran { success, .. } => assert!(!success),
            other => panic!("expected a reported run, got {}", other.describe()),
        }
    }
}

#[cfg(test)]
mod maven_probe {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn pom(modules: &[&str]) -> String {
        let inner: String = modules
            .iter()
            .map(|m| format!("    <module>{m}</module>\n"))
            .collect();
        format!("<project>\n  <modules>\n{inner}  </modules>\n</project>\n")
    }

    #[test]
    fn modules_are_read_from_the_reactor_pom() {
        assert_eq!(
            parse_pom_modules(&pom(&["core", "api"])),
            vec!["core".to_string(), "api".to_string()]
        );
        assert!(parse_pom_modules("<project></project>").is_empty());
    }

    /// A commented-out module is not part of the reactor. Reporting it sends an
    /// agent to build a module that does not exist.
    #[test]
    fn commented_out_modules_are_not_reported() {
        let text = "<project><modules>\n<module>live</module>\n\
                    <!-- <module>retired</module> -->\n</modules></project>";
        assert_eq!(parse_pom_modules(text), vec!["live".to_string()]);
    }

    #[test]
    fn nested_aggregators_are_walked_depth_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "pom.xml", &pom(&["group"]));
        write(root, "group/pom.xml", &pom(&["leaf-a", "leaf-b"]));
        write(root, "group/leaf-a/pom.xml", "<project/>");
        write(root, "group/leaf-b/pom.xml", "<project/>");

        let facts = maven_build_facts(root, "mvn", "mvn -q -B test-compile").unwrap();
        assert_eq!(
            facts.modules,
            vec![
                "group".to_string(),
                "group/leaf-a".to_string(),
                "group/leaf-b".to_string()
            ]
        );
    }

    /// A POM that lists itself must not spin forever.
    #[test]
    fn a_self_referential_reactor_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "pom.xml", &pom(&["loop"]));
        write(root, "loop/pom.xml", &pom(&["../loop"]));

        let facts = maven_build_facts(root, "mvn", "cmd").unwrap();
        assert!(facts.modules.len() < 12, "{:?}", facts.modules);
    }

    /// Test source roots are claimed only where they exist, and `src/test/kotlin`
    /// only when the reactor actually wires Kotlin.
    #[test]
    fn test_source_dirs_are_claimed_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "pom.xml", &pom(&["core"]));
        write(root, "core/pom.xml", "<project/>");
        write(root, "core/src/test/java/T.java", "class T {}");

        let facts = maven_build_facts(root, "mvn", "cmd").unwrap();
        assert_eq!(facts.test_source_dirs, vec!["src/test/java".to_string()]);
        assert_eq!(facts.flavour, "multi-module reactor (JVM)");
    }

    #[test]
    fn a_kotlin_reactor_claims_the_kotlin_test_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "pom.xml",
            "<project><build><plugins><plugin>\
             <artifactId>kotlin-maven-plugin</artifactId></plugin></plugins></build></project>",
        );
        write(root, "src/test/kotlin/T.kt", "class T");

        let facts = maven_build_facts(root, "mvn", "cmd").unwrap();
        assert_eq!(facts.test_source_dirs, vec!["src/test/kotlin".to_string()]);
        assert_eq!(facts.flavour, "single-module with Kotlin");
    }

    /// `-pl` alone fails on a module whose sibling dependency the reactor has
    /// not built; `-am` is what makes the hint actually run.
    #[test]
    fn the_test_hint_builds_what_the_module_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pom.xml", "<project/>");
        let facts = maven_build_facts(dir.path(), "mvn", "cmd").unwrap();
        assert!(facts.test_target_hint.contains("-pl <module>"), "{facts:?}");
        assert!(facts.test_target_hint.contains("-am"), "{facts:?}");
    }

    /// A project shipping `mvnw` pins its own Maven; the ambient `mvn` can
    /// resolve different plugin versions than the build expects.
    #[test]
    fn a_wrapper_is_preferred_over_the_ambient_maven() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(maven_program(dir.path()), "mvn");
        std::fs::write(dir.path().join("mvnw"), "#!/bin/sh\n").unwrap();
        assert_eq!(maven_program(dir.path()), "./mvnw");
    }

    /// The `<build>` block must speak Maven, not Gradle: an agent told to avoid
    /// `tasks --all` in a Maven workspace learns nothing.
    #[test]
    fn the_rendered_block_speaks_maven() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pom.xml", &pom(&["core"]));
        write(dir.path(), "core/pom.xml", "<project/>");

        let block = maven_build_facts(dir.path(), "mvn", "mvn -q -B test-compile")
            .unwrap()
            .render();

        assert!(block.contains("Build system: Maven"), "{block}");
        assert!(block.contains("-pl <module> -am test"), "{block}");
        assert!(!block.contains("Gradle"), "{block}");
        assert!(!block.contains("tasks --all"), "{block}");
        assert!(block.contains("core"), "{block}");
    }
}

#[cfg(test)]
mod jdk_fallback {
    use super::*;

    /// The exact line bc-java's Gradle wrapper produces on a JDK 26 image:
    /// major version 70 is Java 26, and it fails while Groovy is still parsing
    /// the build script, before a line is compiled.
    #[test]
    fn a_jdk_too_new_for_the_build_tooling_is_recognised() {
        let observed = "BUG! exception in phase 'semantic analysis' in source unit \
                        '_BuildScript_' Unsupported class file major version 70";
        assert!(needs_older_jdk(observed));
        assert!(needs_older_jdk("Unsupported major.minor version 65.0"));
        assert!(needs_older_jdk(
            "Gradle 8.5 no longer supports Java versions below 8."
        ));
    }

    /// This decides whether to silently change the JDK the whole run builds on,
    /// so an ordinary failure must not trigger it.
    #[test]
    fn ordinary_build_failures_do_not_trigger_a_jdk_switch() {
        for benign in [
            "error: cannot find symbol\n  symbol: class Foo",
            "Execution failed for task ':core:compileJava'.",
            "> Could not resolve org.example:missing:1.0",
            "FAILURE: Build failed with an exception.",
            "2 tests completed, 1 failed",
            "",
        ] {
            assert!(!needs_older_jdk(benign), "{benign}");
        }
    }

    /// One test for the whole properties path: `GRADLE_USER_HOME` and
    /// `FLAWRENCE_JAVA_INSTALLATIONS` are process-global, so splitting this in
    /// two would race under the default parallel test threads.
    #[test]
    fn jdk_installations_are_registered_pinned_once_and_never_overridden() {
        let home = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        for jdk in [old.path(), new.path()] {
            std::fs::create_dir_all(jdk.join("bin")).unwrap();
            std::fs::write(jdk.join("bin").join("java"), "").unwrap();
        }
        unsafe {
            std::env::set_var("GRADLE_USER_HOME", home.path());
            std::env::set_var(
                "FLAWRENCE_JAVA_INSTALLATIONS",
                format!("{},{}", old.path().display(), new.path().display()),
            );
        }

        // Registering every JDK is what keeps 21-as-default from stranding a
        // project whose toolchain genuinely needs a newer one: Gradle resolves
        // a declared `languageVersion` per project from these paths.
        assert_eq!(java_installations().len(), 2);
        ensure_gradle_java_installations().expect("a writable home succeeds");
        let written = std::fs::read_to_string(home.path().join("gradle.properties")).unwrap();
        assert!(
            written.contains("org.gradle.java.installations.paths="),
            "{written}"
        );
        assert!(
            written.contains(&new.path().display().to_string()),
            "{written}"
        );

        // Oldest first, so a "too new" failure retries on the JDK most likely
        // to work rather than on another bleeding-edge one.
        assert_eq!(fallback_java_home().as_deref(), Some(old.path()));

        pin_gradle_java_home(old.path()).expect("a writable home succeeds");
        let pinned = std::fs::read_to_string(home.path().join("gradle.properties")).unwrap();
        assert!(
            pinned.contains(&format!("org.gradle.java.home={}", old.path().display())),
            "{pinned}"
        );

        // A project that already chose its JDK outranks the harness: overriding
        // it would build the target on a toolchain its maintainers did not pick.
        let message = pin_gradle_java_home(new.path()).unwrap();
        assert!(message.contains("already pinned"), "{message}");
        let after = std::fs::read_to_string(home.path().join("gradle.properties")).unwrap();
        assert!(
            !after.contains(&format!("org.gradle.java.home={}", new.path().display())),
            "{after}"
        );

        // A path that is not actually a JDK must not be advertised to Gradle.
        unsafe {
            std::env::set_var(
                "FLAWRENCE_JAVA_INSTALLATIONS",
                format!("{},/nonexistent/jdk", old.path().display()),
            );
        }
        assert_eq!(java_installations().len(), 1);

        unsafe {
            std::env::remove_var("FLAWRENCE_JAVA_INSTALLATIONS");
            std::env::remove_var("GRADLE_USER_HOME");
        }
        assert!(fallback_java_home().is_none());
    }
}

#[cfg(test)]
mod partial_builds {
    use super::*;

    /// bc-java cross-compiles for several JDKs and one target fails, while the
    /// modules an analysis needs are already built. Both of these are real
    /// pre-warm lines from the same project; only the second is worthless.
    #[test]
    fn a_build_that_compiled_something_is_not_the_same_as_one_that_never_started() {
        let productive = parse_gradle_work(
            "> Task :core:compileJava\n> Task :core:classes UP-TO-DATE\n\
             BUILD FAILED in 4s",
        );
        assert!(productive.produced_artifacts());

        // The JDK-rejected case: Gradle died parsing the build script.
        let barren = parse_gradle_work(
            "BUG! exception in phase 'semantic analysis' Unsupported class file major version 70\n\
             BUILD FAILED in 495ms",
        );
        assert!(!barren.produced_artifacts());
        assert!(!GradleWork::default().produced_artifacts());
    }

    /// Facts from a partial build must still reach the agents. Dropping them is
    /// what sends every agent to rediscover the build alone.
    #[test]
    fn a_partial_build_still_offers_its_facts_and_says_they_are_unproven() {
        let facts = BuildFacts {
            system: "Gradle".to_string(),
            program: "./gradlew".to_string(),
            flavour: "plain JVM".to_string(),
            test_target_hint: ":<module>:test".to_string(),
            modules: vec!["core".to_string()],
            test_source_dirs: vec!["src/test/java".to_string()],
            verified_command: "./gradlew classes testClasses".to_string(),
            toolchain_verified: false,
            commit: None,
            failure_excerpt: None,
        };
        let block = facts.render();

        // The facts themselves are still there to be used.
        assert!(block.contains("core"), "{block}");
        assert!(block.contains(":<module>:test"), "{block}");
        // But nothing claims the toolchain was proven.
        assert!(!block.contains("Toolchain verified"), "{block}");
        assert!(block.contains("failed"), "{block}");
        // And an agent is told not to go chasing the project's own build break.
        assert!(block.contains("not yours to fix"), "{block}");
    }

    #[test]
    fn a_verified_build_keeps_its_plain_wording() {
        let facts = BuildFacts {
            system: "Maven".to_string(),
            program: "mvn".to_string(),
            flavour: "single-module (JVM)".to_string(),
            test_target_hint: "-pl <module> -am test".to_string(),
            modules: Vec::new(),
            test_source_dirs: Vec::new(),
            verified_command: "mvn -q -B test-compile".to_string(),
            toolchain_verified: true,
            commit: None,
            failure_excerpt: None,
        };
        let block = facts.render();
        assert!(block.contains("Toolchain verified at startup"), "{block}");
        assert!(!block.contains("not yours to fix"), "{block}");
    }
}

#[cfg(test)]
mod failure_reporting {
    use super::*;

    /// "BUILD FAILED in 4s" is what bc-java's pre-warm reported, and it tells an
    /// agent nothing. The lines before it name the module and the reason, which
    /// is what decides whether a reproducer can route around the break.
    #[test]
    fn the_excerpt_keeps_the_end_where_the_reason_is() {
        let mut output = String::new();
        for i in 0..500 {
            output.push_str(&format!("> Task :module{i}:compileJava\n"));
        }
        output.push_str("> Task :jmail:compileJava FAILED\n");
        output.push_str("error: package jakarta.mail does not exist\n");
        output.push_str("BUILD FAILED in 4s\n");

        let excerpt = failure_excerpt(&output);

        assert!(excerpt.contains("jakarta.mail does not exist"), "{excerpt}");
        assert!(excerpt.contains(":jmail:compileJava FAILED"), "{excerpt}");
        assert!(
            excerpt.contains("earlier lines"),
            "must admit what it dropped: {excerpt}"
        );
        // Charged to every bash agent on every task, so it stays small.
        assert!(
            excerpt.chars().count() <= 1_400,
            "{}",
            excerpt.chars().count()
        );
    }

    #[test]
    fn a_short_failure_is_kept_whole_and_blank_lines_dropped() {
        let excerpt = failure_excerpt("\n> Task :core:compileJava FAILED\n\n\nBUILD FAILED\n");
        assert_eq!(excerpt, "> Task :core:compileJava FAILED\nBUILD FAILED");
    }

    /// A partial build now offers its facts; the excerpt is what makes them
    /// actionable, by naming which module broke.
    #[test]
    fn a_partial_build_block_carries_the_reason_it_failed() {
        let facts = BuildFacts {
            system: "Gradle".to_string(),
            program: "./gradlew".to_string(),
            flavour: "plain JVM".to_string(),
            test_target_hint: ":<module>:test".to_string(),
            modules: vec!["core".to_string(), "jmail".to_string()],
            test_source_dirs: vec!["src/test/java".to_string()],
            verified_command: "./gradlew classes testClasses".to_string(),
            toolchain_verified: false,
            commit: None,
            failure_excerpt: Some("> Task :jmail:compileJava FAILED".to_string()),
        };
        let block = facts.render();

        assert!(block.contains(":jmail:compileJava FAILED"), "{block}");
        assert!(block.contains("which modules are affected"), "{block}");
        assert!(block.contains("build-facts.md"), "{block}");
        // A successful build must not carry any of this.
        let ok = BuildFacts {
            toolchain_verified: true,
            failure_excerpt: None,
            ..facts
        };
        assert!(!ok.render().contains("FAILED"), "{}", ok.render());
    }
}

#[cfg(test)]
mod cached_facts {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            crate::workspace_patch::git(root, &args).unwrap();
        }
        std::fs::write(root.join("build.gradle.kts"), "").unwrap();
        crate::workspace_patch::git(root, &["add", "-A"]).unwrap();
        crate::workspace_patch::git(root, &["commit", "-qm", "base"]).unwrap();
        dir
    }

    fn facts() -> BuildFacts {
        BuildFacts {
            system: "Gradle".to_string(),
            program: "./gradlew".to_string(),
            flavour: "plain JVM".to_string(),
            test_target_hint: ":<module>:test".to_string(),
            modules: vec!["core".to_string()],
            test_source_dirs: vec!["src/test/java".to_string()],
            verified_command: "./gradlew classes".to_string(),
            toolchain_verified: true,
            commit: None,
            failure_excerpt: None,
        }
    }

    /// An adopted run skips the pre-warm, so the ordinary report would claim the
    /// pre-warm was "disabled by configuration" -- the opposite of what happened
    /// -- and would overwrite the previous run's report, taking the emitted
    /// `<build>` block with it, while the agents were given exactly that block.
    #[test]
    fn an_adopted_run_records_what_the_agents_were_actually_given() {
        let mut facts = facts();
        facts.commit = Some("abc123def456".to_string());
        let report = adopted_report(&facts);

        // The block agents received, reproduced verbatim.
        assert!(
            report.contains("## Emitted to every bash-capable agent"),
            "{report}"
        );
        assert!(report.contains(&facts.render()), "{report}");
        // Why there was no build, stated as reuse rather than as being disabled.
        assert!(report.contains("reused from a previous run"), "{report}");
        assert!(report.contains("abc123def456"), "{report}");
        assert!(!report.contains("disabled by configuration"), "{report}");
        // And the facts themselves, so the report stands alone.
        assert!(report.contains("core"), "{report}");
        assert!(report.contains(":<module>:test"), "{report}");
    }

    /// The point: a run whose own pre-warm cannot derive facts still gets them,
    /// so forty agents do not each discover the build for themselves.
    #[test]
    fn facts_survive_to_a_later_run_against_the_same_commit() {
        let target = repo();
        let artifacts = tempfile::tempdir().unwrap();

        save_facts(artifacts.path(), target.path(), &facts()).unwrap();
        let loaded = load_cached_facts(artifacts.path(), target.path())
            .expect("the same commit reuses them");

        assert_eq!(loaded.modules, vec!["core".to_string()]);
        assert_eq!(loaded.test_target_hint, ":<module>:test");
        // The commit is recorded at save time, not left to the caller.
        assert!(loaded.commit.is_some());
    }

    /// A wrong build fact is worse than none: it points an agent at a module or
    /// task that may have moved, and the failure reads as the agent's mistake.
    #[test]
    fn facts_from_another_commit_are_refused() {
        let target = repo();
        let artifacts = tempfile::tempdir().unwrap();
        save_facts(artifacts.path(), target.path(), &facts()).unwrap();

        std::fs::write(target.path().join("new.kt"), "fun x() {}").unwrap();
        crate::workspace_patch::git(target.path(), &["add", "-A"]).unwrap();
        crate::workspace_patch::git(target.path(), &["commit", "-qm", "moved on"]).unwrap();

        assert!(load_cached_facts(artifacts.path(), target.path()).is_none());
    }

    /// An unknown commit on either side is not agreement.
    #[test]
    fn facts_without_a_commit_are_never_reused() {
        let target = repo();
        let artifacts = tempfile::tempdir().unwrap();
        std::fs::write(
            artifacts.path().join("build-facts.json"),
            serde_json::to_string(&facts()).unwrap(),
        )
        .unwrap();
        assert!(load_cached_facts(artifacts.path(), target.path()).is_none());

        // And a target that is not a repository cannot be matched either.
        let bare = tempfile::tempdir().unwrap();
        save_facts(artifacts.path(), bare.path(), &facts()).unwrap();
        assert!(load_cached_facts(artifacts.path(), bare.path()).is_none());
    }

    #[test]
    fn a_corrupt_cache_is_ignored_rather_than_fatal() {
        let target = repo();
        let artifacts = tempfile::tempdir().unwrap();
        std::fs::write(artifacts.path().join("build-facts.json"), "{ not json").unwrap();
        assert!(load_cached_facts(artifacts.path(), target.path()).is_none());
    }
}

#[cfg(test)]
mod no_build_system {
    use super::*;

    /// A decompiled artifact or a vendored source drop has nothing to build.
    /// That must read as a fact about the target rather than a failure, or an
    /// agent spends its turns hunting for a build that does not exist.
    #[test]
    fn the_block_lifts_the_reproducer_requirement_and_says_what_replaces_it() {
        let block = no_build_system_block(Path::new("/workspace/decompiled-apk"));

        assert!(
            block.contains("<build>") && block.contains("</build>"),
            "{block}"
        );
        assert!(block.contains("/workspace/decompiled-apk"), "{block}");
        // Not a failure, and not worth retrying.
        assert!(block.contains("not a failure"), "{block}");
        assert!(block.contains("re-probing will not change it"), "{block}");
        assert!(block.contains("do NOT treat this as a blocker"), "{block}");
        // The requirement that cannot be met is lifted, and replaced.
        assert!(
            block.contains("requirement") && block.contains("lifted"),
            "{block}"
        );
        assert!(block.contains("exact file and line"), "{block}");
        assert!(block.contains("no reproducer was possible"), "{block}");
        // A partly-buildable tree is still usable.
        assert!(block.contains("If part of the tree does build"), "{block}");
    }

    /// The operator reads build-facts.md to decide whether to intervene, so the
    /// absence has to be legible there, not only in an agent's prompt.
    #[test]
    fn the_audit_report_says_it_plainly_enough_to_act_on() {
        let report = PrewarmOutcome::NoBuildSystem.audit_report();

        assert!(report.contains("No build system was detected"), "{report}");
        assert!(report.contains("legitimate state"), "{report}");
        assert!(report.contains("the run continues"), "{report}");
        // And tells the operator what would make it wrong.
        assert!(report.contains("worth intervening over"), "{report}");
        assert!(
            report.contains("workspace root is the directory you meant"),
            "{report}"
        );
    }
}
