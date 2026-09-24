#!/usr/bin/env bash
#
# Build the XFS images the tests read.
#
# WHY THIS EXISTS. The tests used to run against a hand-built image
# copied from erofs-win-driver, which wrote an EROFS superblock -- so
# XFS read offset 0, found zeros, and every test that needed a mounted
# filesystem failed. Gating them made the failures go away without
# making the tests true.
#
# These are real filesystems, made by mkfs.xfs and populated by the
# kernel, so a test that reads one is checking this driver against what
# XFS actually writes rather than against our own idea of it.
#
# WHAT IT NEEDS. mkfs.xfs (xfsprogs) to create, and root to mount and
# populate. That means Linux: CI runs it on ubuntu-latest, and a Linux
# dev box or VM runs it locally. On macOS there is no mkfs.xfs, so the
# tests skip and say so -- see tests/common/mod.rs.
#
# THE CONTENT IS DELIBERATE. Every file here exists because a test
# asserts something specific about it. Adding a case means adding a file
# here and asserting on it there; a test that just walks "whatever mkfs
# left" proves very little.
#
# Usage:  scripts/build-fixtures.sh [output-dir]     (default: .fixtures)
set -euo pipefail

OUT=${1:-.fixtures}
IMG="$OUT/xfs-content.img"
MNT=$(mktemp -d)

command -v mkfs.xfs >/dev/null || {
  echo "mkfs.xfs not found. Install xfsprogs (Linux); this cannot run on macOS." >&2
  exit 1
}
[ "$(id -u)" = 0 ] || { echo "must run as root: the image has to be mounted to populate it" >&2; exit 1; }

mkdir -p "$OUT"
rm -f "$IMG"

# 300 MiB: comfortably over mkfs.xfs's ~16 MiB floor, small enough to
# build in a second and to hold in a runner's disk without thought.
truncate -s 300M "$IMG"
mkfs.xfs -q -L DJTEST "$IMG"

mount -o loop "$IMG" "$MNT"
trap 'umount "$MNT" 2>/dev/null || true; rmdir "$MNT" 2>/dev/null || true' EXIT

# --- the deliberate content ------------------------------------------
# Each line is asserted somewhere in tests/. Keep the two in step.

# A short file whose exact bytes a test compares. No trailing newline,
# so the length is exactly what it looks like.
printf 'hello xfs' > "$MNT/small.txt"

# Empty: is_empty(), a zero-length read, and reading at offset 0 of
# nothing must all behave rather than error.
: > "$MNT/empty.txt"

