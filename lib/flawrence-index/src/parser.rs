use std::path::Path;
use tree_sitter::Parser;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use super::types::{Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Node-kind lists (single source of truth)
// ---------------------------------------------------------------------------

/// Container node kinds that define a scope: classes, structs, enums,
/// interfaces, traits, impl blocks, namespaces, modules.
/// Used by scope resolution AND body extraction.
const CONTAINER_KINDS: &[&str] = &[
    // Classes / structs
    "class_definition",   // Python
    "class_declaration",  // JS/TS, Java, C#, Kotlin, Swift
    "class",              // JS/TS alternate, Ruby
    "class_specifier",    // C++
    "struct_item",        // Rust
    "struct_specifier",   // C++
    "struct_declaration", // Swift
    // Enums
    "enum_item",        // Rust
    "enum_declaration", // Java
    // Interfaces / traits / protocols
    "interface_declaration", // Java, C#
    "trait_item",            // Rust
    "protocol_declaration",  // Swift
    // Impl blocks / namespaces / modules
    "impl_item",             // Rust
    "namespace_definition",  // C++
    "namespace_declaration", // C#
    "module",                // Ruby
    "mod_item",              // Rust
    "object_declaration",    // Kotlin
    // Go
    "type_declaration",
];

/// Module-like scope nodes where "methods" are really just namespaced functions.
/// The Rust tags query tags any function_item inside a declaration_list as
/// definition.method, but declaration_list is shared by impl, trait, AND mod.
const MODULE_SCOPE_KINDS: &[&str] = &[
    "module",   // Ruby
    "mod_item", // Rust
];

/// Leaf definition node kinds: functions, methods, and language-specific
/// declaration forms that don't contain nested definitions.
const LEAF_DEFINITION_KINDS: &[&str] = &[
    "function_definition",
    "function_item",
    "function_declaration",
    "method_definition",
    "method_declaration",
    "arrow_function",
    "lambda",
    // JS/TS exports
    "export_statement",
    // C/C++ specific
    "declaration",
    "field_declaration",
    "template_declaration",
];

fn is_definition_node(kind: &str) -> bool {
    CONTAINER_KINDS.contains(&kind) || LEAF_DEFINITION_KINDS.contains(&kind)
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

pub fn extract_symbols(
    file: &Path,
    source: &[u8],
    config: &TagsConfiguration,
    filter_macros: bool,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    // Bail early on non-UTF-8 files before doing any parsing work
    if std::str::from_utf8(source).is_err() {
        return symbols;
    }

    let mut context = TagsContext::new();

    // Generate tags
    let Ok((tags_iter, _has_error)) = context.generate_tags(config, source, None) else {
        return symbols;
    };

    // Also parse with tree-sitter for scope resolution (cache the tree)
    let tree = {
        let mut parser = Parser::new();
        if parser.set_language(&config.language).is_err() {
            None
        } else {
            parser.parse(source, None)
        }
    };

    for tag_result in tags_iter {
        let Ok(tag) = tag_result else { continue };

        // Extract the symbol name from the source using the name range
        let name = match source.get(tag.name_range.clone()) {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            },
            None => continue,
        };

        // Skip C/C++ macro invocations: ALL_CAPS_WITH_UNDERSCORES names (2+ chars)
        // are macros by convention, and tree-sitter misidentifies them as function calls.
        // Only apply this heuristic to C/C++ files — ALL_CAPS names are legitimate
        // constants in Python, JS/TS, Java, Go, Ruby, etc.
        if filter_macros && is_macro_name(&name) {
            continue;
        }

        // Determine the kind
        let syntax_type = config.syntax_type_name(tag.syntax_type_id);
        let kind = match SymbolKind::from_tag(syntax_type, tag.is_definition) {
            Some(k) => k,
            None => continue,
        };

        // Line numbers (1-indexed)
        let line_start = tag.span.start.row + 1;
        let line_end = tag.span.end.row + 1;

        // Find scope via a single upward walk through the AST
        let scope_result = tree
            .as_ref()
            .and_then(|t| resolve_scope(t, source, tag.name_range.start));

        // Reclassify: the Rust tags query marks all functions inside a
        // declaration_list as "method", but declaration_list is shared by
        // impl, trait, AND mod blocks. Functions in modules aren't methods.
        let kind = match (&kind, &scope_result) {
            (SymbolKind::Method, Some(sr)) if sr.is_module => SymbolKind::Function,
            _ => kind,
        };

        let scope = scope_result.map(|sr| sr.name);

        // Build qualified name
        let qualified_name = match &scope {
            Some(s) => format!("{}::{}", s, name),
            None => name.clone(),
        };

        // Extract signature: use the AST to find where the body starts,
        // then take only the declaration head (everything before the body).
        let sig_end = tree
            .as_ref()
            .and_then(|t| find_body_start(t, tag.range.start))
            .filter(|&pos| pos > tag.range.start && pos <= tag.range.end)
            .unwrap_or(tag.range.end);
        let signature = source
            .get(tag.range.start..sig_end)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(|s| collapse_whitespace(s, 200))
            .unwrap_or_default();

        symbols.push(Symbol::new_code(
            name,
            qualified_name,
            scope,
            kind,
            file.to_path_buf(),
            line_start,
            line_end,
            signature,
        ));
    }

    symbols
}

// ---------------------------------------------------------------------------
// Scope resolution (single walk up the AST)
// ---------------------------------------------------------------------------

struct ScopeResult {
    name: String,
    /// True if the scope is a module/namespace (not a type/impl).
    /// Used to reclassify "methods" inside modules as functions.
    is_module: bool,
}

/// Walk up from a byte offset to resolve the enclosing scope.
/// Checks for inline qualifiers first (C++ `::`, Go receivers), then
/// falls back to enclosing container nodes (class, impl, namespace, etc.).
fn resolve_scope(
    tree: &tree_sitter::Tree,
    source: &[u8],
    byte_offset: usize,
) -> Option<ScopeResult> {
    let root = tree.root_node();
    let mut node = root.descendant_for_byte_range(byte_offset, byte_offset)?;

    loop {
        match node.kind() {
            // C++: `qualified_identifier` wraps `namespace::name`
            "qualified_identifier" => {
                let text = node_text(&node, source)?;
                let parts: Vec<&str> = text.rsplitn(2, "::").collect();
                if parts.len() == 2 {
                    return Some(ScopeResult {
                        name: parts[1].to_string(),
                        is_module: false,
                    });
                }
            }
            // Go: method_declaration has a `receiver` field with the type
            "method_declaration" => {
                if let Some(scope) = extract_go_receiver(&node, source) {
                    return Some(ScopeResult {
                        name: scope,
                        is_module: false,
                    });
                }
            }
            // Container nodes provide enclosing scope
            kind if CONTAINER_KINDS.contains(&kind) => {
                // Skip if the scope node's name occupies the same byte position as
                // our symbol — that means we ARE this node, not inside it.
                // This allows constructors (Foo inside class Foo) to get the correct
                // scope while preventing a class from scoping itself.
                if let Some((scope_name, name_start)) = extract_scope_name(&node, source)
                    && name_start != byte_offset
                {
                    return Some(ScopeResult {
                        name: scope_name,
                        is_module: MODULE_SCOPE_KINDS.contains(&kind),
                    });
                }
            }
            _ => {}
        }
        node = node.parent()?;
    }
}

/// Extract the name and its byte offset from a scope-defining node.
/// Returns (name_string, name_start_byte) so callers can distinguish
/// self-scoping (same byte position) from enclosing scope (different position).
fn extract_scope_name(node: &tree_sitter::Node, source: &[u8]) -> Option<(String, usize)> {
    // Try common field names: "name" covers most languages,
    // "type" covers Rust impl_item (which has no "name" field).
    for field in &["name", "type"] {
        if let Some(child) = node.child_by_field_name(field) {
            return node_text(&child, source).map(|s| (s, child.start_byte()));
        }
    }

    // Fallback: first identifier-like child
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|c| matches!(c.kind(), "identifier" | "type_identifier" | "constant"))
        .and_then(|c| node_text(&c, source).map(|s| (s, c.start_byte())))
}

