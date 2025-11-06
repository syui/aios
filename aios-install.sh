#!/bin/bash
# aios installer - systemd-nspawn with aigpt + Claude Code

set -e

AIOS_VERSION="0.1.0"
AIOS_ROOT="/var/lib/machines/aios"
AIOS_CONFIG="$HOME/.config/syui/ai"

echo "=== aios installer v${AIOS_VERSION} ==="
echo ""
echo "Installing: aigpt + Claude Code in systemd-nspawn"
echo ""

# 1. Create shared memory directory
echo "[1/6] Creating shared memory directory..."
mkdir -p "${AIOS_CONFIG}/gpt"
chmod 700 "${AIOS_CONFIG}"
echo "✓ Created: ${AIOS_CONFIG}"

# 2. Download bootstrap container (if not exists)
if [ ! -d "$AIOS_ROOT" ]; then
    echo "[2/6] Downloading aios bootstrap container..."
    if [ "$EUID" -eq 0 ]; then
        mkdir -p /var/lib/machines
        cd /var/lib/machines
        curl -sL https://github.com/syui/aios/releases/download/latest/aios-bootstrap.tar.gz | tar xz
        echo "✓ Bootstrap container extracted to: $AIOS_ROOT"
    else
        echo "⚠ Skipping (requires root)"
    fi
else
    echo "[2/6] Bootstrap container already exists"
fi

# 3. Install aigpt (if not installed)
if ! command -v aigpt &>/dev/null; then
    echo "[3/6] Installing aigpt..."
    if command -v cargo &>/dev/null; then
        cd /tmp
        git clone https://git.syui.ai/ai/gpt || git clone https://github.com/syui/aigpt
        cd gpt 2>/dev/null || cd aigpt
        cargo build --release

        if [ "$EUID" -eq 0 ]; then
            cp target/release/aigpt /usr/bin/
        else
            mkdir -p ~/.local/bin
            cp target/release/aigpt ~/.local/bin/
            echo "  Add to PATH: export PATH=\$HOME/.local/bin:\$PATH"
        fi
        echo "✓ aigpt installed"
    else
        echo "⚠ cargo not found. Install rust first:"
        echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
else
    echo "[3/6] aigpt already installed"
fi

# 4. Initialize aigpt database
if [ ! -f "${AIOS_CONFIG}/gpt/memory.db" ]; then
    echo "[4/6] Initializing aigpt database..."

    # Start aigpt server temporarily to create DB
    if command -v aigpt &>/dev/null; then
        aigpt server --enable-layer4 &
        AIGPT_PID=$!
        sleep 2
        kill $AIGPT_PID 2>/dev/null || true

        # Enable WAL mode for concurrent access
        if command -v sqlite3 &>/dev/null; then
            sqlite3 "${AIOS_CONFIG}/gpt/memory.db" <<EOF
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA cache_size=-64000;
EOF
            echo "✓ Database initialized with WAL mode"
        else
            echo "⚠ sqlite3 not found, skipping WAL mode setup"
        fi
    else
        echo "⚠ aigpt not available, skipping database init"
    fi
else
    echo "[4/6] Database already exists"
fi

# 5. Install MCP configuration
echo "[5/6] Installing MCP configuration..."
cp cfg/mcp.json "${AIOS_CONFIG}/mcp.json"
cp cfg/config.toml "${AIOS_CONFIG}/config.toml"
echo "✓ Configuration files installed"

# 6. Install systemd-nspawn configuration
if [ "$EUID" -eq 0 ]; then
    echo "[6/6] Installing systemd-nspawn configuration..."
    mkdir -p /etc/systemd/nspawn

    # Replace %h with actual home directory
    sed "s|%h|$HOME|g" cfg/nspawn/aios.nspawn > /etc/systemd/nspawn/aios.nspawn

    echo "✓ systemd-nspawn configuration installed"

    # Enable and start container
    echo ""
    echo "Starting aios container..."
    systemctl enable systemd-nspawn@aios
    systemctl start systemd-nspawn@aios
    echo "✓ aios container started"
else
    echo "[6/6] Skipping systemd setup (requires root)"
fi

echo ""
echo "================================================"
echo "✓ aios installation complete!"
echo "================================================"
echo ""
echo "Next steps:"
echo ""
echo "  # Enter aios container:"
echo "  $ sudo machinectl shell aios"
echo ""
echo "  # Inside container, start Claude Code:"
echo "  $ claude"
echo "  # or"
echo "  $ ai"
echo ""
echo "Configuration:"
echo "  Shared memory: ${AIOS_CONFIG}/gpt/memory.db"
echo "  MCP config:    ${AIOS_CONFIG}/mcp.json"
echo ""
