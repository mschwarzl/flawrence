use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;
use crate::skill::Skill;

/// Job creation request — the payload the container server receives.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobRequest {
    pub entrypoint: String,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Skill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<JobOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Vec<SetupStep>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JobOptions {
    pub max_iterations: Option<usize>,
    pub max_agent_depth: Option<usize>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetupStep {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Status response from the container server's `GET /job`.
///
/// `job_id` is the canonical job identifier — the server reads it at
/// startup from `CLOUDFLARE_DURABLE_OBJECT_ID` (injected by Cloudflare
/// Containers) and echoes it on every response so the worker can act
/// as a pure proxy.
///
/// `query` is the entrypoint prompt the job was launched with; it's
/// included so UIs polling `/job` don't have to round-trip through the
/// transcript or job-creation request to render the prompt.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobView {
    Running {
        job_id: String,
        agent: String,
        query: String,
    },
    Completed {
        job_id: String,
        agent: String,
        query: String,
        elapsed_secs: f64,
        output: serde_json::Value,
        usage: serde_json::Value,
        cost_usd: f64,
    },
    Failed {
        job_id: String,
        agent: String,
        query: String,
        error: String,
    },
    /// User-initiated cancellation, distinct from `Failed`.
    Cancelled {
        job_id: String,
        agent: String,
        query: String,
        reason: String,
    },
}
