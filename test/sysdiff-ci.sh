#!/bin/sh
# ロボット (test/sysdiff.c) を CI で: 手本の Linux の答えを test/sysdiff/ に置いておき、push のたびに aios の VM で
# 同じ case を動かしてくらべる (0 differ になったものが、いつの間にかくずれないように)。答えはアーキテクチャによらない
# (sysdiff.c の出力の決まり) ので、Linux を CI で起こさなくてよい
#   作る (外で、手本の Linux の答え): test/sysdiff-ci.sh golden [latest|X.Y.Z]
#       test/linux-vm.sh で作った Linux で SETS をみな動かし、test/sysdiff/linux-FS-OPS.txt.gz に。sysdiff.c の
#       sha256 と Linux の版は test/sysdiff/linux.sum に。test/sysdiff.c を変えたら作りなおす
#   作る (外で、VM に渡すもの): test/sysdiff-ci.sh build [CC]
#       test/bin/sysdiff (静的なもの。既定は aarch64-linux-gnu-gcc、arm64 の CI では gcc)。sysdiff.c が答えを
#       作ったときのものでなければ止まる
#   動かす (中で): fetch http://10.0.2.2:8000/sysdiff-ci.sh > /tmp/sd.sh && sh /tmp/sd.sh run
#       (test/ を http で見せている VM: test/vm.py start --www test)。出力は /tmp/sysdiff/FS-OPS.out
#   くらべる (外で): test/sysdiff-ci.sh compare
#       test/vm.py get で出力を取り、case ごとに最初にちがう行を出す。ちがうものか、ないものがあれば 1
# aish の sysdiff (aish-vm) と同じく、aios の中は ai で、home はホーム (ext4)、tmp は /tmp (tmpfs)。Linux は nobody で
# SETS は FS:OPS:CASES (aish の sysdiff でふだん確かめている大きさ)
SETS="home:30:1-300 home:80:1-200 tmp:30:1-300"
top=$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)
golden=$top/test/sysdiff

sum() {
  sha256sum "$top/test/sysdiff.c" | cut -d' ' -f1
}

case $1 in
golden)
  set -e
  v=$(sh "$top/test/linux-vm.sh" version "${2:-latest}")
  image=$(sh "$top/test/linux-vm.sh" build "$v" | tail -1)
  cc=${CC:-aarch64-linux-gnu-gcc}
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  $cc -O2 -static -s -o "$tmp/sysdiff" "$top/test/sysdiff.c"
  mkdir -p "$golden"
  for s in $SETS; do
    fs=${s%%:*} rest=${s#*:}
    ops=${rest%%:*} cases=${rest#*:}
    echo "$cases" > "$tmp/cases.txt"
    echo "sysdiff-ci: Linux $v $fs $ops ops, cases $cases" >&2
    sh "$top/test/linux-vm.sh" run "$image" "$tmp/sysdiff" "$tmp/cases.txt" "$ops" "$fs" "$tmp/out.txt"
    gzip -9n -c "$tmp/out.txt" > "$golden/linux-$fs-$ops.txt.gz"
  done
  printf 'sysdiff.c %s\nlinux %s\ncc %s\n' "$(sum)" "$v" "$($cc --version | head -1)" > "$golden/linux.sum"
  cat "$golden/linux.sum"
  ;;
build)
  cc=${2:-aarch64-linux-gnu-gcc}
  want=$(sed -n 's/^sysdiff\.c //p' "$golden/linux.sum" 2>/dev/null)
  if [ "$want" != "$(sum)" ]; then
    echo "sysdiff-ci: test/sysdiff.c changed since the Linux answers in test/sysdiff/ were made: run test/sysdiff-ci.sh golden" >&2
    exit 2
  fi
  mkdir -p "$top/test/bin" && $cc -O2 -static -s -o "$top/test/bin/sysdiff" "$top/test/sysdiff.c" || exit 2
  ;;
run)
  # 中 (aios) で。SETS をみな動かし、終わりの番号を 1 行ずつ
  mkdir -p /tmp/sysdiff && cd /tmp/sysdiff || exit 2
  if ! fetch http://10.0.2.2:8000/bin/sysdiff -o sysdiff >/dev/null 2>&1 || ! chmod +x sysdiff; then
    echo "sysdiff-ci: cannot fetch sysdiff"
    exit 2
  fi
  fail=0
  for s in $SETS; do
    fs=${s%%:*} rest=${s#*:}
    ops=${rest%%:*} cases=${rest#*:}
    if [ "$fs" = tmp ]; then work=/tmp/sd; else work=$HOME/sd; fi
    mkdir -p "$work"
    ./sysdiff -n "$ops" "$work" "$cases" > "$fs-$ops.out" 2> "$fs-$ops.err"
    st=$?
    echo "sysdiff-ci: $fs $ops ops, cases $cases: status $st"
    [ "$st" = 0 ] || { tail -3 "$fs-$ops.err"; fail=$((fail + 1)); }
  done
  [ "$fail" = 0 ]
  ;;
compare)
  out=$top/build/sysdiff-ci
  mkdir -p "$out"
  for s in $SETS; do
    fs=${s%%:*} rest=${s#*:}
    ops=${rest%%:*}
    python3 "$top/test/vm.py" get "/tmp/sysdiff/$fs-$ops.out" "$out/$fs-$ops.out" >/dev/null || exit 2
  done
  python3 - "$golden" "$out" $SETS <<'EOF'
import gzip, sys
golden, out, sets = sys.argv[1], sys.argv[2], sys.argv[3:]

def split(text):
    """case ごとの行 (sys の行はそのカーネルのものなので、くらべない)"""
    cases, cur = {}, None
    for l in text.splitlines():
        if l.startswith('case '):
            cur = cases.setdefault(l[5:], [])
        elif cur is not None:
            cur.append(l)
    return cases

bad = 0
for s in sets:
    fs, ops, _ = s.split(':')
    with gzip.open('%s/linux-%s-%s.txt.gz' % (golden, fs, ops), 'rt') as f:
        linux = split(f.read())
    with open('%s/%s-%s.out' % (out, fs, ops)) as f:
        aios = split(f.read())
    same, differ, missing = 0, [], []
    for c, l in linux.items():
        a = aios.get(c)
        if a is None:
            missing.append(c)
        elif a == l:
            same += 1
        else:
            i = next((i for i, (x, y) in enumerate(zip(l, a)) if x != y), min(len(l), len(a)))
            differ.append((c, l[i] if i < len(l) else '(none)', a[i] if i < len(a) else '(none)'))
    print('sysdiff-ci: %s %s ops: %d same, %d differ, %d missing (of %d)' % (fs, ops, same, len(differ), len(missing), len(linux)))
    for c, x, y in differ[:5]:
        print('  case %s\n    linux: %s\n    aios:  %s' % (c, x, y))
    if missing:
        print('  missing cases: %s' % ' '.join(missing[:10]))
    bad += len(differ) + len(missing)
# ちがいは aish の sysdiff (linux= に答えのファイル) で縮めて見られる
sys.exit(1 if bad else 0)
EOF
  ;;
*)
  echo "usage: test/sysdiff-ci.sh golden [latest|X.Y.Z] | build [CC] | run | compare" >&2
  exit 2
  ;;
esac
