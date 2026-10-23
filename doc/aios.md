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

### だれが守るか (kernel/src/cell.rs)

カーネルは大きなロック (BKL) 1 つで守ってきたので、表の多くは `static mut` だった。`static mut` には Rust のコンパイラが
何も言わないので、BKL を外した道 (syscall::fast、ロックなしのフォールト、ロックなしの作る・消す) がうっかり触っても気づけ
ない (2 つの CPU が同時に BTreeMap を書けば、そこで壊れる)。そこで、だれが守るかを型に書く:

| 入れもの | 守り手 | 中を出すとき |
|---|---|---|
| `Bkl<T>` | 大きなロック | 呼んだ CPU が BKL を持っているかを見る。持っていなければ、呼んだ場所 (ファイル:行) を 1 度 kmsg に出し、`/proc/bkl` に `unlocked FILE:LINE 回数` で数える (機械は止めない) |
| `Once<T>` | 1 度だけ入れる | 入ったあとは、どの CPU もロックなしで読む (ルートの ext4 など) |
| `Early<T>` | 起動のとき (ほかの CPU を起こす前) だけ書く | 読むのはロックなし (DTB の写し、UART・割り込みの番地、RAM の上限、ディスクの区画、vDSO のページ)。ほかの CPU が動いてから書けば、呼んだ場所を知らせる |
| `PerCpu<T>` | CPU ごとに 1 つ | 自分の CPU の分だけを、割り込みを止めて (ヒープの置き場、まとめて作った乱数、いまのスレッド、スケジューラの文脈)。割り込みが止まっているかは触るたびには見ない (QEMU では DAIF を読むのが重く、システムコールが 2 割遅くなった)。かわりに `test/kernel-json.py` が、割り込みを開けるのはスケジューラが眠るところと SpinLock の戻しだけであることを見る |
| `Raw<T>` | 番地を渡した先のきまり | Rust からは参照を作らず、番地だけ (ヒープの区画、カーネルのスタック、プロセスの表) |
| `SpinLock<T>` | 自分のロック | kmsg の輪、パスの覚えの版の表、ヒープの「どこで取ったか」の表 |

`test/vm-check.sh` の `bkl-unlocked` と CI の VM test (C のテストとロボットのあと) は `/proc/bkl` に unlocked があれば落ちる。
`echo "bkl selftest" > /proc/ai/ctl` は BKL をいったん放して `Bkl` を触り、見はりが気づくかを確かめる (`bkl: selftest ok`)。
AF_UNIX と AF_INET の表、ファイルのロック、inotify、timerfd、tty、入力、futex、vfs (マウント、パスの覚え)、ファイルの
共有ページ、swap、ドライバ (gpu、net、sound、sd、virtio-blk)、ホスト名など 59 を `Bkl` にし、のこりも上の入れものか
atomic (ディスクの読み書きの数、tmpfs のページの数、CPU 間の割り込みの宛先) にした。vm-check、C のテスト、ロボットの
800 case、smpbench (4 つの CPU) で 1 度も unlocked は出なかった。`static mut` は 145 から 1 つ (boot.rs のアセンブリが
持つページ表 `boot_l1_hi`。extern の中なので型を変えられない) になり、`test/kernel-json.py` が数を見て
(`STATIC_MUT_MAX`)、増えれば落ちる。kmsg の輪は、はじめの値をみな 0 にして .data から .bss へ (カーネルの写しが 256 KiB 小さく)。
Linux は、どのデータをどのロックが守るかをコメントに書き、lockdep は実行時にロックの順を見るだけ。aios は型に書くので、
守り手のない道は、動かせば分かる。

### ディスクを待つ眠り (ps の D)

キャッシュにない ext4 のファイルを読むとき、virtio-blk の要求の終わりを眠って待つ (大きなロックをほかの CPU に渡す)。
その眠りが、シグナルの来たプロセスでは EINTR ですぐ戻り、大きなロックを持ったまま回りつづけていた。要求の終わりを
受ける割り込みもタイマも大きなロックを待つので、だれも終わりを見とどけず、`cat` の読むとちゅうに Ctrl-C しただけで
機械ごと止まった。いまはシグナルで起きない眠り (`proc::sleep_nointr`。Linux の TASK_UNINTERRUPTIBLE) で待ち、
シグナルはシステムコールから戻るときに受ける。そのあいだは `/proc/PID/stat` と `ps` で `D` (disk sleep)。
`test/vm-check.sh` の `disk-read-signal` は、まだ読んでいない大きなファイルを読むとちゅうで止める (前のカーネルでは止まる)。

### 眠って書く書き出し (fsync と sync のあいだも、ほかは動く)

前は ext4 の書き出し (ジャーナルへのコミットと、もとの場所への書きこみ) を、大きなロックを持ったまま virtio-blk の
終わりを回って待っていた。ディスクがおそい (HDD) と、`fsync` 1 回のあいだ、ほかの CPU のシステムコールもみんな止まる。
いまは眠ってよいところ (`fsync` / `fdatasync` / `syncfs` / `sync` / `RWF_SYNC` / `RWF_DSYNC` / reboot。システムコールの
いちばん上だけ: `proc::flush_sleepable`) からの書き出しは `ExtFs::flush_sleeping` で、書くものを先に写してから、大きなロックを
はなして眠って書く (`block::write_wait`)。そのあいだもほかのプロセスは同じファイルシステムを読み書きできる。決まりは:

- **同じときの写し**: 書くデータのページと、ジャーナルに入れるメタ (inode、ディレクトリ、ビットマップ、グループ記述子、
  スーパーブロック) を同じときに写す。データが先、そのあとコミット (data=ordered と同じ: こわれても古いデータが見えない)。
  書いているあいだに変わったものは、汚れたままにして次の書き出しで書く。
- **書いているブロックは追い出さない** (`writeback`)。そのあいだに消されたページは、書き終わるまで放すのを待つ (`wb_held`)。
  ページの参照数は増やさない (vm.rs は参照数で共有ページを放すかを決めるので、増やすと inode が放されず orphan がのこった)。
- **スーパーブロックの recover の印** は、いまのものではなく、最後にコミットしたスーパーブロックの写し (`sb_disk`) に立てて
  書く (いまのものはまだコミットしていない orphan の頭を持っていて、e2fsck が「orphan のつながりがこわれている」と言った)。
