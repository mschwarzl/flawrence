//! Cross-crate check: every shipped agent definition must actually load.
//!
//! `agents/*.md` is edited constantly — by hand, and by agents editing each
//! other's prompts — and nothing else in the suite reads the real files. The
//! unit tests in `flawrence-agent` parse inline fixture strings, so a YAML typo
//! in a real definition surfaced only at runtime, potentially hours into a run.
//!
//! The `output_schema` check matters more than it looks. `TaskLoop` builds its
//! validator with `if let Ok(validator) = jsonschema::validator_for(schema)`,
//! so a schema that fails to compile does not raise an error — it silently
//! disables `submit_result` validation for that agent entirely. A broken schema
//! is therefore invisible in production and has to be caught here.

use std::path::{Path, PathBuf};

fn agents_dir() -> PathBuf {
    // tests/ -> flawrence-task/ -> lib/ -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agents")
        .canonicalize()
        .expect("agents/ directory next to the workspace root")
}

fn definitions() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(agents_dir())
        .expect("agents/ is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or_default()
                .to_string();
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()));
            (name, body)
        })
        .collect();
    found.sort();
    assert!(
        found.len() >= 19,
        "expected the full agent set, found {}",
        found.len()
    );
    found
}

#[test]
fn every_agent_definition_parses() {
    let mut broken = Vec::new();
    for (name, body) in definitions() {
        if let Err(error) = flawrence_agent::AgentConfig::parse(&name, &body) {
            broken.push(format!("{name}: {error:#}"));
        }
    }
    assert!(
        broken.is_empty(),
        "unparseable agent definitions:\n{}",
        broken.join("\n")
    );
}

#[test]
fn every_agent_declares_only_tools_that_exist() {
    let available = flawrence_tool::registry().tool_names();
    let mut invalid = Vec::new();
    for (name, body) in definitions() {
        let Ok(config) = flawrence_agent::AgentConfig::parse(&name, &body) else {
            continue; // reported by the parse test
        };
        if let Err(error) = config.validate(&available) {
            invalid.push(format!("{name}: {error:#}"));
        }
    }
    assert!(
        invalid.is_empty(),
        "agents referencing unknown tools:\n{}",
        invalid.join("\n")
    );
}

/// Levenshtein, duplicated here because the matcher's copy is `pub(crate)`
/// and an integration test is an external crate.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ac) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ac != *bc);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Collect every `enum` array anywhere in a schema, with the path to it.
fn enums(schema: &serde_json::Value, path: &str, out: &mut Vec<(String, Vec<String>)>) {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Array(values)) = map.get("enum") {
                let values: Vec<String> = values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                if !values.is_empty() {
                    out.push((path.to_string(), values));
                }
            }
            for (key, value) in map {
                enums(value, &format!("{path}/{key}"), out);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                enums(item, &format!("{path}/{index}"), out);
            }
        }
        _ => {}
    }
}

