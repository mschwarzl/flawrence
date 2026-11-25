use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::tool_args::required_string_field_from_value;
use crate::{ToolContext, ToolDef};
use flawrence_chat::UsageByModel;

/// Strict-teaching deserializer for the required `finding_key` field (see
/// `docs/SPEC_TOOL_ID_ROBUSTNESS.md`).
fn deserialize_finding_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    required_string_field_from_value(Value::deserialize(deserializer)?, "finding_key")
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct WriteAdvisoryArgs {
    /// Artifact key of the confirmed vulnerability (e.g. `vulnerability_idor_responses`).
    /// Pass it as a quoted string, never a bare number or unquoted token.
    #[serde(deserialize_with = "deserialize_finding_key")]
    pub finding_key: String,
    /// CWE id for the header line, e.g. "CWE-639". Optional.
    /// Null means no CWE is claimed — preferable to guessing one.
    pub cwe: Option<String>,
    /// Tested-on version string (e.g. "v0.20.1"). The commit hash is taken from git
    /// metadata; this is the upstream version label.
    /// Null means the version was not established; do not invent one.
    pub tested_version: Option<String>,
}

const CWE_BY_BUGCLASS: &[(&str, &str)] = &[
    ("idor", "CWE-639"),
    ("authz", "CWE-285"),
    ("cross-tenant", "CWE-639"),
    ("ssrf", "CWE-918"),
    ("sql", "CWE-89"),
    ("path-travers", "CWE-22"),
    ("crlf", "CWE-93"),
    ("uaf", "CWE-416"),
    ("oob", "CWE-125"),
    ("type-confus", "CWE-843"),
    ("integer-overflow", "CWE-190"),
    ("toctou", "CWE-367"),
    ("info-leak", "CWE-200"),
];

fn guess_cwe(title: &str, root_cause: &str) -> Option<&'static str> {
    let hay = format!("{} {}", title, root_cause).to_lowercase();
    CWE_BY_BUGCLASS
        .iter()
        .find(|(needle, _)| hay.contains(needle))
        .map(|(_, cwe)| *cwe)
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(|x| x.as_str()).unwrap_or("")
}

