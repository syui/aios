#!/bin/sh
set -e
cd "$(dirname "$0")/.."

remote=
if [ "$1" = -r ]; then
  remote=1
  shift
fi
server=${AIOS_SERVER:-https://git.syui.ai/ai/repo/raw/branch/main/aarch64/rust}
top=${server%/rust}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

rm -rf rootfs
mkdir rootfs
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
[ -n "$remote" ] && echo "rootfs: packages from $top"

kind_of() {
  for k in rust c desktop; do
    [ -f "pkg/$k/$1/PKGBUILD" ] && { echo "$k"; return 0; }
  done
  return 1
}

db() {
  [ -d "$tmp/db-$1" ] && return 0
  mkdir "$tmp/db-$1"
  curl -fsSL --retry 3 "$top/$1/aios.db" -o "$tmp/aios-$1.db"
  tar -xzf "$tmp/aios-$1.db" -C "$tmp/db-$1"
}

built() {
  case " $AIOS_BUILD " in *" $1 "*) return 0 ;; esac
  return 1
}

fetch() {
  d=
  f=
  kind=$2
  repo=repo/aarch64/$kind
  db "$kind"
  for x in "$tmp/db-$kind/$1"-[0-9]*; do
    [ -f "$x/desc" ] || continue
    [ "$(sed -n '/^%NAME%$/{n;p;}' "$x/desc")" = "$1" ] && d=$x
  done
  if [ -z "$d" ]; then
    echo "rootfs: $1 is not in $top/$kind; building it here"
    return 0
  fi
  file=$(sed -n '/^%FILENAME%$/{n;p;}' "$d/desc")
  case $file in '' | */* | .*) echo "rootfs: $1: bad file name in aios.db: $file" >&2; exit 1 ;; esac
  sum=$(sed -n '/^%SHA256SUM%$/{n;p;}' "$d/desc")
  if [ ! -f "$repo/$file" ]; then
    echo "fetch: $file"
    mkdir -p "$repo"
    curl -fsSL --retry 3 "$top/$kind/$file" -o "$repo/$file.part"
    mv "$repo/$file.part" "$repo/$file"
  fi
  if [ -n "$sum" ] && [ "$(sha256 "$repo/$file")" != "$sum" ]; then
    echo "$file: checksum mismatch" >&2
    rm -f "$repo/$file"
    exit 1
  fi
  f=$repo/$file
}

register() {
  info=$(zstd -dcq "$1" | tar -xOf - .PKGINFO)
  name=$(printf '%s\n' "$info" | sed -n 's/^pkgname = //p')
  ver=$(printf '%s\n' "$info" | sed -n 's/^pkgver = //p')
  dir=rootfs/var/lib/aipkg/local/$name-$ver
  mkdir -p "$dir"
  printf '%s\n' "$info" | awk -v now="$(date +%s)" '
    BEGIN {
      split("pkgname NAME pkgbase BASE pkgver VERSION pkgdesc DESC url URL builddate BUILDDATE packager PACKAGER size SIZE arch ARCH license LICENSE depend DEPENDS provides PROVIDES conflict CONFLICTS replaces REPLACES backup BACKUP", a, " ")
      for (i = 1; i < 32; i += 2) key[a[i]] = a[i + 1]
    }
    / = / {
      k = substr($0, 1, index($0, " = ") - 1)
      if (k in key) v[key[k]] = v[key[k]] substr($0, index($0, " = ") + 3) "\n"
    }
    END {
      v["INSTALLDATE"] = now "\n"
      v["REASON"] = "0\n"
      for (k in v) if (k != "BACKUP") printf "%%%s%%\n%s\n", k, v[k]
    }' > "$dir/desc"
  backup=$(printf '%s\n' "$info" | sed -n 's/^backup = //p')
  if [ -n "$backup" ]; then
    echo '%BACKUP%' >> "$dir/desc"
    for b in $backup; do
      printf '%s\t%s\n' "$b" "$(sha256 "rootfs/$b")" >> "$dir/desc"
    done
    echo >> "$dir/desc"
  fi
  { echo '%FILES%'; zstd -dcq "$1" | tar -tf - | grep -v '^\.'; echo; } > "$dir/files"
}

done=" "
install() {
  case "$done" in *" $1 "*) return 0 ;; esac
  done="$done$1 "
  kind=$(kind_of "$1") || { echo "unknown pkg: $1" >&2; exit 1; }
  repo=repo/aarch64/$kind
  f=
  if [ -n "$remote" ] && ! built "$1"; then
    fetch "$1" "$kind"
  fi
  if [ -z "$f" ]; then
    f=$(ls "$repo/$1"-[0-9]*-[0-9]*-*.pkg.tar.zst 2>/dev/null | head -1)
  fi
  if [ -z "$f" ]; then
    bin/mkpkg.sh "pkg/$kind/$1"
    f=$(ls "$repo/$1"-[0-9]*-[0-9]*-*.pkg.tar.zst | head -1)
  fi
  echo "rootfs: $(basename "$f")"
  zstd -dcq "$f" | tar -xpf - -C rootfs --exclude=.PKGINFO
  register "$f"
  for p in $(zstd -dcq "$f" | tar -xOf - .PKGINFO | sed -n 's/^\(provides\|replaces\) = //p' | sed 's/[<>=].*//'); do
    case "$done" in *" $p "*) ;; *) done="$done$p " ;; esac
  done
  for d in $(zstd -dcq "$f" | tar -xOf - .PKGINFO | sed -n 's/^depend = //p' | sed 's/[<>=].*//'); do
    install "$d"
  done
}

if [ -z "$remote" ] || built base; then
  bin/mkpkg.sh pkg/rust/base
fi
[ "$*" = all ] && set -- $(ls pkg/rust | grep -v -e '\.' -e '^base$')
for pkg in base "$@"; do
  install "$pkg"
done
