#!/bin/bash
# User setup for aios
# Creates ai user, configures auto-login, sudo, zshrc

ROOTFS="root.x86_64/var/lib/machines/aios"

echo "=== User Setup ==="

# Create default user 'ai'
echo "Creating user 'ai'..."
arch-chroot $ROOTFS /bin/sh -c 'useradd -m -G wheel -s /bin/zsh ai'
arch-chroot $ROOTFS /bin/sh -c 'echo "ai:root" | chpasswd'

# Configure securetty for pts login (required for systemd-nspawn)
echo "Configuring securetty..."
cat >> $ROOTFS/etc/securetty <<'EOF'
pts/0
pts/1
pts/2
pts/3
pts/4
pts/5
pts/6
pts/7
pts/8
pts/9
EOF

# Enable systemd-machined for container management
echo "Enabling systemd-machined..."
arch-chroot $ROOTFS /bin/sh -c 'systemctl enable systemd-machined'

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

# Copy container initialization script
cp -rf ./cfg/init-containers.sh $ROOTFS/usr/local/bin/init-containers.sh
arch-chroot $ROOTFS /bin/sh -c 'chmod +x /usr/local/bin/init-containers.sh'

# Add initialization, MCP auto-setup and claude auto-start for ai user (login shell only)
cat >> $ROOTFS/home/ai/.zshrc <<'EOF'

# Initialize workspace containers on first login
if [ ! -f ~/.aios-initialized ]; then
    echo "First login detected. Initializing workspace containers..."
    if command -v sudo &>/dev/null && [ -x /usr/local/bin/init-containers.sh ]; then
        /usr/local/bin/init-containers.sh && touch ~/.aios-initialized
    fi
fi

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
