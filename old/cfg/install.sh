#!/bin/bash
# aios installation script

NAME="aios"
TARBALL="aios-bootstrap.tar.gz"

echo "=== aios installation ==="

# Extract and install
tar xf "$TARBALL"
mkdir -p /var/lib/machines
mv root.x86_64 /var/lib/machines/$NAME

# Create aios.nspawn for network access
echo "Creating network configuration..."
mkdir -p /etc/systemd/nspawn
cat > /etc/systemd/nspawn/$NAME.nspawn <<'EOF'
[Exec]
Boot=yes

[Network]
Private=no
EOF

echo "=== Installation complete ==="
echo ""
echo "Usage:"
echo "  sudo machinectl start $NAME"
echo "  sudo machinectl shell $NAME /bin/su - ai"
echo ""
