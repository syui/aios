# ふだんのしごとの速さを aios と Linux で同じようにはかる (doc/aios.md の「ふだんのしごと」の表)。zsh で:
#   zsh test/workload.sh BASE
# BASE には、静的な道具の bin/ (bsdtar、coreutils (uutils。cp、du、rm、ls など)、find (uutils findutils)、rg、git、
# make (GNU)、zsh)、zig 0.17 の tarball (zig.tar.gz)、C のファイル src/*.c (test/*.c をいくつか) を置く。aios では
# test/vm.py の VM のホーム、Linux では test/linux-vm.sh と同じ init で ext4 の /work (init は /now で時計を合わせる:
# 時計が 1970 年だと、zig cc がキャッシュを信じない)。1 行に 1 つ: 名前 秒 status user sys idle (/proc/stat の tick)
B=$1
export PATH=$B/bin HOME=$B LC_ALL=C
export GIT_AUTHOR_NAME=a GIT_AUTHOR_EMAIL=a@a GIT_COMMITTER_NAME=a GIT_COMMITTER_EMAIL=a@a
export ZIG_GLOBAL_CACHE_DIR=$B/zc ZIG_LOCAL_CACHE_DIR=$B/zl
zmodload zsh/datetime || { echo "no zsh/datetime"; exit 1; }
cd $B || exit 1
run() {
  local name=$1; shift
  local c u n s i c2 u2 n2 s2 i2 rest t0 t1 st
  read -r c u n s i rest < /proc/stat
  t0=$EPOCHREALTIME
  "$@" > /dev/null 2> $B/err.$name
  st=$?
  t1=$EPOCHREALTIME
  read -r c2 u2 n2 s2 i2 rest < /proc/stat
  printf "%-12s %8.2f s status %d user %d sys %d idle %d\n" $name $(( t1 - t0 )) $st $(( u2 - u )) $(( s2 - s )) $(( i2 - i ))
}
rm -rf t t2 g zc zl zc.tpl out h h.c Makefile
mkdir t
sync
run tar-x       bsdtar -xzf zig.tar.gz -C t
run sync        sync
run find        find t -name '*.h' -type f
run du          du -s t
run rg          rg -c memcpy t
run rg-j1       rg -j1 -c memcpy t
run cp-r        cp -r t t2
run rm-r        rm -rf t2
# git: musl と zig の std (4 千ほどのファイル)
mkdir g && cp -r t/lib/libc/musl t/lib/std g/
run git-init    git -C g init -q
run git-add     git -C g add -A
run git-commit  git -C g commit -q -m x
run git-status  git -C g status --porcelain
touch g/std/std.zig g/musl/src/string/memcpy.c
run git-status2 git -C g status --porcelain
run git-log     git -C g log --stat
# zig: musl の libc と compiler_rt を作ってリンクする (キャッシュは空から)
print 'int main(void){return 0;}' > h.c
run zig-libc    t/zig cc -target aarch64-linux-musl -O2 h.c -o h
# make の j1 と j4 は同じキャッシュ (libc と compiler_rt だけがある) から
cp -r zc zc.tpl
rm -rf zl
# make: 40 のプログラムをコンパイルしてリンク (libc はキャッシュにある)
srcs=(src/*.c)
srcs=(${srcs[1,40]})
{
  print "SHELL = $B/bin/zsh"
  print "ZIG = $B/t/zig"
  print -n "all:"
  for s in $srcs; do print -n " out/${${s:t}%.c}"; done
  print ""
  print 'out/%: src/%.c'
  print '\t@$(ZIG) cc -target aarch64-linux-musl -O2 -static -o $@ $< 2>/dev/null || true'
} > Makefile
mkdir out
run make-j1     make -j1
print "linked $(ls out | wc -l)"
rm -rf out zc zl; mkdir out; cp -r zc.tpl zc
run make-j4     make -j4
print "linked $(ls out | wc -l)"
run rm-tree     rm -rf t
