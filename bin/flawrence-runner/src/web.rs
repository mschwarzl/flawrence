//! Standalone, single-run launcher for the Flawrence CLI.
//!
//! Starts a child runner and proxies its read-only dashboard through the same
//! public listener, including streaming events. Unix signals retain the runner's
//! existing staged Ctrl+C behavior.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::SeekFrom,
    net::{IpAddr, Ipv4Addr},
    path::{Path as FilePath, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use flawrence_agent::AgentConfig;
use flawrence_tool::{
    config::{
        DEFAULT_MAX_ITERATIONS, DEFAULT_MAX_PARALLEL_AGENTS, DEFAULT_MAX_TOKENS,
        DEFAULT_PREWARM_BUILD, DEFAULT_WORKTREE_ISOLATION,
    },
    event_log::DEFAULT_MAX_EVENTS,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::{Child, Command},
    sync::Mutex,
};

mod web_providers;
use web_providers::{
    ProviderStatus, configured_providers, discover_models, is_provider_key, model_error,
};

#[derive(Parser)]
#[command(about = "Launch a Flawrence run from a browser (Unix only)")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// Public port; PORT takes precedence over the default for hosted deployments.
    #[arg(long)]
    port: Option<u16>,
    /// Runner binary, defaulting to flawrence-runner beside this executable.
    #[arg(long)]
    runner: Option<PathBuf>,
}

struct App {
    runner: PathBuf,
    client: reqwest::Client,
    authorization: Option<String>,
    run: Mutex<Run>,
}

#[derive(Default)]
struct Run {
    child: Option<Child>,
    id: u64,
    port: u16,
    interrupts: u32,
    exit_code: Option<i32>,
    log: Option<PathBuf>,
    input: Option<Start>,
}

