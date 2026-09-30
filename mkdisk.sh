#!/bin/sh
# rootfs/ から ext2 のディスクイメージ disk.img を作る
#   ./mkdisk.sh [size]   (既定 1G)
set -e
cd "$(dirname "$0")"
[ -d rootfs ] || { echo "rootfs/ がありません。先に ./mkrootfs.sh を実行してください" >&2; exit 1; }
rm -f disk.img
mkfs.ext2 -q -F -b 4096 -O ^dir_index,^resize_inode,^ext_attr -E root_owner=0:0 -d rootfs disk.img "${1:-1G}"
echo "disk.img ready"
