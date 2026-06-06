//! Directory enumeration.
//!
//! Three on-disk directory shapes are read, all yielding the same `(name,
//! inumber, ftype)` triples:
//!   * **shortform** (`LOCAL`): entries packed inline in the inode data fork.
//!   * **block** (single `EXTENTS` block): data entries followed by a trailing
//!     hash-index (leaf) + tail, which enumeration skips.
//!   * **leaf/node** (`EXTENTS`/`BTREE`, multiple blocks): standalone data
//!     blocks below the 32 GiB leaf offset; the hash index above it is ignored.
//!
//! Enumeration never consults the hash index — it exists only to accelerate
//! lookup-by-name, which a sequential read does not need. `.` and `..` are
//! omitted from the result (they are implicit in shortform and uninteresting to
//! a path walker).

use alloc::{vec, vec::Vec};

use crate::{
    be,
    block_read::BlockRead,
    error::{Error, Location, Result},
    format,
    inode::Dinode,
    superblock::Superblock,
};

/// One directory entry, owned.
pub(crate) struct RawDirEntry {
    pub name: Vec<u8>,
    pub inumber: u64,
    pub ftype: u8,
}

const MAX_DIR_ENTRIES: usize = 1 << 20;
const MAX_DIR_BLOCKS: usize = 1 << 16;

#[inline]
fn roundup8(x: usize) -> usize {
    (x + 7) & !7
}

#[inline]
fn is_dot_or_dotdot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

/// Enumerate the directory `inode`.
pub(crate) fn read_dir<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
) -> Result<Vec<RawDirEntry>> {
    match inode.format {
        format::DINODE_FMT_LOCAL => parse_shortform(sb, inode),
        format::DINODE_FMT_EXTENTS | format::DINODE_FMT_BTREE => {
            parse_block_dirs(reader, sb, inode)
        }
        _ => Err(Error::Inconsistent {
            token: "inode_unknown_format",
            where_: Location::Inode { ino: inode.ino },
        }),
    }
}

/// Shortform directory: entries inline in the data fork.
fn parse_shortform(sb: &Superblock, inode: &Dinode) -> Result<Vec<RawDirEntry>> {
    let where_ = Location::Inode { ino: inode.ino };
    let bad = |t| Error::Inconsistent { token: t, where_ };
    let fork = inode.data_fork();
    let count = be::u8_at(fork, 0).ok_or(bad("dir_sf_short"))? as usize;
    let i8count = be::u8_at(fork, 1).ok_or(bad("dir_sf_short"))?;
    let i8 = i8count > 0;
    let inolen = if i8 { 8 } else { 4 };

    let mut pos = 2 + inolen; // skip parent inode pointer
    let mut out = Vec::with_capacity(count.min(256));
    for _ in 0..count.min(MAX_DIR_ENTRIES) {
        let namelen = be::u8_at(fork, pos).ok_or(bad("dir_sf_entry_oob"))? as usize;
        // namelen(1) + offset(2) + name + [ftype(1)] + inumber(inolen)
        let name_at = pos + 3;
        let name = fork
            .get(name_at..name_at + namelen)
            .ok_or(bad("dir_sf_entry_oob"))?
            .to_vec();
        let ftype_byte = usize::from(sb.ftype);
        let ftype = if sb.ftype {
            be::u8_at(fork, name_at + namelen).ok_or(bad("dir_sf_entry_oob"))?
        } else {
            0
        };
        let ino_at = name_at + namelen + ftype_byte;
        let inumber = if i8 {
            be::u64_at(fork, ino_at).ok_or(bad("dir_sf_entry_oob"))?
        } else {
            u64::from(be::u32_at(fork, ino_at).ok_or(bad("dir_sf_entry_oob"))?)
        };
        pos = ino_at + inolen;
        if !is_dot_or_dotdot(&name) {
            out.push(RawDirEntry {
                name,
                inumber,
                ftype,
            });
        }
    }
    Ok(out)
}

