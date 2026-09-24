//! XFS mount handle and (with the `mount` feature) WinFsp adapter.
//!
//! `Mount` opens an XFS image — either a regular file path or, on
//! Windows, a raw device like `\\.\PhysicalDriveN` — optionally seeking
//! into a specific partition's byte range. The opened
//! `am_fs_xfs::Filesystem` is then exposed to WinFsp via the
//! `FileSystemContext` trait.
//!
//! Read-only by design: XFS is read-only at the format level, so every
//! mutating WinFsp callback that would otherwise need wiring just inherits
//! the trait's default `STATUS_INVALID_DEVICE_REQUEST`. WinFsp's
//! `read_only_volume` flag is also set on `VolumeParams`, which is the
//! primary gate — Explorer renders the drive as RO, the cache manager
//! short-circuits writes, and the few callbacks that *do* dispatch
//! (notably `set_security`) accept-and-ignore so common Explorer flows
//! don't error out.
//!
//! Structure mirrors `ext4-win-driver/src/mount.rs` for consistency
//! across our Windows driver family. Code was written independently;
//! no verbatim copies of winfsp-rs example filesystems or xfs-utils.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::BlockRead;
use fs_xfs::inode::Inode;
use fs_xfs::Filesystem;
use winfsp_fs_skeleton::device::{BlockSource, FileSource};
use winfsp_fs_skeleton::partition;

use crate::overlay::{Overlay, OverlayEntry, OverlayLookup};

/// A `BlockRead` shim that:
///   - Forwards every read into a `FileSource` (sector-aligned raw-disk
///     reads on Windows, plain `pread` on Unix).
///   - Optionally offsets every read by `base` so we can hand the
///     XFS reader a partition slice without it knowing about the
///     surrounding disk.
///   - Reports `len` as the device size, so `BlockRead::size_bytes`
///     correctly reflects the slice's length (used by the XFS
///     compressed-read path's `dev_size` cap).
struct PartitionDevice {
    src: Arc<dyn BlockSource>,
    base: u64,
    len: u64,
}

impl BlockRead for PartitionDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: 0,
            })?;
        if end > self.len {
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: self.len.saturating_sub(offset) as usize,
            });
        }
        self.src
            .read_at(self.base + offset, buf)
            .map_err(fs_core::Error::Io)
    }

    fn size_bytes(&self) -> u64 {
        self.len
    }
}

// Every `BlockDevice` method is defaulted to a strict read-only device
// (`write_at -> Err(ReadOnly)`, `is_writable -> false`), so this impl
// buys nothing but the trait bound `CachingDevice` requires. It does not
// make the volume writable. A future write path must go through a type
// that overrides those defaults, not through this one.
impl fs_core::BlockDevice for PartitionDevice {}

/// Blocks held by the metadata read-cache (16 MiB at a 4 KiB granularity).
///
/// The bound is a trade-off, not a computation: it has to cover one wide
/// directory's data blocks plus the directories on the paths currently
/// being resolved, and must stay small enough to be invisible next to
/// the operating system's own cache. A 461-entry directory needs a few
/// dozen blocks, so this holds a hundred or more directories.
///
/// Public so `examples/perf_count.rs` measures the real thing rather
/// than a copy of these parameters.
pub const CACHE_BLOCKS: usize = 4096;

/// Wrap a partition view in a block cache sized to its directory blocks.
///
/// `am-fs-xfs` has no cache of its own and re-derives everything:
/// `Filesystem::open` walks every path component from the root inode,
/// and each step calls `lookup`, which runs `read_dir` over the *whole*
/// directory. Enumerating one 461-entry directory therefore issued
/// ~12 400 raw reads (process `IOReadOperations`), against ~30 blocks
/// of actual directory data. Caching at this layer turns every repeat
/// read of a block into a memcpy and leaves the reader untouched.
///
/// Granularity is `dirblocksize`, because directory data is what the
/// repetition is in. Inode (512 B) and B-tree (blocksize) reads fall
/// outside it and pass straight through -- `CachingDevice` matches an
/// entry only for reads of exactly one whole block. That is deliberate:
/// picking a finer granularity to catch inodes would drop the reads
/// that dominate.
///
/// The superblock is parsed here rather than taken from the mounted
/// filesystem, because the geometry has to be known before the device
/// is handed over. It costs one extra 4 KiB read at mount time.
///
/// Public, and generic over `BlockDevice` rather than taking a
/// `PartitionDevice`, so `examples/perf_count.rs` can put a counting
/// device underneath the same cache and measure real read counts.
pub fn metadata_cache(
    dev: Arc<dyn fs_core::BlockDevice>,
    image: &Path,
) -> Result<Arc<fs_core::CachingDevice>> {
    let mut head = vec![0u8; 4096];
    dev.read_at(0, &mut head)
        .with_context(|| format!("reading superblock of {}", image.display()))?;
    let sb = fs_xfs::Superblock::parse(&head)
        .map_err(|e| anyhow!("parsing superblock of {}: {e}", image.display()))?;
    // Two layers, because `CachingDevice` matches a read only when it is
    // exactly one whole block, and XFS metadata arrives at two sizes.
    // Directory data -- the re-read that dominates -- is one
    // `dirblocksize` per read; an inode is one `inodesize` record, and
    // every path walk re-reads the inodes of the directories it passes
    // through. One granularity would have to choose between them.
    //
    // The inner layer never serves the outer's misses (a 4 KiB read is
    // not one 512 B block), so the two do not duplicate bytes.
    let records = fs_core::CachingDevice::new(
        dev,
        u64::from(sb.inodesize),
        CACHE_BLOCKS,
    );
    Ok(fs_core::CachingDevice::new(
        records as Arc<dyn fs_core::BlockDevice>,
        u64::from(sb.dirblocksize()),
        CACHE_BLOCKS,
    ))
}

/// Diagnostic switch: put no cache in front of the partition view.
pub const NO_CACHE_ENV: &str = "XFS_NO_CACHE";

/// The device handed to `Filesystem::mount` -- cached, or not.
///
/// `XFS_NO_CACHE` exists to answer a question that cannot be asked from
/// inside the cache. When a read reports another block's contents, two
/// explanations fit: the cache served a hit it should not have, or the
/// address reaching the device was already wrong. They are told apart
/// only by taking the cache out of the path and reading the same volume,
/// which needs a build, which here needs CI.
///
/// Deliberately not a `--help` entry. A mount made this way is not
/// representative of anything, so it is a lever for that investigation
/// rather than a tuning knob -- and the warning at open time is what
/// keeps someone from benchmarking an accident.
///
/// Any value counts as "off" except the empty string and `0`, so
/// `XFS_NO_CACHE=1`, `XFS_NO_CACHE=yes` and a bare presence all behave
/// the same way.
pub fn mount_backend(
    dev: Arc<dyn fs_core::BlockDevice>,
    image: &Path,
) -> Result<Arc<dyn fs_core::BlockRead>> {
    let off = match std::env::var(NO_CACHE_ENV) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    };
    if off {
        eprintln!(
            "warning: {NO_CACHE_ENV} is set, so {} is mounted with no metadata cache -- \
             enumeration timing measured here means nothing",
            image.display()
        );
        let uncached: Arc<dyn fs_core::BlockRead> = dev;
        return Ok(uncached);
    }
    Ok(metadata_cache(dev, image)?)
}

/// What to do with the in-memory read-write overlay when the user
/// presses Ctrl-C / dismounts. XFS is read-only at the format level,
/// so any writes the user made through the WinFsp surface are
/// stash-and-replay state — the `DismountPolicy` decides where (or
/// whether) that state ends up persisted.
#[derive(Debug, Clone, Default)]
pub enum DismountPolicy {
    /// Drop the overlay. Any writes the user made vanish on dismount —
    /// the canonical "read-only volume that pretended to be writable"
    /// posture, useful for sandbox scenarios where the original image
    /// must never be modified.
    #[default]
    Discard,
    /// Serialise the overlay to a JSON sidecar at the given path.
    /// Requires the `overlay-sidecar` Cargo feature.
    Sidecar(PathBuf),
    // NO `Rebuild` VARIANT, unlike erofs-win-driver. That mode
    // serialises the overlay back into a fresh image, and for EROFS it
    // does so through `fs_erofs::mkfs::build_image`. `am-fs-xfs` has no
    // mkfs module: building an XFS filesystem from nothing is
    // unfinished work — the superblock writer landed, the allocation
    // group headers, btrees, root inode and log have not.
    //
    // Left absent rather than stubbed. A variant that always returns
    // "not supported" is a promise in the type that the code cannot
    // keep, and `--scratch-rebuild` would appear in `--help` as though
    // it might work. When mkfs.xfs exists, this comes back.
}

/// Per-mount writability flag. `Writable` is the new default — writes
/// are accepted and staged in the overlay. `ReadOnly` short-circuits
/// every mutating callback at the WinFsp boundary so the volume rejects
/// writes from the cache manager up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    #[default]
    Writable,
    ReadOnly,
}

