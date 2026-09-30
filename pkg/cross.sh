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
