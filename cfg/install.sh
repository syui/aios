#!/bin/bash
# aios installation script

set -e

NAME="aios"
BACKUP="${NAME}back"
TARBALL="aios-bootstrap.tar.gz"

echo "=== aios installation ==="
echo ""

# Check if running as root
if [ "$EUID" -ne 0 ]; then
    echo "Error: This script must be run as root"
    exit 1
fi

# Check if tarball exists
if [ ! -f "$TARBALL" ]; then
    echo "Error: $TARBALL not found"
    echo "Please download aios-bootstrap.tar.gz first"
    exit 1
fi

# Extract tarball
echo "1. Extracting $TARBALL..."
tar xf "$TARBALL"

# Move to /var/lib/machines/
echo "2. Installing to /var/lib/machines/$NAME..."
rm -rf /var/lib/machines/$NAME
mkdir -p /var/lib/machines
mv root.x86_64/var/lib/machines/arch /var/lib/machines/$NAME

# Copy nspawn configuration
echo "3. Installing systemd-nspawn configuration..."
mkdir -p /etc/systemd/nspawn

# Create aios.nspawn
cat > /etc/systemd/nspawn/$NAME.nspawn <<'EOF'
[Exec]
Boot=yes
PrivateUsers=pick
ResolvConf=copy-host

[Files]
Bind=/root/.config/syui/ai:/root/.config/syui/ai

[Network]
VirtualEthernet=no
EOF

# Create aiosback.nspawn
cat > /etc/systemd/nspawn/$BACKUP.nspawn <<'EOF'
[Exec]
Boot=yes
PrivateUsers=pick
ResolvConf=copy-host

[Files]
Bind=/root/.config/syui/ai:/root/.config/syui/ai

[Network]
VirtualEthernet=no
EOF

# Create workspace.nspawn
cat > /etc/systemd/nspawn/workspace.nspawn <<'EOF'
[Exec]
Boot=yes
PrivateUsers=pick
ResolvConf=copy-host

[Files]
Bind=/root/.config/syui/ai:/root/.config/syui/ai

[Network]
VirtualEthernet=no
EOF

# Create bind mount directory
mkdir -p /root/.config/syui/ai

# Enable systemd-machined
echo "4. Enabling systemd-machined..."
systemctl enable --now systemd-machined

# Create initial backup
echo "5. Creating initial backup image..."
machinectl clone $NAME $BACKUP

# Create workspace container for AI operations
echo "6. Creating workspace container..."
machinectl clone $NAME workspace

echo ""
echo "=== Installation complete ==="
echo ""
echo "Next steps for each user:"
echo "  1. Copy control script to your home:"
echo "     cp /var/lib/machines/$NAME/opt/aios-ctl.zsh ~/.aios-ctl.zsh"
echo ""
echo "  2. Add to your .zshrc:"
echo "     echo 'source ~/.aios-ctl.zsh' >> ~/.zshrc"
echo "     source ~/.zshrc"
echo ""
echo "  3. Start aios:"
echo "     aios-start"
echo ""
echo "  4. Login to aios:"
echo "     aios-login"
echo ""
echo "Available commands:"
echo "  aios-start, aios-stop, aios-shell, aios-login"
echo "  aios-backup, aios-reset, aios-update"
echo "  aios-help for full list"
echo ""
