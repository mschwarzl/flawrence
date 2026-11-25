//! Per-agent `git worktree` isolation.
//!
//! The fan-out puts ~20 agents into one checkout. Agents write reproducers into
//! the target's own source sets, so one syntactically broken file breaks the
//! build for every other agent — and the agents respond by hiding each other's
//! files behind invented suffixes (`.NOTMINE`, `.disabled`, …) and moving them
//! back and forth, which burns a large share of a run's tool calls. Giving each
//! writing agent its own worktree removes the shared-state contention at the
//! cost of one `git worktree add` (~200-500ms) and some disk.
//!
//! Only agents that actually write files get one. Read-only agents keep using
//! the shared root: a worktree they never write to is pure overhead.
//!
//! Worktrees are created lazily, on the first tool call that asks for a
//! workspace, so an agent that never runs a tool never pays for one. The
//! committed content of every worktree is identical to the root's, which is why
//! the shared [`CodebaseIndex`](flawrence_index::CodebaseIndex) stays valid and is
//! never rebuilt per worktree.
//!
//! The isolation wanted is between *siblings* — concurrent agents fighting over
//! one source set — and never between a task and its own descendants, which are
//! collaborating on a single finding: `vulnerability-validation` runs as a child
//! of `vulnerability-research` and is documented to inspect and re-run the
//! reproducer its parent wrote. A worktree is therefore keyed by the root of the
//! write-capable task subtree, not by the individual task, so a child, a
//! grandchild and any deeper descendant all resolve to the ancestor's checkout.
//!
//! Finished subtrees save source changes as a patch before their checkout and
//! ignored build outputs are removed. Failed or incomplete archives keep the
//! checkout for recovery. The shared target checkout is never removed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use flawrence_agent::AgentConfig;

use crate::workspace_patch::{ChangeRecord, capture_worktree_changes, git};

/// Tools whose presence means an agent puts files on disk. `bash` counts: an
/// agent with a shell writes reproducers and runs builds regardless of which
/// dedicated write tools it was granted.
const WRITE_CAPABLE_TOOLS: &[&str] = &["bash", "write_file", "apply_patch"];

/// One agent task's private checkout.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Absolute path of the worktree's root directory.
    pub path: PathBuf,
    /// Agent that owns it.
    pub agent: String,
    /// Task title as shown in the hierarchy, when the task had one.
    pub title: Option<String>,
}

/// Whether an agent definition implies the agent writes into the workspace.
///
/// Decided from the declared tools rather than from a name list, so a new agent
/// gets the right treatment without this file being edited. The read-only
/// analysis agents (`attack-surface`, `threat-model`, `dedup-compare`) declare
/// no write-capable tool and therefore keep the shared root.
#[must_use]
pub fn agent_writes_to_workspace(agent: &AgentConfig) -> bool {
    agent
        .tools
        .iter()
        .any(|tool| WRITE_CAPABLE_TOOLS.contains(&tool.as_str()))
}

/// Directory name for one write-capable task subtree's worktree.
///
/// The task id is part of the name because two tasks can run the same agent
/// concurrently, and they must not share a checkout — that is the very
/// contention this module exists to remove. The id is always the *subtree
/// root's*, so every descendant of that root names the same directory. The
/// agent name is kept in front so a leftover directory can be attributed by eye.
fn worktree_dir_name(agent: &str, task_id: &str) -> String {
    format!("{}-{}", slugify(agent), slugify(task_id))
}

/// Reduce a name to characters that are safe in a path and in a filename on
/// every platform the harness runs on.
pub(crate) fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

/// One task's position in the run's task tree, as far as isolation cares.
///
/// Held by task *id*: the hierarchy the tools see is a list of task titles, and
/// titles are neither unique nor stable enough to identify a task — two research
/// agents can be given the same title, which would collapse two sibling subtrees
/// into one checkout.
#[derive(Debug, Clone)]
struct Lineage {
    parent: Option<String>,
    agent: String,
    /// Whether this task's agent writes files, i.e. whether it can own a
    /// worktree that descendants then inherit.
    writes: bool,
    title: Option<String>,
}

