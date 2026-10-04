# cargo (Rust) のリンカ: aios には gcc がないので rustc に入っている rust-lld で (cc は base-devel の zig)
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
