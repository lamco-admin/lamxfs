//! Log recovery in memory: replay the committed transactions a dirty log still
//! holds onto an overlay of the metadata blocks they change, so reads see what
//! Linux shows after mounting the volume, while nothing is written to it.
//!
//! The rules follow the public on-disk format (the XFS Algorithms & Data
//! Structures book, "Journaling Log") as checked against Linux recovery on
//! crash images:
//! - Operations are grouped into transactions by id; a transaction applies at
//!   its commit, in commit order, and one started before the tail is skipped.
//! - A first pass counts the cancelled buffers (blocks freed and possibly
//!   reused). A change to a block whose cancel is still ahead is skipped.
//! - A v5 block or inode whose stored LSN is at or past the transaction's is
//!   already newer than the change and is left alone. Replayed v5 blocks get
//!   the transaction's LSN and a new CRC, as Linux writes them.
//! - Buffer items carry 128-byte chunks; inode items carry the inode core in
//!   the host's byte order plus the fork bytes; inode-create items stand for
//!   freshly initialised inodes.
//! - Intent and done items are passed over: Linux finishes an intent the crash
//!   interrupted after replay. Unfinished extent-free, rmap, refcount and
//!   attribute intents change nothing a read here sees, so the replayed view
//!   reads the same. An unfinished bmap or exchange-range intent would leave a
//!   file's mapping half-changed, so it refuses the volume.
//!
//! Only v5 filesystems are replayed. A v4 log that needs replay, an unknown
//! item, or a log that doesn't parse still refuses the volume with
//! `sb_dirty_log`.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    vec,
    vec::Vec,
};

use crate::{
    be,
    block_read::BlockRead,
    crc,
    error::{Error, Result, SuperblockReason},
    log::{self, LogState, Record},
    superblock::Superblock,
};

const BBSIZE: usize = 512;
/// The overlay's ceiling. A `/boot` log replays a handful of blocks; a busy
/// root filesystem's log could ask for far more than a bootloader should hold.
const MAX_OVERLAY_BYTES: usize = 128 * 1024 * 1024;
/// The largest block range one item may name. The largest real ones are an
/// inode chunk (64 inodes of 2 KiB) and a 64 KiB directory or attribute
/// block, far below this.
const MAX_ITEM_BBS: u32 = 2048;
/// `sb_features_log_incompat` bit for logged extended attribute updates. Its
/// intent items only change attributes, which nothing here reads.
const LOG_INCOMPAT_XATTRS: u32 = 0x1;

const XFS_TRANSACTION: u8 = 0x69;
const XFS_LOG: u8 = 0xAA;
const OP_START: u8 = 0x01;
const OP_COMMIT: u8 = 0x02;
const OP_CONTINUE: u8 = 0x04;
const OP_WAS_CONT: u8 = 0x08;
const OP_UNMOUNT: u8 = 0x20;
const OP_HEADER: usize = 12;
/// `th_magic` of the transaction header region, stored in the host's order.
const TRANS_MAGIC: u32 = 0x5452_414E;

const LI_INODE: u16 = 0x123b;
const LI_BUF: u16 = 0x123c;
const LI_ICREATE: u16 = 0x123f;
/// Intents whose unfinished work changes a file's mapping: bmap updates and
/// file range exchanges, each with its done item. Linux finishes them after
/// replay, so a volume left with one unfinished reads differently until it
/// does, and is refused.
const LI_MAPPING_INTENTS: [(u16, u16); 2] = [(0x1244, 0x1245), (0x1248, 0x1249)];
/// Items that change nothing a read sees: EFI/EFD, IUNLINK, dquot, quotaoff,
/// RUI/RUD, CUI/CUD, BUI/BUD, ATTRI/ATTRD, XMI/XMD.
const LI_PASSED_OVER: [u16; 15] = [
    0x1236, 0x1237, 0x1238, 0x123d, 0x123e, 0x1240, 0x1241, 0x1242, 0x1243, 0x1244, 0x1245, 0x1246,
    0x1247, 0x1248, 0x1249,
];

const BLF_INODE_BUF: u16 = 0x1;
const BLF_CANCEL: u16 = 0x2;
/// Bytes per bit of a buffer item's dirty map.
const BLF_CHUNK: usize = 128;

const ILOG_CORE: u32 = 0x1;
const ILOG_DDATA: u32 = 0x2;
const ILOG_DEXT: u32 = 0x4;
const ILOG_DBROOT: u32 = 0x8;
const ILOG_DEV: u32 = 0x10;
const ILOG_ADATA: u32 = 0x40;
const ILOG_AEXT: u32 = 0x80;
const ILOG_ABROOT: u32 = 0x100;
const ILOG_DFORK: u32 = ILOG_DDATA | ILOG_DEXT | ILOG_DBROOT;
const ILOG_AFORK: u32 = ILOG_ADATA | ILOG_AEXT | ILOG_ABROOT;