/// Enum values must be far enough apart that a near-miss cannot be repaired
/// into the WRONG one.
///
/// The hint machinery suggests a permitted value for a rejected one, and its
/// only safe footing is that unrelated values are further apart than related
/// ones. Two real violations shipped before this test existed: `patch is
/// correct`/`patch is incorrect` at 2 edits, and `bypass_found` nested inside
/// `no_bypass_found` — both on verdict fields, where a repaired-to-the-opposite
/// hint inverts the result. This is a ratchet: it fails on a NEW offender, so
/// the class cannot come back through a schema edit.
#[test]
fn enum_values_are_mutually_distinct_enough_to_repair_safely() {
    let mut offenders = Vec::new();
    for (agent, body) in definitions() {
        let Ok(config) = flawrence_agent::AgentConfig::parse(&agent, &body) else {
            continue;
        };
        let Some(schema) = config.output_schema else {
            continue;
        };
        let mut found = Vec::new();
        enums(&schema, "", &mut found);
        for (path, values) in found {
            for (i, left) in values.iter().enumerate() {
                for right in values.iter().skip(i + 1) {
                    let l = left.to_ascii_lowercase();
                    let r = right.to_ascii_lowercase();
                    if l.contains(&r) || r.contains(&l) {
                        offenders.push(format!(
                            "{agent}{path}: {left:?} and {right:?} — one contains the other"
                        ));
                    } else if edit_distance(&l, &r) <= 2 {
                        offenders.push(format!(
                            "{agent}{path}: {left:?} and {right:?} — only {} edits apart",
                            edit_distance(&l, &r)
                        ));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "enum values too close to repair safely:\n{}",
        offenders.join("\n")
    );
}

/// Every `enum` must offer at least one value, and no duplicates. An empty
/// enum is unsatisfiable, so nothing an agent submits could ever validate.
#[test]
fn every_enum_is_non_empty_and_free_of_duplicates() {
    let mut broken = Vec::new();
    for (agent, body) in definitions() {
        let Ok(config) = flawrence_agent::AgentConfig::parse(&agent, &body) else {
            continue;
        };
        let Some(schema) = config.output_schema else {
            continue;
        };
        let mut found = Vec::new();
        enums(&schema, "", &mut found);
        for (path, values) in found {
            let mut unique = values.clone();
            unique.sort();
            unique.dedup();
            if unique.len() != values.len() {
                broken.push(format!("{agent}{path}: duplicate values in {values:?}"));
            }
        }
    }
    assert!(broken.is_empty(), "malformed enums:\n{}", broken.join("\n"));
}

fn skills_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills")
        .canonicalize()
        .expect("skills/ directory next to the workspace root")
}

/// Per-build-system reproducer guidance lives in skills, so recording the idiom
/// for another build system is a new directory rather than an edit to this
/// agent and a rebuild. The bootstrap must keep pointing at them, and must not
/// quietly reabsorb the detail: a section here would be read instead of the
/// skill and would go stale without anything failing.
#[test]
fn build_system_idioms_stay_in_skills_and_the_bootstrap_points_at_them() {
    let agent = std::fs::read_to_string(agents_dir().join("test-idiom.md"))
        .expect("test-idiom.md is readable");

    assert!(
        agent.contains("test-idiom-gradle") && agent.contains("test-idiom-maven"),
        "the bootstrap must name the skills it expects to load"
    );
    // A build system the harness has no skill for must not be a dead end.
    assert!(
        agent.contains("If no skill matches"),
        "the bootstrap must say what to do when no idiom skill exists"
    );

    for system in ["gradle", "maven", "cargo", "python", "go", "make", "cmake"] {
        let skill = skills_dir()
            .join(format!("test-idiom-{system}"))
            .join("SKILL.md");
        let raw = std::fs::read_to_string(&skill)
            .unwrap_or_else(|_| panic!("{} is readable", skill.display()));
        // Collapsed, because these are wrapped markdown files: any phrase long
        // enough to be worth asserting on is long enough to straddle a line
        // break, and a test that breaks on reflowing prose teaches nothing.
        let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");

        // Frontmatter the loader needs, with the name matching its directory.
        assert!(raw.starts_with("---\n"), "{system}: missing frontmatter");
        assert!(
            text.contains(&format!("name: test-idiom-{system}")),
            "{system}: name must match its directory"
        );
        assert!(
            text.contains("description:"),
            "{system}: needs a description"
        );
        // Enough substance to be worth loading, and anchored to its own system.
        assert!(
            raw.len() > 1_000,
            "{system}: {} chars is too thin",
            raw.len()
        );
        // Every idiom must end somewhere other than "it did not work": name the
        // project's own test route as the fallback.
        assert!(
            text.contains("instead when") || text.contains("project test"),
            "{system}: no fallback route recorded"
        );

        // And every idiom must bound what it executes. The container cannot run
        // an emulator, a browser or a foreign target, and each of those fails in
        // a way that reads like the agent's reproducer being wrong -- so an
        // agent that tries burns its budget on the environment and proves
        // nothing about the code.
        assert!(
            text.contains("Nothing else.") || text.contains("sketch-only"),
            "{system}: does not bound what it executes"
        );
        // The gap is a deliverable, handed to a human who can run it -- but only
        // if it is never mistaken for evidence.
        assert!(
            text.contains("never executed"),
            "{system}: no sketch-for-a-human route"
        );
        assert!(
            text.contains("does not support one"),
            "{system}: does not say a sketch is not evidence"
        );
        assert!(
            !agent.contains("## Kotlin/JVM and Gradle")
                && !agent.contains("## Java/Kotlin and Maven"),
            "{system}: the bootstrap has reabsorbed a per-system section"
        );
    }

    // Each skill must be about its own build system, not a copy of the other.
    let gradle = std::fs::read_to_string(skills_dir().join("test-idiom-gradle/SKILL.md")).unwrap();
    let maven = std::fs::read_to_string(skills_dir().join("test-idiom-maven/SKILL.md")).unwrap();
    assert!(
        gradle.contains("--init-script"),
        "gradle skill lost its classpath export"
    );
    assert!(
        maven.contains("dependency:build-classpath"),
        "maven skill lost its export"
    );
    assert!(
        !gradle.contains("dependency:build-classpath"),
        "gradle skill carries Maven's route"
    );
}
