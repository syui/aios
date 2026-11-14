#!/bin/sh
# GPU の道 (DRM と virtio-gpu の 3D) の Linux の答え: Alpine (aarch64、musl) の linux-virt と Mesa を、QEMU の
# virtio-gpu-gl-device (virgl) で起こし、DRM を使うプログラムの strace と /sys を集める (aios の DRM を作るときの手本)。
#   test/drm-linux.sh [OUT]      既定 build/drm-linux。OUT/t/ に NAME.st (strace) と NAME.out、sys-*.txt、dmesg.txt
# 動かすもの: drmdevice (libdrm のデバイスさがし: /sys)、modetest (KMS の問い合わせ)、kmscube (Alpine の Mesa: llvmpipe と
# dumb buffer で KMS)、aios の Mesa (repo/aarch64/c の mesa と依存を /opt/c に) で egldraw (test/egldraw.c: 画面なしに
# virgl で描いて読み返す。LIBGL_ALWAYS_SOFTWARE=1 で llvmpipe も)、eglinfo、kmscube (virgl で描いて KMS に出す)、
# drmrender と drmkms と drmatomic (test/drmrender.c、test/drmkms.c、test/drmatomic.c: render の口と KMS と atomic の ioctl の
# 答え。aios の VM の答えとくらべる)
# 要るもの: qemu-system-aarch64 (virtio-gpu-gl-device)、xvfb-run (QEMU の SDL の GL で virglrenderer に描かせる。この
# マシンに GPU がなければホストの llvmpipe で: 速さは見られない)、bsdtar、curl、python3、zig (build/zig)
# Alpine のパッケージは APKINDEX から依存を解いて、tar で広げるだけ (apk も install のスクリプトも動かさない)。
# 気をつけること: QEMU の virtio-mmio はふつう legacy で見せるが、Linux の virtio_gpu は VERSION_1 がないと何も言わずに
# ENODEV で断る (force-legacy=false)。Alpine の aarch64 の Mesa には virgl がない (virtio_gpu: driver missing) ので、virgl は
# aios の Mesa で見る
set -e
top=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$top/build/drm-linux}
alpine=${ALPINE:-https://dl-cdn.alpinelinux.org/alpine/v3.24}
mkdir -p "$out"
cd "$out"

# 1. Alpine のパッケージ (依存を解いて) を rootfs に
python3 -I - "$alpine" > urls.txt <<'EOF'
import io, re, sys, tarfile, urllib.request
base = sys.argv[1]
pkgs, prov = {}, {}
for repo in ("main", "community"):
    data = urllib.request.urlopen(f"{base}/{repo}/aarch64/APKINDEX.tar.gz", timeout=60).read()
    idx = tarfile.open(fileobj=io.BytesIO(data)).extractfile("APKINDEX").read().decode()
    for block in idx.split("\n\n"):
        f = {}
        for l in block.splitlines():
            if len(l) > 2 and l[1] == ":":
                f.setdefault(l[0], l[2:])
        if "P" not in f:
            continue
        f["repo"] = repo
        pkgs.setdefault(f["P"], f)
        prov.setdefault(f["P"], f["P"])
        for p in f.get("p", "").split():
            prov.setdefault(re.split(r"[=<>~]", p)[0], f["P"])
want = "busybox busybox-binsh musl linux-virt mesa-dri-gallium mesa-egl mesa-gles mesa-gbm mesa-utils mesa-demos kmscube libdrm-tests strace".split()
seen = []
while want:
    n = re.split(r"[=<>~]", want.pop())[0]
    if n.startswith("!") or n not in prov or prov[n] in seen:
        continue
    seen.append(prov[n])
    want.extend(pkgs[prov[n]].get("D", "").split())
for n in seen:
    print(f"{base}/{pkgs[n]['repo']}/aarch64/{n}-{pkgs[n]['V']}.apk")
EOF
rm -rf apks rootfs
mkdir -p apks rootfs
(cd apks && xargs -n 8 -P 4 curl -fsSL --remote-name-all < ../urls.txt)
for f in apks/*.apk; do
  tar -xzif "$f" -C rootfs --exclude='.PKGINFO' --exclude='.SIGN.*' --exclude='.pre-*' --exclude='.post-*' --exclude='.trigger' 2>/dev/null
done
kver=$(ls rootfs/lib/modules)
# busybox の insmod で読めるように、モジュールを広げておく (modules.dep の名前も)
find "rootfs/lib/modules/$kver" -name '*.ko.gz' -exec gunzip -q {} +
sed -i 's/\.ko\.gz/.ko/g' "rootfs/lib/modules/$kver/modules.dep"
rm -rf rootfs/boot/dtbs-virt

# 2. aios の Mesa (virgl) と依存を /opt/c に、egldraw
mkdir -p rootfs/opt/c/lib rootfs/usr/local/bin aios
for p in mesa llvm libdrm expat zlib wayland libffi; do
  f=$(ls "$top"/repo/aarch64/c/$p-[0-9]*-aarch64.pkg.tar.zst | sort -V | tail -1)
  tar --zstd -xf "$f" -C aios
done
cp -a aios/opt/c/lib/*.so* aios/opt/c/lib/gbm rootfs/opt/c/lib/
ln -sf /lib/ld-musl-aarch64.so.1 rootfs/opt/c/lib/libc.so
"$top"/build/zig/zig-*/zig cc -target aarch64-linux-musl -O2 -I aios/opt/c/include -o rootfs/usr/local/bin/egldraw \
  "$top"/test/egldraw.c -L aios/opt/c/lib -lEGL -lGLESv2 -Wl,--allow-shlib-undefined -Wl,-rpath,/opt/c/lib
for t in drmrender drmkms drmatomic; do
  "$top"/build/zig/zig-*/zig cc -target aarch64-linux-musl -O2 -static -o rootfs/usr/local/bin/$t "$top"/test/$t.c
done

# 3. init: virtio のモジュールを依存の順に読み、集めたものを 2 つめのディスク (tar) に書く
cat > rootfs/init <<EOF
#!/bin/busybox sh
/bin/busybox --install -s
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin HOME=/root
mkdir -p /proc /sys /dev /tmp /root
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp
M=/lib/modules/$kver
EOF
cat >> rootfs/init <<'EOF'
load() {
  line=$(grep "^$1:" $M/modules.dep)
  deps=""
  for d in ${line#*:}; do deps="$d $deps"; done
  for d in $deps $1; do busybox insmod $M/$d 2>/dev/null; done
}
for m in kernel/drivers/virtio/virtio_mmio.ko kernel/drivers/block/virtio_blk.ko kernel/drivers/gpu/drm/virtio/virtio-gpu.ko; do load $m; done
sleep 3
mkdir -p /tmp/t && cd /tmp/t
dmesg > dmesg.txt
ls -l /dev/dri > dev.txt 2>&1
find /sys/class/drm/ /sys/dev/char/ -maxdepth 1 > sys-list.txt 2>&1
for d in /sys/class/drm/* /sys/class/drm/card0/device /sys/class/drm/renderD128/device; do echo "== $d"; ls -l $d/; cat $d/uevent $d/dev $d/modalias 2>/dev/null; done > sys-drm.txt 2>&1
ST="strace -f -v -s 512 -yy -X verbose"
A="env LD_LIBRARY_PATH=/opt/c/lib"
run() { name=$1; shift; echo "== $name"; $ST -o $name.st "$@" > $name.out 2>&1; tail -n 3 $name.out; }
run drmrender drmrender
run drmkms drmkms
run drmatomic drmatomic
run drmdevice drmdevice
run modetest modetest -M virtio_gpu -c -e -p
run kmscube-llvmpipe timeout 90 kmscube -c 20
run egldraw $A egldraw
run egldraw-llvmpipe $A LIBGL_ALWAYS_SOFTWARE=1 egldraw
run eglinfo $A eglinfo -B
run kmscube-virgl $A timeout 90 kmscube -c 20
cd /tmp && tar -cf /dev/vda t && sync
echo "== done"
poweroff -f
EOF
chmod +x rootfs/init
bsdtar --format newc -cf - -C rootfs . | gzip -1 > initramfs.gz
dd if=/dev/zero of=out.img bs=1M count=64 2>/dev/null

# 4. 起こす (aarch64 の TCG なので数分)
xvfb-run -a -s "-screen 0 1280x800x24" qemu-system-aarch64 -M virt -cpu cortex-a72 -smp 2 -m 3G \
  -global virtio-mmio.force-legacy=false -kernel rootfs/boot/vmlinuz-virt -initrd initramfs.gz \
  -append "console=ttyAMA0 rdinit=/init" -device virtio-gpu-gl-device -display sdl,gl=on \
  -drive file=out.img,format=raw,if=none,id=o -device virtio-blk-device,drive=o \
  -serial file:serial.log -monitor none -no-reboot -nic none
rm -rf t
tar -xf out.img
grep -E '^== |^FAIL |egldraw:|drmrender:|drmkms:|drmatomic:|renderer:|Rendered|\[drm\] features' serial.log || true
grep -h -E 'renderer|fps|devices' t/*.out || true
echo "drm-linux: $out/t"
