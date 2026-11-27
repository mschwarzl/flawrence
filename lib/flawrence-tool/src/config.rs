use anyhow::{Context, Result};
use std::collections::HashMap;
use std::env;

/// Global iteration cap when `MAX_ITERATIONS` is absent.
pub const DEFAULT_MAX_ITERATIONS: usize = 500;
/// Root subagent concurrency when `FLAWRENCE_MAX_PARALLEL_AGENTS` is absent.
pub const DEFAULT_MAX_PARALLEL_AGENTS: usize = 40;
/// Completion token limit when `MAX_TOKENS` is absent.
pub const DEFAULT_MAX_TOKENS: u32 = 65_536;
/// Whether builds are prewarmed when `FLAWRENCE_PREWARM_BUILD` is absent.
pub const DEFAULT_PREWARM_BUILD: bool = true;
/// Whether worktrees are isolated when `FLAWRENCE_WORKTREE_ISOLATION` is absent.
pub const DEFAULT_WORKTREE_ISOLATION: bool = false;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ClientMode {
    #[default]
    Streaming,
    Batch,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    /// Extra HTTP headers, from `FLAWRENCE_PROVIDER_<NAME>_HEADER_<KEY>` env vars.
    pub headers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
pub struct TokenCostOverride {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub cf_api_token: Option<String>,
    pub cf_account_id: Option<String>,
    pub max_iterations: usize,
    pub max_agent_depth: usize,
    /// Maximum active direct children of root agents across this invocation.
    /// Descendants do not take slots, so validators cannot deadlock their parents.
    pub max_parallel_agents: usize,
    /// Optional sampled RSS threshold in MiB per direct subagent tree, shared by descendants.
    pub agent_memory_mb: Option<u64>,
    pub max_tokens: u32,
    /// Combined input/output context limit per request. `None` disables automatic
    /// context-pressure checkpoints; no provider discovery is performed.
    pub max_context_tokens: Option<u32>,
    /// Cumulative token budget for the whole runner invocation (all agents and
    /// subagents). `None` means unlimited. Enforced against global usage.
    pub max_total_tokens: Option<u64>,
    /// Cumulative cost budget in USD for the whole runner invocation. `None`
    /// means unlimited. Enforced against global usage using live pricing.
    pub max_cost_usd: Option<f64>,
    pub token_cost_override: Option<TokenCostOverride>,
    pub proxy_url: Option<String>,
    pub client_mode: ClientMode,
    pub anthropic_api_key: Option<String>,
    /// Key for `openai/` models on api.openai.com or `FLAWRENCE_OPENAI_BASE_URL`.
    pub openai_api_key: Option<String>,
    /// opencode gateway token. Used for `openai/` models when no API key is set.
    pub opencode_access_token: Option<String>,
    pub model_override: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub openai_base_url: Option<String>,
    pub workers_ai_base_url: Option<String>,
    pub vuln_ingest_endpoint: String,
    pub vuln_ingest_token_path: Option<String>,
    /// Give every file-writing agent its own `git worktree` instead of letting
    /// the whole fan-out share one checkout. Off by default so a run can be
    /// compared against the shared-checkout behaviour it replaces.
    pub worktree_isolation: bool,
    /// Compile the target's main and test sources once before the fan-out, so
    /// each agent inherits a warm dependency and build cache instead of paying
    /// a cold compile. On by default; best-effort in all cases.
    pub prewarm_build: bool,
    /// Wall-clock ceiling for the pre-warm build, in seconds.
    pub prewarm_timeout_secs: u64,
    pub providers: HashMap<String, ProviderConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cf_api_token = env::var("CLOUDFLARE_API_TOKEN").ok();
        let cf_account_id = env::var("CLOUDFLARE_ACCOUNT_ID").ok();
        let anthropic_api_key = env::var("ANTHROPIC_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let openai_api_key = env::var("OPENAI_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let opencode_access_token = env::var("OPENCODE_ACCESS_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let model_override = env::var("FLAWRENCE_MODEL").ok();
        let anthropic_base_url = env::var("FLAWRENCE_ANTHROPIC_BASE_URL").ok();
        let openai_base_url = env::var("FLAWRENCE_OPENAI_BASE_URL").ok();
        let workers_ai_base_url = env::var("FLAWRENCE_WORKERS_AI_BASE_URL").ok();

        let max_iterations = env::var("MAX_ITERATIONS")
            .unwrap_or_else(|_| DEFAULT_MAX_ITERATIONS.to_string())
            .parse()
            .context("MAX_ITERATIONS must be a valid number")?;
        let max_agent_depth = env::var("MAX_AGENT_DEPTH")
            .unwrap_or_else(|_| "5".to_string())
            .parse()
            .context("MAX_AGENT_DEPTH must be a valid number")?;
        let max_parallel_agents =
            parse_parallel_agent_limit(env::var("FLAWRENCE_MAX_PARALLEL_AGENTS").ok().as_deref())?;
        let max_tokens = env::var("MAX_TOKENS")
            .unwrap_or_else(|_| DEFAULT_MAX_TOKENS.to_string())
            .parse()
            .context("MAX_TOKENS must be a valid number")?;
        let max_context_tokens = parse_optional_env("MAX_CONTEXT_TOKENS")
            .context("MAX_CONTEXT_TOKENS must be a positive integer greater than MAX_TOKENS")?
            .map(|limit| validate_context_limit(limit, max_tokens))
            .transpose()?;
        let agent_memory_mb = parse_optional_env("FLAWRENCE_AGENT_MEMORY_MB")
            .context("FLAWRENCE_AGENT_MEMORY_MB must be a positive integer in MiB")?;
        if let Some(limit) = agent_memory_mb {
            crate::memory::memory_bytes(limit)?;
        }
        let max_total_tokens = parse_optional_env("MAX_TOTAL_TOKENS")
            .context("MAX_TOTAL_TOKENS must be a valid positive integer")?
            .map(validate_positive_token_budget)
            .transpose()?;
        let max_cost_usd = parse_optional_env("MAX_COST_USD")
            .context("MAX_COST_USD must be a valid finite number")?
            .map(validate_positive_cost_budget)
            .transpose()?;
        let token_cost_override = parse_token_cost_override(
            env::var("TOKEN_COST_INPUT").ok(),
            env::var("TOKEN_COST_OUTPUT").ok(),
            env::var("TOKEN_COST_CACHE_READ").ok(),
        )?;
        let proxy_url = env::var("HTTP_PROXY").ok();
        let vuln_ingest_endpoint = env::var("VULN_INGEST_ENDPOINT").unwrap_or_default();
        let vuln_ingest_token_path = env::var("VULN_INGEST_TOKEN_PATH").ok();
        let worktree_isolation =
            parse_bool_env("FLAWRENCE_WORKTREE_ISOLATION", DEFAULT_WORKTREE_ISOLATION);
        let prewarm_build = parse_bool_env("FLAWRENCE_PREWARM_BUILD", DEFAULT_PREWARM_BUILD);
        // A Kotlin Multiplatform project with Apple targets can legitimately
        // need an hour to compile main plus test sources, so a tight cap would
        // SIGKILL a healthy build partway and leave the agents a half-populated
        // build directory. Default generously and let the operator lower it.
        let prewarm_timeout_secs = parse_u64_env("FLAWRENCE_PREWARM_TIMEOUT_SECS", 90 * 60);
        let client_mode = match env::var("FLAWRENCE_CLIENT").as_deref() {
            Ok("batch") => ClientMode::Batch,
            _ => ClientMode::Streaming,
        };

        let mut providers: HashMap<String, ProviderConfig> = HashMap::new();
        for (key, value) in env::vars() {
            let Some(rest) = key.strip_prefix("FLAWRENCE_PROVIDER_") else {
                continue;
            };
            let Some((name, field)) = rest.split_once('_') else {
                continue;
            };
            let name = name.to_lowercase();
            let entry = providers.entry(name).or_default();
            match field {
                "BASE_URL" => entry.base_url = value,
                "API_KEY" => entry.api_key = value,
                _ => {
                    if let Some(header) = field.strip_prefix("HEADER_") {
                        // Env vars cannot hold dashes, so HEADER_X_OPENCODE_SESSION
                        // maps to the HTTP header `x-opencode-session`.
                        entry.headers.insert(header.replace('_', "-"), value);
                    }
                }
            }
        }
        providers.retain(|_, p| !p.base_url.is_empty() && !p.api_key.is_empty());

        Ok(Config {
            cf_api_token,
            cf_account_id,
            max_iterations,
            max_agent_depth,
            max_parallel_agents,
            agent_memory_mb,
            max_tokens,
            max_context_tokens,
            max_total_tokens,
            max_cost_usd,
            token_cost_override,
            proxy_url,
            client_mode,
            anthropic_api_key,
            openai_api_key,
            opencode_access_token,
            model_override,
            anthropic_base_url,
            openai_base_url,
            workers_ai_base_url,
            vuln_ingest_endpoint,
            vuln_ingest_token_path,
            worktree_isolation,
            prewarm_build,
            prewarm_timeout_secs,
            providers,
        })
    }

    /// Fold the effective model override into the config. A `--model` flag wins
    /// over the `FLAWRENCE_MODEL` value that `from_env` read; with no flag the env
    /// value is kept. Returning `Self` keeps the call site a single immutable
    /// expression instead of a mutable binding patched after construction.
    #[must_use]
    pub fn with_model_override(mut self, cli_model: Option<String>) -> Self {
        if cli_model.is_some() {
            self.model_override = cli_model;
        }
        self
    }
}

/// Environment-driven settings that are not part of [`Config`] but still change
/// how a run behaves. Values are read live so the snapshot matches the process.
const EXTRA_ENV_SETTINGS: &[&str] = &[
    "TRAILMARK_BIN",
    "COMMITRAIDER_PATH",
    "FLAWRENCE_AGENTS_DIR",
    "FLAWRENCE_SKILLS_DIR",
    "FLAWRENCE_ARTIFACTS_BASE_DIR",
    "FLAWRENCE_WEB_MAX_EVENTS",
    "FLAWRENCE_STRICT_TOOLS",
    "FLAWRENCE_GHSA_LIVE",
    "FLAWRENCE_VULN_INGEST_LIVE",
];

/// Credential variables. Only their presence is ever reported.
const SECRET_ENV_SETTINGS: &[&str] = &["GITHUB_TOKEN", "GH_TOKEN"];

impl Config {
    /// Flawrence configuration as `(name, value)` pairs for the run summary.
    ///
    /// This is deliberately an explicit allow-list rather than a dump of the
    /// environment: the process environment routinely holds credentials that
    /// have nothing to do with the harness. Secrets that *are* harness
    /// settings are reported as `set`/`unset` and never by value, and any
    /// userinfo embedded in a URL is masked.
    pub fn describe(&self) -> Vec<(String, String)> {
        let mut settings: Vec<(String, String)> = Vec::new();
        let mut push = |name: &str, value: String| settings.push((name.to_string(), value));

        push("MAX_ITERATIONS", self.max_iterations.to_string());
        push("MAX_AGENT_DEPTH", self.max_agent_depth.to_string());
        push(
            "FLAWRENCE_MAX_PARALLEL_AGENTS",
            self.max_parallel_agents.to_string(),
        );
        push("MAX_TOKENS", self.max_tokens.to_string());
        push(
            "FLAWRENCE_AGENT_MEMORY_MB",
            optional(self.agent_memory_mb, "disabled"),
        );
        push(
            "MAX_CONTEXT_TOKENS",
            optional(self.max_context_tokens, "unset"),
        );
        push(
            "MAX_TOTAL_TOKENS",
            optional(self.max_total_tokens, "unlimited"),
        );
        push(
            "MAX_COST_USD",
            self.max_cost_usd
                .map(|cost| format!("{cost:.2}"))
                .unwrap_or_else(|| "unlimited".to_string()),
        );
        match self.token_cost_override {
            Some(cost) => {
                push("TOKEN_COST_INPUT", cost.input_per_mtok.to_string());
                push("TOKEN_COST_OUTPUT", cost.output_per_mtok.to_string());
                push(
                    "TOKEN_COST_CACHE_READ",
                    optional(cost.cache_read_per_mtok, "unset"),
                );
            }
            None => push(
                "TOKEN_COST_*",
                "unset (live pricing from models.dev)".into(),
            ),
        }
        push(
            "FLAWRENCE_CLIENT",
            match self.client_mode {
                ClientMode::Streaming => "streaming".into(),
                ClientMode::Batch => "batch".into(),
            },
        );
        // Set from `FLAWRENCE_MODEL` or the `--model` flag; the runner records
        // whichever actually applied.
        push(
            "FLAWRENCE_MODEL / --model",
            optional_ref(self.model_override.as_deref(), "unset"),
        );
        push(
            "FLAWRENCE_ANTHROPIC_BASE_URL",
            optional_url(self.anthropic_base_url.as_deref()),
        );
        push(
            "FLAWRENCE_OPENAI_BASE_URL",
            optional_url(self.openai_base_url.as_deref()),
        );
        push(
            "FLAWRENCE_WORKERS_AI_BASE_URL",
            optional_url(self.workers_ai_base_url.as_deref()),
        );
        push(
            "HTTP_PROXY",
            self.proxy_url
                .as_deref()
                .filter(|url| !url.is_empty())
                .map(redact_url)
                .unwrap_or_else(|| "unset".to_string()),
        );
        push(
            "VULN_INGEST_ENDPOINT",
            if self.vuln_ingest_endpoint.is_empty() {
                "unset".to_string()
            } else {
                redact_url(&self.vuln_ingest_endpoint)
            },
        );
        push(
            "FLAWRENCE_WORKTREE_ISOLATION",
            on_off(self.worktree_isolation),
        );
        push("FLAWRENCE_PREWARM_BUILD", on_off(self.prewarm_build));
        push(
            "FLAWRENCE_PREWARM_TIMEOUT_SECS",
            self.prewarm_timeout_secs.to_string(),
        );
        push(
            "VULN_INGEST_TOKEN_PATH",
            optional_ref(self.vuln_ingest_token_path.as_deref(), "unset"),
        );
        push(
            "ANTHROPIC_API_KEY",
            secret_state(self.anthropic_api_key.as_deref()),
        );
        push(
            "CLOUDFLARE_API_TOKEN",
            secret_state(self.cf_api_token.as_deref()),
        );
        push(
            "CLOUDFLARE_ACCOUNT_ID",
            secret_state(self.cf_account_id.as_deref()),
        );
        push(
            "OPENAI_API_KEY",
            secret_state(self.openai_api_key.as_deref()),
        );
        push(
            "OPENCODE_ACCESS_TOKEN",
            secret_state(self.opencode_access_token.as_deref()),
        );

        let mut provider_names: Vec<&String> = self.providers.keys().collect();
        provider_names.sort();
        for name in provider_names {
            let provider = &self.providers[name];
            push(
                &format!("FLAWRENCE_PROVIDER_{}_BASE_URL", name.to_uppercase()),
                redact_url(&provider.base_url),
            );
            push(
                &format!("FLAWRENCE_PROVIDER_{}_API_KEY", name.to_uppercase()),
                secret_state(Some(provider.api_key.as_str())),
            );
            for header in provider.headers.keys() {
                push(
                    &format!(
                        "FLAWRENCE_PROVIDER_{}_HEADER_{}",
                        name.to_uppercase(),
                        header.to_uppercase()
                    ),
                    "set (redacted)".to_string(),
                );
            }
        }

        for name in EXTRA_ENV_SETTINGS {
            push(name, optional_ref(env::var(name).ok().as_deref(), "unset"));
        }
        for name in SECRET_ENV_SETTINGS {
            push(name, secret_state(env::var(name).ok().as_deref()));
        }

        settings
    }
}

fn optional<T: std::fmt::Display>(value: Option<T>, fallback: &str) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn optional_ref(value: Option<&str>, fallback: &str) -> String {
    value
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

fn on_off(value: bool) -> String {
    if value {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

fn optional_url(value: Option<&str>) -> String {
    value
        .filter(|v| !v.is_empty())
        .map(redact_url)
        .unwrap_or_else(|| "unset (provider default)".to_string())
}

/// Report only whether a credential is configured, never its value.
fn secret_state(value: Option<&str>) -> String {
    match value {
        Some(value) if !value.trim().is_empty() => "set (redacted)".to_string(),
        _ => "unset".to_string(),
    }
}

/// Mask `user:password@` userinfo so a URL can be shown without leaking a
/// credential someone embedded in it.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((authority, tail)) = rest.split_once('/') else {
        return match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        };
    };
    match authority.split_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}/{tail}"),
        None => url.to_string(),
    }
}

