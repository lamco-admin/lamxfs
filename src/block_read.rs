//! The `BlockRead` trait — the byte source `lamxfs` reads a volume through.

/// Block-level reads for a mounted volume.
///
/// `lamxfs` never seeks; it asks for a byte range starting at an absolute
/// offset and expects the implementor to fill the whole buffer. Identical in
/// shape to `lambutter::BlockRead`, so a consumer that already provides one
/// needs only a one-line adapter to satisfy both.
pub trait BlockRead {
    /// Error type surfaced by the underlying device.
    type Error: core::fmt::Debug;

    /// Fill `buf` entirely from the volume starting at `offset_bytes`. A short
    /// read (fewer bytes available than `buf.len()`) is an error — the caller
    /// always sizes `buf` to exactly the range it needs.
    fn read_at(&mut self, offset_bytes: u64, buf: &mut [u8]) -> Result<(), Self::Error>;
}