const DINODE_MAGIC: u16 = 0x494e;
/// v3 inode core length, which is also where the forks start.
const CORE_V3: usize = 176;
const DI_FORKOFF: usize = 82;
const DI_GEN: usize = 92;
const DI_NEXT_UNLINKED: usize = 96;
const DI_CRC: usize = 100;
const DI_LSN: usize = 112;
const DI_INO: usize = 152;
const DI_UUID: usize = 160;
/// Header of the long-format v5 bmbt block a logged btree root is.
const BMBT_V5_HEADER: usize = 72;

fn refuse() -> Error {
    Error::BadSuperblock(SuperblockReason::DirtyLog)
}

/// Replayed sectors, by 512-byte sector index.
#[derive(Default)]
pub(crate) struct Overlay {
    sectors: BTreeMap<u64, Box<[u8; BBSIZE]>>,
}

impl Overlay {
    fn patch(&self, offset: u64, buf: &mut [u8]) {
        if buf.is_empty() || self.sectors.is_empty() {
            return;
        }
        let Some(end) = offset.checked_add(buf.len() as u64) else {
            return;
        };
        let first = offset / BBSIZE as u64;
        let last = (end - 1) / BBSIZE as u64;
        for (&sector, data) in self.sectors.range(first..=last) {
            let s_start = sector * BBSIZE as u64;
            let from = s_start.max(offset);
            let to = (s_start + BBSIZE as u64).min(end);
            buf[(from - offset) as usize..(to - offset) as usize]
                .copy_from_slice(&data[(from - s_start) as usize..(to - s_start) as usize]);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sectors.is_empty()
    }
}

/// A reader that serves replayed sectors from the overlay and everything else
/// from the volume.
pub(crate) struct Replayed<R> {
    pub(crate) inner: R,
    pub(crate) overlay: Overlay,
}

impl<R: BlockRead> BlockRead for Replayed<R> {
    type Error = R::Error;

    fn read_at(&mut self, offset_bytes: u64, buf: &mut [u8]) -> core::result::Result<(), R::Error> {
        self.inner.read_at(offset_bytes, buf)?;
        self.overlay.patch(offset_bytes, buf);
        Ok(())
    }
}

/// Replay the log, or return an empty overlay when it's clean.
pub(crate) fn recover<R: BlockRead>(reader: &mut R, sb: &Superblock) -> Result<Overlay> {
    let records = match log::state(reader, sb)? {
        LogState::Clean => return Ok(Overlay::default()),
        LogState::Dirty(records) => records,
    };
    if !sb.v5 || sb.log_incompat & !LOG_INCOMPAT_XATTRS != 0 {
        return Err(refuse());
    }
    if records
        .iter()
        .any(|r| r.fmt != log::FMT_LINUX_LE && !r.is_unmount())
    {
        return Err(refuse());
    }
    let transactions = assemble(&records)?;
    let mut items = Vec::with_capacity(transactions.len());
    for t in &transactions {
        items.push(parse_items(t)?);
    }
    if unfinished_mapping_intent(&items) {
        return Err(refuse());
    }
    let mut replay = Replay::new(reader, sb);
    replay.count_cancels(&items);
    for (t, its) in transactions.iter().zip(&items) {
        // Cancels apply after the rest of their transaction, as Linux orders a
        // transaction's buffers.
        for item in its.iter().filter(|i| !i.is_cancel()) {
            replay.apply(item, t.start_lsn)?;
        }
        for item in its.iter().filter(|i| i.is_cancel()) {
            replay.apply(item, t.start_lsn)?;
        }
    }
    Ok(replay.overlay)
}

/// Whether a bmap or exchange intent has no done item after it. Both carry
/// the intent's id at offset 8.
fn unfinished_mapping_intent(items: &[Vec<Item<'_>>]) -> bool {
    let mut open = BTreeSet::new();
    for item in items.iter().flatten() {
        let Some(id) = le64(item.format(), 8) else {
            continue;
        };
        for (intent, done) in LI_MAPPING_INTENTS {
            if item.magic == intent {
                open.insert((intent, id));
            } else if item.magic == done {
                open.remove(&(intent, id));
            }
        }
    }
    !open.is_empty()
}

/// Replay a fuzzed log on a synthetic volume: the input is the filesystem
/// UUID and then the log's first bytes. Every block outside the log reads as
/// a bare v3 inode, so inode items get past their magic check.
#[cfg(fuzzing)]
pub(crate) fn fuzz(data: &[u8]) {
    struct Volume<'a> {
        log: &'a [u8],
        log_base: u64,
        log_len: u64,
    }

    impl BlockRead for Volume<'_> {
        type Error = ();

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> core::result::Result<(), ()> {
            for (i, byte) in buf.iter_mut().enumerate() {
                let at = offset + i as u64;
                let rel = at.wrapping_sub(self.log_base);
                *byte = if at >= self.log_base && rel < self.log_len {
                    usize::try_from(rel)
                        .ok()
                        .and_then(|r| self.log.get(r))
                        .copied()
                        .unwrap_or(0)
                } else {
                    bare_inode_byte(at)
                };
            }
            Ok(())
        }
    }

