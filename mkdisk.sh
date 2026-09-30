#!/bin/sh
# rootfs/ からディスクイメージ disk.img を作る
#   ./mkdisk.sh [size]          ext4 (既定 1G)
#   FS=ext2 ./mkdisk.sh [size]  ext2
# aios はまだ htree (dir_index) の索引を書きかえられないので外しておく
set -e
cd "$(dirname "$0")"
[ -d rootfs ] || { echo "rootfs/ がありません。先に ./mkrootfs.sh を実行してください" >&2; exit 1; }
rm -f disk.img
case "${FS:-ext4}" in
  ext4) mkfs.ext4 -q -F -O ^dir_index -E root_owner=0:0 -d rootfs disk.img "${1:-1G}" ;;
  ext2) mkfs.ext2 -q -F -b 4096 -O ^dir_index,^resize_inode,^ext_attr -E root_owner=0:0 -d rootfs disk.img "${1:-1G}" ;;
  *) echo "FS は ext4 か ext2" >&2; exit 1 ;;
esac
echo "disk.img (${FS:-ext4}) ready"
