#!/bin/zsh

set -e

ROOTFS="root.x86_64"
OUTPUT="aios.tar.gz"
BUILD_DATE=$(date +%Y.%m.%d)

echo "=== aios build $BUILD_DATE ==="

rm -rf $ROOTFS
rm -f $OUTPUT
mkdir -p $ROOTFS

pacstrap -c $ROOTFS base

echo 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch
Server = http://mirrors.cat.net/archlinux/$repo/os/$arch' > $ROOTFS/etc/pacman.d/mirrorlist
sed -i 's/CheckSpace/#CheckSpace/' $ROOTFS/etc/pacman.conf

arch-chroot $ROOTFS /bin/sh -c 'pacman-key --init && pacman-key --populate archlinux'
arch-chroot $ROOTFS /bin/sh -c 'pacman -Syu --noconfirm base-devel vim git zsh rust openssh jq nodejs npm'
arch-chroot $ROOTFS /bin/sh -c 'npm i -g @anthropic-ai/claude-code'

arch-chroot $ROOTFS /bin/sh -c 'useradd -m -G wheel -s /bin/zsh ai'
arch-chroot $ROOTFS /bin/sh -c 'echo "ai:ai" | chpasswd'
echo "ai ALL=(ALL:ALL) NOPASSWD: ALL" >> $ROOTFS/etc/sudoers

mkdir -p $ROOTFS/etc/systemd/system/console-getty.service.d
cat > $ROOTFS/etc/systemd/system/console-getty.service.d/override.conf <<'EOF'
[Service]
ExecStart=
ExecStart=-/usr/bin/agetty --autologin ai --noclear --keep-baud console 115200,38400,9600 $TERM
EOF

cp cfg/zshrc $ROOTFS/home/ai/.zshrc
arch-chroot $ROOTFS /bin/sh -c 'chown ai:ai /home/ai/.zshrc'

cat > $ROOTFS/etc/os-release <<EOF
NAME=aios
PRETTY_NAME=aios
ID=ai
ID_LIKE=arch
BUILD_ID=rolling
IMAGE_ID=aios
IMAGE_VERSION=$BUILD_DATE
HOME_URL=https://git.syui.ai/ai/os
EOF

echo "aios" > $ROOTFS/etc/hostname

tar czf $OUTPUT -C $ROOTFS .

echo "=== build complete: $OUTPUT ==="
