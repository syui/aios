#!/bin/bash
# クラウドのセッションが始まるときに: aios のビルドと aish-pkg に要るものを入れ、aish を先にビルドしておく
#   zstd (bin/mkpkg.sh)、qemu-user-static (pkg_test と meson の exe_wrapper)、meson (gtk などの C のパッケージ)、
#   rustup の target (カーネルと aios のコマンド)。aish は bin/aish-mcp.sh --build (MCP が 30 秒で起きるように)
# 入っているものはとばす
set -euo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" = true ] || exit 0
cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"

need=()
command -v zstd >/dev/null || need+=(zstd)
command -v qemu-aarch64-static >/dev/null || need+=(qemu-user-static)
if [ ${#need[@]} -gt 0 ]; then
  apt-get install -y -q "${need[@]}" >/dev/null 2>&1 || { apt-get update -q >/dev/null 2>&1; apt-get install -y -q "${need[@]}" >/dev/null; }
fi
command -v meson >/dev/null || pip install -q --break-system-packages meson >/dev/null 2>&1
rustup target add aarch64-unknown-none-softfloat aarch64-unknown-linux-musl >/dev/null 2>&1 || true
bin/aish-mcp.sh --build
