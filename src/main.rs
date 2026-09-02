use anyhow::{bail, Context, Result};
use clap::Parser;
use memdecomp::{scan, xpress};
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Recover Xpress-compressed pages from Windows 8/10 page files and memory
/// dumps (Linux-native rewrite of MemoryDecompression.exe).
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Input file, or a directory of files (e.g. a Volatility vaddump of the
    /// MemCompression process) — every file in it is scanned.
    input: PathBuf,

    /// Output file. All recovered pages are written here, in scan order.
    output: PathBuf,

    /// Number of worker threads (default: number of logical CPUs).
    #[arg(short, long)]
    threads: Option<usize>,

    /// Minimum decompressed size, in bytes, to accept a candidate as a real
    /// page (matches winmem_decompress's default; the original tool
    /// effectively required exactly 4096 here, which drops truncated pages).
    #[arg(long, default_value_t = 1024)]
    min_size: usize,

    /// Suppress per-file progress output.
    #[arg(short, long)]
    quiet: bool,
}

/// Size of the offset ranges handed to each rayon task. Kept well above
/// PAGE_SIZE so parallel overhead stays negligible; candidate decode windows
/// are still allowed to read past a chunk's end (see scan::scan_range), so
/// this boundary never causes the cross-chunk data loss the original tool had.
const WORK_CHUNK: usize = 64 * 1024 * 1024;

struct Totals {
    pages: u64,
    compressed_bytes: u64,
}

fn scan_slice(data: &[u8], min_size: usize) -> (Vec<u8>, Totals) {
    let mut starts: Vec<usize> = (0..data.len()).step_by(WORK_CHUNK).collect();
    if starts.is_empty() {
        starts.push(0);
    }

    let chunk_results: Vec<Vec<scan::Hit>> = starts
        .par_iter()
        .map(|&start| {
            let end = (start + WORK_CHUNK).min(data.len());
            scan::scan_range(data, start, end, min_size)
        })
        .collect();

    let mut out = Vec::new();
    let mut pages = 0u64;
    let mut compressed_bytes = 0u64;
    for hits in chunk_results {
        for hit in hits {
            out.extend_from_slice(&hit.page);
            pages += 1;
            compressed_bytes += hit.compressed_len as u64;
        }
    }

    (
        out,
        Totals {
            pages,
            compressed_bytes,
        },
    )
}

fn scan_file(path: &Path, min_size: usize) -> Result<(Vec<u8>, Totals)> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    Ok(scan_slice(&mmap, min_size))
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .context("failed to configure thread pool")?;
    }

    if args.output.exists() {
        bail!("output file already exists: {}", args.output.display());
    }
    let out_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.output)
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    let mut writer = BufWriter::new(out_file);

    let start_time = Instant::now();
    let mut total_pages = 0u64;
    let mut total_compressed = 0u64;

    let meta = fs::metadata(&args.input)
        .with_context(|| format!("failed to stat {}", args.input.display()))?;

    if meta.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&args.input)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();

        for path in entries {
            if !args.quiet {
                println!("Decompressing\t{}", path.display());
            }
            let (data, totals) = scan_file(&path, args.min_size)?;
            writer.write_all(&data)?;
            total_pages += totals.pages;
            total_compressed += totals.compressed_bytes;
        }
    } else {
        if !args.quiet {
            println!("Decompressing\t{}", args.input.display());
        }
        let (data, totals) = scan_file(&args.input, args.min_size)?;
        writer.write_all(&data)?;
        total_pages += totals.pages;
        total_compressed += totals.compressed_bytes;
    }

    writer.flush()?;

    let elapsed = start_time.elapsed();
    println!();
    println!("Total decompressed pages:\t{}", total_pages);
    println!("Total compressed data:\t\t{} bytes", total_compressed);
    println!(
        "Total decompressed data:\t{} bytes",
        total_pages * xpress::PAGE_SIZE as u64
    );
    println!();
    println!("Decompression completed in:");
    println!("Total seconds:\t{:.3}", elapsed.as_secs_f64());

    Ok(())
}
