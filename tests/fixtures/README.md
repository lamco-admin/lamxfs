# Test fixtures

Real XFS filesystem images produced by `mkfs.xfs` (xfsprogs), committed as
zstd-compressed `.img.zst` and decompressed in-memory by `tests/oracle.rs`.

| Fixture | Made with | Exercises |
|---|---|---|
| `v5_modern.img.zst` | defaults (xfsprogs 6.13) | v5 + CRC + `NREXT64` + sparse inodes + `ftype` |
| `v5_nonrext64.img.zst` | `-i nrext64=0` | v5 with 32-bit extent counts (offset-76 path) |
| `v4_legacy.img.zst` | `-m crc=0` | v4 legacy: no CRC, 100-byte inode core |
| `v5_lts618.img.zst` | defaults (xfsprogs 7.1.1) | stock mkfs since 6.18: parent pointers + exchange-range, `features_incompat` 0xeb |
| `v5_metadir.img.zst` | `-m metadir=1`, populated by `-p` | metadata directory tree |
| `v5_bs64k.img.zst` | `-b size=65536 -d agcount=2` | 64 KiB blocks, inopblog 7 |
| `v5_sect8k.img.zst` | `-s size=8192 -b size=8192`, populated by `-p` | an 8 KiB superblock sector under the v5 CRC |
| `v5_nrext64_upgraded.img.zst` | `-i nrext64=0`, then `xfs_repair -c nrext64=1`, then one more file | inodes with 32-bit and 64-bit extent counts on one filesystem |
| `v5_wrapped_log.img.zst` | a 3075-block log, churned past its end | a clean log whose head sits mid-ring in cycle 2 |
| `v5_dirty_log.img.zst` | defaults, shut down without a log flush | must be refused with `sb_dirty_log` |
| `v5_rtgroups_reject.img.zst` | `-m metadir=1 -r rtdev=...`, first 64 KiB | must be refused with `sb_realtime_device` |
| `v5_zoned_reject.img.zst` | `-r zoned=1,rgcount=8` (xfsprogs 7.1.1), first 64 KiB | must be refused with `sb_realtime_device` |

Each populated image carries the same tree: `/boot` with a kernel-named file,
an `os-release`, a BLS entry under `loader/entries/`, a 2 MiB multi-block file,
a **sparse** file (data, a 4 MiB hole, data), a 40-extent file, an inline
symlink, a long (remote extent) symlink, and a 200-entry subdirectory.

The Linux XFS driver wrote the files in every image except `v5_metadir` and
`v5_sect8k`. The build host's kernel (6.12) can't mount those two (metadir needs
6.13, and a sector larger than the page needs 6.15), so `mkfs.xfs -p` wrote
them through libxfs instead. The reject images keep only their first 64 KiB,
since the reader refuses them from the superblock.

`MANIFEST` holds `fixture|path|sha256` for the content-verified files, plus a
comment per image recording its features and anything else worth knowing.

## Regenerating

```sh
tests/fixtures/regenerate.sh                  # every fixture
tests/fixtures/regenerate.sh v5_wrapped_log   # just one
```

It needs xfsprogs, zstd and sudo (for loop mounts). `v5_lts618` and
`v5_zoned_reject` need xfsprogs 6.18 or later; if the host is older, run one in
a container with this directory bound at the same path:

```sh
podman run -d --name lamxfs-f44 --security-opt label=disable \
    -v "$PWD/tests/fixtures:$PWD/tests/fixtures" registry.fedoraproject.org/fedora:44 sleep infinity
podman exec lamxfs-f44 dnf -y install xfsprogs
MKFS_NEW="podman exec lamxfs-f44 mkfs.xfs" tests/fixtures/regenerate.sh v5_lts618 v5_zoned_reject
```

Rebuilding an image gives it fresh UUIDs, so commit the new `.img.zst` and
`MANIFEST` together. Images are 320 MiB raw (the `mkfs.xfs` minimum) but mostly
empty, so most compress to a few tens of KiB; the churned log in
`v5_wrapped_log` makes it about 480 KiB.
