# lamxfs

[![Crates.io](https://img.shields.io/crates/v/lamxfs.svg)](https://crates.io/crates/lamxfs)
[![Docs.rs](https://docs.rs/lamxfs/badge.svg)](https://docs.rs/lamxfs)
[![CI](https://github.com/lamco-admin/lamxfs/actions/workflows/ci.yml/badge.svg)](https://github.com/lamco-admin/lamxfs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A `no_std` read-only XFS reader for UEFI bootloaders.

> **Early development.** This repository establishes the build, license, CI, and
> the public API contract. The filesystem implementation is in progress and the
> crate is **not yet functional** — do not depend on it for production use.

## Scope

`lamxfs` reads the XFS v5 on-disk format (superblock, allocation groups, inode B+trees, extents, directories) over a byte source, exposing a read-only volume a
bootloader can traverse to find a kernel and initramfs. Read-only by
construction. The motivating target is RHEL, Rocky, AlmaLinux, Oracle Linux and Amazon Linux, where XFS is the default root and boot filesystem.

## Basis

It vendors the parsing core of [xfs-fuse](https://github.com/KhaledEmaraDev/xfuse) (its `libxfuse` parsing core only) (BSD-2-Clause) — a clean-room
reader, not GPL kernel/userspace source. See [`NOTICE`](NOTICE) for attribution.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Vendored upstream portions retain their own terms (see
[`NOTICE`](NOTICE)).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you shall be dual licensed as above, without any
additional terms or conditions.
