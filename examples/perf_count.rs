//! Counts the raw block reads the mount path issues for directory work,
//! with the metadata cache off and on, over one identical workload.
//!
//! Why an in-process counter rather than `\Process(xfs)\IO Read
//! Operations`: on the host this was written for, that performance
//! counter set is empty -- `Get-Counter -ListSet process` returns no
//! counter names at all -- so a process-level I/O counter cannot be
//! read. Counting the calls the reader makes is available, exact, and
//! independent of what the operating system's own cache absorbs below.
//!
//! The phases run uncached, cached, uncached again. Repeating the
//! uncached run matters: by then the operating system has been offered
//! every block twice already, so its elapsed time is comparable with
//! the cached run's instead of flattered by ordering. Read counts are
//! order-independent either way.
//!
//! Read-only by construction. `Counting` takes its write methods from
//! the `BlockDevice` defaults, which answer `Err(ReadOnly)`, and the
//! `FileSource` it forwards to was opened without write access.
//!
//! ```text
//! cargo run --release --features mount --target aarch64-pc-windows-msvc \
//!   --example perf_count -- \\.\PhysicalDrive0 2 usr/lib/firmware etc
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use fs_core::{BlockDevice, BlockRead, CachingDevice};
use fs_xfs::Filesystem;
use winfsp_fs_skeleton::device::{BlockSource, FileSource};
use winfsp_fs_skeleton::partition;
use xfs_win_driver::mount::metadata_cache;

/// Forwards reads to one partition of a source and counts them.
struct Counting {
    src: Arc<dyn BlockSource>,
    base: u64,
    len: u64,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl Counting {
    /// Reads since the last call, resetting the tally.
    fn take(&self) -> (u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }
}

impl BlockRead for Counting {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.src
            .read_at(self.base + offset, buf)
            .map_err(fs_core::Error::Io)
    }

    fn size_bytes(&self) -> u64 {
        self.len
    }
}

// Deliberately empty: the defaults are the read-only answers.
impl BlockDevice for Counting {}

/// Open every child of every directory given. This is what a file
/// browser does when a folder opens: one row at a time, and each
/// `open` re-walks the whole path from the root inode.
fn workload(fs: &Filesystem, dirs: &[String]) -> Result<(usize, usize)> {
    let mut entries = 0usize;
    let mut opens = 0usize;
    for dir in dirs {
        let trimmed = dir.trim_end_matches('/');
        let handle = fs
            .open(trimmed)
            .with_context(|| format!("opening {trimmed}"))?;
        for entry in handle.entries()? {
            entries += 1;
            let name = String::from_utf8_lossy(&entry.name).into_owned();
            let child = format!("{trimmed}/{name}");
            fs.open(&child).with_context(|| format!("opening {child}"))?;
            opens += 1;
        }
    }
    Ok((entries, opens))
}

fn report(label: &str, dev: &Counting, ms: u128, extra: impl std::fmt::Display) {
    let (reads, bytes) = dev.take();
    println!("{label:<9} reads={reads:<7} bytes={bytes:<11} ms={ms:<6} {extra}");
}

fn main() -> Result<()> {
    let mut argv = std::env::args_os().skip(1);
    let image: PathBuf = argv
        .next()
        .context("usage: perf_count <device|image> <part> <dir> [<dir>...]")?
        .into();
    let part: usize = argv
        .next()
        .map(|p| p.to_string_lossy().parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    let dirs: Vec<String> = argv.map(|d| d.to_string_lossy().into_owned()).collect();
    if dirs.is_empty() {
        bail!("no directories given");
    }

    let src: Arc<dyn BlockSource> = Arc::new(
        FileSource::open(&image).with_context(|| format!("opening {}", image.display()))?,
    );
    let (base, len) = if part == 0 {
        (0, src.size())
    } else {
        let parts = partition::list_from_source(src.as_ref())?;
        let p = parts
            .get(part - 1)
            .with_context(|| format!("no partition {part} ({} found)", parts.len()))?;
        (p.start_lba * 512, p.num_sectors * 512)
    };

    let dev = Arc::new(Counting {
        src,
        base,
        len,
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    });
    let image_ref: &Path = &image;

    println!("image={} part={part}", image.display());
    println!("dirs   {}", dirs.join(" "));

    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>)
        .context("mounting without the cache")?;
    let t = Instant::now();
    let shape = workload(&fs, &dirs)?;
    report("uncached", &dev, t.elapsed().as_millis(), format!("entries={}", shape.0));
    drop(fs);

    let cached: Arc<CachingDevice> =
        metadata_cache(dev.clone() as Arc<dyn BlockDevice>, image_ref)?;
    let fs = Filesystem::mount(cached.clone() as Arc<dyn BlockRead>)
        .context("mounting through the cache")?;
    let t = Instant::now();
    let shape2 = workload(&fs, &dirs)?;
    let (hits, misses) = cached.stats();
    report(
        "cached",
        &dev,
        t.elapsed().as_millis(),
        format_args!(
            "dirblock={} hits={hits} misses={misses}",
            fs.superblock().dirblocksize()
        ),
    );
    if shape != shape2 {
        bail!("the cached run listed {shape2:?}, the uncached run listed {shape:?}");
    }
    drop(fs);

    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>)
        .context("mounting without the cache again")?;
    let t = Instant::now();
    let shape3 = workload(&fs, &dirs)?;
    report("uncached2", &dev, t.elapsed().as_millis(), String::new());
    if shape != shape3 {
        bail!("the second uncached run listed {shape3:?}, the first listed {shape:?}");
    }
    Ok(())
}
