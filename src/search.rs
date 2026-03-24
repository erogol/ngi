//! Search executor: ties index lookup, query plan evaluation, and regex
//! matching together.

use std::cmp::Ordering;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;

use crate::query::{build_query_plan, QueryPlan};
use crate::storage::MappedIndex;

/// A single search match.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: String,
    pub line_number: u32,
    pub line_text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

/// Search result summary.
#[derive(Debug)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub files_searched: usize,
    pub total_files: usize,
    pub duration: Duration,
    /// True if the query plan was FullScan (no index narrowing).
    pub full_scan: bool,
    /// True if rg fullscan was used (candidates exceeded RG_FULLSCAN_RATIO).
    pub rg_fullscan: bool,
}

// ---------------------------------------------------------------------------
// Sorted-set operations on posting lists
// ---------------------------------------------------------------------------

fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

fn union_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => {
                result.push(a[i]);
                i += 1;
            }
            Ordering::Greater => {
                result.push(b[j]);
                j += 1;
            }
            Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

// ---------------------------------------------------------------------------
// Query plan evaluation
// ---------------------------------------------------------------------------

/// Recursively evaluate a query plan against the mmap'd index, returning
/// candidate file IDs.
pub fn evaluate_plan(plan: &QueryPlan, index: &MappedIndex) -> Vec<u32> {
    match plan {
        QueryPlan::Lookup { hash, .. } => {
            index.lookup(*hash).unwrap_or_default()
        }

        QueryPlan::And(subs) => {
            let mut iter = subs.iter();
            let first = match iter.next() {
                Some(p) => evaluate_plan(p, index),
                None => return Vec::new(),
            };
            iter.fold(first, |acc, p| {
                intersect_sorted(&acc, &evaluate_plan(p, index))
            })
        }

        QueryPlan::Or(subs) => {
            let mut iter = subs.iter();
            let first = match iter.next() {
                Some(p) => evaluate_plan(p, index),
                None => return Vec::new(),
            };
            iter.fold(first, |acc, p| {
                union_sorted(&acc, &evaluate_plan(p, index))
            })
        }

        QueryPlan::FullScan => (0..index.file_count() as u32).collect(),
    }
}

// ---------------------------------------------------------------------------
// File-level regex search
// ---------------------------------------------------------------------------

fn search_file(
    path: &Path,
    rel_path: &str,
    re: &Regex,
    context_before: usize,
    context_after: usize,
) -> Vec<SearchMatch> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);

    let need_context = context_before > 0 || context_after > 0;

    if !need_context {
        // Fast path: no context needed, stream line by line
        let mut matches = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if re.is_match(&line) {
                matches.push(SearchMatch {
                    path: rel_path.to_string(),
                    line_number: (idx + 1) as u32,
                    line_text: line,
                    context_before: Vec::new(),
                    context_after: Vec::new(),
                });
            }
        }
        return matches;
    }

    // Context path: read all lines into memory
    let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
    let mut matches = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let before_start = idx.saturating_sub(context_before);
            let before: Vec<String> = lines[before_start..idx].to_vec();

            let after_end = (idx + 1 + context_after).min(lines.len());
            let after: Vec<String> = lines[idx + 1..after_end].to_vec();

            matches.push(SearchMatch {
                path: rel_path.to_string(),
                line_number: (idx + 1) as u32,
                line_text: line.clone(),
                context_before: before,
                context_after: after,
            });
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Rust-native parallel file search (fallback when rg is unavailable)
// ---------------------------------------------------------------------------