pub fn render_ghsa(vuln: &Value, commit: Option<&str>, args: &WriteAdvisoryArgs) -> String {
    let title = s(vuln, "/title");
    let root_cause = s(vuln, "/root_cause");
    let description = s(vuln, "/description");
    let strategy = s(vuln, "/remediation/strategy");
    let impact_reason = s(vuln, "/severity/impact/reason");
    let severity = s(vuln, "/severity/overall_severity");
    let evidence = s(vuln, "/reproduction/evidence");

    let cwe = args
        .cwe
        .clone()
        .or_else(|| guess_cwe(title, root_cause).map(|s| s.to_string()));

    let mut details = String::new();
    details.push_str(description);
    if let Some(trace) = vuln.get("trace").and_then(|t| t.as_array()) {
        details.push_str("\n\n");
        for step in trace {
            let kind = s(step, "/kind");
            let file = s(step, "/file");
            let line = step.get("line").and_then(|l| l.as_u64()).unwrap_or(0);
            let scope = s(step, "/scope");
            details.push_str(&format!("- **{}** `{}` ({}:{})\n", kind, scope, file, line));
        }
    }

    let mut poc = String::new();
    if let Some(payloads) = vuln
        .pointer("/execution/payloads")
        .and_then(|p| p.as_array())
    {
        for p in payloads {
            if let Some(t) = p.as_str() {
                poc.push_str("```\n");
                poc.push_str(t);
                poc.push_str("\n```\n\n");
            }
        }
    }
    if let Some(instr) = vuln
        .pointer("/execution/instructions")
        .and_then(|p| p.as_array())
    {
        for (i, step) in instr.iter().enumerate() {
            if let Some(t) = step.as_str() {
                poc.push_str(&format!("{}. {}\n", i + 1, t));
            }
        }
    }
    if !evidence.is_empty() {
        poc.push_str("\nObserved:\n```\n");
        poc.push_str(evidence);
        poc.push_str("\n```\n");
    }
    let expected = s(vuln, "/execution/expected_result");
    if !expected.is_empty() {
        poc.push_str(&format!("\nExpected result: {}\n", expected));
    }

    let mut conditions = String::new();
    if let Some(conds) = vuln.get("conditions").and_then(|c| c.as_array()) {
        for c in conds {
            let d = s(c, "/description");
            if !d.is_empty() {
                conditions.push_str(&format!("Requires {}. ", d));
            }
        }
    }

    let tested = match (args.tested_version.as_deref(), commit) {
        (Some(v), Some(c)) => format!("\nTested on {} / commit `{}`.\n", v, c),
        (None, Some(c)) => format!("\nTested on commit `{}`.\n", c),
        (Some(v), None) => format!("\nTested on {}.\n", v),
        (None, None) => String::new(),
    };

    let mut out = String::new();
    if let Some(cwe) = cwe {
        out.push_str(&format!("{}\n\n", cwe));
    }
    out.push_str("### Summary\n\n");
    out.push_str(if root_cause.is_empty() {
        title
    } else {
        root_cause
    });
    out.push_str("\n\n### Details\n\n");
    out.push_str(&details);
    out.push_str(&tested);
    out.push_str("\n### PoC\n\n");
    out.push_str(if poc.is_empty() {
        "See trace above.\n"
    } else {
        &poc
    });
    out.push_str("\n### Impact\n\n");
    if !impact_reason.is_empty() {
        out.push_str(impact_reason);
        out.push(' ');
    }
    out.push_str(&conditions);
    if !severity.is_empty() {
        let mut chars = severity.chars();
        let cap: String = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        out.push_str(&format!("{} severity.", cap));
    }
    if !strategy.is_empty() {
        out.push_str(&format!("\n\nFix: {}\n", strategy));
    }
    out
}

impl ToolDef for WriteAdvisoryArgs {
    const NAME: &'static str = "write_advisory";
    const DESCRIPTION: &'static str = "Render a confirmed vulnerability_* artifact as GitHub Security Advisory form markdown (CWE line, ### Summary, ### Details with file:line trace, Tested-on commit, ### PoC from execution.payloads/instructions and reproduction.evidence, ### Impact with conditions and severity, Fix: line). Writes <findings_dir>/advisories/<slug>.ghsa.md and an advisory_<slug> artifact. Call this after submit_finding for upstream-reportable issues.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let slug = args
            .finding_key
            .strip_prefix("vulnerability_")
            .unwrap_or(&args.finding_key)
            .to_string();

        let (vuln_raw, findings_dir) = {
            let store = ctx.artifact_store.read().await;
            let json = store
                .read(&args.finding_key)
                .map_err(|e| anyhow::anyhow!("{}", e))?
                .to_string();
            (json, store.output_dir().cloned())
        };
        let vuln: Value = serde_json::from_str(&vuln_raw)
            .with_context(|| format!("parse {} as JSON", args.finding_key))?;

        let commit = ctx.runtime.git_info.commit_hash.as_deref();
        let md = render_ghsa(&vuln, commit, &args);

        if let Some(dir) = findings_dir {
            let path = dir.join("advisories").join(format!("{}.ghsa.md", slug));
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, &md)?;
        }
        {
            let mut store = ctx.artifact_store.write().await;
            let key = format!("advisory_{}", slug);
            let _ = store.delete(&key);
            store
                .write(key, md.clone())
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }

        Ok((
            format!(
                "Wrote advisories/{}.ghsa.md ({} bytes)\n\n{}",
                slug,
                md.len(),
                md
            ),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_key_integer_rejected_with_corrected_value() {
        let err = serde_json::from_str::<WriteAdvisoryArgs>(r#"{"finding_key":12}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"finding_key\": \"12\""), "{msg}");
    }
}
