use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared artifact store type for passing between tasks and tools.
pub type SharedArtifactStore = Arc<RwLock<ArtifactStore>>;

/// A single artifact's payload plus the metadata renderers need to
/// display it correctly.
///
/// `content_type` is derived from the `extension` passed at write time
/// (`md` -> `text/markdown`, `json` -> `application/json`, anything else
/// -> `text/plain`). We persist both — the extension is what the
/// on-disk flush uses, the MIME type is what HTTP consumers expect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub content: String,
    pub content_type: String,
    pub extension: String,
}

impl ArtifactEntry {
    fn new(content: String, extension: &str) -> Self {
        Self {
            content,
            content_type: extension_to_mime(extension).to_string(),
            extension: extension.to_string(),
        }
    }
}

fn extension_to_mime(extension: &str) -> &'static str {
    match extension {
        "md" => "text/markdown",
        "json" => "application/json",
        "html" => "text/html",
        _ => "text/plain",
    }
}

/// A shared key-value store scoped to a task tree.
/// All tasks in the same tree (root + descendants) share one instance.
/// Keys must match `[a-z0-9_]+`. Writes are write-once (fail if key exists).
#[derive(Debug)]
pub struct ArtifactStore {
    artifacts: HashMap<String, ArtifactEntry>,
    output_dir: Option<PathBuf>,
    session_id: String,
}

impl ArtifactStore {
    /// Create a new empty artifact store wrapped in `Arc<RwLock>`.
    /// If `output_dir` is provided, artifacts are flushed to disk on write.
    pub fn new_shared(output_dir: Option<PathBuf>) -> SharedArtifactStore {
        let session_id = Self::generate_session_id();
        Arc::new(RwLock::new(Self {
            artifacts: HashMap::new(),
            output_dir,
            session_id,
        }))
    }

    /// Generate a random 4-byte hex session ID (8 hex characters).
    fn generate_session_id() -> String {
        format!("{:08x}", rand::random::<u32>())
    }

    /// Return the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Return the on-disk findings directory, if `--artifacts` was set.
    pub fn output_dir(&self) -> Option<&PathBuf> {
        self.output_dir.as_ref()
    }

    /// Map an artifact key to its findings-dir subdirectory and default extension.
    fn subdir_for(key: &str) -> (&'static str, &'static str) {
        match () {
            _ if key.starts_with("vulnerability_") => ("draft", "json"),
            _ if key.starts_with("finding_") => ("draft", "json"),
            _ if key.starts_with("validation_error_") => ("draft", "json"),
            _ if key.starts_with("dedup_") => ("draft", "md"),
            _ if key.starts_with("poc_") => ("reproducers", "md"),
            _ if key.starts_with("advisory_") => ("advisories", "md"),
            _ if key.starts_with("false_positive_") => ("attack-surface", "json"),
            _ if key.starts_with("shared_") => ("", "md"),
            _ if key == "threat_model" || key == "attack_surface" => ("attack-surface", "md"),
            _ => ("", "md"),
        }
    }

    /// Resolve the on-disk path for an artifact key.
    fn path_for(&self, dir: &std::path::Path, key: &str, extension: &str) -> PathBuf {
        let (subdir, _) = Self::subdir_for(key);
        let filename = format!("{}_{}.{}", self.session_id, key, extension);
        if subdir.is_empty() {
            dir.join(filename)
        } else {
            dir.join(subdir).join(filename)
        }
    }

