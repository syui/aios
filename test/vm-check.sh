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

# カーネルのヒープ (tlsf.rs) の決まりが、ここまでのたしかめのあとも守られているか (heap check の答えは kmsg に)
t heap-check sh -c 'echo "heap check" > /proc/ai/ctl && grep "heap: check" /proc/kmsg | tail -1 | grep -q "check ok"'
# /proc/ai/mem と /proc/ai/fs: 1 行の JSON で、ページキャッシュとヒープ、マウントとパスの覚えがある (aish の mem と fs)
t ai-mem sh -c 'm=$(sudo -n cat /proc/ai/mem) && echo "$m" | grep -q "\"page_cache\":{\"pages\":[0-9]" && echo "$m" | grep -q "\"heap\":{\"size\":[0-9]" && echo "$m" | grep -q "\"procs\":\[{\"pid\":"'
# /proc/ai/trace: 記録を空にして始め、ls を動かして止めると、その getdents64 が入っている (aish の trace)
t ai-trace sh -c 'echo "trace clear" > /proc/ai/ctl && echo "trace on ls" > /proc/ai/ctl && ls /tmp > /dev/null && echo "trace off" > /proc/ai/ctl && sudo -n cat /proc/ai/trace > /tmp/trace.txt && head -1 /tmp/trace.txt | grep -q "\"on\":false,\"filter\":\"ls\",\"events\":[1-9]" && grep -q "\"k\":\"sys\",\"n\":\"getdents64\"" /tmp/trace.txt && grep -q "\"k\":\"switch\"" /tmp/trace.txt'
t ai-fs sh -c 'f=$(sudo -n cat /proc/ai/fs) && echo "$f" | grep -q "\"target\":\"/\",\"source\":\"/dev/vda[0-9]*\",\"type\":\"ext4\"" && echo "$f" | grep -q "\"cache\":{\"blocks\":[0-9]" && echo "$f" | grep -q "\"paths\":{\"entries\":[0-9]"'

echo "checks: $n, $fail failed"
[ "$fail" = 0 ]
