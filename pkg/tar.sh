#!/bin/sh
# uutils/tar (バイナリの名前は tarapp)
. "$(dirname "$0")/lib.sh"
fetch tar https://github.com/uutils/tar
musl_build --bin tarapp
