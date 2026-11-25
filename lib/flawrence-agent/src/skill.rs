use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(feature = "std")]
use std::fs;
#[cfg(feature = "std")]
use std::path::Path;

/// A loaded skill with metadata
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Registry holding all pre-loaded skills
#[derive(Clone)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Load all skills from the skills/ directory.
    /// `FLAWRENCE_SKILLS_DIR` overrides the default `skills` lookup path.
    #[cfg(feature = "std")]
    pub fn load_all() -> Result<Self> {
        let dir = std::env::var("FLAWRENCE_SKILLS_DIR").unwrap_or_else(|_| "skills".to_string());
        Self::load_from_dir(dir)
    }

    /// Load all skills from a specific directory
    #[cfg(feature = "std")]
    pub fn load_from_dir<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref();
        let mut skills = HashMap::new();

        if !dir.exists() {
            // Skills are optional - return empty registry if directory doesn't exist
            return Ok(Self { skills });
        }

        for entry in fs::read_dir(dir)
            .with_context(|| format!("Failed to read skills directory '{}'", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();

            // Skills are in subdirectories with SKILL.md files
            if path.is_dir() {
                let skill_file = path.join("SKILL.md");
                if skill_file.exists() {
                    let name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Invalid skill directory: {}", path.display())
                        })?
                        .to_string();

                    let content = fs::read_to_string(&skill_file).with_context(|| {
                        format!("Failed to read skill file '{}'", skill_file.display())
                    })?;

                    let skill = Self::parse_skill(&name, &content)
                        .with_context(|| format!("Failed to parse skill '{}'", name))?;

                    eprintln!("  Loaded skill: {}", skill.name);
                    skills.insert(name, skill);
                }
            }
        }

        Ok(Self { skills })
    }

    fn parse_skill(name: &str, content: &str) -> Result<Skill> {
        if let Some(stripped) = content.strip_prefix("---")
            && let Some(end_idx) = stripped.find("---")
        {
            let yaml_content = stripped[..end_idx].trim();
            let skill_content = stripped[end_idx + 3..].trim().to_string();

            // Parse YAML frontmatter for description
            #[derive(Deserialize)]
            struct SkillMeta {
                description: String,
            }

            let meta = serde_yaml::from_str::<SkillMeta>(yaml_content).with_context(|| {
                format!(
                    "Skill '{}' must have YAML frontmatter with 'description' field",
                    name
                )
            })?;

            if meta.description.is_empty() {
                anyhow::bail!("Skill '{}' must have a non-empty description", name);
            }

            return Ok(Skill {
                name: name.to_string(),
                description: meta.description,
                content: skill_content,
            });
        }

        anyhow::bail!(
            "Skill '{}' must have YAML frontmatter with 'description' field. Format:\n---\ndescription: Skill purpose\n---\nSkill content here",
            name
        )
    }

    /// Build a registry from pre-constructed skills (e.g. from a job request).
    pub fn from_skills(skills: Vec<Skill>) -> Self {
        let map = skills.into_iter().map(|s| (s.name.clone(), s)).collect();
        Self { skills: map }
    }

    /// Get a skill by name
    pub fn get(&self, name: &str) -> Result<&Skill> {
        self.skills.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Skill '{}' not found. Available: {}",
                name,
                self.skills.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// Format skills as a string for injection into prompts
    /// Names of every loaded skill, sorted. Used to constrain `load_skill`'s
    /// argument to what actually exists.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.skills.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn format_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut output = String::from("<available_skills>\n");

        for skill in self.skills.values() {
            output.push_str(&format!(
                "  <skill name=\"{}\">{}</skill>\n",
                skill.name, skill.description
            ));
        }

        output.push_str("</available_skills>");
        output
    }
}
