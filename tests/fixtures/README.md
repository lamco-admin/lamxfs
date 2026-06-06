# Test fixtures

Real XFS filesystem images produced by `mkfs.xfs` (xfsprogs), committed as
zstd-compressed `.img.zst` and decompressed in-memory by `tests/oracle.rs`. The
Linux XFS driver wrote the bytes, so any read divergence is a bug in `lamxfs`.

| Fixture | mkfs args | Exercises |
|---|---|---|
| `v5_modern.img.zst` | (defaults) | v5 + CRC + `NREXT64` + sparse inodes + `ftype` |
| `v5_nonrext64.img.zst` | `-i nrext64=0` | v5 with 32-bit extent counts (offset-76 path) |
| `v4_legacy.img.zst` | `-m crc=0` | v4 legacy: no CRC, 100-byte inode core |

Each image is populated identically: `/boot` with a kernel-named file, an
`os-release`, a BLS entry under `loader/entries/`, a 2 MiB multi-block file, a
**sparse** file (data — 4 MiB hole — data), an inline symlink, a long (remote
extent) symlink, and a 200-entry subdirectory (leaf-format directory).

`MANIFEST` holds `fixture|path|sha256` for the content-verified files.

## Regenerating

```sh
./regenerate.sh    # needs xfsprogs + sudo (loop mount) + zstd
```

Regeneration produces fresh UUIDs, so commit the new `.img.zst` and `MANIFEST`
together. Images are ≥ 320 MiB raw (the `mkfs.xfs` minimum) but mostly empty, so
each compresses to a few tens of KiB.
