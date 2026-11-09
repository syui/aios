#!/bin/sh
set -e
cd "$(dirname "$0")/.."
firefox=
[ "$1" = --firefox ] && firefox=1

if [ "$(id -u)" = 0 ]; then
  sudo=
else
  sudo=sudo
fi

if [ -f /var/lib/dpkg/statoverride ]; then
  while read -r user group _mode _path; do
    getent group "$group" >/dev/null || $sudo groupadd -r "$group"
    getent passwd "$user" >/dev/null || $sudo useradd -r -g "$group" -s /usr/sbin/nologin -M "$user"
  done < /var/lib/dpkg/statoverride
fi

need=
want() {
  command -v "$1" >/dev/null 2>&1 || need="$need $2"
}
want clang-20 clang-20
want llvm-nm-20 llvm-20
want ld.lld lld
want cmake cmake
want ninja ninja-build
want pkg-config pkg-config
want flex flex
want bison bison
want glslangValidator glslang-tools
want wayland-scanner libwayland-bin
want qemu-aarch64-static qemu-user-static
want qemu-system-aarch64 qemu-system-arm
want mcopy mtools
want mkfs.vfat dosfstools
want sfdisk fdisk
want mkfs.ext4 e2fsprogs
want debugfs e2fsprogs
want aarch64-linux-gnu-gcc gcc-aarch64-linux-gnu
want xz xz-utils
want zstd zstd
want unzip unzip
want bsdtar libarchive-tools
want git git
want gpg gnupg
want pip3 python3-pip
[ -f /usr/include/expat.h ] || need="$need libexpat1-dev"
if [ -n "$firefox" ]; then
  [ -e /usr/lib/llvm-20/lib/libclang.so ] || need="$need libclang-20-dev"
  want node nodejs
fi
if [ -n "$need" ]; then
  echo "setup-build: apt:$need"
  $sudo apt-get update -q
  DEBIAN_FRONTEND=noninteractive $sudo apt-get install -y -q $need
fi

if ! command -v qemu-aarch64 >/dev/null 2>&1; then
  $sudo ln -sf "$(command -v qemu-aarch64-static)" /usr/local/bin/qemu-aarch64
fi

py=
command -v meson >/dev/null 2>&1 || py="$py meson"
python3 -c 'import mako' 2>/dev/null || py="$py mako"
python3 -c 'import yaml' 2>/dev/null || py="$py pyyaml"
if [ -n "$py" ]; then
  echo "setup-build: pip:$py"
  $sudo pip3 install -q --break-system-packages $py 2>/dev/null || $sudo pip3 install -q $py
fi

if command -v rustup >/dev/null 2>&1; then
  for t in aarch64-unknown-linux-musl aarch64-unknown-none-softfloat; do
    rustup target list --installed | grep -qx "$t" || rustup target add "$t"
  done
fi
if [ -n "$firefox" ] && ! command -v cbindgen >/dev/null 2>&1; then
  cargo install --locked cbindgen
fi

miss=
for c in clang-20 llvm-nm-20 cmake ninja meson pkg-config flex bison glslangValidator qemu-aarch64 qemu-system-aarch64 \
  mcopy mkfs.vfat sfdisk mkfs.ext4 debugfs aarch64-linux-gnu-gcc xz zstd unzip bsdtar git gpg; do
  command -v "$c" >/dev/null 2>&1 || miss="$miss $c"
done
python3 -c 'import mako, yaml' 2>/dev/null || miss="$miss python3-mako/yaml"
if [ -n "$miss" ]; then
  echo "setup-build: still missing:$miss" >&2
  exit 1
fi
echo "setup-build: ok"
