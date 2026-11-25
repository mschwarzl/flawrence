//! A best-effort summary from the findings already on disk, for a run whose own
//! consolidation could not complete.
//!
//! The normal end-of-run summary is written by the root agent's final turns:
//! they deduplicate the findings, submit the real vulnerabilities, and describe
//! what was found. If the provider is down when those turns run, or the run is
//! stopped, that summary never gets written even though the finding artifacts
//! are already on disk. This scans them and writes a plain inventory, so a run
//! that fell over at the last step is not a total loss.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// What the salvage found, and where it wrote the summary.
pub struct SalvageReport {
    /// Where the salvaged summary was written, if anything was found.
    pub summary_path: Option<PathBuf>,
    pub vulnerabilities: Vec<String>,
    pub false_positives: Vec<String>,
    pub validation_errors: Vec<String>,
    /// Drafts that never reached a terminal state, so their validation was cut
    /// short. These are the findings most worth a second look.
    pub unresolved_drafts: Vec<String>,
}

impl SalvageReport {
    #[must_use]
    pub fn found_anything(&self) -> bool {
        !self.vulnerabilities.is_empty()
            || !self.false_positives.is_empty()
            || !self.validation_errors.is_empty()
            || !self.unresolved_drafts.is_empty()
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} confirmed, {} unresolved draft(s), {} false positive(s), {} validation error(s)",
            self.vulnerabilities.len(),
            self.unresolved_drafts.len(),
            self.false_positives.len(),
            self.validation_errors.len(),
        )
    }
}

/// Scan `artifacts_dir` for finding artifacts and, when any exist, write
/// `<session>_salvaged-summary.md` next to them. A run that produced nothing
/// writes no file.
pub fn salvage_summary(artifacts_dir: &Path, session_id: &str) -> SalvageReport {
    let mut vulnerabilities = BTreeSet::new();
    let mut false_positives = BTreeSet::new();
    let mut validation_errors = BTreeSet::new();
    let mut drafts = BTreeSet::new();

    for path in finding_files(artifacts_dir) {
        let Some(key) = key_of(&path) else { continue };
        if let Some(slug) = key.strip_prefix("vulnerability_") {
            vulnerabilities.insert(slug.to_string());
        } else if let Some(slug) = key.strip_prefix("false_positive_") {
            false_positives.insert(slug.to_string());
        } else if let Some(slug) = key.strip_prefix("validation_error_") {
            validation_errors.insert(slug.to_string());
        } else if let Some(slug) = key.strip_prefix("finding_") {
            drafts.insert(slug.to_string());
        }
    }

    // A draft is "unresolved" only if it never reached a terminal state under
    // the same slug. A draft that did is already counted under its result.
    let resolved: BTreeSet<&String> = vulnerabilities
        .iter()
        .chain(&false_positives)
        .chain(&validation_errors)
        .collect();
    let unresolved_drafts: Vec<String> = drafts
        .iter()
        .filter(|slug| !resolved.contains(slug))
        .cloned()
        .collect();

    let mut report = SalvageReport {
        summary_path: None,
        vulnerabilities: vulnerabilities.into_iter().collect(),
        false_positives: false_positives.into_iter().collect(),
        validation_errors: validation_errors.into_iter().collect(),
        unresolved_drafts,
    };
    if !report.found_anything() {
        return report;
    }

    let path = artifacts_dir.join(format!("{session_id}_salvaged-summary.md"));
    match std::fs::write(&path, render(&report, session_id)) {
        Ok(()) => report.summary_path = Some(path),
        Err(error) => tracing::warn!(%error, "Failed to write salvaged summary"),
    }
    report
}

/// Finding artifacts live at the top level and one directory down (the store
/// files some kinds under a subdirectory), so walk exactly one level deep.
fn finding_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(children) = std::fs::read_dir(&path) {
                out.extend(children.flatten().map(|c| c.path()).filter(|p| p.is_file()));
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// `<session>_<key>.<ext>` -> `<key>`. The session id carries no underscore, so
/// the first one separates it from the key (which may itself contain more).
fn key_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.split_once('_').map(|(_, key)| key.to_string())
}

fn render(report: &SalvageReport, session_id: &str) -> String {
    let mut body = String::from("# Salvaged findings summary\n\n");
    body.push_str(
        "This run did not finish its own consolidation (the provider was unavailable, or it \
         was stopped), so this inventory was assembled from the finding artifacts already on \
         disk. It is not the agent's own review: entries may be unvalidated or duplicated.\n\n",
    );
    body.push_str(&format!("Session: `{session_id}`\n\n"));

    let section = |body: &mut String, title: &str, prefix: &str, slugs: &[String]| {
        body.push_str(&format!("## {title} ({})\n\n", slugs.len()));
        if slugs.is_empty() {
            body.push_str("_none_\n\n");
        } else {
            for slug in slugs {
                body.push_str(&format!("- `{prefix}{slug}`\n"));
            }
            body.push('\n');
        }
    };
    section(
        &mut body,
        "Confirmed vulnerabilities",
        "vulnerability_",
        &report.vulnerabilities,
    );
    section(
        &mut body,
        "Unresolved drafts (validation did not finish)",
        "finding_",
        &report.unresolved_drafts,
    );
    section(
        &mut body,
        "False positives",
        "false_positive_",
        &report.false_positives,
    );
    section(
        &mut body,
        "Validation errors",
        "validation_error_",
        &report.validation_errors,
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "{}").unwrap();
    }

    #[test]
    fn inventories_findings_and_writes_a_summary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "abcd1234_vulnerability_sqli.json");
        touch(root, "abcd1234_false_positive_xss.json");
        // A draft that was resolved (has a terminal result) is not "unresolved".
        touch(root, "abcd1234_finding_sqli.json");
        // A draft with no terminal result is unresolved.
        touch(root, "abcd1234_finding_pathtraversal.json");
        // A subdirectory is walked one level deep.
        std::fs::create_dir(root.join("draft")).unwrap();
        touch(&root.join("draft"), "abcd1234_validation_error_race.json");

        let report = salvage_summary(root, "abcd1234");
        assert_eq!(report.vulnerabilities, ["sqli"]);
        assert_eq!(report.false_positives, ["xss"]);
        assert_eq!(report.validation_errors, ["race"]);
        assert_eq!(report.unresolved_drafts, ["pathtraversal"]);

        let path = report.summary_path.expect("summary written");
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("vulnerability_sqli"));
        assert!(text.contains("finding_pathtraversal"));
        assert!(!text.contains("finding_sqli"));
    }

    #[test]
    fn writes_nothing_when_there_are_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "abcd1234_summary.md");
        touch(dir.path(), "abcd1234_attack_surface.md");
        let report = salvage_summary(dir.path(), "abcd1234");
        assert!(!report.found_anything());
        assert!(report.summary_path.is_none());
    }
}
