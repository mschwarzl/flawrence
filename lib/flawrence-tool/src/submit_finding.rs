use anyhow::{Context, Result};
use flawrence_chat::UsageByModel;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::advisory::{WriteAdvisoryArgs, render_ghsa};
use crate::dedup::{DedupVerdict, run_dedup};
use crate::tool_args::required_string_field_from_value;
use crate::{ToolContext, ToolDef};

/// Strict-teaching deserializer for the required `finding_key` field (see
/// `docs/SPEC_TOOL_ID_ROBUSTNESS.md`). Accepts only a string; a wrong type
/// yields an actionable error.
fn deserialize_finding_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    required_string_field_from_value(Value::deserialize(deserializer)?, "finding_key")
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct SubmitFindingArgs {
    /// Artifact key of the confirmed vulnerability to submit (e.g. `vulnerability_sqli_users`).
    /// Pass it as a quoted string, never a bare number or unquoted token.
    #[serde(deserialize_with = "deserialize_finding_key")]
    pub finding_key: String,
    /// Also publish to VULN_INGEST_ENDPOINT. Local finalization always happens first.
    #[serde(default)]
    pub live: bool,
    /// Bypass only the DUP-STRONG gate.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
struct IngestResponse {
    #[serde(rename = "issueKey")]
    issue_key: Option<String>,
    #[serde(rename = "createdIssue")]
    created_issue: Option<bool>,
    #[serde(rename = "exactDuplicate")]
    exact_duplicate: Option<bool>,
}

fn expand_home(path: &str) -> PathBuf {
    path.strip_prefix("~/")
        .and_then(|rest| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)))
        .unwrap_or_else(|| PathBuf::from(path))
}

fn read_token(configured: Option<&str>, base_dir: &Path, findings_dir: &Path) -> Result<String> {
    let candidates = configured.map_or_else(
        || {
            vec![
                base_dir.join(".token"),
                findings_dir.join(".token"),
                expand_home("~/.token"),
            ]
        },
        |path| vec![expand_home(path)],
    );
    for path in &candidates {
        if let Ok(token) = std::fs::read_to_string(path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }
    anyhow::bail!("No vuln-ingest token found. Looked in: {candidates:?}")
}

fn passes_reproducer_gate(vulnerability: &Value, poc: Option<&str>) -> bool {
    vulnerability
        .pointer("/reproduction/verified")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || poc.is_some_and(|body| body.lines().next().map(str::trim) == Some("STATUS: PASS"))
}

fn trace_reproducer(vulnerability: &Value) -> String {
    let trace = vulnerability.get("trace").and_then(Value::as_array);
    let format_step = |step: Option<&Value>| {
        step.map(|step| {
            format!(
                "{}:{} {}",
                step.get("file").and_then(Value::as_str).unwrap_or(""),
                step.get("line").and_then(Value::as_u64).unwrap_or(0),
                step.get("scope").and_then(Value::as_str).unwrap_or("")
            )
        })
        .unwrap_or_default()
    };
    let first = trace.and_then(|steps| steps.first());
    let last = trace.and_then(|steps| steps.last());
    format!(
        "entrypoint: {}\nsink:       {}\n(source-only; no separate PoC attached)",
        format_step(first),
        format_step(last)
    )
}

fn finalize(mut vulnerability: Value, poc: Option<&str>, commit: Option<&str>) -> Value {
    let poc = poc
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| trace_reproducer(&vulnerability));
    if let Value::Object(fields) = &mut vulnerability {
        fields
            .entry("verdict")
            .or_insert(Value::String("confirmed".into()));
        if let Some(commit) = commit {
            fields
                .entry("commit_hash")
                .or_insert(Value::String(commit.into()));
        }
        fields
            .entry("poc/testcase")
            .or_insert(Value::String(format!("```\n{poc}\n```")));
    }
    vulnerability
}

