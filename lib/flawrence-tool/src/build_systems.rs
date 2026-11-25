//! Build-system detection, declared in skills rather than compiled in.
//!
//! Each `test-idiom-*` skill carries the rules for recognising its build system
//! and the commands that are expensive enough to need a verifier lane. Adding a
//! build system is then a file, not an edit and a rebuild -- which matters
//! because the alternative is that detection always lags behind the guidance
//! written for it, and a project whose idiom exists sits unrecognised.
//!
//! The trade is that a typo can no longer be a compile error. So a skill that
//! declares detection and gets it wrong aborts the run naming the file, rather
//! than being skipped into a silent "no build system found".

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The detection and execution rules one build system declares.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BuildSystemSpec {
    /// Display name, and the key facts derivation dispatches on.
    pub name: String,
    /// Files whose presence at the workspace root identifies this system.
    #[serde(default)]
    pub any_of: Vec<String>,
    /// Lower wins when several systems match.
    ///
    /// Ordering is the whole difficulty. A `Makefile` is a completely reliable
    /// file and a weak signal -- plenty of projects ship one that wraps cargo,
    /// cmake or gradle -- so Make must sort after every system with a
    /// distinctive manifest, or the harness follows the wrapper instead of the
    /// build.
    pub priority: u32,
    /// Command words that mean "this is a real build": expensive, memory-hungry
    /// and worth a lane and a concurrency permit. Cheap per-file compiles are
    /// deliberately absent -- they must not queue behind a build.
    #[serde(default)]
    pub lane_commands: Vec<String>,
    /// How to invoke this build system, most specific first. The first entry
    /// whose `when_file` exists at the workspace root wins; an entry without
    /// one always matches, so it serves as the default.
    ///
    /// This is what lets a project's pinned wrapper outrank whatever is on
    /// PATH -- `./gradlew` and `./mvnw` exist precisely so the build runs on
    /// the version the project chose.
    #[serde(default)]
    pub program: Vec<ProgramChoice>,
    /// A fixed pre-warm command, with `{program}` substituted.
    #[serde(default)]
    pub prewarm: Option<String>,
    /// A pre-warm whose command cannot be written down in advance, because it
    /// depends on what the project turns out to offer.
    #[serde(default)]
    pub prewarm_derive: Option<PrewarmDerive>,
    /// Globs, relative to a lane, where this system leaves machine-readable
    /// test reports. A lane that runs a build and copies nothing back looks
    /// exactly like a test that never ran.
    #[serde(default)]
    pub test_reports: Vec<String>,
}

/// One way of invoking a build system, and when it applies.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ProgramChoice {
    /// Present at the workspace root for this choice to apply. Absent means
    /// "always", which is how the fallback is written.
    #[serde(default)]
    pub when_file: Option<String>,
    /// The executable, as it should appear in a command the operator could
    /// paste and re-run.
    pub run: String,
}

/// A pre-warm derived by asking the build system what it offers.
///
/// Gradle is why this exists: which compile tasks a project has depends on the
/// plugins it applies, so no fixed command works for every project. Probe once,
/// then take the first candidate whose tasks all appear -- which is cheap to
/// express and covers any system that can list its own targets.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PrewarmDerive {
    /// Run once to find out what exists. `{program}` is substituted.
    pub probe: String,
    /// Ordered; the first that applies wins.
    pub candidates: Vec<PrewarmCandidate>,
    /// The pre-warm itself, with `{program}` and `{tasks}` substituted.
    pub command: String,
}

/// One possible pre-warm target set, and the conditions for choosing it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PrewarmCandidate {
    /// All of these must appear in the probe output for this candidate to
    /// apply, and they become `{tasks}`.
    pub tasks: Vec<String>,
    /// Human phrase for the project shape this candidate implies.
    #[serde(default)]
    pub flavour: Option<String>,
    /// Any of these appearing in the probe output disqualifies this candidate,
    /// even when its own tasks are present.
    ///
    /// Gradle's `assemble` needs this: it exists everywhere, but on a
    /// multiplatform project it drags in every declared target and spends an
    /// hour producing artifacts no agent will run.
    #[serde(default)]
    pub excluded_by: Vec<String>,
}

