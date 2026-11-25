//! Capture everything a run changed in the workspace as an applicable patch.
//!
//! Runs are expected to modify the target: agents write PoCs, probe tests and
//! occasionally edit sources. Those edits are evidence, but they live only in a
//! working tree that is reset before the next run. This records them as a
//! `git`-based patch, independently of which agent made them or how.
//!
//! The workspace is left exactly as it was found: nothing is committed, `HEAD`
//! never moves, and no `reset --hard` or `clean` is ever issued. A repository
//! initialised here for lack of one is removed again afterwards.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Beyond this, the patch is written without binary hunks: a run that compiles
/// jars into the workspace can otherwise produce hundreds of megabytes.
const MAX_BINARY_PATCH_BYTES: usize = 64 * 1024 * 1024;

/// Where the patch lands inside the artifacts directory.
const PATCH_DIR: &str = "changes";

/// A short id identifying one harness invocation, used to prefix the two
/// patch-directory files whose names are otherwise fixed.
///
/// Artifacts are already session-prefixed, so pointing two runs at one
/// artifacts directory keeps both sets. `changes/` was the exception:
/// `workspace.patch` and `index.md` are written once per invocation under
/// fixed names, so a second run silently overwrote the first run's
/// shared-checkout patch and the index explaining which agent produced what --
/// no error, and the evidence for the earlier run simply gone.
#[must_use]
pub fn new_patch_session() -> String {
    format!("{:08x}", rand::random::<u32>())
}

pub struct WorkspaceTracker {
    root: PathBuf,
    /// True when this tracker ran `git init` because the workspace had no
    /// repository — the only case in which it may remove a `.git` directory.
    created_repository: bool,
    /// Modifications already present when the run started. They cannot be
    /// separated from the run's own changes, so the patch notes them.
    started_dirty: bool,
    /// Paths already staged at start. The final `git reset` unstages them, so
    /// this is reported rather than silently changed.
    started_staged: bool,
    /// Tree object recording the workspace as found, when it was already dirty.
    ///
    /// The run leaves the workspace untouched by design — an agent's PoC is the
    /// evidence behind its finding, and this may be a checkout with the
    /// operator's own uncommitted work in it. That means a second run starts
    /// from the first run's leftovers, and a patch taken against `HEAD` would
    /// report both runs' changes as this run's. Diffing against a snapshot of
    /// the starting state instead keeps each patch to what its own run did,
    /// without deleting anything to achieve it.
    baseline_tree: Option<String>,
    head: Option<String>,
}

pub struct PatchOutcome {
    pub patch_path: PathBuf,
    pub files_changed: usize,
    /// Paths the patch touches, for the change index.
    pub files: Vec<String>,
    pub bytes: usize,
    pub binary_omitted: bool,
    /// Paths left changed in the workspace once the run finished. Non-zero is
    /// the normal outcome: the run records its changes and leaves them.
    pub left_dirty: usize,
}

impl WorkspaceTracker {
    /// Record the workspace's starting state, initialising a repository if the
    /// target is not one. Returns `Ok(None)` when git is unusable, so a missing
    /// `git` never fails a run.
    pub fn start(root: &Path) -> Result<Option<Self>> {
        if git(root, &["--version"]).is_err() {
            return Ok(None);
        }
        let root = root
            .canonicalize()
            .with_context(|| format!("failed to resolve workspace {}", root.display()))?;

        let is_repository = git(&root, &["rev-parse", "--is-inside-work-tree"])
            .map(|out| out.trim() == "true")
            .unwrap_or(false);

        let created_repository = if is_repository {
            false
        } else {
            git(&root, &["init", "-q"]).context("failed to initialise change tracking")?;
            stage_without_dotfiles(&root)?;
            git(
                &root,
                &[
                    "-c",
                    "user.email=flawrence@localhost",
                    "-c",
                    "user.name=flawrence",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "harness baseline",
                ],
            )
            .context("failed to record change-tracking baseline")?;
            true
        };

        // `git diff` lists only tracked modifications; a previous run's PoC
        // directories are untracked, so the case most likely to matter was the
        // one this check could not see. `status --porcelain` sees both and still
        // honours the repository's own ignore rules.
        let started_dirty = !git(&root, &["status", "--porcelain", "--", "."])
            .unwrap_or_default()
            .trim()
            .is_empty();
        let baseline_tree = if started_dirty {
            snapshot_tree(&root)
        } else {
            None
        };

        Ok(Some(Self {
            started_dirty,
            baseline_tree,
            started_staged: !git(&root, &["diff", "--cached", "--name-only", "--", "."])
                .unwrap_or_default()
                .trim()
                .is_empty(),
            head: git(&root, &["rev-parse", "HEAD"])
                .ok()
                .map(|out| out.trim().to_string()),
            root,
            created_repository,
        }))
    }

