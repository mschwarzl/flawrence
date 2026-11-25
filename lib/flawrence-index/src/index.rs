use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;

use super::discovery;
use super::parser;
use super::registry::Registry;
use super::types::{CodebaseSummary, Symbol, SymbolKind, SymbolSource};

pub struct CodebaseIndex {
    by_name: HashMap<String, Vec<Arc<Symbol>>>,
    by_qualified_name: HashMap<String, Vec<Arc<Symbol>>>,
    by_file: HashMap<PathBuf, Vec<Arc<Symbol>>>,
    languages: HashMap<String, usize>,
    registry: Registry,
    root: PathBuf,
}

type SymbolBuckets<K> = HashMap<K, Vec<Arc<Symbol>>>;

/// Merge parsed `(path, symbols)` pairs into the three index maps.
fn merge_symbols(
    parsed: Vec<(PathBuf, Vec<Symbol>)>,
) -> (
    SymbolBuckets<String>,
    SymbolBuckets<String>,
    SymbolBuckets<PathBuf>,
) {
    let mut by_name: HashMap<String, Vec<Arc<Symbol>>> = HashMap::new();
    let mut by_qualified_name: HashMap<String, Vec<Arc<Symbol>>> = HashMap::new();
    let mut by_file: HashMap<PathBuf, Vec<Arc<Symbol>>> = HashMap::new();

    for (path, symbols) in parsed {
        for symbol in symbols {
            let symbol = Arc::new(symbol);
            by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(Arc::clone(&symbol));
            by_qualified_name
                .entry(symbol.qualified_name.clone())
                .or_default()
                .push(Arc::clone(&symbol));
            by_file.entry(path.clone()).or_default().push(symbol);
        }
    }

    (by_name, by_qualified_name, by_file)
}

impl CodebaseIndex {
    /// Build a code index from a root directory.
    pub fn build(root: &Path) -> Self {
        let registry = Registry::new();
        let code_files = discovery::discover_code(root, &registry);

        // Count languages from discovered code files
        let mut languages: HashMap<String, usize> = HashMap::new();
        for f in &code_files {
            *languages.entry(f.language.name.clone()).or_insert(0) += 1;
        }

        // Parallel: read + parse each code file, collecting (path, symbols) pairs
        let parsed_code: Vec<(PathBuf, Vec<Symbol>)> = code_files
            .par_iter()
            .filter_map(|discovered| {
                let source = std::fs::read(&discovered.path).ok()?;
                let symbols = parser::extract_symbols(
                    &discovered.path,
                    &source,
                    &discovered.language.config,
                    discovered.language.filter_macros,
                );
                Some((discovered.path.clone(), symbols))
            })
            .collect();

        let (by_name, by_qualified_name, by_file) = merge_symbols(parsed_code);

        Self {
            by_name,
            by_qualified_name,
            by_file,
            languages,
            registry,
            root: root.to_path_buf(),
        }
    }

    /// Find definitions by name: exact qualified → exact name → dot-normalised
    /// exact → substring on name. Results are not truncated — callers are
    /// responsible for limiting output.
    ///
    /// A `Class.method` query is normalised to `Class::method` first: models
    /// write qualified names in the target language's syntax (Kotlin, Java and
    /// Python all use a dot), while the index keys on `::`. Without this, a
    /// perfectly good `DerDecoder.decodeCurrentElementWith` lookup falls
    /// through to the substring pass and finds nothing.
    pub fn find_symbol(&self, query: &str) -> Vec<&Symbol> {
        let results = self.find_symbol_exact(query);
        if !results.is_empty() {
            return results;
        }
        if query.contains('.') && !query.contains("::") {
            let normalised = query.replace('.', "::");
            let results = self.find_symbol_exact(&normalised);
            if !results.is_empty() {
                return results;
            }
        }
        self.find_symbol_fuzzy(query)
    }

    fn find_symbol_exact(&self, query: &str) -> Vec<&Symbol> {
        // 1. Exact qualified name match
        if let Some(symbols) = self.by_qualified_name.get(query)
            && !symbols.is_empty()
        {
            return symbols.iter().map(|s| s.as_ref()).collect();
        }

        // 2. Exact bare name match
        if let Some(symbols) = self.by_name.get(query)
            && !symbols.is_empty()
        {
            return symbols.iter().map(|s| s.as_ref()).collect();
        }

        Vec::new()
    }

