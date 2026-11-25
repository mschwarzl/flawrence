use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "std")]
use std::fs;
#[cfg(feature = "std")]
use std::path::Path;

/// A single entry in the artifact `read` permission list.
/// Can be a plain string ("finding_*") or a tagged dict ({"threat_model": "input"}).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArtifactReadEntry {
    Plain(String),
    Tagged(HashMap<String, String>),
}

impl ArtifactReadEntry {
    /// The artifact key / glob pattern.
    pub fn key(&self) -> &str {
        match self {
            ArtifactReadEntry::Plain(s) => s.as_str(),
            ArtifactReadEntry::Tagged(map) => map.keys().next().map(|s| s.as_str()).unwrap_or(""),
        }
    }

    /// The tag value, if this is a tagged entry.
    pub fn tag(&self) -> Option<&str> {
        match self {
            ArtifactReadEntry::Plain(_) => None,
            ArtifactReadEntry::Tagged(map) => map.values().next().map(|s| s.as_str()),
        }
    }

    /// Whether this entry is tagged as an input artifact.
    pub fn is_input(&self) -> bool {
        self.tag() == Some("input")
    }
}

/// Artifact tool permissions: which keys can be read/written.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPermissions {
    #[serde(default)]
    pub read: Vec<ArtifactReadEntry>,
    #[serde(default)]
    pub write: Vec<String>,
}

impl ArtifactPermissions {
    /// Check if a key matches any of the given glob patterns.
    /// Glob patterns support only trailing `*` (e.g., `vulnerability_*`).
    fn matches_glob(key: &str, patterns: &[String]) -> bool {
        patterns.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                key.starts_with(prefix)
            } else {
                key == pattern
            }
        })
    }

    pub fn can_read(&self, key: &str) -> bool {
        self.read.iter().any(|entry| {
            let pattern = entry.key();
            Self::matches_glob(key, &[pattern.to_string()])
        })
    }

    pub fn can_write(&self, key: &str) -> bool {
        Self::matches_glob(key, &self.write)
    }

    pub fn has_any(&self) -> bool {
        !self.read.is_empty() || !self.write.is_empty()
    }
}

/// Declarative binding of output-schema fields to a tool's actual call data, so
/// the harness can verify (not trust) the model's `submit_result` values against
/// real tool calls. Generic: any agent can ground any output field in any tool's
/// result/args.
///
/// # Examples
///
/// A binding that grounds `verified_command` in the `command` argument of the
/// last *passing* `run_check` call. The filter `path` is relative to the
/// result object (`"passed"` => `result.passed`), never `"result.passed"`.
///
/// ```
/// # use flawrence_agent::OutputGrounding;
/// let grounding: OutputGrounding = serde_json::from_value(serde_json::json!({
///     "verified_command": {
///         "tool": "run_check",
///         "source": "args",
///         "path": "command",
///         "when": "last",
///         "filter": { "path": "passed", "value": true }
///     }
/// }))?;
/// let cd = &grounding.0["verified_command"];
/// assert_eq!(cd.source, "args");
/// assert_eq!(cd.path, "command");
/// assert_eq!(cd.filter.as_ref().unwrap().path, "passed");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct OutputGrounding(pub HashMap<String, FieldGrounding>);

/// How to extract one grounded field from the tool-call history.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldGrounding {
    /// Tool name whose calls provide the source data.
    pub tool: String,
    /// `"args"` = the model's call arguments; `"result"` = the tool's output JSON.
    pub source: String,
    /// Dot-path into the JSON, e.g. `passed` or `errors`.
    pub path: String,
    /// `"last"` = use the final matching call in the transcript.
    pub when: String,
    /// Only match calls whose extracted value equals this. Makes the binding
    /// fully declarative (no field-name hardcoding in the harness).
    #[serde(default)]
    pub filter: Option<FilterExpr>,
}

