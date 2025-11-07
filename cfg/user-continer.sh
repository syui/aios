#!/bin/bash
# Create child containers inside aios for ai user
# Simply copy the aios itself as child containers

ROOTFS="root.x86_64/var/lib/machines/aios"

echo "=== Creating child containers ==="

# Backup current aios to temp location
echo "Backing up aios..."
cp -a $ROOTFS /tmp/aios-backup-$$

# Create directory for child containers
mkdir -p $ROOTFS/var/lib/machines

# Copy backup as workspace
echo "Creating workspace container (copy of aios)..."
cp -a /tmp/aios-backup-$$ $ROOTFS/var/lib/machines/workspace

# Copy backup as restore-img (clean backup)
echo "Creating restore-img container (copy of aios)..."
cp -a /tmp/aios-backup-$$ $ROOTFS/var/lib/machines/restore-img

# Cleanup temp backup
rm -rf /tmp/aios-backup-$$

echo "✓ Child containers created"
echo "  - workspace (copy of aios)"
echo "  - restore-img (copy of aios)"
