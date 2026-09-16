#!/bin/zsh

set -e

ROOTFS="$(pwd)/root.x86_64"
BUILD_MODE="${1:-tarball}"
BUILD_DATE=$(date +%Y.%m.%d)
GPG_KEY=B6A582E551A2F6181A5CC99E338E6F42F9544D9B

echo "=== aios build $BUILD_DATE (mode: $BUILD_MODE) ==="

rm -rf $ROOTFS
mkdir -p $ROOTFS

pacstrap -c $ROOTFS base

if [[ ! -f $ROOTFS/etc/pacman.conf ]]; then
  cp /etc/pacman.conf $ROOTFS/etc/pacman.conf
fi

echo 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' > $ROOTFS/etc/pacman.d/mirrorlist
sed -i 's/CheckSpace/#CheckSpace/' $ROOTFS/etc/pacman.conf
sed -i '/\[options\]/a NoUpgrade = etc/os-release' $ROOTFS/etc/pacman.conf

arch-chroot $ROOTFS /bin/sh -c 'pacman-key --init && pacman-key --populate archlinux'
arch-chroot $ROOTFS /bin/sh -c 'pacman -Syu --noconfirm vim git zsh rust gcc openssh jq nodejs pnpm uv zsh-autosuggestions zsh-syntax-highlighting zsh-history-substring-search uutils-coreutils sudo-rs ufw'

if [[ "$BUILD_MODE" == "image" ]]; then
  arch-chroot $ROOTFS /bin/sh -c 'pacman -S --noconfirm linux-aios linux-firmware mkinitcpio'
fi

# PNPM_HOME=/usr/local/bin (not a dedicated pnpm dir): global bin shims land directly on
# the default PATH, so `claude` stays reachable even under a minimal PATH (e.g. a
# systemd service's) with no extra symlink step.
#
# pnpm skips packages' postinstall scripts by default (supply-chain safety) — but
# claude-code's postinstall is what actually fetches its native binary, so without it
# `claude` installs but exits with "claude native binary not installed" on every run.
# Find and run it explicitly; its path is content-hashed so can't be hardcoded.
arch-chroot $ROOTFS /bin/sh -c '
  export PNPM_HOME=/usr/local/bin PATH="/usr/local/bin:$PATH"
  pnpm add -g @anthropic-ai/claude-code
  node "$(find /usr/local/bin/store -path "*claude-code/install.cjs" | head -1)"
'

cat >> $ROOTFS/etc/pacman.conf <<'EOF'

[aios]
SigLevel = Required DatabaseOptional
Server = https://git.syui.ai/ai/repo/raw/branch/main/$arch
EOF

cp cfg/aios.gpg $ROOTFS/aios.gpg
arch-chroot $ROOTFS /bin/sh -c "
  pacman-key --add /aios.gpg
  pacman-key --lsign-key $GPG_KEY
  rm /aios.gpg
"
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

# security hardening defaults — IPv6 off, rare fs/net-proto modules blacklisted, ufw on
cat > $ROOTFS/etc/sysctl.d/99-aios-hardening.conf <<'EOF'
net.ipv6.conf.all.disable_ipv6=1
net.ipv6.conf.default.disable_ipv6=1
EOF

cat > $ROOTFS/etc/modprobe.d/aios-blacklist.conf <<'EOF'
# Rare/legacy filesystems
blacklist cramfs
blacklist freevxfs
blacklist jffs2
blacklist hfs
blacklist hfsplus
blacklist udf

# Rare/legacy network protocols
blacklist dccp
blacklist sctp
blacklist rds
blacklist tipc
blacklist n-hdlc
blacklist ax25
blacklist netrom
blacklist x25
blacklist decnet
blacklist appletalk
EOF

cp cfg/aios-firewall.sh $ROOTFS/usr/local/bin/aios-firewall.sh
cp cfg/aios-firewall.service $ROOTFS/etc/systemd/system/aios-firewall.service
arch-chroot $ROOTFS /bin/sh -c '
  chmod +x /usr/local/bin/aios-firewall.sh
  systemctl enable aios-firewall.service
'

# gnu-coreutils -> uutils-coreutils
arch-chroot $ROOTFS /bin/bash -c '
  for b in /usr/bin/uu-*; do [ -e "$b" ] || continue; n=${b##*/uu-}; [ "$n" = coreutils ] && continue; ln -sf "$b" "/usr/local/bin/$n"; done
  for p in sudo:sudo-rs su:su-rs visudo:visudo-rs; do t="/usr/bin/${p#*:}"; [ -e "$t" ] && ln -sf "$t" "/usr/local/bin/${p%:*}"; done
'

# pacman cache 削除 — downloaded pkg は runtime 不要 / イメージ軽量化
rm -rf $ROOTFS/var/cache/pacman/pkg

if [[ "$BUILD_MODE" == "image" ]]; then
  zsh cfg/image.zsh $ROOTFS
  echo "=== build complete: aios.img ==="
else
  tar czf aios.tar.gz --numeric-owner -C $ROOTFS .
  echo "=== build complete: aios.tar.gz ==="
fi
