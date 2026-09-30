#!/bin/sh
# user/ と pkg をビルドして rootfs/ を作る。カーネルはビルド時に rootfs/ を initramfs として埋め込む
#   bin/mkrootfs.sh            user/ だけ
#   bin/mkrootfs.sh coreutils  uutils/coreutils も入れる (初回は build/ に clone してビルド)
#   bin/mkrootfs.sh sudo-rs    sudo-rs も入れる
#   ほかのパッケージ: grep diffutils sed awk tar findutils (ビルドは pkg/NAME.sh)
#   ぜんぶ: bin/mkrootfs.sh coreutils sudo-rs grep diffutils sed awk tar findutils
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
      # GNU と同じく egrep / fgrep は grep -E / -F を呼ぶだけのスクリプト
      for v in egrep:E fgrep:F; do
        printf '#!/bin/sh\nexec grep -%s "$@"\n' "${v#*:}" > "rootfs/bin/${v%:*}"
        chmod 755 "rootfs/bin/${v%:*}"
      done
      ;;
    diffutils)
      out=build/diffutils/target/aarch64-unknown-linux-musl/release
      [ -x "$out/diffutils" ] || pkg/diffutils.sh
      cp "$out/diffutils" rootfs/bin/diffutils
      # 呼ばれた名前 (diff / cmp) で動きが変わる
      ln -sf diffutils rootfs/bin/diff
      ln -sf diffutils rootfs/bin/cmp
      ;;
    sed | awk)
      out=build/$pkg/target/aarch64-unknown-linux-musl/release
      [ -x "$out/$pkg" ] || "pkg/$pkg.sh"
      cp "$out/$pkg" "rootfs/bin/$pkg"
      ;;
    tar)
      out=build/tar/target/aarch64-unknown-linux-musl/release
      [ -x "$out/tarapp" ] || pkg/tar.sh
      cp "$out/tarapp" rootfs/bin/tar
      ;;
    findutils)
      out=build/findutils/target/aarch64-unknown-linux-musl/release
      [ -x "$out/find" ] || pkg/findutils.sh
      cp "$out/find" "$out/xargs" rootfs/bin/
      ;;
    *) echo "unknown pkg: $pkg" >&2; exit 1 ;;
  esac
done
