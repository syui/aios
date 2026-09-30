#!/bin/sh
# rootfs/ からディスクイメージ disk.img を作る
#   bin/mkdisk.sh [size]            GPT (既定 1G):
#                                     区画 1: EFI System Partition (FAT32, 128 MiB) = /boot
#                                     区画 2: Linux root (arm64) の ext4 = /
#   LAYOUT=ext4 bin/mkdisk.sh       区画なしで、ディスク全体を ext4 に (/boot も ext4 の中)
#   FS=ext2 ...                     root を ext2 に
#   OUT=FILE で出力先を変える (bin/mksd.sh が root の区画を作るのに使う。いつも区画なし)
#   NOBOOT=1 で root の /boot を空に (boot は別の FAT の区画に入れるとき)
# ESP には rootfs/boot の中身 (aios-kernel の Image と EFI/BOOT/BOOTAA64.EFI) を入れる。
# UEFI のファームウェアは EFI/BOOT/BOOTAA64.EFI (= Image、EFI スタブつき) を起動し、
# aios は起動後に ESP を /boot にマウントするので、aipkg でカーネルを入れかえられる
# GPT には sfdisk、FAT には mkfs.vfat と mcopy (dosfstools, mtools) が要る
# aios はまだ htree (dir_index) の索引を書きかえられないので外しておく
set -e
cd "$(dirname "$0")/.."
[ -d rootfs ] || { echo "rootfs/ がありません。先に bin/mkrootfs.sh を実行してください" >&2; exit 1; }
layout=${LAYOUT:-gpt}
[ -n "$OUT" ] && layout=ext4
if [ "$layout" = gpt ]; then
  for c in sfdisk mkfs.vfat mcopy; do
    command -v $c >/dev/null || { echo "$c がありません (LAYOUT=ext4 なら要りません)" >&2; exit 1; }
  done
fi
# 中のファイルは root のものにする (root でなければ fakeroot の中で)
if [ "$(id -u)" != 0 ]; then
  command -v fakeroot >/dev/null || { echo "root か fakeroot が必要です" >&2; exit 1; }
  exec fakeroot "$0" "$@"
fi
# chown は setuid/setgid のビットを消すので、覚えておいて付けなおす
suid=$(find rootfs -type f -perm -4000)
sgid=$(find rootfs -type f -perm -2000)
chown -R 0:0 rootfs
[ -z "$suid" ] || chmod u+s $suid
[ -z "$sgid" ] || chmod g+s $sgid

# mkext FILE SIZE: rootfs/ から ext の FS を作る
mkext() {
  rm -f "$1"
  case "${FS:-ext4}" in
    ext4) mkfs.ext4 -q -F -O ^dir_index -E root_owner=0:0 -d rootfs "$1" "$2" ;;
    ext2) mkfs.ext2 -q -F -b 4096 -O ^dir_index,^resize_inode,^ext_attr -E root_owner=0:0 -d rootfs "$1" "$2" ;;
    *) echo "FS は ext4 か ext2" >&2; exit 1 ;;
  esac
}

case "$layout" in
  gpt | ext4) ;;
  *) echo "LAYOUT は gpt か ext4" >&2; exit 1 ;;
esac
tmp=$(mktemp -d)
# /boot の中身は FAT の区画へ。root の /boot はマウント先の空のディレクトリ
restore() {
  if [ -d "$tmp/boot" ]; then
    rm -rf rootfs/boot
    mv "$tmp/boot" rootfs/boot
  fi
  rm -rf "$tmp"
}
trap restore EXIT
if [ "$layout" = gpt ] || [ -n "$NOBOOT" ]; then
  mkdir -p rootfs/boot
  mv rootfs/boot "$tmp/boot"
  mkdir rootfs/boot
fi

if [ "$layout" = ext4 ]; then
  out=${OUT:-disk.img}
  mkext "$out" "${1:-1G}"
  echo "$out (${FS:-ext4}) ready"
  exit
fi

out=disk.img
size=${1:-1G}
esp=262144        # 128 MiB (セクタ)
rm -f "$out"
truncate -s "$size" "$out"
sfdisk -q "$out" <<EOF
label: gpt
start=2048, size=$esp, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="ESP"
start=$((2048 + esp)), type=B921B045-1DF0-41C3-AF44-4C6F280D3FAE, name="root"
EOF
# root: 区画 2 の大きさ (sfdisk -d の出力から)
rsize=$(sfdisk -d "$out" | sed -n 's/.*start= *\([0-9]*\), size= *\([0-9]*\), type=B921B045.*/\2/p')
mkext "$tmp/root.img" "$((rsize / 2))k"
dd if="$tmp/root.img" of="$out" bs=512 seek=$((2048 + esp)) conv=notrunc 2>/dev/null
rm -f "$tmp/root.img"

# ESP
truncate -s $((esp * 512)) "$tmp/esp.img"
mkfs.vfat -F 32 -n ESP "$tmp/esp.img" >/dev/null
if [ -f "$tmp/boot/Image" ] && [ ! -f "$tmp/boot/EFI/BOOT/BOOTAA64.EFI" ]; then
  mkdir -p "$tmp/boot/EFI/BOOT"
  cp "$tmp/boot/Image" "$tmp/boot/EFI/BOOT/BOOTAA64.EFI"
fi
if [ -n "$(ls -A "$tmp/boot")" ]; then
  (cd "$tmp/boot" && mcopy -s -i "$tmp/esp.img" ./* ::/)
fi
dd if="$tmp/esp.img" of="$out" bs=512 seek=2048 conv=notrunc 2>/dev/null
echo "$out ready (GPT: p1 ESP $((esp / 2048)) MiB = /boot, p2 root ${FS:-ext4} $((rsize / 2048)) MiB)"
