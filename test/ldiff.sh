#!/bin/sh
# 出すだけのテスト (test/NAME.c) を、手本の Linux と aios の VM で動かして、出力をくらべる (答えあわせを 1 つの口で)
#   test/ldiff.sh [--root] [--linux VER] [--fresh] [--kernel .|FILE|COMMIT] NAME [ARG...]
#     1. test/NAME.c を aarch64 の静的なもの (test/bin/NAME) に作る (なければ、.c のほうが新しければ)
#     2. 手本の Linux (test/linux-vm.sh exec VER。既定は mainline) で動かす。答えは build/linux-answers/ にとっておき、
#        プログラムと引数と Linux の版と test/linux.config が同じなら、もう一度は動かさない (--fresh で動かしなおす)
#     3. いま動いている aios の VM (test/vm.py start --www test) で動かす (Linux と同時に)。--kernel なら、そのカーネル
#        (. は作業中のソース) を作って VM を起こしなおしてから (test/vm.py start --kernel ... --snapshot --www test)
#     4. くらべる: 同じなら "same: N lines"、ちがえば diff -u (--- Linux、+++ aios) と、それぞれの終わりの番号
#   --root: Linux は rootexec、aios は sudo -n で (test/apparmor.c のようなもの)
#   引数の @home は ext4 のところ: Linux は /work、aios はホーム ($HOME) に読みかえる (test/chosen.c @home /tmp など)
#     5. 手本の Linux に足りない機能 (aios にはある) を知らせる: test/features.c を両方で動かしてくらべる。テストが使う
#        機能が手本の Linux になければ、ENOSYS や EINVAL がテストの答えにかくれて同じに見えることがある (そのときは
#        test/linux.config に足す)。LDIFF_NO_FEATURES=1 で見ない
#   出力: build/ldiff/NAME.linux と NAME.aios。終わりの番号: 同じなら 0、ちがえば 1、動かせなければ 2
# Linux は nobody で、aios は ai で動く。uid や pid の番号をじかに出すテストは、そのぶんちがって見える (あてはまるかを出す)
set -e
top=$(cd "$(dirname "$0")/.." && pwd)
cd "$top"
root= ver=mainline fresh= kernel=
while :; do
  case "$1" in
  --root) root=1; shift ;;
  --kernel) kernel=$2; shift 2 ;;
  --linux) ver=$2; shift 2 ;;
  --fresh) fresh=1; shift ;;
  *) break ;;
  esac
done
name=$1
[ -n "$name" ] || { sed -n '2,12p' "$0" >&2; exit 2; }
shift
src=test/$name.c bin=test/bin/$name
[ -f "$src" ] || { echo "ldiff: no $src" >&2; exit 2; }
mkdir -p test/bin build/ldiff build/linux-answers
if [ ! -x "$bin" ] || [ "$src" -nt "$bin" ]; then
  echo "ldiff: building $bin" >&2
  aarch64-linux-gnu-gcc -O2 -static -pthread -o "$bin" "$src" 2>&1 | grep -E 'error' >&2 || true
  [ -x "$bin" ] && [ ! "$src" -nt "$bin" ] || { echo "ldiff: cannot build $bin" >&2; exit 2; }
fi

# aios の VM は動いているか (test/ を http で見せているもの)
if [ -n "$kernel" ]; then
  python3 test/vm.py start --kernel "$kernel" --snapshot --www test >/dev/null || { echo "ldiff: the VM did not come up" >&2; exit 2; }
fi
st=$(python3 test/vm.py status 2>/dev/null || true)
case "$st" in
*'"ready": true'*'"www": "'"$top"/test'"'*) ;;
*) echo "ldiff: no aios VM with test/ served; start one: test/vm.py start --kernel . --snapshot --www test (or ldiff --kernel .)" >&2; exit 2 ;;
esac

# Linux の答え: とっておいたものか、動かす (うしろで)
v=$(sh test/linux-vm.sh version "$ver")
key=$(cat "$bin" test/linux.config | sha256sum | cut -c1-16)-$(printf '%s %s %s' "$v" "$root" "$*" | sha256sum | cut -c1-8)
ans=build/linux-answers/$name-$key
lout=build/ldiff/$name.linux aout=build/ldiff/$name.aios
lpid=
if [ -z "$fresh" ] && [ -s "$ans.out" ]; then
  echo "ldiff: Linux $v: using the saved answer ($ans.out)" >&2
