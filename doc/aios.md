# aios を把握・設定・操作・改造する仕組み (aiosd と aios コマンド)

人も Claude も、aios ぜんたいを **1 つの形** で扱う。読むところ (/proc や /etc)、変える道具 (systemctl、ap、useradd …)、
ソースとビルドがばらばらにならないように、ひとつの木 (状態) と、ひとつの設定ファイル (望む状態) にまとめる。

```
 人 ── aios コマンド ─┐
                     ├── 状態の木 (読む) ─── /proc /sys /etc /var/lib/aipkg …
 Claude ── aish-sys ─┤
   (MCP のツール)     └── aiosd (root) ── 変える: サービス、パッケージ、ユーザー、設定の apply、改造
                              /run/aiosd.sock (1 行 1 つの JSON)
```

## 段階

1. **把握 (get)** ✅: `aios get [PATH]` で状態の木を読む。読むだけなので aiosd なしで、呼んだプロセスの中で集める
2. **操作 (do)** ✅: aiosd (root で動くサービス) が `/run/aiosd.sock` で受ける。だれが呼んだかは SO_PEERCRED で見る
   (読むのはだれでも、変えるのは root と wheel)。したことは `/var/log/aiosd.log` に残す
3. **設定 (apply)** ✅: `/etc/aios.json` (望む状態) と状態の木をくらべて (`aios diff`)、ちがうところだけ直す (`aios apply`)。
   apply のたびに前の設定とパッケージの版を `/var/lib/aios/history/` に残し、`aios rollback` で戻せる
4. **改造 (src / build)** ✅: aios のソースを `/usr/src/aios` に置き、カーネルやパッケージをビルドして入れる。
   カーネルは前のものを ESP に残し、起動しなかったら前のもので起動しなおせるようにする

## 状態の木 (aios get)

| PATH | 中身 |
|---|---|
| `host` | 名前、OS (/etc/os-release の NAME と VARIANT と base の版: `aios (aikernel) 0.0.1.r…`。VARIANT の aikernel は、カーネルから作った aios を Arch Linux から作った aios と見分ける) |
| `kernel` | 版、コマンドライン、起きてからの秒、CPU の数、モジュール、BKL (`/proc/bkl`)、メッセージの終わり (`/proc/kmsg`)、`sysctl` (`/proc/sys`) |
| `mem` | メモリとスワップ (KiB) |
| `disk` | マウントごとの使った量と大きさ |
| `proc` | プロセス (pid ppid 状態 スレッド CPU 秒 メモリ コマンド) |
| `service` | ユニット (動いているか、enable か、説明) |
| `pkg` | 入っているパッケージと版、リポジトリ |
| `net` | インターフェイスとアドレス、経路、DNS |
| `user` | ユーザー (uid、ホーム、シェル) |
| `boot` | ESP のカーネルとローダーのエントリ、`try` (新しいカーネルを試す残りの回数)、`fallback` (前のカーネルに戻して起動したか)、`prev` (`/boot/Image.prev` があるか)、`last` (最後の起動の結果) |
| `log` | `aiosd` (`/var/log/aiosd.log` の新しい 20 件)、`apply` (`aios apply` / `rollback` の記録の一覧: 番号、時刻、うまくいったか) |

- `PATH` は点でつなぐ: `kernel.cpus`、`pkg.installed.cargo`、`service.sshd.active`。
  配列は番号か名前 (name / mount / pid) で選ぶ
- ふだんは `PATH = 値` の行 (sysctl と同じで、grep しやすい)。`--json` で JSON

`aios top [N]` は `proc` を CPU の時間 (起動からの秒) の多い順に N 個 (10 個) だけ出す。重いものを探すとき。

## 操作 (aios do)

```sh
aios do ping                          # aiosd が動いているか、自分が変えられるか
aios do service restart sshd          # start stop restart enable disable
aios do pkg install git vim           # install remove / pkg upgrade / pkg refresh
aios do reboot                        # poweroff も
```
Claude は aish-sys の `do` ツールで同じことを頼む。aiosd は SO_PEERCRED で相手を見る (カーネルは connect / listen /
socketpair のときのプロセスを覚えておく)。

## 設定ファイル (/etc/aios.json)

望む状態だけを書く。書いていないものは aios が触らない。

```sh
aios config | sudo tee /etc/aios.json   # はじめは、いまの状態から作る
sudoedit /etc/aios.json                 # 望む状態を書く
aios diff                               # いまとのちがいと、そろえる手順 (動かさない)
aios apply                              # そろえる (aiosd に頼む)。記録は /var/lib/aios/history/N.json
aios history [N]                        # apply の記録
aios rollback                           # ひとつ前の apply の設定に戻す
```

- `host.name`: /etc/hostname とカーネル (sethostname)
- `pkg`: 入っていてほしいもの (書いていないものは外さない。rollback は、最後の apply で入れたものを外す)
- `service`: `"enabled"` (enable して動かす) か `"disabled"` (止めて disable)
- `kernel.modules`: /etc/modules-load.d/aios.conf に書き、入っていないものは modprobe
- `user`: いなければ useradd -m。`shell` と `groups` (足すだけ) をそろえる
- `net`: インターフェースごとに `{"dhcp": true}` か `{"address": "A.B.C.D/N", "gateway": ..., "dns": [...]}`
  (dhcp でも `dns` は書ける)。`/etc/systemd/network/00-aios-IFACE.network` に書いて `networkd` で決める。
  aios が書いたもので、もう書いていないインターフェースのものは消す
- `sysctl`: カーネルの値 (`/proc/sys`)。`/etc/sysctl.d/aios.conf` に書き (起動のときに init が入れる)、
  いまの値がちがうものは今 `/proc/sys` に書く

```json
{
  "host":    { "name": "aios" },
  "pkg":     ["openssh", "cargo", "base-devel", "git", "aiwm", "mesa"],
  "service": { "sshd": "enabled", "aiwm": "disabled" },
  "kernel":  { "modules": ["virtio_gpu", "virtio_input", "virtio_snd"] },
  "user":    { "ai": { "shell": "/bin/aish", "groups": ["wheel"] } },
  "net":     { "eth0": { "address": "10.0.2.15/24", "gateway": "10.0.2.2", "dns": ["1.1.1.1"] } },
  "sysctl":  { "vm.min_free_kbytes": 8192, "fs.inotify.max_user_watches": 65536 }
}
```

