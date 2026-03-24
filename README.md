# ngi — N-gram Indexed Regex Search

[![CI](https://github.com/erogol/ngi/actions/workflows/ci.yml/badge.svg)](https://github.com/erogol/ngi/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Made with KeCHe](https://img.shields.io/badge/made_with-KeCHe-8A2BE2)](https://github.com/coqui-ai/keche)

Fast regex search over large codebases. Builds a trigram index to pre-filter files, then delegates matching to [ripgrep](https://github.com/BurntSushi/rg) for SIMD-accelerated regex.

**13–69× faster than grep. 2–6× faster than ripgrep on selective queries.**

## How it works

```
regex pattern
  → extract trigrams from the pattern
  → intersect posting lists (mmap'd binary search)
  → if <10% of files match → pass candidates to rg
  → if >10% of files match → let rg scan everything (its walker is faster)
  → results
```

The index is built once and updated incrementally. First search auto-builds it.

## Benchmarks

### Linux kernel (92,916 files)

| Pattern | ngi | rg | grep | ngi vs rg | ngi vs grep |
|---|---|---|---|---|---|
| `__attribute__.*section` | 27ms | 161ms | 1866ms | **5.9×** | **69×** |
| `dma_alloc_coherent` | 42ms | 161ms | 1646ms | **3.8×** | **39×** |
| `struct file_operations` | 71ms | 160ms | 1642ms | **2.3×** | **23×** |
| `EXPORT_SYMBOL_GPL` | 90ms | 174ms | 1607ms | **1.9×** | **17×** |
| `mutex_lock` | 95ms | 162ms | 1314ms | **1.7×** | **13×** |

Correctness: **100% match with ripgrep** across all tested queries (13/13 exact).

### Index overhead

| Codebase | Files | Index size | Build time |
|---|---|---|---|
| Small project (500 files) | 500 | ~1 MB | <300ms |
| CPython | 5,354 | 13 MB | 1.9s |
| Linux kernel | 92,916 | 146 MB | 16s |

## Install

```sh
cargo install ngi
```

Requires [ripgrep](https://github.com/BurntSushi/rg) on `$PATH` for best performance. Falls back to a built-in Rust regex matcher if rg isn't available.

## Usage

```sh
# Just search — index is built automatically on first run
ngi search 'fn.*parse'

# Explicit index management
ngi index                      # Build/rebuild index
ngi index --force              # Force full rebuild
ngi index --max-file-size 50M  # Include files up to 50MB

# Search options
ngi search 'pattern'              # Regex search
ngi search -i 'pattern'           # Case-insensitive
ngi search -l 'pattern'           # File names only
ngi search -f '\.rs$' 'pattern'   # Filter by file extension
ngi search -C 3 'pattern'         # 3 lines of context around matches
ngi search -A 2 -B 1 'pattern'    # 2 after, 1 before
ngi search -m 10 'pattern'        # Stop after 10 matches
ngi search --json 'pattern'       # JSONL output (machine-readable)
ngi search --json -C 2 -m 5 'fn'  # All flags compose
ngi search --no-index 'pattern'   # Skip index, search all files
ngi search --explain 'pattern'    # Show query plan

# Maintenance
ngi status                     # Show index stats
ngi clean                      # Remove index
```

## How the index works

ngi extracts all 3-byte substrings (trigrams) from every file and builds an inverted index mapping each trigram to the files containing it.

When you search for `fn.*parse`:
1. Extract trigrams from the regex: `fn `, `par`, `ars`, `rse`
2. Look up each trigram's file list in the index (binary search on mmap'd data)
3. Intersect the lists → only files containing ALL trigrams survive
4. Run ripgrep on the survivors for the actual regex match

For the pattern `__attribute__.*section` in the Linux kernel, this narrows 92,916 files down to 526 candidates (0.6%) before rg even starts.

## Auto-index

The first time you run `ngi search` in a project, it automatically builds the index. Subsequent searches detect file changes and incrementally reindex.

The index lives in `.ngi/` at the project root (detected via `.git/`). Add `.ngi/` to your `.gitignore`.

## Ignoring files

ngi respects `.gitignore` and skips common non-source directories (node_modules, \_\_pycache\_\_, .venv, etc.). For custom exclusions, create a `.ngiignore` file with the same syntax as `.gitignore`.

## Machine-readable output

`--json` produces JSONL (one JSON object per line):

```jsonl
{"type":"match","path":"src/main.rs","line_number":42,"line_text":"fn parse()","context_before":["// comment"],"context_after":["  let x = 1;"]}
{"type":"summary","match_count":15,"file_count":3,"files_searched":100,"total_files":5000,"duration_ms":45,"mode":"indexed"}
```

Context arrays are empty unless `-C`/`-A`/`-B` is used. Modes: `indexed`, `rg-fullscan`, `full-scan`, `no-index`.

## Inspired by

[Cursor's blog post on fast code search](https://cursor.com/blog/fast-regex-search) describing sparse n-gram indexing. ngi implements the trigram subset of that approach — sparse n-grams produced 12M+ unique entries vs 181K trigrams for CPython, making the index impractically large for marginal selectivity gain.

## License

MIT
