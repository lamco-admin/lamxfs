//! Block-map B+tree (BMBT): decode the data fork's extents.
//!
//! Two on-disk shapes, both producing the same logical→physical extent list:
//!   * `EXTENTS`: a packed array of 16-byte records in the inode's data fork.
//!   * `BTREE`: a short B+tree root in the data fork pointing at full-block
//!     interior/leaf nodes; the leaves hold the same 16-byte records.

use alloc::{vec, vec::Vec};

use crate::{
    be,
    block_read::BlockRead,
    error::{Error, Location, Result},
    format,
    inode::Dinode,
    superblock::Superblock,
};

/// A logical→physical mapping. `startoff`/`blockcount` are in fs-blocks within
/// the file; `startblock` is an AG-encoded fs-block number. `unwritten` extents
/// are allocated-but-undefined and read back as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub startoff: u64,
    pub startblock: u64,
    pub blockcount: u64,
    pub unwritten: bool,
}

const REC_LEN: usize = 16;
/// Hard cap on total extents collected — a denial-of-boot guard against a
/// hostile inode declaring an enormous extent count.
const MAX_EXTENTS: usize = 1 << 22;
/// Hard cap on interior B+tree nodes visited (depth/fanout runaway guard).
const MAX_BTREE_NODES: usize = 1 << 16;

/// Decode one 16-byte packed BMBT record (128 bits: flag·1, startoff·54,
/// startblock·52, blockcount·21).
pub(crate) fn decode_rec(buf: &[u8]) -> Option<Extent> {
    let l0 = be::u64_at(buf, 0)?;
    let l1 = be::u64_at(buf, 8)?;
    Some(Extent {
        unwritten: l0 >> 63 != 0,
        startoff: (l0 >> 9) & ((1u64 << 54) - 1),
        startblock: ((l0 & 0x1ff) << 43) | (l1 >> 21),
        blockcount: l1 & ((1u64 << 21) - 1),
    })
}

/// Collect every data-fork extent of `inode` (EXTENTS or BTREE format).
pub(crate) fn read_extents<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
) -> Result<Vec<Extent>> {
    match inode.format {
        format::DINODE_FMT_EXTENTS => {
            let fork = inode.data_fork();
            let want = (inode.nextents as usize)
                .min(fork.len() / REC_LEN)
                .min(MAX_EXTENTS);
            let mut out = Vec::with_capacity(want);
            for i in 0..want {
                let rec = &fork[i * REC_LEN..i * REC_LEN + REC_LEN];
                out.push(decode_rec(rec).ok_or(Error::Inconsistent {
                    token: "bmbt_rec_short",
                    where_: Location::Bmbt {
                        ino: inode.ino,
                        block: 0,
                    },
                })?);
            }
            Ok(out)
        }
        format::DINODE_FMT_BTREE => walk_btree(reader, sb, inode),
        other => Err(Error::Inconsistent {
            token: if other == format::DINODE_FMT_LOCAL {
                "inode_unexpected_local"
            } else {
                "inode_unknown_format"
            },
            where_: Location::Inode { ino: inode.ino },
        }),
    }
}

