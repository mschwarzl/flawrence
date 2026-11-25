use crate::{ToolContext, ToolDef};
use anyhow::{Context, Result};
use flawrence_chat::UsageByModel;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportFindingArgs {
    /// Stable artifact key. Must start with `finding_`.
    pub finding_key: String,
    /// One-line title naming the defect.
    pub title: String,
    /// One of: critical, high, medium, low.
    pub severity: String,
    /// Full write-up: mechanism, reachability, and evidence.
    pub description: String,
    /// Primary file the finding is in, relative to the working directory. Null means the
    /// finding is not tied to a single file — it also skips the in-flight duplicate check,
    /// so give a file whenever one applies.
    pub file: Option<String>,
    /// 1-indexed line in `file`. Null means the finding is not tied to a single line;
    /// only a file *and* a line together enable the duplicate check.
    pub line: Option<u32>,
}

/// Keep the verdict readable without pasting an entire validation essay back
/// into the researcher's context.
///
/// The validator is asked for under 2000 characters (the largest reason across
/// a full production run was 2435), so this cap is a safety net at roughly
/// three times the requested budget rather than the primary control — a size
/// limit the model can honour beats one imposed after the fact.
///
/// When it does engage it keeps both ends: the decisive part of a rejection is
/// its conclusion, and cutting the tail would hand the researcher the setup
/// while dropping the reason it was rejected, aiming its rebuttal at the wrong
/// argument. The complete text is always on disk in the verdict artifact.
fn truncate_reason(reason: &str, result_key: &str) -> String {
    const MAX_REASON_CHARS: usize = 6_000;
    let characters: Vec<char> = reason.chars().collect();
    if characters.len() <= MAX_REASON_CHARS {
        return reason.to_string();
    }
    let head: String = characters[..MAX_REASON_CHARS / 2].iter().collect();
    let tail: String = characters[characters.len() - MAX_REASON_CHARS / 2..]
        .iter()
        .collect();
    let omitted = characters.len() - MAX_REASON_CHARS;
    format!(
        "{head}\n\n[… {omitted} characters omitted from the middle; \
         read the '{result_key}' artifact for the full reasoning …]\n\n{tail}"
    )
}

fn claimed_sink(content: &str, file: &str, line: u32) -> Option<String> {
    for claim in content.lines() {
        let Some((location, rest)) = claim.split_once(" | ") else {
            continue;
        };
        let Some((claimed_file, claimed_line)) = location.rsplit_once(':') else {
            continue;
        };
        let Ok(claimed_line) = claimed_line.parse::<u32>() else {
            continue;
        };
        if claimed_file == file && claimed_line.abs_diff(line) <= 5 {
            return Some(rest.to_string());
        }
    }
    None
}

fn finding_draft(args: &ReportFindingArgs) -> Value {
    let trace: Vec<_> = args
        .file
        .as_ref()
        .zip(args.line)
        .map(|(file, line)| {
            json!({
                "kind": "sink",
                "file": file,
                "line": line,
                "description": &args.title,
            })
        })
        .into_iter()
        .collect();
    json!({
        "finding_key": &args.finding_key,
        "title": &args.title,
        "reported_severity": &args.severity,
        "description": &args.description,
        "trace": trace,
    })
}

impl ToolDef for ReportFindingArgs {
    const NAME: &'static str = "report_finding";
    const DESCRIPTION: &'static str = "Persist a finding_<slug> draft, reject an in-flight duplicate sink, run vulnerability-validation, and persist exactly one terminal result as vulnerability_<slug>, false_positive_<slug>, or validation_error_<slug>. The finding_key must be stable and start with finding_.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let slug = args
            .finding_key
            .strip_prefix("finding_")
            .filter(|slug| !slug.is_empty())
            .context("finding_key must start with 'finding_' and include a slug")?;
        if !ctx
            .agent_config
            .artifact_permissions()
            .can_write(&args.finding_key)
        {
            anyhow::bail!("No write permission for artifact '{}'", args.finding_key);
        }
        if !ctx.agent_config.can_spawn("vulnerability-validation") {
            anyhow::bail!(
                "Agent '{}' cannot spawn vulnerability-validation",
                ctx.agent_name()
            );
        }

        let draft = finding_draft(&args);

        {
            let mut store = ctx.artifact_store.write().await;
            if let (Some(file), Some(line)) = (&args.file, args.line) {
                if let Ok(claims) = store.read("shared_claimed_sinks")
                    && let Some(owner) = claimed_sink(claims, file, line)
                {
                    return Ok((format!("DUPLICATE-INFLIGHT: {owner}"), None));
                }
                store
                    .append(
                        "shared_claimed_sinks",
                        &format!("{}:{} | {} | {}", file, line, args.finding_key, args.title),
                    )
                    .map_err(anyhow::Error::msg)?;
            }
            store
                .write_with_extension(
                    args.finding_key.clone(),
                    serde_json::to_string_pretty(&draft)?,
                    "json",
                )
                .map_err(anyhow::Error::msg)?;
        }

