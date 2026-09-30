#!/bin/sh
# uutils/findutils (find, xargs)
. "$(dirname "$0")/lib.sh"
fetch findutils https://github.com/uutils/findutils
musl_build --bin find --bin xargs