- **書き出しは 1 つずつ** (`flushing`)。眠ってよいところは前のが終わるのを待ち、眠れないところ (メモリが足りないとき、
  ほかの道からの `maybe_flush`) は飛ばして次にまかせる (`/proc/ai/fs` の `skipped_flushes`。眠ったものは `slept_flushes`)。
  入れ子の書き出し (`vm::sync_shared` の中の write から) は眠らない。
- reboot は、眠って書いたあとにもう一度、眠らずに全部書く (書いているあいだに汚れたものを、電源を切る前にのこさない)。
- **放したブロックは、放したことがコミットされるまで取らない** (`Cache::freed`。眠って書いている写しの前に放したものは
  `freed_wb` で、そのコミットのあと。Linux の ext4 と同じ)。書き出しはファイルの中身をコミットより先に書くので、extent の
  葉を放してすぐ別のファイルの中身に使うと、コミットの前に電源が切れたとき、ディスクではまだそれを葉として指している
  inode に、ほかのファイルの中身が入る (e2fsck: invalid extent node)。前のカーネルの書き出しも同じ順 (その 3 回では
  出なかった) で、眠って書くようになって、書き出しのあいだに消す・使いなおすが増えて見えた。ほかに空きがないときだけ、
  放したばかりのものも取る。

`test/flushrace.c` (`vm-ctests.sh` に入っている) は、書くもの 4 つ (作る・のばす・切りつめる・書きなおす・消す・名前の入れかえ・
MAP_SHARED) と `sync` / `syncfs` / `fsync` / `fdatasync` を回すもの 2 つを同時に動かし、最後に中身を確かめて目録をのこす
(`-v` で起こしなおしたあとに確かめる)。30 秒で眠った書き出しが 8000 ほど、中身のちがいは 0。電源を抜くのは
`test/powercut.sh [KERNEL] [N]`: おそくしたディスク (`throttling.iops-write=200`) で flushrace を回しているとちゅうに
QEMU を SIGKILL して (`test/vm.py stop` は `poweroff` するので、電源を抜くことにならない)、外から e2fsck -fy。
放したブロックの決まりの前は 6 回のうち 4 回 invalid extent node、いまは 12 回ともこわれていない (ジャーナルを書く
とちゅうで切れたものは、再生してから)。
`test/fsyncstall.c` は、`fsync` を回すあいだに別のプロセスのシステムコール (大きなロックを取る `uname`) が何 ms
止まったかを測る。おそいディスクのまねは `AIOS_DISK_OPTS=throttling.iops-write=N bin/run.sh` (QEMU のドライブの引数に足す)。
1 秒に 50 回書けるディスクで 10 秒: 前は `uname` が 109 回しか通らなかった (いちばん長く止まったのは 374 ms) のが、
いまは 237 万回 (いちばん長くて 8 ms。`fsync` を回さないときは 257 万回)。`fsync` の数はほぼ同じ (58 と 56)。

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
ページをそのまま使う (adopt_hole) ので、穴に write() しても写しに映る。0 のページを作るのは写すときだけで、read() で穴を
読むときは 0 を写すだけ (Linux と同じ。0 のページは inode を持ち、写すのをやめるときに表から外れるので、read() で作ると
だれも外さず、消したファイルが orphan のまま残った: sysdiff-ci のあと 28 個。test/orphan.c)。切りつめたところと穴をあけたところは、写しているところ
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

4 KiB の pwrite も大きなロックの中だった (4 つで Linux の 3.70 倍)。Linux はページキャッシュのページに写して、書き出す印
(PG_dirty) をつけるだけで、ロックはそのファイルの i_rwsem。aios の ext4 は、ブロックが 1 ページなら、ページキャッシュ
(vm.rs の PRIV) のページがブロックのキャッシュのページそのもので、時刻は秒までなので、同じ秒の中で大きさを変えない
書きかえは inode を 1 バイトも変えない (上の段)。そこで、ext4 が大きなロックの中で書いたあと (write_at)、そうなる inode
(setuid / setgid なし、IN_MODIFY の見張りなし、RWF_DONTCACHE でない) なら、その秒の「許し」(vm::WritePermit: 秒、
statc の inode の箱の版、全体の版) をページキャッシュの表に置く。次の pwrite64 は速い道で (file::fast_pwrite と
vm::write_fast)、ユーザーのところがみな写っていて、許しがいまの秒で新しく、大きさの中で、ページがみな表にある
ファイルシステムのページそのものなら、表のロックの中でユーザーから直接そのページに写し、ページを「書いた」列に入れる
(参照をひとつ持つ: 捨てられないように)。ext4 は書き出す前 (maybe_flush と flush。fsync、sync、暇なとき、再起動も) に列を
受けとり、いまもそのブロックのキャッシュのページそのものなら、中身のブロックとして書き出す印 (data) をつける。
印は書き出す前に落とすので、書き出しのあいだに書いたものは、また列に入って次に書かれる。列が長いか 5 秒より古ければ
ふつうの道へ (そこで書き出す)。ext4 がそのファイルを変えるとき (write_op の入り口。書く、切りつめる、chmod、時刻、
swapon) は許しを捨てる: 表のロックを取るので、いまロックなしで書いているものは終わっていて、そのあとはふつうの道
(大きなロックで 1 つずつ) を通るので、同じところへの書きこみはまざらない (Linux も i_rwsem で 1 つずつ)。
IN_MODIFY の見張りを置くと、みなの許しが古くなる (できごとはふつうの道が出す)。O_DSYNC / O_DIRECT、fd の表を
分けたスレッドの表を変えている途中、`fail` の決まりがあるときもふつうの道へ。test/pwfast.c が、でたらめな場所と長さの
2 万回の書きかえ、秒が変わったら時刻が進むこと、setuid の印が落ちること、inotify、MAP_SHARED、2 つのスレッドが同じ
ところ (ページをまたぐ) に書きあうあいだに終わりを伸ばすもの、4 つのプロセス、半分写っていないバッファ、fsync で
書き出されること (/proc/ai/fs の数) を確かめる (Linux でも同じに通る)。再起動のあとも中身が残ることも確かめた。
write-4k は 1 つで 26.9 → 6.6 µs、4 つで 34.8 → 5.6 µs (同じホストで、前のカーネルとくらべて)、大きなロックを待った時間は
161% → 7%。記録 (bench/smp.md) では 4 つで Linux 7.3 の 1.02 倍 (前は 3.70 倍)。
/proc/bkl の「ファイルのロックなしの pwrite」に、書いた数とふつうの道へ行ったわけ。

