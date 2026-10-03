# PKGBUILD の build() から . で読む: aios (aarch64) 向けに Rust のツールを
# musl の静的バイナリとしてクロスビルドする設定
#   必要なもの: rustup (target aarch64-unknown-linux-musl), aarch64-linux-gnu-gcc (C の依存に)

TARGET=aarch64-unknown-linux-musl
export CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar
# C の部分は glibc 用のクロスコンパイラで作るので、musl にない __*_chk を呼ばないよう FORTIFY を切る
export CFLAGS_aarch64_unknown_linux_musl="-U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
# デバッグ情報は持たない (rootfs はカーネルに埋め込まれる)
export CARGO_PROFILE_RELEASE_DEBUG=false
export CARGO_PROFILE_RELEASE_STRIP=symbols
# makepkg の CFLAGS/LDFLAGS (x86_64 用) を cargo や cc に渡さない
unset CFLAGS CXXFLAGS LDFLAGS RUSTFLAGS

# musl_build [cargo の引数...]: 今のディレクトリの crate をビルドする
# (rust-toolchain.toml で版が決まっていても、その版に target を入れる)
musl_build() {
  rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release --target "$TARGET" "$@"
}

# musl_zig: C の依存を Zig (musl のヘッダー) で作る。glibc のクロスコンパイラは glibc 2.38 からの
# __isoc23_strtol などを呼ぶ形にしてしまい、musl とつながらないことがある (aws-lc など)。
# Zig は pkg/c/zig.sh と同じもの (build/zig/)。musl_build の前に呼ぶ
musl_zig() {
  . "$startdir/../../c/zig.sh"
  zig_env || return 1
  unset CC CXX AR RANLIB CFLAGS CXXFLAGS LDFLAGS
  # cc-rs が付ける --target=aarch64-unknown-linux-musl (Rust の名前) は Zig に通じないので外す。
  # Zig は -O のないものに UBSan を入れる (__ubsan_handle_* は Rust のリンクにはない) ので切る
  for t in cc c++; do
    cat > "$srcdir/zig-$t" <<ZIG
#!/bin/sh
for a; do
  shift
  case \$a in --target=*) ;; *) set -- "\$@" "\$a" ;; esac
done
exec zig $t -target aarch64-linux-musl -fno-sanitize=undefined "\$@"
ZIG
    chmod +x "$srcdir/zig-$t"
  done
  printf '#!/bin/sh\nexec zig ar "$@"\n' > "$srcdir/zig-ar"
  chmod +x "$srcdir/zig-ar"
  export CC_aarch64_unknown_linux_musl=$srcdir/zig-cc CXX_aarch64_unknown_linux_musl=$srcdir/zig-c++ AR_aarch64_unknown_linux_musl=$srcdir/zig-ar
  unset CFLAGS_aarch64_unknown_linux_musl
}
