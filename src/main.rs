use anyhow::{bail, Context, Result};
use clap::Parser;
use memdecomp::{scan, xpress};
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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

    /// Number of scan worker threads. Default: logical CPUs minus one, to
    /// leave a dedicated core for the writer thread that streams output to
    /// disk concurrently with scanning (this overlap is why it's not just
    /// "all cores"). With --dry-run, where there's no real output to write,
    /// the default is all cores instead.
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

    /// Print a per-file timing breakdown (mmap vs. scan+write pipeline) to
    /// stderr, for perf diagnosis.
    #[arg(long)]
    timing: bool,

    /// Scan and report stats without writing any output file. Useful to
    /// preview how much data would be recovered, and to isolate scan time
    /// from write time when benchmarking.
    #[arg(long)]
    dry_run: bool,
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

/// Scan `data` and stream recovered pages to `writer`, in original file
/// order, as a pipeline: each chunk is decoded and concatenated into its own
/// buffer on a rayon worker thread, then handed to a dedicated writer thread
/// over a channel. The writer reorders chunks (rayon doesn't finish them in
/// order) and streams each one to disk as soon as it's next in sequence —
/// so writing chunk N overlaps with decoding chunk N+1..k on the other
/// worker threads, instead of the whole scan finishing before any bytes hit
/// disk.
fn scan_slice(
    data: &[u8],
    min_size: usize,
    timing: bool,
    writer: &mut (impl Write + Send),
) -> Result<Totals> {
    let mut starts: Vec<usize> = (0..data.len()).step_by(WORK_CHUNK).collect();
    if starts.is_empty() {
        starts.push(0);
    }

    let t_pipeline = Instant::now();
    let (tx, rx) = mpsc::channel::<(usize, Vec<u8>, u64, u64)>();

    let pipeline_result: Result<(u64, u64)> = std::thread::scope(|scope| {
        let writer_handle = scope.spawn(move || -> (u64, u64, std::io::Result<()>) {
            // Chunks can finish out of order; buffer the early arrivals
            // until the ones before them show up, so output stays in the
            // same file-order the original tool produced.
            let mut pending: BTreeMap<usize, (Vec<u8>, u64, u64)> = BTreeMap::new();
            let mut next = 0usize;
            let mut pages = 0u64;
            let mut compressed = 0u64;
            let mut io_err = None;

            for (idx, buf, p, c) in rx {
                pending.insert(idx, (buf, p, c));
                while let Some((b, p2, c2)) = pending.remove(&next) {
                    if io_err.is_none() {
                        if let Err(e) = writer.write_all(&b) {
                            io_err = Some(e);
                        }
                    }
                    pages += p2;
                    compressed += c2;
                    next += 1;
                }
            }

            (pages, compressed, io_err.map_or(Ok(()), Err))
        });

        starts
            .par_iter()
            .enumerate()
            .for_each_with(tx, |tx, (i, &start)| {
                let end = (start + WORK_CHUNK).min(data.len());
                let hits = scan::scan_range(data, start, end, min_size);

                let mut buf = Vec::with_capacity(hits.len() * xpress::PAGE_SIZE);
                let mut pages = 0u64;
                let mut compressed = 0u64;
                for hit in &hits {
                    buf.extend_from_slice(&hit.page);
                    pages += 1;
                    compressed += hit.compressed_len as u64;
                }

                // Only fails if the writer thread already gave up (e.g. a
                // prior write error); nothing to do differently here, the
                // error itself is surfaced via writer_handle.join() below.
                let _ = tx.send((i, buf, pages, compressed));
            });

        let (pages, compressed, io_result) = writer_handle.join().expect("writer thread panicked");
        io_result.context("failed writing output")?;
        Ok((pages, compressed))
    });

    let (pages, compressed_bytes) = pipeline_result?;
    if timing {
        eprintln!(
            "  scan+write (pipelined): {:.3}s",
            t_pipeline.elapsed().as_secs_f64()
        );
    }

    Ok(Totals {
        pages,
        compressed_bytes,
    })
}

fn scan_file(
    path: &Path,
    min_size: usize,
    timing: bool,
    writer: &mut (impl Write + Send),
) -> Result<Totals> {
    let t_open = Instant::now();
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    if timing {
        eprintln!("  mmap:     {:.3}s", t_open.elapsed().as_secs_f64());
    }
    scan_slice(&mmap, min_size, timing, writer)
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Writing to disk also costs real CPU (copying into the page cache on
    // every write() call), and it runs concurrently with scanning on its
    // own dedicated thread (see scan_slice). On a fully core-saturated
    // machine that thread has nowhere free to run, so it just steals cycles
    // from a scan worker instead of overlapping "for free" — reserve one
    // core for it by default so the pipeline actually overlaps. Skipped for
    // --dry-run, where the writer is an io::sink() with near-zero cost, and
    // skipped whenever the user passes --threads explicitly.
    let scan_threads = args.threads.unwrap_or_else(|| {
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if args.dry_run {
            available
        } else {
            available.saturating_sub(1).max(1)
        }
    });
    rayon::ThreadPoolBuilder::new()
        .num_threads(scan_threads)
        .build_global()
        .context("failed to configure thread pool")?;

    let mut writer: Box<dyn Write + Send> = if args.dry_run {
        Box::new(std::io::sink())
    } else {
        if args.output.exists() {
            bail!("output file already exists: {}", args.output.display());
        }
        let out_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&args.output)
            .with_context(|| format!("failed to create {}", args.output.display()))?;
        Box::new(BufWriter::new(out_file))
    };

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
            let totals = scan_file(&path, args.min_size, args.timing, &mut writer)?;
            total_pages += totals.pages;
            total_compressed += totals.compressed_bytes;
        }
    } else {
        if !args.quiet {
            println!("Decompressing\t{}", args.input.display());
        }
        let totals = scan_file(&args.input, args.min_size, args.timing, &mut writer)?;
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
