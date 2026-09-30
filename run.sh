#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   ./run.sh [kernel]
# ネットワークは QEMU の user (10.0.2.15、ホストは 10.0.2.2)
# disk.img (./mkdisk.sh で作る) があれば virtio-blk としてつなぎ、ルートにする
k=${1:-target/aarch64-unknown-none-softfloat/debug/aios}
set -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f disk.img ]; then
  set -- "$@" \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 512M -nographic "$@" -kernel "$k"
