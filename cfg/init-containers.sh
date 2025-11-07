#!/bin/bash
# Initialize child containers for ai user
# This script runs once on first login

echo "=== Initializing workspace containers ==="
echo "This may take a few minutes..."

# Create workspace directory
mkdir -p /tmp/workspace-init

# Create base workspace
echo "Creating workspace container..."
sudo pacstrap -c /tmp/workspace-init base

# Configure workspace
sudo arch-chroot /tmp/workspace-init /bin/sh -c 'pacman -Syu --noconfirm vim git zsh openssh nodejs npm sqlite'

# Add securetty for pts login
sudo bash -c 'cat >> /tmp/workspace-init/etc/securetty <<EOF
pts/0
pts/1
pts/2
pts/3
pts/4
pts/5
EOF'

# Move to /var/lib/machines
sudo mkdir -p /var/lib/machines
sudo mv /tmp/workspace-init /var/lib/machines/workspace

# Create restore-img as clean backup
echo "Creating restore-img (backup)..."
sudo cp -a /var/lib/machines/workspace /var/lib/machines/restore-img

echo ""
echo "✓ Initialization complete!"
echo ""
echo "Available containers:"
echo "  workspace    - Working environment"
echo "  restore-img  - Clean backup"
echo ""
echo "Usage:"
echo "  sudo machinectl start workspace"
echo "  sudo machinectl shell workspace"
echo ""
