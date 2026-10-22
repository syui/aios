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
`heap hog 0` で返す)。

小さいもの (ヘッダこみ 512 バイトまで) は、CPU ごとの置き場 (heap.rs の PCPU: Linux の slab の CPU ごとの freelist と
同じ考え) から取って返す: 返されたブロックを CPU ごと・16 バイトきざみの大きさごとに 16 こまで持っておき、同じ大きさを
取るときはヒープのロックも TLSF も通らない (lockbench の heap alloc+free は 310 → 144 ns。パスを写すたびなどに使う)。
カーネルの中は IRQ を止めて走り (開けるのはアイドルで割り込みを待つあいだだけ)、alloc の中でほかへ切りかわらないので、
CPU ごとのものはロックなしで触れる。大きさの組は取るときと返すときの Layout から同じに決まる。置き場にあるあいだも TLSF
からは使っているもの (`/proc/ai/heap` の `cached`、ふつう数十 KiB)。heap trace と `fail` のあいだ、予備のブロックは通らず、
足りなくなれば (遅い道) その CPU の分を TLSF に返してから足しにいく。

足すのに要る続いたページは、物理ページのアロケータ (kernel/src/kalloc.rs) が buddy (Linux の mm/page_alloc.c と同じ考え)
なので作れる: 空きは 2^k ページ (4 KiB から 4 MiB) のかたまりで持ち、返ってきたかたまりは、となりの同じ大きさのかたまり
(buddy) も空いていればまとめなおす。かたまりの印 (頭のしるし、リストの次と前、大きさ) は空いているページの中ではなく、RAM の
おしまいの表 (ページあたり 8 バイト。Linux の mem_map のようなもの) に書くので、起動のときにも空いているページには触らない
(VM のホストは触ったところにだけメモリを用意する。はじめにページの中に書いたら、4 MiB ごとに 1 ページ触ることになって、2 GiB の
VM の起動が 8 秒おそくなった)。起動のときのヒープの区画は、buddy を作る前にカーネルのすぐうしろから切り出す (Linux の memblock
のように)。前はフリーリスト 1 本で、続いたものはまだ一度も使っていない領域からしか作れず、RAM を
ひととおり使ったあと (大きなビルドのあとなど) はヒープを足せなかった (捨てる・ENOMEM・予備と oom だけ)。いまはページキャッシュを
捨てたあとにも、まとまった空きでもう一度足しにいく。1 枚ずつ返ってきたものは、すぐにはまとめず少し (1 MiB) 置いておく
(すぐまた取るならキャッシュにのっている。Linux の per-cpu のリスト)。`ctl` の `page cycle` は空きのページを (少し残して)
みな取ってから返し (RAM をひととおり使ったあとの様子)、`page check` は空きの決まり (境目にそろう、重ならない、まとめ残しが
ない、数があう、参照の数が 0) を確かめる (vm-check の ai-page-buddy: page cycle のあとでも hog 100 が足して取れる)。
大きさごとのかたまりの数は `/proc/buddyinfo` (Linux と同じ形) と `/proc/ai/mem` の `ram.blocks` (aish の `mem` は、いちばん
大きな続いた空きとその数)。

### 速さを測る (bench と /proc/ai/prof)

速さは **測る → 記録する → くらべる → いちばん遅いところを見つけて直す → また測る** のくりかえしで上げる。そのための道具:

- **測るプログラム** (aios でも Linux でも動く。1 行 1 つの JSON で op ごとの 1 回の µs か MB/s)
  - test/fsbench.c: ファイルのよくある操作 (create、stat、ないものの stat、open、4 KiB の書き読み、readdir、深いパス、rename、
    unlink、mkdir、16 MiB の書き読み、fsync)
  - test/netbench.c: ループバック。パイプ、AF_UNIX、TCP、UDP の 1 バイトの往復を 2 つのプロセスで (`*_rr`) と 1 つのプロセスで
    (`*_local`: 切りかえなしの、ソケットの道だけ)、epoll で待つ往復、64 KiB と 4 KiB の流しこみ、connect から閉じるまで。
    パイプの往復はくらべるための土台 (プロセスの切りかえと起こしだけ)。`futex_rr` は 2 つのスレッドが futex で交互に起こし
    あう往復 (スレッドの切りかえと futex の起こしだけ: pthread のミューテックスや条件変数の土台)。`-r RUNS` でくりかえし、
    `-o OP,...` で選ぶ
  - test/smpbench.c: いくつもの CPU でいっしょに。getppid、umask、futex、stat、open、4 KiB の読み書き、作って消す、パイプ、
    AF_UNIX、mmap、fork を、1 つのプロセスと CPU の数だけのプロセス (それぞれ自分のファイルなど) で。伸び (1 つのときの何倍
    まわせたか) を Linux とくらべ、op ごとに /proc/ai/bkl の大きなロックを待った割合と、長く持ったシステムコール (どこから
    大きなロックの外に出すか)。TCG では QEMU も例外のたびに自分のロックを取るので、いちばん軽いもの (getppid) は Linux でも
    伸びない: くらべるのは伸びより、4 つのときの時間の比
- **aish の `bench`** (aish-vm。bench.rs): `kind` (fs か net) のプログラムを VM (`-snapshot`、`smp` 個の CPU、既定 1) で n 回
  動かして op ごとのまんなかをとり、`prof on` にした回で /proc/ai/prof の part ごとの CPU の時間の割合と、時間の多い区間を出す
  (net は op ごとに)。`against` で 2 つの aios のカーネルを、`linux` で同じプログラム (同じ libc の静的なもの) を手本の Linux
  (test/linux-vm.sh の latest) の VM で動かして op ごとにくらべる。VM はどちらも QEMU の cortex-a72、同じ CPU の数、2 GiB
  (`LINUX_CPU` と `LINUX_SMP`)。TCG (x86 の上で aarch64 をまねる) なので、本物の機械とは比がちがうことがある
- **記録** (`record`): bench/KIND.jsonl に 1 回 1 行 (いつ、コミット (作業中なら dirty)、VM とホスト、aios と Linux の op ごとの値、
  時間の比、op ごとの part と CPU を使った区間の 1 回あたりの µs) を足し、bench/KIND.md (表) を作りなおす。表には、いちばん
  新しい記録の aios と Linux と比、前の記録 (同じ VM と量のもの) からの比、part、CPU を使っている区間、「直すところの候補」
  (Linux より遅いもの、前の記録より 1 割より遅くなったもの)、これまでの値の並びが出る。記録はリポジトリに入れる
  (コミットしてから測って記録し、記録をコミットする)。`bench` はいつも前の記録からの変わりかたを `since_last` に出し、
  `show` で測らずに記録を見る
- **/proc/ai/prof**: 区間の名前は `part:what` (part は kernel.json の名前。test/kernel-json.py が確かめる)。self_us は CPU を
  使った時間で、眠っていた時間 (相手やディスクを待った: proc.rs の sched で CPU を手放してから戻るまで) は、いちばん内側の
  区間の `wait_us` に分ける (往復の bench で、相手を待つ時間が read の時間に見えないように)。システムコールの区間は
  syscall.rs の sys_span (sysfile、socket、epoll、futex)、中は inet:* (tcp_send、tcp_recv、deliver、wake、syn、accept、close、udp_*)、
  unix:* (send、recv、deliver)、file:pipe_*、proc:wakeup、それと vfs / extfs / vm / block / jbd2

