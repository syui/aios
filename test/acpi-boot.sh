#!/bin/sh
# UEFI と ACPI (DTB なし) でカーネルを起こす: test/acpi-boot.sh [KERNEL] [秒]
#   aios.img を疎なまま build/acpi-boot/disk.img に写し (aios.img が新しくなったときだけ)、その ESP の /Image (aiboot が
#   読むもの) を KERNEL (ELF。既定 build/aish-vm/kernels/work) から作ったものにかえて、QEMU の virt,acpi=on と AAVMF
#   (EDK2) で起こす。ファームウェアは ACPI の表をわたし、DTB はわたさない (ほんものの UEFI の機械と同じ)。ディスクは
#   -snapshot。シリアルは build/acpi-boot.log に、終わりを出す (AIOS_ACPI_TAIL 行。GIC の版は AIOS_GIC)
#   AIOS_ACPI_CMD があれば、プロンプトが出たらそれを打つ (そのあと 5 秒待つ)
set -e
top=$(cd "$(dirname "$0")/.." && pwd)
cd "$top"
k=${1:-build/aish-vm/kernels/work}
secs=${2:-60}
code=
for f in "$AIOS_EFI_CODE" /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd /usr/share/AAVMF/AAVMF_CODE.fd /usr/share/qemu/edk2-aarch64-code.fd; do
  [ -n "$f" ] && [ -f "$f" ] && { code=$f; break; }
done
[ -n "$code" ] || { echo "acpi-boot: no UEFI firmware (AAVMF / edk2-aarch64)" >&2; exit 2; }
mkdir -p build/acpi-boot
objcopy=$(command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy)
"$objcopy" -O binary "$k" build/acpi-boot/Image
disk=build/acpi-boot/disk.img
if [ ! -f "$disk" ] || [ aios.img -nt "$disk" ]; then
  cp --sparse=always aios.img "$disk"
fi
start=$(od -An -tu8 -j $((1024 + 32)) -N 8 "$disk" | tr -d ' ')
mcopy -o -i "$disk@@$((start * 512))" build/acpi-boot/Image ::/Image
vars=build/acpi-boot/vars.fd
tmpl=$(echo "$code" | sed 's/CODE\(\.[a-z-]*\)\{0,1\}\.fd$/VARS.fd/; s/-code\.fd$/-vars.fd/')
if [ -f "$tmpl" ] && [ "$tmpl" != "$code" ]; then cp "$tmpl" "$vars"; else dd if=/dev/zero of="$vars" bs=1 count=0 seek="$(wc -c < "$code" | tr -d ' ')" 2>/dev/null; fi
log=build/acpi-boot.log
rm -f "$log"
set -- -machine "virt,acpi=on${AIOS_GIC:+,gic-version=$AIOS_GIC}" -cpu "${AIOS_CPU:-cortex-a72}" -smp "${AIOS_SMP:-2}" -m "${AIOS_MEM:-2G}" \
  -nographic -no-reboot -drive if=pflash,format=raw,readonly=on,file="$code" -drive if=pflash,format=raw,file="$vars" \
  -drive file="$disk",if=none,format=raw,id=root,snapshot=on -device virtio-blk-device,drive=root \
  -global virtio-mmio.force-legacy=false -netdev user,id=n0 -device virtio-net-device,netdev=n0 -device virtio-rng-device
if [ -n "$AIOS_ACPI_CMD" ]; then
  python3 - "$secs" "$log" "$AIOS_ACPI_CMD" qemu-system-aarch64 "$@" <<'EOF'
import os, select, subprocess, sys, time
secs, log, cmd, argv = float(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4:]
p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
out, sent, end = b'', None, time.time() + secs
while time.time() < end and p.poll() is None:
    if select.select([p.stdout], [], [], 0.2)[0]:
        out += os.read(p.stdout.fileno(), 65536)
    if sent is None and (b'$ ' in out[-200:] or b'# ' in out[-200:]) and b'login' not in out[-200:].lower():
        p.stdin.write(cmd.encode() + b'\r'); p.stdin.flush(); sent = time.time(); end = min(end, sent + 5)
p.kill()
open(log, 'wb').write(out)
EOF
else
  timeout "$secs" qemu-system-aarch64 "$@" > "$log" 2>&1 < /dev/null || true
fi
tr -d '\r' < "$log" | grep -av '^\s*$' | sed 's/\x1b\[[0-9;=?]*[A-Za-z]//g' | tail -n "${AIOS_ACPI_TAIL:-40}"
