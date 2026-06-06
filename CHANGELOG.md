# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
