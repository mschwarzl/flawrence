use anyhow::{Context, Result};
use flawrence_chat::UsageByModel;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::ToolContext;
use crate::registry::ToolDef;

fn token_from_dotenv() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let body = std::fs::read_to_string(std::path::Path::new(&home).join(".env")).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("GITHUB_TOKEN=")
            .or_else(|| line.strip_prefix("GH_TOKEN="))
        {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn github_token() -> Result<String> {
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .or_else(token_from_dotenv)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "GITHUB_TOKEN or GH_TOKEN must be set (env or ~/.env) for GitHub operations"
            )
        })?;
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("GITHUB_TOKEN or GH_TOKEN is set but empty");
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        anyhow::bail!("GITHUB_TOKEN contains newline characters — refusing to use");
    }
    Ok(trimmed)
}

fn github_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("flawrence/0.1")
        .build()
        .context("Failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// Create a draft security advisory (optionally with private fork)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHubCreateAdvisoryArgs {
    /// Repository owner (org or user).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Advisory summary (max 1024 chars).
    pub summary: String,
    /// Detailed description of the vulnerability.
    pub description: String,
    /// Severity: critical, high, medium, low.
    pub severity: String,
    /// CWE IDs (e.g., ["CWE-79"]). Null means none are claimed; an empty list means the
    /// same thing, so prefer null.
    pub cwe_ids: Option<Vec<String>>,
    /// Whether to create a temporary private fork for collaboration.
    #[serde(default)]
    pub start_private_fork: bool,
}

impl ToolDef for GitHubCreateAdvisoryArgs {
    const NAME: &'static str = "github_create_advisory";
    const DESCRIPTION: &'static str = "Create a draft GitHub Security Advisory (GHSA) on a repository. Optionally starts a temporary private fork for patch collaboration. Requires GITHUB_TOKEN or GH_TOKEN with repository_advisories:write scope.";

    async fn execute(_ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let token = github_token()?;
        let client = github_client()?;

        let mut body = serde_json::json!({
            "summary": args.summary,
            "description": args.description,
            "severity": args.severity,
            "vulnerabilities": [],
        });

        if let Some(ref cwes) = args.cwe_ids {
            body["cwe_ids"] = serde_json::json!(cwes);
        }
        if args.start_private_fork {
            body["start_private_fork"] = serde_json::json!(true);
        }

        let url = format!(
            "https://api.github.com/repos/{}/{}/security-advisories",
            args.owner, args.repo
        );

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .context("Failed to send advisory creation request")?;

        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse advisory response")?;

        if !status.is_success() {
            let msg = resp_body["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("GitHub API returned {}: {}", status.as_u16(), msg);
        }

        let ghsa_id = resp_body["ghsa_id"].as_str().unwrap_or("unknown");
        let html_url = resp_body["html_url"].as_str().unwrap_or("(no URL)");
        let state = resp_body["state"].as_str().unwrap_or("draft");
        let fork_info = if args.start_private_fork {
            resp_body["private_fork"]["full_name"]
                .as_str()
                .map(|f| format!(", private fork: {}", f))
                .unwrap_or_default()
        } else {
            String::new()
        };

        Ok((
            format!(
                "Created advisory {} ({}): {}{}",
                ghsa_id, state, html_url, fork_info
            ),
            None,
        ))
    }
}

// ---------------------------------------------------------------------------
// Create a temporary private fork from an existing advisory
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHubCreateAdvisoryForkArgs {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// GHSA identifier of the advisory (e.g., "GHSA-xxxx-xxxx-xxxx").
    pub ghsa_id: String,
}

impl ToolDef for GitHubCreateAdvisoryForkArgs {
    const NAME: &'static str = "github_create_advisory_fork";
    const DESCRIPTION: &'static str = "Create a temporary private fork for an existing GitHub Security Advisory. The fork is used to collaborate on a patch privately before the advisory is published. Requires GITHUB_TOKEN or GH_TOKEN with repository_advisories:write scope.";

