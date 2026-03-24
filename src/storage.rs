//! On-disk storage format and mmap reader for the n-gram index.
//!
//! Three files in the `.ngi/` directory:
//! - `lookup.ngi`:   32-byte header + sorted array of 16-byte entries (hash, offset, length)
//! - `postings.ngi`: contiguous blocks of delta+varint encoded file IDs
//! - `files.txt`:    one file path per line (file_id → path mapping)
//! - `meta.json`:    build metadata

use std::cmp::Ordering;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

// ---------------------------------------------------------------------------
// File metadata for incremental reindex
// ---------------------------------------------------------------------------

/// Per-file metadata stored at index time for freshness checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Write file metadata to `filemeta.bin` in the index directory.
///
/// Format: num_files(u64) then for each file: path_len(u32), path bytes, mtime(u64), size(u64).
pub fn write_filemeta(states: &[FileState], index_dir: &Path) -> Result<()> {
    let path = index_dir.join("filemeta.bin");
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&(states.len() as u64).to_le_bytes());
    for s in states {
        let path_bytes = s.path.as_bytes();
        buf.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(path_bytes);
        buf.extend_from_slice(&s.mtime.to_le_bytes());
        buf.extend_from_slice(&s.size.to_le_bytes());
    }
    fs::write(&path, &buf).context("failed to write filemeta.bin")?;
    Ok(())
}

/// Read file metadata from `filemeta.bin`.
pub fn read_filemeta(index_dir: &Path) -> Result<Vec<FileState>> {
    let path = index_dir.join("filemeta.bin");
    let data = fs::read(&path).context("failed to read filemeta.bin")?;
    if data.len() < 8 {
        bail!("filemeta.bin too small");
    }
    let num_files = u64::from_le_bytes(data[0..8].try_into()?) as usize;
    let mut states = Vec::with_capacity(num_files);
    let mut pos = 8;
    for _ in 0..num_files {
        if pos + 4 > data.len() {
            bail!("filemeta.bin truncated (path_len)");
        }
        let path_len = u32::from_le_bytes(data[pos..pos + 4].try_into()?) as usize;
        pos += 4;
        if pos + path_len > data.len() {
            bail!("filemeta.bin truncated (path)");
        }
        let file_path = std::str::from_utf8(&data[pos..pos + path_len])
            .context("invalid UTF-8 in filemeta path")?
            .to_string();
        pos += path_len;
        if pos + 16 > data.len() {
            bail!("filemeta.bin truncated (mtime/size)");
        }
        let mtime = u64::from_le_bytes(data[pos..pos + 8].try_into()?);
        pos += 8;
        let size = u64::from_le_bytes(data[pos..pos + 8].try_into()?);
        pos += 8;
        states.push(FileState {
            path: file_path,
            mtime,
            size,
        });
    }
    Ok(states)
}

use crate::indexer::InMemoryIndex;

const MAGIC: [u8; 4] = *b"NGI\x01";
const VERSION: u32 = 3;
/// We accept older versions for backward compatibility.
const VERSION_V1: u32 = 1;
const VERSION_V2: u32 = 2;
const HEADER_SIZE: usize = 32;
const ENTRY_SIZE: usize = 16;

// ---------------------------------------------------------------------------
// Varint (LEB128) encoding / decoding
// ---------------------------------------------------------------------------

fn encode_varint(value: u32, buf: &mut Vec<u8>) {
    let mut v = value;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn decode_varint(data: &[u8]) -> Result<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if shift >= 35 {
            bail!("varint overflow");
        }
        result |= ((byte & 0x7F) as u32) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    bail!("unexpected end of varint data")
}

// ---------------------------------------------------------------------------
// Delta + varint posting list codec (v3: just file IDs, no bloom masks)
// ---------------------------------------------------------------------------

fn encode_posting_list(file_ids: &[u32]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut prev: u32 = 0;
    for &fid in file_ids {
        encode_varint(fid - prev, &mut buf);
        prev = fid;
    }
    buf
}

/// Decode a v3/v1 posting list (no bloom masks) — just delta-varint file IDs.
fn decode_posting_list(data: &[u8]) -> Result<Vec<u32>> {
    let mut file_ids = Vec::new();
    let mut pos = 0;
    let mut prev: u32 = 0;
    while pos < data.len() {
        let (delta, consumed) = decode_varint(&data[pos..])?;
        prev = prev.checked_add(delta).context("file ID overflow")?;
        file_ids.push(prev);
        pos += consumed;
    }
    Ok(file_ids)
}

