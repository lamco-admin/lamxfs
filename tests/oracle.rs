//! Layer-B oracle tests: read real `mkfs.xfs`-produced images and verify every
//! file against its kernel-produced SHA-256.
//!
//! Fixtures are committed `.img.zst` (decompressed in-memory via `ruzstd`); the
//! Linux XFS driver wrote the bytes, so any divergence is a bug in this reader.
//! Regenerate with `tests/fixtures/regenerate.sh`.

use std::io::Read;

use lamxfs::{BlockRead, EntryKind, Path, Xfs};
use sha2::{Digest, Sha256};

struct Mem(Vec<u8>);

impl BlockRead for Mem {
    type Error = ();
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ()> {
        let off = usize::try_from(off).map_err(|_| ())?;
        let end = off.checked_add(buf.len()).ok_or(())?;
        buf.copy_from_slice(self.0.get(off..end).ok_or(())?);
        Ok(())
    }
}

const MANIFEST: &str = include_str!("fixtures/MANIFEST");

fn image(name: &str) -> Vec<u8> {
    let comp: &[u8] = match name {
        "v5_modern" => include_bytes!("fixtures/v5_modern.img.zst"),
        "v5_nonrext64" => include_bytes!("fixtures/v5_nonrext64.img.zst"),
        "v4_legacy" => include_bytes!("fixtures/v4_legacy.img.zst"),
        other => panic!("unknown fixture {other}"),
    };
    let mut dec = ruzstd::decoding::StreamingDecoder::new(comp).expect("zstd");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("decompress");
    out
}

fn sha_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn open(name: &str) -> Xfs<Mem> {
    let data = image(name);
    let len = data.len() as u64;
    Xfs::open(Mem(data), len).unwrap_or_else(|e| panic!("open {name}: {e:?}"))
}

/// Every manifest file reads back with the exact kernel-produced hash.
fn check_manifest(name: &str) {
    let mut fs = open(name);
    let mut checked = 0;
    for line in MANIFEST.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue; // comment / blank (e.g. the recorded inode-format note)
        }
        let mut it = line.split('|');
        let (fx, path, want) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        if fx != name {
            continue;
        }
        let got = fs
            .read_file(Path::new(path.as_bytes()))
            .unwrap_or_else(|e| {
                panic!("{name}: read_file {path}: {e:?}");
            });
        assert_eq!(sha_hex(&got), want, "{name}: {path} content mismatch");
        checked += 1;
    }
    // ≥6 includes /boot/fragmented, whose inode is BTREE format — so a passing
    // hash here is end-to-end proof of the BMBT B+tree extent walk.
    assert!(
        checked >= 6,
        "{name}: expected ≥6 manifest files, got {checked}"
    );
}

/// Structural checks shared by every fixture (identical contents).
fn check_structure(name: &str) {
    let mut fs = open(name);

    assert_ne!(fs.uuid(), [0u8; 16], "{name}: zero uuid");

    // /boot listing: names + kinds.
    let mut entries = fs.read_dir(Path::new(b"/boot")).expect("read_dir /boot");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let by_name = |n: &[u8]| entries.iter().find(|e| e.name == n).cloned();

    let grub2 = by_name(b"grub2").unwrap_or_else(|| panic!("{name}: no grub2"));
    assert_eq!(grub2.kind, EntryKind::Directory);
    assert_eq!(by_name(b"vmlinuz").unwrap().kind, EntryKind::Symlink);
    assert_eq!(by_name(b"longlink").unwrap().kind, EntryKind::Symlink);
    assert_eq!(by_name(b"os-release").unwrap().kind, EntryKind::Regular);

    // Leaf-format directory: 200 entries.
    let g = fs
        .read_dir(Path::new(b"/boot/grub2"))
        .expect("read_dir grub2");
    assert_eq!(g.len(), 200, "{name}: grub2 entry count");

    // Inline + remote symlinks.
    assert_eq!(
        fs.read_link(Path::new(b"/boot/vmlinuz")).unwrap(),
        b"vmlinuz-6.1.0-test"
    );
    let ll = fs.read_link(Path::new(b"/boot/longlink")).unwrap();
    assert_eq!(ll.len(), 159);
    assert!(ll.starts_with(b"deliberately-long-symlink-target"));

    // Sparse file: HEAD, a 4 MiB hole of zeros, TAIL.
    let sparse = fs.read_file(Path::new(b"/boot/sparse")).unwrap();
    assert_eq!(&sparse[..4], b"HEAD");
    assert_eq!(&sparse[4 * 1024 * 1024..4 * 1024 * 1024 + 4], b"TAIL");
    assert!(
        sparse[4..4 * 1024 * 1024].iter().all(|&b| b == 0),
        "{name}: hole not zero"
    );

    // Streaming read of a multi-block file matches the whole-file read.
    let ino = fs.resolve(Path::new(b"/boot/bigfile")).unwrap();
    let whole = fs.read_file(Path::new(b"/boot/bigfile")).unwrap();
    let mut buf = [0u8; 7777];
    let n = fs.read_file_at(&ino, 1000, &mut buf).unwrap();
    assert_eq!(&buf[..n], &whole[1000..1000 + n], "{name}: streamed range");

    // Nonexistent path.
    assert!(fs.read_file(Path::new(b"/boot/nope")).is_err());
}

#[test]
fn v5_modern() {
    check_manifest("v5_modern");
    check_structure("v5_modern");
}

#[test]
fn v5_nonrext64() {
    check_manifest("v5_nonrext64");
    check_structure("v5_nonrext64");
}

#[test]
fn v4_legacy() {
    check_manifest("v4_legacy");
    check_structure("v4_legacy");
}
