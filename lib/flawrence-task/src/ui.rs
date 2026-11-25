use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

use flawrence_tool::artifact_store::SharedArtifactStore;
use flawrence_tool::event_log::SharedEventLog;

/// Registry of all jobs spawned by the task pool.
pub struct JobRegistry {
    jobs: RwLock<HashMap<String, JobEntry>>,
}

/// Per-job metadata and references to live data.
pub struct JobEntry {
    pub run_id: usize,
    pub task_id: String,
    pub agent_name: String,
    pub query: String,
    pub target: Option<String>,
    pub status: RwLock<JobStatus>,
    pub created_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub completed_at: RwLock<Option<DateTime<Utc>>>,
    pub event_log: SharedEventLog,
    pub artifact_store: SharedArtifactStore,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed {
        output: String,
        #[serde(rename = "token_usage")]
        token_usage: serde_json::Value,
        elapsed_secs: f64,
    },
    Failed {
        error: String,
    },
}

/// Lightweight view of a job for listing.
#[derive(Debug, Clone, Serialize)]
pub struct JobSummary {
    pub run_id: usize,
    pub task_id: String,
    pub agent_name: String,
    pub query: String,
    pub target: Option<String>,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Detailed view of a single job.
#[derive(Debug, Clone, Serialize)]
pub struct JobDetail {
    pub run_id: usize,
    pub task_id: String,
    pub agent_name: String,
    pub query: String,
    pub target: Option<String>,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl JobRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            jobs: RwLock::new(HashMap::new()),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The UI receives the complete run registration explicitly"
    )]
    pub async fn insert(
        &self,
        run_id: usize,
        task_id: String,
        agent_name: String,
        query: String,
        target: Option<String>,
        event_log: SharedEventLog,
        artifact_store: SharedArtifactStore,
    ) {
        let now = Utc::now();
        let entry = JobEntry {
            run_id,
            task_id: task_id.clone(),
            agent_name,
            query,
            target,
            status: RwLock::new(JobStatus::Running),
            created_at: now,
            started_at: now,
            completed_at: RwLock::new(None),
            event_log,
            artifact_store,
        };
        self.jobs.write().await.insert(task_id, entry);
    }

    pub async fn finalize(
        &self,
        task_id: &str,
        outcome: &anyhow::Result<flawrence_chat::TaskOutput>,
    ) {
        let jobs = self.jobs.read().await;
        let Some(entry) = jobs.get(task_id) else {
            return;
        };
        let now = Utc::now();
        *entry.completed_at.write().await = Some(now);
        let mut status = entry.status.write().await;
        *status = match outcome {
            Ok(output) => JobStatus::Completed {
                output: output.response.clone(),
                token_usage: output.usage.to_json(),
                elapsed_secs: output.elapsed_secs,
            },
            Err(e) => JobStatus::Failed {
                error: format!("{e:#}"),
            },
        };
    }

    pub async fn list_jobs(&self) -> Vec<JobSummary> {
        let jobs = self.jobs.read().await;
        let mut result: Vec<JobSummary> = Vec::with_capacity(jobs.len());
        for entry in jobs.values() {
            let status = entry.status.read().await.clone();
            let completed_at = *entry.completed_at.read().await;
            result.push(JobSummary {
                run_id: entry.run_id,
                task_id: entry.task_id.clone(),
                agent_name: entry.agent_name.clone(),
                query: entry.query.clone(),
                target: entry.target.clone(),
                status,
                created_at: entry.created_at,
                started_at: entry.started_at,
                completed_at,
            });
        }
        // Sort by run_id for stable ordering
        result.sort_by_key(|j| j.run_id);
        result
    }

    pub async fn get_job(&self, task_id: &str) -> Option<JobDetail> {
        let jobs = self.jobs.read().await;
        let entry = jobs.get(task_id)?;
        let status = entry.status.read().await.clone();
        let completed_at = *entry.completed_at.read().await;
        Some(JobDetail {
            run_id: entry.run_id,
            task_id: entry.task_id.clone(),
            agent_name: entry.agent_name.clone(),
            query: entry.query.clone(),
            target: entry.target.clone(),
            status,
            created_at: entry.created_at,
            started_at: entry.started_at,
            completed_at,
        })
    }

    pub async fn get_event_log(&self, task_id: &str) -> Option<SharedEventLog> {
        let jobs = self.jobs.read().await;
        Some(jobs.get(task_id)?.event_log.clone())
    }

    pub async fn get_artifact_store(&self, task_id: &str) -> Option<SharedArtifactStore> {
        let jobs = self.jobs.read().await;
        Some(jobs.get(task_id)?.artifact_store.clone())
    }
}
