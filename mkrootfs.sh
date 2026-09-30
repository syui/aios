#!/bin/sh
# user/ をビルドして rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
set -e
cd "$(dirname "$0")"

(cd user && cargo build --release)
bin=user/target/aarch64-unknown-linux-musl/release

rm -rf rootfs
mkdir -p rootfs/bin
cp "$bin/init" rootfs/init
cp "$bin/hello" rootfs/bin/hello
