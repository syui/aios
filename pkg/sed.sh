#!/bin/sh
# uutils/sed
. "$(dirname "$0")/lib.sh"
fetch sed https://github.com/uutils/sed
musl_build
