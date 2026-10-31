# UFS reader

For catalogue ingestion, `--content-only extract IMAGE NEW_DIRECTORY` leaves
regular files and directories accessible to the extracting user and records
symlinks without creating them. Original metadata and link targets remain in
the JSON Lines manifest.

A read-only UFS1/FFS listing and extraction tool, with RISC iX old-BSD
filesystem support validated against Linux's `ufstype=old` reader.

This fork builds on [Dan Cross's ufsreader](https://github.com/dancrossnyc/ufsreader),
originally an illumos filesystem-reading demonstration. The on-disc cylinder-group,
inode and indirect-block mapping has been adapted to checked, bounded seek/read
I/O. The image is never mounted, modified, memory-mapped or loaded in full.

## Build

Use a current stable Rust toolchain on a Unix host:

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

The binary is `target/release/ufsreader`. No root access, FUSE or kernel module
is required. The source is licensed under MPL-2.0; see `LICENSE.txt`.

## Commands

RISC iX is the default interpretation. Commands return zero on success and
non-zero on an error; diagnostics go to stderr.

```sh
# Inspect and validate the superblock and root inode.
ufsreader info root.img

# Recursively list every inode path as JSON Lines.
ufsreader list root.img
ufsreader list root.img --hashes > listing.jsonl

# Follow image-internal symlinks and stream one regular file.
ufsreader cat root.img /etc/fstab

# Extract into a directory which MUST NOT already exist.
ufsreader extract root.img extracted > metadata.jsonl
```

Operate directly on a partition inside a larger image using byte offsets:

```sh
ufsreader --offset 62976512 --length 186777600 list hdimage --hashes
```

`--format riscix` uses the old 16-bit UID/GID and directory name-length fields.
`--format illumos` retains old directory entries but reads extended UID/GID.
`--format bsd44` selects type-byte/8-bit-name-length directory entries and extended
UID/GID. Both byte orders are decoded explicitly from the UFS1 superblock magic.
Only RISC iX has been validated against the complete real-image corpus below;
the other format switches need broader validation before production use.
UFS2 is not supported. Format selection is explicit because magic alone does
not distinguish these UFS1 variants.

## Extracted data and metadata

Extraction writes regular files, preserves regular-file hard links, and creates
symlinks last. Capability-relative filesystem operations keep writes inside the
new destination. Symlink targets are preserved verbatim, including absolute or
parent-relative targets; the extractor never follows them on the host.

Regular files and directories receive their original ordinary `rwx` permissions,
access times and modification times. Host ownership is left with the invoking
user, and set-ID/sticky bits are not applied. Special files (devices, FIFOs and
sockets) are represented in the JSON metadata only, not created on the host.
Symlink timestamps, original ownership, special mode bits and inode change times
are recorded but not restored. Sparse holes read as zeroes; output files are
currently written densely.

Each JSON line has `schema_version: 1`, a display `path`, lossless `path_hex`,
an `inode` object, `sha256`, symlink target fields and an `action`. The inode
object includes the number, numeric Unix mode, link count, UID/GID, byte size,
second-resolution timestamps, allocated 512-byte block count and raw device word.
`path_hex` and `symlink_target_hex` preserve non-UTF-8 names; display strings may
contain replacement characters. Hashes are populated for `list --hashes` and
extracted regular files. Actions distinguish files, hard links, directories,
symlinks and `special_metadata_only` records.

Treat a manifest as complete only after the process exits successfully. A failed
extraction leaves a partial destination and partial manifest for diagnosis;
retry into a new directory. The tool refuses to merge with or overwrite an
existing destination. Output is not a bootable restoration of a Unix system.

## Bounds and malformed images

The parser uses checked arithmetic and extent validation for geometry, inode
tables, fragments and all three indirect-block levels. Directory records must
fit their 512-byte directory block and contain valid component names. Read errors
are propagated rather than silently ending iteration. The crate forbids unsafe
Rust code.

Default processing limits are 100,000 entries and 64 GiB of regular-file data
(counting each hard-link path). Adjust with `--max-entries` and `--max-bytes`.
Hashless listings do not read regular-file payloads. Fixed traversal limits are
4,096-byte paths, 256 nested directories, 40 symlink resolutions, 16 MiB per
directory and 256 MiB of directory data per tree walk. Directory cycles, aliases,
duplicate names and exceeded limits are errors. Memory use depends on bounded
traversal bookkeeping, not the image's byte size.

The reader does not repair filesystems or validate all allocation bitmaps. A
successful listing alone is not a filesystem consistency check. RISC iX-specific
compression or other extensions beyond the tested corpus remain unverified.

## Validation

The test suite constructs small synthetic filesystems and exercises sparse
blocks, single/double/triple indirect addressing, old ownership, both byte orders,
symlink loops, slice bounds, malformed geometry, bad pointers, directory corruption,
cycle detection, byte/entry limits, hard links, non-UTF-8 names and extraction
containment. A deterministic mutation smoke test covers superblock, inode and
directory fields without embedding historical filesystem contents in the repo.

Full-tree validation used Linux 6.8, mounted read-only with `ufstype=old`, as an
independent reference:

| Sample | Entries | Block / fragment size |
|---|---:|---:|
| [Recovery floppy](http://sink.me.uk/RISCiX/mknewfs_disc.img) | 41 | 4096 / 1024 |
| [270 MB image](https://www.4corn.co.uk/archive/riscix/riscix121chdimage.zip), Root slice | 6,777 | 4096 / 1024 |
| [2 GB image](https://www.4corn.co.uk/archive/riscix/riscix121chdimage2G.zip), Root slice | 6,784 | 8192 / 1024 |

All 13,602 records matched reference paths, inode numbers, modes, UID/GID, sizes,
modification times, link counts, regular-file SHA-256 hashes and symlink targets.
Files were then extracted and re-read to verify their contents, hard-link identity,
symlink targets, ordinary permissions and modification times. The 2 GB tree's
hashed listing peaked at 3,228 KiB RSS on the development Linux host; timing and
memory figures are environment-dependent.

These samples are external downloads, not committed test fixtures. Their Root
slice offsets and lengths are respectively `62976512 / 186777600` and
`63439360 / 2062000128` bytes. The recovery floppy is a whole filesystem image.
The original demo's unchecked pointer reads, zero-direct-block handling and
modern UID/GID assumptions have been replaced; malformed cases which previously
panicked, segfaulted or silently truncated now produce errors.
