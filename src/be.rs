//! Big-endian field accessors.
//!
//! XFS stores every multi-byte on-disk field **big-endian**. Rather than derive
//! packed zero-copy structs (fiddly and alignment-sensitive on the densely
//! packed XFS records), the parsers read fields by explicit byte offset through
//! these bounds-checked helpers — clean-room, `no_std`, and free of `unsafe`.
//!
//! Every accessor returns `None` when the requested field would read past the
//! end of `buf`, so a truncated or hostile block can never index out of bounds;
//! callers map `None` to a structural error at the call site.

#[inline]
pub(crate) fn u8_at(buf: &[u8], off: usize) -> Option<u8> {
    buf.get(off).copied()
}

#[inline]
pub(crate) fn u16_at(buf: &[u8], off: usize) -> Option<u16> {
    let b = buf.get(off..off.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

#[inline]
pub(crate) fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    let b = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[inline]
pub(crate) fn u64_at(buf: &[u8], off: usize) -> Option<u64> {
    let b = buf.get(off..off.checked_add(8)?)?;
    Some(u64::from_be_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// A 16-byte UUID copied out of `buf`, or `None` if it would read past the end.
#[inline]
pub(crate) fn uuid_at(buf: &[u8], off: usize) -> Option<[u8; 16]> {
    let b = buf.get(off..off.checked_add(16)?)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(b);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_big_endian() {
        let buf = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        assert_eq!(u8_at(&buf, 0), Some(0x12));
        assert_eq!(u16_at(&buf, 0), Some(0x1234));
        assert_eq!(u32_at(&buf, 0), Some(0x1234_5678));
        assert_eq!(u64_at(&buf, 0), Some(0x1234_5678_9abc_def0));
    }

    #[test]
    fn rejects_out_of_bounds_instead_of_panicking() {
        let buf = [0u8; 4];
        assert_eq!(u64_at(&buf, 0), None);
        assert_eq!(u32_at(&buf, 2), None);
        assert_eq!(u16_at(&buf, 4), None);
        assert_eq!(u8_at(&buf, 4), None);
        assert_eq!(uuid_at(&buf, 0), None);
    }
}
