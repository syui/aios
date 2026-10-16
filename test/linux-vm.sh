#!/bin/sh
# ロボット (test/sysdiff.c) の手本にする Linux を、このマシンのカーネルでなく、kernel.org の最新の安定版にする:
# arm64 の Linux を小さな設定 (test/linux.config) で作り、aios と同じ QEMU の virt で起こして、その中で動かす。
#   test/linux-vm.sh version [latest|X.Y.Z]      kernel.org の latest_stable (または X.Y.Z) の版
#   test/linux-vm.sh build [latest|X.Y.Z]        作る (できていればそのまま)。Image のパスを出す
#   test/linux-vm.sh run IMAGE SYSDIFF CASES OPS home|tmp OUT
#                                                 sysdiff (aarch64 の静的なもの) を nobody で、home なら ext4、
#                                                 tmp なら tmpfs で動かし、出力を OUT に (終わりの番号は sysdiff の)
#   test/linux-vm.sh exec [latest|X.Y.Z] PROG [ARG...]
#                                                 テストのプログラム (aarch64 の静的なもの) を nobody で動かして出力を出す。
#                                                 /work は ext4、/tmp は tmpfs (たとえば pipeslots /work/.ps)
# 置き場は build/linux/ (VERSION/Image と、作るときのソースと tarball。作ったらソースは消す)。
# 作るのに要るもの: aarch64-linux-gnu-gcc、make、flex、bison、bc、curl、xz。動かすのに qemu-system-aarch64、mkfs.ext4、debugfs
set -e
top=$(cd "$(dirname "$0")/.." && pwd)
dir=$top/build/linux
cross=${CROSS_COMPILE:-aarch64-linux-gnu-}

version() {
  case "${1:-latest}" in
  latest)
    v=$(curl -fsS --max-time 30 https://www.kernel.org/releases.json 2>/dev/null |
      python3 -c 'import json,sys; print(json.load(sys.stdin)["latest_stable"]["version"])' 2>/dev/null) || v=
    # kernel.org に届かなければ、作ってあるいちばん新しいもの
    [ -n "$v" ] || v=$(ls -1 "$dir" 2>/dev/null | grep -E '^[0-9]+\.[0-9]+' | while read -r x; do [ -s "$dir/$x/Image" ] && echo "$x"; done | sort -V | tail -1)
    if [ -z "$v" ]; then
      echo "linux-vm: cannot reach kernel.org, and no Linux is built yet" >&2
      exit 1
    fi
    echo "$v" ;;
  *) echo "$1" ;;
  esac
}

build() {
  v=$(version "$1")
  out=$dir/$v
  if [ -s "$out/Image" ]; then
    echo "$out/Image"
    return
  fi
  mkdir -p "$out"
  major=${v%%.*}
  tar=$dir/linux-$v.tar.xz
  if [ ! -s "$tar" ]; then
    curl -fsSL --max-time 900 -o "$tar.part" "https://cdn.kernel.org/pub/linux/kernel/v$major.x/linux-$v.tar.xz"
    want=$(curl -fsS --max-time 60 "https://cdn.kernel.org/pub/linux/kernel/v$major.x/sha256sums.asc" | awk -v f="linux-$v.tar.xz" '$2 == f { print $1 }')
    got=$(sha256sum "$tar.part" | cut -d' ' -f1)
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
      echo "linux-vm: linux-$v.tar.xz: sha256 $got, kernel.org says ${want:-nothing}" >&2
      rm -f "$tar.part"
      exit 1
    fi
    mv "$tar.part" "$tar"
  fi
  src=$dir/src-$v
  obj=$dir/obj-$v
  rm -rf "$src" "$obj"
  mkdir -p "$src" "$obj"
  tar -xJf "$tar" -C "$src" --strip-components=1
  mk() { make -s -C "$src" O="$obj" ARCH=arm64 CROSS_COMPILE="$cross" "$@"; }
  mk tinyconfig >/dev/null
  cat "$top/test/linux.config" >> "$obj/.config"
  mk olddefconfig >/dev/null
  # 設定に入らなかったもの (名前が変わった、たりないものがある) を知らせる
  grep '^CONFIG_' "$top/test/linux.config" | while IFS= read -r l; do
    grep -qx "$l" "$obj/.config" || echo "linux-vm: $l did not stick" >&2
  done
  mk -j"$(nproc)" Image
  cp "$obj/arch/arm64/boot/Image" "$out/Image"
  cp "$obj/.config" "$out/config"
  rm -rf "$src" "$obj" "$tar"
  echo "$out/Image"
}