## カーネルの値 (/proc/sys と sysctl)

Linux と同じ場所と形。書けるのは root。値の表は kernel/src/sysctl.rs (1 行足せば 1 つ増える)。

| 名前 | |
|---|---|
| `kernel.hostname` | uname の nodename (sethostname と同じ) |
| `kernel.ostype` `kernel.osrelease` | 読むだけ |
| `kernel.sched_timeslice_ms` | ほかに順番をゆずるまで走る長さ (既定 10 = 1 tick。10 ms の倍数に切り上げ、1 秒まで)。長くすると切りかえが減り、短いほど返事が速い |
| `kernel.sched_wakeup_preempt` | 1 (既定) なら、起こされたもの (音、入力、aiwm など、ふだん眠っているもの) が、CPU を使い続けているものに割りこむ。眠っている CPU がなければ、走っているもののうち一番使っているものの CPU に IPI を送ってゆずらせる。0 ならタイムスライスの終わりまで待つ |
| `kernel.random.entropy_avail` `kernel.random.poolsize` | 読むだけ。乱数のもとのビット (256 なら ready) と 256。乱数 (getrandom、/dev/urandom) は Linux と同じ作りで、もと (DTB の rng-seed、EFI_RNG_PROTOCOL、CPU の RNDR、virtio-rng) を BLAKE2s の池にまぜ、ChaCha20 で鍵を消しながら出す (kernel/src/rand.rs)。起動のときの `rand: ready (ChaCha20) from ...` に、どこからもらったかが出る |
| `kernel.random.boot_id` `kernel.random.uuid` | 読むだけ。この起動の UUID と、読むたびに新しい UUID |
| `fs.inotify.max_queued_events` | inotify にためておくできごとの数 (こえたら IN_Q_OVERFLOW) |
| `fs.inotify.max_user_watches` | 1 つの inotify の watch の数 |
| `fs.nr_open` | 読むだけ (開けるファイルの数の上限) |
| `vm.min_free_kbytes` | 空きがこれを割ったら swap へ回収する (その 4 倍になるまで) |

```sh
sysctl -a                                  # ぜんぶ
sysctl vm.min_free_kbytes                  # 読む
sudo sysctl vm.min_free_kbytes=8192        # 書く
sudo sysctl --system                       # /etc/sysctl.conf と /etc/sysctl.d/*.conf を入れる (起動のときに init がする)
```

### 値をくらべる (aios tune)

値を順に変えて、同じ仕事の時間をくらべる。終わったら元の値に戻す。`--apply` でいちばん速かった値を
`/etc/aios.json` の `sysctl` に書いて `aios apply` する (root)。Claude は aish-sys の `tune` ツールで同じことをする。

```sh
sudo aios tune kernel.sched_timeslice_ms=10,20,50,100 -n 3 -- 'make -j8'
sudo aios tune kernel.sched_timeslice_ms=10,50 --apply -- 'cargo build --release'
```

## 砂場 (aibox)

`aibox [-w PATH]... [-W PATH]... [-n PORT]... [--no-net] [-v] [--] CMD` は CMD を砂場の中で動かす。カーネルの landlock (Linux と同じシステムコール) を使う。

- 読む・動かすのはどこでも。書く (作る・消す・名前を変える) のは、いまのディレクトリ、`/tmp`、`/dev` と `-w` の下だけ
- `-W PATH` は `-w` と同じだが、なければだまって飛ばす (`~/.cargo` のように入れたら使うもの。`-w` はなければ知らせる)
- `--no-net` で TCP はどこへもつなげない。`-n PORT` でその口だけ
- 子にも引き継がれ、外せない。中で aibox を重ねると、もっと狭くなるだけ。`sudo` (setuid) は効かない
- かかっているかは `/proc/self/status` の `NoNewPrivs` と `Landlock` (層の数)

Claude の使う aish (`/etc/claude-code/managed-mcp.json`) は `aibox -W ~/.cache -W ~/.cargo -w /proc/ai/ctl -- aish --mcp` で起きる (`/proc/ai/ctl` は下の調べもののスイッチ)。Claude が動かすものはみな、Claude を起こしたディレクトリの中にしか書けない。root のする操作は `aios do` (aiosd が wheel の人かを見る) を通す。

## カーネルの地図 (kernel/kernel.json)

カーネルのファイルと手当てを、AI が先に読んで見通せるように 3 つに分けた一覧 (pkg/pkg.json と同じく 1 行に 1 つ)。
動いている aios では `/proc/ai/kernel` (だれでも読める)。

| type | 中身 | 1 行 |
|---|---|---|
| `core` | 起動して /init が動き、シリアルに書けるのに要るもの。いつも小さくしていく | 1 つのファイル: name、src、what (測れるものは bench) |
| `extra` | Linux の口 (syscall、/proc、ioctl、ファイルシステム)、aios の機能、ドライバ。なくてもカーネルは動く | 1 つのファイル: name、src、what (切れるものは off、測れるものは bench) |
| `quirk` | ある機械やプログラムのつごうで入れた手当て (Linux の quirk と同じ考え)。外せる条件 (`until`) がある | 1 つの手当て: name、src (いくつものファイル)、cause、what、until、test、off |

quirk のところには、ソースに札 `[quirk NAME]` をつける。`python3 test/kernel-json.py` (CI でも) が、kernel/src の .rs が
みなちょうど 1 度ずつ core か extra にあるか、quirk の札と一覧がそろっているかを見て、数 (core と extra の行) と、core が
extra を使っているところを出す (core を小さくするときの目じるし)。リポジトリの `.aios.json` の `kernel` は、この一覧の名前を
type ごとに並べたもの (`core`、`extra`、`quirk`。同じスクリプトがそろっているかを見る)。

### カーネルの砂場 (aios.off= と aish の box)

