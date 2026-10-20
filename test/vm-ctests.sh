#!/bin/sh
# カーネルの C のテスト (test/*.c) を aios の中で動かす。CI の VM test で、vm-check.sh のあとに:
#   作る (外で): test/vm-ctests.sh build [CC]   test/bin/NAME に静的なプログラム (既定は gcc。aarch64 の Linux で)
#   動かす (中で): fetch http://10.0.2.2:8000/vm-ctests.sh > /tmp/t.sh && sh /tmp/t.sh run
#                 (test/ を http で見せている VM: test/vm.py start --www test。プログラムは bin/ から取る)
# どれもしくじった数を終了コードで返す。1 行に ok NAME か FAIL NAME。おしまいに ctests: N failed
# hang.c と watch.c はテストではなく、わざと止まる道具なので入れない
TESTS="chosen clone3 cpuid dev dirents epoll exec-thread fsrace fsx inotify landlock-eexist mmapcoh mnt ns openat2 pagecache pathcache pipeslots procmaps procwait random rlock seccomp sigtramp splice statcache unixsock userns"

case $1 in
build)
  cc=${2:-gcc}
  cd "$(dirname "$0")" && mkdir -p bin || exit 2
  for t in $TESTS; do
    $cc -O2 -static -pthread -o "bin/$t" "$t.c" || { echo "vm-ctests: cannot build $t" >&2; exit 2; }
  done
  ;;
run)
  dir=/tmp/ctests
  mkdir -p "$dir" && cd "$dir" || exit 2
  fail=0
  for t in $TESTS; do
    if ! fetch "http://10.0.2.2:8000/bin/$t" -o "$t" >/dev/null 2>&1 || ! chmod +x "$t"; then
      echo "FAIL $t: cannot fetch"
      fail=$((fail + 1))
      continue
    fi
    # landlock-eexist は砂場の中で (書けない / と /home に、もうあるものを作ろうとする)
    # pathcache は tmpfs (/tmp) と ext4 (ホーム) で
    case $t in
    landlock-eexist) cmd="aibox -- ./$t" ;;
    pathcache) cmd="./$t /tmp $HOME" ;;
    # pipeslots は ext4 (ホーム) のファイルから sendfile する
    pipeslots) cmd="./$t $HOME/.pipeslots" ;;
    # fsx は ext4 (ホーム) と tmpfs (/tmp) で、ふつうと frag
    fsx) cmd="./$t --suite $HOME /tmp" ;;
    # chosen (aios がえらんだちがい)、mmapcoh (ファイルのページは 1 枚)、rlock (ファイルのロック) も ext4 と tmpfs で
    chosen | mmapcoh | rlock) cmd="./$t $HOME /tmp" ;;
    *) cmd="./$t" ;;
    esac
    # pathcache は root でも (マウントと、ほかの人から引いたとき。ext4 はほかの人も通れる /var/tmp で)
    root=
    [ "$t" = pathcache ] && [ "$(id -u)" != 0 ] && root="sudo -n ./$t /tmp /var/tmp"
    if timeout 300 $cmd > "$t.out" 2>&1 && { [ -z "$root" ] || timeout 300 $root >> "$t.out" 2>&1; }; then
      echo "ok $t"
    else
      echo "FAIL $t (status $?):"
      tail -15 "$t.out" | sed 's/^/  /'
      fail=$((fail + 1))
    fi
  done
  echo "ctests: $fail failed"
  [ "$fail" = 0 ]
  ;;
*)
  echo "usage: test/vm-ctests.sh build [CC] | run" >&2
  exit 2
  ;;
esac
