use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::tool_args::required_string_field_from_value;
use crate::{ToolContext, ToolDef};
use flawrence_chat::UsageByModel;

/// Strict-teaching deserializer for the artifact `key` field (see
/// `docs/SPEC_TOOL_ID_ROBUSTNESS.md`). Accepts only a string; a wrong type
/// yields an actionable error instead of serde's opaque message.
fn deserialize_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    required_string_field_from_value(Value::deserialize(deserializer)?, "key")
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOperation {
    /// Write a new artifact. Fails if key already exists (write-once).
    Write,
    /// Append to an artifact, creating it if absent. Use when a result is too
    /// large to produce in one response: append each part as you finish it,
    /// rather than holding everything and writing once at the end.
    Append,
    /// Read an artifact by key.
    Read,
    /// Delete an artifact by key (requires write permission).
    Delete,
    /// List all artifact keys visible to this agent.
    List,
}

#[derive(Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct ArtifactArgs {
    /// The operation to perform.
    pub operation: ArtifactOperation,
    /// Artifact key, **as a string**. Must match [a-z0-9_]+. Use an empty
    /// string for list. Pass it as a quoted string (e.g. `"finding_x"`),
    /// never a bare number or unquoted token.
    #[serde(deserialize_with = "deserialize_key")]
    pub key: String,
    /// Content to write. Use an empty string for read, delete, and list.
    pub content: String,
    /// Character offset to start reading from. Omit to read from the start.
    /// A read that does not fit one response says how large the artifact is and
    /// which offset continues it -- pass that back to page through the rest.
    pub offset: Option<usize>,
}

/// Return one response-sized window of an artifact, and say how to get the rest.
///
/// A read used to be all-or-nothing: an artifact larger than one response was
/// silently cut at the budget, so the end was unreachable. That is exactly the
/// content a continuation agent needs -- an attack surface too big to write in
/// one turn is extended by an agent that must first see where the table stops,
/// and it could only ever see the beginning. It then hunts the artifact on disk
/// and, failing, re-derives from scratch what was already written.
fn paginate(content: &str, offset: usize, key: &str) -> String {
    let total = content.chars().count();
    if offset >= total && offset > 0 {
        return format!(
            "[offset {offset} is past the end of '{key}', which is {total} characters]"
        );
    }
    // Reserve room for the trailing note, so the guidance is never the thing
    // that gets cut.
    const NOTE_ALLOWANCE: usize = 400;
    let window = crate::MAX_TOOL_RESPONSE_CHARS.saturating_sub(NOTE_ALLOWANCE);
    let shown: String = content.chars().skip(offset).take(window).collect();
    let end = offset + shown.chars().count();
    if end >= total && offset == 0 {
        return shown;
    }
    let mut out = String::new();
    if offset > 0 {
        out.push_str(&format!("[continuing '{key}' from character {offset}]\n"));
    }
    out.push_str(&shown);
    if end < total {
        out.push_str(&format!(
            "\n\n[showing characters {offset}-{end} of {total}. Read '{key}' again with \
             offset={end} for the next part; the text above stops mid-content.]"
        ));
    } else {
        out.push_str(&format!("\n\n[end of '{key}' ({total} characters)]"));
    }
    out
}

/// Keys this agent may read, as a display string. Mirrors what `list` would
/// show, so it discloses nothing extra.
fn readable_keys(
    store: &crate::artifact_store::ArtifactStore,
    perms: &flawrence_agent::ArtifactPermissions,
) -> String {
    let keys: Vec<&str> = store
        .keys()
        .into_iter()
        .filter(|key| perms.can_read(key))
        .collect();
    if keys.is_empty() {
        "(none yet)".to_string()
    } else {
        keys.join(", ")
    }
}

