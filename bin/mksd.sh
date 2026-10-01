#!/bin/sh
# ラズパイ用の SD カードのイメージ sd.img を作る (1 GiB。QEMU の SD は 2 のべき乗の大きさ)
#   区画 1: FAT32 (0x0c) の boot  128 MiB  rootfs/boot の中身 (カーネル (unix パッケージ) の Image)、config.txt、DTB
#   区画 2: Linux (0x83) の root  残り     rootfs/ から ext4
#   bin/mksd.sh [KERNEL]    KERNEL は ELF か Image (既定は rootfs/boot/Image、なければ開発用カーネル)
# aios は区画 1 を /boot にマウントするので、aipkg でカーネル (unix) を上げるとファームウェアが
# 読む Image (config.txt の kernel=Image) も新しくなる
# 本物のラズパイ 3 で動かすなら、boot にファームウェア (bootcode.bin start.elf fixup.dat)
# を足す: FIRMWARE=1 bin/mksd.sh (raspberrypi/firmware から取ってくる)。FAT を作るのに
# mkfs.vfat と mcopy (dosfstools, mtools) が要る。なければ boot は空のまま
# QEMU では: AIOS_MACHINE=raspi3b bin/run.sh (sd.img があればつなぐ)
set -e
cd "$(dirname "$0")/.."
[ -d rootfs ] || { echo "rootfs/ がありません。先に bin/mkrootfs.sh を実行してください" >&2; exit 1; }
k=$1
[ -n "$k" ] || [ -f rootfs/boot/Image ] || k=target/aarch64-unknown-none-softfloat/debug/aios
out=sd.img
total=2097152        # 1 GiB (セクタ)
p1=2048
p1n=262144           # 128 MiB
p2=$((p1 + p1n))
p2n=$((total - p2))

# le32 N: 4 バイトの little endian
le32() {
  v=$1
  for _ in 1 2 3 4; do
    printf "\\$(printf %03o $((v & 255)))"
    v=$((v >> 8))
  done
}
# entry TYPE START COUNT: MBR の区画の 16 バイト (CHS は使わない印)
entry() {
  printf '\000\376\377\377'
  printf "\\$(printf %03o "$1")"
  printf '\376\377\377'
  le32 "$2"
  le32 "$3"
}

rm -f "$out"
dd if=/dev/zero of="$out" bs=512 count=0 seek=$total 2>/dev/null
{
  dd if=/dev/zero bs=446 count=1 2>/dev/null
  entry 12 $p1 $p1n
  entry 131 $p2 $p2n
  dd if=/dev/zero bs=32 count=1 2>/dev/null
  printf '\125\252'
} | dd of="$out" conv=notrunc 2>/dev/null

# root
NOBOOT=1 OUT=build/sd-root.img sh bin/mkdisk.sh "$((p2n / 2))k" >/dev/null
dd if=build/sd-root.img of="$out" bs=512 seek=$p2 conv=notrunc 2>/dev/null
rm -f build/sd-root.img

# boot
if command -v mkfs.vfat >/dev/null && command -v mcopy >/dev/null; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  objcopy=$(command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy)
  cp -r rootfs/boot/. "$tmp/"
  if [ -z "$k" ]; then
    :
  elif [ "$(head -c 4 "$k" | od -An -c | tr -d ' ')" = '177ELF' ]; then
    "$objcopy" -O binary "$k" "$tmp/Image"
  else
    cp "$k" "$tmp/Image"
  fi
  cat > "$tmp/config.txt" <<'CFG'
# aios
arm_64bit=1
kernel=Image
enable_uart=1
# PL011 (aios のコンソール) を GPIO 14/15 に (Bluetooth には mini UART)
dtoverlay=disable-bt
CFG
  printf 'console=ttyAMA0 root=/dev/mmcblk0p2\n' > "$tmp/cmdline.txt"
  if [ -n "$FIRMWARE" ]; then
    fw=https://raw.githubusercontent.com/raspberrypi/firmware/master/boot
    for f in bootcode.bin start.elf fixup.dat bcm2710-rpi-3-b.dtb bcm2710-rpi-3-b-plus.dtb overlays/disable-bt.dtbo; do
      mkdir -p "$tmp/$(dirname "$f")"
      curl -fsSL -o "$tmp/$f" "$fw/$f"
    done
  fi
  dd if=/dev/zero of="$tmp/boot.img" bs=512 count=0 seek=$p1n 2>/dev/null
  mkfs.vfat -F 32 -n BOOT "$tmp/boot.img" >/dev/null
  (cd "$tmp" && mcopy -s -i boot.img $(ls | grep -v '^boot.img$') ::/)
  dd if="$tmp/boot.img" of="$out" bs=512 seek=$p1 conv=notrunc 2>/dev/null
else
  echo "mkfs.vfat / mcopy がないので boot の区画は空です" >&2
fi
echo "$out ready (p1 boot $((p1n / 2048)) MiB, p2 root $((p2n / 2048)) MiB)"