`off` のあるもの (extra と quirk) は、起動の引数 `aios.off=NAME,NAME` で切って、それがないカーネルのように起こせる
(kernel/src/parts.rs。システムコールはそれを入れずに作った Linux と同じ ENOSYS、装置は起こさない、quirk は手当てをしない。
切るとどうなるかはそれぞれの `off`)。起動のときに `parts: off NAME ...` と出る。切ってもほかが動くか、quirk がまだ要るかを
確かめるのは aish の `box` (aish-vm): 切って VM (`-snapshot`) を起こし、CI と同じテスト (vm-check と vm-ctests) を動かして、
何も切らないときにはない FAIL を出す (`each` で 1 つずつ全部: どのテストがどれを要るか)。手では `vm` の start に `off`。
起こしなおさなくても、使うたびに見るもの (システムコールのもの: epoll、inotify、landlock、module、ns、seccomp、swap、timerfd、
vdso と、quirk) は `/proc/ai/ctl` に `part NAME off` / `part NAME on` で動かしたまま切ったり入れたりできる (aish の `part`。
いまの様子は `/proc/ai/parts`)。起動のときに起こす装置 (net、virtio_rng、bcm2835_rng) と aiproc 自身はできない。

### ヒープが足りないとき (足す、捨てる、ENOMEM、予備と oom)

カーネルのヒープ (kernel/src/heap.rs。空きの管理は TLSF) は、起動のときに bss の 32 MiB と RAM の 1/32 (2 GiB で 64 MiB)。
前はそれを使いきると、Rust の `alloc` の既定どおりそこで panic していた (`fail ... heap` が見せたもの)。いまは取れないと順に:
まだだれも使っていない続いたページを区画として**足す** (8 MiB ずつ、RAM の 1/4 まで)、だれも写していないページキャッシュを
**捨てる**、それでもなければ、**耐えるところ** (`heap::fallible` の区間: read / write / splice の入れ物、exec の引数) には
null を返して `Vec::try_reserve` が Err になり、ユーザーには ENOMEM。**耐えないところ** (`Vec::push` など) には **予備**
(2 MiB) から渡して、次のシステムコールの入り口で kernel/src/oom.rs が、いちばんメモリ (rss) を持っているプロセス (init のほか)
を SIGKILL で終わらせて空ける (Linux の OOM killer と同じ考え。alloc の中では、カーネルの表を変えている途中かもしれないので
殺せない)。ふつうの道に空きが戻れば `heap: back to normal`。予備もなければ、そこで panic する (使っている量と割れぐあいを出して)。
tmpfs の inode は上限 (ヒープの大きさ / 1 KiB、64 MiB で 65536) で ENOSPC (作りつづけてヒープを使いきれないように。Linux の
`nr_inodes`)。様子は `/proc/ai/mem` の `heap` (`grown` 足した分、`reclaims` 遅い道の回数、`reserve_used`、`emergencies`、
`oom_kills`) と `tmpfs` の `inodes`。`fail ... heap` は耐えるところの、`fail ... oom` は耐えないところの道を、わざと起こす
(vm-check の ai-heap-enomem と ai-oom)。`ctl` の `heap hog N` は N MiB をわざと取って持っておく (足す道を試す: ai-heap-grow。
`heap hog 0` で返す)。まだ使っていないページがなくなると (返されたページはばらばらで、続いたものは作れない)
足せないので、そのときは捨てる・ENOMEM・予備と oom だけになる。

### 速さを測る (bench と /proc/ai/prof)

`bench` のあるもの (いまは vfs、extfs と spinlock、heap、smp) は、その速さを測るものがある。test/fsbench.c は、ファイルのよくある操作 (create、stat、
ないものの stat、open、4 KiB の書き読み、readdir、深いパス、rename、unlink、mkdir、16 MiB の書き読み、fsync) をそれぞれくりかえして、
1 回あたりの µs (大きなファイルは MB/s) を出す (Linux でも動く)。aish の `bench` (aish-vm) が VM (`-snapshot`) で n 回動かして op ごとの
まんなかをとり、`prof on` にした 1 回で /proc/ai/prof の part ごとの割合 (どこがボトルネックか) と、時間の多い区間を出す。
`against` で 2 つのカーネル (たとえば HEAD と作業中のもの) を op ごとにくらべる。結果は build/aish-vm/bench/ にも残る (速くするループ:
測る → いちばん遅いところを見つける → 直す → くらべる)。区間の名前は `part:what` (part は kernel.json の名前。test/kernel-json.py が確かめる)。
spinlock、heap、smp は lockbench (`echo lockbench > /proc/ai/ctl`。kernel/src/spinlock.rs) で、スピンロックを取って放す、ヒープから
取って返す、大きなロックを放して取りなおす、時計を読む (uptime_ns) の 1 回の ns (10 万回の平均) を kmsg に出す。aish の `bench` は
これも 5 回動かして、まんなかを `lock` に出す (`against` があれば、それもくらべる)。

### Linux と答えあわせ (sysdiff)

test/sysdiff.c は、種 (seed) から決まるシステムコールの並び (open、openat2、read、write、pread、lseek、truncate、stat、statx、
mkdir、unlink、rename (RENAME_NOREPLACE / EXCHANGE)、link (AT_EMPTY_PATH も)、symlink、readlink、chmod、chown、utimensat、umask、
access、getdents (小さなバッファも)、dup、fcntl、pipe、fsync、fallocate (PUNCH_HOLE / ZERO_RANGE)、copy_file_range、sendfile、
splice (パイプとファイル、パイプどうし)、tee、readv / writev / preadv2 / pwritev2 (iovec の形: 1〜4 こ、長さ 0 のもの、
1025 こ、さわれないもの。RWF_HIPRI / DSYNC / SYNC / NOWAIT / APPEND / NOAPPEND / ATOMIC / DONTCACHE / NOSIGNAL と知らないビット。
NOWAIT で読むときは cold (まず fadvise DONTNEED で外して EAGAIN を見る) か warm (まず読んでおく))、mmap / munmap /
msync と、その写しの中の読み書き (mread / mwrite: MAP_SHARED と MAP_PRIVATE、PROT_*、長さと位置は表から、終わりの先も。
SIGBUS / SIGSEGV は受けて答えにする)) を、
はじめの木 (a、b/、b/c、d -> b) の上で動かし、答えを 1 行ずつ出す。並びは種だけで決まる (動かした答えによらない) ので、同じ
種なら Linux でも aios でも同じシステムコールが同じ順に動き、2 つの出力の最初にちがう行がふるまいのちがいになる。答えは
アーキテクチャによらない形 (フラグは名前、stat は種類と許可とリンクの数と大きさ、ino や時刻は出さない) で、x86_64 の Linux と
aarch64 の aios でもくらべられる。手本は syzkaller (並びの作りかた。fd は前の op が開けたものを使う)、gVisor の syscall テスト
(本物の Linux で動かしたものを答えにする)、Csmith と C-Reduce (答えくらべと、ちがいを小さくする)。

