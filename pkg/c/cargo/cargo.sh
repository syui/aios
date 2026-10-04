# cargo (Rust) のリンカ: aios には gcc がないので rustc に入っている rust-lld で
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
# C の部分がある crate (ring など) は base-devel の cc / ar (zig) で。
# aios のソースの .cargo/config.toml は Linux からのクロスビルド用に aarch64-linux-gnu-gcc を書いているが、
# cargo の [env] はすでにある環境変数を上書きしないので、ここで決めたものが使われる
export CC_aarch64_unknown_linux_musl=cc AR_aarch64_unknown_linux_musl=ar
