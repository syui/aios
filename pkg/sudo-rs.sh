#!/bin/sh
# sudo-rs (sudo, visudo)
# aios には PAM ライブラリがないので、pkg/sudo-rs/aios_pam.rs (shadow を見る小さな PAM) を中に入れる
. "$(dirname "$0")/lib.sh"
pam=$PWD/pkg/sudo-rs/aios_pam.rs
fetch sudo-rs https://github.com/trifectatechfoundation/sudo-rs
cp "$pam" src/pam/aios_pam.rs
mod=src/pam/mod.rs
if ! grep -q "mod aios_pam;" "$mod"; then
  # libpam にリンクする代わりに aios_pam を使う
  sed -i 's|^#\[link(name = "pam")\]$|mod aios_pam;|; /^mod aios_pam;$/{n; s|^unsafe extern "C" {}$||}' "$mod"
fi
grep -q "mod aios_pam;" "$mod" || { echo "patch failed: $mod" >&2; exit 1; }
musl_build --bin sudo --bin visudo
