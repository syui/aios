#!/bin/bash
set -euo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" = true ] || exit 0
cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"

need=()
command -v zstd >/dev/null || need+=(zstd)
command -v qemu-aarch64-static >/dev/null || need+=(qemu-user-static)
command -v wayland-scanner >/dev/null || need+=(libwayland-bin)
command -v aarch64-linux-gnu-gcc >/dev/null || need+=(gcc-aarch64-linux-gnu)
if [ ${#need[@]} -gt 0 ]; then
  apt-get install -y -q "${need[@]}" >/dev/null 2>&1 || { apt-get update -q >/dev/null 2>&1; apt-get install -y -q "${need[@]}" >/dev/null; }
fi
command -v meson >/dev/null || pip install -q --break-system-packages meson >/dev/null 2>&1
rustup target add aarch64-unknown-none-softfloat aarch64-unknown-linux-musl aarch64-unknown-uefi >/dev/null 2>&1 || true
[ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = true ] && git fetch -q --unshallow origin || true
bin/gitea.sh id || true
bin/aish-mcp.sh --build
bin/aigpt-mcp.sh --build || echo "aigpt-mcp.sh --build: failed" >&2
nohup bin/setup-build.sh > /tmp/setup-build.log 2>&1 &
if [ ! -f aios.img ]; then
  nohup sh -c 'for u in https://github.com/syui/aios/releases/download/latest/aios-aarch64.img.zst \
      https://git.syui.ai/ai/os/releases/download/latest/aios-aarch64.img.zst; do
    curl -fsSL "$u" | zstd -d -q -f --sparse -o aios.img.part && mv aios.img.part aios.img && echo "aios.img: ok ($u)" && exit 0
    rm -f aios.img.part
  done; echo "aios.img: failed"; exit 1' > /tmp/aios-img.log 2>&1 &
fi
