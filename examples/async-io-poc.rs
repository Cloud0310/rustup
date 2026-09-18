//! Run with --help. Fixture generation and repeated comparisons live in
//! scripts/bench-async-io.py. Output validation is outside the extraction timer.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Result, ensure};
use clap::{Parser, ValueEnum};
use rustup::{
    async_io_poc::{self, Backend, Compression},
    process::IoThreadCount,
};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Threaded,
    Tokio,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum)]
    backend: Mode,
    #[arg(long)]
    archive: PathBuf,
    /// Existing parent on the filesystem being measured.
    #[arg(long)]
    output_parent: PathBuf,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 64)]
    ram_mib: usize,
    /// Exercise automatic low-memory fallback instead of an explicit override.
    #[arg(long)]
    automatic_threads: bool,
    #[arg(long, default_value_t = 32)]
    blocking_threads: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.threads > 0 && args.blocking_threads > 0,
        "thread counts must be positive"
    );
    let ram = args
        .ram_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("RAM budget overflow"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(args.blocking_threads)
        .thread_stack_size(1_048_576)
        .build()?;
    let compression = match args.archive.extension().and_then(|e| e.to_str()) {
        Some("gz") => Compression::Gzip,
        Some("xz") => Compression::Xz,
        Some("zst") => Compression::Zstd,
        _ => Compression::None,
    };
    let backend = match args.backend {
        Mode::Threaded => Backend::Threaded,
        Mode::Tokio => Backend::Tokio,
    };
    let threads = if args.automatic_threads {
        IoThreadCount::Default(args.threads)
    } else {
        IoThreadCount::UserSpecified(args.threads)
    };
    let (cpu_start, _) = usage();
    let start = Instant::now();
    let output = runtime.block_on(async_io_poc::unpack(
        args.archive,
        args.output_parent,
        compression,
        backend,
        threads,
        ram,
    ))?;
    let seconds = start.elapsed().as_secs_f64();
    let (cpu_end, rss_kib) = usage();
    let mut hash = Sha256::new();
    let (files, bytes) = fingerprint(output.path(), output.path(), &mut hash)?;
    let digest = faster_hex::hex_string(&hash.finalize());
    println!(
        "{{\"seconds\":{seconds:.9},\"cpu_seconds\":{:.9},\"rss_kib\":{rss_kib},\"files\":{files},\"bytes\":{bytes},\"sha256\":\"{digest}\"}}",
        cpu_end - cpu_start,
    );
    Ok(())
}

// Includes relative paths, types, lengths, modes and all file bytes. A sorted
// traversal makes the digest independent of worker completion order.
fn fingerprint(root: &Path, path: &Path, hash: &mut Sha256) -> Result<(u64, u64)> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let (mut files, mut bytes) = (0, 0);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(!metadata.is_symlink(), "unexpected symlink");
        let relative = path.strip_prefix(root)?.to_string_lossy();
        hash.update((relative.len() as u64).to_le_bytes());
        hash.update(relative.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
        }
        if metadata.is_dir() {
            hash.update(b"d");
            let (child_files, child_bytes) = fingerprint(root, &path, hash)?;
            files += child_files;
            bytes += child_bytes;
        } else {
            hash.update(b"f");
            hash.update(metadata.len().to_le_bytes());
            files += 1;
            bytes += metadata.len();
            let mut file = File::open(path)?;
            let mut buffer = [0; 65536];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(unix)]
fn usage() -> (f64, u64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes a valid rusage to an aligned, writable pointer.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(status, 0);
    // SAFETY: the successful getrusage call initialized the value.
    let usage = unsafe { usage.assume_init() };
    let time = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0;
    #[cfg(not(target_os = "linux"))]
    let rss = usage.ru_maxrss as u64;
    #[cfg(target_os = "linux")]
    let rss = {
        // getrusage's high-water mark survives exec and can include the
        // launcher. VmHWM belongs to this executable's address space.
        let status = fs::read_to_string("/proc/self/status").expect("reading process RSS");
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .expect("VmHWM is available on Linux")
    };
    #[cfg(target_os = "macos")]
    let rss = rss / 1024;
    (time(usage.ru_utime) + time(usage.ru_stime), rss)
}

#[cfg(not(unix))]
fn usage() -> (f64, u64) {
    (0.0, 0) // Collect process CPU/RSS with the platform profiler instead.
}
