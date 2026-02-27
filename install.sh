#!/bin/bash

set -e

NAME="aios"
TARBALL="aios.tar.gz"
DEST="/var/lib/machines/$NAME"

if [[ ! -f "$TARBALL" ]]; then
	echo "error: $TARBALL not found"
	exit 1
fi

echo "=== aios install ==="

mkdir -p "$DEST"
tar xzf "$TARBALL" -C "$DEST"

mkdir -p /etc/systemd/nspawn
cp cfg/aios.nspawn /etc/systemd/nspawn/$NAME.nspawn

echo "=== install complete ==="
echo ""
echo "  sudo machinectl start $NAME"
echo "  sudo machinectl shell $NAME /bin/su - ai"
