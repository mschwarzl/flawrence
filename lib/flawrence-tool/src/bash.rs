use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::command::run_command_bounded;
use crate::filesystem::safe_canonicalize_path;
use crate::{ToolContext, ToolDef};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_LANE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Per-stream cap. Halved from the shared response budget so that stdout and
/// stderr together stay inside it, and bash keeps reporting its own truncation
/// rather than being cut a second time by the task loop.
const MAX_OUTPUT_LENGTH: usize = crate::MAX_TOOL_RESPONSE_CHARS / 2;

fn resolve_cwd(base_dir: &Path, cwd: Option<&str>) -> Result<PathBuf> {
    if let Some(cwd) = cwd {
        safe_canonicalize_path(base_dir, &expand_workspace(base_dir, cwd))
    } else {
        Ok(base_dir.to_path_buf())
    }
}

/// Substitute `$WORKSPACE` / `${WORKSPACE}` in a `cwd` argument. The variable is
/// exported into every command's environment, and models reach for it in the
/// `cwd` field too -- where it is a tool argument, not a shell string, so
/// nothing would expand it. Honour it rather than failing on a literal `$`.
fn expand_workspace(base_dir: &Path, path: &str) -> String {
    let root = base_dir.to_string_lossy();
    path.replace("${WORKSPACE}", &root)
        .replace("$WORKSPACE", &root)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// The shell command to evaluate
    pub command: String,
    /// The working directory for the command. Null means the workspace root, which is
    /// almost always what you want; relative paths in the command resolve from here.
    /// Must resolve inside the workspace.
    pub cwd: Option<String>,
    /// Timeout in seconds. Null means 30 seconds, or 15 minutes for a verifier lane.
    pub timeout: Option<u64>,
}

impl ToolDef for BashArgs {
    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "Evaluates a shell expression and returns both stderr and stdout. \
        Commands run at the workspace root unless `cwd` is set, and relative paths resolve from there; \
        the directory used is reported with the output. \
        `$WORKSPACE` holds the absolute workspace root in every command, and is also accepted in `cwd` -- \
        prefer `cd $WORKSPACE/sub` over hardcoding or probing for the absolute path. \
        Build launcher commands run in persistent verifier lanes that snapshot the caller's \
        private worktree and retain incremental build state. Waiting for a lane does not consume the command \
        timeout, and a lane defaults to 15 minutes when `timeout` is null. \
        Do not use the bash tool if a more specialized tool can accomplish the same task.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let workspace = ctx.workspace();
        let cwd = resolve_cwd(&workspace, args.cwd.as_deref())?;
        let lane_pool =
            crate::build_systems::is_lane_command(&ctx.runtime.build_systems, &args.command)
                .then_some(ctx.runtime.verifier_lanes.as_ref())
                .flatten();

        let timeout = if let Some(timeout) = args.timeout {
            std::time::Duration::from_secs(timeout)
        } else if lane_pool.is_some() {
            DEFAULT_LANE_TIMEOUT
        } else {
            DEFAULT_TIMEOUT
        };

        // A build whose system declares verifier lanes gets a warm lane. Every
        // other shell command keeps the direct path it already had.
        let mut lane_lease = None;
        let (run_workspace, run_cwd, shell_command, execution_note) = match lane_pool {
            Some(pool) => {
                let lease = pool.acquire().await?;
                lease.sync_from(&workspace)?;
                let relative_cwd = cwd.strip_prefix(&workspace).with_context(|| {
                    format!("{} is outside {}", cwd.display(), workspace.display())
                })?;
                let lane_root = lease.path().to_path_buf();
                let lane_cwd = lane_root.join(relative_cwd);
                let caller = workspace.to_string_lossy();
                let lane = lane_root.to_string_lossy();
                let command = args.command.replace(caller.as_ref(), lane.as_ref());
                let note = format!("persistent verifier lane {}", lease.number());
                lane_lease = Some(lease);
                (lane_root, lane_cwd, command, Some(note))
            }
            None => (workspace.clone(), cwd.clone(), args.command.clone(), None),
        };

        let mut cmd = Command::new("bash");
        cmd.current_dir(&run_cwd)
            .env("WORKSPACE", &run_workspace)
            .arg("-c")
            .arg(&shell_command);
        if ctx.runtime.config.agent_memory_mb.is_some() {
            // Shared Gradle/Kotlin daemons would execute another agent's work
            // outside the tracked tree. One-shot daemons stay in its process tree.
            let options = std::env::var("GRADLE_OPTS").unwrap_or_default();
            cmd.env(
                "GRADLE_OPTS",
                format!("{options} -Dorg.gradle.daemon=false"),
            )
            .env(
                "ORG_GRADLE_PROJECT_kotlin.compiler.execution.strategy",
                "in-process",
            );
        }

