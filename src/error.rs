//! Typed error surface with stable `&'static str` tokens.
//!
//! Every error carries a stable vocabulary token (see [`Error::token`]) so a
//! bootloader's trust log can record *why* a read failed without string
//! formatting and without the token drifting across releases.

/// A read or mount failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An underlying [`BlockRead`](crate::BlockRead) call failed.
    Io { token: &'static str, offset: u64 },
    /// Superblock missing, malformed, or an unsupported magic/version/geometry.
    BadSuperblock(SuperblockReason),
    /// An incompat feature flag this reader does not implement.
    UnsupportedFeature(&'static str),
    /// A v5 self-describing-metadata CRC32C mismatch.
    BadCrc {
        ag: u32,
        ag_block: u32,
        what: &'static str,
    },
    /// Structural inconsistency detected mid-walk (bad magic, key order,
    /// out-of-range count, entry past block end, …).
    Inconsistent {
        token: &'static str,
        where_: Location,
    },
    /// Path resolution found no such component.
    NotFound { component: &'static str },
    /// The target exists but is not a regular file (for `read_file`).
    NotARegularFile,
    /// The target exists but is not a symlink (for `read_link`).
    NotASymlink,
    /// A file's declared size exceeds the read cap
    /// ([`MAX_FILE_BYTES`](crate::MAX_FILE_BYTES)) — refused before allocating,
    /// so a hostile inode size cannot drive a denial-of-boot allocation.
    FileTooLarge { size: u64, max: u64 },
    /// An allocation failed at the named site. `lamxfs` never falls back.
    OutOfMemory { site: &'static str },
}

/// Why a superblock was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperblockReason {
    BadMagic,
    BadCrc,
    UnsupportedVersion,
    BadGeometry,
    ExternalLog,
    RealtimeDevice,
    DirtyLog,
}

/// Where in the structure a [`Error::Inconsistent`] was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Inode { ino: u64 },
    Bmbt { ino: u64, block: u32 },
    Dir { ino: u64, block: u32 },
    Symlink { ino: u64 },
}

impl Error {
    /// The stable vocabulary token for this error (trust-log friendly).
    pub fn token(&self) -> &'static str {
        match self {
            Error::Io { token, .. } => token,
            Error::BadSuperblock(r) => r.token(),
            Error::UnsupportedFeature(t) => t,
            Error::BadCrc { what, .. } => what,
            Error::Inconsistent { token, .. } => token,
            Error::NotFound { .. } => "not_found",
            Error::NotARegularFile => "not_a_regular_file",
            Error::NotASymlink => "not_a_symlink",
            Error::FileTooLarge { .. } => "file_too_large",
            Error::OutOfMemory { site } => site,
        }
    }
}

impl SuperblockReason {
    pub(crate) fn token(self) -> &'static str {
        match self {
            SuperblockReason::BadMagic => "sb_bad_magic",
            SuperblockReason::BadCrc => "sb_bad_crc",
            SuperblockReason::UnsupportedVersion => "sb_unsupported_version",
            SuperblockReason::BadGeometry => "sb_bad_geometry",
            SuperblockReason::ExternalLog => "sb_external_log",
            SuperblockReason::RealtimeDevice => "sb_realtime_device",
            SuperblockReason::DirtyLog => "sb_dirty_log",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lamxfs: {}", self.token())
    }
}

#[cfg(feature = "std")]
extern crate std;
#[cfg(feature = "std")]
impl std::error::Error for Error {}

pub(crate) type Result<T> = core::result::Result<T, Error>;
