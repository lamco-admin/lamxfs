# lamxfs

[![Crates.io](https://img.shields.io/crates/v/lamxfs.svg)](https://crates.io/crates/lamxfs)
[![Docs.rs](https://docs.rs/lamxfs/badge.svg)](https://docs.rs/lamxfs)

A `no_std`, read-only **XFS** filesystem reader for UEFI bootloaders and other
pre-OS contexts. Clean-room, memory-safe Rust (`#![forbid(unsafe_code)]`).

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

Supported on-disk layout (the subset present on stock RHEL-family installs):

| Feature | Status |
|---|---|
| XFS v5 (CRC, self-describing metadata) | ✅ (CRC32C verified) |
| XFS v4 (legacy, no CRC) | ✅ |
| `NREXT64` (64-bit extent counts) | ✅ |
| Sparse inodes | ✅ |
| `ftype` directory entries | ✅ |
| Reflinks | ✅ read-pass-through (shared bytes read transparently) |
| `meta_uuid` | ✅ |
| `bigtime` | ✅ (timestamps not surfaced; layout-compatible) |
| Shortform / block / leaf / node directories | ✅ |
| Inline + remote (extent) symlinks | ✅ |

Refused with a typed error (never mis-read): realtime device, external log,
in-progress mkfs, unknown incompat feature, per-file encryption.

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
- **Layer B** — oracle tests against real `mkfs.xfs` images (v5+NREXT64, v5
  without NREXT64, v4 legacy): every file is verified against its kernel-produced
  SHA-256 (`tests/oracle.rs`; regenerate with `tests/fixtures/regenerate.sh`).
- **Layer C** — `cargo fuzz` targets over arbitrary and seed-mutated volumes.

## License

MIT OR Apache-2.0. Clean-room from the public XFS on-disk format; see `NOTICE`.
