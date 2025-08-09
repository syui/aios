#!/bin/bash
pacman -Syuu --noconfirm git base-devel archiso
git clone https://gitlab.archlinux.org/archlinux/archiso
./archiso/archiso/mkarchiso -v -o ./ ./archiso/configs/releng/
mkdir -p work/x86_64/airootfs/var/lib/machines/arch
pacstrap -c work/x86_64/airootfs/var/lib/machines/arch base
arch-chroot work/x86_64/airootfs/ /bin/sh -c 'pacman-key --init'
arch-chroot work/x86_64/airootfs/ /bin/sh -c 'pacman-key --populate archlinux'
tar -zcvf archlinux.tar.gz -C work/x86_64/airootfs/ .
