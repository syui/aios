#!/bin/zsh
pacman -Syuu --noconfirm base-devel archiso docker git nodejs bc
git clone https://gitlab.archlinux.org/archlinux/archiso
cp -rf ./cfg/profiledef.sh /usr/share/archiso/configs/releng/
cp -rf ./cfg/profiledef.sh ./archiso/configs/releng/profiledef.sh
cp -rf ./cfg/profiledef.sh ./archiso/configs/baseline/profiledef.sh
cp -rf ./scpt/mkarchiso ./archiso/archiso/mkarchiso
./archiso/archiso/mkarchiso -v -o ./ ./archiso/configs/releng/
tar xf aios-bootstrap*.tar.gz
mkdir -p root.x86_64/var/lib/machines/arch
pacstrap -c root.x86_64/var/lib/machines/arch base
echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch
Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/var/lib/machines/arch/etc/pacman.d/mirrorlist
sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/var/lib/machines/arch/etc/pacman.conf
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'pacman-key --init'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'pacman-key --populate archlinux'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'pacman -Syu --noconfirm base base-devel linux vim git zsh rust openssh openssl jq go nodejs npm docker podman bc sqlite'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'mkdir -p /etc/containers/registries.conf.d'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'curl -sL -o /etc/containers/registries.conf.d/ai.conf https://git.syui.ai/ai/os/raw/branch/main/cfg/ai.conf'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'chsh -s /bin/zsh'

# Install Claude Code
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'npm i -g @anthropic-ai/claude-code'

# Copy os-release
cp -rf ./cfg/os-release root.x86_64/var/lib/machines/arch/etc/os-release

# Copy .zshrc
cp -rf ./cfg/zshrc root.x86_64/var/lib/machines/arch/root/.zshrc

# Install aigpt (AI memory system)
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'git clone https://git.syui.ai/ai/gpt && cd gpt && cargo build --release && cp -rf ./target/release/aigpt /bin/'

# Install ai/bot (optional, for backward compatibility)
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'git clone https://git.syui.ai/ai/bot && cd bot && cargo build && cp -rf ./target/debug/ai /bin/ && ai ai'

# Create config directory
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'mkdir -p /root/.config/syui/ai/gpt'

# Copy MCP and aios configuration
cp -rf ./cfg/mcp.json root.x86_64/var/lib/machines/arch/root/.config/syui/ai/mcp.json
cp -rf ./cfg/config.toml root.x86_64/var/lib/machines/arch/root/.config/syui/ai/config.toml

# Initialize aigpt database with WAL mode
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'aigpt server --enable-layer4 &'
sleep 2
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'pkill aigpt'
arch-chroot root.x86_64/var/lib/machines/arch /bin/sh -c 'if command -v sqlite3 &>/dev/null; then sqlite3 /root/.config/syui/ai/gpt/memory.db "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;"; fi'

tar -zcvf aios-bootstrap.tar.gz root.x86_64/
