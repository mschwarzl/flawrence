//! Read-only HTTP dashboard for live Flawrence runs.
//!
//! Exposes run state, artifacts, and event streams, and serves the built Vue UI.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json, Response, Sse},
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use flawrence_agent::AgentRegistry;
use flawrence_task::ui::{JobDetail, JobRegistry, JobSummary};

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    job_registry: Arc<JobRegistry>,
    agents: Option<Arc<AgentRegistry>>,
}

/// Run the Axum status server until the shutdown signal fires.
pub async fn run_server(
    job_registry: Arc<JobRegistry>,
    agents: Option<Arc<AgentRegistry>>,
    addr: &str,
    shutdown: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let state = AppState {
        job_registry,
        agents,
    };

    let cors = CorsLayer::permissive();
    let assets = if PathBuf::from("frontend/dist/index.html").is_file() {
        PathBuf::from("frontend/dist")
    } else {
        std::env::current_exe()?
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("frontend/dist")
    };

    let app = Router::new()
        .route("/api/jobs", axum::routing::get(list_jobs))
        .route("/api/jobs/{id}", axum::routing::get(get_job))
        .route("/api/jobs/{id}/events", axum::routing::get(get_job_events))
        .route(
            "/api/jobs/{id}/events/stream",
            axum::routing::get(stream_job_events),
        )
        .route(
            "/api/jobs/{id}/artifacts",
            axum::routing::get(list_artifacts),
        )
        .route(
            "/api/jobs/{id}/artifacts/{key}",
            axum::routing::get(get_artifact),
        )
        .route("/api/agents", axum::routing::get(list_agents))
        .route("/health", axum::routing::get(health_check))
        .fallback_service(
            ServeDir::new(&assets).fallback(ServeFile::new(assets.join("index.html"))),
        )
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    eprintln!("[flawrence-status] UI listening on http://{}/", local_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await?;

    Ok(())
}

// ------------------------------------------------------------------
// Handlers
// ------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn list_jobs(State(state): State<AppState>) -> Json<Vec<JobSummary>> {
    Json(state.job_registry.list_jobs().await)
}

