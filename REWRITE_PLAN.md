# Rewrite plan: Linux port + performance + truncated-page support

> Note: this plan was written while the rewrite was being developed inside a
> clone of the original [aleksost/MemoryDecompression](https://github.com/aleksost/MemoryDecompression)
> repo, in a `memdecomp/` subdirectory alongside the original C++ project —
> hence the relative paths below. It has since been spun out into this
> standalone repo, with `memdecomp/`'s former contents now at the repo root.

## 1. Why rewrite, not port 1:1

The current `MemoryDecompression.cpp` cannot simply be recompiled for Linux: its
entire decompression step is one call into `ntdll.dll`'s `RtlDecompressBuffer`
(`GetProcAddress(ntdll, "RtlDecompressBuffer")`), a Windows-only API. There is no
POSIX/Linux equivalent, so the actual decoder has to be reimplemented natively —
which is also the opportunity to fix the two other problems in scope.

## 2. Problems found in the current implementation

**Correctness**
- Requires the decompressed output to be *exactly* 4096 bytes to accept a match
  (`MemoryDecompression.cpp:55`). Any page whose compressed data is cut off
  (page slack, or the last page in a page file) never decompresses to exactly
  4096 bytes and is silently dropped. This is exactly **issue #1**
  ("The tool doesn't extract truncated compressed memory pages").
- Reads the input file in fixed 0x80000-byte (512 KiB) chunks and re-derives
  page candidates with raw index math into that buffer
  (`MemoryDecompression.cpp:91`). A compressed block straddling a chunk
  boundary is never read as one contiguous span, so hits near every 512 KiB
  boundary can be missed or corrupted.

**Performance**
- `GetProcAddress` is called *inside* `decompress_buffer`, i.e. once per
  candidate offset per candidate length — potentially tens of millions of
  times per run. Trivial fix (do it once) but symptomatic of the deeper issue:
- For every 16-byte-aligned offset, the tool doesn't decompress once — it
  tries `bufferLen = 16, 17, 18, …` up to 4096, calling the decompressor fresh
  each time, until the result happens to be exactly 4096 bytes
  (`MemoryDecompression.cpp:111-148`). That's up to ~4080 full decompression
  attempts to resolve *one* candidate. This quadratic-ish trial loop is the
  dominant cost (documented runs: 5h for a memory dump, 4h38m for a 6 GB page
  file).
- Two heap allocations (`new UCHAR[4096]` ×2) per candidate offset, immediately
  freed — millions of malloc/free cycles.
- Single-threaded; no use of multiple cores at all.

**Reference implementation that already solves the truncation bug**

Issue #1 links to [msuhanov/winmem_decompress](https://github.com/msuhanov/winmem_decompress)
(GPL-3.0, same license as this repo). Its `LZ77DecompressBuffer` is a from-scratch
plain-LZ77 decoder (the exact algorithm behind `COMPRESSION_FORMAT_XPRESS` /
`COMPRESSION_ENGINE_STANDARD` — see MS-XCA §2.4 "Plain LZ77") that:
- decodes byte-by-byte and simply **stops** when it runs out of valid input,
  returning whatever output it produced so far instead of requiring an exact size;
- pads the result with zero bytes up to 4096 if it's short, and truncates if
  a bogus/garbage match run overshoots — so truncated data (page slack, EOF)
  is recovered instead of discarded;
- accepts anything ≥1024 decompressed bytes as a hit (configurable), rather
  than demanding an exact page;
- parallelizes with a 4-worker process pool.

Because we already have to write our own decoder for Linux, porting this
algorithm (not the Python code verbatim, but the same public MS-XCA "Plain
LZ77" scheme it implements) gets us Linux support *and* the truncated-page
fix in one step, with credit to msuhanov in the README/CHANGELOG.

## 3. Target design

**Language: Rust.** Rationale: this is a byte-level binary parser processing
untrusted/malformed input by design (brute-force scanning) — Rust's bounds
checking turns "bogus data" cases into a `None`/`Result` instead of a
potential OOB read, safe threading (`rayon`) avoids hand-rolled pthread
plumbing, and `memmap2` gives zero-copy file access without the 512 KiB
chunk-boundary bug above.

**Crate layout** (new `memdecomp/` directory alongside the existing C++
project, which stays as-is for historical/Windows use):
```
memdecomp/
  Cargo.toml
  src/
    main.rs      - CLI, input dispatch (file vs directory), orchestration, stats/timing
    xpress.rs    - the plain-LZ77 (Xpress standard) decoder, truncation-tolerant
    scan.rs       - candidate scanning over a byte slice, zero-page skipping, offset advance
  tests/
    golden.rs    - decompresses VADDUMP-segment-compressed.bin and diffs it
                   byte-for-byte against VADDUMP-segment-decompressed.bin
                   (both already committed in the repo) as a regression test
```

**Algorithm changes**
1. `xpress::decompress_page(input: &[u8]) -> DecodedPage { consumed: usize, data: Vec<u8> }`
   runs the plain-LZ77 state machine once per candidate and *naturally* knows
   how many input bytes it consumed to produce however much output it got —
   no more growing-length trial loop. This alone turns an O(4096) retry chain
   into a single O(compressed size) decode: the single biggest performance
   win.
2. Accept the page if `data.len() >= MIN_DECOMPRESSED` (default 1024, matching
   winmem_decompress) instead of requiring exactly 4096. Truncate to 4096 if
   longer (trailing garbage after a real page boundary), zero-pad to 4096 if
   shorter (truncated/slack data) — this is the issue #1 fix.
3. On a hit, advance the scan cursor by `round_up(consumed, 16)` (as the
   original tool does) to skip past bytes already accounted for; on a miss,
   advance by 16 (the known alignment of compressed chunk storage, per both
   the original tool and winmem_decompress).

**Performance changes**
- `mmap` the whole input file (`memmap2`) instead of manual chunked
  `ReadFile`/`fread` — removes the chunk-boundary bug and the buffering
  overhead, and lets the OS page cache handle prefetch.
- Parallelize with `rayon`: split the mapped file into large (e.g. 64 MiB)
  slices, scan each slice on a thread pool, each producing an ordered
  `Vec<(offset, page)>`. This is embarrassingly parallel since candidate
  offsets are independent.
- Reassemble results in original file order (stable per-chunk ordering plus
  chunk index order) and stream them to a single buffered writer thread —
  preserves the original tool's "output in scan order" behavior while still
  parallelizing the expensive part.
- Fast zero-page skip stays (`memcmp`-equivalent slice comparison; Rust/LLVM
  auto-vectorizes this), but is done once per candidate window instead of
  being tangled into the retry loop.
- No allocation per candidate on the hot path where possible (reuse a
  scratch output buffer per worker thread instead of `Vec::new()` each time).
- Directory input (vaddump segment folders) processed with the same
  per-file parallelism, plus rayon across files.

**CLI** (kept close to the original for muscle memory, extended):
```
memdecomp <input file-or-dir> <output-file> [--threads N] [--min-size BYTES] [--quiet]
```
Defaults: `--threads` = available cores, `--min-size` = 1024.

## 4. Testing plan

- **Golden regression test**: the repo already ships a matched pair,
  `VADDUMP-segment-compressed.bin` → `VADDUMP-segment-decompressed.bin`. The
  new decoder must reproduce the decompressed file byte-for-byte from the
  compressed one. This is checked into `tests/golden.rs` and run in CI.
- **Truncation unit tests**: synthetic compressed buffers that are cut off
  mid-match/mid-literal, asserting the decoder returns the correct partial
  output (zero-padded) instead of erroring or panicking.
- **Fuzz target** (optional, `cargo fuzz`): the decoder is the one place that
  parses attacker-controlled/corrupted bytes; worth fuzzing given the past
  bogus-data handling bugs in this class of tool.
- **Benchmark**: time the new binary against the same page-file scenario
  documented in the current README, to confirm we've actually improved on
  the ~4h38m baseline.

## 4b. Benchmark results (real-world data, post-implementation)

Measured against a real, previously-untested Windows pagefile (2.55 GB,
copied to local NVMe first to remove drive-speed as a variable):

| | Original tool (documented in old README) | memdecomp |
|---|---|---|
| Dataset | ~6 GB pagefile, 803,827 pages | 2.55 GB pagefile, 393,744 pages |
| Time | 16,731s (4h 38m) | 5.1s |
| Rate | 48 pages/sec | ~77,000 pages/sec |

That's roughly **~640x** the original's pages/sec (different dataset sizes,
so treat the multiplier as directional, not a precise ratio — but the order
of magnitude is real and reproducible).

**First pass** got the pagefile down to ~6.2-7.5s using the design in
section 3 as originally implemented (mmap + rayon scan, then a single
serial `write_all` of the assembled output). Phase timing (`--timing`)
showed scan ≈ 3.7s and write ≈ 2-3s as two fully serial phases.

**Second pass — pipelining scan and write.** Restructured so each rayon
chunk sends its decoded buffer to a dedicated writer thread over a channel
as soon as it's ready (the writer reorders out-of-sequence chunks with a
small `BTreeMap` buffer, since rayon doesn't finish chunks in order, and
streams them to disk in original file order). First attempt at this made
things *worse* (~6.2-7.2s, no better than serial) — because `write()`
also costs real CPU (copying into the page cache), so on a fully
core-saturated 8-core box the writer thread had nowhere free to run: it
just stole cycles from a scan worker instead of overlapping "for free".
Confirmed via a `--dry-run` mode (scan with an `io::sink()` writer) showing
decode-only time was ~4.2-4.8s — the write phase was really adding ~2s on
top even with the pipeline, i.e. no actual overlap was happening.

**Fix**: default to `available_cores - 1` scan threads (not all cores)
whenever writing a real output file, explicitly reserving one core for the
writer thread so its CPU-side work has somewhere to run concurrently
instead of contending with the scan pool. That took the pipelined,
default-flags run down to a consistent ~5.1-5.4s — close to the decode-only
`--dry-run` floor, and a genuine ~20-30% improvement over the original
serial-phases version. Output was verified byte-identical to the
pre-optimization run (the pipeline's reordering logic doesn't change what
gets written, only when).

## 5. Rollout

1. Implement `xpress.rs` + golden test first — correctness gate before any
   perf work.
2. Implement `scan.rs` single-threaded, confirm it matches the golden test
   and roughly matches the old tool's output on a real sample.
3. Add `mmap` + `rayon` parallelism.
4. Wire up CLI/main.rs, directory handling, stats output.
5. Update top-level `README.md`: Linux usage, note issue #1 is fixed, credit
   `msuhanov/winmem_decompress` for the algorithm this reuses, and mark the
   old `MemoryDecompression.cpp` project as the legacy Windows build (or drop
   it, if this repo has no Windows Store users left — leaving as a call for
   later).