    // Magic "IN" and version 3 at the start of every 512 bytes.
    fn bare_inode_byte(at: u64) -> u8 {
        match at % BBSIZE as u64 {
            0 => 0x49,
            1 => 0x4e,
            4 => 3,
            _ => 0,
        }
    }

    let Some((uuid, log)) = data.split_first_chunk::<16>() else {
        return;
    };
    let sb = Superblock::for_fuzzing(*uuid);
    let mut volume = Volume {
        log,
        log_base: sb.fsblock_byte_offset(sb.logstart),
        log_len: u64::from(sb.logblocks) * u64::from(sb.blocksize),
    };
    let _ = recover(&mut volume, &sb);
}

/// One committed transaction's regions (operation payloads), in log order.
struct Transaction {
    start_lsn: u64,
    regions: Vec<Vec<u8>>,
}

/// Group operations into transactions by id; keep those whose start and
/// commit both lie between the tail and the head, in commit order.
fn assemble(records: &[Record]) -> Result<Vec<Transaction>> {
    let mut open: BTreeMap<u32, Transaction> = BTreeMap::new();
    let mut partial: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut done = Vec::new();
    for record in records {
        let body = &record.body;
        let mut at = 0usize;
        for _ in 0..record.num_ops {
            let header = body.get(at..at + OP_HEADER).ok_or_else(refuse)?;
            let tid = be::u32_at(header, 0).ok_or_else(refuse)?;
            let len =
                usize::try_from(be::u32_at(header, 4).ok_or_else(refuse)?).map_err(|_| refuse())?;
            let (client, flags) = (header[8], header[9]);
            let end = (at + OP_HEADER).checked_add(len).ok_or_else(refuse)?;
            let payload = body.get(at + OP_HEADER..end).ok_or_else(refuse)?;
            at = end;
            if client == XFS_LOG || flags & OP_UNMOUNT != 0 {
                continue;
            }
            if client != XFS_TRANSACTION {
                return Err(refuse());
            }
            if flags & OP_START != 0 {
                open.insert(
                    tid,
                    Transaction {
                        start_lsn: record.lsn,
                        regions: Vec::new(),
                    },
                );
                continue;
            }
            if flags & OP_COMMIT != 0 {
                partial.remove(&tid);
                done.extend(open.remove(&tid));
                continue;
            }
            let Some(t) = open.get_mut(&tid) else {
                // Started before the tail: its earlier operations are gone.
                continue;
            };
            if let Some(region) = join_region(&mut partial, tid, flags, payload) {
                t.regions.push(region);
            }
        }
    }
    Ok(done)
}

/// Add an operation's payload to its transaction's region. A region too big
/// for one record continues in the next; it comes back once its last piece
/// has arrived.
fn join_region(
    partial: &mut BTreeMap<u32, Vec<u8>>,
    tid: u32,
    flags: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let mut region = if flags & OP_WAS_CONT != 0 {
        partial.remove(&tid).unwrap_or_default()
    } else {
        Vec::new()
    };
    region.extend_from_slice(payload);
    if flags & OP_CONTINUE != 0 {
        partial.insert(tid, region);
        return None;
    }
    Some(region)
}

/// A log item: its magic and its regions, the format region first.
struct Item<'a> {
    magic: u16,
    regions: &'a [Vec<u8>],
}

impl Item<'_> {
    fn format(&self) -> &[u8] {
        &self.regions[0]
    }

    fn is_cancel(&self) -> bool {
        self.magic == LI_BUF && le16(self.format(), 4).is_some_and(|f| f & BLF_CANCEL != 0)
    }
}

/// Split a transaction's regions after its header into items. Each item's
/// first region starts with its magic and its region count.
fn parse_items(t: &Transaction) -> Result<Vec<Item<'_>>> {
    let header = t.regions.first().ok_or_else(refuse)?;
    if le32(header, 0) != Some(TRANS_MAGIC) {
        return Err(refuse());
    }
    let mut items = Vec::new();
    let mut i = 1;
    while i < t.regions.len() {
        let first = &t.regions[i];
        let magic = le16(first, 0).ok_or_else(refuse)?;
        let size = usize::from(le16(first, 2).ok_or_else(refuse)?);
        if size == 0 || i + size > t.regions.len() {
            return Err(refuse());
        }
        if !matches!(magic, LI_INODE | LI_BUF | LI_ICREATE) && !LI_PASSED_OVER.contains(&magic) {
            return Err(refuse());
        }
        items.push(Item {
            magic,
            regions: &t.regions[i..i + size],
        });
        i += size;
    }
    Ok(items)
}