/// Extract the receiver type from a Go method_declaration node.
/// Structure: receiver (parameter_list) → parameter_declaration → type
fn extract_go_receiver(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let param = receiver
        .named_children(&mut receiver.walk())
        .find(|c| c.kind() == "parameter_declaration")?;
    let type_node = param.child_by_field_name("type")?;
    // Unwrap pointer receiver (*MyType → MyType)
    let base = if type_node.kind() == "pointer_type" {
        type_node
            .named_children(&mut type_node.walk())
            .find(|c| c.kind() == "type_identifier")?
    } else {
        type_node
    };
    node_text(&base, source)
}

// ---------------------------------------------------------------------------
// Body & signature extraction
// ---------------------------------------------------------------------------

/// Extract the full source text of the AST node at the given line.
/// Parses the source with the appropriate language grammar, finds the named
/// node covering the target line, and returns its complete source range.
pub fn extract_body(
    source: &[u8],
    line: usize, // 1-indexed
    config: &TagsConfiguration,
) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&config.language).ok()?;
    let tree = parser.parse(source, None)?;

    // Convert 1-indexed line to byte offset, skipping leading whitespace.
    // This is critical for indented definitions (e.g. methods inside classes):
    // the whitespace bytes belong to the parent node, not the definition node,
    // so we must land on the first non-whitespace byte to find the right AST node.
    let line_start = line_to_byte_offset(source, line)?;
    let target_byte = skip_whitespace(source, line_start);

    let root = tree.root_node();
    let leaf = root.descendant_for_byte_range(target_byte, target_byte)?;

    // Walk up from the leaf to find the smallest meaningful definition node
    let mut node = leaf;
    loop {
        if is_definition_node(node.kind()) {
            break;
        }
        node = node.parent()?;
    }

    let text = source.get(node.byte_range())?;
    std::str::from_utf8(text).ok().map(|s| s.to_string())
}