    /// Validate that a key matches `[a-z0-9_]+`.
    pub fn validate_key(key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("Artifact key must not be empty".to_string());
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "Artifact key '{}' is invalid. Keys must match [a-z0-9_]+.",
                key
            ));
        }
        Ok(())
    }

    /// Write a new artifact. Fails if key already exists (write-once).
    /// Uses the key's default extension when flushing to disk.
    pub fn write(&mut self, key: String, content: String) -> Result<(), String> {
        let (_, extension) = Self::subdir_for(&key);
        if extension == "json" {
            return Err(format!(
                "Artifact '{}' is managed structured output. Use report_finding to produce it.",
                key
            ));
        }
        self.write_with_extension(key, content, extension)
    }

    /// Write a new artifact with a specific file extension. Fails if key already exists
    /// (write-once), except for `shared_*` keys which are append-only via `append()`.
    pub fn write_with_extension(
        &mut self,
        key: String,
        content: String,
        extension: &str,
    ) -> Result<(), String> {
        Self::validate_key(&key)?;
        if extension == "json" {
            serde_json::from_str::<serde_json::Value>(&content)
                .map_err(|error| format!("Artifact '{}' contains invalid JSON: {}", key, error))?;
        }
        if key.starts_with("shared_") {
            return Err(format!(
                "Artifact '{}' is append-only. Use append(), not write().",
                key
            ));
        }
        if self.artifacts.contains_key(&key) {
            return Err(format!(
                "Artifact '{}' already exists (write-once). Use delete then write to update.",
                key
            ));
        }
        let entry = ArtifactEntry::new(content.clone(), extension);
        self.artifacts.insert(key.clone(), entry);
        self.flush_to_disk(&key, &content, extension)?;
        Ok(())
    }

    /// Flush an artifact to disk if output_dir is set.
    fn flush_to_disk(&self, key: &str, content: &str, extension: &str) -> Result<(), String> {
        if let Some(ref dir) = self.output_dir {
            let path = self.path_for(dir, key, extension);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create '{}': {}", parent.display(), e))?;
            }
            std::fs::write(&path, content)
                .map_err(|e| format!("Failed to write artifact '{}' to disk: {}", key, e))?;
        }
        Ok(())
    }

    /// Remove an artifact file from disk if output_dir is set.
    /// Tries .md, .json, and .html extensions since different artifact types use different extensions.
    fn remove_from_disk(&self, key: &str) -> Result<(), String> {
        if let Some(ref dir) = self.output_dir {
            for ext in &["md", "json", "html"] {
                let path = self.path_for(dir, key, ext);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|e| {
                        format!("Failed to remove artifact '{}' from disk: {}", key, e)
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Append a line to an artifact. Only permitted for `shared_*` keys, which
    /// are exempt from the write-once rule.
    /// Create the shared claim log up front, empty.
    ///
    /// Research agents are told to read `shared_claimed_sinks` at the start of
    /// their task, and every one of them hit "not found" until the first
    /// finding was reported. `append` creates it on demand and the duplicate
    /// check treats empty and missing alike, so seeding changes nothing except
    /// removing a failed call per agent.
    pub fn seed_shared_claims(&mut self) {
        self.artifacts
            .entry("shared_claimed_sinks".to_string())
            .or_insert_with(|| ArtifactEntry::new(String::new(), "md"));
    }

    pub fn append(&mut self, key: &str, line: &str) -> Result<(), String> {
        Self::validate_key(key)?;
        let (_, extension) = Self::subdir_for(key);
        // Managed structured output is written whole by its own tool; appending
        // to half a JSON document would produce something nothing can parse.
        if extension == "json" {
            return Err(format!(
                "Artifact '{}' is managed structured output; use its dedicated tool.",
                key
            ));
        }
        let entry = self
            .artifacts
            .entry(key.to_string())
            .or_insert_with(|| ArtifactEntry::new(String::new(), extension));
        // Separate from whatever is already there. Without this an append onto
        // content that does not end in a newline -- anything created by `write`
        // -- joins the two, so the first appended row silently becomes part of
        // the last existing line.
        if !entry.content.is_empty() && !entry.content.ends_with('\n') {
            entry.content.push('\n');
        }
        entry.content.push_str(line);
        if !line.ends_with('\n') {
            entry.content.push('\n');
        }
        let snapshot = entry.content.clone();
        self.flush_to_disk(key, &snapshot, extension)?;
        Ok(())
    }

    /// Read an artifact's content body by key.
    pub fn read(&self, key: &str) -> Result<&str, String> {
        self.artifacts
            .get(key)
            .map(|e| e.content.as_str())
            .ok_or_else(|| format!("Artifact '{}' not found.", key))
    }

    /// Read an artifact entry (body + metadata) by key.
    pub fn read_entry(&self, key: &str) -> Result<&ArtifactEntry, String> {
        self.artifacts
            .get(key)
            .ok_or_else(|| format!("Artifact '{}' not found.", key))
    }

    /// Resolve a registered artifact's persisted file without copying its body.
    ///
    /// Returns `None` for an in-memory store.
    ///
    /// # Errors
    ///
    /// Returns an error if the artifact key is not registered.
    pub fn artifact_path(&self, key: &str) -> Result<Option<PathBuf>, String> {
        let entry = self.read_entry(key)?;
        Ok(self
            .output_dir
            .as_ref()
            .map(|directory| self.path_for(directory, key, &entry.extension)))
    }

    /// Delete an artifact by key.
    pub fn delete(&mut self, key: &str) -> Result<(), String> {
        if self.artifacts.remove(key).is_none() {
            return Err(format!("Artifact '{}' not found.", key));
        }
        self.remove_from_disk(key)?;
        Ok(())
    }

    /// List all artifact keys, sorted.
    pub fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.artifacts.keys().map(|k| k.as_str()).collect();
        keys.sort();
        keys
    }

    /// Snapshot the entire store as a sorted map suitable for JSON
    /// serialization. `BTreeMap` keeps the on-wire output deterministic.
    pub fn snapshot(&self) -> BTreeMap<String, ArtifactEntry> {
        self.artifacts
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_store() -> ArtifactStore {
        ArtifactStore {
            artifacts: HashMap::new(),
            output_dir: None,
            session_id: "deadbeef".to_string(),
        }
    }

    #[test]
    fn test_validate_key() {
        assert!(ArtifactStore::validate_key("threat_model").is_ok());
        assert!(ArtifactStore::validate_key("validation_123").is_ok());
        assert!(ArtifactStore::validate_key("a").is_ok());
        assert!(ArtifactStore::validate_key("").is_err());
        assert!(ArtifactStore::validate_key("Has-Dash").is_err());
        assert!(ArtifactStore::validate_key("has space").is_err());
        assert!(ArtifactStore::validate_key("UPPER").is_err());
    }

    #[test]
    fn test_write_once() {
        let mut store = test_store();
        assert!(store.write("key1".into(), "value1".into()).is_ok());
        assert!(store.write("key1".into(), "value2".into()).is_err());
    }

    #[test]
    fn test_read_write_delete() {
        let mut store = test_store();
        store.write("test".into(), "content".into()).unwrap();
        assert_eq!(store.read("test").unwrap(), "content");
        store.delete("test").unwrap();
        assert!(store.read("test").is_err());
        // Can write again after delete
        assert!(store.write("test".into(), "new content".into()).is_ok());
    }

    #[test]
    fn test_keys() {
        let mut store = test_store();
        store.write("beta".into(), "b".into()).unwrap();
        store.write("alpha".into(), "a".into()).unwrap();
        assert_eq!(store.keys(), vec!["alpha", "beta"]);
    }

    #[test]
    fn test_extension_to_mime() {
        assert_eq!(extension_to_mime("md"), "text/markdown");
        assert_eq!(extension_to_mime("json"), "application/json");
        assert_eq!(extension_to_mime("html"), "text/html");
        assert_eq!(extension_to_mime("txt"), "text/plain");
        assert_eq!(extension_to_mime(""), "text/plain");
    }

    #[test]
    fn test_write_records_content_type() {
        let mut store = test_store();
        store.write("note".into(), "# hi".into()).unwrap();
        store
            .write_with_extension("finding_example".into(), "{}".into(), "json")
            .unwrap();
        store
            .write_with_extension("payload".into(), "{}".into(), "json")
            .unwrap();

        let note = store.read_entry("note").unwrap();
        assert_eq!(note.content, "# hi");
        assert_eq!(note.content_type, "text/markdown");
        assert_eq!(note.extension, "md");

        let finding = store.read_entry("finding_example").unwrap();
        assert_eq!(finding.content_type, "application/json");
        assert_eq!(finding.extension, "json");

        let payload = store.read_entry("payload").unwrap();
        assert_eq!(payload.content, "{}");
        assert_eq!(payload.content_type, "application/json");
        assert_eq!(payload.extension, "json");
    }

    #[test]
    fn test_structured_artifacts_require_dedicated_valid_json_write() {
        let mut store = test_store();
        let managed_error = store
            .write("finding_example".into(), "# markdown".into())
            .unwrap_err();
        assert!(
            managed_error.contains("report_finding"),
            "message should point to report_finding: {managed_error}"
        );
        assert!(
            store
                .write_with_extension("finding_example".into(), "# markdown".into(), "json")
                .is_err()
        );
        assert!(
            store
                .write("validation_error_example".into(), "{}".into())
                .is_err()
        );
        assert!(store.read("finding_example").is_err());
    }

    #[test]
    fn test_snapshot_round_trip() {
        let mut store = test_store();
        store
            .write("threat_model".into(), "## model".into())
            .unwrap();
        store
            .write_with_extension("finding".into(), "{\"k\":1}".into(), "json")
            .unwrap();

        let snap = store.snapshot();
        let keys: Vec<&String> = snap.keys().collect();
        assert_eq!(keys, vec!["finding", "threat_model"]);

        let json = serde_json::to_string(&snap).expect("serialize");
        let parsed: BTreeMap<String, ArtifactEntry> =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["threat_model"].content, "## model");
        assert_eq!(parsed["threat_model"].content_type, "text/markdown");
        assert_eq!(parsed["finding"].content, "{\"k\":1}");
        assert_eq!(parsed["finding"].content_type, "application/json");
    }
}

#[cfg(test)]
mod incremental {
    use super::tests::test_store;

    /// An attack surface too large for one response was assembled in the
    /// agent's head and written once at the end -- so a run stopped partway
    /// lost every row, and a table that outgrew the response was cut with no
    /// way to extend it, since writes are write-once.
    #[test]
    fn appending_builds_an_artifact_across_several_turns() {
        let mut store = test_store();

        store
            .append("attack_surface", "| Name | Location |")
            .unwrap();
        store
            .append("attack_surface", "| ASN1InputStream | a.java:1 |")
            .unwrap();
        store
            .append("attack_surface", "| ASN1OutputStream | b.java:2 |\n")
            .unwrap();

        let built = store.read("attack_surface").unwrap();
        assert_eq!(
            built,
            "| Name | Location |\n| ASN1InputStream | a.java:1 |\n| ASN1OutputStream | b.java:2 |\n"
        );
        // Whatever was appended stands on its own: nothing here needs a final
        // write to become real.
        assert!(store.keys().contains(&"attack_surface"));
    }

    /// Appending to half a JSON document produces something nothing can parse,
    /// and those keys belong to their own tools anyway.
    #[test]
    fn managed_structured_output_still_refuses_appends() {
        let mut store = test_store();
        for managed in ["finding_x", "vulnerability_x", "false_positive_x"] {
            let error = store.append(managed, "{").unwrap_err();
            assert!(
                error.contains("managed structured output"),
                "{managed}: {error}"
            );
        }
    }

    /// Write-once is unchanged: append is the way to extend, not write.
    #[test]
    fn append_extends_where_a_second_write_would_be_refused() {
        let mut store = test_store();
        store
            .write("threat_model".to_string(), "first".to_string())
            .unwrap();
        assert!(
            store
                .write("threat_model".to_string(), "second".to_string())
                .is_err()
        );

        store.append("threat_model", "second").unwrap();
        assert_eq!(store.read("threat_model").unwrap(), "first\nsecond\n");
    }
}
