#!/bin/zsh
pacman -Syuu --noconfirm base-devel archiso docker git
git clone https://gitlab.archlinux.org/archlinux/archiso
cp -rf ./cfg/profiledef.sh /usr/share/archiso/configs/releng/
cp -rf ./cfg/profiledef.sh ./archiso/configs/releng/profiledef.sh
cp -rf ./cfg/profiledef.sh ./archiso/configs/baseline/profiledef.sh
cp -rf ./scpt/mkarchiso ./archiso/archiso/mkarchiso
./archiso/archiso/mkarchiso -v -o ./ ./archiso/configs/releng/
tar xf aios-bootstrap*.tar.gz
echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch
Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/etc/pacman.d/mirrorlist
sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/etc/pacman.conf
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --init'
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --populate archlinux'
arch-chroot root.x86_64 /bin/sh -c 'pacman -Syu --noconfirm base base-devel linux vim git zsh rust openssh openssl jq'
arch-chroot root.x86_64 /bin/sh -c 'chsh -s /bin/zsh'
arch-chroot root.x86_64 /bin/sh -c 'git clone https://git.syui.ai/ai/bot && cd bot && cargo build && cp -rf ./target/debug/ai /bin/ && ai ai'
tar -zcvf aios-bootstrap.tar.gz root.x86_64/