    async fn execute(_ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let token = github_token()?;
        let client = github_client()?;

        let url = format!(
            "https://api.github.com/repos/{}/{}/security-advisories/{}/forks",
            args.owner, args.repo, args.ghsa_id
        );

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to send fork creation request")?;

        let status = resp.status();
        let resp_body: serde_json::Value =
            resp.json().await.context("Failed to parse fork response")?;

        if !status.is_success() {
            let msg = resp_body["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("GitHub API returned {}: {}", status.as_u16(), msg);
        }

        let full_name = resp_body["full_name"].as_str().unwrap_or("unknown");
        let clone_url = resp_body["clone_url"].as_str().unwrap_or("(no clone URL)");
        let html_url = resp_body["html_url"].as_str().unwrap_or("(no URL)");

        Ok((
            format!(
                "Created private fork '{}' for {}: {}\nClone: {}",
                full_name, args.ghsa_id, html_url, clone_url
            ),
            None,
        ))
    }
}

// ---------------------------------------------------------------------------
// Privately report a vulnerability (external-reporter endpoint)
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AffectedPackage {
    /// Package ecosystem (e.g. "maven", "pip", "npm", "other").
    pub ecosystem: String,
    /// Package name (e.g. "io.trino:trino-geospatial").
    pub name: String,
    /// Affected version range, e.g. "<= 481". Null means the range is unknown.
    pub vulnerable_version_range: Option<String>,
    /// First patched version. Null means no fix is known or released yet.
    pub patched_versions: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHubReportVulnerabilityArgs {
    /// Repository owner (org or user).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// One-line summary (max 1024 chars). Becomes the advisory title.
    pub summary: String,
    /// Full report body. Use the Summary/Details/Reachable via/Tested on/PoC/Impact/Fix template.
    pub description: String,
    /// Severity: critical, high, medium, low.
    pub severity: String,
    /// CWE IDs, e.g. ["CWE-611"]. Null means none are claimed.
    pub cwe_ids: Option<Vec<String>>,
    /// Affected packages.
    #[serde(default)]
    pub vulnerabilities: Vec<AffectedPackage>,
    /// Request a temporary private fork for patch collaboration.
    #[serde(default)]
    pub start_private_fork: bool,
    /// Local artifact key for the dry-run/response record (e.g. "ghsa_trino_26").
    pub record_key: String,
    /// If true, write the wire payload to the artifact store and return WITHOUT posting.
    #[serde(default)]
    pub dry_run: bool,
}

impl ToolDef for GitHubReportVulnerabilityArgs {
    const NAME: &'static str = "github_report_vulnerability";
    const DESCRIPTION: &'static str = "Privately report a security vulnerability to a repository via POST /repos/{o}/{r}/security-advisories/reports. Works for any user when the target repo has private vulnerability reporting enabled. Hard guard: refuses to POST unless FLAWRENCE_GHSA_LIVE=1 is set. With dry_run=true, writes the exact wire payload to the artifact store and returns without any network call.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        if args.summary.len() > 1024 {
            anyhow::bail!(
                "summary exceeds 1024 characters ({} chars)",
                args.summary.len()
            );
        }
        let sev = args.severity.to_lowercase();
        if !matches!(sev.as_str(), "critical" | "high" | "medium" | "low") {
            anyhow::bail!(
                "severity must be one of critical/high/medium/low, got '{}'",
                args.severity
            );
        }

        let url = format!(
            "https://api.github.com/repos/{}/{}/security-advisories/reports",
            args.owner, args.repo
        );

        let vulns: Vec<serde_json::Value> = args
            .vulnerabilities
            .iter()
            .map(|p| {
                let mut v = serde_json::json!({
                    "package": { "ecosystem": p.ecosystem, "name": p.name },
                });
                if let Some(r) = &p.vulnerable_version_range {
                    v["vulnerable_version_range"] = serde_json::json!(r);
                }
                if let Some(pv) = &p.patched_versions {
                    v["patched_versions"] = serde_json::json!(pv);
                }
                v
            })
            .collect();

        let mut body = serde_json::json!({
            "summary": args.summary,
            "description": args.description,
            "severity": sev,
            "vulnerabilities": vulns,
        });
        if let Some(ref cwes) = args.cwe_ids {
            body["cwe_ids"] = serde_json::json!(cwes);
        }
        if args.start_private_fork {
            body["start_private_fork"] = serde_json::json!(true);
        }

        let record = serde_json::json!({
            "url": url,
            "method": "POST",
            "headers": {
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            "body": body,
        });
        let record_str = serde_json::to_string_pretty(&record)?;

        {
            let mut store = ctx.artifact_store.write().await;
            store
                .write_with_extension(
                    format!("{}_payload", args.record_key),
                    record_str.clone(),
                    "json",
                )
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }

        if args.dry_run {
            return Ok((
                format!(
                    "DRY RUN: payload for {} written to artifact '{}_payload' ({} bytes). No network call made.",
                    url,
                    args.record_key,
                    record_str.len()
                ),
                None,
            ));
        }

        if std::env::var("FLAWRENCE_GHSA_LIVE").as_deref() != Ok("1") {
            return Ok((
                format!(
                    "BLOCKED: live POST to {} requires FLAWRENCE_GHSA_LIVE=1. Payload saved to artifact '{}_payload'. Set the env var only for the single approved submission.",
                    url, args.record_key
                ),
                None,
            ));
        }

        let token = github_token()?;
        let client = github_client()?;

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .context("Failed to send vulnerability report")?;

        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse report response")?;

        {
            let mut store = ctx.artifact_store.write().await;
            let _ = store.write_with_extension(
                format!("{}_response", args.record_key),
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": status.as_u16(),
                    "body": resp_body,
                }))?,
                "json",
            );
        }

        if !status.is_success() {
            let msg = resp_body["message"].as_str().unwrap_or("unknown error");
            let errs = resp_body["errors"].to_string();
            anyhow::bail!(
                "GitHub API returned {}: {} (errors: {})",
                status.as_u16(),
                msg,
                errs
            );
        }

        let ghsa_id = resp_body["ghsa_id"].as_str().unwrap_or("unknown");
        let html_url = resp_body["html_url"].as_str().unwrap_or("(no URL)");
        let fork = resp_body["private_fork"]["full_name"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}/{}-{}", args.owner, args.repo, ghsa_id.to_lowercase()));
        let fork_clone = resp_body["private_fork"]["clone_url"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://github.com/{}.git", fork));

        Ok((
            format!(
                "Filed private vulnerability report {}: {} (state: {})\nprivate_fork: {}\nfork_clone_url: {}",
                ghsa_id,
                html_url,
                resp_body["state"].as_str().unwrap_or("triage"),
                fork,
                fork_clone
            ),
            None,
        ))
    }
}
