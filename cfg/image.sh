#!/bin/bash

set -e

ROOTFS="$1"
IMG="aios.img"
IMG_SIZE="4G"
ESP_SIZE=512  # MiB
LOOP=""

cleanup() {
  set +e
  umount -R /mnt/aios 2>/dev/null
  [[ -n "$LOOP" ]] && losetup -d "$LOOP" 2>/dev/null
  rmdir /mnt/aios 2>/dev/null
  set -e
}
trap cleanup EXIT

if [[ -z "$ROOTFS" || ! -d "$ROOTFS" ]]; then
  echo "Usage: $0 <rootfs-dir>"
  exit 1
fi

rm -f "$IMG"

echo "--- Creating raw image ($IMG_SIZE) ---"
fallocate -l "$IMG_SIZE" "$IMG"

echo "--- Partitioning (GPT: ESP + root) ---"
sgdisk -Z "$IMG"
sgdisk -n 1:0:+${ESP_SIZE}M -t 1:EF00 -c 1:"ESP" "$IMG"
sgdisk -n 2:0:0 -t 2:8300 -c 2:"root" "$IMG"

echo "--- Setting up loop device ---"
LOOP=$(losetup --find --show --partscan "$IMG")
PART_ESP="${LOOP}p1"
PART_ROOT="${LOOP}p2"

# Wait for partition devices
udevadm settle
sleep 1

echo "--- Formatting partitions ---"
mkfs.fat -F 32 "$PART_ESP"
mkfs.ext4 -F "$PART_ROOT"

echo "--- Mounting and copying rootfs ---"
mkdir -p /mnt/aios
mount "$PART_ROOT" /mnt/aios
mkdir -p /mnt/aios/boot
mount "$PART_ESP" /mnt/aios/boot

cp -a "$ROOTFS"/. /mnt/aios/

echo "--- Generating fstab ---"
genfstab -U /mnt/aios > /mnt/aios/etc/fstab

echo "--- Installing systemd-boot ---"
arch-chroot /mnt/aios bootctl install

echo "--- Creating boot entry ---"
ROOT_UUID=$(blkid -s UUID -o value "$PART_ROOT")

mkdir -p /mnt/aios/boot/loader/entries

cat > /mnt/aios/boot/loader/loader.conf <<'EOF'
default aios.conf
timeout 3
console-mode max
EOF

cat > /mnt/aios/boot/loader/entries/aios.conf <<EOF
title   aios
linux   /vmlinuz-linux
initrd  /initramfs-linux.img
options root=UUID=$ROOT_UUID rw
EOF

echo "--- Generating initramfs ---"
arch-chroot /mnt/aios mkinitcpio -P

echo "--- Unmounting ---"
umount -R /mnt/aios
losetup -d "$LOOP"
LOOP=""
rmdir /mnt/aios 2>/dev/null

echo "--- $IMG ready ---"
