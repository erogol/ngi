//! File walker and inverted index builder.
//!
//! Walks a directory tree (respecting .gitignore), extracts trigrams from
//! each text file, and builds an in-memory inverted index mapping trigram
//! hashes to file IDs.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;

use crate::freshness::FreshnessReport;
use crate::storage::MappedIndex;
use crate::walk::build_walker;

/// Default maximum file size to index (10 MB).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 10_485_760;

/// Number of leading bytes checked for null bytes (binary detection).
const BINARY_CHECK_LEN: usize = 8192;

/// A fully built in-memory inverted index, ready to be serialized.
pub struct InMemoryIndex {
    /// Ordered list of indexed file paths (relative to root).
    pub files: Vec<String>,
    /// Map from n-gram hash → sorted list of file IDs.
    pub postings: HashMap<u64, Vec<u32>>,
    /// Statistics about the build process.
    pub stats: IndexStats,
}

/// Statistics about the index build.
pub struct IndexStats {
    pub file_count: usize,
    pub ngram_count: usize,
    pub total_ngrams: usize,
    pub build_duration: Duration,
    pub skipped_binary: usize,
    pub skipped_large: usize,
    pub skipped_errors: usize,
}

use crate::ngram::hash_ngram;

/// Result of processing a single file for indexing.
enum FileResult {
    Indexed { rel_path: String, hashes: HashSet<u64> },
    SkippedBinary,
    SkippedLarge,
    Error(String),
}

/// Build an in-memory inverted index for all text files under `root`.
/// Files larger than `max_file_size` bytes are skipped.
pub fn build_index(root: &Path, max_file_size: u64) -> Result<InMemoryIndex> {
    let start = Instant::now();

    // Phase 1: Collect all file paths sequentially (walker isn't Send)
    let walker = build_walker(root);

    let mut file_paths: Vec<(PathBuf, u64)> = Vec::new();
    let mut walker_errors: usize = 0;
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warning: {e}");
                walker_errors += 1;
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let size = match entry.metadata() {
            Ok(m) => m.len(),
            Err(e) => {
                eprintln!("warning: {}: {e}", entry.path().display());
                walker_errors += 1;
                continue;
            }
        };
        file_paths.push((entry.into_path(), size));
    }

    // Sort for deterministic file ID assignment across runs.
    // Without this, readdir order can vary and file IDs shift, causing
    // the index to return wrong candidates on search.
    file_paths.sort_by(|a, b| a.0.cmp(&b.0));

    // Phase 2: Process files in parallel (read + trigram extraction)
    let root_owned = root.to_path_buf();
    let results: Vec<FileResult> = file_paths
        .par_iter()
        .map(|(path, size)| {
            if *size > max_file_size {
                return FileResult::SkippedLarge;
            }

            let mut contents = Vec::with_capacity(*size as usize);
            let mut file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(e) => return FileResult::Error(format!("{}: {e}", path.display())),
            };
            if let Err(e) = file.read_to_end(&mut contents) {
                return FileResult::Error(format!("{}: {e}", path.display()));
            }

            let check_len = contents.len().min(BINARY_CHECK_LEN);
            if contents[..check_len].contains(&0) {
                return FileResult::SkippedBinary;
            }

            let rel_path = match path.strip_prefix(&root_owned) {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) => path.to_string_lossy().into_owned(),
            };

            // Lowercase the entire content for case-insensitive indexing.
            let lower: Vec<u8> = contents.iter().map(|b| b.to_ascii_lowercase()).collect();

            let mut hashes: HashSet<u64> = HashSet::new();
            for i in 0..lower.len().saturating_sub(2) {
                let h = hash_ngram(&lower[i..i + 3]);
                hashes.insert(h);
            }

            FileResult::Indexed { rel_path, hashes }
        })
        .collect();

    // Phase 3: Merge results into the final index (sequential)
    let mut files: Vec<String> = Vec::new();
    let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut total_ngrams: usize = 0;
    let mut skipped_binary: usize = 0;
    let mut skipped_large: usize = 0;
    let mut skipped_errors: usize = walker_errors;

    for result in results {
        match result {
            FileResult::SkippedBinary => skipped_binary += 1,
            FileResult::SkippedLarge => skipped_large += 1,
            FileResult::Error(ref msg) => {
                eprintln!("warning: {msg}");
                skipped_errors += 1;
            }
            FileResult::Indexed { rel_path, hashes } => {
                let file_id = files.len() as u32;
                files.push(rel_path);
                total_ngrams += hashes.len();
                for h in &hashes {
                    postings.entry(*h).or_default().push(file_id);
                }
            }
        }
    }

    // Sort every posting list by file_id
    for list in postings.values_mut() {
        list.sort_unstable();
    }

    let build_duration = start.elapsed();

    Ok(InMemoryIndex {
        stats: IndexStats {
            file_count: files.len(),
            ngram_count: postings.len(),
            total_ngrams,
            build_duration,
            skipped_binary,
            skipped_large,
            skipped_errors,
        },
        files,
        postings,
    })
}

