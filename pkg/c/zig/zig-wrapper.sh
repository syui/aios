#!/bin/sh
# /opt/c/bin/zig: /opt/zig/zig を呼ぶ。C や Zig を作るサブコマンドで -target がなければ -target aarch64-linux-musl を足す
# (/opt/c/bin/cc と同じ)。
# zig はこのマシンの ABI (gnu か musl か) を /usr/bin/env の動的リンカで調べるが、aios のものは静的なので決められず、
# glibc をさがして「failed to find libc installation」で止まる。native-native-musl では zig の持つ musl のヘッダを使わない
case $1 in
cc | c++ | build-exe | build-lib | build-obj | run | test | translate-c)
  for a in "$@"; do
    case $a in
    -target | -target=* | --target | --target=*) exec /opt/zig/zig "$@" ;;
    esac
  done
  sub=$1
  shift
  exec /opt/zig/zig "$sub" -target aarch64-linux-musl "$@"
  ;;
esac
exec /opt/zig/zig "$@"
