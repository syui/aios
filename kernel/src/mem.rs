//! memcpy / memmove / memset (mem.S)。compiler_builtins のもの (8 バイトずつ、src がそろっていなければ 1 バイトずつ)
//! のかわりに、Linux の arch/arm64/lib と同じく ldp / stp で 64 バイトずつ、0 で埋める長いものは DC ZVA。
//! ユーザーとのコピー (copy_in / copy_out)、パイプ、ソケット、ページキャッシュ、ページを 0 にするのもみなこれを通る。
//! test/mem.sh が外 (qemu-aarch64) で libc のものとくらべる。速さは lockbench (spinlock.rs) の memcpy と memset
core::arch::global_asm!(include_str!("mem.S"));