fn validate_context_limit(limit: u32, output: u32) -> Result<u32> {
    anyhow::ensure!(limit > output, "MAX_CONTEXT_TOKENS must exceed MAX_TOKENS");
    Ok(limit)
}

fn validate_positive_token_budget(value: u64) -> Result<u64> {
    anyhow::ensure!(value > 0, "MAX_TOTAL_TOKENS must be greater than zero");
    Ok(value)
}

fn validate_positive_cost_budget(value: f64) -> Result<f64> {
    anyhow::ensure!(
        value.is_finite() && value > 0.0,
        "MAX_COST_USD must be a finite number greater than zero"
    );
    Ok(value)
}

fn parse_token_cost_override(
    input: Option<String>,
    output: Option<String>,
    cache_read: Option<String>,
) -> Result<Option<TokenCostOverride>> {
    match (input, output) {
        (None, None) => {
            anyhow::ensure!(
                cache_read.is_none(),
                "TOKEN_COST_CACHE_READ requires TOKEN_COST_INPUT and TOKEN_COST_OUTPUT"
            );
            Ok(None)
        }
        (Some(_), None) | (None, Some(_)) => anyhow::bail!(
            "TOKEN_COST_INPUT and TOKEN_COST_OUTPUT must either both be set or both be unset"
        ),
        (Some(input), Some(output)) => Ok(Some(TokenCostOverride {
            input_per_mtok: parse_token_cost("TOKEN_COST_INPUT", &input)?,
            output_per_mtok: parse_token_cost("TOKEN_COST_OUTPUT", &output)?,
            cache_read_per_mtok: cache_read
                .as_deref()
                .map(|value| parse_token_cost("TOKEN_COST_CACHE_READ", value))
                .transpose()?,
        })),
    }
}