# Big enough to span several extents and several blocks, with a
# POSITION-DEPENDENT pattern so a ranged read that returns the right
# NUMBER of bytes from the WRONG OFFSET still fails. A constant fill
# would hide exactly that bug.
python3 - "$MNT/pattern.bin" <<'PY'
import sys
size = 3 * 1024 * 1024
with open(sys.argv[1], "wb") as f:
    # Each 8-byte group is its own offset, little-endian.
    f.write(b"".join((i).to_bytes(8, "little") for i in range(0, size // 8)))
PY

# Nested directories, so a walk has somewhere to go and path joining is
# exercised beyond one level.
mkdir -p "$MNT/dir/nested/deep"
printf 'level three' > "$MNT/dir/nested/deep/leaf.txt"
printf 'level one' > "$MNT/dir/one.txt"

# Enough entries to push the directory past short form into block form,
# which is a different on-disk representation and a different code path.
mkdir -p "$MNT/manyentries"
for i in $(seq 1 200); do printf 'e%s' "$i" > "$MNT/manyentries/entry-$i.txt"; done

# A directory whose EXTENT LIST no longer fits inside the inode, so XFS
# moves its data fork into a bmap B+tree. This is the case the live volume
# hits and the case nothing here covered until now: with a 512-byte inode
# the fork holds 23 inline extent records, and past that the reader stops
# parsing an array and starts walking a tree (bmbt::walk).
#
# Entries alone do not produce it. XFS extends a directory with adjacent
# blocks, so a 7000-entry directory is routinely ONE extent and the tree
# is never built -- a fixture that merely grows the directory would claim
# to cover bmbt while testing the inline path again. The wedges are what
# force it: a one-block file written after each batch sits between this
# batch's directory block and the next one, so each extension has to be
# allocated somewhere else and becomes its own extent.
#
# tests/mount_reads.rs asserts the fork really is in btree format, so if
# XFS ever stops fragmenting this way the test fails instead of quietly
# reverting to coverage we already had.
mkdir -p "$MNT/bmbtdir"
for batch in $(seq 1 60); do
    for j in $(seq 1 120); do
        printf '%s-%s' "$batch" "$j" > "$MNT/bmbtdir/f-$batch-$j"
    done
    printf 'w' > "$MNT/wedge-$batch"
done

# A file with thousands of one-block extents, so its bmap tree is several
# levels deep rather than the root-plus-one-leaf shape /bmbtdir happens to
# produce. Depth is the thing no case here reached until now, and it is the
# thing that separates "the walk is right on this tree" from "the walk is
# right on any tree": a 512-byte inode's root holds 23 pointers, so 24+
# children means an intermediate level, and each leaf holds ~251 records, so
# thousands of extents means the descent has to go down, back up, and across
# sibling blocks the way a 1500-entry /usr/bin does on a live volume.
#
# HOW NOT TO DO IT, learned the expensive way: interleave the writes with a
# second file and flush every iteration. Two attempts, both "1 extents".
# File data is delayed-allocated, so at writeback the allocator is asked for
# "6000 blocks for this inode" as one job and hands back a contiguous run --
# MAXEXTLEN is 2^21 blocks, so a single extent covering the whole file is
# perfectly legal. Interleaving happens between inodes, not between blocks,
# and wedges of one byte do not even occupy a block (XFS keeps a file that
# small inside the inode).
#
# What does work is a gap between successive blocks -- two extents can only
# merge if they are adjacent in the file, so writing every other block breaks
# the run however the allocator schedules its flushes. One detail came out
# differently than expected: XFS turns those gaps into ALLOCATED-UNWRITTEN
# extents rather than leaving them unmapped, so the observed count was 11988
# extents across 11999 blocks instead of 6000. The holes still read back as
# zeros -- the test asserts exactly that -- and the number that matters is the
# one the guard below checks, because at ~12k extents the tree cannot be a
# root and a single leaf.
mkdir -p "$MNT/dwedge"
python3 - "$OUT/deepblocks.tmp" <<'PY'
import sys
# 6000 blocks of 4096 bytes; the first 8 bytes of each are its index, LE.
n = 6000
with open(sys.argv[1], "wb") as f:
    for i in range(n):
        f.write(i.to_bytes(8, "little") + b"\0" * (4096 - 8))
PY
: > "$MNT/deepfile.bin"
for i in $(seq 0 5999); do
    dd if="$OUT/deepblocks.tmp" of="$MNT/deepfile.bin" bs=4096 count=1 \
        skip="$i" seek="$((i * 2))" conv=notrunc status=none
done
rm -f "$OUT/deepblocks.tmp"
# The gap file the earlier attempts needed; nothing writes it any more, but a
# stale one would occupy blocks the assertion counts on.
rm -f "$MNT/dwedge/gap.bin" "$MNT/dwedge/blocks"
rmdir "$MNT/dwedge" 2>/dev/null || true

# Remember the inode number while the file is still mounted; the extent count
# is read from the unmounted image below, because xfs_db against the backing
# file of a live mount reads whatever has reached disk, not the state the
# kernel is holding.
deep_ino=$(stat -c '%i' "$MNT/deepfile.bin")

# A symlink, and one that dangles: reading the target must work without
# resolving it, and a dangling target is a normal thing on disk rather
# than an error.
ln -s /small.txt "$MNT/link-to-small"
ln -s /nowhere-at-all "$MNT/dangling-link"

# A file whose name is not ASCII. XFS stores names as bytes and does not
# require valid UTF-8, so the driver must not assume it.
printf 'unicode name' > "$MNT/naïve-café.txt"

sync
umount "$MNT"
trap - EXIT
rmdir "$MNT"

# Fail HERE rather than in a test three steps later, and now off a clean
# unmounted image. The extent count is the whole reason deepfile.bin exists;
# if the allocator stops cooperating, the fixture says so where it is made,
# with the number attached, instead of leaving a test to report that the
# thing it means to exercise is not there.
# `head -1` here would kill the build: with pipefail, head closing the pipe
# early leaves xfs_db dead of SIGPIPE, the substitution returns non-zero, and
# `set -e` aborts on the line that only meant to ask a question. Take the
# whole print into a variable instead and let the guard report what it found.
deep_db=$(xfs_db -r -c "inode $deep_ino" -c print "$IMG" 2>/dev/null || true)
deep_extents=$(printf '%s\n' "$deep_db" |
    sed -n 's/^core\.nextents *= *\([0-9][0-9]*\).*/\1/p' | tail -1)
echo "deepfile.bin: inode $deep_ino, ${deep_extents:-unreadable} extents"
[ "${deep_extents:-0}" -ge 5000 ] || {
    echo "fixture did not fragment deepfile.bin (got '${deep_extents:-unreadable}' extents, wanted >=5000);" >&2
    echo "a file that is not fragmented cannot build a bmap tree with intermediate levels," >&2
    echo "which is the only thing this case tests." >&2
    if [ -z "$deep_extents" ]; then
        echo "xfs_db printed no core.nextents; the field name or the image path is wrong." >&2
        printf '%s\n' "$deep_db" | head -20 >&2
    fi
    exit 1
}

echo "built $IMG"
ls -la "$IMG"
