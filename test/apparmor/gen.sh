#!/bin/sh
# test/apparmor/test.profile を apparmor_parser (Debian / Ubuntu の apparmor) でバイナリにして、test/apparmor.c が
# 読みこむ blob.h に。aios が名のるもの (features: kernel/src/apparmor.rs の FEATURES と同じ) に合う形で作る (aa_blob)。
# Linux 7.3.1 は network_v9/af_inet を名のらない (aios がえらんだちがい apparmor-inet) ので、それをのぞいた features でも
# 作り (aa_blob_linux: ip= や port= のある inet の決まりは (inet, type) に広がる)、Linux ではそちらを読む。
# プロファイルを変えたら動かしなおして blob.h もコミットする (テストを作るところに apparmor_parser はいらない)
# AF_UNIX と AF_INET の細かい決まり (network_v9/af_unix、af_inet) を書けるのは 5.0 から。それより古いと unix の決まりがみな
# network unix に書きおろされる (deny は落ちる) ので、APPARMOR_PARSER で 5.0 からのものを渡せる
set -e
cd "$(dirname "$0")"
P=${APPARMOR_PARSER:-apparmor_parser}
command -v "$P" >/dev/null || { echo "gen.sh: apparmor_parser がない (apt install apparmor)" >&2; exit 2; }
v=$("$P" --version | head -1 | awk '{print $NF}')
[ "${v%%.*}" -ge 5 ] || { echo "gen.sh: apparmor_parser $v は network_v9/af_unix を知らない (5.0 から)。APPARMOR_PARSER で渡す" >&2; exit 2; }
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
sed '/^af_inet {yes$/,/^}$/d' features > "$t/features.linux"
! grep -q af_inet "$t/features.linux"
# プロファイルの abi <abi/4.0> には network_v9 がないので、features をそのまま abi にする (--override-policy-abi)
"$P" -Q -T --features-file features --override-policy-abi features -S test.profile > "$t/aios.bin"
"$P" -Q -T --features-file "$t/features.linux" --override-policy-abi "$t/features.linux" -S test.profile > "$t/linux.bin"
arr() {
  echo "static const unsigned char $1[] = {"
  od -An -v -tx1 "$2" | sed 's/ *\([0-9a-f][0-9a-f]\)/0x\1,/g; s/^/ /'
  echo "};"
}
{
  echo "/* test/apparmor/gen.sh が test.profile から作ったもの (apparmor_parser $v)。aa_blob は aios の features で、"
  echo "   aa_blob_linux は network_v9/af_inet をのぞいた features (Linux 7.3.1 と同じ) で */"
  arr aa_blob "$t/aios.bin"
  arr aa_blob_linux "$t/linux.bin"
} > blob.h
echo "gen.sh: blob.h (aa_blob $(wc -c < "$t/aios.bin") bytes, aa_blob_linux $(wc -c < "$t/linux.bin") bytes)"
