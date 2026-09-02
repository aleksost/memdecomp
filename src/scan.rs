//! Brute-force scanner: walk 16-byte-aligned offsets in a byte slice looking
//! for Plain-LZ77 (Xpress) compressed pages.
//!
//! Compared to the original `MemoryDecompression.cpp`:
//! - one decode attempt per candidate offset instead of up to ~4080 growing-
//!   length trial decodes (see `xpress::decompress`'s single-pass design);
//! - a hit no longer has to decompress to exactly 4096 bytes — anything
//!   producing at least `min_size` decompressed bytes is accepted and padded
//!   /truncated to a full page, which recovers truncated pages (issue #1);
//! - large all-zero runs are skipped a whole window at a time instead of
//!   16 bytes at a time.

use crate::xpress::{self, PAGE_SIZE};

pub struct Hit {
    /// Offset of the hit within the slice that was scanned.
    pub offset: usize,
    /// Always exactly PAGE_SIZE bytes: real decoded bytes, truncated or
    /// zero-padded as needed.
    pub page: Vec<u8>,
    /// How many compressed bytes this hit accounted for (for stats only).
    pub compressed_len: usize,
}

fn is_all_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

/// Scan `data[start..end]` for compressed pages. `end` is a soft bound on
/// where new candidates may *start*; a candidate's decode window is still
/// allowed to read past `end` (up to `data.len()`) since compressed blocks
/// can straddle whatever boundary a caller chose for parallel work-splitting.
/// This is what avoids the original tool's cross-chunk-boundary data loss.
pub fn scan_range(data: &[u8], start: usize, end: usize, min_size: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut pos = start;

    while pos < end && pos < data.len() {
        let window_end = (pos + PAGE_SIZE).min(data.len());
        let window = &data[pos..window_end];

        if is_all_zero(window) {
            pos += window.len().max(1);
            continue;
        }

        let decoded = xpress::decompress(window, PAGE_SIZE);
        if decoded.data.len() >= min_size {
            let mut page = decoded.data;
            page.resize(PAGE_SIZE, 0); // truncate (Vec::resize shrinks too) or zero-pad
            let compressed_len = decoded.consumed.max(1);
            hits.push(Hit {
                offset: pos,
                page,
                compressed_len,
            });
            pos += xpress::round_up_next(decoded.consumed, xpress::CHUNK_ALIGN);
        } else {
            pos += xpress::CHUNK_ALIGN;
        }
    }

    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_buffer_yields_no_hits() {
        let data = vec![0u8; PAGE_SIZE * 4];
        let hits = scan_range(&data, 0, data.len(), 1024);
        assert!(hits.is_empty());
    }
}
