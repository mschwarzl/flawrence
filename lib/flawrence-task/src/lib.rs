mod checkpoint;
pub(crate) mod enum_hints;
pub mod logging;
mod r#loop;
pub mod pool;
mod task;
pub mod ui;

use anyhow::Result;
use std::sync::Arc;

use flawrence_agent::AgentConfig;
use flawrence_chat::{SharedUsage, TaskOutput};
use flawrence_tool::artifact_store::SharedArtifactStore;
use flawrence_tool::event_log::SharedEventLog;
use flawrence_tool::runtime::RuntimeContext;

pub use flawrence_tool::event_log::{EventLog, SequencedEvent, TaskEvent, ToolCallSummary};
pub use r#loop::TaskLoop;
pub use task::Task;

/// Generate a fresh UUIDv7 task id, time-ordered for stream replay.
#[must_use]
pub fn new_task_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Run a task to completion using the specified agent configuration.
///
/// `task_id` is the UUIDv7 attributing every event from this loop;
/// `parent_task_id` is `None` for the root run, otherwise the spawner's
/// own `task_id`.
#[expect(
    clippy::too_many_arguments,
    reason = "Preserve the task-spawner callback signature"
)]
pub async fn run_task(
    runtime: Arc<RuntimeContext>,
    agent: &AgentConfig,
    query: String,
    ancestors: Vec<String>,
    title: Option<String>,
    artifact_store: SharedArtifactStore,
    event_log: SharedEventLog,
    run_usage: SharedUsage,
    task_id: String,
    parent_task_id: Option<String>,
) -> Result<TaskOutput> {
    // Resolve prompt placeholders uniformly for every agent (root and spawned
    // children both enter here). `arg_overrides` is set once on RuntimeContext
    // from the CLI `--arg` flags, so subagents inherit the main agent's values
    // automatically. Hard-fail on any unresolved `{{...}}` so a missing arg is
    // surfaced with a teaching message rather than leaking a literal placeholder
    // into the prompt.
    let mut agent = agent.clone();
    agent.resolve_params(&runtime.arg_overrides)?;

    let task = Task::new(
        task_id,
        query,
        agent,
        ancestors,
        title,
        artifact_store,
        event_log,
        run_usage,
        parent_task_id,
    )?;
    let mut task_loop = TaskLoop::new(runtime, task);
    task_loop.run().await
}