open と close も大きなロックの中だった (open-close が 4 つで Linux の 1.86 倍)。Linux は RCU-walk でパスをたどり、
struct file を作る。aios で open を大きなロックの外に出せないのは、開いたファイル (OpenFile) が inode を Rc で持つから:
作れば inode の参照の数が変わり、その数はほかの CPU が大きなロックの中で変えている。そこで aios は、作らずに使いまわす
(Linux にはないしかけ。file::Shelf、閉じたファイルの棚): 読むだけの口で (O_RDONLY と O_CLOEXEC、O_LARGEFILE、
O_NOCTTY だけで) 開いた ext4 のふつうのファイルを閉じるとき、自分だけが持っていて (dup も fork も epoll もしていない)、
flock / OFD / POSIX のロックを置いたことがなく、IN_OPEN / IN_CLOSE の見張りもなければ、OpenFile を落とさずに fd の表の
棚 (8 つまで) にしまう (大きなロックなしの close)。同じパスを同じ口でまた開くとき (大きなロックなしの openat)、statc の
表が、そのパスはいまも同じ inode をさしていて (途中のディレクトリの版、inode の版、マウントの版)、呼んだ人が途中を通れて
読めると答え、棚に同じパスと inode と口のものがあれば、棚から出して位置を 0 にし、いちばん小さい空いた fd に置く。
どちらも Rc の数は変えず、動かすだけ。表に置くのも棚から出すのも、表ごとのスピンロックの中で。ふつうの openat は答えを
statc に覚えさせ (newfstatat と同じく、「ない」も)、表が「ない」と答える openat は大きなロックなしで ENOENT (ライブラリや
モジュールさがしで多い)。棚のものを捨てるのは大きなロックの中だけ: ファイルを消してリンクが 0 になるとき (extfs の
unlink_node。棚から捨て、いま開いているものにも、閉じたらしまわない印を: 消したファイルの場所がすぐ返るように)、
umount、IN_OPEN / IN_CLOSE の見張りを置くとき (それからはふつうの道)、exec と exit (表といっしょに)、棚がいっぱいの
ふつうの close (いちばん古いものを)。test/opencache.c が、開きなおしの位置と fd と O_CLOEXEC、棚にあるファイルを
消したらディスクの空きが戻ること、rename での上書き、chmod 000、inotify のできごとの数、flock と POSIX のロックが閉じると
外れること、dup、fork、ないパス、/proc/self/fd、4 つのプロセスが読むあいだに rename で入れかえつづけること (読んだ版が
古くならない) を確かめる (Linux でも同じに通る)。open-close は 1 つで 11.7 → 7.2 µs、4 つで 14.2 → 7.9 µs、記録
(bench/smp.md) では 4 つで Linux 7.3 の 0.75 倍 (前は 1.86 倍)、大きなロックを待った時間は 174% → 5%。
/proc/bkl の「ロックなしの open / close」に数。
/proc/bkl の「ロックなしの stat」に、答えた数とふつうの道へ行った数。

ファイルを作って消す (create-unlink: O_CREAT | O_EXCL の openat、close、unlinkat) は、まだ大きなロックの中。外に出す前に、
答えは Linux と同じまま、ロックを持つ時間を短くした。はかると、1 回のうち openat と unlinkat がそれぞれ 130〜150 µs で、
ディスクは待っていなかった (書き出しは 4.5 秒に 3 回)。重かったのは、同じことを何度もしていたところ:
ディレクトリのブロックのチェックサム (metadata_csum の 4 KiB の crc32c、TCG で 14 µs) は、書きかえるたびに計算せず、
ブロックに「まだ」の印 (Cache の dir_csum) をつけておき、ジャーナルやディスクへ写す直前 (jbd2 の commit と write_blocks) に
1 回だけ計算する (Linux は書きかえるたびに計算する: ext4_handle_dirty_dirblock)。写すところごとに計算するので、書き出しの
あいだに眠ってほかが書きかえても、ずれない。ほかのものになったブロック (htree の索引、放したブロック) からは印を外す。
作ったものの持ち主 (euid と egid、setgid のディレクトリの下ならそのグループと、ディレクトリなら setgid) は、作るときの
inode に書く (Linux の inode_init_owner。前は作ったあとに chown と同じ道で inode をもう一度書いていた)。O_CREAT の openat は
親までを 1 度だけたどる (前は 2 度)。名前を足すときは、もうあるかと入るすき間を 1 度でなめ (前は、あるかを 2 度、すき間を
1 度)、消すときは見つけた場所をそのまま消し (前は 2 度なめていた)、ディレクトリの inode は読みなおさずに 1 度だけ書く。
unlinkat は、消すものの覚え (パスの覚え、dcache) を消す前に捨てる: 覚えが inode を持っていると、ext4 は消したファイルを
まだ使われているものとして orphan のリストに入れ、覚えを捨てたときに片づけていた (inode とスーパーブロックを 2 度ずつ
書く)。Linux の d_delete も、dentry を使っているのが自分だけなら、その場で inode を手放す。消すパスで引けない覚え
(名前を変えたディレクトリの下の古いパス、RENAME_EXCHANGE で入れかえる前の名前) も、ext4 がリンクを 0 にするところで
inode の id から捨てる (vfs::forget_ino。前は残って、消したファイルが orphan のままだった。test/orphan.c)。閉じたファイルの棚は、消す
inode が棚にありそうなときだけなめる (inode の id のハッシュで 256 の箱に分けた数。前は消すたびに、みなのプロセスの fd を
見ていた)。inode とブロックのビットマップは 64 ビットずつ探す (Linux の find_next_zero_bit。前は 1 ビットずつ)。
create-unlink は 1 つで 235 → 126〜166 µs、4 つで 430 → 196〜235 µs (同じホストで、ぶれるので何回か)。記録 (bench/smp.md)
では 1 つで 290 → 148 µs (Linux 7.3 は 522 µs)、4 つで 369 → 209 µs、4 つで Linux 7.3 の 1.47 倍 (前は 3.22 倍)。4 つのとき
大きなロックを持っている時間は openat 1 回 88 µs、unlinkat 89 µs で、ロックはいつもだれかが持っている (92%)。Linux 7.3 は
4 つで 1 つの 3.7 倍伸びるので、ここから先は、作る・消すを大きなロックの外に出すしかない。ディスクのコピーで、作る・消す、600 の名前の
ディレクトリ (htree になる)、mkdir と rmdir、setgid のディレクトリをしたあと、外から e2fsck -fn できれいなことも確かめた。
はかるとき: /proc/ai/prof の区間は、TCG では 1 つ 10 µs ほどかかる (区間を 12 足すと、create-unlink の 1 回が 120 µs
増えた) ので、細かく区切るほど数がふくらむ。速さは prof を切ってくらべる。

