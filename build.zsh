#!/bin/zsh

d=${0:a:h}
cd $d

if ! ls ./*.tar.gz;then
	rm -rf ./*.tar.gz
fi

if [ -d ./work ];then
	rm -rf ./work
fi

if [ -d ./root.x86_64 ];then
	rm -rf ./root.x86_64
fi

if [ -d ./archiso ];then
	rm -rf ./archiso
fi

git clone https://gitlab.archlinux.org/archlinux/archiso

# rm -rf $d/archlinux-docker
#	git clone https://gitlab.archlinux.org/archlinux/archlinux-docker

cp -rf ./cfg/profiledef.sh /usr/share/archiso/configs/releng/
cp -rf ./cfg/profiledef.sh ./archiso/configs/releng/profiledef.sh
cp -rf ./cfg/profiledef.sh ./archiso/configs/baseline/profiledef.sh
cp -rf ./scpt/mkarchiso ./archiso/archiso/mkarchiso

./archiso/archiso/mkarchiso -v -o ./ ./archiso/configs/releng

if [ ! -d ./root.x86_64 ];then
	tar xf ./aios-bootstrap*.tar.gz
fi

echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/etc/pacman.d/mirrorlist
sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/etc/pacman.conf
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --init'
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --populate archlinux'
arch-chroot root.x86_64 /bin/sh -c 'pacman -Syu --noconfirm base base-devel linux vim git zsh rust openssh openssl jq'
arch-chroot root.x86_64 /bin/sh -c 'git clone https://git.syui.ai/ai/bot && cd bot && cargo build && cp -rf ./target/debug/ai /bin/ && ai ai'

systemctl start docker
tar -C ./root.x86_64 -c . | docker import - syui/aios

docker images -a
docker run --rm syui/aios ai
docker push syui/aios
