//! Persistent verifier execution worktrees.
//!
//! Agents keep private source worktrees, while a small fixed pool keeps a
//! build's project-local incremental state warm. Each invocation snapshots the
//! caller's source changes into one lane, runs there exclusively, and copies XML
//! test results back so the caller can inspect them normally. A build system
//! opts into lanes by declaring `lane_commands`; today that is Gradle, but the
//! pool itself is not build-system specific.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::workspace_patch::git;
use crate::worktree::slugify;

const DEFAULT_MAX_VERIFIER_LANES: usize = 4;
const MAX_VERIFIER_LANES_VAR: &str = "FLAWRENCE_MAX_VERIFIER_LANES";
const MAX_UNTRACKED_FILES: usize = 1_024;
const MAX_UNTRACKED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TEST_REPORT_FILES: usize = 512;
const MAX_TEST_REPORT_BYTES: u64 = 16 * 1024 * 1024;

#[must_use]
pub fn max_verifier_lanes() -> usize {
    configured_parallelism(std::env::var(MAX_VERIFIER_LANES_VAR).ok().as_deref())
}

fn configured_parallelism(raw: Option<&str>) -> usize {
    raw.and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|permits| *permits > 0)
        .unwrap_or(DEFAULT_MAX_VERIFIER_LANES)
}

pub struct VerifierLanePool {
    root: PathBuf,
    container: PathBuf,
    lanes: Vec<PathBuf>,
    available: Mutex<Vec<usize>>,
    permits: Arc<Semaphore>,
}

impl VerifierLanePool {
    /// Create the complete requested pool. A Gradle target must not silently
    /// fall back to cold per-agent builds when lane setup is broken.
    pub fn new(root: &Path) -> Result<Arc<Self>> {
        Self::new_with_count(root, max_verifier_lanes())
    }

    fn new_with_count(root: &Path, requested: usize) -> Result<Arc<Self>> {
        let commit = git(root, &["rev-parse", "HEAD"])?;
        if commit.trim().is_empty() {
            anyhow::bail!("verifier lanes require a Git commit");
        }
        let container = lane_container(root);
        let lanes = create_lanes(root, &container, commit.trim(), requested)?;
        let count = lanes.len();
        let available = (0..count).rev().collect();
        Ok(Arc::new(Self {
            root: root.to_path_buf(),
            container,
            lanes,
            available: Mutex::new(available),
            permits: Arc::new(Semaphore::new(count)),
        }))
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} persistent verifier lane(s) at {}",
            self.lanes.len(),
            self.container.display()
        )
    }

    /// The first lane receives the startup pre-warm, so its first reproducer
    /// also benefits from project-local incremental state.
    #[must_use]
    pub fn prewarm_root(&self) -> &Path {
        &self.lanes[0]
    }

    pub async fn acquire(self: &Arc<Self>) -> Result<VerifierLaneLease> {
        let permit = self.permits.clone().acquire_owned().await?;
        let index = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .context("lane permit had no corresponding lane")?;
        Ok(VerifierLaneLease {
            pool: self.clone(),
            index,
            _permit: permit,
        })
    }
}

impl Drop for VerifierLanePool {
    fn drop(&mut self) {
        for lane in &self.lanes {
            if let Err(error) = git(
                &self.root,
                &["worktree", "remove", "--force", &lane.to_string_lossy()],
            ) {
                tracing::warn!(path = %lane.display(), "Could not remove lane: {error:#}");
            }
        }
        let _ = std::fs::remove_dir(&self.container);
    }
}

pub struct VerifierLaneLease {
    pool: Arc<VerifierLanePool>,
    index: usize,
    _permit: OwnedSemaphorePermit,
}

impl VerifierLaneLease {
    #[must_use]
    pub fn number(&self) -> usize {
        self.index + 1
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.pool.lanes[self.index]
    }

    /// Restore source state without touching ignored Gradle/build outputs, then
    /// overlay the exact tracked and untracked files visible to the caller.
    pub fn sync_from(&self, source: &Path) -> Result<()> {
        git(self.path(), &["reset", "--hard", "--quiet", "HEAD"])?;
        git(
            self.path(),
            &["clean", "-fd", "--quiet", "-e", ".gradle/", "-e", "build/"],
        )?;
        apply_tracked_changes(source, self.path())?;
        copy_untracked_files(source, self.path())?;
        copy_optional_file(source, self.path(), Path::new("local.properties"))?;
        clear_test_reports(self.path())?;
        clear_test_reports(source)
    }

    /// Copy only bounded XML test evidence back. Compilation outputs stay in
    /// the lane, which is the state subsequent invocations need to remain fast.
    pub fn copy_test_reports_to(&self, destination: &Path) -> Result<usize> {
        copy_test_reports(self.path(), destination)
    }
}

