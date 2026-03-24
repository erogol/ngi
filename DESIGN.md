# ngi — N-Gram Indexed Search

Fast regex search over codebases using sparse n-gram inverted indexes.

**Problem:** `ripgrep` is fast, but it scans every file. On large repos (100k+ files), regex searches take 5-15+ seconds. Agents grep constantly — this is the bottleneck.

**Solution:** Pre-build a sparse n-gram index. On search, decompose the regex into n-gram queries, look up candidate files from the index, then run `rg` only on those candidates. Typical speedup: 10-100x.

**Based on:** [Cursor's blog post](https://cursor.com/blog/fast-regex-search), Russ Cox's trigram index (Google Code Search), GitHub's Blackbird, ClickHouse sparse n-grams.

---

## Architecture

```
┌──────────────┐     ┌─────────────────┐     ┌──────────────┐
│  ngi index   │────▶│  .ngi/ on disk   │◀────│  ngi search  │
│  (build)     │     │  lookup + posts  │     │  (query)     │
└──────────────┘     └─────────────────┘     └──────┬───────┘
                                                     │
                                              candidate files
                                                     │
                                                     ▼
                                              ┌──────────────┐
                                              │  rg --files   │
                                              │  (final match)│
                                              └──────────────┘
```

---

## Core Algorithm: Sparse N-Grams

### Weight Function

No frequency table needed. Weight of a character pair is simply `crc32c(a, b)`:

```rust
fn weight(a: u8, b: u8) -> u32 {
    crc32c(&[a, b])
}
```

This creates a deterministic pseudo-random weight landscape over any string.

### Extracting Sparse N-Grams (`build_all` — Indexing)

For each string, find all maximal substrings where boundary weights exceed all interior weights. Uses a **monotonic stack**:

```
Input: "example_config"

Weights between chars:  e-x  x-a  a-m  m-p  p-l  l-e  e-_  _-c  c-o  o-n  n-f  f-i  i-g
                        482  127  891  203  67   340  912  156  445  88   723  201  559

Sparse n-grams extracted at valleys between peaks:
  "exa" (peak 482, valley 127, peak 891)
  "ampl" (peak 891, valleys 203/67, peak 340)  
  "le_c" (peak 340→912→156, split at peak)
  etc.
```

**Algorithm (monotonic stack):**

```
function build_all(text, min_len=3, max_len=8):
    weights = [weight(text[i], text[i+1]) for i in 0..len-1]
    grams = []
    stack = []  // monotonic decreasing stack of (weight, position)
    
    for i, w in enumerate(weights):
        while stack is not empty and stack.top().weight <= w:
            // Found a valley — emit n-gram from previous peak to current peak
            valley = stack.pop()
            left = stack.top().position if stack else 0
            right = i + 2  // include char after current weight
            gram = text[left..right]
            if min_len <= len(gram) <= max_len:
                grams.append(gram)
        stack.push((w, i))
    
    // Drain remaining stack
    // ... handle remaining valleys
    
    return deduplicate(grams)
```

Produces ≤ 2n-2 grams for n characters. In practice, much fewer after dedup.

### Query Decomposition (`build_covering` — Searching)

Given a literal string from the regex, find the **minimum covering set** of n-grams:

```
function build_covering(text, min_len=3):
    weights = [weight(text[i], text[i+1]) for i in 0..len-1]
    covering = []
    deque = []  // sliding window minimum
    pos = 0
    
    while pos < len(text):
        // Find next valley (local minimum weight)
        // Extend n-gram boundaries to include surrounding peaks
        // Add to covering set
        // Advance pos past covered region
    
    return covering
```

For "chester" → `["chest", "ster"]` instead of 5 trigrams. Fewer lookups, same precision.

---

## Regex → N-Gram Query

Use `regex-syntax` crate to parse regex into HIR, then extract literal strings:

```rust
use regex_syntax::hir::literal::{Extractor, ExtractKind};

let hir = regex_syntax::parse(r"fn\s+parse_(\w+)_token").unwrap();
let literals = Extractor::new().extract(&hir);
// → ["fn"] (prefix before \s+), ["parse_"] (between known literals), ["_token"] (suffix)
```

Then apply `build_covering` to each literal to get sparse n-gram queries.

**Query algebra (from Russ Cox):**

| Regex | N-gram query |
|---|---|
| `abc` | AND(covering("abc")) |
| `abc\|def` | OR(AND(covering("abc")), AND(covering("def"))) |
| `ab.cd` | AND(covering("ab"), covering("cd")) |
| `[ab]cd` | OR(AND(covering("acd")), AND(covering("bcd"))) |
| `.*` | ANY (no filtering possible) |

If the regex has no extractable literals → fall back to full `rg` scan.

---

## Storage Format

Index stored in `.ngi/` directory:

### File 1: `lookup.ngi` (mmap'd for search)

Sorted array of fixed-size entries, binary-searchable:

```
Header (32 bytes):
  magic: [u8; 4]     = b"NGI\x01"
  version: u32        = 1
  num_entries: u64     
  num_files: u64
  reserved: [u8; 8]

Entry (16 bytes each):
  hash: u64           // hash of n-gram string
  offset: u32         // byte offset into postings.ngi  
  length: u32         // byte length of posting list

File list (at end, after entries):
  [varint-encoded path lengths + UTF-8 path bytes]
```

Total lookup file ≈ 16 bytes × num_unique_ngrams + file_list_size.

### File 2: `postings.ngi`

Contiguous blocks of delta-encoded, varint-compressed file IDs:

```
Block for n-gram "parse_to":
  [delta-varint encoded file IDs: 3, +5, +1, +12, ...]
  → files: [3, 8, 9, 21, ...]
```

Delta + varint = ~1-2 bytes per file ID on average (vs 4 bytes raw u32).

### File 3: `meta.json`

```json
{
  "version": 1,
  "root": "/path/to/project",
  "git_head": "abc123...",
  "file_count": 12345,
  "ngram_count": 89012,
  "built_at": "2026-03-22T10:00:00Z",
  "build_duration_ms": 1200
}
```

---

## CLI Interface

```bash
# Build index
ngi index [path]              # default: current directory
ngi index --force             # rebuild from scratch
ngi index --stats             # print index statistics

# Search
ngi search <regex>            # search using index
ngi search -i <regex>         # case insensitive
ngi search -f '\.rs$' <regex> # file pattern filter
ngi search -l <regex>         # files only (no line content)
ngi search --no-index <regex> # bypass index, pure rg (for comparison)
ngi search --explain <regex>  # show n-gram query plan without searching

# Status
ngi status                    # index freshness, stats
ngi clean                     # remove .ngi/ directory
```

Output format matches ripgrep for drop-in compatibility:
```
src/indexer.rs:42:fn build_sparse_ngrams(text: &[u8]) -> Vec<NGram> {
src/query.rs:118:    let covering = build_covering(&literal);
```

---

## Crate Dependencies

```toml
[dependencies]
regex-syntax = "0.8"      # regex → HIR → literal extraction
memmap2 = "0.9"           # mmap index files
ignore = "0.4"            # .gitignore-aware file walking (ripgrep's crate)
crc32fast = "1.4"         # CRC32 for weight function
clap = { version = "4", features = ["derive"] }  # CLI
anyhow = "1"              # error handling

[dev-dependencies]
tempfile = "3"
assert_cmd = "2"          # CLI integration tests
```

No runtime dependency on ripgrep binary — we do our own matching using the `regex` crate directly. (Optional: delegate to `rg` for output formatting compatibility.)

---

## Implementation Phases

### Phase 1: Core Engine (MVP)
1. **weight.rs** — CRC32 weight function for character pairs
2. **ngram.rs** — `build_all()` and `build_covering()` sparse n-gram algorithms  
3. **indexer.rs** — walk files, extract n-grams, build inverted index in memory
4. **storage.rs** — serialize to two-file format, mmap reader
5. **query.rs** — regex → HIR → literal extraction → n-gram queries
6. **search.rs** — load index, evaluate query, intersect posting lists, regex match candidates
7. **main.rs** — CLI with `index` and `search` subcommands

**Exit criteria:** `ngi index .` builds an index, `ngi search 'pattern'` returns correct results faster than `rg`.

### Phase 2: Correctness & Polish
- Handle edge cases: binary files, huge files, symlinks, empty files
- Unicode handling in n-grams (byte-level, not char-level — like ripgrep)
- Case-insensitive search (lowercase n-grams in index, query both cases)
- File pattern filtering (`-f` flag)
- `--explain` mode showing query plan
- Comprehensive test suite

### Phase 3: Performance
- Parallel file walking + indexing (rayon)
- Posting list intersection optimization (galloping/skip pointers)
- Memory-efficient index building (streaming, not all-in-RAM)
- Benchmark suite against `rg` on real repos (Linux kernel, chromium)

### Phase 4: Freshness & Incremental
- Git HEAD tracking in meta.json
- mtime-based dirty file detection
- Incremental reindex: only re-scan changed files, merge posting lists
- Auto-index on first search if no index exists

### Phase 5: Integration
- PyO3 Python bindings (`pip install ngi`)
- Keche tool wrapper (`code_search` tool)
- JSON output mode for programmatic use
- LSP-compatible output for editor integration

---

## Key Design Decisions

| Decision | Choice | Why |
|---|---|---|
| Weight function | CRC32 of char pairs | Deterministic, no training data, GitHub/ClickHouse proven |
| N-gram granularity | Byte-level | Matches ripgrep behavior, avoids Unicode complexity |
| Posting list encoding | Delta + varint | Standard, 2-4x compression over raw u32 |
| Index location | `.ngi/` in project root | Discoverable, `.gitignore`-able |
| Final matching | Built-in `regex` crate | No external dependency, same engine as ripgrep |
| File walking | `ignore` crate | Same .gitignore handling as ripgrep |
| Query when no literals | Fall back to full scan | Honest — `.*` can't be indexed |

---

## Estimated Index Sizes

Based on Cursor's blog and trigram index empirics:

| Repository | Files | Source Size | Estimated Index |
|---|---|---|---|
| Small project (1k files) | 1,000 | 10 MB | ~2 MB |
| Medium (10k files) | 10,000 | 100 MB | ~15 MB |
| Large monorepo (100k files) | 100,000 | 1 GB | ~120 MB |
| Linux kernel | 80,000 | 1.2 GB | ~150 MB |

Sparse n-grams should be 30-50% smaller than trigram indexes due to fewer unique terms.

---

## References

1. [Cursor: Fast Regex Search](https://cursor.com/blog/fast-regex-search) — primary inspiration
2. [Russ Cox: Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html) — foundational algorithm
3. [github/codesearch](https://github.com/google/codesearch) — Go reference implementation (trigrams)
4. [ClickHouse sparse n-grams](https://github.com/ClickHouse/ClickHouse) — production sparse n-gram implementation
5. [GitHub Blackbird](https://github.blog/engineering/architecture-optimization/how-we-built-github-code-search/) — origin of sparse n-gram idea
