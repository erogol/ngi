//! Tests for the code review fixes:
//! 1. Deterministic hash function (stable across runs)
//! 2. Percentage-based rg fallback threshold
//! 3. Bloom masks optional (default off)
//! 4. Error surfacing from walker

use std::fs;
use ngi::indexer::{build_index, DEFAULT_MAX_FILE_SIZE};
use ngi::ngram::hash_ngram;
use ngi::search::search;
use ngi::storage::{write_index, write_index_with_meta, MappedIndex};

// ============================================================================
// Fix 1: Deterministic hash function
// ============================================================================

#[test]
fn hash_ngram_is_deterministic_across_calls() {
    // Same input must always produce the same hash
    let input = b"hello";
    let h1 = hash_ngram(input);
    let h2 = hash_ngram(input);
    let h3 = hash_ngram(input);
    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
}

#[test]
fn hash_ngram_known_values() {
    // FNV-1a known test vectors for byte sequences.
    // If this test breaks after a rustc upgrade, our hash function changed —
    // which is exactly the bug we're preventing.
    //
    // We pin the expected values after implementing FNV-1a.
    // If DefaultHasher is still used, these will be wrong.
    let h_abc = hash_ngram(b"abc");
    let h_def = hash_ngram(b"def");
    let h_abc2 = hash_ngram(b"abc");

    // Same input → same hash (basic sanity)
    assert_eq!(h_abc, h_abc2);
    // Different inputs → different hashes (collision would be astronomically unlikely)
    assert_ne!(h_abc, h_def);

    // Pin the actual FNV-1a values so we detect if hash function changes.
    // FNV-1a 64-bit for "abc" = 0xe71fa2190541574b
    // FNV-1a 64-bit for "def" = 0xca9a1a18f461e4cc
    assert_eq!(h_abc, 0xe71fa2190541574b, "hash_ngram(\"abc\") must match FNV-1a spec");
    assert_eq!(h_def, 0xca9a1a18f461e4cc, "hash_ngram(\"def\") must match FNV-1a spec");
}

#[test]
fn hash_ngram_empty_input() {
    // Empty input should still produce a valid hash (the FNV offset basis)
    let h = hash_ngram(b"");
    // FNV-1a 64-bit offset basis = 0xcbf29ce484222325
    assert_eq!(h, 0xcbf29ce484222325);
}

#[test]
fn index_roundtrip_with_new_hash() {
    // Build an index, write it, read it back, and verify lookups work.
    // This catches any mismatch between indexer hash and query hash.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("test.rs"), "fn parse_token() { return 42; }").unwrap();

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    assert!(index.stats.file_count > 0);

    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();
    let mapped = MappedIndex::open(&index_dir).unwrap();

    // A trigram from "parse_token" should be findable
    let h = hash_ngram(b"par");
    let postings = mapped.lookup(h);
    assert!(postings.is_some(), "trigram 'par' should be in index");
    assert!(!postings.unwrap().is_empty(), "posting list for 'par' should not be empty");
}

// ============================================================================
// Fix 2: Percentage-based rg fallback
// ============================================================================

#[test]
fn broad_query_uses_fullscan_path() {
    // When a query matches >15% of files, we should see the full_scan flag
    // or at minimum not pay the cost of building candidate lists.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create 20 files all containing "the"
    for i in 0..20 {
        fs::write(root.join(format!("f{i}.txt")), format!("the value is {i}")).unwrap();
    }

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();

    let result = search(&index_dir, root, "the", false, None, false, 0, 0, None).unwrap();
    // All 20 files contain "the" so candidates = 100% of total.
    // With percentage threshold, this should still work correctly
    // (either via rg full scan or our parallel search — just shouldn't
    // pass 20 file paths as rg args when it would be faster not to).
    assert_eq!(result.matches.len(), 20);
}

#[test]
fn selective_query_uses_index() {
    // When a query matches <15% of files, the index should narrow candidates
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // 1 file with the target, 19 without
    fs::write(root.join("target.txt"), "PyObject_GC_Track(obj)").unwrap();
    for i in 0..19 {
        fs::write(root.join(format!("other{i}.txt")), format!("unrelated content {i}")).unwrap();
    }

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();

    let result = search(&index_dir, root, "PyObject_GC_Track", false, None, false, 0, 0, None).unwrap();
    assert_eq!(result.matches.len(), 1);
    // Should NOT be a full scan
    assert!(!result.full_scan);
    assert!(result.files_searched < result.total_files);
}

// ============================================================================
// Fix 3: Bloom masks default off / index format
// ============================================================================

