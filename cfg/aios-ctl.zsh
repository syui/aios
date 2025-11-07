#!/bin/zsh
# aios machine control commands

NAME="aios"
BACKUP="${NAME}back"

# Start aios container
function aios-start() {
    sudo machinectl start $NAME
}

# Stop aios container
function aios-stop() {
    sudo machinectl poweroff $NAME > /dev/null 2>&1
    sleep 2
    sudo machinectl terminate $NAME > /dev/null 2>&1
}

# Shell into aios container
function aios-shell() {
    sudo machinectl shell $NAME
}

# Login to aios container
function aios-login() {
    sudo machinectl login $NAME
}

# Create backup of current aios
function aios-backup() {
    echo "Creating backup: $BACKUP"
    sudo machinectl poweroff $BACKUP > /dev/null 2>&1
    sleep 2
    sudo machinectl terminate $BACKUP > /dev/null 2>&1
    sleep 2
    sudo machinectl remove $BACKUP > /dev/null 2>&1
    sleep 2
    sudo machinectl clone $NAME $BACKUP
    echo "Backup created: $BACKUP"
}

# Reset aios from backup
function aios-reset() {
    if ! sudo machinectl list-images | grep -q $BACKUP; then
        echo "Error: No backup found. Run 'aios-backup' first."
        return 1
    fi

    echo "Resetting $NAME from $BACKUP..."
    sudo machinectl poweroff $NAME > /dev/null 2>&1
    sleep 2
    sudo machinectl terminate $NAME > /dev/null 2>&1
    sleep 2
    sudo machinectl remove $NAME
    sleep 2
    sudo machinectl clone $BACKUP $NAME
    sleep 2
    sudo machinectl start $NAME
    echo "Reset complete"
}

# Update packages in backup
function aios-update() {
    if ! sudo machinectl list-images | grep -q $BACKUP; then
        echo "Error: No backup found. Run 'aios-backup' first."
        return 1
    fi

    echo "Updating $BACKUP..."
    sudo machinectl start $BACKUP
    sleep 5
    sudo machinectl shell $BACKUP /bin/sh -c 'pacman -Syu --noconfirm'
    sleep 2
    sudo machinectl poweroff $BACKUP
    echo "Update complete"
}

# Remove aios container
function aios-remove() {
    echo "Removing $NAME..."
    sudo machinectl poweroff $NAME > /dev/null 2>&1
    sleep 2
    sudo machinectl terminate $NAME > /dev/null 2>&1
    sleep 2
    sudo machinectl remove $NAME
    echo "Removed $NAME"
}

# List all machines
function aios-list() {
    sudo machinectl list-images
}

# Show status
function aios-status() {
    sudo machinectl status $NAME
}

# Execute command in aios
function aios-exec() {
    if [ -z "$1" ]; then
        echo "Usage: aios-exec <command>"
        return 1
    fi
    sudo machinectl shell $NAME /bin/sh -c "$*"
}

# Show help
function aios-help() {
    cat <<'EOF'
aios machine control commands:

  aios-start      Start aios container
  aios-stop       Stop aios container
  aios-shell      Open shell in aios container
  aios-login      Login to aios container console
  aios-backup     Create backup snapshot (aiosback)
  aios-reset      Reset aios from backup
  aios-update     Update packages in backup
  aios-remove     Remove aios container
  aios-list       List all machine images
  aios-status     Show aios status
  aios-exec       Execute command in aios
  aios-help       Show this help

Example workflow:
  1. aios-start          # Start container
  2. aios-login          # Login and use
  3. aios-backup         # Create backup before major changes
  4. aios-reset          # Restore if something breaks
EOF
}
