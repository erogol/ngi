//! Tests for auto-index on first search.
//!
//! When `ngi search` is run and no `.ngi/` exists, it should:
//! 1. Detect the missing index
//! 2. Build it automatically
//! 3. Run the search
//! 4. Return correct results
//!
//! This also covers the CLI integration (binary invocation).

use std::fs;
use std::process::Command;

fn ngi_bin() -> Command {
    let cmd = Command::new(env!("CARGO_BIN_EXE_ngi"));
    cmd
}

// ============================================================================
// Core: auto-index triggers on missing index
// ============================================================================

#[test]
fn search_without_index_auto_builds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("hello.rs"), "fn hello_world() { println!(\"hi\"); }").unwrap();
    fs::write(root.join("other.rs"), "fn goodbye() { return; }").unwrap();

    // No .ngi/ exists
    assert!(!root.join(".ngi").exists());

    // Search should work anyway
    let output = ngi_bin()
        .args(["search", "hello_world"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should find the match
    assert!(stdout.contains("hello_world"), "stdout: {stdout}");

    // Should mention auto-indexing in stderr
    assert!(
        stderr.contains("Auto-indexing") || stderr.contains("auto-index") || stderr.contains("Building index"),
        "stderr should mention auto-indexing: {stderr}"
    );

    // Index should now exist
    assert!(root.join(".ngi").exists());
    assert!(root.join(".ngi/lookup.ngi").exists());
    assert!(root.join(".ngi/postings.ngi").exists());
    assert!(root.join(".ngi/files.txt").exists());
}

#[test]
fn auto_index_results_match_explicit_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.rs"), "fn parse_token() { return token; }").unwrap();
    fs::write(root.join("b.rs"), "fn compile_ast() { return ast; }").unwrap();
    fs::write(root.join("c.rs"), "fn parse_and_compile() { parse_token(); compile_ast(); }").unwrap();

    // Auto-index search
    let auto_output = ngi_bin()
        .args(["search", "parse_token", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();
    let auto_stdout = String::from_utf8_lossy(&auto_output.stdout);

    // Clean and rebuild explicitly
    let _ = fs::remove_dir_all(root.join(".ngi"));
    let _ = ngi_bin()
        .args(["index", "."])
        .current_dir(root)
        .output()
        .unwrap();

    let explicit_output = ngi_bin()
        .args(["search", "parse_token", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();
    let explicit_stdout = String::from_utf8_lossy(&explicit_output.stdout);

    // Results should be identical
    assert_eq!(
        auto_stdout.to_string(),
        explicit_stdout.to_string(),
        "auto-index and explicit index should produce identical results"
    );
}

#[test]
fn auto_index_with_no_matches_still_builds_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.txt"), "hello world").unwrap();

    let _output = ngi_bin()
        .args(["search", "zzzznotfound"])
        .current_dir(root)
        .output()
        .unwrap();

    // No matches — exit code should be non-zero (grep convention)
    // But the index should still be created for next time
    assert!(root.join(".ngi").exists(), "index should be built even with no matches");
}

#[test]
fn auto_index_handles_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // No files at all
    let output = ngi_bin()
        .args(["search", "anything"])
        .current_dir(root)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should not crash, should handle gracefully
    assert!(
        !stderr.contains("panic") && !stderr.contains("RUST_BACKTRACE"),
        "should not panic on empty dir: {stderr}"
    );
}

#[test]
fn second_search_uses_existing_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("code.rs"), "fn important_function() { }").unwrap();

    // First search: auto-builds
    let _ = ngi_bin()
        .args(["search", "important_function", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();

    assert!(root.join(".ngi").exists());

    // Second search: uses existing index, should NOT mention auto-indexing
    let output = ngi_bin()
        .args(["search", "important_function", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Auto-indexing") && !stderr.contains("Building index"),
        "second search should not rebuild: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("important_function"),
        "should still find results"
    );
}

#[test]
fn auto_index_with_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.rs"), "fn MyFunction() { }").unwrap();

    let output = ngi_bin()
        .args(["search", "-i", "myfunction", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("MyFunction"), "case-insensitive should work with auto-index");
}

#[test]
fn auto_index_with_file_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("code.rs"), "fn target() { }").unwrap();
    fs::write(root.join("code.py"), "def target(): pass").unwrap();

    let output = ngi_bin()
        .args(["search", "target", "-f", "\\.rs$", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("code.rs"), "should find .rs file");
    assert!(!stdout.contains("code.py"), "should filter out .py file");
}

#[test]
fn auto_index_with_files_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("match.rs"), "fn hello() { }\nfn hello_again() { }").unwrap();

    let output = ngi_bin()
        .args(["search", "-l", "hello", "--no-color"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // -l should output each matching file only once
    assert_eq!(lines.len(), 1, "files-only should deduplicate: {stdout}");
    assert!(lines[0].contains("match.rs"));
}

// ============================================================================
// Edge case: --no-index should NOT auto-build
// ============================================================================

#[test]
fn no_index_flag_does_not_auto_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("a.txt"), "findme here").unwrap();

    let output = ngi_bin()
        .args(["search", "--no-index", "findme"])
        .current_dir(root)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("findme"), "should find without index");
    assert!(!root.join(".ngi").exists(), "--no-index should not create .ngi/");
}
