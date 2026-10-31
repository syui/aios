#!/bin/sh
# test/apparmor/test.profile を apparmor_parser (Debian / Ubuntu の apparmor) でバイナリにして、test/apparmor.c が
# 読みこむ blob.h に。aios が名のるもの (features: kernel/src/apparmor.rs の FEATURES と同じ) に合う形で作る。
# プロファイルを変えたら動かしなおして blob.h もコミットする (テストを作るところに apparmor_parser はいらない)
set -e
cd "$(dirname "$0")"
command -v apparmor_parser >/dev/null || { echo "gen.sh: apparmor_parser がない (apt install apparmor)" >&2; exit 2; }
apparmor_parser -Q -T --features-file features -S test.profile > blob.bin
{
  echo "/* test/apparmor/gen.sh が test.profile から作ったもの (apparmor_parser $(apparmor_parser --version | head -1 | awk '{print $NF}')) */"
  echo "static const unsigned char aa_blob[] = {"
  od -An -v -tx1 blob.bin | sed 's/ *\([0-9a-f][0-9a-f]\)/0x\1,/g; s/^/ /'
  echo "};"
} > blob.h
rm -f blob.bin
echo "gen.sh: blob.h ($(grep -o 0x blob.h | wc -l) bytes)"
