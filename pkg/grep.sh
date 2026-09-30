#!/bin/sh
# uutils/grep (正規表現は oniguruma)
. "$(dirname "$0")/lib.sh"
fetch grep https://github.com/uutils/grep
musl_build
