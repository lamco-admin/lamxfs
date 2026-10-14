#!/bin/bash
# Build the lamxfs fixture images.
#
#   tests/fixtures/regenerate.sh [FIXTURE...]
#
# With no arguments every fixture is rebuilt. A rebuilt fixture's MANIFEST lines
# are replaced and the others are kept, so rebuilding one image leaves the rest
# and their hashes alone. Needs xfsprogs, zstd and sudo (loop mounts).
# v5_lts618 and v5_zoned_reject need xfsprogs 6.18 or later: set MKFS_NEW to a
# command that runs one (README.md has a container recipe).
set -euo pipefail
export PATH="/usr/sbin:/sbin:$PATH"
cd "$(dirname "$0")"
FX="$PWD"
MKFS_NEW="${MKFS_NEW:-mkfs.xfs}"
MAN=MANIFEST
FILES="boot/vmlinuz-6.1.0-test boot/os-release boot/loader/entries/rocky.conf boot/bigfile boot/sparse boot/fragmented"
ALL="v5_modern v5_nonrext64 v4_legacy v5_lts618 v5_metadir v5_bs64k v5_sect8k
     v5_nrext64_upgraded v5_wrapped_log v5_dirty_log v5_rtgroups_reject v5_zoned_reject"

WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/lamxfs-fixtures.XXXXXX")
M=""
cleanup() {
  if [ -n "$M" ]; then sudo umount "$M" 2>/dev/null || true; rmdir "$M" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# 40 one-block writes each followed by a one-block hole: more than 21 extents,
# which overflows the inline extent list into BTREE format.
fragments() {
  for i in $(seq 0 39); do
    printf "$2-block-%03d-pad" "$i" | dd of="$1" bs=4096 count=1 seek=$((i * 2)) conv=notrunc status=none
  done
}

# The tree every populated fixture carries, built once as the current user.
stage() {
  local d="$1/boot"
  mkdir -p "$d/grub2" "$d/loader/entries"
  printf 'VMLINUZ-CONTENT-12345\n' > "$d/vmlinuz-6.1.0-test"
  printf 'NAME="Rocky Linux"\nVERSION="9.3"\n' > "$d/os-release"
  printf 'title Rocky\nlinux /vmlinuz-6.1.0-test\ninitrd /initramfs-6.1.0.img\n' > "$d/loader/entries/rocky.conf"
  # deterministic, compressible, multi-block file (repeating pattern, 2 MiB)
  head -c 2097152 < <(yes 'lamxfs-extent-pattern-0123456789abcdef') > "$d/bigfile"
  # sparse file: data at 0, a 4 MiB hole, data at 4 MiB
  printf 'HEAD' > "$d/sparse"; truncate -s 4194304 "$d/sparse"; printf 'TAIL' >> "$d/sparse"
  fragments "$d/fragmented" frag
  ln -s vmlinuz-6.1.0-test "$d/vmlinuz"
  ln -s "deliberately-long-symlink-target-overflowing-shortform-into-a-remote-extent-block-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" "$d/longlink"
  for i in $(seq 1 200); do printf 'k%03d\n' "$i" > "$d/grub2/file-$i"; done
}

# A protofile for mkfs -p that reproduces a staged tree, for the fixtures the
# host kernel can't mount.
proto_dir() {
  local e n
  for e in "$1"/*; do
    n=$(basename "$e")
    if [ -L "$e" ]; then echo "$n l--777 0 0 $(readlink "$e")"
    elif [ -d "$e" ]; then echo "$n d--755 0 0"; proto_dir "$e"; echo '$'
    else echo "$n ---644 0 0 $e"; fi
  done
}

# mkimg NAME SIZE MKFS...: a fresh image made by MKFS (the image path is
# appended, and is absolute so a container with the directory bound can use it).
mkimg() {
  local name=$1 size=$2; shift 2
  rm -f "$name.img" "$name.img.zst"
  truncate -s "$size" "$name.img"
  "$@" "$FX/$name.img" >/dev/null
}

mount_img() { M=$(mktemp -d); sudo mount -o loop "$1" "$M"; }
unmount_img() { sudo umount "$M"; rmdir "$M"; M=""; }

# Copy staged files in through the kernel and check it hands the same bytes back.
copy_in() {
  local base=$1; shift
  sudo cp -a --sparse=always "$base/boot" "$M/"
  sync
  local f
  for f in "$@"; do
    [ "$(sudo sha256sum "$M/$f" | cut -d' ' -f1)" = "$(sha256sum "$base/$f" | cut -d' ' -f1)" ] \
      || { echo "kernel copy of $f differs" >&2; exit 1; }
  done
}

# record NAME BASE FILE...: MANIFEST lines for files whose content came from BASE.
record() {
  local name=$1 base=$2 f; shift 2
  for f in "$@"; do echo "$name|/$f|$(sha256sum "$base/$f" | cut -d' ' -f1)" >> "$MAN"; done
}

note() { echo "# $1 $2" >> "$MAN"; }

# features_incompat straight from the superblock, so it works whatever the
# host's xfs_db knows about.
incompat() { echo "0x$(od -An -tx1 -j216 -N4 "$1.img" | tr -d ' ')"; }

# The fragmented file's inode format, recorded so a reader can see which
# fixtures exercise the BMBT walk.
frag_note() {
  local name=$1 ino=$2 fmt
  fmt=$(xfs_db -r -c "inode $ino" -c "p core.format" "$name.img" 2>/dev/null | awk -F'[()]' '{print $2}') || true
  note "$name" "fragmented inode=$ino format=${fmt:-unknown}"
}

finish() { zstd -q -19 -f --rm "$1.img"; }

# "log tail: T head: H state: <S>". Read whole, since a grep that stops at the
# first match would SIGPIPE xfs_logprint and fail the pipeline.
logstate() { xfs_logprint -t "$1.img" 2>/dev/null | grep -o 'log tail.*' || true; }

# A reject fixture keeps only the first 64 KiB: the reader refuses it from the
# superblock before reading anything else.
head_only() { head -c 65536 "$1.img" > "$1.img.head"; mv "$1.img.head" "$1.img"; }

kernel_fixture() {
  local name=$1 size=$2 ino; shift 2
  mkimg "$name" "$size" "$@"
  mount_img "$name.img"
  copy_in "$STAGE" $FILES
  ino=$(sudo stat -c %i "$M/boot/fragmented")
  unmount_img
  record "$name" "$STAGE" $FILES
  note "$name" "features_incompat=$(incompat "$name")"
  frag_note "$name" "$ino"
  finish "$name"
}

proto_fixture() {
  local name=$1 size=$2; shift 2
  { echo /dev/null; echo 0 0; echo 'd--755 0 0'; proto_dir "$STAGE"; echo '$'; } > "$WORK/proto"
  mkimg "$name" "$size" "$@" -p "$WORK/proto"
  record "$name" "$STAGE" $FILES
  note "$name" "features_incompat=$(incompat "$name") populated by mkfs -p, not the kernel"
  finish "$name"
}

build_v5_modern() { kernel_fixture v5_modern 320M mkfs.xfs -q -f; }
build_v5_nonrext64() { kernel_fixture v5_nonrext64 320M mkfs.xfs -q -f -i nrext64=0; }
build_v4_legacy() { kernel_fixture v4_legacy 320M mkfs.xfs -q -f -m crc=0; }

# Stock mkfs from xfsprogs 6.18 on: parent pointers and exchange-range on.
build_v5_lts618() {
  kernel_fixture v5_lts618 320M $MKFS_NEW -q -f
  note v5_lts618 "made by $($MKFS_NEW -V)"
}

# The host kernel (6.12) can't mount these two: metadir arrived in 6.13, and a
# sector larger than the page needs block-device support from 6.15.
build_v5_metadir() { proto_fixture v5_metadir 320M mkfs.xfs -q -f -m metadir=1; }
build_v5_sect8k() { proto_fixture v5_sect8k 320M mkfs.xfs -q -f -s size=8192 -b size=8192; }

# 64 KiB blocks of 512-byte inodes: inopblog 7. Two AGs leave room for the
# minimum log.
build_v5_bs64k() { kernel_fixture v5_bs64k 320M mkfs.xfs -q -f -b size=65536 -d agcount=2; }

# xfs_repair -c nrext64=1 turns the feature on and leaves every existing inode
# on the 32-bit layout; a file written afterwards gets the 64-bit one.
build_v5_nrext64_upgraded() {
  local name=v5_nrext64_upgraded old new
  mkdir -p "$WORK/late/boot"
  fragments "$WORK/late/boot/fragmented-late" late
  mkimg "$name" 320M mkfs.xfs -q -f -i nrext64=0
  mount_img "$name.img"; copy_in "$STAGE" $FILES; unmount_img
  xfs_repair -c nrext64=1 "$name.img" >/dev/null 2>&1
  mount_img "$name.img"
  copy_in "$WORK/late" boot/fragmented-late
  old=$(sudo stat -c %i "$M/boot/fragmented")
  new=$(sudo stat -c %i "$M/boot/fragmented-late")
  unmount_img
  local oldf newf
  oldf=$(xfs_db -r -c "inode $old" -c "p v3.nrext64" "$name.img" | awk '{print $3}')
  newf=$(xfs_db -r -c "inode $new" -c "p v3.nrext64" "$name.img" | awk '{print $3}')
  [ "$oldf" = 0 ] && [ "$newf" = 1 ] || { echo "$name: expected nrext64 0 then 1, got $oldf $newf" >&2; exit 1; }
  record "$name" "$STAGE" $FILES
  record "$name" "$WORK/late" boot/fragmented-late
  note "$name" "features_incompat=$(incompat "$name") fragmented inode=$old nrext64=0, fragmented-late inode=$new nrext64=1"
  finish "$name"
}

# mkfs won't make a log under 64 MiB outside fstests; its override allows the
# 3075-block minimum, which 10000 forced checkpoints wrap past the ring's end.
build_v5_wrapped_log() {
  local name=v5_wrapped_log
  TEST_DIR=/nonexistent TEST_DEV=/nonexistent QA_CHECK_FS=1 mkimg "$name" 320M mkfs.xfs -q -f -l size=3075b
  mount_img "$name.img"
  sudo mkdir "$M/churn"
  sudo bash -c "for i in \$(seq 1 20000); do touch '$M/churn/f'\$((i % 64)); [ \$((i % 2)) = 0 ] && sync -f '$M'; done; true"
  sudo rm -rf "$M/churn"
  copy_in "$STAGE" $FILES
  unmount_img
  xfs_logprint -d "$name.img" > "$WORK/logdump" 2>/dev/null || true
  grep -q 'HEADER Cycle 2' "$WORK/logdump" || { echo "$name: log did not wrap" >&2; exit 1; }
  [[ "$(logstate "$name")" == *'<CLEAN>'* ]] || { echo "$name: log not clean" >&2; exit 1; }
  record "$name" "$STAGE" $FILES
  note "$name" "$(logstate "$name") in cycle 2"
  finish "$name"
}

# Shut down without flushing, so the unmount writes no unmount record.
build_v5_dirty_log() {
  local name=v5_dirty_log
  mkimg "$name" 320M mkfs.xfs -q -f
  mount_img "$name.img"
  copy_in "$STAGE" $FILES
  sudo sync -f "$M"
  sudo touch "$M/boot/after-sync"
  sudo xfs_io -x -c shutdown "$M"
  unmount_img
  [[ "$(logstate "$name")" == *'<DIRTY>'* ]] || { echo "$name: log not dirty" >&2; exit 1; }
  note "$name" "$(logstate "$name")"
  finish "$name"
}

build_v5_rtgroups_reject() {
  local name=v5_rtgroups_reject
  truncate -s 64M "$WORK/rt.img"
  mkimg "$name" 320M mkfs.xfs -q -f -m metadir=1 -r rtdev="$WORK/rt.img"
  note "$name" "features_incompat=$(incompat "$name") realtime groups on a separate device; first 64 KiB only"
  head_only "$name"; finish "$name"
}

build_v5_zoned_reject() {
  local name=v5_zoned_reject
  mkimg "$name" 1G $MKFS_NEW -q -f -r zoned=1,rgcount=8
  note "$name" "features_incompat=$(incompat "$name") internal zoned realtime section; first 64 KiB only"
  head_only "$name"; finish "$name"
}

STAGE="$WORK/stage"
stage "$STAGE"
[ $# -gt 0 ] || set -- $ALL
touch "$MAN"
echo "generating fixtures:"
for name in "$@"; do
  grep -vE "^($name\||# $name )" "$MAN" > "$WORK/man" || true
  cat "$WORK/man" > "$MAN"
  "build_$name"
  echo "  $name -> $(du -h "$name.img.zst" | cut -f1)"
done