それから、作る・消すを大きなロックの外に出した (kernel/src/fastdir.rs)。Linux は、ちがうディレクトリなら同時に動く
(ディレクトリごとの i_rwsem と、グループごとのロック)。aios はまず、ext4 の中を ext4 のロック (extfs.rs の fs_lock) で
1 つずつにした: ext4 に入るところ (Inode のメソッド、ExtInode を捨てるところ、ExtFs の外から呼ぶ関数) はみなこのロックを
取る。入れ子にでき (持ち主はスレッド)、眠るとき (proc::sched) は放して、起きたら取りなおす (大きなロックと同じ)。順は
大きなロック → ext4 のロック → fd の表のスピンロック。ExtFs の Rc の数も、このロックの中でだけ変える。O_CREAT の openat と
unlinkat は、大きなロックを取らずに (syscall::fast)、親のディレクトリを statc の表で引き (ふつうの道の O_CREAT の openat と
unlinkat が statc::learn_dir で覚えさせる)、ext4 のロックの中で ExtFs::fast_create / fast_unlink を呼ぶ。ふつうの道と同じ
書きかたで、書きかえる前にみな確かめる (読むブロックがみなキャッシュにある、名前がある / ない、すき間がある、消すものが
使われていない、書けて通れる)。どれかがだめなら何も変えずにふつうの道へ行き、答え (しくじりのわけも) はふつうの道が出す
ので、答えは Linux と同じ。名前が変わったことは、書きかえたあとに vfs::name_changed で知らせ (パスの覚え、getdents、statc の
版の順)、statc の表に書くふつうの道は、statc の版を読んでからパスの覚えがいまも正しいかを確かめる (vfs::fast_stat_trail)。
fd は大きなロックなしの open と同じく、表ごとのスピンロックの中で置く。ふつうの道へ行くもの: 作る・消すのできごとを待つ
inotify の見張りが置かれたことがある、砂場 (landlock)、読むだけのマウントがある、htree のディレクトリ、.. のあるパスや
/dev と /proc、開いたディレクトリからのもの、O_CREAT だけでもうあるもの、ないものを消す、ディレクトリ、開いているもの。
見張りと読むだけのマウントは、印を立ててから ext4 のロックを取りなおし、いま書きかえているものが終わるのを待つ。
statc の表のハッシュも直した: FNV-1a を右にずらして使っていたので、最後の 1 文字だけがちがう名前 (w0、w1 ...) が同じ
箱に入り、たがいに追い出していた (最後にまぜる)。
create-unlink は 1 つで 148 → 45〜47 µs、4 つで 209 → 57〜62 µs (同じホストで)。記録 (bench/smp.md) では 1 つで
148 → 64.8 µs (Linux 7.3 は 444 µs)、4 つで 209 → 61.8 µs、4 つで Linux 7.3 の 0.57 倍 (前は 1.47 倍)、大きなロックを待った
時間は 237% → 20%。close はまだ大きなロックの中で、ExtInode を
捨てるところで ext4 のロックを待つ。4 つが同じディレクトリでも、ちがうディレクトリでも、ext4 の中は 1 つずつなので、ここから
先は ext4 のロックを細かくする (グループごと、ディレクトリごと)。test/createrace.c (いくつもの CPU で、同じディレクトリと
ちがうディレクトリに作って消し、読む人が名前を数える。許し、setgid、大きいディレクトリ、開いたまま消す、ハードリンク、
inotify、覚えた親の statx の mask) は Linux でも aios でも同じ答えで、したあとのディスクのコピーは e2fsck -fn できれい。
/proc/bkl の「ロックなしの作る・消す」に、作った数、消した数、ふつうの道へ行ったわけごとの数、ext4 のロックを待った時間。

4 つで作る・消すと、ext4 のロックの中の仕事が 1 つのときの 1.6〜2 倍かかっていた。しらべると、QEMU (TCG) では時計
(cntvct) を読むたびに QEMU の大きなロック (BQL) を取るので、1 回 110 ns の読みが、2 つの CPU で同時に読むと 580 ns、
4 つで 1.1 µs になり、読むところで 1 つずつになる (ユーザーの中で回るだけのものが 3 つあっても、ほかは 2 割ほどしか
遅くならない。待つ CPU の wfe も関係なかった)。作る・消すの 1 回は、ext4 のロックの中で時計を 7 度読んでいた (inode の
時刻 5 つ、書き出すころかの 2 つ)。Linux はファイルの時刻に、tick のときに覚えた時刻を使う (current_time。時計は読まない)
ので、aios も粗い時計 (timer.rs: どの CPU でも tick のたびに覚える) を、ext4 の時刻 (秒まで)、パイプとソケットの時刻、
ext4 の書き出すころかに使う。細かい時計でつけた時刻 (utimensat の NOW、ftruncate、tmpfs) は床に覚え、粗い時刻はそれより
前にならない (Linux の multigrain の floor と同じ: ちがう道でつけた時刻が逆にならない)。ロックを待った時間 (大きなロック、
ext4 のロック) とページフォルトの時間も、待つたび・フォルトのたびに時計を 2 度読んでいた (取ったあとの 1 度は、ロックを
持ったまま) ので、くじ (SAMPLE: 8 回に 1 回) にあたったものだけ測って 8 倍する (待った回数はいつも数える)。
create-unlink は 1 つで 72 → 55 µs、4 つで 64 → 61 µs ほど (同じホストで 3 回ずつかわりばんこに、中央値)。