struct Replay<'r, R> {
    reader: &'r mut R,
    sb: &'r Superblock,
    overlay: Overlay,
    /// Live cancels by (block, length), each with the cancels still ahead.
    cancels: BTreeMap<(i64, u32), u32>,
    /// The volume in 512-byte sectors, which every replayed block must fit in.
    sectors: u64,
}

impl<'r, R: BlockRead> Replay<'r, R> {
    fn new(reader: &'r mut R, sb: &'r Superblock) -> Self {
        let sectors = sb.dblocks.saturating_mul(u64::from(sb.blocksize)) / BBSIZE as u64;
        Replay {
            reader,
            sb,
            overlay: Overlay::default(),
            cancels: BTreeMap::new(),
            sectors,
        }
    }

    fn count_cancels(&mut self, items: &[Vec<Item<'_>>]) {
        for item in items.iter().flatten().filter(|i| i.is_cancel()) {
            if let Some(key) = buf_key(item.format()) {
                *self.cancels.entry(key).or_insert(0) += 1;
            }
        }
    }

    fn apply(&mut self, item: &Item<'_>, lsn: u64) -> Result<()> {
        match item.magic {
            LI_BUF => self.apply_buf(item, lsn),
            LI_INODE => self.apply_inode(item, lsn),
            LI_ICREATE => self.apply_icreate(item),
            _ => Ok(()),
        }
    }

    /// Read `bbs` sectors from `daddr` as replay has left them so far.
    fn load(&mut self, daddr: i64, bbs: u32) -> Result<Vec<u8>> {
        let start = self.span(daddr, bbs)?;
        let mut buf = vec![0u8; bbs as usize * BBSIZE];
        let offset = start * BBSIZE as u64;
        self.reader
            .read_at(offset, &mut buf)
            .map_err(|_| Error::Io {
                token: "io_log_replay",
                offset,
            })?;
        self.overlay.patch(offset, &mut buf);
        Ok(buf)
    }

    /// Put the sectors of `buf` (loaded from `daddr`) that `range` touches
    /// into the overlay.
    fn store(&mut self, daddr: i64, buf: &[u8], range: core::ops::Range<usize>) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        let start = self.span(daddr, (buf.len() / BBSIZE) as u32)?;
        for s in range.start / BBSIZE..=(range.end - 1) / BBSIZE {
            let src = buf.get(s * BBSIZE..(s + 1) * BBSIZE).ok_or_else(refuse)?;
            let mut sector = Box::new([0u8; BBSIZE]);
            sector.copy_from_slice(src);
            self.overlay.sectors.insert(start + s as u64, sector);
        }
        if self.overlay.sectors.len() * BBSIZE > MAX_OVERLAY_BYTES {
            return Err(refuse());
        }
        Ok(())
    }

    /// First sector of a replayed range, refusing one outside the volume.
    fn span(&self, daddr: i64, bbs: u32) -> Result<u64> {
        let start = u64::try_from(daddr).map_err(|_| refuse())?;
        let end = start.checked_add(u64::from(bbs)).ok_or_else(refuse)?;
        if bbs == 0 || bbs > MAX_ITEM_BBS || end > self.sectors {
            return Err(refuse());
        }
        Ok(start)
    }