impl Run {
    fn running(&mut self) -> Result<bool> {
        let Some(child) = &mut self.child else {
            return Ok(false);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(true);
        };
        self.exit_code = status.code();
        self.child = None;
        Ok(false)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Environment {
    #[serde(default)]
    env: String,
    /// Present for the structured editor; omitted keys must not inherit server values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    controls: Option<HashMap<String, String>>,
    /// Structured provider fields own inherited credentials and endpoint overrides too.
    #[serde(default)]
    manage_providers: bool,
}

const CONTROLLED_ENV: [&str; 12] = [
    "FLAWRENCE_MODEL",
    "FLAWRENCE_AGENT_MEMORY_MB",
    "FLAWRENCE_MAX_PARALLEL_AGENTS",
    "FLAWRENCE_WEB_MAX_EVENTS",
    "MAX_TOKENS",
    "MAX_CONTEXT_TOKENS",
    "TOKEN_COST_CACHE_READ",
    "TOKEN_COST_INPUT",
    "TOKEN_COST_OUTPUT",
    "MAX_ITERATIONS",
    "FLAWRENCE_PREWARM_BUILD",
    "FLAWRENCE_WORKTREE_ISOLATION",
];

impl Environment {
    fn values(&self) -> Result<HashMap<String, String>> {
        let mut text = String::new();
        if let Some(controls) = &self.controls {
            ensure!(
                controls
                    .keys()
                    .all(|key| CONTROLLED_ENV.contains(&key.as_str()) || is_provider_key(key)),
                "Unknown environment control"
            );
            text = environment_text(controls.clone().into_iter());
        }
        // The free-form editor comes last, so it wins even for duplicate keys.
        text.push_str(&self.env);
        let values: HashMap<String, String> = dotenvy::from_read_iter(text.as_bytes())
            .collect::<std::result::Result<_, _>>()
            // Never reflect a malformed line: it may contain a credential.
            .map_err(|_| anyhow::anyhow!("Invalid environment; use dotenv KEY=value syntax"))?;
        ensure!(
            values
                .iter()
                .all(|(key, value)| !key.contains('\0') && !value.contains('\0')),
            "Environment must not contain NUL bytes"
        );
        Ok(values)
    }

    fn into_editor(self) -> Result<Self> {
        if self.controls.is_some() && self.manage_providers {
            return Ok(self);
        }
        let (controls, extra): (HashMap<_, _>, HashMap<_, _>) = self
            .values()?
            .into_iter()
            .partition(|(key, _)| CONTROLLED_ENV.contains(&key.as_str()) || is_provider_key(key));
        Ok(Self {
            env: environment_text(extra.into_iter()),
            controls: Some(controls),
            manage_providers: true,
        })
    }
}

#[derive(Serialize)]
struct EnvironmentPreview {
    effective: String,
    editor: Environment,
    providers: Vec<ProviderStatus>,
    model_error: Option<String>,
}

async fn preview_environment(
    Json(input): Json<Environment>,
) -> ApiResult<Json<EnvironmentPreview>> {
    let values = effective_environment(&input, std::env::vars()).map_err(bad_request)?;
    let providers = configured_providers(&values);
    let model_error = model_error(&values, &providers);
    let effective = environment_text(values.into_iter());
    Ok(Json(EnvironmentPreview {
        effective,
        editor: input.into_editor().map_err(bad_request)?,
        providers,
        model_error,
    }))
}

fn effective_environment(
    input: &Environment,
    inherited: impl Iterator<Item = (String, String)>,
) -> Result<HashMap<String, String>> {
    let mut values: HashMap<_, _> = inherited
        .filter(|(key, _)| {
            configurable(key)
                && (input.controls.is_none() || !CONTROLLED_ENV.contains(&key.as_str()))
                && (!input.manage_providers || !is_provider_key(key))
        })
        .collect();
    values.extend(input.values()?);
    values.remove("FLAWRENCE_WEB_PASSWORD");
    Ok(values)
}

#[derive(Deserialize)]
struct ModelsRequest {
    provider: String,
    #[serde(flatten)]
    environment: Environment,
}

async fn models(Json(input): Json<ModelsRequest>) -> ApiResult<Json<Vec<String>>> {
    let values =
        effective_environment(&input.environment, std::env::vars()).map_err(bad_request)?;
    discover_models(&values, &input.provider)
        .await
        .map(Json)
        .map_err(bad_request)
}

#[derive(Clone, Deserialize, Serialize)]
struct Start {
    directory: PathBuf,
    artifacts: PathBuf,
    agent: String,
    query: String,
    #[serde(default = "default_web_ui")]
    web_ui: bool,
    #[serde(flatten)]
    environment: Environment,
}

fn default_web_ui() -> bool {
    true
}

#[derive(Serialize)]
struct Status {
    running: bool,
    ready: bool,
    url: Option<String>,
    interrupts: u32,
    exit_code: Option<i32>,
    log: Option<PathBuf>,
    input: Option<Start>,
}

// ponytail: one owned child behind a lock; use a job queue only if concurrent runs are requested.
type Shared = Arc<App>;
type ApiResult<T> = std::result::Result<T, (StatusCode, String)>;

fn bad_request(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{error:#}"))
}

fn top_agents(environment: &HashMap<String, String>) -> Result<Vec<String>> {
    let directory = environment
        .get("FLAWRENCE_AGENTS_DIR")
        .cloned()
        .or_else(|| std::env::var("FLAWRENCE_AGENTS_DIR").ok())
        .unwrap_or_else(|| "agents".into());
    let agents: Vec<_> = fs::read_dir(&directory)
        .with_context(|| format!("Cannot read agents directory {directory}"))?
        .map(|entry| -> Result<Option<AgentConfig>> {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                return Ok(None);
            }
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("Invalid agent filename")?;
            Ok(Some(AgentConfig::parse(name, &fs::read_to_string(&path)?)?))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let children: HashSet<_> = agents
        .iter()
        .flat_map(|agent| agent.subagents.iter().chain(agent.output_reviewer.iter()))
        .collect();
    let mut roots: Vec<_> = agents
        .iter()
        .filter(|agent| !children.contains(&agent.name))
        .map(|agent| agent.name.clone())
        .collect();
    roots.sort();
    ensure!(!roots.is_empty(), "No top-level agents found");
    Ok(roots)
}

async fn agents(Json(environment): Json<Environment>) -> ApiResult<Json<Vec<String>>> {
    top_agents(&environment.values().map_err(bad_request)?)
        .map(Json)
        .map_err(bad_request)
}

#[derive(Serialize)]
struct Defaults {
    #[serde(flatten)]
    environment: Environment,
    control_defaults: HashMap<&'static str, String>,
    directory: PathBuf,
    artifacts: PathBuf,
}

fn configurable(name: &str) -> bool {
    (name.starts_with("FLAWRENCE_") && name != "FLAWRENCE_WEB_PASSWORD")
        || name.starts_with("TOKEN_COST_")
        || matches!(
            name,
            "ANTHROPIC_API_KEY"
                | "CLOUDFLARE_API_TOKEN"
                | "CLOUDFLARE_ACCOUNT_ID"
                | "OPENAI_API_KEY"
                | "OPENCODE_ACCESS_TOKEN"
                | "MAX_ITERATIONS"
                | "MAX_AGENT_DEPTH"
                | "MAX_TOKENS"
                | "MAX_CONTEXT_TOKENS"
                | "MAX_TOTAL_TOKENS"
                | "MAX_COST_USD"
                | "GITHUB_TOKEN"
                | "GH_TOKEN"
                | "VULN_INGEST_ENDPOINT"
                | "VULN_INGEST_TOKEN_PATH"
                | "TRAILMARK_BIN"
                | "COMMITRAIDER_PATH"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "NO_COLOR"
                | "RUST_LOG"
                | "GRADLE_USER_HOME"
                | "JAVA_HOME"
                | "ANDROID_HOME"
                | "ANDROID_SDK_ROOT"
        )
}

fn environment_defaults(values: impl Iterator<Item = (String, String)>) -> String {
    environment_text(values.filter(|(key, _)| configurable(key)))
}

fn environment_text(values: impl Iterator<Item = (String, String)>) -> String {
    let mut values: Vec<_> = values.collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
        .into_iter()
        .map(|(key, value)| {
            let value = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
                .replace('\n', "\\n");
            format!("{key}=\"{value}\"\n")
        })
        .collect()
}

async fn defaults() -> ApiResult<Json<Defaults>> {
    Ok(Json(Defaults {
        environment: Environment {
            env: environment_defaults(std::env::vars()),
            ..Environment::default()
        }
        .into_editor()
        .map_err(bad_request)?,
        control_defaults: HashMap::from([
            (
                "FLAWRENCE_MAX_PARALLEL_AGENTS",
                DEFAULT_MAX_PARALLEL_AGENTS.to_string(),
            ),
            ("FLAWRENCE_WEB_MAX_EVENTS", DEFAULT_MAX_EVENTS.to_string()),
            ("MAX_TOKENS", DEFAULT_MAX_TOKENS.to_string()),
            ("MAX_ITERATIONS", DEFAULT_MAX_ITERATIONS.to_string()),
            ("FLAWRENCE_PREWARM_BUILD", DEFAULT_PREWARM_BUILD.to_string()),
            (
                "FLAWRENCE_WORKTREE_ISOLATION",
                DEFAULT_WORKTREE_ISOLATION.to_string(),
            ),
        ]),
        directory: std::env::var_os("FLAWRENCE_WORKSPACE_DIR")
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir().map_err(|error| bad_request(error.into()))?),
        artifacts: std::env::var_os("FLAWRENCE_ARTIFACTS_BASE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("artifacts")),
    }))
}

fn command(
    app: &App,
    input: &Start,
    environment: &HashMap<String, String>,
    port: u16,
    log: &File,
) -> Result<Command> {
    let mut command = Command::new(&app.runner);
    if input.environment.controls.is_some() || input.environment.manage_providers {
        // Unset optional controls must not return through inheritance or a second .env load.
        command.arg("--no-dotenv");
        for key in CONTROLLED_ENV {
            command.env_remove(key);
        }
    }
    if input.environment.manage_providers {
        for (key, _) in std::env::vars().filter(|(key, _)| is_provider_key(key)) {
            command.env_remove(key);
        }
    }
    command
        .args(["--agent", &input.agent, "--directory"])
        .arg(input.directory.canonicalize()?)
        .arg("--artifacts")
        .arg(input.artifacts.canonicalize()?)
        .envs(environment)
        // Do not inherit launcher authentication through the child environment.
        .env_remove("FLAWRENCE_WEB_PASSWORD")
        .stdin(Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .kill_on_drop(true);
    if input.web_ui {
        command.args([
            "--web-ui",
            "--web-ui-host",
            "127.0.0.1",
            "--web-ui-port",
            &port.to_string(),
        ]);
    }
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

async fn start(State(app): State<Shared>, Json(input): Json<Start>) -> ApiResult<Json<Status>> {
    let mut run = app.run.lock().await;
    if run.running().map_err(bad_request)? {
        return Err((
            StatusCode::CONFLICT,
            "A flawrence process is already running".into(),
        ));
    }
    let environment =
        effective_environment(&input.environment, std::env::vars()).map_err(bad_request)?;
    validate_start(&input, &environment).map_err(bad_request)?;
    fs::create_dir_all(&input.artifacts).map_err(|error| bad_request(error.into()))?;
    let log_path = input
        .artifacts
        .canonicalize()
        .map_err(|error| bad_request(error.into()))?
        .join("launcher.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| bad_request(error.into()))?;
    // Reserve a fresh loopback port so separate launchers do not fight over 3000.
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| bad_request(error.into()))?;
    let port = listener
        .local_addr()
        .map_err(|error| bad_request(error.into()))?
        .port();
    let mut command = command(&app, &input, &environment, port, &log).map_err(bad_request)?;
    drop(listener);
    let mut child = command
        .spawn()
        .context("Cannot start flawrence-runner")
        .map_err(bad_request)?;
    let stdin = child
        .stdin
        .take()
        .context("Runner stdin unavailable")
        .map_err(bad_request)?;
    let query = input.query.clone();
    *run = Run {
        child: Some(child),
        id: run.id.saturating_add(1),
        port,
        log: Some(log_path),
        input: Some(input),
        ..Run::default()
    };
    tokio::spawn(async move {
        let mut stdin = stdin;
        if let Err(error) = stdin.write_all(query.as_bytes()).await {
            eprintln!("Cannot write runner query: {error}");
        }
    });
    drop(run);
    status(State(app)).await
}

fn validate_start(input: &Start, environment: &HashMap<String, String>) -> Result<()> {
    if input.environment.manage_providers
        && let Some(error) = model_error(environment, &configured_providers(environment))
    {
        anyhow::bail!(error);
    }
    ensure!(
        input.directory.is_dir(),
        "Workspace must be an existing host directory"
    );
    ensure!(
        !input.artifacts.as_os_str().is_empty(),
        "Artifacts directory is required"
    );
    ensure!(!input.query.trim().is_empty(), "Query is required");
    ensure!(
        top_agents(environment)?.contains(&input.agent),
        "Choose a top-level agent"
    );
    Ok(())
}

async fn status(State(app): State<Shared>) -> ApiResult<Json<Status>> {
    let mut run = app.run.lock().await;
    let running = run.running().map_err(bad_request)?;
    let (id, port) = (run.id, run.port);
    let mut status = Status {
        running,
        ready: false,
        url: (id > 0).then(|| format!("/run/{id}/")),
        interrupts: run.interrupts,
        exit_code: run.exit_code,
        log: run.log.clone(),
        input: run.input.clone(),
    };
    drop(run);
    if running && status.input.as_ref().is_none_or(|input| input.web_ui) {
        status.ready = app
            .client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
    }
    Ok(Json(status))
}

const LOG_TAIL_LINES: usize = 1_000;
const LOG_TAIL_BYTES: usize = 1024 * 1024;

#[derive(Serialize)]
struct LogTail {
    run_id: u64,
    text: String,
    truncated: bool,
}

/// Render terminal styling as plain text without changing the persisted log.
fn plain_log_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let mut plain = String::with_capacity(text.len());
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    // CSI ends at its command byte, including color and cursor controls.
                    chars.by_ref().find(|next| ('@'..='~').contains(next));
                }
                Some(']') => {
                    // OSC (e.g. terminal hyperlinks) ends at BEL or ESC followed by '\\'.
                    let mut escaped = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (escaped && next == '\\') {
                            break;
                        }
                        escaped = next == '\u{1b}';
                    }
                }
                _ => {}
            }
        } else if !character.is_control() || matches!(character, '\n' | '\t') {
            plain.push(character);
        }
    }
    plain
}

