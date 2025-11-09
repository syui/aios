#!/bin/bash
set -e

echo "Installing AIOS..."

# Check if running as root for system-wide install
if [ "$EUID" -eq 0 ]; then
    INSTALL_DIR="/usr/local/bin"
    SYSTEMD_DIR="/etc/systemd/system"
    echo "Installing system-wide to $INSTALL_DIR"
else
    INSTALL_DIR="$HOME/.local/bin"
    SYSTEMD_DIR="$HOME/.config/systemd/user"
    echo "Installing for user to $INSTALL_DIR"
fi

# Build release binaries if not already built
if [ ! -f "target/release/aios-runtime" ]; then
    echo "Building AIOS..."
    cargo build --release
fi

# Create directories
mkdir -p "$INSTALL_DIR"
mkdir -p "$HOME/.config/aios"

# Install binaries
echo "Installing binaries..."
cp target/release/aios-runtime "$INSTALL_DIR/"
cp target/release/aios "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/aios-runtime"
chmod +x "$INSTALL_DIR/aios"

# Install systemd service (user mode)
if [ "$EUID" -ne 0 ]; then
    echo "Installing systemd user service..."
    mkdir -p "$SYSTEMD_DIR"

    # Create user service file
    cat > "$SYSTEMD_DIR/aios-runtime.service" <<EOF
[Unit]
Description=AIOS Runtime Daemon
After=default.target

[Service]
Type=simple
Environment="RUST_LOG=info"
ExecStart=$INSTALL_DIR/aios-runtime
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=default.target
EOF

    echo "Enabling systemd service..."
    systemctl --user daemon-reload
    systemctl --user enable aios-runtime.service

    echo ""
    echo "AIOS installed successfully!"
    echo ""
    echo "To start the daemon:"
    echo "  systemctl --user start aios-runtime"
    echo ""
    echo "To check status:"
    echo "  systemctl --user status aios-runtime"
    echo ""
    echo "To use the CLI:"
    echo "  export OPENAI_API_KEY='your-api-key'"
    echo "  aios chat 'Hello, AIOS!'"
    echo ""
    echo "For interactive mode:"
    echo "  aios shell"
else
    echo ""
    echo "System-wide installation complete!"
    echo "Binaries installed to: $INSTALL_DIR"
    echo ""
    echo "To use AIOS as a user service, run:"
    echo "  su - <username>"
    echo "  ./install.sh"
fi
