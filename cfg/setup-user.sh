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

# Create workspace container configuration (bind ai user dir to container root)
echo "Creating workspace container configuration..."
mkdir -p $ROOTFS/etc/systemd/nspawn
cat > $ROOTFS/etc/systemd/nspawn/workspace.nspawn <<'EOF'
[Exec]
Boot=yes
PrivateUsers=pick
ResolvConf=copy-host

[Files]
Bind=/home/ai:/root

[Network]
VirtualEthernet=no
EOF

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

# Add workspace container auto-start and entry (shared .zshrc for ai user and workspace root)
cat >> $ROOTFS/home/ai/.zshrc <<'EOF'

# MCP auto-setup (run once after .claude.json is created)
if [[ -f ~/.claude.json ]] && ! grep -q '"aigpt"' ~/.claude.json 2>/dev/null; then
    if command -v claude &>/dev/null && command -v aigpt &>/dev/null; then
        claude mcp add aigpt aigpt server &>/dev/null || true
    fi
fi

# aios concept: container from start (ai user and workspace root share this .zshrc)
if [[ -o login ]] && [[ -o interactive ]]; then
    if [[ -z "$INSIDE_WORKSPACE" ]]; then
        # Running as ai user on aios OS - enter workspace container
        export INSIDE_WORKSPACE=1
        sudo machinectl start workspace 2>/dev/null || true
        sleep 1
        exec sudo machinectl shell workspace
    else
        # Running as root inside workspace container - start claude
        if command -v claude &>/dev/null; then
            claude
        fi
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
