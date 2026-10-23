#!/bin/sh
# 新しいカーネルを試して、だめなら前のものに戻る (aiboot の boot counting と aiosd。doc/aios.md の「改造」) のテスト。
# 開発のマシンで: aios.img (bin/run.sh と同じ) を -snapshot で UEFI から起こし、3 つのカーネルを aios install kernel
# で入れて再起動する (ディスクには何も残らない)
#   good  AIOS_RELEASE=boot-good のもの → そのカーネルで起動し、boot.last は booted
#   load  good の Image の頭 (64 バイト) だけで中は 0 のもの → UEFI の LoadImage がしくじり、前のもの (good) に
#         戻る (why=load)
#   hang  AIOS_TEST_HANG=1 のもの (aiosd の前で止まる) → --reset-after でリセットし、前のもの (good) に戻る
#         (why=unfinished)
#   chosen hang をもう一度入れ、起動の一覧 (timeout 5) で前のもの (good) を選ぶ (why=chosen)
# どれのあとも /boot/Image は前のもの (good) に戻り、try は消える
#   test/vm-boot.sh          ディスクの aiboot、aiosd、aios で
#   test/vm-boot.sh --new    このソースの aiboot、aiosd、aios をディスクに入れてから (それらを変えたとき)
# 1 行に ok NAME か FAIL NAME。おしまいに boot: N failed (0 でなければ失敗のステータス)
cd "$(dirname "$0")/.."
new=
[ "$1" = --new ] && new=1
out=build/vm-boot
mkdir -p "$out"
vm() { python3 test/vm.py "$@"; }
fail=0
n=0
check() {
  name=$1 got=$2 want=$3
  n=$((n + 1))
  case "$got" in
    *"$want"*) echo "ok $name" ;;
    *) echo "FAIL $name: want $want, got: $got"; fail=$((fail + 1)) ;;
  esac
}

# カーネル (Image)。good と hang は、それぞれの target で作る (ふつうのビルドとまざらないように)
objcopy=$(command -v llvm-objcopy || command -v rust-objcopy || command -v aarch64-linux-gnu-objcopy) || { echo "vm-boot: no objcopy" >&2; exit 2; }
kernel() {
  (cd kernel && env AIOS_INITRD=none AIOS_RELEASE="$1" "$2" CARGO_TARGET_DIR="../$out/target-$1" cargo build -q --release -p aios) || exit 2
  "$objcopy" -O binary "$out/target-$1/aarch64-unknown-none-softfloat/release/aios" "$out/Image-$1"
}
kernel boot-good AIOS_TEST_HANG=
kernel boot-hang AIOS_TEST_HANG=1
python3 -c 'import sys; b = open(sys.argv[1], "rb").read(); open(sys.argv[2], "wb").write(b[:64] + bytes(len(b) - 64))' "$out/Image-boot-good" "$out/Image-load"

vm stop >/dev/null 2>&1
AIOS_KERNEL= AIOS_QEMU_ARGS=-snapshot vm start >/dev/null || { echo "vm-boot: the VM did not boot from aios.img" >&2; exit 2; }
trap 'vm stop >/dev/null 2>&1' EXIT
# /tmp は再起動で消えるので、ホームに置く
dir=/home/ai/.vm-boot
for f in Image-boot-good Image-boot-hang Image-load; do
  vm put "$out/$f" "$dir/$f" || exit 2
done
if [ -n "$new" ]; then
  (cd boot && cargo build -q --release) && (cd user && cargo build -q --release --target aarch64-unknown-linux-musl --bin aios --bin aiosd) || exit 2
  vm put boot/target/aarch64-unknown-uefi/release/aiboot.efi "$dir/aiboot.efi" &&
    vm put user/target/aarch64-unknown-linux-musl/release/aiosd "$dir/aiosd" &&
    vm put user/target/aarch64-unknown-linux-musl/release/aios "$dir/aios" || exit 2
  vm run "sudo install -m644 $dir/aiboot.efi /boot/EFI/BOOT/BOOTAA64.EFI && sudo install -m755 $dir/aiosd $dir/aios /usr/bin/ && sudo systemctl restart aiosd" >/dev/null || exit 2
fi

# install KERNEL → reboot (reset_after があれば、それだけ待ってリセット) → いまのカーネルと boot.last
# ($(try ...) の中で動くので、しくじりは数えずに出力に書く: あとの check が落ちる)
try() {
  vm run "aios install kernel $dir/$1" -t 50 >/dev/null || { echo "install $1 failed"; return; }
  vm reboot -t 180 ${2:+--reset-after "$2"} >/dev/null || { echo "no prompt after rebooting with $1"; return; }
  # 「uname -r | boot.last.result | boot.last.why | try=boot.try」(aios get は 1 つの値なら値だけを出す)
  vm run 'echo "$(uname -r) | $(aios get boot.last.result) | $(aios get boot.last.why 2>/dev/null) | try=$(aios get boot.try)"' -t 30
}

r=$(try Image-boot-good)
check good "$r" "boot-good | the new kernel booted | "
check good-try "$r" "try=null"
r=$(try Image-load)
check load "$r" "boot-good | the new kernel could not be loaded"
check load-why "$r" "| load |"
r=$(try Image-boot-hang 30)
check hang "$r" "boot-good | the new kernel did not finish booting"
check hang-why "$r" "| unfinished |"
check hang-try "$r" "try=null"
# chosen: 起動の一覧で前のものを選ぶ (loader.conf に timeout 5 を足し、一覧が出たらシリアルから番号を押す)。
# 新しいものは hang なので、選べなければ止まって unfinished になる
vm run "sudo cp /boot/loader/loader.conf $dir/loader.conf.orig 2>/dev/null; { grep -v '^timeout' $dir/loader.conf.orig 2>/dev/null; echo 'timeout 5'; } > $dir/loader.conf && sudo install -m644 $dir/loader.conf /boot/loader/loader.conf" >/dev/null
vm run "aios install kernel $dir/Image-boot-hang" -t 50 >/dev/null
c=$(vm reboot -t 180 --reset-after 90 --choose previous)
check chosen-menu "$c" '"chose": "aios (previous kernel)"'
r=$(vm run 'echo "$(uname -r) | $(aios get boot.last.result) | $(aios get boot.last.why 2>/dev/null) | try=$(aios get boot.try)"' -t 30)
check chosen "$r" "boot-good | the previous kernel was chosen"
check chosen-why "$r" "| chosen |"
check chosen-try "$r" "try=null"
vm run "if [ -f $dir/loader.conf.orig ]; then sudo install -m644 $dir/loader.conf.orig /boot/loader/loader.conf; else sudo rm -f /boot/loader/loader.conf; fi" >/dev/null
r=$(vm run "cmp /boot/Image $dir/Image-boot-good && echo same")
check image-back "$r" same

echo "boot: $n checks, $fail failed"
[ "$fail" = 0 ]