    fn apply_buf(&mut self, item: &Item<'_>, lsn: u64) -> Result<()> {
        let format = item.format();
        let flags = le16(format, 4).ok_or_else(refuse)?;
        let key = buf_key(format).ok_or_else(refuse)?;
        if flags & BLF_CANCEL != 0 {
            if let Some(count) = self.cancels.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    self.cancels.remove(&key);
                }
            }
            return Ok(());
        }
        if self.cancels.contains_key(&key) {
            return Ok(());
        }
        let map_size = le32(format, 16).ok_or_else(refuse)? as usize;
        let map_end = map_size
            .checked_mul(4)
            .and_then(|n| n.checked_add(20))
            .ok_or_else(refuse)?;
        let map = format.get(20..map_end).ok_or_else(refuse)?;
        let regions = place_regions(map, &item.regions[1..])?;

        let mut buf = self.load(key.0, key.1)?;
        if flags & BLF_INODE_BUF != 0 {
            return self.apply_inode_buf(key.0, buf, &regions);
        }
        // The block as it stands says whether it already holds this change.
        if let Some((lsn_off, _)) = lsn_crc_offsets(&buf) {
            if newer_or_same(be::u64_at(&buf, lsn_off), lsn) {
                return Ok(());
            }
        }
        for &(at, region) in &regions {
            let end = at.checked_add(region.len()).ok_or_else(refuse)?;
            buf.get_mut(at..end)
                .ok_or_else(refuse)?
                .copy_from_slice(region);
        }
        // The block as replayed says what it is now: a freshly allocated block
        // gets its type from the logged chunks.
        if let Some((lsn_off, crc_off)) = lsn_crc_offsets(&buf) {
            buf[lsn_off..lsn_off + 8].copy_from_slice(&lsn.to_be_bytes());
            let sum = crc::checksum(&buf, crc_off);
            buf[crc_off..crc_off + 4].copy_from_slice(&sum.to_le_bytes());
        }
        let len = buf.len();
        self.store(key.0, &buf, 0..len)
    }

    /// An inode cluster buffer is logged only for unlinked-list changes, so
    /// replay takes just the `di_next_unlinked` words its chunks carry; the
    /// rest of each inode comes from inode items.
    fn apply_inode_buf(
        &mut self,
        daddr: i64,
        mut buf: Vec<u8>,
        regions: &[(usize, &[u8])],
    ) -> Result<()> {
        let isize = usize::from(self.sb.inodesize);
        for &(start, region) in regions {
            let end = start + region.len();
            for slot in start / isize..=end.saturating_sub(1) / isize {
                let word = slot * isize + DI_NEXT_UNLINKED;
                if word < start || word + 4 > end || (slot + 1) * isize > buf.len() {
                    continue;
                }
                buf[word..word + 4].copy_from_slice(&region[word - start..word - start + 4]);
                let inode = &mut buf[slot * isize..(slot + 1) * isize];
                let sum = crc::checksum(inode, DI_CRC);
                inode[DI_CRC..DI_CRC + 4].copy_from_slice(&sum.to_le_bytes());
            }
        }
        let len = buf.len();
        self.store(daddr, &buf, 0..len)
    }

    fn apply_inode(&mut self, item: &Item<'_>, lsn: u64) -> Result<()> {
        let format = item.format();
        let fields = le32(format, 4).ok_or_else(refuse)?;
        let blkno = le64(format, 40).ok_or_else(refuse)? as i64;
        let len = le32(format, 48).ok_or_else(refuse)?;
        let boff = le32(format, 52).ok_or_else(refuse)? as usize;
        if fields & ILOG_CORE == 0 {
            return Err(refuse());
        }
        if self.cancels.contains_key(&(blkno, len)) {
            return Ok(());
        }
        let regions = &item.regions[1..];
        let wanted =
            1 + usize::from(fields & ILOG_DFORK != 0) + usize::from(fields & ILOG_AFORK != 0);
        if regions.len() != wanted {
            return Err(refuse());
        }

        let isize = usize::from(self.sb.inodesize);
        let mut cluster = self.load(blkno, len)?;
        let end = boff.checked_add(isize).ok_or_else(refuse)?;
        let inode = cluster.get_mut(boff..end).ok_or_else(refuse)?;
        if be::u16_at(inode, 0) != Some(DINODE_MAGIC) {
            return Err(refuse());
        }
        if newer_or_same(be::u64_at(inode, DI_LSN), lsn) {
            return Ok(());
        }

        let core = logged_core(&regions[0], self.sb.bigtime())?;
        if be::u16_at(&core, 0) != Some(DINODE_MAGIC) {
            return Err(refuse());
        }
        let mut next_unlinked = [0u8; 4];
        next_unlinked.copy_from_slice(&inode[DI_NEXT_UNLINKED..DI_NEXT_UNLINKED + 4]);
        inode[..CORE_V3].copy_from_slice(&core);
        inode[DI_NEXT_UNLINKED..DI_NEXT_UNLINKED + 4].copy_from_slice(&next_unlinked);

        let forkoff = usize::from(inode[DI_FORKOFF]) * 8;
        if CORE_V3 + forkoff >= isize {
            return Err(refuse());
        }
        let data_size = if forkoff == 0 {
            isize - CORE_V3
        } else {
            forkoff
        };
        let mut next = 1;
        if fields & ILOG_DFORK != 0 {
            put_fork(
                &mut inode[CORE_V3..CORE_V3 + data_size],
                &regions[next],
                fields & ILOG_DBROOT != 0,
            )?;
            next += 1;
        } else if fields & ILOG_DEV != 0 {
            let rdev = le32(format, 24).ok_or_else(refuse)?;
            inode[CORE_V3..CORE_V3 + 4].copy_from_slice(&rdev.to_be_bytes());
        }
        if fields & ILOG_AFORK != 0 {
            if forkoff == 0 {
                return Err(refuse());
            }
            put_fork(
                &mut inode[CORE_V3 + forkoff..isize],
                &regions[next],
                fields & ILOG_ABROOT != 0,
            )?;
        }
        inode[DI_LSN..DI_LSN + 8].copy_from_slice(&lsn.to_be_bytes());
        let sum = crc::checksum(inode, DI_CRC);
        inode[DI_CRC..DI_CRC + 4].copy_from_slice(&sum.to_le_bytes());
        self.store(blkno, &cluster, boff..end)
    }

    /// Fresh inodes for a newly allocated chunk, as Linux initialises them.
    fn apply_icreate(&mut self, item: &Item<'_>) -> Result<()> {
        let f = item.format();
        let field = |off| be::u32_at(f, off).ok_or_else(refuse);
        let (ag, agbno, count, isize, length, gen) = (
            field(4)?,
            field(8)?,
            field(12)?,
            field(16)?,
            field(20)?,
            field(24)?,
        );
        let sb = self.sb;
        if isize != u32::from(sb.inodesize) || ag >= sb.agcount || agbno >= sb.agblocks {
            return Err(refuse());
        }
        let bb_per_block = sb.blocksize / BBSIZE as u32;
        let daddr =
            (u64::from(ag) * u64::from(sb.agblocks) + u64::from(agbno)) * u64::from(bb_per_block);
        let daddr = i64::try_from(daddr).map_err(|_| refuse())?;
        let bbs = length.checked_mul(bb_per_block).ok_or_else(refuse)?;
        if u64::from(count) * u64::from(isize) > u64::from(bbs) * BBSIZE as u64 {
            return Err(refuse());
        }
        // The chunk is no larger than MAX_ITEM_BBS once loaded, so the slot
        // arithmetic below stays small.
        let mut chunk = self.load(daddr, bbs)?;
        let isize = isize as usize;
        let count = count as usize;
        let ino_base = (u64::from(ag) << (u32::from(sb.agblklog) + u32::from(sb.inopblog)))
            | (u64::from(agbno) << sb.inopblog);
        let isize_bbs = (isize / BBSIZE).max(1) as i64;
        let mut touched: Option<(usize, usize)> = None;
        for n in 0..count {
            let slot_bb = daddr + (n * isize / BBSIZE) as i64;
            // A cluster freed later in the log may hold file data by now.
            if self
                .cancels
                .keys()
                .any(|&(b, l)| b <= slot_bb && slot_bb + isize_bbs <= b + i64::from(l))
            {
                continue;
            }
            let inode = &mut chunk[n * isize..(n + 1) * isize];
            inode.fill(0);
            inode[0..2].copy_from_slice(&DINODE_MAGIC.to_be_bytes());
            inode[4] = 3;
            inode[DI_GEN..DI_GEN + 4].copy_from_slice(&gen.to_be_bytes());
            inode[DI_NEXT_UNLINKED..DI_NEXT_UNLINKED + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            inode[DI_INO..DI_INO + 8].copy_from_slice(&(ino_base + n as u64).to_be_bytes());
            inode[DI_UUID..DI_UUID + 16].copy_from_slice(&sb.uuid);
            let sum = crc::checksum(inode, DI_CRC);
            inode[DI_CRC..DI_CRC + 4].copy_from_slice(&sum.to_le_bytes());
            touched = Some(touched.map_or((n, n), |(lo, _)| (lo, n)));
        }
        match touched {
            Some((lo, hi)) => self.store(daddr, &chunk, lo * isize..(hi + 1) * isize),
            None => Ok(()),
        }
    }
}

