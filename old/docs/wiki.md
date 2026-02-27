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

### gh-actions

[.github/workflows/push.yml](https://docs.github.com/en/enterprise-cloud@latest/packages/managing-github-packages-using-github-actions-workflows/publishing-and-installing-a-package-with-github-actions)

```yml
name: Demo Push
on:
  push:
    branches:
      - main
      - seed
    tags:
      - v*
  pull_request:

env:
  IMAGE_NAME: ghtoken_product_demo

jobs:
  push:
    runs-on: ubuntu-latest
    permissions:
      packages: write
      contents: read
    steps:
      - uses: actions/checkout@v4
      - name: Build image
        run: docker build . --file Dockerfile --tag $IMAGE_NAME --label "runnumber=${GITHUB_RUN_ID}"
      - name: Log in to registry
        run: echo "${{ secrets.GITHUB_TOKEN }}" | docker login ghcr.io -u $ --password-stdin
      - name: Push image
        run: |
          IMAGE_ID=ghcr.io/${{ github.repository_owner }}/$IMAGE_NAME
          IMAGE_ID=$(echo $IMAGE_ID | tr '[A-Z]' '[a-z]')
          VERSION=$(echo "${{ github.ref }}" | sed -e 's,.*/\(.*\),\1,')
          [[ "${{ github.ref }}" == "refs/tags/"* ]] && VERSION=$(echo $VERSION | sed -e 's/^v//')
          [ "$VERSION" == "main" ] && VERSION=latest
          echo IMAGE_ID=$IMAGE_ID
          echo VERSION=$VERSION
          docker tag $IMAGE_NAME $IMAGE_ID:$VERSION
          docker push $IMAGE_ID:$VERSION
```

### github-token

```yml
env:
  IMAGE_NAME: ${{ github.repository }}
  GITHUB_TOKEN: ${{ secrets.APP_TOKEN }}
  REGISTRY: ghcr.io

jobs:
  release:
    name: Release
    runs-on: ubuntu-latest
    container: 
      image: archlinux
      options: --privileged
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Log in to the Container registry
        uses: docker/login-action@v3
        with:
          registry: ${{ env.REGISTRY }}
          username: ${{ github.actor }}
          password: ${{ env.GITHUB_TOKEN }}
      - name: github container registry
        run: |
          docker tag ${{ env.IMAGE_NAME }} ${{ env.REGISTRY }}/${{ env.IMAGE_NAME }}
          docker push ${{ env.REGISTRY }}/${{ env.IMAGE_NAME }}
```

```sh
# make gh-actions 
$ vim ./build.zsh
$ ./scpt/gh-actions.zsh
```
