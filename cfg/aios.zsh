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
    echo "  Shell mode enabled"
    echo ""

    # claudeを起動
    if command -v claude &>/dev/null; then
        exec claude
    fi
fi
