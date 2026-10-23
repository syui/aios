# aios の中で動かす短いたしかめ (CI の起動のテストのあと、test/vm.py run "$(cat test/vm-check.sh)" で)。
# 外 (このマシンの aish のテスト) ではわからない、カーネルとイメージの組み合わせだけを見る:
#   /sys と /proc (psutil などが読むところ)、イメージに入っているコマンド (tar は bsdtar、awk は gawk)、
#   sh の名前の aish (printf と select、/dev/fd)、aibox (seccomp と namespace、--tmp のマウント)、sudo
# 1 行に ok NAME か FAIL NAME。おしまいに checks: N failed (0 でなければ失敗のステータス)
fail=0
n=0
t() {
  name=$1
  shift
  n=$((n + 1))
  if "$@" >/dev/null 2>&1; then
    echo "ok $name"
  else
    echo "FAIL $name"
    fail=$((fail + 1))
  fi
}
eq() {
  [ "$1" = "$2" ] || { echo "  want: $2" >&2; echo "  got:  $1" >&2; return 1; }
}

# /sys: ディスク (大きさは /proc/partitions と同じか)、ネットワーク、CPU
# ls は使わない (/etc/aishrc の alias ls='ls -a' で . と .. も出る)
for d in /sys/block/*; do disk=${d##*/}; break; done
t sys-block-size test "$(cat /sys/block/$disk/size)" -gt 0
t sys-block-part test -n "$(ls /sys/block/$disk | grep "^$disk")"
t sys-block-stat test "$(wc -w < /sys/block/$disk/stat)" -eq 11
t sys-net-mac grep -q ':' /sys/class/net/eth0/address
t sys-net-rx test "$(cat /sys/class/net/eth0/statistics/rx_bytes)" -gt 0
t sys-cpu-online grep -q '^0' /sys/devices/system/cpu/online
t sys-readonly sh -c '! echo x > /sys/block/'"$disk"'/size'
t sys-in-mounts grep -q '^sysfs /sys sysfs' /proc/mounts

# /proc
t proc-net-dev grep -q 'eth0:' /proc/net/dev
t proc-diskstats grep -q " $disk " /proc/diskstats
t proc-stat grep -q '^cpu ' /proc/stat
t proc-self-fd test -L /proc/self/fd/0

# イメージのコマンド (base が replaces / provides で入れかえたもの)
t tar-is-bsdtar sh -c 'tar --version | grep -q bsdtar'
t awk-is-gawk sh -c 'awk --version | grep -q "GNU Awk"'
t tar-roundtrip sh -c 'cd /tmp && mkdir -p vc/a && echo hi > vc/a/f && tar -czf vc.tgz vc && rm -rf vc && tar -xzf vc.tgz && [ "$(cat vc/a/f)" = hi ]'

# sh の名前の aish (bash の書き方)
t sh-printf-q eq "$(sh -c "printf '%q' 'a b'")" 'a\ b'
t sh-printf-time eq "$(TZ=UTC sh -c "printf '%(%Y-%m-%d)T' 86400")" '1970-01-02'
t sh-select eq "$(sh -c 'select x in a b; do echo $x; break; done <<< 2' 2>/dev/null)" 'b'
t sh-dev-fd eq "$(sh -c 'cat <(echo pipe)')" 'pipe'
t sh-pipestatus eq "$(sh -c 'true | false | true; echo "${PIPESTATUS[@]}"')" '0 1 0'

# aibox の砂場 (landlock と seccomp)
t aibox-seccomp sh -c 'aibox -- grep -q "^Seccomp:.2" /proc/self/status'
t aibox-deny sh -c '! aibox --deny uname -- uname'
t aibox-pid eq "$(aibox -- sh -c 'echo $$')" 1
t aibox-tmp sh -c 'echo x > /tmp/vc-out && aibox --tmp -- sh -c "! test -e /tmp/vc-out"'
t aibox-root eq "$(aibox --root -- id -u)" 0
t aibox-quiet-W sh -c '[ -z "$(aibox -W /aibox-nonexistent -- true 2>&1)" ] && [ -n "$(aibox -w /aibox-nonexistent -- true 2>&1)" ]'
t aibox-no-net sh -c '[ "$(aibox --no-net -- readlink /proc/self/ns/net)" != "$(readlink /proc/self/ns/net)" ]'

# パイプ (大きなロックなしの read/write と、いっぱいのパイプに 4 つで書くとき止まらないこと。くわしくは pipe-check.sh)
t pipe-chain eq "$(seq 1 100000 | cat | cat | sha256sum | cut -c1-16)" "$(seq 1 100000 | sha256sum | cut -c1-16)"
t pipe-writers eq "$( (seq 1 30000 & seq 1 30000 & seq 1 30000 & seq 1 30000 & wait) | wc -l | tr -d ' ')" 120000

