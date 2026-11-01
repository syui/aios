#!/bin/sh
# 新しいカーネルを試して、だめなら前のものに戻る (systemd-boot と同じ boot counting と aiosd。doc/aios.md の「改造」)
# のテスト。開発のマシンで: aios.img (bin/run.sh と同じ) を -snapshot で UEFI から起こし、3 つのカーネルを
# aios install kernel で入れて再起動する (ディスクには何も残らない)
#   good  AIOS_RELEASE=boot-good のもの → そのカーネルで起動し、/boot/Image になる (boot.last は booted)
#   load  good の Image の頭 (64 バイト) だけで中は 0 のもの → UEFI の LoadImage がしくじり、/boot/Image (good) で
#         起動する (why=load。systemd-boot は起動しなおすので unfinished)
#   hang  AIOS_TEST_HANG=1 のもの (aiosd の前で止まる) → --reset-after でリセットし、/boot/Image (good) で起動する
#         (why=unfinished)
#   chosen hang をもう一度入れ、起動の一覧 (timeout 5) で「aios」(good) を選ぶ (why=chosen。aiboot だけ)
# どれのあとも /boot/Image は good のまま、試す entry (aios-new+*.conf) と /boot/Image.new は消える
#   test/vm-boot.sh            ディスクの aiboot、aiosd、aios で
#   test/vm-boot.sh --new      このソースの aiboot、aiosd、aios をディスクに入れてから (それらを変えたとき)
#   test/vm-boot.sh --sdboot   ローダーを本物の systemd-boot (Debian の systemd-boot-efi、下の版) にして
#                              (--new といっしょに使える)
# 1 行に ok NAME か FAIL NAME。おしまいに boot: N failed (0 でなければ失敗のステータス)
cd "$(dirname "$0")/.."
new= sdboot=
for a; do
  case "$a" in
    --new) new=1 ;;
    --sdboot) sdboot=1 ;;
    *) echo "usage: test/vm-boot.sh [--new] [--sdboot]" >&2; exit 2 ;;
  esac
done
out=build/vm-boot
# systemd-boot (aarch64)。Debian 13 の systemd-boot-efi から取りだす
SDBOOT_DEB=systemd-boot-efi_257.13-1~deb13u1_arm64.deb
SDBOOT_SHA256=0e4529d9a72b35c31f34151ef8a0df20f322cfef679f4f5439394e60505b91a2
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
# NAME の出力に UNWANTED がない
checknot() {
  name=$1 got=$2 unwanted=$3
  n=$((n + 1))
  case "$got" in
    *"$unwanted"*) echo "FAIL $name: $unwanted in: $got"; fail=$((fail + 1)) ;;
    *) echo "ok $name" ;;
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
if [ -n "$sdboot" ]; then
  efi=$out/systemd-bootaa64.efi
  if [ ! -f "$efi" ]; then
    curl -sSfL -o "$out/$SDBOOT_DEB" "http://deb.debian.org/debian/pool/main/s/systemd/$SDBOOT_DEB" &&
      echo "$SDBOOT_SHA256  $out/$SDBOOT_DEB" | sha256sum -c --quiet &&
      rm -rf "$out/sdboot" && mkdir -p "$out/sdboot" && (cd "$out/sdboot" && ar x "../$SDBOOT_DEB" && tar xf data.tar.*) &&
      cp "$out/sdboot/usr/lib/systemd/boot/efi/systemd-bootaa64.efi" "$efi" || { echo "vm-boot: cannot get systemd-boot" >&2; exit 2; }
  fi
  vm put "$efi" "$dir/systemd-bootaa64.efi" &&
    vm run "sudo install -m644 $dir/systemd-bootaa64.efi /boot/EFI/BOOT/BOOTAA64.EFI" >/dev/null || exit 2
fi

# install KERNEL → reboot (reset_after があれば、それだけ待ってリセット) → いまのカーネルと boot.last
# ($(try ...) の中で動くので、しくじりは数えずに出力に書く: あとの check が落ちる)
try() {
  vm run "aios install kernel $dir/$1" -t 50 >/dev/null || { echo "install $1 failed"; return; }
  vm reboot -t 180 ${2:+--reset-after "$2"} >/dev/null || { echo "no prompt after rebooting with $1"; return; }
  state
}
# 「uname -r | boot.last.result | boot.last.why | new=boot.new | ls /boot/Image*」(aios get は 1 つの値なら値だけを出す)
state() {
  vm run 'echo "$(uname -r) | $(aios get boot.last.result) | $(aios get boot.last.why 2>/dev/null) | new=$(aios get boot.new) | $(cd /boot && echo Image*)"' -t 30
}
# 起動のあと、試すものが残っていない (/boot/Image.new も)
clean="new=null | Image Image.prev"

# 入れたあと: 試す entry がある (systemd-boot の名前の決まり)
vm run "aios install kernel $dir/Image-boot-good" -t 50 >/dev/null
r=$(vm run 'ls /boot/loader/entries; aios get boot.new; aios get boot.try' -t 30)
check entry "$r" "aios-new+1.conf"
vm run "aios install kernel --revert" -t 50 >/dev/null
r=$(state)
check revert "$r" "new=null | Image"
checknot revert-new "$r" "Image.new"

r=$(try Image-boot-good)
check good "$r" "boot-good | the new kernel booted | "
check good-clean "$r" "$clean"
r=$(vm run "cmp /boot/Image $dir/Image-boot-good && echo same")
check good-image "$r" same
# systemd-boot は読めなければ起動しなおす (リセットを待つ) ので、試したのに来なかった (unfinished) になる
if [ -n "$sdboot" ]; then
  load_res="did not finish booting" load_why=unfinished
else
  load_res="could not be loaded" load_why=load
fi
r=$(try Image-load ${sdboot:+40})
check load "$r" "boot-good | the new kernel $load_res"
check load-why "$r" "| $load_why |"
check load-clean "$r" "$clean"
r=$(try Image-boot-hang 30)
check hang "$r" "boot-good | the new kernel did not finish booting"
check hang-why "$r" "| unfinished |"
check hang-clean "$r" "$clean"
# chosen: 起動の一覧で「aios」(いまの /boot/Image) を選ぶ (loader.conf に timeout 5 を足し、一覧が出たらシリアル
# から番号を押す)。新しいものは hang なので、選べなければ止まって unfinished になる。systemd-boot の一覧は
# シリアルでは読めないので aiboot だけ
if [ -z "$sdboot" ]; then
  vm run "sudo cp /boot/loader/loader.conf $dir/loader.conf.orig 2>/dev/null; { grep -v '^timeout' $dir/loader.conf.orig 2>/dev/null; echo 'timeout 5'; } > $dir/loader.conf && sudo install -m644 $dir/loader.conf /boot/loader/loader.conf" >/dev/null
  vm run "aios install kernel $dir/Image-boot-hang" -t 50 >/dev/null
  c=$(vm reboot -t 180 --reset-after 90 --choose aios)
  check chosen-menu "$c" '"chose": "aios"'
  r=$(state)
  check chosen "$r" "boot-good | another entry was chosen"
  check chosen-why "$r" "| chosen |"
  check chosen-clean "$r" "$clean"
  vm run "if [ -f $dir/loader.conf.orig ]; then sudo install -m644 $dir/loader.conf.orig /boot/loader/loader.conf; else sudo rm -f /boot/loader/loader.conf; fi" >/dev/null
fi
r=$(vm run "cmp /boot/Image $dir/Image-boot-good && echo same")
check image-good "$r" same

echo "boot: $n checks, $fail failed"
[ "$fail" = 0 ]
