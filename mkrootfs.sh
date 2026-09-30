#!/bin/sh
# user/ と pkg をビルドして rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   ./mkrootfs.sh            user/ だけ
#   ./mkrootfs.sh coreutils  uutils/coreutils も入れる (初回は build/ に clone してビルド)
set -e
cd "$(dirname "$0")"

(cd user && cargo build --release)
bin=user/target/aarch64-unknown-linux-musl/release

rm -rf rootfs
mkdir -p rootfs/bin rootfs/usr/bin
cp "$bin/init" rootfs/init
for p in sh hello aipkg fetch; do
  cp "$bin/$p" rootfs/bin/$p
done
cp -r etc rootfs/etc

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
    *) echo "unknown pkg: $pkg" >&2; exit 1 ;;
  esac
done