/// Block / leaf / node directories: parse the data blocks (below the leaf
/// offset) reached through the inode's extents.
fn parse_block_dirs<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
) -> Result<Vec<RawDirEntry>> {
    let extents = crate::bmbt::read_extents(reader, sb, inode)?;
    let bs = u64::from(sb.blocksize);
    let dir_block_bytes = (bs << sb.dirblklog) as usize;
    let leaf_first_block = format::DIR2_LEAF_OFFSET / bs;

    let mut out = Vec::new();
    let mut blocks_read = 0usize;
    let mut block = vec![0u8; dir_block_bytes];

    for ext in &extents {
        if ext.startoff >= leaf_first_block {
            continue; // leaf / free index — not data
        }
        // Walk this extent one directory block at a time.
        let step_blocks = 1u64 << sb.dirblklog;
        let mut b = 0u64;
        while b < ext.blockcount {
            if ext.startoff + b >= leaf_first_block {
                break;
            }
            blocks_read += 1;
            if blocks_read > MAX_DIR_BLOCKS || out.len() > MAX_DIR_ENTRIES {
                return Err(Error::Inconsistent {
                    token: "dir_too_large",
                    where_: Location::Inode { ino: inode.ino },
                });
            }
            let phys = sb
                .fsblock_byte_offset(ext.startblock)
                .saturating_add(b * bs);
            reader.read_at(phys, &mut block).map_err(|_| Error::Io {
                token: "io_dir",
                offset: phys,
            })?;
            parse_data_block(&block, sb, inode.ino, &mut out)?;
            b += step_blocks;
        }
    }
    Ok(out)
}

/// Parse one directory data block, appending entries (skipping `.`/`..` and
/// free regions).
fn parse_data_block(
    block: &[u8],
    sb: &Superblock,
    ino: u64,
    out: &mut Vec<RawDirEntry>,
) -> Result<()> {
    let where_ = Location::Dir { ino, block: 0 };
    let bad = |t| Error::Inconsistent { token: t, where_ };

    let magic = be::u32_at(block, 0).ok_or(bad("dir_block_short"))?;
    let (hdr, single_block) = match magic {
        m if m == format::DIR3_DATA_MAGIC => (format::DIR3_DATA_HDR_LEN, false),
        m if m == format::DIR2_DATA_MAGIC => (format::DIR2_DATA_HDR_LEN, false),
        m if m == format::DIR3_BLOCK_MAGIC => (format::DIR3_DATA_HDR_LEN, true),
        m if m == format::DIR2_BLOCK_MAGIC => (format::DIR2_DATA_HDR_LEN, true),
        _ => return Err(bad("dir_bad_magic")),
    };

    // Single-block dirs carry a trailing leaf index + tail; bound the data area
    // to exclude it. Multi-block data blocks are data+free to the end.
    let data_end = if single_block {
        let tail_count = be::u32_at(block, block.len().saturating_sub(8))
            .ok_or(bad("dir_block_short"))? as usize;
        block.len().saturating_sub(8 + tail_count.saturating_mul(8))
    } else {
        block.len()
    };

    let ftype_byte = usize::from(sb.ftype);
    let mut pos = hdr;
    while pos + 2 <= data_end {
        // A free region is tagged 0xffff; its u16 `length` says how far to skip.
        if be::u16_at(block, pos) == Some(format::DIR2_DATA_FREE_TAG) {
            let len = be::u16_at(block, pos + 2).ok_or(bad("dir_entry_oob"))? as usize;
            if len == 0 {
                break;
            }
            pos += len;
            continue;
        }
        // Data entry: inumber(8) namelen(1) name [ftype(1)] tag(2).
        let inumber = be::u64_at(block, pos).ok_or(bad("dir_entry_oob"))?;
        let namelen = be::u8_at(block, pos + 8).ok_or(bad("dir_entry_oob"))? as usize;
        let name_at = pos + 9;
        let name = block
            .get(name_at..name_at + namelen)
            .ok_or(bad("dir_entry_oob"))?
            .to_vec();
        let ftype = if sb.ftype {
            be::u8_at(block, name_at + namelen).unwrap_or(0)
        } else {
            0
        };
        let entlen = roundup8(11 + namelen + ftype_byte);
        if entlen == 0 || pos + entlen > block.len() {
            return Err(bad("dir_entry_oob"));
        }
        pos += entlen;
        if !is_dot_or_dotdot(&name) {
            if out.len() >= MAX_DIR_ENTRIES {
                break;
            }
            out.push(RawDirEntry {
                name,
                inumber,
                ftype,
            });
        }
    }
    Ok(())
}
