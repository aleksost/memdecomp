# memdecomp

Recovers pages compressed by the Windows 8/10 memory manager
(`COMPRESSION_FORMAT_XPRESS | COMPRESSION_ENGINE_STANDARD`) from page files,
memory dumps, or `vaddump` directories of the `MemCompression` process —
without depending on `ntdll.dll`, so it runs on Linux (and anywhere else Rust
targets).

This is a from-scratch Linux-native rewrite of
[aleksost/MemoryDecompression](https://github.com/aleksost/MemoryDecompression)
(`MemoryDecompression.exe`), a Windows-only C++/`ntdll.dll`-dependent tool.
See [`REWRITE_PLAN.md`](REWRITE_PLAN.md) for the full design rationale.

Compared to the original tool:
- **No Windows dependency**: the Xpress "Plain LZ77" decoder
  (`src/xpress.rs`) is a native Rust implementation of the public MS-XCA
  algorithm, not a call into `RtlDecompressBuffer`.
- **Recovers truncated pages**: a candidate no longer has to decompress to
  exactly 4096 bytes to count as a hit. This fixes
  [issue #1](https://github.com/aleksost/MemoryDecompression/issues/1),
  using the same truncation-tolerant approach as
  [msuhanov/winmem_decompress](https://github.com/msuhanov/winmem_decompress)
  (GPL-3.0, same license as this project).
- **Much faster**: one decode pass per candidate offset (vs. the original's
  up-to-~4080 growing-length trial decodes), `mmap`'d input, and multi-core
  scanning via `rayon`.

## Build

```
cargo build --release
```

## Usage

```
memdecomp <input file-or-dir> <output-file> [--threads N] [--min-size BYTES] [--quiet]
```

```
$ memdecomp VADDUMP-segment-compressed.bin VADDUMP-segment-decompressed.bin
Decompressing  VADDUMP-segment-compressed.bin

Total decompressed pages:      130
Total compressed data:         122652 bytes
Total decompressed data:       532480 bytes

Decompression completed in:
Total seconds:  0.002
```

- `input` may be a single file or a directory (all files inside are scanned,
  e.g. a Volatility `vaddump` output directory).
- `--min-size` (default 1024) is the minimum decompressed byte count to
  accept a candidate as a real page; matches `winmem_decompress`'s default.
- `--threads` defaults to the number of logical CPUs.

## Tests

```
cargo test
```

`tests/golden.rs` decompresses the repo's committed
`VADDUMP-segment-compressed.bin` and asserts the result is byte-for-byte
identical to the committed `VADDUMP-segment-decompressed.bin` (both produced
by the original tool) — a regression pin against known-good output.
