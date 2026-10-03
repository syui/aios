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

# meson_cross DEP...: meson で aios 用にクロスビルドする準備。c_deps DEP... (と、動かすための musl) を
# sysroot に広げ、$srcdir/cross.ini を作る。ビルドの途中で aios 用のプログラムを動かすもの (glib など) は
# qemu-aarch64 で動かす (exe_wrapper。Debian / Ubuntu なら qemu-user)。
#   meson setup build --cross-file "$srcdir/cross.ini" --prefix="$PREFIX" --libdir=lib ...
meson_cross() {
  c_deps musl "$@"
  local root=$srcdir/sysroot
  # musl の動的リンカは /lib/ld-musl-aarch64.so.1 (qemu -L で sysroot をつける)
  [ -e "$root/lib" ] || ln -s usr/lib "$root/lib"
  # zig cc は -E と -c が両方あると -c (コンパイル) にしてしまう。meson の cc.preprocess は両方を渡すので、
  # -E のときは -c を外す小さな包みを通す。
  # zig は "-Wl,--version-script" "FILE" のように 2 つに分かれたものを読めないので、"-Wl,--version-script=FILE" に
  local zb=$srcdir/zigbin t
  mkdir -p "$zb"
  for t in cc c++; do
    cat > "$zb/$t" <<SH
#!/bin/sh
pre=
for a in "\$@"; do [ "\$a" = -E ] && pre=1; done
n=\$#
join=
while [ \$n -gt 0 ]; do
  a=\$1; shift; n=\$((n - 1))
  if [ -n "\$join" ]; then
    set -- "\$@" "\$join=\$a"
    join=
    continue
  fi
  case "\$a" in
    -c) [ -n "\$pre" ] && continue ;;
    -Wl,--version-script|-Wl,--dynamic-list) join=\$a; continue ;;
  esac
  set -- "\$@" "\$a"
done
exec zig $t -target aarch64-linux-musl "\$@"
SH
    chmod 755 "$zb/$t"
  done
  local wrap=
  if command -v qemu-aarch64 >/dev/null; then
    wrap="exe_wrapper = ['qemu-aarch64', '-L', '$root']"
  fi
  cat > "$srcdir/cross.ini" <<INI
[binaries]
c = '$zb/cc'
cpp = '$zb/c++'
ar = ['zig', 'ar']
ranlib = ['zig', 'ranlib']
pkg-config = 'pkg-config'
$wrap

[built-in options]
c_args = ['-O2', '-Wno-date-time']
cpp_args = ['-O2', '-Wno-date-time']
c_link_args = ['-s']
cpp_link_args = ['-s']

[properties]
sys_root = '$root'
pkg_config_libdir = '$root$PREFIX/lib/pkgconfig:$root$PREFIX/share/pkgconfig'

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
INI
  # c_deps の環境変数はこのマシン用のもの (wayland-scanner など) の検索まで変えるので、cross.ini にまかせる
  unset PKG_CONFIG_LIBDIR PKG_CONFIG_SYSROOT_DIR CPPFLAGS LDFLAGS CFLAGS CXXFLAGS
}

# autotools / cmake で共有ライブラリを作るときの環境 (zig_env の -static を外す)
shared_env() {
  export LDFLAGS="-Wl,-s${SYSROOT:+ -L$SYSROOT/lib}"
}

# cmake_cross DEP...: cmake で aios 用にクロスビルドする準備。c_deps DEP... を sysroot に広げ、
# $srcdir/toolchain.cmake を作る (cmake は空白のあるコンパイラ "zig cc ..." を扱えないので小さな包みを通す)。
# プログラムは動的にリンクする (共有ライブラリを使うので。zig の musl は静的が既定)
#   cmake -S . -B build -G Ninja -DCMAKE_TOOLCHAIN_FILE="$srcdir/toolchain.cmake" -DCMAKE_INSTALL_PREFIX="$PREFIX" ...
cmake_cross() {
  c_deps musl "$@"
  local root=$srcdir/sysroot tc=$srcdir/tc t
  [ -e "$root/lib" ] || ln -s usr/lib "$root/lib"
  mkdir -p "$tc"
  # zig cc は -E や -S と -c が両方あると -c (オブジェクト) にしてしまうので、そのときは -c を外す。
  # -S と -MD / -MF が両方だと -o のファイルを書かないので、それらを外して依存のファイル (-MF) は自分で書く
  for t in cc c++; do
    cat > "$tc/$t" <<SH
#!/bin/sh
only= asm= mf= mt=
for a in "\$@"; do case "\$a" in -E) only=1 ;; -S) only=1; asm=1 ;; esac; done
if [ -n "\$only" ]; then
  n=\$#
  prev=
  while [ \$n -gt 0 ]; do
    a=\$1; shift; n=\$((n - 1))
    p=\$prev; prev=\$a
    if [ -n "\$asm" ]; then
      case "\$p" in -MF) mf=\$a; continue ;; -MT) mt=\$a; continue ;; esac
      case "\$a" in -MD|-MMD|-MF|-MT) continue ;; esac
    fi
    [ "\$a" = -c ] && continue
    set -- "\$@" "\$a"
  done
fi
if [ -n "\$asm" ] && [ -n "\$mf" ]; then
  zig $t -target aarch64-linux-musl "\$@" || exit
  echo "\$mt:" > "\$mf"
  exit 0
fi
exec zig $t -target aarch64-linux-musl "\$@"
SH
  done
  printf '#!/bin/sh\nexec zig ar "$@"\n' > "$tc/ar"
  printf '#!/bin/sh\nexec zig ranlib "$@"\n' > "$tc/ranlib"
  chmod 755 "$tc"/*
  cat > "$srcdir/toolchain.cmake" <<CM
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER $tc/cc)
set(CMAKE_CXX_COMPILER $tc/c++)
set(CMAKE_ASM_COMPILER $tc/cc)
set(CMAKE_AR $tc/ar)
set(CMAKE_RANLIB $tc/ranlib)
set(CMAKE_C_FLAGS_INIT "-O2")
set(CMAKE_CXX_FLAGS_INIT "-O2")
set(CMAKE_EXE_LINKER_FLAGS_INIT "-dynamic -s")
set(CMAKE_SHARED_LINKER_FLAGS_INIT "-s")
set(CMAKE_FIND_ROOT_PATH $root$PREFIX $root/usr)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(ENV{PKG_CONFIG_LIBDIR} "$root$PREFIX/lib/pkgconfig:$root$PREFIX/share/pkgconfig")
set(ENV{PKG_CONFIG_SYSROOT_DIR} "$root")
CM
  unset PKG_CONFIG_LIBDIR PKG_CONFIG_SYSROOT_DIR CPPFLAGS LDFLAGS CFLAGS CXXFLAGS CC CXX AR RANLIB
}
