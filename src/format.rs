//! XFS on-disk format constants: magics, feature bits, field offsets.
//!
//! All values are facts of the published XFS on-disk format. Offsets are byte
//! offsets into the relevant on-disk structure; every multi-byte field is
//! big-endian (read via [`crate::be`]).

// ---- magic numbers ----
pub(crate) const SB_MAGIC: u32 = 0x5846_5342; // "XFSB"
pub(crate) const INODE_MAGIC: u16 = 0x494e; // "IN"
pub(crate) const DIR3_DATA_MAGIC: u32 = 0x5844_4433; // "XDD3" (v5 data block)
pub(crate) const DIR2_DATA_MAGIC: u32 = 0x5844_3244; // "XD2D" (v4 data block)
pub(crate) const DIR3_BLOCK_MAGIC: u32 = 0x5844_4233; // "XDB3" (v5 single-block dir)
pub(crate) const DIR2_BLOCK_MAGIC: u32 = 0x5844_3242; // "XD2B" (v4 single-block dir)
pub(crate) const SYMLINK_MAGIC: u32 = 0x5853_4c4d; // "XSLM" (v5 remote symlink hdr)

// ---- superblock version ----
pub(crate) const SB_VERSION_NUMBITS: u16 = 0x000f;
pub(crate) const SB_VERSION_4: u16 = 4;
pub(crate) const SB_VERSION_5: u16 = 5;

// ---- v5 incompat feature bits (sb_features_incompat) ----
pub(crate) const INCOMPAT_FTYPE: u32 = 1 << 0;
pub(crate) const INCOMPAT_SPINODES: u32 = 1 << 1;
pub(crate) const INCOMPAT_META_UUID: u32 = 1 << 2;
pub(crate) const INCOMPAT_BIGTIME: u32 = 1 << 3;
pub(crate) const INCOMPAT_NEEDSREPAIR: u32 = 1 << 4;
pub(crate) const INCOMPAT_NREXT64: u32 = 1 << 5;
/// Incompat bits this reader understands. Any bit outside this set means the
/// on-disk layout may differ in a way we cannot safely read → reject.
/// `NEEDSREPAIR` is deliberately excluded: it marks a filesystem that must be
/// repaired before use.
pub(crate) const INCOMPAT_SUPPORTED: u32 =
    INCOMPAT_FTYPE | INCOMPAT_SPINODES | INCOMPAT_META_UUID | INCOMPAT_BIGTIME | INCOMPAT_NREXT64;

// ---- v4 features2 (sb_features2) — the bit relevant to a reader ----
pub(crate) const FEATURES2_FTYPE: u32 = 0x0000_0200;

// ---- inode di_format ----
pub(crate) const DINODE_FMT_DEV: u8 = 0;
pub(crate) const DINODE_FMT_LOCAL: u8 = 1;
pub(crate) const DINODE_FMT_EXTENTS: u8 = 2;
pub(crate) const DINODE_FMT_BTREE: u8 = 3;

// ---- inode core field offsets (xfs_dinode) ----
pub(crate) const DI_MAGIC: usize = 0;
pub(crate) const DI_MODE: usize = 2;
pub(crate) const DI_VERSION: usize = 4;
pub(crate) const DI_FORMAT: usize = 5;
/// 64-bit data-fork extent count when the NREXT64 incompat bit is set. This
/// repurposes the offset-24 union (di_v2_pad/di_flushiter / di_v3_pad).
pub(crate) const DI_BIG_NEXTENTS: usize = 24;
pub(crate) const DI_SIZE: usize = 56;
/// 32-bit data-fork extent count for the non-NREXT64 layout.
pub(crate) const DI_NEXTENTS: usize = 76;
pub(crate) const DI_FORKOFF: usize = 82;

/// Inode-core size (where the data fork / literal area begins).
pub(crate) const DINODE_CORE_V3: usize = 176; // v5 inodes (di_version 3)
pub(crate) const DINODE_CORE_V2: usize = 100; // v4 inodes (di_version 1/2)

// ---- POSIX mode bits (di_mode) ----
pub(crate) const S_IFMT: u16 = 0xf000;
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFDIR: u16 = 0x4000;
pub(crate) const S_IFLNK: u16 = 0xa000;

// ---- directory geometry ----
/// Logical byte offset at which a directory's leaf (hash-index) blocks begin;
/// data blocks live strictly below it. 32 GiB. We enumerate data blocks only —
/// the leaf/free index is a lookup accelerator a reader does not need.
pub(crate) const DIR2_LEAF_OFFSET: u64 = 32 * 1024 * 1024 * 1024;

/// v5 directory data-block header size (xfs_dir3_data_hdr): a 48-byte
/// xfs_dir3_blk_hdr + best-free[3] (12) + 4 pad.
pub(crate) const DIR3_DATA_HDR_LEN: usize = 64;
/// v4 directory data-block header size (xfs_dir2_data_hdr): magic(4) +
/// best-free[3] (12).
pub(crate) const DIR2_DATA_HDR_LEN: usize = 16;

/// Marker u16 for an unused (free) directory data region.
pub(crate) const DIR2_DATA_FREE_TAG: u16 = 0xffff;

/// v5 remote-symlink block header length (xfs_dsymlink_hdr): magic(4) + offset(4)
/// + bytes(4) + crc(4) + uuid(16) + owner(8) + blkno(8) + lsn(8) = 56.
pub(crate) const SYMLINK_HDR_LEN: usize = 56;