/// Decode a v2 posting list (with bloom masks) — ignores masks, returns just file IDs.
fn decode_posting_list_v2(data: &[u8]) -> Result<Vec<u32>> {
    let mut file_ids = Vec::new();
    let mut pos = 0;
    let mut prev: u32 = 0;
    while pos < data.len() {
        let (delta, consumed) = decode_varint(&data[pos..])?;
        pos += consumed;
        if pos + 2 > data.len() {
            bail!("v2 posting list truncated: missing bloom masks");
        }
        // Skip loc_mask and next_mask
        pos += 2;
        prev = prev.checked_add(delta).context("file ID overflow")?;
        file_ids.push(prev);
    }
    Ok(file_ids)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Write an InMemoryIndex to disk at `index_dir` (the `.ngi/` directory).
///
/// `file_states` contains per-file mtime/size for freshness checking.
/// If `None`, filemeta.bin is not written (backwards-compatible).
pub fn write_index(
    index: &InMemoryIndex,
    index_dir: &Path,
    root: &Path,
) -> Result<()> {
    write_index_with_meta(index, index_dir, root, None, None)
}

/// Write an InMemoryIndex to disk, including optional file metadata and git HEAD.
pub fn write_index_with_meta(
    index: &InMemoryIndex,
    index_dir: &Path,
    root: &Path,
    file_states: Option<&[FileState]>,
    git_head: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(index_dir)?;

    // Sort n-grams by hash for binary-searchable lookup
    let mut sorted_hashes: Vec<u64> = index.postings.keys().copied().collect();
    sorted_hashes.sort_unstable();

    // 1. Write postings.ngi — collect (hash, offset, length) for each entry
    let postings_path = index_dir.join("postings.ngi");
    let mut postings_file = fs::File::create(&postings_path)?;
    let mut entries: Vec<(u64, u32, u32)> = Vec::with_capacity(sorted_hashes.len());
    let mut offset: u32 = 0;

    for &hash in &sorted_hashes {
        let posting_list = &index.postings[&hash];
        let encoded = encode_posting_list(posting_list);
        let length = encoded.len() as u32;
        postings_file.write_all(&encoded)?;
        entries.push((hash, offset, length));
        offset += length;
    }

    // 2. Write lookup.ngi — header + sorted entries
    let lookup_path = index_dir.join("lookup.ngi");
    let mut lookup_file = fs::File::create(&lookup_path)?;

    // Header (32 bytes)
    lookup_file.write_all(&MAGIC)?;
    lookup_file.write_all(&VERSION.to_le_bytes())?;
    lookup_file.write_all(&(entries.len() as u64).to_le_bytes())?;
    lookup_file.write_all(&(index.files.len() as u64).to_le_bytes())?;
    lookup_file.write_all(&[0u8; 8])?; // reserved

    // Entries (16 bytes each)
    for &(hash, off, len) in &entries {
        lookup_file.write_all(&hash.to_le_bytes())?;
        lookup_file.write_all(&off.to_le_bytes())?;
        lookup_file.write_all(&len.to_le_bytes())?;
    }

    // 3. Write files.txt — one path per line
    let files_path = index_dir.join("files.txt");
    fs::write(&files_path, index.files.join("\n"))?;

    // 4. Write meta.json
    let meta_path = index_dir.join("meta.json");
    let root_escaped = root
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let duration_ms = index.stats.build_duration.as_millis();
    let git_head_line = match git_head {
        Some(h) => format!("  \"git_head\": \"{}\",\n", h),
        None => String::new(),
    };
    let meta = format!(
        "{{\n  \"version\": 1,\n  \"root\": \"{}\",\n{}  \"file_count\": {},\n  \"ngram_count\": {},\n  \"built_at_unix\": {},\n  \"build_duration_ms\": {}\n}}\n",
        root_escaped,
        git_head_line,
        index.files.len(),
        index.postings.len(),
        built_at,
        duration_ms,
    );
    fs::write(&meta_path, meta)?;

    // 5. Write filemeta.bin if provided
    if let Some(states) = file_states {
        write_filemeta(states, index_dir)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Reader (mmap'd)
// ---------------------------------------------------------------------------

/// An mmap'd index for fast search lookups.
pub struct MappedIndex {
    lookup_mmap: Mmap,
    postings_mmap: Option<Mmap>,
    files: Vec<String>,
    num_entries: u64,
    /// Format version (1 = no masks, 2 = with bloom masks, 3 = no masks v3).
    version: u32,
}

impl MappedIndex {
    /// Open an existing index from the `.ngi/` directory.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let lookup_file =
            fs::File::open(index_dir.join("lookup.ngi")).context("failed to open lookup.ngi")?;
        let postings_file = fs::File::open(index_dir.join("postings.ngi"))
            .context("failed to open postings.ngi")?;

        // SAFETY: read-only mapping of files we control
        let lookup_mmap = unsafe { Mmap::map(&lookup_file)? };

        let postings_len = postings_file.metadata()?.len();
        let postings_mmap = if postings_len > 0 {
            Some(unsafe { Mmap::map(&postings_file)? })
        } else {
            None
        };

        // Validate header
        if lookup_mmap.len() < HEADER_SIZE {
            bail!("lookup.ngi too small for header");
        }
        if lookup_mmap[0..4] != MAGIC {
            bail!("invalid magic bytes in lookup.ngi");
        }
        let version = u32::from_le_bytes(lookup_mmap[4..8].try_into()?);
        if version != VERSION && version != VERSION_V1 && version != VERSION_V2 {
            bail!("unsupported index version: {version}");
        }
        let num_entries = u64::from_le_bytes(lookup_mmap[8..16].try_into()?);

        let expected_size = HEADER_SIZE + (num_entries as usize) * ENTRY_SIZE;
        if lookup_mmap.len() < expected_size {
            bail!(
                "lookup.ngi truncated: expected at least {expected_size} bytes, got {}",
                lookup_mmap.len()
            );
        }

        // Load file list
        let files_content =
            fs::read_to_string(index_dir.join("files.txt")).context("failed to read files.txt")?;
        let files: Vec<String> = if files_content.is_empty() {
            Vec::new()
        } else {
            files_content.lines().map(String::from).collect()
        };

        Ok(MappedIndex {
            lookup_mmap,
            postings_mmap,
            files,
            num_entries,
            version,
        })
    }

    /// Look up an n-gram hash, return the list of file IDs.
    pub fn lookup(&self, hash: u64) -> Option<Vec<u32>> {
        let n = self.num_entries as usize;
        if n == 0 {
            return None;
        }

        // Binary search over sorted entries in the mmap
        let mut lo: usize = 0;
        let mut hi: usize = n;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let base = HEADER_SIZE + mid * ENTRY_SIZE;
            let entry_hash =
                u64::from_le_bytes(self.lookup_mmap[base..base + 8].try_into().ok()?);

            match entry_hash.cmp(&hash) {
                Ordering::Equal => {
                    let post_offset = u32::from_le_bytes(
                        self.lookup_mmap[base + 8..base + 12].try_into().ok()?,
                    ) as usize;
                    let post_length = u32::from_le_bytes(
                        self.lookup_mmap[base + 12..base + 16].try_into().ok()?,
                    ) as usize;

                    let postings_mmap = self.postings_mmap.as_ref()?;
                    if post_offset + post_length > postings_mmap.len() {
                        return None;
                    }
                    let data = &postings_mmap[post_offset..post_offset + post_length];
                    return if self.version == VERSION_V2 {
                        decode_posting_list_v2(data).ok()
                    } else {
                        decode_posting_list(data).ok()
                    };
                }
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }

        None
    }

    /// Get the file path for a file ID.
    pub fn file_path(&self, file_id: u32) -> Option<&str> {
        self.files.get(file_id as usize).map(|s| s.as_str())
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of unique n-grams.
    pub fn ngram_count(&self) -> u64 {
        self.num_entries
    }

    /// Get the list of all indexed file paths.
    pub fn file_list(&self) -> &[String] {
        &self.files
    }

    /// Iterate over all (hash, file_id_list) entries in the index.
    /// Used by incremental reindex to preserve unchanged entries.
    pub fn all_postings(&self) -> Vec<(u64, Vec<u32>)> {
        let n = self.num_entries as usize;
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            let base = HEADER_SIZE + i * ENTRY_SIZE;
            let hash = u64::from_le_bytes(
                self.lookup_mmap[base..base + 8].try_into().unwrap(),
            );
            let post_offset = u32::from_le_bytes(
                self.lookup_mmap[base + 8..base + 12].try_into().unwrap(),
            ) as usize;
            let post_length = u32::from_le_bytes(
                self.lookup_mmap[base + 12..base + 16].try_into().unwrap(),
            ) as usize;
            let file_ids = match &self.postings_mmap {
                Some(mmap) if post_offset + post_length <= mmap.len() => {
                    let data = &mmap[post_offset..post_offset + post_length];
                    if self.version == VERSION_V2 {
                        decode_posting_list_v2(data).unwrap_or_default()
                    } else {
                        decode_posting_list(data).unwrap_or_default()
                    }
                }
                _ => Vec::new(),
            };
            result.push((hash, file_ids));
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{InMemoryIndex, IndexStats};
    use std::collections::HashMap;
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_stats(file_count: usize, ngram_count: usize) -> IndexStats {
        IndexStats {
            file_count,
            ngram_count,
            total_ngrams: ngram_count,
            build_duration: Duration::from_millis(1),
            skipped_binary: 0,
            skipped_large: 0,
            skipped_errors: 0,
        }
    }

    // -- Varint --

    #[test]
    fn varint_roundtrip() {
        for v in [0u32, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf);
            let (decoded, consumed) = decode_varint(&buf).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn varint_encoding_size() {
        // Single byte for 0..=127
        let mut buf = Vec::new();
        encode_varint(127, &mut buf);
        assert_eq!(buf.len(), 1);

        // Two bytes for 128
        buf.clear();
        encode_varint(128, &mut buf);
        assert_eq!(buf.len(), 2);

        // Five bytes for u32::MAX
        buf.clear();
        encode_varint(u32::MAX, &mut buf);
        assert_eq!(buf.len(), 5);
    }

    // -- Posting list --

    #[test]
    fn posting_list_roundtrip() {
        let cases: Vec<Vec<u32>> = vec![
            vec![3, 8, 9, 21],
            vec![],
            vec![42],
            vec![0, 100_000, 200_000],
            vec![0, 1, 2, 3, 4, 5],
        ];
        for file_ids in &cases {
            let encoded = encode_posting_list(file_ids);
            let decoded = decode_posting_list(&encoded).unwrap();
            assert_eq!(&decoded, file_ids);
        }
    }

    #[test]
    fn posting_list_large_gaps() {
        let file_ids = vec![0, 1_000_000, 2_000_000, 2_000_001];
        let encoded = encode_posting_list(&file_ids);
        let decoded = decode_posting_list(&encoded).unwrap();
        assert_eq!(decoded, file_ids);
    }

    // -- Write then read --

    #[test]
    fn write_then_read_basic() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let mut postings = HashMap::new();
        postings.insert(100u64, vec![0u32, 1, 2]);
        postings.insert(200u64, vec![1u32, 3]);
        postings.insert(300u64, vec![0u32]);

        let index = InMemoryIndex {
            files: vec![
                "src/main.rs".into(),
                "src/lib.rs".into(),
                "README.md".into(),
                "Cargo.toml".into(),
            ],
            postings: postings.clone(),
            stats: make_stats(4, 3),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        let r100 = mapped.lookup(100).unwrap();
        assert_eq!(r100, vec![0u32, 1, 2]);
        let r200 = mapped.lookup(200).unwrap();
        assert_eq!(r200, vec![1u32, 3]);
        let r300 = mapped.lookup(300).unwrap();
        assert_eq!(r300, vec![0u32]);

        assert_eq!(mapped.file_count(), 4);
        assert_eq!(mapped.ngram_count(), 3);
    }

    #[test]
    fn write_then_read_empty_index() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let index = InMemoryIndex {
            files: Vec::new(),
            postings: HashMap::new(),
            stats: make_stats(0, 0),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        assert!(mapped.lookup(42).is_none());
        assert_eq!(mapped.file_count(), 0);
        assert_eq!(mapped.ngram_count(), 0);
    }

    // -- Binary search correctness --

    #[test]
    fn binary_search_many_ngrams() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let mut postings = HashMap::new();
        for i in 0u64..1000 {
            let fid = (i % 10) as u32;
            postings.insert(i * 7 + 13, vec![fid]);
        }

        let index = InMemoryIndex {
            files: (0..10).map(|i| format!("file_{i}.txt")).collect(),
            postings: postings.clone(),
            stats: make_stats(10, 1000),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        for (&hash, expected) in &postings {
            let result = mapped.lookup(hash).expect(&format!("hash {hash} not found"));
            assert_eq!(&result, expected, "mismatch for hash {hash}");
        }
    }

    // -- Missing hash --

    #[test]
    fn missing_hash_returns_none() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let mut postings = HashMap::new();
        postings.insert(42u64, vec![0u32]);

        let index = InMemoryIndex {
            files: vec!["a.txt".into()],
            postings,
            stats: make_stats(1, 1),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        assert!(mapped.lookup(0).is_none());
        assert!(mapped.lookup(999).is_none());
        assert!(mapped.lookup(u64::MAX).is_none());
    }

    // -- file_path --

    #[test]
    fn file_path_mapping() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let index = InMemoryIndex {
            files: vec!["src/main.rs".into(), "README.md".into()],
            postings: HashMap::new(),
            stats: make_stats(2, 0),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        assert_eq!(mapped.file_path(0), Some("src/main.rs"));
        assert_eq!(mapped.file_path(1), Some("README.md"));
        assert_eq!(mapped.file_path(2), None);
    }

    // -- Integration: build_index → write → read → lookup --

    #[test]
    fn end_to_end_with_real_indexer() {
        use crate::indexer::build_index;

        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("hello.rs"),
            "fn main() { println!(\"hello world\"); }",
        )
        .unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn greet() { println!(\"greetings\"); }",
        )
        .unwrap();

        let index = build_index(dir.path(), crate::indexer::DEFAULT_MAX_FILE_SIZE).unwrap();
        let index_dir = dir.path().join(".ngi");
        write_index(&index, &index_dir, dir.path()).unwrap();

        let mapped = MappedIndex::open(&index_dir).unwrap();
        assert_eq!(mapped.file_count(), 2);
        assert!(mapped.ngram_count() > 0);

        // Every hash from the in-memory index should be findable with matching postings
        for (&hash, expected) in &index.postings {
            let result = mapped.lookup(hash).expect(&format!("hash {hash} missing"));
            assert_eq!(&result, expected);
        }
    }

    // -- Filemeta roundtrip --

    #[test]
    fn filemeta_roundtrip() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");
        fs::create_dir_all(&index_dir).unwrap();

        let states = vec![
            FileState { path: "src/main.rs".into(), mtime: 1711000000, size: 4096 },
            FileState { path: "README.md".into(), mtime: 1711000001, size: 256 },
            FileState { path: "dir/deep/file.txt".into(), mtime: 1711000002, size: 0 },
        ];

        write_filemeta(&states, &index_dir).unwrap();
        let loaded = read_filemeta(&index_dir).unwrap();
        assert_eq!(loaded, states);
    }

    #[test]
    fn filemeta_empty() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");
        fs::create_dir_all(&index_dir).unwrap();

        write_filemeta(&[], &index_dir).unwrap();
        let loaded = read_filemeta(&index_dir).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn all_postings_roundtrip() {
        let dir = TempDir::new().unwrap();
        let index_dir = dir.path().join(".ngi");

        let mut postings = HashMap::new();
        postings.insert(100u64, vec![0u32, 1]);
        postings.insert(200u64, vec![2u32]);

        let index = InMemoryIndex {
            files: vec!["a.txt".into(), "b.txt".into(), "c.txt".into()],
            postings: postings.clone(),
            stats: make_stats(3, 2),
        };

        write_index(&index, &index_dir, dir.path()).unwrap();
        let mapped = MappedIndex::open(&index_dir).unwrap();

        let all = mapped.all_postings();
        assert_eq!(all.len(), 2);

        let all_map: HashMap<u64, Vec<u32>> = all.into_iter().collect();
        assert_eq!(all_map[&100], vec![0u32, 1]);
        assert_eq!(all_map[&200], vec![2u32]);

        assert_eq!(mapped.file_list(), &["a.txt", "b.txt", "c.txt"]);
    }

}