fn parse_token_cost(name: &str, value: &str) -> Result<f64> {
    let cost = value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("{name} must be a valid finite number"))?;
    anyhow::ensure!(
        cost.is_finite() && cost >= 0.0,
        "{name} must be a finite number greater than or equal to zero"
    );
    Ok(cost)
}

/// Parse a boolean switch from the environment.
///
/// Unparseable values fall back to the default rather than failing the run: a
/// toggle typo must not cost a multi-hour analysis, and the effective value is
/// reported in the run configuration either way.
fn parse_parallel_agent_limit(raw: Option<&str>) -> Result<usize> {
    let limit = raw
        .map(str::trim)
        .map(str::parse)
        .transpose()
        .context("FLAWRENCE_MAX_PARALLEL_AGENTS must be a positive integer")?
        .unwrap_or(DEFAULT_MAX_PARALLEL_AGENTS);
    anyhow::ensure!(
        (1..=tokio::sync::Semaphore::MAX_PERMITS).contains(&limit),
        "FLAWRENCE_MAX_PARALLEL_AGENTS must be a positive integer no greater than {}",
        tokio::sync::Semaphore::MAX_PERMITS
    );
    Ok(limit)
}

/// A positive integer from the environment, falling back to `default`. An
/// unparseable or zero value warns and keeps the default rather than failing a
/// run hours in.
fn parse_u64_env(name: &str, default: u64) -> u64 {
    parse_u64_value(env::var(name).ok().as_deref(), name, default)
}