aish の `sysdiff` (aish-vm) が、手本の Linux と VM (`-snapshot`) の aios で動かし (どちらもふつうの人で: Linux は nobody、
aios の中は ai。許可のしらべが同じになるように)、ちがいを (op、Linux の答え、aios の答え) でまとめ、多いものから、op を減らしても
同じちがいが同じ op で出るいちばん小さな並び (`SEED:I,J,...`) に縮める。手本は、既定で kernel.org の最新の安定版 (`base: latest`):
test/linux-vm.sh が arm64 の Linux を小さな設定 (test/linux.config) で作り (版ごとに一度、3 分ほど。build/linux/)、aios と同じ
QEMU の virt で起こして、その中で動かす (/init は test/linux-init.c。ext4 は 4 KiB のブロックの新しいディスク、tmpfs は /tmp)。
新しい安定版が出れば、次の `sysdiff` がそれを作ってくらべるので、手本はいつも最新になる。`base: 7.2.11` のように版を選ぶことも、
`base: host` でこのマシンのカーネルにすることもできる。ほかの Linux (たとえば aios(main)) の答えでくらべるなら、そこで
`cc -O2 -o sysdiff test/sysdiff.c && ./sysdiff -n 30 DIR 1-300 > linux.txt` とし、`linux` にそのファイルをわたす。

ホームの ext4 (`fs` の既定) と /tmp の tmpfs (`fs: tmp`) の両方で、30 op と、長い並び (`ops: 80`) の種を見る。いまの aios は
Linux 7.2.11 (最新の安定版) と、ext4 で 500 の種 (30 op) と 3000 の種 (80 op)、tmpfs で 1000 の種 (80 op) のどれもちがわない。
手本を 6.18 (このマシン) から 7.2 にしたときに出たちがいは rename の順だけ: Linux 7 は名前を引いたすぐあと (`__start_renaming`) に
自分の上や下への rename (EINVAL、ENOTEMPTY) を見るので、`name/` の ENOTDIR より先になった。aios は新しい順に合わせた。
えらんだちがいを 6 つにしたあとも、どれもちがわない。そのとき見つかった同じファイルの中の sendfile (Linux は読むページの
写しをとらないので、読むところと書くところが重なると、同じ回に書いたものがまざる) は、Linux に合わせずに sendfile-self をえらんだ。
splice と tee を覚えさせたときに出たちがいは 4 つ: 読み手のいないパイプへの splice は、ファイルの終わりでも EPIPE (aios の速い道が
ファイルを先に見ていた)、rename の . や .. への EBUSY は、消したディレクトリの中の名前の ENOENT より先、終わりが / のリンク
(`d/`) の先に O_CREAT で作るのは EISDIR、それと、splice でパイプに入れたあとにファイルを切りつめると Linux はパイプの中身まで
変わる (splice-snapshot をえらんだ)。tmpfs では、truncate(2) で大きさが変わらないときは時刻も変わらない (ftruncate(2) は変わる。
ext4 はどちらも変わる) のも出た。同じころ、Linux 7.2.11 の ext4 の ZERO_RANGE のまちがい (下の表) も見つけた。
test/linux-vm.sh の exec で、ほかのテスト (pipeslots、openat2、fsx) もその Linux の中で動かせる。ディレクトリを読む位置も
Linux と同じ: ext4 (dir_index) は名前のハッシュの順で、位置は `(hash >> 1) << 32 | minor_hash` (終わりは i64::MAX)、tmpfs は
Linux 6.18 の simple_offset (名前ごとに 3 からの offset、新しいものから、終わりは S32_MAX)。ちがいのうち、まちがいでもあったもの:
消した ext4 のファイルの番号を使った新しいファイルを、大きなロックなしの read が前のファイルのページキャッシュから読めた
(ほかの人のファイルの中身が見えた)、tmpfs のファイルの 1 TiB 先に 1 バイト書くとページの表が 4 GiB に伸びてカーネルが止まった
(どちらもふつうの人にできた)、開いたまま rmdir したディレクトリに名前を作れた、いっぱいに近いパイプへの 4KiB (PIPE_BUF) までの
write を分けて書けた (いくつもの書き手のものがまざる。いまはパイプを Linux と同じく 4KiB の箱 16 こで数える: test/pipeslots.c)。

readv / writev / preadv2 / pwritev2 を覚えさせたとき (ext4 600 と 1000 × 30、300 × 80、tmpfs 400 × 30 でどれもちがわない):
preadv2 / pwritev2 の flags をカーネルが見ていなかった (RWF_NOWAIT も RWF_APPEND も黙って無視) のを直し、Linux 7.2 の
kiocb_set_rw_flags と同じ決まりにした (知らないビットは EOPNOTSUPP、APPEND と NOAPPEND はいっしょに使えない、NOWAIT は
ext4 のファイルとパイプと /dev/null などだけで tmpfs は EOPNOTSUPP、ATOMIC はできない、DONTCACHE は ext4 だけ、ext4 の
バッファ書きは NOWAIT ではできない)。NOWAIT で読むときは、ディスクを待つなら EAGAIN (そのときそのページを読んでおくので、
次は読める。Linux は先読みを出す)。DONTCACHE は新しく読んだブロックを残さない。posix_fadvise の DONTNEED は本当に
(まるごと入るページだけ。Linux と同じ) キャッシュから外すようにした (ロボットの cold がこれを使う)。パイプの readv で、
入れ物にさわれないとき取り出した中身を失っていたのも直した (取り出す前に入れ物をたしかめる)。読み手のいないパイプへの
writev は、さわれない入れ物でも EPIPE が先 (Linux の pipe_write)。えらんだちがいが 2 つふえ (pipe-fault、pipe-nowait)、
Linux のまちがいも 1 つ (efault-extend: さわれない入れ物の書きこみで、ファイルが位置まで伸びる)。

