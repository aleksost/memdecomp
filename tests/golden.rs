//! Regression test: decompressing the repo's sample
//! `VADDUMP-segment-compressed.bin` must reproduce
//! `VADDUMP-segment-decompressed.bin` byte-for-byte. Both files were
//! produced by (and already committed alongside) the original C++ tool, so
//! this pins the new decoder to the same known-good behavior.

use memdecomp::scan;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn vaddump_segment_matches_known_good_output() {
    let compressed_path = repo_root().join("VADDUMP-segment-compressed.bin");
    let expected_path = repo_root().join("VADDUMP-segment-decompressed.bin");

    let compressed = fs::read(&compressed_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", compressed_path.display()));
    let expected = fs::read(&expected_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", expected_path.display()));

    let hits = scan::scan_range(&compressed, 0, compressed.len(), 1024);
    let mut actual = Vec::new();
    for hit in &hits {
        actual.extend_from_slice(&hit.page);
    }

    assert_eq!(
        actual.len(),
        expected.len(),
        "decompressed {} bytes across {} pages, expected {} bytes",
        actual.len(),
        hits.len(),
        expected.len()
    );
    assert_eq!(
        actual, expected,
        "decompressed bytes do not match the known-good output"
    );
}
