#!/bin/sh
# カーネルの memcpy / memmove / memset (kernel/src/mem.S) を外でためす (test/mem.c)。mem.S の名前だけを aios_* に
# 変えて作り、libc のものとくらべる。aarch64 でなければ qemu-aarch64 (ユーザーモード、cortex-a72: DC ZVA が 64 バイト) で
#   sh test/mem.sh [CC]   (既定は aarch64-linux-gnu-gcc、なければ gcc)
cd "$(dirname "$0")/.." || exit 2
cc=${1:-$(command -v aarch64-linux-gnu-gcc || echo gcc)}
out=${TMPDIR:-/tmp}/aios-mem.$$
trap 'rm -f "$out" "$out.o"' EXIT
$cc -c -o "$out.o" -x assembler-with-cpp kernel/src/mem.S -Dmemcpy=aios_memcpy -Dmemmove=aios_memmove -Dmemset=aios_memset || exit 2
$cc -O2 -static -fno-builtin -o "$out" test/mem.c "$out.o" || exit 2
if [ "$(uname -m)" = aarch64 ]; then
  "$out"
else
  qemu-aarch64 -cpu cortex-a72 "$out"
fi
