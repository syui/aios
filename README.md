# <img src="./icon/ai.png" width="30"> ai `os` 

`aios` is a simple linux distribution based on `archlinux`.

|rule|var|
|---|---|
|name|ai os|
|code|aios|
|id|ai|
|container|[git.syui.ai/ai/os](https://git.syui.ai/ai/-/packages/container/os/latest)|
|image|[aios-bootstrap.tar.gz](https://github.com/syui/aios/releases/download/latest/aios-bootstrap.tar.gz)|

```sh
$ docker run -it git.syui.ai/ai/os ai
```

## link

|host|command|url|
|---|---|---|
|docker|syui/aios|https://hub.docker.com/r/syui/aios|
|github|ghcr.io/syui/aios|https://github.com/users/syui/packages/container/package/aios|
|syui|git.syui.ai/ai/os|https://git.syui.ai/ai/-/packages/container/os|

## base

```sh
# https://gitlab.archlinux.org/archlinux
$ git clone https://gitlab.archlinux.org/archlinux/archiso
```

## docker

```sh
# https://git.syui.ai/ai/-/packages/container/os
$ docker run -it git.syui.ai/ai/os ai

# https://hub.docker.com/r/syui/aios
$ docekr run -it syui/aios ai

# https://github.com/users/syui/packages/container/package/aios
$ docker run -it ghcr.io/syui/aios ai
```

## token

|env|body|
|---|---|
|${{ github.repository }}|syui/aios|
|${{ secrets.DOCKER_USERNAME }}|syui|
|${{ secrets.DOCKER_TOKEN }}|[token](https://matsuand.github.io/docs.docker.jp.onthefly/docker-hub/access-tokens/)|
|${{ secrets.APP_TOKEN }}|[token](https://docs.github.com/ja/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens), pacakge|

## podman

```sh
if [ ! -d ~/ai/os/.git ];then
    mkdir -p ~/ai
    git clone https://git.syui.ai/ai/os ~/ai/os
fi
if [ ! -d ~/.config/containers/registries.conf.d ];then
    mkdir -p ~/.config/containers/registries.conf.d
fi
ln -s ~/ai/os/.config/containers/registries.conf.d/ai.conf ~/.config/containers/registries.conf.d/ai.conf
```

```sh
$ podman pull aios
```

> ~/.config/containers/registries.conf.d/ai.conf

```sh
# https://github.com/containers/shortnames
# ~/.config/containers/registries.conf.d/ai.conf
unqualified-search-registries = ['git.syui.ai', 'docker.io', 'ghcr.io']

[aliases]
"aios" = "git.syui.ai/ai/os"
```

```sh
$ podman pull aios
Resolved "aios" as an alias (/etc/containers/registries.conf.d/ai.conf)
Trying to pull git.syui.ai/ai/os:latest...
Getting image source signatures
Copying blob c7e55fecf0be [====================>-----------------] 917.4MiB / 1.7GiB
```

## cron

stop

```sh
  schedule:
      - cron:  "0 0 * * *"
```

## update action

```sh
$ vim build.zszh
$ ./scpt/gh-actions.zsh
```

## link

- https://git.syui.ai/ai/os
- https://github.com/syui/aios