        let validation = async {
            let output = ctx
                .spawn_child(
                    "vulnerability-validation",
                    format!("validate {slug}"),
                    format!("Validate finding: {}", args.finding_key),
                )
                .await?;
            let validated: Value = serde_json::from_str(&output.response)
                .with_context(|| "vulnerability-validation returned invalid JSON")?;
            let verdict = validated
                .get("verdict")
                .and_then(Value::as_str)
                .context("vulnerability-validation result has no verdict")?;
            let result_key = match verdict {
                "confirmed" => format!("vulnerability_{slug}"),
                "rejected" => format!("false_positive_{slug}"),
                other => anyhow::bail!("unknown validation verdict '{other}'"),
            };
            let mut store = ctx.artifact_store.write().await;
            store
                .write_with_extension(
                    result_key.clone(),
                    serde_json::to_string_pretty(&validated)?,
                    "json",
                )
                .map_err(anyhow::Error::msg)?;
            // The researcher's own prompt tells it to resubmit a rejected
            // finding "with counter-arguments to the validator's reasoning",
            // which it cannot do from a verdict alone. Hand back the reason
            // here rather than making it read an artifact it may not be
            // granted.
            let reason = validated
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let summary = if reason.is_empty() {
                format!("{verdict}: {result_key}")
            } else {
                format!(
                    "{verdict}: {result_key}\n\nValidator's reasoning:\n{}",
                    truncate_reason(reason, &result_key)
                )
            };
            Ok((summary, Some(output.usage)))
        }
        .await;

        match validation {
            Ok(result) => Ok(result),
            Err(error) => {
                let error_key = format!("validation_error_{slug}");
                let record = json!({
                    "finding_key": args.finding_key,
                    "status": "validation_failed",
                    "error": format!("{error:#}"),
                });
                let mut store = ctx.artifact_store.write().await;
                store
                    .write_with_extension(
                        error_key.clone(),
                        serde_json::to_string_pretty(&record)?,
                        "json",
                    )
                    .map_err(anyhow::Error::msg)?;
                Ok((format!("validation failed: {error_key}: {error:#}"), None))
            }
        }
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn a_normal_verdict_is_passed_through_whole() {
        // The longest reason in a full production run was 2435 characters.
        let reason = "x".repeat(2_435);
        assert_eq!(truncate_reason(&reason, "false_positive_x"), reason);
    }

    #[test]
    fn an_outsized_verdict_keeps_its_conclusion() {
        let reason = format!("{}THEREFORE NOT EXPLOITABLE", "s".repeat(20_000));
        let shortened = truncate_reason(&reason, "false_positive_x");

        // Cutting the tail would drop exactly the part the researcher must
        // rebut.
        assert!(
            shortened.ends_with("THEREFORE NOT EXPLOITABLE"),
            "conclusion lost"
        );
        assert!(shortened.starts_with("sss"), "opening lost");
        assert!(shortened.contains("characters omitted from the middle"));
        assert!(shortened.contains("false_positive_x"));
        assert!(shortened.chars().count() < reason.chars().count());
    }
}

#[cfg(test)]
mod tests {
    use super::{ReportFindingArgs, claimed_sink, finding_draft};

    #[test]
    fn finding_write_up_is_not_duplicated_in_the_sink_trace() {
        let args = ReportFindingArgs {
            finding_key: "finding_length".to_string(),
            title: "Unbounded length".to_string(),
            severity: "high".to_string(),
            description: "Detailed mechanism and executed evidence".to_string(),
            file: Some("src/parser.rs".to_string()),
            line: Some(42),
        };
        let draft = finding_draft(&args);
        assert_eq!(draft["description"], args.description);
        assert_eq!(draft["trace"][0]["description"], args.title);
        assert_eq!(draft.to_string().matches(&args.description).count(), 1);
        assert!(
            finding_draft(&ReportFindingArgs { file: None, ..args })["trace"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn finds_nearby_claim_for_same_file() {
        let claims = "src/auth.rs:40 | finding_old | Old title\n";
        assert_eq!(
            claimed_sink(claims, "src/auth.rs", 45).as_deref(),
            Some("finding_old | Old title")
        );
        assert!(claimed_sink(claims, "src/auth.rs", 46).is_none());
        assert!(claimed_sink(claims, "src/other.rs", 40).is_none());
    }
}
