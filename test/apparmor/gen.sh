#!/bin/sh
# test/apparmor/test.profile を apparmor_parser (Debian / Ubuntu の apparmor) でバイナリにして、test/apparmor.c が
# 読みこむ blob.h に。aios が名のるもの (features: kernel/src/apparmor.rs の FEATURES と同じ) に合う形で作る。
# プロファイルを変えたら動かしなおして blob.h もコミットする (テストを作るところに apparmor_parser はいらない)
# AF_UNIX の細かい決まり (network_v9/af_unix) を書けるのは 5.0 から。それより古いと unix の決まりがみな
# network unix に書きおろされる (deny は落ちる) ので、APPARMOR_PARSER で 5.0 からのものを渡せる
set -e
cd "$(dirname "$0")"
P=${APPARMOR_PARSER:-apparmor_parser}
command -v "$P" >/dev/null || { echo "gen.sh: apparmor_parser がない (apt install apparmor)" >&2; exit 2; }
v=$("$P" --version | head -1 | awk '{print $NF}')
[ "${v%%.*}" -ge 5 ] || { echo "gen.sh: apparmor_parser $v は network_v9/af_unix を知らない (5.0 から)。APPARMOR_PARSER で渡す" >&2; exit 2; }
# プロファイルの abi <abi/4.0> には network_v9 がないので、features をそのまま abi にする (--override-policy-abi)
"$P" -Q -T --features-file features --override-policy-abi features -S test.profile > blob.bin
{
  echo "/* test/apparmor/gen.sh が test.profile から作ったもの (apparmor_parser $v) */"
  echo "static const unsigned char aa_blob[] = {"
  od -An -v -tx1 blob.bin | sed 's/ *\([0-9a-f][0-9a-f]\)/0x\1,/g; s/^/ /'
  echo "};"
} > blob.h
rm -f blob.bin
echo "gen.sh: blob.h ($(grep -o 0x blob.h | wc -l) bytes)"
