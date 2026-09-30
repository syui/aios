# pkg/*.sh から . で読む: Rust のツールを aarch64 musl の静的バイナリとしてビルドする共通部分
#   必要なもの: rustup target aarch64-unknown-linux-musl, aarch64-linux-gnu-gcc (C の依存に)
set -e
cd "$(dirname "$0")/.."

# C の部分は glibc 用のクロスコンパイラで作るので、musl にない __*_chk を呼ばないよう FORTIFY を切る
export CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar
export CFLAGS_aarch64_unknown_linux_musl="-U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
# rootfs は initramfs としてカーネルに入るので、デバッグ情報は持たない
export CARGO_PROFILE_RELEASE_DEBUG=false
export CARGO_PROFILE_RELEASE_STRIP=symbols

TARGET=aarch64-unknown-linux-musl

# fetch NAME URL: build/NAME になければ clone して、そこへ移る
fetch() {
  [ -d "build/$1" ] || git clone --depth 1 "$2" "build/$1"
  cd "build/$1"
}

# musl_build [cargo の引数...]
musl_build() {
  cargo build --release --target "$TARGET" "$@"
}
