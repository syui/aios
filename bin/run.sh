#!/bin/sh
# QEMU で aios を起動する。cargo run からも呼ばれる
#   bin/run.sh KERNEL   そのカーネルで (cargo run は作ったばかりの開発用カーネルを渡す)
#   bin/run.sh          disk.img から起動する:
#                         GPT (bin/mkdisk.sh の既定) なら UEFI のファームウェア (AAVMF / edk2) で
#                         ESP の EFI/BOOT/BOOTAA64.EFI を。ファームウェアがないか AIOS_CMDLINE が
#                         あるときは ESP の /Image を直に (mtools が要る)
#                         区画なしの ext4 なら /boot/Image を (debugfs が要る)
#                       どれもなければ開発用カーネルで
#                       disk.img がなく aios-unix-aarch64.img.zst (リリース unix-latest) があれば、展開して使う:
#                         git clone -b unix https://git.syui.ai/ai/os aios && cd aios
#                         curl -fLO https://git.syui.ai/ai/os/releases/download/unix-latest/aios-unix-aarch64.img.zst
#                         (GitHub の https://github.com/syui/aios/releases/download/unix-latest/ にも同じもの)
#                         ./bin/run.sh    (Mac: brew install qemu zstd)
#   AIOS_EFI_CODE=FILE  UEFI のファームウェア (既定はよくある場所から探す)。変数は build/efivars.fd
# カーネルは Linux の arm64 Image として渡すので、QEMU は DTB を x0 に入れてくれる
# (ELF なら Image に変える)。AIOS_CMDLINE はカーネルのコマンドライン (例: init=/bin/sh)
# ネットワークは QEMU の user (DHCP)。disk.img (bin/mkdisk.sh で作る) があればルートにする
# AIOS_SMP=N で CPU の数 (既定 4。ラズパイ 3B はいつも 4)
# AIOS_MEM=SIZE でメモリ (既定 2G。Claude Code (Bun) は 512M では足りない)
# AIOS_DISPLAY=1 で画面の窓を出す (virtio-gpu と virtio のキーボード・タブレット。Mac は cocoa、
#   ほかは gtk)。AIOS_DISPLAY=cocoa / gtk / sdl / none で選べる。シリアル (この端末) もそのまま使える
#   音 (virtio-sound、QEMU 8.2 から) もつける: Mac は coreaudio、ほかは pipewire / pa / alsa のあるもの。
#   AIOS_SOUND=coreaudio / pipewire / pa / alsa / none (鳴らさない) で選べる。AIOS_SOUND=0 で音の装置なし
# AIOS_QEMU_ARGS で QEMU に引数を足せる
# AIOS_SSH=PORT でホストの 127.0.0.1:PORT を aios の 22 (sshd) につなぐ (既定 2222、0 でつながない)
#   ssh -p 2222 ai@127.0.0.1 で入れる (aios で sudo ssh-keygen -A && sudo /opt/c/sbin/sshd)
# Mac (Apple Silicon) では Hypervisor.framework (HVF) で速く動かす (-accel hvf -cpu host、GICv3)。
#   AIOS_ACCEL=tcg でソフトのエミュレーションに、AIOS_ACCEL=kvm で Linux (arm64) の KVM に
#   AIOS_GIC=3 で TCG でも GICv3 に
# AIOS_MACHINE=raspi3b でラズパイ 3B (DTB は build/rpi/ に取ってくる。sd.img (bin/mksd.sh) があれば SD カード、ネットワークなし)
dev=target/aarch64-unknown-none-softfloat/debug/aios
k=$1
case "$k" in "" | /*) ;; *) k="$PWD/$k" ;; esac
# 渡されたカーネルがなければ止まる (zsh で「bin/run.sh  # コメント」と打つと # からが引数になる)
if [ -n "$k" ] && [ ! -f "$k" ]; then
  echo "./bin/run.sh: カーネル $1 がありません (引数なしなら disk.img か aios-unix-aarch64.img.zst から起動)" >&2
  exit 1
fi
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -z "$k" ] && [ ! -f disk.img ] && [ -f aios-unix-aarch64.img.zst ]; then
  echo "disk.img: aios-unix-aarch64.img.zst を展開します" >&2
  zstd -d -q aios-unix-aarch64.img.zst -o disk.img || exit 1
fi
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
if [ -z "$uefi" ] && [ ! -f "$k" ]; then
  echo "./bin/run.sh: 起動するものがありません。どれかを:" >&2
  echo "  curl -fLO https://git.syui.ai/ai/os/releases/download/unix-latest/aios-unix-aarch64.img.zst" >&2
  echo "  bin/mkrootfs.sh -r all && cargo run" >&2
  exit 1
fi
# ELF なら中身だけの Image に
if [ -n "$k" ] && [ "$(head -c 4 "$k" | od -An -c | tr -d ' ')" = '177ELF' ]; then
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
# 速くする: Mac なら HVF (M1 は物理アドレスが 36 bit なので highmem=off)
accel=${AIOS_ACCEL:-}
if [ -z "$accel" ] && [ "$(uname -s)" = Darwin ] && [ "$(sysctl -n kern.hv_support 2>/dev/null)" = 1 ]; then
  accel=hvf
fi
case "$accel" in
  hvf) cpu="-accel hvf -cpu host"; machine="virt,gic-version=3,highmem=off" ;;
  kvm) cpu="-accel kvm -cpu host"; machine="virt,gic-version=host" ;;
  *) cpu="-cpu cortex-a72"; machine="virt${AIOS_GIC:+,gic-version=$AIOS_GIC}" ;;
esac
[ -n "$accel" ] && [ "$accel" != tcg ] && echo "accel: $accel" >&2
ssh_port=${AIOS_SSH:-2222}
fwd=
if [ "$ssh_port" != 0 ]; then
  # もう使われている番号 (ほかの aios がまだ動いているなど) なら、転送せずに起動する
  if command -v nc >/dev/null 2>&1 && nc -z 127.0.0.1 "$ssh_port" 2>/dev/null; then
    echo "ssh: 127.0.0.1:$ssh_port は使われているので転送しません (AIOS_SSH=PORT でほかの番号に)" >&2
  else
    fwd=",hostfwd=tcp:127.0.0.1:$ssh_port-:22"
  fi
fi
set -- -netdev "user,id=n0$fwd" -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f disk.img ]; then
  set -- "$@" \
    -drive file=disk.img,if=none,format=raw,id=hd0 \
    -device virtio-blk-device,drive=hd0
fi
# 画面: あれば窓とシリアル、なければシリアルだけ (-nographic)。
# virtio の装置はディスクのあとに足す (前に足すとディスクの場所が変わり、UEFI が起動の項目を見失う)
out=-nographic
if [ -n "$AIOS_DISPLAY" ]; then
  disp=$AIOS_DISPLAY
  if [ "$disp" = 1 ]; then
    if [ "$(uname -s)" = Darwin ]; then disp=cocoa; else disp=gtk; fi
  fi
  set -- "$@" -device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device
  out="-display $disp -serial mon:stdio"
  echo "display: $disp (virtio-gpu, keyboard, tablet)" >&2
  snd=${AIOS_SOUND:-}
  if [ -z "$snd" ]; then
    if [ "$(uname -s)" = Darwin ]; then snd=coreaudio
    elif command -v pw-cli >/dev/null 2>&1; then snd=pipewire
    elif command -v pactl >/dev/null 2>&1; then snd=pa
    elif [ -d /dev/snd ]; then snd=alsa
    else snd=none
    fi
  fi
  if [ "$snd" != 0 ] && qemu-system-aarch64 -device help 2>/dev/null | grep -q '"virtio-sound-device"'; then
    set -- "$@" -audiodev "$snd,id=snd0" -device virtio-sound-device,audiodev=snd0
    echo "sound: $snd (virtio-sound)" >&2
  fi
fi
# shellcheck disable=SC2086
set -- "$@" ${AIOS_QEMU_ARGS:-}
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
  # shellcheck disable=SC2086
  qemu-system-aarch64 -machine "$machine,acpi=off" $cpu -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-2G}" $out "$@" \
    -drive if=pflash,format=raw,readonly=on,file="$uefi" -drive if=pflash,format=raw,file="$vars"
  exit
fi
# shellcheck disable=SC2086
qemu-system-aarch64 -machine "$machine" $cpu -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-2G}" $out "$@" \
  -kernel "$k" -append "${AIOS_CMDLINE:-}"
