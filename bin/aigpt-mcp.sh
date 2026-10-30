#!/bin/sh
# Claude Code から aigpt (アイの記憶: core・memory・feed・os) を使う (.mcp.json)。aigpt server を起こす
#   入っている aigpt (aios の /usr/bin/aigpt、Mac など) があれば、それをそのまま
#   なければ (クラウドのセッション): git.syui.ai の ai/gpt をこのマシン向けにビルドして ~/.local/bin に入れる
# 標準出力は MCP が使うので、ビルドの出力は標準エラーへ
cd "$(dirname "$0")/.."
PATH=$HOME/.local/bin:$PATH
# このリポジトリの .cargo/config.toml はカーネル向け (aarch64-unknown-none-softfloat) なので、--target でこのマシン向けに
install() {
  host=$(rustc -vV | sed -n 's/^host: //p')
  cargo install -q --locked --git "https://git.syui.ai/ai/$1" "$2" --root "$HOME/.local" --target "$host"
}
# bin/aigpt-mcp.sh --build: クラウドの新しいコンテナで (.claude/hooks/session-start.sh)。aigpt を入れ、
#   config.json (アイの did・handle・PDS。ailog と同じ $cfg/ai.syui.log/) を ai/log から置き、記憶を PDS から取ってくる。
#   PDS へ書く (aigpt sync) ための ailog も入れる。ailog のログインは OAuth の 2 段で、syui がブラウザで認可する
#   (ailog oauth ai.syui.ai --bot -s syu.is --url、そのあと --code)
if [ "$1" = --build ]; then
  command -v aigpt > /dev/null || install gpt aigpt >&2 || exit 1
  cfg=${XDG_CONFIG_HOME:-$HOME/.config}/ai.syui.log
  # とちゅうで切れたものを config.json と思わないように、.part に取ってから名前を変える
  if [ ! -f "$cfg/config.json" ]; then
    mkdir -p "$cfg" &&
      curl -fsSL https://git.syui.ai/ai/log/raw/branch/main/public/config.json -o "$cfg/config.json.part" &&
      mv "$cfg/config.json.part" "$cfg/config.json" || exit 1
  fi
  # 記憶がまだないとき (新しいコンテナ) だけ。ローカルで消して sync していないものを、PDS から戻さないように
  aigpt read-core > /dev/null 2>&1 || aigpt pull >&2 || exit 1
  # ailog は大きい (1 分ほど) ので、うしろで (nohup と同じく HUP を無視。終わりは /tmp/ailog-install.log)。
  # SessionStart は resume や compact でも来るので、ビルドの途中ならもう 1 つは始めない
  if ! command -v ailog > /dev/null && ! pgrep -f 'cargo install .*ai/log ailog' > /dev/null; then
    (trap '' HUP; install log ailog && echo "ailog: ok") > /tmp/ailog-install.log 2>&1 &
  fi
  exit
fi
command -v aigpt > /dev/null || install gpt aigpt >&2 || exit 1
exec aigpt server
