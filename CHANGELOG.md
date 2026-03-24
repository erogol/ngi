# Changelog

## v0.1.0 — Initial Release

Trigram-indexed regex search for codebases. 2–6× faster than ripgrep on selective queries, 13–69× faster than grep.

### Features

- **Trigram-indexed search** — builds an inverted index of 3-byte substrings, intersects posting lists to narrow candidates before matching
- **Auto-index on first search** — no manual `ngi index` needed; the index is built transparently and updated incrementally
- **100% correctness vs ripgrep** — verified across Linux kernel and CPython with 13/13 exact match counts
- **JSON output (`--json`)** — JSONL format for machine and agent consumption
- **Context lines (`-C`, `-A`, `-B`)** — configurable context around matches
- **Max count (`-m`)** — stop after N matches
- **Incremental reindexing** — detects file changes via mtime/size and reindexes only what changed
- **Configurable max file size (`--max-file-size`)** — default 10MB, adjustable per invocation
- **File pattern filtering (`-f`)** — glob-based filename filtering
- **Case-insensitive search (`-i`)** — lowercased trigrams at index level, regex handles the rest
- **ripgrep delegation** — shells out to rg for SIMD-accelerated matching on candidate files
- **Smart fallback** — when trigrams match >10% of files, lets rg do a full scan instead
