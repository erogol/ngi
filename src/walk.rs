//! Shared file walker configuration.
//!
//! Provides a consistent `build_walker` that applies default ignore patterns
//! for common non-source directories (node_modules, __pycache__, etc.) and
//! supports a custom `.ngiignore` file. Used by both the indexer and freshness
//! checker so they always agree on which files to consider.

use std::path::Path;

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Default ignore patterns for directories that are almost never useful to index.
///
/// Patterns with `**/` match at any depth; root-only patterns use `!/` prefix.
/// `target/`, `build/`, `dist/` are root-only because they're build output
/// directories in Rust/JS/Python but legitimate source dirs in other projects
/// (e.g. `drivers/target/` in the Linux kernel).
const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "!**/node_modules/",
    "!**/venv/",
    "!**/.venv/",
    "!**/__pycache__/",
    "!/target/",
    "!**/.tox/",
    "!/dist/",
    "!**/.mypy_cache/",
    "!/build/",
    "!**/.eggs/",
    "!**/*.egg-info/",
    "!**/.ngi/",
];

/// Build a file walker with default ignore patterns and `.ngiignore` support.
pub fn build_walker(root: &Path) -> ignore::Walk {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in DEFAULT_IGNORE_PATTERNS {
        overrides.add(pattern).expect("invalid default ignore pattern");
    }

    WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .overrides(overrides.build().expect("failed to build overrides"))
        .add_custom_ignore_filename(".ngiignore")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    /// Collect all file paths (relative to root) from the walker.
    fn walked_files(root: &Path) -> HashSet<String> {
        let mut paths = HashSet::new();
        for entry in build_walker(root) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(root) {
                paths.insert(rel.to_string_lossy().into_owned());
            }
        }
        paths
    }

    #[test]
    fn skips_node_modules() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("node_modules/lodash")).unwrap();
        fs::write(dir.path().join("node_modules/lodash/index.js"), "module.exports = {}").unwrap();
        fs::write(dir.path().join("app.js"), "require('lodash')").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("app.js"));
        assert!(!files.iter().any(|p| p.contains("node_modules")));
    }

    #[test]
    fn skips_pycache() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("__pycache__")).unwrap();
        fs::write(dir.path().join("__pycache__/mod.cpython-311.pyc"), "binary").unwrap();
        fs::write(dir.path().join("main.py"), "print('hello')").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("main.py"));
        assert!(!files.iter().any(|p| p.contains("__pycache__")));
    }

    #[test]
    fn skips_venv() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("venv/lib")).unwrap();
        fs::write(dir.path().join("venv/lib/site.py"), "# venv").unwrap();
        fs::create_dir_all(dir.path().join(".venv/lib")).unwrap();
        fs::write(dir.path().join(".venv/lib/site.py"), "# .venv").unwrap();
        fs::write(dir.path().join("app.py"), "import os").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("app.py"));
        assert!(!files.iter().any(|p| p.contains("venv")));
    }

    #[test]
    fn skips_ngi_directory() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".ngi")).unwrap();
        fs::write(dir.path().join(".ngi/lookup.ngi"), "index data").unwrap();
        fs::write(dir.path().join("src.rs"), "fn main() {}").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("src.rs"));
        assert!(!files.iter().any(|p| p.contains(".ngi")));
    }

    #[test]
    fn skips_root_target() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::write(dir.path().join("target/debug/binary"), "elf").unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn foo() {}").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("lib.rs"));
        assert!(!files.iter().any(|p| p.starts_with("target/")));
    }

    #[test]
    fn includes_nested_target_dirs() {
        // Directories named "target" deeper in the tree are legitimate source
        // (e.g. drivers/target/ in the Linux kernel)
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("drivers/target")).unwrap();
        fs::write(dir.path().join("drivers/target/core.c"), "int main() {}").unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn foo() {}").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("lib.rs"));
        assert!(files.contains("drivers/target/core.c"), "nested target/ dirs should be indexed");
    }

    #[test]
    fn respects_ngiignore() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".ngiignore"), "secret.txt\n").unwrap();
        fs::write(dir.path().join("secret.txt"), "password=123").unwrap();
        fs::write(dir.path().join("public.txt"), "hello world").unwrap();

        let files = walked_files(dir.path());
        assert!(files.contains("public.txt"));
        assert!(!files.contains("secret.txt"));
    }
}
