#!/bin/bash
# User setup for aios
# Creates ai user, configures auto-login, sudo, zshrc

ROOTFS="root.x86_64/var/lib/machines/arch"

echo "=== User Setup ==="

# Create default user 'ai'
echo "Creating user 'ai'..."
arch-chroot $ROOTFS /bin/sh -c 'useradd -m -G wheel -s /bin/zsh ai'
arch-chroot $ROOTFS /bin/sh -c 'echo "ai:root" | chpasswd'

# Setup auto-login for user 'ai'
echo "Setting up auto-login..."
arch-chroot $ROOTFS /bin/sh -c 'mkdir -p /etc/systemd/system/getty@tty1.service.d'
cat > $ROOTFS/etc/systemd/system/getty@tty1.service.d/override.conf <<'EOF'
[Service]
ExecStart=
ExecStart=-/usr/bin/agetty --autologin ai --noclear %I $TERM
EOF

# Copy .zshrc for root
echo "Copying zshrc..."
cp -rf ./cfg/zshrc $ROOTFS/root/.zshrc

# Copy .zshrc for user 'ai'
cp -rf ./cfg/zshrc $ROOTFS/home/ai/.zshrc

# Add MCP auto-setup and claude auto-start for ai user (login shell only)
cat >> $ROOTFS/home/ai/.zshrc <<'EOF'

# MCP auto-setup (run once after .claude.json is created)
if [[ -f ~/.claude.json ]] && ! grep -q '"aigpt"' ~/.claude.json 2>/dev/null; then
    if command -v claude &>/dev/null && command -v aigpt &>/dev/null; then
        claude mcp add aigpt aigpt server &>/dev/null || true
    fi
fi

# Auto-start claude in interactive login shell
if [[ -o login ]] && [[ -o interactive ]]; then
    if command -v claude &>/dev/null; then
        claude
    fi
fi
EOF

arch-chroot $ROOTFS /bin/sh -c 'chown ai:ai /home/ai/.zshrc'

# Copy aios startup script
cp -rf ./cfg/aios.zsh $ROOTFS/usr/local/bin/aios-startup
arch-chroot $ROOTFS /bin/sh -c 'chmod +x /usr/local/bin/aios-startup'

# Create default config directory and file for user 'ai'
arch-chroot $ROOTFS /bin/sh -c 'mkdir -p /home/ai/.config/syui/ai/os'
cat > $ROOTFS/home/ai/.config/syui/ai/os/config.json <<'EOF'
{
  "shell": false
}
EOF
arch-chroot $ROOTFS /bin/sh -c 'chown -R ai:ai /home/ai/.config'

# Update .zshrc to source startup script
cat >> $ROOTFS/home/ai/.zshrc <<'EOF'

# aios startup
source /usr/local/bin/aios-startup
EOF

echo "✓ User setup complete"
