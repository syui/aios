#!/bin/bash
# Claude Code MCP setup for aios
# Configures MCP, sets up shared memory

ROOTFS="root.x86_64/var/lib/machines/arch"

echo "=== Claude MCP Setup ==="

# Setup Claude Code MCP configuration (shared via symlink)
echo "Configuring MCP..."
# Create actual config in syui/ai/claude (bind-mounted)
arch-chroot $ROOTFS /bin/sh -c 'mkdir -p /root/.config/syui/ai/claude'
cat > $ROOTFS/root/.config/syui/ai/claude/claude_desktop_config.json <<'EOF'
{
  "mcpServers": {
    "aigpt": {
      "command": "aigpt",
      "args": ["server", "--enable-layer4"]
    }
  }
}
EOF

# Create symlink for root
arch-chroot $ROOTFS /bin/sh -c 'ln -sf /root/.config/syui/ai/claude /root/.config/claude'

# Setup for ai user too
arch-chroot $ROOTFS /bin/sh -c 'mkdir -p /home/ai/.config/syui/ai/claude'
arch-chroot $ROOTFS /bin/sh -c 'cp /root/.config/syui/ai/claude/claude_desktop_config.json /home/ai/.config/syui/ai/claude/'
arch-chroot $ROOTFS /bin/sh -c 'ln -sf /home/ai/.config/syui/ai/claude /home/ai/.config/claude'
arch-chroot $ROOTFS /bin/sh -c 'chown -R ai:ai /home/ai/.config/syui'

# Create config directory
arch-chroot $ROOTFS /bin/sh -c 'mkdir -p /root/.config/syui/ai/gpt'

# Copy MCP and aios configuration
echo "Copying configuration files..."
cp -rf ./cfg/mcp.json $ROOTFS/root/.config/syui/ai/mcp.json
cp -rf ./cfg/config.toml $ROOTFS/root/.config/syui/ai/config.toml

# Initialize aigpt database with WAL mode
echo "Initializing aigpt database..."
arch-chroot $ROOTFS /bin/sh -c 'aigpt server --enable-layer4 &'
sleep 2
arch-chroot $ROOTFS /bin/sh -c 'pkill aigpt'
arch-chroot $ROOTFS /bin/sh -c 'if command -v sqlite3 &>/dev/null; then sqlite3 /root/.config/syui/ai/gpt/memory.db "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;"; fi'

echo "✓ Claude MCP setup complete"
