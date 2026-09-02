//! Decoder for "Plain LZ77" (MS-XCA §2.4), the algorithm behind Windows'
//! `COMPRESSION_FORMAT_XPRESS | COMPRESSION_ENGINE_STANDARD` — the format the
//! Windows 8/10 memory manager uses to compress pages before storing them in
//! the `MemCompression` process or a page file.
//!
//! This is a from-scratch Rust implementation of the public MS-XCA algorithm.
//! The truncation-tolerant behavior (stop cleanly and return whatever output
//! was produced instead of requiring an exact 4096-byte result) mirrors the
//! approach in Maxim Suhanov's `winmem_decompress`
//! (https://github.com/msuhanov/winmem_decompress, GPL-3.0), which is what
//! fixes https://github.com/aleksost/MemoryDecompression/issues/1 — the
//! original tool required an exact-size match and so silently dropped
//! truncated pages (page slack, end-of-file, etc).

/// Windows memory-manager pages are always 4096 bytes.
pub const PAGE_SIZE: usize = 4096;

/// Compressed chunks are stored on 16-byte boundaries.
pub const CHUNK_ALIGN: usize = 16;

pub struct Decoded {
    /// Decompressed bytes actually produced (may be shorter or longer than
    /// PAGE_SIZE — the caller decides how to pad/truncate).
    pub data: Vec<u8>,
    /// How many bytes of `input` were consumed. Used by the scanner to know
    /// where the next candidate chunk starts, even for a truncated/failed
    /// decode (so the scan cursor still advances sensibly).
    pub consumed: usize,
}

/// Decompress `input` using Plain LZ77, stopping as soon as `max_output`
/// bytes have been produced (we never need more than one page's worth), the
/// input is exhausted, or the stream turns out to be bogus data.
///
/// This never panics on malformed input: every out-of-bounds read or
/// nonsensical back-reference just ends decoding early and returns whatever
/// was decoded so far, exactly like the reference implementation this is
/// ported from.
pub fn decompress(input: &[u8], max_output: usize) -> Decoded {
    let mut out: Vec<u8> = Vec::with_capacity(max_output.min(input.len().max(64) * 2));

    let mut pos = 0usize; // input cursor
    let mut flags: u32 = 0;
    let mut flag_bits_left: u32 = 0;
    // Xpress packs two 4-bit nibble match-lengths into one byte when two
    // back-to-back matches both need the "length == 7" extended-length
    // encoding. `pending_len_byte` remembers the offset of that byte between
    // the two matches that share it.
    let mut pending_len_byte: Option<usize> = None;

    macro_rules! byte_at {
        ($i:expr) => {
            match input.get($i) {
                Some(&b) => b,
                None => break,
            }
        };
    }

    loop {
        if out.len() >= max_output {
            break;
        }

        if flag_bits_left == 0 {
            if pos + 4 > input.len() {
                break; // truncated flag field: stop cleanly
            }
            flags =
                u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
            pos += 4;
            flag_bits_left = 32;
        }

        flag_bits_left -= 1;
        let is_match = (flags & (1 << flag_bits_left)) != 0;

        if !is_match {
            // Literal byte.
            let b = byte_at!(pos);
            out.push(b);
            pos += 1;
            continue;
        }

        if pos == input.len() {
            // Clean end of stream right at a flag boundary: success.
            break;
        }

        if pos + 2 > input.len() {
            break;
        }
        let match_bytes = u16::from_le_bytes([input[pos], input[pos + 1]]);
        pos += 2;

        let mut match_len = (match_bytes % 8) as u32;
        let match_offset = ((match_bytes / 8) as usize) + 1;

        if match_len == 7 {
            match_len = match pending_len_byte.take() {
                None => {
                    let b = byte_at!(pos);
                    pending_len_byte = Some(pos);
                    pos += 1;
                    (b % 16) as u32
                }
                Some(prev_pos) => {
                    // prev_pos is guaranteed in-range: it was read successfully before.
                    (input[prev_pos] / 16) as u32
                }
            };

            if match_len == 15 {
                let b = byte_at!(pos);
                pos += 1;
                match_len = b as u32;
                if match_len == 255 {
                    if pos + 2 > input.len() {
                        break;
                    }
                    let ext = u16::from_le_bytes([input[pos], input[pos + 1]]) as u32;
                    pos += 2;
                    if ext < 15 + 7 {
                        break; // bogus data
                    }
                    match_len = ext - (15 + 7);
                }
                match_len += 15;
            }
            match_len += 7;
        }
        match_len += 3;

        if match_offset > out.len() {
            break; // back-reference points before the start of output: bogus
        }

        // Byte-by-byte copy (not a bulk memcpy) because LZ77 back-references
        // are allowed to overlap the bytes currently being written (this is
        // how run-length repeats are encoded).
        for src in (out.len() - match_offset..).take(match_len as usize) {
            if out.len() >= max_output {
                break;
            }
            let b = out[src];
            out.push(b);
        }
    }

    Decoded {
        data: out,
        consumed: pos,
    }
}

/// Round `n` up to the next boundary that is a multiple of `align` and
/// strictly greater than `n` — matches the original tool's `roundUp`, which
/// guarantees the scan cursor always makes forward progress even when `n`
/// already happens to be aligned.
pub fn round_up_next(n: usize, align: usize) -> usize {
    let remainder = n % align;
    n + align - remainder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_clean_eof() {
        let d = decompress(&[], PAGE_SIZE);
        assert_eq!(d.data.len(), 0);
        assert_eq!(d.consumed, 0);
    }

    #[test]
    fn truncated_flags_field_does_not_panic() {
        let d = decompress(&[0xFFu8], PAGE_SIZE);
        assert_eq!(d.data.len(), 0);
    }

    #[test]
    fn all_literals_roundtrip() {
        // flags = 0 (all literals), 32 literal bytes follow.
        let mut input = vec![0u8, 0, 0, 0];
        let literals: Vec<u8> = (0..32).collect();
        input.extend_from_slice(&literals);
        let d = decompress(&input, PAGE_SIZE);
        assert_eq!(d.data, literals);
        assert_eq!(d.consumed, input.len());
    }

    #[test]
    fn round_up_next_matches_reference_semantics() {
        assert_eq!(round_up_next(0, 16), 16);
        assert_eq!(round_up_next(16, 16), 32);
        assert_eq!(round_up_next(17, 16), 32);
        assert_eq!(round_up_next(31, 16), 32);
    }

    #[test]
    fn bogus_backreference_stops_cleanly() {
        // flags = all-ones nibble forcing a match on the first token, with no
        // prior output to reference -> should stop without panicking.
        let input = vec![0xFFu8, 0xFF, 0xFF, 0xFF, 0x00, 0x00];
        let d = decompress(&input, PAGE_SIZE);
        assert!(d.data.len() <= PAGE_SIZE);
    }
}
