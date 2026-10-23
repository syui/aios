#!/bin/sh
# 書き出しのとちゅうで電源を抜いても ext4 がこわれないか (このマシンで。e2fsck と QEMU がいる)。
#   test/powercut.sh [KERNEL] [N]   KERNEL (既定は disk.img のもの) で N 回 (既定 6):
#   disk.img の写しで起こし、test/bin/flushrace (sh test/vm-ctests.sh build で作る) を回して、起こしてから 7 秒、8 秒 ...
#   たったところで QEMU を SIGKILL する (test/vm.py stop は中で poweroff するので、電源を抜くことにならない)。
#   書き終わっていない要求は書かれない。ディスクはおそくする (AIOS_DISK_OPTS、既定 throttling.iops-write=200) ので、
#   書き出しのとちゅうで切れる。そのあと外からルートの区画を e2fsck -fy して、直したものが orphan の片づけと
#   extent を短くできるという知らせのほかにあれば、こわれていた (ジャーナルの再生はこわれではない)。
#   1 回に 1 行: ok N か FAIL N (と e2fsck の言ったこと)。おしまいに powercut: F failed (0 でなければ失敗のステータス)
cd "$(dirname "$0")/.." || exit 1
k=${1:-}
n=${2:-6}
export AIOS_DISK_OPTS=${AIOS_DISK_OPTS:-throttling.iops-write=200}
d=build/powercut
# ルートの区画 (bin/mkdisk.sh の name="root") のはじめと長さ (MiB)
set -- $(sfdisk -d disk.img | awk -F'[=,]' '/name="root"/ { print int($2 / 2048), int($4 / 2048) }')
[ $# -eq 2 ] || { echo "powercut: no root partition in disk.img" >&2; exit 1; }
skip=$1 count=$2
fail=0
i=1
while [ $i -le $n ]; do
  rm -rf $d; mkdir -p $d
  cp --sparse=always disk.img $d/disk.img
  if ! AIOS_DISK=$d/disk.img AIOS_KERNEL=$k timeout 120 python3 test/vm.py start --www test > $d/start.log 2>&1; then
    echo "FAIL $i (start)"; fail=$((fail + 1)); python3 test/vm.py stop > /dev/null 2>&1; i=$((i + 1)); continue
  fi
  python3 test/vm.py run 'fetch http://10.0.2.2:8000/bin/flushrace > /tmp/flushrace && chmod +x /tmp/flushrace' -t 30 > /dev/null 2>&1
  python3 test/vm.py bg '/tmp/flushrace -t 60 ~/fr > /tmp/fr.out 2>&1' > /dev/null 2>&1
  sleep $((i + 6))
  # 電源を抜く
  q=$(pgrep -f "file=$d/disk.img")
  [ -n "$q" ] && kill -9 $q
  sleep 1
  python3 test/vm.py stop > /dev/null 2>&1
  dd if=$d/disk.img of=$d/root.img bs=1M skip=$skip count=$count conv=sparse status=none
  rm -f $d/disk.img
  rec=$(dumpe2fs -h $d/root.img 2>/dev/null | grep -c needs_recovery)
  e2fsck -fy $d/root.img > $d/fsck.log 2>&1
  bad=$(grep -v "could be narrower\|Optimize\|^$\|^Pass [12345]:\|^e2fsck 1\|Pass 1E\|FILE SYSTEM WAS MODIFIED\|non-contiguous\|recovering journal\|Clearing orphaned inode\|Truncating orphaned inode\|^Clear? yes" $d/fsck.log | head -8)
  if [ -n "$bad" ]; then
    echo "FAIL $i (delay $((i + 6)) s)"; echo "$bad" | sed 's/^/  /'; fail=$((fail + 1))
    cp $d/fsck.log build/powercut-$i.fsck.log
  else
    echo "ok $i (delay $((i + 6)) s$([ "$rec" -gt 0 ] && echo ', journal replayed'))"
  fi
  rm -f $d/root.img
  i=$((i + 1))
done
echo "powercut: $fail failed"
[ $fail -eq 0 ]
