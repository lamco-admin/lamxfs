//! The public `Xfs<R>` handle and its result types.

use alloc::{vec, vec::Vec};

use crate::{
    block_read::BlockRead,
    dir,
    error::{Error, Result, SuperblockReason},
    file, format,
    inode::Dinode,
    log,
    path::Path,
    resolve,
    superblock::Superblock,
};

/// Largest file `read_file` will allocate up front. A hostile inode can declare
/// a multi-GiB `di_size` while occupying no extents; this cap refuses the
/// allocation rather than letting it abort the boot (the LamBoot `fs_backend`
/// read contract; mirrors `MAX_BOOT_FILE_BYTES`). `read_file_at` streams into a
/// caller-sized buffer and is unaffected.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// The kind of a directory entry / inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// An inode handle (wraps the AG-encoded XFS inode number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inode {
    pub ino: u64,
}

/// Inode metadata derived from the inode core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    size: u64,
    mode: u32,
    kind: EntryKind,
}

impl Metadata {
    /// Logical file size in bytes (`di_size`).
    pub fn size(&self) -> u64 {
        self.size
    }
    /// POSIX mode bits.
    pub fn mode(&self) -> u32 {
        self.mode
    }
    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::Regular
    }
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
    pub fn is_symlink(&self) -> bool {
        self.kind == EntryKind::Symlink
    }
}

/// A directory entry. `name` is the raw on-disk name (not NUL-terminated).
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub kind: EntryKind,
    pub inode_number: u64,
}

/// A mounted, read-only XFS filesystem.
pub struct Xfs<R: BlockRead> {
    reader: R,
    sb: Superblock,
}

impl<R: BlockRead> Xfs<R> {
    /// Open + validate the filesystem. Reads and checks the primary superblock
    /// (magic, version, geometry, v5 CRC, supported features), refuses a log
    /// that still needs replay, and roots the handle at `sb_rootino`.
    /// `device_size_bytes` is accepted for parity and future SB-vs-device
    /// sanity checks.
    pub fn open(mut reader: R, _device_size_bytes: u64) -> Result<Self> {
        let io = |_| Error::Io {
            token: "io_sb",
            offset: 0,
        };
        // Most sectors are at most 4 KiB. A larger sector (up to the format's
        // 32 KiB) is read again whole, since the v5 CRC covers all of it.
        let mut buf = vec![0u8; 4096];
        reader.read_at(0, &mut buf).map_err(io)?;
        if let Some(sectsize @ 4097..=32768) = Superblock::sector_size(&buf) {
            buf = vec![0u8; usize::from(sectsize)];
            reader.read_at(0, &mut buf).map_err(io)?;
        }
        let sb = Superblock::parse(&buf)?;

        // Under `cargo fuzz` the verdict is ignored, like the CRC gates, so
        // mutated images still reach the inode, directory and BMBT parsers; the
        // log walk itself still runs.
        let clean = log::is_clean(&mut reader, &sb);
        if !cfg!(fuzzing) && !clean? {
            return Err(Error::BadSuperblock(SuperblockReason::DirtyLog));
        }
        Ok(Xfs { reader, sb })
    }

    /// Filesystem UUID (`sb_uuid` — what `blkid` reports and `root=UUID=` matches).
    pub fn uuid(&self) -> [u8; 16] {
        self.sb.uuid
    }

    /// Volume label (`sb_fname`), or `None` if empty.
    pub fn label(&self) -> Option<&str> {
        self.sb.label()
    }

    /// Resolve a path to an inode (does not follow a symlink at the leaf).
    pub fn resolve(&mut self, path: Path<'_>) -> Result<Inode> {
        resolve::resolve(&mut self.reader, &self.sb, path).map(|ino| Inode { ino })
    }

    /// Read an inode's metadata.
    pub fn metadata(&mut self, inode: &Inode) -> Result<Metadata> {
        let di = Dinode::read(&mut self.reader, &self.sb, inode.ino)?;
        Ok(meta_of(&di))
    }

