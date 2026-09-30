#!/bin/bash
# repo-add の代わり: DIR の *.pkg.tar.zst から同期データベース DIR/REPO.db を作りなおす
#   bin/mkrepo.sh [DIR] [REPO]    (既定は repo/aarch64 と aios)
# できた DIR をそのまま git.syui.ai/ai/repo の aarch64/ に置けば
# aipkg.conf の Server = https://git.syui.ai/ai/repo/raw/branch/main/$arch で読める
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
dir=${1:-$root/repo/aarch64}
repo=${2:-aios}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# section KEY VALUES: pacman の desc の 1 項目 (値は 1 行に 1 つ、空なら書かない)
section() {
  [ -n "$2" ] || return 0
  printf '%%%s%%\n%s\n\n' "$1" "$2"
}

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
  } > "$tmp/$name-$ver/desc"
  echo "  $name $ver"
  n=$((n + 1))
done
(cd "$tmp" && LC_ALL=C tar --numeric-owner --owner=0 --group=0 -czf "$dir/$repo.db.tar.gz" $(ls))
# pacman はリンクにするが、git のホスティングからも読めるよう実体を置く
cp -f "$dir/$repo.db.tar.gz" "$dir/$repo.db"
echo "==> $repo.db: $n packages"