else
  echo "ldiff: running $name in Linux $v" >&2
  (
    set +e
    for a in "$@"; do
      shift
      case "$a" in @home*) set -- "$@" "/work${a#@home}" ;; *) set -- "$@" "$a" ;; esac
    done
    sh test/linux-vm.sh "$([ -n "$root" ] && echo rootexec || echo exec)" "$v" "$bin" "$@" > "$ans.tmp" 2> "$ans.err"
    echo $? > "$ans.st"
    mv "$ans.tmp" "$ans.out"
  ) &
  lpid=$!
fi

# aios で (うしろの job: 長いテストでも run の時間切れにならないように)
echo "ldiff: running $name in aios" >&2
args=
for a in "$@"; do
  case "$a" in
  @home*) args="$args \"\$HOME\"'$(printf '%s' "${a#@home}" | sed "s/'/'\\\\''/g")'" ;;
  *) args="$args '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'" ;;
  esac
done
run="./$name$args"
[ -n "$root" ] && run="sudo -n ./$name$args"
job=$(python3 test/vm.py bg "mkdir -p /tmp/ldiff && cd /tmp/ldiff && fetch http://10.0.2.2:8000/bin/$name -o $name >/dev/null && chmod +x $name && $run > /tmp/ldiff/$name.out 2> /tmp/ldiff/$name.err; echo \$? > /tmp/ldiff/$name.st" |
  python3 -c 'import json,sys; print(json.load(sys.stdin).get("job", ""))')
[ -n "$job" ] || { echo "ldiff: cannot start it in the VM" >&2; exit 2; }
while :; do
  r=$(python3 test/vm.py job "$job" -w 30 2>&1 || true)
  case "$r" in
  *'"done": true'*) break ;;
  *'"error"'*) echo "ldiff: vm job: $r" >&2; exit 2 ;;
  esac
done
python3 test/vm.py get "/tmp/ldiff/$name.out" "$aout" >/dev/null
ast=$(python3 test/vm.py run "cat /tmp/ldiff/$name.st; tail -3 /tmp/ldiff/$name.err" -t 30 || true)

if [ -n "$lpid" ]; then
  kill -0 "$lpid" 2>/dev/null && echo "ldiff: waiting for Linux $v (its progress: $ans.err)" >&2
  wait "$lpid" || true
  [ -s "$ans.out" ] || { echo "ldiff: Linux gave no output:" >&2; tail -5 "$ans.err" >&2; exit 2; }
fi
cp "$ans.out" "$lout"
lst=$(cat "$ans.st" 2>/dev/null || echo '?')

# 手本の Linux に足りない機能 (aios にはある)。features そのものと、その中からは見ない
lacks() {
  [ "$name" = features ] && return 0
  [ -n "${LDIFF_NO_FEATURES:-}" ] && return 0
  LDIFF_NO_FEATURES=1 sh "$0" --linux "$ver" features >/dev/null 2>&1 || true
  l=build/ldiff/features.linux a=build/ldiff/features.aios
  [ -s "$l" ] && [ -s "$a" ] || return 0
  miss=$(awk 'NR == FNR { lin[$1] = $2; next } $2 == "yes" && lin[$1] == "no" { printf "%s ", $1 }' "$l" "$a")
  [ -z "$miss" ] || echo "note: the reference Linux lacks what aios has: $miss(test/features.c; if the test uses them, add them to test/linux.config)"
}

if cmp -s "$lout" "$aout"; then
  echo "same: $(wc -l < "$lout") lines (Linux $v exit $lst, aios exit $(echo "$ast" | head -1))"
  lacks
  exit 0
fi
diff -u --label "Linux $v" --label aios "$lout" "$aout" || true
echo "differ: Linux $v exit $lst, aios exit $(echo "$ast" | head -1)"
echo "$ast" | tail -n +2 | sed 's/^/aios stderr: /'
lacks
exit 1
