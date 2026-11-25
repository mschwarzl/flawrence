use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::scheduling::SharedSchedule;
use crate::verifier_lanes::VerifierLanePool;
use crate::worktree::WorktreeManager;
use flawrence_agent::{AgentRegistry, SkillRegistry};
use flawrence_api::ChatClient;
use flawrence_chat::{SharedUsage, UsageByModel};
use flawrence_index::CodebaseIndex;

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub repository_url: Option<String>,
    pub commit_hash: Option<String>,
}

impl GitInfo {
    pub fn resolve(dir: &Path) -> Self {
        let repository_url = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if url.is_empty() {
                    None
                } else {
                    Some(Self::normalize_remote_url(&url))
                }
            });
        let commit_hash = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if hash.is_empty() { None } else { Some(hash) }
            });
        Self {
            repository_url,
            commit_hash,
        }
    }

    fn normalize_remote_url(url: &str) -> String {
        let mut n = url.trim().to_string();
        if n.ends_with(".git") {
            n.truncate(n.len() - 4);
        }
        while n.ends_with('/') {
            n.pop();
        }
        n
    }
}

pub struct RuntimeContext {
    pub client: Arc<dyn ChatClient>,
    pub config: Config,
    /// Sampled subprocess RSS watchdog, absent when disabled.
    pub agent_memory: Option<crate::memory::MemoryLimiter>,
    /// Slots for root agents' direct children; grandchildren bypass this gate.
    pub root_agent_slots: Semaphore,
    pub agents: AgentRegistry,
    pub skills: SkillRegistry,
    /// Root checkout of the target. Also the default workspace: agents that
    /// need no isolation, and every run with isolation switched off, work here.
    pub base_directory: PathBuf,
    /// Hands out per-agent worktrees. Shared by the whole task tree; a
    /// disabled manager simply returns [`Self::base_directory`].
    pub worktrees: Arc<WorktreeManager>,
    /// Warm verifier worktrees, present only for Git-backed targets whose build system declares verifier-lane commands.
    pub verifier_lanes: Option<Arc<VerifierLanePool>>,
    pub schedule: SharedSchedule,
    pub codebase_index: Arc<CodebaseIndex>,
    pub git_info: GitInfo,
    /// What the build pre-warm probed about the target, once it has run.
    ///
    /// A `OnceLock` because the pre-warm happens after this context exists and
    /// is already shared: the alternative is either a lock nothing else needs
    /// or reordering startup so a minutes-long build precedes agent
    /// registration. Unset whenever the pre-warm was disabled, found no build
    /// system, failed, or could not read the facts reliably.
    pub build_facts: OnceLock<crate::prewarm::BuildFacts>,
    /// Build systems declared by the shipped skills, loaded and validated once
    /// at startup. Held here rather than in a global so it is passed explicitly
    /// to everything that detects, routes lanes, or names a system.
    pub build_systems: Vec<crate::build_systems::BuildSystemSpec>,
    pub global_usage: SharedUsage,
    /// Staged Ctrl-C level, shared across the whole task tree. Escalated by the
    /// runner's signal handler and observed by the loop and the task spawner.
    pub shutdown: Arc<crate::shutdown::ShutdownSignal>,
    /// CLI `--arg` overrides applied to every agent's prompt placeholders,
    /// uniformly, in `run_task` (root and spawned children alike). Setting
    /// this once at construction means subagents inherit the same values as
    /// the main agent without per-level forwarding.
    pub arg_overrides: HashMap<String, String>,
}

impl RuntimeContext {
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the existing runtime initialization API"
    )]
    pub async fn new(
        client: Arc<dyn ChatClient>,
        config: Config,
        agents: AgentRegistry,
        skills: SkillRegistry,
        base_directory: PathBuf,
        schedule: SharedSchedule,
        codebase_index: CodebaseIndex,
        arg_overrides: HashMap<String, String>,
    ) -> Result<Arc<Self>> {
        let agent_memory = config
            .agent_memory_mb
            .map(crate::memory::MemoryLimiter::new)
            .transpose()?;
        if let Some(override_cost) = config.token_cost_override {
            flawrence_chat::init_default_pricing(flawrence_chat::ModelPricing {
                input: override_cost.input_per_mtok,
                output: override_cost.output_per_mtok,
                cache_read: override_cost.cache_read_per_mtok.unwrap_or(0.0),
                ..flawrence_chat::ModelPricing::default()
            });
            tracing::info!("Using TOKEN_COST_INPUT and TOKEN_COST_OUTPUT pricing overrides");
        } else {
            match flawrence_api::pricing::fetch_pricing().await {
                Ok(pricing) => {
                    let count = pricing.len();
                    flawrence_chat::init_pricing(pricing);
                    tracing::info!("Loaded pricing for {count} models from models.dev");
                }
                Err(e) if config.max_cost_usd.is_some() => {
                    return Err(e).context("failed to load model pricing required by MAX_COST_USD");
                }
                Err(e) => tracing::warn!("Failed to fetch model pricing: {e}"),
            }
        }
        let git_info = GitInfo::resolve(&base_directory);
        let worktrees = Arc::new(WorktreeManager::new(
            base_directory.clone(),
            config.worktree_isolation,
        ));
        tracing::info!("Worktree isolation: {}", worktrees.describe());
        let build_systems = crate::build_systems::loaded().to_vec();
        // Lanes exist for builds heavy enough to need bounding. A system that
        // declares no lane commands runs its builds directly, which is right
        // for the cheap ones and is why this is the system's own declaration
        // rather than a list here.
        let takes_lanes = crate::build_systems::detect(&build_systems, &base_directory)
            .is_some_and(|spec| !spec.lane_commands.is_empty());
        // Lane setup fails on a target that is not a Git repository or has no
        // commit to detach from. That is a reason to run builds directly in each
        // worktree, not to refuse the whole run, so degrade with a warning the
        // way worktree isolation does rather than propagating the error.
        let verifier_lanes = if takes_lanes {
            match VerifierLanePool::new(&base_directory) {
                Ok(pool) => Some(pool),
                Err(error) => {
                    tracing::warn!(
                        "Verifier lanes unavailable, running builds directly in each worktree ({error:#})"
                    );
                    None
                }
            }
        } else {
            None
        };
        if let Some(lanes) = &verifier_lanes {
            tracing::info!("Verifier lanes: {}", lanes.describe());
        }
        Ok(Arc::new(Self {
            client,
            root_agent_slots: Semaphore::new(config.max_parallel_agents),
            config,
            agent_memory,
            agents,
            skills,
            base_directory,
            worktrees,
            verifier_lanes,
            schedule,
            codebase_index: Arc::new(codebase_index),
            git_info,
            build_facts: OnceLock::new(),
            build_systems,
            global_usage: UsageByModel::new_shared(),
            arg_overrides,
            shutdown: Arc::new(crate::shutdown::ShutdownSignal::new()),
        }))
    }
}
