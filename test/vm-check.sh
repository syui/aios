# aios の中で動かす短いたしかめ (CI の起動のテストのあと、test/vm.py run "$(cat test/vm-check.sh)" で)。
# 外 (このマシンの aish のテスト) ではわからない、カーネルとイメージの組み合わせだけを見る:
#   /sys と /proc (psutil などが読むところ)、イメージに入っているコマンド (tar は bsdtar、awk は gawk)、
#   sh の名前の aish (printf と select、/dev/fd)、aibox (seccomp と namespace)、sudo
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
disk=$(ls /sys/block | head -1)
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
t aibox-no-net sh -c '[ "$(aibox --no-net -- readlink /proc/self/ns/net)" != "$(readlink /proc/self/ns/net)" ]'

# sudo (パスワードなしで使えるイメージの ai)
t sudo sudo -n true

echo "checks: $n, $fail failed"
[ "$fail" = 0 ]
