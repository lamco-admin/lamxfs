//! Internal-log cleanliness: is there anything left to replay?
//!
//! lamxfs never replays the journal, so a filesystem whose log still holds
//! committed metadata that never reached its home location would read stale.
//! The kernel makes this call in `xlog_find_tail`: find the log head from the
//! cycle number stamped into every 512-byte basic block, find the last record
//! header before it, and treat the log as clean only when that record is a lone
//! unmount record ending exactly at the head. This module makes the same call.
//! It leaves out the CRC-driven torn-write trimming the kernel applies to a log
//! that is already dirty, so a crash in the middle of the first log write after
//! a mount is reported dirty even where the kernel would find the old unmount
//! record. That direction refuses a readable volume; it never misreads one.

use crate::{
    be,
    block_read::BlockRead,
    error::{Error, Result, SuperblockReason},
    superblock::Superblock,
};

/// Log basic block: the unit of log addressing and of cycle stamping.
const BBSIZE: usize = 512;
const BBSHIFT: u32 = 9;
/// Word 0 of a record header block. Data blocks carry their cycle number in
/// word 0 instead, so a block starting with this value is a header.
const HEADER_MAGIC: u32 = 0xFEED_BABE;
/// `h_version` bit for v2 logs, whose large records need extra header blocks.
const VERSION_2: u32 = 2;
/// Bytes of record data whose cycle words one header block can hold.
const HEADER_CYCLE_SIZE: u32 = 32 * 1024;
/// `oh_flags` bit on the single operation of an unmount record.
const UNMOUNT_TRANS: u8 = 0x20;
/// The largest record: 256 KiB of data (stripe padding included in `h_len`)
/// plus its eight header blocks. A header further back than this from the head
/// can't belong to the last record.
const MAX_RECORD_BBS: u64 = (256 * 1024 / BBSIZE as u64) + 8;

// xlog_rec_header field offsets.
mod rec {
    pub(super) const MAGICNO: usize = 0;
    pub(super) const CYCLE: usize = 4;
    pub(super) const VERSION: usize = 8;
    pub(super) const LEN: usize = 12;
    pub(super) const NUM_LOGOPS: usize = 40;
    pub(super) const SIZE: usize = 320;
}

// xlog_op_header field offset.
const OP_FLAGS: usize = 9;

/// The internal log as a ring of basic blocks.
struct Log {
    base: u64,
    bbnum: u64,
}

impl Log {
    fn new(sb: &Superblock) -> Result<Self> {
        // blocksize is at least one basic block, so the shift is in range.
        let bbnum = u64::from(sb.logblocks) << (u32::from(sb.blocklog) - BBSHIFT);
        // A log start outside the AGs maps to u64::MAX, so this refuses it.
        let base = sb.fsblock_byte_offset(sb.logstart);
        base.checked_add(bbnum << BBSHIFT)
            .ok_or(Error::BadSuperblock(SuperblockReason::BadGeometry))?;
        Ok(Log { base, bbnum })
    }

    fn read_bb<R: BlockRead>(&self, reader: &mut R, blk: u64) -> Result<[u8; BBSIZE]> {
        let offset = self.base + (blk << BBSHIFT);
        let mut bb = [0u8; BBSIZE];
        reader.read_at(offset, &mut bb).map_err(|_| Error::Io {
            token: "io_log",
            offset,
        })?;
        Ok(bb)
    }

    /// The cycle a block was written in. A header block keeps its cycle in
    /// `h_cycle`, since word 0 holds the magic (`xlog_get_cycle`).
    fn cycle<R: BlockRead>(&self, reader: &mut R, blk: u64) -> Result<u32> {
        let bb = self.read_bb(reader, blk)?;
        let word0 = be::u32_at(&bb, rec::MAGICNO).unwrap_or(0);
        if word0 == HEADER_MAGIC {
            Ok(be::u32_at(&bb, rec::CYCLE).unwrap_or(0))
        } else {
            Ok(word0)
        }
    }

    fn wrap(&self, blk: u64) -> u64 {
        blk % self.bbnum
    }

    /// The first block in `first..=last` written in `cycle`, given that `last`
    /// was and `first` wasn't (`xlog_find_cycle_start`).
    fn cycle_start<R: BlockRead>(
        &self,
        reader: &mut R,
        mut first: u64,
        mut last: u64,
        cycle: u32,
    ) -> Result<u64> {
        let mut mid = first + (last - first) / 2;
        while mid != first && mid != last {
            if self.cycle(reader, mid)? == cycle {
                last = mid;
            } else {
                first = mid;
            }
            mid = first + (last - first) / 2;
        }
        Ok(last)
    }

