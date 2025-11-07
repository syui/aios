#!/bin/bash
# aios installation script

NAME="aios"
TARBALL="aios-bootstrap.tar.gz"

echo "=== aios installation ==="

# Extract and install
tar xf "$TARBALL"
mkdir -p /var/lib/machines
mv root.x86_64/var/lib/machines/aios /var/lib/machines/$NAME

echo "=== Installation complete ==="
echo ""
echo "Usage:"
echo "  sudo machinectl start $NAME"
echo "  sudo machinectl shell $NAME /bin/su - ai"
echo ""
