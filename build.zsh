#!/bin/zsh

set -e

ROOTFS="$(pwd)/root.x86_64"
BUILD_MODE="${1:-tarball}"
BUILD_DATE=$(date +%Y.%m.%d)

echo "=== aios build $BUILD_DATE (mode: $BUILD_MODE) ==="

rm -rf $ROOTFS
mkdir -p $ROOTFS

# --- rootfs構築 (共通) ---

pacstrap -c $ROOTFS base

# pacman.conf がない場合はホストからコピー
if [[ ! -f $ROOTFS/etc/pacman.conf ]]; then
  cp /etc/pacman.conf $ROOTFS/etc/pacman.conf
fi

echo 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch
Server = http://mirrors.cat.net/archlinux/$repo/os/$arch' > $ROOTFS/etc/pacman.d/mirrorlist
sed -i 's/CheckSpace/#CheckSpace/' $ROOTFS/etc/pacman.conf

arch-chroot $ROOTFS /bin/sh -c 'pacman-key --init && pacman-key --populate archlinux'
arch-chroot $ROOTFS /bin/sh -c 'pacman -Syu --noconfirm base-devel vim git zsh rust openssh jq nodejs npm zsh-autosuggestions zsh-syntax-highlighting zsh-history-substring-search'

if [[ "$BUILD_MODE" == "image" ]]; then
  arch-chroot $ROOTFS /bin/sh -c 'pacman -S --noconfirm linux linux-firmware mkinitcpio'
fi

arch-chroot $ROOTFS /bin/sh -c 'npm i -g @anthropic-ai/claude-code'

bash cfg/pkg.sh $ROOTFS

arch-chroot $ROOTFS /bin/sh -c 'chsh -s /bin/zsh'
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

mkdir -p $ROOTFS/home/ai/.config/claude
cp cfg/mcp.json $ROOTFS/home/ai/.config/claude/mcp.json
arch-chroot $ROOTFS /bin/sh -c 'chown -R ai:ai /home/ai/.config'

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

# --- 出力 ---

if [[ "$BUILD_MODE" == "image" ]]; then
  bash cfg/image.sh $ROOTFS
  echo "=== build complete: aios.img ==="
else
  tar czf aios.tar.gz -C $ROOTFS .
  echo "=== build complete: aios.tar.gz ==="
fi
