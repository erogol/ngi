//! Freshness checking: compare stored file metadata against the current filesystem
//! to determine which files need re-indexing.

use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;

use crate::storage::{read_filemeta, FileState};
use crate::walk::build_walker;

/// Result of a freshness check.
#[derive(Debug)]
pub struct FreshnessReport {
    /// Paths that changed (mtime or size differ).
    pub changed: Vec<String>,
    /// Paths in the index but no longer on disk.
    pub deleted: Vec<String>,
    /// Paths on disk but not in the index.
    pub added: Vec<String>,
    /// True if nothing changed.
    pub is_fresh: bool,
    /// Number of files currently in the index.
    pub total_indexed: usize,
}

/// Get mtime (nanoseconds since epoch) and size for a file, or None if stat fails.
///
/// Using nanosecond precision avoids missed updates when a file is modified
/// within the same second as the last index build (same size, same second).
fn file_mtime_size(path: &Path) -> Option<(u64, u64)> {
    let meta = path.metadata().ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, meta.len()))
}

/// Walk the filesystem and return the set of indexable file paths (relative to root).
fn walk_current_files(root: &Path) -> Result<HashSet<String>> {
    let walker = build_walker(root);

    let mut paths = HashSet::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            paths.insert(rel.to_string_lossy().into_owned());
        }
    }
    Ok(paths)
}

/// Compare stored file metadata against the current filesystem state.
pub fn check_freshness(index_dir: &Path, root: &Path) -> Result<FreshnessReport> {
    let stored = read_filemeta(index_dir)?;
    let total_indexed = stored.len();

    let stored_map: std::collections::HashMap<&str, &FileState> = stored
        .iter()
        .map(|s| (s.path.as_str(), s))
        .collect();
    let stored_paths: HashSet<&str> = stored_map.keys().copied().collect();

    let current_files = walk_current_files(root)?;

    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    let mut added = Vec::new();

    // Check stored files: changed or deleted?
    for state in &stored {
        if !current_files.contains(&state.path) {
            deleted.push(state.path.clone());
        } else {
            let full_path = root.join(&state.path);
            if let Some((mtime, size)) = file_mtime_size(&full_path) {
                if mtime != state.mtime || size != state.size {
                    changed.push(state.path.clone());
                }
            } else {
                // Can't stat → treat as deleted
                deleted.push(state.path.clone());
            }
        }
    }

    // Check for new files (on disk but not in stored)
    for path in &current_files {
        if !stored_paths.contains(path.as_str()) {
            added.push(path.clone());
        }
    }

    changed.sort();
    deleted.sort();
    added.sort();

    let is_fresh = changed.is_empty() && deleted.is_empty() && added.is_empty();

    Ok(FreshnessReport {
        changed,
        deleted,
        added,
        is_fresh,
        total_indexed,
    })
}

/// Collect FileState for all files that would be indexed.
pub fn collect_file_states(root: &Path) -> Result<Vec<FileState>> {
    let walker = build_walker(root);

    let mut states = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root)
            && let Some((mtime, size)) = file_mtime_size(entry.path())
        {
            states.push(FileState {
                path: rel.to_string_lossy().into_owned(),
                mtime,
                size,
            });
        }
    }
    Ok(states)
}

/// Try to read git HEAD from the repository root.
pub fn read_git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::build_index;
    use crate::storage::write_index_with_meta;
    use std::fs;
    use tempfile::TempDir;

    fn build_and_write_with_meta(dir: &Path) {
        let index = build_index(dir, crate::indexer::DEFAULT_MAX_FILE_SIZE).unwrap();
        let index_dir = dir.join(".ngi");
        let states = collect_file_states(dir).unwrap();
        write_index_with_meta(&index, &index_dir, dir, Some(&states), None).unwrap();
    }

    #[test]
    fn fresh_after_build() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world").unwrap();
        fs::write(dir.path().join("b.txt"), "goodbye world").unwrap();
        build_and_write_with_meta(dir.path());

        let report = check_freshness(&dir.path().join(".ngi"), dir.path()).unwrap();
        assert!(report.is_fresh, "expected fresh, got: changed={:?}, deleted={:?}, added={:?}",
            report.changed, report.deleted, report.added);
        assert_eq!(report.total_indexed, 2);
    }

    #[test]
    fn detects_changed_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "original content").unwrap();
        build_and_write_with_meta(dir.path());

        // Modify the file (change size to ensure detection)
        fs::write(dir.path().join("a.txt"), "modified content with more text").unwrap();

        let report = check_freshness(&dir.path().join(".ngi"), dir.path()).unwrap();
        assert!(!report.is_fresh);
        assert!(report.changed.contains(&"a.txt".to_string()));
    }

    #[test]
    fn detects_deleted_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.txt"), "world").unwrap();
        build_and_write_with_meta(dir.path());

        fs::remove_file(dir.path().join("b.txt")).unwrap();

        let report = check_freshness(&dir.path().join(".ngi"), dir.path()).unwrap();
        assert!(!report.is_fresh);
        assert!(report.deleted.contains(&"b.txt".to_string()));
    }

    #[test]
    fn detects_added_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        build_and_write_with_meta(dir.path());

        fs::write(dir.path().join("c.txt"), "new file").unwrap();

        let report = check_freshness(&dir.path().join(".ngi"), dir.path()).unwrap();
        assert!(!report.is_fresh);
        assert!(report.added.contains(&"c.txt".to_string()));
    }

    #[test]
    fn no_changes_is_fresh() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "content").unwrap();
        build_and_write_with_meta(dir.path());

        let report = check_freshness(&dir.path().join(".ngi"), dir.path()).unwrap();
        assert!(report.is_fresh);
        assert!(report.changed.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.added.is_empty());
    }
}
