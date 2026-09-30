#!/bin/sh
# user/ とパッケージから rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   bin/mkrootfs.sh                  user/ (init, sh, aipkg, ...) だけ
#   bin/mkrootfs.sh grep sed ...     pkg/NAME のパッケージも入れる
#   bin/mkrootfs.sh all              pkg/ のパッケージをぜんぶ入れる
# パッケージは repo/aarch64/NAME-*.pkg.tar.zst を使い、なければ bin/mkpkg.sh で作る。
# 入れたものは aipkg と同じ形で /var/lib/aipkg/local に記録するので、aipkg -Q で見え、-Syu で上がる
# 並びは Arch と同じく /usr にまとめる: /usr/bin が本体で、/bin と /sbin はそこへのリンク
set -e
cd "$(dirname "$0")/.."

(cd user && cargo build --release)
bin=user/target/aarch64-unknown-linux-musl/release

rm -rf rootfs
mkdir -p rootfs/usr/bin
ln -s usr/bin rootfs/bin
ln -s usr/bin rootfs/sbin
ln -s bin rootfs/usr/sbin
cp "$bin/init" rootfs/init
for p in sh hello aipkg fetch systemctl journalctl login passwd su; do
  cp "$bin/$p" rootfs/usr/bin/$p
done
# passwd と su は root の権限で動く
chmod 4755 rootfs/usr/bin/passwd rootfs/usr/bin/su
ln -s systemctl rootfs/usr/bin/poweroff
ln -s systemctl rootfs/usr/bin/reboot
cp -r etc rootfs/etc
chmod 600 rootfs/etc/shadow
chmod 440 rootfs/etc/sudoers
mkdir -p rootfs/root rootfs/home rootfs/var/log rootfs/var/lib/aipkg/local rootfs/var/cache/aipkg rootfs/run rootfs/tmp
chmod 700 rootfs/root
chmod 1777 rootfs/tmp

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
      for (k in v) printf "%%%s%%\n%s\n", k, v[k]
    }' > "$dir/desc"
  { echo '%FILES%'; zstd -dcq "$1" | tar -tf - | grep -v '^\.'; echo; } > "$dir/files"
}

[ "$*" = all ] && set -- $(ls pkg | grep -v '\.')
for pkg in "$@"; do
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
