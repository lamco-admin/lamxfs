//! CRC32C (Castagnoli) for v5 self-describing metadata.
//!
//! XFS v5 stores a CRC32C in each metadata block, computed over the whole block
//! with the 4-byte CRC field treated as zero. `CRC_32_ISCSI` is the iSCSI
//! Castagnoli polynomial — identical to what XFS (and btrfs) store on disk — and
//! the `crc` crate is `no_std`-clean. CRC is linear, so we feed the block in
//! three chunks: bytes before the field, four zero bytes, then the rest.

use crc::{Crc, CRC_32_ISCSI};

static CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// A running CRC32C for data that arrives in pieces.
pub(crate) fn digest() -> crc::Digest<'static, u32> {
    CRC32C.digest()
}

/// XFS-style CRC32C over `buf`, with the 4-byte field at `crc_off` zeroed.
/// Returns 0 if `crc_off` is out of range (the caller's bounds check will have
/// already rejected such a block).
pub(crate) fn checksum(buf: &[u8], crc_off: usize) -> u32 {
    let Some(end) = crc_off.checked_add(4) else {
        return 0;
    };
    if end > buf.len() {
        return 0;
    }
    let mut d = CRC32C.digest();
    d.update(&buf[..crc_off]);
    d.update(&[0u8; 4]);
    d.update(&buf[end..]);
    d.finalize()
}
