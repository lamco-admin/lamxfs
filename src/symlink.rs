//! Symlink target resolution.
//!
//! Short symlinks store the target inline in the data fork. Long ("remote")
//! symlinks store it in data blocks; v5 prefixes each block with a 56-byte
//! `xfs_dsymlink_hdr` (magic `XSLM`) which is stripped, v4 stores the raw bytes.

use alloc::{vec, vec::Vec};

use crate::{
    be,
    block_read::BlockRead,
    error::{Error, Result},
    format,
    inode::Dinode,
    superblock::Superblock,
};

/// Read a symlink's raw target bytes (capped at `cap` to bound allocation).
pub(crate) fn read_link<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
    cap: u64,
) -> Result<Vec<u8>> {
    if !inode.is_symlink() {
        return Err(Error::NotASymlink);
    }
    let total = usize::try_from(inode.size.min(cap)).unwrap_or(usize::MAX);

    if inode.format == format::DINODE_FMT_LOCAL {
        let fork = inode.data_fork();
        let n = total.min(fork.len());
        return Ok(fork[..n].to_vec());
    }

    // Remote symlink: walk the data blocks in logical order.
    let mut extents = crate::bmbt::read_extents(reader, sb, inode)?;
    extents.sort_by_key(|e| e.startoff);
    let bs = sb.blocksize as usize;
    let mut out = Vec::with_capacity(total);
    let mut block = vec![0u8; bs];

    'collect: for ext in &extents {
        for b in 0..ext.blockcount {
            if out.len() >= total {
                break 'collect;
            }
            let phys = sb
                .fsblock_byte_offset(ext.startblock)
                .saturating_add(b * u64::from(sb.blocksize));
            reader.read_at(phys, &mut block).map_err(|_| Error::Io {
                token: "io_symlink",
                offset: phys,
            })?;
            // v5 blocks carry the XSLM header; v4 blocks are raw bytes.
            let body_off = if be::u32_at(&block, 0) == Some(format::SYMLINK_MAGIC) {
                format::SYMLINK_HDR_LEN
            } else {
                0
            };
            let body = block.get(body_off..).unwrap_or(&[]);
            let take = body.len().min(total - out.len());
            out.extend_from_slice(&body[..take]);
        }
    }
    out.truncate(total);
    Ok(out)
}