    /// Read a regular file's full contents (capped at [`MAX_FILE_BYTES`]).
    pub fn read_file(&mut self, path: Path<'_>) -> Result<Vec<u8>> {
        let ino = resolve::resolve(&mut self.reader, &self.sb, path)?;
        let di = Dinode::read(&mut self.reader, &self.sb, ino)?;
        if !di.is_reg() {
            return Err(Error::NotARegularFile);
        }
        if di.size > MAX_FILE_BYTES {
            return Err(Error::FileTooLarge {
                size: di.size,
                max: MAX_FILE_BYTES,
            });
        }
        let cap = usize::try_from(di.size).map_err(|_| Error::FileTooLarge {
            size: di.size,
            max: MAX_FILE_BYTES,
        })?;
        let extents = file::extents_of(&mut self.reader, &self.sb, &di)?;
        let mut out = vec![0u8; cap];
        let n = file::read_into_with_extents(
            &mut self.reader,
            &self.sb,
            &extents,
            di.size,
            0,
            &mut out,
        )?;
        out.truncate(n);
        Ok(out)
    }

    /// Read up to `buf.len()` bytes of `inode` from `offset` (0 = EOF). The
    /// streaming path the PE loader uses; bounded by the caller's buffer.
    pub fn read_file_at(&mut self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let di = Dinode::read(&mut self.reader, &self.sb, inode.ino)?;
        if !di.is_reg() {
            return Err(Error::NotARegularFile);
        }
        file::read_into(&mut self.reader, &self.sb, &di, offset, buf)
    }

    /// Read a symlink's raw target bytes.
    pub fn read_link(&mut self, path: Path<'_>) -> Result<Vec<u8>> {
        let ino = resolve::resolve(&mut self.reader, &self.sb, path)?;
        let di = Dinode::read(&mut self.reader, &self.sb, ino)?;
        crate::symlink::read_link(&mut self.reader, &self.sb, &di, MAX_FILE_BYTES)
    }

    /// Enumerate directory entries. Per-entry kind is advisory: when the `ftype`
    /// feature is absent and a child inode read fails, the entry surfaces as
    /// [`EntryKind::Other`] rather than failing the whole listing (a hostile
    /// entry cannot hide its siblings).
    pub fn read_dir(&mut self, path: Path<'_>) -> Result<Vec<DirEntry>> {
        let ino = resolve::resolve(&mut self.reader, &self.sb, path)?;
        let di = Dinode::read(&mut self.reader, &self.sb, ino)?;
        let raws = dir::read_dir(&mut self.reader, &self.sb, &di)?;
        let mut out = Vec::with_capacity(raws.len());
        for e in raws {
            let kind = if self.sb.ftype {
                kind_from_ftype(e.ftype)
            } else {
                Dinode::read(&mut self.reader, &self.sb, e.inumber)
                    .map_or(EntryKind::Other, |d| meta_of(&d).kind)
            };
            out.push(DirEntry {
                name: e.name,
                kind,
                inode_number: e.inumber,
            });
        }
        Ok(out)
    }
}

fn meta_of(di: &Dinode) -> Metadata {
    let kind = if di.is_reg() {
        EntryKind::Regular
    } else if di.is_dir() {
        EntryKind::Directory
    } else if di.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    Metadata {
        size: di.size,
        mode: u32::from(di.mode),
        kind,
    }
}

/// Map an XFS `ftype` directory-entry byte to a kind.
fn kind_from_ftype(ft: u8) -> EntryKind {
    // XFS_DIR3_FT_*: 1=REG_FILE, 2=DIR, 7=SYMLINK; others (dev/fifo/sock) → Other.
    match ft {
        1 => EntryKind::Regular,
        2 => EntryKind::Directory,
        7 => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

// Silence "field never read" on the DEV format constant import path; the
// constant documents the on-disk value even though the read path treats DEV
// inodes as Other.
const _: u8 = format::DINODE_FMT_DEV;