/// Only the fields this module cares about; a skill's own metadata is ignored.
#[derive(Deserialize)]
struct SkillFrontmatter {
    #[serde(default)]
    build_system: Option<BuildSystemSpec>,
}

/// Every declared build system, ordered by priority.
///
/// Loaded once. An empty result is legitimate -- a deployment may ship no
/// idiom skills at all -- but a *malformed* declaration is not, and fails here.
pub fn load_specs(skills_dir: &Path) -> Result<Vec<BuildSystemSpec>> {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return Ok(Vec::new());
    };
    let mut specs: Vec<(PathBuf, BuildSystemSpec)> = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let manifest = entry.path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Some(front) = frontmatter(&text) else {
            continue;
        };
        // Parsed strictly: a skill that declares a build system and gets the
        // shape wrong must not be read as one that declares nothing.
        let parsed: SkillFrontmatter = serde_yaml::from_str(front).with_context(|| {
            format!(
                "{} has frontmatter that does not parse; fix it or remove the \
                 build_system block",
                manifest.display()
            )
        })?;
        if let Some(spec) = parsed.build_system {
            if spec.any_of.is_empty() {
                bail!(
                    "{} declares build_system '{}' with no `any_of` files, so it can never be \
                     detected",
                    manifest.display(),
                    spec.name
                );
            }
            specs.push((manifest, spec));
        }
    }

    specs.sort_by(|a, b| {
        a.1.priority
            .cmp(&b.1.priority)
            .then(a.1.name.cmp(&b.1.name))
    });
    let mut seen: HashMap<u32, String> = HashMap::new();
    for (manifest, spec) in &specs {
        if let Some(other) = seen.insert(spec.priority, spec.name.clone()) {
            bail!(
                "{} gives '{}' priority {}, already used by '{}'. Detection order decides which \
                 system wins when both match, so it must not be a tie.",
                manifest.display(),
                spec.name,
                spec.priority,
                other
            );
        }
    }
    Ok(specs.into_iter().map(|(_, spec)| spec).collect())
}

/// Split out the YAML frontmatter of a skill file.
fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// The declared systems, loaded once from the skills directory.
///
/// A malformed declaration is fatal at load, but this accessor cannot return an
/// error to every caller, so the failure is reported once and detection then
/// finds nothing -- which the pre-warm already handles as "no recognised build
/// system". Startup validation exists to make that unreachable in practice.
pub fn loaded() -> &'static [BuildSystemSpec] {
    static SPECS: std::sync::OnceLock<Vec<BuildSystemSpec>> = std::sync::OnceLock::new();
    SPECS.get_or_init(|| match load_specs(&skills_dir()) {
        Ok(specs) => specs,
        Err(error) => {
            tracing::error!("build-system detection is unavailable: {error:#}");
            Vec::new()
        }
    })
}

/// Where skills live, matching how the skill registry resolves them.
fn skills_dir() -> PathBuf {
    std::env::var("FLAWRENCE_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("skills"))
}

/// Validate the declarations without caching them, for startup.
///
/// Called before any run begins so a broken skill stops the harness with the
/// file name, rather than being read as a skill that declares nothing and
/// leaving that build system silently undetectable.
pub fn validate() -> Result<usize> {
    load_specs(&skills_dir()).map(|specs| specs.len())
}

/// The highest-priority system whose marker files are present at `root`.
#[must_use]
pub fn detect<'a>(specs: &'a [BuildSystemSpec], root: &Path) -> Option<&'a BuildSystemSpec> {
    specs
        .iter()
        .find(|spec| spec.any_of.iter().any(|file| root.join(file).is_file()))
}

/// How to invoke this system here, honouring a wrapper the project ships.
#[must_use]
pub fn resolve_program(spec: &BuildSystemSpec, root: &Path) -> Option<String> {
    spec.program
        .iter()
        .find(|choice| {
            choice
                .when_file
                .as_ref()
                .is_none_or(|file| root.join(file).is_file())
        })
        .map(|choice| choice.run.clone())
}