それでも 4 つのとき、作る・消すは ext4 のロックで 1 つずつだった (1 回のうち、ロックの中が 40 µs ほど)。そこで ext4 のロックを
読み書きロックにした: ふつうの道 (大きなロックの中) はいままでどおりひとりで持ち、大きなロックなしの作る・消す (fastdir) と、
ExtInode を落とすところ (数を減らすだけのとき: close) はいっしょに持つ。いっしょに持つものどうしは、ディレクトリごとのロック
(64 本: Linux の i_rwsem)、inode を取る・返すロック (ALLOC: ビットマップ、グループディスクリプタとスーパーブロックの数、
Linux のグループごとのロック)、帳簿のロック (BOOK、META_BOOK、NAME_BOOK: キャッシュの書いた印、ExtInode の数、meta() の覚え、
名前の覚え) で分ける。ExtFs の RefCell はみな Shared (同じ使いかたで、いっしょに持っているときだけ帳簿のロックを取る) に、
Cell は atomic にしたので、ふつうの道のコードはそのまま。ひとりで持とうとするものは、いっしょに持つものがみな出るのを待つ。
はじめは帳簿のロックが 1 本で、1 回に 71 度取って 1 度に 4.7 回待っていた (ディレクトリのロックは 1 度も待たなかった)。
いっしょに持っているあいだはブロックのキャッシュの表に足したり捨てたりしない (足すのはディスクを読むとき、捨てるのは
ブロックを返すときで、どちらもひとりで持つときだけ: いっしょに持つ消す道は、リンクが 1 つで中身のブロックのないものだけ。
ほかはひとりで持ちなおして消す) ので、表は帳簿なしで引く。グループディスクリプタの場所の欄もマウントしてから変わらないので
帳簿なしで読み、書き出すころかも数だけ読む。ロックは順 (rank) の小さいものから取り、まちがえると panic。新しい inode の
generation の乱数は、CPU ごとにまとめて作る (Linux の get_random_u32: いままでは 1 つごとに鍵を進めて ChaCha20 のブロックを
作り、時計も読んでいた)。create-unlink は 4 つで 55〜58 → 30〜34 µs (同じホストで、かわりばんこに)。1 つのときは、帳簿の
ロックを取るぶん (1 回に 40 度ほど) 少し重い。記録 (bench/smp.md) では 4 つで 61.8 → 34.5 µs、Linux 7.3 の 0.57 倍 → 0.28 倍、
伸びは 1.05 倍 → 1.89 倍 (Linux は 4.08 倍)、大きなロックを待った時間は 20% → 6% (close が持つ時間は 27.2 → 6.5 µs)。

4 つのとき Linux 7.3 より遅いのは write-4k だけになった (1.19 倍)。大きなロックなしの pwrite (vm::write_fast) は、1 回に
時計を 2 度読み (許しの秒と、書いたページの列の古さ)、4 KiB を写すあいだページキャッシュぜんたいのロック (PRIV) を持って
いたので、ちがうファイルでも 1 つずつだった。秒は粗い時計で読む (ext4 の時刻も粗い時計なので、同じ秒ならふつうの道が書いても
時刻は変わらず、答えは同じ)。写すあいだは、ファイルごとの書くロック (64 本: Linux の i_rwsem。同じファイルへの書きこみは
まざらない) だけを持ち、PRIV は許しと大きさを確かめて、写すページの参照を取るあいだだけ持つ (表から外されても捨てられない)。
ファイルシステムが書きはじめるとき (write_permit_drop)、ページを捨てるとき (file_changed)、IN_MODIFY の見張りが置かれた
とき (write_permits_revoke) は、そのファイルの (みなの) 書くロックを取って、写している途中のものが終わるのを待つ。書いた
印は写したあとにつける: ページがもうみな書いたページの列にあり、写すあいだにファイルシステムが列を受けとらなければ
(TAKEN が同じ)、PRIV を取りなおさない。ほかの道 (file_page) がページを表から外していても列には入れる (take_written は
そのまま渡し、まだブロックのキャッシュのページなら書き出す印がつく)。write-4k は 4 つで 4.5〜4.9 → 2.8〜2.9 µs (1 つは
4.4〜5.0 µs のまま。同じホストで、かわりばんこに)。記録 (bench/smp.md) では 4 つで 4.5 → 2.5 µs、Linux 7.3 の 1.19 倍 → 0.64 倍、
伸びは 1.04 倍 → 1.83 倍 (Linux は 2.39 倍)。これで 4 つのときに Linux 7.3 よりはっきり遅いものはなくなった (futex-wake、pipe、
stat の 4 つは 1.01〜1.13 倍で、はっきりしたちがいなし: どれも、いちばん軽い getppid でも 4 つで 1 つより遅くなるところ
(QEMU の TCG の中で、システムコールが 1 つずつになるらしい) に、Linux といっしょにはりついている)。はじめは印をつけるたびに PRIV を取りなおしてページごとに表を引き、
1 つで 2 µs 遅くなっていた。

ファイルを写したところのページフォルト (Firefox の起動で 6 万回、大きなロックを持つ時間の 2.5 秒) をしらべると、大きなロック
なしの道 (fast_fault) のほうが、ロックの道より遅かった (MAP_PRIVATE で 1 ページ 40 µs)。PTE を書くたびに、すべての CPU の
TLB を消していた (tlbi …is) が、QEMU の TCG ではこれが全部の CPU を止めて合わせるので重い。arm64 では、無効な PTE と
アクセスフラグの落ちた PTE は TLB に入らないので、新しいページを入れるときは消さなくてよい (Linux も消さない)。消すのは
TLB に入っているかもしれない PTE を変えたときだけにし (flush_changed)、ロックなしの道でも、ほかの CPU がそのページ表を
載せていなければこの CPU の分だけにする (flush_va_unlocked: PTE を書いてから ACTIVE_ROOT を見る。載せる CPU は ACTIVE_ROOT を
書いてから TLB を消して表を読む)。40 → 4 µs。それから Linux と同じく、ファイルを読むフォルトでは、まわり (64 KiB のまど:
fault_around_bytes) のページも、ページキャッシュかブロックのキャッシュにあればいっしょに写す (ディスクは読まない。ファイルの
終わりより先と、.bss の始まりのページは写さない)。ロックなしの道は、まどのページをページキャッシュの木の 1 回の range で
引く (ページごとに引くと、4 つで表のロックに並んだ)。MAP_SHARED も写しの表を通して同じ。写したところで読むブロックが
キャッシュになければ、まわりの 32 ブロック (128 KiB) を、ディスクの上でつながっているぶんずつ 1 回で読む (Linux の mmap の
read-around。いままでは 4 KiB ずつで、Firefox の起動で 1 万 7 千回読んでいた: おそいディスクでは、そのたびに探しにいく)。
32 MiB のファイルを読むだけで写して 1 ページずつさわると、1 ページ 40 → 0.7 µs (Linux 7.3 は 2.2 µs)、4 つで 150 → 1.9 µs
(2.6 µs)、MAP_SHARED は 9.4 → 4.0 µs (2.0 µs)。Firefox (headless) の大きなロックの中のページフォルトは 25608 → 9068 回
(まわりで 7 万 7 千ページ)。

