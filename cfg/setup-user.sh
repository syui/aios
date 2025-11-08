#!/bin/bash
# User setup for aios
# Creates ai user, configures auto-login, sudo, zshrc

ROOTFS="root.x86_64"

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

# Add claude auto-start on login (once, not exec)
cat >> $ROOTFS/home/ai/.zshrc <<'EOF'

# Start claude on login (once)
if [[ -o login ]] && [[ -o interactive ]]; then
    if command -v claude &>/dev/null; then
        claude
    fi
fi
EOF

arch-chroot $ROOTFS /bin/sh -c 'chown ai:ai /home/ai/.zshrc'

echo "✓ User setup complete"
