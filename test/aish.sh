#!/bin/sh
# aish のテスト: 書き方 (test/aish-syntax.txt)、sh の名前での bash の書き方 (test/aish-bash.txt) と MCP (test/aish-mcp.py)
#   test/aish.sh [AISH [PLUGINS]]   既定はこのマシン向けにビルドしたもの (bin/aish-mcp.sh --build)
#   CI (arm64 の Linux) は aios 向けのもの (user/target/aarch64-unknown-linux-musl/release/aish) をそのまま動かす
cd "$(dirname "$0")/.."
host=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
aish=${1:-user/target/$host/release/aish}
plugins=${2:-shell/target/$host/release}
[ -x "$aish" ] || { echo "aish.sh: $aish がない (bin/aish-mcp.sh --build)" >&2; exit 2; }
aish=$(cd "$(dirname "$aish")" && pwd)/$(basename "$aish")
plugins=$(cd "$plugins" && pwd)
fail=0
n=0
tab=$(printf '\t')
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
while IFS= read -r line; do
  case $line in '#'*|'') continue ;; esac
  cmd=${line%%"$tab"*}
  want=${line#*"$tab"}
  n=$((n + 1))
  got=$(cd "$tmp" && "$aish" -c "$cmd" 2>&1 | tr '\n' '|' | sed 's/|$//')
  if [ "$got" != "$want" ]; then
    fail=$((fail + 1))
    printf 'FAIL %s\n  want: %s\n  got:  %s\n' "$cmd" "$want" "$got"
  fi
done < test/aish-syntax.txt
echo "syntax: $n cases, $fail failed"
# sh の名前で (bash の書き方): test/aish-bash.txt
ln -s "$aish" "$tmp/sh"
nb=0
fb=0
while IFS= read -r line; do
  case $line in '#'*|'') continue ;; esac
  cmd=${line%%"$tab"*}
  want=${line#*"$tab"}
  nb=$((nb + 1))
  got=$(cd /tmp && "$tmp/sh" -c "$cmd" 2>&1 | tr '\n' '|' | sed 's/|$//')
  if [ "$got" != "$want" ]; then
    fb=$((fb + 1))
    printf 'FAIL (sh) %s\n  want: %s\n  got:  %s\n' "$cmd" "$want" "$got"
  fi
done < test/aish-bash.txt
echo "bash: $nb cases, $fb failed"
fail=$((fail + fb))
python3 test/aish-mcp.py "$aish" "$plugins" || fail=$((fail + 1))
[ "$fail" = 0 ]
