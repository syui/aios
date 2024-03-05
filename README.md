## <img src="./icon/ai.png" width="30"> ai `os` 

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

### base

```sh
# https://gitlab.archlinux.org/archlinux
$ git clone https://gitlab.archlinux.org/archlinux/archiso
```

### docker

```sh
# https://git.syui.ai/ai/-/packages/container/os
$ docker run -it git.syui.ai/ai/os ai

# https://hub.docker.com/r/syui/aios
$ docekr run -it syui/aios ai

# https://github.com/users/syui/packages/container/package/aios
$ docker run -it ghcr.io/syui/aios ai
```

### link

- https://git.syui.ai/ai/os
- https://github.com/syui/aios
