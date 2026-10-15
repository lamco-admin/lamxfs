//! `lamxfs` — a `no_std` read-only XFS filesystem reader for UEFI bootloaders.
//!
//! Clean-room implementation written from the public XFS on-disk format (the
//! SGI/Red Hat *XFS Algorithms & Data Structures* document and the Linux
//! `fs/xfs/libxfs` on-disk headers — uncopyrightable format facts). No
//! third-party filesystem source is vendored.
//!
//! ## Scope
//!
//! Read-only. Single data device. The subset of XFS actually present on `/boot`
//! and `/` of stock installs: v4 (no-CRC) and v5 (CRC, self-describing)
//! layouts, `NREXT64` large extent counts, parent pointers, exchange-range, the
//! metadata directory tree, sparse inodes, `ftype` directory entries, reflinks
//! (transparent read-pass-through), and `meta_uuid`. A v5 log that still holds
//! committed changes (a power cut, a crash) is replayed in memory the way Linux
//! replays it at mount, so reads see what Linux would show; nothing is written
//! to the volume. Realtime devices, external logs, a log that can't be
//! replayed (a v4 one, an unfinished file-mapping change, an unknown log item),
//! and per-file encryption are rejected with a typed error rather than
//! mis-read. There is no write path and no `BlockWrite` trait — read-only by
//! construction.
//!
//! ## Shape
//!
//! The public surface mirrors `lambutter::Btrfs` so a bootloader's filesystem
//! adapter is a one-for-one analogue across the read-only-FS family:
//!
//! ```ignore
//! let mut fs = Xfs::open(reader, device_size_bytes)?;
//! let data = fs.read_file(Path::new(b"/boot/vmlinuz-6.1.0"))?;
//! ```
//!
//! `read_file` caps its own allocation at [`MAX_FILE_BYTES`] (a hostile inode can
//! declare a multi-GiB size while occupying no extents — the read contract from
//! LamBoot's `fs_backend` security audit). `read_file_at` streams into a
//! caller-sized buffer and is the path the PE loader uses for kernel images.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![warn(unreachable_pub)]

extern crate alloc;

mod be;
mod block_read;
mod bmbt;
mod crc;
mod dir;
mod error;
mod file;
mod format;
mod inode;
mod log;
mod path;
mod replay;
mod resolve;
mod superblock;
mod symlink;
mod xfs;

pub use block_read::BlockRead;
pub use error::{Error, Location, SuperblockReason};
pub use path::Path;
pub use xfs::{DirEntry, EntryKind, Inode, Metadata, Xfs, MAX_FILE_BYTES};

/// Log replay on a synthetic volume, for the replay fuzz target.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_replay(data: &[u8]) {
    replay::fuzz(data);
}
