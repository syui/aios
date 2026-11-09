#!/bin/sh
set -e
cd "$(dirname "$0")/.."
[ -d rootfs ] || { echo "rootfs/ がありません。先に bin/mkrootfs.sh を実行してください" >&2; exit 1; }
k=$1
[ -n "$k" ] || [ -f rootfs/boot/Image ] || k=target/aarch64-unknown-none-softfloat/debug/aios
out=sd.img
total=2097152
p1=2048
p1n=262144
p2=$((p1 + p1n))
p2n=$((total - p2))

le32() {
  v=$1
  for _ in 1 2 3 4; do
    printf "\\$(printf %03o $((v & 255)))"
    v=$((v >> 8))
  done
}
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

NOBOOT=1 OUT=build/sd-root.img sh bin/mkdisk.sh "$((p2n / 2))k" >/dev/null
dd if=build/sd-root.img of="$out" bs=512 seek=$p2 conv=notrunc 2>/dev/null
rm -f build/sd-root.img

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
arm_64bit=1
kernel=Image
enable_uart=1
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