/// A filter predicate: keep calls where `result.<path>` equals `value`.
///
/// `path` is a dot-path **relative to the result object** — the `result.`
/// prefix is already implied by the application, so do NOT repeat it. For
/// example `path: "passed"` checks `result.passed`; writing
/// `path: "result.passed"` would look for `result.result.passed` and
/// silently never match.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterExpr {
    /// Dot-path **relative to the result object** (e.g. `"passed"` means
    /// `result.passed`). Do NOT prefix with `result.` — see the struct docs.
    pub path: String,
    /// The value the extracted result field must equal for the call to match
    /// (e.g. `true` to keep only passing `run_check` calls).
    pub value: serde_json::Value,
}

/// Parsed YAML frontmatter from agent file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFrontmatter {
    #[serde(default)]
    pub tools: Vec<String>,
    pub model: String,
    pub description: String,
    pub output_schema: Option<serde_json::Value>,
    pub reasoning_effort: flawrence_chat::ReasoningEffort,
    #[serde(default)]
    pub todos: Vec<String>,
    /// Per-tool configuration. Keys are tool names, values are tool-specific config.
    #[serde(default)]
    pub tool_configs: HashMap<String, serde_json::Value>,
    /// List of agents this agent can spawn. If non-empty, spawn_task is implicit.
    #[serde(default)]
    pub subagents: Vec<String>,
    pub output_reviewer: Option<String>,
    /// Parameters for prompt substitution.
    #[serde(default)]
    pub params: HashMap<String, String>,
    /// Per-agent iteration cap. Overrides the global `MAX_ITERATIONS` for this
    /// agent (e.g. to bound a subagent). `None` = use the global default.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Declarative binding of output fields to real tool-call data. When set,
    /// the harness verifies the agent's `submit_result` against the tool-call
    /// transcript and nudges on mismatch (G). `None` = no grounding.
    #[serde(default)]
    pub output_grounding: Option<OutputGrounding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub tools: Vec<String>,
    pub model: String,
    pub description: String,
    #[serde(default)]
    pub prompt: String,
    pub output_schema: Option<serde_json::Value>,
    pub reasoning_effort: flawrence_chat::ReasoningEffort,
    #[serde(default)]
    pub todos: Vec<String>,
    /// Per-tool configuration. Keys are tool names, values are tool-specific config.
    /// The `artifact` tool reads `{ "read": [...], "write": [...] }` from `tool_configs["artifact"]`.
    #[serde(default)]
    pub tool_configs: HashMap<String, serde_json::Value>,
    /// List of agents this agent can spawn. If non-empty, `spawn_task` is implicitly available.
    #[serde(default)]
    pub subagents: Vec<String>,
    pub output_reviewer: Option<String>,
    /// Parameters for prompt substitution.
    #[serde(default)]
    pub params: HashMap<String, String>,
    /// Per-agent iteration cap. Overrides the global `MAX_ITERATIONS`.
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Declarative binding of output fields to real tool-call data (G).
    #[serde(default)]
    pub output_grounding: Option<OutputGrounding>,
}

impl AgentConfig {
    pub fn parse(name: &str, content: &str) -> Result<Self> {
        if let Some(stripped) = content.strip_prefix("---")
            && let Some(end_idx) = stripped.find("---")
        {
            let yaml_content = stripped[..end_idx].trim();
            let prompt = stripped[end_idx + 3..].trim().to_string();

            let frontmatter: AgentFrontmatter = serde_yaml::from_str(yaml_content)
                .with_context(|| "Failed to parse agent frontmatter")?;

            let tools = frontmatter.tools;
            let tool_configs = frontmatter.tool_configs;

            return Ok(Self {
                name: name.to_string(),
                tools,
                model: frontmatter.model,
                description: frontmatter.description,
                prompt,
                output_schema: frontmatter.output_schema,
                reasoning_effort: frontmatter.reasoning_effort,
                todos: frontmatter.todos,
                tool_configs,
                subagents: frontmatter.subagents,
                output_reviewer: frontmatter.output_reviewer,
                params: frontmatter.params,
                max_iterations: frontmatter.max_iterations,
                output_grounding: frontmatter.output_grounding,
            });
        }

        anyhow::bail!(
            "Agent '{}' must have YAML frontmatter with 'model' and 'description' fields. Format:\n---\nmodel: model-name\ndescription: Agent purpose\ntools:\n  - tool1\n---\nPrompt here",
            name
        )
    }