# initramfs (newc の cpio) を python で: /init、/sysdiff、/cases.txt、/args (OPS と home|tmp)
initramfs() {
  python3 - "$@" <<'EOF'
import os, sys
out, files = sys.argv[1], sys.argv[2:]
def entry(name, mode, data, ino):
    hdr = "070701" + "".join("%08X" % x for x in (ino, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name) + 1, 0))
    b = hdr.encode() + name.encode() + b"\0"
    b += b"\0" * (-len(b) % 4) + data
    return b + b"\0" * (-len(b) % 4)
blob, ino = b"", 1
for d in ("dev", "proc", "sys", "tmp", "work"):
    blob += entry(d, 0o40755, b"", ino); ino += 1
for f in files:
    name, path = f.split("=", 1)
    data = open(path, "rb").read() if path != "-" else b""
    mode = 0o100755 if name in ("init", "sysdiff", "prog") else 0o100644
    blob += entry(name, mode, data, ino); ino += 1
blob += entry("TRAILER!!!", 0, b"", 0)
open(out, "wb").write(blob)
EOF
}

run() {
  image=$1 sysdiff=$2 cases=$3 ops=$4 fs=$5 out=$6
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  printf '%s %s\n' "$ops" "$fs" > "$tmp/args"
  boot "$image" sysdiff="$sysdiff" cases.txt="$cases" args="$tmp/args"
  st=$(debugfs -R "cat /status" "$tmp/work.img" 2>/dev/null | tr -d '\n')
  debugfs -R "dump /out.txt $out" "$tmp/work.img" >/dev/null 2>&1 || true
  if [ ! -s "$out" ] || [ -z "$st" ]; then
    echo "linux-vm: sysdiff did not finish in the VM; serial:" >&2
    tail -20 "$tmp/serial" >&2
    exit 2
  fi
  debugfs -R "cat /err.txt" "$tmp/work.img" 2>/dev/null | tail -3 >&2
  exit "$st"
}

# /init を作り (test/linux-init.c。aarch64 の静的なもの)、initramfs (NAME=FILE ...) と 1 GiB の ext4 で起こす。
# $tmp に work.img と serial が残る
boot() {
  image=$1
  shift
  init=$dir/init
  if [ ! -s "$init" ] || [ "$top/test/linux-init.c" -nt "$init" ]; then
    zig=${ZIG:-$(ls "$top"/build/zig/*/zig 2>/dev/null | head -1)}
    if [ -n "$zig" ]; then
      "$zig" cc -target aarch64-linux-musl -O2 -static -s -o "$init" "$top/test/linux-init.c"
    else
      aarch64-linux-gnu-gcc -O2 -static -s -o "$init" "$top/test/linux-init.c"
    fi
  fi
  initramfs "$tmp/initrd" init="$init" "$@"
  # ext4 は aios のディスクと同じく 4 KiB のブロックで (小さなものの既定は 1 KiB)
  truncate -s 1G "$tmp/work.img"
  mkfs.ext4 -q -F -b 4096 -E root_owner=0:0 "$tmp/work.img"
  timeout 1800 qemu-system-aarch64 -M virt -cpu max -m 2G -smp 1 -nographic -no-reboot \
    -kernel "$image" -initrd "$tmp/initrd" -append "console=ttyAMA0 rdinit=/init quiet" \
    -drive file="$tmp/work.img",if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0 \
    > "$tmp/serial" 2>&1 || true
}

exec_prog() {
  image=$(build "$1")
  prog=$2
  shift 2
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  echo "exec $*" > "$tmp/args"
  boot "$image" prog="$prog" args="$tmp/args"
  st=$(debugfs -R "cat /status" "$tmp/work.img" 2>/dev/null | tr -d '\n')
  debugfs -R "cat /out.txt" "$tmp/work.img" 2>/dev/null
  debugfs -R "cat /err.txt" "$tmp/work.img" 2>/dev/null >&2
  if [ -z "$st" ]; then
    echo "linux-vm: the program did not finish in the VM; serial:" >&2
    tail -20 "$tmp/serial" >&2
    exit 2
  fi
  exit "$st"
}

case "$1" in
version) version "$2" ;;
build) build "$2" ;;
run) shift; run "$@" ;;
exec) shift; exec_prog "$@" ;;
*) sed -n '2,10p' "$0" >&2; exit 2 ;;
esac