    fn find_symbol_fuzzy(&self, query: &str) -> Vec<&Symbol> {
        // Case-insensitive substring fallback
        let query_lower = query.to_lowercase();
        let mut results: Vec<&Symbol> = self
            .by_name
            .iter()
            .filter(|(name, _)| name.to_lowercase().contains(&query_lower))
            .flat_map(|(_, symbols)| symbols.iter().map(|s| s.as_ref()))
            .collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    /// List all definitions in a file, optionally filtered by kind
    pub fn list_symbols(&self, file: &Path, kind: Option<&SymbolKind>) -> Vec<&Symbol> {
        let file_path = self.resolve_path(file);
        let Some(symbols) = self.by_file.get(&file_path) else {
            return Vec::new();
        };

        symbols
            .iter()
            .map(|s| s.as_ref())
            .filter(|s| kind.is_none_or(|k| &s.kind == k))
            .collect()
    }

    /// High-level overview: file count, language breakdown, definition count
    pub fn summarize(&self) -> CodebaseSummary {
        let definition_count = self.by_file.values().map(|syms| syms.len()).sum();

        CodebaseSummary {
            file_count: self.by_file.len(),
            definition_count,
            languages: self.languages.clone(),
        }
    }

    /// Get the full body/source of a symbol given its name and file.
    /// Re-parses the file with tree-sitter-tags.
    pub fn get_body(&self, name: &str, file: &Path) -> Option<SymbolSource> {
        let file_path = self.resolve_path(file);

        // Find the symbol in the index
        let symbols = self.by_file.get(&file_path)?;
        // Accept `Class.method` as well as `Class::method`, matching
        // `find_symbol`: a `find` that resolved a dotted name must not be
        // followed by a `source` call that rejects it.
        let normalised = name.replace('.', "::");
        let symbol = symbols.iter().find(|s| {
            s.qualified_name == name || s.name == name || s.qualified_name == normalised
        })?;

        let ext = file_path.extension()?.to_str()?.to_lowercase();

        let source = std::fs::read(&file_path).ok()?;
        let lang = self.registry.get(&ext)?;
        let body = parser::extract_body(&source, symbol.line_start, &lang.config)?;
        let language = lang.name.clone();

        Some(SymbolSource {
            qualified_name: symbol.qualified_name.clone(),
            file: file_path,
            line_start: symbol.line_start,
            line_end: symbol.line_end,
            language,
            body,
        })
    }

    /// Indexed files whose file name matches `name`, relative to the root.
    ///
    /// Callers pass paths copied out of prose — abbreviated with `...`, or with
    /// a guessed package directory. The file name is usually the one part they
    /// got right, so it is the cheapest way to turn a dead end into a
    /// suggestion.
    pub fn files_named(&self, name: &str) -> Vec<PathBuf> {
        let mut matches: Vec<PathBuf> = self
            .by_file
            .keys()
            .filter(|path| path.file_name().and_then(|n| n.to_str()) == Some(name))
            .map(|path| path.strip_prefix(&self.root).unwrap_or(path).to_path_buf())
            .collect();
        matches.sort();
        matches
    }

    /// Check whether a file path exists in the index.
    /// Accepts both absolute and relative (to root) paths.
    pub fn has_file(&self, file: &Path) -> bool {
        let file_path = self.resolve_path(file);
        self.by_file.contains_key(&file_path)
    }

    /// The root directory this index was built from
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a potentially relative path against the index root
    fn resolve_path(&self, file: &Path) -> PathBuf {
        if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.root.join(file)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `find_symbol` is exact-first and only substrings on bare names.
    #[test]
    fn test_code_index_is_exact_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // A code file
        std::fs::write(
            root.join("main.rs"),
            "pub struct ReadableStream;\nimpl ReadableStream { pub fn cancel(&self) {} }\n",
        )
        .unwrap();
        let index = CodebaseIndex::build(root);

        // Exact-first: searching the bare method name returns it; searching the
        // interface name does NOT return the method (qualified-name substring is
        // not part of the code path).
        let cancel = index.find_symbol("cancel");
        assert!(cancel.iter().any(|s| s.name == "cancel"));

        let rs = index.find_symbol("ReadableStream");
        // The struct matches; the method `cancel` (qualified `ReadableStream::cancel`)
        // must NOT appear because the code path only substrings on bare name.
        assert!(rs.iter().any(|s| s.name == "ReadableStream"));
        assert!(!rs.iter().any(|s| s.name == "cancel"));
    }
}
