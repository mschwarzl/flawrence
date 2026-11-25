use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolContext, ToolDef};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadSkillArgs {
    /// Name of the skill to load. Must be one of the skills listed in
    /// `<available_skills>`; the runtime narrows this to the installed names.
    pub skill_name: String,
}

impl ToolDef for LoadSkillArgs {
    const NAME: &'static str = "load_skill";
    const DESCRIPTION: &'static str = "Load domain knowledge about a specific topic.  Returns structured knowledge to help with the current task.";

    async fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> Result<(String, Option<flawrence_chat::UsageByModel>)> {
        let skill = ctx.runtime.skills.get(&args.skill_name)?;
        Ok((
            format!("# Skill: {}\n\n{}", skill.name, skill.content),
            None,
        ))
    }
}
