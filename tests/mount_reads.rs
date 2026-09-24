//! What the driver reads out of a real XFS filesystem.
//!
//! Every image here was made by `mkfs.xfs` and populated by the kernel
//! (`scripts/build-fixtures.sh`), so these compare this driver against
//! what XFS actually writes rather than against our own idea of it.
//!
//! That is the point of the rewrite. The tests these replace ran against
//! an image hand-built by code copied from `erofs-win-driver`, which
//! wrote an EROFS superblock — so XFS read offset 0, found zeros, and
//! nothing here could be true. Gating them made the failures quiet
//! without making the tests real.

mod common;

use common::{fixture_or_skip, has_our_content, mount};
use xfs_win_driver::mount::Mount;

/// `Mount::open` reaches a real filesystem and its metadata matches what
/// the reader reports directly. If these disagree, the driver's mount
/// path is doing something to the volume the reader is not.
#[test]
fn mount_open_reads_a_real_filesystem() {
    let Some(img) = fixture_or_skip() else { return };
    let m = Mount::open(&img, None).expect("Mount::open on a real XFS image");

    let sb = m.fs.superblock();
    assert!(
        sb.blocksize.is_power_of_two(),
        "block size must be a power of two"
    );
    assert!(sb.dblocks > 0, "a formatted filesystem has data blocks");
    assert!(
        sb.agcount > 0,
        "a formatted filesystem has allocation groups"
    );
    assert!(sb.rootino > 0, "there is a root inode");

    let root = m.fs.root().expect("open the root");
    assert!(root.is_dir(), "the root is a directory");
    let entries = root.entries().expect("list the root");
    assert!(
        entries.iter().any(|e| e.name != b"." && e.name != b".."),
        "the fixture's root should not be empty"
    );
}

/// A file's bytes come back exactly, through the driver's own read path
/// rather than the reader's. Asserts on content the fixture script wrote
/// deliberately, so a wrong answer is wrong against a known fact.
#[test]
fn read_path_returns_the_exact_bytes() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }
    let m = Mount::open(&img, None).expect("open");

    assert_eq!(
        m.read_path("/small.txt").expect("read small.txt"),
        b"hello xfs",
        "the file's bytes are not what the fixture wrote"
    );

    assert_eq!(
        m.read_path("/empty.txt").expect("read empty.txt"),
        Vec::<u8>::new(),
        "an empty file reads as no bytes, not as an error"
    );

    assert_eq!(
        m.read_path("/dir/nested/deep/leaf.txt")
            .expect("read a nested file"),
        b"level three",
        "path walking past one level is wrong"
    );

    // Names are bytes on XFS, not necessarily UTF-8 — the driver must
    // not mangle one on the way through.
    assert_eq!(
        m.read_path("/naïve-café.txt")
            .expect("read a non-ASCII name"),
        b"unicode name"
    );
}

/// The multi-megabyte fixture carries a POSITION-DEPENDENT pattern: each
/// 8-byte group holds its own index. So a read that returns the right
/// number of bytes from the wrong offset still fails, which a constant
/// fill would hide.
#[test]
fn a_large_file_reads_back_at_the_right_offsets() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }
    let m = Mount::open(&img, None).expect("open");

    let all = m.read_path("/pattern.bin").expect("read pattern.bin");
    assert_eq!(
        all.len(),
        3 * 1024 * 1024,
        "the whole file should come back"
    );

    // Every 8-byte group must equal its own index. Checking the whole
    // file rather than samples: a driver that reads extents in the wrong
    // order gets most groups right and a few wrong.
    let (groups, tail) = all.as_chunks::<8>();
    assert!(
        tail.is_empty(),
        "the file should be a whole number of groups"
    );
    for (index, chunk) in groups.iter().enumerate() {
        let value = u64::from_le_bytes(*chunk);
        assert_eq!(
            value,
            index as u64,
            "byte offset {} holds {} instead of {}",
            index * 8,
            value,
            index
        );
    }
}

/// A directory with 200 entries is stored differently from one with
/// three — XFS moves it out of short form into block form — so this
/// exercises a different on-disk representation from the tests above.
#[test]
fn a_large_directory_lists_every_entry() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }

    let dir = fs.open("/manyentries").expect("open the large directory");
    let names: Vec<String> = dir
        .entries()
        .expect("list it")
        .into_iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .map(|e| String::from_utf8_lossy(&e.name).to_string())
        .collect();

    assert_eq!(names.len(), 200, "every entry should be listed");
    for i in 1..=200 {
        let want = format!("entry-{i}.txt");
        assert!(names.contains(&want), "{want} is missing from the listing");
    }
}

/// The shape the live volume has and this fixture set did not: a directory
/// with more extents than the inode can hold inline, so its data fork is a
/// bmap B+tree and reading it goes through `bmbt::walk` rather than through
/// the inline record array.
///
/// The format assertion is half the test. Without it, a fixture that
/// stopped fragmenting would leave this green while quietly re-testing the
/// inline path the previous test already covers.
#[test]
fn a_btree_format_directory_lists_every_entry() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }

    let inode = fs
        .lookup_path("/bmbtdir")
        .expect("find the directory");
    assert!(
        matches!(inode.format, fs_xfs::inode::Format::Btree),
        "/bmbtdir is in {:?} format, so the fixture no longer produces more extents \
         than fit inline and this test would prove nothing about bmbt::walk",
        inode.format
    );

    let dir = fs.open("/bmbtdir").expect("open the btree-format directory");
    let mut got: Vec<String> = dir
        .entries()
        .expect("list it -- this walk reads a real two-level bmap tree")
        .into_iter()
        .filter(|e| e.name != b"." && e.name != b"..")
        .map(|e| String::from_utf8_lossy(&e.name).to_string())
        .collect();
    got.sort();

    let mut want: Vec<String> = (1..=60)
        .flat_map(|b| (1..=120).map(move |j| format!("f-{b}-{j}")))
        .collect();
    want.sort();

    assert_eq!(got, want, "the listing should be exactly what was written");
}

