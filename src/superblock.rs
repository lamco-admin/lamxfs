//! Superblock parse + validation: geometry, version/feature dispatch, and the
//! reject rules for layouts a read-only boot reader must refuse.

use crate::{
    be,
    error::{Error, Result, SuperblockReason},
    format,
};

// Superblock field byte offsets (xfs_dsb).
mod off {
    pub(super) const MAGICNUM: usize = 0;
    pub(super) const BLOCKSIZE: usize = 4;
    pub(super) const RBLOCKS: usize = 16;
    pub(super) const UUID: usize = 32;
    pub(super) const LOGSTART: usize = 48;
    pub(super) const ROOTINO: usize = 56;
    pub(super) const AGBLOCKS: usize = 84;
    pub(super) const AGCOUNT: usize = 88;
    pub(super) const LOGBLOCKS: usize = 96;
    pub(super) const VERSIONNUM: usize = 100;
    pub(super) const SECTSIZE: usize = 102;
    pub(super) const INODESIZE: usize = 104;
    pub(super) const FNAME: usize = 108; // 12 bytes
    pub(super) const BLOCKLOG: usize = 121 - 1; // 120
    pub(super) const SECTLOG: usize = 121;
    pub(super) const INODELOG: usize = 122;
    pub(super) const INOPBLOG: usize = 123;
    pub(super) const AGBLKLOG: usize = 124;
    pub(super) const INPROGRESS: usize = 126;
    pub(super) const DIRBLKLOG: usize = 192;
    pub(super) const FEATURES2: usize = 200;
    pub(super) const FEATURES_INCOMPAT: usize = 216;
    pub(super) const CRC: usize = 224; // __le32
}

/// CRC32C field offset within a v5 superblock sector.
const SB_CRC_OFF: usize = off::CRC;

/// Parsed, validated XFS geometry — everything the read path needs, cached once
/// at [`crate::Xfs::open`].
#[derive(Debug, Clone)]
pub(crate) struct Superblock {
    pub blocksize: u32,
    pub blocklog: u8,
    pub agblocks: u32,
    pub agcount: u32,
    pub agblklog: u8,
    pub inodesize: u16,
    pub inopblog: u8,
    pub rootino: u64,
    /// v5 (CRC + self-describing metadata) vs v4 (legacy).
    pub v5: bool,
    pub ftype: bool,
    pub nrext64: bool,
    /// Directory block size in fs-blocks log2 (`sb_dirblklog`).
    pub dirblklog: u8,
    pub uuid: [u8; 16],
    /// Internal log: first fs-block (AG-encoded) and length in fs-blocks.
    pub logstart: u64,
    pub logblocks: u32,
    fname: [u8; 12],
}

impl Superblock {
    /// `sb_sectsize` as stored, read before validation so the caller knows how
    /// much of the volume the CRC-covered superblock sector spans.
    pub(crate) fn sector_size(head: &[u8]) -> Option<u16> {
        be::u16_at(head, off::SECTSIZE)
    }

