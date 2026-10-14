# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-10-14

### Fixed

- Filesystems made by `mkfs.xfs` from xfsprogs 6.18 on (Fedora 44, Ubuntu
  26.04, openSUSE Tumbleweed, Arch, Debian 14) were refused with
  `feat_unknown_incompat`, because parent pointers and exchange-range became
  mkfs defaults. Both are accepted now, and so is the metadata directory tree
  (`metadir`) when there's no realtime section.
- Extent counts are read per inode. `xfs_repair -c nrext64=1` sets the
  superblock bit but leaves existing inodes on 32-bit counts, and 0.1.0 read
  those files as zeros.
- A log that still needs replay is refused with `sb_dirty_log`. 0.1.0 never
  looked at the log, so it read such a filesystem stale.
- 64 KiB blocks of 512-byte inodes were refused with `sb_bad_geometry`.
- Superblock sectors larger than 4 KiB failed the v5 CRC check with
  `sb_bad_crc`.
- A block or inode pointer outside the filesystem's allocation groups could
  overflow the byte-offset arithmetic, a panic wherever overflow checks are on
  (found by `fuzz_walk`). Such a pointer now fails its read.

### Changed

- Superblock geometry is checked the way the kernel checks it: sector size and
  log, inode size and log, and inodes per block must agree.
- An inode that claims 64-bit extent counts on a filesystem without the
  feature is refused with `inode_nrext64_unsupported`, as the kernel's inode
  verifier refuses it.
- Dev-dependencies move to `sha2` 0.11 and `ruzstd` 0.9.1, which carries the
  fix for RUSTSEC-2026-0344. `ruzstd` only decompresses the test fixtures.
- `LICENSE-APACHE` now carries the full license text, the package description
  names Lamco Development LLC, and the README gains website, documentation
  and install links.

### Added

- Fixtures for stock xfsprogs 7.1.1, metadir, 64 KiB blocks, 8 KiB sectors, an
  NREXT64-upgraded filesystem and a wrapped clean log, plus a dirty log and
  realtime-group and zoned images that must be refused.
- CI also runs weekly, so new toolchain lints and advisories surface without a
  push.

## [0.1.0] - 2026-06-05

### Added

- Clean-room `no_std` read-only XFS reader, complete for the RHEL-family `/boot`
  and `/` feature set:
  - Superblock parse + validation with v5 CRC32C; rejects realtime device,
    external/in-progress log, unknown incompat features.
  - v4 (legacy, no-CRC) and v5 (self-describing) layouts; `NREXT64`, sparse
    inodes, `ftype`, `meta_uuid`, `bigtime`, reflink read-pass-through.
  - Inode read-by-number (arithmetic AG location), data-fork dispatch
    (local/extents/btree), 16-byte packed extent decode + BMBT B+tree walk.
  - File reads with sparse-hole / unwritten-extent zero-fill; shortform / block
    / leaf / node directory enumeration; inline + remote (extent) symlinks;
    path resolution.
  - Public `Xfs<R>` API mirroring `lambutter::Btrfs`
    (`open`/`uuid`/`label`/`resolve`/`metadata`/`read_file`/`read_file_at`/
    `read_link`/`read_dir`); `MAX_FILE_BYTES` read cap; advisory `read_dir`.
  - `#![forbid(unsafe_code)]`; every on-disk count/offset/shift bounds- and
    range-checked.
- Tests: Layer-A unit tests per parser; Layer-B oracle tests against real
  `mkfs.xfs` images (v5+NREXT64, v5 without NREXT64, v4 legacy) verifying every
  file's kernel-produced SHA-256; `cargo fuzz` targets (`fuzz_open`,
  `fuzz_walk`).
