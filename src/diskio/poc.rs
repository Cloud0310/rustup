//! Experimental async extraction entry point for issue #4159.
//!
//! The production tar parser, directory dependency graph, slab buffers, chunk
//! acknowledgements and filesystem operations are shared by both backends.
//! Only admission, execution and joining of disk work differ. This is a first
//! migration stage, not a native asynchronous filesystem implementation.

use std::{
    fs::File,
    io::{BufReader, Read},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    thread,
};

use anyhow::{Context, anyhow, ensure};
use tempfile::TempDir;
use tokio::{runtime::Handle, sync::oneshot};

use super::{Executor, IO_CHUNK_SIZE, effective_thread_count, get_executor, threaded::Threaded};
use crate::{dist::component::unpack_without_first_dir, process::IoThreadCount};

#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Threaded,
    Tokio,
}

#[derive(Clone, Copy, Debug)]
pub enum Compression {
    None,
    Gzip,
    Xz,
    Zstd,
}

/// Extract into an owned temporary directory, stripping the archive's top level.
///
/// `ram_budget` has the existing buffer-pool semantics; it is not an RSS limit.
/// Automatic low-memory fallback and explicit thread overrides are preserved.
/// The runtime must remain alive until extraction has drained. Dropping this
/// future does not interrupt syscalls: the producer finishes and then removes
/// its owned output, so callers cannot remove a directory still being written.
pub async fn unpack(
    archive: PathBuf,
    output_parent: PathBuf,
    compression: Compression,
    backend: Backend,
    threads: IoThreadCount,
    ram_budget: usize,
) -> anyhow::Result<TempDir> {
    let count = effective_thread_count(ram_budget, threads);
    ensure!(count > 0, "thread count must be positive");
    ensure!(
        ram_budget >= 2 * IO_CHUNK_SIZE,
        "RAM budget must be at least 32 MiB"
    );
    let runtime = Handle::current();
    let (tx, rx) = oneshot::channel();
    // Do not put the producer in Tokio's blocking pool: it waits for disk jobs
    // in that same pool, which would deadlock when the pool has only one slot.
    thread::Builder::new()
        .name("rustup-tar-poc".into())
        .stack_size(1_048_576)
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let output = tempfile::Builder::new()
                    .prefix("rustup-async-poc-")
                    .tempdir_in(output_parent)?;
                let reader = BufReader::new(File::open(archive)?);
                let reader: Box<dyn Read> = match compression {
                    Compression::None => Box::new(reader),
                    Compression::Gzip => Box::new(flate2::bufread::GzDecoder::new(reader)),
                    Compression::Xz => Box::new(xz2::bufread::XzDecoder::new(reader)),
                    Compression::Zstd => {
                        Box::new(zstd::stream::read::Decoder::with_buffer(reader)?)
                    }
                };
                let executor: Box<dyn Executor> = match backend {
                    Backend::Tokio if count > 1 => {
                        Box::new(Threaded::new_tokio(count, ram_budget, runtime))
                    }
                    _ => get_executor(ram_budget, IoThreadCount::UserSpecified(count)),
                };
                unpack_without_first_dir(&mut tar::Archive::new(reader), output.path(), executor)?;
                Ok(output)
            }))
            .unwrap_or_else(|_| Err(anyhow!("tar producer panicked")));
            // If the caller cancelled, the undelivered TempDir is dropped here,
            // after the executor has joined all writes.
            let _ = tx.send(result);
        })
        .context("starting tar producer")?;
    rx.await.context("tar producer stopped without a result")?
}
