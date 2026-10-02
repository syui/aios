# C の拡張 (pkg/c/) を作るときの Zig。PKGBUILD の build() で読む:
#   . "$startdir/../zig.sh"; zig_env
# このマシン (Linux / Mac、aarch64 / x86_64) の公式の Zig を build/zig/ に取ってきて (sha256 を確かめる)、
# aios 用 (aarch64-linux-musl、静的) の CC CXX AR RANLIB を用意する。aios の中の base-devel と同じ Zig
# 入れる場所は /opt/c (PREFIX)
ZIG_VERSION=0.16.0
PREFIX=/opt/c

zig_sha256() {
  case $1 in
    aarch64-linux) echo ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17 ;;
    x86_64-linux) echo 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00 ;;
    aarch64-macos) echo b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489 ;;
    x86_64-macos) echo 0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7 ;;
    *) return 1 ;;
  esac
}

zig_env() {
  local m os host dir top tarball want
  m=$(uname -m)
  [ "$m" = arm64 ] && m=aarch64
  case $(uname -s) in Darwin) os=macos ;; *) os=linux ;; esac
  host=$m-$os
  want=$(zig_sha256 "$host") || { echo "zig.sh: no Zig for $host" >&2; return 1; }
  top=$(cd "$startdir/../../.." && pwd)
  dir=$top/build/zig/zig-$host-$ZIG_VERSION
  if [ ! -x "$dir/zig" ]; then
    tarball=$top/build/zig/zig-$host-$ZIG_VERSION.tar.xz
    mkdir -p "$top/build/zig"
    [ -f "$tarball" ] || curl -fL -o "$tarball" "https://ziglang.org/download/$ZIG_VERSION/zig-$host-$ZIG_VERSION.tar.xz"
    if [ "$(sha256 "$tarball")" != "$want" ]; then
      echo "zig.sh: $tarball: sha256 mismatch" >&2
      rm -f "$tarball"
      return 1
    fi
    tar -C "$top/build/zig" -xf "$tarball"
  fi
  export PATH=$dir:$PATH
  export CC="zig cc -target aarch64-linux-musl"
  export CXX="zig c++ -target aarch64-linux-musl"
  export AR="zig ar" RANLIB="zig ranlib"
  # -s: デバッグ情報は入れない (Zig は入れるのが既定)
  export CFLAGS="-O2" CXXFLAGS="-O2" LDFLAGS="-static -s"
  export ZIG_GLOBAL_CACHE_DIR=$top/build/zig/cache
  unset RUSTFLAGS
}

# c_deps NAME...: ほかの C のパッケージ (repo/aarch64/c/NAME-*.pkg.tar.zst、なければ作る) を
# $srcdir/sysroot に広げ、そこの include と lib を CPPFLAGS / LDFLAGS / PKG_CONFIG に足す
# (makepkg の makedepends の代わり。ビルドするマシンの /opt/c は触らない)
c_deps() {
  local top root n f
  top=$(cd "$startdir/../../.." && pwd)
  root=$srcdir/sysroot
  rm -rf "$root"
  mkdir -p "$root"
  for n in "$@"; do
    f=$(ls "$top/repo/aarch64/c/$n"-[0-9]*-[0-9]*-*.pkg.tar.zst 2>/dev/null | head -1)
    if [ -z "$f" ]; then
      "$top/bin/mkpkg.sh" "$top/pkg/c/$n" >&2
      f=$(ls "$top/repo/aarch64/c/$n"-[0-9]*-[0-9]*-*.pkg.tar.zst | head -1)
    fi
    zstd -dcq "$f" | tar -xf - -C "$root" --exclude=.PKGINFO
  done
  SYSROOT=$root$PREFIX
  export CPPFLAGS="-I$SYSROOT/include" LDFLAGS="$LDFLAGS -L$SYSROOT/lib"
  export PKG_CONFIG_LIBDIR=$SYSROOT/lib/pkgconfig PKG_CONFIG_SYSROOT_DIR=$root
}
