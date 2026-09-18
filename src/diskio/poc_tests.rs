use std::{
    fs,
    io::{self, Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::{
    IO_CHUNK_SIZE,
    poc::{self, Backend, Compression},
    tokio_pool::TokioPool,
};
use crate::process::IoThreadCount;

fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, size: u64, mode: u32) {
    struct Pattern {
        position: u64,
        size: u64,
    }
    impl Read for Pattern {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let within_chunk = self.position as usize % IO_CHUNK_SIZE;
            let count = buffer
                .len()
                .min((self.size - self.position) as usize)
                .min(IO_CHUNK_SIZE - within_chunk);
            buffer[..count].fill(91 + (self.position / IO_CHUNK_SIZE as u64) as u8);
            self.position += count as u64;
            Ok(count)
        }
    }
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(mode);
    header.set_cksum();
    builder
        .append_data(&mut header, path, Pattern { position: 0, size })
        .unwrap();
}

fn fixture() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    // Missing parents and a file before its explicit directory entry.
    append(&mut tar, "pkg/a/b/tool", 100, 0o700);
    let mut directory = tar::Header::new_gnu();
    directory.set_entry_type(tar::EntryType::Directory);
    directory.set_size(0);
    directory.set_mode(0o755);
    directory.set_cksum();
    tar.append_data(&mut directory, "pkg/a/b", io::empty())
        .unwrap();
    append(&mut tar, "pkg/empty", 0, 0o600);
    append(
        &mut tar,
        "pkg/large",
        (IO_CHUNK_SIZE * 2 + 17) as u64,
        0o600,
    );
    tar.into_inner().unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn round_trip_and_compressions() {
    let work = tempfile::tempdir().unwrap();
    let bytes = fixture();
    for compression in [
        Compression::None,
        Compression::Gzip,
        Compression::Xz,
        Compression::Zstd,
    ] {
        let data = match compression {
            Compression::None => bytes.clone(),
            Compression::Gzip => {
                let mut encoder =
                    flate2::read::GzEncoder::new(Cursor::new(&bytes), flate2::Compression::fast());
                let mut out = Vec::new();
                encoder.read_to_end(&mut out).unwrap();
                out
            }
            Compression::Xz => {
                let mut encoder = xz2::read::XzEncoder::new(Cursor::new(&bytes), 0);
                let mut out = Vec::new();
                encoder.read_to_end(&mut out).unwrap();
                out
            }
            Compression::Zstd => zstd::stream::encode_all(Cursor::new(&bytes), 1).unwrap(),
        };
        let archive = work.path().join("input");
        fs::write(&archive, data).unwrap();
        for backend in [Backend::Threaded, Backend::Tokio] {
            let output = tokio::time::timeout(
                Duration::from_secs(30),
                poc::unpack(
                    archive.clone(),
                    work.path().into(),
                    compression,
                    backend,
                    IoThreadCount::UserSpecified(2),
                    32 * 1024 * 1024,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                fs::read(output.path().join("a/b/tool")).unwrap(),
                vec![91; 100]
            );
            assert_eq!(fs::metadata(output.path().join("empty")).unwrap().len(), 0);
            let mut large = fs::File::open(output.path().join("large")).unwrap();
            let mut buffer = [0; 65536];
            let mut length = 0;
            loop {
                let n = large.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                assert!(
                    buffer[..n].iter().enumerate().all(|(offset, b)| {
                        *b == 91 + ((length + offset) / IO_CHUNK_SIZE) as u8
                    })
                );
                length += n;
            }
            assert_eq!(length, IO_CHUNK_SIZE * 2 + 17);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(output.path().join("a/b/tool"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o755
                );
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn truncated_large_file_drains_and_cleans_up() {
    let work = tempfile::tempdir().unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    append(&mut tar, "pkg/large", (IO_CHUNK_SIZE * 2) as u64, 0o644);
    let mut bytes = tar.into_inner().unwrap();
    bytes.truncate(IO_CHUNK_SIZE + 1024);
    let archive = work.path().join("truncated.tar");
    fs::write(&archive, bytes).unwrap();
    for backend in [Backend::Threaded, Backend::Tokio] {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            poc::unpack(
                archive.clone(),
                work.path().into(),
                Compression::None,
                backend,
                IoThreadCount::UserSpecified(2),
                32 * 1024 * 1024,
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_dir(work.path()).unwrap().count(), 1);
    }
}

#[test]
fn bounded_scheduler_and_single_blocking_slot() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let runtime = rt.handle().clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let heartbeat = Arc::new(AtomicUsize::new(0));
    let ticks = heartbeat.clone();
    std::thread::spawn(move || {
        let pool = TokioPool::new(4, runtime);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..32 {
            let (active, peak, done) = (active.clone(), peak.clone(), done.clone());
            pool.execute(move || {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
        pool.join();
        assert_eq!(done.load(Ordering::SeqCst), 32);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        tx.send(()).unwrap();
    });
    rt.block_on(async {
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1)).await;
                ticks.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
        ticker.abort();
    });
    assert!(heartbeat.load(Ordering::SeqCst) >= 5);
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_links_and_parent_traversal() {
    let work = tempfile::tempdir().unwrap();
    for link in [true, false] {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        if link {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("outside").unwrap();
            header.set_path("pkg/link").unwrap();
        } else {
            // Bypass tar's safe path setter to construct a hostile archive.
            header.as_mut_bytes()[..14].copy_from_slice(b"pkg/../outside");
        }
        header.set_cksum();
        tar.append(&header, io::empty()).unwrap();
        let archive = work.path().join("bad.tar");
        fs::write(&archive, tar.into_inner().unwrap()).unwrap();
        for backend in [Backend::Threaded, Backend::Tokio] {
            assert!(
                poc::unpack(
                    archive.clone(),
                    work.path().into(),
                    Compression::None,
                    backend,
                    IoThreadCount::UserSpecified(2),
                    32 * 1024 * 1024
                )
                .await
                .is_err()
            );
            assert_eq!(fs::read_dir(work.path()).unwrap().count(), 1);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_error_drains_and_cleans_up() {
    let work = tempfile::tempdir().unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    append(&mut tar, "pkg/occupied", 1, 0o644);
    append(&mut tar, "pkg/occupied/child", 1, 0o644);
    let archive = work.path().join("bad-parent.tar");
    fs::write(&archive, tar.into_inner().unwrap()).unwrap();
    for backend in [Backend::Threaded, Backend::Tokio] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            poc::unpack(
                archive.clone(),
                work.path().into(),
                Compression::None,
                backend,
                IoThreadCount::UserSpecified(2),
                32 * 1024 * 1024,
            ),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_dir(work.path()).unwrap().count(), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_caller_keeps_output_owned_until_drain() {
    let work = tempfile::tempdir().unwrap();
    let archive = work.path().join("input.tar");
    fs::write(&archive, fixture()).unwrap();
    let output_parent = work.path().to_path_buf();
    let task = tokio::spawn(poc::unpack(
        archive,
        output_parent,
        Compression::None,
        Backend::Tokio,
        IoThreadCount::UserSpecified(2),
        32 * 1024 * 1024,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while fs::read_dir(work.path()).unwrap().count() == 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    drop(task.await);
    // Keep the runtime alive while non-abortable disk operations drain.
    tokio::time::timeout(Duration::from_secs(5), async {
        while fs::read_dir(work.path()).unwrap().count() != 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn scheduler_respects_disk_limit_and_reclaims_buffers() {
    use super::{CompletedIo, Executor, Item, threaded::Threaded};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(8)
        .build()
        .unwrap();
    let runtime = rt.handle().clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let pool = TokioPool::new(2, runtime.clone());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let (active, peak) = (active.clone(), peak.clone());
            pool.execute(move || {
                let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        pool.join();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        let work = tempfile::tempdir().unwrap();
        let mut executor = Threaded::new_tokio(2, 32 * 1024 * 1024, runtime);
        let (item, mut sender) = Item::write_file_segmented(
            work.path().join("file"),
            0o644,
            executor.incremental_file_state(),
        )
        .unwrap();
        executor.execute(item).for_each(drop);
        let mut chunk = executor.get_buffer(IO_CHUNK_SIZE);
        chunk.extend_from_slice(b"hello");
        sender.submit(chunk.finished());
        while !executor.buffer_available(IO_CHUNK_SIZE) {
            executor.completed().for_each(drop);
            std::thread::yield_now();
        }
        let eof = executor.get_buffer(IO_CHUNK_SIZE).finished();
        sender.submit(eof);
        for completed in executor.join() {
            if let CompletedIo::Item(item) = completed {
                item.result.unwrap();
            }
        }
        assert_eq!(executor.buffer_used(), 0);
        assert_eq!(fs::read(work.path().join("file")).unwrap(), b"hello");
        drop(executor);
        tx.send(()).unwrap();
    });
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();
    });
}