impl Drop for VerifierLaneLease {
    fn drop(&mut self) {
        self.pool
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(self.index);
    }
}

fn create_lanes(root: &Path, container: &Path, commit: &str, count: usize) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(container)?;
    let mut lanes = Vec::with_capacity(count);
    for index in 0..count {
        let path = container.join(format!("lane-{}", index + 1));
        let result = git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &path.to_string_lossy(),
                commit,
            ],
        );
        match result {
            Ok(_) => {
                if let Err(error) = copy_optional_file(root, &path, Path::new("local.properties")) {
                    tracing::warn!("Could not seed Gradle local.properties: {error:#}");
                }
                lanes.push(path);
            }
            Err(error) => {
                for lane in &lanes {
                    let _ = git(
                        root,
                        &["worktree", "remove", "--force", &lane.to_string_lossy()],
                    );
                }
                let _ = std::fs::remove_dir(container);
                return Err(error).context(format!("could not create lane {}", index + 1));
            }
        }
    }
    Ok(lanes)
}

fn copy_optional_file(source: &Path, destination: &Path, relative: &Path) -> Result<()> {
    let from = source.join(relative);
    if !from.is_file() {
        return Ok(());
    }
    let to = destination.join(relative);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to)?;
    Ok(())
}

fn apply_tracked_changes(source: &Path, destination: &Path) -> Result<()> {
    let patch = command_output(source, &["diff", "--binary", "HEAD", "--", "."])?;
    if patch.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .args(["apply", "--binary", "-"])
        .current_dir(destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("git apply stdin missing")?
        .write_all(&patch)?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "git apply failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn copy_untracked_files(source: &Path, destination: &Path) -> Result<()> {
    let raw = command_output(
        source,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )?;
    let mut bytes = 0_u64;
    for (index, path) in raw
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .enumerate()
    {
        if index >= MAX_UNTRACKED_FILES {
            anyhow::bail!("more than {MAX_UNTRACKED_FILES} untracked files in Gradle snapshot");
        }
        let relative = PathBuf::from(OsString::from_vec(path.to_vec()));
        validate_relative(&relative)?;
        bytes = copy_one(source, destination, &relative, bytes)?;
        if bytes > MAX_UNTRACKED_BYTES {
            anyhow::bail!("untracked Gradle snapshot exceeds {MAX_UNTRACKED_BYTES} bytes");
        }
    }
    Ok(())
}

fn copy_one(source: &Path, destination: &Path, relative: &Path, bytes: u64) -> Result<u64> {
    let from = source.join(relative);
    let to = destination.join(relative);
    let metadata = std::fs::symlink_metadata(&from)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&from)?;
        std::os::unix::fs::symlink(target, to)?;
        return Ok(bytes);
    }
    std::fs::copy(&from, &to)?;
    std::fs::set_permissions(&to, metadata.permissions())?;
    Ok(bytes.saturating_add(metadata.len()))
}

/// Where each supported build system leaves its machine-readable test report.
///
/// An agent confirms a reproducer by reading these, so a lane that runs a build
/// but copies nothing back looks exactly like a test that never ran. A new
/// build system needs its layout added here as well as a probe.
/// Where each build system leaves its machine-readable test report, declared by
/// the skill that carries that system's idiom.
///
/// An agent confirms a reproducer by reading these, so a lane that runs a build
/// but copies nothing back looks exactly like a test that never ran. Reading
/// them from the declarations keeps that in step with detection: a system that
/// can be detected and given a lane also says where its reports land.
fn test_report_patterns() -> Vec<String> {
    let declared = crate::build_systems::test_report_patterns(crate::build_systems::loaded());
    if declared.is_empty() {
        // A deployment with no idiom skills still copies back the two layouts
        // that were hardcoded here, rather than silently copying nothing.
        return vec![
            "**/build/test-results/**/*.xml".to_string(),
            "**/target/surefire-reports/*.xml".to_string(),
            "**/target/failsafe-reports/*.xml".to_string(),
        ];
    }
    declared
}

fn copy_test_reports(source: &Path, destination: &Path) -> Result<usize> {
    let escaped = glob::Pattern::escape(&source.to_string_lossy());
    let mut count = 0_usize;
    let mut bytes = 0_u64;
    for suffix in test_report_patterns() {
        for entry in glob::glob(&format!("{escaped}/{suffix}"))? {
            let from = entry?;
            let metadata = std::fs::metadata(&from)?;
            count += 1;
            bytes = bytes.saturating_add(metadata.len());
            if count > MAX_TEST_REPORT_FILES || bytes > MAX_TEST_REPORT_BYTES {
                anyhow::bail!("XML test reports exceed the copy limit");
            }
            let relative = from.strip_prefix(source)?;
            validate_relative(relative)?;
            let to = destination.join(relative);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(from, to)?;
        }
    }
    Ok(count)
}