    /// Extract artifact permissions from `tool_configs["artifact"]`.
    pub fn artifact_permissions(&self) -> ArtifactPermissions {
        self.tool_configs
            .get("artifact")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Extract input artifact keys from `artifact.read` entries tagged with `"input"`.
    pub fn input_artifacts(&self) -> Vec<String> {
        self.artifact_permissions()
            .read
            .iter()
            .filter(|e| e.is_input())
            .map(|e| e.key().to_string())
            .collect()
    }

    /// Apply parameter substitution to the prompt.
    /// CLI-provided values override YAML defaults.
    pub fn apply_params(&mut self, overrides: &HashMap<String, String>) {
        // Merge: CLI overrides YAML defaults
        let mut merged = self.params.clone();
        for (key, value) in overrides {
            merged.insert(key.clone(), value.clone());
        }

        // Substitute {{key}} → value in prompt
        for (key, value) in &merged {
            let placeholder = format!("{{{{{}}}}}", key);
            self.prompt = self.prompt.replace(&placeholder, value);
        }
    }

    /// After [`AgentConfig::apply_params`], scan the prompt for any remaining `{{...}}`
    /// placeholders. Returns the unresolved placeholder names in order of
    /// first appearance (deduplicated). An empty list means every placeholder
    /// resolved.
    pub fn unresolved_placeholders(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut rest = self.prompt.as_str();
        while let Some(open) = rest.find("{{") {
            let after = &rest[open + 2..];
            match after.find("}}") {
                Some(close) => {
                    let key = after[..close].trim();
                    if !key.is_empty() && seen.insert(key.to_string()) {
                        out.push(key.to_string());
                    }
                    rest = &after[close + 2..];
                }
                None => break,
            }
        }
        out
    }

    /// Apply CLI/parent parameter overrides, then verify every `{{...}}`
    /// placeholder in the prompt resolved. Returns a teaching error naming the
    /// first unresolved placeholder if any remain. Called uniformly for every
    /// agent (root and spawned) before it runs — see `run_task` in
    /// `flawrence-task`.
    pub fn resolve_params(
        &mut self,
        overrides: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        self.apply_params(overrides);
        let missing = self.unresolved_placeholders();
        if let Some(key) = missing.first() {
            anyhow::bail!(
                "Agent '{}' has unresolved placeholder '{{{{{}}}}}' in its prompt — \
                 param '{}' is neither declared in this agent's `params:` nor provided via `--arg`. \
                 Declare it in the agent's frontmatter `params:` or pass `--arg {}=<value>`.",
                self.name,
                key,
                key,
                key
            );
        }
        Ok(())
    }

    /// Check if this agent is allowed to spawn the given agent.
    pub fn can_spawn(&self, agent_name: &str) -> bool {
        self.subagents.iter().any(|a| a == agent_name)
    }

    /// Validate that an artifact key matches [a-z0-9_]+.
    pub fn validate_artifact_key(key: &str) -> Result<(), String> {
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

    /// Validate that the agent config is correct
    pub fn validate(&self, available_tools: &[String]) -> Result<()> {
        if self.model.is_empty() {
            anyhow::bail!("Agent '{}' must specify a model", self.name);
        }

        if self.description.is_empty() {
            anyhow::bail!("Agent '{}' must specify a description", self.name);
        }

        // Validate tool names against registry.
        // `spawn_task` is implicit when subagents is non-empty, so allow it even if not
        // in the registry's tool list.
        if !self.tools.is_empty() {
            let invalid: Vec<_> = self
                .tools
                .iter()
                .filter(|t| *t != "spawn_task" && !available_tools.contains(t))
                .cloned()
                .collect();

            if !invalid.is_empty() {
                anyhow::bail!(
                    "Agent '{}' specifies invalid tools: {}. Available tools: {}",
                    self.name,
                    invalid.join(", "),
                    available_tools.join(", ")
                );
            }
        }

        // Fail-closed: artifact in tools requires tool_configs["artifact"] to have patterns
        let artifact_perms = self.artifact_permissions();
        if self.tools.contains(&"artifact".to_string()) && !artifact_perms.has_any() {
            anyhow::bail!(
                "Agent '{}' has 'artifact' in tools but no artifact config. \
                 Add tool_configs.artifact.read and/or tool_configs.artifact.write.",
                self.name
            );
        }

        // Fail-closed: spawn_task in tools requires subagents
        if self.tools.contains(&"spawn_task".to_string()) && self.subagents.is_empty() {
            anyhow::bail!(
                "Agent '{}' has 'spawn_task' in tools but no subagents configured. \
                 Add subagents list.",
                self.name
            );
        }

        // Warn about orphan configs
        if artifact_perms.has_any() && !self.tools.contains(&"artifact".to_string()) {
            eprintln!(
                "  Warning: Agent '{}' has artifact tool_configs but 'artifact' is not in tools list",
                self.name
            );
        }

        Ok(())
    }
}

/// Registry holding all pre-loaded agents
#[derive(Clone)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentConfig>,
}

impl AgentRegistry {
    /// Load all agents from the agents/ directory, validating tool names against the provided list.
    /// `FLAWRENCE_AGENTS_DIR` overrides the default `agents` lookup path.
    #[cfg(feature = "std")]
    pub fn load_all(available_tools: &[String]) -> Result<Self> {
        let dir = std::env::var("FLAWRENCE_AGENTS_DIR").unwrap_or_else(|_| "agents".to_string());
        Self::load_from_dir(dir, available_tools)
    }

