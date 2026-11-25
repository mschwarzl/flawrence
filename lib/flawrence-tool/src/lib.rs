mod advisory;
mod agent;
mod artifact;
mod bash;
pub mod build_systems;
mod code_index;
mod commitraider;
mod context;
mod dedup;
mod filesystem;
mod git_history;
mod github;
mod registry;
mod report_finding;
mod skill;
mod submit;
mod submit_finding;
mod todo;
mod tool_args;
mod trailmark;
pub mod verifier_lanes;

pub mod artifact_store;
pub mod command;
pub mod config;
pub mod event_log;
pub mod memory;
pub mod prewarm;
pub mod runtime;
pub mod salvage;
pub mod scheduling;
pub mod shutdown;
pub mod todo_list;
pub mod workspace_patch;
pub mod worktree;

/// Maximum characters a single tool response may put into the model's context.
///
/// Tools that can exceed it truncate themselves with a precise, resumable
/// message; the task loop enforces the same budget as a backstop for the ones
/// that cannot predict their own size.
pub const MAX_TOOL_RESPONSE_CHARS: usize = 30_000;

pub use context::{TaskSpawner, ToolContext};
pub use registry::{ToolDef, ToolRegistry, registry};
