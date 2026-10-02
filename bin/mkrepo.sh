#!/bin/bash
# repo-add の代わり: DIR の *.pkg.tar.zst から同期データベース DIR/aios.db を作りなおす
#   bin/mkrepo.sh [DIR...]    (既定は repo/aarch64/ の rust, c, shell, desktop)
# repo/aarch64 をそのまま git.syui.ai/ai/repo の aarch64/ に置けば、aipkg.conf の
# Server = https://git.syui.ai/ai/repo/raw/branch/main/$arch/rust (と .../$arch/c) で読める
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
[ $# -gt 0 ] || set -- "$root/repo/aarch64/rust" "$root/repo/aarch64/c" "$root/repo/aarch64/shell" "$root/repo/aarch64/desktop"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# section KEY VALUES: pacman の desc の 1 項目 (値は 1 行に 1 つ、空なら書かない)
section() {
  [ -n "$2" ] || return 0
  printf '%%%s%%\n%s\n\n' "$1" "$2"
}

# mkdb DIR: DIR/aios.db
mkdb() {
  dir=$(cd "$1" && pwd)
  rm -rf "${tmp:?}"/*
  n=0
  for f in "$dir"/*.pkg.tar.zst; do
    [ -e "$f" ] || continue
    info=$(zstd -dcq "$f" | tar -xOf - .PKGINFO)
    get() { printf '%s\n' "$info" | sed -n "s/^$1 = //p"; }
    name=$(get pkgname)
    ver=$(get pkgver)
    mkdir -p "$tmp/$name-$ver"
    {
      section FILENAME "${f##*/}"
      section NAME "$name"
      section BASE "$(get pkgbase)"
      section VERSION "$ver"
      section DESC "$(get pkgdesc)"
      section CSIZE "$(stat -c %s "$f")"
      section ISIZE "$(get size)"
      section SHA256SUM "$(sha256sum "$f" | cut -d' ' -f1)"
      section URL "$(get url)"
      section LICENSE "$(get license)"
      section ARCH "$(get arch)"
      section BUILDDATE "$(get builddate)"
      section PACKAGER "$(get packager)"
      section REPLACES "$(get replaces)"
      section CONFLICTS "$(get conflict)"
      section PROVIDES "$(get provides)"
      section DEPENDS "$(get depend)"
      section OPTDEPENDS "$(get optdepend)"
    } > "$tmp/$name-$ver/desc"
    echo "  $name $ver"
    n=$((n + 1))
  done
  (cd "$tmp" && LC_ALL=C tar --numeric-owner --owner=0 --group=0 -czf "$dir/aios.db.tar.gz" $(ls))
  # pacman はリンクにするが、git のホスティングからも読めるよう実体を置く
  cp -f "$dir/aios.db.tar.gz" "$dir/aios.db"
  echo "==> ${dir#"$root"/}/aios.db: $n packages"
}

for d in "$@"; do
  mkdb "$d"
done
