#!/bin/sh
# uutils/coreutils を aarch64 musl の静的バイナリとしてビルドする
# 必要なもの: rustup target aarch64-unknown-linux-musl, aarch64-linux-gnu-gcc (blake3 の C 部分)
set -e
cd "$(dirname "$0")/.."
src=build/coreutils
[ -d "$src" ] || git clone --depth 1 https://github.com/uutils/coreutils "$src"
cd "$src"
CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo build --release --target aarch64-unknown-linux-musl --no-default-features --features feat_os_unix_musl
