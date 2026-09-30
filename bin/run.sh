#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   bin/run.sh KERNEL   そのカーネルで (cargo run は作ったばかりの開発用カーネルを渡す)
#   bin/run.sh          disk.img に /boot/Image (aios-kernel パッケージ) があればそれで、
#                       なければ開発用カーネルで。ブートローダーのかわり
# カーネルは Linux の arm64 Image として渡すので、QEMU は DTB を x0 に入れてくれる
# (ELF なら Image に変える)。AIOS_CMDLINE はカーネルのコマンドライン (例: init=/bin/sh)
# ネットワークは QEMU の user (DHCP)。disk.img (bin/mkdisk.sh で作る) があればルートにする
# AIOS_MACHINE=raspi3b でラズパイ 3B (DTB は build/rpi/ に取ってくる。ルートは initramfs、ネットワークなし)
dev=target/aarch64-unknown-none-softfloat/debug/aios
k=$1
case "$k" in "" | /*) ;; *) k="$PWD/$k" ;; esac
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -z "$k" ] && [ -f disk.img ] && command -v debugfs >/dev/null; then
  for f in /boot/Image /boot/aios; do
    if debugfs -R "dump $f $tmp/boot" disk.img >/dev/null 2>&1 && [ -s "$tmp/boot" ]; then
      echo "boot: $f from disk.img" >&2
      k=$tmp/boot
      break
    fi
  done
fi
[ -n "$k" ] && [ -f "$k" ] || k=$dev
# ELF なら中身だけの Image に
if [ "$(head -c 4 "$k" | od -An -c | tr -d ' ')" = '177ELF' ]; then
  objcopy=$(command -v llvm-objcopy || command -v rust-objcopy || command -v aarch64-linux-gnu-objcopy)
  if [ -n "$objcopy" ] && "$objcopy" -O binary "$k" "$tmp/Image"; then
    k=$tmp/Image
  fi
fi
if [ "${AIOS_MACHINE:-virt}" = raspi3b ]; then
  dtb=build/rpi/bcm2710-rpi-3-b.dtb
  if [ ! -f "$dtb" ]; then
    mkdir -p build/rpi
    curl -fsSL -o "$dtb" https://raw.githubusercontent.com/raspberrypi/firmware/master/boot/bcm2710-rpi-3-b.dtb || exit 1
  fi
  qemu-system-aarch64 -M raspi3b -serial stdio -display none -dtb "$dtb" -kernel "$k" -append "${AIOS_CMDLINE:-}"
  exit
fi
set -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f disk.img ]; then
  set -- "$@" \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m "${AIOS_MEM:-512M}" -nographic "$@" \
  -kernel "$k" -append "${AIOS_CMDLINE:-}"
