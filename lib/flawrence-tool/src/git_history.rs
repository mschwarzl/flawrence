use crate::command::run_command_bounded;
use crate::{ToolContext, ToolDef};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryArgs {
    pub operation: GitOperation,
    /// Null means the whole repository rather than one file.
    pub path: Option<String>,
    /// Null means HEAD for `show`, and the full history for `log`.
    pub commit: Option<String>,
    /// Null means from the first line. Only meaningful for `blame`.
    pub start_line: Option<usize>,
    /// Null means to the last line. Only meaningful for `blame`.
    pub end_line: Option<usize>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitOperation {
    Log,
    Show,
    Blame,
}

impl ToolDef for GitHistoryArgs {
    const NAME: &'static str = "git_history";
    const DESCRIPTION: &'static str = "Explore git repository history. Operations: log (commit history), show (commit details), blame (line attribution).";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let workspace = ctx.workspace();
        let base_dir = workspace.as_path();
        // Clamp rather than reject: asking for 200 commits is not an error the
        // caller needs a turn to correct, it is a request for as many as the
        // tool will give.
        const MAX_LIMIT: usize = 50;
        let limit_note = if args.limit > MAX_LIMIT {
            format!(
                "\n\n[Requested limit {} exceeds the maximum; showing {MAX_LIMIT}.]",
                args.limit
            )
        } else {
            String::new()
        };
        let args = Self {
            limit: args.limit.min(MAX_LIMIT),
            ..args
        };
        match args.operation {
            GitOperation::Log => {
                let path = args
                    .path
                    .ok_or_else(|| anyhow::anyhow!("path required for log"))?;
                let safe = crate::filesystem::safe_canonicalize_path(base_dir, &path)?;
                let mut cmd = Command::new("git");
                cmd.current_dir(base_dir)
                    .args([
                        "log",
                        "--oneline",
                        "--no-decorate",
                        &format!("-{}", args.limit),
                        "--",
                    ])
                    .arg(&safe);
                let out =
                    run_command_bounded(&mut cmd, std::time::Duration::from_secs(30), 30000, 15000)
                        .await?;
                anyhow::ensure!(!out.timed_out, "git history command timed out");
                Ok((format!("{}{limit_note}", out.stdout), None))
            }
            GitOperation::Show => {
                let commit = args
                    .commit
                    .ok_or_else(|| anyhow::anyhow!("commit required"))?;
                if !commit
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '^' || c == '~')
                    || commit.is_empty()
                {
                    anyhow::bail!(
                        "Invalid commit reference: must be hexadecimal with optional ^/~ suffixes"
                    );
                }
                let mut cmd = Command::new("git");
                cmd.current_dir(base_dir)
                    .args(["show", "--stat", &commit, "--"]);
                let out =
                    run_command_bounded(&mut cmd, std::time::Duration::from_secs(30), 30000, 15000)
                        .await?;
                anyhow::ensure!(!out.timed_out, "git history command timed out");
                let s = out.stdout;
                Ok((
                    format!("{}{limit_note}", s.chars().take(8000).collect::<String>()),
                    None,
                ))
            }
            GitOperation::Blame => {
                let path = args
                    .path
                    .ok_or_else(|| anyhow::anyhow!("path required for blame"))?;
                let safe = crate::filesystem::safe_canonicalize_path(base_dir, &path)?;
                let (s, e) = match (args.start_line, args.end_line) {
                    (Some(s), Some(e)) => (s, e),
                    (Some(s), None) => (s, s + 50),
                    (None, Some(e)) => (if e > 50 { e - 50 } else { 1 }, e),
                    (None, None) => (1, 50),
                };
                let mut cmd = Command::new("git");
                cmd.current_dir(base_dir)
                    .args(["blame", "-L", &format!("{},{}", s, e), "--"])
                    .arg(&safe);
                let out =
                    run_command_bounded(&mut cmd, std::time::Duration::from_secs(30), 30000, 15000)
                        .await?;
                anyhow::ensure!(!out.timed_out, "git history command timed out");
                Ok((format!("{}{limit_note}", out.stdout), None))
            }
        }
    }
}
