#!/bin/sh
# カーネルの C のテスト (test/*.c) を aios の中で動かす。CI の VM test で、vm-check.sh のあとに:
#   作る (外で): test/vm-ctests.sh build [CC] [NAME...]   test/bin/NAME に静的なプログラム (既定は gcc。aarch64 の Linux で)
#   動かす (中で): fetch http://10.0.2.2:8000/vm-ctests.sh > /tmp/t.sh && sh /tmp/t.sh run [NAME...]
#                 (test/ を http で見せている VM: test/vm.py start --www test。プログラムは bin/ から取る)
#   NAME をあげれば、それだけを (TESTS にないテストも。aish-vm の ctests が使う)
# どれもしくじった数を終了コードで返す。1 行に ok NAME か FAIL NAME。おしまいに ctests: N failed
# hang.c と watch.c はテストではなく、わざと止まる道具なので入れない
TESTS="apparmor appendrace atime audit auditdir auditexec auditpath auditrules bigunmap chattr chosen chrootlink clone3 closerange closeshared cpuclock cpuid cputime cpuvuln createrace ctev ctnl cwdperm cwdrace dad6 deleg dentsfast dev dirents dnotify drmatomic drmkms drmrender drmsyncobj dumpable efivarfs epoll exec-thread fastmap fasync fasync2 fchroot fhandle flushrace forkcow frag6 fsrace fsuid fsx futex futex2 hrsleep futexrace inet6 inet6net inetsock inotify ipcns landlock-eexist landlockabi lease listns loginuid madvfree madvise mincore mld6 mldrep6 mlock mmapcoh mnt mountinfo mremap mseal ndsilence netlink newmount nftables nfteval nfteval6 nftsk nice ns onlink6 opentreeattr openat2 opencache openfast openrace orphan pagecache pagetables pathcache peerpidfd perf perf2 perf3 perf4 perf5 perf6 perf7 perf8 perf9 pidfdinfo pidnswait pipeslots pivotns pivotroot pmtu6 pmu portparity posrace prctl procfiles procio procknobs procmadv procmaps procmem procpid procstatus procwait ptrace ptrace2 ptrace3 ptrace4 pwfast random rcrace remapfp rdnss6 recverr6 rlimit rlock robust route rtnl6 rtnladdr rtnllink seccomp seccompnotify shm settime setns signalfd sigstatus sigtramp sleeps splice statc statmount statcache syncrange sysctl6 syslog sysmisc sysvipc temp6 threadmap ttou umountbusy uname unixrace unixsock userns vdso vgetrandom vmsplice xattr"

