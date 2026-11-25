use crate::command::run_command_bounded;
use crate::{ToolContext, ToolDef};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitRaiderArgs {
    #[serde(default = "default_op")]
    pub operation: ScanOperation,
    #[serde(default)]
    pub stats: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_op() -> ScanOperation {
    ScanOperation::Scan
}
fn default_limit() -> usize {
    50
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScanOperation {
    Scan,
    Quick,
}

impl ToolDef for CommitRaiderArgs {
    const NAME: &'static str = "commitraider";
    const DESCRIPTION: &'static str = "Security-focused git history analysis using CommitRaider.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let workspace = ctx.workspace();
        let base_dir = workspace.as_path();
        // Clamp rather than reject; the tool already narrows to 50 below.
        let limit_note = if args.limit > 100 {
            format!(
                "\n\n[Requested limit {} exceeds the maximum; showing at most 50.]",
                args.limit
            )
        } else {
            String::new()
        };
        let bin = find_commitraider()?;
        let limit = args.limit.min(50).to_string();
        match args.operation {
            ScanOperation::Scan => {
                let mut cmd = Command::new(&bin);
                cmd.current_dir(base_dir).arg("--repo").arg(base_dir);
                cmd.args(["--output", "agent-json", "--top-n", &limit]);
                if args.stats {
                    cmd.arg("--stats");
                }
                let out = run_command_bounded(
                    &mut cmd,
                    std::time::Duration::from_secs(300),
                    50000,
                    15000,
                )
                .await?;
                if out.timed_out {
                    anyhow::bail!("commitraider timed out");
                }
                if out.exit_code != 0 {
                    anyhow::bail!("commitraider failed: {}", out.stderr);
                }
                Ok((
                    format!(
                        "{}{limit_note}",
                        out.stdout.chars().take(50000).collect::<String>()
                    ),
                    None,
                ))
            }
            ScanOperation::Quick => {
                let mut cmd = Command::new(&bin);
                cmd.current_dir(base_dir).arg("--repo").arg(base_dir).args([
                    "--output",
                    "agent-json",
                    "--compact",
                ]);
                let out = run_command_bounded(
                    &mut cmd,
                    std::time::Duration::from_secs(300),
                    20000,
                    15000,
                )
                .await?;
                if out.timed_out {
                    anyhow::bail!("commitraider timed out");
                }
                if out.exit_code != 0 {
                    anyhow::bail!("commitraider failed: {}", out.stderr);
                }
                Ok((
                    format!(
                        "{}{limit_note}",
                        out.stdout.chars().take(20000).collect::<String>()
                    ),
                    None,
                ))
            }
        }
    }
}

fn find_commitraider() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("COMMITRAIDER_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    for name in ["commitraider"] {
        if let Ok(p) = std::process::Command::new("which").arg(name).output()
            && p.status.success()
        {
            let path = String::from_utf8_lossy(&p.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let cargo = PathBuf::from(&home).join(".cargo/bin/commitraider");
    if cargo.exists() {
        return Ok(cargo);
    }
    anyhow::bail!("commitraider not found. Install with: cargo install commitraider")
}
