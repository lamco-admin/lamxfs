# lamxfs

[![Crates.io](https://img.shields.io/crates/v/lamxfs.svg)](https://crates.io/crates/lamxfs)
[![Docs.rs](https://docs.rs/lamxfs/badge.svg)](https://docs.rs/lamxfs)
[![CI](https://github.com/lamco-admin/lamxfs/actions/workflows/ci.yml/badge.svg)](https://github.com/lamco-admin/lamxfs/actions/workflows/ci.yml)
[![Website](https://img.shields.io/badge/website-lamco.ai-blue.svg)](https://lamco.ai/products/lamboot/)

A `no_std`, read-only **XFS** filesystem reader for UEFI bootloaders and other
pre-OS contexts. Clean-room, memory-safe Rust (`#![forbid(unsafe_code)]`).

- **Website:** [LamBoot](https://lamco.ai/products/lamboot/), the bootloader lamxfs was written for
- **Documentation:** [docs.rs/lamxfs](https://docs.rs/lamxfs) and the [LamBoot docs](https://lamco.ai/products/lamboot/docs/)
- **Repository:** [github.com/lamco-admin/lamxfs](https://github.com/lamco-admin/lamxfs)
- **Company:** [Lamco Development LLC](https://lamco.ai/)

`lamxfs` exists so a bootloader (LamBoot in particular) can read kernels,
initrds, and boot configuration directly from XFS `/boot` and `/` partitions —
the default on RHEL 9, Rocky 9, AlmaLinux 9, Oracle Linux 9, CentOS Stream, and
Amazon Linux — **without** a GPL UEFI filesystem driver (unsignable under
Microsoft Secure Boot), without `std`, and without the Linux kernel.

It is the XFS counterpart to `ext4-view` and the Lamco-authored `lambutter`
(btrfs), with a deliberately matching API so a consumer's filesystem adapter is
one-for-one across the read-only family.

## Scope

Read-only, single data device. There is no write path and no `BlockWrite`
trait — read-only **by construction**.

Supported on-disk layout (what stock `mkfs.xfs` produces, through xfsprogs 7.x):

| Feature | Status |
|---|---|
| XFS v5 (CRC, self-describing metadata) | ✅ (CRC32C verified) |
| XFS v4 (legacy, no CRC) | ✅ |
| `NREXT64` (64-bit extent counts) | ✅ per inode, so a filesystem upgraded with `xfs_repair -c nrext64=1` reads correctly |
| Parent pointers, exchange-range (`mkfs.xfs` defaults since xfsprogs 6.18) | ✅ |
| Metadata directory tree (`metadir`) | ✅ without a realtime section |
| Block sizes to 64 KiB, sector sizes to 32 KiB | ✅ |
| Sparse inodes | ✅ |
| `ftype` directory entries | ✅ |
| Reflinks | ✅ read-pass-through (shared bytes read transparently) |
| `meta_uuid` | ✅ |
| `bigtime` | ✅ (timestamps not surfaced; layout-compatible) |
| Shortform / block / leaf / node directories | ✅ |
| Inline + remote (extent) symlinks | ✅ |

Refused with a typed error (never mis-read): a log that still needs replay
(`sb_dirty_log`), realtime device or realtime groups (including zoned),
external log, in-progress mkfs, unknown incompat feature, per-file encryption.
A dirty log means Linux didn't unmount the filesystem cleanly; mounting and
unmounting it once under Linux replays the log.

## Installation

```sh
cargo add lamxfs
```

or in `Cargo.toml`:

```toml
[dependencies]
lamxfs = "0.1"
```

The crate is `no_std` with `alloc`. The `std` feature only adds an
`std::error::Error` impl for `Error`.

## Usage

```rust
use lamxfs::{Xfs, Path, BlockRead};

let mut fs = Xfs::open(reader, device_size_bytes)?;       // validates the superblock
let cfg = fs.read_file(Path::new(b"/boot/loader/entries/rocky.conf"))?;
let ino = fs.resolve(Path::new(b"/boot/vmlinuz-6.1.0"))?;
let mut buf = [0u8; 1 << 20];
let n = fs.read_file_at(&ino, 0, &mut buf)?;              // streaming (PE-loader path)
for e in fs.read_dir(Path::new(b"/boot"))? { /* … */ }
```

`read_file` caps its up-front allocation at `MAX_FILE_BYTES` (256 MiB): a hostile
inode can declare a huge `di_size` while occupying no extents, so the size field
is never trusted to drive an allocation. `read_file_at` streams into a
caller-sized buffer and is unaffected.

You implement `BlockRead` (one method, `read_at`) over your device — the same
shape as `lambutter::BlockRead`.

## Safety & hardening

`#![forbid(unsafe_code)]`. Every on-disk count, offset, and shift is bounds- and
range-checked before use, so malformed or hostile media yields a typed error
rather than a panic, out-of-bounds read, or denial-of-boot allocation. Validated
by fuzzing (`cargo +nightly fuzz run fuzz_open` / `fuzz_walk`).

## Testing

- **Layer A** — synthetic-byte unit tests for each parser (`cargo test`).
- **Layer B** — oracle tests against real `mkfs.xfs` images (v5 with and
  without NREXT64, v4 legacy, stock xfsprogs 7.1 defaults, metadir, 64 KiB
  blocks, 8 KiB sectors, an NREXT64-upgraded filesystem, a wrapped log): every
  file is verified against its SHA-256, and dirty-log and realtime images must
  be refused (`tests/oracle.rs`; regenerate with `tests/fixtures/regenerate.sh`).
- **Layer C** — `cargo fuzz` targets over arbitrary and seed-mutated volumes.

## Contributing

Issues and pull requests are welcome at
[github.com/lamco-admin/lamxfs](https://github.com/lamco-admin/lamxfs). A
change to the read path should come with a fixture: `tests/fixtures/README.md`
explains how the images are built.

## License

MIT OR Apache-2.0, copyright Lamco Development LLC. Clean-room from the public
XFS on-disk format; see `NOTICE`.