async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobDetail>, StatusCode> {
    match state.job_registry.get_job(&id).await {
        Some(job) => Ok(Json(job)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn get_job_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    let log = state
        .job_registry
        .get_event_log(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let events = log.snapshot_since(None).await;
    Ok((
        [("x-flawrence-max-events", log.max_events().to_string())],
        Json(events),
    )
        .into_response())
}

#[derive(Default, Deserialize)]
struct EventCursor {
    since: Option<u64>,
}

async fn stream_job_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(cursor): Query<EventCursor>,
    headers: HeaderMap,
) -> Result<
    Sse<impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, Infallible>>>,
    StatusCode,
> {
    let log = state
        .job_registry
        .get_event_log(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let since = cursor
        .since
        .into_iter()
        .chain(
            headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok()?.parse::<u64>().ok()),
        )
        .max();
    let stream = log.subscribe(since, Some(id));
    let sse_stream = stream.map(|ev| {
        let data = match serde_json::to_string(&ev) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("Failed to serialize event: {}", e);
                return Ok(axum::response::sse::Event::default().data("{}"));
            }
        };
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event("job-event")
                .id(ev.seq.to_string())
                .data(data),
        )
    });

    Ok(Sse::new(sse_stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

#[derive(Serialize)]
struct ArtifactListItem {
    key: String,
    content_type: String,
    extension: String,
}

#[derive(Serialize)]
struct ArtifactListResponse {
    artifacts: Vec<ArtifactListItem>,
}

async fn list_artifacts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ArtifactListResponse>, StatusCode> {
    let store = state
        .job_registry
        .get_artifact_store(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let guard = store.read().await;
    let artifacts: Vec<ArtifactListItem> = guard
        .keys()
        .into_iter()
        .filter_map(|key| {
            let entry = guard.read_entry(key).ok()?;
            Some(ArtifactListItem {
                key: key.to_string(),
                content_type: entry.content_type.clone(),
                extension: entry.extension.clone(),
            })
        })
        .collect();
    Ok(Json(ArtifactListResponse { artifacts }))
}

async fn get_artifact(
    State(state): State<AppState>,
    Path((id, key)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    let store = state
        .job_registry
        .get_artifact_store(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let guard = store.read().await;
    let entry = guard.read_entry(&key).map_err(|_| StatusCode::NOT_FOUND)?;
    let content_type = entry.content_type.clone();
    if let Some(path) = guard
        .artifact_path(&key)
        .map_err(|_| StatusCode::NOT_FOUND)?
    {
        drop(guard);
        let response = ServeFile::new(path)
            .try_call(Request::new(Body::empty()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(([(header::CONTENT_TYPE, content_type)], response).into_response());
    }
    Ok((
        [(header::CONTENT_TYPE, content_type)],
        entry.content.clone(),
    )
        .into_response())
}

#[derive(Serialize)]
struct AgentSummaryView {
    name: String,
    model: String,
    description: String,
    tools: Vec<String>,
    subagents: Vec<String>,
    recent_runs: Vec<JobSummary>,
}

#[derive(Serialize)]
struct AgentsResponse {
    agents: Vec<AgentSummaryView>,
}

async fn list_agents(State(state): State<AppState>) -> Json<AgentsResponse> {
    let jobs = state.job_registry.list_jobs().await;
    let agents = state.agents.as_ref().map(|reg| {
        reg.iter()
            .map(|agent| {
                let recent_runs: Vec<JobSummary> = jobs
                    .iter()
                    .filter(|j| j.agent_name == agent.name)
                    .cloned()
                    .rev()
                    .take(5)
                    .collect();
                AgentSummaryView {
                    name: agent.name.clone(),
                    model: agent.model.clone(),
                    description: agent.description.clone(),
                    tools: agent.tools.clone(),
                    subagents: agent.subagents.clone(),
                    recent_runs,
                }
            })
            .collect()
    });

    Json(AgentsResponse {
        agents: agents.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use flawrence_tool::{
        artifact_store::{ArtifactStore, SharedArtifactStore},
        event_log::{EventLog, TaskEvent},
    };
    use std::num::NonZeroUsize;

    async fn state() -> (AppState, Arc<EventLog>, SharedArtifactStore) {
        let log = EventLog::new_with_limit(NonZeroUsize::new(2).unwrap());
        let artifacts = ArtifactStore::new_shared(None);
        let registry = JobRegistry::new();
        registry
            .insert(
                0,
                "root".into(),
                "agent".into(),
                "query".into(),
                None,
                log.clone(),
                artifacts.clone(),
            )
            .await;
        (
            AppState {
                job_registry: registry,
                agents: None,
            },
            log,
            artifacts,
        )
    }

    #[tokio::test]
    async fn event_window_does_not_remove_artifacts() {
        let (state, log, artifacts) = state().await;
        let evidence = "full evidence\n".repeat(100);
        artifacts
            .write()
            .await
            .write("evidence_one".into(), evidence.clone())
            .unwrap();
        for iteration in 0..5 {
            log.emit("root", TaskEvent::IterationStarted { iteration })
                .await;
        }
        let response = get_job_events(State(state.clone()), Path("root".into()))
            .await
            .unwrap();
        assert_eq!(response.headers()["x-flawrence-max-events"], "2");
        let events: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap())
                .unwrap();
        assert_eq!(events.as_array().unwrap().len(), 2);
        assert_eq!(events[0]["seq"], 3);
        let listing = list_artifacts(State(state.clone()), Path("root".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(listing.artifacts.len(), 1);
        assert_eq!(listing.artifacts[0].key, "evidence_one");
        let response = get_artifact(State(state), Path(("root".into(), "evidence_one".into())))
            .await
            .unwrap();
        assert_eq!(
            to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .as_ref(),
            evidence.as_bytes()
        );
    }

    #[tokio::test]
    async fn persisted_artifacts_stream_the_file_with_recorded_content_type() {
        let directory = tempfile::tempdir().unwrap();
        let artifacts = ArtifactStore::new_shared(Some(directory.path().to_path_buf()));
        artifacts
            .write()
            .await
            .write("evidence".into(), "original body".into())
            .unwrap();
        let path = artifacts
            .read()
            .await
            .artifact_path("evidence")
            .unwrap()
            .unwrap();
        // Distinct disk bytes prove that the response does not clone the stored body.
        let persisted = b"Full persisted evidence, streamed without server rendering.";
        std::fs::write(path, persisted).unwrap();
        let log = EventLog::new_with_limit(NonZeroUsize::MIN);
        let registry = JobRegistry::new();
        registry
            .insert(
                0,
                "root".into(),
                "agent".into(),
                "query".into(),
                None,
                log,
                artifacts,
            )
            .await;
        let state = AppState {
            job_registry: registry,
            agents: None,
        };
        let response = get_artifact(State(state), Path(("root".into(), "evidence".into())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/markdown");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            persisted.len().to_string()
        );
        assert_eq!(
            to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .as_ref(),
            persisted
        );
    }

    #[tokio::test]
    async fn sse_resumes_from_the_latest_cursor_and_sends_event_ids() {
        let (state, log, _) = state().await;
        for iteration in 0..5 {
            log.emit("root", TaskEvent::IterationStarted { iteration })
                .await;
        }
        log.emit(
            "root",
            TaskEvent::TaskFailed {
                error: "done".into(),
            },
        )
        .await;
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "4".parse().unwrap());
        let response = stream_job_events(
            State(state.clone()),
            Path("root".into()),
            Query(EventCursor { since: Some(3) }),
            headers,
        )
        .await
        .unwrap()
        .into_response();
        let body = to_bytes(response.into_body(), 100_000).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("id: 5"));
        assert!(!text.contains("id: 4"));
        let response = stream_job_events(
            State(state),
            Path("root".into()),
            Query(EventCursor {
                since: Some(u64::MAX),
            }),
            HeaderMap::new(),
        )
        .await
        .unwrap()
        .into_response();
        assert!(
            to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