kernel.json の `bench` のあるもの (vfs、extfs、inet、socket、unix と spinlock、heap、smp、mem) は、その速さを測るものがある。spinlock、
heap、smp、mem は lockbench (`echo lockbench > /proc/ai/ctl`。kernel/src/spinlock.rs) で、スピンロックを取って放す、ヒープから取って返す、
大きなロックを放して取りなおす、時計を読む (uptime_ns)、物理ページを 1 枚取って返す・1024 枚取ってから返す (kalloc) の 1 回の ns
(10 万回の平均)、64 KiB の memcpy (src が 1 バイトずれたものも) と 4 KiB を 0 にする memset (mem.S) の 1 回の ns を kmsg に出す。aish の `bench` はこれも 5 回動かして、まんなかを `lock` に出す (`against` があれば、それもくらべる)。
2 行めの trapbench (kernel/src/trap.rs の bench) は、システムコール 1 回のうちベクタの asm (レジスタを積んでおろす、eret) の
ほかの重さ: 偽の TrapFrame で trap_handler をそのまま呼んで、速い道 (getppid、ロックなし)、大きなロックを取る道 (umask)、
その中の dispatch だけ、ユーザーへ戻るまえ (resched、killed、signal) だけ、ESR を 1 回読む、CPU の番号を読む (7 回まわして速いもの)。

### Linux と答えあわせ (sysdiff)

