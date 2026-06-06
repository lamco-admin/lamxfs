#!/bin/bash
set -e
export PATH="/usr/sbin:/sbin:$PATH"
FX=tests/fixtures
MAN="$FX/MANIFEST"
: > "$MAN"

populate() {
  local M="$1"
  sudo mkdir -p "$M/boot/grub2" "$M/boot/loader/entries"
  printf 'VMLINUZ-CONTENT-12345\n' | sudo tee "$M/boot/vmlinuz-6.1.0-test" >/dev/null
  printf 'NAME="Rocky Linux"\nVERSION="9.3"\n' | sudo tee "$M/boot/os-release" >/dev/null
  printf 'title Rocky\nlinux /vmlinuz-6.1.0-test\ninitrd /initramfs-6.1.0.img\n' | sudo tee "$M/boot/loader/entries/rocky.conf" >/dev/null
  # deterministic, compressible, multi-block file (repeating pattern, 2 MiB)
  sudo bash -c "yes 'lamxfs-extent-pattern-0123456789abcdef' | head -c 2097152 > '$M/boot/bigfile'"
  # sparse file: data at 0, a hole, data at 4 MiB -> multiple extents + a hole
  sudo bash -c "printf 'HEAD' > '$M/boot/sparse'; truncate -s 4194304 '$M/boot/sparse'; printf 'TAIL' >> '$M/boot/sparse'"
  # fragmented file: 40 data blocks each separated by a 1-block hole -> >21
  # extents, overflowing the inline extent list into BTREE inode format.
  sudo bash -c "for i in \$(seq 0 39); do printf 'frag-block-%03d-pad' \$i | dd of='$M/boot/fragmented' bs=4096 count=1 seek=\$((i*2)) conv=notrunc status=none; done"
  sudo ln -s vmlinuz-6.1.0-test "$M/boot/vmlinuz"
  sudo ln -s "deliberately-long-symlink-target-overflowing-shortform-into-a-remote-extent-block-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" "$M/boot/longlink"
  for i in $(seq 1 200); do printf 'k%03d\n' "$i" | sudo tee "$M/boot/grub2/file-$i" >/dev/null; done
  sync
}

gen_one() {
  local name="$1"; shift
  local img="$FX/$name.img"
  truncate -s 320M "$img"
  mkfs.xfs -q -f "$@" "$img" >/dev/null 2>&1
  local feat; feat=$(xfs_db -r -c "sb 0" -c "p versionnum features_incompat" "$img" 2>/dev/null | tr '\n' ' ')
  local M; M=$(mktemp -d)
  sudo mount -o loop "$img" "$M"
  populate "$M"
  for f in boot/vmlinuz-6.1.0-test boot/os-release boot/loader/entries/rocky.conf boot/bigfile boot/sparse boot/fragmented; do
    local h; h=$(sudo sha256sum "$M/$f" | awk '{print $1}')
    echo "$name|/$f|$h" >> "$MAN"
  done
  # Record the fragmented file's inode format so the test can assert BTREE.
  local fino; fino=$(sudo stat -c %i "$M/boot/fragmented")
  sudo umount "$M"
  local fmt; fmt=$(xfs_db -r -c "inode $fino" -c "p core.format" "$img" 2>/dev/null | awk -F'[()]' '{print $2}')
  echo "# $name fragmented inode=$fino format=$fmt" >> "$MAN"
  rmdir "$M"
  zstd -q -19 -f --rm "$img"
  echo "  $name: $feat -> $(du -h "$img.zst" | awk '{print $1}')"
}

echo "generating fixtures:"
gen_one v5_modern
gen_one v5_nonrext64 -i nrext64=0
gen_one v4_legacy -m crc=0
echo "--- MANIFEST ---"; cat "$MAN"
ls -la "$FX"/*.zst
