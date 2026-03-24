use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;

use ngi::freshness::{check_freshness, collect_file_states, read_git_head};
use ngi::indexer::{build_index, incremental_reindex, DEFAULT_MAX_FILE_SIZE};
use ngi::query::{build_query_plan, explain_plan};
use ngi::search::search;
use ngi::storage::{write_index_with_meta, MappedIndex};
use ngi::walk::build_walker;

#[derive(Parser)]
#[command(name = "ngi", about = "Fast regex search over codebases using sparse n-gram indexes")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build or rebuild the n-gram index
    Index {
        /// Root path to index
        #[arg(default_value = ".")]
        path: String,
        /// Force full re-index
        #[arg(long)]
        force: bool,
        /// Show index statistics without rebuilding
        #[arg(long)]
        stats: bool,
        /// Maximum file size to index (e.g. "10M", "500K", "1M"). Default: 10M
        #[arg(long = "max-file-size", default_value = "10M")]
        max_file_size: String,
        /// Output result as a single JSON line
        #[arg(long = "json")]
        json: bool,
    },
    /// Search the codebase with a regex pattern
    Search {
        /// Regex pattern to search for
        pattern: String,
        /// Case-insensitive search
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        /// Filter by file glob pattern
        #[arg(short = 'f', long = "file")]
        file_pattern: Option<String>,
        /// Print only filenames
        #[arg(short = 'l', long = "files-only")]
        files_only: bool,
        /// Bypass index, grep all files
        #[arg(long = "no-index")]
        no_index: bool,
        /// Show the query plan
        #[arg(long)]
        explain: bool,
        /// Disable colored output
        #[arg(long = "no-color")]
        no_color: bool,
        /// Lines of context around each match (sets both -A and -B)
        #[arg(short = 'C', long = "context", default_value_t = 0)]
        context: usize,
        /// Lines of context after each match
        #[arg(short = 'A', long = "after-context")]
        after_context: Option<usize>,
        /// Lines of context before each match
        #[arg(short = 'B', long = "before-context")]
        before_context: Option<usize>,
        /// Output results as JSON (one object per line, JSONL format)
        #[arg(long = "json")]
        json: bool,
        /// Stop after N total matches
        #[arg(short = 'm', long = "max-count")]
        max_count: Option<usize>,
    },
    /// Show index status
    Status {
        /// Output result as a single JSON line
        #[arg(long = "json")]
        json: bool,
    },
    /// Remove the index
    Clean,
}

/// Walk up from the current directory looking for `.ngi/`.
/// Returns `(index_dir, project_root)`.
fn find_index_dir() -> Option<(PathBuf, PathBuf)> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let ngi = dir.join(".ngi");
        if ngi.is_dir() {
            return Some((ngi, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Find the project root by walking up from cwd looking for `.git/`.
/// Falls back to cwd if no git root is found.
fn find_project_root() -> PathBuf {
    if let Ok(mut dir) = std::env::current_dir() {
        let start = dir.clone();
        loop {
            if dir.join(".git").exists() {
                return dir;
            }
            if !dir.pop() {
                return start;
            }
        }
    }
    PathBuf::from(".")
}

/// Auto-build the index for first-time search.
/// Returns `(index_dir, project_root)`.
fn auto_build_index() -> Result<(PathBuf, PathBuf)> {
    let root = find_project_root();
    let index_dir = root.join(".ngi");

    let start = Instant::now();
    eprintln!("Building index (first run)...");

    let index = build_index(&root, DEFAULT_MAX_FILE_SIZE)?;
    let file_states = collect_file_states(&root)?;
    let git_head = read_git_head(&root);
    write_index_with_meta(&index, &index_dir, &root, Some(&file_states), git_head.as_deref())?;

    let mut total_size: u64 = 0;
    for entry in fs::read_dir(&index_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total_size += entry.metadata()?.len();
        }
    }

    eprintln!(
        "Indexed {} files in {:.0}ms ({})",
        index.stats.file_count,
        start.elapsed().as_secs_f64() * 1000.0,
        format_size(total_size),
    );

    Ok((index_dir, root))
}

/// Read and print stats from meta.json.
fn print_meta_stats(index_dir: &Path) -> Result<()> {
    let meta_path = index_dir.join("meta.json");
    let content = fs::read_to_string(&meta_path).context("failed to read meta.json")?;
    // Parse fields manually (no serde dependency)
    let file_count = extract_json_u64(&content, "file_count");
    let ngram_count = extract_json_u64(&content, "ngram_count");
    let duration_ms = extract_json_u64(&content, "build_duration_ms");

    println!("Index statistics:");
    println!("  files:    {}", file_count.unwrap_or(0));
    println!("  n-grams:  {}", ngram_count.unwrap_or(0));
    println!("  built in: {}ms", duration_ms.unwrap_or(0));

    // Index size on disk
    let mut total_size: u64 = 0;
    for entry in fs::read_dir(index_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total_size += entry.metadata()?.len();
        }
    }
    println!("  size:     {}", format_size(total_size));

    Ok(())
}

fn extract_json_u64(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{}\":", key);
    let pos = json.find(&pattern)?;
    let rest = &json[pos + pattern.len()..];
    let trimmed = rest.trim_start();
    let end = trimmed.find(|c: char| !c.is_ascii_digit())?;
    trimmed[..end].parse().ok()
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Parse a human-readable size string like "10M", "500K", "1048576" into bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size string");
    }
    let (num_part, multiplier) = if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1_048_576u64)
    } else if s.ends_with('K') || s.ends_with('k') {
        (&s[..s.len() - 1], 1_024u64)
    } else if s.ends_with('G') || s.ends_with('g') {
        (&s[..s.len() - 1], 1_073_741_824u64)
    } else {
        (s, 1u64)
    };
    let n: u64 = num_part
        .trim()
        .parse()
        .with_context(|| format!("invalid size: {s}"))?;
    Ok(n * multiplier)
}

