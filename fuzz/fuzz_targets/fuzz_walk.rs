//! Fuzz a full open + walk (resolve → inode → bmbt → dir → file → symlink) over
//! a fuzzed volume. Seeded with a real image so mutations exercise the deep
//! parsers (the `cfg(fuzzing)` CRC bypass lets mutated metadata through). No
//! input may panic, OOB, or hang.
#![no_main]

use libfuzzer_sys::fuzz_target;

use lamxfs::{BlockRead, EntryKind, Path, Xfs};

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
    let Ok(mut fs) = Xfs::open(Mem(data), len) else {
        return;
    };
    let _ = fs.uuid();
    let _ = fs.label();

    if let Ok(entries) = fs.read_dir(Path::new(b"/")) {
        for e in entries.iter().take(64) {
            let mut path = Vec::with_capacity(e.name.len() + 1);
            path.push(b'/');
            path.extend_from_slice(&e.name);
            let p = Path::new(&path);
            match e.kind {
                EntryKind::Directory => {
                    let _ = fs.read_dir(p);
                }
                EntryKind::Symlink => {
                    let _ = fs.read_link(p);
                }
                _ => {
                    let _ = fs.read_file(p);
                }
            }
        }
    }
    // Known seed paths drive the nested-resolve + leaf-dir + extent paths.
    let _ = fs.read_file(Path::new(b"/boot/os-release"));
    let _ = fs.read_file(Path::new(b"/boot/bigfile"));
    let _ = fs.read_file(Path::new(b"/boot/sparse"));
    let _ = fs.read_dir(Path::new(b"/boot/grub2"));
    let _ = fs.read_link(Path::new(b"/boot/longlink"));
});
