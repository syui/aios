#!/bin/sh
# uutils/grep を aarch64 musl の静的バイナリとしてビルドする
# 正規表現は oniguruma (C)。glibc 用のクロスコンパイラで作るので、
# musl にない __*_chk を呼ばないよう FORTIFY を切る
set -e
cd "$(dirname "$0")/.."
src=build/grep
[ -d "$src" ] || git clone --depth 1 https://github.com/uutils/grep "$src"
cd "$src"
CFLAGS_aarch64_unknown_linux_musl="-U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0" \
CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo build --release --target aarch64-unknown-linux-musl