/// Try incremental reindex. Returns:
/// - Ok(Some((index, description))) if reindex was performed
/// - Ok(None) if index is fresh
/// - Err if something went wrong
fn try_incremental_reindex(
    root: &Path,
    index_dir: &Path,
    max_file_size: u64,
) -> Result<Option<(ngi::indexer::InMemoryIndex, String)>> {
    let report = check_freshness(index_dir, root)?;
    if report.is_fresh {
        return Ok(None);
    }
    let old_index = MappedIndex::open(index_dir)?;
    let new_index = incremental_reindex(root, index_dir, &report, &old_index, max_file_size)?;
    drop(old_index); // release mmap before caller overwrites files
    let desc = format!(
        "incremental: {} changed, {} new, {} deleted",
        report.changed.len(),
        report.added.len(),
        report.deleted.len(),
    );
    Ok(Some((new_index, desc)))
}

/// Search all files directly without using the index (for --no-index).
fn search_no_index(
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    file_pattern: Option<&str>,
    files_only: bool,
    context_before: usize,
    context_after: usize,
    max_count: Option<usize>,
) -> Result<(Vec<ngi::search::SearchMatch>, usize, std::time::Duration)> {
    let start = Instant::now();

    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .with_context(|| format!("invalid regex pattern: {pattern}"))?;

    let file_re = match file_pattern {
        Some(fp) => Some(regex::Regex::new(fp).context("invalid file pattern")?),
        None => None,
    };

    // Phase 1: Collect file paths sequentially (walker isn't Send)
    let walker = build_walker(root);

    let mut file_paths: Vec<(PathBuf, String)> = Vec::new();

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

        let rel_path = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => continue,
        };

        if let Some(ref fre) = file_re
            && !fre.is_match(&rel_path)
        {
            continue;
        }

        file_paths.push((entry.into_path(), rel_path));
    }

    let files_searched = file_paths.len();
    let need_context = context_before > 0 || context_after > 0;

    // Phase 2: Search files in parallel
    let mut matches: Vec<ngi::search::SearchMatch> = file_paths
        .par_iter()
        .flat_map(|(full_path, rel_path)| {
            let file = match fs::File::open(full_path) {
                Ok(f) => f,
                Err(_) => return vec![],
            };
            let reader = BufReader::new(file);

            if files_only {
                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    if re.is_match(&line) {
                        return vec![ngi::search::SearchMatch {
                            path: rel_path.clone(),
                            line_number: 0,
                            line_text: String::new(),
                            context_before: Vec::new(),
                            context_after: Vec::new(),
                        }];
                    }
                }
                vec![]
            } else if need_context {
                // Read all lines for context collection
                let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
                let mut file_matches = Vec::new();
                for (idx, line) in lines.iter().enumerate() {
                    if re.is_match(line) {
                        let before_start = idx.saturating_sub(context_before);
                        let before: Vec<String> = lines[before_start..idx].to_vec();
                        let after_end = (idx + 1 + context_after).min(lines.len());
                        let after: Vec<String> = lines[idx + 1..after_end].to_vec();
                        file_matches.push(ngi::search::SearchMatch {
                            path: rel_path.clone(),
                            line_number: (idx + 1) as u32,
                            line_text: line.clone(),
                            context_before: before,
                            context_after: after,
                        });
                    }
                }
                file_matches
            } else {
                let mut file_matches = Vec::new();
                for (idx, line) in reader.lines().enumerate() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                    if re.is_match(&line) {
                        file_matches.push(ngi::search::SearchMatch {
                            path: rel_path.clone(),
                            line_number: (idx + 1) as u32,
                            line_text: line,
                            context_before: Vec::new(),
                            context_after: Vec::new(),
                        });
                    }
                }
                file_matches
            }
        })
        .collect();

    // Sort for deterministic output order
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));

    // Apply max_count truncation
    if let Some(mc) = max_count {
        matches.truncate(mc);
    }

    Ok((matches, files_searched, start.elapsed()))
}