/// Highest write-capable task at or above `task_id`, i.e. the task whose
/// worktree the whole subtree shares.
///
/// Walking to the *topmost* write-capable ancestor rather than the nearest one
/// makes inheritance transitive without recursion: a grandchild lands on the
/// same root as its parent. Read-only tasks in between do not break the chain —
/// they own no checkout, so skipping them keeps a deeper descendant next to the
/// evidence its ancestor produced.
fn subtree_root(lineage: &HashMap<String, Lineage>, task_id: &str) -> Option<String> {
    let mut root = None;
    let mut current = Some(task_id.to_string());
    // A malformed parent chain (a cycle from a reused id) must not hang the run.
    for _ in 0..MAX_LINEAGE_DEPTH {
        let Some(id) = current else { break };
        let Some(entry) = lineage.get(&id) else { break };
        if entry.writes {
            root = Some(id);
        }
        current = entry.parent.clone();
    }
    root
}

/// Cap on how far [`subtree_root`] walks up. Far beyond `max_agent_depth`, so it
/// only ever fires on a corrupt chain.
const MAX_LINEAGE_DEPTH: usize = 64;

/// Hands out the workspace directory each agent task should use.
///
/// Shared by the whole task tree through [`RuntimeContext`](crate::runtime::RuntimeContext);
/// all mutation goes through one lock, so a burst of concurrent first tool calls
/// creates each worktree exactly once.
pub struct WorktreeManager {
    root: PathBuf,
    /// Commit the run was started against. Worktrees are created detached at
    /// this commit so every agent sees the code the index was built from.
    base_commit: Option<String>,
    /// Where worktree directories are placed. Outside the target checkout, so
    /// the target's own `git status`, ignore rules and the codebase index are
    /// unaffected by their presence.
    container: PathBuf,
    enabled: bool,
    /// Worktrees by directory name, i.e. by subtree root.
    state: Mutex<HashMap<String, Worktree>>,
    /// Small index entries for archived worktrees; build outputs are never retained here.
    completed: Mutex<Vec<ChangeRecord>>,
    /// Completion cleanup may outlive an aborted task's blocking future. Serialize
    /// it with the end-of-run sweep so they cannot capture/delete the same tree twice.
    archiving: Mutex<()>,
    /// Task tree as reported by [`WorktreeManager::register_task`], used to
    /// resolve a task to its subtree root.
    lineage: Mutex<HashMap<String, Lineage>>,
}

impl WorktreeManager {
    /// Build a manager for `root`.
    ///
    /// Degrades to the shared root — with a warning, never an error — when
    /// isolation is switched off, when the target is not a git repository, or
    /// when it has no commit to branch from. A target that cannot be worktree'd
    /// is a reason to run less isolated, not to refuse to run.
    #[must_use]
    pub fn new(root: PathBuf, enabled: bool) -> Self {
        let mut manager = Self {
            container: container_for(&root),
            base_commit: None,
            root,
            enabled: false,
            state: Mutex::new(HashMap::new()),
            completed: Mutex::new(Vec::new()),
            archiving: Mutex::new(()),
            lineage: Mutex::new(HashMap::new()),
        };
        if !enabled {
            return manager;
        }
        let is_repository = git(&manager.root, &["rev-parse", "--is-inside-work-tree"])
            .map(|out| out.trim() == "true")
            .unwrap_or(false);
        if !is_repository {
            tracing::warn!(
                "Worktree isolation requested but {} is not a git repository; \
                 all agents will share the checkout",
                manager.root.display()
            );
            return manager;
        }
        match git(&manager.root, &["rev-parse", "HEAD"]) {
            Ok(commit) if !commit.trim().is_empty() => {
                manager.base_commit = Some(commit.trim().to_string());
                manager.enabled = true;
            }
            _ => tracing::warn!(
                "Worktree isolation requested but {} has no commits; \
                 all agents will share the checkout",
                manager.root.display()
            ),
        }
        manager
    }

    /// Whether worktrees are actually being handed out.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The shared checkout every non-isolated agent uses.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Commit worktrees are created at, when isolation is active.
    #[must_use]
    pub fn base_commit(&self) -> Option<&str> {
        self.base_commit.as_deref()
    }

    /// One line for the run-configuration block.
    #[must_use]
    pub fn describe(&self) -> String {
        match (self.enabled, self.base_commit.as_deref()) {
            (true, Some(commit)) => format!(
                "per-agent worktrees at {} from {}",
                self.container.display(),
                &commit[..commit.len().min(12)]
            ),
            _ => "disabled; all agents share the checkout".to_string(),
        }
    }

