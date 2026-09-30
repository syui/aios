#!/bin/sh
# sudo-rs を aarch64 musl の静的バイナリとしてビルドする
# aios には PAM ライブラリがないので、pkg/sudo-rs/aios_pam.rs (shadow を見る小さな PAM) を中に入れる
set -e
cd "$(dirname "$0")/.."
src=build/sudo-rs
[ -d "$src" ] || git clone --depth 1 https://github.com/trifectatechfoundation/sudo-rs "$src"
cp pkg/sudo-rs/aios_pam.rs "$src/src/pam/aios_pam.rs"
mod="$src/src/pam/mod.rs"
if ! grep -q "mod aios_pam;" "$mod"; then
  # libpam にリンクする代わりに aios_pam を使う
  sed -i 's|^#\[link(name = "pam")\]$|mod aios_pam;|; /^mod aios_pam;$/{n; s|^unsafe extern "C" {}$||}' "$mod"
fi
grep -q "mod aios_pam;" "$mod" || { echo "patch failed: $mod" >&2; exit 1; }
cd "$src"
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo build --release --target aarch64-unknown-linux-musl --bin sudo --bin visudo
