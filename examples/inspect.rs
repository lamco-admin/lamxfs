//! Inspect an XFS image: print the superblock summary and either list a
//! directory or read a file.
//!
//! ```text
//! cargo run --example inspect -- <image> [path]
//! ```
//!
//! `path` defaults to `/`. If it names a directory its entries are listed; if it
//! names a file, the byte count is printed (and the contents, if small).

use std::{fmt::Write as _, io::Read};

use lamxfs::{BlockRead, Path, Xfs};

/// Reads an in-memory image.
struct MemReader(Vec<u8>);

impl BlockRead for MemReader {
    type Error = ();
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ()> {
        let off = usize::try_from(off).map_err(|_| ())?;
        let end = off.checked_add(buf.len()).ok_or(())?;
        buf.copy_from_slice(self.0.get(off..end).ok_or(())?);
        Ok(())
    }
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let img = args.next().ok_or("usage: inspect <image> [path]")?;
    let path = args.next().unwrap_or_else(|| "/".to_string());

    let mut data = Vec::new();
    std::fs::File::open(&img)
        .and_then(|mut f| f.read_to_end(&mut data))
        .map_err(|e| format!("{img}: {e}"))?;
    let len = data.len() as u64;

    let mut fs = Xfs::open(MemReader(data), len).map_err(|e| format!("open: {e}"))?;
    let mut uuid = String::new();
    for b in fs.uuid() {
        let _ = write!(uuid, "{b:02x}");
    }
    println!("uuid  = {uuid}");
    println!("label = {:?}", fs.label());

    let p = Path::new(path.as_bytes());
    if let Ok(entries) = fs.read_dir(p) {
        println!("--- {path} ({} entries) ---", entries.len());
        for e in entries {
            println!(
                "  {:<28} {:?} ino={}",
                String::from_utf8_lossy(&e.name),
                e.kind,
                e.inode_number
            );
        }
    } else {
        // Not a directory — read it as a file instead.
        let bytes = fs.read_file(p).map_err(|e| format!("read {path}: {e}"))?;
        println!("{path}: {} bytes", bytes.len());
        if bytes.len() <= 4096 {
            print!("{}", String::from_utf8_lossy(&bytes));
        }
    }
    Ok(())
}
