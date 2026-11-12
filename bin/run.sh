#!/bin/sh
dev=target/aarch64-unknown-none-softfloat/debug/aios
disk=${AIOS_DISK:-aios.img}
k=$1
case "$k" in "" | /*) ;; *) k="$PWD/$k" ;; esac
if [ -n "$k" ] && [ ! -f "$k" ]; then
  echo "./bin/run.sh: カーネル $1 がありません (引数なしなら aios.img か aios-aarch64.img.zst から起動)" >&2
  exit 1
fi
cd "$(dirname "$0")/.."
[ -z "$AIOS_DISK" ] && [ ! -f "$disk" ] && [ -f disk.img ] && disk=disk.img
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -z "$k" ] && [ ! -f "$disk" ]; then
  for z in aios-aarch64.img.zst aios-unix-aarch64.img.zst; do
    [ -f "$z" ] || continue
    echo "$disk: $z を展開します" >&2
    zstd -d -q "$z" -o "$disk" || exit 1
    break
  done
fi
uefi=
gpt=
[ -f "$disk" ] && [ "$(dd if="$disk" bs=1 skip=512 count=8 2>/dev/null)" = "EFI PART" ] && gpt=1
if [ -z "$k" ] && [ -n "$gpt" ]; then
  if [ -z "$AIOS_CMDLINE" ]; then
    for f in "$AIOS_EFI_CODE" /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd /usr/share/AAVMF/AAVMF_CODE.fd \
      /usr/share/edk2/aarch64/QEMU_CODE.fd /usr/share/edk2-armvirt/aarch64/QEMU_CODE.fd /usr/share/qemu/edk2-aarch64-code.fd \
      /opt/homebrew/share/qemu/edk2-aarch64-code.fd /usr/local/share/qemu/edk2-aarch64-code.fd; do
      [ -n "$f" ] && [ -f "$f" ] && { uefi=$f; break; }
    done
  fi
  if [ -z "$uefi" ] && command -v mcopy >/dev/null; then
    start=$(od -An -tu8 -j $((1024 + 32)) -N 8 "$disk" | tr -d ' ')
    if mcopy -n -i "$disk@@$((start * 512))" ::/Image "$tmp/boot" 2>/dev/null && [ -s "$tmp/boot" ]; then
      echo "boot: /boot/Image from the ESP of $disk" >&2
      k=$tmp/boot
    fi
  fi
fi
if [ -z "$k" ] && [ -z "$uefi" ] && [ -z "$gpt" ] && [ -f "$disk" ] && command -v debugfs >/dev/null; then
  for f in /boot/Image /boot/aios; do
    if debugfs -R "dump $f $tmp/boot" "$disk" >/dev/null 2>&1 && [ -s "$tmp/boot" ]; then
      echo "boot: $f from $disk" >&2
      k=$tmp/boot
      break
    fi
  done
fi
if [ -z "$uefi" ]; then [ -n "$k" ] && [ -f "$k" ] || k=$dev; fi
if [ -z "$uefi" ] && [ ! -f "$k" ]; then
  echo "./bin/run.sh: 起動するものがありません。どれかを:" >&2
  echo "  curl -fLO https://git.syui.ai/ai/os/releases/download/latest/aios-aarch64.img.zst" >&2
  echo "  bin/mkrootfs.sh -r all && cargo run" >&2
  exit 1
fi
if [ -n "$k" ] && [ "$(head -c 4 "$k" | od -An -c | tr -d ' ')" = '177ELF' ]; then
  objcopy=$(command -v llvm-objcopy || command -v rust-objcopy || command -v aarch64-linux-gnu-objcopy)
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
  qemu-system-aarch64 -M raspi3b -serial stdio -display none -dtb "$dtb" $sd -kernel "$k" -append "${AIOS_CMDLINE:-}"
  exit
fi
accel=${AIOS_ACCEL:-}
if [ -z "$accel" ] && [ "$(uname -s)" = Darwin ] && [ "$(sysctl -n kern.hv_support 2>/dev/null)" = 1 ]; then
  accel=hvf
fi
case "$accel" in
  hvf) cpu="-accel hvf -cpu host"; machine="virt,gic-version=3,highmem=off" ;;
  kvm) cpu="-accel kvm -cpu host"; machine="virt,gic-version=host" ;;
  *) cpu="-cpu ${AIOS_CPU:-cortex-a72}"; machine="virt${AIOS_GIC:+,gic-version=$AIOS_GIC}" ;;
esac
[ -n "$accel" ] && [ "$accel" != tcg ] && echo "accel: $accel" >&2
ssh_port=${AIOS_SSH:-2222}
fwd=
if [ "$ssh_port" != 0 ]; then
  if command -v nc >/dev/null 2>&1 && nc -z 127.0.0.1 "$ssh_port" 2>/dev/null; then
    echo "ssh: 127.0.0.1:$ssh_port は使われているので転送しません (AIOS_SSH=PORT でほかの番号に)" >&2
  else
    fwd=",hostfwd=tcp:127.0.0.1:$ssh_port-:22"
  fi
fi
if [ -n "${AIOS_FWD:-}" ]; then
  fwd="$fwd,hostfwd=tcp:127.0.0.1:${AIOS_FWD%%,*}-:7001,hostfwd=udp:127.0.0.1:${AIOS_FWD#*,}-:7002"
fi
set -- -netdev "user,id=n0$fwd" -device virtio-net-device,netdev=n0 -global virtio-mmio.force-legacy=false
if [ -f "$disk" ]; then
  set -- "$@" \
    -drive file="$disk",if=none,format=raw,id=hd0${AIOS_DISK_OPTS:+,$AIOS_DISK_OPTS} \
    -device virtio-blk-device,drive=hd0
fi
set -- "$@" -device virtio-rng-device
out=-nographic
if [ -n "$AIOS_DISPLAY" ]; then
  disp=$AIOS_DISPLAY
  if [ "$disp" = 1 ]; then
    if [ "$(uname -s)" = Darwin ]; then disp=cocoa; else disp=gtk; fi
  fi
  # 画面に GL があれば (-display sdl,gl=on など) 3D のある virtio-gpu (virgl: ホストの GPU で描く。/dev/dri ができる)
  gpu=virtio-gpu-device
  case $disp in *gl=on* | *gl=es* | *gl=core*) gpu=virtio-gpu-gl-device ;; esac
  set -- "$@" -device $gpu -device virtio-keyboard-device -device virtio-tablet-device
  out="-display $disp -serial mon:stdio"
  echo "display: $disp ($gpu, keyboard, tablet)" >&2
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
set -- "$@" ${AIOS_QEMU_ARGS:-}
if [ -n "$uefi" ]; then
  vars=build/efivars.fd
  if [ ! -f "$vars" ]; then
    mkdir -p build
    tmpl=$(echo "$uefi" | sed 's/CODE\(\.[a-z-]*\)\{0,1\}\.fd$/VARS.fd/; s/-code\.fd$/-vars.fd/')
    if [ "$tmpl" != "$uefi" ] && [ -f "$tmpl" ]; then
      cp "$tmpl" "$vars"
    else
      dd if=/dev/zero of="$vars" bs=1 count=0 seek="$(wc -c < "$uefi" | tr -d ' ')" 2>/dev/null
    fi
  fi
  echo "boot: UEFI ($uefi) from $disk" >&2
  qemu-system-aarch64 -machine "$machine,acpi=off" $cpu -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-2G}" $out "$@" \
    -drive if=pflash,format=raw,readonly=on,file="$uefi" -drive if=pflash,format=raw,file="$vars"
  exit
fi
qemu-system-aarch64 -machine "$machine" $cpu -smp "${AIOS_SMP:-4}" -m "${AIOS_MEM:-2G}" $out "$@" \
  -kernel "$k" -append "${AIOS_CMDLINE:-}"
