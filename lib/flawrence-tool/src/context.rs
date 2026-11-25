use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::info;

use crate::artifact_store::SharedArtifactStore;
use crate::event_log::{SharedEventLog, TaskEvent};
use crate::runtime::RuntimeContext;
use crate::todo_list::SharedTodoList;
use flawrence_agent::AgentConfig;
use flawrence_chat::{SharedUsage, TaskOutput};

/// Type for the task spawner function injected by flawrence-task.
pub type TaskSpawner = Arc<
    dyn Fn(
            Arc<RuntimeContext>,
            String,         // agent_name
            String,         // query
            Vec<String>,    // ancestors
            Option<String>, // title
            SharedArtifactStore,
            SharedEventLog,
            SharedUsage,    // run_usage (shared by the complete task tree)
            String,         // task_id (assigned by caller; emitted in SubagentSpawned)
            Option<String>, // parent_task_id
        ) -> Pin<Box<dyn Future<Output = Result<TaskOutput>> + Send>>
        + Send
        + Sync,
>;

/// Context passed to tool handlers at runtime (owned version for async)
#[derive(Clone)]
pub struct ToolContext {
    pub runtime: Arc<RuntimeContext>,
    /// Full agent config (includes tool_configs, subagents, name, etc.)
    pub agent_config: AgentConfig,
    /// Ancestor titles (including current task) for hierarchy tracking
    pub ancestors: Vec<String>,
    /// Shared todo list for the current task
    pub todo_list: SharedTodoList,
    /// Shared artifact store (shared across entire task tree)
    pub artifact_store: SharedArtifactStore,
    /// Shared event log (shared across entire task tree)
    pub event_log: SharedEventLog,
    /// Usage for the complete root run, including failed child paths.
    pub run_usage: SharedUsage,
    /// task_id of the loop hosting this tool call. Subagent spawns
    /// thread this through as the child's `parent_task_id`.
    pub parent_task_id: String,
    /// Function to spawn child tasks (injected by flawrence-task)
    pub task_spawner: TaskSpawner,
}

impl ToolContext {
    #[expect(
        clippy::too_many_arguments,
        reason = "Preserve the explicit tool-context wiring API"
    )]
    pub fn new(
        runtime: Arc<RuntimeContext>,
        agent_config: AgentConfig,
        ancestors: Vec<String>,
        todo_list: SharedTodoList,
        artifact_store: SharedArtifactStore,
        event_log: SharedEventLog,
        run_usage: SharedUsage,
        parent_task_id: String,
        task_spawner: TaskSpawner,
    ) -> Self {
        Self {
            runtime,
            agent_config,
            ancestors,
            todo_list,
            artifact_store,
            event_log,
            run_usage,
            parent_task_id,
            task_spawner,
        }
    }

    /// Convenience accessor for the agent name.
    pub fn agent_name(&self) -> &str {
        &self.agent_config.name
    }

    /// Directory this task's tools operate in.
    ///
    /// With worktree isolation on, a file-writing agent gets its own checkout
    /// here, created on this call the first time the task asks for it; every
    /// other case resolves to [`RuntimeContext::base_directory`]. Tools must go
    /// through this rather than reading `base_directory` directly, so the
    /// fallback exists in exactly one place.
    #[must_use]
    pub fn workspace(&self) -> std::path::PathBuf {
        self.runtime.worktrees.workspace_for(
            &self.agent_config,
            &self.parent_task_id,
            self.ancestors.last().map(String::as_str),
        )
    }

    /// Spawn a child agent task. Handles depth checking and task hierarchy.
    pub async fn spawn_child(
        &self,
        agent_name: &str,
        title: String,
        query: String,
    ) -> Result<TaskOutput> {
        // Verify agent exists
        let _agent = self.runtime.agents.get(agent_name)?;

        // Check depth before spawning (ancestors includes current task, so +1 for the new child)
        let depth = self.ancestors.len() + 1;
        if depth > self.runtime.config.max_agent_depth {
            anyhow::bail!(
                "Task depth limit ({}) exceeded. Cannot spawn '{}' at depth {}. This prevents infinite recursion.",
                self.runtime.config.max_agent_depth,
                agent_name,
                depth
            );
        }

        // Once the operator has asked in-flight work to be cancelled, start no
        // new subagents: the run is consolidating what it already found, and a
        // fresh fan-out would be exactly the long, token-burning work the second
        // Ctrl-C was meant to stop.
        if self.runtime.shutdown.at_least(crate::shutdown::CONSOLIDATE) {
            return Ok(TaskOutput::cancelled(format!(
                "Subagent '{agent_name}' was not started: the run is shutting down. \
                 Consolidate and submit the findings already gathered."
            )));
        }

        info!(agent = %agent_name, title = %title, depth = depth, "Spawning subtask");

        // Generate the child's task id eagerly so we can emit the
        // `SubagentSpawned` event from the parent's perspective before
        // the child loop boots.
        let child_task_id = uuid::Uuid::now_v7().to_string();

        let child = async {
            // Holding a slot for descendants too would deadlock when every
            // research agent waits for a validator while the root queue is full.
            let _permit = if depth == 2 {
                Some(self.runtime.root_agent_slots.acquire().await?)
            } else {
                None
            };
            if self.runtime.shutdown.at_least(crate::shutdown::CONSOLIDATE) {
                return Ok(TaskOutput::cancelled(format!(
                    "Subagent '{agent_name}' was cancelled while waiting: the run is shutting down."
                )));
            }
            self.event_log
                .emit(
                    &self.parent_task_id,
                    TaskEvent::SubagentSpawned {
                        child_task_id: child_task_id.clone(),
                        agent: agent_name.to_string(),
                        title: title.clone(),
                        query: query.clone(),
                    },
                )
                .await;

            (self.task_spawner)(
                self.runtime.clone(),
                agent_name.to_string(),
                query,
                self.ancestors.clone(),
                Some(title),
                self.artifact_store.clone(),
                self.event_log.clone(),
                self.run_usage.clone(),
                child_task_id.clone(),
                Some(self.parent_task_id.clone()),
            )
            .await
        };

        // Race the child against a shutdown escalation. When the operator asks
        // for in-flight work to stop, dropping the `child` future cancels the
        // whole subtree it is running, mid-request, and the parent loop gets a
        // plain result back so it can proceed straight to consolidation.
        let output = tokio::select! {
            biased;
            result = child => result?,
            () = self.runtime.shutdown.reached(crate::shutdown::CONSOLIDATE) => {
                TaskOutput::cancelled(format!(
                    "Subagent '{agent_name}' was cancelled before finishing: the run is \
                     shutting down. Use the findings already gathered."
                ))
            }
        };

        self.event_log
            .emit(
                &self.parent_task_id,
                TaskEvent::SubagentCompleted {
                    child_task_id,
                    output: output.response.clone(),
                    usage: output.usage.to_json(),
                },
            )
            .await;

        Ok(output)
    }
}