fn walk_btree<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    inode: &Dinode,
) -> Result<Vec<Extent>> {
    let inconsistent = |token| Error::Inconsistent {
        token,
        where_: Location::Bmbt {
            ino: inode.ino,
            block: 0,
        },
    };
    let fork = inode.data_fork();
    // Short root header (xfs_bmdr_block): level(2), numrecs(2), then key/ptr
    // areas sized for the records that fit in the fork.
    let numrecs = be::u16_at(fork, 2).ok_or(inconsistent("bmbt_root_short"))? as usize;
    let maxrecs = fork.len().saturating_sub(4) / 16;
    let ptr_area = 4 + maxrecs * 8;
    let numrecs = numrecs.min(maxrecs);

    let mut pending: Vec<u64> = Vec::with_capacity(numrecs);
    for i in 0..numrecs {
        pending.push(be::u64_at(fork, ptr_area + i * 8).ok_or(inconsistent("bmbt_root_ptr"))?);
    }

    let mut out = Vec::new();
    let mut nodes_visited = 0usize;
    let mut block = vec![0u8; sb.blocksize as usize];

    while let Some(fsbno) = pending.pop() {
        nodes_visited += 1;
        if nodes_visited > MAX_BTREE_NODES || out.len() > MAX_EXTENTS {
            return Err(inconsistent("bmbt_too_large"));
        }
        let byte = sb.fsblock_byte_offset(fsbno);
        reader.read_at(byte, &mut block).map_err(|_| Error::Io {
            token: "io_bmbt",
            offset: byte,
        })?;

        let level = be::u16_at(&block, 4).ok_or(inconsistent("bmbt_block_short"))?;
        let nrecs = be::u16_at(&block, 6).ok_or(inconsistent("bmbt_block_short"))? as usize;
        let hdr = btree_block_header_len(&block).ok_or(inconsistent("bmbt_bad_magic"))?;

        if level == 0 {
            // Leaf: 16-byte records.
            let avail = (block.len().saturating_sub(hdr)) / REC_LEN;
            let nrecs = nrecs.min(avail);
            for i in 0..nrecs {
                let at = hdr + i * REC_LEN;
                out.push(
                    decode_rec(&block[at..at + REC_LEN]).ok_or(inconsistent("bmbt_rec_short"))?,
                );
                if out.len() > MAX_EXTENTS {
                    return Err(inconsistent("bmbt_too_large"));
                }
            }
        } else {
            // Interior: keys[maxrecs] then ptrs[maxrecs]; follow `nrecs` ptrs.
            let block_maxrecs = (block.len().saturating_sub(hdr)) / 16;
            let nrecs = nrecs.min(block_maxrecs);
            let ptrs_at = hdr + block_maxrecs * 8;
            for i in 0..nrecs {
                pending.push(
                    be::u64_at(&block, ptrs_at + i * 8).ok_or(inconsistent("bmbt_block_ptr"))?,
                );
            }
        }
    }
    Ok(out)
}

/// Header length of an on-disk BMBT block from its magic: v5 (`BMA3`) long-form
/// header is 72 bytes, v4 (`BMAP`) is 24.
fn btree_block_header_len(block: &[u8]) -> Option<usize> {
    match be::u32_at(block, 0)? {
        0x424d_4133 => Some(72), // "BMA3"
        0x424d_4150 => Some(24), // "BMAP"
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip the 128-bit packing (flag·1 | startoff·54 | startblock·52 |
    /// blockcount·21).
    fn encode(e: Extent) -> [u8; 16] {
        let l0 = (u64::from(e.unwritten) << 63) | (e.startoff << 9) | (e.startblock >> 43);
        let l1 = (e.startblock << 21) | e.blockcount;
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&l0.to_be_bytes());
        out[8..].copy_from_slice(&l1.to_be_bytes());
        out
    }

    #[test]
    fn decode_rec_round_trips() {
        for e in [
            Extent {
                startoff: 0,
                startblock: 0,
                blockcount: 1,
                unwritten: false,
            },
            Extent {
                startoff: 5,
                startblock: 100,
                blockcount: 3,
                unwritten: false,
            },
            // startblock large enough to use the 9 low bits of l0:
            Extent {
                startoff: 1 << 40,
                startblock: 1 << 50,
                blockcount: (1 << 21) - 1,
                unwritten: true,
            },
            Extent {
                startoff: (1 << 54) - 1,
                startblock: (1 << 52) - 1,
                blockcount: 1,
                unwritten: false,
            },
        ] {
            assert_eq!(decode_rec(&encode(e)), Some(e), "round-trip {e:?}");
        }
    }

    #[test]
    fn decode_rec_rejects_short_buffer() {
        assert_eq!(decode_rec(&[0u8; 15]), None);
    }

    #[test]
    fn btree_header_len_by_magic() {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&0x424d_4133u32.to_be_bytes());
        assert_eq!(btree_block_header_len(&b), Some(72));
        b[..4].copy_from_slice(&0x424d_4150u32.to_be_bytes());
        assert_eq!(btree_block_header_len(&b), Some(24));
        b[..4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        assert_eq!(btree_block_header_len(&b), None);
    }
}
