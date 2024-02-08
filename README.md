## ai `os`

<img src="./icon/ai.png" width="100">

- name : ai os
- base : [archlinux](https://gitlab.archlinux.org/archlinux)

### docker

```sh
$ docker run --rm syui/aios ai
```

### archiso

- [profile.rst](https://gitlab.archlinux.org/archlinux/archiso/-/blob/master/docs/README.profile.rst)

```sh
$ pacman -S archiso
```

```sh
$ git clone https://git.syui.ai/ai/os
$ cd os
$ git clone https://gitlab.archlinux.org/archlinux/archlinux-docker
$ git clone https://gitlab.archlinux.org/archlinux/archiso

$ vim ./archiso/configs/releng/profiledef.sh

$ mkarchiso -v -o ./ ./archiso/configs/releng
```

### system

> ./archiso/configs/releng/profiledef.sh

```sh
buildmodes=('bootstrap')
```

```sh
$ mkarchiso -v -o ./ ./archiso/configs/releng
$ tar xf aios-bootstrap*.tar.gz
$ echo -e 'Server = http://mirrors.cat.net/archlinux/$repo/os/$arch\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch' >> ./root.x86_64/etc/pacman.d/mirrorlist
$ sed -i s/CheckSpace/#CheckeSpace/ root.x86_64/etc/pacman.conf
$ arch-chroot ./root.x86_64
---
$ pacman -S base base-devel linux vim git zsh rust
$ pacman-key --init
$ pacman-key --populate archlinux
$ exit
---
$ tar -C root.x86_64 -c . | docker import - syui/aios
$ docker images

$ docker run --rm syui/aios cargo version
cargo 1.75.0
```