fn search_files_parallel(
    root: &Path,
    candidates: &[(u32, String)],
    pattern: &str,
    case_insensitive: bool,
    files_only: bool,
    context_before: usize,
    context_after: usize,
) -> Vec<SearchMatch> {
    let re = match regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    candidates
        .par_iter()
        .flat_map(|(_id, rel_path)| {
            let full_path = root.join(rel_path);
            if files_only {
                let file_matches = search_file(&full_path, rel_path, &re, 0, 0);
                if !file_matches.is_empty() {
                    vec![SearchMatch {
                        path: rel_path.clone(),
                        line_number: 0,
                        line_text: String::new(),
                        context_before: Vec::new(),
                        context_after: Vec::new(),
                    }]
                } else {
                    vec![]
                }
            } else {
                search_file(&full_path, rel_path, &re, context_before, context_after)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ripgrep-accelerated search
// ---------------------------------------------------------------------------

/// Maximum number of candidate files to pass to rg via CLI args.
/// Beyond this, fall back to the Rust-native search to avoid hitting
/// OS argument-length limits.
const RG_MAX_CANDIDATES: usize = 10_000;

/// When candidates exceed this fraction of total files, skip the index
/// and let rg do a full scan with its own walker. rg's filesystem traversal
/// is faster than passing thousands of file args.
const RG_FULLSCAN_RATIO: f64 = 0.10;

/// Check (once) whether `rg` is available on $PATH.
fn rg_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// Delegate candidate matching to `rg` for SIMD-accelerated regex search.
///
/// Passes candidate file paths as positional arguments to `rg -e PATTERN`.
fn search_with_rg(
    root: &Path,
    candidates: &[(u32, String)],
    pattern: &str,
    case_insensitive: bool,
    files_only: bool,
    context_before: usize,
    context_after: usize,
    max_count: Option<usize>,
) -> Result<Vec<SearchMatch>> {
    let need_context = context_before > 0 || context_after > 0;

    let mut cmd = Command::new("rg");

    if need_context {
        // Use --json for structured context parsing
        cmd.arg("--json");
    } else {
        cmd.arg("--no-heading")
            .arg("--with-filename")
            .arg("-n");
    }

    cmd.arg("-e").arg(pattern);

    if case_insensitive {
        cmd.arg("-i");
    }
    if files_only && !need_context {
        cmd.arg("-l");
    }
    if context_before > 0 {
        cmd.arg("-B").arg(context_before.to_string());
    }
    if context_after > 0 {
        cmd.arg("-A").arg(context_after.to_string());
    }
    if let Some(mc) = max_count {
        cmd.arg("-m").arg(mc.to_string());
    }

    for (_, path) in candidates {
        cmd.arg(path);
    }

    cmd.current_dir(root);
    let output = cmd.output().context("failed to run rg")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if need_context {
        parse_rg_json_output(&stdout)
    } else {
        parse_rg_plain_output(&stdout, files_only)
    }
}

/// Parse rg's plain text output (--no-heading --with-filename -n).
fn parse_rg_plain_output(stdout: &str, files_only: bool) -> Result<Vec<SearchMatch>> {
    let mut matches = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if files_only {
            matches.push(SearchMatch {
                path: line.to_string(),
                line_number: 0,
                line_text: String::new(),
                context_before: Vec::new(),
                context_after: Vec::new(),
            });
        } else if let Some((path, rest)) = line.split_once(':') {
            if let Some((num_str, text)) = rest.split_once(':') {
                if let Ok(num) = num_str.parse::<u32>() {
                    matches.push(SearchMatch {
                        path: path.to_string(),
                        line_number: num,
                        line_text: text.to_string(),
                        context_before: Vec::new(),
                        context_after: Vec::new(),
                    });
                }
            }
        }
    }
    Ok(matches)
}

/// Parse rg's --json output to extract matches with context lines.
///
/// rg JSON output has one JSON object per line:
/// - type "match": has path, line_number, lines.text
/// - type "context": has path, line_number, lines.text
/// - type "begin"/"end"/"summary": metadata we skip
///
/// Context lines appear before/after their associated match.
fn parse_rg_json_output(stdout: &str) -> Result<Vec<SearchMatch>> {
    // Collect all events first, then assemble matches with context
    struct RgEvent {
        kind: RgEventKind,
        path: String,
        line_number: u32,
        text: String,
    }
    enum RgEventKind {
        Match,
        Context,
    }

    let mut events: Vec<RgEvent> = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        // Extract "type" field
        let event_type = extract_json_str(line, "type");
        match event_type.as_deref() {
            Some("match") | Some("context") => {}
            _ => continue,
        }

        let is_match = event_type.as_deref() == Some("match");

        // Extract path from "path":{"text":"..."}
        let path = extract_rg_json_path(line).unwrap_or_default();
        // Extract line_number
        let line_number = extract_json_u64_field(line, "line_number").unwrap_or(0) as u32;
        // Extract text from "lines":{"text":"..."}
        let text = extract_rg_json_lines_text(line).unwrap_or_default();
        // rg includes trailing newline in text
        let text = text.trim_end_matches('\n').to_string();

        events.push(RgEvent {
            kind: if is_match {
                RgEventKind::Match
            } else {
                RgEventKind::Context
            },
            path,
            line_number,
            text,
        });
    }

    // Now assemble: for each match, gather surrounding context events
    let mut matches = Vec::new();
    for (i, event) in events.iter().enumerate() {
        if !matches!(event.kind, RgEventKind::Match) {
            continue;
        }

        // Collect context_before: walk backwards from i
        let mut before = Vec::new();
        for j in (0..i).rev() {
            if !matches!(events[j].kind, RgEventKind::Context) {
                break;
            }
            if events[j].path != event.path {
                break;
            }
            before.push(events[j].text.clone());
        }
        before.reverse();

        // Collect context_after: walk forward from i
        let mut after = Vec::new();
        for j in (i + 1)..events.len() {
            if !matches!(events[j].kind, RgEventKind::Context) {
                break;
            }
            if events[j].path != event.path {
                break;
            }
            after.push(events[j].text.clone());
        }

        matches.push(SearchMatch {
            path: event.path.clone(),
            line_number: event.line_number,
            line_text: event.text.clone(),
            context_before: before,
            context_after: after,
        });
    }

    Ok(matches)
}

/// Extract a string value for a top-level key like "type":"match"
fn extract_json_str(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let pos = json.find(&pattern)?;
    let rest = &json[pos + pattern.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract numeric value for a key like "line_number":42
fn extract_json_u64_field(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{}\":", key);
    let pos = json.find(&pattern)?;
    let rest = &json[pos + pattern.len()..];
    let trimmed = rest.trim_start();
    let end = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    trimmed[..end].parse().ok()
}

/// Extract path from rg JSON "path":{"text":"..."} structure
fn extract_rg_json_path(json: &str) -> Option<String> {
    let marker = "\"path\":{\"text\":\"";
    let pos = json.find(marker)?;
    let rest = &json[pos + marker.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract text from rg JSON "lines":{"text":"..."} structure
fn extract_rg_json_lines_text(json: &str) -> Option<String> {
    let marker = "\"lines\":{\"text\":\"";
    let pos = json.find(marker)?;
    let rest = &json[pos + marker.len()..];
    // Need to handle escaped characters in the text
    let mut result = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next() {
            Some('\\') => match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => break,
            },
            Some('"') => break,
            Some(c) => result.push(c),
            None => break,
        }
    }
    Some(result)
}

/// Let rg scan the full project directory (no file args).
/// Used when the index indicates most files match — rg's walker is faster
/// than passing thousands of paths as CLI arguments.
fn search_with_rg_fullscan(
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    files_only: bool,
    context_before: usize,
    context_after: usize,
    max_count: Option<usize>,
) -> Result<Vec<SearchMatch>> {
    let need_context = context_before > 0 || context_after > 0;

    let mut cmd = Command::new("rg");

    if need_context {
        cmd.arg("--json");
    } else {
        cmd.arg("--no-heading")
            .arg("--with-filename")
            .arg("-n");
    }

    cmd.arg("-e").arg(pattern);

    if case_insensitive {
        cmd.arg("-i");
    }
    if files_only && !need_context {
        cmd.arg("-l");
    }
    if context_before > 0 {
        cmd.arg("-B").arg(context_before.to_string());
    }
    if context_after > 0 {
        cmd.arg("-A").arg(context_after.to_string());
    }
    if let Some(mc) = max_count {
        cmd.arg("-m").arg(mc.to_string());
    }

    cmd.current_dir(root);

    let output = cmd.output().context("failed to run rg")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if need_context {
        parse_rg_json_output(&stdout)
    } else {
        parse_rg_plain_output(&stdout, files_only)
    }
}

// ---------------------------------------------------------------------------
// Main search entry point
// ---------------------------------------------------------------------------

/// Run a regex search over the indexed codebase.
///
/// - `index_dir`: path to the `.ngi/` directory
/// - `root`: project root (for resolving file paths)
/// - `pattern`: regex pattern
/// - `case_insensitive`: compile regex with case-insensitive flag
/// - `file_pattern`: optional regex to filter candidate file paths
/// - `files_only`: if true, collect only file paths (no line content)
/// - `context_before`: lines of context before each match
/// - `context_after`: lines of context after each match
/// - `max_count`: stop after this many matches (None = unlimited)
pub fn search(
    index_dir: &Path,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    file_pattern: Option<&str>,
    files_only: bool,
    context_before: usize,
    context_after: usize,
    max_count: Option<usize>,
) -> Result<SearchResult> {
    let start = Instant::now();

    let index = MappedIndex::open(index_dir).context("failed to open index")?;
    let total_files = index.file_count();

    // Build and evaluate query plan
    let plan = build_query_plan(pattern, case_insensitive)?;
    let full_scan = matches!(plan, QueryPlan::FullScan);
    let candidate_ids = evaluate_plan(&plan, &index);

    // Resolve candidate IDs to paths, applying file_pattern filter
    let file_re = match file_pattern {
        Some(fp) => Some(Regex::new(fp).context("invalid file pattern")?),
        None => None,
    };

    let candidates: Vec<(u32, String)> = candidate_ids
        .iter()
        .filter_map(|&id| {
            let path = index.file_path(id)?;
            if let Some(ref fre) = file_re
                && !fre.is_match(path)
            {
                return None;
            }
            Some((id, path.to_string()))
        })
        .collect();

    let files_searched = candidates.len();

    // If candidates > 15% of total files, rg's own walker is faster than
    // passing thousands of file paths as args
    let use_rg_fullscan = rg_available()
        && !candidates.is_empty()
        && (candidates.len() as f64 / total_files.max(1) as f64) > RG_FULLSCAN_RATIO;

    let mut matches: Vec<SearchMatch> = if use_rg_fullscan {
        // Let rg scan the whole project with its own walker
        let mut rg_matches = search_with_rg_fullscan(
            root, pattern, case_insensitive, files_only,
            context_before, context_after, max_count,
        )
        .unwrap_or_else(|_| search_files_parallel(
            root, &candidates, pattern, case_insensitive, files_only,
            context_before, context_after,
        ));
        // Apply file pattern filter since rg fullscan searches all files
        if let Some(ref fre) = file_re {
            rg_matches.retain(|m| fre.is_match(&m.path));
        }
        rg_matches
    } else if rg_available() && !candidates.is_empty() && candidates.len() <= RG_MAX_CANDIDATES {
        search_with_rg(
            root, &candidates, pattern, case_insensitive, files_only,
            context_before, context_after, max_count,
        )
        .unwrap_or_else(|_| {
            // rg failed (e.g. unsupported syntax), fall back to Rust regex
            search_files_parallel(
                root, &candidates, pattern, case_insensitive, files_only,
                context_before, context_after,
            )
        })
    } else {
        search_files_parallel(
            root, &candidates, pattern, case_insensitive, files_only,
            context_before, context_after,
        )
    };

    // Sort for deterministic output order
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));

    // Apply max_count truncation (rg handles it per-file, so we apply globally)
    if let Some(mc) = max_count {
        matches.truncate(mc);
    }

    Ok(SearchResult {
        matches,
        files_searched,
        total_files,
        duration: start.elapsed(),
        full_scan,
        rg_fullscan: use_rg_fullscan,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::build_index;
    use crate::storage::write_index;
    use std::fs;
    use tempfile::TempDir;

    /// Create a temp dir with files, build + write index, return (dir, index_dir).
    fn setup_test_dir(files: &[(&str, &str)]) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        let index = build_index(dir.path(), crate::indexer::DEFAULT_MAX_FILE_SIZE).unwrap();
        let index_dir = dir.path().join(".ngi");
        write_index(&index, &index_dir, dir.path()).unwrap();
        (dir, index_dir)
    }

    // 1. End-to-end search (use short pattern → FullScan to test regex matching)
    #[test]
    fn end_to_end_search() {
        let (dir, index_dir) = setup_test_dir(&[
            ("src/main.rs", "fn main() {\n    println!(\"hello world\");\n}\n"),
            ("src/lib.rs", "pub fn greet() -> &'static str {\n    \"hello\"\n}\n"),
            ("readme.txt", "This is a readme file.\n"),
        ]);

        // "hello" uses index narrowing via n-gram lookups
        let result = search(&index_dir, dir.path(), "hello", false, None, false, 0, 0, None).unwrap();
        assert!(result.matches.len() >= 2, "expected at least 2 matches for 'hello', got {}", result.matches.len());
        assert!(result.matches.iter().any(|m| m.path.contains("main.rs")));
        assert!(result.matches.iter().any(|m| m.path.contains("lib.rs")));
        assert_eq!(result.total_files, 3);

        // "h.llo" extracts literal "llo" — narrows candidates to files containing "llo"
        let result_fs = search(&index_dir, dir.path(), "h.llo", false, None, false, 0, 0, None).unwrap();
        assert!(result_fs.matches.len() >= 2, "expected at least 2 matches, got {}", result_fs.matches.len());
        assert!(result_fs.matches.iter().any(|m| m.path.contains("main.rs")));
        assert!(result_fs.matches.iter().any(|m| m.path.contains("lib.rs")));
        assert_eq!(result_fs.total_files, 3);
    }

    // 2. Intersection correctness
    #[test]
    fn intersect_sorted_basic() {
        assert_eq!(intersect_sorted(&[1, 3, 5, 7], &[2, 3, 5, 8]), vec![3, 5]);
        assert_eq!(intersect_sorted(&[1, 2, 3], &[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(intersect_sorted(&[1, 2], &[3, 4]), Vec::<u32>::new());
        assert_eq!(intersect_sorted(&[], &[1, 2, 3]), Vec::<u32>::new());
        assert_eq!(intersect_sorted(&[1, 2, 3], &[]), Vec::<u32>::new());
    }

    // 3. Union correctness
    #[test]
    fn union_sorted_basic() {
        assert_eq!(union_sorted(&[1, 3, 5], &[2, 4, 6]), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(union_sorted(&[1, 2, 3], &[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(union_sorted(&[1, 3], &[2]), vec![1, 2, 3]);
        assert_eq!(union_sorted(&[], &[1, 2]), vec![1, 2]);
        assert_eq!(union_sorted(&[1, 2], &[]), vec![1, 2]);
    }

    // 4. File pattern filter (use FullScan-triggering regex)
    #[test]
    fn file_pattern_filter() {
        let (dir, index_dir) = setup_test_dir(&[
            ("src/main.rs", "fn main() { hello(); }\n"),
            ("src/lib.rs", "pub fn hello() {}\n"),
            ("docs/guide.txt", "hello world guide\n"),
        ]);

        let result = search(
            &index_dir,
            dir.path(),
            "h.llo",
            false,
            Some(r"\.rs$"),
            false,
            0, 0, None,
        )
        .unwrap();

        // Only .rs files should be searched
        for m in &result.matches {
            assert!(m.path.ends_with(".rs"), "unexpected file: {}", m.path);
        }
        assert!(!result.matches.is_empty());
    }

    // 5. Case insensitive
    #[test]
    fn case_insensitive_search() {
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "Hello World\n"),
            ("b.txt", "HELLO WORLD\n"),
            ("c.txt", "hello world\n"),
        ]);

        let result = search(&index_dir, dir.path(), "hello world", true, None, false, 0, 0, None).unwrap();
        assert!(
            result.matches.len() >= 3,
            "expected at least 3 case-insensitive matches, got {}",
            result.matches.len()
        );
    }

    // 6. FullScan — pattern with no extractable literals
    #[test]
    fn fullscan_pattern() {
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "foo 123 bar\n"),
            ("b.txt", "baz 456 qux\n"),
        ]);

        // [0-9]+ has no extractable literals → FullScan
        let result = search(&index_dir, dir.path(), "[0-9]+", false, None, false, 0, 0, None).unwrap();
        assert_eq!(result.files_searched, 2, "FullScan should search all files");
        assert!(result.matches.len() >= 2);
    }

    // 7. files_only mode
    #[test]
    fn files_only_mode() {
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "hello from a\n"),
            ("b.txt", "hello from b\n"),
            ("c.txt", "nothing here\n"),
        ]);

        let result = search(&index_dir, dir.path(), "hello", false, None, true, 0, 0, None).unwrap();
        assert!(result.matches.len() >= 2);
        for m in &result.matches {
            assert_eq!(m.line_number, 0, "files_only should have line_number=0");
            assert!(m.line_text.is_empty(), "files_only should have empty line_text");
        }
    }

    // 8. No matches
    #[test]
    fn no_matches() {
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "foo bar baz\n"),
            ("b.txt", "qux quux corge\n"),
        ]);

        let result = search(
            &index_dir,
            dir.path(),
            "zzz_nonexistent_pattern_zzz",
            false,
            None,
            false,
            0, 0, None,
        )
        .unwrap();
        assert!(result.matches.is_empty());
    }

    // evaluate_plan unit tests
    #[test]
    fn evaluate_fullscan() {
        let files = &[("a.txt", "content"), ("b.txt", "content")];
        let (dir, index_dir) = setup_test_dir(files);
        let index = MappedIndex::open(&index_dir).unwrap();

        let ids = evaluate_plan(&QueryPlan::FullScan, &index);
        assert_eq!(ids.len(), index.file_count());

        // Drop dir explicitly so tmp dir doesn't get cleaned up too early
        drop(dir);
    }

    #[test]
    fn evaluate_or_plan() {
        // OR of two lookups that don't exist should yield empty
        let (_dir, index_dir) = setup_test_dir(&[("a.txt", "some text content")]);
        let index = MappedIndex::open(&index_dir).unwrap();

        let plan = QueryPlan::Or(vec![
            QueryPlan::Lookup { hash: 0xDEAD, trigram: vec![] },
            QueryPlan::Lookup { hash: 0xBEEF, trigram: vec![] },
        ]);
        let ids = evaluate_plan(&plan, &index);
        assert!(ids.is_empty());
    }

    #[test]
    fn evaluate_and_empty_returns_empty() {
        let (_dir, index_dir) = setup_test_dir(&[("a.txt", "some text content")]);
        let index = MappedIndex::open(&index_dir).unwrap();

        let plan = QueryPlan::And(vec![]);
        let ids = evaluate_plan(&plan, &index);
        assert!(ids.is_empty());
    }

    #[test]
    fn search_still_works_end_to_end() {
        // Multiple files, some matching, some not
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "fn parse_token_stream() {}\n"),
            ("b.txt", "fn compile_ast_node() {}\n"),
            ("c.txt", "fn parse_token_stream() { more stuff }\n"),
        ]);

        let result = search(&index_dir, dir.path(), "parse_token_stream", false, None, false, 0, 0, None).unwrap();
        assert!(result.matches.len() >= 2);
        for m in &result.matches {
            assert!(m.path == "a.txt" || m.path == "c.txt", "unexpected match in {}", m.path);
        }
    }

    #[test]
    fn case_insensitive_end_to_end() {
        let (dir, index_dir) = setup_test_dir(&[
            ("a.txt", "Hello World From Here\n"),
            ("b.txt", "hello world from here\n"),
        ]);

        let result = search(&index_dir, dir.path(), "hello world", true, None, false, 0, 0, None).unwrap();
        assert!(result.matches.len() >= 2, "case-insensitive search should find both");
    }
}