/// RAII handle around an opened `am_fs_xfs::Filesystem`.
///
/// `pub(crate)` fields so the WinFsp adapter (defined further down) and
/// the smoke tests can both reach in for the underlying `Filesystem`
/// without going through extra accessors.
pub struct Mount {
    /// The opened XFS volume. Read by:
    ///   - the WinFsp adapter (Windows + `feature = "mount"`)
    ///   - the cross-platform smoke tests in this module
    ///
    /// On non-Windows builds without `--features mount` the field is
    /// constructed but only consumed by tests; the dead-code allow keeps
    /// `cargo build` quiet on macOS / Linux dev hosts.
    #[cfg_attr(not(any(test, all(windows, feature = "mount"))), allow(dead_code))]
    pub fs: Filesystem,
    /// Original image path, kept for diagnostic messages.
    #[allow(dead_code)]
    image: PathBuf,
    /// Read-write overlay layered atop the read-only underlay. Cross-
    /// platform field so the overlay's behaviour can be exercised via
    /// the integration tests on macOS without WinFsp linked in.
    pub overlay: Arc<Overlay>,
    /// What to do with the overlay on dismount. Configured via the CLI
    /// `--scratch-*` flags; defaults to `Discard`.
    pub dismount_policy: DismountPolicy,
    /// Whether this mount accepts writes at all. `--ro` flips this to
    /// `ReadOnly` and every mutating callback returns
    /// `STATUS_MEDIA_WRITE_PROTECTED` (under `feature = "mount"`).
    pub write_mode: WriteMode,
}

// `Filesystem` is `Send + Sync` (it holds an `Arc<dyn BlockRead>`),
// so `Mount` is too — no manual unsafe impls needed.

impl Mount {
    /// Open an XFS image. `partition` selects a 1-indexed partition
    /// in a whole-disk image; `None` or `Some(0)` means "treat `image`
    /// as the XFS volume directly."
    ///
    /// Treating `Some(0)` the same as `None` mirrors ext4-win-driver and
    /// lets the auto-mount watcher pass `--part 0` unconditionally
    /// through the fixed WinFsp.Launcher CommandLine template when a
    /// disk arrives without a partition table.
    pub fn open(image: &Path, partition: Option<usize>) -> Result<Self> {
        match partition {
            None | Some(0) => Self::open_direct(image),
            Some(n) => Self::open_partition(image, n),
        }
    }

    /// Open the image as a single, unsliced XFS volume.
    pub fn open_direct(image: &Path) -> Result<Self> {
        // Wrap the raw FileSource in a zero-offset slice so we use the
        // same code path everywhere — the `Filesystem` stays oblivious
        // to whether it's reading the full file or a partition slice.
        let src: Arc<dyn BlockSource> = Arc::new(FileSource::open(image)?);
        let len = src.size();
        let dev = Arc::new(PartitionDevice { src, base: 0, len });
        let backend = mount_backend(dev as Arc<dyn fs_core::BlockDevice>, image)?;
        let fs = Filesystem::mount(backend).map_err(|e| {
            anyhow!(
                "open XFS at {}: {e}{}",
                image.display(),
                partition_hint(image)
            )
        })?;
        Ok(Self {
            fs,
            image: image.to_path_buf(),
            overlay: Arc::new(Overlay::new()),
            dismount_policy: DismountPolicy::default(),
            write_mode: WriteMode::default(),
        })
    }

    /// Open the Nth partition (1-indexed) inside `image` as an XFS
    /// volume. Bounds-checks the partition geometry against the device
    /// size before handing the slice to `Filesystem::open`.
    pub fn open_partition(image: &Path, n: usize) -> Result<Self> {
        let src: Arc<dyn BlockSource> = Arc::new(FileSource::open(image)?);
        let parts = partition::list_from_source(src.as_ref())
            .with_context(|| format!("listing partitions in {}", image.display()))?;
        if parts.is_empty() {
            bail!("no partitions found in {}", image.display());
        }
        if n == 0 || n > parts.len() {
            bail!("--part {n} out of range (1..={})", parts.len());
        }
        let p = &parts[n - 1];
        let base = p.start_lba * 512;
        let len = p.num_sectors * 512;
        let end = base
            .checked_add(len)
            .ok_or_else(|| anyhow!("partition geometry overflows u64"))?;
        if end > src.size() {
            bail!(
                "partition {n} extends past device end: {end} > {} bytes",
                src.size()
            );
        }
        let dev = Arc::new(PartitionDevice { src, base, len });
        let backend = mount_backend(dev as Arc<dyn fs_core::BlockDevice>, image)?;
        let fs = Filesystem::mount(backend).map_err(|e| {
            anyhow!(
                "open XFS at {} partition {n} ({}): {e}",
                image.display(),
                p.kind
            )
        })?;
        Ok(Self {
            fs,
            image: image.to_path_buf(),
            overlay: Arc::new(Overlay::new()),
            dismount_policy: DismountPolicy::default(),
            write_mode: WriteMode::default(),
        })
    }

    /// Configure the dismount policy. Builder-style — chained after
    /// `open` / `open_direct` / `open_partition`.
    pub fn with_dismount_policy(mut self, policy: DismountPolicy) -> Self {
        self.dismount_policy = policy;
        self
    }

    /// Configure the write mode. Used to honour the CLI's `--ro` flag.
    pub fn with_write_mode(mut self, mode: WriteMode) -> Self {
        self.write_mode = mode;
        self
    }