test/sysdiff.c は、種 (seed) から決まるシステムコールの並び (open、openat2、read、write、pread、lseek、truncate、stat、statx、
mkdir、unlink、rename (RENAME_NOREPLACE / EXCHANGE)、link (AT_EMPTY_PATH も)、symlink、readlink、chmod、chown、utimensat、umask、
access、getdents (小さなバッファも)、dup、fcntl、pipe、fsync、fallocate (PUNCH_HOLE / ZERO_RANGE)、copy_file_range、sendfile、
splice (パイプとファイル、パイプどうし)、tee、readv / writev / preadv2 / pwritev2 (iovec の形: 1〜4 こ、長さ 0 のもの、
1025 こ、さわれないもの。RWF_HIPRI / DSYNC / SYNC / NOWAIT / APPEND / NOAPPEND / ATOMIC / DONTCACHE / NOSIGNAL と知らないビット。
NOWAIT で読むときは cold (まず fadvise DONTNEED で外して EAGAIN を見る) か warm (まず読んでおく))、mmap / munmap /
msync と、その写しの中の読み書き (mread / mwrite: MAP_SHARED と MAP_PRIVATE、PROT_*、長さと位置は表から、終わりの先も。
SIGBUS / SIGSEGV は受けて答えにする)、flock (LOCK_SH / LOCK_EX / LOCK_UN。いつも LOCK_NB) と fcntl のレコードロック
(F_SETLK / F_GETLK / F_OFD_SETLK / F_OFD_GETLK: 種類、whence、始まりと長さ (負の長さも)、l_pid。同じファイルを別に開いた
fd を多めに選び、なければそのファイルをもう一度開く op にして、取り合いが起きるように。F_GETLK の答えのうち pid は
self / other / -1 で)、プロセスとシグナル (fork する子は、すぐ終わる、自分にシグナルを送って死ぬ、止まる、待つ (SIGTERM、
SIGUSR1、SIGINT、SIGTSTP をそれぞれ既定 / 無視 / ハンドラ / ブロック。自分のグループやセッションを作ることも) のどれか。
kill (その子か、そのグループに。シグナルの番号は 0 とだめなものも)、wait4 / waitid (いつも WNOHANG。相手はその子、だれでも、
自分のグループ、その子のグループ、いない番号。options は WUNTRACED / WSTOPPED / WCONTINUED / WNOWAIT と知らないビット)、
自分の SIGCHLD の扱い (SIG_DFL / SIG_IGN / ハンドラと SA_NOCLDSTOP / SA_NOCLDWAIT)、setpgid / getpgid / getsid。子の番号は
kN、自分は me と出す)、AF_UNIX のソケット (socket / socketpair: SOCK_STREAM / DGRAM / SEQPACKET / RAW と知らない型、
protocol、知らないフラグ、AF_INET と知らない family。bind / connect の名前: パス、ディレクトリの中、リンクの先、ふつうの
ファイル、ないディレクトリ、抽象名前空間、autobind、長さ 0 の抽象、108 文字、AF_INET、長さ 1、AF_UNSPEC。listen の backlog、
accept4、send / sendto / sendmsg (MSG_OOB / DONTWAIT / NOSIGNAL / EOR / MORE / CONFIRM / PEEK、SCM_RIGHTS、SCM_CREDENTIALS
(自分と、ほかの pid)、知らない cmsg)、recv / recvfrom / recvmsg (MSG_PEEK / TRUNC / WAITALL / DONTWAIT / OOB / CMSG_CLOEXEC /
ERRQUEUE、付帯データの入れ物の大きさ)、shutdown、getsockopt / setsockopt、getsockname / getpeername、poll と FIONREAD。
いつも SOCK_NONBLOCK で作る)、AF_INET のソケット (ループバックの TCP と UDP。名前は 127.0.0.1 / 0.0.0.0 / 127.0.0.2 と、
ポート 0、だれもいない 1、root だけの 80、前の op のソケットのポート、ないアドレス (bind だけ)、長さ 4、family のまちがい。
ポートは名前で出す: #j は op j のソケットのポート、ほかは出てきた順に P1、P2 ...。inetpair はロボットの op で、つながった
TCP か UDP の 2 つを作る (socketpair の AF_INET 版: 送る・受ける・閉じるの奥まで届くように)。IPPROTO_TCP と IPPROTO_IP の
sockopt、IP_RECVERR と MSG_ERRQUEUE、MSG_OOB も。TCP はいつも TCP_NODELAY で、1 回に送るのは 4096 バイトまで、MSG_MORE
なし (Linux の Nagle とためたものは、相手の ACK が来たときに出る: 遅らせた ACK の時間しだい)。つながったあとの TCP_MAXSEG
は 32741 からは wnd と出す (相手の窓の半分までで、Linux の受ける窓は受けた skb の truesize の比で育つ: ページのつくりしだい)) を、
はじめの木 (a、b/、b/c、d -> b) の上で動かし、答えを 1 行ずつ出す。並びは種だけで決まる (動かした答えによらない) ので、同じ
種なら Linux でも aios でも同じシステムコールが同じ順に動き、2 つの出力の最初にちがう行がふるまいのちがいになる。答えは
アーキテクチャによらない形 (フラグは名前、stat は種類と許可とリンクの数と大きさ、ino や時刻は出さない) で、x86_64 の Linux と
aarch64 の aios でもくらべられる。手本は syzkaller (並びの作りかた。fd は前の op が開けたものを使う)、gVisor の syscall テスト
(本物の Linux で動かしたものを答えにする)、Csmith と C-Reduce (答えくらべと、ちがいを小さくする)。
プロセスの op は、子が落ちつくまで (/proc/PID/stat の状態が 眠っている・止まっている・ゾンビ・もういない で、3 回つづけて
同じになるまで。待つ子は、シグナルの扱いを決めおえた印を同期のパイプで送ってから) 待ってから次へ進むので、子の動く速さに
答えがよらない。子のハンドラが受けとったシグナルも同期のパイプで知る (handled)。case はそれぞれ自分のセッションを作り、op は
その中の別のグループのプロセスが動かすので、孤児のプロセスグループの決まり (止めるシグナルを捨てるか) が、ロボットを動かす
ところ (端末か init か) によらない。case の終わりには、のこった子をみな SIGKILL で終わらせて回収する。

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

書いたものもページキャッシュに残す (Linux と同じ。前は書くたびにそのファイルのページをみな捨てたので、書いてすぐ読む
16 MiB は Linux の 3.6 倍の時間だった)。ext4 (ブロックが 1 ページ) のページキャッシュのページはブロックのキャッシュの
ページそのものなので、write() はそれをその場で書きかえる: 捨てずに、書いたページも入れる (vm::file_writing と
file_written。ブロックのキャッシュは写されているページを捨てないので、表のページはいつもブロックのページ)。ファイルの版
(更新時刻と大きさ) はページごとでなくファイルに 1 つ (Linux の i_size と同じ: 書いて伸びても、ページを書きなおさない)。
命令キャッシュは、実行できるように写すときにだけあわせる (ページに印。Linux の PG_dcache_clean。read で読むだけのページは
しない)。mprotect で実行できるようにしたファイルの写しも、そのときにあわせる。read-seq (書いてすぐの 16 MiB) は
472 → 2086 MB/s (Linux 7.3 は 1721)、read-4k も Linux より速くなった。そのとき見つけたもの: tmpfs のファイルを写した
(MAP_PRIVATE) ページが、ファイルを消してもページキャッシュに残っていた (fsrace 1 回で 4 万ページ。inode がなくなるときに
捨てる)、vfat は同じ場所に作ったファイルが前のファイルのページを見えた (番号がエントリの場所なので: 消す、名前を変える、
作るときに捨てる)、さわれない入れ物の write / writev (EFAULT) で setuid と setgid が落ちなかった (Linux の file_modified は
写す前。いまは写す前に落とす)、pwritev2 の RWF_DONTCACHE を書くときは見ていなかった (いまは書いたページを、だれも写して
いなければ残さない)。ロボットの writev は、位置をとらないのに pwritev2 の位置で efault-extend をただそうとしていた (直した)。

パスの覚え (vfs.rs の PATHS: 正規化したパスから inode) から答える stat も、Linux の dcache より遅かった (深いパスで 1.8 倍)。
測ると、当たっているのに、パスを整える normalize (Vec に分けて join) と、覚えの BTreeMap が長いパス (前の部分が同じ) を
何度もくらべるのが重かった。いまは、normalize は 1 つの String にバイトで分けて足し (文字の上で畳めないパスかも同じ回で
見る)、覚えの鍵はパスのハッシュが先 (くらべるのはハッシュが同じもの 1 つ)、stat (resolve) はたどったパスを写さない。
開いているディレクトリから引くもの (fstatat の dirfd。find、rm -r、ls -l、fts) は覚えを使っていなかった: いまは、そのディレクトリの
パスの覚えがいまもそのディレクトリ (名前を変えられていない) なら覚えから答え、通れるかはそのディレクトリから先だけを見る
(Linux も開いているところより上は見ないので、上が通れない人も引ける。test/pathcache.c の dirfd-*)。fsbench の deep-stat は
16.9 → 8.5 µs、stat-at (fsbench に足した) は 31.9 → 14.3 µs。測る道具 (/proc/ai/prof) も直した: 区間の名前の表を頭から
1 つずつ見ていたので、あとから入った名前ほど区間のたびに重く、表 (64) より名前が多い (いまは 85) と、あふれた区間は
数えられなかった (256 にして、名前の場所のハッシュで引く)。

自分のことを読むだけのシステムコールも、Linux より 1〜2 µs 遅かった (umask 3.0 / Linux 2.5 µs、getpgid 3.5 / 1.6、
lseek 3.6 / 1.7)。trapbench で分けると、大きなロックを取る道はカーネルの中だけで 2.8 µs (速い道は 0.27) で、どれも小さな
ものの積みかさねだった。直したもの: (1) ユーザーへ戻るたびに signal::deliver が、止められたスレッドグループがないかを
プロセスの表を見て回って探していた (stop_while_stopped)。いまは止まっているグループの数 (STOPPED_GROUPS) が 0 なら探さず、
届けるもの・やり直すもの・戻すマスクがなければ見るだけで戻る (Linux もスレッドごとの印を見るだけ)。(2) /proc/bkl のために
例外ごとに時計 (cntvct) を 2 回読んでいた。QEMU の TCG では 1 回 150 ns ほどで、出入りのほかのものぜんぶにくらべて 4 割になって
いた。いまは待たずに取れたものの 8 回に 1 回ほど (smp::SAMPLE。くじは xorshift: 決まった順だと、交互に呼ぶもの (read と
write など) の片方だけが測られる) だけ測って 8 倍して足す。待ったものはいつも測る。Linux もプロセスの時間は出入りのたびに
測らず、時計の割り込みで数える (tick)。(3) dispatch が TrapFrame の 31 のレジスタを写していた (引数の 6 つだけに)、trace・
strace・oom・fail・unix の gc の「なにもしない」の確かめが関数を呼んでいた (確かめだけを呼ぶ側に入れる)、ESR / FAR を
1 回の例外で 6 回読んでいた (TCG では読むたびに HCR_EL2 を確かめる関数を呼ぶ。ESR を 1 度、FAR はフォルトのときだけ)。
速い道 (syscall::fast) も、まだ速い道かわからないうちに 31 のレジスタを写し、番号ごとの数を atomic で足していた (数えるのは
/proc/sysstat を読みはじめてから、速い道で終わったものだけ)。(4) 自分が代表なのに、代表を表から探していた
(current_leader。getpgid(0) など)。getpgid(0) と getsid(0) は getppid と同じく速い道で答える (Linux も rcu_read_lock だけ)。
lseek の SEEK_SET / SEEK_CUR (と、ページキャッシュが大きさを知っている SEEK_END) と、開いていない fd の close (EBADF) も、
fd の表と開いたファイルをだれとも分けていなければ速い道で (pread と同じ。Linux も分けていなければ f_pos_lock を取らない)。
(5) ext4 の inode の meta (stat、lseek の大きさ) を、使うたびに metas の木 (BTreeMap) から引いていた。いまは ExtInode が
自分の meta を覚え、どれかの inode を書く (write_inode) たびに進む版 (meta_ver) が同じあいだはそれを使う (Linux の struct inode
の i_size などと同じに、開いたファイルの fstat は読むだけ)。(6) 開いているディレクトリからの fstatat が、当たっていても String を
4 つ作って捨てていた (ディレクトリのパスの写し、畳んだパス、鍵、たどったパスの写し)。いまは畳んだパスは使いまわしの入れもの
(vfs.rs の NORM) に作り、覚えは鍵を作らずに引き (PathKeyRef と Borrow)、開いたファイルを借りたまま引いて、答えは inode だけ。
ふつうの stat (lookup_by) も、当たれば鍵を作らない。大きなロックの道はカーネルの中で 2.8 → 0.7 µs、速い道は 0.27 → 0.10 µs。
umask 1.7 µs、getpgid 1.0、close(-1) 1.1、lseek 1.1、fstat 2.3、fstatat の dirfd 6.8〜7.4 (Linux 7.3 は 2.5、1.6、1.6、1.7、2.4、6.8)。

いくつもの CPU で動かすと (bench/smp.md)、大きなロック (smp.rs、Linux 2.0 と同じ 1 つのロック) の中を通るものは 1 つずつしか
動けない。stat は 4 つのプロセスで 1 つのときの 0.81 倍しかまわせず (Linux は 2.11 倍)、時間は Linux の 1.71 倍で、大きなロックを
待った時間は CPU 1.6 個ぶんだった。Linux は RCU-walk で、ロックを取らずにパスをたどり、とちゅうで変わっていれば (d_seq) ロックを
取ってたどりなおす。aios はパスの覚え (vfs.rs の PATHS) の答えを、ロックの外から読める表に写して、同じ考えで答える (statc.rs):
ふつうの道の newfstatat が答えたら、その答えと、それが正しいあいだの印 (通ったディレクトリの版、inode の中身の版、だれでもは
通れないディレクトリの許し、全体の版) を seqlock の表 (AtomicU64 の並び。Rc にも RefCell にもヒープにもさわらない) に書き、
次の newfstatat と statx は速い道 (syscall::fast) で、表にあって印がみないまと同じで、呼んだ人がその許しで通れれば、それを
struct stat か struct statx にして返す (C の stat は newfstatat、aish や coreutils など Rust の std::fs::metadata は statx)。
「ない」(ENOENT) も覚える (シェルの PATH さがし、ライブラリや locale のファイルさがしで、いちばん多い stat。答えるときは
ふつうの道と同じわけ `missing_cached` を /proc/ai/why に残す)。
版は、vfs が名前やディレクトリの属性を変えるたびと、ext4 が inode を書くたび (write_inode。meta の覚えを捨てるところと同じ) に
進み、マウントでみな古くなる。ある答えは ext4 のファイルだけ (tmpfs や /proc は中身が変わっても印が変わらない。「ない」は
パスの覚えにあれば、どのファイルシステムでも)、はじめのマウントの
namespace、chroot なし、fd の表 (cwd) を自分だけで使うプロセス (スレッドがいれば、ほかのスレッドが chdir するかもしれない)、
ユーザーの namespace の外 (uid が読みかわる) だけで、/dev と /proc のパス、..、開いているディレクトリからのもの、`fail` の
決まりがあるときはふつうの道へ。test/statc.c が、変える人と見る人を別の CPU にして (chmod、時刻、大きさ、リンクの数、名前を
変えてもどす、消して作りなおす、リンクの付けかえ、途中のディレクトリの取りかえ、だれでもは通れないディレクトリを root が
覚えてから uid 65534 が引く、止めずに変えつづけるあいだ、chdir、ユーザーの namespace の中と外。stat と statx の両方で) 古い答えが出ないかを確かめる
(Linux でも同じに通る)。stat は 1 つで 4.9 → 2.6 µs、4 つで 6.0 → 2.5 µs (Linux 7.3 は 8.0 と 4.0)、待った時間は 158% → 7%。
VM の中の 1 回: /etc/passwd の stat 9.6 → 2.5 µs、statx 9.3 → 2.3、ないものの stat 7.0 → 2.6。

4 KiB の pwrite は 1 つで 15.0 µs (Linux 7.3 は 7.1)。/proc/ai/prof を細かく分けると、ext4 は書くたびに inode を書いていた:
同じ秒の中で、大きさもブロックの場所も変わらない書きかえでは、inode は 1 バイトも変わらないのに、inode のブロックを
ジャーナルに入れ、meta の覚え (と stat の覚え) を捨て、次の呼び出しが読みなおしていた。Linux は時刻が変わったときだけ
inode を書く (file_update_time)。いまは変わったときだけ書く。ページキャッシュに書いたことを知らせるところ (vm::file_written) も、
書いたページがどれも、もう表にあるファイルシステムのページそのもので版も同じ (その場の書きかえ) なら、表をさわらず
命令キャッシュの印を落とすだけに。pwrite は inotify のために開いたときのパスを写していた (書けたあとに借りる)。
write-4k は 1 つで 15.0 → 9.9 µs、4 つで 24.6 → 16.4 µs。まだ大きなロックの中なので伸びない (残りはユーザーの 4 KiB を
いったんヒープに写すこと、inode を読むこと、ブロックのキャッシュの表)。
umask も大きなロックの中だった (4 つで Linux の 1.55 倍)。umask は fd の表のもの (Linux の fs_struct と同じくスレッドで
分ける) なので、表をだれとも分けていなければ変えるのは自分だけ: 速い道で答える (1 つで 1.6 → 0.9 µs、4 つで 4.5 → 2.3 µs、
Linux 7.3 は 3.0)。

AF_UNIX のソケットも、みな大きなロックの中だった (1 バイトの往復が 4 つで Linux の 1.51 倍)。パイプはもう、眠らずにすむ
read / write を大きなロックなしで動かしている (Pipe::fast_rw と PipeCell)。ソケットも同じ形にした: まずソケットを
Rc<RefCell<Unix>> から Arc<UnixCell> (自分のスピンロック) に (送ったメッセージは送った口を持つので、大きなロックなしでも
その数を変えられるように。借りるのはいつも 1 人になるので、同じソケットを同時に 2 度借りていたところ (名前のない
SO_PASSCRED、listen の行列がいっぱいか、自分につないだ dgram の相手の名前、自分に送る dgram の資格情報) は 1 度ずつに)。
それから、つないだ SOCK_STREAM の、fd も資格情報も MSG_OOB もないバイトの read / write を速い道で (unix::fast_rw): 書く人は
写してから自分の wmem を先に足し、相手の行列に入れる。読む人は自分の行列から写して、送り手の wmem を返す。ロックは
いつも 1 つずつ。眠る人 (stream_recv と wait_wmem が、眠る前にソケットのロックの中で数える)、poll で見ている人
(file::PollWatch)、epoll に入れたもの (mark_epolled) があれば、起こしは大きなロックを持つ CPU に頼む (smp::defer_wake)。
fd のないメッセージは落ちるときに INFLIGHT (gc の数) にさわらない (0 を引くのも、大きなロックなしでは書きもどしになる)。
読んだメッセージを落とすのは相手 (こちらの peer が持っている) のものだけなので、送り手の最後の参照にはならない。
test/unixrace.c が、別の CPU で眠ったり起きたりするとき (1 バイトの往復、16 MiB を書いて書き手が眠る、poll、epoll、
2 つのスレッドが同じソケットに書く) に起こしそこねず中身がそろうかを確かめる (Linux でも同じに通る)。unix は 1 つで
9.6 → 4.3〜5.7 µs、4 つで 13.5 → 4.9 µs (Linux 7.3 は 7.1)。/proc/bkl の「AF_UNIX のロックなしの読み書き」に数。
/proc/bkl の「ロックなしの stat」に、答えた数とふつうの道へ行った数。

手本が Linux 7.3 になったとき (ext4 300 × 30 と 300 × 80、tmpfs 300 × 30 でどれもちがわない) は、手本のまちがい 3 つが
7.3 にもまだあるかを確かめた (`sysdiff -r` は Linux の側でもただしをしないので、-r ありとなしの答えをくらべれば、そのまちがいが
出る並びかが分かる。zero-range-eof はいまの並びではめったに出ず、ブロックにそろった ZERO_RANGE の小さな試しで見た)。

プロセスとシグナルを覚えさせたとき (Linux 7.3 と、ext4 300 と 1000 × 30、300 × 80、tmpfs 300 × 30 と 300 × 80 でどれも
ちがわない): はじめは 300 の case のうち 139 がちがった。いちばん多かったのは、aios が終わってまだ回収されていない子
(ゾンビ) を /proc に出さず、kill も ESRCH にしていたこと (Linux は回収されるまでいる: /proc/PID の stat は Z、kill は 0、
getpgid / getsid / setpgid も見つける)。ほかに、setpgid のしらべる順 (負の pgid は先に EINVAL、ちがうセッションの子は EPERM、
exec したあとの子は EACCES、同じセッションにないグループへは EPERM)、wait4 / waitid の知らない options は EINVAL (waitid の
負の P_PGID も)、WSTOPPED / WCONTINUED だけで待つときはゾンビを待つ相手にしない (ほかにいなければ ECHILD)、kill はまず相手を
さがし (いなければ ESRCH)、それからシグナルの番号 (EINVAL)、tkill / tgkill の 0 以下の番号は EINVAL、待てる子が何人もいれば
古い子から、孤児のプロセスグループ (setsid した子のように、グループの外の同じセッションに親がいない) には SIGTSTP / SIGTTIN /
SIGTTOU の止める既定の動作をしない (SIGSTOP は止める)。長い並びでは、ほかのところのちがいも 3 つ出た: いくつもの入れ物の
NOWAIT の読みで読めなかったときに、aios は最初の入れ物の分しか読んでおかなかった (Linux の先読みは頼まれた全部)、未初期化の
extent や穴も NOWAIT ではキャッシュになければ EAGAIN (Linux のページキャッシュと同じ。読んでおいた 0 のページは fadvise
DONTNEED で外れる)、pipe(2) のパイプは書いても時刻が変わらない (Linux 6.7 から。名前つきの FIFO は変わる)、tmpfs の
ディレクトリの大きさは Linux と同じ 40 + 名前ごとに 20。test/procwait.c が、プロセスの決まり (ゾンビ、知らせの順、止まって
動いた知らせ、SIGCHLD の扱い、孤児のグループ、setpgid、kill の順) をじかに確かめる (Linux 7.3 でも同じ答え)。

ロック (flock と fcntl のレコードロック) を覚えさせたとき (ext4 300 × 30 と 200 × 80、tmpfs 300 × 30 でどれもちがわない): aios の
レコードロックはそれまで仮のもの (かけるのはいつも成功、聞かれたら「だれもかけていない」) だったので、本物にした
(kernel/src/locks.rs。Linux の fs/locks.c と同じ決まり): POSIX のロックの持ち主はプロセス (ファイルの表: スレッドは同じ)、
OFD のロックの持ち主は開いたファイル (dup と fork は同じ持ち主)。同じ持ち主どうしはぶつからず、重なるか隣りあう同じ種類は
1 つにまとめ、種類がちがえば切り分ける。POSIX と OFD は持ち主がちがうので、同じプロセスの中でもぶつかる (F_SETLK は EAGAIN、
F_GETLK はぶつかるロックの種類と場所と pid (OFD は -1) を答える)。そのファイルの fd をプロセスがどれか 1 つ閉じると
(dup2 で上書きしても) POSIX のロックはみな外れ、OFD と flock は最後の参照が閉じたら外れる。F_SETLKW と LOCK_NB なしの
flock は、ほかの持ち主が外すまで眠り、POSIX どうしの行きどまりは EDEADLK。しらべる順も Linux と同じ (F_GETLK は種類が
F_RDLCK / F_WRLCK でなければ先に EINVAL、F_OFD_GETLK は範囲のあとで種類 (F_UNLCK も聞ける) と l_pid、F_SETLK は範囲、
口 (読めない fd に F_RDLCK は EBADF)、OFD の l_pid。flock はだめな op の EINVAL が fd の EBADF より先、O_PATH の fd は
EBADF)。flock はパイプやソケットにもかけられる (パイプは両方の口で 1 つ)。test/rlock.c が、同じプロセスの中と、fork した
子との取り合い (待つ、EDEADLK、子の終わりで外れる) を ext4 と tmpfs でじかに確かめる (Linux でも同じ答え)。/proc/ai/fs の
locks に、かかっているロックと待っている人の数が出る。長い並びで出たもう 1 つのちがいは NOWAIT の読みの先読み:
aios は NOWAIT で読めなかったとき 1 ページしか読んでおかず、ディスクでつながっていない次のページが 2 回めにも読めなかった
(Linux の先読みは読もうとした範囲をみな読む)。読もうとした範囲 (128 KiB まで) のページをみな読んでおくようにした。
ロボットの側も、Linux の 2 回めの読みは先読みのディスクが終わるまで (EAGAIN のあいだ 1 秒まで) 待つようにした
(RWF_DONTCACHE のときは、終わるまで EAGAIN のまま)。

ソケット (AF_UNIX) を覚えさせたとき (Linux 7.3 と、ext4 1500 × 30 と 400 × 80、tmpfs 500 × 30 でどれもちがわない): はじめは
300 の case のうち 259 がちがった (61 種類)。aios の AF_UNIX は SOCK_STREAM だけで、つながるとパイプ 2 本にしていたので、作り
なおした (kernel/src/unix.rs。Linux の net/unix/af_unix.c と同じ考え): どのソケットも受ける行列 (Linux の skb の並び) を持ち、
SOCK_DGRAM (SOCK_RAW もこれ) と SOCK_SEQPACKET はメッセージの区切りを、SOCK_STREAM は MSG_OOB の 1 バイトと、ちがう人の
資格情報をつなげない決まりを保つ。送った大きさは Linux と同じ skb の truesize (中身と kmalloc の箱とページの分。Linux 7.3 で
測った) で送った側に数え、sndbuf をこえたら EAGAIN、4 分の 1 をこえたら POLLOUT なし。dgram は相手の行列が max_dgram_qlen (10)
をこえたら待つ。名前は、パスなら bind でソケットのファイルを作り (許可はソケットの許可 (fchmod で変わる) から umask を引いたもの)、
つなぐ相手はその inode で見つける。抽象名前空間は型ごと (Linux の unix_abstract_hash は型をまぜる)。connect はつながる前の口
(embryo) を listen しているものの行列に入れ、accept の名前はつないできたほう。閉じたあとは Linux と同じ (読まれていないものが
あれば相手に ECONNRESET、listen しているものが accept せずに閉じれば、つないだほうにも)。SCM_RIGHTS と SCM_CREDENTIALS
(自分の pid と uid / gid のどれかだけ。SO_PASSCRED のない送り手の資格情報は pid 0 と overflowuid) は Linux の __scm_send と
scm_recv の順と長さで (MSG_CTRUNC も)、MSG_PEEK と SO_PEEK_OFF、MSG_TRUNC、MSG_WAITALL、SO_RCVLOWAT、SO_RCVTIMEO /
SO_SNDTIMEO、SO_LINGER、SO_PEERCRED (listen と connect と socketpair のときのプロセス)、SIOCINQ / SIOCOUTQ / SIOCATMARK も。
read / readv / write / writev は 1 回の sendmsg / recvmsg (dgram の 1 つのメッセージ)、0 バイトの read は何もせずに 0。
poll / select / epoll は、ソケットとパイプとふつうのファイルについて Linux の f_op->poll と同じビットを見る (unix_poll と
unix_dgram_poll、パイプの pipe_poll: 書く口は読み手がいなければ POLLERR だけ。aios は POLLHUP も出していた)。ほかにも直した
もの: ソケットのシステムコールは O_PATH の fd を EBADF に (ioctl も)、パイプの書く口の FIONREAD、ふつうのファイルの FIONREAD は
終わりより先なら負、getdents のソケットの型 (DT_SOCK)、ソケットの fd への utimensat (pipefs と同じく、ns のしらべと持ち主)、
ソケットからパイプへの splice / sendfile (Linux の sock_splice_read: stream はメッセージごとにパイプの箱を分ける)。
送られている途中のソケットの輪 (自分の fd を自分の行列に入れたものや、たがいの fd を持ちあうもの) は、どの fd の表からもたどれ
なければ片づける (Linux の unix_gc と同じ考え)。aios は、ソケットの fd を閉じたシステムコールの終わりにすぐ見る (Linux は
ほかのソケットが閉じたときに、うしろで)。Linux とちがうところがもう 1 つ: MSG_OOB の 1 バイトを読みとばしたとき
(SO_OOBINLINE なし)、FIONREAD の数からも引く (Linux 7.3 の manage_oob は引かず、FIONREAD が 1 多いままになる)。
test/unixsock.c が、ロボットの見ないプロセスをまたぐところ (待つ read / accept / poll / epoll、SO_RCVTIMEO、MSG_WAITALL、
1 MiB の stream、fd を渡す、SO_PEERCRED、閉じたあと、SEQPACKET の区切り、dgram のいっぱいの行列と POLLOUT、gc) をじかに
確かめる (Linux 7.3 でも同じ答え)。速さは前 (パイプ 2 本) と同じか速い (VM の中で、1 バイトの往復と fd を渡す往復は
同じ、poll しながらの往復は 2 割、4 KiB と 64 KiB の書きこみの流しこみは 3 割速い): 読まれたときに送り手を起こすのは、入れ物が
いっぱいで待っていたときだけ (Linux の sk_write_space)、1 回の読み書きでヒープを使うのは中身の分だけ。手本の Linux の設定 (test/linux.config) には NET と UNIX (AF_UNIX_OOB も) と INET を足した
(設定が変われば test/linux-vm.sh が作りなおす)。

ソケット (AF_INET) を覚えさせたとき (Linux 7.3 と、ext4 1000 × 30、400 × 80、600 × 80、tmpfs 500 × 80 でどれもちがわない):
はじめは 300 の case のうち 90 がちがった。aios の TCP と UDP は smoltcp (外とつなぐ小さな TCP/IP) のソケットで、ループバックも
パケットにして smoltcp に通していたので、作りなおした (kernel/src/inet.rs。Linux の net/ipv4/ の tcp*.c、udp.c、
inet_connection_sock.c と同じ考え)。Linux とちがうところが大事なところ: ループバック (127/8、自分のアドレス、0.0.0.0) は
パケットを作らない。つなぐ (SYN、SYN-ACK、ACK)、送る、FIN、RST は、相手のソケットをじかに呼ぶ (送ったものは相手の受ける
行列にそのまま入る)。だから Linux の遅らせた ACK や Nagle で待つことはなく、loopback の往復は関数を呼ぶだけ。smoltcp は外
(eth0) の相手にだけ使う。表も端から探さない: ポートの表 (ポートごとに bind した順の番号で)、listen しているものだけの表、
時刻の表 (SYN のやりなおしと TIME_WAIT の終わりを時刻の順に。tick は時刻の来たものだけを見る)、fd のないものの表。だから
TIME_WAIT やつながりがいくつあっても、socket、connect、accept、close の手間は変わらない。見えるふるまいは Linux に合わせた: ポートの表と bind の決まり (SO_REUSEADDR / SO_REUSEPORT、
Linux の inet_bind_conflict と udp_lib_lport_inuse)、空いたポート 32768〜60999、1024 より下は root だけ、TIME_WAIT と
閉じたあとの FIN_WAIT2 は 60 秒 (/proc/net/tcp にも出る。/proc/net/sockstat では Linux と同じく tw に数え、閉じて終わって
いないものは orphan)。listen の行列があふれたら SYN を落とし (syncookies なし)、
つなぐほうは SYN_SENT のまま 1、2、4 ... 秒でやりなおす (TCP_SYNCNT の 6 回で ETIMEDOUT)。自分につなぐ、urgent (MSG_OOB と
SIOCATMARK と SO_OOBINLINE)、FIN と RST の状態の移りかた (閉じた相手に送ると RST、shutdown(SHUT_RD) のあとに来たデータでも
RST: RFC 1122)、SO_LINGER 0 の RST、MSG_MORE と TCP_CORK、入れ物の大きさ (Linux 7.3 で測った: つながると SO_SNDBUF は
2626560、TCP_MAXSEG はつないだほうが 32741 で相手から ACK が来ると 32768、SYN を送ったときは 524、IP_MTU は 65535)、accept した
口の IP_MULTICAST_TTL は相手の TTL。UDP は Linux の点数での相手さがし、ICMP の port unreachable (つないだ相手か IP_RECVERR
なら ECONNREFUSED、エラーの行列と MSG_ERRQUEUE)、MSG_MORE、送った大きさの truesize (SO_RCVBUF をこえたら落とす)、
SO_PEEK_OFF、IP_PKTINFO / IP_RECVTOS / IP_RECVTTL の付帯データ。ほかに直したもの: TCP のソケットの mmap (Linux の tcp_mmap:
書く・実行するは EPERM、ページはなく SIGBUS。写しはソケットを持つので、fd を閉じても写しがあるあいだは閉じない)、写したところの
msync(MS_SYNC) は fsync のないもの (ソケット、/dev/fb0、音) なら EINVAL、ソケットからパイプへの sendfile、O_PATH の fd の
poll は POLLNVAL で select は EBADF、AF_UNIX の SO_REUSEPORT。ping のソケット (SOCK_DGRAM の ICMP) は、だれでも使える
(Linux は ping_group_range で決める。aios は gid を見ない): ロボットはこれを使わない。test/inetsock.c が、ロボットの見ない
プロセスをまたぐところ (待つ accept / read / poll / epoll、SO_RCVTIMEO、4 MiB の stream、listen の行列があふれたときの 1 秒後の
やりなおし、ECONNREFUSED、RST、SIGPIPE、TIME_WAIT とポート、/proc/net/tcp、fd を渡した TCP、UDP の待つ recvfrom) をじかに
確かめる (Linux 7.3 でも同じ答え)。手本の Linux の VM は lo を上げる (test/linux-init.c)。

スレッドと futex を覚えさせたとき (Linux 7.3 と、ext4 300 × 30、400 × 40、600 × 60、600 × 80、tmpfs 400 × 60 でどれも
ちがわない): ロボットは case
の中でスレッドを作り (fthread: FUTEX_WAIT、WAIT_BITSET、LOCK_PI、futex_waitv のどれかで待たせる)、case のスレッドから
futex の op (WAKE、WAKE_BITSET、REQUEUE、CMP_REQUEUE、WAKE_OP、WAIT と WAIT_BITSET の時間切れ、LOCK_PI、TRYLOCK_PI、
UNLOCK_PI、語に書く store、まちがった op) をかけ、そのたびに終わったスレッドの答えと 4 つの語の値 (スレッドの番号は me や
tN、PI の印は |WAITERS) を出す (スレッドが眠るか終わるまで待ってから)。はじめは 300 の case のうち 40 がちがった。aios の
futex は、起こすときは数えずに全員起こし (起きすぎても待つ側が確かめなおす)、bitset を見ず、REQUEUE も全員起こし、WAKE_OP
と PI と futex_waitv はなかった (ENOSYS)。作りなおした (kernel/src/futex.rs。Linux の kernel/futex/ と同じ決まり): 待ち
合わせの場所 (key) ごとに来た順の行列を持ち、起こす数はそのとおり (0 や負でも 1 つ)、bitset の重なるものだけ。key は Linux と
同じく 3 つで、PRIVATE はアドレス空間とアドレス、共有の op は自分だけの領域ならアドレス空間とアドレス (PRIVATE のものとは
会わない)、MAP_SHARED の領域ならページの物理アドレス (プロセスをまたぐ)。REQUEUE / CMP_REQUEUE は nr_wake まで起こして
のこりを移し (うしろに並ぶ)、答えは起こした数と移した数の合計 (負の数は EINVAL)。WAKE_OP は語を原子的に変えて (SET ADD OR
ANDN XOR と OPARG_SHIFT)、前の値をくらべた答えで 2 つめの語も起こす。PI のロックは、語が 0 なら自分の番号を入れて取り、自分の
ものなら EDEADLK、ほかのものなら WAITERS の印を立てて持ち主につながり (持ち主がいなければ ESRCH)、UNLOCK_PI は待っている
いちばん前のものにゆずる (語は WAITERS | その番号)。TRYLOCK と時間切れでも印はのこる (Linux と同じ)。futex_waitv はいくつもの
語でまとめて待ち、起こされたものの番号を返す。スレッドが終わるときの CLONE_CHILD_CLEARTID の起こしは共有の op (Linux の
mm_release) で、持っていた PI のロックは待っているものにゆずる。CLOCK_REALTIME の印は WAIT_BITSET と LOCK_PI2 だけ
(Linux 7.3 は FUTEX_WAIT にも ENOSYS)。シグナルで割り込まれたら、時間のない WAIT は SA_RESTART でやり直し、時間のある WAIT
はハンドラが動けば EINTR、LOCK_PI はいつもやり直す (Linux の ERESTARTSYS、ERESTART_RESTARTBLOCK、ERESTARTNOINTR)。待つ
スレッドはみな同じ場所で眠り、起こす側が行列から出したスレッドだけを起こす。ロックなしの速い道 (PRIVATE の WAKE で待つもの
がいない、WAIT で値がちがう) はそのまま (時間、そろっていないアドレス、bitset 0 のときは答えがちがうので、ふつうの道へ)。
長い並びで出たほかのちがいは 2 つ: listen する前に shutdown(SHUT_WR) した口から生まれた子は、印を受けついでつながったところで
FIN を送る (Linux の sk_clone_lock と TCP_SYN_RECV)。もう 1 つは AF_UNIX の、自分の fd を自分の行列に送って閉じたソケット:
aios は閉じたときに片づけ (相手はすぐ EOF)、Linux 7.3 はほかの AF_UNIX のソケットが閉じるまで片づけない (相手はつながった
まま)。aios のほうを、えらんだちがい unix-gc-now にした (下の表)。ロボットの側では、自分もいるプロセスグループに止めるシグナルを送ると case が
止まったまま (アラームも効かない) になるので、送らないようにした。test/futex.c が、ロボットの見ないところ (来た順、PRIVATE と
共有の key、fork をまたぐ MAP_SHARED の futex、相対と時刻の時間切れ、シグナルとやり直し、PI のゆずりあい、REQUEUE で移った
ものを起こす、futex_waitv、pthread_join) をじかに確かめる (Linux 7.3 でも同じ答え)。

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
| unix-gc-now | 送られている途中の AF_UNIX のソケットの輪 (自分の fd を自分の行列に入れて閉じた、たがいの fd を持ちあって閉じた) は、fd を閉じたそのシステムコールの終わりに片づける (相手からはすぐ EOF や ECONNRESET) | ほかの AF_UNIX のソケットが閉じたとき (か、送られている途中のものが多すぎるとき) に、うしろ (kworker) で片づける。それまで輪の中のソケットは生きていて、相手からはつながったまま (読めば EAGAIN、待てばいつまでも) | 答えが、ほかのソケットが閉じたかや、うしろの仕事がいつ走ったかで変わらない。もうだれも使えないソケットに相手が待ちつづけない。ロボットは Linux で、ほかの AF_UNIX のソケットを作って閉じて片づけを起こし、終わるのを待ってまねる |

#### 手本の Linux のまちがい

手本の Linux にまちがい (書いてあるとおりに動かないところ) があれば、aios はそれに合わせない。test/sysdiff.c の
`linux_bugs` に名前とわけを書き (`sysdiff -l` の `linuxbug` の行)、Linux で動かすときはそのあとでただして答えにする
(`[linuxbug 名前]` がつき、aish の `sysdiff` は `linux_bugs` に数える)。Linux が直ったら消す (新しい版で `sysdiff -r` (ただしを
しない) とふつうの答えが同じになれば、そのまちがいはもうない)。test/kernel-json.py が、この表と
そろっているかを見る。

| 名前 | Linux のまちがい | ロボットは Linux で |
|---|---|---|
| zero-range-eof | ext4 の fallocate ZERO_RANGE で、範囲がファイルの終わりをこえ、終わりのブロックがもうディスクにあると、そのブロックの中 (ファイルの終わりまで) が 0 にならない。7.2.11 の ext4_alloc_file_blocks が、大きさをのばすときに ext4_block_zero_eof で終わりのブロックの残りを 0 にするが、それが ZERO_RANGE で落としたばかりのページを読みもどす。7.3 にもある。6.18 にはない (delalloc でまだディスクにないブロックなら 0 になる。ロボットの並びではめったに起きず、ブロックにそろった ZERO_RANGE を fsync と fadvise DONTNEED のあとにすると出る) | ZERO_RANGE がうまくいったら、範囲のうちファイルの終わりまでを PUNCH_HOLE で 0 にする |
| efault-extend | ファイルの終わりより先への write / pwrite / pwritev2 がさわれない入れ物で EFAULT になっても、ファイルがその位置まで伸びる (ext4 も tmpfs も: generic_perform_write が write_begin のあとに写し、0 バイトでも write_end が i_size を pos にする) | 6.18、7.2.11、7.3 | なにも書けなかったのだから伸びない (aios はそう) |
| tmpfs-extend | tmpfs で、写し (mmap) でファイルの終わりの先 (同じページの中) に書いたものが、ファイルを伸ばすと (ftruncate、fallocate、終わりより先への write) そのまま中身になる (shmem_setattr と shmem_write_end はページの終わりの先を 0 にしない。ext4 は pagecache_isize_extended で 0 にする) | 7.2.11 と 7.3 | POSIX は伸ばしたところは 0 と読めると決めている (ftruncate(2)、write(2))。伸ばした op のあとで、前の終わりからそのページの終わり (op が書いたところの手前) までに 0 を書く (aios は ext4 でも tmpfs でも 0) |

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
| `/proc/ai/mem` | メモリの中 (aish の `mem`): RAM の全部と空き (`min_free` を切ると取りもどす。`blocks` は空きの 2^k ページのかたまりの k ごとの数、`hot` はまだまとめていない 1 枚ずつのもの)、カーネルのヒープ (使っている、いちばん多かった、空きのかたまり、取れなかった回数) とスタック、ページキャッシュ (ページの数と上限、読みそこねた数とその時間、捨てた数、ファイルごとのページの数と名前 (パスの覚えにあれば)、多い順)、MAP_SHARED のページ (書いたもの)、tmpfs、スワップ、ページフォルト、メモリを多く使っているプロセス (rss、vsize)。数はページ (4 KiB) かバイト |
| `/proc/ai/fail` | わざとしくじらせる決まり (フォルト注入。aish の `fail`): `ctl` に `fail pid N\|name NAME sys NR\|all ERRNO [TIMES]` で、そのプロセスのシステムコール (番号か名前か、みな) を動かさずに -ERRNO に、`fail ... heap [TIMES]` でカーネルのヒープの alloc を、`fail ... page [TIMES]` で物理ページを取れないことに (TIMES 回。既定 1)。`fail off` でやめる。読むと決まりと、あと何回か (`left`)、効いた数 (`hits`)。プログラムが ENOMEM や EIO にどう耐えるか (musl の起こりの mmap を ENOMEM にすると落ちる、など) と、カーネルの中の「取れなかった」道を確かめる: page は Option で返るので、そのプロセスが out of memory で殺されてカーネルは続く。heap は耐えるところ (read の入れ物など) で: そのシステムコールが ENOMEM。`fail ... oom [TIMES]` は耐えないところで: 予備から渡り、次のシステムコールで oom がいちばん大きなプロセスを殺す (`-snapshot` の VM で。上の「ヒープが足りないとき」) |
| `/proc/ai/parts` | 切れるところ (kernel.json の `off` のあるもの) のいまの様子: `off` (切ってあるか)、`runtime` (動かしたまま変えられるか)。だれでも読める。`ctl` の `part NAME off\|on` で、使うたびに見るもの (システムコールと quirk) は動かしたまま切る・入れる (aish の `part`)。起動のときに起こす装置と aiproc は `aios.off=` だけ |
| `/proc/ai/trace` | カーネルの中の出来事の記録 (aish の `trace`): `ctl` に `trace on [NAME]` で始め (NAME があれば、その名前で始まるプロセスのシステムコールとフォルトだけ。切りかえ・眠り・起こし・割り込み・ディスクはみな)、`trace off` で止め、`trace clear` で空にする。8192 こまでの輪 (古いものから消える)。はじめの行が様子 (`on`、`filter`、`events`、`dropped`、`names`: tid → 名前)、あとは 1 行 1 つ: `t` (起動からの ns)、`c` (CPU)、`p` (tid)、`k`: `sys` (入り。`n` 名前、`a` 1 つめの引数) / `ret` (出。`r` 答え) / `switch` (その CPU が選んだ。`from` 前のもの) / `sleep` (`chan`) / `wake` (`chan`、`by` 起こしたもの) / `irq` / `blk` (`op`、`sector`、`n` セクタ) / `blkdone` (`ns`) / `fault` (`va`、`w`) / `exec` / `exit` (`code`)。読みはじめで 1 度作り、続きはそれを切って返す |
| `/proc/ai/why` | しくじったわけ (kernel/src/why.rs。aios だけのもの): システムコールが errno を返したとき、Linux と同じ errno のほかに、なぜそうなったかを残す (errno もふるまいも変えない)。パスのどこで止まったか (`missing`: そこまではあって、その名前がない。`not_dir`、`loop`、`name_too_long`、パスの覚えにあった「ない」は `missing_cached`)、どの許しが足りなかったか (`perm`: ほしかった r/w/x、そのものの mode と持ち主、呼んだ人の uid と gid、答えを決めた組 (持ち主・グループ・ほか))、seccomp、landlock (足りなかった権利の名前)、`fail` でわざと、切ったところ、aios にないシステムコール。わけを書いたところがパスを知らなければ (open の最後のファイルの許しなど)、プログラムがたのんだパス (`asked`)。1 行 1 つで新しい順: `pid`、`comm`、`syscall`、`errno`、`code`、`path`、`name`、`why` (人と AI が読む 1 文)。ほんとうのしくじりと、さがしもの (`probe`: stat のなかまの ENOENT と ENOTDIR、ioctl の ENOTTY。シェルの PATH さがし、aish の色づけ、locale のファイル) は別の輪 (256 こずつ) に。EAGAIN、EINTR、ETIMEDOUT は入れない。だれでも読めるが、root と wheel でない人には自分のものだけ。aish は対話でコマンドが 0 でなく終わると (シグナルのときはのぞく)、そのジョブのわけを 1 行出し (コマンドに書いたパスにかかわるものを先に)、`why` でみな (`why -a` は残っているものみな) |
| `/proc/ai/fs` | ファイルシステムの中 (aish の `fs`): マウントごと (ext4: ブロックキャッシュの数と上限、まだ書いていないメタデータと中身、当たり・外れ・まとめ読み・捨てた数、書き出した回数と書いたブロック、ジャーナルの大きさと次のトランザクション、inode と名前の覚え、使っている inode、片づけ待ち、空き。tmpfs: ページ)、パスの覚え (数、ないもの、上限、当たり・外れ、捨てた回数、変わったディレクトリの数)、開いているファイルの数、区画ごとのディスクの読み書き (回数、セクタ、ms) |
| `/proc/ai/prof` | カーネルの中の区間 (`part:what`。part は kernel.json の名前) ごとの回数と時間 (ctl に `prof on` から `prof off` まで): まず part ごとの `self_us` (中の区間をのぞいた、そこだけの時間) と割合 (`pct`。いちばん多いものがボトルネック)、つぎに区間ごとの `count` `total_us` `self_us` `avg_us`。いまはファイルまわり (sysfile、vfs、extfs、vm、block、jbd2) |
| `/proc/ai/ctl` | 書くスイッチ (root:wheel の 0664): `kick PID` (futex で眠っているスレッドをみな起こす。起こしが消えたのか、だれも起こしていないのかを見分ける)、`raw TID` (レジスタとスタックを kmsg に)、`pcmiss on\|off`、`strace NAME`、`bkl reset`、`heap trace on\|off`、`heap check` (ヒープの決まりを確かめて kmsg に)、`heap hog N`、`page cycle` (空きの物理ページをみな取って返す)、`page check` (物理ページの空きの決まりを確かめて kmsg に)、`prof on\|off`、`lockbench` (ロックなどの 1 回の時間を kmsg に) |

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
