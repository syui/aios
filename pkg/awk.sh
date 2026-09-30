#!/bin/sh
# uutils/awk。rust-toolchain.toml で版が決まっているので、その版にも musl の target を入れる
. "$(dirname "$0")/lib.sh"
fetch awk https://github.com/uutils/awk
rustup target add "$TARGET" >/dev/null
musl_build