    /// The block the next log write would go to, or `None` for a log that was
    /// never written (all zero), which the kernel also treats as clean.
    fn head<R: BlockRead>(&self, reader: &mut R) -> Result<Option<Head>> {
        let last_blk = self.bbnum - 1;
        let first = self.cycle(reader, 0)?;
        if first == 0 {
            return Ok(None);
        }
        let last = self.cycle(reader, last_blk)?;
        let head = if last == 0 {
            // Never wrapped: the head is the first block still zero.
            self.cycle_start(reader, 0, last_blk, 0)?
        } else if first == last {
            // The whole log is one cycle, so the head sits at block 0.
            0
        } else if last.wrapping_add(1) == first {
            self.cycle_start(reader, 0, last_blk, last)?
        } else {
            // Cycles that aren't one pass apart only come from a log in the
            // middle of being written.
            return Ok(Some(Head::Inconsistent));
        };
        Ok(Some(Head::At(head)))
    }
}

enum Head {
    At(u64),
    Inconsistent,
}

/// Whether the internal log needs no replay.
pub(crate) fn is_clean<R: BlockRead>(reader: &mut R, sb: &Superblock) -> Result<bool> {
    let log = Log::new(sb)?;
    let head = match log.head(reader)? {
        None => return Ok(true),
        Some(Head::Inconsistent) => return Ok(false),
        Some(Head::At(head)) => head,
    };

    // Walk back from the head to the last record header.
    let mut rhead_blk = None;
    let mut header = [0u8; BBSIZE];
    for back in 1..=MAX_RECORD_BBS.min(log.bbnum) {
        let blk = log.wrap(head + log.bbnum - back);
        let bb = log.read_bb(reader, blk)?;
        if be::u32_at(&bb, rec::MAGICNO) == Some(HEADER_MAGIC) {
            rhead_blk = Some(blk);
            header = bb;
            break;
        }
    }
    let Some(rhead_blk) = rhead_blk else {
        return Ok(false);
    };

    let field = |off| be::u32_at(&header, off).unwrap_or(0);
    let (version, len, num_logops, size) = (
        field(rec::VERSION),
        field(rec::LEN),
        field(rec::NUM_LOGOPS),
        field(rec::SIZE),
    );
    let hblks = if version & VERSION_2 != 0 && size > HEADER_CYCLE_SIZE {
        u64::from(size.div_ceil(HEADER_CYCLE_SIZE))
    } else {
        1
    };
    let len_bbs = u64::from(len).div_ceil(BBSIZE as u64);
    if num_logops != 1 || log.wrap(rhead_blk + hblks + len_bbs) != head {
        return Ok(false);
    }
    // Word 0 of the data block carries the cycle stamp, but the flags byte of
    // the first operation header is untouched.
    let data = log.read_bb(reader, log.wrap(rhead_blk + hblks))?;
    Ok(data[OP_FLAGS] & UNMOUNT_TRANS != 0)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    const LOG_BBS: u64 = 64;
    // One 4 KiB fs-block per AG offset keeps the arithmetic simple: the log
    // starts at fs-block 1, so at byte 4096.
    const LOG_BASE: usize = 4096;

    struct Mem(Vec<u8>);

    impl BlockRead for Mem {
        type Error = ();
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
            let off = usize::try_from(off).map_err(|_| ())?;
            let end = off.checked_add(buf.len()).ok_or(())?;
            buf.copy_from_slice(self.0.get(off..end).ok_or(())?);
            Ok(())
        }
    }

    fn sb() -> Superblock {
        Superblock::for_log_tests(1, (LOG_BBS / 8) as u32)
    }

    fn volume() -> Vec<u8> {
        vec![0u8; LOG_BASE + LOG_BBS as usize * BBSIZE]
    }

    fn bb(vol: &mut [u8], blk: u64) -> &mut [u8] {
        let at = LOG_BASE + blk as usize * BBSIZE;
        &mut vol[at..at + BBSIZE]
    }

    /// Stamp `blk..blk+n` (wrapping) as data blocks of `cycle`.
    fn stamp(vol: &mut [u8], blk: u64, n: u64, cycle: u32) {
        for i in 0..n {
            bb(vol, (blk + i) % LOG_BBS)[..4].copy_from_slice(&cycle.to_be_bytes());
        }
    }

    /// Write a one-block record at `blk` with `ops` operations, the first of
    /// which carries `flags`.
    fn record(vol: &mut [u8], blk: u64, cycle: u32, ops: u32, flags: u8) {
        let h = bb(vol, blk);
        h[0..4].copy_from_slice(&HEADER_MAGIC.to_be_bytes());
        h[4..8].copy_from_slice(&cycle.to_be_bytes());
        h[8..12].copy_from_slice(&VERSION_2.to_be_bytes());
        h[12..16].copy_from_slice(&(BBSIZE as u32).to_be_bytes());
        h[40..44].copy_from_slice(&ops.to_be_bytes());
        h[320..324].copy_from_slice(&HEADER_CYCLE_SIZE.to_be_bytes());
        let d = bb(vol, (blk + 1) % LOG_BBS);
        d[..4].copy_from_slice(&cycle.to_be_bytes());
        d[OP_FLAGS] = flags;
    }

    fn clean(vol: Vec<u8>) -> bool {
        is_clean(&mut Mem(vol), &sb()).unwrap()
    }

    #[test]
    fn fresh_mkfs_log_is_clean() {
        // mkfs: an unmount record at block 0, the rest zero.
        let mut vol = volume();
        record(&mut vol, 0, 1, 1, UNMOUNT_TRANS);
        assert!(clean(vol));
    }

    #[test]
    fn zeroed_log_is_clean() {
        assert!(clean(volume()));
    }

    #[test]
    fn transaction_after_unmount_record_is_dirty() {
        let mut vol = volume();
        record(&mut vol, 0, 1, 1, UNMOUNT_TRANS);
        record(&mut vol, 2, 1, 3, 0x01);
        assert!(!clean(vol));
    }

    #[test]
    fn unmount_record_with_more_ops_is_dirty() {
        let mut vol = volume();
        record(&mut vol, 0, 1, 2, UNMOUNT_TRANS);
        assert!(!clean(vol));
    }

    #[test]
    fn wrapped_log_head_mid_ring_is_clean() {
        // Pass 3 has written up to block 20; blocks from 22 on are still pass 2.
        let mut vol = volume();
        stamp(&mut vol, 0, LOG_BBS, 2);
        stamp(&mut vol, 0, 20, 3);
        record(&mut vol, 20, 3, 1, UNMOUNT_TRANS);
        assert!(clean(vol));
    }

    #[test]
    fn wrapped_log_dirty_record_at_head_is_dirty() {
        let mut vol = volume();
        stamp(&mut vol, 0, LOG_BBS, 2);
        stamp(&mut vol, 0, 20, 3);
        record(&mut vol, 20, 3, 4, 0x02);
        assert!(!clean(vol));
    }

    #[test]
    fn unmount_record_at_end_of_ring_is_clean() {
        // The record fills the last two blocks, so the head is block 0 and the
        // whole ring is one cycle.
        let mut vol = volume();
        stamp(&mut vol, 0, LOG_BBS, 4);
        record(&mut vol, LOG_BBS - 2, 4, 1, UNMOUNT_TRANS);
        assert!(clean(vol));
    }

    #[test]
    fn unmount_record_split_across_the_wrap_is_clean() {
        // Header in the last block, data in block 0, head at block 1.
        let mut vol = volume();
        stamp(&mut vol, 0, LOG_BBS, 4);
        record(&mut vol, LOG_BBS - 1, 4, 1, UNMOUNT_TRANS);
        stamp(&mut vol, 0, 1, 5);
        assert!(clean(vol));
    }

    #[test]
    fn cycles_more_than_one_pass_apart_are_dirty() {
        let mut vol = volume();
        stamp(&mut vol, 0, LOG_BBS, 2);
        stamp(&mut vol, 0, 10, 7);
        assert!(!clean(vol));
    }

    #[test]
    fn missing_header_is_dirty() {
        let mut vol = volume();
        stamp(&mut vol, 0, 10, 1);
        assert!(!clean(vol));
    }

    #[test]
    fn log_past_device_end_is_an_io_error() {
        let mut vol = vec![0u8; LOG_BASE + BBSIZE];
        vol[LOG_BASE..LOG_BASE + 4].copy_from_slice(&1u32.to_be_bytes());
        assert!(is_clean(&mut Mem(vol), &sb()).is_err());
    }

    #[test]
    fn log_start_outside_the_ags_is_rejected() {
        let mut sb = sb();
        sb.logstart = u64::MAX;
        assert!(is_clean(&mut Mem(volume()), &sb).is_err());
    }
}
