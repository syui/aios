#!/bin/bash

set -e

ROOTFS="$1"

arch-chroot $ROOTFS /bin/sh -c '
cd /tmp
git clone https://git.syui.ai/ai/gpt && cd gpt && cargo build --release && cp target/release/aigpt /usr/local/bin/ && cd ..
git clone https://git.syui.ai/ai/log && cd log && cargo build --release && cp target/release/ailog /usr/local/bin/ && cd ..
git clone https://git.syui.ai/ai/shell && cd shell && cargo build --release && cp target/release/aishell /usr/local/bin/ && cd ..
rm -rf gpt log shell
'
