#!/bin/sh
# user/ と pkg をビルドして rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   bin/mkrootfs.sh            user/ だけ
#   bin/mkrootfs.sh coreutils  uutils/coreutils も入れる (初回は build/ に clone してビルド)
#   bin/mkrootfs.sh sudo-rs    sudo-rs も入れる
set -e
cd "$(dirname "$0")/.."

(cd user && cargo build --release)
bin=user/target/aarch64-unknown-linux-musl/release

rm -rf rootfs
mkdir -p rootfs/bin rootfs/usr/bin
cp "$bin/init" rootfs/init
for p in sh hello aipkg fetch systemctl journalctl login passwd su; do
  cp "$bin/$p" rootfs/bin/$p
done
# passwd と su は root の権限で動く
chmod 4755 rootfs/bin/passwd rootfs/bin/su
ln -s systemctl rootfs/bin/poweroff
ln -s systemctl rootfs/bin/reboot
cp -r etc rootfs/etc
chmod 600 rootfs/etc/shadow
chmod 440 rootfs/etc/sudoers
mkdir -p rootfs/root rootfs/home rootfs/var/log rootfs/run rootfs/tmp
chmod 700 rootfs/root
chmod 1777 rootfs/tmp

for pkg in "$@"; do
  case "$pkg" in
    coreutils)
      out=build/coreutils/target/aarch64-unknown-linux-musl/release
      [ -x "$out/coreutils" ] || pkg/coreutils.sh
      cp "$out/coreutils" rootfs/bin/coreutils
      # ビルドされた uu_* ごとにリンクを張る
      for f in "$out"/deps/libuu_*.rlib; do
        n=${f##*/libuu_}
        n=${n%%-*}
        case "$n" in *_common) continue ;; esac
        [ -e "rootfs/bin/$n" ] || ln -s coreutils "rootfs/bin/$n"
      done
      ;;
    sudo-rs)
      out=build/sudo-rs/target/aarch64-unknown-linux-musl/release
      [ -x "$out/sudo" ] || pkg/sudo-rs.sh
      cp "$out/sudo" "$out/visudo" rootfs/bin/
      chmod 4755 rootfs/bin/sudo
      ;;
    grep)
      out=build/grep/target/aarch64-unknown-linux-musl/release
      [ -x "$out/grep" ] || pkg/grep.sh
      cp "$out/grep" rootfs/bin/grep
      ;;
    *) echo "unknown pkg: $pkg" >&2; exit 1 ;;
  esac
done