    /// Load all agents from a specific directory, validating tool names against the provided list.
    #[cfg(feature = "std")]
    pub fn load_from_dir<P: AsRef<Path>>(dir: P, available_tools: &[String]) -> Result<Self> {
        let dir = dir.as_ref();
        let mut agents = HashMap::new();

        if !dir.exists() {
            anyhow::bail!("Agents directory '{}' does not exist", dir.display());
        }

        for entry in fs::read_dir(dir)
            .with_context(|| format!("Failed to read agents directory '{}'", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "md").unwrap_or(false) {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| anyhow::anyhow!("Invalid agent filename: {}", path.display()))?
                    .to_string();

                let content = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read agent file '{}'", path.display()))?;

                let agent = AgentConfig::parse(&name, &content)
                    .with_context(|| format!("Failed to parse agent '{}'", name))?;

                agent
                    .validate(available_tools)
                    .with_context(|| format!("Failed to validate agent '{}'", name))?;

                eprintln!("  Loaded agent: {} ({} tools)", name, agent.tools.len());
                agents.insert(name, agent);
            }
        }

        if agents.is_empty() {
            anyhow::bail!("No agents found in '{}'", dir.display());
        }

        // Cross-reference validation: every `output_reviewer` must point at a
        // loaded agent. We do this after the per-agent loop so the order in
        // which files are read doesn't matter.
        for agent in agents.values() {
            if let Some(reviewer) = &agent.output_reviewer
                && !agents.contains_key(reviewer)
            {
                anyhow::bail!(
                    "Agent '{}' references unknown output_reviewer '{}'. \
                         Available agents: {}",
                    agent.name,
                    reviewer,
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }

        Ok(Self { agents })
    }

    /// Load only agents reachable from `root_name` via subagent/output_reviewer refs.
    /// Traverses the directory at `dir`.
    #[cfg(feature = "std")]
    pub fn load_reachable<P: AsRef<Path>>(
        dir: P,
        root_name: &str,
        available_tools: &[String],
    ) -> Result<Self> {
        let dir = dir.as_ref();
        if !dir.exists() {
            anyhow::bail!("Agents directory '{}' does not exist", dir.display());
        }

        let mut agents: HashMap<String, AgentConfig> = HashMap::new();
        let mut visited = HashSet::new();
        let mut stack = vec![root_name.to_string()];

        while let Some(name) = stack.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }

            let path = dir.join(format!("{}.md", name));
            if !path.exists() {
                let referenced_by: Option<String> = agents
                    .iter()
                    .find(|(_, a)| a.subagents.contains(&name))
                    .map(|(n, _)| n.clone());
                let msg = match referenced_by {
                    Some(by) => format!(
                        "Agent '{}' referenced by '{}' not found at '{}' . Check your agents directory.",
                        name,
                        by,
                        path.display()
                    ),
                    None => format!(
                        "Root agent '{}' not found at '{}'. Check your agents directory.",
                        name,
                        path.display()
                    ),
                };
                anyhow::bail!("{}", msg);
            }

            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read agent file '{}'", path.display()))?;

            let agent = AgentConfig::parse(&name, &content)
                .with_context(|| format!("Failed to parse agent '{}'", name))?;

            // Queue dependencies for discovery.
            for sub in &agent.subagents {
                stack.push(sub.clone());
            }
            if let Some(ref reviewer) = agent.output_reviewer {
                stack.push(reviewer.clone());
            }

            // Validate immediately after parsing — fail fast on tool/config errors
            agent
                .validate(available_tools)
                .with_context(|| format!("Failed to validate agent '{}'", name))?;

            eprintln!("  Loaded agent: {} ({} tools)", name, agent.tools.len());
            agents.insert(name, agent);
        }

