//! N-gram hashing for trigram index lookups.

/// FNV-1a 64-bit hash. Deterministic across all Rust versions and platforms.
/// Unlike DefaultHasher (SipHash), the output is guaranteed stable for on-disk
/// index format compatibility.
pub fn hash_ngram(text: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001B3;
    let mut hash = FNV_OFFSET;
    for &byte in text {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_deterministic() {
        // These exact values are pinned — changing them breaks existing indexes.
        assert_eq!(hash_ngram(b"abc"), 0xe71fa2190541574b);
        assert_eq!(hash_ngram(b"fn "), 0xdcb5d018fedca5ef);
    }

    #[test]
    fn fnv1a_empty() {
        assert_eq!(hash_ngram(b""), 0xcbf29ce484222325); // FNV offset basis
    }

    #[test]
    fn fnv1a_single_byte_differs() {
        assert_ne!(hash_ngram(b"a"), hash_ngram(b"b"));
    }
}