fn clear_test_reports(root: &Path) -> Result<()> {
    let escaped = glob::Pattern::escape(&root.to_string_lossy());
    for suffix in test_report_patterns() {
        for entry in glob::glob(&format!("{escaped}/{suffix}"))? {
            std::fs::remove_file(entry?)?;
        }
    }
    Ok(())
}

fn command_output(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    anyhow::bail!(
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        anyhow::bail!(
            "unsafe relative path in Gradle snapshot: {}",
            path.display()
        );
    }
    Ok(())
}

fn lane_container(root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .map(|name| slugify(&name.to_string_lossy()))
        .unwrap_or_else(|| "target".to_string());
    std::env::temp_dir().join(format!(
        "flawrence-verifier-lanes-{name}-{}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_gradle_repo(root: &Path) {
        git(root, &["init", "-q"]).unwrap();
        std::fs::write(
            root.join("settings.gradle.kts"),
            "rootProject.name = \"test\"\n",
        )
        .unwrap();
        std::fs::write(root.join("tracked.kt"), "val answer = 1\n").unwrap();
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
    fn a_lane_keeps_ignored_build_state_but_replaces_source_state() {
        let repo = tempfile::tempdir().unwrap();
        init_gradle_repo(repo.path());
        let pool = VerifierLanePool::new_with_count(repo.path(), 1).unwrap();
        let lane = &pool.lanes[0];
        std::fs::create_dir_all(lane.join("build")).unwrap();
        std::fs::create_dir_all(lane.join("module/build")).unwrap();
        std::fs::write(lane.join("build/warm.bin"), "warm").unwrap();
        std::fs::write(lane.join("module/build/warm.bin"), "warm").unwrap();
        std::fs::write(repo.path().join("tracked.kt"), "val answer = 2\n").unwrap();
        std::fs::write(
            repo.path().join("Reproducer.kt"),
            "fun reproduce() = Unit\n",
        )
        .unwrap();

        let lease = VerifierLaneLease {
            pool: pool.clone(),
            index: 0,
            _permit: pool.permits.clone().try_acquire_owned().unwrap(),
        };
        lease.sync_from(repo.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(lane.join("tracked.kt")).unwrap(),
            "val answer = 2\n"
        );
        assert!(lane.join("Reproducer.kt").is_file());
        assert!(lane.join("build/warm.bin").is_file());
        assert!(lane.join("module/build/warm.bin").is_file());
    }

    #[test]
    fn parallelism_defaults_to_four_and_accepts_an_override() {
        assert_eq!(configured_parallelism(None), 4);
        assert_eq!(configured_parallelism(Some("2")), 2);
        assert_eq!(configured_parallelism(Some(" 8 ")), 8);
        for raw in ["0", "-3", "many", ""] {
            assert_eq!(
                configured_parallelism(Some(raw)),
                DEFAULT_MAX_VERIFIER_LANES
            );
        }
    }

    #[test]
    fn lane_setup_fails_on_a_non_git_target() {
        // A non-git target has no commit to detach lanes from. The runtime
        // degrades to direct builds on this error rather than aborting the run,
        // so it must stay a recoverable error and not a panic.
        let dir = tempfile::tempdir().unwrap();
        assert!(VerifierLanePool::new_with_count(dir.path(), 1).is_err());
    }

    #[tokio::test]
    async fn independent_worktrees_snapshot_into_distinct_parallel_lanes() {
        let repo = tempfile::tempdir().unwrap();
        let agents = tempfile::tempdir().unwrap();
        init_gradle_repo(repo.path());
        let first = agents.path().join("first");
        let second = agents.path().join("second");
        for path in [&first, &second] {
            git(
                repo.path(),
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--quiet",
                    &path.to_string_lossy(),
                    "HEAD",
                ],
            )
            .unwrap();
        }
        std::fs::write(first.join("Reproducer.kt"), "val owner = \"first\"\n").unwrap();
        std::fs::write(second.join("Reproducer.kt"), "val owner = \"second\"\n").unwrap();

        let pool = VerifierLanePool::new_with_count(repo.path(), 2).unwrap();
        let first_lane = pool.acquire().await.unwrap();
        let second_lane = pool.acquire().await.unwrap();
        first_lane.sync_from(&first).unwrap();
        second_lane.sync_from(&second).unwrap();

        assert_eq!(
            std::fs::read_to_string(first_lane.path().join("Reproducer.kt")).unwrap(),
            "val owner = \"first\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(second_lane.path().join("Reproducer.kt")).unwrap(),
            "val owner = \"second\"\n"
        );
        drop(first_lane);
        drop(second_lane);
        drop(pool);
        for path in [&first, &second] {
            git(
                repo.path(),
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            )
            .unwrap();
        }
    }
}
