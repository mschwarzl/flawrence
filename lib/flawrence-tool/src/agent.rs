use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolContext, ToolDef};
use flawrence_chat::UsageByModel;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnTaskArgs {
    /// Name of the agent to spawn (must be in your allowed agents list).
    #[schemars(length(max = 100))]
    pub agent_name: String,
    /// Short human-readable title for the subtask (e.g., "analyze auth module").
    #[schemars(length(max = 60))]
    pub task_title: String,
    /// Detailed query or instructions for the spawned agent.
    #[schemars(length(max = 5000))]
    pub query: String,
}

impl ToolDef for SpawnTaskArgs {
    const NAME: &'static str = "spawn_task";
    const DESCRIPTION: &'static str = "Spawn a subtask using a specific agent. You must have permission to spawn the target agent (check your agents: list in config). Provide a concise task_title (max 60 chars) and the query for the agent. Use when: a task requires specialized analysis that another agent is better suited for — e.g., PoC construction with vulnerability-poc, or a domain-specific review with a specialized agent. Prefer direct tool calls (code_index, grep_files, etc.) for simple lookups; use spawn_task when the subtask requires multi-step reasoning or an agent's full workflow.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        // Authorization: fail-closed — agent must be explicitly listed in subagents
        if !ctx.agent_config.can_spawn(&args.agent_name) {
            anyhow::bail!(
                "Agent '{}' is not allowed to spawn '{}'. Allowed subagents: {}",
                ctx.agent_name(),
                args.agent_name,
                if ctx.agent_config.subagents.is_empty() {
                    "(none)".to_string()
                } else {
                    ctx.agent_config.subagents.join(", ")
                }
            );
        }

        let output = ctx
            .spawn_child(&args.agent_name, args.task_title, args.query)
            .await?;
        Ok((output.response, Some(output.usage)))
    }
}