// ANSI color codes
const MAGENTA_BOLD: &str = "\x1b[1;35m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const RED_BOLD: &str = "\x1b[1;31m";
const RESET: &str = "\x1b[0m";

/// Format a search match line with ANSI colors.
/// Highlights: file path (magenta bold), line number (green), separator (cyan),
/// matched portions (red bold).
fn format_match_colored(m: &ngi::search::SearchMatch, re: &regex::Regex, files_only: bool) -> String {
    if files_only {
        return format!("{MAGENTA_BOLD}{}{RESET}", m.path);
    }
    // Highlight match positions in the line text
    let highlighted = highlight_matches(&m.line_text, re);
    format!(
        "{MAGENTA_BOLD}{}{RESET}{CYAN}:{RESET}{GREEN}{}{RESET}{CYAN}:{RESET}{}",
        m.path, m.line_number, highlighted
    )
}

/// Wrap regex matches in a line with red bold ANSI escapes.
fn highlight_matches(line: &str, re: &regex::Regex) -> String {
    let mut result = String::with_capacity(line.len() + 64);
    let mut last_end = 0;
    for mat in re.find_iter(line) {
        result.push_str(&line[last_end..mat.start()]);
        result.push_str(RED_BOLD);
        result.push_str(mat.as_str());
        result.push_str(RESET);
        last_end = mat.end();
    }
    result.push_str(&line[last_end..]);
    result
}

/// Print search results to stdout (plain or colored).
fn print_results(
    matches: &[ngi::search::SearchMatch],
    pattern: &str,
    ignore_case: bool,
    files_only: bool,
    use_color: bool,
    has_context: bool,
) {
    if use_color {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .unwrap_or_else(|_| regex::Regex::new("$.").unwrap());
        let mut first = true;
        for m in matches {
            if has_context && !first {
                println!("--");
            }
            first = false;

            // Context before
            if has_context {
                let start_line = m.line_number.saturating_sub(m.context_before.len() as u32);
                for (i, ctx_line) in m.context_before.iter().enumerate() {
                    println!(
                        "{MAGENTA_BOLD}{}{RESET}{CYAN}-{RESET}{GREEN}{}{RESET}{CYAN}-{RESET}{}",
                        m.path, start_line + i as u32, ctx_line,
                    );
                }
            }

            // Match line
            println!("{}", format_match_colored(m, &re, files_only));

            // Context after
            if has_context {
                for (i, ctx_line) in m.context_after.iter().enumerate() {
                    println!(
                        "{MAGENTA_BOLD}{}{RESET}{CYAN}-{RESET}{GREEN}{}{RESET}{CYAN}-{RESET}{}",
                        m.path, m.line_number + 1 + i as u32, ctx_line,
                    );
                }
            }
        }
    } else {
        let mut first = true;
        for m in matches {
            if has_context && !first {
                println!("--");
            }
            first = false;

            if files_only {
                println!("{}", m.path);
                continue;
            }

            // Context before
            if has_context {
                let start_line = m.line_number.saturating_sub(m.context_before.len() as u32);
                for (i, ctx_line) in m.context_before.iter().enumerate() {
                    println!("{}-{}-{}", m.path, start_line + i as u32, ctx_line);
                }
            }

            // Match line
            println!("{}:{}:{}", m.path, m.line_number, m.line_text);

            // Context after
            if has_context {
                for (i, ctx_line) in m.context_after.iter().enumerate() {
                    println!("{}-{}-{}", m.path, m.line_number + 1 + i as u32, ctx_line);
                }
            }
        }
    }
}

/// Escape a string for JSON output.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                // Control characters
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Format a JSON array of strings.
fn json_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(item));
        out.push('"');
    }
    out.push(']');
    out
}

