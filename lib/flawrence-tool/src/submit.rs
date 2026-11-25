use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolContext, ToolDef};
use anyhow::Result;
use flawrence_chat::UsageByModel;

#[derive(Deserialize, JsonSchema)]
/// Placeholder — the real schema is injected by build_tools() from the agent's output_schema.
pub struct SubmitResultArgs {}

impl ToolDef for SubmitResultArgs {
    const NAME: &'static str = "submit_result";
    const DESCRIPTION: &'static str = "Submit your final structured result and end the task. This must be the LAST thing you do — call it ALONE, never batched with other tool calls. Use when: all required investigation is done and you can provide a complete, well-supported answer. Do NOT call prematurely — ensure all evidence is gathered and reasoning is sound before submitting. The result must conform to the expected output schema defined in your agent configuration.";

    /// This tool is never actually executed - the loop intercepts it.
    /// But we need a handler for the registry.
    async fn execute(_ctx: ToolContext, _args: Self) -> Result<(String, Option<UsageByModel>)> {
        // This should never be called - the loop handles submit_result specially
        anyhow::bail!(
            "submit_result must be intercepted by the task loop — direct execution is not supported"
        )
    }
}
