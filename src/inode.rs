//! Inode read-by-number and core parsing.
//!
//! A specific inode's on-disk location is computed arithmetically from its
//! AG-encoded number ([`Superblock::inode_byte_offset`]) — the inode B+tree
//! (inobt) is an allocation/iteration structure a path-walking reader never
//! needs, so it is not parsed. Sparse inodes do not change this: an *allocated*
//! inode (which is what a directory entry points at) always sits at its
//! number's computed location.

use alloc::{vec, vec::Vec};

use crate::{
    be,
    block_read::BlockRead,
    error::{Error, Location, Result},
    format,
    superblock::Superblock,
};

/// A read inode: its parsed core fields plus the raw bytes (so the data fork can
/// be sliced without a second read).
pub(crate) struct Dinode {
    raw: Vec<u8>,
    pub mode: u16,
    pub format: u8,
    pub size: u64,
    /// Data-fork extent count (64-bit; NREXT64-aware).
    pub nextents: u64,
    pub ino: u64,
    core_size: usize,
    fork_bytes: usize,
}

impl Dinode {
    /// Read + parse the inode numbered `ino`.
    pub(crate) fn read<R: BlockRead>(reader: &mut R, sb: &Superblock, ino: u64) -> Result<Self> {
        // Reject an inode number whose AG is out of range before turning it into
        // a byte offset — a hostile directory entry cannot steer a read off into
        // arbitrary device space.
        let agino_bits = u32::from(sb.agblklog) + u32::from(sb.inopblog);
        if (ino >> agino_bits) >= u64::from(sb.agcount) {
            return Err(Error::Inconsistent {
                token: "inode_agno_oob",
                where_: Location::Inode { ino },
            });
        }
        let offset = sb.inode_byte_offset(ino);
        let mut raw = vec![0u8; usize::from(sb.inodesize)];
        reader.read_at(offset, &mut raw).map_err(|_| Error::Io {
            token: "io_inode",
            offset,
        })?;
        Self::parse(raw, sb, ino)
    }

    fn parse(raw: Vec<u8>, sb: &Superblock, ino: u64) -> Result<Self> {
        let where_ = Location::Inode { ino };
        let inconsistent = |token| Error::Inconsistent { token, where_ };

        if be::u16_at(&raw, format::DI_MAGIC) != Some(format::INODE_MAGIC) {
            return Err(inconsistent("inode_bad_magic"));
        }
        let version = be::u8_at(&raw, format::DI_VERSION).ok_or(inconsistent("inode_bad_magic"))?;
        let mode = be::u16_at(&raw, format::DI_MODE).ok_or(inconsistent("inode_bad_magic"))?;
        let format_ = be::u8_at(&raw, format::DI_FORMAT).ok_or(inconsistent("inode_bad_magic"))?;
        let size = be::u64_at(&raw, format::DI_SIZE).ok_or(inconsistent("inode_bad_magic"))?;

        let nextents = if sb.nrext64 {
            be::u64_at(&raw, format::DI_BIG_NEXTENTS).ok_or(inconsistent("inode_bad_magic"))?
        } else {
            u64::from(be::u32_at(&raw, format::DI_NEXTENTS).ok_or(inconsistent("inode_bad_magic"))?)
        };

        let forkoff = be::u8_at(&raw, format::DI_FORKOFF).ok_or(inconsistent("inode_bad_magic"))?;
        let core_size = if version >= 3 {
            format::DINODE_CORE_V3
        } else {
            format::DINODE_CORE_V2
        };

        // v5 inode CRC32C (di_crc, __le32 immediately after the v2 core).
        if sb.v5 && version >= 3 {
            const DI_CRC_OFF: usize = format::DINODE_CORE_V2; // 100
            let stored = raw
                .get(DI_CRC_OFF..DI_CRC_OFF + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or(inconsistent("inode_bad_magic"))?;
            if !cfg!(fuzzing) && stored != crate::crc::checksum(&raw, DI_CRC_OFF) {
                return Err(Error::BadCrc {
                    ag: (ino >> (u32::from(sb.agblklog) + u32::from(sb.inopblog))) as u32,
                    ag_block: 0,
                    what: "crc_inode",
                });
            }
        }

        // Data-fork byte span: forkoff==0 → fork runs to the end of the inode;
        // else the data fork is forkoff*8 bytes and an attr fork follows.
        let literal = usize::from(sb.inodesize).saturating_sub(core_size);
        let fork_bytes = if forkoff == 0 {
            literal
        } else {
            (usize::from(forkoff) * 8).min(literal)
        };

        Ok(Dinode {
            raw,
            mode,
            format: format_,
            size,
            nextents,
            ino,
            core_size,
            fork_bytes,
        })
    }

    /// The data fork's literal bytes (inline data, packed extent array, or BMBT
    /// root, depending on [`Self::format`]).
    pub(crate) fn data_fork(&self) -> &[u8] {
        let end = self.core_size + self.fork_bytes;
        self.raw.get(self.core_size..end).unwrap_or(&[])
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.mode & format::S_IFMT == format::S_IFDIR
    }
    pub(crate) fn is_reg(&self) -> bool {
        self.mode & format::S_IFMT == format::S_IFREG
    }
    pub(crate) fn is_symlink(&self) -> bool {
        self.mode & format::S_IFMT == format::S_IFLNK
    }
}
