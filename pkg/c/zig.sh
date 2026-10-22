# C の拡張 (pkg/c/) を作るときの Zig。PKGBUILD の build() で読む:
#   . "$startdir/../zig.sh"; zig_env
# このマシン (Linux / Mac、aarch64 / x86_64) の公式の Zig を build/zig/ に取ってきて (sha256 を確かめる)、
# aios 用 (aarch64-linux-musl、静的) の CC CXX AR RANLIB を用意する。aios の中の base-devel と同じ Zig
# 入れる場所は /opt/c (PREFIX)
ZIG_VERSION=0.17.0
# aios の直し (zig_fix_libc) の回。作るときの Zig は zig=ZIG_VERSION-ZIG_REL として .PKGINFO の makedepend に残り
# (zig_env が $srcdir/.toolchain に書き、mkpkg.sh が入れる)、aish の pkg_check は、いまのものとちがう Zig で
# 作ったパッケージを rebuild に出す。zig_fix_libc を変えて、作るものが変わるときは 1 つ上げる
#   1: realloc(p, 0)   2: libc.so の見本のデータ   3: iconv の gb18030 (CVE-2026-6042)
#   4: regcomp の int あふれ (CVE-2026-107891)
ZIG_REL=4
PREFIX=/opt/c

zig_sha256() {
  case $1 in
    aarch64-linux) echo 9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8 ;;
    x86_64-linux) echo 1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026 ;;
    aarch64-macos) echo b607e9b9234790a008116ae5bdb71c6243b84b9fb42a53a9e70fde41c06c536a ;;
    x86_64-macos) echo 4f9a1c5269aa17ebda5e6d3c2b89d6cbf36f7d2b22a0306e9ab98f25f95529c6 ;;
    *) return 1 ;;
  esac
}

# zig_fix_libc LIBDIR: Zig 0.17 の libc を musl と同じに直す (2 度あててもよい)。Zig は AI で書いた報告を
# 受けとらないので上流には出していない。ZIG_VERSION を上げたら、まだ要るか見る
#  - realloc (lib/c/malloc.zig): 大きさ 0 なら NULL を返すが、realloc(NULL, 0) は malloc(0) と同じでなければならず
#    (C の決まり)、musl は realloc(p, 0) にも NULL でないものを返す。gnulib は musl ならそうだと思って xnmalloc(0, ...)
#    などを NULL で「memory exhausted」にする (GNU sed の s/a/b/ が動かない)。
#    大きさ 0 の分かれ道を消して、musl と同じく 0 バイトの場所を返す
#  - libc.so の見本 (lib/libc/musl/libc.S。動的にリンクするとき、Zig はこれから名前だけの libc.so を作ってリンクする):
#    データ (__environ、__stack_chk_guard、optind、stdin、stdout、stderr など) がみな同じ番地にある。PIC でない
#    プログラムが __stack_chk_guard や stderr を使うと、lld はそれを COPY の再配置でプログラムの中に写し、同じ番地の
#    名前を別名と見て、みな同じ場所に置く。すると musl の __environ が __stack_chk_guard を読んで setenv で落ち (VLC)、
#    stdout も stderr を指す。データごとに場所をとり、musl のほんとうの別名 (src の weak_alias(__environ, environ)
#    など。こちらは同じ番地でないと、プログラムと musl が別の場所を見る) だけを .set で同じ番地にする
#  - iconv (lib/libc/musl/src/locale/iconv.c。静的にリンクしたものはこれを使う): gb18030 の 4 バイトの字を、1 字に
#    1 秒より長くかけて探し、BMP の終わりと BMP の外のものをまちがえる (U+10000 が U+3404C。CVE-2026-6042)。musl の
#    上流の直し (pkg/c/musl/CVE-2026-6042.patch。musl のパッケージと同じもの) をあてる。qsort (CVE-2026-40200) は
#    Zig の libc では Zig のもの (lib/c/stdlib.zig) なので、あてはまらない
#  - regcomp (lib/libc/musl/src/regex/regcomp.c。静的にリンクしたものはこれを使う): くりかえしの多い正規表現で、
#    位置や遷移の数の int があふれる (CVE-2026-107891)。musl の上流の直し (pkg/c/musl/CVE-2026-107891.patch) をあてる
zig_fix_libc() {
  zig_fix_realloc "$1/c/malloc.zig" && zig_fix_stub "$1/libc/musl/libc.S" "$1/libc/musl/src" &&
    zig_fix_iconv "$1/libc/musl" && zig_fix_regcomp "$1/libc/musl"
}

