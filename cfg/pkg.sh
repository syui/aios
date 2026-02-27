#!/bin/bash

set -e

ROOTFS="$1"

arch-chroot $ROOTFS /bin/sh -c 'rm -rf /tmp/gpt /tmp/log /tmp/shell'
arch-chroot $ROOTFS /bin/sh -c '
cd /tmp && git clone https://git.syui.ai/ai/gpt && cd /tmp/gpt && cargo build --release && cp target/release/aigpt /usr/local/bin/
'
arch-chroot $ROOTFS /bin/sh -c '
cd /tmp && git clone -b main https://git.syui.ai/ai/log && cd /tmp/log && cargo build --release && cp target/release/ailog /usr/local/bin/
'
arch-chroot $ROOTFS /bin/sh -c '
cd /tmp && git clone https://git.syui.ai/ai/shell && cd /tmp/shell && cargo build --release && cp target/release/aishell /usr/local/bin/
'
arch-chroot $ROOTFS /bin/sh -c 'rm -rf /tmp/gpt /tmp/log /tmp/shell'