fn ingest_payload(finalized: &Value) -> Value {
    let mut payload = finalized.clone();
    if let Value::Object(fields) = &mut payload {
        let reproduction = fields.remove("reproduction");
        fields.remove("reason");
        fields.remove("target");
        if let (Some(Value::Object(reproduction)), Some(Value::Object(confidence))) =
            (reproduction, fields.get_mut("confidence"))
        {
            let previous = confidence
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("");
            let method = reproduction
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("");
            let verified = reproduction
                .get("verified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let evidence = reproduction
                .get("evidence")
                .and_then(Value::as_str)
                .unwrap_or("");
            confidence.insert(
                "reason".into(),
                Value::String(
                    format!("{previous} Reproduction ({method}, verified={verified}): {evidence}")
                        .trim()
                        .into(),
                ),
            );
        }
    }
    payload
}

fn save_local(findings_dir: &Path, slug: &str, vulnerability: &Value) -> Result<()> {
    let human_path = findings_dir
        .join("submitted_humanreadable")
        .join(format!("{slug}.md"));
    std::fs::create_dir_all(
        human_path
            .parent()
            .context("human-readable submission path has no parent")?,
    )?;
    let commit = vulnerability.get("commit_hash").and_then(Value::as_str);
    let markdown = render_ghsa(
        vulnerability,
        commit,
        &WriteAdvisoryArgs {
            cwe: None,
            tested_version: None,
            finding_key: format!("vulnerability_{slug}"),
        },
    );
    std::fs::write(human_path, markdown)?;

    let path = findings_dir.join("submitted").join(format!("{slug}.json"));
    std::fs::create_dir_all(path.parent().context("submitted path has no parent")?)?;
    std::fs::write(&path, serde_json::to_vec_pretty(vulnerability)?)?;
    let title = vulnerability
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("");
    let row = format!("| LOCAL | {slug}.json | {title} |\n");
    let index = findings_dir.join("INDEX.md");
    let existing = std::fs::read_to_string(&index).unwrap_or_default();
    if !existing
        .lines()
        .any(|line| line.contains(&format!("| {slug}.json |")))
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(index)?
            .write_all(row.as_bytes())?;
    }
    Ok(())
}

async fn post(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    body: &str,
    source_reference: Option<&str>,
) -> Result<(reqwest::StatusCode, String)> {
    let mut request = client
        .post(endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some(source_reference) = source_reference {
        request = request.header("X-Source-Reference", source_reference);
    }
    let response = request.send().await.context("vuln-ingest POST failed")?;
    let status = response.status();
    Ok((status, response.text().await.unwrap_or_default()))
}

async fn publish(ctx: &ToolContext, findings_dir: &Path, finalized: &Value) -> Result<String> {
    anyhow::ensure!(
        std::env::var("FLAWRENCE_VULN_INGEST_LIVE").as_deref() == Ok("1"),
        "live POST requires FLAWRENCE_VULN_INGEST_LIVE=1"
    );
    let endpoint = &ctx.runtime.config.vuln_ingest_endpoint;
    anyhow::ensure!(
        !endpoint.is_empty(),
        "VULN_INGEST_ENDPOINT is not configured"
    );
    anyhow::ensure!(
        ctx.runtime.git_info.commit_hash.is_some(),
        "commit_hash is unavailable"
    );
    let token = read_token(
        ctx.runtime.config.vuln_ingest_token_path.as_deref(),
        // The run root, not the agent's workspace: the ingest token is placed
        // by the operator next to the checkout, and a worktree created from a
        // commit does not contain it.
        &ctx.runtime.base_directory,
        findings_dir,
    )?;
    let body = serde_json::to_string(&ingest_payload(finalized))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let (mut status, mut text) = post(&client, endpoint, &token, &body, None).await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {} from vuln-ingest: {text}", status.as_u16());
    }
    let mut response: IngestResponse =
        serde_json::from_str(&text).context("invalid ingest response")?;
    if response.created_issue == Some(false) && response.exact_duplicate != Some(true) {
        let reference = format!("adhoc:{}", uuid::Uuid::new_v4());
        (status, text) = post(&client, endpoint, &token, &body, Some(&reference)).await?;
        anyhow::ensure!(
            status.is_success(),
            "HTTP {} on retry: {text}",
            status.as_u16()
        );
        response = serde_json::from_str(&text).context("invalid ingest retry response")?;
    }
    let issue = response.issue_key.unwrap_or_else(|| "?".into());
    Ok(if response.exact_duplicate == Some(true) {
        format!("SERVER-DUP {issue}")
    } else if response.created_issue == Some(true) {
        format!("SUBMITTED {issue}")
    } else {
        format!("unexpected ingest response: {text}")
    })
}