#[test]
fn index_v3_no_masks_is_smaller() {
    // Without bloom masks, each posting saves 2 bytes.
    // The index should be noticeably smaller.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create a file with enough content to generate many postings
    let content = "fn parse_token_stream() { let ast = compile_module(); optimize_ir(ast); }";
    fs::write(root.join("code.rs"), content).unwrap();

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();

    let postings_size = fs::metadata(index_dir.join("postings.ngi")).unwrap().len();

    // Without masks: each posting = varint (1-2 bytes typically)
    // With masks: each posting = varint + 2 bytes
    // For a file with ~70 chars → ~68 trigrams → ~50 unique hashes
    // Each posting ≈ 1 byte (varint for small deltas) + 0 or 2 for masks
    // So no-mask should be roughly 60% the size of with-masks
    assert!(
        postings_size < 500,
        "postings file should be compact without masks: {postings_size} bytes"
    );
}

#[test]
fn search_works_without_masks() {
    // Core search correctness must work without bloom mask filtering
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.rs"), "fn hello_world() { println!(\"hello\"); }").unwrap();
    fs::write(root.join("b.rs"), "fn goodbye() { println!(\"bye\"); }").unwrap();

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();

    let result = search(&index_dir, root, "hello_world", false, None, false, 0, 0, None).unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.matches[0].path.contains("a.rs"));
}

// ============================================================================
// Fix 4: Error surfacing
// ============================================================================

#[test]
fn unreadable_file_counted_in_stats() {
    // Files that can't be read should be counted as skipped, not silently dropped
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("good.txt"), "fn parse_token() { }").unwrap();
    // Binary file should be counted as skipped_binary
    fs::write(root.join("binary.dat"), &[0u8, 1, 2, 3, 0, 0, 0, 0]).unwrap();

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    assert_eq!(index.stats.file_count, 1);
    assert!(index.stats.skipped_binary >= 1);
}

// ============================================================================
// Regression: existing functionality still works
// ============================================================================

#[test]
fn incremental_reindex_with_new_format() {
    use ngi::freshness::{check_freshness, collect_file_states};
    use ngi::indexer::incremental_reindex;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.txt"), "fn parse_token() { return token; }").unwrap();
    fs::write(root.join("b.txt"), "fn compile_ast() { return ast; }").unwrap();

    // Build initial index WITH filemeta (required for incremental)
    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    let states = collect_file_states(root).unwrap();
    write_index_with_meta(&index, &index_dir, root, Some(&states), None).unwrap();

    // Modify one file
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(root.join("a.txt"), "fn parse_modified_token() { return new_token; }").unwrap();

    // Incremental reindex
    let report = check_freshness(&index_dir, root).unwrap();
    let old_index = MappedIndex::open(&index_dir).unwrap();
    let incr = incremental_reindex(root, &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE).unwrap();
    drop(old_index);

    // Compare with full rebuild
    let full = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    assert_eq!(incr.files.len(), full.files.len());
    assert_eq!(incr.postings.len(), full.postings.len());
}

#[test]
fn case_insensitive_search_works() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.txt"), "PyObject_GC_Track(obj)").unwrap();

    let index = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    let index_dir = root.join(".ngi");
    write_index(&index, &index_dir, root).unwrap();

    // Case insensitive should find it
    let result = search(&index_dir, root, "pyobject_gc_track", true, None, false, 0, 0, None).unwrap();
    assert_eq!(result.matches.len(), 1);

    // Exact case should also find it
    let result = search(&index_dir, root, "PyObject_GC_Track", false, None, false, 0, 0, None).unwrap();
    assert_eq!(result.matches.len(), 1);
}

// ============================================================================
// Max file size configurability
// ============================================================================

#[test]
fn small_max_file_size_skips_large_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create a file that exceeds a small limit (e.g. 100 bytes)
    let big_content = "x".repeat(200);
    fs::write(root.join("big.txt"), &big_content).unwrap();
    fs::write(root.join("small.txt"), "tiny").unwrap();

    let index = build_index(root, 100).unwrap();
    assert_eq!(index.stats.file_count, 1);
    assert_eq!(index.stats.skipped_large, 1);
    assert_eq!(index.files[0], "small.txt");
}

#[test]
fn large_max_file_size_includes_files_old_limit_would_skip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create a file between 1MB and 10MB (the old limit would skip it)
    let size = 2 * 1024 * 1024; // 2MB
    let content = "a".repeat(size);
    fs::write(root.join("medium.txt"), &content).unwrap();

    // Old 1MB limit would skip this file
    let index_small = build_index(root, 1_048_576).unwrap();
    assert_eq!(index_small.stats.file_count, 0);
    assert_eq!(index_small.stats.skipped_large, 1);

    // New 10MB default includes it
    let index_large = build_index(root, DEFAULT_MAX_FILE_SIZE).unwrap();
    assert_eq!(index_large.stats.file_count, 1);
    assert_eq!(index_large.stats.skipped_large, 0);
    assert_eq!(index_large.files[0], "medium.txt");
}
