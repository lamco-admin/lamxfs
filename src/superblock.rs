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
    pub(super) const VERSIONNUM: usize = 100;
    pub(super) const SECTSIZE: usize = 102;
    pub(super) const INODESIZE: usize = 104;
    pub(super) const FNAME: usize = 108; // 12 bytes
    pub(super) const BLOCKLOG: usize = 121 - 1; // 120
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
    fname: [u8; 12],
}

impl Superblock {
    /// Parse + validate the primary superblock from the first sector(s) of the
    /// volume. `sector` must contain at least the on-disk superblock; for a v5
    /// CRC check it should be the full `sb_sectsize` sector.
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
        let inopblog =
            be::u8_at(sector, off::INOPBLOG).ok_or(bad(SuperblockReason::BadGeometry))?;
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
            || inopblog >= 32
            || agblklog >= 32
            || dirblklog >= 16
            || u64::from(blocksize) << dirblklog > 65536
        {
            return Err(bad(SuperblockReason::BadGeometry));
        }

        // Geometry sanity: power-of-two block/inode sizes within sane bounds,
        // logs consistent, AG layout non-degenerate.
        if !(512..=65536).contains(&blocksize)
            || !blocksize.is_power_of_two()
            || 1u32 << blocklog != blocksize
            || !(256..=2048).contains(&inodesize)
            || !inodesize.is_power_of_two()
            || agblocks == 0
            || agcount == 0
            || inopblog > 6
        {
            return Err(bad(SuperblockReason::BadGeometry));
        }

        // Refuse layouts the read path cannot honor.
        if be::u64_at(sector, off::RBLOCKS).ok_or(bad(SuperblockReason::BadGeometry))? != 0 {
            return Err(bad(SuperblockReason::RealtimeDevice));
        }
        if be::u64_at(sector, off::LOGSTART).ok_or(bad(SuperblockReason::BadGeometry))? == 0 {
            return Err(bad(SuperblockReason::ExternalLog));
        }
        if be::u8_at(sector, off::INPROGRESS).ok_or(bad(SuperblockReason::BadGeometry))? != 0 {
            // mkfs did not finish writing this filesystem.
            return Err(bad(SuperblockReason::BadGeometry));
        }

        let (ftype, nrext64) = detect_features(sector, v5)?;

        Ok(Superblock {
            blocksize,
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

    /// Absolute byte offset of an AG-encoded inode number's on-disk core.
    pub(crate) fn inode_byte_offset(&self, ino: u64) -> u64 {
        let agino_bits = u32::from(self.agblklog) + u32::from(self.inopblog);
        let agno = ino >> agino_bits;
        let agino = ino & ((1u64 << agino_bits) - 1);
        let agbno = agino >> self.inopblog;
        let in_block = agino & ((1u64 << self.inopblog) - 1);
        let abs_block = agno * u64::from(self.agblocks) + agbno;
        abs_block * u64::from(self.blocksize) + in_block * u64::from(self.inodesize)
    }

    /// Absolute byte offset of an fs-block number (`xfs_fsblock_t`, AG-encoded).
    pub(crate) fn fsblock_byte_offset(&self, fsbno: u64) -> u64 {
        let agno = fsbno >> self.agblklog;
        let agbno = fsbno & ((1u64 << self.agblklog) - 1);
        let abs_block = agno * u64::from(self.agblocks) + agbno;
        abs_block * u64::from(self.blocksize)
    }
}

/// Determine `(ftype, nrext64)` from the feature fields, verify the v5 CRC, and
/// reject a needs-repair or unknown-incompat feature set.
fn detect_features(sector: &[u8], v5: bool) -> Result<(bool, bool)> {
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
    // 264-byte struct, so slice to the sector before hashing.
    let sectsize =
        usize::from(be::u16_at(sector, off::SECTSIZE).ok_or(bad(SuperblockReason::BadGeometry))?);
    verify_sb_crc(sector.get(..sectsize).unwrap_or(sector))?;
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
