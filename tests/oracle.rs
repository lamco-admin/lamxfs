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
        "v5_lts618" => include_bytes!("fixtures/v5_lts618.img.zst"),
        "v5_metadir" => include_bytes!("fixtures/v5_metadir.img.zst"),
        "v5_bs64k" => include_bytes!("fixtures/v5_bs64k.img.zst"),
        "v5_sect8k" => include_bytes!("fixtures/v5_sect8k.img.zst"),
        "v5_nrext64_upgraded" => include_bytes!("fixtures/v5_nrext64_upgraded.img.zst"),
        "v5_wrapped_log" => include_bytes!("fixtures/v5_wrapped_log.img.zst"),
        "v5_dirty_log" => include_bytes!("fixtures/v5_dirty_log.img.zst"),
        "v5_rtgroups_reject" => include_bytes!("fixtures/v5_rtgroups_reject.img.zst"),
        "v5_zoned_reject" => include_bytes!("fixtures/v5_zoned_reject.img.zst"),
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

/// What stock mkfs.xfs makes since xfsprogs 6.18: parent pointers and
/// exchange-range on, `features_incompat` 0xeb.
#[test]
fn v5_lts618() {
    check_manifest("v5_lts618");
    check_structure("v5_lts618");
}

#[test]
fn v5_metadir() {
    check_manifest("v5_metadir");
    check_structure("v5_metadir");
}

/// 64 KiB blocks of 512-byte inodes, so 128 inodes per block (inopblog 7).
#[test]
fn v5_bs64k() {
    check_manifest("v5_bs64k");
    check_structure("v5_bs64k");
}

/// An 8 KiB superblock sector, all of which the v5 CRC covers.
#[test]
fn v5_sect8k() {
    check_manifest("v5_sect8k");
    check_structure("v5_sect8k");
}

/// `xfs_repair -c nrext64=1` sets the superblock bit and leaves existing
/// inodes on 32-bit extent counts; `fragmented-late` was written afterwards
/// with 64-bit ones. Both have to read back.
#[test]
fn v5_nrext64_upgraded() {
    check_manifest("v5_nrext64_upgraded");
    check_structure("v5_nrext64_upgraded");
}

/// A cleanly unmounted log that has wrapped past the end of its ring.
#[test]
fn v5_wrapped_log() {
    check_manifest("v5_wrapped_log");
    check_structure("v5_wrapped_log");
}

fn open_err(name: &str) -> &'static str {
    let data = image(name);
    let len = data.len() as u64;
    match Xfs::open(Mem(data), len) {
        Ok(_) => panic!("{name}: opened"),
        Err(e) => e.token(),
    }
}

/// Shut down without flushing: the log holds transactions a reader can't
/// replay.
#[test]
fn v5_dirty_log_is_refused() {
    assert_eq!(open_err("v5_dirty_log"), "sb_dirty_log");
}

#[test]
fn realtime_layouts_are_refused() {
    assert_eq!(open_err("v5_rtgroups_reject"), "sb_realtime_device");
    assert_eq!(open_err("v5_zoned_reject"), "sb_realtime_device");
}