    /// One line for the run-configuration block.
    pub fn describe(&self) -> String {
        let mut note = if self.created_repository {
            "not a git repository; initialised temporarily to record changes".to_string()
        } else {
            match &self.head {
                Some(head) => format!("git repository at {}", &head[..head.len().min(12)]),
                None => "git repository with no commits yet".to_string(),
            }
        };
        if self.started_dirty {
            note.push_str(match self.baseline_tree {
                Some(_) => "; already had local changes at start, which the patch excludes",
                None => "; had uncommitted changes at start, which the patch also contains",
            });
        }
        if self.started_staged {
            note.push_str("; had staged changes at start, which are left unstaged");
        }
        note.push_str("; changes are recorded as a patch and left in place, never reverted");
        note
    }

    /// Whether the workspace already had changes when the run started.
    #[must_use]
    pub fn started_dirty(&self) -> bool {
        self.started_dirty
    }

    /// Paths still changed relative to `HEAD`, as the run leaves them.
    fn changed_paths(root: &Path) -> usize {
        git(root, &["status", "--porcelain", "--", "."])
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    }

    /// Write the patch, then restore the workspace.
    pub fn finish(self, artifacts_dir: &Path, session: &str) -> Result<PatchOutcome> {
        let diff = collect_diff(&self.root, self.baseline_tree.as_deref())?;
        let Diff {
            stat,
            patch,
            files,
            binary_omitted,
        } = diff;
        let files_changed = files.len();

        // Restore first: the workspace must end up untouched even if writing
        // the artifact fails.
        let _ = git(&self.root, &["reset", "-q", "--", "."]);
        if self.created_repository {
            let repository = self.root.join(".git");
            if repository.is_dir() {
                std::fs::remove_dir_all(&repository).with_context(|| {
                    format!("failed to remove {} after tracking", repository.display())
                })?;
            }
        }

        let directory = artifacts_dir.join(PATCH_DIR);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;

        // An empty .patch file reads as a failure. When nothing changed, say so
        // in prose instead and write no patch at all.
        if files_changed == 0 {
            let note_path = directory.join("workspace-unchanged.md");
            std::fs::write(
                &note_path,
                format!(
                    "# Workspace unchanged\n\nThis run made no changes to the workspace \
                     (dotfiles and dot-directories excluded). No patch was produced.\n\n\
                     Workspace: {}\n",
                    self.describe()
                ),
            )
            .with_context(|| format!("failed to write {}", note_path.display()))?;
            return Ok(PatchOutcome {
                patch_path: note_path,
                files_changed: 0,
                files: Vec::new(),
                bytes: 0,
                binary_omitted: false,
                left_dirty: Self::changed_paths(&self.root),
            });
        }

        let file_name = &format!("{session}_workspace.patch");
        let patch_path = directory.join(file_name);
        write_patch_file(
            &patch_path,
            file_name,
            &self.describe(),
            &stat,
            &patch,
            binary_omitted,
        )?;

        Ok(PatchOutcome {
            patch_path,
            files_changed,
            files,
            bytes: patch.len(),
            binary_omitted,
            left_dirty: Self::changed_paths(&self.root),
        })
    }
}