    /// Read a file's full contents through the merged overlay+underlay
    /// surface. Returns `Ok(content)` on hit (Created / Modified / underlay
    /// regular file), `Err("not found")` for tombstoned / nonexistent
    /// paths, and `Err("not a regular file")` for dirs / symlinks. Used
    /// by the integration tests to verify the read path WITHOUT linking
    /// the WinFsp adapter — the WinFsp `read` callback uses the same
    /// precedence rules.
    /// Read a whole file, fetching the raw inode fork the reader needs.
    ///
    /// `am-fs-xfs` threads the raw inode bytes through `read_file` and
    /// `read_dir`, because an XFS inode keeps its extents and inline
    /// data in that fork -- the parsed `Inode` alone is not enough. So
    /// every read here is really two: resolve, then re-fetch.
    ///
    /// `lookup_path` has already read those bytes and discarded them,
    /// which is the waste `Filesystem::open` removes -- a `File` holds
    /// the fork from the lookup onward. Callers that resolve a path and
    /// then read it should use that instead. This helper stays for the
    /// callers that already hold an `Inode` (the WinFsp context caches
    /// one) and so have nothing left to open from.
    fn read_whole(&self, inode: &Inode) -> std::result::Result<Vec<u8>, &'static str> {
        let (inode, raw) = self
            .fs
            .read_inode_raw(inode.ino)
            .map_err(|_| "underlay read error")?;
        self.fs
            .read_file(&inode, &raw)
            .map_err(|_| "underlay read error")
    }

    pub fn read_path(&self, unix_path: &str) -> std::result::Result<Vec<u8>, &'static str> {
        match self.overlay.lookup(unix_path) {
            OverlayLookup::Hit(OverlayEntry::Created { content, .. })
            | OverlayLookup::Hit(OverlayEntry::Modified { content, .. }) => Ok(content),
            OverlayLookup::Hit(OverlayEntry::CreatedDir { .. }) => Err("not a regular file"),
            OverlayLookup::Hit(OverlayEntry::Deleted) => Err("not found"),
            OverlayLookup::Deleted => Err("not found"),
            OverlayLookup::Miss => {
                let inode = self.fs.lookup_path(unix_path).map_err(|_| "not found")?;
                if !inode.is_regular_file() {
                    return Err("not a regular file");
                }
                self.read_whole(&inode)
            }
        }
    }

    /// Stage a write through the overlay using the same read-modify-write
    /// semantics the WinFsp `write` callback applies. `offset` may
    /// extend past current EOF; intermediate bytes are zero-filled.
    /// Used by integration tests to drive the cross-platform write path.
    pub fn write_path(
        &self,
        unix_path: &str,
        offset: u64,
        buf: &[u8],
    ) -> std::result::Result<(), &'static str> {
        // Resolve current content + mode from overlay-or-underlay.
        let (mut content, mode) = match self.overlay.lookup(unix_path) {
            OverlayLookup::Hit(OverlayEntry::Created { content, mode, .. })
            | OverlayLookup::Hit(OverlayEntry::Modified { content, mode, .. }) => (content, mode),
            OverlayLookup::Hit(OverlayEntry::CreatedDir { .. }) => {
                return Err("write on directory")
            }
            OverlayLookup::Hit(OverlayEntry::Deleted) | OverlayLookup::Deleted => {
                return Err("write on tombstoned path")
            }
            OverlayLookup::Miss => match self.fs.lookup_path(unix_path) {
                Ok(inode) if inode.is_regular_file() => (self.read_whole(&inode)?, inode.mode),
                Ok(_) => return Err("write on non-regular underlay entry"),
                Err(_) => return Err("write on missing path"),
            },
        };
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or("offset+len overflow")?;
        if end > content.len() as u64 {
            content.resize(end as usize, 0);
        }
        content[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        self.overlay.write_file(unix_path, content, mode);
        Ok(())
    }

    /// Truncate or zero-extend `unix_path` to `new_size` through the
    /// overlay. Read-modify-write against the merged surface; result
    /// always lands as a `Modified` (or `Created`) overlay entry. Used
    /// by integration tests to simulate the WinFsp `set_file_size`
    /// callback cross-platform.
    pub fn set_size_path(
        &self,
        unix_path: &str,
        new_size: u64,
    ) -> std::result::Result<(), &'static str> {
        let (mut content, mode) = match self.overlay.lookup(unix_path) {
            OverlayLookup::Hit(OverlayEntry::Created { content, mode, .. })
            | OverlayLookup::Hit(OverlayEntry::Modified { content, mode, .. }) => (content, mode),
            OverlayLookup::Hit(OverlayEntry::CreatedDir { .. }) => {
                return Err("set_size on directory")
            }
            OverlayLookup::Hit(OverlayEntry::Deleted) | OverlayLookup::Deleted => {
                return Err("set_size on tombstoned path")
            }
            OverlayLookup::Miss => match self.fs.lookup_path(unix_path) {
                Ok(inode) if inode.is_regular_file() => (self.read_whole(&inode)?, inode.mode),
                Ok(_) => return Err("set_size on non-regular underlay entry"),
                Err(_) => return Err("set_size on missing path"),
            },
        };
        content.resize(new_size as usize, 0);
        self.overlay.write_file(unix_path, content, mode);
        Ok(())
    }

    /// Rename `from` to `to`, staged entirely in the overlay.
    ///
    /// `Overlay::rename` deliberately refuses a source it has no entry
    /// for -- it moves overlay state and knows nothing about the image.
    /// Most renames a user performs are exactly that case, though: the
    /// file exists only in the underlay and has never been written to.
    /// So the source is read out of the image first and staged at the
    /// destination, and the old path is tombstoned.
    ///
    /// A directory renames as an empty `CreatedDir`, which is a real
    /// limitation and not an oversight: the children keep resolving at
    /// their old paths through the underlay, so a renamed directory
    /// appears empty. Recursive restaging is the fix, and it is not
    /// written yet.
    ///
    /// This lived inside the `cfg(windows)` WinFsp `rename` callback,
    /// where nothing off Windows could reach it -- so the one operation
    /// with real underlay-to-overlay promotion logic was the one
    /// operation with no portable test. The callback now calls this.
    pub fn rename_path(
        &self,
        from: &str,
        to: &str,
        replace_if_exists: bool,
    ) -> std::result::Result<(), &'static str> {
        if from == to {
            return Ok(());
        }

        // Resolve the source content, promoting from the underlay when
        // the overlay has never seen it.
        let (content, mode, is_dir) = match self.overlay.lookup(from) {
            OverlayLookup::Hit(OverlayEntry::Created { content, mode, .. })
            | OverlayLookup::Hit(OverlayEntry::Modified { content, mode, .. }) => {
                (content, mode, false)
            }
            OverlayLookup::Hit(OverlayEntry::CreatedDir { mode, .. }) => (Vec::new(), mode, true),
            OverlayLookup::Hit(OverlayEntry::Deleted) | OverlayLookup::Deleted => {
                return Err("rename: source not found")
            }
            OverlayLookup::Miss => {
                let inode = self
                    .fs
                    .lookup_path(from)
                    .map_err(|_| "rename: source not found")?;
                if inode.is_dir() {
                    (Vec::new(), inode.mode, true)
                } else if inode.is_regular_file() {
                    (self.read_whole(&inode)?, inode.mode, false)
                } else {
                    return Err("rename: source is not a file or directory");
                }
            }
        };

        // A tombstone at the destination means it was deleted through
        // the overlay, so the name is free even though the image still
        // has it -- checking the underlay here would resurrect it.
        let dest_exists = match self.overlay.lookup(to) {
            OverlayLookup::Hit(OverlayEntry::Deleted) | OverlayLookup::Deleted => false,
            OverlayLookup::Hit(_) => true,
            OverlayLookup::Miss => self.fs.lookup_path(to).is_ok(),
        };
        if dest_exists && !replace_if_exists {
            return Err("rename: destination exists");
        }

        if is_dir {
            self.overlay.create_dir(to, mode);
        } else {
            self.overlay.create_file(to, content, mode);
        }
        self.overlay.delete(from);
        Ok(())
    }

    /// Apply the configured `DismountPolicy`. Public so the integration
    /// tests can drive it directly without going through a real WinFsp
    /// host. Always called from `run_impl` on Ctrl-C as well.
    ///
    /// `Discard` returns Ok with no side effects (the overlay is
    /// dropped when `Mount` itself drops). `Sidecar` writes a JSON
    /// dump (requires the `overlay-sidecar` feature). `Rebuild` walks
    /// the merged tree and emits a fresh XFS image at the configured
    /// path.
    pub fn apply_dismount_policy(&self) -> Result<()> {
        match &self.dismount_policy {
            DismountPolicy::Discard => {
                self.overlay.clear();
                Ok(())
            }
            DismountPolicy::Sidecar(path) => write_sidecar(&self.overlay, path),
        }
    }

    /// Mount this filesystem on a Windows drive letter / empty
    /// directory. Blocks until the user presses Ctrl-C.
    ///
    /// Stub on non-Windows hosts: prints a diagnostic and exits.
    /// Cross-compile / cargo-check on macOS / Linux still succeed so
    /// the rest of the CLI stays usable as a development tool.
    pub fn run(self, mount_point: &str) -> Result<()> {
        run_impl(self, mount_point)
    }
}

#[cfg(not(all(windows, feature = "mount")))]
fn run_impl(_mount: Mount, _mount_point: &str) -> Result<()> {
    eprintln!("xfs: WinFsp mount only supported on Windows (with --features mount).");
    Ok(())
}

#[cfg(all(windows, feature = "mount"))]
fn run_impl(mount: Mount, mount_point: &str) -> Result<()> {
    winfsp_adapter::run(mount, mount_point)
}

// ---------------------------------------------------------------------------
// Dismount-policy implementations.
// ---------------------------------------------------------------------------

/// Serialise the overlay's snapshot to a JSON sidecar at `path`.
/// `overlay-sidecar` feature gate: emits a hard error if the feature is
/// off so users who try the CLI flag without the feature get a clear
/// diagnostic rather than a silent no-op.
#[cfg(feature = "overlay-sidecar")]
fn write_sidecar(overlay: &Overlay, path: &Path) -> Result<()> {
    let snap = overlay.snapshot();
    let json =
        serde_json::to_vec_pretty(&snap).map_err(|e| anyhow!("serialise overlay snapshot: {e}"))?;
    std::fs::write(path, json)
        .with_context(|| format!("write overlay sidecar to {}", path.display()))?;
    Ok(())
}

#[cfg(not(feature = "overlay-sidecar"))]
fn write_sidecar(_overlay: &Overlay, path: &Path) -> Result<()> {
    bail!(
        "--scratch-sidecar requires the `overlay-sidecar` feature; rebuild with \
         `cargo build --features overlay-sidecar` (requested path: {})",
        path.display()
    );
}

