#!/bin/bash
# Create child containers inside aios for ai user
# Simply copy the aios itself as child containers

ROOTFS="root.x86_64/var/lib/machines/aios"

echo "=== Creating child containers ==="

# Create directory for child containers
mkdir -p $ROOTFS/var/lib/machines

# Copy aios as workspace
echo "Creating workspace container..."
cp -a $ROOTFS $ROOTFS/var/lib/machines/workspace

# Copy aios as restore-img
echo "Creating restore-img container..."
cp -a $ROOTFS $ROOTFS/var/lib/machines/restore-img

echo "✓ Child containers created"