        let mut output =
            run_command_bounded(&mut cmd, timeout, MAX_OUTPUT_LENGTH, MAX_OUTPUT_LENGTH).await?;
        if let Some(lease) = &lane_lease {
            let reports = lease.copy_test_reports_to(&workspace)?;
            let lane = lease.path().to_string_lossy();
            let caller = workspace.to_string_lossy();
            output.stdout = output.stdout.replace(lane.as_ref(), caller.as_ref());
            output.stderr = output.stderr.replace(lane.as_ref(), caller.as_ref());
            if reports > 0 {
                output.stderr.push_str(&format!(
                    "\nHarness copied {reports} XML test report(s) from the verifier lane."
                ));
            }
        }

        let stdout = if output.stdout_truncated {
            format!(
                "{} [Truncated to {} chars]",
                output
                    .stdout
                    .chars()
                    .take(MAX_OUTPUT_LENGTH)
                    .collect::<String>(),
                MAX_OUTPUT_LENGTH
            )
        } else {
            output.stdout
        };

        let stderr = if output.stderr_truncated || output.stderr.len() > MAX_OUTPUT_LENGTH {
            format!(
                "{} [Truncated to {} chars]",
                output
                    .stderr
                    .chars()
                    .take(MAX_OUTPUT_LENGTH)
                    .collect::<String>(),
                MAX_OUTPUT_LENGTH
            )
        } else {
            output.stderr
        };

        Ok((
            format_result(
                &args.command,
                &cwd,
                execution_note.as_deref(),
                output.exit_code,
                &stdout,
                &stderr,
            ),
            None,
        ))
    }
}

/// Report the directory the command actually ran in. It defaults to the
/// workspace root, whose absolute path the model has never been told, so a
/// command that fails on a relative path otherwise costs a `pwd` probe just to
/// find out where it was.
fn format_result(
    command: &str,
    cwd: &Path,
    execution_note: Option<&str>,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> String {
    let note = execution_note
        .map(|note| format!("\nexecution: {note}"))
        .unwrap_or_default();
    format!(
        "$ {command}\ncwd: {}{note}\nexit code: {exit_code}\nstdout: {stdout}\nstderr: {stderr}",
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn the_result_says_where_the_command_ran() {
        let result = format_result(
            "cd / && bash poc/focus_run.sh",
            Path::new("/workspace/awesn1"),
            None,
            127,
            "",
            "bash: poc/focus_run.sh: No such file or directory",
        );

        assert!(result.contains("cwd: /workspace/awesn1"), "{result}");
        assert!(result.contains("exit code: 127"), "{result}");
    }

    #[test]
    fn resolve_cwd_defaults_to_base_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        assert_eq!(resolve_cwd(&base, None).unwrap(), base);
    }

    #[test]
    fn resolve_cwd_accepts_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("subdir")).unwrap();

        assert_eq!(
            resolve_cwd(&base, Some("subdir")).unwrap(),
            base.join("subdir")
        );
    }

    #[test]
    fn resolve_cwd_expands_workspace_variable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("subdir")).unwrap();
        assert_eq!(
            resolve_cwd(&base, Some("$WORKSPACE/subdir")).unwrap(),
            base.join("subdir")
        );
        assert_eq!(
            resolve_cwd(&base, Some("${WORKSPACE}/subdir")).unwrap(),
            base.join("subdir")
        );
        assert_eq!(resolve_cwd(&base, Some("$WORKSPACE")).unwrap(), base);
    }

    /// $WORKSPACE is a convenience, not an escape hatch: it still resolves
    /// through the same containment check.
    #[test]
    fn resolve_cwd_still_rejects_traversal_through_workspace_variable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        assert!(resolve_cwd(&base, Some("$WORKSPACE/..")).is_err());
    }

    #[test]
    fn resolve_cwd_rejects_parent_traversal() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();

        assert!(resolve_cwd(&base, Some("..")).is_err());
    }

    #[test]
    fn resolve_cwd_rejects_absolute_path_outside_base() {
        let base_tmp = TempDir::new().unwrap();
        let outside_tmp = TempDir::new().unwrap();
        let base = base_tmp.path().canonicalize().unwrap();
        let outside = outside_tmp.path().canonicalize().unwrap();

        assert!(resolve_cwd(&base, Some(outside.to_str().unwrap())).is_err());
    }
}
