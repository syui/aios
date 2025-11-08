#!/bin/bash
# Create workspace container inside aios for ai user
# Backup aios before creating /var/lib/machines to avoid recursion

ROOTFS="root.x86_64"

echo "=== Creating workspace container ==="

# Backup current aios to temp location (before creating /var/lib/machines)
echo "Backing up aios..."
cp -a $ROOTFS /tmp/aios-backup-$$

# Create directory for child containers
mkdir -p $ROOTFS/var/lib/machines

# Copy backup as workspace
echo "Creating workspace container..."
cp -a /tmp/aios-backup-$$ $ROOTFS/var/lib/machines/workspace

# Cleanup temp backup
rm -rf /tmp/aios-backup-$$

echo "✓ Workspace container created"