そのとき、ほかの重いところも 3 つ見つかった。mmap の場所は MMAP_BASE から上へ進むだけで、外したところを使わなかったので、
写して外すのをくりかえすプログラム (8 GiB の予約や、長く使った Firefox) は、下が空いていても 512 GiB の端で ENOMEM になった。
上の端までになければ、MMAP_BASE からさがしなおす (Linux もいつも空いているところからさがす)。munmap は、ページの表のない
ところを 2 MiB ずつたどっていた (L2 の表がなければ 1 GiB とばす)。それから munmap と exec (fork した写しを捨てる) は、
MAP_SHARED の領域のページをみな鍵にして 1 つずつ写しの表を引いていた (写していないページも)。表を range で引くと、256 MiB の
MAP_SHARED (1 ページさわった) の munmap が 6.6 ms → 93 µs (Linux 7.3 は 0.7 ms)、それを持ったままの fork + exec が
7.4 → 2.6 ms (7.6 ms)。smpbench に map-file (1 MiB の自分のファイルを写して 256 ページさわって外す) を足した。Linux は
その munmap で 1 ページずつすべての CPU の TLB を消す (TCG では全部の CPU を止める) ので、ここでは大きくちがう (aios は
ほかの CPU が載せていなければ自分の分だけ)。スレッドのあるプロセス (ほかの CPU でもスレッドが動いている) では aios も
すべての CPU に消させるしかなく、64 ページまでは 1 ページずつだったので、64 ページの munmap が 2.8 ms (Linux 7.3 は 4.0 ms)
かかった。本物の CPU では 1 ページ消させるのは 1 µs もかからず、全部消すと (aios は ASID を使わないので) すべての CPU の
すべてのプロセスの TLB を捨てることになるので、いままでどおりがよい。TCG では 1 ページごとに全部の CPU を止めて合わせる
(40〜80 µs) ので、全部消す 1 回 (100 µs ほど) のほうが 2 ページから安い。起動してすべての CPU が動いたら、すべての CPU に
1 ページ消させるのにかかる時間を測り (calibrate_tlbi)、5 µs をこえれば 2 ページから全部消す。64 ページの munmap は
2.8 ms → 115 µs、16 ページは 662 → 120 µs。

それでも Firefox で大きなロックをいちばん長く持っていたのは munmap (1 回 154〜218 µs) だった。munmap の中を区間に分けて
測ると、TLB を消すところではなく、写したファイルの鍵を集めるところ (shared_keys) が 1 回 62 µs で、いちばん重かった。
[start, end) にかかる領域をさがすのに、領域の表の頭から end まで (start より前の、かからないものもみな) 見ていた。
Firefox は領域が何千もあるので、munmap のたびに何千も見る。MADV_DONTNEED (discard) も同じだった。領域は重ならないので、
start より前に始まるもので start にかかりうるのは、最後の 1 つだけ (Linux の vma_find も start から引く)。そこから
見るようにした (vmas_in)。smpbench に mmap-busy (ファイルの小さな写しを 3000 持ったプロセスで mmap と同じこと) を足した:
129〜274 µs → 30 µs (領域のない mmap は 24 µs)。Firefox の munmap は 1 回 154〜218 → 76〜93 µs、ロックを持った合計は
980〜1290 → 400〜470 ms になり、いちばん長く持つものではなくなった。

つぎに長く持っていた read と write を分けるため、/proc/bkl の「hold の長いもの」で、1 つめの引数が fd のシステムコール
(read、write、readv、pread64、ioctl、sendmsg、recvmsg など。syscall::FD_SYS) を、その fd の開いたものの種類
(file、proc、pipe、unix、inet、eventfd、tty、dev、timerfd、other: file::CLASSES) ごとに「read pipe」のように出すようにした
(Linux にはないもの)。Firefox では read pipe が 2961 回 163 µs で、ロックなしの道からはずれたわけはほとんど「空」だった。
O_NONBLOCK で空のパイプを読むもの (イベントループは起こしの印のパイプを EAGAIN まで読む) は、Linux の pipe_read と同じく
何も変えずに EAGAIN を返すだけなので、ロックなしの道で答えるようにした (書き手がいなければ 0)。ただ Firefox のは、ほとんどが
中身が来るまで眠る read だった。眠るにはロックが要る。その道を prof で見ると、proc:wakeup が 12969 回、1 回 50 µs だった。
眠っているものを起こすのに、生きている Proc をみな見ていた (Firefox はスレッドが多い)。スケジューラが次を選ぶ pick も、
タイマの割り込みのたびの wake_expired も同じだった (pick を含む切りかえが 1 回 49 µs、13440 回)。Linux のように並びを持つ
ようにした: chan のハッシュごとの、眠っているものの並び (wait queue。256 こ)、期限つきで眠っているものの並び、走れるものの
並び (runqueue)。Proc の添字でつなぐので、眠るとき・起きるときに heap を使わない。眠るときに自分を入れ、起きて走りだしたとき
に自分で出る (起こし方が wakeup、シグナル、期限、kill といくつもあるので、起こす側は出さない。並びに残った起こされたものは
state でとばす)。Runnable にするところ (5 か所) は make_runnable 1 つにした。終わったスレッドは pick が見つけて片づけていた
ので、スケジューラが、そのスレッドから切りかわってページ表を外したところで片づける。起こしたものにゆずらせる相手は、CPU
ごとの、いま走らせているもの (ON_CPU) から選ぶ。Firefox で wakeup は 50 → 23 µs (残りはほかの CPU を起こす IPI。TCG では
重い)、切りかえは 49 → 32 µs、大きなロックを待った時間は 62.8% → 55.0%。net の往復 (4 CPU、3 回): pipe_rr は 0.85〜0.91
倍の時間、tcp_rr_epoll 0.86〜0.96 倍、unix_rr 0.96〜0.98 倍、futex_rr は 0.97〜1.14 倍 (ゆれの中)。