/// Print search results as JSONL.
fn print_json_results(
    matches: &[ngi::search::SearchMatch],
    match_count: usize,
    file_count: usize,
    files_searched: usize,
    total_files: usize,
    duration: std::time::Duration,
    mode: &str,
) {
    for m in matches {
        println!(
            "{{\"type\":\"match\",\"path\":\"{}\",\"line_number\":{},\"line_text\":\"{}\",\"context_before\":{},\"context_after\":{}}}",
            json_escape(&m.path),
            m.line_number,
            json_escape(&m.line_text),
            json_string_array(&m.context_before),
            json_string_array(&m.context_after),
        );
    }
    // Summary line
    println!(
        "{{\"type\":\"summary\",\"match_count\":{},\"file_count\":{},\"files_searched\":{},\"total_files\":{},\"duration_ms\":{},\"mode\":\"{}\"}}",
        match_count,
        file_count,
        files_searched,
        total_files,
        duration.as_millis(),
        json_escape(mode),
    );
}

fn run() -> Result<bool> {
    let cli = Cli::parse();

    match cli.command {
        Command::Index { path, force, stats, max_file_size, json } => {
            let max_size = parse_size(&max_file_size)
                .with_context(|| format!("invalid --max-file-size value: {max_file_size}"))?;
            let root = fs::canonicalize(&path)
                .with_context(|| format!("path not found: {path}"))?;
            let index_dir = root.join(".ngi");

            // --stats: just print existing stats without rebuilding
            if stats {
                if index_dir.is_dir() {
                    print_meta_stats(&index_dir)?;
                } else {
                    bail!("no index found at {}", index_dir.display());
                }
                return Ok(true);
            }

            // --force: remove existing index first
            if force && index_dir.is_dir() {
                fs::remove_dir_all(&index_dir)
                    .context("failed to remove existing .ngi/ directory")?;
            }

            let git_head = read_git_head(&root);
            let start = Instant::now();

            // Try incremental reindex if index exists and has filemeta
            let (index, mode) = if !force && index_dir.is_dir() && index_dir.join("filemeta.bin").exists() {
                match try_incremental_reindex(&root, &index_dir, max_size) {
                    Ok(Some((idx, report_summary))) => (idx, report_summary),
                    Ok(None) => {
                        // Fresh, nothing to do — print and exit
                        let elapsed = start.elapsed();
                        if json {
                            // Read existing stats from meta.json for the JSON output
                            let meta_content = fs::read_to_string(index_dir.join("meta.json")).unwrap_or_default();
                            let fc = extract_json_u64(&meta_content, "file_count").unwrap_or(0);
                            let nc = extract_json_u64(&meta_content, "ngram_count").unwrap_or(0);
                            let mut sz: u64 = 0;
                            if let Ok(entries) = fs::read_dir(&index_dir) {
                                for entry in entries.flatten() {
                                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                                        sz += entry.metadata().map(|m| m.len()).unwrap_or(0);
                                    }
                                }
                            }
                            println!(
                                "{{\"type\":\"index\",\"file_count\":{},\"ngram_count\":{},\"duration_ms\":{},\"index_size\":\"{}\",\"mode\":\"fresh\",\"max_file_size\":\"{}\"}}",
                                fc, nc, elapsed.as_millis(), json_escape(&format_size(sz)), json_escape(&format_size(max_size)),
                            );
                        } else {
                            println!("Index is fresh (took {:.0}ms)", elapsed.as_secs_f64() * 1000.0);
                        }
                        return Ok(true);
                    }
                    Err(_) => {
                        // Fall back to full rebuild
                        let idx = build_index(&root, max_size)?;
                        (idx, "full rebuild (incremental failed)".to_string())
                    }
                }
            } else {
                let idx = build_index(&root, max_size)?;
                (idx, "full rebuild".to_string())
            };

            let file_states = collect_file_states(&root)?;
            write_index_with_meta(
                &index,
                &index_dir,
                &root,
                Some(&file_states),
                git_head.as_deref(),
            )?;
            let elapsed = start.elapsed();

            // Calculate index size on disk
            let mut total_size: u64 = 0;
            for entry in fs::read_dir(&index_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    total_size += entry.metadata()?.len();
                }
            }

            if json {
                println!(
                    "{{\"type\":\"index\",\"file_count\":{},\"ngram_count\":{},\"duration_ms\":{},\"index_size\":\"{}\",\"mode\":\"{}\",\"max_file_size\":\"{}\"}}",
                    index.stats.file_count,
                    index.stats.ngram_count,
                    elapsed.as_millis(),
                    json_escape(&format_size(total_size)),
                    json_escape(&mode),
                    json_escape(&format_size(max_size)),
                );
            } else {
                println!(
                    "Indexed {} files, {} unique n-grams in {:.0}ms ({}) [{}] (max file size: {})",
                    index.stats.file_count,
                    index.stats.ngram_count,
                    elapsed.as_secs_f64() * 1000.0,
                    format_size(total_size),
                    mode,
                    format_size(max_size),
                );
            }

            if index.stats.skipped_errors > 0 {
                eprintln!(
                    "{} files skipped due to read errors",
                    index.stats.skipped_errors,
                );
            }

            Ok(true)
        }

        Command::Search {
            pattern,
            ignore_case,
            file_pattern,
            files_only,
            no_index,
            explain,
            no_color,
            context,
            after_context,
            before_context,
            json,
            max_count,
        } => {
            // -C sets both, but specific -A/-B flags win
            let ctx_before = before_context.unwrap_or(context);
            let ctx_after = after_context.unwrap_or(context);

            let use_color = !no_color && !json && std::io::IsTerminal::is_terminal(&std::io::stdout());
            if explain {
                let plan = build_query_plan(&pattern, ignore_case)?;
                println!("{}", explain_plan(&plan));
                return Ok(true);
            }

            if no_index {
                // Find project root (from .ngi/ or just use cwd)
                let root = match find_index_dir() {
                    Some((_, root)) => root,
                    None => std::env::current_dir()?,
                };

                let (matches, files_searched, duration) =
                    search_no_index(
                        &root, &pattern, ignore_case, file_pattern.as_deref(), files_only,
                        ctx_before, ctx_after, max_count,
                    )?;

                let match_count = matches.len();
                let file_count = count_unique_files(&matches);

                if json {
                    print_json_results(
                        &matches, match_count, file_count, files_searched,
                        files_searched, duration, "no-index",
                    );
                } else {
                    print_results(
                        &matches, &pattern, ignore_case, files_only,
                        use_color, ctx_before > 0 || ctx_after > 0,
                    );
                }

                eprintln!(
                    "{} matches in {} files (no index, walked {} files, took {:.0}ms)",
                    match_count,
                    file_count,
                    files_searched,
                    duration.as_secs_f64() * 1000.0,
                );

                return Ok(match_count > 0);
            }

            // Indexed search — auto-build index if missing
            let (index_dir, root) = match find_index_dir() {
                Some(found) => found,
                None => auto_build_index()?,
            };

            // Auto-reindex if stale
            if index_dir.join("filemeta.bin").exists()
                && let Ok(report) = check_freshness(&index_dir, &root)
                && !report.is_fresh
            {
                let n_changes = report.changed.len() + report.added.len() + report.deleted.len();
                eprintln!("Auto-reindexing ({} files changed)...", n_changes);
                let old_index = MappedIndex::open(&index_dir)?;
                let new_index = incremental_reindex(&root, &index_dir, &report, &old_index, DEFAULT_MAX_FILE_SIZE)?;
                drop(old_index); // release mmap before overwriting
                let file_states = collect_file_states(&root)?;
                let git_head = read_git_head(&root);
                write_index_with_meta(
                    &new_index,
                    &index_dir,
                    &root,
                    Some(&file_states),
                    git_head.as_deref(),
                )?;
            }

            let result = search(
                &index_dir,
                &root,
                &pattern,
                ignore_case,
                file_pattern.as_deref(),
                files_only,
                ctx_before,
                ctx_after,
                max_count,
            )?;

            let match_count = result.matches.len();
            let file_count = count_unique_files(&result.matches);

            if json {
                let mode = if result.full_scan {
                    "full-scan"
                } else if result.rg_fullscan {
                    "rg-fullscan"
                } else {
                    "indexed"
                };
                print_json_results(
                    &result.matches, match_count, file_count,
                    result.files_searched, result.total_files,
                    result.duration, mode,
                );
            } else {
                print_results(
                    &result.matches, &pattern, ignore_case, files_only,
                    use_color, ctx_before > 0 || ctx_after > 0,
                );
            }

            if result.full_scan {
                eprintln!(
                    "{} matches in {} files ({} files scanned, took {:.0}ms) [full scan]",
                    match_count,
                    file_count,
                    result.total_files,
                    result.duration.as_secs_f64() * 1000.0,
                );
            } else {
                let mode = if result.rg_fullscan { "rg fullscan" } else { "indexed" };
                eprintln!(
                    "{} matches in {} files ({} candidates from {} total, took {:.0}ms) [{}]",
                    match_count,
                    file_count,
                    result.files_searched,
                    result.total_files,
                    result.duration.as_secs_f64() * 1000.0,
                    mode,
                );
            }

            Ok(match_count > 0)
        }

        Command::Status { json } => {
            match find_index_dir() {
                Some((index_dir, root)) => {
                    if json {
                        let meta_content = fs::read_to_string(index_dir.join("meta.json")).unwrap_or_default();
                        let file_count = extract_json_u64(&meta_content, "file_count").unwrap_or(0);
                        let ngram_count = extract_json_u64(&meta_content, "ngram_count").unwrap_or(0);

                        let (freshness, changes) = if index_dir.join("filemeta.bin").exists() {
                            match check_freshness(&index_dir, &root) {
                                Ok(report) if report.is_fresh => ("fresh", 0usize),
                                Ok(report) => ("stale", report.changed.len() + report.added.len() + report.deleted.len()),
                                Err(_) => ("unknown", 0),
                            }
                        } else {
                            ("unknown", 0)
                        };

                        println!(
                            "{{\"type\":\"status\",\"indexed_files\":{},\"ngram_count\":{},\"freshness\":\"{}\",\"changes\":{}}}",
                            file_count, ngram_count, freshness, changes,
                        );
                    } else {
                        println!("Index found at: {}", index_dir.display());
                        println!("Project root:   {}", root.display());
                        print_meta_stats(&index_dir)?;

                        // Freshness check
                        if index_dir.join("filemeta.bin").exists() {
                            match check_freshness(&index_dir, &root) {
                                Ok(report) if report.is_fresh => {
                                    println!("  freshness: index is fresh");
                                }
                                Ok(report) => {
                                    println!(
                                        "  freshness: STALE — {} changed, {} new, {} deleted",
                                        report.changed.len(),
                                        report.added.len(),
                                        report.deleted.len(),
                                    );
                                }
                                Err(e) => {
                                    println!("  freshness: unknown ({})", e);
                                }
                            }
                        } else {
                            println!("  freshness: unknown (no filemeta.bin — run `ngi index` to enable)");
                        }
                    }
                }
                None => {
                    if json {
                        println!("{{\"type\":\"status\",\"indexed_files\":0,\"ngram_count\":0,\"freshness\":\"none\",\"changes\":0}}");
                    } else {
                        println!("No index found. Run `ngi index` to create one.");
                    }
                }
            }
            Ok(true)
        }

        Command::Clean => {
            match find_index_dir() {
                Some((index_dir, _root)) => {
                    fs::remove_dir_all(&index_dir)
                        .context("failed to remove .ngi/ directory")?;
                    println!("Removed {}", index_dir.display());
                }
                None => {
                    println!("No .ngi/ directory found.");
                }
            }
            Ok(true)
        }
    }
}

fn count_unique_files(matches: &[ngi::search::SearchMatch]) -> usize {
    let mut seen = std::collections::HashSet::new();
    for m in matches {
        seen.insert(&m.path);
    }
    seen.len()
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1), // no matches (like grep)
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