/// `(blf_blkno, blf_len)` of a buffer item's format region.
fn buf_key(format: &[u8]) -> Option<(i64, u32)> {
    Some((le64(format, 8)? as i64, u32::from(le16(format, 6)?)))
}

/// Where in the buffer each logged region goes. A region starts at the next
/// dirty chunk in the map and covers as many chunks as it is long: a run of
/// dirty chunks is usually one region, but one split across a page boundary
/// arrives as several.
fn place_regions<'a>(map: &[u8], data: &'a [Vec<u8>]) -> Result<Vec<(usize, &'a [u8])>> {
    let dirty = |chunk: usize| {
        map.get(chunk / 32 * 4..chunk / 32 * 4 + 4)
            .is_some_and(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) >> (chunk % 32) & 1 == 1)
    };
    let chunks = map.len() / 4 * 32;
    let mut placed = Vec::with_capacity(data.len());
    let mut chunk = 0;
    for region in data {
        while chunk < chunks && !dirty(chunk) {
            chunk += 1;
        }
        if chunk >= chunks || region.is_empty() {
            return Err(refuse());
        }
        let at = chunk.checked_mul(BLF_CHUNK).ok_or_else(refuse)?;
        placed.push((at, region.as_slice()));
        chunk += region.len().div_ceil(BLF_CHUNK);
    }
    Ok(placed)
}

/// Whether a stored LSN means the block already holds this change or a later
/// one. Zero and all-ones mean "never written through the log".
fn newer_or_same(stored: Option<u64>, lsn: u64) -> bool {
    matches!(stored, Some(s) if s != 0 && s != u64::MAX && s >= lsn)
}