/// Fill `{program}` and `{tasks}` in a declared command template.
#[must_use]
pub fn fill(template: &str, program: &str, tasks: &str) -> String {
    template
        .replace("{program}", program)
        .replace("{tasks}", tasks)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first candidate whose tasks are all present and which nothing
/// disqualifies.
#[must_use]
pub fn choose_candidate<'a>(
    derive: &'a PrewarmDerive,
    available: &[String],
) -> Option<&'a PrewarmCandidate> {
    let has = |name: &String| available.iter().any(|known| known == name);
    derive
        .candidates
        .iter()
        .find(|candidate| candidate.tasks.iter().all(has) && !candidate.excluded_by.iter().any(has))
}

/// Whether a shell command launches a build heavy enough to need a lane.
///
/// Fuzzy on purpose, and biased: a false positive costs one unnecessary permit,
/// while a missed build runs outside the concurrency cap and can exhaust the
/// box. Wrong in the cheap direction.
#[must_use]
pub fn is_lane_command(specs: &[BuildSystemSpec], command: &str) -> bool {
    // Every word, not just the one in command position. Agents write
    // `cd $WORKSPACE && timeout 600 ./gradlew ...`, and a rule that only looked
    // at the first word would let that build run outside the cap entirely.
    // The cost is the occasional false positive -- `grep -rn make README.md`
    // takes a permit it does not need -- which is one wasted permit against an
    // unbounded build.
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .map(|word| word.trim_matches(['\'', '"']))
        .filter_map(|word| word.rsplit('/').next())
        .collect();
    specs.iter().any(|spec| {
        spec.lane_commands
            .iter()
            .any(|name| words.iter().any(|word| word == name))
    })
}

