#!/bin/sh
# Claude Code から aish を使う (.mcp.json)。aish --mcp を、このリポジトリのプラグインといっしょに起こす
#   aios の中: 入っている aish (/usr/bin/aish) とプラグイン (/usr/lib/aish/plugins) をそのまま
#   Linux (開発のマシン): このリポジトリの aish とプラグインをこのマシン向けにビルドして (済んでいればすぐ終わる)
# 標準出力は MCP が使うので、ビルドの出力は標準エラーへ
cd "$(dirname "$0")/.."
if grep -qs '^ID=aios$' /etc/os-release; then
  exec aish --mcp
fi
if [ "$(uname -s)" != Linux ]; then
  echo "aish-mcp.sh: aish は Linux (と aios) で動く (memfd などを使う)" >&2
  exit 1
fi
host=$(rustc -vV | sed -n 's/^host: //p')
build() {
  (cd user && cargo build -q --release --bin aish --target "$host") &&
    (cd shell && cargo build -q --release --target "$host")
}
# bin/aish-mcp.sh --build: ビルドだけ (クラウドの環境のセットアップスクリプトで先に作っておく。
#   新しいコンテナで一からビルドすると 30 秒では終わらず、MCP がつながらない)
if [ "$1" = --build ]; then
  build >&2
  exit
fi
# Claude Code は MCP のサーバーを 30 秒しか待たない。ソースを変えたあとのビルドはそれより長いことがあるので、
# できているものがあればすぐそれで起こし、ビルドはうしろでする (新しいものは次に起こしたときから)
if [ -x "user/target/$host/release/aish" ] && [ -d "shell/target/$host/release" ]; then
  (build > /dev/null 2>&1 &)
else
  build >&2 || exit 1
fi
AISH_PLUGIN_PATH=$PWD/shell/target/$host/release \
  exec "user/target/$host/release/aish" --mcp shell/mcp.rc