/// One workspace's contribution to a run's changes, as listed in the index.
///
/// Patches are kept separate on purpose. Agents legitimately create, move and
/// delete files in the same paths, so merging their patches would produce a
/// diff that no agent ever produced and that describes no state the run was
/// ever in.
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    /// Agent that owned the workspace, or [`SHARED_ROOT_LABEL`] for the
    /// checkout that non-isolated agents share.
    pub agent: String,
    /// Title of the task that owned the workspace, when it had one.
    pub task_title: Option<String>,
    /// Directory the changes were captured from.
    pub workspace: PathBuf,
    /// Patch file name inside `changes/`, or `None` when nothing changed.
    pub patch_file: Option<String>,
    /// Paths the patch touches, relative to the workspace root.
    pub files: Vec<String>,
    pub bytes: usize,
    pub binary_omitted: bool,
}

/// Label used for the checkout shared by every agent that has no worktree.
pub const SHARED_ROOT_LABEL: &str = "shared-checkout";

impl ChangeRecord {
    /// Describe the shared checkout's own changes from its tracker outcome.
    #[must_use]
    pub fn for_shared_root(workspace: PathBuf, outcome: &PatchOutcome) -> Self {
        Self {
            agent: SHARED_ROOT_LABEL.to_string(),
            task_title: None,
            workspace,
            patch_file: (outcome.files_changed > 0).then(|| file_name_of(&outcome.patch_path)),
            files: outcome.files.clone(),
            bytes: outcome.bytes,
            binary_omitted: outcome.binary_omitted,
        }
    }

    /// Whether this workspace changed anything at all.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.files.is_empty()
    }
}

/// Capture one agent worktree's changes as its own patch in `changes/`.
///
/// `base_commit` is the commit the worktree started from, including when an
/// agent subsequently commits its edits. Untracked files are staged
/// first, because an agent's reproducer is untracked and is exactly the change
/// worth keeping. Unlike the shared-checkout snapshot, this includes non-ignored
/// dotfiles: a private worktree may be removed after capture, so its source
/// changes must be complete. Ignored build outputs are excluded.
pub fn capture_worktree_changes(
    agent: &str,
    task_title: Option<&str>,
    worktree: &Path,
    artifacts_dir: &Path,
    base_commit: &str,
) -> Result<ChangeRecord> {
    git(worktree, &["add", "-A", "--", "."]).context("failed to stage agent changes")?;
    let diff = collect_staged_diff(worktree, Some(base_commit));
    // Restore the index even if reading the diff fails.
    let _ = git(worktree, &["reset", "-q", "--", "."]);
    let Diff {
        stat,
        patch,
        files,
        binary_omitted,
    } = diff?;

    let mut record = ChangeRecord {
        agent: agent.to_string(),
        task_title: task_title.map(str::to_string),
        workspace: worktree.to_path_buf(),
        patch_file: None,
        files,
        bytes: patch.len(),
        binary_omitted,
    };
    if !record.changed() {
        record.bytes = 0;
        return Ok(record);
    }

    let directory = artifacts_dir.join(PATCH_DIR);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    // Two sibling subtrees of the SAME agent are the explicit case this
    // isolation exists for, so the agent name alone cannot name their patches:
    // one would silently overwrite the other while the index listed both as
    // separate work. The worktree directory is already unique per subtree
    // (agent slug plus root task id), so reuse it and the patch name stays
    // traceable back to the checkout it came from.
    let file_name = worktree
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map_or_else(
            || format!("agent-{}.patch", crate::worktree::slugify(agent)),
            |directory| format!("agent-{directory}.patch"),
        );
    write_patch_file(
        &directory.join(&file_name),
        &file_name,
        &format!("changes made by agent '{agent}' in its own worktree (base: {base_commit})"),
        &stat,
        &patch,
        binary_omitted,
    )?;
    record.patch_file = Some(file_name);
    Ok(record)
}

