//! Cross-crate check: the schemas the tool registry emits must be enforceable
//! by the providers the API layer talks to. A tool that drops out of strict
//! mode still works, but loses argument validation — so a regression here is
//! worth failing a build over.

use flawrence_api::strict::strict_schema_supported;
use flawrence_tool::registry;

/// `submit_result` ships a placeholder schema; the real one is injected per
/// agent from its `output_schema`, so it cannot be judged here.
const NOT_STRICT_BY_DESIGN: &[&str] = &["submit_result"];

#[test]
fn every_tool_schema_can_be_strictly_enforced() {
    let registry = registry();
    let mut ineligible = Vec::new();

    for name in registry.tool_names() {
        if NOT_STRICT_BY_DESIGN.contains(&name.as_str()) {
            continue;
        }
        let tools = registry.tools_filtered(std::slice::from_ref(&name));
        let tool = &tools[0];
        assert_eq!(
            tool.strict,
            Some(true),
            "{name} does not request strict mode"
        );
        let schema = tool.schema.as_ref().expect("tool has a schema");
        if !strict_schema_supported(schema) {
            ineligible.push(format!("{name}: {schema}"));
        }
    }

    assert!(
        ineligible.is_empty(),
        "tool schemas outside the strict subset:\n{}",
        ineligible.join("\n")
    );
}

#[test]
fn tool_arguments_named_like_schema_keywords_stay_strict() {
    // `glob_files` and `grep_files` take an argument called `pattern`, which is
    // also a JSON Schema keyword the strict subset excludes.
    for name in ["glob_files", "grep_files"] {
        let tools = registry().tools_filtered(&[name.to_string()]);
        let schema = tools[0].schema.as_ref().unwrap();
        assert!(
            schema["properties"].get("pattern").is_some(),
            "{name} no longer has a `pattern` argument; pick another fixture"
        );
        assert!(
            strict_schema_supported(schema),
            "{name} was misread as using the `pattern` keyword"
        );
    }
}