renameat を見る前に fsbench をくらべなおすと、10/21 の記録 (ebc0798) より stat (作ったばかりのファイルを 1 回ずつ) が
15.5 → 37 µs、deep-stat が 7.6 → 15 µs と遅くなっていた。st1 (1000 のファイルを作って 2 回ずつ stat) で 1 つずつコミットを
さかのぼると、2 つのわけがあった。(1) statc の表 (61e3060) は 1024 こで、1 つの場所に 1 つだけ: 1000 のファイルを順に引くと、
同じ場所になったものが追い出しあって、2 回目も 4 割しか当たらない (当たらないと、ふつうの道で引いて書きなおすので、ふつうの
道より遅い)。深いパス (12 段より多い) は表に入らず、毎回引いて外れていた。(2) 大きなロックなしの create (8f3f33e) で作った
ファイルは、パスの覚え (PATHS) に入らない (パスの覚えは大きなロックの中のもの)。そのうえ ext4 の名前のキャッシュは、
ディレクトリのブロックを書きかえるたびに、そのディレクトリの分をみな捨てていたので、はじめの stat は索引をなめなおしていた。
直したもの: statc の表を 4096 こ・4 つずつの組 (4-way) にし、通ったディレクトリは 22 段まで。表の版を、ディレクトリごとから
(ディレクトリ, 名前) ごと (65536 の箱) にした (Linux の dentry ごとの d_seq): ディレクトリに名前を足しても、ほかの名前の答えは
古くならない (ディレクトリの chmod と chown は全体の版を進める。まれなので)。大きなロックなしの create は、作ったファイルを
statc に書く (learn_new。Linux の d_instantiate): 表にある親の答えに、親で名前を引いたことを 1 つ足す。作る前の (親, 名前) の
箱の版を読んでおき、作ったあとにちょうど 1 つだけ進んでいれば (進めたのが自分だけなら) 書く。stat と inode の版は、inode を
書いてから名前を足す前 (まだだれも引けず書けないあいだ) に取るので、ext4 のロックをひとりで取らない (取っていたときは、4 つの
CPU の create-unlink が 1.55 倍の時間になった)。表に書く人は、場所ごとの seq を CAS で取る (取れなければ書かない: 覚えなので)。
ext4 の名前のキャッシュは、消した名前と指すものが変わった名前だけを捨て、探しているあいだの版は借りているあいだにくらべる。
足した名前は、はじめは入れなかった (stat は statc が答える。あとで大きなロックなしの create の分だけ入れた: 下の open-close)。meta の覚えも、
inode を書いたら捨てずに書いたものに入れかえる (Linux のメモリの inode と同じ)。st1 で stat 35.4 → 17.0 µs (Linux 7.3 は
17.3)、2 回目 13.7 → 8.2、create と unlink は HEAD と同じ (171 と 128 µs)、fsbench で deep-stat 15.4 → 5.5。smpbench
(4 CPU) で create-unlink-x4 は 0.74 倍の時間、stat-x4 0.80 倍。test/statc.c は 0 failed。

それから renameat (Firefox はファイルを保存するたびに、書いた写しを上へ rename する: 1 回 0.75〜1.1 ms)。sysfile の renameat は、
動かすものと上書きされるものを 1 度引いたあと、砂場 (landlock) のしらべで 2 度、上書きされたものを見るところで 2 度、同じ
名前を引きなおしていた (ext4 の lookup は 1 回 20 µs ほど)。Linux の do_renameat2 は 1 度引いた dentry をそのまま使う。引いた
ものを使うようにした。ext4 の上書きの rename は、先の名前を消して (unlink_node)、足しなおし (すき間をさがす)、元の名前を消す
と、ディレクトリのブロックを 3 度書いていた。Linux の ext4_rename は、先の項目の inode をその場で書きかえ (ext4_setent: 名前も
ハッシュも変わらない)、元の項目を消してから、上書きされたもののリンクを減らす。同じにした (unlink_node を、しらべる victim と
リンクを減らす drop_victim に分けた)。その場で書きかえる set_entry も、ディレクトリの項目をみな名前の文字列にして並べてから
探していたので、索引つきなら葉を 1 つだけ、なければ見つけたところまでなめる (entry_loc)。st3 (300 のファイル、6 回のいちばん
速い回、3 度): rename 325〜388 → 216〜239 µs、上書きの rename 302〜315 → 255〜310 µs。fsbench の rename 335 → 215 µs。

次は open-close (ちがうファイルを次々に開いて閉じる。fsbench の open-close と同じ形)。st6 (1000 のファイルを作り、stat を 2 度、
開いて閉じるのを 3 度) で、作ったばかりのものをはじめて開くのが 45〜49 µs、2 度目からも 29〜31 µs (O_NONBLOCK をつけて棚を
使わないふつうの道は 16 µs、棚 (4c3a50b) の前は 13.6 µs) と遅くなっていた。prof に一時の区間を足して分けると、棚にないときに、
むだな仕事が 2 つあった。(1) 大きなロックなしの open (file::fast_open) は statc の表で答えを確かめてから棚を見るが (7 µs ほど:
パスを写す 1.6、畳む 1.7、確かめる 3.4)、棚になければふつうの道へ行き、ふつうの道の openat が同じ答えを statc にもう一度書いて
いた (6.7〜8.9 µs)。(2) 閉じるとき棚がいっぱいなら、いちばん古いものを捨てる (shelf_prune) が、そのたびに棚の Vec を 3 つ
作りなおし、棚の場所も手放していた。そのうえ、ロックなしで作ったファイルはパスの覚えに入らないので、はじめて開くときは
パスをたどり、ext4 の名前のキャッシュにもないので、ディレクトリの索引をなめていた (1 回 23 µs)。直したもの: fast_open が
確かめてからふつうの道へ渡したもの (CPU ごとに、スレッドとパスのアドレス) は、ふつうの道で書きなおさない (ちがうものが
残っていても、覚えるか覚えないかだけ: 答えは変わらない)。shelf_prune は棚の Vec をその場で使う。大きなロックなしの create は、
作った名前を ext4 の名前のキャッシュに入れる (Linux の d_instantiate。1 回 3.3 µs: TCG)。st6 で、はじめて開くのが
45〜49 → 33 µs、2 度目から 29〜31 → 23 µs。st2 (300 を作って消す) は ab で 8 回ずつ、はっきりしたちがいなし。

