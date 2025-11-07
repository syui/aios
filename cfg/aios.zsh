#!/bin/zsh
# aios startup script

CONFIG_DIR="$HOME/.config/syui/ai/os"
CONFIG_FILE="$CONFIG_DIR/config.json"

# 設定ファイルが存在しない場合は何もしない
if [ ! -f "$CONFIG_FILE" ]; then
    return
fi

# jqで設定読み込み
if ! command -v jq &>/dev/null; then
    return
fi

SHELL_MODE=$(cat "$CONFIG_FILE" | jq -r '.shell // false')

if [ "$SHELL_MODE" = "true" ]; then
    echo "aios - AI-managed OS"
    echo "  Starting workspace container..."
    echo ""

    # Check if workspace exists
    if ! sudo machinectl list-images | grep -q "^workspace"; then
        echo "Error: workspace container not found"
        echo "Please run install.sh first to create workspace container"
        return
    fi

    # Start workspace container
    sudo machinectl start workspace 2>/dev/null || true
    sleep 2

    # Login to workspace (claude.service will auto-start inside)
    echo "Connecting to workspace container..."
    exec sudo machinectl login workspace
fi
