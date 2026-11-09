#!/bin/sh
cd "$(dirname "$0")/.."
PATH=$HOME/.local/bin:$PATH
signers="$GPG_SIG"
signed() {
  st=$(env ${1:+"GNUPGHOME=$1"} git -C "$src" log -1 --format=%G?)
  fpr=$(env ${1:+"GNUPGHOME=$1"} git -C "$src" log -1 --format=%GP)
  case $st in G | U) ;; *) fpr= ;; esac
  case " $signers " in *" ${fpr:-none} "*) return 0 ;; esac
  return 1
}
fetch() {
  src=$(mktemp -d "${TMPDIR:-/tmp}/aigpt-mcp-$1.XXXXXX") || return 1
  git clone -q --depth 1 --filter=blob:none --no-checkout "https://git.syui.ai/ai/$1" "$src" || { rm -rf "$src"; return 1; }
  signed && return 0
  if [ "$st" = E ] && keys=$(mktemp -d "${TMPDIR:-/tmp}/aigpt-mcp-keys.XXXXXX"); then
    curl -fsSL https://git.syui.ai/ai.syui.ai.gpg | gpg --homedir "$keys" --batch -q --import > /dev/null 2>&1
    signed "$keys"
    r=$?
    gpgconf --homedir "$keys" --kill all > /dev/null 2>&1
    rm -rf "$keys"
    [ $r = 0 ] && return 0
  fi
  echo "aigpt-mcp.sh: ai/$1 $(git -C "$src" rev-parse --short HEAD) is not signed by ai (%G? = $st${fpr:+, $fpr}; signers: $signers)" >&2
  rm -rf "$src"
  return 1
}
install() {
  host=$(rustc -vV | sed -n 's/^host: //p')
  fetch "$1" || return 1
  git -C "$src" reset -q --hard &&
    cargo install -q --locked --path "$src" --root "$HOME/.local" --target "$host"
  r=$?
  rm -rf "$src"
  return $r
}
if [ "$1" = --build ]; then
  command -v aigpt > /dev/null || install gpt >&2 || exit 1
  cfg=${XDG_CONFIG_HOME:-$HOME/.config}/ai.syui.log
  if [ ! -f "$cfg/config.json" ]; then
    fetch log || exit 1
    mkdir -p "$cfg" &&
      git -C "$src" show HEAD:public/config.json > "$cfg/config.json.part" &&
      mv "$cfg/config.json.part" "$cfg/config.json"
    r=$?
    rm -rf "$src"
    [ $r = 0 ] || exit 1
  fi
  aigpt read-core > /dev/null 2>&1 || aigpt pull >&2 || exit 1
  if ! command -v ailog > /dev/null && ! pgrep -f 'aigpt-mcp-log\.' > /dev/null; then
    (trap '' HUP; install log && echo "ailog: ok") > /tmp/ailog-install.log 2>&1 &
  fi
  exit
fi
command -v aigpt > /dev/null || install gpt >&2 || exit 1
exec aigpt server