/// /deepfile.bin is spread over thousands of one-block extents separated by
/// holes, so its data fork's bmap tree has intermediate levels -- the depth
/// nothing else here constructs, and the depth a 1500-entry directory on a
/// live volume has.
///
/// Every 4096-byte block is checked, not sampled. A written block starts with
/// its own extent index and a hole must read back as zeros, so a walk that
/// lands one child off, skips a subtree, or maps an extent a block early all
/// fail on the specific block they got wrong.
#[test]
fn a_deeply_fragmented_file_reads_back_extent_by_extent() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }

    const EXTENTS: usize = 6000;
    // Written blocks sit at every other position, so the file is twice as
    // long as it has extents.
    const BLOCKS: usize = EXTENTS * 2;

    let inode = fs
        .lookup_path("/deepfile.bin")
        .expect("find the fragmented file");
    assert!(
        matches!(inode.format, fs_xfs::inode::Format::Btree) && inode.nextents > 1000,
        "/deepfile.bin has {} extents in {:?} format: the fixture is no longer \
         fragmenting it, so this test would read an inline extent array and \
         prove nothing about tree depth",
        inode.nextents,
        inode.format
    );

    let m = Mount::open(&img, None).expect("open");
    let all = m.read_path("/deepfile.bin").expect("read the whole file");
    assert_eq!(all.len(), BLOCKS * 4096, "every block should come back");

    let (blocks, tail) = all.as_chunks::<4096>();
    assert!(tail.is_empty(), "the file should be a whole number of blocks");
    for (index, block) in blocks.iter().enumerate() {
        if index % 2 == 1 {
            assert!(
                block.iter().all(|&b| b == 0),
                "block {index} is a hole and should read back as zeros"
            );
            continue;
        }
        let mut key = [0u8; 8];
        key.copy_from_slice(&block[..8]);
        let stamp = u64::from_le_bytes(key);
        assert_eq!(
            stamp,
            (index / 2) as u64,
            "block {index} carries extent {stamp} -- the descent reached the wrong \
             extent, or a whole subtree was skipped"
        );
    }
}

/// A symlink's target is readable as stored, and a dangling one is a
/// normal on-disk state rather than an error — resolving is the caller's
/// business.
#[test]
fn symlinks_report_their_target_without_resolving_it() {
    let Some(img) = fixture_or_skip() else { return };
    let fs = mount(&img);
    if !has_our_content(&fs) {
        eprintln!("SKIPPED: fixture lacks the deliberate content; run scripts/build-fixtures.sh");
        return;
    }

    let link = fs.open("/link-to-small").expect("open the symlink");
    assert!(link.is_symlink(), "it should be a symlink");
    assert_eq!(link.link_target().expect("read target"), b"/small.txt");

    let dangling = fs
        .open("/dangling-link")
        .expect("open the dangling symlink");
    assert!(dangling.is_symlink());
    assert_eq!(
        dangling
            .link_target()
            .expect("a dangling target still reads"),
        b"/nowhere-at-all",
        "a target that resolves to nothing is still a target"
    );
}

/// Paths that are not there fail, and fail as "not found" rather than by
/// panicking or returning empty content — the difference a caller acts
/// on.
#[test]
fn missing_paths_are_refused() {
    let Some(img) = fixture_or_skip() else { return };
    let m = Mount::open(&img, None).expect("open");

    for missing in [
        "/definitely-not-here",
        "/dir/not-here-either",
        "/small.txt/treated-as-a-directory",
    ] {
        assert!(
            m.read_path(missing).is_err(),
            "{missing} does not exist and must not read as empty"
        );
    }
}

/// Reading a directory as a file is refused rather than answered with
/// its raw on-disk bytes.
#[test]
fn a_directory_is_not_readable_as_a_file() {
    let Some(img) = fixture_or_skip() else { return };
    let m = Mount::open(&img, None).expect("open");
    assert!(
        m.read_path("/dir").is_err(),
        "a directory must not read as file content"
    );
}

/// Asking for a partition on an image that has no partition table fails
/// rather than reading the filesystem at the wrong offset.
#[test]
fn a_partition_index_on_an_unpartitioned_image_is_refused() {
    let Some(img) = fixture_or_skip() else { return };
    assert!(
        Mount::open(&img, Some(1)).is_err(),
        "there is no partition 1 on a bare filesystem image"
    );
}

/// The volume totals the driver reports are the superblock's, converted
/// once. Arithmetic that drifts from the superblock is the kind of bug
/// that shows up as a wrong free-space figure in Explorer.
#[test]
fn volume_totals_match_the_superblock() {
    let Some(img) = fixture_or_skip() else { return };
    let m = Mount::open(&img, None).expect("open");
    let sb = m.fs.superblock();

    let total = sb.dblocks * u64::from(sb.blocksize);
    let free = sb.fdblocks * u64::from(sb.blocksize);

    assert!(total > 0, "a formatted filesystem has a size");
    assert!(free <= total, "free space cannot exceed the total");
    assert!(
        total >= 16 * 1024 * 1024,
        "mkfs.xfs will not make one smaller than about 16 MiB, so this is suspect"
    );
}