/// The parsing half, split from the lookup so it can be tested without
/// mutating process environment — which races with every other test in the
/// binary under edition 2024.
fn parse_u64_value(raw: Option<&str>, name: &str, default: u64) -> u64 {
    let Some(raw) = raw else { return default };
    match raw.trim().parse::<u64>() {
        Ok(value) if value > 0 => value,
        _ => {
            eprintln!("  Warning: {name}='{raw}' is not a positive integer; using {default}");
            default
        }
    }
}

fn parse_bool_env(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            "" => default,
            other => {
                eprintln!("  Warning: {name}='{other}' is not a boolean; using {default}");
                default
            }
        },
        Err(_) => default,
    }
}

/// Parse an optional numeric environment variable.
///
/// Returns `Ok(None)` when the variable is unset or blank (the "unlimited"
/// case), `Ok(Some(value))` when it parses, and `Err` when a non-empty value
/// fails to parse — invalid budgets fail fast rather than silently disabling
/// the cap.
fn parse_optional_env<T: std::str::FromStr>(name: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{e}")),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn context_limit_must_leave_room_beyond_the_output_reservation() {
        assert_eq!(
            super::validate_context_limit(524_288, 65_536).unwrap(),
            524_288
        );
        assert!(super::validate_context_limit(65_536, 65_536).is_err());
        assert!(super::validate_context_limit(0, 65_536).is_err());
    }
    /// A bad value must never abort a run that is already hours in, and zero
    /// is a bad value: a zero-second cap would kill every build instantly.
    #[test]
    fn a_bad_timeout_falls_back_instead_of_failing_the_run() {
        use super::parse_u64_value;
        let name = "FLAWRENCE_PREWARM_TIMEOUT_SECS";
        assert_eq!(parse_u64_value(None, name, 5400), 5400);
        assert_eq!(parse_u64_value(Some("3600"), name, 5400), 3600);
        assert_eq!(parse_u64_value(Some("  3600  "), name, 5400), 3600);
        assert_eq!(parse_u64_value(Some("0"), name, 5400), 5400);
        assert_eq!(parse_u64_value(Some("soon"), name, 5400), 5400);
        assert_eq!(parse_u64_value(Some(""), name, 5400), 5400);
        assert_eq!(parse_u64_value(Some("-5"), name, 5400), 5400);
    }

    use super::*;

    #[test]
    fn describe_never_leaks_secret_values() {
        let config = Config {
            anthropic_api_key: Some("sk-ant-supersecret".to_string()),
            cf_api_token: Some("cf-token".to_string()),
            openai_api_key: Some("sk-openai-secret".to_string()),
            opencode_access_token: Some("oc-token".to_string()),
            anthropic_base_url: Some("https://user:pw@proxy.internal/v1".to_string()),
            providers: HashMap::from([(
                "vllm".to_string(),
                ProviderConfig {
                    base_url: "https://vllm.internal/v1".to_string(),
                    api_key: "provider-secret".to_string(),
                    headers: std::collections::BTreeMap::from([(
                        "X-Opencode-Session".to_string(),
                        "sess-secret".to_string(),
                    )]),
                },
            )]),
            ..blank_config()
        };

        let described = config.describe();
        let rendered = described
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("\n");

        for secret in [
            "sk-ant-supersecret",
            "cf-token",
            "oc-token",
            "sk-openai-secret",
            "provider-secret",
            "user:pw",
        ] {
            assert!(
                !rendered.contains(secret),
                "described settings leaked {secret}: {rendered}"
            );
        }
        assert!(rendered.contains("ANTHROPIC_API_KEY=set (redacted)"));
        assert!(rendered.contains("FLAWRENCE_PROVIDER_VLLM_API_KEY=set (redacted)"));
        assert!(
            rendered.contains("FLAWRENCE_PROVIDER_VLLM_HEADER_X-OPENCODE-SESSION=set (redacted)")
        );
        assert!(!rendered.contains("sess-secret"));
        assert!(rendered.contains("https://***@proxy.internal/v1"));
        assert!(rendered.contains("https://vllm.internal/v1"));
    }

    #[test]
    fn describe_reports_the_documented_limits() {
        let config = Config {
            max_iterations: 42,
            max_agent_depth: 3,
            max_tokens: 1234,
            max_total_tokens: Some(999),
            max_context_tokens: Some(524_288),
            agent_memory_mb: Some(1024),
            ..blank_config()
        };
        let described = config.describe();
        let lookup = |name: &str| {
            described
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("{name} missing from run configuration"))
        };
        assert_eq!(lookup("MAX_ITERATIONS"), "42");
        assert_eq!(lookup("MAX_AGENT_DEPTH"), "3");
        assert_eq!(lookup("MAX_TOKENS"), "1234");
        assert_eq!(lookup("MAX_CONTEXT_TOKENS"), "524288");
        assert_eq!(lookup("FLAWRENCE_AGENT_MEMORY_MB"), "1024");
        assert_eq!(lookup("MAX_TOTAL_TOKENS"), "999");
        assert_eq!(lookup("MAX_COST_USD"), "unlimited");
    }

    fn blank_config() -> Config {
        Config {
            cf_api_token: None,
            cf_account_id: None,
            max_iterations: 500,
            max_agent_depth: 5,
            max_parallel_agents: 40,
            agent_memory_mb: None,
            max_tokens: 65536,
            max_total_tokens: None,
            max_context_tokens: None,
            max_cost_usd: None,
            token_cost_override: None,
            proxy_url: None,
            client_mode: ClientMode::Streaming,
            anthropic_api_key: None,
            openai_api_key: None,
            opencode_access_token: None,
            model_override: None,
            anthropic_base_url: None,
            openai_base_url: None,
            workers_ai_base_url: None,
            vuln_ingest_endpoint: String::new(),
            vuln_ingest_token_path: None,
            worktree_isolation: false,
            prewarm_build: true,
            prewarm_timeout_secs: 5400,
            providers: HashMap::new(),
        }
    }

    #[test]
    fn parallel_agent_limit_defaults_to_forty_and_rejects_invalid_values() {
        assert_eq!(parse_parallel_agent_limit(None).unwrap(), 40);
        assert_eq!(parse_parallel_agent_limit(Some(" 4 ")).unwrap(), 4);
        for raw in ["0", "-1", "bad", "", "99999999999999999999999999999"] {
            assert!(parse_parallel_agent_limit(Some(raw)).is_err());
        }
        assert_eq!(
            blank_config()
                .describe()
                .into_iter()
                .find(|(key, _)| key == "FLAWRENCE_MAX_PARALLEL_AGENTS")
                .unwrap()
                .1,
            "40"
        );
    }

    #[test]
    fn describe_reports_the_isolation_switches() {
        let described = Config {
            worktree_isolation: true,
            prewarm_build: false,
            prewarm_timeout_secs: 5400,
            ..blank_config()
        }
        .describe();
        let lookup = |name: &str| {
            described
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("{name} missing from run configuration"))
        };
        assert_eq!(lookup("FLAWRENCE_WORKTREE_ISOLATION"), "true");
        assert_eq!(lookup("FLAWRENCE_PREWARM_BUILD"), "false");
    }

    #[test]
    fn budget_values_must_be_positive_and_finite() {
        assert!(validate_positive_token_budget(0).is_err());
        assert!(validate_positive_token_budget(1).is_ok());
        assert!(validate_positive_cost_budget(0.0).is_err());
        assert!(validate_positive_cost_budget(f64::NAN).is_err());
        assert!(validate_positive_cost_budget(f64::INFINITY).is_err());
        assert!(validate_positive_cost_budget(0.01).is_ok());
    }

    #[test]
    fn token_cost_overrides_must_be_paired_and_nonnegative() {
        assert!(parse_token_cost_override(Some("1".to_string()), None, None).is_err());
        assert!(parse_token_cost_override(None, Some("1".to_string()), None).is_err());
        assert!(parse_token_cost_override(None, None, Some("1".to_string())).is_err());
        assert!(
            parse_token_cost_override(Some("-1".to_string()), Some("1".to_string()), None).is_err()
        );
        assert!(
            parse_token_cost_override(
                Some("1".to_string()),
                Some("2".to_string()),
                Some("0.5".to_string())
            )
            .is_ok()
        );
    }
}