    /// Announce a task and its parent, before the task can run a tool or spawn a
    /// child.
    ///
    /// Isolation cannot be decided from the task alone: whether a task gets its
    /// own checkout depends on whether an ancestor already owns one, and the
    /// ancestor may not have touched its workspace yet when the child asks.
    /// Registering the whole tree as it is built means the answer never depends
    /// on the order in which tasks happen to make their first tool call.
    pub fn register_task(
        &self,
        task_id: &str,
        parent_task_id: Option<&str>,
        agent: &AgentConfig,
        title: Option<&str>,
    ) {
        if !self.enabled {
            return;
        }
        let mut lineage = self
            .lineage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lineage.insert(
            task_id.to_string(),
            Lineage {
                parent: parent_task_id.map(str::to_string),
                agent: agent.name.clone(),
                writes: agent_writes_to_workspace(agent),
                title: title.map(str::to_string),
            },
        );
    }

    /// Workspace directory for one agent task, creating the subtree's worktree on
    /// first use. Falls back to the shared root whenever a worktree cannot be
    /// created, so a tool call never fails because of isolation.
    ///
    /// A task with a write-capable ancestor gets *that* ancestor's checkout, even
    /// when the task itself only reads: it was spawned to look at what the
    /// ancestor produced, and the shared root does not contain it.
    pub fn workspace_for(
        &self,
        agent: &AgentConfig,
        task_id: &str,
        title: Option<&str>,
    ) -> PathBuf {
        if !self.enabled {
            return self.root.clone();
        }
        let owner = match self.owner_of(agent, task_id, title) {
            Some(owner) => owner,
            // No write-capable task at or above this one: nobody in this branch
            // of the tree needs a private checkout.
            None => return self.root.clone(),
        };
        let key = worktree_dir_name(&owner.agent, &owner.task_id);
        // A poisoned lock means another thread panicked mid-creation. The
        // shared root still works, so keep the run going.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = state.get(&key) {
            return existing.path.clone();
        }
        match self.create(&key) {
            Ok(path) => {
                state.insert(
                    key,
                    Worktree {
                        path: path.clone(),
                        agent: owner.agent.clone(),
                        title: owner.title.clone(),
                    },
                );
                tracing::info!(
                    agent = %owner.agent,
                    path = %path.display(),
                    "Created agent worktree"
                );
                path
            }
            Err(error) => {
                tracing::warn!(
                    agent = %owner.agent,
                    "Could not create a worktree ({error:#}); falling back to the shared checkout"
                );
                self.root.clone()
            }
        }
    }

    /// Task that owns the worktree `task_id` should use, if any.
    ///
    /// An unregistered task is treated as its own root: callers outside the task
    /// loop (and a registration that somehow did not happen) must still get an
    /// isolated checkout rather than silently sharing the target.
    fn owner_of(&self, agent: &AgentConfig, task_id: &str, title: Option<&str>) -> Option<Owner> {
        let lineage = self
            .lineage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match subtree_root(&lineage, task_id) {
            Some(root_id) => {
                let entry = lineage.get(&root_id)?;
                Some(Owner {
                    agent: entry.agent.clone(),
                    title: entry.title.clone(),
                    task_id: root_id,
                })
            }
            None if lineage.contains_key(task_id) => None,
            None if agent_writes_to_workspace(agent) => Some(Owner {
                agent: agent.name.clone(),
                title: title.map(str::to_string),
                task_id: task_id.to_string(),
            }),
            None => None,
        }
    }

    /// Archive and remove the private worktree of a finished subtree.
    ///
    /// Only the *subtree root* releases anything: a child shares its ancestor's
    /// checkout, and removing it when the child finishes would pull the files out
    /// from under the still-running ancestor. Because a parent only completes
    /// after every descendant it spawned has returned, the root's release is the
    /// first moment at which no task can still be using the directory.
    ///
    /// Changed worktrees are retained when no artifacts directory is available.
    ///
    /// # Errors
    ///
    /// Returns an error and keeps the worktree if its changes cannot be archived
    /// completely or removal fails.
    pub fn release_task(&self, task_id: &str, artifacts_dir: Option<&Path>) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let owner = {
            let lineage = self
                .lineage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match subtree_root(&lineage, task_id) {
                Some(root_id) if root_id == task_id => lineage
                    .get(&root_id)
                    .map(|entry| worktree_dir_name(&entry.agent, &root_id)),
                _ => None,
            }
        };
        let Some(key) = owner else { return Ok(()) };

        let worktree = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match state.get(&key) {
                Some(worktree) => worktree.clone(),
                // The subtree never ran a tool that needed a workspace.
                None => return Ok(()),
            }
        };
        if let Some(directory) = artifacts_dir {
            self.archive_and_remove(&worktree, directory)?;
        } else if !has_changes(
            &worktree.path,
            self.base_commit.as_deref().unwrap_or("HEAD"),
        ) && self.remove(&worktree.path)
        {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&key);
        }
        Ok(())
    }

    /// Save a complete source patch before removing an owned, inactive worktree.
    ///
    /// The caller must ensure no task still uses this worktree. Ignored build
    /// outputs are discarded; archived source changes remain in `changes/`.
    ///
    /// # Errors
    ///
    /// Keeps the worktree if capture fails, binary hunks were omitted, the patch
    /// cannot be checked against the source, or Git cannot remove the worktree.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use flawrence_tool::worktree::{Worktree, WorktreeManager};
    /// # fn archive(manager: &WorktreeManager, worktree: &Worktree, artifacts: &std::path::Path) -> anyhow::Result<()> {
    /// manager.archive_and_remove(worktree, artifacts)?;
    /// assert!(!worktree.path.exists());
    /// # Ok(())
    /// # }
    /// ```
    pub fn archive_and_remove(&self, worktree: &Worktree, artifacts_dir: &Path) -> Result<()> {
        let _archive = self
            .archiving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|owned| owned.path == worktree.path)
        {
            ensure!(
                self.completed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .any(|record| record.workspace == worktree.path),
                "Refusing to remove an unowned worktree"
            );
            return Ok(());
        }
        let directory = artifacts_dir
            .canonicalize()
            .context("Cannot resolve artifacts directory")?;
        ensure!(
            !directory.starts_with(worktree.path.canonicalize()?),
            "Artifacts must be outside the worktree being removed"
        );
        let record = capture_worktree_changes(
            &worktree.agent,
            worktree.title.as_deref(),
            &worktree.path,
            &directory,
            self.base_commit
                .as_deref()
                .context("Worktree has no base commit")?,
        )?;
        ensure!(
            !record.binary_omitted,
            "Patch omitted binary changes; keeping worktree"
        );
        if let Some(name) = &record.patch_file {
            let patch = directory.join("changes").join(name);
            git(
                &worktree.path,
                &["apply", "--reverse", "--check", &patch.to_string_lossy()],
            )
            .context("Cannot verify saved patch; keeping worktree")?;
        }
        ensure!(
            self.remove(&worktree.path),
            "Could not remove archived worktree"
        );
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, owned| owned.path != worktree.path);
        tracing::info!(path = %worktree.path.display(), patch = ?record.patch_file, "Archived and removed agent worktree");
        self.completed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
        Ok(())
    }

    /// Index entries for worktrees archived and removed during this invocation.
    #[must_use]
    pub fn completed_changes(&self) -> Vec<ChangeRecord> {
        let _archive = self
            .archiving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.completed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Every worktree handed out so far, sorted by agent for stable reporting.
    #[must_use]
    pub fn worktrees(&self) -> Vec<Worktree> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut all: Vec<Worktree> = state.values().cloned().collect();
        all.sort_by(|a, b| (&a.agent, &a.path).cmp(&(&b.agent, &b.path)));
        all
    }

    /// Detach and delete one worktree whose changes have already been preserved.
    /// Returns whether Git successfully removed it.
    #[must_use]
    pub fn remove(&self, worktree: &Path) -> bool {
        // `--force` because the worktree is intentionally dirty-adjacent: git
        // refuses to remove one with untracked files without it.
        if let Err(error) = git(
            &self.root,
            &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        ) {
            tracing::warn!(
                "Could not remove worktree {}: {error:#}; remove it by hand",
                worktree.display()
            );
            return false;
        }
        // The container is only ours, so an empty one is litter.
        let _ = std::fs::remove_dir(&self.container);
        true
    }

    fn create(&self, dir_name: &str) -> anyhow::Result<PathBuf> {
        let commit = self
            .base_commit
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no base commit"))?;
        std::fs::create_dir_all(&self.container)?;
        let path = self.container.join(dir_name);
        // `--detach` keeps every worktree off a branch: nineteen agents cannot
        // all check out the same branch, and none of them wants one.
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &path.to_string_lossy(),
                commit,
            ],
        )?;
        Ok(path)
    }
}