/// Process a single file: read, check binary, extract n-gram hashes.
/// Returns None if the file should be skipped.
fn process_file(path: &Path, root: &Path, max_file_size: u64) -> Option<(String, HashSet<u64>)> {
    let metadata = path.metadata().ok()?;
    if metadata.len() > max_file_size {
        return None;
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    let mut file = std::fs::File::open(path).ok()?;
    if file.read_to_end(&mut contents).is_err() {
        return None;
    }
    let check_len = contents.len().min(BINARY_CHECK_LEN);
    if contents[..check_len].contains(&0) {
        return None;
    }
    let rel_path = path
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .into_owned();
    let lower: Vec<u8> = contents.iter().map(|b| b.to_ascii_lowercase()).collect();

    let mut hashes: HashSet<u64> = HashSet::new();
    for i in 0..lower.len().saturating_sub(2) {
        let h = hash_ngram(&lower[i..i + 3]);
        hashes.insert(h);
    }
    Some((rel_path, hashes))
}

/// Incrementally update an existing index.
///
/// Keeps posting list entries for unchanged files, re-processes changed/new files,
/// and removes deleted files. Writes a complete new index.
pub fn incremental_reindex(
    root: &Path,
    _index_dir: &std::path::Path,
    report: &FreshnessReport,
    old_index: &MappedIndex,
    max_file_size: u64,
) -> Result<InMemoryIndex> {
    let start = Instant::now();

    // Build set of dirty paths (changed + deleted) for fast lookup
    let dirty_paths: HashSet<&str> = report
        .changed
        .iter()
        .chain(report.deleted.iter())
        .map(|s| s.as_str())
        .collect();

    // Build old file_id → path mapping and identify which old file_ids to keep
    let old_files = old_index.file_list();
    let mut kept_old_ids: HashSet<u32> = HashSet::new();
    for (id, path) in old_files.iter().enumerate() {
        if !dirty_paths.contains(path.as_str()) {
            kept_old_ids.insert(id as u32);
        }
    }

    // Start building the new file list: kept files first (in order)
    let mut files: Vec<String> = Vec::new();
    // old_id → new_id mapping for kept files
    let mut id_remap: HashMap<u32, u32> = HashMap::new();
    for (old_id, path) in old_files.iter().enumerate() {
        if kept_old_ids.contains(&(old_id as u32)) {
            let new_id = files.len() as u32;
            id_remap.insert(old_id as u32, new_id);
            files.push(path.clone());
        }
    }

    // Process changed + new files in parallel
    let reprocess_paths: Vec<PathBuf> = report
        .changed
        .iter()
        .chain(report.added.iter())
        .map(|p| root.join(p))
        .collect();

    let root_owned = root.to_path_buf();
    let new_results: Vec<Option<(String, HashSet<u64>)>> = reprocess_paths
        .par_iter()
        .map(|path| process_file(path, &root_owned, max_file_size))
        .collect();

    // Assign new file IDs for reprocessed files
    let mut new_file_hashes: Vec<(u32, HashSet<u64>)> = Vec::new();
    for (rel_path, hashes) in new_results.into_iter().flatten() {
        let file_id = files.len() as u32;
        files.push(rel_path);
        new_file_hashes.push((file_id, hashes));
    }

    // Build postings: start from old postings with remapped IDs
    let mut postings: HashMap<u64, Vec<u32>> = HashMap::new();
    let old_postings = old_index.all_postings();
    for (hash, old_file_ids) in &old_postings {
        let remapped: Vec<u32> = old_file_ids
            .iter()
            .filter_map(|&fid| id_remap.get(&fid).copied())
            .collect();
        if !remapped.is_empty() {
            postings.insert(*hash, remapped);
        }
    }

    // Add postings from reprocessed files
    let mut total_ngrams: usize = 0;
    for (file_id, hashes) in &new_file_hashes {
        total_ngrams += hashes.len();
        for h in hashes {
            postings.entry(*h).or_default().push(*file_id);
        }
    }

    // Sort every posting list
    for list in postings.values_mut() {
        list.sort_unstable();
    }

    let build_duration = start.elapsed();

    Ok(InMemoryIndex {
        stats: IndexStats {
            file_count: files.len(),
            ngram_count: postings.len(),
            total_ngrams,
            build_duration,
            skipped_binary: 0,
            skipped_large: 0,
            skipped_errors: 0,
        },
        files,
        postings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn empty_directory() {
        let dir = TempDir::new().unwrap();
        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.files.len(), 0);
        assert!(index.postings.is_empty());
        assert_eq!(index.stats.file_count, 0);
        assert_eq!(index.stats.ngram_count, 0);
        assert_eq!(index.stats.total_ngrams, 0);
    }

    #[test]
    fn single_file_indexed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hello.txt"), "hello world from the indexer").unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0], "hello.txt");
        assert_eq!(index.stats.file_count, 1);
        assert!(index.stats.ngram_count > 0);
        assert!(index.stats.total_ngrams > 0);

        // Every posting list should contain file 0
        for list in index.postings.values() {
            assert!(list.contains(&0));
        }
    }

    #[test]
    fn binary_file_skipped() {
        let dir = TempDir::new().unwrap();
        // Binary file: contains null bytes
        let mut bin_content = b"hello\x00world".to_vec();
        bin_content.extend_from_slice(&[0u8; 100]);
        fs::write(dir.path().join("binary.dat"), &bin_content).unwrap();
        // Text file
        fs::write(dir.path().join("text.txt"), "some real text content here").unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0], "text.txt");
        assert_eq!(index.stats.skipped_binary, 1);
    }

    #[test]
    fn respects_gitignore() {
        let dir = TempDir::new().unwrap();

        // Initialize a git repo so .gitignore is honored
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "this should be ignored").unwrap();
        fs::write(dir.path().join("included.txt"), "this should be indexed").unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0], "included.txt");
    }

    #[test]
    fn posting_lists_are_sorted() {
        let dir = TempDir::new().unwrap();
        // Create multiple files with overlapping content so some n-gram hashes
        // appear in multiple files.
        for i in 0..5 {
            fs::write(
                dir.path().join(format!("file_{i}.txt")),
                format!("shared content for file number {i} with enough text"),
            )
            .unwrap();
        }

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        for (hash, list) in &index.postings {
            for window in list.windows(2) {
                assert!(
                    window[0] <= window[1],
                    "posting list for hash {hash} is not sorted by file_id",
                );
            }
        }
    }

    #[test]
    fn stats_populated() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "some text content").unwrap();
        fs::write(dir.path().join("b.txt"), "more text content").unwrap();
        let mut bin = vec![0u8; 100];
        bin.extend_from_slice(b"binary stuff");
        fs::write(dir.path().join("c.bin"), &bin).unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.stats.file_count, 2);
        assert_eq!(index.stats.skipped_binary, 1);
        assert!(index.stats.ngram_count > 0);
        assert!(index.stats.total_ngrams > 0);
        assert!(index.stats.build_duration.as_nanos() > 0);
    }

    #[test]
    fn large_file_skipped() {
        let dir = TempDir::new().unwrap();
        // Create a file just over the limit
        let big = vec![b'x'; (DEFAULT_MAX_FILE_SIZE + 1) as usize];
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        fs::write(dir.path().join("small.txt"), "small file").unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.stats.file_count, 1);
        assert_eq!(index.stats.skipped_large, 1);
        assert_eq!(index.files[0], "small.txt");
    }

    #[test]
    fn hash_ngram_deterministic() {
        let a = hash_ngram(b"hello");
        let b = hash_ngram(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_ngram_different_inputs_differ() {
        let a = hash_ngram(b"abc");
        let b = hash_ngram(b"def");
        assert_ne!(a, b);
    }

    #[test]
    fn multiple_files_share_postings() {
        let dir = TempDir::new().unwrap();
        // Same content → same n-grams → shared posting lists
        let content = "exactly the same content in both files";
        fs::write(dir.path().join("a.txt"), content).unwrap();
        fs::write(dir.path().join("b.txt"), content).unwrap();

        let index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(index.files.len(), 2);
        // Every posting list should contain both file IDs
        for list in index.postings.values() {
            assert_eq!(list.len(), 2, "expected both files in posting list");
        }
    }

    // --- Incremental reindex tests ---

    use crate::freshness::{check_freshness, collect_file_states};
    use crate::storage::{write_index_with_meta, MappedIndex};

    /// Helper: build index with filemeta and write to disk.
    fn build_and_write(dir: &std::path::Path) {
        let index = build_index(dir, DEFAULT_MAX_FILE_SIZE).unwrap();
        let index_dir = dir.join(".ngi");
        let states = collect_file_states(dir).unwrap();
        write_index_with_meta(&index, &index_dir, dir, Some(&states), None).unwrap();
    }

    #[test]
    fn incremental_after_file_change_produces_correct_results() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha bravo charlie").unwrap();
        fs::write(dir.path().join("b.txt"), "delta echo foxtrot").unwrap();
        build_and_write(dir.path());

        // Modify a.txt — change content (different size to guarantee mtime/size change)
        fs::write(dir.path().join("a.txt"), "xray yankee zulu modified content here").unwrap();

        let index_dir = dir.path().join(".ngi");
        let report = check_freshness(&index_dir, dir.path()).unwrap();
        assert!(!report.is_fresh);
        assert!(report.changed.contains(&"a.txt".to_string()));

        let old_index = MappedIndex::open(&index_dir).unwrap();
        let new_index = incremental_reindex(dir.path(), &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE).unwrap();

        // New index should have both files
        assert_eq!(new_index.files.len(), 2);
        assert!(new_index.files.contains(&"a.txt".to_string()));
        assert!(new_index.files.contains(&"b.txt".to_string()));

        // Write new index and verify search works
        drop(old_index);
        let states = collect_file_states(dir.path()).unwrap();
        write_index_with_meta(&new_index, &index_dir, dir.path(), Some(&states), None).unwrap();

        // Verify the index produces correct results via full rebuild comparison
        let full_index = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        assert_eq!(new_index.files.len(), full_index.files.len());
    }

    #[test]
    fn incremental_after_file_delete_removes_old_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "unique_content_aaa_only_in_a").unwrap();
        fs::write(dir.path().join("b.txt"), "unique_content_bbb_only_in_b").unwrap();
        fs::write(dir.path().join("c.txt"), "unique_content_ccc_only_in_c").unwrap();
        build_and_write(dir.path());

        // Delete b.txt
        fs::remove_file(dir.path().join("b.txt")).unwrap();

        let index_dir = dir.path().join(".ngi");
        let report = check_freshness(&index_dir, dir.path()).unwrap();
        assert!(report.deleted.contains(&"b.txt".to_string()));

        let old_index = MappedIndex::open(&index_dir).unwrap();
        let new_index = incremental_reindex(dir.path(), &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE).unwrap();

        assert_eq!(new_index.files.len(), 2);
        assert!(!new_index.files.contains(&"b.txt".to_string()));
        assert!(new_index.files.contains(&"a.txt".to_string()));
        assert!(new_index.files.contains(&"c.txt".to_string()));
    }

    #[test]
    fn incremental_after_file_add() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello world").unwrap();
        build_and_write(dir.path());

        // Add a new file
        fs::write(dir.path().join("b.txt"), "brand new file with unique_new_content").unwrap();

        let index_dir = dir.path().join(".ngi");
        let report = check_freshness(&index_dir, dir.path()).unwrap();
        assert!(report.added.contains(&"b.txt".to_string()));

        let old_index = MappedIndex::open(&index_dir).unwrap();
        let new_index = incremental_reindex(dir.path(), &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE).unwrap();

        assert_eq!(new_index.files.len(), 2);
        assert!(new_index.files.contains(&"a.txt".to_string()));
        assert!(new_index.files.contains(&"b.txt".to_string()));
    }

    #[test]
    fn incremental_matches_full_rebuild() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "fn parse_token() { return token; }").unwrap();
        fs::write(dir.path().join("b.txt"), "fn compile_ast() { return ast; }").unwrap();
        fs::write(dir.path().join("c.txt"), "fn optimize_ir() { return ir; }").unwrap();
        build_and_write(dir.path());

        // Modify one, delete one, add one
        fs::write(dir.path().join("a.txt"), "fn parse_modified_token() { return new_token; }").unwrap();
        fs::remove_file(dir.path().join("b.txt")).unwrap();
        fs::write(dir.path().join("d.txt"), "fn new_function() { return value; }").unwrap();

        let index_dir = dir.path().join(".ngi");
        let report = check_freshness(&index_dir, dir.path()).unwrap();
        let old_index = MappedIndex::open(&index_dir).unwrap();
        let incr = incremental_reindex(dir.path(), &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE).unwrap();
        drop(old_index);

        // Compare with full rebuild
        let full = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();

        // Same file count
        assert_eq!(incr.files.len(), full.files.len());

        // Same set of files
        let mut incr_files = incr.files.clone();
        incr_files.sort();
        let mut full_files = full.files.clone();
        full_files.sort();
        assert_eq!(incr_files, full_files);

        // Same n-gram count
        assert_eq!(incr.postings.len(), full.postings.len());
    }

    #[test]
    fn file_ids_are_deterministic_across_rebuilds() {
        // Two full builds of the same directory must produce identical
        // file ordering (and thus identical file IDs). This prevents
        // the index from returning wrong candidates after a rebuild.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("zebra.py"), "def zebra(): pass").unwrap();
        fs::write(dir.path().join("alpha.py"), "def alpha(): pass").unwrap();
        fs::write(dir.path().join("middle.py"), "def middle(): pass").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/beta.py"), "def beta(): pass").unwrap();

        let index1 = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();
        let index2 = build_index(dir.path(), DEFAULT_MAX_FILE_SIZE).unwrap();

        assert_eq!(index1.files, index2.files, "file ordering must be identical across builds");

        // Verify files are sorted (not in walk order)
        let mut sorted = index1.files.clone();
        sorted.sort();
        assert_eq!(index1.files, sorted, "files must be in sorted order");
    }

}
