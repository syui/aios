#!/bin/zsh

d=${0:a:h}
case $1 in
	bsae)
		a=baseline
		;;
	*)
		a=releng
		;;
esac


if ! ls $d/*.tar.gz;then
	rm -rf $d/*.tar.gz
fi

if [ -d $d/work ];then
	rm -rf $d/work
fi

if [ -d $d/root.x86_64 ];then
	rm -rf $d/root.x86_64
fi

if [ -d $d/archiso ];then
	rm -rf $d/archiso
fi

git clone https://gitlab.archlinux.org/archlinux/archiso

# rm -rf $d/archlinux-docker
#	git clone https://gitlab.archlinux.org/archlinux/archlinux-docker

cp -rf $d/cfg/profiledef.sh $d/archiso/configs/$a/profiledef.sh
cp -rf $d/scpt/mkarchiso $d/archiso/archiso/mkarchiso

$d/archiso/archiso/mkarchiso -v -o $d/ $d/archiso/configs/releng

if [ ! -d $d/root.x86_64 ];then
	tar xf $d/aios-bootstrap*.tar.gz
fi

echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/etc/pacman.d/mirrorlist
sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/etc/pacman.conf
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --init'
arch-chroot root.x86_64 /bin/sh -c 'pacman-key --populate archlinux'
arch-chroot root.x86_64 /bin/sh -c 'pacman -Syu --noconfirm base base-devel linux vim git zsh rust openssh openssl jq'
arch-chroot root.x86_64 /bin/sh -c 'git clone https://git.syui.ai/ai/bot && cd bot && cargo build && cp -rf ./target/debug/ai /bin/ && ai ai'

# docker image
systemctl start docker
tar -C $d/root.x86_64 -c . | docker import - syui/aios

docker images -a
docker run --rm syui/aios ai
docker push syui/aios

# docker run -it syui/aios zsh 
