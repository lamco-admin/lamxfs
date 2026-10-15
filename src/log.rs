//! The internal log as the volume left it: where its head and tail are, and the
//! records between them.
//!
//! Every 512-byte basic block of the log carries the cycle (pass over the ring)
//! it was written in, so the head is where the cycle number steps back. The last
//! record header before the head says where the tail is: the oldest record whose
//! changes may not have reached their home blocks. A log whose last record is a
//! lone unmount record ending at the head is clean and needs nothing replayed;
//! anything else is dirty, and [`crate::replay`] applies the records from the
//! tail to the head in memory.
//!
//! A crash can tear the last records Linux was writing. As Linux does, a CRC
//! mismatch or a missing header among the last eight records before the head
//! cuts the log there, and the same fault further back refuses the volume.

use alloc::{vec, vec::Vec};

use crate::{
    be,
    block_read::BlockRead,
    crc,
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
const MAX_RECORD_DATA: u32 = 256 * 1024;
/// The most live log replay takes on. Every record between the tail and the
/// head is held in memory, so a larger span refuses the volume rather than
/// exhausting a bootloader's memory.
const MAX_LIVE_LOG_BYTES: u64 = 128 * 1024 * 1024;
/// How many records before the head Linux checks for a torn write.
const TORN_WINDOW: usize = 8;
/// Bytes of the record header the CRC covers (the rest of its block is zero).
const HEADER_CRC_BYTES: usize = 328;
/// Bytes of an extension header block the CRC covers: its cycle and the 64
/// cycle words it holds.
const EXT_HEADER_CRC_BYTES: usize = 260;
/// Body blocks whose cycle words one header block holds.
const CYCLE_WORDS_PER_HEADER: u64 = 64;
/// `h_fmt` of a log written by a little-endian host, whose host-order items
/// replay reads as little-endian.
pub(crate) const FMT_LINUX_LE: u32 = 1;

// xlog_rec_header field offsets.
mod rec {
    pub(super) const MAGICNO: usize = 0;
    pub(super) const CYCLE: usize = 4;
    pub(super) const VERSION: usize = 8;
    pub(super) const LEN: usize = 12;
    pub(super) const LSN: usize = 16;
    pub(super) const TAIL_LSN: usize = 24;
    pub(super) const CRC: usize = 32;
    pub(super) const NUM_LOGOPS: usize = 40;
    pub(super) const CYCLE_DATA: usize = 44;
    pub(super) const FMT: usize = 300;
    pub(super) const FS_UUID: usize = 304;
    pub(super) const SIZE: usize = 320;
}

// xlog_op_header field offset.
const OP_FLAGS: usize = 9;

/// The internal log as a ring of basic blocks.
pub(crate) struct Log {
    base: u64,
    pub(crate) bbnum: u64,
}

impl Log {
    pub(crate) fn new(sb: &Superblock) -> Result<Self> {
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

    /// Blocks from `from` forward to `to` around the ring.
    fn distance(&self, from: u64, to: u64) -> u64 {
        (to + self.bbnum - from) % self.bbnum
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

    /// The record whose header is at `blk`, with the cycle-stamped first word
    /// of each body block restored, or `None` when `blk` holds no header.
    pub(crate) fn record<R: BlockRead>(
        &self,
        reader: &mut R,
        blk: u64,
        sb: &Superblock,
    ) -> Result<Option<Record>> {
        let (uuid, v5) = (&sb.uuid, sb.v5);
        let header = self.read_bb(reader, blk)?;
        let field = |off| be::u32_at(&header, off).unwrap_or(0);
        if field(rec::MAGICNO) != HEADER_MAGIC {
            return Ok(None);
        }
        let corrupt = Error::BadSuperblock(SuperblockReason::DirtyLog);
        let (version, len, size) = (field(rec::VERSION), field(rec::LEN), field(rec::SIZE));
        if len > MAX_RECORD_DATA
            || size > MAX_RECORD_DATA
            || header.get(rec::FS_UUID..rec::FS_UUID + 16) != Some(uuid)
        {
            return Err(corrupt);
        }
        let hblks = header_blocks(version, size);
        let len_bbs = u64::from(len).div_ceil(BBSIZE as u64);
        if hblks + len_bbs >= self.bbnum {
            return Err(corrupt);
        }

        let mut digest = crc::digest();
        digest.update(&header[..rec::CRC]);
        digest.update(&[0u8; 4]);
        digest.update(&header[rec::CRC + 4..HEADER_CRC_BYTES]);
        // The CRC takes in only the extension headers the body needs, and only
        // their cycle words.
        let ext_covered = len_bbs.div_ceil(CYCLE_WORDS_PER_HEADER).saturating_sub(1);
        let mut cycle_words = Vec::with_capacity(usize::try_from(len_bbs).unwrap_or(0));
        for i in 0..64 {
            cycle_words.push(field(rec::CYCLE_DATA + 4 * i));
        }
        for j in 1..hblks {
            let ext = self.read_bb(reader, self.wrap(blk + j))?;
            if j <= ext_covered {
                digest.update(&ext[..EXT_HEADER_CRC_BYTES]);
            }
            for i in 0..64 {
                cycle_words.push(be::u32_at(&ext, 4 + 4 * i).unwrap_or(0));
            }
        }
        let mut body = vec![0u8; usize::try_from(len_bbs).unwrap_or(0) * BBSIZE];
        for (i, chunk) in body.chunks_exact_mut(BBSIZE).enumerate() {
            chunk.copy_from_slice(&self.read_bb(reader, self.wrap(blk + hblks + i as u64))?);
        }
        let len = len as usize;
        digest.update(&body[..len]);
        let stored = u32::from_le_bytes(
            header[rec::CRC..rec::CRC + 4]
                .try_into()
                .map_err(|_| Error::BadSuperblock(SuperblockReason::DirtyLog))?,
        );
        let computed = digest.finalize();
        for (i, chunk) in body.chunks_exact_mut(BBSIZE).enumerate() {
            let word = cycle_words
                .get(i)
                .copied()
                .ok_or(Error::BadSuperblock(SuperblockReason::DirtyLog))?;
            chunk[..4].copy_from_slice(&word.to_be_bytes());
        }
        body.truncate(len);
        let mut record = Record {
            lsn: be::u64_at(&header, rec::LSN).unwrap_or(0),
            tail_lsn: be::u64_at(&header, rec::TAIL_LSN).unwrap_or(0),
            num_ops: field(rec::NUM_LOGOPS),
            fmt: field(rec::FMT),
            body,
            crc_ok: computed == stored,
            start: blk,
            next: self.wrap(blk + hblks + len_bbs),
        };
        // mkfs writes its unmount record without a CRC, and Linux checks
        // record CRCs only on v5 filesystems. A v4 log that needs replay is
        // refused anyway, so passing its CRCs only keeps it dirty.
        if (stored == 0 && record.is_unmount()) || !v5 {
            record.crc_ok = true;
        }
        Ok(Some(record))
    }
}

/// Header blocks of a record: one, or for a v2 log whose in-core buffers are
/// larger than 32 KiB, one per 32 KiB of record data.
fn header_blocks(version: u32, size: u32) -> u64 {
    if version & VERSION_2 != 0 && size > HEADER_CYCLE_SIZE {
        u64::from(size.div_ceil(HEADER_CYCLE_SIZE))
    } else {
        1
    }
}

enum Head {
    At(u64),
    Inconsistent,
}

/// One log record, its body restored to the bytes Linux wrote.
pub(crate) struct Record {
    pub(crate) lsn: u64,
    pub(crate) tail_lsn: u64,
    pub(crate) num_ops: u32,
    /// The byte order of the host that wrote the record's items.
    pub(crate) fmt: u32,
    pub(crate) body: Vec<u8>,
    crc_ok: bool,
    /// Where the header is, and the block after the record.
    start: u64,
    next: u64,
}

impl Record {
    /// A lone unmount operation: what a clean unmount writes last.
    pub(crate) fn is_unmount(&self) -> bool {
        self.num_ops == 1
            && self
                .body
                .get(OP_FLAGS)
                .is_some_and(|f| f & UNMOUNT_TRANS != 0)
    }
}

/// What the log needs before the volume can be read.
pub(crate) enum LogState {
    /// Nothing to replay.
    Clean,
    /// Replay these records, tail first.
    Dirty(Vec<Record>),
}

/// Find the head and tail and read the records between them.
pub(crate) fn state<R: BlockRead>(reader: &mut R, sb: &Superblock) -> Result<LogState> {
    let log = Log::new(sb)?;
    let mut head = match log.head(reader)? {
        None => return Ok(LogState::Clean),
        Some(Head::Inconsistent) => return Err(refuse()),
        Some(Head::At(head)) => head,
    };
    // A torn write moves the head back to the bad record, and the tail then
    // comes from the last good record before it, so each cut starts over.
    for _ in 0..=TORN_WINDOW {
        let last = log.last_before(reader, head, sb)?;
        if last.next == head && last.is_unmount() && last.crc_ok {
            return Ok(LogState::Clean);
        }
        match log.read_range(reader, sb, last.tail_lsn, head)? {
            Ok(records) => return Ok(LogState::Dirty(records)),
            Err(new_head) if new_head != head => head = new_head,
            Err(_) => return Err(refuse()),
        }
    }
    Err(refuse())
}

fn refuse() -> Error {
    Error::BadSuperblock(SuperblockReason::DirtyLog)
}

impl Log {
    /// The last record whose header lies before `head`.
    fn last_before<R: BlockRead>(
        &self,
        reader: &mut R,
        head: u64,
        sb: &Superblock,
    ) -> Result<Record> {
        for back in 1..=MAX_RECORD_BBS.min(self.bbnum) {
            let blk = self.wrap(head + self.bbnum - back);
            if let Some(record) = self.record(reader, blk, sb)? {
                return Ok(record);
            }
        }
        Err(refuse())
    }

    /// The records from the tail to `head`, or the block a torn write near the
    /// head says the log really ends at: a missing header, a record running
    /// past the head, or a bad CRC among the last records. The same fault
    /// further back refuses the volume.
    fn read_range<R: BlockRead>(
        &self,
        reader: &mut R,
        sb: &Superblock,
        tail_lsn: u64,
        head: u64,
    ) -> Result<core::result::Result<Vec<Record>, u64>> {
        let tail = self.wrap(tail_lsn & 0xFFFF_FFFF);
        let span = self.distance(tail, head);
        if span << BBSHIFT > MAX_LIVE_LOG_BYTES {
            return Err(refuse());
        }
        let mut records: Vec<Record> = Vec::new();
        let mut blk = tail;
        while blk != head {
            let record = self.record(reader, blk, sb)?.filter(|r| {
                let end = self.distance(tail, r.next);
                end != 0 && end <= span
            });
            let Some(record) = record else {
                if self.distance(blk, head) <= TORN_WINDOW as u64 * MAX_RECORD_BBS {
                    return Ok(Err(blk));
                }
                return Err(refuse());
            };
            blk = record.next;
            records.push(record);
            if records.len() as u64 > self.bbnum {
                return Err(refuse());
            }
        }
        match records.iter().position(|r| !r.crc_ok) {
            None => Ok(Ok(records)),
            Some(bad) if records.len() - bad <= TORN_WINDOW => Ok(Err(records[bad].start)),
            Some(_) => Err(refuse()),
        }
    }
}

/// Whether the internal log needs no replay.
#[cfg(test)]
pub(crate) fn is_clean<R: BlockRead>(reader: &mut R, sb: &Superblock) -> Result<bool> {
    match state(reader, sb) {
        Ok(LogState::Clean) => Ok(true),
        Ok(LogState::Dirty(_)) | Err(Error::BadSuperblock(SuperblockReason::DirtyLog)) => Ok(false),
        Err(e) => Err(e),
    }
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
    /// which carries `flags`, naming itself as the tail.
    fn record(vol: &mut [u8], blk: u64, cycle: u32, ops: u32, flags: u8) {
        record_tail(vol, blk, cycle, ops, flags, blk);
    }

    /// The same, naming the record at `tail` as the tail.
    fn record_tail(vol: &mut [u8], blk: u64, cycle: u32, ops: u32, flags: u8, tail: u64) {
        let h = bb(vol, blk);
        h[0..4].copy_from_slice(&HEADER_MAGIC.to_be_bytes());
        h[4..8].copy_from_slice(&cycle.to_be_bytes());
        h[8..12].copy_from_slice(&VERSION_2.to_be_bytes());
        h[12..16].copy_from_slice(&(BBSIZE as u32).to_be_bytes());
        h[40..44].copy_from_slice(&ops.to_be_bytes());
        h[320..324].copy_from_slice(&HEADER_CYCLE_SIZE.to_be_bytes());
        let lsn = u64::from(cycle) << 32 | blk;
        h[16..24].copy_from_slice(&lsn.to_be_bytes());
        h[24..32].copy_from_slice(&(u64::from(cycle) << 32 | tail).to_be_bytes());
        let d = bb(vol, (blk + 1) % LOG_BBS);
        d[..4].copy_from_slice(&cycle.to_be_bytes());
        d[OP_FLAGS] = flags;
        seal(vol, blk);
    }

    /// Set the CRC of the one-block record at `blk` over its blocks as they
    /// now stand, which is what Linux checks.
    fn seal(vol: &mut [u8], blk: u64) {
        let body: Vec<u8> = bb(vol, (blk + 1) % LOG_BBS).to_vec();
        let h = bb(vol, blk);
        h[rec::CRC..rec::CRC + 4].fill(0);
        let mut digest = crc::digest();
        digest.update(&h[..HEADER_CRC_BYTES]);
        digest.update(&body);
        let sum = digest.finalize();
        h[rec::CRC..rec::CRC + 4].copy_from_slice(&sum.to_le_bytes());
    }

    fn clean(vol: Vec<u8>) -> bool {
        is_clean(&mut Mem(vol), &sb()).unwrap()
    }

    /// The records left to replay, or `None` for a clean log.
    fn live(vol: Vec<u8>) -> Result<Option<usize>> {
        match state(&mut Mem(vol), &sb())? {
            LogState::Clean => Ok(None),
            LogState::Dirty(records) => Ok(Some(records.len())),
        }
    }

    /// Spoil the CRC of the record at `blk` by changing a body byte.
    fn tear(vol: &mut [u8], blk: u64) {
        bb(vol, (blk + 1) % LOG_BBS)[100] ^= 0xFF;
    }

    #[test]
    fn records_from_tail_to_head_are_live() {
        let mut vol = volume();
        for blk in [0, 2, 4] {
            record_tail(&mut vol, blk, 1, 2, 0x01, 0);
        }
        assert_eq!(live(vol).unwrap(), Some(3));
    }

    #[test]
    fn torn_last_record_is_cut() {
        // As Linux does: the bad record becomes the head, and the tail comes
        // from the record before it.
        let mut vol = volume();
        for blk in [0, 2, 4] {
            record_tail(&mut vol, blk, 1, 2, 0x01, 0);
        }
        tear(&mut vol, 4);
        assert_eq!(live(vol).unwrap(), Some(2));
    }

    #[test]
    fn earliest_bad_record_in_the_window_wins() {
        let mut vol = volume();
        for blk in (0..12).step_by(2) {
            record_tail(&mut vol, blk, 1, 2, 0x01, 0);
        }
        tear(&mut vol, 6);
        tear(&mut vol, 10);
        assert_eq!(live(vol).unwrap(), Some(3));
    }

    #[test]
    fn bad_record_before_the_window_refuses() {
        // Nine records after the bad one: too far back for a torn write.
        let mut vol = volume();
        for blk in (0..20).step_by(2) {
            record_tail(&mut vol, blk, 1, 2, 0x01, 0);
        }
        tear(&mut vol, 0);
        assert!(matches!(
            live(vol),
            Err(Error::BadSuperblock(SuperblockReason::DirtyLog))
        ));
    }

    #[test]
    fn torn_unmount_record_leaves_the_records_before_it() {
        let mut vol = volume();
        record_tail(&mut vol, 0, 1, 2, 0x01, 0);
        record_tail(&mut vol, 2, 1, 1, UNMOUNT_TRANS, 0);
        tear(&mut vol, 2);
        assert_eq!(live(vol).unwrap(), Some(1));
    }

    #[test]
    fn foreign_uuid_refuses() {
        let mut vol = volume();
        record(&mut vol, 0, 1, 2, 0x01);
        bb(&mut vol, 0)[rec::FS_UUID] = 1;
        assert!(live(vol).is_err());
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
        seal(&mut vol, LOG_BBS - 1);
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
