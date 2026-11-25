use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

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
pub struct DedupCheckArgs {
    /// Artifact key of the confirmed vulnerability to check (e.g. `vulnerability_sqli_users`).
    /// Pass it as a quoted string, never a bare number or unquoted token.
    #[serde(deserialize_with = "deserialize_finding_key")]
    pub finding_key: String,
}

/// Stage-1 result from the local corpus scan.
struct LocalDedup {
    verdict: String,
    sink: String,
    semantic_key: String,
    title: String,
    root_cause: String,
    corpus_size: usize,
    shortlist: Vec<ShortlistEntry>,
}

struct ShortlistEntry {
    file: String,
    strong: bool,
    jaccard: f64,
    signals: Vec<String>,
    sink: String,
    semantic_key: String,
    title: String,
    root_cause: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupVerdict {
    New,
    DupWeak,
    DupStrong,
}

impl DedupVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            DedupVerdict::New => "NEW",
            DedupVerdict::DupWeak => "DUP-WEAK",
            DedupVerdict::DupStrong => "DUP-STRONG",
        }
    }
}

pub struct DedupOutcome {
    pub verdict: DedupVerdict,
    pub canonical: Option<String>,
    pub reason: String,
    pub usage: Option<UsageByModel>,
}

fn text(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn sink(value: &Value) -> (String, String, u64) {
    let step = value
        .get("trace")
        .and_then(Value::as_array)
        .and_then(|steps| {
            steps
                .iter()
                .rfind(|step| step.get("kind").and_then(Value::as_str) == Some("sink"))
                .or_else(|| steps.last())
        });
    let file = step
        .and_then(|step| step.get("file"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let line = step
        .and_then(|step| step.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (format!("{file}:{line}"), file.to_string(), line)
}

fn words(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn jaccard(left: &str, right: &str) -> f64 {
    let left = words(left);
    let right = words(right);
    let union = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(&right).count() as f64 / union as f64
}

fn semantic_key(value: &Value, sink_file: &str) -> String {
    const CLASSES: &[(&str, &str)] = &[
        ("cross-tenant", "authz"),
        ("authorization", "authz"),
        ("idor", "authz"),
        ("sql", "sql-injection"),
        ("path traversal", "path-traversal"),
        ("ssrf", "ssrf"),
        ("xss", "xss"),
        ("overflow", "overflow"),
        ("race", "race"),
        ("denial of service", "dos"),
        ("dos", "dos"),
        ("information", "information-disclosure"),
        ("leak", "information-disclosure"),
    ];
    let title = text(value, "/title").to_ascii_lowercase();
    let bug_class = CLASSES
        .iter()
        .find(|(needle, _)| title.contains(needle))
        .map(|(_, class)| *class)
        .unwrap_or("other");
    let component = Path::new(sink_file)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    format!("{component}:{bug_class}")
}

fn compare(candidate: &Value, prior: &Value, file: String) -> Option<ShortlistEntry> {
    let title = text(prior, "/title");
    let root_cause = text(prior, "/root_cause");
    let (_, candidate_file, candidate_line) = sink(candidate);
    let (prior_sink, prior_file, prior_line) = sink(prior);
    let candidate_semantic = semantic_key(candidate, &candidate_file);
    let prior_semantic = semantic_key(prior, &prior_file);
    let similarity = jaccard(
        &format!(
            "{} {}",
            text(candidate, "/title"),
            text(candidate, "/root_cause")
        ),
        &format!("{title} {root_cause}"),
    );
    let mut signals = Vec::with_capacity(3);
    if !candidate_file.is_empty()
        && candidate_file == prior_file
        && candidate_line.abs_diff(prior_line) <= 5
    {
        signals.push("sink-proximity".into());
    }
    if similarity >= 0.30 {
        signals.push("text-jaccard".into());
    }
    if candidate_semantic == prior_semantic && !candidate_semantic.starts_with("unknown:") {
        signals.push("semantic-key".into());
    }
    if signals.is_empty() {
        return None;
    }
    let strong = signals.len() >= 2
        || (signals.iter().any(|signal| signal == "sink-proximity") && similarity >= 0.12);
    Some(ShortlistEntry {
        file,
        strong,
        jaccard: similarity,
        signals,
        sink: prior_sink,
        semantic_key: prior_semantic,
        title,
        root_cause,
    })
}

fn local_dedup(candidate: &Value, findings_dir: &Path) -> Result<LocalDedup> {
    let mut corpus_size = 0;
    let mut shortlist = Vec::new();
    for directory in ["submitted", "validated"] {
        let pattern = findings_dir
            .join(directory)
            .join("*.json")
            .to_string_lossy()
            .to_string();
        for path in glob::glob(&pattern)? {
            let path = path?;
            let prior: Value = match std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(prior) => prior,
                None => {
                    tracing::warn!(path = %path.display(), "Skipping unreadable dedup corpus entry");
                    continue;
                }
            };
            corpus_size += 1;
            if let Some(entry) = compare(candidate, &prior, path.display().to_string()) {
                shortlist.push(entry);
            }
        }
    }
    shortlist.sort_by(|left, right| {
        right
            .strong
            .cmp(&left.strong)
            .then_with(|| right.jaccard.total_cmp(&left.jaccard))
    });
    shortlist.truncate(10);
    let (candidate_sink, candidate_file, _) = sink(candidate);
    Ok(LocalDedup {
        verdict: if shortlist.iter().any(|entry| entry.strong) {
            "DUP-STRONG".into()
        } else if shortlist.is_empty() {
            "NEW".into()
        } else {
            "DUP-WEAK".into()
        },
        sink: candidate_sink,
        semantic_key: semantic_key(candidate, &candidate_file),
        title: text(candidate, "/title"),
        root_cause: text(candidate, "/root_cause"),
        corpus_size,
        shortlist,
    })
}

/// Run the full dedup pipeline (local shortlist + LLM compare) for a finding.
/// Reusable from `submit_finding`.
pub async fn run_dedup(ctx: &ToolContext, finding_key: &str) -> Result<DedupOutcome> {
    let slug = finding_key
        .strip_prefix("vulnerability_")
        .or_else(|| finding_key.strip_prefix("finding_"))
        .unwrap_or(finding_key);

    let (vuln_json, findings_dir) = {
        let store = ctx.artifact_store.read().await;
        let json = store
            .read(finding_key)
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .to_string();
        let dir = store
            .output_dir()
            .cloned()
            .context("dedup_check requires --artifacts <findings_dir> so the corpus is on disk")?;
        (json, dir)
    };

    let draft_path = findings_dir.join("draft").join(format!("{}.json", slug));
    std::fs::create_dir_all(draft_path.parent().unwrap())?;
    std::fs::write(&draft_path, &vuln_json)
        .with_context(|| format!("write {}", draft_path.display()))?;

    let candidate: Value =
        serde_json::from_str(&vuln_json).with_context(|| format!("parse {finding_key} as JSON"))?;
    // ponytail: exhaustive scan is simpler and deterministic; add an index if corpora become large.
    let stage1 = local_dedup(&candidate, &findings_dir)?;

    let local_verdict = match stage1.verdict.as_str() {
        "DUP-STRONG" => DedupVerdict::DupStrong,
        "DUP-WEAK" => DedupVerdict::DupWeak,
        _ => DedupVerdict::New,
    };

    if stage1.shortlist.is_empty() {
        return Ok(DedupOutcome {
            verdict: DedupVerdict::New,
            canonical: None,
            reason: format!(
                "no sink/text/semantic candidates in corpus of {}",
                stage1.corpus_size
            ),
            usage: None,
        });
    }

    // Stage 2: LLM compare on the shortlist only.
    let mut prompt = String::new();
    prompt.push_str("# Candidate finding\n");
    prompt.push_str(&format!("title: {}\n", stage1.title));
    prompt.push_str(&format!("sink: {}\n", stage1.sink));
    prompt.push_str(&format!("semantic_key: {}\n", stage1.semantic_key));
    prompt.push_str(&format!("root_cause: {}\n\n", stage1.root_cause));
    prompt.push_str("# Prior findings (shortlisted by sink/text/semantic-key)\n");
    for (i, e) in stage1.shortlist.iter().enumerate() {
        prompt.push_str(&format!(
            "[{}] file={} strong={} jaccard={:.2} signals={:?}\n    title: {}\n    sink: {}\n    semantic_key: {}\n    root_cause: {}\n",
            i, e.file, e.strong, e.jaccard, e.signals, e.title, e.sink, e.semantic_key, e.root_cause
        ));
    }
    prompt.push_str("\nDecide: is the candidate a duplicate of any prior entry?");

    let llm = ctx
        .spawn_child("dedup-compare", format!("dedup {}", slug), prompt)
        .await?;
    let parsed: Value = serde_json::from_str(&llm.response).unwrap_or(Value::Null);
    let llm_dup = parsed.get("verdict").and_then(|v| v.as_str()) == Some("duplicate");
    let llm_idx = parsed
        .get("canonical_index")
        .and_then(|v| v.as_u64())
        .map(|i| i as usize);
    let llm_reason = parsed
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let (verdict, canonical) = match (local_verdict, llm_dup) {
        (DedupVerdict::DupStrong, _) | (_, true) => {
            let canon = llm_idx
                .and_then(|i| stage1.shortlist.get(i))
                .or(stage1.shortlist.first())
                .map(|e| e.file.clone());
            (DedupVerdict::DupStrong, canon)
        }
        (DedupVerdict::DupWeak, false) => (DedupVerdict::New, None),
        (DedupVerdict::New, false) => (DedupVerdict::New, None),
    };

    Ok(DedupOutcome {
        verdict,
        canonical,
        reason: format!(
            "local={} llm={} {}",
            stage1.verdict,
            if llm_dup { "duplicate" } else { "distinct" },
            llm_reason
        ),
        usage: Some(llm.usage),
    })
}

impl ToolDef for DedupCheckArgs {
    const NAME: &'static str = "dedup_check";
    const DESCRIPTION: &'static str = "Check whether a confirmed vulnerability duplicates the local submitted/ or validated/ corpus. Stage 1 performs built-in sink, text, and semantic-key shortlisting; stage 2 asks dedup-compare about at most 10 candidates. No external script is required. Writes dedup_<slug>.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let outcome = run_dedup(&ctx, &args.finding_key).await?;
        let slug = args
            .finding_key
            .strip_prefix("vulnerability_")
            .unwrap_or(&args.finding_key);

        let body = format!(
            "verdict: {}\ncanonical: {}\nreason: {}\n",
            outcome.verdict.as_str(),
            outcome.canonical.as_deref().unwrap_or("-"),
            outcome.reason
        );
        {
            let mut store = ctx.artifact_store.write().await;
            let key = format!("dedup_{}", slug);
            let _ = store.delete(&key);
            let _ = store.write(key, body.clone());
        }

        let summary = match outcome.verdict {
            DedupVerdict::New => format!("NEW: {} ({})", args.finding_key, outcome.reason),
            DedupVerdict::DupWeak => format!(
                "DUP-WEAK: {} possibly duplicates {} ({})",
                args.finding_key,
                outcome.canonical.as_deref().unwrap_or("?"),
                outcome.reason
            ),
            DedupVerdict::DupStrong => format!(
                "DUP-STRONG: {} duplicates {} ({}). Do not submit.",
                args.finding_key,
                outcome.canonical.as_deref().unwrap_or("?"),
                outcome.reason
            ),
        };
        Ok((summary, outcome.usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flawrence_agent::AgentRegistry;
    use serde_json::json;

    #[test]
    fn security_review_loads_dedup_compare() -> Result<()> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents");
        let agents = AgentRegistry::load_reachable(
            directory,
            "security-review",
            &crate::registry().tool_names(),
        )?;

        agents.get("dedup-compare")?;
        Ok(())
    }

    #[test]
    fn shortlist_detects_same_sink_without_external_script() {
        let candidate = json!({
            "title": "Authorization bypass in account lookup",
            "root_cause": "lookup omits the owner check",
            "trace": [{"kind": "sink", "file": "src/account.rs", "line": 40}]
        });
        let prior = json!({
            "title": "Account lookup authorization bypass",
            "root_cause": "lookup does not check the owner",
            "trace": [{"kind": "sink", "file": "src/account.rs", "line": 43}]
        });
        let entry = compare(&candidate, &prior, "prior.json".into()).unwrap();
        assert!(entry.strong);
        assert!(
            entry
                .signals
                .iter()
                .any(|signal| signal == "sink-proximity")
        );
        assert!(jaccard("same title words", "same title") > 0.5);
    }

    #[test]
    fn finding_key_integer_rejected_with_corrected_value() {
        let err = serde_json::from_str::<DedupCheckArgs>(r#"{"finding_key":9}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"finding_key\": \"9\""), "{msg}");
    }
}
