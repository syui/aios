#!/bin/sh
# uutils/diffutils (diff と cmp をまとめた 1 つのバイナリ)
. "$(dirname "$0")/lib.sh"
fetch diffutils https://github.com/uutils/diffutils
musl_build