mmap を覚えさせたとき (ext4 600 × 30 と 300 × 80、tmpfs 400 × 30 でどれもちがわない): はじめに出たちがいは 5 つで、
mmap のしらべる順 (fd がだめなら長さ 0 より先に EBADF、O_PATH の fd は EBADF、読めない fd (O_WRONLY と O_ACCMODE) は
EACCES、ディレクトリやパイプは ENODEV)、写したファイルの終わりより先のページは SIGBUS (aios は 0 を見せていた)、msync の
フラグ (MS_SYNC と MS_ASYNC の両方や知らないビットは EINVAL、領域のないところがあれば ENOMEM)。それより大きいのは、
ロボットが見る前から分かっていた、read / write と写しの食いちがい: aios は MAP_SHARED の写しに別のページを使い、
write() で書いたものは写しに映らず、写しに書いたものは msync まで read() に見えなかった (MAP_PRIVATE は古いページを
見つづけた)。そこで「ファイルのページは 1 枚」にした (kernel/src/vm.rs の頭): ファイルシステムが持っているページ
(ext4 ならブロックのキャッシュのページ、tmpfs ならファイルのページそのもの。Inode::page_frame) を、read()/write() も、
MAP_SHARED の写しも、MAP_PRIVATE の読むだけの写しも、ページキャッシュも、みなそのまま見る (Linux のページキャッシュと
同じ作り。写すのに読みなおさず、同じ中身が 3 枚あることもなくなった)。写しに書いたページは、ファイルシステムに
書き出す印をつけなおす (msync、munmap、sync、ファイルシステムの書き出し。はじめて書いたときは更新時刻もいまに: Linux の
page_mkwrite)。穴 (ブロックのないところ) の写しは 0 のページを vm.rs が持ち、ブロックをとるときにファイルシステムがその
ページをそのまま使う (adopt_hole) ので、穴に write() しても写しに映る。切りつめたところと穴をあけたところは、写しているところ
からみな外す (drop_file_pages: Linux の unmap_mapping_range。次に触れると SIGBUS、伸びればまた写せる)。ファイルの終わりの
先 (同じページの中) に写しで書いたものは、伸ばすとき (ftruncate、fallocate、終わりより先への write) に 0 にする (ext4 の
pagecache_isize_extended と同じ。tmpfs でも: Linux の tmpfs はそのまま中身にしてしまうので、手本のまちがい tmpfs-extend に)。
ブロックのキャッシュは、写されているページ (参照が表だけでない) を捨てない (fadvise DONTNEED と RWF_DONTCACHE も)。
動かしているプログラムは、ページが 1 枚になったので書けば動いているコードが変わる: 書くために開く、切りつめるのは
ETXTBSY (Linux と同じ。aipkg はふつうのファイルも先に外してから入れるので、動いているものは古い inode を使いつづける)。
test/mmapcoh.c が、これを ext4 と tmpfs でじかに確かめる (Linux でも通る。tmpfs の「伸ばしたら 0」だけは Linux がちがう)。

#### aios がえらんだちがい

Linux に合わせるのは最初の手本で、Linux よりよい道があれば、そちらをえらぶ。えらんだところは test/sysdiff.c の `chosen` に
名前とわけを書き (`sysdiff -l` で一覧)、Linux で動かすときは、そこだけ aios のふるまいをまねて答えにする (手本は、Linux の
答えに aios がえらんだものを足したもの)。だから aios がえらんだとおりに動かなければ、ふつうのちがいとして見つかる。それを
通った op の行の終わりには `[chosen 名前]` がつき、aish の `sysdiff` は `chosen` に、いくつの case で使ったかを、まちがいの
ちがい (`differ`) とは分けて出す。新しいちがいが見つかったら、まちがいか、もっとよい道かを決めて、よい道ならここに足す。
test/chosen.c は、表のものを 1 つずつじかに確かめる: aios の中では aios がえらんだとおりか (vm-ctests.sh で、CI でも)、
Linux の中では Linux がまだちがうか (`test/linux-vm.sh exec latest` で最新の Linux の中で動かす。Linux が aios と同じに
なったら `SAME` と出すので、表を見なおす)。