        // Cross-reference validation: every output_reviewer must be in the loaded set.
        for agent in agents.values() {
            if let Some(reviewer) = &agent.output_reviewer
                && !agents.contains_key(reviewer)
            {
                anyhow::bail!(
                    "Agent '{}' references output_reviewer '{}' which is not reachable. \
                         Loaded agents: {}",
                    agent.name,
                    reviewer,
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }

        if agents.is_empty() {
            anyhow::bail!("No agents found in '{}'", dir.display());
        }

        Ok(Self { agents })
    }

    /// Build a registry from pre-constructed agent configs (e.g. from a job request).
    pub fn from_configs(configs: Vec<AgentConfig>, available_tools: &[String]) -> Result<Self> {
        let mut agents = HashMap::new();
        for agent in configs {
            agent.validate(available_tools)?;
            eprintln!(
                "  Loaded agent: {} ({} tools)",
                agent.name,
                agent.tools.len()
            );
            agents.insert(agent.name.clone(), agent);
        }

        if agents.is_empty() {
            anyhow::bail!("No agents provided");
        }

        // Cross-reference validation: output_reviewer must point at a loaded agent.
        for agent in agents.values() {
            if let Some(reviewer) = &agent.output_reviewer
                && !agents.contains_key(reviewer)
            {
                anyhow::bail!(
                    "Agent '{}' references unknown output_reviewer '{}'. \
                         Available agents: {}",
                    agent.name,
                    reviewer,
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }

        Ok(Self { agents })
    }

    /// Override the model for all loaded agents.
    pub fn apply_model_override(&mut self, model: &str) {
        for agent in self.agents.values_mut() {
            agent.model = model.to_string();
        }
    }

    /// Get an agent by name
    pub fn get(&self, name: &str) -> Result<&AgentConfig> {
        self.agents.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Agent '{}' not found. Available: {}",
                name,
                self.agents.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// Iterate over all loaded agents.
    pub fn iter(&self) -> impl Iterator<Item = &AgentConfig> {
        self.agents.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_with_frontmatter() {
        let content = r#"---
tools:
  - grep
  - read_file
model: test-model
description: Test agent for unit tests
reasoning_effort: Medium
---
You are an assistant."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        assert_eq!(agent.tools, vec!["grep", "read_file"]);
        assert_eq!(agent.prompt, "You are an assistant.");
        assert_eq!(agent.description, "Test agent for unit tests");
    }

    #[test]
    fn test_parse_with_tool_configs() {
        let content = r#"---
tools:
  - artifact
model: test-model
description: Test agent
reasoning_effort: Medium
tool_configs:
  artifact:
    read:
      - threat_model
    write:
      - vulnerability_*
subagents:
  - attack-surface
---
Prompt."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        let perms = agent.artifact_permissions();
        assert!(perms.can_read("threat_model"));
        assert!(!perms.can_read("secret"));
        assert!(perms.can_write("vulnerability_xss"));
        assert!(!perms.can_write("threat_model"));
        assert!(agent.can_spawn("attack-surface"));
        assert!(!agent.can_spawn("unknown"));
    }

    #[test]
    fn test_artifact_read_entry_plain() {
        let entry = serde_yaml::from_str::<ArtifactReadEntry>("finding_*").unwrap();
        assert_eq!(entry.key(), "finding_*");
        assert_eq!(entry.tag(), None);
        assert!(!entry.is_input());
    }

    #[test]
    fn test_artifact_read_entry_tagged() {
        let entry = serde_yaml::from_str::<ArtifactReadEntry>("{threat_model: input}").unwrap();
        assert_eq!(entry.key(), "threat_model");
        assert_eq!(entry.tag(), Some("input"));
        assert!(entry.is_input());
    }

    #[test]
    fn test_artifact_read_entry_tagged_empty() {
        let entry = serde_yaml::from_str::<ArtifactReadEntry>("{finding_: ''}").unwrap();
        assert_eq!(entry.key(), "finding_");
        assert_eq!(entry.tag(), Some(""));
        assert!(!entry.is_input());
    }

    #[test]
    fn test_input_artifacts_from_read_tagged() {
        let content = r#"---
model: test-model
description: Test agent with tagged read entries
reasoning_effort: Medium
tool_configs:
  artifact:
    read:
      - threat_model: input
      - attack_surface: input
      - "finding_*"
    write:
      - vulnerability_*
---
Prompt."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        let perms = agent.artifact_permissions();
        assert!(perms.can_read("threat_model"));
        assert!(perms.can_read("attack_surface"));
        assert!(perms.can_read("finding_xss"));
        assert!(perms.can_write("vulnerability_test"));

        let input = agent.input_artifacts();
        assert_eq!(input, vec!["threat_model", "attack_surface"]);
    }

    #[test]
    fn test_input_artifacts_no_tag_means_regular_read() {
        let content = r#"---
model: test-model
description: Test agent with plain read entries
reasoning_effort: Medium
tool_configs:
  artifact:
    read:
      - finding_*  
      - threat_model
    write:
      - vulnerability_*
---
Prompt."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        let perms = agent.artifact_permissions();
        assert!(perms.can_read("threat_model"));
        assert!(perms.can_read("finding_xss"));

        let input = agent.input_artifacts();
        assert!(input.is_empty());
    }

    #[test]
    fn test_deny_unknown_fields() {
        let content = r#"---
tools:
  - artifact
model: test-model
description: Test agent
reasoning_effort: Medium
permissions:
  artifact:
    read:
      - threat_model
---
Prompt."#;

        // `permissions` is no longer a recognized field — deny_unknown_fields rejects it
        assert!(AgentConfig::parse("test", content).is_err());
    }

    #[test]
    fn test_fail_closed_defaults() {
        // can_read/can_write with empty patterns = denied
        let artifact = ArtifactPermissions::default();
        assert!(!artifact.can_read("any"));
        assert!(!artifact.can_write("any"));

        // can_spawn with empty subagents = denied
        let agent = AgentConfig {
            name: "test".to_string(),
            tools: vec![],
            model: "m".to_string(),
            description: "d".to_string(),
            prompt: String::new(),
            output_schema: None,
            reasoning_effort: flawrence_chat::ReasoningEffort::Medium,
            todos: vec![],
            tool_configs: HashMap::new(),
            subagents: vec![],
            output_reviewer: None,
            params: HashMap::new(),
            max_iterations: None,
            output_grounding: None,
        };
        assert!(!agent.can_spawn("any"));
    }

    #[test]
    fn test_load_reachable_happy_path() {
        use std::fs;

        let dir = std::env::temp_dir().join("flawrence_test_agents");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("root.md"),
            r#"---
model: test-model
description: Root agent
reasoning_effort: Medium
subagents:
  - child
---
Root prompt."#,
        )
        .unwrap();

        fs::write(
            dir.join("child.md"),
            r#"---
model: test-model
description: Child agent
reasoning_effort: Medium
---
Child prompt."#,
        )
        .unwrap();

        fs::write(
            dir.join("orphan.md"),
            r#"---
model: test-model
description: Orphan agent
reasoning_effort: Medium
---
Orphan prompt."#,
        )
        .unwrap();

        let tools = vec!["artifact".to_string()];
        let registry = AgentRegistry::load_reachable(&dir, "root", &tools).unwrap();

        assert!(registry.get("root").is_ok());
        assert!(registry.get("child").is_ok());
        assert!(registry.get("orphan").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_reachable_missing_root() {
        use std::fs;

        let dir = std::env::temp_dir().join("flawrence_test_missing_root");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let tools: Vec<String> = vec![];
        let result = AgentRegistry::load_reachable(&dir, "nonexistent", &tools);
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("Root agent 'nonexistent' not found"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_reachable_missing_subagent() {
        use std::fs;

        let dir = std::env::temp_dir().join("flawrence_test_missing_sub");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("root.md"),
            r#"---
model: test-model
description: Root agent
reasoning_effort: Medium
subagents:
  - missing_child
---
Root prompt."#,
        )
        .unwrap();

        let tools: Vec<String> = vec![];
        let result = AgentRegistry::load_reachable(&dir, "root", &tools);
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("referenced by 'root'"));
        assert!(err.contains("missing_child"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_reachable_output_reviewer() {
        use std::fs;

        let dir = std::env::temp_dir().join("flawrence_test_reviewer");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            dir.join("root.md"),
            r#"---
model: test-model
description: Root agent
reasoning_effort: Medium
output_reviewer: reviewer
---
Root prompt."#,
        )
        .unwrap();

        fs::write(
            dir.join("reviewer.md"),
            r#"---
model: test-model
description: Reviewer agent
reasoning_effort: Medium
---
Reviewer prompt."#,
        )
        .unwrap();

        let tools: Vec<String> = vec![];
        let registry = AgentRegistry::load_reachable(&dir, "root", &tools).unwrap();

        assert!(registry.get("root").is_ok());
        assert!(registry.get("reviewer").is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_with_tagged_read_entries() {
        let content = r#"---
model: test-model
description: Agent with tagged artifact read entries
reasoning_effort: Medium
tool_configs:
  artifact:
    read:
      - threat_model: input
      - attack_surface: input
      - finding_*
    write:
      - vulnerability_*
---
Prompt."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        let perms = agent.artifact_permissions();
        assert!(perms.can_read("threat_model"));
        assert!(perms.can_read("attack_surface"));
        assert!(perms.can_read("finding_xss"));
        assert!(perms.can_write("vulnerability_test"));

        let inputs = agent.input_artifacts();
        assert_eq!(inputs, vec!["threat_model", "attack_surface"]);
    }

    #[test]
    fn test_parse_with_plain_read_entries() {
        let content = r#"---
model: test-model
description: Agent with plain artifact read entries
reasoning_effort: Medium
tool_configs:
  artifact:
    read:
      - threat_model
      - finding_*
    write:
      - vulnerability_*
---
Prompt."#;

        let agent = AgentConfig::parse("test", content).unwrap();
        let perms = agent.artifact_permissions();
        assert!(perms.can_read("threat_model"));
        assert!(perms.can_read("finding_xss"));
        assert!(perms.can_write("vulnerability_test"));

        // No entries tagged with "input", so no input artifacts
        let inputs = agent.input_artifacts();
        assert!(inputs.is_empty());
    }

    #[test]
    fn test_apply_params_substitution() {
        let content = r#"---
model: test-model
description: Agent with params
reasoning_effort: Medium
params:
  target: nginx
  subsystem: "the auth module"
---
You are analyzing a {{target}} codebase.
Focus on: {{subsystem}}."#;

        let mut agent = AgentConfig::parse("test", content).unwrap();
        assert!(agent.prompt.contains("{{target}}"));
        assert!(agent.prompt.contains("{{subsystem}}"));

        // Apply default params (no overrides)
        let overrides = HashMap::new();
        agent.apply_params(&overrides);
        assert!(agent.prompt.contains("nginx"));
        assert!(agent.prompt.contains("the auth module"));
        assert!(!agent.prompt.contains("{{target}}"));
        assert!(!agent.prompt.contains("{{subsystem}}"));
    }

    #[test]
    fn test_apply_params_cli_override() {
        let content = r#"---
model: test-model
description: Agent with params
reasoning_effort: Medium
params:
  target: nginx
---
You are analyzing a {{target}} codebase."#;

        let mut agent = AgentConfig::parse("test", content).unwrap();

        // CLI override
        let mut overrides = HashMap::new();
        overrides.insert("target".to_string(), "nodejs".to_string());
        agent.apply_params(&overrides);

        assert!(agent.prompt.contains("nodejs"));
        assert!(!agent.prompt.contains("nginx"));
        assert!(!agent.prompt.contains("{{target}}"));
    }

    #[test]
    fn test_unresolved_placeholders_reports_missing() {
        let content = r#"---
model: test-model
description: uses a missing param
reasoning_effort: Medium
params:
  target: nginx
---
You analyze {{target}}. Output dir is {{output_dir}}."#;

        let mut agent = AgentConfig::parse("test", content).unwrap();
        // No overrides, no default for output_dir → it stays unresolved.
        let overrides = HashMap::new();
        agent.apply_params(&overrides);
        let missing = agent.unresolved_placeholders();
        assert_eq!(missing, vec!["output_dir".to_string()]);
    }

    #[test]
    fn test_resolve_params_ok_when_all_bound() {
        let content = r#"---
model: test-model
description: all params bound
reasoning_effort: Medium
params:
  target: nginx
  output_dir: NOT_PROVIDED
---
Target {{target}}, output {{output_dir}}."#;

        let mut agent = AgentConfig::parse("test", content).unwrap();
        let overrides = HashMap::new();
        // Both placeholders resolve (one via default).
        assert!(agent.resolve_params(&overrides).is_ok());
        assert!(!agent.prompt.contains("{{"));
        assert!(agent.prompt.contains("NOT_PROVIDED"));

        // Override wins over default — on a fresh (unresolved) agent, since
        // apply_params is destructive and each agent instance is resolved once.
        let mut agent = AgentConfig::parse("test", content).unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("output_dir".to_string(), "/tmp/out".to_string());
        assert!(agent.resolve_params(&overrides).is_ok());
        assert!(agent.prompt.contains("/tmp/out"));
    }

    #[test]
    fn test_parse_output_grounding_flat_map() {
        let content = r#"---
model: test-model
description: grounding
reasoning_effort: Medium
output_grounding:
  passed:
    tool: run_check
    source: result
    path: passed
    when: last
  verified_command:
    tool: run_check
    source: args
    path: command
    when: last
    filter:
      path: passed
      value: true
---
Prompt."#;
        let agent = AgentConfig::parse("test", content).unwrap();
        let grounding = agent.output_grounding.expect("output_grounding");
        let passed = grounding.0.get("passed").expect("passed binding");
        assert_eq!(passed.tool, "run_check");
        assert_eq!(passed.source, "result");
        assert_eq!(passed.path, "passed");
        assert!(passed.filter.is_none());
        let corrected = grounding
            .0
            .get("verified_command")
            .expect("verified_command binding");
        let filter = corrected.filter.as_ref().expect("filter");
        assert_eq!(filter.path, "passed");
        assert_eq!(filter.value, serde_json::Value::Bool(true));
    }

    #[test]
    fn test_parse_max_iterations() {
        let content = r#"---
model: test-model
description: cap
reasoning_effort: Medium
max_iterations: 25
---
Prompt."#;
        let agent = AgentConfig::parse("test", content).unwrap();
        assert_eq!(agent.max_iterations, Some(25));
    }

    #[test]
    fn test_resolve_params_errors_on_unresolved() {
        let content = r#"---
model: test-model
description: missing param
reasoning_effort: Medium
---
Needs {{output_dir}}."#;

        let mut agent = AgentConfig::parse("test", content).unwrap();
        let overrides = HashMap::new();
        let err = agent.resolve_params(&overrides).unwrap_err().to_string();
        assert!(err.contains("'test'"), "error should name the agent: {err}");
        assert!(
            err.contains("output_dir"),
            "error should name the missing param: {err}"
        );
        assert!(
            err.contains("--arg output_dir"),
            "error should suggest the fix: {err}"
        );
    }
}