/// Where a v5 metadata block keeps its LSN and its CRC, by its magic.
fn lsn_crc_offsets(block: &[u8]) -> Option<(usize, usize)> {
    let found = match be::u32_at(block, 0)? {
        // superblock, AGF, AGI, AGFL
        0x5846_5342 => (240, 224),
        0x5841_4746 => (208, 216),
        0x5841_4749 => (320, 312),
        0x5841_464C => (24, 32),
        // short-format btree blocks: bno, cnt, inobt, finobt, rmapbt, refcountbt
        0x4142_3342 | 0x4142_3343 | 0x4941_4233 | 0x4649_4233 | 0x524D_4233 | 0x5233_4643 => {
            (24, 52)
        }
        // long-format bmbt
        0x424D_4133 => (32, 64),
        // dir3 block, data, free
        0x5844_4233 | 0x5844_4433 | 0x5844_4633 => (16, 4),
        // remote symlink
        0x5853_4C4D => (48, 12),
        // realtime bitmap and summary
        0x424D_505A | 0x5355_4D59 => (24, 4),
        // dir3 leaf, leafn, da3 node, attr3 leaf: a 16-bit magic at offset 8
        _ => match be::u16_at(block, 8)? {
            0x3DF1 | 0x3DFF | 0x3EBE | 0x3BEE => (24, 12),
            _ => return None,
        },
    };
    (found.0 + 8 <= block.len() && found.1 + 4 <= block.len()).then_some(found)
}

/// The on-disk v3 inode core from the logged one, which has the same layout
/// in the host's byte order (little-endian on every UEFI target).
fn logged_core(logged: &[u8], bigtime: bool) -> Result<[u8; CORE_V3]> {
    // (offset, width) of every multi-byte field; single bytes, the padding at
    // 132 and the UUID at 160 copy as they are, and the timestamps follow.
    const FIELDS: [(usize, usize); 25] = [
        (0, 2),
        (2, 2),
        (6, 2),
        (8, 4),
        (12, 4),
        (16, 4),
        (20, 2),
        (22, 2),
        (24, 8),
        (56, 8),
        (64, 8),
        (72, 4),
        (76, 4),
        (80, 2),
        (84, 4),
        (88, 2),
        (90, 2),
        (92, 4),
        (96, 4),
        (100, 4),
        (104, 8),
        (112, 8),
        (120, 8),
        (128, 4),
        (152, 8),
    ];
    let mut core = [0u8; CORE_V3];
    core.copy_from_slice(logged.get(..CORE_V3).ok_or_else(refuse)?);
    for (off, width) in FIELDS {
        core[off..off + width].reverse();
    }
    // Timestamps: one 64-bit value each with bigtime, else seconds and
    // nanoseconds as two 32-bit values.
    for ts in [32, 40, 48, 144] {
        if bigtime {
            core[ts..ts + 8].reverse();
        } else {
            core[ts..ts + 4].reverse();
            core[ts + 4..ts + 8].reverse();
        }
    }
    Ok(core)
}

