//! Launcher connection validation and optional OpenAI-compatible model discovery.
//!
//! Credentials are checked locally for completeness; discovery errors never imply
//! that a manually entered model cannot be used.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use serde::Serialize;

const BUILTIN_KEYS: [&str; 8] = [
    "ANTHROPIC_API_KEY",
    "FLAWRENCE_ANTHROPIC_BASE_URL",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "FLAWRENCE_WORKERS_AI_BASE_URL",
    "OPENAI_API_KEY",
    "OPENCODE_ACCESS_TOKEN",
    "FLAWRENCE_OPENAI_BASE_URL",
];
const MAX_MODELS_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 10_000;

/// Configuration status, without exposing credentials or making a live API call.
#[derive(Serialize)]
pub(super) struct ProviderStatus {
    pub id: String,
    pub custom: bool,
    pub available: bool,
    pub problem: Option<String>,
}

fn custom_name(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("FLAWRENCE_PROVIDER_")?;
    rest.strip_suffix("_BASE_URL")
        .or_else(|| rest.strip_suffix("_API_KEY"))
}

/// Environment keys owned by the provider editor, including incomplete pairs.
pub(super) fn is_provider_key(key: &str) -> bool {
    BUILTIN_KEYS.contains(&key) || custom_name(key).is_some()
}