# sudo (パスワードなしで使えるイメージの ai)
t sudo sudo -n true

# ロック (spinlock.rs、smp.rs): lockbench が大きなロックを 10 万回放して取りなおすあいだも、ほかの CPU のパイプ
# (大きなロックなしで読み書きし、起こすのは持っている CPU に頼む) が止まらずに終わること
t lockbench sh -c 'seq 1 300000 | cat | wc -l > /tmp/lockbench.n & echo lockbench > /proc/ai/ctl; wait; [ "$(tr -d " " < /tmp/lockbench.n)" = 300000 ] && grep -q "lockbench: SpinLock" /proc/kmsg'

# 大きなロックなしの速い道 (syscall::fast の splice と read) が、ほかの CPU で大きなロックを持って動くもの (ls、stat) と
# ぶつからないこと。前は uutils の cat の splice (ファイル → パイプ) が ext4 の inode の覚えをロックなしで借りて、
# 4 つの CPU で "already borrowed" のパニックになった (カーネルが止まれば、このスクリプトはおしまいまで来ない)
t fast-smp sh -c 'for j in 1 2 3; do for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do (ls -l /usr/bin > /dev/null; cat /etc/os-release /usr/bin/ls > /dev/null; stat /etc/passwd > /dev/null) & done; wait; done'

# ディスクを眠って待つ読みの中にシグナルが来ても止まらないこと。前は眠りが EINTR ですぐ戻り、大きなロックを持ったまま
# 回りつづけ、要求の終わりを受ける割り込みとタイマがロックを待って、機械ごと止まった (いまは sleep_nointr、ps の D)。
# まだ読んでいない大きなファイル (/opt/c/lib の静的ライブラリ) を読むとちゅうで止める
t disk-read-signal sh -c 'for f in /opt/c/lib/*.a; do cat "$f" > /dev/null & p=$!; sleep 0.02; kill -TERM $p 2>/dev/null; wait $p; done; true'