/// Read only the end of the log, including for a single oversized output line.
async fn read_log_tail(path: &FilePath) -> std::io::Result<(String, bool)> {
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let offset = length.saturating_sub(LOG_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(offset)).await?;
    let mut bytes = Vec::with_capacity(length.min(LOG_TAIL_BYTES as u64) as usize);
    file.take(LOG_TAIL_BYTES as u64)
        .read_to_end(&mut bytes)
        .await?;
    // A terminating newline ends the last line; it does not start an extra one.
    let end = bytes
        .len()
        .saturating_sub(usize::from(bytes.last() == Some(&b'\n')));
    let start = bytes[..end]
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(LOG_TAIL_LINES.saturating_sub(1))
        .map(|(index, _)| index.saturating_add(1));
    let truncated = offset > 0 && start.is_none();
    Ok((plain_log_text(&bytes[start.unwrap_or(0)..]), truncated))
}

async fn log_tail(State(app): State<Shared>) -> ApiResult<Json<LogTail>> {
    let (run_id, path) = {
        let run = app.run.lock().await;
        (run.id, run.log.clone())
    };
    let (text, truncated) = if let Some(path) = path {
        read_log_tail(&path).await.map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Cannot read runner log: {error}"),
            )
        })?
    } else {
        (String::new(), false)
    };
    Ok(Json(LogTail {
        run_id,
        text,
        truncated,
    }))
}

