#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   ./run.sh [kernel]
# disk.img (./mkdisk.sh で作る) があれば virtio-blk としてつなぎ、ルートにする
k=${1:-target/aarch64-unknown-none-softfloat/debug/aios}
set --
if [ -f disk.img ]; then
  set -- -global virtio-mmio.force-legacy=false \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 512M -nographic "$@" -kernel "$k"
