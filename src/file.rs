//! File content reads over a decoded extent list.
//!
//! Holes (logical ranges with no extent) and `unwritten` extents read back as
//! zero, matching XFS semantics for sparse files and preallocated-but-unwritten
//! regions.

use alloc::vec::Vec;

use crate::{
    block_read::BlockRead,
    bmbt::{self, Extent},
    error::{Error, Result},
    inode::Dinode,
    superblock::Superblock,
};

/// Read up to `buf.len()` bytes of `inode` starting at logical byte `offset`,
/// clamped to the file's `di_size`. Returns the number of bytes written
/// (0 = EOF). Decodes the inode's extents on each call; callers streaming a
/// large file may cache the extent list via [`read_into_with_extents`].
pub(crate) fn read_into<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let extents = bmbt::read_extents(reader, sb, inode)?;
    read_into_with_extents(reader, sb, &extents, inode.size, offset, buf)
}

/// Range read over a pre-decoded extent list (the cacheable path).
pub(crate) fn read_into_with_extents<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    extents: &[Extent],
    file_size: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let end = offset.saturating_add(buf.len() as u64).min(file_size);
    if offset >= end {
        return Ok(0);
    }
    let want = (end - offset) as usize;
    let out = &mut buf[..want];
    // Holes and unwritten extents default to zero.
    out.fill(0);

    let bs = u64::from(sb.blocksize);
    for ext in extents {
        if ext.unwritten {
            continue;
        }
        let ext_start = ext.startoff.saturating_mul(bs);
        let ext_end = ext_start.saturating_add(ext.blockcount.saturating_mul(bs));
        let lo = offset.max(ext_start);
        let hi = end.min(ext_end);
        if lo >= hi {
            continue;
        }
        let phys = sb
            .fsblock_byte_offset(ext.startblock)
            .saturating_add(lo - ext_start);
        let dst_lo = (lo - offset) as usize;
        let dst_hi = (hi - offset) as usize;
        reader
            .read_at(phys, &mut out[dst_lo..dst_hi])
            .map_err(|_| Error::Io {
                token: "io_file",
                offset: phys,
            })?;
    }
    Ok(want)
}

/// Decode `inode`'s extents once (for callers that will stream many ranges).
pub(crate) fn extents_of<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
) -> Result<Vec<Extent>> {
    bmbt::read_extents(reader, sb, inode)
}
