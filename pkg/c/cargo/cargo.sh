# cargo (Rust) のリンカ
# base-devel の cc (zig) があれば、それで動的リンクにする (Alpine と同じ)。winit や ash のように
# libwayland-client や libvulkan を dlopen で読む crate は、静的なバイナリでは動かない。
# 静的にしたいときは RUSTFLAGS="-C target-feature=+crt-static" (cc がそのときは -static でリンクする)。
# cc がなければ rustc に入っている rust-lld で静的リンク
if [ -x /opt/c/bin/cc ]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=/opt/c/bin/cc
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static"
else
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
fi
# C の部分がある crate (ring など) は base-devel の cc / ar (zig) で。
# aios のソースの .cargo/config.toml は Linux からのクロスビルド用に aarch64-linux-gnu-gcc を書いているが、
# cargo の [env] はすでにある環境変数を上書きしないので、ここで決めたものが使われる
export CC_aarch64_unknown_linux_musl=cc AR_aarch64_unknown_linux_musl=ar