| 名前 | aios | Linux | わけ |
|---|---|---|---|
| pwrite-append | O_APPEND で開いた fd でも、pwrite64 と pwritev / pwritev2 は off に書く (pwritev2 に RWF_APPEND があれば終わりに、RWF_NOAPPEND なら Linux も off に) | off を見ずに、いつも終わりに書く | POSIX は O_APPEND が pwrite の書く場所を変えないと決めていて、Linux の pwrite(2) も BUGS にそれとちがうと書いている。終わりに足すなら write でよい |
| seek-unwritten | fallocate (場所をとる、ZERO_RANGE) でとって、まだ書いていないところ (ext4 の未初期化の extent) は、SEEK_DATA / SEEK_HOLE でいつも穴 | ext4 は、そこのページがキャッシュにあれば (一度読むと) データ、追い出されると穴 (tmpfs はいつも穴) | 答えが、読んだかどうかやメモリのぐあいで変わらない。中身はどちらも 0 なので、データとしても cp や tar が 0 を写すだけ。ロボットは Linux で、見る前に posix_fadvise(DONTNEED) でキャッシュから追い出してまねる |
| utimensat-omit | utimensat の時刻がどちらも UTIME_OMIT でも、フラグと fd とパスはほかのときと同じにしらべる (ないファイルは ENOENT、閉じた fd は EBADF)。たどれたら、何も変えずに 0 (変えないので、許可や読むだけのマウントは見ない) | パスも fd も見ずに、いつも 0 (ないファイルでも。fs/utimes.c に「パスも見ない」とある) | POSIX は、たどれないパスをエラーにすると決めている。何もしない呼び出しでも、あるはずのファイルがないことを 0 で隠さない。ロボットは Linux で、同じたどりかたの fstatat (fd なら fcntl) でまねる |
| copy-append | O_APPEND で開いたファイルへも copy_file_range と sendfile で写せる。書く位置をわたせばそこへ (pwrite-append と同じ)、わたさなければ write と同じく終わりに足し、位置は終わり + 写した分 | copy_file_range は EBADF、sendfile は EINVAL | `>>` で開いたファイルへも、read と write にもどらずにカーネルの中で写せる (cat や cp がやりなおさなくてよい)。O_APPEND は位置のない書きこみを終わりにする決まりで、写すのを止めるわけはない。splice(2) ももとから同じ (ロボットはまだ splice を見ていない)。ロボットは Linux で、O_APPEND をはずして終わりの位置で写してまねる |
| zero-range-tmpfs | tmpfs でも fallocate の ZERO_RANGE ができる (0 にして、KEEP_SIZE でなければ大きくする。ext4 と同じ) | tmpfs (shmem) は KEEP_SIZE と PUNCH_HOLE だけで、ZERO_RANGE は EOPNOTSUPP | 使えるかがファイルシステムで変わらない。/tmp でも、穴あけと場所とりの 2 回に分けなくてよい。memfd の封は、書く封 (F_SEAL_WRITE) と大きくする封 (F_SEAL_GROW) を先に両方見る (半分だけ 0 にならない)。ロボットは Linux で、穴をあけてから場所をとってまねる |
| sendfile-self | 同じファイルの中の sendfile は copy_file_range と同じ決まり: 読みはじめたときの終わりまでを写し、読むところと書くところが重なれば EINVAL | 重なっても写す。読むのがページキャッシュのページそのもの (写しをとらない) なので、同じ回に書いたものが読みにまざり、答えが内側のくぎり (16 ページ) やページのまとまり (folio) の大きさで変わる | 答えがきちんと決まる (同じことをする copy_file_range と同じ決まり)。ロボットは Linux で、長さ 0 の sendfile でほかのしらべをしてから、長さを終わりまでに縮め、重なりを見てまねる |
| splice-snapshot | ファイルからパイプへの splice / sendfile は、そのときの中身をパイプの箱に写す。あとでファイルを書いたり切りつめたりしても、パイプの中身は変わらない | ページキャッシュのページそのものを箱に入れる (写さない)。読む前にファイルが変われば、パイプからもそれが読める。切りつめると、端のページの残りが 0 で読める | パイプに入れたものは、入れたときのもの (cat や cp が、書きかえ中のファイルのまぜこぜを送らない)。aios はもとから写しているので、そのぶん速くもならない。ロボットは Linux で、そのときの中身の写し (名前のないファイル) から入れてまねる |
| pipe-fault | パイプの readv / writev で途中の入れ物にさわれないとき、さわれる分をみな読み書きしてその数を返す (なにもできなければ EFAULT) | パイプの箱 (4 KiB) ごとに写し、途中でさわれなくなった箱は捨てる (7 バイトの writev は EFAULT、12000 バイトは 4096。readv は写した分も数えず箱に残す) | どこまでできたかがそのまま分かる。ふつうのファイルでは Linux もバイトごとなので、パイプだけ箱ごとなのは中の作りが見えているだけ |
| pipe-nowait | パイプの preadv2 / pwritev2 の RWF_NOWAIT は、そのパイプを splice や tee に使ったあとも効く | splice に使った (しくじっても) パイプの FMODE_NOWAIT を消すので、それからは EOPNOTSUPP | aios のパイプは splice でも写しを入れる (splice-snapshot) ので、読むときに待つことがない。フラグが黙って効かなくなるのは分かりにくい |

#### 手本の Linux のまちがい

手本の Linux にまちがい (書いてあるとおりに動かないところ) があれば、aios はそれに合わせない。test/sysdiff.c の
`linux_bugs` に名前とわけを書き (`sysdiff -l` の `linuxbug` の行)、Linux で動かすときはそのあとでただして答えにする
(`[linuxbug 名前]` がつき、aish の `sysdiff` は `linux_bugs` に数える)。Linux が直ったら消す。test/kernel-json.py が、この表と
そろっているかを見る。

| 名前 | Linux のまちがい | ロボットは Linux で |
|---|---|---|
| zero-range-eof | ext4 の fallocate ZERO_RANGE で、範囲がファイルの終わりをこえ、終わりのブロックがもうディスクにあると、そのブロックの中 (ファイルの終わりまで) が 0 にならない。7.2.11 の ext4_alloc_file_blocks が、大きさをのばすときに ext4_block_zero_eof で終わりのブロックの残りを 0 にするが、それが ZERO_RANGE で落としたばかりのページを読みもどす。6.18 にはない (delalloc でまだディスクにないブロックなら 7.2.11 でも 0 になる) | ZERO_RANGE がうまくいったら、範囲のうちファイルの終わりまでを PUNCH_HOLE で 0 にする |
| efault-extend | ファイルの終わりより先への write / pwrite / pwritev2 がさわれない入れ物で EFAULT になっても、ファイルがその位置まで伸びる (ext4 も tmpfs も: generic_perform_write が write_begin のあとに写し、0 バイトでも write_end が i_size を pos にする) | 7.2.11 と 6.18 | なにも書けなかったのだから伸びない (aios はそう) |
| tmpfs-extend | tmpfs で、写し (mmap) でファイルの終わりの先 (同じページの中) に書いたものが、ファイルを伸ばすと (ftruncate、fallocate、終わりより先への write) そのまま中身になる (shmem_setattr と shmem_write_end はページの終わりの先を 0 にしない。ext4 は pagecache_isize_extended で 0 にする) | 7.2.11 | POSIX は伸ばしたところは 0 と読めると決めている (ftruncate(2)、write(2))。伸ばした op のあとで、前の終わりからそのページの終わり (op が書いたところの手前) までに 0 を書く (aios は ext4 でも tmpfs でも 0) |

## カーネルの中を見る (/proc/ai)

