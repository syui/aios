#!/bin/sh
# パッケージから rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   bin/mkrootfs.sh                  base (init, sh, aipkg, ... と /etc) とその依存 (coreutils など)
#   bin/mkrootfs.sh grep sed ...     pkg/rust/NAME のパッケージも入れる
#   bin/mkrootfs.sh all              pkg/rust/ のパッケージをぜんぶ入れる (C の拡張 pkg/c/ は入れない)
#   bin/mkrootfs.sh -r all           ビルドしないで、ai/repo (AIOS_SERVER) のパッケージを取ってきて使う
#                                    (Mac など、Linux のビルドの道具がないところで。curl と zstd が要る)
#   AIOS_BUILD="base aikernel" bin/mkrootfs.sh -r all
#                                    -r でも、AIOS_BUILD のパッケージはここのソースからビルドする (リリース用)
# base はこのリポジトリの user/ と etc/ から毎回作りなおす。ほかのパッケージは
# repo/aarch64/rust/NAME-*.pkg.tar.zst を使い、なければ bin/mkpkg.sh で作る。
# 入れたものは aipkg と同じ形で /var/lib/aipkg/local に記録するので、aipkg -Q で見え、-Syu で上がる
set -e
cd "$(dirname "$0")/.."

remote=
if [ "$1" = -r ]; then
  remote=1
  shift
fi
server=${AIOS_SERVER:-https://git.syui.ai/ai/repo/raw/branch/main/aarch64/rust}
repo=repo/aarch64/rust

# sha256sum がなければ (Mac) shasum で
sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

rm -rf rootfs
mkdir rootfs
mkdir -p "$repo"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# -r: リポジトリの aios.db (desc の tar.gz) を読んでおく
if [ -n "$remote" ]; then
  echo "rootfs: packages from $server"
  curl -fsSL --retry 3 "$server/aios.db" -o "$tmp/aios.db"
  mkdir "$tmp/db"
  tar -xzf "$tmp/aios.db" -C "$tmp/db"
fi

# built NAME: -r でも、ここでビルドするパッケージか (AIOS_BUILD)
built() {
  case " $AIOS_BUILD " in *" $1 "*) return 0 ;; esac
  return 1
}

# fetch NAME: aios.db にある NAME のパッケージを repo/aarch64/rust に取ってくる (チェックサムを確かめる)
fetch() {
  d=
  for x in "$tmp/db/$1"-[0-9]*; do
    [ "$(sed -n '/^%NAME%$/{n;p;}' "$x/desc")" = "$1" ] && d=$x
  done
  [ -n "$d" ] || { echo "not in $server: $1" >&2; exit 1; }
  file=$(sed -n '/^%FILENAME%$/{n;p;}' "$d/desc")
  sum=$(sed -n '/^%SHA256SUM%$/{n;p;}' "$d/desc")
  if [ ! -f "$repo/$file" ]; then
    echo "fetch: $file"
    curl -fsSL --retry 3 "$server/$file" -o "$repo/$file.part"
    mv "$repo/$file.part" "$repo/$file"
  fi
  if [ -n "$sum" ] && [ "$(sha256 "$repo/$file")" != "$sum" ]; then
    echo "$file: checksum mismatch" >&2
    rm -f "$repo/$file"
    exit 1
  fi
  f=$repo/$file
}

# register FILE: 入れたパッケージを aipkg の記録 (desc と files) に書く
register() {
  info=$(zstd -dcq "$1" | tar -xOf - .PKGINFO)
  name=$(printf '%s\n' "$info" | sed -n 's/^pkgname = //p')
  ver=$(printf '%s\n' "$info" | sed -n 's/^pkgver = //p')
  dir=rootfs/var/lib/aipkg/local/$name-$ver
  mkdir -p "$dir"
  printf '%s\n' "$info" | awk -v now="$(date +%s)" '
    BEGIN {
      split("pkgname NAME pkgbase BASE pkgver VERSION pkgdesc DESC url URL builddate BUILDDATE packager PACKAGER size SIZE arch ARCH license LICENSE depend DEPENDS provides PROVIDES conflict CONFLICTS backup BACKUP", a, " ")
      for (i = 1; i < 30; i += 2) key[a[i]] = a[i + 1]
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
  # 設定ファイルは「パス<TAB>sha256」で (aipkg が更新のときに手で変えたかを比べる)
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

# install NAME: パッケージ (と depends) を rootfs に入れる。入れたものは done に覚える
done=" "
install() {
  case "$done" in *" $1 "*) return 0 ;; esac
  done="$done$1 "
  [ -f "pkg/rust/$1/PKGBUILD" ] || { echo "unknown pkg: $1" >&2; exit 1; }
  if [ -n "$remote" ] && ! built "$1"; then
    fetch "$1"
  else
    f=$(ls "$repo/$1"-[0-9]*-[0-9]*-*.pkg.tar.zst 2>/dev/null | head -1)
  fi
  if [ -z "$f" ]; then
    bin/mkpkg.sh "pkg/rust/$1"
    f=$(ls "$repo/$1"-[0-9]*-[0-9]*-*.pkg.tar.zst | head -1)
  fi
  echo "rootfs: $(basename "$f")"
  zstd -dcq "$f" | tar -xpf - -C rootfs --exclude=.PKGINFO
  register "$f"
  # depends (版の条件 >= などは見ない)
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
