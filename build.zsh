#!/bin/zsh
# aios build script
# 1. Build minimal Arch Linux base
# 2. Setup user (ai) and shell
# 3. Setup Claude Code and aigpt

echo "=== aios build ==="
echo ""

# Clean up previous build artifacts
echo "Cleaning up previous build..."
rm -rf root.x86_64/ archiso/ install.sh
rm -f aios-bootstrap*.tar.gz 2>/dev/null || true

# ============================================
# 1. Arch Linux Base Construction
# ============================================

echo "=== Step 1: Arch Linux Base ==="

# Install build dependencies
pacman -Syuu --noconfirm base-devel archiso docker git nodejs bc

# Clone archiso
git clone https://gitlab.archlinux.org/archlinux/archiso

# Copy configuration
cp -rf ./cfg/profiledef.sh /usr/share/archiso/configs/releng/
cp -rf ./cfg/profiledef.sh ./archiso/configs/releng/profiledef.sh
cp -rf ./cfg/profiledef.sh ./archiso/configs/baseline/profiledef.sh
cp -rf ./scpt/mkarchiso ./archiso/archiso/mkarchiso

# Build bootstrap
./archiso/archiso/mkarchiso -v -o ./ ./archiso/configs/releng/

# Extract and prepare
tar xf aios-bootstrap*.tar.gz
mkdir -p root.x86_64
pacstrap -c root.x86_64 base

# Configure pacman
echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch
Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/etc/pacman.d/mirrorlist
sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/etc/pacman.conf

# Initialize pacman keys
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --init'
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --populate archlinux'

# Install base packages (including systemd-container for machinectl)
arch-chroot root.x86_64 /bin/sh -c 'pacman -Syu --noconfirm base base-devel linux vim git zsh rust openssh openssl jq go nodejs npm docker podman bc sqlite systemd arch-install-scripts'

# Configure containers
arch-chroot root.x86_64 /bin/sh -c 'mkdir -p /etc/containers/registries.conf.d'
arch-chroot root.x86_64 /bin/sh -c 'curl -sL -o /etc/containers/registries.conf.d/ai.conf https://git.syui.ai/ai/os/raw/branch/main/cfg/ai.conf'

# Set default shell
arch-chroot root.x86_64 /bin/sh -c 'chsh -s /bin/zsh'

# Install Claude Code
arch-chroot root.x86_64 /bin/sh -c 'npm i -g @anthropic-ai/claude-code'

# Copy os-release
cp -rf ./cfg/os-release root.x86_64/etc/os-release

# Configure sudoers for wheel group
echo "Configuring sudoers..."
arch-chroot root.x86_64 /bin/sh -c 'echo "%wheel ALL=(ALL:ALL) NOPASSWD: /usr/bin/pacman, /usr/bin/pacstrap, /usr/bin/arch-chroot, /usr/bin/rm, /usr/bin/mkdir, /usr/bin/mv, /usr/bin/cp, /usr/bin/poweroff, /usr/bin/reboot, /usr/bin/machinectl, /usr/bin/systemd-nspawn, /usr/bin/mount, /usr/bin/chroot, /bin/bash" >> /etc/sudoers'

# Install aigpt (aios core package)
echo "Installing aigpt..."
arch-chroot root.x86_64 /bin/sh -c 'git clone https://git.syui.ai/ai/gpt && cd gpt && cargo build --release && cp -rf ./target/release/aigpt /bin/ && cd .. && rm -rf gpt'

# Install aibot (aios core package)
echo "Installing aibot..."
arch-chroot root.x86_64 /bin/sh -c 'git clone https://git.syui.ai/ai/bot && cd bot && cargo build && cp -rf ./target/debug/aibot /bin/ && aibot ai && cd .. && rm -rf bot'

echo "✓ Arch Linux base complete"
echo ""

# ============================================
# 2. User Setup
# ============================================

bash ./cfg/setup-user.sh
echo ""

# ============================================
# 3. Create workspace container (after all configuration)
# ============================================

bash ./cfg/user-continer.sh
echo ""

# ============================================
# Finalize
# ============================================

echo "=== Finalizing ==="

# Copy aios-ctl.zsh for host machine control
cp -rf ./cfg/aios-ctl.zsh root.x86_64/opt/aios-ctl.zsh

# Copy install script
cp -rf ./cfg/install.sh ./install.sh
chmod +x ./install.sh

# Create tarball with aios (includes child containers)
echo "Creating tarball..."
tar -zcvf aios-bootstrap.tar.gz root.x86_64/ install.sh

echo ""
echo "=== Build Complete ==="
echo "Output: aios-bootstrap.tar.gz"
echo ""