止まった・遅いプログラムを調べるための、カーネルの中の様子。AI が読むので、どれも 1 行 1 つの JSON。
読み書きできるのは root と wheel の人だけ (aibox の中でも。`aibox --root` の中は不可)。pid はいつも本当の番号。

| ファイル | 中身 |
|---|---|
| `/proc/ai/threads` | すべてのスレッド: 状態、最後のシステムコール、走った回数。眠っていれば `slept_s` (眠りつづけている秒) と `wait`: futex (アドレス、待つ値、いまの値) / poll (見張っている fd) / chan |
| `/proc/ai/fd/PID` | そのプロセスの fd: 何か、読める・書ける・閉じた、読まれずに残っているもの (`pending`) |
| `/proc/ai/stack/TID` | スレッドのレジスタ、スタック (sp から 64 KiB まで)、ファイルを写している地図。aish-sys の `stack` がファイルの `.eh_frame` で呼び出しの並びをたどる |
| `/proc/ai/bkl` | 大きなロックの統計 (`/proc/bkl` と同じもの): CPU ごとの待ち・持ち、長く持ったシステムコール、デバイスとスケジューラを待った時間、ページフォルトの種類 |
| `/proc/ai/kernel` | カーネルの地図 (kernel/kernel.json そのもの。だれでも読める) |
| `/proc/ai/mem` | メモリの中 (aish の `mem`): RAM の全部と空き (`min_free` を切ると取りもどす)、カーネルのヒープ (使っている、いちばん多かった、空きのかたまり、取れなかった回数) とスタック、ページキャッシュ (ページの数と上限、読みそこねた数とその時間、捨てた数、ファイルごとのページの数と名前 (パスの覚えにあれば)、多い順)、MAP_SHARED のページ (書いたもの)、tmpfs、スワップ、ページフォルト、メモリを多く使っているプロセス (rss、vsize)。数はページ (4 KiB) かバイト |
| `/proc/ai/fail` | わざとしくじらせる決まり (フォルト注入。aish の `fail`): `ctl` に `fail pid N\|name NAME sys NR\|all ERRNO [TIMES]` で、そのプロセスのシステムコール (番号か名前か、みな) を動かさずに -ERRNO に、`fail ... heap [TIMES]` でカーネルのヒープの alloc を、`fail ... page [TIMES]` で物理ページを取れないことに (TIMES 回。既定 1)。`fail off` でやめる。読むと決まりと、あと何回か (`left`)、効いた数 (`hits`)。プログラムが ENOMEM や EIO にどう耐えるか (musl の起こりの mmap を ENOMEM にすると落ちる、など) と、カーネルの中の「取れなかった」道を確かめる: page は Option で返るので、そのプロセスが out of memory で殺されてカーネルは続く。heap は耐えるところ (read の入れ物など) で: そのシステムコールが ENOMEM。`fail ... oom [TIMES]` は耐えないところで: 予備から渡り、次のシステムコールで oom がいちばん大きなプロセスを殺す (`-snapshot` の VM で。上の「ヒープが足りないとき」) |
| `/proc/ai/parts` | 切れるところ (kernel.json の `off` のあるもの) のいまの様子: `off` (切ってあるか)、`runtime` (動かしたまま変えられるか)。だれでも読める。`ctl` の `part NAME off\|on` で、使うたびに見るもの (システムコールと quirk) は動かしたまま切る・入れる (aish の `part`)。起動のときに起こす装置と aiproc は `aios.off=` だけ |
| `/proc/ai/trace` | カーネルの中の出来事の記録 (aish の `trace`): `ctl` に `trace on [NAME]` で始め (NAME があれば、その名前で始まるプロセスのシステムコールとフォルトだけ。切りかえ・眠り・起こし・割り込み・ディスクはみな)、`trace off` で止め、`trace clear` で空にする。8192 こまでの輪 (古いものから消える)。はじめの行が様子 (`on`、`filter`、`events`、`dropped`、`names`: tid → 名前)、あとは 1 行 1 つ: `t` (起動からの ns)、`c` (CPU)、`p` (tid)、`k`: `sys` (入り。`n` 名前、`a` 1 つめの引数) / `ret` (出。`r` 答え) / `switch` (その CPU が選んだ。`from` 前のもの) / `sleep` (`chan`) / `wake` (`chan`、`by` 起こしたもの) / `irq` / `blk` (`op`、`sector`、`n` セクタ) / `blkdone` (`ns`) / `fault` (`va`、`w`) / `exec` / `exit` (`code`)。読みはじめで 1 度作り、続きはそれを切って返す |
| `/proc/ai/fs` | ファイルシステムの中 (aish の `fs`): マウントごと (ext4: ブロックキャッシュの数と上限、まだ書いていないメタデータと中身、当たり・外れ・まとめ読み・捨てた数、書き出した回数と書いたブロック、ジャーナルの大きさと次のトランザクション、inode と名前の覚え、使っている inode、片づけ待ち、空き。tmpfs: ページ)、パスの覚え (数、ないもの、上限、当たり・外れ、捨てた回数、変わったディレクトリの数)、開いているファイルの数、区画ごとのディスクの読み書き (回数、セクタ、ms) |
| `/proc/ai/prof` | カーネルの中の区間 (`part:what`。part は kernel.json の名前) ごとの回数と時間 (ctl に `prof on` から `prof off` まで): まず part ごとの `self_us` (中の区間をのぞいた、そこだけの時間) と割合 (`pct`。いちばん多いものがボトルネック)、つぎに区間ごとの `count` `total_us` `self_us` `avg_us`。いまはファイルまわり (sysfile、vfs、extfs、vm、block、jbd2) |
| `/proc/ai/ctl` | 書くスイッチ (root:wheel の 0664): `kick PID` (futex で眠っているスレッドをみな起こす。起こしが消えたのか、だれも起こしていないのかを見分ける)、`raw TID` (レジスタとスタックを kmsg に)、`pcmiss on\|off`、`strace NAME`、`bkl reset`、`heap trace on\|off`、`heap check` (ヒープの決まりを確かめて kmsg に)、`prof on\|off`、`lockbench` (ロックなどの 1 回の時間を kmsg に) |