/// Find the byte offset where the body of a definition starts, using the AST.
/// Looks for a child named "body" on the smallest named node covering `byte_offset`.
/// This works across languages because tree-sitter grammars consistently use "body"
/// as the field name for blocks (Rust, Python, JS/TS, C/C++, Go, Java, Ruby, etc.).
fn find_body_start(tree: &tree_sitter::Tree, byte_offset: usize) -> Option<usize> {
    let root = tree.root_node();
    let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;

    // Walk up to find the definition node that owns a "body" field.
    let mut current = node;
    loop {
        if let Some(body) = current.child_by_field_name("body") {
            let body_start = body.start_byte();
            if body_start >= current.start_byte() && body_start < current.end_byte() {
                return Some(body_start);
            }
        }
        current = current.parent()?;
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Returns true if the name looks like a C/C++ macro: all uppercase letters,
/// digits, and underscores (e.g. KJ_IF_SOME, JSG_RESOURCE_TYPE).
fn is_macro_name(name: &str) -> bool {
    name.len() >= 2
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn node_text(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let text = source.get(node.byte_range())?;
    std::str::from_utf8(text).ok().map(|s| s.to_string())
}

/// Advance past ASCII whitespace (spaces and tabs) from a given offset.
fn skip_whitespace(source: &[u8], offset: usize) -> usize {
    let mut i = offset;
    while i < source.len() && (source[i] == b' ' || source[i] == b'\t') {
        i += 1;
    }
    i
}

fn line_to_byte_offset(source: &[u8], line: usize) -> Option<usize> {
    if line <= 1 {
        return Some(0);
    }
    let mut current_line = 1usize;
    for (i, &byte) in source.iter().enumerate() {
        if byte == b'\n' {
            current_line += 1;
            if current_line == line {
                return Some(i + 1);
            }
        }
    }
    None
}

/// Collapse runs of whitespace (including newlines) into single spaces and truncate.
/// When truncating, breaks at the last clean boundary (comma or closing bracket)
/// to avoid cutting mid-type in heavily-templated signatures.
fn collapse_whitespace(s: &str, max_len: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max_len {
        collapsed
    } else {
        // Find char boundary at or before max_len
        let mut end = max_len;
        while !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        // Try to break at the last comma or closing bracket for cleaner output
        if let Some(pos) = collapsed[..end].rfind([',', '>', ')'])
            && pos > end / 2
        {
            end = pos + 1;
        }
        let mut truncated = collapsed[..end].to_string();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_kotlin_symbols() {
        let source = br#"
package at.asitplus.demo

object Der {
    fun decode(bytes: ByteArray): Element = DerDecoder(bytes).read()
}

class DerDecoder(private val input: ByteArray) {
    fun read(): Element = Element(input[0])
}

typealias Tag = UByte
"#;
        let config = TagsConfiguration::new(
            brokk_tree_sitter_kotlin::LANGUAGE.into(),
            crate::registry::kotlin_tags_query(),
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("Der.kt"), source, &config, false);

        let decoder = symbols.iter().find(|s| s.name == "DerDecoder");
        assert!(decoder.is_some(), "Should find DerDecoder class");

        let read = symbols
            .iter()
            .find(|s| s.name == "read")
            .expect("Should find read()");
        assert_eq!(read.scope, Some("DerDecoder".to_string()));
        assert_eq!(read.qualified_name, "DerDecoder::read");

        assert!(
            symbols.iter().any(|s| s.name == "Tag"),
            "Should find the typealias"
        );
    }

    #[test]
    fn dotted_qualified_names_resolve_like_double_colon() {
        // Models write `DerDecoder.decodeCurrentElementWith`; the index keys on
        // `DerDecoder::decodeCurrentElementWith`.
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Decoder.kt"),
            "class DerDecoder {\n    fun decodeCurrentElementWith() {}\n}\n",
        )
        .unwrap();
        let index = crate::CodebaseIndex::build(directory.path());

        let dotted = index.find_symbol("DerDecoder.decodeCurrentElementWith");
        assert_eq!(dotted.len(), 1, "dotted lookup found nothing");
        assert_eq!(
            dotted[0].qualified_name,
            "DerDecoder::decodeCurrentElementWith"
        );

        let colons = index.find_symbol("DerDecoder::decodeCurrentElementWith");
        assert_eq!(colons.len(), 1);

        // And `source` accepts the same spelling `find` just resolved.
        let body = index
            .get_body(
                "DerDecoder.decodeCurrentElementWith",
                std::path::Path::new("Decoder.kt"),
            )
            .expect("body for the dotted name");
        assert!(body.body.contains("decodeCurrentElementWith"));
    }

    #[test]
    fn test_extract_kotlin_constructors_and_invoke() {
        let source = br#"
class Element(val tag: UByte) {
    constructor(tag: Int) : this(tag.toUByte())

    companion object {
        operator fun invoke(hex: String): Element = Element(0x30u)
    }
}
"#;
        let config = TagsConfiguration::new(
            brokk_tree_sitter_kotlin::LANGUAGE.into(),
            crate::registry::kotlin_tags_query(),
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("Element.kt"), source, &config, false);

        // Secondary constructors have no identifier; the keyword carries the scope.
        assert!(
            symbols
                .iter()
                .any(|s| s.qualified_name == "Element::constructor"),
            "Should find the secondary constructor"
        );
        // `operator fun invoke` on a companion is the idiomatic faux-constructor.
        assert!(
            symbols
                .iter()
                .any(|s| s.qualified_name == "Element::invoke"),
            "Should find the companion invoke"
        );
        // `val`/`var` primary-constructor parameters are properties.
        assert!(
            symbols.iter().any(|s| s.qualified_name == "Element::tag"),
            "Should find the primary constructor property"
        );
    }

    #[test]
    fn test_extract_python_symbols() {
        let source = br#"
class MyClass:
    def my_method(self):
        pass

def top_level():
    pass
"#;
        let config = TagsConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.py"), source, &config, false);

        assert!(!symbols.is_empty(), "Should find at least one definition");

        let class_sym = symbols.iter().find(|s| s.name == "MyClass");
        assert!(class_sym.is_some(), "Should find MyClass");

        let method_sym = symbols.iter().find(|s| s.name == "my_method");
        assert!(method_sym.is_some(), "Should find my_method");
        if let Some(m) = method_sym {
            assert_eq!(m.scope, Some("MyClass".to_string()));
            assert_eq!(m.qualified_name, "MyClass::my_method");
        }

        let top_fn = symbols.iter().find(|s| s.name == "top_level");
        assert!(top_fn.is_some(), "Should find top_level");
        if let Some(f) = top_fn {
            assert!(f.scope.is_none());
        }
    }

    #[test]
    fn test_extract_rust_symbols() {
        let source = br#"
struct Foo;

impl Foo {
    fn bar(&self) {}
}

fn standalone() {}
"#;
        let config = TagsConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.rs"), source, &config, false);
        assert!(!symbols.is_empty());
    }

    // -----------------------------------------------------------------------
    // Regression-locking tests (must pass before AND after changes)
    // -----------------------------------------------------------------------

    #[test]
    fn test_rust_impl_method_has_scope() {
        let source = br#"
struct Foo;

impl Foo {
    fn bar(&self) {}
    fn baz() {}
}

fn standalone() {}
"#;
        let config = TagsConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.rs"), source, &config, false);

        let bar = symbols.iter().find(|s| s.name == "bar");
        assert!(bar.is_some(), "Should find bar");
        assert_eq!(bar.unwrap().scope, Some("Foo".to_string()));
        assert_eq!(bar.unwrap().qualified_name, "Foo::bar");

        let baz = symbols.iter().find(|s| s.name == "baz");
        assert!(baz.is_some(), "Should find baz");
        assert_eq!(baz.unwrap().scope, Some("Foo".to_string()));

        let standalone = symbols.iter().find(|s| s.name == "standalone");
        assert!(standalone.is_some(), "Should find standalone");
        assert!(standalone.unwrap().scope.is_none());
    }

    #[test]
    fn test_cpp_macro_names_filtered() {
        let source = br#"
void JSG_RESOURCE_TYPE() {}
void KJ_MAP() {}
void normalFunction() {}
"#;
        let config = TagsConfiguration::new(
            tree_sitter_cpp::LANGUAGE.into(),
            tree_sitter_cpp::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.cpp"), source, &config, true);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(
            !names.contains(&"JSG_RESOURCE_TYPE"),
            "C++ macro should be filtered"
        );
        assert!(!names.contains(&"KJ_MAP"), "C++ macro should be filtered");
        assert!(
            names.contains(&"normalFunction"),
            "Normal function should NOT be filtered"
        );
    }

    #[test]
    fn test_signature_does_not_contain_body() {
        let source = br#"
def my_function(x, y):
    result = x + y
    return result
"#;
        let config = TagsConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.py"), source, &config, false);
        let func = symbols.iter().find(|s| s.name == "my_function");
        assert!(func.is_some(), "Should find my_function");
        let sig = &func.unwrap().signature;
        assert!(sig.contains("my_function"), "Signature should contain name");
        assert!(
            !sig.contains("return result"),
            "Signature should not contain body: {}",
            sig
        );
    }

    // -----------------------------------------------------------------------
    // Bug-demonstrating tests (currently #[ignore], remove after fixes)
    // -----------------------------------------------------------------------

    #[test]
    fn test_python_allcaps_constant_not_filtered() {
        let source = br#"
MAX_CONNECTIONS = 100
DEFAULT_TIMEOUT = 30
"#;
        let config = TagsConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("test.py"), source, &config, false);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(
            names.contains(&"MAX_CONNECTIONS"),
            "Python ALL_CAPS constant should NOT be filtered, got: {:?}",
            names
        );
        assert!(
            names.contains(&"DEFAULT_TIMEOUT"),
            "Python ALL_CAPS constant should NOT be filtered, got: {:?}",
            names
        );
    }

    #[test]
    fn test_ts_allcaps_constant_not_filtered() {
        use crate::registry::Registry;

        let registry = Registry::new();
        let lang = registry.get("ts").expect("TS should be registered");

        let source = br#"
export const API_KEY = "secret";
export const MAX_RETRIES = 3;
"#;

        let symbols = extract_symbols(
            Path::new("test.ts"),
            source,
            &lang.config,
            lang.filter_macros,
        );
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(
            names.contains(&"API_KEY"),
            "TS ALL_CAPS constant should NOT be filtered, got: {:?}",
            names
        );
        assert!(
            names.contains(&"MAX_RETRIES"),
            "TS ALL_CAPS constant should NOT be filtered, got: {:?}",
            names
        );
    }

    #[test]
    fn test_extract_body_returns_method_not_class() {
        let source = br#"
class MyClass:
    def target_method(self):
        x = 1
        return x

    def other_method(self):
        pass
"#;
        let config = TagsConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::TAGS_QUERY,
            "",
        )
        .unwrap();

        // target_method is on line 3 (1-indexed)
        let body = extract_body(source, 3, &config);
        assert!(body.is_some(), "Should extract body");
        let text = body.unwrap();

        assert!(
            text.contains("target_method"),
            "Body should contain the method name: {}",
            text
        );
        assert!(
            !text.contains("class MyClass"),
            "Body should be the METHOD, not the entire class. Got:\n{}",
            text
        );
        assert!(
            !text.contains("other_method"),
            "Body should not contain sibling methods. Got:\n{}",
            text
        );
    }

    #[test]
    fn test_java_constructor_has_class_scope() {
        use crate::registry::Registry;

        let registry = Registry::new();
        let lang = registry.get("java").expect("Java should be registered");

        let source = br#"
class Foo {
    Foo() {
        System.out.println("constructor");
    }

    void bar() {}
}
"#;

        let symbols = extract_symbols(
            Path::new("Test.java"),
            source,
            &lang.config,
            lang.filter_macros,
        );

        // The constructor "Foo" inside class "Foo" should have scope "Foo"
        let constructors: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.name == "Foo" && s.kind != SymbolKind::Class)
            .collect();
        assert!(
            !constructors.is_empty(),
            "Should find Foo constructor. All symbols: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind, &s.scope))
                .collect::<Vec<_>>()
        );
        let ctor = constructors[0];
        assert_eq!(
            ctor.scope,
            Some("Foo".to_string()),
            "Constructor should have scope Foo, got: {:?}",
            ctor.scope
        );
        assert_eq!(ctor.qualified_name, "Foo::Foo");
    }

    #[test]
    fn test_go_method_receiver_scope() {
        let source = br#"
package main

type Server struct {}

func (s *Server) Handle() {}

func (s Server) Name() string { return "" }

func TopLevel() {}
"#;
        let config = TagsConfiguration::new(
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::TAGS_QUERY,
            "",
        )
        .unwrap();

        let symbols = extract_symbols(Path::new("main.go"), source, &config, false);

        let handle = symbols.iter().find(|s| s.name == "Handle");
        assert!(handle.is_some(), "Should find Handle");
        assert_eq!(
            handle.unwrap().scope,
            Some("Server".to_string()),
            "Handle should have scope Server (pointer receiver)"
        );

        let name_method = symbols.iter().find(|s| s.name == "Name");
        assert!(name_method.is_some(), "Should find Name");
        assert_eq!(
            name_method.unwrap().scope,
            Some("Server".to_string()),
            "Name should have scope Server (value receiver)"
        );

        let top = symbols.iter().find(|s| s.name == "TopLevel");
        assert!(top.is_some(), "Should find TopLevel");
        assert!(
            top.unwrap().scope.is_none(),
            "TopLevel should have no scope"
        );
    }
}
