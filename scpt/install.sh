#!/bin/bash
set -euo pipefail

# 変数定義
DISK="/dev/sda"
HOSTNAME="ai-arch"
USERNAME="ai"

# パーティション作成（自動）
parted $DISK mklabel gpt
parted $DISK mkpart ESP fat32 1MiB 1GiB
parted $DISK set 1 esp on
parted $DISK mkpart primary linux-swap 1GiB 5GiB
parted $DISK mkpart primary ext4 5GiB 100%

# ファイルシステム作成
mkfs.fat -F32 ${DISK}1
mkswap ${DISK}2
mkfs.ext4 ${DISK}3

# マウント
mount ${DISK}3 /mnt
mkdir -p /mnt/boot
mount ${DISK}1 /mnt/boot
swapon ${DISK}2

# インストール
pacstrap -K /mnt base linux linux-firmware base-devel vim networkmanager grub efibootmgr

# 設定
genfstab -U /mnt >> /mnt/etc/fstab

arch-chroot /mnt /bin/bash << EOF
ln -sf /usr/share/zoneinfo/Asia/Tokyo /etc/localtime
hwclock --systohc
echo "ja_JP.UTF-8 UTF-8" >> /etc/locale.gen
locale-gen
echo "LANG=ja_JP.UTF-8" > /etc/locale.conf
echo "$HOSTNAME" > /etc/hostname
grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=ARCH
grub-mkconfig -o /boot/grub/grub.cfg
systemctl enable NetworkManager
useradd -m -G wheel $USERNAME
EOF