fn valid_identifier(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && !["anthropic", "openai", "workers-ai"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn validate_endpoint(raw: &str, https_only: bool) -> Result<()> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("Invalid API base URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && (!https_only || url.scheme() == "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Use an {} API base URL without credentials, query, or fragment",
        if https_only { "HTTPS" } else { "HTTP(S)" }
    );
    Ok(())
}

fn status(
    id: &str,
    custom: bool,
    required: &[&str],
    endpoint: &str,
    values: &HashMap<String, String>,
) -> ProviderStatus {
    let problem = required
        .iter()
        .find(|key| {
            values
                .get(**key)
                .is_none_or(|value| value.trim().is_empty())
        })
        .map(|key| format!("Missing {key}"))
        .or_else(|| {
            values
                .get(endpoint)
                .filter(|value| !value.is_empty())
                .and_then(|url| validate_endpoint(url, id == "openai" && !custom).err())
                .map(|error| error.to_string())
        });
    ProviderStatus {
        id: id.to_lowercase(),
        custom,
        available: problem.is_none(),
        problem,
    }
}

/// `openai/` takes an API key, or a gateway token together with an explicit endpoint.
fn openai_status(values: &HashMap<String, String>) -> ProviderStatus {
    let has = |key: &str| {
        values
            .get(key)
            .is_some_and(|value| !value.trim().is_empty())
    };
    let required: &[&str] = if has("OPENAI_API_KEY") {
        &[]
    } else if has("OPENCODE_ACCESS_TOKEN") {
        &["FLAWRENCE_OPENAI_BASE_URL"]
    } else {
        &["OPENAI_API_KEY"]
    };
    status(
        "openai",
        false,
        required,
        "FLAWRENCE_OPENAI_BASE_URL",
        values,
    )
}

/// All built-ins and every custom pair found in the effective environment.
pub(super) fn configured_providers(values: &HashMap<String, String>) -> Vec<ProviderStatus> {
    let mut providers = vec![
        status(
            "anthropic",
            false,
            &["ANTHROPIC_API_KEY"],
            "FLAWRENCE_ANTHROPIC_BASE_URL",
            values,
        ),
        status(
            "workers-ai",
            false,
            &["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID"],
            "FLAWRENCE_WORKERS_AI_BASE_URL",
            values,
        ),
        openai_status(values),
    ];
    let names: BTreeSet<_> = values.keys().filter_map(|key| custom_name(key)).collect();
    let mut counts = HashMap::<String, u32>::new();
    for name in &names {
        let count = counts.entry(name.to_lowercase()).or_default();
        *count = count.saturating_add(1);
    }
    for name in names {
        let endpoint = format!("FLAWRENCE_PROVIDER_{name}_BASE_URL");
        let key = format!("FLAWRENCE_PROVIDER_{name}_API_KEY");
        let mut provider = status(name, true, &[&endpoint, &key], &endpoint, values);
        if !valid_identifier(name) {
            provider.available = false;
            provider.problem = Some("Use an alphanumeric identifier starting with a letter; built-in names are reserved".into());
        } else if counts[&name.to_lowercase()] > 1 {
            provider.available = false;
            provider.problem = Some("Duplicate provider identifier (case insensitive)".into());
        }
        providers.push(provider);
    }
    providers
}

/// Missing connections or an unavailable global override block structured launches.
/// An absent override deliberately leaves validation of agent-defined models to the runner.
pub(super) fn model_error(
    values: &HashMap<String, String>,
    providers: &[ProviderStatus],
) -> Option<String> {
    if !providers.iter().any(|provider| provider.available) {
        return Some("Configure at least one available provider before starting a run".into());
    }
    let model = values
        .get("FLAWRENCE_MODEL")
        .filter(|value| !value.trim().is_empty())?;
    let (prefix, id) = model.split_once('/').unwrap_or(("workers-ai", model));
    if prefix.is_empty() {
        return Some("Choose a provider for the model override".into());
    }
    if id.is_empty() {
        return Some("Enter a model identifier after the provider prefix".into());
    }
    (!providers.iter().any(|provider| provider.available && provider.id == prefix))
        .then(|| format!("Model references unavailable provider '{prefix}'; configure it or clear the model override"))
}

/// Discover IDs only, with bounded time and response size and no credential-bearing redirects.
pub(super) async fn discover_models(
    values: &HashMap<String, String>,
    id: &str,
) -> Result<Vec<String>> {
    ensure!(
        configured_providers(values)
            .iter()
            .any(|provider| provider.id == id && provider.custom && provider.available),
        "Select an available OpenAI-compatible provider to discover models"
    );
    let endpoint_key = values
        .keys()
        .find(|key| {
            key.ends_with("_BASE_URL")
                && custom_name(key).is_some_and(|name| name.eq_ignore_ascii_case(id))
        })
        .ok_or_else(|| anyhow::anyhow!("Provider endpoint is unavailable"))?;
    let name = custom_name(endpoint_key).unwrap_or_default();
    let key = &values[&format!("FLAWRENCE_PROVIDER_{name}_API_KEY")];
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut response = client
        .get(format!(
            "{}/models",
            values[endpoint_key].trim_end_matches('/')
        ))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot discover models; enter a model ID manually"))?;
    ensure!(
        response.status().is_success(),
        "Models endpoint returned HTTP {}; manual entry is available",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot read model list"))?
    {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_MODELS_BYTES,
            "Model list exceeds 2 MiB; enter a model ID manually"
        );
        bytes.extend_from_slice(&chunk);
    }
    parse_models(&bytes)
}

fn parse_models(bytes: &[u8]) -> Result<Vec<String>> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        bail!("Models endpoint returned invalid JSON; enter a model ID manually");
    };
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Models endpoint did not return a data array"))?;
    let mut ids: Vec<_> = data
        .iter()
        .filter_map(|model| model.get("id").and_then(serde_json::Value::as_str))
        .filter(|id| !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control))
        .take(MAX_MODELS)
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, extract::Request, http::StatusCode, response::Redirect, routing::get,
    };
    use serde_json::json;

    fn values(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn openai_gateway_token_needs_an_endpoint() {
        let openai = |pairs: &[(&str, &str)]| {
            configured_providers(&values(pairs))
                .into_iter()
                .find(|provider| provider.id == "openai" && !provider.custom)
                .unwrap()
        };
        assert!(!openai(&[]).available);
        assert!(!openai(&[("OPENCODE_ACCESS_TOKEN", "token")]).available);
        assert!(
            openai(&[
                ("OPENCODE_ACCESS_TOKEN", "token"),
                ("FLAWRENCE_OPENAI_BASE_URL", "https://gateway.invalid/v1"),
            ])
            .available
        );
        assert!(openai(&[("OPENAI_API_KEY", "sk-test")]).available);
    }

    #[test]
    fn complete_connections_are_available_and_incomplete_or_invalid_ones_are_reported() {
        let connections = values(&[
            ("ANTHROPIC_API_KEY", "anthropic-secret"),
            ("CLOUDFLARE_API_TOKEN", "workers-secret"),
            ("CLOUDFLARE_ACCOUNT_ID", "account"),
            ("OPENAI_API_KEY", "openai-secret"),
            (
                "FLAWRENCE_PROVIDER_LOCAL_BASE_URL",
                "http://localhost:8000/v1",
            ),
            ("FLAWRENCE_PROVIDER_LOCAL_API_KEY", "custom-secret"),
            (
                "FLAWRENCE_PROVIDER_SECOND_BASE_URL",
                "https://second.invalid/v1",
            ),
            ("FLAWRENCE_PROVIDER_SECOND_API_KEY", "second-secret"),
            (
                "FLAWRENCE_PROVIDER_INCOMPLETE_BASE_URL",
                "https://incomplete.invalid/v1",
            ),
            (
                "FLAWRENCE_PROVIDER_BAD_NAME_BASE_URL",
                "https://bad.invalid/v1",
            ),
            ("FLAWRENCE_PROVIDER_BAD_NAME_API_KEY", "bad-secret"),
            (
                "FLAWRENCE_PROVIDER_OPENAI_BASE_URL",
                "https://reserved.invalid/v1",
            ),
            ("FLAWRENCE_PROVIDER_OPENAI_API_KEY", "reserved-secret"),
        ]);
        let providers = configured_providers(&connections);
        for id in ["anthropic", "workers-ai", "openai", "local", "second"] {
            assert!(
                providers
                    .iter()
                    .any(|provider| provider.id == id && provider.available)
            );
        }
        for id in ["incomplete", "bad_name"] {
            assert!(
                providers
                    .iter()
                    .any(|provider| provider.id == id && !provider.available)
            );
        }
        assert!(
            providers
                .iter()
                .any(|provider| provider.id == "openai" && provider.custom && !provider.available)
        );
        assert!(
            !serde_json::to_string(&providers)
                .unwrap()
                .contains("-secret")
        );
    }

    #[test]
    fn endpoint_validation_keeps_optional_defaults_and_rejects_unsafe_or_invalid_urls() {
        for raw in [
            "not-a-url",
            "file:///tmp/models",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/#secret",
        ] {
            assert!(validate_endpoint(raw, false).is_err());
        }
        assert!(validate_endpoint("http://localhost:8000/v1", false).is_ok());
        assert!(validate_endpoint("http://localhost:8000/v1", true).is_err());
        let providers = configured_providers(&values(&[("ANTHROPIC_API_KEY", "secret")]));
        assert!(
            providers
                .iter()
                .any(|provider| provider.id == "anthropic" && provider.available)
        );
    }

    #[test]
    fn empty_override_uses_agent_models_and_explicit_models_need_an_available_provider() {
        let mut connections = values(&[("ANTHROPIC_API_KEY", "secret")]);
        let providers = configured_providers(&connections);
        assert!(model_error(&connections, &providers).is_none());
        for model in ["unknown/model", "openai/model", "anthropic/", "/model"] {
            connections.insert("FLAWRENCE_MODEL".into(), model.into());
            assert!(model_error(&connections, &providers).is_some());
        }
        connections.insert("FLAWRENCE_MODEL".into(), "anthropic/model".into());
        assert!(model_error(&connections, &providers).is_none());
        assert!(model_error(&HashMap::new(), &configured_providers(&HashMap::new())).is_some());
    }

    #[test]
    fn model_discovery_preserves_served_ids_and_ignores_pricing_metadata() {
        let ids = parse_models(
            br#"{"data":[
            {"id":"org/model-instant","model_id":"org/wrong-base","pricing":{"prompt":"9"}},
            {"id":"org/model-instant"},{"id":"another/model"},{"id":""},{"id":12}
        ]}"#,
        )
        .unwrap();
        assert_eq!(ids, ["another/model", "org/model-instant"]);
        assert!(parse_models(br#"{"data":[]}"#).unwrap().is_empty());
        assert!(parse_models(br#"{"data":{}}"#).is_err());
        assert!(
            !parse_models(b"upstream secret")
                .unwrap_err()
                .to_string()
                .contains("upstream secret")
        );
    }

    #[tokio::test]
    async fn discovery_uses_the_configured_api_root_and_bearer_token_without_redirecting_or_leaking_errors()
     {
        let app = Router::new()
            .route(
                "/v1/models",
                get(|request: Request| async move {
                    assert_eq!(request.headers()["authorization"], "Bearer test-secret");
                    Json(json!({"data": [{"id": "org/model"}]}))
                }),
            )
            .route(
                "/denied/models",
                get(|| async { (StatusCode::UNAUTHORIZED, "upstream secret") }),
            )
            .route(
                "/redirect/models",
                get(|| async { Redirect::temporary("/v1/models") }),
            )
            .route(
                "/large/models",
                get(|| async { "x".repeat(MAX_MODELS_BYTES + 1) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let mut connections = values(&[
            ("FLAWRENCE_PROVIDER_LOCAL_BASE_URL", &format!("{root}/v1/")),
            ("FLAWRENCE_PROVIDER_LOCAL_API_KEY", "test-secret"),
        ]);
        assert_eq!(
            discover_models(&connections, "local").await.unwrap(),
            ["org/model"]
        );
        for (path, message) in [("denied", "401"), ("redirect", "307"), ("large", "2 MiB")] {
            connections.insert(
                "FLAWRENCE_PROVIDER_LOCAL_BASE_URL".into(),
                format!("{root}/{path}"),
            );
            let error = discover_models(&connections, "local")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
            assert!(!error.contains("secret"));
        }
        assert!(discover_models(&connections, "anthropic").await.is_err());
        server.abort();
    }
}
