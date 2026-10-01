#!/bin/sh
# リリース (unix-latest) の disk.img を QEMU で動かす。リリースでは run.sh という名前で入っている
#   brew install qemu   (Linux なら qemu-system-arm)
#   zstd -d aios-unix-aarch64.img.zst -o disk.img
#   ./run.sh             同じディレクトリの disk.img と Image で起動 (ext4 の /、FAT の /boot、スワップ)
#   AIOS_MEM=128M ./run.sh でメモリ、AIOS_SMP=2 で CPU の数、AIOS_CMDLINE=init=/bin/sh でレスキュー
# Image は disk.img の /boot/Image と同じ aios-kernel。aipkg -Syu でカーネルを上げたら、
# リポジトリの bin/run.sh (mtools で /boot/Image を取りだす) を使うか、Image を取りなおす
# 終わるのは Ctrl-a x (または aios の中で sudo poweroff)。Ctrl-P でプロセスの一覧
cd "$(dirname "$0")" || exit 1
[ -f disk.img ] || { echo "disk.img がありません (zstd -d aios-unix-aarch64.img.zst -o disk.img)" >&2; exit 1; }
exec qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-512M}" -nographic \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false \
  -drive file=disk.img,if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0 \
  -kernel Image -append "${AIOS_CMDLINE:-}"
