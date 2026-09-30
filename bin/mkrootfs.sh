#!/bin/sh
# パッケージから rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   bin/mkrootfs.sh                  aios-base (init, sh, aipkg, ... と /etc) だけ
#   bin/mkrootfs.sh grep sed ...     pkg/NAME のパッケージも入れる
#   bin/mkrootfs.sh all              pkg/ のパッケージをぜんぶ入れる
# aios-base はこのリポジトリの user/ と etc/ から毎回作りなおす。ほかのパッケージは
# repo/aarch64/NAME-*.pkg.tar.zst を使い、なければ bin/mkpkg.sh で作る。
# 入れたものは aipkg と同じ形で /var/lib/aipkg/local に記録するので、aipkg -Q で見え、-Syu で上がる
set -e
cd "$(dirname "$0")/.."

rm -rf rootfs
mkdir rootfs

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
      printf '%s\t%s\n' "$b" "$(sha256sum "rootfs/$b" | cut -d' ' -f1)" >> "$dir/desc"
    done
    echo >> "$dir/desc"
  fi
  { echo '%FILES%'; zstd -dcq "$1" | tar -tf - | grep -v '^\.'; echo; } > "$dir/files"
}

bin/mkpkg.sh pkg/aios-base
[ "$*" = all ] && set -- $(ls pkg | grep -v -e '\.' -e '^aios-base$')
for pkg in aios-base "$@"; do
  [ -f "pkg/$pkg/PKGBUILD" ] || { echo "unknown pkg: $pkg" >&2; exit 1; }
  f=$(ls repo/aarch64/"$pkg"-[0-9]*-[0-9]*-*.pkg.tar.zst 2>/dev/null | head -1)
  if [ -z "$f" ]; then
    bin/mkpkg.sh "pkg/$pkg"
    f=$(ls repo/aarch64/"$pkg"-[0-9]*-[0-9]*-*.pkg.tar.zst | head -1)
  fi
  echo "rootfs: $(basename "$f")"
  zstd -dcq "$f" | tar -xpf - -C rootfs --exclude=.PKGINFO
  register "$f"
done
