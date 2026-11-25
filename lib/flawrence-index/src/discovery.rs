use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;

use super::registry::{LanguageEntry, Registry};

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "build",
    "dist",
    "__pycache__",
    ".venv",
    "obj",
    ".git",
];

const MAX_CODE_FILE_SIZE: u64 = 1_000_000; // 1MB

pub struct DiscoveredFile {
    pub path: PathBuf,
    pub language: Arc<LanguageEntry>,
}

/// Walk a directory tree, yielding file entries that pass the shared skip
/// filters (hidden files, skip-dirs, binary detection). The caller decides
/// which extensions to keep and which size cap to apply.
fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    let walker = WalkBuilder::new(root)
        .hidden(true) // skip hidden files/dirs
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                return !SKIP_DIRS.contains(&name.as_ref());
            }
            true
        })
        .build();

    walker
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| !is_binary(path))
}

/// Discover code files: registry-matched extensions only, 1MB cap.
pub fn discover_code(root: &Path, registry: &Registry) -> Vec<DiscoveredFile> {
    let mut code_files = Vec::new();
    for path in walk_files(root) {
        // Skip files over the code size cap
        if let Ok(metadata) = path.metadata()
            && metadata.len() > MAX_CODE_FILE_SIZE
        {
            continue;
        }

        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
        else {
            continue;
        };

        let Some(language) = registry.get(&ext) else {
            continue;
        };

        code_files.push(DiscoveredFile {
            path,
            language: Arc::clone(language),
        });
    }
    code_files
}

fn is_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return true;
    };
    let mut buf = [0u8; 512];
    let Ok(n) = file.read(&mut buf) else {
        return true;
    };
    buf[..n].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skip_dirs_contains_expected() {
        assert!(SKIP_DIRS.contains(&"node_modules"));
        assert!(SKIP_DIRS.contains(&"target"));
        assert!(SKIP_DIRS.contains(&"__pycache__"));
    }
}