それから、索引つき (htree) のディレクトリの create と unlink。大きなロックなしの create と unlink (fastdir) は、索引つきの
ディレクトリをみな断っていた: ディレクトリが 1 ブロックをこえると (fsbench の 1000 のファイルなら 340 こめから) ふつうの道
だった。Linux の ext4_dx_add_entry は、ハッシュの入る葉にすき間があれば、葉を 1 つ書きかえるだけ (分けるときだけ、索引と
新しいブロック)。消すのも葉から消すだけ。索引のないディレクトリのすき間に入れるのと同じなので、ロックなしの道でもできる。
dx_new_slot が索引を 1 度たどり、もうないか (同じハッシュが続く葉も) を見て、入る葉のすき間を返す (なければ None: 分けるのは
ふつうの道の dx_add_entry)。entry_at は索引つきなら葉を 1 つだけ見て (dx_entry_at)、前の項目といっしょに返すので、
unlink_node も fast_unlink も、見つけた場所をそのまま消す (前の unlink_node は名前で引いてから dx_remove でもう 1 度
たどっていた)。根だけの索引の最後の葉で見つからなければ、ない (前は「決められない」で全部をなめていた: Linux の
ext4_htree_next_block も根で終わる)。st8 (4000 のファイル、名前の長さはまちまち、作る・消す・名前を変える・上書きの rename
を混ぜて、毎回 stat と readdir で答えを確かめ、終わってから外から e2fsck): ロックなしで作ったもの 216 → 13671、
create 216〜228 → 110〜189 µs、混ぜたもの 398〜462 → 333〜371、unlink 84〜92 → 45〜86、空にしてから作りなおす
260〜286 → 114〜126。答えはみな合い、e2fsck もエラーなし。

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

0 differ になったものが、いつの間にかくずれないように、CI (.github/workflows/unix.yml の VM test) でも push のたびに答えあわせ
をする (test/sysdiff-ci.sh)。Linux の答えはアーキテクチャによらないので、CI で Linux を起こさず、手本の Linux で動かした
答えを test/sysdiff/ に置いておく (`test/sysdiff-ci.sh golden 7.3` が test/linux-vm.sh の Linux で作る。ふだん確かめている
大きさの ext4 300 × 30 op、ext4 200 × 80 op、tmpfs 300 × 30 op の 800 case で 310 KB)。CI は同じ case を aios の VM で動かし
(2 分ほど)、case ごとに最初にちがう行を Linux と aios で並べて出す。答えは、作ったときの test/sysdiff.c の sha256 と
いっしょに置くので、sysdiff.c を変えて答えを作りなおし忘れると、CI はくらべる前に止まる。CI の gcc (glibc) で作ったもの
と aish の zig (musl) で作ったものの Linux の答えは、同じだった。

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
| `/proc/ai/why` | しくじったわけ (kernel/src/why.rs。aios だけのもの): システムコールが errno を返したとき、Linux と同じ errno のほかに、なぜそうなったかを残す (errno もふるまいも変えない)。パスのどこで止まったか (`missing`: そこまではあって、その名前がない。`not_dir`、`loop`、`name_too_long`、パスの覚えにあった「ない」は `missing_cached`)、どの許しが足りなかったか (`perm`: ほしかった r/w/x、そのものの mode と持ち主、呼んだ人の uid と gid、答えを決めた組 (持ち主・グループ・ほか))、seccomp、landlock (足りなかった権利の名前)、`fail` でわざと、切ったところ、aios にないシステムコール、readlink したものがリンクでない (`not_link`)、開いていない fd (`bad_fd`)、EINVAL はどれも断ったカーネルのソースの場所 (`where`: `src` にファイルと行。EINVAL は Linux でもわけがいくつもあるので、その行と上に書いたわけを見られるように)。わけを書いたところがパスを知らなければ (open の最後のファイルの許しなど)、プログラムがたのんだパス (`asked`)。1 行 1 つで新しい順: `pid`、`comm`、`syscall`、`errno`、`code`、`path`、`name`、`why` (人と AI が読む 1 文)。ほんとうのしくじりと、さがしもの (`probe`: stat のなかまの ENOENT と ENOTDIR、ioctl の ENOTTY、readlink の EINVAL、fcntl と close の EBADF、getxattr の ENODATA。シェルの PATH さがし、aish の色づけ、locale のファイル) は別の輪 (256 こずつ) に。EAGAIN、EINTR、ETIMEDOUT は入れない。だれでも読めるが、root と wheel でない人には自分のものだけ。aish は対話でコマンドが 0 でなく終わると (シグナルのときはのぞく)、そのジョブのわけを 1 行出し (コマンドに書いたパスにかかわるものを先に)、`why` でみな (`why -a` は残っているものみな) |
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
- `test/vm-boot.sh [--new]` (開発のマシン) は、これを aios.img の VM (-snapshot) で確かめる: 起動するもの、読めないもの、
  止まるもの (`AIOS_TEST_HANG=1` でビルドしたカーネル) を入れて再起動し (`test/vm.py reboot --reset-after`)、
  前のものに戻ったか、わけ (why) を見る。`--new` はこのソースの aiboot、aiosd、aios を入れてから
- aikernel のパッケージを上げると /boot/Image はパッケージのものになる
- `aios build pkg` は bin/mkpkg.sh と同じものを aios の中で作る (user/src/lib/mkpkg.rs)。PKGBUILD の関数
  (pkgver prepare build package) は bash (brush) で動かし、ソース (git+、http(s) は fetch、横のファイル)、sha256、
  .PKGINFO、tar と zstd はこちらでする。zstd のコマンドがなければ ruzstd で縮める (速いが、-19 ほどは縮まない)。
  作業の場所は ~/.cache/aios/build/NAME。root はいらない (ファイルの持ち主は tar の中で root にする)
