use std::collections::HashMap;
use std::sync::Arc;
use tree_sitter_tags::TagsConfiguration;

pub struct LanguageEntry {
    pub config: TagsConfiguration,
    pub name: String,
    /// Whether to filter ALL_CAPS names as macro invocations (C/C++ only).
    pub filter_macros: bool,
}

pub struct Registry {
    by_extension: HashMap<String, Arc<LanguageEntry>>,
}

macro_rules! register_languages {
    ($map:ident, $( $name:expr, [$($ext:expr),+], $lang:expr, $($query:expr),+; )*) => {
        $(
            {
                let combined = concat_queries(&[$($query),+]);
                if let Ok(config) = TagsConfiguration::new($lang.into(), &combined, "") {
                    let filter_macros = $name == "C/C++";
                    let entry = Arc::new(LanguageEntry { config, name: $name.to_string(), filter_macros });
                    $( $map.insert($ext.to_string(), Arc::clone(&entry)); )+
                }
            }
        )*
    };
}

fn concat_queries(queries: &[&str]) -> String {
    queries.join("\n")
}

/// Additional tags query for Java to capture constructor declarations.
/// The upstream query only captures `method_declaration`, not `constructor_declaration`.
const JAVA_CONSTRUCTOR_QUERY: &str = r#"
(constructor_declaration
  name: (identifier) @name) @definition.method
"#;

/// Additional tags query for JS/TS to capture constant declarations.
/// The upstream query only captures `lexical_declaration` when the value is
/// `arrow_function` or `function_expression` (as @definition.function).
/// This adds capture for non-function constant values (strings, numbers, etc.).
const JS_CONSTANT_QUERY: &str = r#"
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(number) (string) (template_string) (true) (false) (null) (undefined)
            (identifier) (array) (object) (new_expression) (binary_expression)
            (unary_expression) (call_expression) (member_expression)
            (ternary_expression)]) @definition.constant)

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(number) (string) (template_string) (true) (false) (null) (undefined)
            (identifier) (array) (object) (new_expression) (binary_expression)
            (unary_expression) (call_expression) (member_expression)
            (ternary_expression)]) @definition.constant)
"#;

/// Tags query for Kotlin. The `brokk-tree-sitter-kotlin` crate ships
/// `queries/tags.scm` in its package but its Rust bindings only re-export
/// `HIGHLIGHTS_QUERY`, so the query is inlined here (MIT, BrokkAi/tree-sitter-kotlin).
/// Interfaces are captured by the `class_declaration` rule and land as classes.
/// Properties are anchored to `source_file`/`class_body` so that locals declared
/// inside function bodies do not pollute outlines. Secondary constructors have no
/// identifier, so the `constructor` keyword is captured as the name and scoping
/// makes them `<Class>::constructor`; `val`/`var` primary-constructor parameters are
/// captured as properties.
const KOTLIN_TAGS_QUERY: &str = r#"
(class_declaration
  (type_identifier) @name) @definition.class

(object_declaration
  (type_identifier) @name) @definition.class

(companion_object
  (type_identifier) @name) @definition.class

(function_declaration
  (simple_identifier) @name) @definition.function

(source_file
  (property_declaration
    (variable_declaration
      (simple_identifier) @name)) @definition.constant)

(class_body
  (property_declaration
    (variable_declaration
      (simple_identifier) @name)) @definition.constant)

(secondary_constructor
  "constructor" @name) @definition.method

(primary_constructor
  (class_parameter
    (binding_pattern_kind)
    (simple_identifier) @name) @definition.constant)

(enum_entry
  (simple_identifier) @name) @definition.constant

(type_alias
  (type_identifier) @name) @definition.type

(call_expression
  (simple_identifier) @name) @reference.call

(call_expression
  (navigation_expression
    (navigation_suffix
      (simple_identifier) @name))) @reference.call

(constructor_invocation
  (user_type
    (type_identifier) @name)) @reference.class
"#;

#[cfg(test)]
pub(crate) fn kotlin_tags_query() -> &'static str {
    KOTLIN_TAGS_QUERY
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let mut by_extension = HashMap::new();

        register_languages!(by_extension,
            "Python",     ["py"],                    tree_sitter_python::LANGUAGE,               tree_sitter_python::TAGS_QUERY;
            "Rust",       ["rs"],                    tree_sitter_rust::LANGUAGE,                 tree_sitter_rust::TAGS_QUERY;
            "JavaScript", ["js", "mjs", "cjs"],      tree_sitter_javascript::LANGUAGE,            tree_sitter_javascript::TAGS_QUERY, JS_CONSTANT_QUERY;
            "TypeScript", ["ts"],                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT, tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, JS_CONSTANT_QUERY;
            "TSX",        ["tsx"],                   tree_sitter_typescript::LANGUAGE_TSX,        tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY, JS_CONSTANT_QUERY;
            "C/C++",      ["c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"], tree_sitter_cpp::LANGUAGE, tree_sitter_cpp::TAGS_QUERY;
            "Go",         ["go"],                    tree_sitter_go::LANGUAGE,                   tree_sitter_go::TAGS_QUERY;
            "Java",       ["java"],                  tree_sitter_java::LANGUAGE,                 tree_sitter_java::TAGS_QUERY, JAVA_CONSTRUCTOR_QUERY;
            "Ruby",       ["rb", "gradle"],          tree_sitter_ruby::LANGUAGE,                 tree_sitter_ruby::TAGS_QUERY;
            "Kotlin",     ["kt", "kts"],             brokk_tree_sitter_kotlin::LANGUAGE,          KOTLIN_TAGS_QUERY;
        );

        Self { by_extension }
    }

    pub fn get(&self, extension: &str) -> Option<&Arc<LanguageEntry>> {
        self.by_extension.get(extension)
    }

    #[allow(dead_code)]
    pub fn supported_extensions(&self) -> Vec<&str> {
        self.by_extension.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_lookup() {
        let registry = Registry::new();
        assert!(registry.get("py").is_some());
        assert!(registry.get("rs").is_some());
        assert!(registry.get("js").is_some());
        assert!(registry.get("ts").is_some());
        assert!(registry.get("tsx").is_some());
        assert!(registry.get("kt").is_some());
        assert!(registry.get("kts").is_some());
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn test_registry_language_names() {
        let registry = Registry::new();
        assert_eq!(registry.get("py").unwrap().name, "Python");
        assert_eq!(registry.get("rs").unwrap().name, "Rust");
        assert_eq!(registry.get("js").unwrap().name, "JavaScript");
        assert_eq!(registry.get("kt").unwrap().name, "Kotlin");
    }
}
