#!/bin/sh
cd "$(dirname "$0")/.."
PATH=$HOME/.local/bin:$PATH
install() {
  host=$(rustc -vV | sed -n 's/^host: //p')
  cargo install -q --locked --git "https://git.syui.ai/ai/$1" "$2" --root "$HOME/.local" --target "$host"
}
if [ "$1" = --build ]; then
  command -v aigpt > /dev/null || install gpt aigpt >&2 || exit 1
  cfg=${XDG_CONFIG_HOME:-$HOME/.config}/ai.syui.log
  if [ ! -f "$cfg/config.json" ]; then
    mkdir -p "$cfg" &&
      curl -fsSL https://git.syui.ai/ai/log/raw/branch/main/public/config.json -o "$cfg/config.json.part" &&
      mv "$cfg/config.json.part" "$cfg/config.json" || exit 1
  fi
  aigpt read-core > /dev/null 2>&1 || aigpt pull >&2 || exit 1
  if ! command -v ailog > /dev/null && ! pgrep -f 'cargo install .*ai/log ailog' > /dev/null; then
    (trap '' HUP; install log ailog && echo "ailog: ok") > /tmp/ailog-install.log 2>&1 &
  fi
  exit
fi
command -v aigpt > /dev/null || install gpt aigpt >&2 || exit 1
exec aigpt server
