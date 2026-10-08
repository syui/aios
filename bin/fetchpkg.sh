#!/bin/sh
# ai/repo のパッケージを repo/aarch64/KIND に取ってくる (チェックサムを確かめる。もうあるものはとばす)
#   bin/fetchpkg.sh KIND NAME...
# ソースからビルドせずに、ビルドに要るほかのパッケージ (c_deps の gtk3 など) をそろえるため。CI の firefox で使う
set -e
cd "$(dirname "$0")/.."
[ $# -ge 2 ] || { sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }
kind=$1
shift
top=https://git.syui.ai/ai/repo/raw/branch/main/aarch64
repo=repo/aarch64/$kind
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 "$top/$kind/aios.db" -o "$tmp/aios.db"
mkdir "$tmp/db"
tar -xzf "$tmp/aios.db" -C "$tmp/db"
mkdir -p "$repo"
for name in "$@"; do
  d=
  for x in "$tmp/db/$name"-[0-9]*; do
    [ -f "$x/desc" ] || continue
    [ "$(sed -n '/^%NAME%$/{n;p;}' "$x/desc")" = "$name" ] && d=$x
  done
  [ -n "$d" ] || { echo "fetchpkg: $name is not in ai/repo ($kind)" >&2; exit 1; }
  file=$(sed -n '/^%FILENAME%$/{n;p;}' "$d/desc")
  sum=$(sed -n '/^%SHA256SUM%$/{n;p;}' "$d/desc")
  if [ ! -f "$repo/$file" ]; then
    echo "fetchpkg: $file"
    curl -fsSL --retry 3 "$top/$kind/$file" -o "$repo/$file.part"
    mv "$repo/$file.part" "$repo/$file"
  fi
  if [ -n "$sum" ] && [ "$(sha256sum "$repo/$file" | cut -d' ' -f1)" != "$sum" ]; then
    echo "fetchpkg: $file: checksum mismatch" >&2
    rm -f "$repo/$file"
    exit 1
  fi
done
