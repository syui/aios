#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   bin/run.sh KERNEL   そのカーネルで (cargo run は作ったばかりの開発用カーネルを渡す)
#   bin/run.sh          disk.img に /boot/aios (aios-kernel パッケージ) があればそれで、
#                       なければ開発用カーネルで。ブートローダーのかわり
# ネットワークは QEMU の user (10.0.2.15、ホストは 10.0.2.2)
# disk.img (bin/mkdisk.sh で作る) があれば virtio-blk としてつなぎ、ルートにする
dev=target/aarch64-unknown-none-softfloat/debug/aios
k=$1
case "$k" in "" | /*) ;; *) k="$PWD/$k" ;; esac
cd "$(dirname "$0")/.."
if [ -z "$k" ] && [ -f disk.img ] && command -v debugfs >/dev/null; then
  boot=$(mktemp)
  trap 'rm -f "$boot"' EXIT
  if debugfs -R "dump /boot/aios $boot" disk.img >/dev/null 2>&1 && [ -s "$boot" ]; then
    echo "boot: /boot/aios from disk.img" >&2
    k=$boot
  fi
fi
[ -n "$k" ] && [ -f "$k" ] || k=$dev
set -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f disk.img ]; then
  set -- "$@" \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 512M -nographic "$@" -kernel "$k"