impl ToolDef for ArtifactArgs {
    const NAME: &'static str = "artifact";
    const DESCRIPTION: &'static str = "Read, write, delete, or list artifacts in a shared store. Always provide operation, key, and content. Use an empty key for list. Use empty content for read, delete, and list. Artifacts persist across the task tree (parent and child agents share one store). Keys must match [a-z0-9_]+. Writes are write-once. Use operation=append to build up a result too large for one response, which also means a cancelled run keeps what was finished. Managed finding_*, vulnerability_*, false_positive_*, and validation_error_* artifacts cannot be written here. Use report_finding instead. Permissions are controlled by the agent config.";

    async fn execute(ctx: ToolContext, args: Self) -> Result<(String, Option<UsageByModel>)> {
        let perms = ctx.agent_config.artifact_permissions();

        match args.operation {
            ArtifactOperation::Write => {
                let key = args.key;
                let content = args.content;
                if !perms.can_write(&key) {
                    anyhow::bail!(
                        "No write permission for artifact '{}'. Allowed patterns: {}",
                        key,
                        if perms.write.is_empty() {
                            "(none)".to_string()
                        } else {
                            perms.write.join(", ")
                        }
                    );
                }
                let mut store = ctx.artifact_store.write().await;
                store
                    .write(key.clone(), content)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok((format!("Artifact '{}' written.", key), None))
            }
            ArtifactOperation::Append => {
                let key = args.key;
                let content = args.content;
                if !perms.can_write(&key) {
                    anyhow::bail!(
                        "No write permission for artifact '{}'. Allowed patterns: {}",
                        key,
                        if perms.write.is_empty() {
                            "(none)".to_string()
                        } else {
                            perms.write.join(", ")
                        }
                    );
                }
                if content.is_empty() {
                    anyhow::bail!(
                        "append requires content; an empty string appends nothing. Use \
                         operation=read to see what '{key}' already holds."
                    );
                }
                let mut store = ctx.artifact_store.write().await;
                store
                    .append(&key, &content)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let total = store.read(&key).map(str::len).unwrap_or(0);
                Ok((
                    format!("Appended to '{key}', now {total} characters."),
                    None,
                ))
            }
            ArtifactOperation::Read => {
                let key = args.key;
                // An empty key is documented for `list` only. Reporting it as a
                // permission failure sends the model looking for a grant
                // problem it does not have.
                if key.is_empty() {
                    anyhow::bail!(
                        "read requires a key; an empty key is only valid for operation=list. \
                         Readable artifacts right now: {}",
                        readable_keys(&*ctx.artifact_store.read().await, &perms)
                    );
                }
                if !perms.can_read(&key) {
                    // Patterns alone leave a wildcard grant like `vulnerability_*`
                    // unanswered — the agent still has to guess a suffix. Name
                    // what exists as well; `list` would show the same set, so
                    // this reveals nothing the agent could not already ask for.
                    let store = ctx.artifact_store.read().await;
                    let readable = readable_keys(&store, &perms);
                    anyhow::bail!(
                        "No read permission for artifact '{}'. Allowed patterns: {}. Readable artifacts right now: {}",
                        key,
                        if perms.read.is_empty() {
                            "(none)".to_string()
                        } else {
                            perms
                                .read
                                .iter()
                                .map(|e| e.key())
                                .collect::<Vec<_>>()
                                .join(", ")
                        },
                        readable
                    );
                }
                let store = ctx.artifact_store.read().await;
                let content = store.read(&key).map_err(|error| {
                    // A missing artifact is usually one a sibling agent has not
                    // written yet, so naming what does exist saves the model a
                    // guess about whether to wait, write, or move on.
                    anyhow::anyhow!(
                        "{error} Readable artifacts right now: {}",
                        readable_keys(&store, &perms)
                    )
                })?;
                Ok((paginate(content, args.offset.unwrap_or(0), &key), None))
            }
            ArtifactOperation::Delete => {
                let key = args.key;
                if key.is_empty() {
                    anyhow::bail!(
                        "delete requires a key; an empty key is only valid for operation=list."
                    );
                }
                if !perms.can_write(&key) {
                    anyhow::bail!(
                        "No write permission to delete artifact '{}'. Allowed patterns: {}",
                        key,
                        if perms.write.is_empty() {
                            "(none)".to_string()
                        } else {
                            perms.write.join(", ")
                        }
                    );
                }
                let mut store = ctx.artifact_store.write().await;
                store.delete(&key).map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok((format!("Artifact '{}' deleted.", key), None))
            }
            ArtifactOperation::List => {
                let store = ctx.artifact_store.read().await;
                let all_keys = store.keys();
                let visible: Vec<&str> = all_keys
                    .iter()
                    .filter(|k| perms.can_read(k) || perms.can_write(k))
                    .copied()
                    .collect();
                if visible.is_empty() {
                    Ok(("No artifacts visible to this agent.".to_string(), None))
                } else {
                    Ok((
                        format!(
                            "Artifacts:\n{}",
                            visible
                                .iter()
                                .map(|k| format!("- \"{}\"", k))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                        None,
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod pagination {
    use super::*;

    /// The bc-java case: a 39,205-character attack surface against a 30,000
    /// character budget. The agent spawned to extend the table could only ever
    /// see its beginning, so it hunted the artifact on disk and then re-derived
    /// what was already written.
    #[test]
    fn an_oversized_artifact_can_be_paged_to_its_end() {
        // 'Z' because the guidance note itself contains "text" and "next";
        // counting 'x' would score the note as content.
        let content: String = "Z".repeat(39_205);
        let first = paginate(&content, 0, "attack_surface");

        assert!(
            first.len() <= crate::MAX_TOOL_RESPONSE_CHARS,
            "{}",
            first.len()
        );
        assert!(
            first.contains("of 39205"),
            "must state the true size: {}",
            tail(&first)
        );
        let next = next_offset(&first);
        assert!(next > 0 && next < 39_205, "{next}");

        let second = paginate(&content, next, "attack_surface");
        assert!(
            second.contains("end of 'attack_surface'"),
            "{}",
            tail(&second)
        );
        // The two windows together cover everything, with nothing dropped.
        let seen = first.matches('Z').count() + second.matches('Z').count();
        assert_eq!(seen, 39_205);
    }

    /// The common case must be untouched: an artifact that fits comes back as
    /// itself, with no framing for an agent to parse or mistake for content.
    #[test]
    fn an_artifact_that_fits_is_returned_verbatim() {
        let content = "## Threat model\n- one\n- two\n";
        assert_eq!(paginate(content, 0, "threat_model"), content);
    }

    #[test]
    fn an_offset_past_the_end_says_so_rather_than_returning_nothing() {
        let out = paginate("short", 500, "notes");
        assert!(out.contains("past the end"), "{out}");
        assert!(out.contains("5 characters"), "{out}");
    }

    /// Multi-byte content must not be split mid-character or miscounted.
    #[test]
    fn paging_counts_characters_not_bytes() {
        // Longer than one window, or there is nothing to page.
        let content: String = "é".repeat(40_000);
        let first = paginate(&content, 0, "notes");
        let next = next_offset(&first);
        let second = paginate(&content, next, "notes");
        assert_eq!(
            first.matches('é').count() + second.matches('é').count(),
            40_000
        );
    }

    fn tail(s: &str) -> String {
        s.chars()
            .skip(s.chars().count().saturating_sub(200))
            .collect()
    }

    fn next_offset(response: &str) -> usize {
        response
            .rsplit("offset=")
            .next()
            .and_then(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .expect("a truncated read must name the next offset")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_accepts_string() {
        let a: ArtifactArgs =
            serde_json::from_str(r#"{"operation":"list","key":"finding_x","content":""}"#).unwrap();
        assert_eq!(a.key, "finding_x");
    }

    #[test]
    fn key_integer_rejected_with_teaching_message() {
        let err =
            serde_json::from_str::<ArtifactArgs>(r#"{"operation":"read","key":5}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'key' must be a string"), "{msg}");
        assert!(msg.contains("number"), "{msg}");
        assert!(msg.contains("\"key\": \"5\""), "{msg}");
    }
}
