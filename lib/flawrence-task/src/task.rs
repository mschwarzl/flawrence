use anyhow::Result;
use flawrence_agent::AgentConfig;
use flawrence_chat::{ChatMessage, SharedUsage, UsageByModel};

use flawrence_tool::artifact_store::SharedArtifactStore;
use flawrence_tool::event_log::SharedEventLog;
use flawrence_tool::todo_list::{SharedTodoList, TodoList};

/// One recorded tool call, kept for output-grounding verification (G).
///
/// `args` and `result` capture the model's call arguments and the tool's
/// output so the harness can verify grounded `submit_result` fields against
/// the real transcript instead of trusting the model's claims.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    /// Tool name, e.g. `bash`.
    pub tool: String,
    /// The model's call arguments (JSON), e.g. `{"command": "..."}`.
    pub args: serde_json::Value,
    /// The tool's output — parsed as JSON when the tool returned JSON, else a
    /// [`serde_json::Value::String`] wrapping the raw text.
    pub result: serde_json::Value,
}

/// Truncate a string to max_chars, appending "..." if truncated.
/// Safe for UTF-8: always cuts at char boundaries.
fn truncate_string(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{}...", truncated)
    }
}

/// Unified task struct that holds all execution state.
/// Forms a linked chain through parent references for hierarchy tracking.
pub struct Task {
    /// 128-bit random session identifier (32 hex chars).
    /// Used for provider-specific routing (e.g. GPU affinity).
    pub session_id: String,
    /// UUIDv7 task identifier. Stable for the lifetime of this loop.
    /// Every event emitted from this loop carries this id; subagent
    /// events carry their own id and reference this one as `parent_task_id`.
    pub task_id: String,
    /// Parent loop's task_id, or `None` for the root.
    pub parent_task_id: Option<String>,
    pub title: Option<String>,
    pub query: String,
    pub config: AgentConfig,
    pub messages: Vec<ChatMessage>,
    /// Current context window size (prompt tokens from last API call)
    pub context_size: u32,
    /// Cumulative token usage across all iterations, per model (shared for log formatter access)
    pub usage: SharedUsage,
    /// Cumulative usage for the complete root run, including failed child paths.
    pub run_usage: SharedUsage,
    /// Ancestor task titles (from root to immediate parent)
    pub ancestors: Vec<String>,
    /// Shared todo list for this task (exit guard checks this)
    pub todo_list: SharedTodoList,
    /// Shared artifact store (shared across entire task tree)
    pub artifact_store: SharedArtifactStore,
    /// Shared event log (shared across entire task tree)
    pub event_log: SharedEventLog,
    /// Recorded tool calls for output-grounding verification (G). Only collected
    /// when `config.output_grounding` is set. Sequential access within the task
    /// loop — pushed in the result-processing pass, read at `submit_result`.
    pub tool_call_history: Vec<ToolCallRecord>,
    /// Count of grounding mismatch nudges already issued this run (G).
    /// Bounded by `MAX_GROUNDING_NUDGES`; after that the harness forces the
    /// grounded truth.
    pub nudge_count: u32,
    /// Set when this subagent hit its iteration cap (B/F); the loop is now in
    /// the grace window before a synthesized result is returned.
    pub depth_limit_hit: bool,
}

impl Task {
    /// Create a new task. `task_id` is supplied by the caller (root
    /// callers generate one with [`crate::new_task_id`]; subagent spawns thread
    /// the value emitted in their `SubagentSpawned` event).
    #[expect(
        clippy::too_many_arguments,
        reason = "Task linkage and shared stores are supplied explicitly"
    )]
    pub fn new(
        task_id: String,
        query: String,
        mut config: AgentConfig,
        ancestors: Vec<String>,
        title: Option<String>,
        artifact_store: SharedArtifactStore,
        event_log: SharedEventLog,
        run_usage: SharedUsage,
        parent_task_id: Option<String>,
    ) -> Result<Self> {
        crate::checkpoint::scope_permissions(&mut config, &task_id);
        let title = title.map(|t| t.trim().to_string());
        let todo_list = TodoList::new_shared(&config.todos);

        let session_id = format!("{:032x}", rand::random::<u128>());

        Ok(Self {
            session_id,
            task_id,
            parent_task_id,
            title,
            query,
            config,
            messages: Vec::new(),
            context_size: 0,
            usage: UsageByModel::new_shared(),
            run_usage,
            ancestors,
            todo_list,
            artifact_store,
            event_log,
            tool_call_history: Vec::new(),
            nudge_count: 0,
            depth_limit_hit: false,
        })
    }

    /// Get the hierarchy as a vector of task titles from root to current (truncated to 60 chars)
    pub fn hierarchy(&self) -> Vec<String> {
        let mut titles: Vec<String> = self
            .ancestors
            .iter()
            .map(|t| truncate_string(t, 60))
            .collect();
        if let Some(ref title) = self.title {
            titles.push(truncate_string(title, 60));
        }
        titles
    }

    /// Update context size from API response (total prompt tokens)
    pub fn update_context_size(&mut self, prompt_tokens: u32) {
        self.context_size = prompt_tokens;
    }

    pub(crate) fn output_usage(&self) -> UsageByModel {
        select_output_usage(self.parent_task_id.is_none(), &self.usage, &self.run_usage)
    }

    pub(crate) fn attributed_usage(&self) -> UsageByModel {
        self.usage.lock().unwrap().clone()
    }
}

fn select_output_usage(
    is_root: bool,
    task_usage: &SharedUsage,
    run_usage: &SharedUsage,
) -> UsageByModel {
    let usage = if is_root { run_usage } else { task_usage };
    usage.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_output_uses_complete_run_usage() {
        let task_usage = UsageByModel::new_shared();
        let run_usage = UsageByModel::new_shared();
        task_usage.lock().unwrap().add("model", 1, 0, 0, 0);
        run_usage.lock().unwrap().add("model", 5, 0, 0, 1);

        assert_eq!(
            select_output_usage(true, &task_usage, &run_usage).total(),
            6
        );
        assert_eq!(
            select_output_usage(false, &task_usage, &run_usage).total(),
            1
        );
    }
}