impl ToolDef for SubmitFindingArgs {
    const NAME: &'static str = "submit_finding";
    const DESCRIPTION: &'static str = "Finalize a confirmed vulnerability locally at <artifacts>/submitted/<slug>.json, write a human-readable copy to <artifacts>/submitted_humanreadable/<slug>.md, and update INDEX.md after confidence, reproducer, and dedup gates. Set live=true only when explicitly asked to also POST; live publication additionally requires FLAWRENCE_VULN_INGEST_LIVE=1. Local output is retained if publication fails.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let slug = args
            .finding_key
            .strip_prefix("vulnerability_")
            .context("finding_key must start with vulnerability_")?;
        let (raw, poc, findings_dir, has_dedup) = {
            let store = ctx.artifact_store.read().await;
            let raw = store
                .read(&args.finding_key)
                .map_err(|error| anyhow::anyhow!(error))?
                .to_string();
            let poc = store.read(&format!("poc_{slug}")).ok().map(str::to_string);
            let directory = store
                .output_dir()
                .cloned()
                .context("submit_finding requires --artifacts")?;
            (
                raw,
                poc,
                directory,
                store.read(&format!("dedup_{slug}")).is_ok(),
            )
        };
        let vulnerability: Value =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", args.finding_key))?;
        let confidence = vulnerability
            .pointer("/confidence/score")
            .and_then(Value::as_str)
            .unwrap_or("");
        if confidence != "high" {
            return Ok((
                format!(
                    "BLOCKED: confidence={}",
                    if confidence.is_empty() {
                        "<missing>"
                    } else {
                        confidence
                    }
                ),
                None,
            ));
        }
        if !passes_reproducer_gate(&vulnerability, poc.as_deref()) {
            return Ok(("BLOCKED: no verified reproducer".into(), None));
        }
        let (dedup, usage) = if has_dedup {
            let store = ctx.artifact_store.read().await;
            let body = store.read(&format!("dedup_{slug}")).unwrap_or("");
            (body.lines().any(|line| line == "verdict: DUP-STRONG"), None)
        } else {
            let outcome = run_dedup(&ctx, &args.finding_key).await?;
            (outcome.verdict == DedupVerdict::DupStrong, outcome.usage)
        };
        if dedup && !args.force {
            return Ok(("BLOCKED: DUP-STRONG".into(), usage));
        }
        let finalized = finalize(
            vulnerability,
            poc.as_deref(),
            ctx.runtime.git_info.commit_hash.as_deref(),
        );
        save_local(&findings_dir, slug, &finalized)?;
        if !args.live {
            return Ok((format!("SAVED submitted/{slug}.json (local only)"), usage));
        }
        let status = publish(&ctx, &findings_dir, &finalized)
            .await
            .unwrap_or_else(|error| format!("LIVE FAILED: {error}"));
        Ok((format!("SAVED submitted/{slug}.json; {status}"), usage))
    }
}

#[cfg(test)]
mod tests {
    use super::{finalize, ingest_payload, passes_reproducer_gate, save_local};
    use serde_json::{Value, json};

    #[test]
    fn local_result_keeps_reproduction_while_ingest_folds_it() {
        let finding = json!({
            "confidence": {"score": "high", "reason": "checked"},
            "reproduction": {"method": "static_proof", "verified": true, "evidence": "proof"}
        });
        assert!(passes_reproducer_gate(&finding, None));
        let finalized = finalize(finding, Some("STATUS: PASS\nproof"), Some("abc"));
        assert!(finalized.get("reproduction").is_some());
        assert!(ingest_payload(&finalized).get("reproduction").is_none());
        assert_eq!(
            finalized.get("commit_hash").and_then(Value::as_str),
            Some("abc")
        );
    }

    #[test]
    fn local_result_also_writes_human_readable_markdown() {
        let directory = tempfile::tempdir().unwrap();
        let finding = json!({
            "title": "Readable title",
            "root_cause": "Readable root cause",
            "description": "Readable details",
            "commit_hash": "abc123",
            "severity": {"overall_severity": "high"}
        });

        save_local(directory.path(), "example", &finding).unwrap();

        assert!(directory.path().join("submitted/example.json").is_file());
        let markdown =
            std::fs::read_to_string(directory.path().join("submitted_humanreadable/example.md"))
                .unwrap();
        assert!(markdown.contains("### Summary\n\nReadable root cause"));
        assert!(markdown.contains("Tested on commit `abc123`."));
    }
}

#[cfg(test)]
mod tool_args_tests {
    use super::*;

    #[test]
    fn finding_key_integer_rejected_with_corrected_value() {
        let err = serde_json::from_str::<SubmitFindingArgs>(r#"{"finding_key":3}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"finding_key\": \"3\""), "{msg}");
    }
}
