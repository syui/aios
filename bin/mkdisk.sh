#!/bin/sh
# rootfs/ からディスクイメージ disk.img を作る
#   bin/mkdisk.sh [size]            GPT (既定 1G):
#                                     区画 1: EFI System Partition (FAT32, 128 MiB) = /boot
#                                     区画 2: Linux root (arm64) の ext4 = /
#   LAYOUT=ext4 bin/mkdisk.sh       区画なしで、ディスク全体を ext4 に (/boot も ext4 の中)
#   FS=ext2 ...                     root を ext2 に
#   OUT=FILE で出力先を変える (bin/mksd.sh が root の区画を作るのに使う。いつも区画なし)
#   NOBOOT=1 で root の /boot を空に (boot は別の FAT の区画に入れるとき)
#   SWAP=256M で区画 3 にスワップ (GPT のとき。mkswap が要る)。/etc/fstab に足すので起動すると swapon -a で使う
# ESP には rootfs/boot の中身 (カーネル (unix パッケージ) の Image と loader entry、aiboot) を入れる。
# UEFI のファームウェアは EFI/BOOT/BOOTAA64.EFI (aiboot) を起動し、aiboot が entry の Image を起動する。
# aios は起動後に ESP を /boot にマウントするので、aipkg でカーネルを入れかえられる
# GPT には sfdisk、FAT には mkfs.vfat と mcopy (dosfstools, mtools) が要る
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

# put FILE SECTOR: FILE をディスクの SECTOR から書く (区画は 1 MiB ごとなので 1 MiB ずつ)
put() {
  if [ $(($2 % 2048)) = 0 ]; then
    dd if="$1" of="$out" bs=1048576 seek=$(($2 / 2048)) conv=notrunc 2>/dev/null
  else
    dd if="$1" of="$out" bs=512 seek="$2" conv=notrunc 2>/dev/null
  fi
}

# mkext FILE SIZE: rootfs/ から ext の FS を作る
mkext() {
  rm -f "$1"
  case "${FS:-ext4}" in
    ext4) mkfs.ext4 -q -F -E root_owner=0:0 -d rootfs "$1" "$2" ;;
    ext2) mkfs.ext2 -q -F -b 4096 -O ^resize_inode,^ext_attr -E root_owner=0:0 -d rootfs "$1" "$2" ;;
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
  [ -f "$tmp/fstab" ] && cp -p "$tmp/fstab" rootfs/etc/fstab
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
if [ -n "$SWAP" ]; then
  command -v mkswap >/dev/null || { echo "mkswap がありません (util-linux)" >&2; exit 1; }
  swap=$(($(numfmt --from=iec "$SWAP") / 512 / 2048 * 2048))
  # GPT の後ろの写し (33 セクタ) の分をあけて、ディスクの終わりに置く
  total=$(($(stat -c %s "$out") / 512))
  sstart=$(((total - 34 - swap) / 2048 * 2048))
  rsize=$((sstart - 2048 - esp))
  sfdisk -q "$out" <<EOF
label: gpt
start=2048, size=$esp, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="ESP"
start=$((2048 + esp)), size=$rsize, type=B921B045-1DF0-41C3-AF44-4C6F280D3FAE, name="root"
start=$sstart, size=$swap, type=0657FD6D-A4AB-43C4-84E5-0933C84B4F4F, name="swap"
EOF
  truncate -s $((swap * 512)) "$tmp/swap.img"
  mkswap -q "$tmp/swap.img"
  put "$tmp/swap.img" $sstart
  rm -f "$tmp/swap.img"
  # ディスクの root にだけ書く (rootfs/ は initramfs にもなるので、あとで元にもどす)
  mkdir -p rootfs/etc
  touch rootfs/etc/fstab
  cp -p rootfs/etc/fstab "$tmp/fstab"
  grep -qs '^/dev/vda3[[:space:]]' rootfs/etc/fstab || echo '/dev/vda3   none   swap    defaults' >> rootfs/etc/fstab
else
  sfdisk -q "$out" <<EOF
label: gpt
start=2048, size=$esp, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="ESP"
start=$((2048 + esp)), type=B921B045-1DF0-41C3-AF44-4C6F280D3FAE, name="root"
EOF
fi
# root: 区画 2 の大きさ (sfdisk -d の出力から)
rsize=$(sfdisk -d "$out" | sed -n 's/.*start= *\([0-9]*\), size= *\([0-9]*\), type=B921B045.*/\2/p')
mkext "$tmp/root.img" "$((rsize / 2))k"
put "$tmp/root.img" $((2048 + esp))
rm -f "$tmp/root.img"

# ESP
truncate -s $((esp * 512)) "$tmp/esp.img"
mkfs.vfat -F 32 -n ESP "$tmp/esp.img" >/dev/null
# ブートローダー (aiboot パッケージ) がなければ、カーネルそのものを既定の場所に置く
# (起動はできるが、aipkg でカーネルを上げてもこの写しは古いまま)
if [ -f "$tmp/boot/Image" ] && [ ! -f "$tmp/boot/EFI/BOOT/BOOTAA64.EFI" ]; then
  echo "aiboot がないので EFI/BOOT/BOOTAA64.EFI は Image の写しです (bin/mkrootfs.sh unix aiboot ...)" >&2
  mkdir -p "$tmp/boot/EFI/BOOT"
  cp "$tmp/boot/Image" "$tmp/boot/EFI/BOOT/BOOTAA64.EFI"
fi
if [ -n "$(ls -A "$tmp/boot")" ]; then
  (cd "$tmp/boot" && mcopy -s -i "$tmp/esp.img" ./* ::/)
fi
put "$tmp/esp.img" 2048
echo "$out ready (GPT: p1 ESP $((esp / 2048)) MiB = /boot, p2 root ${FS:-ext4} $((rsize / 2048)) MiB${swap:+, p3 swap $((swap / 2048)) MiB})"
