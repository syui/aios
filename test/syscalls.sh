# aios にまだないシステムコールを探す (aios の中で動かす。test/vm.py run "$(cat test/syscalls.sh)" か、
# aish-vm の put と run で)。よく使うプログラムをひととおり動かし、カーネルが出す
# 「syscall: unknown N by NAME」(番号ごとに最初の 1 回) を /proc/kmsg から集める。
# 先に入れておくもの: sudo ap -S git jq curl vim tmux tree file make openssh nodejs less patch xz zstd zsh
# 1 行に 1 つ: unknown N NAME。その前に、しくじったコマンドを fail STATUS CMD (エラーの終わりつき)。
# おしまいに unknown: N numbers, M failed
w=$(mktemp -d)
cd "$w" || exit 1
failed=0
q() {
  "$@" >/dev/null 2>"$w/.err" </dev/null && return
  echo "fail $? $*: $(tail -c 200 "$w/.err" | tr '\n' ' ')"
  failed=$((failed + 1))
}
# ファイルとテキスト
q ls -la /usr/bin
q cp -a /usr/share "$w/etc"
q du -sh "$w/etc"
q df -h
q stat /etc/passwd
q find /usr/share -name '*.txt'
q sort /etc/passwd
q tar -cf "$w/a.tar" "$w/etc"
q zstd -q "$w/a.tar"
q xz -k "$w/a.tar"
q tree -L 2 /usr
q file /bin/aish
q rg -n root /etc/passwd /etc/group
q fd conf /etc
q jq -n '[1,2,3] | add'
q diff /etc/passwd /etc/passwd
q diff -ru /usr/share "$w/etc"
q patch --version
q less -F /etc/hostname
q vim -es -c 'qa!' /etc/hostname
q ps aux
q top -b -n 1
q date
# git
q git init -q "$w/r"
(cd "$w/r" && q git -c user.name=a -c user.email=a@a commit -q --allow-empty -m x && q git log && q git status && q git gc -q)
q git clone -q --depth 1 https://git.syui.ai/ai/os "$w/os"
# ネットワーク
q curl -fsSL -o /dev/null https://git.syui.ai/
q ssh-keygen -q -t ed25519 -N '' -f "$w/k"
# シェルとジョブ
q zsh -c 'echo $ZSH_VERSION'
q tmux new-session -d -s t 'sleep 2'
q tmux kill-server
q make --version
q node -e 'require("fs").readdirSync("/etc"); const {Worker} = require("worker_threads"); new Worker("0", {eval: true})'
q node -e 'const {execSync}=require("child_process"); execSync("true"); require("os").cpus(); require("http").createServer().listen(0, function(){ this.close() })'
cd / && rm -rf "$w"
grep -a 'syscall: unknown' /proc/kmsg | sed 's/.*syscall: unknown \([0-9]*\) by \([^ ]*\).*/unknown \1 \2/' | sort -n -k2 -u
echo "unknown: $(grep -a 'syscall: unknown' /proc/kmsg | sed 's/.*unknown \([0-9]*\).*/\1/' | sort -u | wc -l) numbers, $failed failed"