/// Write a logged fork into the inode's fork area. Extent and local forks are
/// stored as they are; a btree root arrives as a long-format bmbt block and is
/// rewritten into the inode's compact root, leaving other bytes in place.
fn put_fork(fork: &mut [u8], region: &[u8], btree_root: bool) -> Result<()> {
    if !btree_root {
        let n = region.len().min(fork.len());
        fork[..n].copy_from_slice(&region[..n]);
        return Ok(());
    }
    let level = be::u16_at(region, 4).ok_or_else(refuse)?;
    let numrecs = usize::from(be::u16_at(region, 6).ok_or_else(refuse)?);
    let maxrecs = fork.len().saturating_sub(4) / 16;
    if fork.len() < 4 || numrecs > maxrecs {
        return Err(refuse());
    }
    let keys = region
        .get(BMBT_V5_HEADER..BMBT_V5_HEADER + 8 * numrecs)
        .ok_or_else(refuse)?;
    let ptrs = region
        .get(BMBT_V5_HEADER + 8 * numrecs..BMBT_V5_HEADER + 16 * numrecs)
        .ok_or_else(refuse)?;
    fork[0..2].copy_from_slice(&level.to_be_bytes());
    fork[2..4].copy_from_slice(&(numrecs as u16).to_be_bytes());
    fork[4..4 + 8 * numrecs].copy_from_slice(keys);
    let ptr_at = 4 + 8 * maxrecs;
    fork[ptr_at..ptr_at + 8 * numrecs].copy_from_slice(ptrs);
    Ok(())
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    #[test]
    fn logged_core_comes_back_big_endian() {
        let mut logged = [0u8; CORE_V3];
        logged[0..2].copy_from_slice(&DINODE_MAGIC.to_le_bytes());
        logged[2..4].copy_from_slice(&0o100644u16.to_le_bytes());
        logged[4] = 3;
        logged[56..64].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        logged[32..40].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        logged[160] = 0xAB;
        let core = logged_core(&logged, true).unwrap();
        assert_eq!(be::u16_at(&core, 0), Some(DINODE_MAGIC));
        assert_eq!(be::u16_at(&core, 2), Some(0o100644));
        assert_eq!(core[4], 3);
        assert_eq!(be::u64_at(&core, 56), Some(0x0102_0304_0506_0708));
        // bigtime: one 64-bit value.
        assert_eq!(be::u64_at(&core, 32), Some(0x1122_3344_5566_7788));
        // The UUID copies as it is.
        assert_eq!(core[160], 0xAB);
    }

    #[test]
    fn logged_timestamps_without_bigtime_are_two_words() {
        let mut logged = [0u8; CORE_V3];
        logged[32..36].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        logged[36..40].copy_from_slice(&0x5566_7788u32.to_le_bytes());
        let core = logged_core(&logged, false).unwrap();
        assert_eq!(be::u32_at(&core, 32), Some(0x1122_3344));
        assert_eq!(be::u32_at(&core, 36), Some(0x5566_7788));
    }

    #[test]
    fn regions_follow_the_dirty_map() {
        // Chunks 0..2 and 5 dirty; the first run arrives split in two.
        let map = (0b10_0111u32).to_le_bytes();
        let data = vec![vec![1u8; 128], vec![2u8; 256], vec![3u8; 128]];
        let placed = place_regions(&map, &data).unwrap();
        let at: Vec<usize> = placed.iter().map(|p| p.0).collect();
        assert_eq!(at, [0, 128, 640]);
    }

    #[test]
    fn more_regions_than_dirty_chunks_refuses() {
        let map = 1u32.to_le_bytes();
        let data = vec![vec![1u8; 128], vec![2u8; 128]];
        assert!(place_regions(&map, &data).is_err());
    }

    #[test]
    fn logged_btree_root_becomes_the_inode_root() {
        // A v5 long-format block: level 1, two records.
        let mut region = vec![0u8; BMBT_V5_HEADER + 32];
        region[4..6].copy_from_slice(&1u16.to_be_bytes());
        region[6..8].copy_from_slice(&2u16.to_be_bytes());
        region[BMBT_V5_HEADER..BMBT_V5_HEADER + 16].fill(0x11);
        region[BMBT_V5_HEADER + 16..].fill(0x22);
        let mut fork = [0xEEu8; 68];
        put_fork(&mut fork, &region, true).unwrap();
        // maxrecs = (68 - 4) / 16 = 4: keys at 4, pointers at 4 + 8 * 4.
        assert_eq!(be::u16_at(&fork, 0), Some(1));
        assert_eq!(be::u16_at(&fork, 2), Some(2));
        assert!(fork[4..20].iter().all(|&b| b == 0x11));
        assert!(fork[20..36].iter().all(|&b| b == 0xEE));
        assert!(fork[36..52].iter().all(|&b| b == 0x22));
    }

    #[test]
    fn btree_root_too_big_for_the_fork_refuses() {
        let mut region = vec![0u8; BMBT_V5_HEADER + 80];
        region[6..8].copy_from_slice(&5u16.to_be_bytes());
        assert!(put_fork(&mut [0u8; 68], &region, true).is_err());
    }

    fn item(magic: u16, id: u64) -> Vec<u8> {
        let mut f = vec![0u8; 16];
        f[0..2].copy_from_slice(&magic.to_le_bytes());
        f[2..4].copy_from_slice(&1u16.to_le_bytes());
        f[8..16].copy_from_slice(&id.to_le_bytes());
        f
    }

    #[test]
    fn mapping_intent_without_done_is_unfinished() {
        let bui = [item(0x1244, 7)];
        let bud = [item(0x1245, 7)];
        let open = vec![vec![Item {
            magic: 0x1244,
            regions: &bui,
        }]];
        assert!(unfinished_mapping_intent(&open));
        let done = vec![
            vec![Item {
                magic: 0x1244,
                regions: &bui,
            }],
            vec![Item {
                magic: 0x1245,
                regions: &bud,
            }],
        ];
        assert!(!unfinished_mapping_intent(&done));
    }

    #[test]
    fn free_intents_left_open_do_not_matter() {
        let efi = [item(0x1236, 9)];
        let open = vec![vec![Item {
            magic: 0x1236,
            regions: &efi,
        }]];
        assert!(!unfinished_mapping_intent(&open));
    }

    #[test]
    fn stamp_offsets_by_block_type() {
        let mut agf = vec![0u8; 512];
        agf[0..4].copy_from_slice(b"XAGF");
        assert_eq!(lsn_crc_offsets(&agf), Some((208, 216)));
        let mut leaf = vec![0u8; 4096];
        leaf[8..10].copy_from_slice(&0x3DF1u16.to_be_bytes());
        assert_eq!(lsn_crc_offsets(&leaf), Some((24, 12)));
        assert_eq!(lsn_crc_offsets(&[0u8; 512]), None);
    }

    #[test]
    fn stored_lsn_zero_or_unset_is_never_newer() {
        assert!(!newer_or_same(Some(0), 5));
        assert!(!newer_or_same(Some(u64::MAX), 5));
        assert!(newer_or_same(Some(5), 5));
        assert!(!newer_or_same(Some(4), 5));
    }
}
