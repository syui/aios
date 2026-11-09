#!/bin/sh
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
if [ "$1" = --build ]; then
  build >&2
  exit
fi
if [ -x "user/target/$host/release/aish" ] && [ -d "shell/target/$host/release" ]; then
  (build > /dev/null 2>&1 &)
else
  build >&2 || exit 1
fi
AISH_SRC=$PWD AISH_PLUGIN_PATH=$PWD/shell/target/$host/release \
  exec "user/target/$host/release/aish" --mcp shell/mcp.rc
