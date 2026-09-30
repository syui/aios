#!/bin/sh
# uutils/coreutils (ls, cat, cp, ... をまとめた 1 つのバイナリ)
. "$(dirname "$0")/lib.sh"
fetch coreutils https://github.com/uutils/coreutils
musl_build --no-default-features --features feat_os_unix_musl
