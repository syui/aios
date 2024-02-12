#!/bin/zsh

d=${0:a:h}
dd=${0:a:h:h}

yml_a='name: release

on:
  push:
    branches:
    - main
  schedule:
    - cron:  "0 0 * * *"

permissions:
  contents: write

env:
  DOCKER_TOKEN: ${{ secrets.DOCKER_TOKEN }}
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
      - name: Initialize
        run: |'

yml_c='          tar -C ./root.x86_64 -c . | docker import - ${{ env.IMAGE_NAME }}
          echo "${{ env.DOCKER_TOKEN }}" | docker login -u syui --password-stdin
          docker push ${{ env.IMAGE_NAME }}

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

      - name: Create new release
        uses: softprops/action-gh-release@v1
        with:
          name: latest
          tag_name: latest
          files:
            aios-bootstrap.tar.gz
'

yml_b=`cat $dd/build.zsh |sed '1d'`

echo $yml_a >! $dd/cfg/gh-actions.yml
echo $yml_b|sed 's/^/          /g' >> $dd/cfg/gh-actions.yml
echo $yml_c >> $dd/cfg/gh-actions.yml
cat $dd/cfg/gh-actions.yml

echo '#!/bin/zsh' >! $dd/build.zsh
echo $yml_b >> $dd/build.zsh
cat $dd/build.zsh

