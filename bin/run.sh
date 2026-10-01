#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   bin/run.sh KERNEL   そのカーネルで (cargo run は作ったばかりの開発用カーネルを渡す)
#   bin/run.sh          disk.img から起動する:
#                         GPT (bin/mkdisk.sh の既定) なら UEFI のファームウェア (AAVMF / edk2) で
#                         ESP の EFI/BOOT/BOOTAA64.EFI を。ファームウェアがないか AIOS_CMDLINE が
#                         あるときは ESP の /Image を直に (mtools が要る)
#                         区画なしの ext4 なら /boot/Image を (debugfs が要る)
#                       どれもなければ開発用カーネルで
#   AIOS_EFI_CODE=FILE  UEFI のファームウェア (既定はよくある場所から探す)。変数は build/efivars.fd
# カーネルは Linux の arm64 Image として渡すので、QEMU は DTB を x0 に入れてくれる
# (ELF なら Image に変える)。AIOS_CMDLINE はカーネルのコマンドライン (例: init=/bin/sh)
# ネットワークは QEMU の user (DHCP)。disk.img (bin/mkdisk.sh で作る) があればルートにする
# AIOS_SMP=N で CPU の数 (既定 4。ラズパイ 3B はいつも 4)
# AIOS_MACHINE=raspi3b でラズパイ 3B (DTB は build/rpi/ に取ってくる。sd.img (bin/mksd.sh) があれば SD カード、ネットワークなし)
dev=target/aarch64-unknown-none-softfloat/debug/aios
k=$1
case "$k" in "" | /*) ;; *) k="$PWD/$k" ;; esac
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
uefi=
gpt=
[ -f disk.img ] && [ "$(dd if=disk.img bs=1 skip=512 count=8 2>/dev/null)" = "EFI PART" ] && gpt=1
if [ -z "$k" ] && [ -n "$gpt" ]; then
  if [ -z "$AIOS_CMDLINE" ]; then
    for f in "$AIOS_EFI_CODE" /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd /usr/share/AAVMF/AAVMF_CODE.fd \
      /usr/share/edk2/aarch64/QEMU_CODE.fd /usr/share/edk2-armvirt/aarch64/QEMU_CODE.fd /usr/share/qemu/edk2-aarch64-code.fd \
      /opt/homebrew/share/qemu/edk2-aarch64-code.fd /usr/local/share/qemu/edk2-aarch64-code.fd; do
      [ -n "$f" ] && [ -f "$f" ] && { uefi=$f; break; }
    done
  fi
  if [ -z "$uefi" ] && command -v mcopy >/dev/null; then
    # 区画 1 (ESP) の始まり: GPT の区画表 (LBA 2) の最初の項目の +32
    start=$(od -An -tu8 -j $((1024 + 32)) -N 8 disk.img | tr -d ' ')
    if mcopy -n -i "disk.img@@$((start * 512))" ::/Image "$tmp/boot" 2>/dev/null && [ -s "$tmp/boot" ]; then
      echo "boot: /boot/Image from the ESP of disk.img" >&2
      k=$tmp/boot
    fi
  fi
fi
if [ -z "$k" ] && [ -z "$uefi" ] && [ -z "$gpt" ] && [ -f disk.img ] && command -v debugfs >/dev/null; then
  for f in /boot/Image /boot/aios; do
    if debugfs -R "dump $f $tmp/boot" disk.img >/dev/null 2>&1 && [ -s "$tmp/boot" ]; then
      echo "boot: $f from disk.img" >&2
      k=$tmp/boot
      break
    fi
  done
fi
if [ -z "$uefi" ]; then [ -n "$k" ] && [ -f "$k" ] || k=$dev; fi
# ELF なら中身だけの Image に
if [ "$(head -c 4 "$k" | od -An -c | tr -d ' ')" = '177ELF' ]; then
  objcopy=$(command -v llvm-objcopy || command -v rust-objcopy || command -v aarch64-linux-gnu-objcopy)
  # rustup component add llvm-tools の llvm-objcopy (Mac など)
  if [ -z "$objcopy" ] && command -v rustc >/dev/null; then
    for f in "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy; do
      [ -x "$f" ] && objcopy=$f
    done
  fi
  if [ -n "$objcopy" ] && "$objcopy" -O binary "$k" "$tmp/Image"; then
    k=$tmp/Image
  fi
fi
if [ "${AIOS_MACHINE:-virt}" = raspi3b ]; then
  dtb=build/rpi/bcm2710-rpi-3-b.dtb
  if [ ! -f "$dtb" ]; then
    mkdir -p build/rpi
    curl -fsSL -o "$dtb" https://raw.githubusercontent.com/raspberrypi/firmware/master/boot/bcm2710-rpi-3-b.dtb || exit 1
  fi
  sd=
  [ -f sd.img ] && sd="-drive if=sd,file=sd.img,format=raw"
  # shellcheck disable=SC2086
  qemu-system-aarch64 -M raspi3b -serial stdio -display none -dtb "$dtb" $sd -kernel "$k" -append "${AIOS_CMDLINE:-}"
  exit
fi
set -- -netdev user,id=n0 -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f disk.img ]; then
  set -- "$@" \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
if [ -n "$uefi" ]; then
  # ACPI を切ると、ファームウェアは QEMU の DTB を渡してくれる
  vars=build/efivars.fd
  if [ ! -f "$vars" ]; then
    mkdir -p build
    tmpl=$(echo "$uefi" | sed 's/CODE\(\.[a-z-]*\)\{0,1\}\.fd$/VARS.fd/; s/-code\.fd$/-vars.fd/')
    if [ "$tmpl" != "$uefi" ] && [ -f "$tmpl" ]; then
      cp "$tmpl" "$vars"
    else
      # truncate のない Mac でも
      dd if=/dev/zero of="$vars" bs=1 count=0 seek="$(wc -c < "$uefi" | tr -d ' ')" 2>/dev/null
    fi
  fi
  echo "boot: UEFI ($uefi) from disk.img" >&2
  qemu-system-aarch64 -machine virt,acpi=off -cpu cortex-a72 -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-512M}" -nographic "$@" \
    -drive if=pflash,format=raw,readonly=on,file="$uefi" -drive if=pflash,format=raw,file="$vars"
  exit
fi
qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-512M}" -nographic "$@" \
  -kernel "$k" -append "${AIOS_CMDLINE:-}"