# カーネルのヒープ (tlsf.rs) の決まりが、ここまでのたしかめのあとも守られているか (heap check の答えは kmsg に)
t heap-check sh -c 'echo "heap check" > /proc/ai/ctl && grep "heap: check" /proc/kmsg | tail -1 | grep -q "check ok"'
# /proc/ai/mem と /proc/ai/fs: 1 行の JSON で、ページキャッシュとヒープ、マウントとパスの覚えがある (aish の mem と fs)
t ai-mem sh -c 'm=$(sudo -n cat /proc/ai/mem) && echo "$m" | grep -q "\"page_cache\":{\"pages\":[0-9]" && echo "$m" | grep -q "\"heap\":{\"size\":[0-9]" && echo "$m" | grep -q "\"procs\":\[{\"pid\":"'
# /proc/ai/trace: 記録を空にして始め、ls を動かして止めると、その getdents64 が入っている (aish の trace)
t ai-trace sh -c 'echo "trace clear" > /proc/ai/ctl && echo "trace on ls" > /proc/ai/ctl && ls /tmp > /dev/null && echo "trace off" > /proc/ai/ctl && sudo -n cat /proc/ai/trace > /tmp/trace.txt && head -1 /tmp/trace.txt | grep -q "\"on\":false,\"filter\":\"ls\",\"events\":[1-9]" && grep -q "\"k\":\"sys\",\"n\":\"getdents64\"" /tmp/trace.txt && grep -q "\"k\":\"switch\"" /tmp/trace.txt'
# /proc/ai/parts と ctl の part: timerfd を動かしたまま切って入れる。装置 (net) は切れない
t ai-part sh -c 'echo "part timerfd off" > /proc/ai/ctl && grep -q "\"name\":\"timerfd\",\"off\":true,\"runtime\":true" /proc/ai/parts && echo "part timerfd on" > /proc/ai/ctl && grep -q "\"name\":\"timerfd\",\"off\":false" /proc/ai/parts && ! sh -c "echo part net off > /proc/ai/ctl" 2>/dev/null'
# /proc/ai/ctl の fail: ls の getdents64 を 1 回 EIO にすると ls がしくじる (hits 1)。やめれば通る
t ai-fail sh -c 'echo "fail name ls sys getdents64 5 1" > /proc/ai/ctl && ! ls /usr/bin > /dev/null 2>&1 && grep -q "\"hits\":1" /proc/ai/fail && echo "fail off" > /proc/ai/ctl && ls /usr/bin > /dev/null && grep -q "\"rule\":null" /proc/ai/fail'
# ヒープが取れないとき (fail ... heap: 耐えるところだけ): read の入れ物が取れず、dd は ENOMEM で止まる (cat や head は
# splice がしくじると read に切りかえて続く)。カーネルは続く
t ai-heap-enomem sh -c 'echo "fail name dd heap 1" > /proc/ai/ctl; dd if=/etc/hostname of=/dev/null bs=4096 count=1 > /dev/null 2> /tmp/enomem; s=$?; grep -q "\"hits\":1" /proc/ai/fail; h=$?; echo "fail off" > /proc/ai/ctl; [ "$s" != 0 ] && [ "$h" = 0 ] && grep -qi "error\|memory" /tmp/enomem'
# ヒープが足りなくなると足す (grow): 64 MiB のヒープに 70 MiB を取ると、まだ使っていないページから区画を足す (grown)。
# 足したあとも空きの管理は正しく (heap check ok)、返せば戻る
t ai-heap-grow sh -c 'echo "heap hog 70" > /proc/ai/ctl && sudo -n cat /proc/ai/mem | grep -q "\"grown\":[1-9]" && echo "heap check" > /proc/ai/ctl && grep "heap: check" /proc/kmsg | tail -1 | grep -q ok && echo "heap hog 0" > /proc/ai/ctl && echo "heap check" > /proc/ai/ctl && grep "heap: check" /proc/kmsg | tail -1 | grep -q ok'
# RAM をひととおり使って返したあとでも (page cycle: 空きのページをみな取ってから返す)、返ったページは buddy (kalloc.rs) が
# まとめなおしているので、ヒープを足せる (前はまだ使っていないページからしか足せず、hog は途中で止まった)。空きの決まりも正しい
t ai-page-buddy sh -c 'g=$(sudo -n cat /proc/ai/mem | grep -o "\"grown\":[0-9]*"); echo "page cycle" > /proc/ai/ctl && echo "page check" > /proc/ai/ctl && grep "page: check" /proc/kmsg | tail -1 | grep -q ok && echo "heap hog 100" > /proc/ai/ctl && grep "heap: hog" /proc/kmsg | tail -1 | grep -q "hog 100 MiB" && ! sudo -n cat /proc/ai/mem | grep -q "$g," && echo "heap hog 0" > /proc/ai/ctl && echo "heap check" > /proc/ai/ctl && grep "heap: check" /proc/kmsg | tail -1 | grep -q ok && echo "page check" > /proc/ai/ctl && grep "page: check" /proc/kmsg | tail -1 | grep -q ok && grep -q "^Node 0, zone   Normal" /proc/buddyinfo'
# どこでも取れないとき (fail ... oom: 耐えないところ): 予備から渡り、次のシステムコールで oom が、いちばんメモリを持つ
# プロセス (40 MiB の変数を持つこの子) を殺す。カーネルは続き、空きが戻れば back to normal
t ai-oom sh -c 'sh -c "x=\$(head -c 40000000 /dev/zero | tr \"\\0\" a); echo \"fail pid \$\$ oom 1\" > /proc/ai/ctl; ls /tmp > /dev/null; ls /tmp > /dev/null; echo survived" > /tmp/oom.out 2>&1; s=$?; echo "fail off" > /proc/ai/ctl; grep -q "oom: killed pid" /proc/kmsg && ! grep -q survived /tmp/oom.out && [ "$s" = 137 ]'
t ai-fs sh -c 'f=$(sudo -n cat /proc/ai/fs) && echo "$f" | grep -q "\"target\":\"/\",\"source\":\"/dev/vda[0-9]*\",\"type\":\"ext4\"" && echo "$f" | grep -q "\"cache\":{\"blocks\":[0-9]" && echo "$f" | grep -q "\"paths\":{\"entries\":[0-9]"'
# 大きなロック (BKL) が守るもの (kernel/src/cell.rs の Bkl) を、BKL を持たずに触ったところがない (ここまでのたしかめの
# あいだに。あれば /proc/bkl に unlocked FILE:LINE 回数 の行が出る)。いちばんあとに置く
t bkl-unlocked sh -c '! grep "^unlocked" /proc/bkl'
# その見はりが効いていること: BKL を放して Bkl を触ると、数えられる (CPU が 1 つなら skipped)
t bkl-selftest sh -c 'echo "bkl selftest" > /proc/ai/ctl && grep "bkl: selftest" /proc/kmsg | tail -1 | grep -q "ok\|skipped"'

echo "checks: $n, $fail failed"
[ "$fail" = 0 ]