    /// Parse + validate the primary superblock from the first sector(s) of the
    /// volume. `sector` must hold the whole `sb_sectsize` sector, which a v5
    /// CRC covers.
    pub(crate) fn parse(sector: &[u8]) -> Result<Self> {
        let bad = |r| Error::BadSuperblock(r);

        if be::u32_at(sector, off::MAGICNUM).ok_or(bad(SuperblockReason::BadGeometry))?
            != format::SB_MAGIC
        {
            return Err(bad(SuperblockReason::BadMagic));
        }

        let versionnum =
            be::u16_at(sector, off::VERSIONNUM).ok_or(bad(SuperblockReason::BadGeometry))?;
        let version = versionnum & format::SB_VERSION_NUMBITS;
        let v5 = match version {
            v if v == format::SB_VERSION_5 => true,
            v if v == format::SB_VERSION_4 => false,
            _ => return Err(bad(SuperblockReason::UnsupportedVersion)),
        };

        let blocksize =
            be::u32_at(sector, off::BLOCKSIZE).ok_or(bad(SuperblockReason::BadGeometry))?;
        let blocklog =
            be::u8_at(sector, off::BLOCKLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let inodesize =
            be::u16_at(sector, off::INODESIZE).ok_or(bad(SuperblockReason::BadGeometry))?;
        let inodelog =
            be::u8_at(sector, off::INODELOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let inopblog =
            be::u8_at(sector, off::INOPBLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let sectsize =
            be::u16_at(sector, off::SECTSIZE).ok_or(bad(SuperblockReason::BadGeometry))?;
        let sectlog = be::u8_at(sector, off::SECTLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let agblocks =
            be::u32_at(sector, off::AGBLOCKS).ok_or(bad(SuperblockReason::BadGeometry))?;
        let agcount = be::u32_at(sector, off::AGCOUNT).ok_or(bad(SuperblockReason::BadGeometry))?;
        let agblklog =
            be::u8_at(sector, off::AGBLKLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let rootino = be::u64_at(sector, off::ROOTINO).ok_or(bad(SuperblockReason::BadGeometry))?;
        let dirblklog =
            be::u8_at(sector, off::DIRBLKLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
        let uuid = be::uuid_at(sector, off::UUID).ok_or(bad(SuperblockReason::BadGeometry))?;
        let mut fname = [0u8; 12];
        fname.copy_from_slice(
            sector
                .get(off::FNAME..off::FNAME + 12)
                .ok_or(bad(SuperblockReason::BadGeometry))?,
        );

        // Bound every log/shift input BEFORE it drives a shift — a hostile
        // superblock with an out-of-range log field would otherwise panic on a
        // shift-overflow (a denial-of-boot). After this gate every `<<` below
        // and in the read path is in range.
        if blocklog >= 32
            || inodelog >= 32
            || inopblog >= 32
            || sectlog >= 32
            || agblklog >= 32
            || dirblklog >= 16
            || u64::from(blocksize) << dirblklog > 65536
        {
            return Err(bad(SuperblockReason::BadGeometry));
        }

        // Geometry sanity, the same consistency rules the kernel applies
        // (xfs_validate_sb_common): power-of-two sizes matching their logs, and
        // inodes per block derived from block and inode size. A 64 KiB block of
        // 512-byte inodes gives inopblog 7, the largest is 8.
        if !(512..=65536).contains(&blocksize)
            || 1u32 << blocklog != blocksize
            || !(256..=2048).contains(&inodesize)
            || 1u32 << inodelog != u32::from(inodesize)
            || blocklog.checked_sub(inodelog) != Some(inopblog)
            || !(512..=32768).contains(&sectsize)
            || 1u32 << sectlog != u32::from(sectsize)
            || agblocks == 0
            || agcount == 0
        {
            return Err(bad(SuperblockReason::BadGeometry));
        }

        // Refuse layouts the read path cannot honor.
        if be::u64_at(sector, off::RBLOCKS).ok_or(bad(SuperblockReason::BadGeometry))? != 0 {
            return Err(bad(SuperblockReason::RealtimeDevice));
        }
        let logstart =
            be::u64_at(sector, off::LOGSTART).ok_or(bad(SuperblockReason::BadGeometry))?;
        if logstart == 0 {
            return Err(bad(SuperblockReason::ExternalLog));
        }
        let logblocks =
            be::u32_at(sector, off::LOGBLOCKS).ok_or(bad(SuperblockReason::BadGeometry))?;
        if logblocks == 0 {
            return Err(bad(SuperblockReason::BadGeometry));
        }
        if be::u8_at(sector, off::INPROGRESS).ok_or(bad(SuperblockReason::BadGeometry))? != 0 {
            // mkfs did not finish writing this filesystem.
            return Err(bad(SuperblockReason::BadGeometry));
        }

        let (ftype, nrext64) = detect_features(sector, v5, usize::from(sectsize))?;

        Ok(Superblock {
            blocksize,
            blocklog,
            agblocks,
            agcount,
            agblklog,
            inodesize,
            inopblog,
            rootino,
            v5,
            ftype,
            nrext64,
            dirblklog,
            uuid,
            logstart,
            logblocks,
            fname,
        })
    }

    /// Volume label (`sb_fname`), trimmed of NUL/space padding. `None` if empty.
    pub(crate) fn label(&self) -> Option<&str> {
        let end = self
            .fname
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.fname.len());
        let s = core::str::from_utf8(&self.fname[..end]).ok()?.trim();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// A v5 geometry of 4 KiB blocks and one 65536-block AG, with only the log
    /// fields chosen by the caller.
    #[cfg(test)]
    pub(crate) fn for_log_tests(logstart: u64, logblocks: u32) -> Self {
        Superblock {
            blocksize: 4096,
            blocklog: 12,
            agblocks: 65536,
            agcount: 1,
            agblklog: 16,
            inodesize: 512,
            inopblog: 3,
            rootino: 128,
            v5: true,
            ftype: true,
            nrext64: true,
            dirblklog: 0,
            uuid: [0; 16],
            logstart,
            logblocks,
            fname: [0; 12],
        }
    }

    /// Absolute byte offset of an AG-encoded inode number's on-disk core, or
    /// `u64::MAX` (see [`Self::ag_block_byte_offset`]).
    pub(crate) fn inode_byte_offset(&self, ino: u64) -> u64 {
        let agino_bits = u32::from(self.agblklog) + u32::from(self.inopblog);
        let agno = ino >> agino_bits;
        let agino = ino & ((1u64 << agino_bits) - 1);
        let agbno = agino >> self.inopblog;
        let in_block = agino & ((1u64 << self.inopblog) - 1);
        self.ag_block_byte_offset(agno, agbno)
            .saturating_add(in_block * u64::from(self.inodesize))
    }

    /// Absolute byte offset of an fs-block number (`xfs_fsblock_t`, AG-encoded),
    /// or `u64::MAX` (see [`Self::ag_block_byte_offset`]).
    pub(crate) fn fsblock_byte_offset(&self, fsbno: u64) -> u64 {
        let agno = fsbno >> self.agblklog;
        let agbno = fsbno & ((1u64 << self.agblklog) - 1);
        self.ag_block_byte_offset(agno, agbno)
    }

    /// A block outside the AGs can only come from a corrupt or hostile pointer.
    /// It maps to `u64::MAX`, which no read can satisfy, so the read fails
    /// instead of landing elsewhere on the device or overflowing the arithmetic.
    fn ag_block_byte_offset(&self, agno: u64, agbno: u64) -> u64 {
        if agno >= u64::from(self.agcount) || agbno >= u64::from(self.agblocks) {
            return u64::MAX;
        }
        // Both factors fit in 32 bits, so the block number can't overflow.
        (agno * u64::from(self.agblocks) + agbno).saturating_mul(u64::from(self.blocksize))
    }
}

/// Determine `(ftype, nrext64)` from the feature fields, verify the v5 CRC, and
/// reject a needs-repair or unknown-incompat feature set.
fn detect_features(sector: &[u8], v5: bool, sectsize: usize) -> Result<(bool, bool)> {
    let bad = Error::BadSuperblock;
    if !v5 {
        // v4: feature bits live in sb_features2. CRC implies v5, so a v4 SB never
        // carries CRC; ftype is the only reader-relevant v4 feature.
        let features2 = be::u32_at(sector, off::FEATURES2).unwrap_or(0);
        return Ok((features2 & format::FEATURES2_FTYPE != 0, false));
    }
    let incompat =
        be::u32_at(sector, off::FEATURES_INCOMPAT).ok_or(bad(SuperblockReason::BadGeometry))?;
    if incompat & format::INCOMPAT_NEEDSREPAIR != 0 {
        return Err(bad(SuperblockReason::DirtyLog));
    }
    if incompat & !format::INCOMPAT_SUPPORTED != 0 {
        return Err(Error::UnsupportedFeature("feat_unknown_incompat"));
    }
    // The superblock CRC covers one on-disk sector (`sb_sectsize`), not just the
    // 264-byte struct, so slice to the sector before hashing. A shorter buffer
    // can't be checked, and hashing what is there would report a false BadCrc.
    verify_sb_crc(
        sector
            .get(..sectsize)
            .ok_or(bad(SuperblockReason::BadGeometry))?,
    )?;
    Ok((
        incompat & format::INCOMPAT_FTYPE != 0,
        incompat & format::INCOMPAT_NREXT64 != 0,
    ))
}

/// Verify a v5 superblock CRC32C. The CRC (`__le32` at [`SB_CRC_OFF`]) covers the
/// sector with its own 4 bytes treated as zero.
fn verify_sb_crc(sector: &[u8]) -> Result<()> {
    let stored = sector
        .get(SB_CRC_OFF..SB_CRC_OFF + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error::BadSuperblock(SuperblockReason::BadGeometry))?;
    let computed = crate::crc::checksum(sector, SB_CRC_OFF);
    // Under `cargo fuzz` the CRC gate is bypassed so mutated seed images reach
    // the inode/dir/bmbt parsers; CRC correctness itself is covered by the
    // fixture oracle tests.
    if !cfg!(fuzzing) && stored != computed {
        return Err(Error::BadSuperblock(SuperblockReason::BadCrc));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_offsets_inside_the_ags() {
        let sb = Superblock::for_log_tests(1, 8);
        assert_eq!(sb.fsblock_byte_offset(1), 4096);
        assert_eq!(sb.inode_byte_offset(128), 16 * 4096);
    }

    #[test]
    fn block_offsets_outside_the_ags_are_unreadable() {
        let sb = Superblock::for_log_tests(1, 8);
        // AG 1 of a one-AG filesystem, and a block number with every bit set.
        assert_eq!(sb.fsblock_byte_offset(1 << 16), u64::MAX);
        assert_eq!(sb.fsblock_byte_offset(u64::MAX), u64::MAX);
        assert_eq!(sb.inode_byte_offset(u64::MAX), u64::MAX);
    }
}
