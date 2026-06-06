//! Fuzz `Xfs::open` on arbitrary bytes: no panic, no OOB, no hang.
#![no_main]

use libfuzzer_sys::fuzz_target;

use lamxfs::{BlockRead, Xfs};

struct Mem<'a>(&'a [u8]);

impl BlockRead for Mem<'_> {
    type Error = ();
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ()> {
        let off = usize::try_from(off).map_err(|_| ())?;
        let end = off.checked_add(buf.len()).ok_or(())?;
        buf.copy_from_slice(self.0.get(off..end).ok_or(())?);
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let len = data.len() as u64;
    let _ = Xfs::open(Mem(data), len);
});