/// The task a worktree is attributed to: the root of its write-capable subtree.
struct Owner {
    agent: String,
    title: Option<String>,
    task_id: String,
}

/// Whether a worktree holds anything worth keeping.
///
/// `status --porcelain` rather than `diff`, because an agent's reproducer is an
/// untracked file and is exactly the change that must not be pruned. A git that
/// fails here is reported as "changed": keeping a directory costs disk, deleting
/// one that held evidence cannot be undone.
fn has_changes(worktree: &Path, base_commit: &str) -> bool {
    match git(worktree, &["status", "--porcelain", "--", "."]).and_then(|status| {
        Ok(!status.trim().is_empty()
            || git(worktree, &["rev-parse", "HEAD"])?.trim() != base_commit)
    }) {
        Ok(changed) => changed,
        Err(error) => {
            tracing::warn!(
                "Could not check {} for changes ({error:#}); keeping it",
                worktree.display()
            );
            true
        }
    }
}

/// Where the worktrees of a run against `root` live.
///
/// Deliberately outside `root`: inside it, every agent's `grep`, glob and
/// `git status` would see nineteen copies of the codebase. The pid keeps
/// concurrent runners against the same target apart.
fn container_for(root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .map(|name| slugify(&name.to_string_lossy()))
        .unwrap_or_else(|| "target".to_string());
    std::env::temp_dir().join(format!("flawrence-worktrees-{name}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, tools: &[&str]) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            model: "test/model".to_string(),
            description: "test".to_string(),
            prompt: String::new(),
            output_schema: None,
            reasoning_effort: flawrence_chat::ReasoningEffort::Low,
            todos: Vec::new(),
            tool_configs: Default::default(),
            subagents: Vec::new(),
            output_reviewer: None,
            max_iterations: None,
            output_grounding: None,
            params: Default::default(),
        }
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q"]).unwrap();
        std::fs::write(root.join("Main.kt"), "fun main() {}\n").unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(
            root,
            &[
                "-c",
                "user.email=t@localhost",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "initial",
            ],
        )
        .unwrap();
    }

    #[test]
    fn only_write_capable_agents_need_isolation() {
        assert!(agent_writes_to_workspace(&agent(
            "vulnerability-poc",
            &["read_file", "bash", "artifact"]
        )));
        assert!(agent_writes_to_workspace(&agent(
            "threat-model",
            &["read_file", "write_file"]
        )));
        assert!(agent_writes_to_workspace(&agent(
            "patch-implementer",
            &["apply_patch"]
        )));
        // The read-only analysis agents, as they are actually declared.
        assert!(!agent_writes_to_workspace(&agent(
            "attack-surface",
            &[
                "code_index",
                "grep_files",
                "read_file",
                "trailmark",
                "artifact",
                "todo"
            ]
        )));
        assert!(!agent_writes_to_workspace(&agent(
            "threat-model",
            &["code_index", "grep_files", "read_file", "artifact", "todo"]
        )));
        assert!(!agent_writes_to_workspace(&agent(
            "dedup-compare",
            &["submit_result"]
        )));
    }

    #[test]
    fn worktree_names_separate_agents_and_concurrent_tasks() {
        // UUIDv7 starts with a timestamp. Concurrent tasks share that prefix,
        // so truncating from the front collapsed a whole fan-out to one path.
        let a = worktree_dir_name("vulnerability-poc", "01a033a6-1111-7000-8000-000000000001");
        let b = worktree_dir_name("vulnerability-poc", "01a033a6-1111-7000-8000-000000000002");
        assert_ne!(a, b, "two tasks of one agent must not share a checkout");
        assert!(a.starts_with("vulnerability-poc-"), "{a}");
        assert!(a.ends_with("000000000001"), "full task id lost: {a}");
        assert!(
            a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{a} is not path-safe"
        );
        assert_eq!(slugify("ghsa/submit v2"), "ghsa-submit-v2");
        assert_eq!(slugify("///"), "unnamed");
    }

    #[test]
    fn writing_agents_get_their_own_checkout_read_only_ones_do_not() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);

        let manager = WorktreeManager::new(root.clone(), true);
        assert!(manager.is_enabled());

        let writer = manager.workspace_for(&agent("poc", &["bash"]), "task-aaaa1111", Some("PoC"));
        assert_ne!(writer, root);
        assert!(writer.join("Main.kt").exists(), "worktree not checked out");
        // Same task asking again is the same directory, not a second worktree.
        assert_eq!(
            manager.workspace_for(&agent("poc", &["bash"]), "task-aaaa1111", None),
            writer
        );

        let reader = manager.workspace_for(
            &agent("threat-model", &["read_file"]),
            "task-bbbb2222",
            None,
        );
        assert_eq!(reader, root, "read-only agent paid for a worktree");

        assert_eq!(manager.worktrees().len(), 1);
        assert!(manager.remove(&writer));
        assert!(!writer.exists());
    }

    #[test]
    fn a_non_git_target_degrades_to_the_shared_checkout() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        std::fs::write(root.join("Main.kt"), "fun main() {}\n").unwrap();

        let manager = WorktreeManager::new(root.clone(), true);
        assert!(
            !manager.is_enabled(),
            "isolation claimed on a non-repository"
        );
        assert!(manager.describe().contains("share the checkout"));
        assert_eq!(
            manager.workspace_for(&agent("poc", &["bash"]), "task-aaaa1111", None),
            root
        );
        assert!(manager.worktrees().is_empty());
        // Nothing was created next to the target either.
        assert!(!root.join(".git").exists());
    }

    #[test]
    fn isolation_off_means_every_agent_sees_the_root() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);

        let manager = WorktreeManager::new(root.clone(), false);
        assert!(!manager.is_enabled());
        assert_eq!(
            manager.workspace_for(&agent("poc", &["bash", "write_file"]), "t-1", None),
            root
        );
    }

    /// The documented contract: `vulnerability-validation` runs as a child of
    /// `vulnerability-research` and inspects — and re-runs — the reproducer its
    /// parent wrote, so it must see the parent's files.
    #[test]
    fn a_child_shares_its_parents_checkout() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), true);

        let research = agent("vulnerability-research", &["bash", "write_file"]);
        let validation = agent("vulnerability-validation", &["bash", "read_file"]);
        manager.register_task("task-parent", None, &research, Some("research"));
        manager.register_task(
            "task-child",
            Some("task-parent"),
            &validation,
            Some("check"),
        );

        let parent = manager.workspace_for(&research, "task-parent", Some("research"));
        assert_ne!(parent, root);
        // The parent's reproducer has to be visible to the child.
        std::fs::write(parent.join("ReproProbe.kt"), "fun repro() {}\n").unwrap();

        let child = manager.workspace_for(&validation, "task-child", Some("check"));
        assert_eq!(child, parent, "child got its own checkout");
        assert!(child.join("ReproProbe.kt").exists());
        assert_eq!(
            manager.worktrees().len(),
            1,
            "a second worktree was created"
        );
    }

    #[test]
    fn inheritance_is_transitive_down_the_subtree() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), true);

        let research = agent("vulnerability-research", &["bash"]);
        let validation = agent("vulnerability-validation", &["bash"]);
        let poc = agent("vulnerability-poc", &["bash"]);
        // A read-only task in the middle owns no checkout, so it must not break
        // the chain for the write-capable grandchild below it.
        let reader = agent("dedup-compare", &["read_file"]);

        manager.register_task("task-root", None, &research, Some("research"));
        manager.register_task("task-mid", Some("task-root"), &validation, Some("validate"));
        manager.register_task("task-leaf", Some("task-mid"), &poc, Some("poc"));
        manager.register_task("task-ro", Some("task-root"), &reader, Some("dedup"));
        manager.register_task("task-deep", Some("task-ro"), &poc, Some("deep poc"));

        let owner = manager.workspace_for(&research, "task-root", None);
        assert_ne!(owner, root);
        assert_eq!(manager.workspace_for(&poc, "task-leaf", None), owner);
        assert_eq!(manager.workspace_for(&reader, "task-ro", None), owner);
        assert_eq!(manager.workspace_for(&poc, "task-deep", None), owner);
        assert_eq!(manager.worktrees().len(), 1);
    }

    #[test]
    fn siblings_are_still_isolated_from_each_other() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), true);

        let research = agent("vulnerability-research", &["bash"]);
        // Same agent, same title: only the task id keeps these two apart, which
        // is why identity is by id and not by the title hierarchy.
        manager.register_task("task-a", None, &research, Some("research"));
        manager.register_task("task-b", None, &research, Some("research"));

        let a = manager.workspace_for(&research, "task-a", Some("research"));
        let b = manager.workspace_for(&research, "task-b", Some("research"));
        assert_ne!(a, b, "concurrent siblings shared one checkout");
        assert_eq!(manager.worktrees().len(), 2);
    }

    #[test]
    fn a_write_capable_task_without_a_write_capable_ancestor_gets_its_own() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), true);

        let reader = agent("threat-model", &["read_file"]);
        let writer = agent("vulnerability-poc", &["bash"]);
        manager.register_task("task-reader", None, &reader, Some("threats"));
        manager.register_task("task-writer", Some("task-reader"), &writer, Some("poc"));

        // The read-only root has nothing to inherit and nothing to hand down.
        assert_eq!(manager.workspace_for(&reader, "task-reader", None), root);
        let own = manager.workspace_for(&writer, "task-writer", None);
        assert_ne!(own, root);
        assert_eq!(manager.worktrees().len(), 1);
    }

    #[test]
    fn inheritance_respects_the_feature_flag_being_off() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), false);

        let research = agent("vulnerability-research", &["bash"]);
        let validation = agent("vulnerability-validation", &["bash"]);
        manager.register_task("task-parent", None, &research, Some("research"));
        manager.register_task("task-child", Some("task-parent"), &validation, None);

        assert_eq!(manager.workspace_for(&research, "task-parent", None), root);
        assert_eq!(manager.workspace_for(&validation, "task-child", None), root);
        assert!(manager.worktrees().is_empty());
    }

    #[test]
    fn a_finished_subtree_prunes_only_a_checkout_it_left_empty() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root.clone(), true);

        let research = agent("vulnerability-research", &["bash"]);
        let child = agent("vulnerability-validation", &["bash"]);
        manager.register_task("task-idle", None, &research, Some("nothing found"));
        manager.register_task("task-busy", None, &research, Some("found something"));
        manager.register_task("task-kid", Some("task-busy"), &child, Some("validate"));

        let idle = manager.workspace_for(&research, "task-idle", None);
        let busy = manager.workspace_for(&research, "task-busy", None);
        std::fs::write(busy.join("ReproProbe.kt"), "fun repro() {}\n").unwrap();

        // A child finishing must not take the checkout its parent is still using.
        manager.release_task("task-kid", None).unwrap();
        assert!(busy.exists(), "a child pruned its parent's worktree");

        manager.release_task("task-idle", None).unwrap();
        assert!(!idle.exists(), "an empty worktree was kept");

        manager.release_task("task-busy", None).unwrap();
        assert!(
            busy.join("ReproProbe.kt").exists(),
            "the evidence behind a finding was pruned"
        );

        // Only the surviving one is still reported, so the end-of-run patch pass
        // never diffs a directory that is gone.
        let left = manager.worktrees();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, busy);
        assert_eq!(left[0].title.as_deref(), Some("found something"));
        // This in-memory-only run has no archive destination, so explicitly clean it up.
        assert!(manager.remove(&busy));
    }

    #[test]
    fn finished_subtree_archives_reproducers_and_removes_ignored_build_outputs() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        std::fs::write(root.join(".gitignore"), "target/\n.env\n").unwrap();
        git(&root, &["add", ".gitignore"]).unwrap();
        git(
            &root,
            &[
                "-c",
                "user.email=t@localhost",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "ignore builds",
            ],
        )
        .unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let manager = WorktreeManager::new(root.clone(), true);
        let research = agent("vulnerability-research", &["bash"]);
        manager.register_task("parent", None, &research, Some("parser"));
        manager.register_task("validator", Some("parent"), &research, Some("verify"));
        manager.register_task("sibling", None, &research, Some("other row"));
        let worktree = manager.workspace_for(&research, "parent", None);
        let sibling = manager.workspace_for(&research, "sibling", None);
        std::fs::write(worktree.join("Main.kt"), "fun main() { repro() }\n").unwrap();
        git(&worktree, &["add", "Main.kt"]).unwrap();
        git(
            &worktree,
            &[
                "-c",
                "user.email=t@localhost",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "agent edit",
            ],
        )
        .unwrap();
        std::fs::write(worktree.join("Repro.kt"), "fun repro() {}\n").unwrap();
        std::fs::write(worktree.join("fixture.bin"), b"\0\x01\xff").unwrap();
        std::fs::create_dir_all(worktree.join(".cargo")).unwrap();
        std::fs::write(worktree.join(".cargo/config.toml"), "[build]\njobs = 1\n").unwrap();
        std::fs::create_dir_all(worktree.join("target/debug/deps")).unwrap();
        std::fs::write(
            worktree.join("target/debug/deps/build-cache"),
            "discard cache",
        )
        .unwrap();
        std::fs::write(worktree.join(".env"), "IGNORED_SECRET=1\n").unwrap();

        manager
            .release_task("validator", Some(artifacts.path()))
            .unwrap();
        assert!(worktree.join("target/debug/deps/build-cache").exists());
        manager
            .release_task("parent", Some(artifacts.path()))
            .unwrap();
        assert!(
            !worktree.exists(),
            "finished source and build tree survived"
        );
        assert!(sibling.exists(), "an active sibling was removed");
        let records = manager.completed_changes();
        assert_eq!(records.len(), 1);
        let patch = artifacts
            .path()
            .join("changes")
            .join(records[0].patch_file.as_ref().unwrap());
        let patch_text = std::fs::read_to_string(&patch).unwrap();
        assert!(!patch_text.contains("discard cache"));
        assert!(!patch_text.contains("IGNORED_SECRET"));

        // The saved patch is sufficient to recover the actual reproducers in a clean checkout.
        let restored_parent = tempfile::tempdir().unwrap();
        let restored = restored_parent.path().join("restored");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &restored.to_string_lossy(),
                "HEAD",
            ],
        )
        .unwrap();
        git(&restored, &["apply", &patch.to_string_lossy()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(restored.join("Main.kt")).unwrap(),
            "fun main() { repro() }\n"
        );
        assert_eq!(
            std::fs::read_to_string(restored.join("Repro.kt")).unwrap(),
            "fun repro() {}\n"
        );
        assert_eq!(
            std::fs::read(restored.join("fixture.bin")).unwrap(),
            b"\0\x01\xff"
        );
        assert!(restored.join(".cargo/config.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(root.join("Main.kt")).unwrap(),
            "fun main() {}\n"
        );
        let index =
            crate::workspace_patch::write_change_index(artifacts.path(), &records, "test").unwrap();
        assert!(
            std::fs::read_to_string(index)
                .unwrap()
                .contains(records[0].patch_file.as_ref().unwrap())
        );
        manager
            .release_task("sibling", Some(artifacts.path()))
            .unwrap();
        git(
            &root,
            &["worktree", "remove", "--force", &restored.to_string_lossy()],
        )
        .unwrap();
        assert!(manager.worktrees().is_empty());
    }

    #[test]
    fn failed_archive_keeps_source_and_build_cache_until_it_can_be_retried() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root, true);
        let research = agent("research", &["bash"]);
        manager.register_task("parent", None, &research, None);
        let worktree = manager.workspace_for(&research, "parent", None);
        std::fs::write(worktree.join("Repro.kt"), "fun repro() {}\n").unwrap();
        std::fs::write(worktree.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
        std::fs::write(worktree.join("target/debug/build-cache"), "cache").unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let blocked = artifacts.path().join("not-a-directory");
        std::fs::write(&blocked, "blocked").unwrap();
        assert!(manager.release_task("parent", Some(&blocked)).is_err());
        assert!(worktree.join("Repro.kt").is_file());
        assert!(worktree.join("target/debug/build-cache").is_file());
        assert_eq!(manager.worktrees().len(), 1);
        assert!(manager.completed_changes().is_empty());
        // An archive inside the tree would itself be deleted by cleanup.
        assert!(manager.release_task("parent", Some(&worktree)).is_err());
        // Normal completion and a shutdown sweep may attempt the same archive concurrently.
        let owned = manager.worktrees().pop().unwrap();
        std::thread::scope(|threads| {
            let first = threads.spawn(|| manager.archive_and_remove(&owned, artifacts.path()));
            let second = threads.spawn(|| manager.archive_and_remove(&owned, artifacts.path()));
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });
        assert!(!worktree.exists());
        assert_eq!(manager.completed_changes().len(), 1);
    }

    #[test]
    fn a_patch_that_does_not_match_the_source_prevents_worktree_deletion() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let manager = WorktreeManager::new(root, true);
        let research = agent("research", &["bash"]);
        manager.register_task("parent", None, &research, None);
        let worktree = manager.workspace_for(&research, "parent", None);
        // Text patches currently use UTF-8; lossy conversion must never cost the original bytes.
        std::fs::write(worktree.join("Main.kt"), b"invalid text \xff\n").unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        assert!(
            manager
                .release_task("parent", Some(artifacts.path()))
                .is_err()
        );
        assert_eq!(
            std::fs::read(worktree.join("Main.kt")).unwrap(),
            b"invalid text \xff\n"
        );
        std::fs::write(worktree.join("Main.kt"), "valid replacement\n").unwrap();
        manager
            .release_task("parent", Some(artifacts.path()))
            .unwrap();
        assert!(!worktree.exists());
    }
}