/// Every place a build system leaves machine-readable test reports.
#[must_use]
pub fn test_report_patterns(specs: &[BuildSystemSpec]) -> Vec<String> {
    let mut patterns: Vec<String> = specs
        .iter()
        .flat_map(|spec| spec.test_reports.iter().cloned())
        .collect();
    patterns.sort();
    patterns.dedup();
    patterns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../skills")
            .canonicalize()
            .expect("skills/ beside the workspace root")
    }

    fn write_skill(dir: &Path, name: &str, frontmatter: &str) {
        let skill = dir.join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"d\"\n{frontmatter}---\n\nbody\n"),
        )
        .unwrap();
    }

    /// The shipped skills are the real detection table. A typo here is no longer
    /// a compile error, so it has to be a test failure.
    #[test]
    fn every_shipped_idiom_declares_a_usable_build_system() {
        let specs = load_specs(&skills_dir()).expect("shipped skills must parse");
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();

        for expected in ["Gradle", "Maven", "Cargo", "Go", "Python", "CMake", "Make"] {
            assert!(
                names.contains(&expected),
                "{expected} is not detectable: {names:?}"
            );
        }
        for spec in &specs {
            assert!(!spec.any_of.is_empty(), "{}: no marker files", spec.name);
            // A system with no unconditional program choice can fail to resolve
            // on a project that ships no wrapper, leaving a pre-warm that
            // silently never runs.
            assert!(
                spec.program.iter().any(|choice| choice.when_file.is_none()),
                "{}: no default program, so it may resolve to nothing",
                spec.name
            );
            // Declaring both a fixed and a derived pre-warm is ambiguous.
            assert!(
                !(spec.prewarm.is_some() && spec.prewarm_derive.is_some()),
                "{}: declares both prewarm and prewarm_derive",
                spec.name
            );
        }

        // Make sorts last on purpose: a Makefile is a reliable file and a weak
        // signal, so a project wrapping cargo or cmake behind one must be
        // detected as what it really builds with.
        assert_eq!(names.last(), Some(&"Make"), "{names:?}");
        let make = specs.iter().find(|s| s.name == "Make").unwrap();
        let cmake = specs.iter().find(|s| s.name == "CMake").unwrap();
        assert!(cmake.priority < make.priority);
    }

    /// A wrapper Makefile beside a real manifest must not win.
    #[test]
    fn the_more_specific_manifest_wins_over_a_wrapper_makefile() {
        let specs = load_specs(&skills_dir()).unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Makefile"), "all:\n\tcargo build\n").unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();

        assert_eq!(
            detect(&specs, root.path()).map(|s| s.name.as_str()),
            Some("Cargo")
        );

        // With only the Makefile, Make is the answer rather than nothing.
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("Makefile"), "all:\n").unwrap();
        assert_eq!(
            detect(&specs, plain.path()).map(|s| s.name.as_str()),
            Some("Make")
        );

        // Nothing recognisable stays None.
        let bare = tempfile::tempdir().unwrap();
        assert!(detect(&specs, bare.path()).is_none());
    }

    /// Lanes bound the builds that cost minutes and gigabytes. A per-file
    /// compile must not queue behind one.
    #[test]
    fn only_real_builds_take_a_lane() {
        let specs = load_specs(&skills_dir()).unwrap();

        for heavy in [
            "./gradlew :core:test",
            "mvn -pl core -am test",
            "cmake --build /tmp/b --target x",
            "cd /w && make -j4 all",
            "ctest --test-dir build -R x",
        ] {
            assert!(is_lane_command(&specs, heavy), "{heavy} should take a lane");
        }
        for cheap in [
            "javac -cp cp.txt T.java",
            "java -cp out Probe",
            "kotlinc -cp cp.txt T.kt",
            "python -c 'import x'",
        ] {
            assert!(
                !is_lane_command(&specs, cheap),
                "{cheap} must not take a lane"
            );
        }

        // A wrapper in front of the build must not hide it: this is the
        // expensive mistake, and agents write exactly this.
        assert!(is_lane_command(
            &specs,
            "cd $WORKSPACE && timeout 600 ./gradlew :kxs:jvmTest"
        ));
        assert!(is_lane_command(&specs, "nice -n 10 mvn -pl core test"));

        // And the cheap one, accepted deliberately: a line merely mentioning a
        // build tool takes a permit it does not need. One wasted permit beats
        // one unbounded build.
        assert!(is_lane_command(&specs, "grep -rn make README.md"));
    }

    /// Every lane-taking system must say where its reports land, or a lane runs
    /// a build and copies nothing back -- indistinguishable from a test that
    /// never ran. Make is exempt: it has no report convention of its own.
    #[test]
    fn lane_taking_systems_declare_where_their_reports_land() {
        let specs = load_specs(&skills_dir()).unwrap();
        for spec in specs.iter().filter(|s| !s.lane_commands.is_empty()) {
            if spec.name == "Make" {
                continue;
            }
            assert!(
                !spec.test_reports.is_empty(),
                "{}: no test_reports",
                spec.name
            );
        }
        let patterns = test_report_patterns(&specs);
        assert!(
            patterns.iter().any(|p| p.contains("surefire")),
            "{patterns:?}"
        );
        assert!(
            patterns.iter().any(|p| p.contains("test-results")),
            "{patterns:?}"
        );
    }

    /// The wrapper a project ships must outrank whatever is on PATH: it exists
    /// precisely so the build runs on the version the project pinned.
    #[test]
    fn a_shipped_wrapper_outranks_the_ambient_tool() {
        let specs = load_specs(&skills_dir()).unwrap();
        let gradle = specs.iter().find(|s| s.name == "Gradle").unwrap();
        let maven = specs.iter().find(|s| s.name == "Maven").unwrap();
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve_program(gradle, root.path()).as_deref(),
            Some("gradle")
        );
        assert_eq!(resolve_program(maven, root.path()).as_deref(), Some("mvn"));

        std::fs::write(root.path().join("gradlew"), "").unwrap();
        std::fs::write(root.path().join("mvnw"), "").unwrap();
        assert_eq!(
            resolve_program(gradle, root.path()).as_deref(),
            Some("./gradlew")
        );
        assert_eq!(
            resolve_program(maven, root.path()).as_deref(),
            Some("./mvnw")
        );
    }

    /// The declared candidates must reproduce the selection that used to be
    /// hardcoded, because getting this wrong warms the wrong module and reports
    /// success while compiling nothing the agents use.
    #[test]
    fn gradle_candidates_pick_what_the_hardcoded_order_picked() {
        let specs = load_specs(&skills_dir()).unwrap();
        let gradle = specs.iter().find(|s| s.name == "Gradle").unwrap();
        let derive = gradle
            .prewarm_derive
            .as_ref()
            .expect("Gradle derives its pre-warm");
        let tasks = |available: &[&str]| {
            let owned: Vec<String> = available.iter().map(|s| (*s).to_string()).collect();
            choose_candidate(derive, &owned).map(|c| c.tasks.join(" "))
        };

        // Multiplatform outranks classes/testClasses even when both exist --
        // those usually belong to some other JVM-only module.
        assert_eq!(
            tasks(&[
                "compileKotlinJvm",
                "compileTestKotlinJvm",
                "classes",
                "testClasses",
                "assemble"
            ])
            .as_deref(),
            Some("compileKotlinJvm compileTestKotlinJvm")
        );
        assert_eq!(
            tasks(&["classes", "testClasses", "assemble"]).as_deref(),
            Some("classes testClasses")
        );
        assert_eq!(
            tasks(&["compileJava", "compileTestJava"]).as_deref(),
            Some("compileJava compileTestJava")
        );
        assert_eq!(tasks(&["assemble"]).as_deref(), Some("assemble"));

        // assemble is never chosen for a multiplatform project: it drags in
        // every declared target and burns an hour on artifacts nobody runs.
        assert_eq!(tasks(&["assemble", "jvmTest"]), None);
        // Nothing recognisable means no pre-warm, rather than a guessed command.
        assert_eq!(tasks(&["build", "check"]), None);
    }

    /// Templates are what let the same declaration serve a wrapper and a bare
    /// tool, so substitution has to be exact.
    #[test]
    fn templates_substitute_program_and_tasks() {
        assert_eq!(
            fill(
                "{program} --max-workers=1 {tasks} --build-cache",
                "./gradlew",
                "classes testClasses"
            ),
            "./gradlew --max-workers=1 classes testClasses --build-cache"
        );
        // An unused placeholder collapses rather than leaving a double space
        // that would split into an empty argument.
        assert_eq!(
            fill("{program} -q -B test-compile", "mvn", ""),
            "mvn -q -B test-compile"
        );
    }

    /// A malformed declaration must stop the run naming the file, not be read
    /// as a skill that declares nothing -- that is the silent failure this
    /// whole mechanism trades a compile error for.
    #[test]
    fn a_broken_declaration_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "bad",
            "build_system:\n  name: X\n  priority: not-a-number\n",
        );
        let error = load_specs(dir.path()).unwrap_err().to_string();
        assert!(error.contains("does not parse"), "{error}");
        assert!(error.contains("bad"), "must name the file: {error}");

        let no_files = tempfile::tempdir().unwrap();
        write_skill(
            no_files.path(),
            "empty",
            "build_system:\n  name: X\n  priority: 1\n",
        );
        let error = load_specs(no_files.path()).unwrap_err().to_string();
        assert!(error.contains("never be detected"), "{error}");

        // A tie makes detection order arbitrary, so it is refused.
        let tied = tempfile::tempdir().unwrap();
        write_skill(
            tied.path(),
            "a",
            "build_system:\n  name: A\n  priority: 5\n  any_of: [a.txt]\n",
        );
        write_skill(
            tied.path(),
            "b",
            "build_system:\n  name: B\n  priority: 5\n  any_of: [b.txt]\n",
        );
        let error = load_specs(tied.path()).unwrap_err().to_string();
        assert!(error.contains("must not be a tie"), "{error}");
    }

    /// Skills that are not build systems are simply not detection rules.
    #[test]
    fn skills_without_a_declaration_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "trailmark", "");
        assert!(load_specs(dir.path()).unwrap().is_empty());
        // And a directory with no SKILL.md at all is not an error.
        std::fs::create_dir_all(dir.path().join("stray")).unwrap();
        assert!(load_specs(dir.path()).unwrap().is_empty());
    }
}