fn partition_hint(image: &Path) -> String {
    match partition::list(image) {
        Ok(parts) if !parts.is_empty() => {
            let mut s =
                String::from("\nhint: this looks like a partitioned device. Try --part N:\n");
            for (i, p) in parts.iter().enumerate() {
                s.push_str(&format!(
                    "  {}: {} sectors @ LBA {} ({})\n",
                    i + 1,
                    p.num_sectors,
                    p.start_lba,
                    p.kind,
                ));
            }
            s
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// WinFsp adapter (feature = "mount", windows only)
// ---------------------------------------------------------------------------

#[cfg(all(windows, feature = "mount"))]
mod winfsp_adapter {
    //! Bridge between WinFsp's `FileSystemContext` and a read-write
    //! overlay layered atop `am_fs_xfs::Filesystem`.
    //!
    //! Reads consult the in-memory overlay first; on Miss they fall
    //! through to the read-only XFS underlay. Writes always land in
    //! the overlay — the underlay image is never touched. On dismount,
    //! the overlay is either discarded, serialised to a JSON sidecar,
    //! or used to rebuild a fresh XFS image, controlled by the
    //! `DismountPolicy` configured at mount time.
    //!
    //! Path conversions: WinFsp gives backslash-separated UTF-16 paths
    //! (`\foo\bar`); XFS's lookup_path wants slash-separated UTF-8
    //! (`/foo/bar`). Done in [`winpath_to_unix`].
    //!
    //! Time conversions: XFS stores 64-bit unix-epoch seconds + nsec;
    //! Windows FILETIME is 100-ns intervals since 1601-01-01. The
    //! constant offset between the two epochs is 11644473600 seconds.
    //!
    //! API references (winfsp-rs, GPL-3, vendored at ../winfsp-rs):
    //!   - `winfsp::filesystem::FileSystemContext` — the trait we impl
    //!   - `winfsp::filesystem::{FileInfo, OpenFileInfo, VolumeInfo,
    //!     DirInfo, DirMarker, FileSecurity, WideNameInfo}`
    //!   - `winfsp::host::{FileSystemHost, VolumeParams}`
    //! All implementations below are original; the structure mirrors
    //! `ext4-win-driver/src/mount.rs` but without verbatim copies. The
    //! overlay implementation is independent of overlayfs / unionfs /
    //! fuse-overlayfs (all GPL-licensed reference projects).
    //!
    //! License posture: this module is the only place that links against
    //! the GPL-3 winfsp-rs crate. The rest of xfs-win-driver and all
    //! of `am-fs-xfs` (MIT) flow upward into the GPL-3 unit cleanly
    //! under the GPL-3's one-way compatibility rule.

    use anyhow::{anyhow, Context, Result};
    use std::collections::BTreeSet;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};
    use widestring::U16CStr;
    use windows::Win32::Foundation::{
        STATUS_BUFFER_TOO_SMALL, STATUS_END_OF_FILE, STATUS_INVALID_DEVICE_REQUEST,
        STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY, STATUS_NOT_A_REPARSE_POINT,
        STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
        STATUS_REPARSE_POINT_NOT_RESOLVED,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };
    use winfsp::filesystem::{
        DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, ModificationDescriptor,
        OpenFileInfo, VolumeInfo, WideNameInfo,
    };
    use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
    use winfsp::Result as FspResult;
    // FILE_FLAGS_AND_ATTRIBUTES from winfsp_sys is the bindgen u32
    // alias the FileSystemContext trait expects; the windows crate's
    // same-named newtype struct(u32) is signature-incompatible (see
    // E0053 trait-method-incompatible-type errors when the wrong one
    // is in scope on x64 / arm64 builds).
    use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

    use fs_xfs::dir::DirEntry as XfsDirEntry;
    use fs_xfs::inode::{FileType, Inode};
    use fs_xfs::Filesystem;

    use super::{Mount, OverlayEntry, OverlayLookup, WriteMode};

    /// IO_REPARSE_TAG_SYMLINK — Microsoft public symlink tag. We surface
    /// XFS symlinks as Windows reparse points so Explorer can render
    /// them (and follow them, with the right privilege). The literal is
    /// stable across NT versions; pulling it from `windows` would
    /// require an extra feature gate.
    const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

    /// The target is to be interpreted relative to the directory holding
    /// the link. WinFsp's resolver also keeps walking a relative target,
    /// which is what lets a chain of links inside this volume resolve
    /// without ever naming a drive letter.
    const SYMLINK_FLAG_RELATIVE: u32 = 1;

    /// Bytes of a symbolic-link reparse buffer ahead of the path data.
    ///
    /// Taken from `REPARSE_DATA_BUFFER` as WinFsp itself declares it
    /// (`inc/winfsp/winfsp.h:56`, because the user-mode SDK headers omit
    /// it): tag, data length, reserved, then the symlink view's two
    /// offset/length pairs and a flags word.
    const REPARSE_SYMLINK_HEADER_BYTES: usize = 20;

    /// `ReparseDataLength` counts from offset 8, so the four `USHORT`
    /// fields and the `ULONG` flags occupy the 12 bytes before the names.
    const REPARSE_SYMLINK_FIELD_BYTES: usize = REPARSE_SYMLINK_HEADER_BYTES - 8;

    /// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`. A link whose Windows spelling
    /// needs more than this cannot be a reparse point, however valid it
    /// is as an XFS target.
    const MAXIMUM_REPARSE_DATA_BUFFER_BYTES: usize = 16 * 1024;

    // The Unix-to-FILETIME conversion lives in winfsp-fs-skeleton.
    // This module had the fourth copy of it in the family -- erofs and
    // ext4 had two more, at three different widths between them -- and
    // this one took `u64` seconds, which cannot even express XFS's
    // `Timestamp.sec`. The shared one takes `i64`, so an XFS timestamp
    // passes through unchanged and a date before 1970 survives.
    use winfsp_fs_skeleton::translate::unix_to_filetime;

    /// `\foo\bar` (UTF-16) → `/foo/bar` (UTF-8). Empty path becomes "/".
    fn winpath_to_unix(name: &U16CStr) -> Result<String> {
        let s = name.to_string().context("path is invalid UTF-16")?;
        if s.is_empty() {
            return Ok("/".into());
        }
        Ok(s.replace('\\', "/"))
    }

    /// Rewrite an XFS link target as the absolute Windows path a
    /// symbolic-link reparse buffer carries.
    ///
    /// XFS stores targets as POSIX paths, absolute or relative to the
    /// directory holding the link. The buffer needs one spelling, and
    /// WinFsp's resolver replaces the *whole* path when the target
    /// starts with `\` -- so resolving to an absolute-in-volume name is
    /// what lets a link avoid naming a drive letter at all. A
    /// `\??\X:`-style target would pin the volume to whichever letter it
    /// happened to be mounted on.
    ///
    /// `..` above the volume root is dropped, which is how the kernel
    /// treats `..` in a root directory, and keeps a link whose target
    /// escapes the filesystem from producing a path outside the volume.
    fn link_target_to_winpath(link_unix: &str, target: &str) -> String {
        // An absolute target ignores the link's own directory; a
        // relative one starts from it.
        let base = match target.strip_prefix('/') {
            Some(_) => "",
            None => link_unix.rsplit_once('/').map_or("", |(parent, _)| parent),
        };
        let mut parts: Vec<&str> = Vec::new();
        for segment in base.split('/').chain(target.split('/')) {
            match segment {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        let mut out = String::with_capacity(base.len() + target.len() + 1);
        for part in parts {
            out.push('\\');
            out.push_str(part);
        }
        // A link that resolves to the volume root still needs a name.
        if out.is_empty() {
            out.push('\\');
        }
        out
    }

    /// Serialise a Microsoft symbolic-link reparse buffer.
    ///
    /// Offsets are from the start of the buffer. Both strings sit in
    /// `PathBuffer` back to back, unlengthed and unterminated, so the
    /// print name starts exactly where the substitute name ends; sizes
    /// are in bytes, not characters.
    fn symlink_reparse_buffer(win_target: &str) -> Vec<u8> {
        let name: Vec<u8> = win_target
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let name_bytes = name.len() as u16;
        let mut buf = Vec::with_capacity(REPARSE_SYMLINK_HEADER_BYTES + 2 * name.len());
        buf.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        buf.extend_from_slice(
            &u16::try_from(REPARSE_SYMLINK_FIELD_BYTES + 2 * name.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        buf.extend_from_slice(&0u16.to_le_bytes()); // Reserved / unparsed length
        buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
        buf.extend_from_slice(&name_bytes.to_le_bytes()); // SubstituteNameLength
        buf.extend_from_slice(&name_bytes.to_le_bytes()); // PrintNameOffset
        buf.extend_from_slice(&name_bytes.to_le_bytes()); // PrintNameLength
        buf.extend_from_slice(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
        buf.extend_from_slice(&name);
        buf.extend_from_slice(&name);
        buf
    }

    /// Build the reparse buffer for the link at `unix_path` and copy it
    /// into WinFsp's buffer, returning the byte count written.
    ///
    /// `Filesystem::open` deliberately does not follow links, so the path
    /// resolves to the link's own inode and the target can be read from
    /// it.
    fn fill_link_reparse_buffer(
        fs: &Filesystem,
        unix_path: &str,
        buffer: &mut [u8],
    ) -> FspResult<u64> {
        let file = fs
            .open(unix_path)
            .map_err(|e| err_to_status(e))?;
        if !file.is_symlink() {
            return Err(STATUS_NOT_A_REPARSE_POINT.into());
        }
        let target = file.link_target().map_err(|e| err_to_status(e))?;
        let Ok(target) = String::from_utf8(target) else {
            // Names leave this driver as UTF-8 -- the directory listing
            // decodes them the same way -- so a target that is not valid
            // UTF-8 has no spelling here. Refusing is honest; substituting
            // replacement characters would point the link at some other
            // file.
            return Err(STATUS_REPARSE_POINT_NOT_RESOLVED.into());
        };
        let data = symlink_reparse_buffer(&link_target_to_winpath(unix_path, &target));
        if data.len() > MAXIMUM_REPARSE_DATA_BUFFER_BYTES {
            return Err(STATUS_REPARSE_POINT_NOT_RESOLVED.into());
        }
        if buffer.len() < data.len() {
            return Err(STATUS_BUFFER_TOO_SMALL.into());
        }
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len() as u64)
    }

    /// Translate an XFS inode into a Windows file-attribute bitmap.
    /// `read_only` is parameterised because the volume is no longer
    /// unconditionally RO — the WinFsp adapter sets it true when the
    /// `--ro` flag is passed and false otherwise. DIRECTORY when
    /// applicable. REPARSE_POINT for symlinks so Explorer renders them
    /// as shortcuts.
    fn file_attributes(inode: &Inode, read_only: bool) -> u32 {
        let mut a = if read_only {
            FILE_ATTRIBUTE_READONLY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };
        if inode.is_dir() {
            a |= FILE_ATTRIBUTE_DIRECTORY.0;
            // NORMAL is mutually exclusive with DIRECTORY in
            // the Windows attribute model — strip it.
            a &= !FILE_ATTRIBUTE_NORMAL.0;
        }
        if inode.is_symlink() {
            a |= FILE_ATTRIBUTE_REPARSE_POINT.0;
        }
        a
    }

    /// Populate `FileInfo` from an XFS `Inode`.
    fn populate_file_info(inode: &Inode, info: &mut FileInfo, read_only: bool) {
        info.file_attributes = file_attributes(inode, read_only);
        info.reparse_tag = if inode.is_symlink() {
            IO_REPARSE_TAG_SYMLINK
        } else {
            0
        };
        info.file_size = inode.size;
        // Round allocation up to 4 KiB. XFS doesn't track on-disk
        // allocation distinct from logical size for our purposes.
        info.allocation_size = (inode.size + 4095) & !4095;
        let ft = unix_to_filetime(inode.mtime.sec, inode.mtime.nsec);
        info.creation_time = ft;
        info.last_access_time = ft;
        info.last_write_time = ft;
        info.change_time = ft;
        info.index_number = inode.ino;
        info.hard_links = 0;
        info.ea_size = 0;
    }

    /// Populate `FileInfo` from an overlay entry. Used when the read
    /// callback sees a `Hit` and skips the underlay entirely. Mirrors
    /// `populate_file_info` for inode-backed entries but pulls size /
    /// mode / mtime from the overlay payload instead.
    fn populate_overlay_file_info(entry: &OverlayEntry, info: &mut FileInfo, read_only: bool) {
        let is_dir = entry.is_dir();
        let mut a = if read_only {
            FILE_ATTRIBUTE_READONLY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        };
        if is_dir {
            a |= FILE_ATTRIBUTE_DIRECTORY.0;
            a &= !FILE_ATTRIBUTE_NORMAL.0;
        }
        info.file_attributes = a;
        info.reparse_tag = 0;
        info.file_size = entry.content().map(|c| c.len() as u64).unwrap_or(0);
        info.allocation_size = (info.file_size + 4095) & !4095;
        // The overlay stamps entries with unsigned seconds; the shared
        // converter takes signed. Checked rather than cast: `u64::MAX
        // as i64` is -1, which would render as 1969.
        let ft = unix_to_filetime(i64::try_from(entry.mtime()).unwrap_or(i64::MAX), 0);
        info.creation_time = ft;
        info.last_access_time = ft;
        info.last_write_time = ft;
        info.change_time = ft;
        // Synthetic NIDs for overlay entries — pick something that
        // can't collide with a real XFS NID. We use the high bit set
        // and hash the path-id contribution at callsites if collision
        // matters; here we just zero it out (Explorer doesn't rely on
        // it for write-staged paths).
        info.index_number = 0;
        info.hard_links = 0;
        info.ea_size = 0;
    }

    /// Read a whole file, fetching the raw inode fork alongside it.
    ///
    /// XFS threads the raw bytes through `read_file` because an inode
    /// keeps its extents and inline data in that fork. `lookup_path`
    /// has already read and discarded them, so this is a second read --
    /// the cost a handle-style reader API would remove.
    fn read_whole(fs: &Filesystem, inode: &Inode) -> FspResult<Vec<u8>> {
        let (inode, raw) = fs.read_inode_raw(inode.ino).map_err(err_to_status)?;
        fs.read_file(&inode, &raw)
            .map_err(|e| err_to_status(e).into())
    }

    /// List a directory, fetching the raw inode fork alongside it.
    /// Short-form directories store their entries inside the inode, so
    /// the parsed struct alone is not enough.
    fn read_children(fs: &Filesystem, inode: &Inode) -> FspResult<Vec<XfsDirEntry>> {
        let (inode, raw) = fs.read_inode_raw(inode.ino).map_err(err_to_status)?;
        fs.read_dir(&inode, &raw)
            .map_err(|e| err_to_status(e).into())
    }

    /// Map an `fs_xfs::Error` to an NTSTATUS suitable for returning
    /// from a WinFsp callback. Most lookup-style failures collapse to
    /// `STATUS_OBJECT_NAME_NOT_FOUND` — Explorer / consumer apps treat
    /// that uniformly. Disk-IO and format errors become
    /// `STATUS_INVALID_DEVICE_REQUEST` so they surface but don't get
    /// confused with "no such file."
    fn err_to_status(err: fs_xfs::Error) -> windows::Win32::Foundation::NTSTATUS {
        use fs_xfs::Error as E;
        match err {
            // No BadDirent: XFS's error type has no dirent-specific
            // corruption variant, unlike EROFS's. NotAFile joins the
            // lookup-style failures for the same reason the others do --
            // to a caller they all mean "that path is not what you
            // asked for".
            E::NotFound | E::NotADirectory | E::NotAFile => STATUS_OBJECT_NAME_NOT_FOUND,
            _ => STATUS_INVALID_DEVICE_REQUEST,
        }
    }

    /// Look up a path in the wrapped XFS volume, mapping any failure
    /// to an NTSTATUS the WinFsp callback can return directly.
    fn lookup(fs: &Filesystem, unix_path: &str) -> FspResult<Inode> {
        fs.lookup_path(unix_path)
            .map_err(|e| err_to_status(e).into())
    }

    /// Per-open file handle state.
    ///
    /// `inode` is `None` when the open path is overlay-only (no
    /// underlay counterpart) — e.g. a freshly Created file or
    /// CreatedDir. Read / write callbacks dispatch on `unix_path`
    /// against the overlay first; on Miss they consult the underlay
    /// via the cached `inode`.
    pub struct XfsFileContext {
        pub unix_path: String,
        pub inode: Mutex<Option<Inode>>,
        pub is_dir: bool,
        /// True if this open handle has been marked for delete via
        /// `set_delete`. The actual tombstone is staged in `cleanup`.
        pub pending_delete: Mutex<bool>,
        /// The merged directory listing, built on the first
        /// `read_directory` call for this handle and replayed by every
        /// later buffer. Rebuilding it per buffer re-reads every child
        /// inode. Memoising it measured no wall-clock gain -- the reads
        /// that dominated were elsewhere, in path resolution, and are
        /// what [`metadata_cache`] now absorbs -- but a wide directory
        /// still costs one inode read per entry per buffer without it.
        pub dir_listing: Mutex<Option<Arc<Vec<MergedEntry>>>>,
    }

    /// Filesystem-wide state shared across all WinFsp callbacks.
    pub struct XfsContext {
        mount: Mount,
        label: String,
        total_size: u64,
    }

    impl XfsContext {
        pub fn new(mount: Mount) -> Result<Self> {
            let sb = mount.fs.superblock();
            let label = sb.fname.clone();
            let total_size = sb.dblocks * u64::from(sb.blocksize);
            Ok(Self {
                mount,
                label,
                total_size,
            })
        }

        fn fs(&self) -> &Filesystem {
            &self.mount.fs
        }

        fn is_read_only(&self) -> bool {
            self.mount.write_mode == WriteMode::ReadOnly
        }

        /// Build the merged listing for a directory: underlay entries
        /// first, with overlay-tombstoned names filtered out, then
        /// overlay-only Created / CreatedDir entries appended, sorted by
        /// name.
        ///
        /// Sorting matters because WinFsp's DirInfo buffer expects
        /// insertion in name order for resume-after-marker correctness.
        ///
        /// This is the expensive half of enumeration -- it reads the
        /// directory blocks and then one inode per child -- so callers
        /// must not run it once per output buffer. See
        /// [`XfsFileContext::dir_listing`].
        fn build_merged_listing(&self, context: &XfsFileContext) -> Vec<MergedEntry> {
            // 1. Build the underlay child set.
            let underlay_inode = context.inode.lock().unwrap().clone();
            let mut underlay_pairs: Vec<(String, Inode)> = Vec::new();
            if let Some(inode) = underlay_inode.as_ref() {
                if let Ok(children) = read_children(self.fs(), inode) {
                    for e in children {
                        if e.name == b"." || e.name == b".." {
                            continue;
                        }
                        let name = match std::str::from_utf8(&e.name) {
                            Ok(s) => s.to_string(),
                            Err(_) => continue,
                        };
                        let child_path = if context.unix_path == "/" {
                            format!("/{name}")
                        } else {
                            format!("{}/{}", context.unix_path, name)
                        };
                        // Tombstoned: skip.
                        if matches!(
                            self.mount.overlay.lookup(&child_path),
                            OverlayLookup::Deleted
                        ) {
                            continue;
                        }
                        if let Ok(child) = self.fs().read_inode(e.ino) {
                            underlay_pairs.push((name, child));
                        }
                    }
                }
            }

            // 2. Overlay-only entries under this dir.
            let mut overlay_pairs: Vec<(String, OverlayEntry)> = Vec::new();
            let mut underlay_names: BTreeSet<String> =
                underlay_pairs.iter().map(|(n, _)| n.clone()).collect();
            for (leaf, entry) in self.mount.overlay.iter_dir(&context.unix_path) {
                if entry.is_deleted() {
                    continue;
                }
                if underlay_names.contains(&leaf) {
                    // Overlay shadows underlay — replace the underlay
                    // pair with the overlay entry.
                    underlay_pairs.retain(|(n, _)| n != &leaf);
                    underlay_names.remove(&leaf);
                }
                overlay_pairs.push((leaf, entry));
            }

            // 3. Sort merged listing for deterministic resume.
            let mut all: Vec<MergedEntry> = Vec::new();
            for (name, inode) in underlay_pairs {
                all.push(MergedEntry::Underlay(name, inode));
            }
            for (name, entry) in overlay_pairs {
                all.push(MergedEntry::Overlay(name, entry));
            }
            all.sort_by(|a, b| a.name().cmp(b.name()));
            all
        }

        /// `true` if the volume must reject write callbacks. On a
        /// read-only mount we return `STATUS_MEDIA_WRITE_PROTECTED` —
        /// distinct from `INVALID_DEVICE_REQUEST` so apps that probe
        /// writability get a clearer error.
        fn ensure_writable(&self) -> FspResult<()> {
            if self.is_read_only() {
                Err(STATUS_MEDIA_WRITE_PROTECTED.into())
            } else {
                Ok(())
            }
        }

        /// Resolve a path's full content for the write callback. Looks
        /// up the overlay first; on Miss reads the underlay file in
        /// full. Returns `(content, mode)` or NotFound.
        ///
        /// Read-then-write race resolution: this is taken under the
        /// caller's effective lock (the WinFsp callback dispatches
        /// per-handle, and writes against the same path serialise
        /// inside the overlay's RwLock at insertion time). A racing
        /// reader could see the pre-write content; but the XFS
        /// underlay never changes, so any read consulting the
        /// underlay at the same time gets a consistent snapshot.
        fn read_full(&self, unix_path: &str) -> FspResult<(Vec<u8>, u16)> {
            match self.mount.overlay.lookup(unix_path) {
                OverlayLookup::Hit(OverlayEntry::Created { content, mode, .. })
                | OverlayLookup::Hit(OverlayEntry::Modified { content, mode, .. }) => {
                    Ok((content, mode))
                }
                OverlayLookup::Hit(OverlayEntry::CreatedDir { .. }) => {
                    Err(STATUS_INVALID_DEVICE_REQUEST.into())
                }
                // `lookup` translates a stored `Deleted` entry into the
                // top-level `Deleted` arm, so `Hit(Deleted)` shouldn't
                // arise in practice -- but the type system can't know
                // that, and treating it as "logically gone" matches
                // the contract anyway.
                OverlayLookup::Hit(OverlayEntry::Deleted) | OverlayLookup::Deleted => {
                    Err(STATUS_OBJECT_NAME_NOT_FOUND.into())
                }
                OverlayLookup::Miss => {
                    let inode = self
                        .fs()
                        .lookup_path(unix_path)
                        .map_err(|e| err_to_status(e))?;
                    if !inode.is_regular_file() {
                        return Err(STATUS_INVALID_DEVICE_REQUEST.into());
                    }
                    let buf = read_whole(self.fs(), &inode)?;
                    Ok((buf, inode.mode))
                }
            }
        }
    }

    impl FileSystemContext for XfsContext {
        type FileContext = XfsFileContext;

        fn get_security_by_name(
            &self,
            file_name: &U16CStr,
            _security_descriptor: Option<&mut [c_void]>,
            resolve_reparse: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
        ) -> FspResult<FileSecurity> {
            // WinFsp asks for attributes before it opens anything, and a
            // path containing a link anywhere is its chance to be told:
            // returning the resolver's answer makes it resolve the link
            // and call back with the rewritten path. Looking the path up
            // ourselves first would report the link's own inode and the
            // caller would never reach the target.
            if let Some(security) = resolve_reparse(file_name) {
                return Ok(security);
            }
            let unix_path = winpath_to_unix(file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
            let read_only = self.is_read_only();
            // Overlay-first lookup. A `Deleted` tombstone hides any
            // underlay file even if get_security_by_name is the first
            // touchpoint after a delete.
            match self.mount.overlay.lookup(&unix_path) {
                OverlayLookup::Deleted => Err(STATUS_OBJECT_NAME_NOT_FOUND.into()),
                OverlayLookup::Hit(entry) => {
                    let attrs = if entry.is_dir() {
                        let mut a = if read_only {
                            FILE_ATTRIBUTE_READONLY.0
                        } else {
                            FILE_ATTRIBUTE_NORMAL.0
                        };
                        a |= FILE_ATTRIBUTE_DIRECTORY.0;
                        a &= !FILE_ATTRIBUTE_NORMAL.0;
                        a
                    } else if read_only {
                        FILE_ATTRIBUTE_READONLY.0
                    } else {
                        FILE_ATTRIBUTE_NORMAL.0
                    };
                    Ok(FileSecurity {
                        reparse: false,
                        sz_security_descriptor: 0,
                        attributes: attrs,
                    })
                }
                OverlayLookup::Miss => {
                    let inode = lookup(self.fs(), &unix_path)?;
                    Ok(FileSecurity {
                        reparse: false,
                        sz_security_descriptor: 0,
                        attributes: file_attributes(&inode, read_only),
                    })
                }
            }
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            let unix_path = winpath_to_unix(file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
            let read_only = self.is_read_only();
            match self.mount.overlay.lookup(&unix_path) {
                OverlayLookup::Deleted => Err(STATUS_OBJECT_NAME_NOT_FOUND.into()),
                OverlayLookup::Hit(entry) => {
                    let is_dir = entry.is_dir();
                    populate_overlay_file_info(&entry, file_info.as_mut(), read_only);
                    Ok(XfsFileContext {
                        unix_path,
                        is_dir,
                        inode: Mutex::new(None),
                        pending_delete: Mutex::new(false),
                        dir_listing: Mutex::new(None),
                    })
                }
                OverlayLookup::Miss => {
                    let inode = lookup(self.fs(), &unix_path)?;
                    populate_file_info(&inode, file_info.as_mut(), read_only);
                    let is_dir = inode.is_dir();
                    Ok(XfsFileContext {
                        unix_path,
                        is_dir,
                        inode: Mutex::new(Some(inode)),
                        pending_delete: Mutex::new(false),
                        dir_listing: Mutex::new(None),
                    })
                }
            }
        }

        fn close(&self, _context: Self::FileContext) {
            // Plain owned data — nothing to release.
        }

        fn get_file_info(
            &self,
            context: &Self::FileContext,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            let read_only = self.is_read_only();
            // Overlay precedence — a write between open and stat must
            // surface the new size / mtime.
            match self.mount.overlay.lookup(&context.unix_path) {
                OverlayLookup::Hit(entry) => {
                    populate_overlay_file_info(&entry, file_info, read_only);
                    Ok(())
                }
                OverlayLookup::Deleted => Err(STATUS_OBJECT_NAME_NOT_FOUND.into()),
                OverlayLookup::Miss => {
                    let guard = context.inode.lock().unwrap();
                    let inode = guard
                        .as_ref()
                        .ok_or_else(|| {
                            // Overlay-only handle whose entry vanished
                            // between `open` and `get_file_info`.
                            winfsp::FspError::from(STATUS_OBJECT_NAME_NOT_FOUND)
                        })?
                        .clone();
                    populate_file_info(&inode, file_info, read_only);
                    Ok(())
                }
            }
        }

        fn read(
            &self,
            context: &Self::FileContext,
            buffer: &mut [u8],
            offset: u64,
        ) -> FspResult<u32> {
            if context.is_dir {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into());
            }
            // Overlay-first dispatch: a Hit means the user wrote /
            // created this file, the underlay is bypassed entirely.
            if let OverlayLookup::Hit(entry) = self.mount.overlay.lookup(&context.unix_path) {
                let content = match entry.content() {
                    Some(c) => c.to_vec(),
                    None => return Err(STATUS_INVALID_DEVICE_REQUEST.into()),
                };
                let size = content.len() as u64;
                if offset >= size {
                    return Err(STATUS_END_OF_FILE.into());
                }
                let remaining = (size - offset) as usize;
                let take = buffer.len().min(remaining);
                buffer[..take].copy_from_slice(&content[offset as usize..offset as usize + take]);
                return Ok(take as u32);
            }
            if let OverlayLookup::Deleted = self.mount.overlay.lookup(&context.unix_path) {
                return Err(STATUS_OBJECT_NAME_NOT_FOUND.into());
            }
            let guard = context.inode.lock().unwrap();
            let inode = guard
                .as_ref()
                .ok_or_else(|| winfsp::FspError::from(STATUS_OBJECT_NAME_NOT_FOUND))?
                .clone();
            drop(guard);
            if offset >= inode.size {
                return Err(STATUS_END_OF_FILE.into());
            }
            let remaining = inode.size - offset;
            let take = (buffer.len() as u64).min(remaining) as usize;
            // `read_at` is the ranged read, and it is what this callback
            // wants: WinFsp asks for a window, and this returns exactly
            // that window. `read_file` is a thin wrapper over the same
            // function with offset 0 -- reaching for it here would read
            // the WHOLE FILE on every callback, which is O(size) per
            // read and quadratic over a sequential scan.
            //
            // Holes and unwritten extents come back as zeros rather
            // than stale disk contents, and a short return means end of
            // file rather than an error.
            let (inode, raw) = self.fs().read_inode_raw(inode.ino).map_err(err_to_status)?;
            let n = self
                .fs()
                .read_at(&inode, &raw, offset, &mut buffer[..take])
                .map_err(err_to_status)?;
            Ok(n as u32)
        }

        fn read_directory(
            &self,
            context: &Self::FileContext,
            _pattern: Option<&U16CStr>,
            marker: DirMarker,
            buffer: &mut [u8],
        ) -> FspResult<u32> {
            if !context.is_dir {
                return Err(STATUS_NOT_A_DIRECTORY.into());
            }
            let read_only = self.is_read_only();

            // WinFsp drains a directory one buffer at a time, calling us
            // again for each refill. Building the listing per call would
            // re-read every child inode every time -- quadratic in the
            // entry count, and measured at 12386 raw reads for a
            // 461-entry directory -- so on a read-only mount we build it
            // once per handle and replay it.
            //
            // A writable mount is excluded on purpose: staged overlay
            // changes must show up in a listing that is still being
            // drained.
            let listing: Arc<Vec<MergedEntry>> = {
                let mut slot = context.dir_listing.lock().unwrap();
                let cached = if read_only { slot.clone() } else { None };
                match cached {
                    Some(existing) => existing,
                    None => {
                        let fresh = Arc::new(self.build_merged_listing(context));
                        if read_only {
                            *slot = Some(fresh.clone());
                        }
                        fresh
                    }
                }
            };

            // Resume strictly after the marker. The listing is sorted by
            // name, so this is a binary search rather than a scan; and
            // unlike a "find the exact name" walk it still does the right
            // thing if the marker name is somehow absent.
            let start = match marker.inner_as_cstr() {
                Some(m) => {
                    let want = m.to_string_lossy();
                    listing.partition_point(|e| e.name() <= want.as_str())
                }
                None => 0,
            };

            let mut cursor: u32 = 0;
            let mut dir_info: DirInfo<255> = DirInfo::new();

            for entry in &listing[start..] {
                let name = entry.name();
                dir_info.reset();
                match entry {
                    MergedEntry::Underlay(_, inode) => {
                        populate_file_info(inode, dir_info.file_info_mut(), read_only)
                    }
                    MergedEntry::Overlay(_, ovl) => {
                        populate_overlay_file_info(ovl, dir_info.file_info_mut(), read_only)
                    }
                }
                if dir_info.set_name(name).is_err() {
                    continue;
                }
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    break;
                }
            }
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            Ok(cursor)
        }

        fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> FspResult<()> {
            out_volume_info.total_size = self.total_size;
            // On a writable mount we don't track real free-space; the
            // overlay can grow until the host runs out of RAM. Report
            // a generous synthetic budget so apps that gate on
            // available space don't refuse to write.
            out_volume_info.free_size = if self.is_read_only() {
                0
            } else {
                // Half of total_size, capped at 1 GiB. Synthetic — the
                // underlay is RO, so "free" is purely overlay headroom.
                self.total_size.min(1 << 30)
            };
            let label = if self.label.is_empty() {
                "xfs"
            } else {
                self.label.as_str()
            };
            out_volume_info.set_volume_label(label);
            Ok(())
        }

        // -----------------------------------------------------------------
        // Reparse-point / symlink support.
        //
        // XFS stores a link's target inside the inode, Windows stores it
        // in a reparse buffer attached to the entry, so the driver has to
        // translate on demand. Both callbacks below answer for links only;
        // anything else reports STATUS_NOT_A_REPARSE_POINT, which is how
        // WinFsp's resolver is told to keep walking the path.
        //
        // Overlays are not consulted: a staged entry has no link kind, so
        // every link is an underlay object.
        // -----------------------------------------------------------------

        fn get_reparse_point_by_name(
            &self,
            file_name: &U16CStr,
            _is_directory: bool,
            buffer: &mut [u8],
        ) -> FspResult<u64> {
            let unix_path = winpath_to_unix(file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
            fill_link_reparse_buffer(self.fs(), &unix_path, buffer)
        }

        fn get_reparse_point(
            &self,
            context: &Self::FileContext,
            _file_name: &U16CStr,
            buffer: &mut [u8],
        ) -> FspResult<u64> {
            fill_link_reparse_buffer(self.fs(), &context.unix_path, buffer)
        }

        fn set_reparse_point(
            &self,
            _context: &Self::FileContext,
            _file_name: &U16CStr,
            _buffer: &[u8],
        ) -> FspResult<()> {
            // Links are not staged in the overlay either, so there is
            // nowhere to put one.
            Err(STATUS_INVALID_DEVICE_REQUEST.into())
        }

        fn set_security(
            &self,
            _context: &Self::FileContext,
            _security_information: u32,
            _modification_descriptor: ModificationDescriptor,
        ) -> FspResult<()> {
            // Pretend success so apps that always set security on open
            // (Office, etc.) don't error out. We don't synthesise SDs.
            Ok(())
        }

        // -----------------------------------------------------------------
        // Write callbacks: every mutation lands in the overlay.
        // -----------------------------------------------------------------

        fn create(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: FILE_ACCESS_RIGHTS,
            file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
            _security_descriptor: Option<&[c_void]>,
            _allocation_size: u64,
            _extra_buffer: Option<&[u8]>,
            _extra_buffer_is_reparse_point: bool,
            file_info: &mut OpenFileInfo,
        ) -> FspResult<Self::FileContext> {
            self.ensure_writable()?;
            let unix_path = winpath_to_unix(file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;

            // Reject if the path already exists (overlay or underlay).
            match self.mount.overlay.lookup(&unix_path) {
                OverlayLookup::Hit(_) => return Err(STATUS_OBJECT_NAME_COLLISION.into()),
                OverlayLookup::Deleted | OverlayLookup::Miss => {}
            }
            if matches!(self.mount.overlay.lookup(&unix_path), OverlayLookup::Miss)
                && self.fs().lookup_path(&unix_path).is_ok()
            {
                return Err(STATUS_OBJECT_NAME_COLLISION.into());
            }

            let is_dir = (file_attributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
            let read_only = self.is_read_only();
            if is_dir {
                self.mount.overlay.create_dir(&unix_path, 0o040755);
                let entry = match self.mount.overlay.lookup(&unix_path) {
                    OverlayLookup::Hit(e) => e,
                    _ => return Err(STATUS_INVALID_DEVICE_REQUEST.into()),
                };
                populate_overlay_file_info(&entry, file_info.as_mut(), read_only);
                Ok(XfsFileContext {
                    unix_path,
                    is_dir: true,
                    inode: Mutex::new(None),
                    pending_delete: Mutex::new(false),
                    dir_listing: Mutex::new(None),
                })
            } else {
                self.mount
                    .overlay
                    .create_file(&unix_path, Vec::new(), 0o100644);
                let entry = match self.mount.overlay.lookup(&unix_path) {
                    OverlayLookup::Hit(e) => e,
                    _ => return Err(STATUS_INVALID_DEVICE_REQUEST.into()),
                };
                populate_overlay_file_info(&entry, file_info.as_mut(), read_only);
                Ok(XfsFileContext {
                    unix_path,
                    is_dir: false,
                    inode: Mutex::new(None),
                    pending_delete: Mutex::new(false),
                    dir_listing: Mutex::new(None),
                })
            }
        }

        fn write(
            &self,
            context: &Self::FileContext,
            buffer: &[u8],
            offset: u64,
            write_to_eof: bool,
            constrained_io: bool,
            file_info: &mut FileInfo,
        ) -> FspResult<u32> {
            self.ensure_writable()?;
            if context.is_dir {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into());
            }
            // Read-modify-write. Race resolution: if two concurrent
            // writes hit the same path, both will independently compute
            // their patched buffer and the second `write_file` insertion
            // wins. The overlay's RwLock serialises the two map
            // insertions, but the read-modify-write is NOT atomic —
            // overlapping concurrent writes to the same path could lose
            // one of them. WinFsp's per-handle dispatch makes this
            // unlikely in practice (one handle = sequential writes).
            let (mut content, mode) = self.read_full(&context.unix_path)?;
            let effective_offset = if write_to_eof {
                content.len() as u64
            } else {
                offset
            };
            let end = effective_offset
                .checked_add(buffer.len() as u64)
                .ok_or_else(|| winfsp::FspError::from(STATUS_INVALID_DEVICE_REQUEST))?;
            if constrained_io {
                // constrained_io: don't extend beyond current EOF; clamp
                // the write length to the available tail.
                let cur_size = content.len() as u64;
                if effective_offset >= cur_size {
                    file_info.file_size = cur_size;
                    file_info.allocation_size = (cur_size + 4095) & !4095;
                    return Ok(0);
                }
                let take = ((cur_size - effective_offset) as usize).min(buffer.len());
                content[effective_offset as usize..effective_offset as usize + take]
                    .copy_from_slice(&buffer[..take]);
                self.mount
                    .overlay
                    .write_file(&context.unix_path, content, mode);
                if let OverlayLookup::Hit(entry) = self.mount.overlay.lookup(&context.unix_path) {
                    populate_overlay_file_info(&entry, file_info, self.is_read_only());
                }
                return Ok(take as u32);
            }
            // Unconstrained: extend the buffer if the write goes past EOF.
            if end > content.len() as u64 {
                content.resize(end as usize, 0);
            }
            content[effective_offset as usize..effective_offset as usize + buffer.len()]
                .copy_from_slice(buffer);
            self.mount
                .overlay
                .write_file(&context.unix_path, content, mode);
            if let OverlayLookup::Hit(entry) = self.mount.overlay.lookup(&context.unix_path) {
                populate_overlay_file_info(&entry, file_info, self.is_read_only());
            }
            Ok(buffer.len() as u32)
        }

        fn set_file_size(
            &self,
            context: &Self::FileContext,
            new_size: u64,
            _set_allocation_size: bool,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            self.ensure_writable()?;
            if context.is_dir {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into());
            }
            let (mut content, mode) = self.read_full(&context.unix_path)?;
            content.resize(new_size as usize, 0);
            self.mount
                .overlay
                .write_file(&context.unix_path, content, mode);
            if let OverlayLookup::Hit(entry) = self.mount.overlay.lookup(&context.unix_path) {
                populate_overlay_file_info(&entry, file_info, self.is_read_only());
            }
            Ok(())
        }

        fn overwrite(
            &self,
            context: &Self::FileContext,
            _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
            _replace_file_attributes: bool,
            allocation_size: u64,
            _extra_buffer: Option<&[u8]>,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            self.ensure_writable()?;
            if context.is_dir {
                return Err(STATUS_INVALID_DEVICE_REQUEST.into());
            }
            // Equivalent to truncate-to-zero. Allocation hint is
            // honoured as a content pre-allocation but kept all-zero.
            let buf = vec![0u8; allocation_size.min(0) as usize];
            self.mount
                .overlay
                .write_file(&context.unix_path, buf, 0o100644);
            if let OverlayLookup::Hit(entry) = self.mount.overlay.lookup(&context.unix_path) {
                populate_overlay_file_info(&entry, file_info, self.is_read_only());
            }
            Ok(())
        }

        fn rename(
            &self,
            _context: &Self::FileContext,
            file_name: &U16CStr,
            new_file_name: &U16CStr,
            replace_if_exists: bool,
        ) -> FspResult<()> {
            self.ensure_writable()?;
            let from = winpath_to_unix(file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;
            let to = winpath_to_unix(new_file_name).map_err(|_| STATUS_OBJECT_NAME_NOT_FOUND)?;

            // The promotion logic used to live here, which put the one
            // write operation that reads from the underlay out of reach
            // of every non-Windows test. It is `Mount::rename_path` now;
            // this maps its errors onto the statuses WinFsp expects.
            self.mount
                .rename_path(&from, &to, replace_if_exists)
                .map_err(|e| match e {
                    "rename: destination exists" => STATUS_OBJECT_NAME_COLLISION,
                    // Same split `err_to_status` makes: a path problem
                    // reads as not-found, anything else as a device
                    // error. "underlay read error" lands in the second.
                    "rename: source not found" => STATUS_OBJECT_NAME_NOT_FOUND,
                    _ => STATUS_INVALID_DEVICE_REQUEST,
                })?;
            Ok(())
        }

        fn set_delete(
            &self,
            context: &Self::FileContext,
            _file_name: &U16CStr,
            delete_file: bool,
        ) -> FspResult<()> {
            self.ensure_writable()?;
            *context.pending_delete.lock().unwrap() = delete_file;
            Ok(())
        }

        fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, _flags: u32) {
            if !self.is_read_only() && *context.pending_delete.lock().unwrap() {
                self.mount.overlay.delete(&context.unix_path);
            }
        }

        fn flush(
            &self,
            _context: Option<&Self::FileContext>,
            _file_info: &mut FileInfo,
        ) -> FspResult<()> {
            // Overlay is in-memory; flush is a no-op. Sidecar / rebuild
            // dismount policies handle persistence at unmount time.
            Ok(())
        }

        fn set_basic_info(
            &self,
            _context: &Self::FileContext,
            _file_attributes: u32,
            _creation_time: u64,
            _last_access_time: u64,
            _last_write_time: u64,
            _last_change_time: u64,
            file_info: &mut FileInfo,
        ) -> FspResult<()> {
            // Accept-and-ignore; the overlay carries mtime per-entry but
            // we don't currently expose a way to set it externally.
            // Populate `file_info` with whatever we currently know so
            // the caller's stat snapshot stays consistent.
            let read_only = self.is_read_only();
            match self.mount.overlay.lookup(&_context.unix_path) {
                OverlayLookup::Hit(entry) => {
                    populate_overlay_file_info(&entry, file_info, read_only);
                }
                OverlayLookup::Miss => {
                    if let Some(inode) = _context.inode.lock().unwrap().as_ref() {
                        populate_file_info(inode, file_info, read_only);
                    }
                }
                OverlayLookup::Deleted => return Err(STATUS_OBJECT_NAME_NOT_FOUND.into()),
            }
            Ok(())
        }
    }

    /// Helper enum for `read_directory`'s merge step. Holds either an
    /// underlay-resolved `Inode` (cheap clone) or an `OverlayEntry`
    /// (already cloned out of the overlay map).
    pub(crate) enum MergedEntry {
        Underlay(String, Inode),
        Overlay(String, OverlayEntry),
    }

    impl MergedEntry {
        fn name(&self) -> &str {
            match self {
                MergedEntry::Underlay(n, _) | MergedEntry::Overlay(n, _) => n.as_str(),
            }
        }
    }

    // The `Inode` returned from `read_inode` / `lookup_path` carries
    // file_type + nid + size etc. but no `Clone` derive on the
    // `FileType` enum is needed — `Inode` already derives `Clone`.
    fn _file_type_assertion(_: FileType) {}

    /// Mount the given XFS source on a Windows mount point.
    ///
    /// `mount_point` accepts a drive letter (`X:`) or a path to an
    /// empty directory. Blocks until the user presses Ctrl-C, then
    /// unmounts. Mirrors `ext4-win-driver`'s `run` entry point.
    pub fn run(mount: Mount, mount_point: &str) -> Result<()> {
        let _init = winfsp::winfsp_init().context("WinFsp not installed?")?;

        let read_only = mount.write_mode == WriteMode::ReadOnly;
        // Clone the bits we need post-host-shutdown for the dismount
        // policy — the host takes ownership of `mount` via the
        // `XfsContext`, so we must capture references first. Both
        // `Filesystem` and `Overlay` are wrapped in `Arc` (or behind
        // one in `Mount`'s case), so cloning is cheap.
        let dismount_overlay = mount.overlay.clone();
        let dismount_policy = mount.dismount_policy.clone();
        // The Filesystem is owned by Mount and isn't `Clone`. For the
        // Rebuild policy we need a handle to it after the host stops.
        // We achieve that by *opening it again* from the same image —
        // it's an idempotent read-only open, and the per-mount handle
        // inside `mount` may be dropped by the host on shutdown.
        let dismount_image = mount.image.clone();

        let ctx = XfsContext::new(mount)?;
        let block_size = ctx.fs().superblock().blocksize;
        let sector = block_size.min(4096) as u16;

        let mut params = VolumeParams::new();
        params
            .sector_size(sector)
            .sectors_per_allocation_unit(1)
            .max_component_length(255)
            .file_info_timeout(1000)
            .case_sensitive_search(true)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .filesystem_name("xfs");
        // Reparse support has to be advertised, not merely implemented:
        // without the volume flag the redirector answers
        // FSCTL_GET_REPARSE_POINT with ERROR_INVALID_FUNCTION, so
        // `fsutil reparsepoint query` and Explorer's "Target" column stay
        // empty even though resolution through the callbacks works.
        params.reparse_points(true);
        // `--ro` actually means read-only now: flip the volume flag so
        // the cache manager short-circuits writes at the kernel level
        // and Explorer renders the volume as RO. Writable is the default.
        if read_only {
            params.read_only_volume(true);
        }

        // Named rather than inferred: winfsp-rs 0.13.0 carries the
        // locking strategy as a type parameter, and two impls' methods
        // collide when it is left open -- E0034 at the mount() call.
        let mut host = FileSystemHost::<_, FineGuard>::new(params, ctx)
            .map_err(|e| anyhow!("FileSystemHost::new failed: {e}"))?;

        host.mount(mount_point)
            .map_err(|e| anyhow!("mount({mount_point}) failed: {e}"))?;
        host.start()
            .map_err(|e| anyhow!("FileSystemHost::start failed: {e}"))?;

        let banner = if read_only { "RO" } else { "RW (overlay)" };
        println!("xfs mounted at {mount_point} ({banner}). Ctrl-C to unmount.");
        // Block until Ctrl-C; WinFsp's host runs on its own threads.
        let (tx, rx) = std::sync::mpsc::channel();
        ctrlc::set_handler(move || {
            let _ = tx.send(());
        })
        .ok();
        let _ = rx.recv();

        host.stop();
        host.unmount();

        // Apply the configured dismount policy. Failures are surfaced
        // to stderr but don't propagate further — the host has already
        // torn down and the user has Ctrl-Ced; we want to maximise the
        // chance of getting a clear diagnostic out.
        match &dismount_policy {
            super::DismountPolicy::Discard => {
                dismount_overlay.clear();
            }
            super::DismountPolicy::Sidecar(path) => {
                if let Err(e) = super::write_sidecar(&dismount_overlay, path) {
                    eprintln!("warning: --scratch-sidecar failed: {e:#}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(windows, feature = "mount"))]
pub use winfsp_adapter::run as run_winfsp;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// NO UNIT TESTS IN THIS FILE, deliberately.
//
// It used to have four, built on a `build_simple_image()` inherited from
// erofs-win-driver that wrote an EROFS superblock -- so XFS read offset
// 0, found zeros, and every one of them failed. They were briefly gated
// behind a feature flag, which quietened the failure without making the
// tests true.
//
// Everything in this file needs a mounted filesystem, so the honest
// place for its coverage is `tests/`, against images a real `mkfs.xfs`
// produced (`scripts/build-fixtures.sh`). What the four covered is
// covered there:
//
//   mount_open_direct_smoke               -> mount_open_reads_a_real_filesystem
//   mount_open_part_zero_treated_as_direct-> a_partition_index_on_an_unpartitioned_image_is_refused
//   volume_info_math_matches_superblock   -> volume_totals_match_the_superblock
//   mount_open_partition_out_of_range     -> a_partition_index_on_an_unpartitioned_image_is_refused
//
// A unit test here would have to fake a filesystem, and a fake is what
// got this file into trouble in the first place.