Claude は aish-sys の `threads` (絞りこみと fd の中身つき)、`hang` (なぜ止まっているかの見立て: 長く眠っているスレッドと待っているもの、
残っているデータ、kick して眠りなおすか。pid がなければ、みなを起こさずに見てあやしいものだけ)、`stack` (呼び出しの並び) で使う。`stack` は `.eh_frame` (DWARF の CFI) でたどり、
CFI のないところ (musl の libc) はスタックから戻り先 (前の命令が bl / blr の値) を探して、CFI で 3 段つながる sp を当てる
(`how`: pc / cfi / lr / scan、つながらなければ guess)。名前は `.symtab` か `.dynsym`、名前のないもの (strip した libxul など) は
その関数のあたりで使っている文字列 (`strings_near`。adrp + add が指すもの) を手がかりに出す。

## カーネルの見張り (aiwatch)

`aiwatch.service` (root) が 5 秒ごとに `/proc/ai` を見て、カーネルのまちがいや詰まりを見つける。

| kind | 見つけるもの |
|---|---|
| `lost_wakeup_poll` | ppoll / select で 10 秒より眠っているのに、待っているもの (読める、書ける、閉じた) がもう来ている (epoll は、エッジトリガーではふつうのことなので見ない) |
| `lost_wakeup_futex` | futex で 10 秒より眠っているのに、値がもう待つ値でない (変えた人が起こしそこねた) |
| `wake_storm` | 待つシステムコールで、眠っては起きるのを 1 秒に 300 回より (関係ないものに起こされているか、本当に仕事が多いか) |
| `bkl_busy` | 大きなロックが 90% より使われている |

起こしが消えたものは続けて 2 回見えたら、起こしすぎと混雑はそれが 2 回続けて見えたら知らせる (起動のときなど、しばらく
本当に忙しいだけのものをのぞく)。記録は `/var/log/aiwatch.service.log` (1 行 1 つの JSON、直ったら `resolved`)、
いま続いているものは `/run/aiwatch.json`。スレッドのことなら、そのときの `/proc/ai/stack/TID` を `/var/lib/aiwatch/` に残す
(64 まで)。プロセスが終わっていても、aish-sys の `stack` の `file` でたどれる。Claude は aish-sys の `watch` で読む。
aish の MCP (`aish --mcp`) は、新しく見つかったものをどのツールの答えにも 1 度だけ `[aiwatch] kind: …` の行で添える
(AI が aios を触っているあいだに、カーネルで起きていることに気づけるように)。
`/proc/ai` のない (古い) カーネルでは起こさない (`ConditionPathExists`。それでも動けば exit 2 で、起きなおさない)。
`aiwatch --rescue` は、起こしそこねた futex のスレッドを知らせたあとで起こしなおす。`aiwatch --once` は 3 回見て結果を出して終わる
(`test/watch.c` でわざと作れる)。

## 改造 (aios src / build / install)

```sh
aios src                       # /usr/src/aios に aios のソース (なければ git clone、あれば git pull)。wheel の人が書ける
                               # (git が要る: sudo ap -S git。aiosd が /etc/gitconfig の safe.directory に足す)
aios build kernel              # AIOS_INITRD=none でビルドし、target/Image (起動できる形) を作る (objcopy の代わりも aios がする)
aios install kernel            # /boot/Image を入れかえる (aiosd)。前のものは /boot/Image.prev
aios install kernel --revert   # /boot/Image と /boot/Image.prev を入れかえる
aios build pkg NAME|DIR [-o DIR]   # PKGBUILD からパッケージを作る (NAME は /usr/src/aios/pkg/*/NAME)
aios install pkg FILE...       # 作ったパッケージを入れる (aiosd が aipkg -U で)
```

- 前のカーネルは起動の一覧に「aios (previous kernel)」(`/boot/loader/entries/prev-aios.conf`) として出る。
  一覧が出るように、loader.conf の `timeout 0` は 3 秒にする
- 起動しなかったら自動で前のものに戻す (systemd-boot の boot counting を小さくしたもの):
  1. `aios install kernel` で aiosd が `/boot/loader/try` に試す回数 (1) を書く
  2. aiboot は try があれば 1 減らして新しいカーネルを起動する。もう 0 なら (前の起動が aiosd まで来なかった)、
     前のカーネルを `aios.fallback=1` をつけて起動する (一覧で前のものを選んだときも同じ)。わけは
     `aios.fallback_why=`: `unfinished` (前の起動が aiosd まで来なかった)、`load` (新しいものを UEFI の LoadImage が
     読めなかった。その場で前のものへ)、`chosen` (一覧で選んだ)
  3. 起動して aiosd が動くと try を消す (起動できた)。`aios.fallback=1` で起きたときは `/boot/Image` を前のもの
     (いま動いているもの) に戻す。どちらも `/var/log/aiosd.log` に `{"op":"boot",...}` で残り、`aios get boot.last`
     の `result` と `why` で読める
  新しいカーネルが止まったら、電源を入れなおす (リセット) だけで前のものに戻る
- `test/vm-boot.sh [--new]` (開発のマシン) は、これを disk.img の VM (-snapshot) で確かめる: 起動するもの、読めないもの、
  止まるもの (`AIOS_TEST_HANG=1` でビルドしたカーネル) を入れて再起動し (`test/vm.py reboot --reset-after`)、
  前のものに戻ったか、わけ (why) を見る。`--new` はこのソースの aiboot、aiosd、aios を入れてから
- aikernel のパッケージを上げると /boot/Image はパッケージのものになる
- `aios build pkg` は bin/mkpkg.sh と同じものを aios の中で作る (user/src/lib/mkpkg.rs)。PKGBUILD の関数
  (pkgver prepare build package) は bash (brush) で動かし、ソース (git+、http(s) は fetch、横のファイル)、sha256、
  .PKGINFO、tar と zstd はこちらでする。zstd のコマンドがなければ ruzstd で縮める (速いが、-19 ほどは縮まない)。
  作業の場所は ~/.cache/aios/build/NAME。root はいらない (ファイルの持ち主は tar の中で root にする)