# zig_fix_iconv MUSL_DIR
zig_fix_iconv() {
  local m=$1 p
  p=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/musl/CVE-2026-6042.patch
  [ -f "$m/src/locale/iconv.c" ] || return 0
  grep -q gb18030utf "$m/src/locale/iconv.c" && return 0
  patch -s -p1 -d "$m" < "$p" && grep -q gb18030utf "$m/src/locale/iconv.c" ||
    { echo "zig.sh: could not fix $m/src/locale/iconv.c" >&2; return 1; }
}

zig_fix_regcomp() {
  local m=$1 p
  p=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/musl/CVE-2026-107891.patch
  [ -f "$m/src/regex/regcomp.c" ] || return 0
  grep -q check_add "$m/src/regex/regcomp.c" && return 0
  patch -s -p1 -d "$m" < "$p" && grep -q check_add "$m/src/regex/regcomp.c" ||
    { echo "zig.sh: could not fix $m/src/regex/regcomp.c" >&2; return 1; }
}

zig_fix_realloc() {
  local f=$1
  [ -f "$f" ] || return 0
  grep -q 'aios: realloc(p, 0)' "$f" && return 0
  awk '
    /^fn realloc\(/ { inr = 1 }
    inr && /^    if \(n == 0\) \{$/ { skip = 1; print "    // aios: realloc(p, 0) returns a 0-byte block, as musl does (no NULL for size 0)"; next }
    skip { if (/^    \}$/) { skip = 0; inr = 0 } next }
    { print }
  ' "$f" > "$f.new" && mv "$f.new" "$f"
  grep -q 'aios: realloc(p, 0)' "$f" || { echo "zig.sh: could not fix $f" >&2; return 1; }
}

# zig_fix_stub LIBC.S MUSL_SRC
zig_fix_stub() {
  local s=$1 src=$2
  [ -f "$s" ] || return 0
  grep -q 'aios: data objects' "$s" && return 0
  # 別名の一覧 (別名 もと)。#define weak_alias(old, new) の行もまざるが、そういう名前のデータはない
  grep -rhoE 'weak_alias\( *[A-Za-z0-9_]+ *, *[A-Za-z0-9_]+ *\)' "$src" |
    sed -E 's/weak_alias\( *([A-Za-z0-9_]+) *, *([A-Za-z0-9_]+) *\)/\2 \1/' > "$s.alias"
  # 1 回目に見本のデータの名前を集め、2 回目に書く。.type NAME, %object; / .size NAME, SIZE / NAME: のならび
  awk '
    pass == 0 { to[$1] = $2; next }
    pass == 1 { if ($1 == ".type" && $3 == "%object;") { n = $2; sub(/,$/, "", n); obj[n] = 1 } next }
    FNR == 1 { print "/* aios: data objects get their own storage; only the aliases in musl share an address */" }
    $1 == ".type" && $3 == "%object;" { cur = $2; sub(/,$/, "", cur) }
    $1 == ".size" { size = $3 }
    cur != "" && $0 == cur ":" {
      if ((cur in to) && (to[cur] in obj)) print ".set " cur ", " to[cur]
      else { print ".balign 16"; print cur ": .skip " size }
      cur = ""
      next
    }
    { print }
  ' "$s.alias" pass=1 "$s" pass=2 "$s" > "$s.new" && mv "$s.new" "$s"
  rm -f "$s.alias"
  grep -q 'aios: data objects' "$s" && grep -q '^\.set environ, __environ$' "$s" ||
    { echo "zig.sh: could not fix $s" >&2; return 1; }
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
  zig_fix_libc "$dir/lib"
  export PATH=$dir:$PATH
  export CC="zig cc -target aarch64-linux-musl"
  export CXX="zig c++ -target aarch64-linux-musl"
  export AR="zig ar" RANLIB="zig ranlib"
  # -s: デバッグ情報は入れない (Zig は入れるのが既定)
  export CFLAGS="-O2" CXXFLAGS="-O2" LDFLAGS="-static -s"
  export ZIG_GLOBAL_CACHE_DIR=$top/build/zig/cache
  unset RUSTFLAGS
  # この Zig で作ったこと (mkpkg.sh が .PKGINFO の makedepend = zig=... にする)
  if [ -n "$srcdir" ]; then
    echo "zig=$ZIG_VERSION-$ZIG_REL" > "$srcdir/.toolchain"
  fi
}

# c_deps NAME...: ほかの C のパッケージ (repo/aarch64/c/NAME-*.pkg.tar.zst、なければ作る) を
# $srcdir/sysroot に広げ、そこの include と lib を CPPFLAGS / LDFLAGS / PKG_CONFIG に足す
# (makepkg の makedepends の代わり。ビルドするマシンの /opt/c は触らない)
# 広げた版は $srcdir/.c_deps に NAME=VER-REL で書きとめる。mkpkg.sh が .PKGINFO の makedepend にし、aish の pkg_check が
# それより新しいライブラリがあれば rebuild に出す (静的にリンクしたものは、作りなおすまで古いままなので)
c_deps() {
  local top root n f
  top=$(cd "$startdir/../../.." && pwd)
  root=$srcdir/sysroot
  rm -rf "$root"
  mkdir -p "$root"
  : > "$srcdir/.c_deps"
  for n in "$@"; do
    f=$(ls "$top/repo/aarch64/c/$n"-[0-9]*-[0-9]*-*.pkg.tar.zst 2>/dev/null | head -1)
    if [ -z "$f" ]; then
      "$top/bin/mkpkg.sh" "$top/pkg/c/$n" >&2
      f=$(ls "$top/repo/aarch64/c/$n"-[0-9]*-[0-9]*-*.pkg.tar.zst | head -1)
    fi
    zstd -dcq "$f" | tar -xf - -C "$root" --exclude=.PKGINFO
    # NAME-VER-REL-ARCH.pkg.tar.zst → NAME=VER-REL
    f=${f##*/}
    f=${f%-*.pkg.tar.zst}
    echo "$n=${f#"$n"-}" >> "$srcdir/.c_deps"
  done
  SYSROOT=$root$PREFIX
  export CPPFLAGS="-I$SYSROOT/include" LDFLAGS="$LDFLAGS -L$SYSROOT/lib"
  export PKG_CONFIG_LIBDIR=$SYSROOT/lib/pkgconfig PKG_CONFIG_SYSROOT_DIR=$root
}

# meson_cross DEP...: meson で aios 用にクロスビルドする準備。c_deps DEP... (と、動かすための musl) を
# sysroot に広げ、$srcdir/cross.ini を作る。ビルドの途中で aios 用のプログラムを動かすもの (glib など) は
# qemu-aarch64 で動かす (exe_wrapper。Debian / Ubuntu なら qemu-user)。
#   meson setup build --wrap-mode=nofallback --cross-file "$srcdir/cross.ini" --prefix="$PREFIX" --libdir=lib ...
#   (--wrap-mode=nofallback: 足りないライブラリを subproject で自分の中にビルドさせない。pango 1.58 が
#    glib を丸ごと抱えこんで、glib2 のパッケージとファイルがぶつかった)
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
