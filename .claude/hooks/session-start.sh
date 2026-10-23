#!/bin/bash
# クラウドのセッションが始まるときに: aios のビルドと aish-pkg に要るものを入れ、aish を先にビルドしておく
#   zstd (bin/mkpkg.sh)、qemu-user-static (pkg_test と meson の exe_wrapper)、meson (gtk などの C のパッケージ)、
#   libwayland-bin (gtk3 の wayland-scanner)、gcc-aarch64-linux-gnu (musl の libc.so)、
#   rustup の target (カーネルと aios のコマンドと aiboot)。aish は bin/aish-mcp.sh --build (MCP が 30 秒で起きるように)
# そのあと、うしろで bin/setup-build.sh (clang-20、glslang、mtools など、残りのビルドの道具。/tmp/setup-build.log)
# と、aios.img がなければリリース latest のディスク (CI が main への push ごとに作って起動をたしかめたもの) を
# 取ってきて展開する (/tmp/aios-img.log)。aish の vm (kernel: "." でも) がすぐ使えるように
# 入っているものはとばす
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
# 浅いクローンだと pkgver() (git rev-list --count) が小さくなり、パッケージが古い版に見える
[ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = true ] && git fetch -q --unshallow origin || true
bin/aish-mcp.sh --build
# 残りの道具 (大きいのでうしろで。終わりは /tmp/setup-build.log の setup-build: ok)
nohup bin/setup-build.sh > /tmp/setup-build.log 2>&1 &
# とちゅうのものを aios.img と思わないように、.part に展開してから名前を変える。GitHub が新しいので先に
if [ ! -f aios.img ]; then
  nohup sh -c 'for u in https://github.com/syui/aios/releases/download/latest/aios-aarch64.img.zst \
      https://git.syui.ai/ai/os/releases/download/latest/aios-aarch64.img.zst; do
    curl -fsSL "$u" | zstd -d -q -f --sparse -o aios.img.part && mv aios.img.part aios.img && echo "aios.img: ok ($u)" && exit 0
    rm -f aios.img.part
  done; echo "aios.img: failed"; exit 1' > /tmp/aios-img.log 2>&1 &
fi