async fn interrupt(State(app): State<Shared>) -> ApiResult<Json<Status>> {
    let mut run = app.run.lock().await;
    if run.running().map_err(bad_request)? {
        #[cfg(unix)]
        if let Some(pid) = run.child.as_ref().and_then(Child::id) {
            // Signal only the runner: it owns cancellation of its agents and tools.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) } != 0 {
                return Err(bad_request(std::io::Error::last_os_error().into()));
            }
            run.interrupts = run.interrupts.saturating_add(1);
        }
    }
    drop(run);
    status(State(app)).await
}

async fn proxy(State(app): State<Shared>, Path(id): Path<u64>, request: Request) -> Response {
    let mut run = app.run.lock().await;
    if run.id != id
        || !run.running().unwrap_or(false)
        || run.input.as_ref().is_some_and(|input| !input.web_ui)
    {
        return (
            StatusCode::GONE,
            "This dashboard is unavailable. Return to the launcher.",
        )
            .into_response();
    }
    let port = run.port;
    drop(run);
    let prefix = format!("/run/{id}");
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    let upstream = format!(
        "http://127.0.0.1:{port}{}",
        path.strip_prefix(&prefix).unwrap_or("/")
    );
    match app.client.get(upstream).send().await {
        Ok(response) => {
            if !path.starts_with(&format!("{prefix}/api/"))
                && response.headers().get(header::CONTENT_TYPE)
                .is_some_and(|value| value.as_bytes().starts_with(b"text/html")) {
                return match response.text().await {
                    Ok(html) => Html(html.replacen("<head>",
                        &format!("<head><base href=\"{prefix}/\">"), 1)).into_response(),
                    Err(_) => StatusCode::BAD_GATEWAY.into_response(),
                };
            }
            let mut builder = Response::builder().status(response.status());
            for name in ["content-type", "cache-control", "x-flawrence-max-events"] {
                if let Some(value) = response.headers().get(name) {
                    builder = builder.header(name, value);
                }
            }
            builder.header("x-accel-buffering", "no")
                .body(Body::from_stream(response.bytes_stream()))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(_) if request.uri().path() == format!("{prefix}/") => Html(
            "<!doctype html><html lang='en'><meta charset='utf-8'><meta http-equiv='refresh' content='2'>\
            <title>Starting Flawrence</title><p>Starting Flawrence. The dashboard opens after initialization.\
            Check launcher.log in the artifacts directory for startup progress.</p></html>"
        ).into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "Dashboard is starting or unavailable").into_response(),
    }
}