case $1 in
build)
  cc=${2:-gcc}
  [ $# -gt 2 ] && shift 2 && TESTS="$*"
  cd "$(dirname "$0")" && mkdir -p bin || exit 2
  for t in $TESTS; do
    # .c より新しいものは作りなおさない (CI は作りたてのリポジトリなので、みな作る)
    [ "bin/$t" -nt "$t.c" ] && continue
    $cc -O2 -static -pthread -o "bin/$t" "$t.c" || { echo "vm-ctests: cannot build $t" >&2; exit 2; }
  done
  ;;
run)
  [ $# -gt 1 ] && shift && TESTS="$*"
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
    # pwfast (大きなロックなしの pwrite)、opencache (閉じたファイルの棚)、createrace (大きなロックなしの作る・消す)、
    # orphan (消したファイルが片づくか)、posrace (位置を使う read をいくつものスレッドとプロセスで) も
    chosen | mmapcoh | rlock | pwfast | opencache | createrace | orphan | posrace) cmd="./$t $HOME /tmp" ;;
    # rcrace (大きなロックなしの munmap と open / close が同じ inode の Rc の数をさわらない) は 10 秒、tmpfs で
    rcrace) cmd="./$t 10 /tmp" ;;
    # appendrace (大きなロックなしの書きたし: 同じファイルと自分のファイルに書きたし、読む子がこわれを探す) は 5 秒ずつ、
    # ext4 (ホーム) と tmpfs (/tmp) で
    appendrace) cmd="./$t 5 $HOME /tmp" ;;
    # mountinfo (マウントの番号と st_dev のそろい方) も ext4 (ホーム) と tmpfs (/tmp) で
    mountinfo) cmd="./$t $HOME /tmp" ;;
    # statc (大きなロックなしの stat) は ext4 (ホーム) と tmpfs (/tmp) で。root でも (見る人をほかの人にして)
    statc) cmd="./$t $HOME /tmp" ;;
    # flushrace (fsync と sync が眠って書くあいだの読み書き) は ext4 (ホーム) で 8 秒
    flushrace) cmd="./$t -t 8 $HOME/.flushrace" ;;
    # openfast と openrace (棚にないファイルの大きなロックなしの open) は ext4 (ホーム) で。openrace は 5 秒
    openfast) cmd="./$t $HOME" ;;
    # procio (/proc/PID/io の write_bytes と cancelled_write_bytes) は ext4 (ホーム) で
    procio) cmd="./$t $HOME" ;;
    openrace) cmd="./$t $HOME 5" ;;
    # cwdrace (fd の表をスレッドで分けたときの大きなロックなしの open と stat、chdir と dup2 といっしょに) も ext4 で 3 秒
    cwdrace) cmd="./$t $HOME 3" ;;
    # fhandle (name_to_handle_at と open_by_handle_at) も ext4 (ホーム) と tmpfs (/tmp) で
    fhandle) cmd="./$t $HOME /tmp" ;;
    # remapfp (remap_file_pages) も ext4 (ホーム) と tmpfs (/tmp) で
    remapfp) cmd="./$t $HOME /tmp" ;;
    # chattr (ext4 の印と file_getattr) も ext4 (ホーム) と tmpfs (/tmp) で、root で (CAP_LINUX_IMMUTABLE)
    chattr) cmd="sudo -n ./$t $HOME /tmp" ;;
    # atime も ext4 (ホーム) と tmpfs (/tmp) で、root で (utimensat で決め、noatime などの tmpfs をマウントする)
    atime) cmd="sudo -n ./$t $HOME /tmp" ;;
    # cwdperm (cwd からの相対パスは cwd から先だけの許可で) も ext4 (ホーム) と tmpfs (/tmp) で、root で (nobody になる)
    cwdperm) cmd="sudo -n ./$t $HOME /tmp" ;;
    # ptrace も root で (nobody で init に ATTACH して EPERM を見る)
    ptrace | ptrace2) cmd="sudo -n ./$t" ;;
    # xattr (拡張属性) は ext4 (ホーム) で (手本の Linux の tmpfs には xattr がない)
    xattr) cmd="./$t $HOME" ;;
    # efivarfs は root で (UEFI の変数を作る・消す・chattr。UEFI から起動していなければ何もしない)
    efivarfs) cmd="sudo -n ./$t" ;;
    # nftables も root で (nft のパッケージがなければ何もしない。出すだけのテスト: 手本の Linux とくらべるのは外で)
    nftables) cmd="sudo -n ./$t" ;;
    # nfteval も root で (hostfwd のポートがないので、外を一回りするものとホストのポートは 0: しない)
    nfteval) cmd="sudo -n ./$t /opt/c/bin/nft 0 0 0" ;;
    nfteval6) cmd="sudo -n ./$t" ;;
    # inet6net も root で (raw の ICMPv6。eth0 に SLAAC のアドレスができなければしくじり)
    inet6net) cmd="sudo -n ./$t" ;;
    # rtnl6 も root で (ip -6 addr / route を足して消す)
    rtnl6) cmd="sudo -n ./$t" ;;
    # dad6 も root で (ip -6 addr を足して DAD を見て消す)
    dad6) cmd="sudo -n ./$t" ;;
    # onlink6 も root で (ip -6 route add ... dev eth0 を足して消す)
    onlink6) cmd="sudo -n ./$t" ;;
    # sysctl6 も root で (/proc/sys/net/ipv6/conf を書いて、もとにもどす)
    sysctl6) cmd="sudo -n ./$t" ;;
    # rdnss6 も root で (raw の ICMPv6 で RS を送る)
    rdnss6) cmd="sudo -n ./$t" ;;
    # pivotroot と pivotns も root で (新しいマウントの namespace で tmpfs を重ねて pivot_root)。newmount (新しい mount の API) と
    # umountbusy (使っているマウントの umount)、opentreeattr、statmount、listns、chroot の chrootlink と fchroot も
    pivotroot | pivotns | newmount | umountbusy | opentreeattr | statmount | listns | chrootlink | fchroot) cmd="sudo -n ./$t" ;;
    # pmtu6 も root で (raw の ICMPv6 で packet too big を作る)
    pmtu6) cmd="sudo -n ./$t" ;;
    # mldrep6 も root で (nftables の counter で MLD の報告を数える。nft がなければ skipped)
    mldrep6) cmd="sudo -n ./$t" ;;
    # frag6 も root で (raw の ICMPv6 で packet too big を作る)
    frag6) cmd="sudo -n ./$t" ;;
    # nftsk も root で (nft の決まりと、uid を変えた子)
    nftsk) cmd="sudo -n ./$t" ;;
    # ctnl も root で (conntrack-tools と nftables のパッケージがいる。なければ skipped)
    ctnl) cmd="sudo -n ./$t" ;;
    # ctev も root で (conntrack -E。conntrack-tools と nftables のパッケージがいる)
    ctev) cmd="sudo -n ./$t" ;;
    # audit も root で (AUDIT_SET で auditd になる。uid を変えた子)
    audit | auditdir | auditexec | auditpath | auditrules | dumpable | fsuid | loginuid) cmd="sudo -n ./$t" ;;
    # temp6 も root で (use_tempaddr を書き、raw の ICMPv6 で RS を送る)
    temp6) cmd="sudo -n ./$t" ;;
    # rtnladdr も root で (ip addr を足して消す)
    rtnladdr) cmd="sudo -n ./$t" ;;
    # ndsilence も root で (raw の ICMP。近所さがしの失敗を 6 秒見る)
    ndsilence) cmd="sudo -n ./$t" ;;
    # deleg は ext4 (ホーム) で (ロックなしの作る・消すの道もとめる)
    deleg) cmd="./$t $HOME" ;;
    # perf7 も root で (CPU ごとのイベント: paranoid が 0 より上なら CAP_PERFMON)。perf9 も (perf_event_max_sample_rate を書く)
    perf7 | perf9) cmd="sudo -n ./$t" ;;
    *) cmd="./$t" ;;
    esac
    # pathcache は root でも (マウントと、ほかの人から引いたとき。ext4 はほかの人も通れる /var/tmp で)
    root=
    [ "$t" = pathcache ] && [ "$(id -u)" != 0 ] && root="sudo -n ./$t /tmp /var/tmp"
    [ "$t" = statc ] && [ "$(id -u)" != 0 ] && root="sudo -n ./$t /var/tmp /tmp"
    # 時間切れ (300 秒) のまぎわに、まだ動いているそのテストのスレッドの様子 (/proc/ai/threads: 状態、システムコール、
    # 待っているもの) を残す。まれにしか止まらないものも、止まったときのわけが見える
    rm -f "$t.hang" "$t.dog"
    ( sleep 290 & echo $! > "$t.dog"; wait $! && grep "\"name\":\"$t\"" /proc/ai/threads > "$t.hang" 2>/dev/null ) &
    if timeout 300 $cmd > "$t.out" 2>&1 && { [ -z "$root" ] || timeout 300 $root >> "$t.out" 2>&1; }; then
      echo "ok $t"
    else
      echo "FAIL $t (status $?):"
      tail -15 "$t.out" | sed 's/^/  /'
      [ -s "$t.hang" ] && head -40 "$t.hang" | sed 's/^/  hang: /'
      fail=$((fail + 1))
    fi
    kill "$(cat "$t.dog" 2>/dev/null)" 2>/dev/null
  done
  echo "ctests: $fail failed"
  [ "$fail" = 0 ]
  ;;
*)
  echo "usage: test/vm-ctests.sh build [CC] | run" >&2
  exit 2
  ;;
esac
