#!/bin/zsh

set -e

ROOTFS="$(pwd)/root.x86_64"
BUILD_MODE="${1:-tarball}"
BUILD_DATE=$(date +%Y.%m.%d)
GPG_KEY=B6A582E551A2F6181A5CC99E338E6F42F9544D9B

echo "=== aios build $BUILD_DATE (mode: $BUILD_MODE) ==="

rm -rf $ROOTFS
mkdir -p $ROOTFS

# --- rootfs構築 (共通) ---

pacstrap -c $ROOTFS base

# pacman.conf がない場合はホストからコピー
if [[ ! -f $ROOTFS/etc/pacman.conf ]]; then
  cp /etc/pacman.conf $ROOTFS/etc/pacman.conf
fi

echo 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' > $ROOTFS/etc/pacman.d/mirrorlist
sed -i 's/CheckSpace/#CheckSpace/' $ROOTFS/etc/pacman.conf
sed -i '/\[options\]/a NoUpgrade = etc/os-release' $ROOTFS/etc/pacman.conf

arch-chroot $ROOTFS /bin/sh -c 'pacman-key --init && pacman-key --populate archlinux'
arch-chroot $ROOTFS /bin/sh -c 'pacman -Syu --noconfirm base-devel vim git zsh rust clang openssh jq nodejs npm zsh-autosuggestions zsh-syntax-highlighting zsh-history-substring-search uutils-coreutils sudo-rs'

if [[ "$BUILD_MODE" == "image" ]]; then
  arch-chroot $ROOTFS /bin/sh -c 'pacman -S --noconfirm linux-aios linux-firmware mkinitcpio'
fi

arch-chroot $ROOTFS /bin/sh -c 'npm i -g @anthropic-ai/claude-code'

cat >> $ROOTFS/etc/pacman.conf <<'EOF'

[aios]
SigLevel = Required DatabaseOptional
Server = https://git.syui.ai/ai/repo/raw/branch/main/$arch
EOF

mv cfg/aios.gpg $ROOTFS/aios.gpg
arch-chroot $ROOTFS /bin/sh -c "
  pacman-key --add /aios.gpg
  pacman-key --lsign-key $GPG_KEY
  rm /aios.gpg
"
# [aios] repo is self-hosted (git.syui.ai) and intermittently stalls from CI
# runners (pacman aborts at <1B/s for 10s); retry before giving up.
synced=0
for i in 1 2 3 4 5; do
  if arch-chroot $ROOTFS /bin/sh -c 'pacman -Sy --noconfirm ailog aigpt aishell'; then
    synced=1; break
  fi
  echo "aios repo sync failed ($i/5), retrying in 10s..."
  sleep 10
done
[[ $synced == 1 ]] || { echo "error: aios repo unreachable after 5 attempts"; exit 1; }

arch-chroot $ROOTFS /bin/sh -c 'chsh -s /bin/zsh'
arch-chroot $ROOTFS /bin/sh -c 'useradd -m -G wheel -s /bin/zsh ai'
# no baked password: console autologin + NOPASSWD sudo only. Lock password login
# (su from root via `machinectl shell` and ssh-key login still work).
arch-chroot $ROOTFS /bin/sh -c 'passwd -l ai'
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
IMAGE_VERSION=0.0.1
HOME_URL=https://git.syui.ai/ai/os
EOF

echo "aios" > $ROOTFS/etc/hostname

# --- Rust userland overlay (uutils coreutils + sudo-rs) ---
# GNU 版は /usr/bin に温存し、/usr/local/bin へ Rust 版の symlink を張る。PATH も
# sudo secure_path も /usr/local/bin が先なので Rust 版が既定になる(image 内で
# `rustify-coreutils revert` で GNU に戻せる)。最後に置くことでビルド処理自体は
# 全て GNU で完了させ、Rust 依存を持ち込まない。
# ※ image モードでは後続の image.sh が mkinitcpio を Rust coreutils 上で走らせる
#   (tarball モードはこの後 host 側の tar のみなので無影響)。
install -Dm755 cfg/rustify-coreutils.sh $ROOTFS/usr/local/bin/rustify-coreutils
arch-chroot $ROOTFS /usr/local/bin/rustify-coreutils apply

# --- 出力 ---

if [[ "$BUILD_MODE" == "image" ]]; then
  bash cfg/image.sh $ROOTFS
  echo "=== build complete: aios.img ==="
else
  tar czf aios.tar.gz -C $ROOTFS .
  echo "=== build complete: aios.tar.gz ==="
fi