async fn guard(State(app): State<Shared>, request: Request, next: Next) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    if let Some(expected) = &app.authorization {
        let actual = request
            .headers()
            .get(header::AUTHORIZATION)
            .map_or(&[][..], |v| v.as_bytes());
        let difference = actual
            .iter()
            .zip(expected.as_bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b));
        if actual.len() != expected.len() || difference != 0 {
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    "Basic realm=\"Flawrence\", charset=\"UTF-8\"",
                )],
                "Sign in as flawrence with FLAWRENCE_WEB_PASSWORD",
            )
                .into_response();
        }
    } else if !request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| {
            host == "localhost"
                || host.starts_with("localhost:")
                || host == "127.0.0.1"
                || host.starts_with("127.0.0.1:")
                || host == "[::1]"
                || host.starts_with("[::1]:")
        })
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    // A non-simple header plus no CORS opt-in blocks cross-site form/JS submissions.
    if request.method() == axum::http::Method::POST
        && request.headers().get("x-flawrence-launcher") != Some(&HeaderValue::from_static("1"))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("launcher.html")) }))
        .route(
            "/launcher/harp.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("harp.svg"),
                )
            }),
        )
        .route("/health", get(|| async { "ok" }))
        .route("/launcher/defaults", get(defaults))
        .route("/launcher/environment", post(preview_environment))
        .route("/launcher/models", post(models))
        .route("/launcher/agents", post(agents))
        .route("/launcher/start", post(start))
        .route("/launcher/status", get(status))
        .route("/launcher/log", get(log_tail))
        .route("/launcher/interrupt", post(interrupt))
        .route("/run/{id}/", get(proxy))
        .route("/run/{id}/{*path}", get(proxy_path))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn proxy_path(
    State(app): State<Shared>,
    Path((id, _)): Path<(u64, String)>,
    request: Request,
) -> Response {
    proxy(State(app), Path(id), request).await
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure!(cfg!(unix), "flawrence-web requires Unix for Ctrl+C signals");
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let port = cli
        .port
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::var("PORT")
                .unwrap_or_else(|_| "8080".into())
                .parse()
        })
        .context("Invalid PORT")?;
    let password = std::env::var("FLAWRENCE_WEB_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    ensure!(
        cli.host.is_loopback() || password.is_some(),
        "Set FLAWRENCE_WEB_PASSWORD before listening on a non-loopback address"
    );
    let app = Arc::new(App {
        runner: cli
            .runner
            .unwrap_or(std::env::current_exe()?.with_file_name("flawrence-runner")),
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(30))
            .build()?,
        authorization: password
            .map(|password| format!("Basic {}", STANDARD.encode(format!("flawrence:{password}")))),
        run: Mutex::new(Run::default()),
    });
    let listener = tokio::net::TcpListener::bind((cli.host, port)).await?;
    eprintln!("Flawrence launcher: http://{}/", listener.local_addr()?);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            if let Some(mut child) = app.run.lock().await.child.take()
                && let Err(error) = child.kill().await
            {
                eprintln!("Cannot stop child runner: {error}");
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::sse::{Event, Sse};
    use futures::StreamExt;
    use std::convert::Infallible;
    use tempfile::{TempDir, tempdir};

    fn app(runner: PathBuf) -> Shared {
        Arc::new(App {
            runner,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            authorization: None,
            run: Mutex::new(Run::default()),
        })
    }

    fn agent_directory() -> TempDir {
        let directory = tempdir().unwrap();
        for (name, references) in [
            (
                "security-review",
                "subagents: [child]\noutput_reviewer: reviewer",
            ),
            ("child", ""),
            ("reviewer", ""),
            ("threat-model", ""),
        ] {
            fs::write(directory.path().join(format!("{name}.md")), format!(
                "---\nmodel: test/model\ndescription: test\nreasoning_effort: High\n{references}\n---\nTest."
            )).unwrap();
        }
        directory
    }

    #[test]
    fn environment_defaults_round_trip_and_exclude_unrelated_secrets() {
        let values = [
            ("FLAWRENCE_MODEL", "custom/model"),
            (
                "FLAWRENCE_PROVIDER_CUSTOM_API_KEY",
                "a$HOME $VALUE \"quote\" \\ slash\nnext",
            ),
            ("MAX_TOKENS", "4096"),
            ("ANTHROPIC_API_KEY", "secret"),
            ("FLAWRENCE_WEB_PASSWORD", "launcher-secret"),
            ("UNRELATED_SECRET", "other-secret"),
        ];
        let text = environment_defaults(
            values
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        let parsed = Environment {
            env: text.clone(),
            ..Environment::default()
        }
        .values()
        .unwrap();
        assert_eq!(parsed.len(), 4);
        for (key, value) in &values[..4] {
            assert_eq!(parsed[*key], *value);
        }
        assert!(!text.contains("launcher-secret"));
        assert!(!text.contains("other-secret"));
        let error = Environment {
            env: "secret goes here invalid".into(),
            ..Environment::default()
        }
        .values()
        .unwrap_err();
        assert!(!error.to_string().contains("secret goes here"));
    }

    #[test]
    fn editor_moves_managed_values_out_of_the_freeform_environment() {
        let editor = Environment {
            env: "MAX_TOKENS=777\nFLAWRENCE_PREWARM_BUILD=off\nFLAWRENCE_MODEL='custom/model'"
                .into(),
            ..Environment::default()
        }
        .into_editor()
        .unwrap();
        let controls = editor.controls.as_ref().unwrap();
        assert_eq!(controls["MAX_TOKENS"], "777");
        assert_eq!(controls["FLAWRENCE_PREWARM_BUILD"], "off");
        assert!(!editor.env.contains("MAX_TOKENS"));
        assert!(!editor.env.contains("FLAWRENCE_MODEL"));
        assert_eq!(controls["FLAWRENCE_MODEL"], "custom/model");
        assert_eq!(editor.values().unwrap()["FLAWRENCE_MODEL"], "custom/model");
        let edited = Environment {
            env: "# escape hatch\nMAX_TOKENS=999999".into(),
            controls: Some(HashMap::from([("MAX_TOKENS".into(), "65536".into())])),
            manage_providers: true,
        };
        let restored = edited.clone().into_editor().unwrap();
        assert_eq!(restored.env, edited.env);
        assert_eq!(restored.controls, edited.controls);
    }

    #[test]
    fn provider_editor_imports_pairs_and_clearing_them_removes_inheritance() {
        let original = Environment {
            env: "FLAWRENCE_PROVIDER_LOCAL_BASE_URL=http://localhost:9000/v1\nFLAWRENCE_PROVIDER_LOCAL_API_KEY=test-secret\nANTHROPIC_API_KEY=other-secret\nFLAWRENCE_MODEL=local/org/model\nCUSTOM=preserved".into(),
            ..Environment::default()
        };
        let inherited = original.values().unwrap();
        let mut editor = original.into_editor().unwrap();
        assert!(editor.manage_providers);
        assert!(!editor.env.contains("API_KEY"));
        assert_eq!(
            editor.controls.as_ref().unwrap()["FLAWRENCE_PROVIDER_LOCAL_API_KEY"],
            "test-secret"
        );
        assert_eq!(editor.values().unwrap()["CUSTOM"], "preserved");
        editor.controls.as_mut().unwrap().clear();
        let cleared = effective_environment(&editor, inherited.clone().into_iter()).unwrap();
        assert!(!cleared.keys().any(|key| is_provider_key(key)));
        editor.env.push_str("\nFLAWRENCE_PROVIDER_LOCAL_BASE_URL=http://localhost:9999/v1\nFLAWRENCE_PROVIDER_LOCAL_API_KEY=override");
        let overridden = effective_environment(&editor, inherited.into_iter()).unwrap();
        assert_eq!(overridden["FLAWRENCE_PROVIDER_LOCAL_API_KEY"], "override");
        assert_eq!(
            overridden["FLAWRENCE_PROVIDER_LOCAL_BASE_URL"],
            "http://localhost:9999/v1"
        );
        assert!(!overridden.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn textarea_overrides_controls_and_unset_controls_do_not_inherit() {
        let input = Environment {
            env: r#"MAX_TOKENS=900000
MAX_TOKENS=999999
FLAWRENCE_WEB_MAX_EVENTS=20000
CUSTOM='literal $dollar "quotes"'
MULTILINE="line one
line two"
FLAWRENCE_WEB_PASSWORD=never-pass-this
"#
            .into(),
            controls: Some(HashMap::from([
                ("MAX_TOKENS".into(), "65536".into()),
                ("FLAWRENCE_PREWARM_BUILD".into(), "false".into()),
            ])),
            ..Environment::default()
        };
        let inherited = [
            ("FLAWRENCE_AGENT_MEMORY_MB", "1024"),
            ("MAX_CONTEXT_TOKENS", "1000000"),
            ("FLAWRENCE_WEB_MAX_EVENTS", "25"),
            ("TOKEN_COST_INPUT", "10"),
            ("FLAWRENCE_MODEL", "server/model"),
            ("FLAWRENCE_WEB_PASSWORD", "secret"),
            ("UNRELATED_SECRET", "secret"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()));
        let values = effective_environment(&input, inherited).unwrap();
        assert_eq!(values["MAX_TOKENS"], "999999");
        assert_eq!(values["FLAWRENCE_WEB_MAX_EVENTS"], "20000");
        assert_eq!(values["FLAWRENCE_PREWARM_BUILD"], "false");
        assert_eq!(values["CUSTOM"], "literal $dollar \"quotes\"");
        assert_eq!(values["MULTILINE"], "line one\nline two");
        for key in [
            "FLAWRENCE_AGENT_MEMORY_MB",
            "MAX_CONTEXT_TOKENS",
            "TOKEN_COST_INPUT",
            "FLAWRENCE_MODEL",
            "FLAWRENCE_WEB_PASSWORD",
            "UNRELATED_SECRET",
        ] {
            assert!(!values.contains_key(key), "{key} must be absent");
        }
        let round_trip = Environment {
            env: environment_text(values.clone().into_iter()),
            ..Environment::default()
        };
        assert_eq!(round_trip.values().unwrap(), values);
    }

    #[test]
    fn model_control_overrides_inheritance_and_empty_controls_leave_it_unset() {
        let mut input = Environment {
            controls: Some(HashMap::from([(
                "FLAWRENCE_MODEL".into(),
                "control/model".into(),
            )])),
            ..Environment::default()
        };
        assert_eq!(input.values().unwrap()["FLAWRENCE_MODEL"], "control/model");
        input.env = "FLAWRENCE_MODEL='override/model'".into();
        assert_eq!(input.values().unwrap()["FLAWRENCE_MODEL"], "override/model");
        input.env.clear();
        input.controls.as_mut().unwrap().clear();
        let values = effective_environment(
            &input,
            [("FLAWRENCE_MODEL".to_string(), "server/model".to_string())].into_iter(),
        )
        .unwrap();
        assert!(!values.contains_key("FLAWRENCE_MODEL"));
    }

    #[test]
    fn dashboard_choice_controls_runner_flags_and_defaults_to_enabled() {
        let directory = tempdir().unwrap();
        let log = File::create(directory.path().join("launcher.log")).unwrap();
        let mut input: Start = serde_json::from_value(serde_json::json!({
            "directory": directory.path(),
            "artifacts": directory.path(),
            "agent": "test",
            "query": "test"
        }))
        .unwrap();
        assert!(input.web_ui);
        for enabled in [false, true] {
            input.web_ui = enabled;
            let command =
                command(&app("unused".into()), &input, &HashMap::new(), 3000, &log).unwrap();
            for flag in ["--web-ui", "--web-ui-host", "--web-ui-port"] {
                assert_eq!(command.as_std().get_args().any(|arg| arg == flag), enabled);
            }
        }
    }

    #[test]
    fn structured_launch_removes_inherited_controls_and_skips_dotenv_reload() {
        let directory = tempdir().unwrap();
        let log = File::create(directory.path().join("launcher.log")).unwrap();
        let input = Start {
            directory: directory.path().into(),
            artifacts: directory.path().into(),
            agent: "test".into(),
            query: "test".into(),
            web_ui: true,
            environment: Environment {
                env: "MAX_TOKENS=999999".into(),
                controls: Some(HashMap::from([("MAX_TOKENS".into(), "65536".into())])),
                ..Environment::default()
            },
        };
        let values = input.environment.values().unwrap();
        let command = command(&app("unused".into()), &input, &values, 3000, &log).unwrap();
        let command = command.as_std();
        assert!(command.get_args().any(|arg| arg == "--no-dotenv"));
        let env: HashMap<_, _> = command.get_envs().collect();
        for key in CONTROLLED_ENV {
            let value = env[std::ffi::OsStr::new(key)];
            if key == "MAX_TOKENS" {
                assert_eq!(value, Some(std::ffi::OsStr::new("999999")));
            } else {
                assert_eq!(value, None, "{key} must not be inherited");
            }
        }
        assert_eq!(env[std::ffi::OsStr::new("FLAWRENCE_WEB_PASSWORD")], None);
    }

    #[tokio::test]
    async fn launcher_defaults_use_the_runner_defaults() {
        let defaults = defaults().await.unwrap().0;
        assert_eq!(
            defaults.control_defaults["MAX_TOKENS"],
            DEFAULT_MAX_TOKENS.to_string()
        );
        assert_eq!(
            defaults.control_defaults["MAX_ITERATIONS"],
            DEFAULT_MAX_ITERATIONS.to_string()
        );
        assert_eq!(
            defaults.control_defaults["FLAWRENCE_MAX_PARALLEL_AGENTS"],
            DEFAULT_MAX_PARALLEL_AGENTS.to_string()
        );
        assert_eq!(
            defaults.control_defaults["FLAWRENCE_PREWARM_BUILD"],
            DEFAULT_PREWARM_BUILD.to_string()
        );
        assert_eq!(
            defaults.control_defaults["FLAWRENCE_WORKTREE_ISOLATION"],
            DEFAULT_WORKTREE_ISOLATION.to_string()
        );
    }

    #[test]
    fn only_unreferenced_agents_are_top_level() {
        let agents = agent_directory();
        let environment = HashMap::from([(
            "FLAWRENCE_AGENTS_DIR".into(),
            agents.path().display().to_string(),
        )]);
        assert_eq!(
            top_agents(&environment).unwrap(),
            ["security-review", "threat-model"]
        );
    }

    #[tokio::test]
    async fn log_tail_removes_terminal_controls_without_changing_the_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("launcher.log");
        for (raw, expected) in [
            (
                "Tool call still running \x1b[3mtool\x1b[0m\x1b[2m=\x1b[0m\x1b[0mbash\x1b[0m\n",
                "Tool call still running tool=bash\n",
            ),
            (
                "\x1b[38;2;1;2;3mUTF-8: 🔒 café\x1b[0m\r\n\tresult\0\x1b[2K\n",
                "UTF-8: 🔒 café\n\tresult\n",
            ),
            (
                "\x1b]8;;https://example.com\x1b\\artifact\x1b]8;;\x1b\\\n",
                "artifact\n",
            ),
            ("\x1b]0;window title\x07output\n", "output\n"),
            ("partial\x1b[38;2;", "partial"),
            ("partial\x1b]8;;unfinished", "partial"),
            ("partial\x1b", "partial"),
        ] {
            fs::write(&path, raw).unwrap();
            assert_eq!(read_log_tail(&path).await.unwrap().0, expected);
            assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[tokio::test]
    async fn log_tail_keeps_the_last_thousand_lines_with_or_without_a_final_newline() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("launcher.log");
        for ending in ["", "\n"] {
            let text = (0..1_005)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n")
                + ending;
            fs::write(&path, text).unwrap();
            let (tail, truncated) = read_log_tail(&path).await.unwrap();
            assert_eq!(tail.lines().count(), LOG_TAIL_LINES);
            assert!(tail.starts_with("line 5\n"));
            assert!(tail.ends_with(&format!("line 1004{ending}")));
            assert!(!truncated);
        }
        fs::write(&path, b"").unwrap();
        assert_eq!(read_log_tail(&path).await.unwrap(), (String::new(), false));
        fs::write(&path, b"utf8: \xf0\x9f\x94\x92\ninvalid: \xff\n").unwrap();
        assert_eq!(
            read_log_tail(&path).await.unwrap().0,
            "utf8: 🔒\ninvalid: \u{fffd}\n"
        );
    }

    #[tokio::test]
    async fn log_tail_caps_oversized_lines_without_clipping_a_complete_line_window() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("launcher.log");
        let mut text = "x".repeat(LOG_TAIL_BYTES * 2);
        text.push_str("\nlast line\n");
        fs::write(&path, &text).unwrap();
        let (tail, truncated) = read_log_tail(&path).await.unwrap();
        assert!(truncated);
        assert_eq!(tail.len(), LOG_TAIL_BYTES);
        assert!(tail.ends_with("\nlast line\n"));
        for line in 0..LOG_TAIL_LINES {
            text.push_str(&format!("line {line}\n"));
        }
        fs::write(&path, text).unwrap();
        let (tail, truncated) = read_log_tail(&path).await.unwrap();
        assert!(!truncated);
        assert_eq!(tail.lines().count(), LOG_TAIL_LINES);
        assert!(tail.starts_with("line 0\n"));
    }

    #[tokio::test]
    async fn log_endpoint_reads_appended_output_even_after_the_child_exits() {
        use std::io::Write;
        let app = app("unused".into());
        let idle = log_tail(State(app.clone())).await.unwrap().0;
        assert_eq!(idle.run_id, 0);
        assert!(idle.text.is_empty());
        let directory = tempdir().unwrap();
        let path = directory.path().join("launcher.log");
        fs::write(&path, "first line\n").unwrap();
        *app.run.lock().await = Run {
            id: 7,
            log: Some(path.clone()),
            ..Run::default()
        };
        let first = log_tail(State(app.clone())).await.unwrap().0;
        assert_eq!(first.run_id, 7);
        assert_eq!(first.text, "first line\n");
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"final line\n")
            .unwrap();
        assert_eq!(
            log_tail(State(app)).await.unwrap().0.text,
            "first line\nfinal line\n"
        );
    }

    async fn wait_for_file(path: &std::path::Path, expected: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while fs::read_to_string(path).unwrap_or_default() != expected {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_receives_inputs_rejects_overlap_and_each_interrupt_reaches_it() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let agents = agent_directory();
        let runner = directory.path().join("runner");
        fs::write(&runner, r#"#!/bin/sh
cat > "$CAPTURE_DIR/query"
printf '%s\n' "$@" > "$CAPTURE_DIR/args"
printf '%s' "$FLAWRENCE_MODEL" > "$CAPTURE_DIR/model"
count=0
trap 'count=$((count + 1)); printf "%s" "$count" > "$CAPTURE_DIR/signals"; if [ "$count" -ge 3 ]; then exit 0; fi' INT
printf ready > "$CAPTURE_DIR/ready"
while :; do sleep 0.05; done
"#).unwrap();
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
        let app = app(runner);
        assert!(status(State(app.clone())).await.unwrap().0.input.is_none());
        let input = || Start {
            directory: directory.path().to_owned(),
            artifacts: directory.path().join("artifacts"),
            agent: "security-review".into(),
            query: "--not-an-option\n$(touch injected)\nline three".into(),
            web_ui: true,
            environment: Environment {
                env: format!(
                    "FLAWRENCE_AGENTS_DIR='{}'\nCAPTURE_DIR='{}'\nFLAWRENCE_MODEL=custom/model",
                    agents.path().display(),
                    directory.path().display()
                ),
                controls: Some(HashMap::from([("MAX_TOKENS".into(), "131072".into())])),
                ..Environment::default()
            },
        };
        let initial = start(State(app.clone()), Json(input())).await.unwrap().0;
        assert!(initial.running);
        assert_eq!(initial.url.as_deref(), Some("/run/1/"));
        let expected = serde_json::to_value(input()).unwrap();
        assert_eq!(serde_json::to_value(initial).unwrap()["input"], expected);
        let mut conflicting = input();
        conflicting.query = "A different query".into();
        conflicting.environment.env = "FLAWRENCE_MODEL=other/model".into();
        assert_eq!(
            start(State(app.clone()), Json(conflicting))
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::CONFLICT
        );
        let restored = status(State(app.clone())).await.unwrap().0;
        assert_eq!(serde_json::to_value(restored).unwrap()["input"], expected);
        wait_for_file(&directory.path().join("ready"), "ready").await;
        assert_eq!(
            fs::read_to_string(directory.path().join("query")).unwrap(),
            input().query
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("model")).unwrap(),
            "custom/model"
        );
        let args = fs::read_to_string(directory.path().join("args")).unwrap();
        assert!(args.contains("--web-ui\n--web-ui-host\n127.0.0.1\n--web-ui-port\n"));
        assert!(args.contains("--no-dotenv\n"));
        assert!(!args.contains("line three"));
        assert!(!directory.path().join("injected").exists());
        for count in 1..=3 {
            let _ = interrupt(State(app.clone())).await.unwrap();
            wait_for_file(&directory.path().join("signals"), &count.to_string()).await;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while status(State(app.clone())).await.unwrap().0.running {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let exited = status(State(app.clone())).await.unwrap().0;
        assert_eq!(exited.exit_code, Some(0));
        assert_eq!(serde_json::to_value(exited).unwrap()["input"], expected);
        assert!(!interrupt(State(app)).await.unwrap().0.running);
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        (url, handle)
    }

    #[tokio::test]
    async fn auth_and_csrf_protect_defaults_and_process_controls() {
        let mut app = Arc::try_unwrap(app("unused".into())).ok().unwrap();
        app.authorization = Some(format!("Basic {}", STANDARD.encode("flawrence:test")));
        let (url, server) = serve(router(Arc::new(app))).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client
                .get(format!("{url}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{url}/launcher/defaults"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{url}/launcher/log"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{url}/launcher/status"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{url}/launcher/interrupt"))
                .basic_auth("flawrence", Some("test"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = client
            .get(format!("{url}/launcher/defaults"))
            .basic_auth("flawrence", Some("test"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains("FLAWRENCE_WEB_PASSWORD")
        );
        for (authenticated, csrf, expected) in [
            (false, true, StatusCode::UNAUTHORIZED),
            (true, false, StatusCode::FORBIDDEN),
            (true, true, StatusCode::OK),
        ] {
            let mut request = client
                .post(format!("{url}/launcher/environment"))
                .json(&serde_json::json!({"env": "MAX_TOKENS=999999", "controls": {"MAX_TOKENS": "65536"}}));
            if authenticated {
                request = request.basic_auth("flawrence", Some("test"));
            }
            if csrf {
                request = request.header("X-Flawrence-Launcher", "1");
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let preview: serde_json::Value = response.json().await.unwrap();
                assert!(
                    preview["effective"]
                        .as_str()
                        .unwrap()
                        .contains("MAX_TOKENS=\"999999\"")
                );
                assert!(
                    !preview["effective"]
                        .as_str()
                        .unwrap()
                        .contains("FLAWRENCE_WEB_PASSWORD")
                );
            }
        }
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_rewrites_dashboard_base_streams_events_and_rejects_stale_tabs() {
        let upstream = Router::new()
            .route(
                "/",
                get(|| async { Html("<html><head></head><body>Dashboard</body></html>") }),
            )
            .route("/health", get(|| async { "ok" }))
            .route(
                "/api/jobs/root/events",
                get(|| async { ([("x-flawrence-max-events", "25")], Json(Vec::<u8>::new())) }),
            )
            .route(
                "/events",
                get(|| async {
                    Sse::new(
                        futures::stream::once(async {
                            Ok::<_, Infallible>(Event::default().data("first"))
                        })
                        .chain(futures::stream::pending()),
                    )
                }),
            );
        let (upstream_url, upstream_server) = serve(upstream).await;
        let app = app("unused".into());
        let child = Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        *app.run.lock().await = Run {
            child: Some(child),
            id: 7,
            port: upstream_url.rsplit(':').next().unwrap().parse().unwrap(),
            ..Run::default()
        };
        let (url, server) = serve(router(app.clone())).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let html = client
            .get(format!("{url}/run/7/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(html.contains("<base href=\"/run/7/\">"));
        let retained = client
            .get(format!("{url}/run/7/api/jobs/root/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(retained.headers()["x-flawrence-max-events"], "25");
        let response = client
            .get(format!("{url}/run/7/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        // The upstream never ends: this succeeds only when proxying without buffering.
        let mut response = response;
        let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&chunk).contains("first"));
        assert_eq!(
            client
                .get(format!("{url}/run/6/"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::GONE
        );
        assert!(status(State(app)).await.unwrap().0.ready);
        server.abort();
        upstream_server.abort();
    }
}
