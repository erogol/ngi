//! Integration tests for agentic/machine-use features:
//! context lines (-C/-A/-B), JSON output (--json), and max count (-m).

use std::fs;
use std::process::Command;

fn ngi_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ngi"))
}

/// Create a temp dir, write files, build the index, return the TempDir.
fn setup(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }
    // Build the index
    let output = ngi_bin()
        .args(["index", "."])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "index failed: {}", String::from_utf8_lossy(&output.stderr));
    dir
}

// ============================================================================
// Context lines (-C/-A/-B)
// ============================================================================

#[test]
fn context_c2_shows_lines_around_match() {
    let dir = setup(&[(
        "f.txt",
        "line1\nline2\nline3\nMATCH\nline5\nline6\nline7\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "-C", "2", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain context before
    assert!(stdout.contains("line2"), "missing before context: {stdout}");
    assert!(stdout.contains("line3"), "missing before context: {stdout}");
    // Should contain match
    assert!(stdout.contains("MATCH"), "missing match: {stdout}");
    // Should contain context after
    assert!(stdout.contains("line5"), "missing after context: {stdout}");
    assert!(stdout.contains("line6"), "missing after context: {stdout}");
}

#[test]
fn context_a1_shows_after_only() {
    let dir = setup(&[(
        "f.txt",
        "before\nMATCH\nafter1\nafter2\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "-A", "1", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("MATCH"), "missing match: {stdout}");
    assert!(stdout.contains("after1"), "missing after context: {stdout}");
    // -A only, so no before context lines
    // (before line won't appear as context — it might appear if it matches, but "before" != "MATCH")
    let lines: Vec<&str> = stdout.lines().collect();
    // Should be match line + 1 after context = 2 lines of file content
    assert!(
        !lines.iter().any(|l| l.contains("-1-before")),
        "should not have before context with -A only: {stdout}"
    );
}

#[test]
fn context_b1_shows_before_only() {
    let dir = setup(&[(
        "f.txt",
        "before1\nbefore2\nMATCH\nafter\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "-B", "1", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("MATCH"), "missing match: {stdout}");
    assert!(stdout.contains("before2"), "missing before context: {stdout}");
    // Should not contain after context
    assert!(
        !lines_with_dash_separator(&stdout).iter().any(|l| l.contains("after")),
        "should not have after context with -B only: {stdout}"
    );
}

/// Collect lines that use the context separator format (path-linenum-text)
fn lines_with_dash_separator(output: &str) -> Vec<String> {
    output.lines()
        .filter(|l| {
            // Context lines have format: path-N-text
            if let Some((_path, rest)) = l.split_once('-') {
                if let Some((num, _text)) = rest.split_once('-') {
                    return num.parse::<u32>().is_ok();
                }
            }
            false
        })
        .map(|l| l.to_string())
        .collect()
}

#[test]
fn context_doesnt_go_past_file_boundaries() {
    let dir = setup(&[(
        "f.txt",
        "MATCH\nafter\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "-C", "5", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // MATCH is on line 1, requesting 5 before — should only get 0 before
    // After: only "after" line available
    assert!(stdout.contains("MATCH"));
    assert!(stdout.contains("after"));
    // Should not panic or produce garbage
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.len() <= 3, "too many lines for small file: {stdout}");
}

#[test]
fn context_adjacent_matches_separated() {
    let dir = setup(&[(
        "f.txt",
        "a\nMATCH1\nb\nMATCH2\nc\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "-C", "1", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Both matches should appear
    assert!(stdout.contains("MATCH1"), "missing MATCH1: {stdout}");
    assert!(stdout.contains("MATCH2"), "missing MATCH2: {stdout}");
    // Group separator should appear between match groups
    assert!(stdout.contains("--"), "missing group separator: {stdout}");
}

// ============================================================================
// JSON output (--json)
// ============================================================================

#[test]
fn json_outputs_valid_jsonl() {
    let dir = setup(&[
        ("a.txt", "hello world\n"),
        ("b.txt", "hello again\n"),
    ]);

    let output = ngi_bin()
        .args(["search", "hello", "--json", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let lines: Vec<&str> = stdout.lines().collect();
    // At least 2 match lines + 1 summary
    assert!(lines.len() >= 3, "expected at least 3 JSON lines, got: {stdout}");

    // Every line should be valid JSON (starts with { and ends with })
    for line in &lines {
        assert!(line.starts_with('{'), "not JSON: {line}");
        assert!(line.ends_with('}'), "not JSON: {line}");
    }

    // Last line should be summary
    let last = lines.last().unwrap();
    assert!(last.contains("\"type\":\"summary\""), "last line should be summary: {last}");
    assert!(last.contains("\"match_count\":"), "summary should have match_count: {last}");
    assert!(last.contains("\"file_count\":"), "summary should have file_count: {last}");
    assert!(last.contains("\"duration_ms\":"), "summary should have duration_ms: {last}");
}

#[test]
fn json_includes_context_arrays() {
    let dir = setup(&[(
        "f.txt",
        "before\nMATCH\nafter\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "--json", "-C", "1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find the match line
    let match_line = stdout.lines().find(|l| l.contains("\"type\":\"match\"")).unwrap();
    assert!(match_line.contains("\"context_before\":["), "missing context_before: {match_line}");
    assert!(match_line.contains("\"context_after\":["), "missing context_after: {match_line}");
    assert!(match_line.contains("before"), "context_before should contain 'before': {match_line}");
    assert!(match_line.contains("after"), "context_after should contain 'after': {match_line}");
}

#[test]
fn json_summary_has_correct_stats() {
    let dir = setup(&[
        ("a.txt", "hello\n"),
        ("b.txt", "hello\nhello\n"),
        ("c.txt", "nothing\n"),
    ]);

    let output = ngi_bin()
        .args(["search", "hello", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let summary = stdout.lines().last().unwrap();
    assert!(summary.contains("\"type\":\"summary\""), "last line should be summary");
    assert!(summary.contains("\"match_count\":3"), "expected 3 matches: {summary}");
    assert!(summary.contains("\"file_count\":2"), "expected 2 files: {summary}");
}

#[test]
fn json_with_files_only() {
    let dir = setup(&[
        ("a.txt", "target\n"),
        ("b.txt", "target\ntarget\n"),
    ]);

    let output = ngi_bin()
        .args(["search", "target", "--json", "-l"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Match lines should have line_number:0 for files-only
    let match_lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("\"type\":\"match\""))
        .collect();
    assert!(match_lines.len() >= 2, "expected at least 2 file matches: {stdout}");
    for ml in &match_lines {
        assert!(ml.contains("\"line_number\":0"), "files-only should have line_number 0: {ml}");
    }
}

// ============================================================================
// Max count (-m)
// ============================================================================

#[test]
fn max_count_1_stops_after_1() {
    let dir = setup(&[
        ("a.txt", "match1\nmatch2\nmatch3\n"),
        ("b.txt", "match4\nmatch5\n"),
    ]);

    let output = ngi_bin()
        .args(["search", "match", "-m", "1", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("match"))
        .collect();
    assert_eq!(match_lines.len(), 1, "expected exactly 1 match with -m 1: {stdout}");
}

#[test]
fn max_count_5_with_many_matches() {
    let dir = setup(&[(
        "big.txt",
        &(0..100).map(|i| format!("line_match_{i}")).collect::<Vec<_>>().join("\n"),
    )]);

    let output = ngi_bin()
        .args(["search", "line_match", "-m", "5", "--no-color"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("line_match"))
        .collect();
    assert_eq!(match_lines.len(), 5, "expected exactly 5 matches with -m 5, got {}: {stdout}", match_lines.len());
}

#[test]
fn max_count_with_json() {
    let dir = setup(&[(
        "f.txt",
        "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n",
    )]);

    let output = ngi_bin()
        .args(["search", "[a-z]", "-m", "3", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("\"type\":\"match\""))
        .collect();
    assert_eq!(match_lines.len(), 3, "expected 3 match lines with -m 3 --json: {stdout}");

    // Summary should reflect truncated count
    let summary = stdout.lines().last().unwrap();
    assert!(summary.contains("\"match_count\":3"), "summary should show 3: {summary}");
}

// ============================================================================
// Combined flags
// ============================================================================

#[test]
fn json_context_and_max_count_together() {
    let dir = setup(&[(
        "f.txt",
        "a\nb\nMATCH1\nc\nd\ne\nMATCH2\nf\ng\nh\nMATCH3\ni\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "--json", "-C", "1", "-m", "2"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("\"type\":\"match\""))
        .collect();
    assert_eq!(match_lines.len(), 2, "expected 2 matches with -m 2: {stdout}");

    // First match should have context
    let first = match_lines[0];
    assert!(first.contains("\"context_before\":["), "should have context_before: {first}");
    assert!(first.contains("\"context_after\":["), "should have context_after: {first}");
}

#[test]
fn c_flag_overridden_by_specific_a_b() {
    // -C 5 -A 1 should give 5 before but only 1 after
    let dir = setup(&[(
        "f.txt",
        "l1\nl2\nl3\nl4\nl5\nMATCH\na1\na2\na3\na4\na5\n",
    )]);

    let output = ngi_bin()
        .args(["search", "MATCH", "--json", "-C", "5", "-A", "1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_line = stdout.lines().find(|l| l.contains("\"type\":\"match\"")).unwrap();
    // context_before should have 5 lines
    assert!(match_line.contains("l1"), "should have l1 in before: {match_line}");
    assert!(match_line.contains("l5"), "should have l5 in before: {match_line}");
    // context_after should have only 1 line (a1), not a2+
    assert!(match_line.contains("a1"), "should have a1 in after: {match_line}");
    assert!(!match_line.contains("a2"), "should NOT have a2 with -A 1: {match_line}");
}

#[test]
fn no_index_with_context_and_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), "before\nTARGET\nafter\n").unwrap();

    let output = ngi_bin()
        .args(["search", "--no-index", "TARGET", "--json", "-C", "1"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_line = stdout.lines().find(|l| l.contains("\"type\":\"match\"")).unwrap();
    assert!(match_line.contains("TARGET"), "should find TARGET: {match_line}");
    assert!(match_line.contains("before"), "should have before context: {match_line}");
    assert!(match_line.contains("after"), "should have after context: {match_line}");

    // Summary present
    let summary = stdout.lines().last().unwrap();
    assert!(summary.contains("\"type\":\"summary\""), "should have summary: {summary}");
}

#[test]
fn index_json_produces_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "hello world\n").unwrap();

    let output = ngi_bin()
        .args(["index", ".", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "index --json failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    // Must be exactly one line
    assert_eq!(stdout.trim().lines().count(), 1, "expected single JSON line: {stdout}");
    // Must have all required fields
    assert!(line.contains("\"type\":\"index\""), "missing type: {line}");
    assert!(line.contains("\"file_count\":"), "missing file_count: {line}");
    assert!(line.contains("\"ngram_count\":"), "missing ngram_count: {line}");
    assert!(line.contains("\"duration_ms\":"), "missing duration_ms: {line}");
    assert!(line.contains("\"index_size\":"), "missing index_size: {line}");
    assert!(line.contains("\"mode\":"), "missing mode: {line}");
    assert!(line.contains("\"max_file_size\":"), "missing max_file_size: {line}");
    // file_count should be >= 1
    assert!(line.contains("\"file_count\":1"), "expected 1 file: {line}");
}

#[test]
fn index_json_incremental_fresh() {
    let dir = setup(&[("a.txt", "content\n")]);

    // Second index should report fresh
    let output = ngi_bin()
        .args(["index", ".", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    assert!(line.contains("\"type\":\"index\""), "missing type: {line}");
    // Mode should be "fresh" since nothing changed
    assert!(line.contains("\"mode\":\"fresh\""), "expected fresh mode: {line}");
}

#[test]
fn status_json_produces_valid_json() {
    let dir = setup(&[("a.txt", "hello world\n")]);

    let output = ngi_bin()
        .args(["status", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "status --json failed: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    assert_eq!(stdout.trim().lines().count(), 1, "expected single JSON line: {stdout}");
    assert!(line.contains("\"type\":\"status\""), "missing type: {line}");
    assert!(line.contains("\"indexed_files\":"), "missing indexed_files: {line}");
    assert!(line.contains("\"ngram_count\":"), "missing ngram_count: {line}");
    assert!(line.contains("\"freshness\":"), "missing freshness: {line}");
    assert!(line.contains("\"changes\":"), "missing changes: {line}");
    // Should be fresh since we just built it
    assert!(line.contains("\"freshness\":\"fresh\""), "expected fresh: {line}");
}

#[test]
fn status_json_no_index() {
    let dir = tempfile::tempdir().unwrap();

    let output = ngi_bin()
        .args(["status", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    assert!(line.contains("\"type\":\"status\""), "missing type: {line}");
    assert!(line.contains("\"freshness\":\"none\""), "expected none freshness: {line}");
    assert!(line.contains("\"indexed_files\":0"), "expected 0 files: {line}");
}

#[test]
fn json_escapes_special_characters() {
    let dir = setup(&[(
        "f.txt",
        "hello \"world\" and\\backslash\n",
    )]);

    let output = ngi_bin()
        .args(["search", "hello", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let match_line = stdout.lines().find(|l| l.contains("\"type\":\"match\"")).unwrap();
    // Quotes and backslashes should be escaped
    assert!(match_line.contains(r#"\\backslash"#), "backslash should be escaped: {match_line}");
    assert!(match_line.contains(r#"\"world\""#), "quotes should be escaped: {match_line}");
}
