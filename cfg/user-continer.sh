#!/bin/bash
# Create child containers inside aios for ai user
# This script runs during build.zsh as root

ROOTFS="root.x86_64/var/lib/machines/aios"

echo "=== Creating child containers ==="

# Create workspace container
echo "Creating workspace container..."
mkdir -p /tmp/workspace-build
pacstrap -c /tmp/workspace-build base

# Configure workspace
arch-chroot /tmp/workspace-build /bin/sh -c 'pacman -Syu --noconfirm vim git zsh openssh nodejs npm sqlite'

# Add securetty for pts login
cat >> /tmp/workspace-build/etc/securetty <<'EOF'
pts/0
pts/1
pts/2
pts/3
pts/4
pts/5
EOF

# Move to aios
mkdir -p $ROOTFS/var/lib/machines
mv /tmp/workspace-build $ROOTFS/var/lib/machines/workspace

# Create restore-img as clean backup
echo "Creating restore-img (backup)..."
cp -a $ROOTFS/var/lib/machines/workspace $ROOTFS/var/lib/machines/restore-img

echo "✓ Child containers created"
echo "  - workspace"
echo "  - restore-img"