/// Write `changes/index.md`: which agent produced which patch.
///
/// A run fans out to ~20 agents and their patches conflict by design, so the
/// list of patch files alone does not say who did what. This is the map from
/// patch back to the agent, task and files it came from.
pub fn write_change_index(
    artifacts_dir: &Path,
    records: &[ChangeRecord],
    session: &str,
) -> Result<PathBuf> {
    let directory = artifacts_dir.join(PATCH_DIR);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let path = directory.join(format!("{session}_index.md"));

    let mut body = String::from("# Workspace changes by agent\n\n");
    body.push_str(
        "Each agent that writes files works in its own git worktree, so patches are per \
         agent and are *not* merged: agents create, move and delete the same paths, and a \
         merged diff would describe a state that never existed. Apply the one you want with \
         `git apply <patch>` from the target checkout. Completed isolated worktrees and their \
         ignored build outputs are removed after capture; workspace paths below record where \
         the changes originated. The patch header identifies the base commit.\n\n",
    );

    let changed: Vec<&ChangeRecord> = records.iter().filter(|r| r.changed()).collect();
    if changed.is_empty() {
        body.push_str("No workspace changed during this run.\n");
    } else {
        body.push_str("| Agent | Task | Patch | Files |\n|---|---|---|---|\n");
        for record in &changed {
            body.push_str(&format!(
                "| `{}` | {} | `{}` | {} |\n",
                record.agent,
                record.task_title.as_deref().unwrap_or("—"),
                record.patch_file.as_deref().unwrap_or("—"),
                record.files.len()
            ));
        }
        for record in &changed {
            body.push_str(&format!("\n## {}\n\n", record.agent));
            if let Some(title) = &record.task_title {
                body.push_str(&format!("- Task: {title}\n"));
            }
            body.push_str(&format!("- Workspace: `{}`\n", record.workspace.display()));
            if let Some(patch) = &record.patch_file {
                body.push_str(&format!("- Patch: `{patch}` ({} bytes)\n", record.bytes));
                body.push_str(&format!("- Apply with: `git apply {patch}`\n"));
            }
            if record.binary_omitted {
                body.push_str("- Binary hunks omitted: the patch exceeded the size limit\n");
            }
            body.push_str("- Files touched:\n");
            for file in &record.files {
                body.push_str(&format!("  - `{file}`\n"));
            }
        }
    }

    let unchanged: Vec<&ChangeRecord> = records.iter().filter(|r| !r.changed()).collect();
    if !unchanged.is_empty() {
        body.push_str("\n## Workspaces with no changes\n\n");
        for record in unchanged {
            body.push_str(&format!("- `{}`\n", record.agent));
        }
    }

    std::fs::write(&path, body.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// The staged difference of a workspace, in the forms the patch file needs.
struct Diff {
    stat: String,
    patch: String,
    files: Vec<String>,
    binary_omitted: bool,
}

/// Stage everything but dotfiles, then read the difference against `base`
/// (`HEAD` when `None`) as stat, patch and file list.
fn collect_diff(root: &Path, base: Option<&str>) -> Result<Diff> {
    stage_without_dotfiles(root)?;
    collect_staged_diff(root, base)
}

fn collect_staged_diff(root: &Path, base: Option<&str>) -> Result<Diff> {
    let stat = git(root, &diff_args(&["--stat"], base))?;
    let patch = git(root, &diff_args(&["--binary", "--no-color"], base))?;
    let binary_omitted = patch.len() > MAX_BINARY_PATCH_BYTES;
    let patch = if binary_omitted {
        git(root, &diff_args(&["--no-color"], base))?
    } else {
        patch
    };
    let files = git(root, &diff_args(&["--name-only"], base))?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    Ok(Diff {
        stat,
        patch,
        files,
        binary_omitted,
    })
}

/// Write a patch with the header that makes it usable on its own: what it is,
/// how to apply it, and the stat block.
fn write_patch_file(
    path: &Path,
    file_name: &str,
    description: &str,
    stat: &str,
    patch: &str,
    binary_omitted: bool,
) -> Result<()> {
    let mut body = String::new();
    body.push_str(&format!("# {description}\n"));
    if binary_omitted {
        body.push_str(
            "# Binary hunks omitted: the full patch exceeded the size limit, so binary \
             files appear as 'Binary files differ' rather than as content.\n",
        );
    }
    body.push_str(&format!("# Apply with: git apply {file_name}\n#\n"));
    for line in stat.lines() {
        body.push_str(&format!("# {line}\n"));
    }
    body.push('\n');
    body.push_str(patch);
    std::fs::write(path, body.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Stage every change except dotfiles and dot-directories.
///
/// Ignore rules cannot express this: they do not apply to already-tracked
/// files, so an edited `.gitignore` or `.github/workflows/*` would still be
/// staged. Staging everything and then unstaging the dot paths is explicit and
/// does not depend on git's pathspec globbing.
fn stage_without_dotfiles(root: &Path) -> Result<()> {
    git(root, &["add", "-A", "--", "."]).context("failed to stage workspace changes")?;

    let staged = git(
        root,
        &["diff", "--cached", "--name-only", "--relative", "--", "."],
    )
    .unwrap_or_default();
    let dotted: Vec<&str> = staged
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| has_dot_component(line))
        .collect();

    for batch in dotted.chunks(64) {
        let mut args = vec!["reset", "-q", "--"];
        args.extend(batch.iter().copied());
        let _ = git(root, &args);
    }
    Ok(())
}

/// Whether any component of the path begins with a dot.
fn has_dot_component(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        matches!(component, Component::Normal(name)
            if name.to_string_lossy().starts_with('.'))
    })
}

/// Record the current working tree as a tree object, leaving the worktree and
/// every ref untouched. Returns `None` if the snapshot cannot be taken, in which
/// case the caller falls back to diffing against `HEAD`.
fn snapshot_tree(root: &Path) -> Option<String> {
    stage_without_dotfiles(root).ok()?;
    let tree = git(root, &["write-tree"]).ok()?.trim().to_string();
    // Restore the index; the snapshot lives on as a dangling tree object.
    let _ = git(root, &["reset", "-q", "--", "."]);
    if tree.is_empty() { None } else { Some(tree) }
}

/// `git diff --cached [extra] [<base>] -- .`
///
/// With no base this compares the index against `HEAD`, which is also the only
/// form that works in a repository with no commits yet.
fn diff_args<'a>(extra: &[&'a str], base: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec!["diff", "--cached"];
    args.extend_from_slice(extra);
    if let Some(base) = base {
        args.push(base);
    }
    args.push("--");
    args.push(".");
    args
}

pub(crate) fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    /// Fixed so assertions can name the files; production uses a random id.
    const SESSION: &str = "0badc0de";

    use super::*;

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
    fn dot_components_are_recognised_at_any_depth() {
        assert!(has_dot_component(".gitignore"));
        assert!(has_dot_component(".github/workflows/ci.yml"));
        assert!(has_dot_component("kxs/.gradle/cache.bin"));
        assert!(!has_dot_component("kxs/src/Main.kt"));
        assert!(!has_dot_component("poc/Test.kt"));
    }

    #[test]
    fn a_run_s_changes_become_a_patch_and_the_repo_is_untouched() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        let head_before = git(&root, &["rev-parse", "HEAD"]).unwrap();

        // What an agent does during a run.
        std::fs::write(root.join("Main.kt"), "fun main() { println(1) }\n").unwrap();
        std::fs::create_dir_all(root.join("poc_depth")).unwrap();
        std::fs::write(root.join("poc_depth/Poc.kt"), "fun poc() {}\n").unwrap();
        std::fs::create_dir_all(root.join(".gradle")).unwrap();
        std::fs::write(root.join(".gradle/cache.bin"), "junk").unwrap();
        std::fs::write(root.join(".env.local"), "SECRET=1").unwrap();

        let outcome = tracker.finish(artifacts.path(), SESSION).unwrap();
        let patch = std::fs::read_to_string(&outcome.patch_path).unwrap();

        assert!(patch.contains("poc_depth/Poc.kt"), "new file missing");
        assert!(patch.contains("println(1)"), "modification missing");
        // Dotfiles are excluded even though nothing ignores them.
        assert!(!patch.contains(".gradle"), "dot directory leaked: {patch}");
        assert!(!patch.contains("SECRET"), "dotfile leaked: {patch}");
        assert_eq!(outcome.files_changed, 2);

        // The workspace is exactly as it was: same HEAD, nothing staged.
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap(), head_before);
        assert!(
            git(&root, &["diff", "--cached", "--name-only"])
                .unwrap()
                .trim()
                .is_empty(),
            "index left dirty"
        );
        // The agent's files are still on disk — the patch is a record, not a move.
        assert!(root.join("poc_depth/Poc.kt").exists());
        assert!(root.join(".env.local").exists());
    }

    #[test]
    fn a_non_repository_is_tracked_then_left_without_a_repository() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::write(root.join("Main.kt"), "fun main() {}\n").unwrap();
        let artifacts = tempfile::tempdir().unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        assert!(
            root.join(".git").is_dir(),
            "baseline repository not created"
        );
        assert!(tracker.describe().contains("initialised temporarily"));

        std::fs::write(root.join("Added.kt"), "fun added() {}\n").unwrap();
        let outcome = tracker.finish(artifacts.path(), SESSION).unwrap();

        let patch = std::fs::read_to_string(&outcome.patch_path).unwrap();
        assert!(patch.contains("Added.kt"), "new file missing: {patch}");
        // Pre-existing content is the baseline, not a change.
        assert!(!patch.contains("fun main"), "baseline treated as a change");
        // And the repository we created is gone again.
        assert!(!root.join(".git").exists(), ".git was left behind");
        assert!(root.join("Main.kt").exists());
    }

    #[test]
    fn a_workspace_with_no_changes_gets_a_note_not_an_empty_patch() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        let outcome = tracker.finish(artifacts.path(), SESSION).unwrap();

        assert_eq!(outcome.files_changed, 0);
        assert_eq!(outcome.bytes, 0);
        // A zero-byte .patch would read as a failure; a note reads as a result.
        assert_eq!(
            outcome.patch_path.file_name().unwrap(),
            "workspace-unchanged.md"
        );
        assert!(
            !artifacts
                .path()
                .join(format!("changes/{SESSION}_workspace.patch"))
                .exists()
        );
        let note = std::fs::read_to_string(&outcome.patch_path).unwrap();
        assert!(note.contains("made no changes to the workspace"), "{note}");
    }

    #[test]
    fn a_second_run_reports_only_its_own_changes() {
        // Exactly the back-to-back case: run one leaves an untracked PoC behind
        // (the harness never deletes it), then run two starts on that tree.
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();

        // Leftovers from a previous run: one untracked file, one tracked edit.
        std::fs::create_dir_all(root.join("poc_first_run")).unwrap();
        std::fs::write(root.join("poc_first_run/Old.kt"), "fun old() {}\n").unwrap();
        std::fs::write(root.join("Main.kt"), "fun main() { /* run one */ }\n").unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        assert!(
            tracker
                .describe()
                .contains("already had local changes at start"),
            "{}",
            tracker.describe()
        );

        // This run adds its own file.
        std::fs::write(root.join("poc_second_run.kt"), "fun new() {}\n").unwrap();
        let outcome = tracker.finish(artifacts.path(), SESSION).unwrap();
        let patch = std::fs::read_to_string(&outcome.patch_path).unwrap();

        assert!(patch.contains("poc_second_run.kt"), "own change missing");
        assert!(
            !patch.contains("poc_first_run"),
            "previous run's leftover attributed to this run: {patch}"
        );
        assert!(
            !patch.contains("run one"),
            "previous run's edit leaked: {patch}"
        );
        assert_eq!(outcome.files_changed, 1);

        // And nothing was deleted to achieve that.
        assert!(root.join("poc_first_run/Old.kt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("Main.kt")).unwrap(),
            "fun main() { /* run one */ }\n"
        );
    }

    #[test]
    fn untracked_leftovers_count_as_a_dirty_start() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        // Untracked only — invisible to `git diff`, which is what the old check used.
        std::fs::write(root.join("Leftover.kt"), "fun x() {}\n").unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        assert!(
            tracker.describe().contains("local changes at start"),
            "{}",
            tracker.describe()
        );
    }

    /// Two invocations pointed at one artifacts directory must both survive.
    ///
    /// Before the session prefix, `workspace.patch` and `index.md` were fixed
    /// names: the second run silently overwrote the first run's shared-checkout
    /// patch and the index explaining which agent produced what. Nothing failed,
    /// nothing warned -- the earlier run's evidence was simply gone, which is the
    /// worst shape for an audit trail to fail in.
    #[test]
    fn a_second_invocation_sharing_an_artifacts_dir_does_not_clobber_the_first() {
        let artifacts = tempfile::tempdir().unwrap();
        let mut written = Vec::new();

        for (session, body) in [
            ("aaaa1111", "fun first() {}\n"),
            ("bbbb2222", "fun second() {}\n"),
        ] {
            let target = tempfile::tempdir().unwrap();
            let root = target.path().canonicalize().unwrap();
            init_repo(&root);
            let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
            std::fs::write(root.join("Poc.kt"), body).unwrap();
            let outcome = tracker.finish(artifacts.path(), session).unwrap();
            let record = ChangeRecord::for_shared_root(root.clone(), &outcome);
            let index = write_change_index(artifacts.path(), &[record], session).unwrap();
            written.push((session, outcome.patch_path, index));
        }

        // Both runs' files exist side by side, each still holding its own content.
        for (session, patch, index) in &written {
            let marker = if *session == "aaaa1111" {
                "first"
            } else {
                "second"
            };
            let patch_body = std::fs::read_to_string(patch).unwrap();
            assert!(
                patch_body.contains(marker),
                "{session} patch lost its content"
            );
            assert!(
                std::fs::read_to_string(index)
                    .unwrap()
                    .contains(SHARED_ROOT_LABEL),
                "{session} index missing"
            );
        }
        assert_ne!(written[0].1, written[1].1, "patches collided");
        assert_ne!(written[0].2, written[1].2, "indexes collided");
    }

    #[test]
    fn two_subtrees_of_the_same_agent_do_not_share_a_patch_filename() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();
        let container = tempfile::tempdir().unwrap();

        // A run fans out ~19 concurrent `vulnerability-research` agents: the
        // very case isolation exists for. Worktree directories carry the root
        // task id, so the patches must too, or 18 of the 19 are lost while the
        // index still claims to list them all.
        let agent = "vulnerability-research";
        let mut records = Vec::new();
        for short_id in ["1a2b3c4d", "9f8e7d6c"] {
            let worktree = container.path().join(format!("{agent}-{short_id}"));
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--quiet",
                    &worktree.to_string_lossy(),
                    "HEAD",
                ],
            )
            .unwrap();
            std::fs::write(
                worktree.join("Repro.kt"),
                format!("fun r{short_id}() {{}}\n"),
            )
            .unwrap();
            records.push(
                capture_worktree_changes(
                    agent,
                    Some("Review decoder"),
                    &worktree,
                    artifacts.path(),
                    "HEAD",
                )
                .unwrap(),
            );
        }

        let names: Vec<&str> = records
            .iter()
            .map(|r| r.patch_file.as_deref().expect("patch file"))
            .collect();
        assert_ne!(names[0], names[1], "sibling patches collided: {names:?}");

        // Both files exist on disk with their own content, and the index names
        // each one rather than pointing twice at a single survivor.
        let index = std::fs::read_to_string(
            write_change_index(artifacts.path(), &records, SESSION).unwrap(),
        )
        .unwrap();
        for (name, short_id) in names.iter().zip(["1a2b3c4d", "9f8e7d6c"]) {
            let body =
                std::fs::read_to_string(artifacts.path().join(PATCH_DIR).join(name)).unwrap();
            assert!(
                body.contains(&format!("r{short_id}")),
                "{name} lost its content"
            );
            assert!(index.contains(name), "index omits {name}");
        }
    }

    #[test]
    fn each_agent_worktree_gets_its_own_patch_and_an_index_entry() {
        let target = tempfile::tempdir().unwrap();
        let root = target.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();

        // Two agents, each in its own worktree, writing conflicting files —
        // the case that made agents hide each other's reproducers.
        let container = tempfile::tempdir().unwrap();
        let mut records = Vec::new();
        for (agent, body) in [("vulnerability-poc", "poc"), ("patch-implementer", "patch")] {
            let worktree = container.path().join(agent);
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--quiet",
                    &worktree.to_string_lossy(),
                    "HEAD",
                ],
            )
            .unwrap();
            std::fs::write(worktree.join("Poc.kt"), format!("fun {body}() {{}}\n")).unwrap();
            records.push(
                capture_worktree_changes(
                    agent,
                    Some(&format!("{agent} task")),
                    &worktree,
                    artifacts.path(),
                    "HEAD",
                )
                .unwrap(),
            );
        }

        // One patch per agent, not one merged diff.
        for (record, body) in records.iter().zip(["poc", "patch"]) {
            let name = record.patch_file.as_deref().expect("patch file");
            assert_eq!(name, format!("agent-{}.patch", record.agent));
            let patch =
                std::fs::read_to_string(artifacts.path().join("changes").join(name)).unwrap();
            assert!(patch.contains("Poc.kt"), "{patch}");
            assert!(patch.contains(&format!("fun {body}()")), "{patch}");
            assert!(patch.contains("Apply with: git apply"), "{patch}");
            assert_eq!(record.files, vec!["Poc.kt".to_string()]);
        }

        // An untouched worktree produces no patch, so nothing is left to prune
        // by hand later.
        let idle = container.path().join("idle");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &idle.to_string_lossy(),
                "HEAD",
            ],
        )
        .unwrap();
        let idle_record =
            capture_worktree_changes("threat-model", None, &idle, artifacts.path(), "HEAD")
                .unwrap();
        assert!(!idle_record.changed());
        assert_eq!(idle_record.patch_file, None);
        records.push(idle_record);

        let index_path = write_change_index(artifacts.path(), &records, SESSION).unwrap();
        let index = std::fs::read_to_string(&index_path).unwrap();
        assert_eq!(
            index_path.file_name().unwrap(),
            format!("{SESSION}_index.md").as_str()
        );
        for agent in ["vulnerability-poc", "patch-implementer"] {
            assert!(index.contains(agent), "{agent} missing from index: {index}");
            assert!(
                index.contains(&format!("agent-{agent}.patch")),
                "patch name missing: {index}"
            );
            assert!(
                index.contains(&format!("{agent} task")),
                "title missing: {index}"
            );
        }
        assert!(index.contains("`Poc.kt`"), "files touched missing: {index}");
        assert!(
            index.contains("threat-model"),
            "idle workspace unlisted: {index}"
        );
    }

    #[test]
    fn the_index_names_the_shared_checkout_when_nothing_was_isolated() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        let artifacts = tempfile::tempdir().unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        std::fs::write(root.join("Poc.kt"), "fun poc() {}\n").unwrap();
        let outcome = tracker.finish(artifacts.path(), SESSION).unwrap();
        let record = ChangeRecord::for_shared_root(root.clone(), &outcome);
        assert_eq!(record.agent, SHARED_ROOT_LABEL);
        assert_eq!(
            record.patch_file.as_deref(),
            Some(format!("{SESSION}_workspace.patch").as_str())
        );

        let index = std::fs::read_to_string(
            write_change_index(artifacts.path(), &[record], SESSION).unwrap(),
        )
        .unwrap();
        assert!(index.contains(SHARED_ROOT_LABEL), "{index}");
        assert!(index.contains("workspace.patch"), "{index}");
    }

    #[test]
    fn an_index_with_no_changes_says_so() {
        let artifacts = tempfile::tempdir().unwrap();
        let index =
            std::fs::read_to_string(write_change_index(artifacts.path(), &[], SESSION).unwrap())
                .unwrap();
        assert!(index.contains("No workspace changed"), "{index}");
    }

    #[test]
    fn pre_existing_modifications_are_reported() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        init_repo(&root);
        std::fs::write(root.join("Main.kt"), "fun main() { /* local edit */ }\n").unwrap();

        let tracker = WorkspaceTracker::start(&root).unwrap().expect("tracker");
        assert!(
            tracker.describe().contains("local changes at start"),
            "{}",
            tracker.describe()
        );
    }
}
