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

1. **把握 (get)** ✅: `aios get [PATH]` で状態の木を読む。読むだけなので aiosd なしで、呼んだプロセスの中で集める。
   `aios health` (と aish-sys の `health`) は、そこから「どこがおかしいか」の見たて: AI がまず見るところ
2. **操作 (do)** ✅: aiosd (root で動くサービス) が `/run/aiosd.sock` で受ける。だれが呼んだかは SO_PEERCRED で見る
   (読むのはだれでも、変えるのは root と wheel)。したことは `/var/log/aiosd.log` に残す
3. **設定 (apply)** ✅: `/etc/aios.json` (望む状態) と状態の木をくらべて (`aios diff`)、ちがうところだけ直す (`aios apply`)。
   apply のたびに前の設定とパッケージの版を `/var/lib/aios/history/` に残し、`aios rollback` で戻せる
4. **改造 (src / build)** ✅: aios のソースを `/usr/src/aios` に置き、カーネルやパッケージをビルドして入れる。
   カーネルは前のものを ESP に残し、起動しなかったら前のもので起動しなおせるようにする

## AI が把握して、直しつづけるしくみ

AI (Claude の aish、アイ) が aios をつかみ、自分で次の仕事を見つけて直しつづけるための 3 つ。どれも「まずここを見る」口:

- **いま (aios health)**: 動いている aios の見たて。サービスのしくじり、ディスクとメモリ、BKL の取りわすれ、カーネルのヒープと
  oom、固まったスレッド、試している新しいカーネル、ファイアウォールの確かめ待ち、apply のしくじり、新しい版のあるパッケージを
  1 つずつ ok / info / warn / crit と、次に使う道具 (hint) で。aish-sys の `health`、`aios health` (終わりの番号 0 / 1 / 2)、
  `aios get health` (下の状態の木)
- **毎日 (改善の種の報告)**: CI (.github/workflows/pkg.yml の report、毎日と手で) の `aish-pkg report` が、パッケージ (新しい版、
  タグだけで tarball のまだないもの、直っていない CVE、見られなかった配布元)、Linux (kernel.org の安定版と mainline が手本の
  test/sysdiff/linux.sum より新しければ: 答えあわせのしなおし)、CI (main の unix がしくじっていれば)、課題 (todo.json の open と
  doing) を、GitHub のリリース pkg-updates の updates.json と Atom の feed (updates.atom) に置く。entry の id は中身で決まるので、
  aigpt の feed (`aios-pkg`) の `fetch_feed` は新しく出たものだけを見せる。`pkg_check` の `daily: true` は報告をすぐ読む (いつから
  遅れているか (since) も)。パッケージは `pkg_update` (daily で) でまとめて上げる
- **課題の一覧 (todo.json)**: リポジトリの一番上の、機械が読める課題の一覧 (1 行 1 つ: id、area、status (open / doing)、
  since、what、why、verify)。doc や記憶にばらばらに書いていた「のこり」をここへ。どの AI のセッションも同じものを見て続ける。
  とりかかったら doing、終わったら (verify のとおりに確かめてから) 行を消してコミットする (コミットのメッセージに id。終わった
  ものの記録は git の歴史と doc に)。新しく見つけたものは足す (test/aios-json.py などのたしかめが CI で見る)
- **リポジトリの JSON の役目**: 1 つのことは 1 か所 (本物) に書き、ほかはそこから作るか、たしかめるだけにする
  | ファイル | なにか | だれが書く | たしかめ |
  |---|---|---|---|
  | `kernel/kernel.json` | カーネルの部品の地図 (core / extra / quirk) | 人 | test/kernel-json.py |
  | `.aios.json` | OS の見取り図 (パッケージの版と組、プラグイン、カーネルの名前など)。`aigpt import os` で PDS の ai.syui.gpt.os に。OS の中の `/etc/aios.json` (のぞむ設定) とは別のもの | 人と aish-pkg (pkg を PKGBUILD にそろえる) | test/aios-json.py |
  | `pkg/upstream.json` | 配布元のどこを見るか | pkg_check (PKGBUILD と Arch の .nvchecker.toml から) | |
  | `pkg/ci.json` | CI で作る重いパッケージ | 人 | pkg.yml |
  | `todo.json` | のこりの課題 (open と doing だけ) | 人 | aish-pkg report |
  パッケージの一覧 (name type src now latest) は `build/aish-pkg/pkg.json` に作り、リポジトリには入れない (PKGBUILD から
  作れて、配布元の最新は CI の毎日の報告にある)

回り方: feed (や pkg_check daily) で新しい種を受けとる → `aios health` といまの様子で確かめる → なおす (todo.json の verify で
確かめ、Linux とくらべるものは test/linux-vm.sh の手本と) → todo.json の行を消し doc を書きかえてコミット → 次の日の報告が消える。

## パッケージの手入れ (aish-pkg)

aish-pkg (shell/pkg、aish-pkg のプラグインと `aish-pkg` のコマンド) は、pkg/ のパッケージを最新に保つ道具。ふつうは
pkg_check → pkg_edit → pkg_build → pkg_test (vm) → pkg_push、重いもの (pkg/ci.json) は CI が作って pkg_fetch、まとめては
pkg_update。

### ツール

- `pkg_check`: 配布元の最新といまの pkgver。どこを見るか `pkg/upstream.json` は PKGBUILD の git と Arch の `.nvchecker.toml`
  から作る。`#commit=` のものはリポジトリの HEAD と、PKGBUILD の `_latest_url` と `_latest_regex` があればそのページと、
  download.gnome.org のものはその cache.json とくらべる。配布元はどれも 30 秒で切り、しくじれば source の tarball
  のディレクトリの一覧 (ftp.gnu.org/gnu/NAME/ など。ファイルの名前に $pkgver があるもの) から見る (`via_listing`。savannah の
  git は 500 を返したり何も返さなかったりする)。新しい版のタグはあっても配る tarball がまだないもの (404、410) は `not out yet`
  (`not_out`。new には入れない)。PKGBUILD のない npm で入れるもの (`.aios.json` の pkg.linux.npm に人が書く。いまはない) は、
  npm の registry の dist-tags の latest と (`pkg_edit` は `.aios.json` の版を書きかえる。`@SCOPE/NAME` の名前でよい)。PKGBUILD
  に `_npm=NAME` と書いたもの (claude と pnpm) は、パッケージの版が npm の NAME の版で、`_latest_url` の dist-tags とくらべる
  (post_install がその版を入れるので、ap -Syu で新しい claude-code が来る)
- `pkg_edit`: PKGBUILD の pkgver・pkgrel・sha256sums と `.aios.json` を書きかえる。source が 404 か 410
  ならやりなおさずにすぐしくじる。`rebuild: true` なら pkgrel を 1 つ上げるだけ
- `pkg_build`: bin/mkpkg.sh で `repo/aarch64/KIND/` に作り、aios.db を作りなおす。前の版にあったライブラリ (SONAME と *.so*
  の名前) が新しいものになければ、それを NEEDED に書いたパッケージを `soname` に出す (作りなおすまで動かない)。
  いくつもいっしょに作っても aios.db と .aios.json がこわれないように錠をかける
- `pkg_fetch`: 重いもの `pkg/ci.json` (firefox、llvm、uv) は GitHub の CI `.github/workflows/pkg.yml` が作ってリリース pkg-NAME
  (新しい版で置きかえる) に置くので、それを取ってきて SHA256SUMS を確かめ、build と同じく置く。PKGBUILD もリリースのものに
- `pkg_test`: 版、ELF が aarch64 か、PIC でないプログラムが COPY で写したライブラリのデータを、別のものなのに 1
  つの場所にまとめていないか (copies。ライブラリのほんとうの番地とくらべる。Zig の libc.so の見本を zig.sh
  で直す前にリンクしたものは stdin・stdout・stderr が 1 つで、pcre2grep の答えが標準エラーに出た)、NEEDED
  のライブラリがパッケージと depends と musl の中にあるか (needs)、bin/ を qemu-aarch64 か aarch64 で --version (なければ
  -version、-V、version、--help。0 でなくても、パッケージの版を出したもの (musl の ldd は版と使い方を出して 1) と、
  使い方を出して 1 か 2 で終わるもの (vulkaninfo のように版の引数がない) は動いたとみる。シグナルで落ちたものと、
  ライブラリが読めない 127 はしくじり)。PATH と XDG_DATA_DIRS は広げたところだけにして、このマシンの man や GSettings の schema
  を使わない。`vm: true` なら aios の VM (`-snapshot` で repo/ を出したもの。動いていなければ起こし、終わっても止めない。
  ちがうものが動いていれば使わない) にも `ap -S` で入れて、bin/ のものを aiwm と同じ XDG_DATA_DIRS で aios
  のカーネルの上で動かし、このマシンで動いたのに aios で動かないものがあればしくじり
- `pkg_scan`: repo/aarch64 のパッケージみなを、動かさずに copies と needs で。Zig を上げたときなどに
- `pkg_rebuild`: いくつものパッケージを順に pkgrel+1 → build → test、`push: true` ならみな通ったら push。names がなければ
  pkg_check の `rebuild` のもの、`unrecorded: true` なら Zig を書きとめる前に作ったものも。c_deps で使うものを先に作る。pkgrel
  を上げるのは、いまの版がもう ai/repo にあるときだけで、もう作りなおしたもの (確かめてあって、古いライブラリもちがう Zig
  もない) は飛ばすので、もういちど動かしても 2 つ上がらない。CI が作るもの (pkg/ci.json) も飛ばす。しくじったものは飛ばして続け
  (`fail_fast` で止める)、さいごにまとめる。`vm: true` で pkg_test を aios の VM の中でも。`dry: true` で順だけ。進みぐあいは
  rebuild.log、くわしくは build-NAME.log / test-NAME.log
- `pkg_update`: 新しい版の出たものをまとめて上げる: names がなければ pkg_check の new のもの、`daily: true` なら CI
  の毎日の報告の new を、いま見なおして。タグだけで tarball がまだないものと見られなかったものは飛ばす。npm で入れるもの
  (pkg.linux.npm) は `.aios.json` の版を書きかえるだけ、CI が作るもの (pkg/ci.json) は PKGBUILD を上げるだけ (main に push
  すると pkg.yml が作るので、そのあと pkg_fetch)、ほかは pkg_edit で上げ (調べたときの版に)、それを中に入れているもの
  (静的なライブラリ) といっしょに pkg_rebuild と同じく作って確かめる。`push: true` ならみな通ったら pkg_push、`dry: true`
  で何をどう上げるかだけ。さいごに、コミットして main に push するファイル (`files`) と次にすること (`next`)
- `pkg_push`: ai/repo とくらべて bin/gitea.sh repo で送る。ai/repo の写し `build/airepo` から、
  変わったものだけ取って変わったものだけ送る。pkg_test を通っていないものや、ai/repo のほうが新しいものがあれば止まる。
  同じ名前で作りなおしたものは、もう一度 pkg_test を通すまで送らない。bin/publish.sh の push とも錠で 1 つずつ

### うしろで動くもの

- check (配布元をみな見ると 1 分ほどかかり、ツールの 60 秒をこえるので) と build と push はうしろで動き、
  もういちど呼ぶと様子か結果 (`stop: true` で孫の make や zig まで止める)。
- pid は `build/aish-pkg/KEY.pid` にも書くので、aish やプラグインが入れかわっても (ビルドしなおすたび)、
  次に呼べば引きついで続きを待つ (終わっていればその答え)。

### 中に入れたライブラリと Zig (作りなおしの見はり)

- 中にまるごと入っているライブラリ (静的リンク。`c_deps` のもの) の見はり: 作るときに入れた版を `.PKGINFO` の
  `makedepend = NAME=VER-REL` に残し (zig.sh の `c_deps` と mkpkg.sh)、`pkg_check` はそれより新しいライブラリがあるものを
  `rebuild` に出す (`pkg_edit` の `rebuild` → build → test → push)。
- 新しい版のあるライブラリには、それを入れているもの (`embedded in: ...`)、`pkg_build` の答えにも作りなおすもの (`rebuild`)。
- 作った Zig も `makedepend = zig=ZIG_VERSION-ZIG_REL` に残し (zig.sh の `zig_env`。`ZIG_REL` は zig_fix_libc の直しの回)、
  いまの zig.sh とちがうもので作ったものも `rebuild` に出す (古い libc.so の見本でリンクしたもののように、
  作りなおすまで直らないので)。
- 書きとめる前に作ったものは、作った日の zig.sh の版 (git の履歴から) ごとの数を 1 行に (名前は JSON の `unrecorded`)。

### 直っていない CVE (vulns、shell/pkg/src/vuln.rs)

- 直っていない CVE の見はり (`vulns`。vuln.rs): Alpine の secdb (edge の main と community の「この版 `VER-rN` で直した CVE」。
  1 日に 1 回 `build/aish-pkg/secdb/` に取ってきて、取れなければ前のものを使ってそう書く。取りなおすのは 1 時間に 1 回まで) と
  pkg/c のもの (と sudo-rs) の pkgver をくらべ、直した版のほうが新しければ `fixed in VER` (pkg_edit で上げる)、Alpine
  が同じ版にパッチをあてて直したもの (`VER-r1` から) は `patched by Alpine` (aports のパッチを足す)。
- PKGBUILD かとなりのファイル (パッチなど) に CVE の番号を書いたものは出さない (直したもの、aios にはあてはまらないもの)。
- 名前のちがうものは ALIAS (glib2 は glib など)、pkg/rust の coreutils (uutils) は名前が同じでも別のものなので見ない。
- firefox は Mozilla の advisories で (github.com/mozilla/foundation-security-advisories の `announce/YEAR/mfsaYEAR-N.yml` の
  fixed_in の `Firefox VER` とその CVE。Android だけのもの (title が `Firefox for Android VER`) も fixed_in は `Firefox VER`
  と書くが、デスクトップにはその版が出ないので見ない。git で浅く、ことしと去年のところだけ `build/aish-pkg/secdb/mfsa` に取り、
  これも 1 日に 1 回。Alpine は firefox の secfixes を書かず、Arch の security tracker は 2023 年からほとんど動いていない
  (firefox は 2019 年の 67.0.4 まで))。
- CVE が多いもの (Mozilla は 1 つのバグに 1 つ) は 4 つと残りの数。
- どこも書いていないもの (mesa など) は見られない。
- Rust の crate も (`crate`): aios の Cargo.lock (いちばん上のカーネルのものと、user・shell・desktop・aimg・boot のもの) と、
  pkg/rust のものの配布元の Cargo.lock (PKGBUILD の source の git のタグかコミットのところを、git で浅く Cargo.lock だけ取って
  `build/aish-pkg/secdb/locks/` に。HEAD から作るものは 1 日に 1 回) を、cargo audit と同じ RustSec の advisory-db (crates/
  のところだけ `build/aish-pkg/secdb/rustsec` に、1 日に 1 回) とくらべる。
- PKGBUILD の `cargo_update CRATE@VER` (pkg/rust/cross.sh: 配布元がまだ上げていない直った版に、同じ semver の中で
  `cargo update --precise` で上げる。配布元が上げていれば何もしない) で上げたものは、上げた版で見る。
- informational (unmaintained や unsound) と withdrawn のもの、Linux や aarch64 にあてはまらないものは出さない。
- 直った版がないものと、プログラムに入らないもの (dev-dependencies) は、RUSTSEC の番号とわけを PKGBUILD (aios の Cargo.lock は、
  となりの Cargo.toml など) に書く。
- npm も (`range`): PKGBUILD の `_npm` のもの (パッケージの版) と `.aios.json` の pkg.linux.npm に書いた版を、npm audit と同じ
  registry.npmjs.org の advisories と (1 日に 1 回。たずねるものが変われば、すぐ)。
- uv で入れる Python も (`.aios.json` の pkg.linux.uv の python。uv python install の CPython): Alpine の python3 の secfixes
  と (直ったものは `uv python install VER` で入れて、その版を書く)。

### 一覧と .aios.json

- 一覧は `build/aish-pkg/pkg.json` (リポジトリには入れない。1 つに 1 行で name type src now latest。latest は見たときの最新か、
  PKGBUILD の版のほうが新しければその版で、`pkg_edit` で上げたあとも足したばかりのものも now より古くならない)。
- `.aios.json` の pkg は、check・edit・build のたびに pkg/ の PKGBUILD にそろえる (新しいものを足し、版を直し、
  なくなったものを消す。npm と uv は見ない)。
- M-p で check を出す。

### 毎日の見はり

- 毎日の見はり: `aish-pkg report DIR [PREV]` が check をまるごと回して DIR に `updates.json` (改善の種として、Linux (kernel.org
  の安定版と mainline が手本の test/sysdiff/linux.sum より新しいもの)、CI (main の unix がしくじっていれば)、todo.json の open
  と doing の課題も。まだ上げていない新しい版、タグだけで tarball がまだないもの、直っていない CVE、見られなかったもの。PREV
  (前の日のもの) から、はじめて見た日 `since` をうけつぐ) と `updates.atom` (そのやること (新しい版と CVE) の Atom の feed。id
  はパッケージと版か CVE の番号なので、同じものは次の日も同じ id) を書き、CI (`.github/workflows/pkg.yml` の report、毎日と手で)
  が GitHub のリリース `pkg-updates` に置く。
- AI は aigpt の feed (`aios-pkg`: https://github.com/syui/aios/releases/download/pkg-updates/updates.atom) の `fetch_feed`
  で新しく出たものだけを見て、`pkg_check` の `daily: true` で報告をすぐ読む (見に行かない)。
- CI は作るのも送るのもしない (送るのは手元の ai の鍵で)。

### コマンド

- `aish-pkg check` / `edit NAME [VER] [--rebuild]` / `build NAME` / `fetch NAME` / `test NAME [--vm]` / `push` /
  `scan [NAME...]` (repo/aarch64 のパッケージみなを、動かさずに pkg_test の copies と needs で。depends にない NEEDED は、
  それのあるパッケージも) / `rebuild [NAME...] [--unrecorded] [--force] [--fail-fast] [--push] [--vm] [--dry]` /
  `update [NAME...] [--daily] [--fail-fast] [--push] [--vm] [--dry]` / `report DIR [PREV]` とコマンドでも
  (`check --json NAME...` は CI が読む 1 行の JSON)

## 状態の木 (aios get)

| PATH | 中身 |
|---|---|
| `health` | いまの見たて (`aios health` が短い形で出し、終わりの番号は ok と info が 0、warn が 1、crit が 2)。`status` (いちばん悪いもの)、`summary`、`checks` (1 つずつ name、status: ok / info / warn / crit、what、warn からは hint: 次に使う道具): `service` (しくじったユニット: crit)、`disk` (9 割で warn、98% で crit)、`mem` (使えるものが 1 割を切る・3% を切る、スワップが半分をこえる)、`bkl` (大きなロックなしでさわったところ: カーネルのまちがいなので crit)、`heap` (カーネルのヒープ、取りそこね、oom で殺した数)、`oom` (kmsg の oom: killed)、`hung` (ディスクを待つ (ps の D) スレッドが 60 秒より長い)、`boot` (試している新しいカーネル)、`firewall` (aios confirm 待ち、aios の外で変えた決まり)、`apply` (いちばん新しい apply がしくじった)、`pkg` (`ap -Qu`: 同期データベースに新しい版があるもの。ネットは見ない。同期したことがなければ info)。/proc/ai が読めない (root と wheel でない) ものは見ずに、そう書く |
| `host` | 名前、OS (/etc/os-release の NAME と VARIANT と base の版: `aios (aikernel) 0.0.1.r…`。VARIANT の aikernel は、カーネルから作った aios を Arch Linux から作った aios と見分ける) |
| `kernel` | 版、コマンドライン、起きてからの秒、CPU の数、モジュール、BKL (`/proc/bkl`)、メッセージの終わり (`/proc/kmsg`)、`sysctl` (`/proc/sys`) |
| `mem` | メモリとスワップ (KiB) |
| `disk` | マウントごとの使った量と大きさ |
| `proc` | プロセス (pid ppid 状態 スレッド CPU 秒 メモリ コマンド) |
| `service` | ユニット (動いているか、enable か、説明) |
| `pkg` | 入っているパッケージと版、リポジトリ |
| `net` | インターフェイスとアドレス、経路、DNS、`firewall` (nftables のいまの様子: 基の鎖、決まりの数、落としたもの、conntrack、aios の外で変わったか、confirm を待っているか) |
| `user` | ユーザー (uid、ホーム、シェル) |
| `boot` | ESP のカーネルとローダーのエントリ、`new` (試している新しいカーネルの entry、`aios-new+1.conf` など)、`try` (その残りの回数)、`prev` (`/boot/Image.prev` があるか)、`last` (最後の起動の結果と、新しいカーネルで起動しなかったわけ `why`) |
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
aios apply --confirm 60                 # ファイアウォールを変えるなら、60 秒のうちに confirm がなければ前の決まりにもどす
aios confirm                            # いまのファイアウォールでよい (もどすのをやめる)
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
- `firewall`: nft の決まり (`"rules"`: 文字か行の並び。nft -f の書きかたそのまま: 「ファイアウォール (nftables)」) と
  `"confirm"` (秒、10 から 3600。なくてもよい)。apply は nft のパッケージ (nftables) がなければ入れ、`nft -c -f` で
  たしかめてから (まちがいがあれば何も変えずにしくじる) `/etc/nftables.conf` に書いて (はじめに `flush ruleset`)
  `nft -f` で入れ、起動のたびに入るよう nftables.service (パッケージのもの。Before=sysinit.target なので sshd などが
  外とつなぐ前) を enable する。aios が入れたときの生成番号とファイルのしるしを `/var/lib/aios/firewall.json` に
  残し、そのあとに nft で変えたもの (生成番号がちがう) も diff に出て、apply で aios.json のものにもどる
  (起動しなおしたときは、ファイルが同じならそろっているとみる)。`aios config` は aios が書いた決まりを書き出す。
  `aios get net.firewall` に、いまの様子 (基の鎖、決まりの数、落とした数と新しいもの、conntrack の数、aios の外で
  変わったか、confirm を待っているか)
- **戻せる apply** (Linux にはない: iptables-apply や netplan try にあたるものを apply そのものに): `confirm` の秒
  (aios.json の `firewall.confirm` か `aios apply --confirm N`) があれば、入れたあとその秒のうちに `aios confirm`
  (aish の do の op confirm) がなければ、aiosd が入れる前の決まり (`nft list ruleset` でとっておいたもの) と
  `/etc/nftables.conf`、`/etc/aios.json` の firewall を前のものにもどし、記録に「firewall of N reverted (not
  confirmed)」を残す。confirm を待っているあいだに電源が切れても、次の起動で aiosd がもどす
  (`/var/lib/aios/firewall-pending.json`)。AI が ssh ごしに決まりを変えて自分を締め出しても、その秒のあとにはまた
  つながる (VM で確かめた: 外を締め出す決まりで hostfwd のつなぎが時間切れになり、20 秒後にもどった)。まちがいの
  ないものは `nft -c` で止まるので、confirm が守るのは「書きかたは正しいが、つながりを切ってしまう」決まり

```json
{
  "host":    { "name": "aios" },
  "pkg":     ["openssh", "cargo", "base-devel", "git", "aiwm", "mesa"],
  "service": { "sshd": "enabled", "aiwm": "disabled" },
  "kernel":  { "modules": ["virtio_gpu", "virtio_input", "virtio_snd"] },
  "user":    { "ai": { "shell": "/bin/aish", "groups": ["wheel"] } },
  "net":     { "eth0": { "address": "10.0.2.15/24", "gateway": "10.0.2.2", "dns": ["1.1.1.1"] } },
  "sysctl":  { "vm.min_free_kbytes": 8192, "fs.inotify.max_user_watches": 65536 },
  "firewall": {
    "rules": [
      "table inet filter {",
      "  chain input {",
      "    type filter hook input priority filter; policy drop;",
      "    ct state established,related accept",
      "    ct state invalid drop",
      "    iif \"lo\" accept",
      "    tcp dport 22 accept",
      "    icmp type echo-request accept",
      "  }",
      "}"
    ],
    "confirm": 60
  }
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
| `fs.suid_dumpable` | euid や egid を変えたプロセスの dumpable (0、1、2。既定 0: 「dumpable と /proc/PID の持ち主」) |
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

Claude の使う aish (`/etc/claude-code/managed-mcp.json`) は `aibox -W ~/.cache -W ~/.cargo -w /proc/ai/ctl -- aish --mcp` で起きる (`/proc/ai/ctl` は下の調べもののスイッチ)。アイの記憶 (aigpt、`ap -S aigpt`) は `aibox -W ~/.cache -W ~/.config/ai.syui.log -- aigpt server` (書くのは記憶と feed の既読だけ。managed-mcp.json があると `aigpt setup` の `claude mcp add` は効かないので、ここに置く)。Claude が動かすものはみな、Claude を起こしたディレクトリの中にしか書けない。root のする操作は `aios do` (aiosd が wheel の人かを見る) を通す。

## カーネルの地図 (kernel/kernel.json)

カーネルのファイルと手当てを、AI が先に読んで見通せるように 3 つに分けた一覧 (1 行に 1 つ)。
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

### 書き出しのスレッド (flush、pid 2)

`fsync` を呼ばない書き手のぶんは、まだ眠らずに書いていた。書いたものがたまる (メタはジャーナルの 1/4 か 512 ブロック、
中身はキャッシュの 1/4) か 5 秒たつと、そのとき書いていたシステムコールの中 (`maybe_flush`) で、大きなロックを持ったまま
ディスクを回って待つ。暇なときの 1 秒ごとの書き出し (`vfs::idle_sync`) も、cpu0 のスケジューラのループで同じに書いていた
(そのあいだ、ほかの CPU は大きなロックを待つ)。Linux では書き手は待たず、ジャーナルのスレッド (jbd2 の kjournald2) と
writeback のスレッドが、ほかのものを止めずに書く。

いまは aios にもカーネルのスレッドがある (`proc::kthread`。Linux の kthread_run): ユーザー空間へ戻らず、スケジューラから
受けとった大きなロックを持って走り、眠るあいだは放す (眠るシステムコールと同じ)。アドレス空間は空、fd はない。init の
すぐあとに作るので pid 2 (Linux の kthreadd と同じところ)、親はいない (ppid 0: init の `wait` には出てこない)。
`/proc/PID/cmdline` は空で `/proc/PID/stat` の flags に PF_KTHREAD (ps は `[flush]` と出す)、`/proc/PID/exe` は ENOENT、
シグナルは送れても何も起きない (`kill -9 2` も)、OOM のときに選ばれない。いまは 1 つ、書き出しのスレッド `flush`
(`vfs::flusher`) だけ:

- `maybe_flush` は書かずに頼む (`vfs::kick_flusher`。もう頼んでいれば何もしない)。書き手はディスクを待たない。
- 暇なときの 1 秒ごとも頼むだけ。スレッドは頼まれなくても 5 秒ごとに起きて書く (Linux のコミットの間隔: 大きなロックなしの
  `pwrite` や MAP_SHARED で書いたものは `maybe_flush` を通らないので、いつも暇がない機械でも 5 秒のうちに書く)。
- スレッドは `sync_all_wait` で書く (`fsync` と同じ、眠って書く書き出し)。書いているあいだに頼まれれば、すぐもう 1 回。
- 追いつかずに 2 倍までたまったら、書いた人だけを、そのシステムコールの終わり (何も借りていない `trap.rs`) で、スレッドが
  次の回を終えるまで眠らせる (`vfs::balance_dirty`。Linux の balance_dirty_pages: 書く速さをディスクに合わせる。シグナルか
  1 秒でやめる)。待たせないと、スレッドが書いているあいだにたまるものに上限がない (書いていないページはキャッシュから
  捨てられず、メモリがなくなる)。それでも 3 倍までたまったら、書き手がそこで書く (ジャーナルに入りきらないほどためない)。
  スレッドがまだいない起動のとちゅうも、そこで書く。
- `/proc/bkl` の `(kernel thread)` は、スレッドが 1 回まわるあいだに大きなロックを持っていた時間。

`test/fsyncstall -n` (fsync しない書き手が 4 KiB ずつ書き足すあいだ、別のプロセスの `uname` が何 ms 止まるか) を、
1 秒に 50 回書けるディスク (`throttling.iops-write=50`)、4 CPU で 12 秒: 前は `uname` がいちばん長くて 0.4〜2.3 秒止まった
(書き手のシステムコールの中の書き出しが、大きなロックを持ったままディスクを待った)。いまは 25〜61 ms。書き手を待たせる
前は、たまった中身がスレッドの書くあいだに上限の 3.6 倍 (58 MiB) までふくらんだ。いまはちょうど 2 倍で止まり、書き手は
ディスクの速さに合わせて書く (12 秒に 5.8 万回)。そのぶん `uname` は前の 6 倍通る (4.7 万回から 27 万回。書き手を
待たせる前の 9 万回からも 3 倍)。Firefox (headless で 1 ページ、3 回のまんなか) は大きなロックを待った割合が 16.3% から
12.4%。のこりの長い止まりは、大きなファイルを切りつめる・消すとき (下)。

### 大きなファイルを切りつめる・消す (extent ごとに放す)

前は、切りつめ (`ftruncate`)、消す (最後の参照がなくなったとき)、穴あけ (`fallocate` の PUNCH_HOLE) で、ブロックを
1 つずつ放していた (`free_block`: ビットマップ、グループ記述子の空きの数、スーパーブロックの空きの数を、ブロックごとに
書きかえ、キャッシュの表からも 1 つずつ外す)。いまは extent 1 つぶんをまとめて (`free_blocks`。Linux の ext4_free_blocks
も extent ごと): ビットマップはグループごとに 1 度、まん中を 1 バイトずつ 0 に (`clear_bits`)、数もグループごとに 1 度
足す。キャッシュの表 (`Cache::forget_range`) とページキャッシュ (`vm::file_changed`) からは、範囲をまとめて抜く
(`BTreeMap::extract_if`。1 つずつ `remove` すると、木を何万回もたどりなおした)。覚えている順の列 (`order`) を
作りなおすのも、のこったものの 4 倍をこえてから (前は 2 倍: extent ごとに覚えている数が半分になって、1 つのファイルで
何度も作りなおした)。

`trunc` (64 MiB を書いて `fsync` してから、`ftruncate` で 0 に、もう一度書いて `close` してから `unlink`) を、同じ
QEMU (cortex-a72) の Linux 7.3 とくらべると: 前の aios は切りつめ 90〜107 ms、消す 88〜134 ms で Linux (85〜90 ms と
86〜96 ms) とほぼ同じ。いまは 22〜41 ms と 31〜38 ms。16 MiB では 24 ms と 43 ms が 6 ms と 7 ms (Linux は 23 ms と
24 ms)。そのあとの e2fsck -fn はきれい (fsx の frag で深い extent の木を切りつめ・穴あけしたものも)。

### 書き足していくファイル (つづいた空きをさがす、葉をその場で)

書き足すブロックは、ひとつ前のブロックの隣 (goal) をねらう。前は goal が使われていると、そのあとのいちばん近い空きを
1 つとったので、すき間の 1 ブロックの穴をひろい歩いた: aios.img の上で 4 KiB ずつ 16 MiB 書き足したファイルが 47 の
extent になった。いまは Linux の mballoc (ext4_mb_normalize_request) のように、goal が使われていたら、ファイルの
ここまでの大きさ (2 のべき、8 MiB まで) くらいつづいて空いているところのはじめをとる (`alloc_block_run`。なければ
見つかったうちでいちばん長いところ。空きの数が足りないグループは見ない)。そのあとはまた goal が空いているので、extent は
そのまま伸びる。fallocate も、とる長さぶんの空きをさがす。同じ 16 MiB は 6 の extent (そのあとのファイルも 5)。

extent が inode に入りきらない (5 つ以上の) ファイルでは、書き足すたびに葉のブロックの最後の extent をのばす。前は
葉の 4 KiB を写して書きもどし、そのたびに crc32c を計算していた (1 回 18 µs)。いまは葉をキャッシュの上でその場で
書きかえ (`ext_extend_leaf`)、チェックサムはディレクトリのブロックと同じく、ジャーナルやディスクへ写す直前に 1 回だけ
(`Cache::lazy_csum`。Linux は書きかえるたびに計算する): 4 µs。新しいブロックのページを 0 にするのも kalloc の 1 度だけ
(前は 3 度)。大きなロックなしの pwrite が書いたページに書き出す印をつけるの (`take_fast_writes`) は、inode ごと・
ページの順に並べ、inode は 1 度だけ読み、同じ extent の中は木をたどりなおさない (前は列がいっぱいの 4096 ページで 12 ms)。

`append` (16 MiB を 4 KiB ずつ書き足して、3 回上書き) の 1 回の書き足しは、aios.img の上で 1 回目が 94〜111 µs から
68〜77 µs に、2 回目 (1 回目のファイルを消したあと) が 56〜77 µs から 50〜55 µs に。同じ QEMU (cortex-a72) の Linux 7.3
(作ったばかりの ext4) は 55〜68 µs。上書きはどちらも 11〜14 µs だが、いちばん長く止まる 1 回は Linux の 0.3〜1 ms に
対して aios は 9〜21 ms (大きなロックなしの pwrite の列がいっぱいになったとき): 次に見るところ。

### 上書きで止めない (印は少しずつ、ページはそのままディスクへ)

その長く止まる 1 回は、2 つが重なっていた。1 つは、大きなロックなしの pwrite が書いたページの列 (vm.rs の `written`)
が 4096 ページでいっぱいになると、次の 1 回が大きなロックの道へ行き、4096 ページにまとめて書き出す印をつけていた
(`take_fast_writes`、7 ms)。Linux は書くたびにページに PG_dirty をつけるので、まとめて待たせない。いまは列を 512 ページ
(2 MiB) にして、少しずつ (1 回 1 ms ほど。あわせての手間は同じ)。もう 1 つは、書き出しのスレッドが大きなロックを持ったまま
長く走っていたこと: 書くブロックを集めるとき、ページを 1 つずつ表から引き、書いている印 (writeback) の木に 1 つずつ入れて
(3〜4 ms)、書き終わると 1 つずつ外し (2 ms)、中身は 1 MiB ずつバッファに写してから書いていた (1 MiB で 0.4 ms、
はじめてさわるメモリなら QEMU で 10 ms)。いまは、ページは番号の順に表といっしょに歩いて拾い、書いている印はつづいたところ
(`Ranges`) で持ち、終わったらまとめて空に。中身は写さずに、キャッシュのページをそのまま virtio-blk に渡す (scatter-gather:
キューを 8 から 256 にして、1 つの要求に 254 ページまで。デバイスの seg_max まで。言わなければ Linux と同じく 1 つずつで、
写して書く。SD も写して書く)。

`append` の上書きでいちばん長く止まる 1 回は、CPU 1 つの VM (Linux とくらべるときと同じ) で 9〜22 ms から 2〜3.5 ms
に (Linux 7.3 は 0.3〜1 ms)。上書きの 1 回あたりも 13〜20 µs から 11〜14 µs に。CPU 4 つでは、ふつうは 2〜6 ms で、
ときどき 10〜20 ms がのこる (止まっているあいだ、その CPU にはカーネルの出来事がなにもない: QEMU の CPU のスレッドが、
ホストの 4 つの CPU を QEMU のディスクの仕事と取りあっているのだと思う)。書いた中身は、起こしなおして読みなおしても同じ (sha256)。

### 棚にないファイルも大きなロックなしで開く (名前の表と ext4 の inode から)

fsbench の open-close (作ったファイルを開いて閉じる) が、10/21 の 17.5 µs から 57〜62 µs になっていた (Linux 7.3 は 23.6 µs)。
1 つずつ戻してたどると d89a5c7 から: 大きなロックなしで作ったファイルを statc の表に入れるようにしたので、stat は
ロックなしで答えるようになったが、そのぶん、パスの覚え (vfs の `PATHS`、大きなロックの中のもの) に一度も入らない。大きな
ロックなしの open は、閉じたファイルの棚 (`file::Shelf`) にあるものしか出せないので、はじめて開くものは大きなロックの道で
パスをたどりなおしていた (vfs:lookup 27 µs)。Linux の open は、dcache と inode だけでロックなしに開く。

いまは、棚になければ `fastdir::fast_open` が新しく開く: statc の表がそのパスはリンクをたどらずに (表の印 plain: たどった
パスがパスそのもの) ext4 のふつうのファイルに着くと答えたら、親のディレクトリも表で引き、ext4 のロック (いっしょに) と
そのディレクトリのロックの中で、名前がいまもその inode をさし、呼んだ人が読める (書ける) ことを ext4 の名前と inode の覚え
から確かめて (`ExtFs::fast_open`)、ふつうの道と同じ OpenFile を fd に置く。消すものとはすれちがわない: ロックなしの unlink は
同じディレクトリのロックの中で、だれも使っていないことを確かめてから消し、大きなロックの道の unlink と rename は ext4 のロックを
ひとりで持つ。書く口 (O_WRONLY、O_RDWR) も同じ道で: 動かしているプログラム (ETXTBSY) かは、プロセスをなめずに、
アドレス空間を作る・落とすたびに数える inode の箱を見る (`proc::maybe_executing`。Linux の i_writecount のように)。
読むだけのファイルシステム、変えられない・足すだけのファイル、リンクを通るパスはふつうの道へ。書いたあとは inode の版が
進んで表の stat が古くなるが、開くのに要るのは名前と inode の番号だけなので、新しく開く道は使える (許しは ext4 の中で
見なおす。棚から出すのは stat がいまのものときだけ)。ただし読むだけの口で開くときは、表の stat が古ければ stat の道と同じく
いまの inode のものに書きなおしてから (`statc::open_now`: inode の覚えか、親のディレクトリのロックの中で ext4 から)
棚を見る。前は書きなおさなかったので、名前を覚えたあとに書いたファイルは、新しく開く道がいつもうまくいき、だれも
覚えなおさず、開きなおすたびに新しく開いて、閉じたものは棚をふさいでいた (test/opencache.c を 2 度動かすと 2 度目に
reopen-shelf が 0 of 200 で落ちた。いまは 1 度目から、覚えたあとに書きなおしたファイルで確かめる)。statc の表は 4096 から 8192 (4 MiB) に: fsbench の 1000 のファイルと
ないもの 1000 で 3000 ほどになると、組の 4 つからあふれた 16% がふつうの道 (50 µs) へ行っていた。

はじめての open-close は 55 µs から 23 µs に、fsbench の open-close は 62 µs から 22〜24 µs に (Linux 7.3 は 23.6 µs)。
書く口で開くものがパスの覚えを引かなくなったので、write-4k も 120 µs から 92〜126 µs に (Linux は 140 µs)。答えは
test/openfast.c (リンクのディレクトリを通ったものの /proc/self/fd、許し、ETXTBSY、置きかえと消したもの。どれも 2 回目は
ロックなしの道で、Linux と同じ) と test/openrace.c (開くのと、ロックなしの unlink と作りなおし、rename での置きかえを
CPU 4 つで同時に。開いたものはいつも同じファイルの中身) で確かめる。閉じるのは、棚がいっぱいのときと書く口のものは
大きなロックの道のまま (flock や inotify の表をさわる)。

### 名前は表から、中身は inode から (開いたディレクトリからの stat と、書いたあとの stat)

開いたものがパスの覚えに入らなくなると、開いているディレクトリからの fstatat (find、ls -l、rm -r、fts の引きかた) が
パスの覚えを引けなくなって、fsbench の stat-at が 12〜24 µs から 51〜62 µs になった (Linux 7.3 は 11.6 µs)。statc の表は
いまのディレクトリか絶対のパスからのものしか引かなかったので、開いているディレクトリ (dirfd) のパスをつないで引くように
した (`statc::dir_base`): fd の表を自分だけで使っているときに、その OpenFile のパスがいまも表でそのディレクトリの inode
をさすときだけ (開いたあとで名前を変えて、前の名前に別のディレクトリを作っても、前のほうを引く)。openat の dirfd も同じ。

もう 1 つは、書いたあとの stat。表は stat まるごとを覚えているので、inode を書いて版が進むと古くなり、ふつうの道へ行って
いた。Linux の stat は dentry の先の inode から読むので、名前の答えが残っていれば速い。いまは、名前と inode の番号は
正しいので、ext4 が親のディレクトリのロックの中で名前がいまもその inode をさすことを確かめて、inode の覚え (meta() と同じ
もの) からいまの stat を読み (`statc::now_stat`、`ExtFs::fast_meta`)、表の stat も書きなおす (次はそのまま当たる)。

CPU 1 つで、書いたファイル 1000 の fstatat は 1 回目が 42 µs から 14 µs、2 回目が 10 µs から 7 µs に、書いたあとの stat が
18 µs から 12 µs に。test/openfast.c に、開いたディレクトリからのもの (名前を変えたあとも) と、書いたあとの stat の
大きさと時刻が fstat と同じことを足した。

### はじめて引く名前も大きなロックなしで (ないもの、あるもの)

表にない名前 (はじめての stat、とくにないものの stat: シェルの PATH さがし、ライブラリや #include のさがし) は、大きな
ロックの道でパスの覚えを引いてからたどっていた (fsbench の stat-missing は 47〜62 µs、Linux 7.3 は 52.6 µs)。Linux の
lookup_slow は、親のディレクトリの i_rwsem をいっしょに持って ->lookup を呼び、dentry (ないものも) を作る。いまは、
親のディレクトリが表にあれば `statc::learn_lookup` が、ext4 のロック (いっしょに) とそのディレクトリのロックの中で名前を
1 つ引き (`ExtFs::fast_lookup`。名前の覚えになければディレクトリのブロックから)、「ない」かその stat を答えて、表にも書く
(親の答えの印に、(ディレクトリ, 名前) の箱と引く前に読んだその版を足したもの。全体の版も引く前に読む: 引いたあとで作ったり
マウントしたりしたものは、書くときに落とすか、書いたものが古くなる)。リンクをたどるもの、マウント先 (かぶせたものの根が
答え。はじめの namespace のマウント先は、マウントの表を変えるたびに `vfs::is_mount_at` の表に写す。全体の版を進める前に)、
呼んだ人が通れないディレクトリの下は、ふつうの道へ。/proc/ai/why のわけも、ふつうの道がたどって見つけなかったときと同じ
(「... exists but has no entry named ...」)。

fsbench の stat-missing は 47〜49 µs から 24〜29 µs に (Linux 7.3 の半分)。test/openfast.c に、はじめて引く名前 (ない
もの、あるもの、リンク、通れないディレクトリの下) とマウント先 (/tmp、/dev、/run、/proc、/sys の stat が、開いたものと同じ
inode) を足した。

書いたあとの stat (表の stat が古いもの) は、まず ext4 の inode の覚え (metas) を、ディレクトリのロックなしで読む。読んだ
あとで、表の答えが書きなおされておらず (seq)、通ったところの版もいまも同じなら、名前はいまもその inode をさしている:
inode を返すものは、みな先に (ディレクトリ, 名前) の版を進める (ふつうの道の unlink と rename はもとから。ロックなしの
unlink も、ディレクトリのロックの中で inode を返す前に進めるようにした)。なので、返した inode をほかのファイルが使っても、
その stat を消した名前の答えにすることはない (Linux の seqcount と同じ考え)。覚えになければ、前のとおりディレクトリの
ロックの中で名前を確かめる。書いたファイル 1000 の stat は 13〜23 µs から 8〜9 µs に、fstatat の 1 回目は 15 から 12 µs に。
test/openrace.c は、ファイルの大きさを 100 + i にして、名前の stat がほかのファイルの大きさを答えないかも見る。

### futex の待ちの行列 (key のハッシュの箱と、スレッドごとの眠る場所)

Firefox を 4 つの CPU で動かすと、大きなロックをいちばん長く持っていたのは futex だった (3673 回、198 ms、1 回 54 µs)。
1 回のうち、起こす相手をさがすのに 36 µs、行列から出すのに 17 µs、入れるのに 15 µs。futex で待つスレッドはみな同じ
chan で眠っていたので、眠っているものの並びの 1 つの箱に futex で待つものがみな入り、起こすたびにそれをみなたどっていた。
行列も key ごとの BTreeMap と VecDeque、入れたものの BTreeMap、起こされたものの BTreeMap で、待つ・起こすたびに木を
いくつもたどってメモリを取っていた。

Linux は、待つもの (futex_q) を key のハッシュの箱 (futex_hash_bucket) に来た順につなぎ、起こすときは行列から出した
そのスレッドを起こす。いまは同じ形: 待つものは表の slab に置いて (空いた場所は使いまわす)、key のハッシュの 256 の箱に前後で
つなぐ (同じ箱にちがう key もまざるので key で選ぶ。REQUEUE はつなぎかえるだけ)。スレッドは Proc の表の場所ごとの chan で
眠り、起こされた印もその場所ごとの表に書く (futex_waitv の、起こされたものの番号)。待つ側は slab の場所と入れたときの番号を
持っていて、まだ自分のものなら出す。

Firefox (4 つの CPU) の futex は 1 回 54 µs から 37 µs に (合計 198 ms から 137 ms に)。のこりのほとんどは、眠っている CPU
を起こす IPI (GIC への書きこみ。QEMU の TCG では重い)。新しい test/futexrace.c は、mutex の取りあい、cond で受けわたす行列
(signal と broadcast)、時間切れで終わる待ち、生の CMP_REQUEUE を、いくつもの CPU で同時に動かす (起こしが消えれば alarm で
失敗)。

### robust futex (set_robust_list と、持ったまま終わったもの)

pthread の robust mutex (PTHREAD_MUTEX_ROBUST) は、持ったまま死んだスレッドがいても、次に取ったものが EOWNERDEAD を受けて
直せるようにするしくみ。glibc はスレッドのはじめと fork した子で set_robust_list に robust list の頭 (struct robust_list_head:
list の next、futex_offset、list_op_pending) を渡し、持っている robust mutex をその list につなぐ。前の aios は長さを見て 0 を
返すだけだったので、持ったまま死んだスレッドのものは取れず、待っているものはいつまでも眠った (c374c01)。

いまは Linux 7.4 の kernel/futex/core.c と同じ: Proc の robust_list に頭を覚え (syscall::fast でも)、スレッドが終わるとき
(clear_child_tid より先: futex_exit_release) と exec のとき (前のアドレス空間で: futex_exec_release) に list をたどる
(futex::exit_robust_list。2048 で止める。番地の 1 ビットめは PI の印、取りかけの list_op_pending は 2 度見ない)。
自分の tid の入った語には FUTEX_OWNER_DIED をつけ (FUTEX_WAITERS はのこす。cmpxchg で、ほかの CPU が変えたら読みなおす)、
PI でなく待っているものがあれば 1 つ起こす (共有の key で: glibc の robust mutex はいつも FUTEX_PRIVATE_FLAG なしで待つ)。
取りかけで持ち主のいない PI でない語は、変えずに起こすだけ (handle_futex_death)。fork の子は 0 から。get_robust_list は
自分のものと、ptrace で読めるもの (本当の uid と gid が相手の 3 つにみな合うか CAP_SYS_PTRACE、それから AppArmor の ptrace
の read) だけ (EPERM、ESRCH、EFAULT)。`test/robust.c` (14 行: 持ったまま終わるスレッド、待っているもの、MAP_SHARED の
プロセスの _exit・SIGKILL・exec、待っているプロセス、手で作った list の自分のもの・ほかのもの・PI・取りかけ、set と get の
まちがい) が Linux 7.4-rc1 と 1 バイトもちがわない。

### 自分だけの無名の mmap / munmap は大きなロックなしで

smpbench を 4 つの CPU で動かすと、大きなロックを待つ時間のいちばん多いものの 1 つは、無名の領域を作って外すだけの
mmap (待ち 88%: munmap 1 回 10 µs、mmap 4 µs) と、領域を何千も持ったプロセスの mmap-busy (226%) だった。それぞれ
自分のアドレス空間なのに、領域の木 (PageTable の vmas) と PTE を変えるのはみな大きなロックの中だった。

ロックなしにしたのは、スレッドで分けていない表 (Mm をだれとも分けていない) の、名前のない MAP_PRIVATE の無名の mmap
(MAP_FIXED でないもの。MAP_NORESERVE、MAP_STACK はよい) と、名前のない自分だけの無名の領域だけにかかり、スワップへ出した
ページのない munmap (vm::PageTable::fast_map、fast_unmap)。ファイルの写し、共有、名前 (Rc)、スワップのスロットは大きなロックで
守っているので、それがまざればふつうの道へ。スレッドがほかにいなければ、同じ表をいっしょに変えうるのは、大きなロックを
持ってほかから来るもの (メモリが足りないときの回収のスワップとファイルのページ、procfs、oom) だけ。それとはロックなしの
ページフォールトと同じ形ですれちがう: ロックなしの側は fast_users を増やしてから mutators を見て、0 でなければふつうの道へ。
大きなロックの側は mutators を増やしてから fast_users が 0 になるのを待つ。表を変えるメソッドはもとから中でこれをとっていたが、
ほかのプロセスの表を読むだけのもの (/proc/PID/maps、cmdline、stat、status、/proc/ai のプロセスの一覧、oom の大きさくらべ、
スタックや待っている futex の出力) はとっていなかったので、読むあいだ `hold` で止める。munmap は PTE を消してから、表を
載せている CPU を見て TLB を消し (ほかに載せている CPU がなければ自分の分だけ)、ページを返す。ほかのスレッドがいないので、
ロックなしで読む CPU を待つ (quiesce) ことはいらない (自分がそれなので、待てば止まる)。

4 つの CPU で、mmap は 1 回 19.7 µs から 14.9 µs、待ち 97% から 15%。mmap-busy は 30.0 µs から 14.6 µs、伸びは 1.04 から
1.39 に (Linux は 1.21)。新しい test/fastmap.c は、外した真ん中 (領域を分ける)、いくつもの領域と穴にかかる munmap、fork の
あとの COW のページ、ファイルの写しをまたぐもの (ふつうの道へ)、外したところの SIGSEGV と、3 つのプロセスが 3000 回ずつ作って
外すあいだに、親がそれぞれの /proc/PID/maps、stat、status を読みつづけるのを確かめる。のこりの大きなものは fork (clone と wait4)。

### fork のページ表は大きなロックの前に写し、exit と exec で返すのはロックを放してから

4 つの CPU の smpbench の fork (fork して子は _exit、親は waitpid) は、大きなロックを待つ時間が 257% で、clone が 1 回
250 µs、wait4 が 84 µs ロックを持っていた。どこに使っているかを分けるため、clone、exit、wait4 の中に prof の区間を入れた
(proc:fork_pt、fork_files、fork_slot、fork_run、exit_kids、exit_files、exit_mm、exit_notify、wait_scan、reap と、システム
コールの syscall:clone、syscall:wait4)。1 回の fork で、clone は 150 µs (ページ表の PTE を写すのが 73、子のスロットを作るのが
44 (そのうち 14 は子の tid を書くときの COW)、fd の表が 7)、exit は 121 µs (ページ表のページを返すのが 68、親に知らせるのが
40)、wait4 は 60 µs ほど。半分ちかくが、そのプロセスだけのページ表を写すのと返すのだった。

exit で返すほうは、はじめにページを返すあいだだけ大きなロックを放して取りなおしてみたが、取りなおすときにほかの CPU の
うしろに並びなおすので、exit_mm の時間は 68 µs から 192 µs にのび、伸びは少ししか変わらなかった。いまは、そのプロセスだけの
表 (Mm をだれとも分けていない) なら、ロックの中ではページ表の根を外すだけにして (vm::PageTable::release_deferred)、ページと
テーブルを返すのは、その CPU がどのみちロックを放すところ (ユーザーへ戻る、新しい子が forkret から走りだす、暇で眠る:
free_deferred) でする。Linux が exit した mm を次の切りかえのあとで捨てる (mmdrop) のと同じ。外すのは mm を None にするのと
同じロックの中なので、ほかの CPU (procfs など) が半分返した表を見ることはない。ページは kalloc (スピンロックと atomic の数) で
返せるが、スワップのスロットは大きなロックの中のものなので、そのときだけ取る。領域 (ファイルの Rc) はロックの中で捨てる。
この表をいま使っている人がいるか (mutating のまま眠っているもの、fast_users)、MAP_SHARED の写しの表にこの写しのページが
あれば (ページを返したあとに release_shared がいる) いつもの Drop で。fork した子が exec したときに捨てる前の表も同じ。

fork で写すほうは、大きなロックを取る前の syscall::fast で PTE を写した新しい表を作っておき (prefork。自分だけの表のとき)、
clone はロックを取ってから、領域 (ファイルの Rc は大きなロックの中) を写すだけにする (fork_from)。写すあいだはロックなしの
mmap と同じく fast_users を増やしておくので、そのあいだに大きなロックを持ってこの表を変えたり読んだりするもの (スワップの
追い出し、/proc/PID/maps、cmdline) は、写し終わるのを待つ。表を変えている人がいる、スワップへ出したページがある、メモリが
足りないときは作らずに、いつもの fork へ。親の PTE を書きこみ禁止 (COW) にしたので、TLB はそこで消す。clone が使わなかった
もの (clone が早くにしくじった、わざとしくじらせた) は、その例外のおしまいに trap.rs が捨てる (次の fork が古い写しを使わない
ように)。

4 つの CPU で、clone がロックを持つ時間は 1 回 250 µs から 81 µs、wait4 は 84 µs から 46 µs (TCG の上なのでゆれる)。fork は
1 つのプロセスで 578 µs から 480 µs、4 つで 361 µs から 328 µs (Linux 7.3 は 2941 µs と 1156 µs)。ロックを待つ時間はまだ 200%
あるので、のこりの子のスロット (alloc_proc と Proc を空に戻すこと)、親に知らせる (SIGCHLD と 2 つの wakeup)、wait4 の子さがしが
つぎ。新しい test/forkcow.c は、子が fork したときの中身を見ること (親があとで書いても)、子が書いても親が変わらないこと、
MAP_SHARED の無名とファイルの写しは親と子で同じページなこと、fork した子の exec、3 つのプロセスが 300 回ずつ fork と書きこみを
くりかえすあいだに親がそれぞれの /proc/PID/maps、status、cmdline を読むのを確かめる。

そのつづき (752aca9): glibc が fork した子とスレッドのはじめにかならず呼ぶ set_robust_list は、0 を返すだけなのに大きなロックを
取っていた (fork のたびに 7.5 µs): syscall::fast で答える (長さが 24 でなければ、ふつうの道で EINVAL)。CLONE_CHILD_SETTID の
書きこみ (fork の子ではそのページの COW: 14 µs) は、親の clone からはずして、子がはじめて走るとき (forkret) にロックを放して
から、ロックなしのフォールト (fast_fault) で写して書く (Linux の schedule_tail と同じ)。子のスロットは NPROC の表を頭から
見て、取ったものを Proc::UNUSED で書きなおしていたのを、LIVE のビットの 0 から取って書きなおさない (free_slot がもう空にした)。
clone がロックを持つ時間は 90 µs から 67 µs。`smpbench -a -o fork` と prof で分けた 1 回の fork のロックの中は、clone 61
(領域の写し 21.6、スロット 13.5、fd の表 6.9)、exit 80 (exit_notify 35、exit_mm 25: release_deferred 10、Mm を捨てる (領域の
木とファイルの Rc。ロックの外では捨てられない) 6、deactivate の TLB 4、exit_files 7〜10)、wait4 (子さがし 13 を眠る前と
起きたあとに 2 回、reap 13.5)。どれも 4〜14 µs のこまかいもので、fork はもう 4 つの CPU で Linux 7.3 の 4 分の 1 ほどの時間
(260〜330 µs と 1156 µs) なので、ここはいったん止める。のこり: wait4 の子さがしを親ごとの子の並びに、exit で init を毎回
起こさない (子を init にわたしたときだけ)、親を起こす wakeup が 2 回。

そのつづき (子の並び): wait4 は、待つたびに生きている Proc をみな見て自分の子を集め、番号の順にならべていた (眠る前と
起きたあとで 2 回、1 回 15 µs)。いまは Linux の children / sibling と同じく、親 (代表スレッド) が子の並び (Proc::kids。子は
Proc::sib でつなぐ) を持ち、clone で末尾につなぎ、free_slot で外す。wait4 はその並びを古い子から見るだけ (prof の
proc:wait_scan はなくした)。親が終わるときは、子を同じ順で init の並びの末尾へうつす (Linux の forget_original_parent)。
exit は、親を表から 2 回探して SIGCHLD を送り、親と、いつも init を起こしていた: 親は子の並びのつなぎから取り、init を
起こすのは終わっている (ゾンビの) 子をわたしたとき (Linux と同じく init に SIGCHLD も) だけ。ハンドラのない SIGCHLD
(無視される) は、send_group がスレッドを集めてから捨てていたので、代表がブロックしていなければ集めずに捨てる
(signal::send_chld)。直していて Linux とのちがいも 1 つ見つけた: 親が終わって init にわたされた子のスレッドの getppid が、
前の親のままだった (代表の ppid だけを 1 にしていた。Linux はスレッドみなの real_parent を変える)。

test/procwait.c の kids は、待てる子だけを古い順に (動いている子はとばす)、スレッドが fork した子を代表スレッドが待つのと
その逆、子のないスレッドの ECHILD、親が終わったあとの孫の getppid (代表スレッドとスレッド) と /proc/PID/status の PPid、
init にわたされたゾンビを確かめる。Linux 7.4-rc1 と同じ 109 checks, 0 failed (前のカーネルは getppid で 1 つ落ちる)。
`smpbench -a -o fork` の prof で、1 回の fork から wait4 の子さがし (15 µs を 2 回) がなくなり、exit_notify は 38 µs から 18 µs、
wakeup は 2.1 回から 1.1 回、exit_kids は 4.1 µs から 1.5 µs。bench smp の fork-x4 は 319 µs から 234 µs (伸び 1.6 から 2.1)、
wait4 がロックを持つ時間は 1 回 68 µs から 37 µs、ロックを待つ時間は 199% から 178%。

たしかめているあいだに test/random.c が一度だけ「AT_RANDOM が exec ごとにちがわない」で落ちた。見ると、AT_HWCAP2 を
足して auxv が 18 になったのに、スタックの並びの大きさは 17 のままで、おしまいの AT_NULL (0 が 16 バイト) が AT_RANDOM の
16 バイトに重なり、そのほとんどを 0 にしていた (ずれは環境の長さによる。glibc はここからスタックの番人をとる)。いまは数と
並びの型を 1 つの定数にした。test/random.c は AT_RANDOM に 0 のバイトが 4 つ以上ないことも見る。

### ファイルの写しの munmap と、ページ表を作るフォルトも大きなロックの外へ

bench smp の map-file (1 MiB のファイルを読むだけで MAP_PRIVATE に写し、256 ページにさわって munmap) は、4 つの CPU で
大きなロックを待つ時間が 174% あった。prof で分けると、1 回のうちロックの中は munmap が 58〜66 µs (vm:unmap)、mmap が 22〜29 µs、
ページフォルトが 1 回に 0.6 回ほど 33〜48 µs だった。フォルトのほとんど (fault-around で 16 ページずつ) はもうロックなしの
fast_fault だが、mmap がいつも先へ写す (mmap_next) ので 2 MiB ごとにまだ L3 のテーブルがなく、テーブルを作るのはロックの道
だった。bench のロックの表のいちばん上の pwrite64 (1 回 100〜200 µs) は、smpbench がはじめに 1 MiB のファイルを書いて伸ばす
ところで、測っているループの外 (ext4 で伸ばす書きこみ。todo.json の ext4-extend-write)。

munmap: スレッドで分けていない表の、自分だけの (MAP_PRIVATE の) ファイルの写しも、まるごと外すならロックなしの
fast_unmap で外す。ページキャッシュのページは参照を返すだけで、無名のときと同じ (PTE を消し、TLB を消し、ページを返す)。
ちがうのは、領域が持つ Rc (inode と /proc/PID/maps の名前) と、MAP_PRIVATE の穴の 0 のページの写しの表 (SHARED_FILE):
どちらも大きなロックの中でだけ変えるので、外した領域はそのまま vm::UNMAPPED にあずけ、次に大きなロックを取ったシステム
コールかフォルトの入口 (trap.rs。割り込みではなくプロセスの中: 消したファイルの inode を最後に放すとディスクにさわる) で
drop_unmapped が捨てる。一部だけ外す (領域を分けると Rc を写す)、共有、スワップへ出したページのあるところ、あずけたものが
1024 をこえたときは、いままでどおりロックの道。フォルト: fast_fault が途中のテーブルを作る (walk_fast: 0 にしたページを
compare-exchange でつなぎ、同じ表のスレッドに負けたら返す。テーブルは表を捨てるまで返さないので、つないだものはそのまま
使える)。ファイルの mmap は、inode の Rc を写すのでロックの道のまま (todo.json の file-mmap-fast)。

test/fastmap.c に、ファイルの写しの munmap (まるごと、一部、書いた写し、無名の領域とならぶもの、穴の 0 のページを外して
からファイルに書いて写しなおす、3000 回、消したファイルの写しを外すと inode が片づく (statvfs の f_ffree がもどる)) を足した。
aios (tmpfs と ext4) でも Linux 7.4-rc1 でも 0 failed。4 つの CPU の map-file で、ロックの表から munmap とフォルトが消え、
ロックを待つ時間は 174% から 115%。かかる時間は 1 つで 272〜292 µs と前 (285〜320) とかわらず、4 つでまんなか 181 µs から
165 µs ほど (のこりは TCG で全部の CPU に TLB を消させるところと、ページキャッシュのスピンロック)。

### munmap で空になったページ表を返す (Linux の free_pgtables)

ページ表のテーブル (L3 は 2 MiB、L2 は 1 GiB ぶん) は、作ったら表を捨てる (exit、exec) まで返さなかった。aios の mmap は
いつも先へ写す (mmap_next、MAXVA で頭にもどる) ので、mmap と munmap をくりかえすプロセスは 2 MiB ごとにテーブルをふやし、
1 MiB を 20000 回で 40 MB (MemFree がそれだけへったまま)、MAXVA (512 GiB) ぶんで 1 GiB までたまりえた。

いまは munmap (vm::PageTable::unmap と、ロックなしの fast_unmap) のあと、外した範囲にかかる 2 MiB と 1 GiB のまどのうち、どの
領域もかからないものの L3 / L2 を親から外して返す (unlink_tables)。PTE があるのは領域の中だけなので、そのテーブルは空 (念の
ため中を見て、空でなければ残す)。返す前に、表をたどる CPU のキャッシュ (walk cache) を TLB ごと消し (flush_all_in。ほかの CPU
が載せていなければ自分のものだけ)、大きなロックの道ではロックなしでたどっている CPU を待つ (quiesce: ページを返すときと同じ。
外したあとにたどりはじめた CPU は 0 を読む)。fast_fault がロックなしで作るテーブル (walk_fast) とは mutating ですれちがう。
返したまどをまた写せば、フォルトがテーブルを作りなおす。ただし、小さな mmap と munmap をくりかえすと、そのたびにテーブルを
返して作りなおすことになる (bench smp の mmap が 23 µs から 82 µs になった) ので、いちばん新しく写した領域の終わり (つぎの mmap
が来るところ) の 2 MiB のテーブルはとっておき、そこから動いたら次の unlink_tables で返す (とっておくのはいつも 1 つ)。Linux は
上から写して外したところをまた使うので、同じことが起きにくい。

/proc/meminfo に PageTables (テーブルのページの数: 根もふくむ) を足した。新しい test/pagetables.c (13 行: 1 MiB の mmap と
munmap を 20000 回、ファイルの写し 5000 回、スレッドがいるとき 5000 回で PageTables が 256 kB もふえない、テーブルを返した
まどをまた写すと 0、となりの 2 MiB のまどの中身はそのまま、ほかのスレッドが読みつづけるあいだに 2000 回写して書いて外しても
0 か書いたものか SIGSEGV しか見えない) は Linux 7.4-rc1 と同じ。1 MiB を 20000 回のあとの MemFree のへりは 40080 kB から
4 kB (とっておいた 1 つ)。bench smp の mmap と mmap-busy は前とゆれの中で同じ。

### AppArmor (Linux と同じプロファイル)

AppArmor のプロファイル (文字で書いた決まり) は、Linux と同じく apparmor_parser がバイナリにして
/sys/kernel/security/apparmor/.load (足す) か .replace (足すか入れかえる) に書き、.remove に名前を書くと外す
(kernel/src/apparmor.rs、securityfs.rs)。バイナリのほどき方は Linux 7.3 の policy_unpack.c と同じ (型の印のついた値の
並び)、ファイルの決まりは DFA (match.c。表は big endian) で、パスを通してたどりついた状態の許しを見る。apparmor_parser は
カーネルが名のること (apparmor/features) を読んで形を選ぶので、aios は v5〜v7 の版と許しの表 (permstable32 の 3。
Linux 7.3 も名のる)、ファイル (create read write exec append mmap_exec link lock)、capability (caps/mask)、ネットワーク
(network_v8 と、network_v9 の af_unix と af_inet)、signal、ptrace、mount、change_hat、change_profile と change_onexec を
名のる。apparmor_parser 5.0 は許しを別の表 (perms) で作り、DFA の accept はその番号になる。許しの表を知らない古い
apparmor_parser は許しを DFA の accept の表にうめこんだ形で作るので、policy_compat.c と同じく読むときに許しの表へ移す
(持ち主のときとほかのときの 2 つ組)。表は読むときに
みな確かめる (状態と行き先が表の中、差分の鎖がまわっていない、許しの番号が表の中: Linux の paranoid_load と同じ)。
.access には「このプロファイルでこの文字列は」を聞ける (libapparmor の aa_query_label)。/sys/module/apparmor/parameters/enabled
は Y、/sys/kernel/security/lsm は capability,landlock,apparmor (securityfs は起動のときにかぶせる)。apparmor/policy/ は Linux
の名前空間のディレクトリと同じ形で、profiles/NAME.N/ に name、mode、attach (子は その下の profiles/。N は読みこんだ順の
番号で、入れかえても変わらない: aa-status が見る)、namespaces/ と raw_data/ (どちらも空。バイナリはとっておかない)、
revision と .load .replace .remove がある。Linux 6.8 の LSM のシステムコール lsm_get_self_attr、lsm_set_self_attr、
lsm_list_modules もある (libapparmor は /proc/self/attr より先にこれを使う。答えるのは AppArmor だけで、中身は
/proc/self/attr と同じ「名前 (モード)」の改行なし。番号は capability 100、apparmor 104、landlock 110)。

プロセスのラベル (cred の apparmor) は fork で受けつぎ、使うたびにプロファイルを名前で引く (入れかえれば新しいものに、
外されれば unconfined に: Linux の proxy と同じ)。exec では、決まりのないプロセスならパスでつくもの (attach。いちばん
長く文字どおりに合ったもの、同じ長さならどれにもつけない: find_attach と leftmatch)、決まりのあるものなら exec の許しの
行き先 (ix はそのまま、px と cx は名前か子のプロファイル、ux は unconfined、見つからなければ ix か ux へ。大文字は
AT_SECURE で、ld.so が LD_* を見ない)。#! のスクリプトは、Linux と同じくスクリプトのパスで 1 度だけ。/proc/PID/attr
(と attr/apparmor) の current は「名前 (モード)」、exec に "exec NAME" を書くと次の exec で (onexec)、current に
"changeprofile NAME" を書くといま移る (決まりのあるものは change_profile の許しが要る。no_new_privs ならほかへは移れない)。

change_hat: プロファイルの中に `^NAME { ... }` と書いた hat (子のプロファイル "親//NAME") には、current に
"changehat TOKEN^NAME" (TOKEN は 16 進) を書くと許しなしで入れる。hat はいくつも \0 で区切って書け (libapparmor の
aa_change_hatv)、はじめにあるものに入る。入るときに前のプロファイルと TOKEN を覚え (attr/prev に見える)、hat の中からは
兄弟の hat へ移れ、"changehat TOKEN^" で前のプロファイルに帰る。どちらも同じ TOKEN のときだけで、ちがえば
`apparmor="KILLED" operation="change_hat" info="failed token match"` と記録して SIGKILL (当てずっぽうをふせぐ。Linux と
同じ)。ない hat は ENOENT、hat でない子は EPERM、子のないプロファイルからは ECHILD、決まりのないプロセスからは EPERM
(記録するのは、hat をいくつもためしたときか complain のときだけ: ひとつだけさがすのは、あるかをためすのにも使うので)。
"permhat" は入れるかを見るだけ。exec で前のプロファイルと TOKEN は消え、fork では受けつぐ。

決まりを見るのは open (開く口の許し: 読む、書く、O_APPEND は a、O_CREAT は c)、作る (O_CREAT、mknod、mkdir、symlink、
AF_UNIX の bind)、消す (unlink、rmdir)、rename (前は読んで消す、新しいほうは作って書く)、link (新しい名前とペアの古い
名前に l。subset なら許しが古いものの中)、truncate、chmod、chown、stat (getattr)、実行して写す mmap (m)、ロック (k、
書くロックは w も)。ディレクトリは名前の終わりに / をつけて見る。enforce なら EACCES と DENIED、complain なら許して ALLOWED、
audit をつけたものは AUDIT、deny したものは complain でも止めて記録しない、kill なら SIGKILL (またはプロファイルの
signal)。記録は Linux の dmesg と同じ形 (`audit: type=1400 audit(...): apparmor="DENIED" operation="open" class="file"
profile=... name=... pid=... comm=... requested_mask="r" denied_mask="r" fsuid=... ouid=...`) で、audit の記録の口
(下の「監査 (NETLINK_AUDIT)」: auditd がいなければ kmsg) に。
プロファイルのついたプロセスは、大きなロックなしの道 (syscall::fast) を通らない。

capability: aios では root (euid 0) がみなの capability を持つが、root かどうかで決めていたところは、みな Linux と同じ
capability を問う `Cred::capable(CAP_*)` にまとめた (kernel/src/cred.rs)。ファイルの許しは generic_permission と同じ順
(mode のビットで足りなければ CAP_DAC_READ_SEARCH、それから CAP_DAC_OVERRIDE)、持ち主やグループで足りるところは先に
そちらを見て、足りないときだけ問う (chmod と utimes などの持ち主は CAP_FOWNER、sticky も、chown は CAP_CHOWN、setgid を
残すのと書くときの setuid / setgid は CAP_FSETID、mknod のデバイスは CAP_MKNOD、setuid の仲間は CAP_SETUID / CAP_SETGID、
kill は CAP_KILL、mount や swapon や sethostname や namespace は CAP_SYS_ADMIN、chroot は CAP_SYS_CHROOT、reboot は
CAP_SYS_BOOT、モジュールは CAP_SYS_MODULE、ネットワークの設定は CAP_NET_ADMIN、raw ソケットは CAP_NET_RAW、1024 より下の
ポートは CAP_NET_BIND_SERVICE、プロファイルを変えるのは CAP_MAC_ADMIN)。landlock と seccomp の no_new_privs のかわりは
Linux と同じく記録しない問い。プロファイルのついた root は、そのプロファイルの `capability` の許しにあるものだけを持つ
(capget の effective と permitted もそれだけ。complain ならみな)。許されなければ Linux と同じ形
(`apparmor="DENIED" operation="capable" class="cap" profile=... pid=... comm=... capability=1  capname="dac_override"`)
で記録し、同じプロセスの同じものは 1 秒のうちは記録しない。/proc/sys や /proc/ai の root のたしかめは、Linux でも
capability ではない (ファイルの持ち主と許し) のでそのまま。

ネットワーク (network_v8: net.c、af_inet.c、af_unix.c): ソケットの作る、bind、connect、listen、accept、送る、受ける
(read と write も)、getsockname と getpeername、sockopt、shutdown を、(family、type、protocol) の DFA で見る (socket(2) の
ときは family と型の範囲のあと、その family があるかより先: aios にない AF_INET6 なども EACCES)。features で名のると
apparmor_parser はどのプロファイルにも net の印を入れるので、ネットワークの決まりのないプロファイルはソケットをみな
止められる (Linux と同じ)。stream をつなぐときは自分が connect send receive、相手 (パスの名前でなければ) のソケットの
ラベルの決まりで accept send receive、dgram を送るときは自分が send、相手のソケットのラベルで receive を見る (ソケットは
作ったプロセスのラベルを持ち、受けた口は listen しているもののラベル)。AF_INET の番地は、決まりのあるときだけ af_inet.c と
同じに確かめて (短いものは EINVAL、ほかの family は EAFNOSUPPORT) 記録に出す (laddr lport faddr fport saddr src daddr dest)。
記録は `apparmor="DENIED" operation="sendmsg" class="net" ... family="inet" sock_type="dgram" protocol=17 requested="send"
denied="send"` (AF_UNIX は addr= と peer_addr= と peer=)。

AF_UNIX (af_unix.c の network_v9): features で af_unix を名のるので、apparmor_parser 5.0 からは unix の決まり
(`unix (bind, listen) type=stream addr=@NAME,` や `unix (send) type=dgram peer=(addr=@NAME, label=NAME),`) を v9 の形で作る。
作るのは (family、type、protocol) まで、bind accept getattr shutdown は自分の番地 (抽象名は \0 から、名前のない
ものは \x01) とラベルまで、listen と sockopt はそのあとの cmd (listen は backlog、sockopt は optname の 2 バイトも)、connect と
送る受けるは cmd 1 のあとの相手の番地と、相手のラベルの名前まで DFA をたどる (しくじったところが info に)。パスの名前の
ソケットは unix の決まりでなくファイルの決まりで見る (bind は mknod と同じ作る、connect はソケットのパスへ w と r など:
unix_fs_perm)。v9 の印のないプロファイル (network_v9 を知らない apparmor_parser で作ったもの) は v8 の形のまま。
apparmor_parser 5.0 は `network unix stream,` からも unix のすべてを許す決まりを作るので、AF_UNIX を細かく止めたいときは
unix の決まりで書く (4.0 などの古いものは、反対に unix の決まりを network unix に書きおろし、deny の unix の決まりは落とす)。

AF_INET (af_inet.c の network_v9。aios がえらんだちがい apparmor-inet、下の表): Linux 7.3 (と次の apparmor-next) の
af_inet.c は番地を記録に書くだけで、許すかは (inet, type, protocol) で決め、features でも af_inet を名のらない。
すると apparmor_parser は `network inet stream ip=127.0.0.1 port=8080,` や `peer=(ip=... port=...)` の ip= と port= を
落として (inet, type) の決まりにする: allow は書いたよりも広く許し、deny は広く止める (`deny network (connect) inet
stream peer=(port=25),` が stream の connect をみな止める)。aios は af_inet を名のり、apparmor_parser 5.0 が作る番地の
決まり (network.cc の gen_net_rule) を、それを読む Ubuntu のカーネルの af_inet.c と同じ順でたどる: (inet, type) で
足りれば止め、protocol、自分の番地 (種類、port、長さ、ip。bind で特権の port は種類 1)、ラベル (いまは区切りだけ)。
listen と sockopt はそのあとの cmd と backlog か level と optname、connect と送る受けるは相手の番地 (種類 2。名前の
ない send と recv は「なし」で、決まりの `peer=(ip=none)`)、CMD_ADDR、自分の番地。ICMP と raw のソケットも同じ。
aios は Ubuntu の読みかたの 3 つのまちがいもなおした: (1) parser は bind を許す決まりの特権の port をみな種類 1 で書くが、
Ubuntu は bind のほかは種類 0 で読むので、`network inet stream port=80,` で bind できても listen や accept ができない。aios は
自分の port が特権なら種類 0 と 1 の両方で読んで、許しを合わせる (どちらかが許し、どちらも拒まないもの)。(2) parser は
条件のある deny の決まりで (inet, type) の「先を見る」印 (AA_CONT_MATCH) まで deny にするので、許しから印が消え、Ubuntu
ではその型の op がみな止まる。aios は許しが足りなければ印がなくても先を読み、先がなかったときだけそこの許しにする
(v8 の形のプロファイルは (inet, type) で止まり、Linux と同じ)。(3) parser は自分の番地の条件のない決まりの自分の
ところを「種類と port はなんでも」で書くので、相手の番地もそこにあたり、Ubuntu は peer=(...) の決まりでもどの相手にも
許す。aios は相手の番地のところでは止めず、CMD_ADDR と自分の番地まで見て決める。DENIED の info= には、どこでたどれ
なくなったか (failed local addr match、failed cmd selection match など) が出る。

signal (ipc.c): kill と tgkill で、uid のたしかめ (EPERM) のあと、送る側の決まりで相手のラベルの名前へ send、受ける側の
決まりで送る側へ receive (両方見る。だめなら EACCES)。シグナルの番号は Linux の表で (0 は exists、リアルタイムは rtmin+N)。

ptrace (task.c): aios に ptrace はないので、/proc/PID の「追える人だけ」のもの (Linux の ptrace_may_access) に効かせる:
environ と maps を開く (mm_access: ほかに CAP_PERFMON か CAP_SYS_ADMIN でもよい)、exe cwd root fd ns のリンク (read)、
stack (trace)。追う側の決まりで相手へ read / trace、追われる側の決まりで readby / tracedby。自分のプロセスは見ない。

mount (mount.c): mount(2) で、マウント先をたどったあと、may_mount (EPERM) より先に (Linux の path_mount と同じ順。
決まりのないプロセスも、マウント先がなければ ENOENT が EPERM より先になった)。remount、bind、伝わり方、move、新しい
マウントで見るものを分け、マウント先、元、fs の種類、フラグ、データの順に DFA をたどる (しくじったところが info に)。
umount はマウントの根でなければ EINVAL のあと。aios に pivot_root はない。mount(2) は data (オプションの文字列) も
受けとるようにした (AppArmor の options= のため)。

network、signal、ptrace、mount の記録の形は、Linux 7.3.1 で同じことをして dmesg (klogctl) に出たものとくらべた
(shutdown の operation は "socket_shutdown"、AF_UNIX には path= を出さない、など)。

Linux とちがうところ (まだないもの): complain で exec の許しがないとき、Linux は学ぶための空のプロファイルに移るが、
aios はいまのプロファイルのまま (complain で、ない hat に change_hat したときも、Linux は学ぶための hat を作って入るが、
aios は記録して ENOENT)。xattr でつけるプロファイル (aios に拡張属性がない) はつかない。名前空間、重ねる (stack)、
rlimit、dbus、io_uring、userns、mqueue の決まりはまだで、features でも名のらない (ソケットのラベルが変わったときの見なおし
(aa_sock_file_perm) もまだ)。network_v9 の af_inet のラベルの条件 (label= と peer=(label=)) はまだ見ない (区切りだけたどる。Ubuntu も)。capability の
DFA の決まり (caps/extended) も名のらない (apparmor_parser は古い 64 ビットの組で作る)。mprotect と、exec のときに開いた
ままのファイルを見なおすこともまだ。

Linux は exec のあと、新しいプロファイルでプログラム (と ld.so) を写すときに見なおす (開いたときのラベルとちがえば
binfmt_elf の mmap で r と m が要る。決まりのないプロセスが開いたものは、わたされたものとして見ない)。なければ戻れない
ところの先で SIGSEGV。aios も同じに、exec の行き先のプロファイルで r と m を見て (exec_map_ok)、だめなら file_mmap の
DENIED を記録して、exec のあとで SIGSEGV を送る。ふつうのプロファイルが abstractions/base で ld.so の mr を持つのはこのため。

新しい test/apparmor.c は、test/apparmor/test.profile (gen.sh が apparmor_parser 5.0 で、aios の features をそのまま abi に
して blob.h の aa_blob に。Linux では af_inet をのぞいた features で作った aa_blob_linux を読む。4.0 は network_v9 の
af_unix を知らない) を
読み、写したテストのプログラムにパスでついたプロファイルの中から、開く・作る・消す・rename・link・chmod・truncate・flock・
exec (ix px cx ux、許しのないもの、行き先で写せないもの)・change_profile の 40 の答えをくらべ、capability を chown
だけ許すプロファイル (aacap) と complain で、ほかの人の 0600 のファイルを読む・書く・chmod、chown、mknod、setuid と capget
の 18 の答えと、change_hat (hat に入る・兄弟へ・帰る、いくつも、token ちがいの SIGKILL、ない hat、hat でないもの、
hat のないプロファイル) の 22 の答えと、LSM のシステムコールの 11 の答え、ネットワーク (aanet: 24 と、相手の側の
決まり 3)、AF_UNIX (aaunix: 番地、cmd、相手の番地とラベル、deny、パスのソケットの 24)、AF_INET (aainet: 番地、port の
範囲、deny、特権の port、dgram の 27。うち 13 は aios と Linux で答えがちがい、Linux のものは広がった決まりの答え)、
signal と ptrace (aasig: 13)、
mount (aamnt: 11。読むだけのマウントはしない: aios はそれがあるとロックなしの作る・消すを使わなくなり、あとの
ctests が変わる) の答えをくらべ、決まりのないプロセスから onexec、complain、動いているプロセスのプロファイル
を外す (unconfined にもどる) のを確かめる。答えは Linux で見たもの:
test/linux.config に AppArmor を入れ (プロファイルを読まなければ、どのプロセスにも決まりはない)、test/linux-vm.sh の
rootexec で同じプログラムを root で動かすと、Linux 7.3.1 でも同じに通る (はじめは Px と Cx の 2 つだけちがい、上の
mmap の見なおしだとわかった)。

AppArmor の道具は apparmor のパッケージ (pkg/c/apparmor。AppArmor 5.0.2 の apparmor_parser、aa-status、aa-exec、
aa-enabled、aa-features-abi、aa-load を静的に /opt/c/bin に。プロファイルを書くための /etc/apparmor.d の abi、tunables、
abstractions も):

```sh
sudo ap -S apparmor
sudo apparmor_parser -r /etc/apparmor.d/NAME     # 書いたプロファイルを読ませる (-R で外す)
sudo aa-status                                   # どのプロファイルが、どのプロセスにかかっているか
aa-exec -p NAME -- CMD                           # プロファイルをつけて動かす
```

/etc/apparmor.d に置いたプロファイルは、起動のたびに apparmor.service (パッケージが入れて有効にする) が
`apparmor_parser --replace --write-cache -- /etc/apparmor.d` で読む (disable/ に同じ名前のリンクがあるものはとばす)。作った
バイナリは /var/cache/apparmor に書き、プロファイルとカーネルの features が変わらなければ次からはそれを読む。Linux の
apparmor.service と同じく Before=sysinit.target で、aios の init は Before=sysinit.target のものをほかのサービスより先に
起こして終わるのを待つので、サービスのプロセスにもパスでつくプロファイルがかかる (init は Before=X を X の After= として
も読む)。作るときに直したところ: libapparmor の af_protos.h は標準入力の <netinet/in.h> を $(CC) -E で見るが、Zig の cc は
標準入力ではこのマシンの /usr/include を読む (musl にない IPPROTO_L2TP が入る) のでファイルにして先に作る、共用体の中の
可変長の配列 (GCC だけのもの) を定数の長さに、apparmor_parser の .c は C++ なので -x c++ で、configure.ac が autoconf-archive
から使う AX_CHECK_COMPILE_FLAG はパッケージに入れた。カーネルのほうでは /proc/PID/fd/N のリンクの大きさを Linux と同じ 64 に
した (0 だと、libapparmor の path_from_fd が lstat の大きさから readlink を始めて足りなければ倍にするので、0 のまま回り
つづけ、--write-cache が止まっていた)。

### 監査 (NETLINK_AUDIT)

Linux の kernel/audit.c と同じ口 (kernel/src/audit.rs)。auditctl -s や auditd が NETLINK_AUDIT (9) のソケットで監査の
設定を読み書きし、login や sudo (PAM の pam_unix など) がユーザーの記録を送る。記録はみな 1 つの番号の数えで
`audit(秒.ミリ秒:番号): ...` の形になり、auditd がいればそのソケットへ、いなければ kmsg へ `audit: type=N ...` (dmesg で
見える)。AppArmor の記録 (type=1400) もここを通る。

- 設定 (AUDIT_GET、AUDIT_SET、FEATURE、LIST_RULES、SIGNAL_INFO、TTY_GET / TTY_SET) は CAP_AUDIT_CONTROL、ユーザーの記録
  (AUDIT_USER と 1100〜1199、2100〜2999) は CAP_AUDIT_WRITE がいる (なければ EPERM)。user namespace の中からは
  ECONNREFUSED (Linux と同じく、PAM が「audit がない」と思ってログインを止めないように)、古い AUDIT_LIST / ADD / DEL は
  EOPNOTSUPP、知らないものは EINVAL
- AUDIT_SET は enabled (0、1、2 は固定)、failure (0 だまる、1 printk、2 panic)、pid、rate_limit、backlog_limit、
  backlog_wait_time の順。変えたものは AUDIT_CONFIG_CHANGE (1305: `op=set audit_rate_limit=100 old=0 auid=... ses=...
  subj=unconfined res=1`) に残るが、残すのは audit が on のときだけ (on にしたときは前が off なので残らず、off にしたときは
  残る)。mask が LOST だけなら、なくした数を 0 にしてその数を ACK のエラーの欄で返す (Linux の古い形)
- auditd になるのは AUDIT_SET の pid に自分の pid を入れたもの (ほかの pid は EINVAL、いる auditd をほかのものに変えるのは
  EEXIST、やめられるのは auditd だけで EACCES)。名乗るたびに、いまの auditd に AUDIT_REPLACE (1329) を送ってつながりを
  たしかめる。auditd への記録の nlmsg_len は中身だけの長さ (頭の 16 バイトを数えない Linux の古い形)、組
  AUDIT_NLGRP_READLOG (1: 入るのは CAP_AUDIT_READ。journald が読むもの) へはふつうの長さで写す。組は 32 まで入れる
  (netlink_kernel_create が 32 より少ない組の数を 32 にする。NETLINK_NETFILTER も同じ)。組に入る・出るのも記録に
  (AUDIT_EVENT_LISTENER 1335: `nl-mcgrp=1 op=connect res=1`。出るのは NETLINK_DROP_MEMBERSHIP と閉じるとき)
- ユーザーの記録は audit が off なら捨てる (AUDIT_USER_AVC だけは off でも残す)。1 バイトしかなければ EINVAL、終わりの
  1 バイトは NUL にする (`msg='hello audi'`: Linux と同じく送った文字の終わりが 1 つ消える。ふつうは NUL をつけて送る)
- AUDIT_SIGNAL_INFO は auditd に SIGTERM / SIGHUP / SIGUSR1 / SIGUSR2 を送った最後のもの (uid、pid、AppArmor のラベル)
- 答え (GET など) は ACK のあと (Linux は答えを別のスレッドから送る)。受け口の中で出た記録も ACK のあと (Linux は kauditd が
  行列から送る): auditd をやめた (pid=0) 記録は kmsg に、いる auditd を変えようとした EEXIST の記録は ACK のあとで auditd に
- kmsg へは 5 秒に 10 こまで (printk_ratelimit: こえたものは捨て、次の窓のはじめに `kauditd_printk_skb: N callbacks
  suppressed`)。AppArmor の記録がたくさん出ても dmesg があふれない

- loginuid と sessionid (/proc/PID/loginuid と sessionid: pam_loginuid が書く。記録の auid= ses=)。書けるのは自分の
  ものだけ (ほかは EPERM、途中からは EINVAL、数でなければ EINVAL、大きすぎれば ERANGE)。決まっていなければだれでも、
  決まっていれば CAP_AUDIT_CONTROL (AUDIT_SET_FEATURE の loginuid_immutable ならだれも、only_unset_loginuid なら外す
  だけ)。決めるたびに新しい sessionid (1 から)、4294967295 を書くと外す。fork で受けつぎ、exec でも setuid でも
  変わらない。audit が on なら AUDIT_LOGIN (1006: `old-auid=... auid=... old-ses=... ses=... res=1`)
- システムコールの入れもの (Linux の audit_context): audit を一度でも on にしたあとに作ったスレッドにつく。audit が on
  なら、システムコールの入口で番号と引数 4 つと時刻を覚え、そのあいだにそのスレッドのことの記録 (設定の変更、
  AUDIT_LOGIN、組の出入り、AppArmor など) が出れば、それらと同じ時刻と番号で、出口に SYSCALL (1300: `arch=c00000b7
  syscall=64 success=yes exit=4 a0=... items=0 ppid=... auid=... comm=... exe=... subj=... key=(null)`)、PROCTITLE
  (1327: 引数の 128 バイトまで。はじめて出すときに作り、exec したら作りなおす: Linux と同じ)、EOE (1320: kmsg には
  出さない) を出す。ユーザーの記録 (AUDIT_USER_*) と TTY_SET はスレッドのことではないので出さない

- ルール (kernel/src/auditfilter.rs: auditctl -a / -d / -l の AUDIT_ADD_RULE / DEL_RULE / LIST_RULES と struct
  audit_rule_data)。組は USER (ユーザーの記録の種類で捨てる)、TASK (fork したスレッドに入れものを作らない・いつも出す)、
  EXIT (システムコールの出口: 番号のマスク、pid uid gid loginuid sessionid arch exit success a0〜a3 perm exe key などで
  合わせ、先に合ったものの ALWAYS / NEVER と key)、EXCLUDE (その種類の記録を出さない)。まちがったもの (古い ENTRY の
  組、知らない欄や比べかた、key の長さ、WATCH / DIR / EXE の道) は EINVAL、WATCH の親や DIR や EXE の親がなければ
  ENOENT、同じものは EEXIST、ないものを消すと ENOENT。足す・消すたびに `op=add_rule key="..." list=4 res=1`。値の決まり
  (LOGINUID_SET は 0 か 1、PERM は 15 まで、FILETYPE、FIELD_COMPARE、SADDR_FAM) も見て EINVAL (Linux は見ない:
  aios がえらんだちがい audit-field-value)。Linux と同じくせ: PREPEND は覚えるときに消えるので、つけたままでは消せない。マスクの上の 16 ビット (クラス) は arm64 の番号に
  ひろげる。システムコールのルールがあれば、入れもののあるスレッドは大きなロックなしの道 (getpid など) も入口と出口を
  通る

test/audit.c (root で) は、上のものをみな Linux 7.4-rc1 とくらべて同じ (152 行)。test/loginuid.c (root で) は
loginuid と sessionid、AUDIT_LOGIN、SYSCALL などの記録を組 READLOG で読んでくらべて同じ (115 行)。test/auditrules.c
(root で) はルールを足す・消す・一覧、まちがったもの、値の決まり (Linux では aios をまねる)、USER と EXCLUDE と
EXIT のフィルタをくらべて同じ (172 行)。

- パスの名前 (Linux の getname、audit_inode、audit_inode_child): 名前を集めているとき (ルールがあるか、記録が出たあと)、
  パスを取るシステムコールの名前を Linux と同じ順に覚えて、出口で CWD (`cwd="/tmp/ap"`) と名前ごとの PATH (`item=0
  name="/tmp/ap/" inode=... dev=... mode=040755 ouid=0 ogid=0 rdev=00:00 nametype=PARENT cap_fp=0 ...`) を、execve なら
  EXECVE (`argc=3 a0="prog" a1="--exec" a2=74776F20776F726473`: 空白などがあれば 16 進) も。open で作る (親の PARENT と
  CREATE)、ある・読む (NORMAL)、ない (UNKNOWN: inode なし)、相対パスの親は いまのディレクトリ、mkdirat (あれば PARENT
  だけ)、symlinkat (中身の名前が UNKNOWN、親、CREATE)、linkat (元の NORMAL、親、CREATE)、renameat2 (親 2 つ、元の DELETE、
  行き先の CREATE。上書きなら行き先の DELETE も)、unlinkat (親と DELETE)、chmod や chown は変える前の mode と持ち主。
  syscall::dispatch がシステムコールの前とあとに sysfile::audit_pre / audit_post を呼ぶ (名前を集めているときだけ)
- 名前で合わせるルール: WATCH (auditctl -w FILE -p wa) は名前の inode がそのファイルのもの (いまあるもの、なければ
  前に見たもの: 消す・動かすシステムコールでも合う)、INODE、DEVMAJOR、OBJ_UID、FILETYPE も名前で。perm の w は
  ディレクトリを変えるもの (mkdir、unlink、rename など) もふくむ (Linux の write のクラス)

- DIR (auditctl -w DIR -p wa: Linux の audit_tree): たどって引いた名前 (NORMAL と PARENT) のどれかが木の根かその下に
  あれば合う (木の中で作る・chmod・mkdir、深いところ、木の外へ動かす、木の根そのもの)。木の外のもの、読むだけのもの
  (perm) は合わない
- ほかのパスのシステムコールの名前: openat2 (OPENAT2 の記録 `oflag=0101 mode=0644 resolve=0x0` も)、mknodat、
  utimensat (パスがなければ名前なし)、faccessat、faccessat2、chdir、chroot、statfs、inotify_add_watch、fchmod と fchown
  (fd のもの: `name=(null)` の PATH)。CWD は、いまの根から見たパス (chroot したあとならその中: Linux の d_path)
- 終わるとき (__audit_free): exit や exit_group の中なら、そのルールを見て記録を出す (success と exit はない)。待っている
  ところをシグナルで殺されたものは、Linux と同じく EINTR で出口を通るので `success=no exit=-4`

test/auditpath.c (root で) は子のパスのシステムコールと WATCH の記録をくらべて同じ (174 行)。test/auditdir.c (root で)
は DIR、ほかのパスのシステムコール、exit_group、殺されたときの記録をくらべて同じ (149 行)。test/auditexec.c (root で) は、
記録を出したあとに exec したスレッドの PROCTITLE が新しいプログラムのものになるのを確かめて同じ (24 行)。kmsg のかぎりがあるので、ところどころで
5 秒待つ。手本の Linux は AppArmor を入れると
AUDIT も入る (select)。のこり: xattr、mount、execveat、name_to_handle_at などの名前、動的なプログラムの ld.so の名前、
AUDIT_MAKE_EQUIV (EOPNOTSUPP)、AppArmor のラベルのルール (SUBJ_ROLE)、/proc/PID/task/TID/loginuid、TTY の入力の記録
(TTY_SET は覚えるだけ)、backlog (記録はすぐ出すので行列がない)。

### ファイアウォール (nftables)

いまの aios のネットワーク: 外 (eth0 の向こう) は virtio-net のフレームを smoltcp にわたし、中どうし (127.0.0.0/8、eth0 の
アドレス) はパケットを作らずに相手のソケットの行列へじかに入れる (inet.rs、unix.rs)。netlink は段階 0 で入れた (それまで
getifaddrs と if_nameindex も EAFNOSUPPORT でしくじっていた)。aibox は landlock でポートをしぼるだけで、名前空間に口 (veth)
はないので、転送と NAT はいまはいらない。ファイアウォールの仕事は、eth0 から入るものと出るもの。

Linux と同じ nftables にする (nft の道具がそのまま動き、Linux の決まりのファイルがそのまま使える)。段階:

- 段階 0 (できた): netlink (AF_NETLINK、kernel/src/netlink.rs) と NETLINK_ROUTE の口とアドレスの一覧 (RTM_GETLINK、
  RTM_GETADDR の dump と 1 つの RTM_GETLINK。道の RTM_GETROUTE はあとで: 下の「道の表」)。getifaddrs、if_nameindex、node の os.networkInterfaces() などが動く。
  ソケットのきまり (ポートは tgid から、bind、データグラムごとに受けて短ければ MSG_TRUNC、dump は中身と NLMSG_DONE の 2 つの
  データグラム、NLMSG_ERROR ともとのメッセージ、NETLINK_CAP_ACK、REQUEST のないものと短いものには答えない、root でなければ
  変える type は EPERM) は Linux 7.3.1 と同じ (test/netlink.c の 79 の答え)。口の番号は Linux と同じ lo 1、eth0 2 に
  そろえた (ioctl の SIOCGIFINDEX と /sys/class/net。前は eth0 が 1)。ioctl は名前で lo も聞ける。SIOCGIFCONF は eth0
  だけのまま (aios の ip、networkd、aios は、はじめのものを eth0 とみなす)。NETLINK_ROUTE と NETLINK_NETFILTER (段階 1)
  のほかの protocol は EPROTONOSUPPORT、口やアドレスを netlink で変えるのはまだ (EOPNOTSUPP。ioctl でできる)
- 段階 1 (できた): nfnetlink と nf_tables の表 (kernel/src/nftables.rs。Linux 7.3 の nfnetlink.c と nf_tables_api.c
  と同じ形と答え)。本物の nft (パッケージ nftables: nft 1.1.7、libnftnl 1.3.2、libmnl 1.0.5、jansson を静的に。
  /opt/c/bin/nft) がそのまま動く: `nft -f`、`nft list ruleset` (`-a`、`-j`)、add / insert / delete / flush。
  - batch: BEGIN (生成番号つきなら、ちがえば ERESTART) から END まで、いまの表を写したものに順に当て、みな通れば
    入れかえて生成番号を進める。しくじれば捨てて、しくじりを順に返す (ほかのものもみな見る)。変えるものは batch の
    中だけ、見るものは外だけ (どちらも外れれば EINVAL)。ぜんぶ CAP_NET_ADMIN
  - table (ip、ip6、inet。arp、bridge、netdev は手本の Linux に入れていないので EOPNOTSUPP。dormant、owner (持ち主の
    ソケットが閉じると消える。persist なら残る)、userdata)、chain (filter と route の型、フックと priority、policy、
    jump の先、use の数)、rule (末尾、先頭、position / position_id の前後、replace、handle と id で消す)、set と map
    (名前のないもの `__set%d` は使う決まりがなくなると消える、constant、interval、timeout、size、concat の長さ、
    verdict の map)、要素 (同じものは EEXIST か何もしない、interval の終わり、map の中身、消す、flush)
  - 番号 (handle): 表は全体で、鎖・決まり・集まりは表ごとに 1 から。しくじった batch でも進んだまま (Linux と同じ)
  - 式は immediate cmp payload meta bitwise byteorder lookup range counter log limit reject quota を読んで、Linux の
    dump と同じ形で返す (レジスタは 32 ビットの番号にしてから返す: NFT_REG32_00 は NFT_REG_1)。exthdr rt last は
    来たまま。ct などの手本の Linux に入れていないものは ENOENT
  - しくじりの答えは Linux と同じところで NLMSGERR_ATTR_OFFS (nft はそれで「^^^」の場所を決める)。ACK は NETLINK_EXT_ACK
    の形 (netlink.rs の ack)。dump は 3776 バイト (NLMSG_GOODSIZE) ずつ、set の要素の dump は Linux と同じく
    おしまいに要素のないメッセージと NLMSG_DONE を 1 つのデータグラムで
  - まだ: object (名前のついた counter や quota)、flowtable、set の要素ごとの式、dynset、nft monitor (組への知らせ)、
    NLM_F_ECHO、決まった要素を聞く get element
  - 確かめかた: `test/nftables.c` (root で。10 の case (段階 2 で jump の深さと reject のフックのたしかめを足した) の
    nft -f と list ruleset -a -j。出力がみな手本の Linux 7.3.1 と同じ: `LINUX_FILES="nft=NFT" test/linux-vm.sh rootexec latest test/bin/nftables /nft`)。手本の Linux の
    test/linux.config に NETFILTER、NF_TABLES (INET、IPV4、IPV6)、NFT_LOG (と NF_LOG_SYSLOG)、NFT_LIMIT、NFT_REJECT、
    NFT_QUOTA と IPV6 を入れた (sysdiff と netlink の答えは入れる前と同じ)
- 段階 2 (できた): 評価とフック。式を動かすところ (nftables.rs の VM) と、パケットが決まりをとおる道 (kernel/src/nfhook.rs)
  - VM: Linux の nft_do_chain と同じ: 20 の 32 ビットのレジスタ (判定は別)、決まりごとに式を順に (NFT_BREAK で次の
    決まり)、jump は 16 段まで (goto はもどらない)、return と鎖の終わりでもどり、基の鎖の終わりは policy。queue は
    受ける人がいないので drop (bypass なら accept)。式は immediate cmp payload (読む、書く: チェックサムも差分で直す)
    meta (len protocol priority mark iif oif iifname oifname iiftype oiftype skuid skgid nfproto l4proto pkttype cpu
    iifgroup oifgroup random time day hour sdif sdifname。mark priority pkttype は書ける) bitwise (and/xor、shift、
    and/or/xor) byteorder lookup (hash、rbtree の区間、pipapo (concat の区間)、catchall、判定の map、消える時刻)
    range counter log limit (ns の token bucket) reject quota last exthdr (TCP のオプション) rt (nexthop、tcpmss)
  - フックの順: ip と inet の表 (dormant でないもの) の基の鎖を priority の順、同じなら先に入れたもの (起こした表は
    後ろへ)。drop したらそこで終わり。どの表もなければパケットを作らない
  - たしかめ (nft_table_validate): 基の鎖からたどって 16 段より深い jump は EMLINK (基の鎖から届かない輪はよい)、
    reject は ip では input forward output だけ (inet は prerouting と ingress も)、rt は forward output postrouting、
    meta の pkttype を変えるのは prerouting、sdif は input と forward。commit でしくじれば、Linux と同じく batch を
    はじめからやりなおし、決まりと要素を足すたびにたしかめて、しくじったものがエラーを返す (番号 handle は 2 度目のぶんも
    進む)。段階 1 で commit のときに BEGIN に返していた ELOOP は、これで Linux と同じ EMLINK に
  - 道: eth0 から来たもの (net.rs の受け) は PRE_ROUTING (iif eth0) と、aios あて (自分のアドレス、ブロードキャスト、
    マルチキャスト。おかしな送り元・あて先は PRE_ROUTING のあとで捨てる) なら LOCAL_IN。頭のおかしいものと、ほかの
    あて先 (PACKET_OTHERHOST) は決まりの前に smoltcp が捨てる。eth0 へ出すものは LOCAL_OUT と POST_ROUTING: smoltcp が
    作る TCP と ICMP は出す前に、UDP は sendmsg のとき (Linux と同じく、出す側で落ちれば EPERM)。DHCP は出すものを
    通さず、受けるものは落ちても届ける (Linux の DHCP は AF_PACKET なので netfilter に関わらない)
  - 中どうし (lo。127.0.0.0/8 と eth0 のアドレス): aios はパケットを作らないので、Linux が lo に流すのと同じ形のものを
    作って LOCAL_OUT、POST_ROUTING (oif lo)、PRE_ROUTING、LOCAL_IN (iif lo。MAC は 0 の 14 バイト) に通す。mark と
    priority は受ける側にも残る。TCP は SYN (窓 65495、MSS・SACK・timestamp・window scale のオプション)、SYN-ACK、
    だれもいないときにカーネルが返す RST。どれかが落ちれば SYN をやりなおす (1、2、4 … 秒、127 秒で ETIMEDOUT)。
    つながったあとのデータも MSS ずつの segment (PSH ACK、timestamp つき) にして通し、受けた側の ACK も通す (`meta length`
    や payload の決まりも効く)。落ちた segment とそのあとのものは相手に届かず、送りなおしを待つ (200 ms から倍に、
    120 秒まで。15 回で ETIMEDOUT: Linux の tcp_retries2。待つあいだの SIOCOUTQ はその長さ、FIN はそのあと)。
    reject の RST が来ればつながりはこわれる (ECONNRESET)。UDP はデータグラムごと (だれもいないときの ICMP の
    port unreachable も)
  - reject: 判定は drop。返すもの (nf_send_reset の RST: id 0、DF、TTL 64、窓 0。nf_send_unreach → __icmp_send の
    ICMP: TOS は 0xc0 を足し、もとのものを 576 バイトまで) も決まりの道をとおる (自分あては lo の 4 つ、外へは eth0 の
    LOCAL_OUT と POST_ROUTING)。RST への RST、ICMP のエラーへのエラー、断片、ブロードキャスト、受けたもののチェックサムの
    ちがうものには返さない。外へ返す ICMP は相手ごとに 1 秒に 1 つ (はじめは 6 つまで)。届いた RST は ECONNREFUSED、
    ICMP は icmp_err_convert の errno (port は ECONNREFUSED、host と admin-prohibited は EHOSTUNREACH、net は
    ENETUNREACH)。UDP は connect したものと IP_RECVERR / IPV6_RECVERR だけ (致命でないものは RECVERR だけ)。smoltcp のソケットへの
    RST は smoltcp に受けさせ、ICMP は poll のあとで inet.rs が当てる
  - log: Linux の nf_log_syslog と同じ 1 行 (IN= OUT= MAC= (flags の macdecode なら MACSRC= MACDST= MACPROTO=) SRC= DST=
    LEN= TOS= PREC= TTL= ID= DF PROTO= …、ICMP のエラーは中のものを [] で) を kmsg に (dmesg で見える。シリアルの
    画面には出さない)
  - Linux よりよいところ: ファイアウォールが落としたものに当たったシステムコールは /proc/ai/why に「the firewall dropped
    the packet: nftables inet filter output handle 5」(policy なら policy drop) と残る (sendmsg の EPERM、connect の
    ECONNREFUSED など)
  - 受けるものの meta skuid / skgid: Linux の early demux (ip_rcv_finish_core: PRE_ROUTING のあと) と同じく、eth0 から
    来た自分あてのもの (PACKET_HOST、断片でないもの) の LOCAL_IN でだけ、TCP はつながり (SYN_SENT も) のソケット、
    UDP は connect して 4 つがみな合うソケットの持ち主 (inet::demux_owner)。PRE_ROUTING と lo から来たもの (dst がある
    ので early demux しない) と listen しているものには持ち主がない。`LINUX_FILES="nft=build/nft-static" test/ldiff.sh
    --root --net nftsk` (QEMU のホストのだれもいないポートへ connect して返る RST を、root と uid 4242 の子で) で手本の
    Linux 7.4-rc1 と 43 行同じ
  - まだ: exthdr の IPv4 オプションと書くもの、ingress、forward (転送しない)、外へつなぐ TCP の SYN のやりなおしの間は smoltcp の
    もの (counter の数が Linux とちがう)
  - 確かめかた: `test/nfteval.c` (root で。12 の case: 中どうしの TCP と UDP、eth0 のアドレス、外のだれもいないポート、
    QEMU を一回りして外から来る TCP と UDP (hostfwd: bin/run.sh の AIOS_FWD と test/linux-vm.sh の LINUX_NET)。答え
    (つながる、ECONNREFUSED、EHOSTUNREACH、時間切れ、届いた数、EPERM)、counter、log の行を、手本の Linux 7.3.1
    (test/linux.config に VIRTIO_NET と IP_PNP_DHCP を入れた) とくらべる: 50 のためしと counter と log の行が、みな
    同じ。netlink と sysdiff の手本の答えは、入れる前と同じ)。`test/nftables.c` にも jump の深さと reject の
    フックの case を足した
    あとで、つながったあとのデータの case を 2 つ足した (lo-data: 向きごとの counter と `meta length`、lo-retransmit:
    つながったあとに drop を入れて SIOCOUTQ を見て、消すと送りなおしで届く)。これも手本の Linux と 1 バイトもちがわない
- 段階 3 (できた): conntrack (kernel/src/conntrack.rs。Linux 7.3 の nf_conntrack_core.c、proto_tcp.c、proto_udp.c、
  proto_icmp.c、proto_generic.c、nf_conntrack_standalone.c と同じ答え) と nft の `ct` の式
  - 動くのは、ip か ip6 か inet の表に ct の式があるときだけ (Linux の nf_ct_netns_get: dormant の表でも)。フックは
    PRE_ROUTING と LOCAL_OUT の priority -200 (見て、なければ作る) と、LOCAL_IN と POST_ROUTING のいちばん後 (confirm:
    表に入れる)。そのあいだで落ちた新しいものは表に入らない。lo は出す側でついたつながりを受ける側もそのまま使う
  - つながりは向きごとの組 (送り元、あて先、protocol、ポートか ICMP の type・code・id) を 2 つ持つ。ctinfo は Linux と同じ
    (答えの向きは ESTABLISHED、元の向きは答えを見たら ESTABLISHED、それまで NEW)。`ct state` は invalid 1、
    established 2、related 4、new 8、untracked 64
  - TCP: tcp_conntracks の表 (2 つの向き × 6 つのフラグの組 × 10 の状態)、sIG (見なかったことに) と、前に見なかった
    SYN への SYN-ACK でそろえなおすこと、TIME_WAIT からの開きなおし (消してもう一度)、同時に開く (SYN_SENT2)、
    RST のたしかめ、RFC 5961 の challenge ACK、tcp_in_window (窓、window scale、SACK、外れは INVALID か IGNORE)、
    tcp_loose (途中から拾う)、時間切れの表 (ESTABLISHED 5 日、TIME_WAIT 120 秒 …、retrans と unacknowledged)、答えが
    RST だけのものは消す、ASSURED。頭のたしかめ (長さ、PRE_ROUTING のチェックサム、フラグの組)
  - UDP: 答えを見るまで 30 秒、答えを見てから 2 秒より後のパケットで 120 秒にのばして ASSURED。ICMP: echo などの問い
    だけがつながりになり (30 秒)、エラーは中の組から見つけたつながりの RELATED (外のあて先が合うものだけ)。ほかは
    generic (600 秒)
  - reject の返すもの (RST と ICMP) は、もとのつながりを持つ (nf_ct_attach: related)。RST で reject した TCP の
    つながりは CLOSE に (nf_ct_set_closing、10 秒)
  - `ct` の式: state direction status mark expiration (ms) l3proto protocol saddr daddr (ip saddr / daddr) proto-src
    proto-dst packets bytes avgpkt (使うと数えはじめる: nf_ct_set_acct) id。`ct mark set`、`notrack` (raw の priority
    で)。手本の Linux にない secmark label zone event は EOPNOTSUPP。dump は Linux と同じ形
  - 中どうし (lo) の TCP: つなぐときの SYN、SYN-ACK に、3 つ目の ACK と閉じるときの FIN、ACK、RST (Linux と同じ
    timestamp つきの 52 バイト) も、lo の決まりと conntrack に通す (落とされても止めない)。データの segment も通すので
    (ファイアウォールが入る前につないだものは途中から拾う)、conntrack の seq もデータの長さだけ進む
    (ESTABLISHED → FIN_WAIT … → TIME_WAIT)。ESTABLISHED で FIN を受けた側の ACK は、Linux の tcp_fin と同じく
    おくらせ (pingpong の delayed ACK: lo では 40 ms)、そのあいだに自分も閉じれば FIN が ACK を運ぶ (前はすぐ ACK を
    出していたので、すぐ閉じる 2 つのソケットで segment が 1 つ多かった)
  - /proc/net/nf_conntrack (root だけ。Linux と同じ行: ipv4 2 tcp 6 残りの秒 状態 組 [UNREPLIED] 組 [ASSURED] mark use)、
    /proc/sys/net/netfilter/nf_conntrack_{count,max,buckets,acct,checksum,tcp_loose,tcp_be_liberal,tcp_max_retrans,
    tcp_ignore_invalid_rst,tcp_timeout_*,udp_timeout,udp_timeout_stream,icmp_timeout,generic_timeout} と
    /proc/sys/net/nf_conntrack_max。max は RAM から Linux と同じ式 (16 KiB ごとに 1 つ、1 GiB より多ければ 65536)
  - 確かめかた: test/nfteval.c に 5 つの case (ct-state: よくある `ct state established,related accept` と policy drop、
    ct-keys、ct-notrack、ct-related、ct-list: /proc/net/nf_conntrack をならべたもの)。手本の Linux (test/linux.config に
    NF_CONNTRACK、NF_CONNTRACK_MARK、NF_CONNTRACK_PROCFS、NFT_CT) と 1 バイトもちがわない。外 (10.0.2.2) とのつながりの
    状態は、smoltcp と Linux の TCP のちがい (FIN の出しかたなど) がでるので数だけ
  - ctnetlink (Linux の nf_conntrack_netlink.c、NETLINK_NETFILTER の subsys 1: kernel/src/ctnetlink.rs) と conntrack の
    道具 (pkg/c/conntrack-tools: conntrack だけを静的に。中で libnetfilter_conntrack、libnfnetlink、libmnl): IPCTNL_MSG_CT_GET
    の dump (conntrack -L。nfgenmsg の家族のものだけ、AF_UNSPEC はみな。libmnl の 4096 の入れ物に入るように分ける) と
    1 つ (-G: 組で引く。use は 2、なければ ENOENT)、CT_DELETE (組で 1 つ、組がなければ家族のものみな: -D と -F)、
    GET_STATS (-C の数といちばん多いとき) と GET_STATS_CPU (-S: Linux と同じ 10 の数。aios が数えるのは invalid だけ、
    ほかはハッシュ表の作りの数で 0)。答えのつながりは CT_NEW (組 2 つ、status、のこり時間、TCP の状態と窓の倍率、
    数 (acct のとき)、mark、id、use)。手本の Linux に NF_CT_NETLINK と NF_CONNTRACK_EVENTS を足した。
    `LINUX_FILES="conntrack=build/conntrack-static nft=build/nft-static" test/ldiff.sh --root ctnl` (中どうしの TCP と
    UDP で -L・-o extended・-p・-f ipv6・--state・-C・-S・-G・-D・-F) で手本の Linux 7.4-rc1 と 71 行同じ
  - ctnetlink の知らせ (ctnetlink_conntrack_event: conntrack -E) と CT_NEW: つながりは知らせることをためておき
    (nf_conntrack_event_cache)、LOCAL_IN と POST_ROUTING の confirm で知らせる。表に入れると NEW (TCP は状態も)、答えを
    見た (REPLY)・ASSURED・TCP の状態が変わった (PROTOINFO)・nft の ct mark set で mark が変わった (MARK) は UPDATE、
    消すと DESTROY (時間切れ、TCP のやりなおし、RST、道具の -D と -F)。組は NFNLGRP_CONNTRACK_NEW / UPDATE / DESTROY
    (NETLINK_NETFILTER の組は 9 まで)、属性は Linux と同じえらびかた (DESTROY は数と TCP の状態、ほかは PROTOINFO の
    ときだけ状態、MARK か 0 でない mark)。道具がしたもの (CT_NEW で作る -I・変える -U、消す) は頭の portid がそのソケット
    (conntrack -E の [USERSPACE] portid=)。CT_NEW はなければ NLM_F_CREATE で作り (組 2 つとのこり時間がいる。status は
    変えられるビットだけ、TCP の状態、mark)、あれば NLM_F_EXCL なら EEXIST、でなければ変える (EXPECTED・CONFIRMED・
    DYING を変えるか SEEN_REPLY・ASSURED をはずすなら EBUSY)。`LINUX_FILES="conntrack=build/conntrack-static
    nft=build/nft-static" test/ldiff.sh --root ctev` (-E を動かしたまま中どうしの TCP をつないで閉じ、UDP、-I・-U・-D、
    -F の DESTROY は並べて。portid は *) で手本の Linux 7.4-rc1 と 48 行同じ
  - まだ: カーネルでしぼる CTA_FILTER (しぼるのは conntrack の道具もする) と 7.4 の CTA_FILTER_ZONE、helper (ftp など)、
    expectation (IPCTNL_MSG_EXP_*)、zone、label、ct count (connlimit)、NAT
- IPv6 (段階 2b、IPv6 の段階 1 のあと): aios の中の ::1 の TCP と UDP も、IPv4 と同じく lo の 4 つのフックに通す。
  パケットは Linux が lo に流すのと同じ IPv6 の頭 (traffic class、flow label、hop limit 64。MAC の型は 86dd)
  - ip6 と inet の表が見る (ip の表は IPv6 を、ip6 の表は IPv4 を見ない。::ffff:127.0.0.1 へのものは IPv4 のパケット)。
    `meta nfproto` は ipv6、`rt ip6 nexthop` はあて先、`rt tcpmss` は 60 を引く。L4 は拡張ヘッダー (hop-by-hop、
    routing、destination、AH、fragment) をとばして見つける
  - SYN は窓 65476 (SYN-ACK は 65464) と MSS 65476 のオプション (Linux の ::1 と同じ 80 バイト)
  - reject: nf_send_reset6 の RST (traffic class 0、flow label 0、hop limit 64) と nf_send_unreach6 の ICMPv6 の
    到達不能 (type 1、もとのものを 1232 バイトまで)。icmpx は IPv6 の code (no-route 0、port 4、host 3、admin 1)。
    マルチキャストのあて先、RST、ICMPv6 のエラーには返さない。受けた側は icmpv6_err_convert の errno (no-route は
    ENETUNREACH、admin と policy は EACCES (致命)、addr と beyond-scope は EHOSTUNREACH、port は ECONNREFUSED
    (致命))。つなぐ TCP は致命のもので終わり (EACCES など)、UDP は connect したものに致命のものだけ。だれもいない UDP
    へのカーネルの port unreachable も決まりの道を通る
  - log: nf_log_ip6_packet と同じ行 (SRC= DST= は 4 けたの 16 進を 8 つ、LEN= TC= HOPLIMIT= FLOWLBL= PROTO= …)
  - eth0 の IPv6 のフレーム (段階 2d、IPv6 の段階 2c のあと): 受けたものは PRE_ROUTING と、自分の番地か受ける
    マルチキャスト (ff02::1、自分の番地の solicited-node) あてなら LOCAL_IN (ip6_rcv_core のたしかめのあと。RA、NS、NA
    も決まりを通る: Linux と同じく、近所さがしの ICMPv6 を通さない policy drop では IPv6 がつながらない。通ったものだけを
    addrconf と近くの番地の表が読む)。smoltcp が出すもの (TCP、ICMPv6、近所さがし、addrconf の RS) は LOCAL_OUT と
    POST_ROUTING。UDP と raw の ICMPv6 は送るときに通して、落ちたら EPERM (ip6_local_out)。自分の番地へのもの
    (raw の ping6 など) は lo の 4 つ
  - eth0 の reject: RST と ICMPv6 は eth0 の向こうへ (LOCAL_OUT と POST_ROUTING を通して)。受けたもののチェックサムを
    確かめ、ICMPv6 は自分あて (PACKET_HOST) のものにだけ、相手ごとに 1 秒に 1 つ (はじめは 6 つまで:
    icmpv6_xrlim_allow)。自分が出したものを返す ICMPv6 の送り元は自分の番地 (icmp6_send)。返したものは raw の
    ソケットと、TCP と UDP のソケット (ICMPv6 のエラー) に届く
  - つなぐ途中の TCP を ICMPv6 のエラーでやめるときは、smoltcp の abort (RST を送る) ではなく close で (Linux の
    tcp_done は何も送らない)
  - /proc/ai/fw の落としたものの輪も IPv6 の番地とポート、icmpv6 の type と code を見せる
  - IPv6 の conntrack (段階 2e: nf_conntrack_ipv6 と nf_conntrack_proto_icmpv6): 組の番地は IPv6 のもの (拡張ヘッダーを
    とばして。断片はまとめないので見ない)、TCP と UDP は IPv4 と同じ状態表 (チェックサムは擬似ヘッダーの IPv6)、
    ICMPv6 は echo と node information の問いがつながり (30 秒: nf_conntrack_icmpv6_timeout)、エラー (type < 128) は
    中のパケットのつながりの related (外のあて先が合うもの)、近所さがしと MLD (130〜136、143) は untracked。ct の式の
    l3proto、original / reply の ip6 saddr / daddr (NFT_CT_SRC_IP6 と DST_IP6。IPv4 のつながりでは NFT_BREAK)、
    /proc/net/nf_conntrack の ipv6 の行 (ipv6 10、番地は %pI6 の形)、/proc/ai/fw のつながり
  - 確かめかた (conntrack): test/nfteval6.c の ct-state6 (lo の TCP と UDP、ICMPv6 の port unreachable の related、
    conntrack の行)、ct-keys6、eth-ct (fec0::2 への ping6 の答えが established、近所さがしを通す policy drop)。
    `--root --net nfteval6 /nft net` で手本の Linux 7.4-rc1 と 344 行、ネットなしで 210 行同じ
  - 確かめかた (eth0): `LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net nfteval6 /nft net` (7 つの case:
    fec0::2 と fe80::2 への ping6 と counter、受けと出しの drop、出しの reject (icmpv6、tcp reset、UDP)、受けの reject
    (外へ返す ICMPv6 を output で数える)、近所さがしを通す policy drop、eth0 の log)。手本の Linux 7.4-rc1 と 289 行
    同じ。QEMU は TCP に fe80::2 から ICMPv6 の no route を返すが、Linux はそれが connect のあいだに着くと待ちつづける
    (時間しだい) ので、TCP は落とすか RST で断るものだけ
  - 確かめかた: `test/nfteval6.c` (root で。11 の case: accept と counter、input の drop と reject (tcp reset、icmpv6 の
    admin-prohibited・addr-unreachable・no-route、icmpx)、output の drop と reject、ip だけ・ip6 だけの表 (::1、
    127.0.0.1、::ffff:127.0.0.1 へ)、inet の policy drop と nfproto、log)。
    `LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root nfteval6 /nft` で手本の Linux 7.4-rc1 と 171 行同じ。
    IPv4 の nfteval (中どうしだけ: `--root --net nfteval /nft 0 0 0` で 370 行) と nftables (457 行) も同じまま。
    nfteval と nftables も、aios では nft が /opt/c/bin/nft にあればそれを使うので、ldiff に同じ引数で渡せる
- AI が見る口 (aios だけ): /proc/ai/fw と aish-sys の `fw` ツール。root と wheel の人だけ
  - 基の鎖ごとに通った数と policy まで来た数、決まりごとに当たった数 (counter がなくても) とさいごに当たってからの時間と
    式 (nft --debug=netlink と同じ形: `[ payload load 2b @ transport header + 2 => reg 1 ] [ cmp eq reg 1 0x00001600 ]`)、
    集まりの要素の数
  - 落としたものの輪 (新しい順に 64: いつ、フック、口、proto、送り元とあて先、TCP のフラグ、長さ、どの決まり (表、鎖、
    handle か policy)、reject、出したソケットの uid)。Linux は log の決まりや `nft monitor trace` を入れないと見えない
  - conntrack の数と状態ごとの数、つながり (64 まで)
  - trace: `/proc/ai/ctl` に `fw trace in eth0 tcp 10.0.2.2:40000 10.0.2.15:22 syn` (in / out、eth0 / lo、tcp / udp /
    icmp、フラグ) で、パケットを流さずに、いまの決まり (写したもの) にとおして、鎖と決まりごとに「どの式で外れたか、
    どの判定か」と答え (/proc/ai/fw の trace の行)。counter も limit も conntrack も変えない。つながらない・届かない
    わけを、AI がパケットを送らずに探せる
- そのあと: NAT と転送 (名前空間に口を作るとき)、IPv6 の conntrack と eth0 の IPv6 のフレーム

Linux とのくらべかた: 手本の Linux (test/linux.config) に nf_tables と virtio-net を入れ、同じ nft の決まりで、同じ相手
(QEMU のユーザーネットワークの 10.0.2.2) へのつなぎと受けの答え (つながる、ECONNREFUSED、時間切れ)、`nft -j list ruleset`、
counter の数をくらべる。nf_tables の大きさは、Linux 7.3.1 で nf_tables_api.c が 1 万 2 千行、netlink (af_netlink.c と
nfnetlink.c) が 3 千 8 百行、conntrack の核と TCP が 4 千 5 百行ほど。

Linux よりよくできそうなところ: (1) 止めたわけが見える: ファイアウォールで捨てたら /proc/ai/why に、どの table、chain、
rule (handle) かを書く (connect が時間切れになったプロセスにも)。Linux は黙って捨てる (log や `nft monitor trace` を自分で
入れる)。(2) 中どうし (lo) も同じ決まりで、パケットを作らない aios でも効く。(3) アプリごとの決まりは、いまでも
AppArmor の network_v9 の af_inet (ip と port。Linux の本体は効かせない: apparmor-inet) と landlock (TCP の bind と
connect のポート) でかけられる。nftables は、システムぜんたいの決まり (外から来るつなぎなど)。

決めたこと (syui、2026-11-01): nft と同じ API にする (aios だけの小さな決まりは作らない)。conntrack (段階 3) も作る。
外 (eth0) だけでなく、中どうし (lo) にもかける。

### 道の表 (NETLINK_ROUTE の RTM_GETROUTE、ip route get)

Firefox が「Network is offline」(services.settings の NetworkOfflineError) と言っていたわけ: Firefox の NetlinkService
(netwerk/system/netlink) は、AF_PACKET の RTM_GETLINK・RTM_GETADDR・RTM_GETROUTE の dump のあと、ROUTE_CHECK の番地への
道を 1 つの RTM_GETROUTE で聞き、答えが unicast で scope universe で RTA_OIF があるときだけ「つながっている」とする。
aios には RTM_GETROUTE がなく (EOPNOTSUPP)、AF_PACKET の RTM_GETADDR も空だったので、ページは読めるのに link down だった。

- 道は口のアドレスとゲートウェイから作る (kernel/src/netlink.rs の routes): Linux の fib_add_ifaddr と同じく、アドレス
  ごとに local の /32、broadcast、網の道 (lo は local の 127.0.0.0/8、eth0 は main の 10.0.2.0/24 proto kernel scope
  link src)、網の broadcast。DHCP のゲートウェイは ip=dhcp の ipconfig と同じ既定の道 (main、proto boot、scope
  universe、prefsrc なし)。IP_MULTIPLE_TABLES のない Linux と同じく local (255) と main (254) は 1 つの trie: dump は
  main を番地の順に、そのあと local (葉の中は長い prefix、同じなら表の番号の大きいもの)
- dump (inet_dump_fib): 中身と NLMSG_DONE の 2 つのデータグラム。RTM_F_PREFIX は空。strict (NETLINK_GET_STRICT_CHK:
  iproute2 の ip) なら ip_valid_fib_dump_req と同じたしかめ (頭の dst_len・src_len・tos・scope は 0、flags は
  CLONED と PREFIX だけ、属性は RTA_TABLE と RTA_OIF だけ。しくじりは DONE の errno) と選びかた (表、protocol、type、
  口。表は 255 のほかはみな main。CLONED は例外だけなので空)。strict ならいつも NLM_F_DUMP_FILTERED、選んだら DONE にも
- 1 つの道 (inet_rtm_getroute、ip route get): ip_route_output_key_hash_rcu と同じ順で、RTA_SRC (multicast や
  255.255.255.255 は EINVAL、自分のでなければ ENETUNREACH)、RTA_OIF (ない口は ENODEV。表で見つからなくても同じ網と
  みなす)、宛先なし (127.0.0.1 の local)、いちばん長い prefix、local は lo から (prefsrc か宛先そのもの)、ほかは
  prefsrc か inet_select_addr。__mkroute_output の 127.x を lo のほかから送るのは EINVAL、0.x.x.x は EINVAL、
  255.255.255.255 は broadcast。答えは rt_fill_info: table 254、scope universe、protocol 0、RTM_F_CLONED と RTCF_LOCAL・
  RTCF_BROADCAST (上の 16 ビット)、RTA_TABLE、DST、SRC、OIF、PREFSRC (src とちがえば)、GATEWAY、MARK、UID (呼んだ人)、
  CACHEINFO。RTM_F_FIB_MATCH は当たった道を fib_dump_info で (table は 254、type は出ていく道の type)。strict は
  inet_rtm_valid_getroute_req (長さは 0 か 32、table・protocol・scope・type は 0、属性は IIF OIF SRC DST IP_PROTO
  SPORT DPORT MARK UID だけ)。入ってくる道 (RTA_IIF) はまだ (EOPNOTSUPP)
- 家族: その家族の口がなければ PF_UNSPEC の口 (rtnl_get_link の fallback)。RTM_GETADDR は AF_INET が v4、AF_INET6
  が v6、ほかは rtnl_dump_all (v4 のあとに v6: 下の「IPv6」)。RTM_GETROUTE の dump は AF_INET6 が v6 の道 (lo の
  local ::1)、ほかは v4 の道。RTM_GETLINK は PF_BRIDGE (ブリッジがないので空) のほかはみな口の一覧。1 つの
  RTM_GETROUTE は AF_INET と AF_INET6 (ほかは EOPNOTSUPP)
- ついでに SIOCGIFNAME (番号から名前: glibc と musl の if_indextoname)。前は ENOTTY だった
- 近くの番地の表 (RTM_GETNEIGH: ip neigh、Firefox の network ID): ARP は smoltcp がする (その表は外から見えない) ので、
  eth0 を通る ARP のフレームから Linux と同じ表を作る (net.rs の受けと出しで netlink::arp_in / arp_out)。smoltcp が問い
  合わせを出したら INCOMPLETE (probes は ucast_probes の 3 から。知っていたものは PROBE)、自分の MAC あての答えで
  REACHABLE、自分への問い合わせで STALE (なければ作る)、ほかは表にあるものだけ STALE。時間で (net::poll から 100 ミリ秒
  ごとと、表を見るとき) REACHABLE は 30 秒で STALE (Linux は 15〜45 秒のどれか)、INCOMPLETE と PROBE は 3 秒で FAILED。
  状態か MAC が変われば RTM_NEWNEIGH を RTMGRP_NEIGH に。DELAY はない (aios は使うたびの確かめの問い合わせを出さず、
  smoltcp が自分の表の 60 秒ごとに問い合わせなおす)。dump は中身と DONE が 1 つのデータグラム (道とちがい
  RTNL_FLAG_DUMP_SPLIT_NLM_DONE がない)、家族は 0 と AF_INET だけ、strict と NDA_IFINDEX。get は neigh_valid_get_req
  のとおり (口がいる、属性は NDA_DST だけ、AF_INET のほかは EAFNOSUPPORT、NTF_PROXY は ENOENT)
- 道を変える (RTM_NEWROUTE / RTM_DELROUTE: ip route add / replace / del): rtm_to_fib_config と fib_create_info と
  fib_table_insert / delete のたしかめの順 (ECN、type、prefix のはみ出しは EINVAL、ゲートウェイが eth0 の網になければ
  ENETUNREACH、broadcast なら EINVAL、あれば NLM_F_EXCL で EEXIST、なくて NLM_F_CREATE がなければ ENOENT、消すものが
  なければ ESRCH)。aios が持てるのは main の表の、eth0 のゲートウェイを通る unicast の道 (smoltcp の道の表に入れる:
  net::set_routes、16 まで) と既定の道だけで、口だけの道、ほかの type や表、metric、prefsrc、同じ宛先の 2 つめ
  (NLM_F_APPEND)、アドレスから作った道を消すのは EOPNOTSUPP (aios のできないこと)
- 知らせ (rtnl_notify): netlink で道を変えたら RTM_NEWROUTE (NLM_F_EXCL|NLM_F_CREATE、replace は NLM_F_REPLACE) か
  RTM_DELROUTE を、送ったソケットのポートと seq で RTMGRP_IPV4_ROUTE に。ioctl (SIOCADDRT / SIOCDELRT / SIOCSIFADDR /
  SIOCSIFNETMASK) と DHCP で変わったら、前とくらべて RTM_DELADDR、RTM_DELROUTE、RTM_NEWADDR (RTMGRP_IPV4_IFADDR)、
  RTM_NEWROUTE をポート 0 で (netlink::changed)。組あての受けの nl_groups は組のビット
- 番地を変える (RTM_NEWADDR / RTM_DELADDR の AF_INET: ip addr add / replace / del): inet_rtm_newaddr と
  inet_rtm_deladdr のたしかめの順 (prefix が 32 より長い、IFA_LOCAL がない、valid が 0 か preferred > valid は EINVAL、
  口がなければ ENODEV、同じ網・長さ・番地があれば NLM_F_EXCL か REPLACE なしで EEXIST、消すものがなければ
  EADDRNOTAVAIL)。eth0 の番地は lease (DHCP か SIOCSIFADDR) と netlink で足したもの (net::Addr4) で、Linux の
  ifa_list の順 (primary のあとに secondary)。先にある番地と同じ網・長さのものは IFA_F_SECONDARY で、道は local
  (prefsrc は primary) と broadcast だけ。primary は fib_add_ifaddr の local、broadcast (brd があれば)、網、網の
  broadcast (IFA_F_NOPREFIXROUTE なら網の道なし)。replace は protocol と lifetime (有限なら PERMANENT がとれる) を
  変える。lifetime は check_lifetime と同じく、preferred が切れたら DEPRECATED、valid が切れたら消す。消すのは
  promote_secondaries 0 の Linux と同じく、primary なら同じ網の secondary もみな (fib_del_ifaddr の網、broadcast、
  local の順で、ほかの番地がまだ使う道はのこす)。lease を消すと DHCP とゲートウェイもやめ、eth0 の番地がなくなれば
  ip route add の道もだまって消える (fib_disable_ip)。送り元は道の prefsrc (secondary の番地へも primary から)。
  smoltcp にも番地を入れる (8 つまで: 127.0.0.1 と IPv6 の場所をのこす)。lo の番地、peer の番地 (point-to-point)、
  lease の lifetime を変えるのは EOPNOTSUPP (aios のできないこと)
  - 知らせ: RTM_NEWADDR (送ったソケットのポートと seq) のあとに足した道 (ポート 0、NLM_F_EXCL|NLM_F_CREATE)。消すと
    RTM_DELADDR のあとに消えた道
  - 確かめかた: `test/ldiff.sh --root --net rtnladdr` (10.0.2.50/24 (secondary) と 192.168.50.1/24 brd label eth0:1 を
    足し、dump、ip route get、送り元、自分の番地への TCP、EEXIST と replace、まちがいの errno、消す)。手本の Linux
    7.4-rc1 と 54 行同じ。ctests にも (root で)
- 確かめかた (つづき): `test/ldiff.sh --root --net rtnl` (10.0.2.3 にはじめて送ったときの ARP と知らせ、dump と get の
  しくじり、ioctl と netlink で既定の道と 192.168.7.0/24 を消して足す知らせ、しくじりの errno。おわりに既定の道を
  もどす)。Linux 7.4-rc1 と 195 行同じ。10.0.2.3 にもう問い合わせた VM では、はじめの知らせがちがって見える
- リンクの答え (rtnl_fill_ifinfo): Linux 7.4 の属性の並びと、lo と virtio-net の値 (NETNS_IMMUTABLE、ALLMULTI、GSO・GRO・
  TSO の大きさ、MAX_PACING_OFFLOAD_HORIZON と 7.4 の PACING_OFFLOAD、CARRIER の数、HEADROOM (virtio-net の hdr の
  12) と TAILROOM、PROTO_DOWN、XDP、PERM_ADDRESS、QDISC (noqueue と pfifo_fast)、AF_SPEC (IPv4 の conf と、IPv6 の
  flags・cacheinfo・conf・統計・token・addr_gen_mode)、MAP、PARENT_DEV_NAME (virtioN: Linux と同じく DTB の順に装置の
  あるものだけ数える virtio::linux_index) と BUS_NAME、中身のない DEVLINK_PORT と DPLL_PIN)。IPv6 の flags は
  IF_READY、IF_RS_SENT、RS のあとに RA を受けたら IF_RA_RCVD (addrconf::if_flags)
- マルチキャストの番地の一覧 (RTM_GETMULTICAST: ip maddr): AF_INET (in_dev_dump_ifmcaddr: どの口も 224.0.0.1)、AF_INET6
  (inet6_dump_ifmcaddr: 口の組 (addrconf::mc_list: ff01::1 と ff02::1、eth0 の番地ごとの solicited-node、ソケットが
  入れたもの) を新しいものから、users は入っているものの数、ff05:: の組は scope が RT_SCOPE_SITE)、7.4 の PF_PACKET
  (dev_mc_dump: eth0 の 33:33:00:00:00:01、01:00:5e:00:00:01、そのあとの IPv6 の組 (ff01:: はのぞく) の MAC を入れた
  順に。IFA_FLAGS の IFA_F_GLOBAL は 0)。どれも中身と DONE が 1 つのデータグラム、ヘッダーの prefixlen・flags・scope は
  0 (PF_PACKET はいつも、ほかは strict のとき見る)、index があればその口だけ (NLM_F_DUMP_FILTERED)。aios は IGMP を
  しないが、入っている組は Linux と同じ。IPv6 の組ができる・なくなると RTNLGRP_IPV6_MCADDR (38) に RTM_NEWMULTICAST /
  RTM_DELMULTICAST (inet6_ifmcaddr_notify: scope はいつも universe)。RTNLGRP_MAX は 7.4 の 39
- 確かめかた: `test/ldiff.sh --net rtnllink` (lo と eth0 のリンクの属性をみな、値も (統計と時刻は長さだけ、virtioN の
  番号と IF_RA_RCVD は見ない)、3 つの家族のマルチキャストと AF_UNSPEC の EOPNOTSUPP)。Linux 7.4-rc1 と 100 行同じ。
  ctests にも
- 確かめかた: `test/ldiff.sh --net route` (手本の Linux は ip=dhcp の virtio-net で、aios の VM と同じ 10.0.2.15/24 と
  ゲートウェイ 10.0.2.2。IPv6 のものは出さない)。dump (家族、strict の表えらび)、1 つの道 (19 の宛先と元と oif、
  fibmatch、strict のしくじり)、Firefox と同じ問いあわせで link up。Linux 7.4-rc1 と 232 行同じ。ldiff に --net を足した

### IPv6 (AF_INET6: ::1 と v4 の番地を写したもの、eth0 の IPv6)

多くのプログラムが IPv6 を先にためす (`::` や `::1` で待つ、名前の AAAA)。aios は AF_INET6 がなく EAFNOSUPPORT だった
(test/features.c で見つけた)。段階 1 は aios の中 (ループバック) と、IPv4 とのつなぎ。

- Linux と同じく 1 つのソケットに v4 の欄 (rcv_saddr、saddr、daddr) と v6 の欄 (rcv6、saddr6、daddr6: sk_v6_rcv_saddr
  など) がある (kernel/src/inet.rs)。IPv4 とのやりとり (`::ffff:a.b.c.d` へ、`::` で待つものへの IPv4) は v4 の欄で
  いつもの道を通り、名前だけ写した番地 (sockaddr_in6) で見せる。IPv6 だけのもの (v6) は v4 の欄が LOOPBACK4_IPV6
  (127.0.0.6) で、v6only なので IPv4 からは見えない。ポートの表は v4 と v6 で 1 つ
- socket(AF_INET6): SOCK_STREAM (TCP) と SOCK_DGRAM (UDP)、SOCK_RAW の IPPROTO_ICMPV6 (段階 2c、root)。ほかの raw は
  EPROTONOSUPPORT、ping ソケット (SOCK_DGRAM の ICMPv6) は ping_group_range (1 0) で EACCES。socketpair は EOPNOTSUPP
- bind (__inet6_bind): 24 バイトから、写した番地は v6only なら EINVAL、IPv6 の番地に bind すると v6only になる、
  fe80:: は口がなければ EINVAL、自分のもの (::1) でなければ EADDRNOTAVAIL、TCP に multicast は EINVAL。ぶつかりは
  inet_rcv_saddr_equal (ipv6_rcv_saddr_equal: `::` は v6only の写した番地でなければ重なる)
- TCP の connect (tcp_v6_connect): `::` は ::1、写した番地は IPv4 の connect (v6only なら ENETUNREACH)、写した番地に
  bind したものから IPv6 へは EAFNOSUPPORT、::1 のほかの IPv6 は ENETUNREACH。listen しているものは inet6 のもの
  (AF_INET6 だけ、その番地か `::`)、IPv4 のさがしかたは v6only をとばし AF_INET を先に。MSS は IPv6 のループバックの
  65476 (つないだ側は窓の半分の 32732)。子は AF_INET6 と IPv6 の設定を引きつぐ
- UDP (udpv6_sendmsg、__ip6_datagram_connect): sockaddr_in6、sockaddr_in (v6only なら ENETUNREACH / EAFNOSUPPORT)、
  AF_UNSPEC。受けた名前は sockaddr_in6 (IPv4 のものは写した番地)、IPV6_PKTINFO (in6_pktinfo)、IPV6_HOPLIMIT、
  IPV6_TCLASS。だれもいない ::1 へは、connect したものの次の送りが ECONNREFUSED
- SOL_IPV6: IPV6_V6ONLY (bind のあとは EINVAL)、UNICAST_HOPS (-1 は 64)、MULTICAST_HOPS (TCP は ENOPROTOOPT)、
  MULTICAST_LOOP、TCLASS (TCP は ECN を変えない)、RECVPKTINFO、RECVHOPLIMIT、RECVTCLASS、MTU_DISCOVER、RECVERR、
  FREEBIND。SOL_IP は IPv4 のもの。ほかの level は ENOPROTOOPT。SO_DOMAIN は 10
- /proc/net/tcp6、udp6 (番地は 32 ビットずつ)、sockstat6、if_inet6 (lo の ::1)。/proc/net/tcp と udp は AF_INET だけで、
  Linux と同じく 1 行を 149 と 127 文字に空白でそろえる (前はそろえていなかった)
- netlink: lo の ::1/128 (scope host、IFA_F_PERMANENT、IFA_PROTO 1)。RTM_GETADDR は AF_INET が v4、AF_INET6 が v6 (中身と
  DONE が 1 つのデータグラム)、ほかは両方。1 つの RTM_GETADDR (AF_INET6)。getifaddrs に lo の inet6 ::1 が出る。
  RTM_GETNEIGH の AF_INET6 は空の表 (ENOENT)。AppArmor は inet6 の決まりで見る
- IPv6 の道 (段階 2a、rt6_fill_node と inet6_rtm_getroute): lo の local ::1 (IPV6_MULTIPLE_TABLES のない Linux と同じく
  表 254、proto kernel、metric 0、RTA_PREF 0)。dump は中身と DONE が別のデータグラム (strict なら NLM_F_DUMP_FILTERED)、
  1 つの道は ::1 なら RTA_PREFSRC ::1 つき、ほかは ENETUNREACH
- ファイアウォール (段階 2b): ::1 の TCP と UDP も lo の 4 つのフックに通す (上の「ファイアウォール (nftables)」の
  IPv6)。ip6 と inet の決まり、reject の RST と ICMPv6、log が Linux と同じ
- eth0 の IPv6 (段階 2c、kernel/src/addrconf.rs: Linux の addrconf.c と ndisc.c の RA のところ)。smoltcp の proto-ipv6
  を入れ、近所さがし (NS と NA) と echo の答えは smoltcp が、ほかはここで:
  - link-local は起こしたときに fe80::/64 + MAC からの EUI-64 (addr_gen_mode 0。IFA_F_PERMANENT、IFA_PROTO 3)。道は
    fe80::/64 (proto kernel、metric 256) と ff00::/8 (RTN_MULTICAST)
  - RS を ff02::2 へ (SLLA つき。RA が来るまで 4 秒から倍に)。RA (link-local から、hop limit 255) を読む: cur hop limit は
    hop_limit と既定の道の RTAX_HOPLIMIT、router lifetime は既定の道 (proto ra、metric 1024、expires)、MTU、prefix
    (L はその道 (proto kernel、metric 256、expires)、A で /64 なら SLAAC の番地: prefix + EUI-64、IFA_F_MANAGETEMPADDR、
    IFA_PROTO 2、preferred と valid の lifetime。切れたら消す)、SLLA は近くの番地の表へ (ルーター)。QEMU の
    ユーザーネットワークでは fec0::5054:ff:fe12:3456/64 (fec0::/10 は site-local: scope 200、if_inet6 の 40)
  - DAD (重複アドレス検出: addrconf_dad_start / addrconf_dad_work / addrconf_dad_completed): 新しい番地 (link-local、
    SLAAC、nodad でない ip -6 addr add) は IFA_F_TENTATIVE で、0〜1 秒 (rtr_solicit_delay) 待って NS を 1 つ
    (dad_transmits。送り元 ::、あて先は solicited-node マルチキャスト、nonce つき: enhanced_dad)、1 秒 (RetransTimer)
    答えがなければ tentative をはずして RTM_NEWADDR と local の道の RTM_NEWROUTE を知らせる。そのあいだは送り元に
    えらばず、そこあては受けず (bind は EADDRNOTAVAIL)、local の道もない (tentative のまま消せば RTM_DELADDR だけ)。
    その番地への NA か、ほかの人 (nonce のちがう) の DAD の NS が来たら IFA_F_DADFAILED (kmsg に duplicate address)。
    RS は link-local の DAD がおわってから。`test/ldiff.sh --root --net dad6` で手本の Linux 7.4-rc1 と 40 行同じ
    (このとき見つけた: netlink の知らせが poll / epoll で待つ人を起こしていなかった。タイマーの中から来る知らせ (DAD の
    おわり) では poll が時間切れまで起きなかったので、deliver で poll_wake も)
  - MLD はしない (QEMU のユーザーネットワークではだれも聞いていない)
  - 送り元のえらびかた (ipv6_dev_get_saddr のうち scope と deprecated と prefix の長さ)、道のさがしかた (長い prefix、
    metric)。自分の番地へは lo (IPv4 と同じく相手の行列へじかに)、ほかは smoltcp。link-local は scope id がいり
    (なければ EINVAL、eth0 でなければ ENETUNREACH)、名前の scope id は eth0 (aios の口は 1 つ)
  - 外への TCP と UDP は smoltcp のソケット (送り元も決めて)。listen は `::` か eth0 の番地なら外からも受け、IPv6 の
    つながりは v4 の欄が LOOPBACK4_IPV6。eth0 から来た ICMPv6 のエラーは中のもとのパケットの組のソケットに
    (tcp_v6_err: つなぐ途中ならどのエラーでも止める。__udp6_lib_err: connect したものに致命のもの)。QEMU のホストに
    IPv6 がなければ、外へのつなぎは ICMPv6 の no route で ENETUNREACH になる (Linux も同じ)
  - raw の ICMPv6 (rawv6: ICMPv6 の頭から送り受け、チェックサムはカーネル、ICMP6_FILTER、IPV6_UNICAST_HOPS)。ping6 が
    外 (fec0::2、fe80::2) と自分の番地に届く
  - netlink: eth0 の番地 (scope、flags、IFA_PROTO、のこりの lifetime)、道 (上のもの、自分の番地の RTN_LOCAL: dump の口は
    eth0、get の答えは lo)、1 つの道 (prefsrc、via、RTA_METRICS)、近くの番地の表の IPv6 (nd_tbl: smoltcp が出す NS で
    INCOMPLETE、NA で REACHABLE か STALE、R はルーター (NTF_ROUTER)、RA の SLLA で STALE)。/proc/net/if_inet6 と
    ipv6_route (refcnt はいつも 1)、/proc/sys/net/ipv6/conf/{all,default,lo,eth0}/ (addrconf::CONF。Linux の
    ipv6_devconf) と net/ipv4/ping_group_range。conf は accept_dad、accept_ra、autoconf、dad_transmits、hop_limit
    (1〜255)、mtu (1280〜口の MTU、all と default は上限なし)、use_tempaddr を書ける (符号つきの数、ほかは EINVAL)。効くの
    は eth0 のもの: hop_limit と mtu (RA でも変わる)、accept_ra が 0 なら RA を見ない、autoconf が 0 なら SLAAC の番地を
    作らない、DAD は all か eth0 の accept_dad が 1 以上のときに dad_transmits 回 (0 なら NS なしで 0〜1 秒あとにおえる)。
    use_tempaddr は覚えるだけ。addr_gen_mode、disable_ipv6、forwarding は読むだけ (Linux は書ける)。ip -6 link の
    IFLA_INET6_CONF にも同じ値。`test/ldiff.sh --root --net sysctl6` で手本の Linux 7.4-rc1 と 88 行同じ
- ファイアウォール (段階 2d): eth0 の IPv6 のフレームも決まりを通る (上の「ファイアウォール (nftables)」の IPv6)
- 近所さがし (ARP と NS) のだまる時間 (1 秒) はあて先ごと (Linux の neighbour ごとの retrans_time)。smoltcp 0.12 (0.14 も)
  は表全体で 1 つで、1 つ問い合わせると、どのあて先にも 1 秒は問い合わせず、答えのないあて先を問い合わせなおすたびに
  またそうなるので、ほかのあて先はいつまでも問い合わせてもらえなかった (IPv4 の 2 つ目と IPv6 がみな止まった)。smoltcp は
  写し (kernel/vendor/smoltcp、ルートの Cargo.toml の [patch.crates-io]) で直した: 変えたところは
  kernel/vendor/smoltcp/AIOS.md。test/ndsilence.c (ldiff --net、ctests にも) が 10.0.2.200/.201 と fec0::200/::201 へ
  別々のソケットで続けて送り、0.3 秒あとに 4 つとも INCOMPLETE (Linux 7.4-rc1 と 3 行同じ)。nfteval6 の ping6 の待ちも
  2.5 秒から 1 秒にもどした
- 近所さがしの失敗 (Linux の neigh_invalidate): 1 秒ごとに 3 度 (mcast_probes) 問い合わせて答えがなければ、待っていた
  パケットを捨て、自分から自分へ「たどりつけない」を返す (arp_error_report → ipv4_link_failure の ICMP host unreachable、
  ndisc_error_report → ip6_link_failure の ICMPv6 address unreachable。送り元とあて先は捨てたパケットの送り元、中身は
  捨てたものの頭から 576 / 1280 バイトまで、ICMP のエラーと断片の 2 つめから先には返さない: net.rs の link_failure)。
  TCP の connect は 3 秒ほどで EHOSTUNREACH (なにも送らずに終わる: 写しの smoltcp に足した TcpSocket::forget。smoltcp の
  close と abort は SYN_SENT でも RST を送っていた)、raw のソケット (ping) はその ICMP を受け、つないだ UDP は (Linux と
  同じく host unreachable は致命でないので) 知らない。近所の表は FAILED のまま、次に送るものがまた問い合わせはじめる。
  smoltcp は写しで: あて先ごとに問い合わせた数をおぼえ (間があけば数えなおす)、3 度で NeighborFailed、そのパケットは
  送ったことにしてソケットの列から外し (前は答えのないあて先へのパケットが列の先にいつまでも残り、うしろも出なかった)、
  Interface::take_neighbor_failures で aios に渡す。近所の表 (smoltcp の neighbor cache) は 8 から 64 に
  (iface-neighbor-cache-count-64)。test/ndsilence.c (--root --net、ctests にも) が Linux 7.4-rc1 と 17 行同じ (raw の
  ソケットに来る順は時間しだいなのでならべる、DAD の終わりを待つ: Linux は DAD のあいだの番地を送り元にしない)。
  まだ: 1 つのソケットの列に同じあて先へのものがいくつもあると、捨てるのは先のものだけで、うしろはまた 3 秒問い合わせる
  (Linux は待っていたものをみな捨てる)
- conntrack (段階 2e): IPv6 も (上の「ファイアウォール (nftables)」の IPv6)
- netlink で変える (段階 2f: inet6_rtm_newaddr / deladdr、inet6_rtm_newroute / delroute) と知らせ:
  - ip -6 addr add: eth0 の番地 (nodad でなければ DAD のあいだ tentative)、IFA_FLAGS (NODAD、NOPREFIXROUTE、MANAGETEMPADDR など)、
    IFA_CACHEINFO の lifetime (valid がいつまでもなら IFA_F_PERMANENT)、IFA_PROTO。prefix の道 (proto kernel、metric 256、
    lifetime があれば expires) を作る。NLM_F_CREATE がなくても作り、もうあれば NLM_F_EXCL か REPLACE なしで EEXIST、
    REPLACE なら flags と lifetime を変える。prefix が 128 より長いと EINVAL、口がなければ ENODEV、::、::1、
    マルチキャストは EADDRNOTAVAIL。lo は EOPNOTSUPP
  - ip -6 addr del: 番地と長さのあうもの (なければ EADDRNOTAVAIL)。同じ prefix の番地がもうなければ prefix の道も
  - ip -6 route add / del: unicast の道 (metric の既定は 1024、proto は来たもの): ゲートウェイを通るものと、口だけの
    もの (ip -6 route add 2001:db8:2::/64 dev eth0: RTF_UP だけで、あて先へじかに。近所さがしはあて先そのもの)。
    link-local のゲートウェイに口がなければ EINVAL、口もゲートウェイもなければ ENODEV、同じあて先と長さと metric が
    あれば EEXIST (REPLACE なら入れかえ)。del はあうもののうち metric の小さいもの (なければ ESRCH)。RA の既定の道も
    消せる (つぎの RA でまたできる)。unicast でないものは EOPNOTSUPP。smoltcp の道の表には prefix ごとに metric の
    いちばん小さいもの (口だけの道は via が :: で、写しの smoltcp はそれをあて先そのものへ: kernel/vendor/smoltcp/AIOS.md)。
    `test/ldiff.sh --root --net onlink6` で手本の Linux 7.4-rc1 と 27 行同じ
  - 知らせ: RTMGRP_IPV6_IFADDR と RTMGRP_IPV6_ROUTE (番地を足すと prefix の道、RTM_NEWADDR、local の道の順、消すと
    RTM_DELADDR、local の道、prefix の道。道の RTM_NEWROUTE は NLM_F_EXCL|NLM_F_CREATE)。RA と lifetime の時間切れで
    変わったものは前とくらべて
  - 送り元のえらびかたに RFC 6724 の rule 2 (あて先の scope 以上で狭いもの: global のあて先には fec0:: より fd00::)
  - 確かめかた: `test/ldiff.sh --root --net rtnl6` (fd00::1/64 と fd00::3/64 を足して入れかえて消し、2001:db8:1::/48 via
    fe80::2 を metric 100 と 200 で足して消す。まちがいの errno、知らせ、dump、ip route get、送り元)。手本の Linux 7.4-rc1
    と 67 行同じ。ctests にも
- エラーの行列 (IPV6_RECVERR と IP_RECVERR、MSG_ERRQUEUE。Linux の __udp6_lib_err → ipv6_icmp_error と
  ipv6_recv_error): UDP が ICMPv6 のエラー (lo の閉じたポートへの port unreachable、ファイアウォールの reject、eth0 から
  来たもの) を受けると、RECVERR ならつないでいなくても行列に入れて sk_err にも (次の recv や send が ECONNREFUSED を
  返す)、でなければ致命のものでつないだものだけ sk_err に。読むと中身は送ったものの UDP の後ろ (ICMPv6 は 1184 バイト
  まで、ICMP は 520)、名前は送ったあて先、付帯データは IPV6_PKTINFO (たのんでいれば: その ICMPv6 のあて先と口) と
  SOL_IPV6 / IPV6_RECVERR (sock_extended_err、origin は SO_EE_ORIGIN_ICMP6、知らせてきた番地)。RECVERR を切ると行列は
  捨てる。`test/ldiff.sh recverr6` (v6 と v4) で手本の Linux 7.4-rc1 と 120 行同じ
- RA のユーザーに渡す項目 (RDNSS など。Linux の ndisc_is_useropt と ndisc_ra_useropt): RA の prefix (3)、RDNSS (25)、
  DNSSL (31)、6CO (34)、captive portal (37)、PREF64 (38) を、RA を読みおえてから 1 つずつ RTM_NEWNDUSEROPT で
  RTNLGRP_ND_USEROPT (20) に (struct nduseroptmsg、その項目のバイト、NDUSEROPT_SRCADDR)。DNS をつかう人 (rdisc6 や
  NetworkManager のようなもの) はこれを聞く。`test/ldiff.sh --root --net rdnss6` (raw の ICMPv6 で RS を送り、QEMU の
  RA の prefix の項目) で手本の Linux 7.4-rc1 と 7 行同じ (このとき見つけた: raw の ICMPv6 が IPV6_MULTICAST_HOPS を
  見ず、マルチキャストへはいつも hop limit 1 で、QEMU は RS に答えなかった)
- RA でもうある SLAAC の番地の lifetime を新しくすると (addrconf_prefix_rcv_add_addr の update_lft)、DAD のとちゅうで
  なければ RTM_NEWADDR (番地と flags は変わらないので、前とくらべる知らせには出ない)。rdnss6 で同じ (8 行)
- path MTU (packet too big: icmpv6_notify → __udp6_lib_err と rt6_update_pmtu): eth0 から来たもの (と、自分から自分へ
  送ったもの: raw の ICMPv6 で作ったものも lo で受ける) で、もとのあて先の path MTU を下げる (addrconf の例外の道:
  1280 より小さいものと、いまより小さくならないものは見ない、10 分で消える)。UDP には EMSGSIZE (IPV6_RECVERR の行列は
  ee_info に知らされた MTU。IPV6_MTU_DISCOVER が DONT でなければ致命、INTERFACE と OMIT は見ない)。IPV6_MTU
  (getsockopt) はつないだもののあて先への path MTU (つないでいなければ ENOTCONN)、ip -6 route get はその例外の道
  (RTM_F_CLONED、RTAX_MTU、expires)。TCP の MSS は smoltcp が変えられないので、そのまま。送るときに path MTU で
  断片にするのと EMSGSIZE は下の「MTU より大きい UDP」。`test/ldiff.sh --root --net pmtu6` で手本の Linux 7.4-rc1 と 22 行同じ
- マルチキャストの組 (Linux の net/ipv6/mcast.c): UDP のソケットが IPV6_JOIN_GROUP / IPV6_LEAVE_GROUP (struct
  ipv6_mreq。TCP は EPROTO) と MCAST_JOIN_GROUP / MCAST_LEAVE_GROUP (struct group_req。AF_INET6 でなければ
  EADDRNOTAVAIL) で組に入る・出る (ipv6_sock_mc_join / drop: マルチキャストでなければ EINVAL、同じ組に 2 度は
  EADDRINUSE、口が 0 なら道 (ff00::/8 は eth0) から、ない口は ENODEV、入っていない組を出るのは EADDRNOTAVAIL、口が 0
  なら新しく入ったほうを出る)。閉じると入っていた組をみな出る。口の組 (addrconf::mc_list) の users を足し引きし、
  できた・なくなった組は netlink が知らせる。組あてに送ると (ip6_finish_output2) hop limit は IPV6_MULTICAST_HOPS、
  IPV6_MULTICAST_LOOP で eth0 がその組に入っていれば自分にも (受けた口は eth0。決まりには通さない)、ff01:: は外に
  出さない。受けるのは (__udp6_lib_mcast_deliver) 同じポートの AF_INET6 のソケットみな (bind した番地が :: かその組、
  つないだ相手が合い、その組に入っているか IPV6_MULTICAST_ALL)。eth0 から来た組あての UDP も smoltcp にわたさずに
  inet.rs が同じように (チェックサムのおかしいものは捨てる)。IPV6_PKTINFO / IP_PKTINFO の口は受けた口 (外からの
  ものは eth0、前はいつも lo だった)。`test/ldiff.sh --net mld6` で手本の Linux 7.4-rc1 と 110 行同じ
- MLDv2 (Linux の mcast.c): eth0 に新しい組ができると CHANGE_TO_EXCLUDE (4)、なくなると CHANGE_TO_INCLUDE (3) の記録
  (送り元なし) を robustness (2) 回 (mld_ifc_event と mld_send_cr: すぐと、0〜1 秒あと。続けて変わったものは 1 つの報告
  に)。ff02::1 と ff01:: の組 (MAF_NOREPORT) と、もうある組に入るだけのものは報告しない。報告は ff02::16 へ、hop limit
  1、hop-by-hop の Router Alert つき、送り元は link-local (DAD のとちゅうなら ::)、ファイアウォールの OUTPUT を通る。
  ルーターの MLDv2 の問い合わせ (130、link-local から) には Maximum Response Delay までのでたらめな時刻に、general なら
  入っている組みな、組あてならその組を MODE_IS_EXCLUDE (2) で (QEMU には問い合わせるものがいないので答えあわせは
  していない)。MLDv1 のルーターには合わせない。起動のときの solicited-node の組は報告しない (Linux は DAD のはじめに
  する)。`LINUX_FILES="nft=build/nft-static" test/ldiff.sh --root --net mldrep6` (nftables の output の鎖の counter で
  数える。aios には nftables のパッケージがいる) で手本の Linux 7.4-rc1 と 107 行同じ
- 一時アドレス (RFC 8981。Linux の manage_tempaddrs、ipv6_create_tempaddr、ipv6_gen_rnd_iid): eth0 の use_tempaddr が
  0 より大きければ、RA で SLAAC の番地ができたとき (と、新しくしたときに一時アドレスがひとつもなければ)、同じ /64 に
  でたらめな IID (0、IANA の Ethernet のところ、subnet anycast はよける) の番地 (IFA_F_TEMPORARY、proto 0) を作る。valid は
  SLAAC のものと temp_valid_lft (172800) の小さいほう、preferred は SLAAC のものと temp_prefered_lft (86400) -
  desync_factor (max_desync_factor 600 より小さいでたらめな秒) の小さいほう (regen_advance: regen_min_advance 2 +
  regen_max_retry 3 × dad_transmits × 1 秒より短くなるなら作らない、max_addresses 16 まで)。RA が SLAAC の番地を新しく
  すると一時アドレスの lifetime もその上限までのばして RTM_NEWADDR、preferred がのこり regen_advance 秒になると次のもの
  を 1 つ作る。これらの sysctl も /proc/sys/net/ipv6/conf と IFLA_INET6_CONF に。送り元のえらびかたの rule 7: use_tempaddr
  が 2 から (か IPV6_ADDR_PREFERENCES の IPV6_PREFER_SRC_TMP) なら一時アドレスを先に、PREFER_SRC_PUBLIC ならあとに
  (rule 8 の prefix は番地の prefix の長さまで)。IPV6_ADDR_PREFERENCES (72) は TMP・PUBLIC・PUBTMP_DEFAULT、HOME・COA、
  CGA・NONCGA がぶつかれば EINVAL、getsockopt は TMP / PUBLIC / PUBTMP_DEFAULT と COA / HOME。カーネルが作った番地
  (link-local、SLAAC、一時アドレス) は DAD のあいだは知らせない (addrconf_dad_begin。ip -6 addr add のものは知らせる)。
  番地の一覧は idev->addr_list の順 (scope の値の大きいもの (site-local 0x40、link-local 0x20、global 0) が先、同じなら
  新しいものが先)。`test/ldiff.sh --root --net temp6` (RS を送って QEMU の RA で作る。IID は <temp>、lifetime は SLAAC の
  ものとくらべる。/proc/net/if_inet6 は Linux はハッシュの順なので並べる) で手本の Linux 7.4-rc1 と 83 行同じ
- MTU より大きい UDP (Linux の __ip6_append_data・ip6_fragment と __ip_append_data・ip_fragment): 送るときの MTU は
  IPv6 は path MTU (packet too big で下がったもの、なければ eth0 の mtu。IPV6_MTU_DISCOVER が PROBE からは口の MTU、
  自分あては lo の mtu。IPV6_MTU の frag_size (0 か 1280 から) がそれより小さければそれ)、IPv4 は 1500 (自分あては 65536)。
  IPV6_DONTFRAG (ソケットか sendmsg の付帯データ: int でなければ EINVAL) で入らなければ EMSGSIZE で、IPV6_RECVPATHMTU
  なら次の recvmsg が 0 バイトと IPV6_PATHMTU (struct ip6_mtuinfo: あて先、口、MTU。1 つだけ: ipv6_local_rxpmtu)。
  IPV6_MTU_DISCOVER / IP_MTU_DISCOVER が DO・PROBE・INTERFACE で入らないか、断片にしても 65535 に入らなければ
  EMSGSIZE。どれも IPV6_RECVERR / IP_RECVERR ならエラーの行列に SO_EE_ORIGIN_LOCAL (中身 0 バイト、ee_info は MTU、知らせて
  きた番地なし、付帯データの口なし、名前は IPv6 は送ったあて先とポートで scope 0、IPv4 はつないだ相手のポート。sk_err は
  変えない: sock_dequeue_err_skb)。getsockopt の IPV6_PATHMTU はつないだあて先への path MTU (番地は 0、つないでいなければ
  ENOTCONN)。ほかは eth0 で断片にする (net.rs の fragments: 決まりはまるごとのものを見たあと)。IPv6 は Fragment ヘッダ、
  上の層は ((MTU - 40) & !7) - 8 バイトずつ (1500 なら 1448)、identification はでたらめ。frag_size があると断片の大きさは
  それで決まるが、断片にするかは path MTU で決まる (1281〜1500 は 1 つのまま: ip6_finish_output のくせ)。IPv4 は 1480
  バイトずつ、DF をはずして MF と offset (smoltcp の写しは MTU より大きい IPv4 をまるごとわたすように: AIOS.md)。virtio の
  送る入れ物は 16 こなので、送り待ちの列 (64 まで) から入れ物があくたびに出す。前は IPv6 の大きなものでカーネルが
  panic し (virtio の入れ物より大きいフレーム)、IPv4 のものは黙って捨てていた。smoltcp の UDP の送る入れ物は 64 KB に
  (65527 バイトのデータグラム)。`test/ldiff.sh --root --net frag6` (断片は外に出るだけなので eth0 の tx_packets と
  tx_bytes で数える。近所さがしが入らないように IPv6 はマルチキャスト、IPv4 はブロードキャストへ。Linux の RS のくりかえし
  は router_solicitations を 0 にして止める) で手本の Linux 7.4-rc1 と 160 行同じ
- 確かめかた: `test/ldiff.sh inet6` (ネットワークのない手本の Linux とくらべる。ソケット、bind、ぶつかり、::1 の TCP、
  二つで一つ、connect のしくじり、::1 の UDP と IPV6_PKTINFO、netlink と getifaddrs、/proc/net、IPv6 の道。eth0 の
  ものは出さない)。Linux 7.4-rc1 と 170 行同じ。route、rtnl、netlink も同じまま。ファイアウォールは `test/nfteval6.c`
  (171 行同じ)。eth0 の IPv6 は `test/ldiff.sh --root --net inet6net` (手本の Linux も ip=dhcp の virtio-net で、MAC が
  同じなので番地も同じ。SLAAC を待ち、if_inet6、RTM_GETADDR、getifaddrs、道の dump と get、送り元、自分の番地への
  TCP と UDP、外への TCP、ping6、近くの番地、sysctl、ipv6_route): 78 行同じ。ctests にも (root で)

### CPU の印 (AT_HWCAP、AT_HWCAP2、/proc/cpuinfo)

プログラムは、この CPU で使える命令を AT_HWCAP と AT_HWCAP2 (getauxval) か /proc/cpuinfo の `Features` で知る。前の aios
は fp、asimd、aes、pmull、sha1、sha2、crc32、atomics と cpuid だけを出し (cpuinfo は `fp asimd` だけ)、AT_HWCAP2 はなかった。
なので dotprod (asimddp)、fp16 (asimdhp)、i8mm を見て道を選ぶもの (llama.cpp の ggml、zstd、dav1d など) は、Apple M や
cortex-a76 でも、いちばんおそい道を選んでいた。いまは Linux (arch/arm64/kernel/cpufeature.c の arm64_elf_hwcaps) と同じ
決まりで、cpuid のほかに ID レジスタから 42 の印を出す (kernel/src/exec.rs の CAPS。cpuinfo の名前と順も Linux と同じ)。カーネルが
使わせていないもの (SVE と SME、MTE、BTI、ポインタ認証、GCS、MOPS、WFxT) と、仕掛けのないもの (evtstrm) は出さない。
ssbs は出す (PSTATE.SSBS をスレッドごとに扱う: 下の「CPU のぜい弱性」。ID_AA64PFR1 の SSBS の欄も EL0 に見せる)。
EL0 の MRS で読む ID レジスタ (trap.rs の emulate_mrs) も同じものを見せる: ID_AA64ISAR2 の RPRES、BC、RPRFM、CSSC を
足し、ID_AA64PFR0 は DIT を足して、EL0 と EL1 を「AArch64 だけ」にした (aios は AArch32 のプログラムを動かさない)。
test/cpuid.c が、MRS で読んだ ID レジスタ、AT_HWCAP / AT_HWCAP2、/proc/cpuinfo の 3 つが合うかを見る (QEMU の
`-cpu max` で、aios も Linux 7.3 も同じ 35 の印)。TCG の VM の CPU は `AIOS_CPU=max` で変えられる (bin/run.sh。既定は
cortex-a72)。

### UEFI のランタイムサービスと efivarfs (/sys/firmware/efi/efivars)

UEFI から起動したとき (aiboot、systemd-boot、ファームウェアからじか)、ExitBootServices のあとも UEFI の変数を読み書き
できる。systemd-boot の `bootctl`、`efibootmgr`、systemd の `LoaderEntrySelected` などを読むものがそのまま動く:

- EFI スタブ (kernel/src/efi.rs) が、ExitBootServices のときのメモリマップからランタイムの領域 (EFI_MEMORY_RUNTIME)
  と、システムテーブル、ランタイムサービスの表、ファームウェアの名前、設定テーブル (ACPI、SMBIOS) を覚える。
  kalloc はランタイムの RAM を使わない (ページの表もそこを避けて置く)
- kernel/src/efirt.rs が、ランタイムの領域を物理アドレスと同じ仮想アドレス (1:1) に写した表を作り、呼ぶあいだだけ
  TTBR0 をそれにする (SetVirtualAddressMap は呼ばない)。カーネルの半分 (TTBR1) はそのままなので、引数のバッファは
  カーネルのもののまま。ひとつのロックで (割り込みも止める)、ユーザーの FP/SIMD のレジスタと x18 は戻す。
  起動のときに Linux と同じく `efi: EFI v2.7 by EDK II` と出す
- /sys/firmware/efi/ に `fw_platform_size` (64)、`systab` (root だけ。ACPI20= SMBIOS3= など)、`efivars/`
  (arm64 の Linux と同じもの)。efivarfs は起動のときに /sys/firmware/efi/efivars にかぶせる (Linux では systemd が。
  `mount -t efivarfs` も。UEFI から起動していなければ ENODEV)
- efivarfs (kernel/src/efivarfs.rs) は Linux 7.3 の fs/efivarfs と同じ: ファイルの名前は「名前-GUID」(GUID は
  大文字小文字をくべつしない)、読むと属性 (4 バイト) と中身、1 回の write が 1 回の SetVariable (頭の 4 バイトが属性。
  知らないビットは EINVAL、APPEND_WRITE で足す、中身 0 バイトで消す)、作っただけ・消したものは 0 バイトで、閉じると
  消える。Linux が知っている変数 (EFI の BootOrder、Boot####、Timeout、Lang など) のほかは変えられない印
  (FS_IMMUTABLE_FL) がつく: 書く・消すには `chattr -i` (FS_IOC_SETFLAGS) を先に。Boot#### と Driver#### は
  EFI_LOAD_OPTION の形を確かめる (Linux は名前の長さに NUL を足してくらべるので、* のない名前は確かめない。同じにした)。
  statfs は QueryVariableInfo (f_bsize 1)
- 確かめかた: `test/efivarfs.c` (root で。aios の中と、手本の Linux を `LINUX_EFI=1 test/linux-vm.sh rootexec`
  で UEFI のファームウェアから起こしたもので同じ答え: aios 73、Linux 74 (efivarfs をかぶせる 1 つが多い)。CI の
  ctests でも sudo で)。手本の Linux の設定 (test/linux.config) に CONFIG_EFI と
  CONFIG_EFIVAR_FS を入れた (-kernel でじかに起こすときは使わない。sysdiff の答えは入れる前と同じ)

### CPU のぜい弱性 (/sys/devices/system/cpu/vulnerabilities、SSBS)

`/sys/devices/system/cpu/vulnerabilities/` に Linux 7.3 と同じ 19 のファイル (meltdown、spectre_v1、spectre_v2、
spec_store_bypass と、x86 などのもの) を出す (kernel/src/cpuvuln.rs)。「当たるか」は Linux の arm64 と同じ決め方
(proton-pack.c と cpufeature.c): MIDR の安全なものの表、ID レジスタ (CSV2、CSV3、SSBS、ECBHB)、ファームウェアへの
問いあわせ (SMCCC 1.1 の ARCH_FEATURES で ARCH_WORKAROUND_1 / 2。PSCI と同じ hvc / smc)。「対策」は aios がしている
ものだけを書く:

| ファイル | aios | Linux |
|---|---|---|
| meltdown | 当たれば Vulnerable (KPTI をしない) | KPTI で Mitigation: PTI |
| spectre_v1 | Mitigation: __user pointer sanitization (ユーザーのポインタはユーザーのページ表を引いてから、カーネルの写しで読み書きする: vm.rs の copy_in / copy_out) | 同じ文 (uaccess のマスク) |
| spectre_v2 | CSV2 か安全な表のものは Not affected。ほかは Vulnerable (ファームウェアの WORKAROUND_1 を呼ばない)。BHB はハードウェア (ECBHB) だけ | ファームウェアと BHB のループ / CLEARBHB / WORKAROUND_3 |
| spec_store_bypass | SSBS があれば Linux と同じ「prctl で」: カーネルは SSBS を落として動き (SCTLR_EL1.DSSBS = 0)、ユーザーはふだん立てて、prctl で止めたスレッドだけ落とす。SSBS がなければ Vulnerable | SSBS がなければファームウェアの WORKAROUND_2 |

prctl の PR_GET/SET_SPECULATION_CTRL (PR_SPEC_STORE_BYPASS: ENABLE、DISABLE、FORCE_DISABLE、DISABLE_NOEXEC。
ほかの種類は ENODEV、ちがう値は ERANGE) と、/proc/PID/status の `Speculation_Store_Bypass` と
`SpeculationIndirectBranch` も Linux と同じ。印はスレッドごとで fork で受けつぎ、DISABLE_NOEXEC は exec で外れ、
PSTATE.SSBS はスケジューラが切りかえのたびに印から決めなおす (Linux の __switch_to と同じ)。sigreturn も SSBS を残す。
AT_HWCAP と /proc/cpuinfo に ssbs を出し、EL0 の MRS で読む ID_AA64PFR1 も SSBS の欄だけ見せる。
QEMU の cortex-a72 と max で、aios と Linux 7.3.1 の答えがみな同じ (`test/cpuvuln.c`: 出すだけのテストなので、
`LINUX_CPU=cortex-a72 test/linux-vm.sh exec latest ...` の出力とくらべる。cortex-a72 は spectre_v2 と
spec_store_bypass が Vulnerable、max は `Mitigation: CSV2, but not BHB` と `... disabled via prctl`)。
Linux より弱いところ (KPTI、BHB のループ、ファームウェアの手当て) は、その CPU で動かすときに足す。

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

### Linux 7.4 に合わせる (7.4-rc1、2026-11-02)

Linux 7.4 は 11 月 1 日に rc1 が出た (安定版はまだ 7.3.1)。新しい機能は merge window で rc1 までに入るので、手本を 7.4-rc1 に
して答えあわせをした (test/linux-vm.sh の `mainline`)。システムコールの表 (scripts/syscall.tbl) は 7.3 と同じ。aios に
かかわる変わりめと、したこと:

- close_range: `CLOSE_RANGE_EXCEPT` (8。範囲の外のものに。範囲は残すもの) と `CLOSE_RANGE_CLOEXEC_ONLY` (16。FD_CLOEXEC の
  ものだけ閉じる。CLOEXEC とはいっしょに使えず EINVAL)。閉じるのは大きい番号から (close_range、exec の close-on-exec)。
  aios は前から知らない印を見ずに閉じていた (8 をわたすとみな閉じた) ので、7.4 と同じく EINVAL に。CLOSE_RANGE_UNSHARE も
  無視していたので、スレッドと分けている fd の表なら先に自分のものにする。`test/closerange.c` (22 の case: 新しい印、
  まちがった印、UNSHARE、スレッドから見た fd) が 7.4-rc1 と 1 バイトもちがわない (6.18 と 7.3 では新しい印は EINVAL)
- pread64 / pwrite64 / preadv / pwritev (fs/read_write.c): たしかめる順が、fd (EBADF)、負の off (EINVAL)、位置のないもの
  (ESPIPE) に (7.3 までは負の off の EINVAL がいちばん先)。sysdiff の `pwrite64(-1, ..., -1)` が EBADF になった。ついでに、
  aios は preadv と pwritev でも off の -1 を「いまの位置」にしていた (それは preadv2 と pwritev2 だけ。Linux の preadv(-1) は
  EINVAL) のを直した
- copy_file_range: 写すものが 0 バイトなら、書く側の大きさの上限 (EFBIG) を見ない
- SO_PASSPIDFD_THREAD / SO_PEERPIDFD_THREAD: 下の「相手を pidfd で見る」(2026-11-03)
- SECCOMP_FILTER_FLAG_RESTART_BEFORE_RECV: 下の「seccomp の user notification」(2026-11-03)
- Landlock の ABI 12: 下の「Landlock の ABI 12」(2026-11-03)
- まだ (aios にもとからないもの): OPEN_TREE_DROP_MNTNS_MOUNTS、ctnetlink の CTA_FILTER_ZONE、RTM_GETMULTICAST (PF_PACKET) と IFA_F_GLOBAL、リンクの IFLA_PACING_OFFLOAD
  (aios のリンクの答えはもとから一部の属性だけ)

たしかめたこと: sysdiff は CI と同じ 800 case (test/sysdiff/ を 7.4-rc1 で作りなおした) が 0 differ、aish の sysdiff (300) も
0 differ。test/nftables.c の出力 (457 行) と test/netlink.c (79) も 7.4-rc1 と同じ。test/nfteval.c (394 行)、
test/chosen.c (aios がえらんだちがいは 7.4 でもみな Linux とちがう)、test/inetsock.c (81) と test/unixsock.c (60) は、Linux 7.3.1
と 7.4-rc1 の答えが同じ (7.3.1 と同じだった aios はそのまま)。

### 相手を pidfd で見る (SO_PEERPIDFD、SCM_PIDFD、7.4 の _THREAD)

systemd や dbus-broker は、AF_UNIX の相手を番号 (SO_PEERCRED の pid は、相手が終わって番号が使いまわされると別人を指す)
ではなく pidfd で持つ。aios は ENOPROTOOPT だった。Linux 7.4-rc1 の net/core/sock.c、scm.c、af_unix.c、fs/pidfs.c と同じに:

- SO_PEERPIDFD (77): 相手 (connect した、listen した、socketpair を作った人。SO_PEERCRED と同じ時に覚える) を指す pidfd を
  fd の表に入れて返す (長さは 4 まで、いつも close-on-exec)。相手がいなければ ENODATA。相手がもう片づけられていても作る
  (Linux の PIDFD_STALE: シグナルは ESRCH、poll はすぐ POLLIN | POLLHUP)。SO_PEERPIDFD_THREAD (87、7.4) は、つないだ
  スレッドを指す PIDFD_THREAD の pidfd
- SO_PASSPIDFD (76) と SO_PASSPIDFD_THREAD (86、7.4): 受けたものに SCM_PIDFD (4) と SCM_PIDFD_THREAD (5) を足す。並びは
  SCM_CREDENTIALS、SCM_RIGHTS、SCM_PIDFD、SCM_PIDFD_THREAD。1 つ 20 バイトが入らなければ MSG_CTRUNC (送った人がわからなく
  ても)、msg_control が NULL でも MSG_CTRUNC。送ったものに資格情報がつくのは、どちらかがこの 3 つ (SO_PASSCRED もふくむ:
  7.4 の unix_may_passcred) のどれかのとき。名前のないものは autobind。accept した口は listen したものから受けつぐ
- stream は、SO_PASSPIDFD_THREAD なら、同じプロセスでも別のスレッドが書いたものをつなげない (7.4 の unix_skb_scm_eq)
- メッセージと相手の資格情報 (unix::Cred) に、送った・つないだスレッドの番号を足した。SCM_CREDENTIALS で自分のプロセスを
  名のれば、そのスレッドは自分
- AF_INET: SO_PEERPIDFD は ENODATA、ほかの 3 つは EOPNOTSUPP
- pidfd: PIDFD_THREAD (O_EXCL。F_GETFL に見える) の pidfd を持てるように。pidfd_open は PIDFD_THREAD でスレッドの番号を
  受け、代表でないスレッドを PIDFD_THREAD なしで指せば ENOENT (7.4)、終わってまだ片づけられていないものも開ける。
  pidfd_send_signal は PIDFD_THREAD ならそのスレッドに、ほかはプロセスに (kill と同じく、ゾンビには送れて何も起きない:
  前は ESRCH)。poll は終わったら POLLIN、片づけられたら POLLHUP も (前は終わったら POLLHUP も)

`test/peerpidfd.c` (71 行: pidfd_open、socketpair・listen・connect・accept・片づけたあとの SO_PEERPIDFD と _THREAD、
SO_PASSPIDFD の get / set、AF_INET の TCP と UDP、dgram の SCM_PIDFD (子が送って終わった、片づけたあと)、入れ物の大きさ
7 とおりの MSG_CTRUNC、SCM_RIGHTS といっしょの並び、スレッドが送ったもの、stream のつなぎかた) は Linux 7.4-rc1 と 1 バイトも
ちがわない。pidfd がどれを指すかは、子なら waitid(P_PIDFD) の si_pid、スレッドなら pidfd_send_signal で起きたハンドラの
gettid で見る。/proc/PID/fdinfo (pidfd の Pid:) と PIDFD_GET_INFO は下の「pidfd のことを答える口」(2026-11-03)。

### seccomp の user notification (NEW_LISTENER、RET_USER_NOTIF、7.4 の RESTART_BEFORE_RECV)

seccomp のフィルタが SECCOMP_RET_USER_NOTIF と答えたシステムコールを、別のプロセスに決めてもらう口 (Linux 5.0 から。コンテナの
道具 (podman、LXD、crun) が mount や mknod をかわりにするのに使う)。aios はフィルタはあったが、USER_NOTIF は ENOSYS だった。
AI がシステムコールを 1 つずつ見て「いいよ・だめだよ」を決める口にもなる。Linux 7.4-rc1 の kernel/seccomp.c と同じに
(kernel/src/seccomp.rs の「user notification」):

- `seccomp(SET_MODE_FILTER, NEW_LISTENER)` はフィルタをかけて listener の fd (close-on-exec、`anon_inode:seccomp notify`) を
  返す。上のフィルタにもう listener があれば EBUSY、TSYNC といっしょなら TSYNC_ESRCH がいる (なければ EINVAL)、待ちかたの印
  (WAIT_KILLABLE_RECV、RESTART_BEFORE_RECV) は NEW_LISTENER がなければ EINVAL。GET_ACTION_AVAIL は USER_NOTIF も 0、
  GET_NOTIF_SIZES は 80 / 24 / 64
- USER_NOTIF になったシステムコールは、知らせ (id、スレッド、seccomp_data) を並べて眠る。listener の持ち主が
  `SECCOMP_IOCTL_NOTIF_RECV` で受け (入れ物は 0 でなければ EINVAL。使う人がいなければ ENOENT)、`NOTIF_SEND` で答える
  (値か、エラー (そのまま返す)、CONTINUE ならそのシステムコールをする。答えは 1 度だけ: 知らない id は ENOENT、受けていない
  ものは EINPROGRESS)。`NOTIF_ID_VALID` は受けて答えていないものなら 0。`NOTIF_ADDFD` は答えの前に相手の fd の表に fd を
  入れる (SETFD で番号を、SEND でそれを答えに)。`NOTIF_SET_FLAGS` は SYNC_WAKE_UP だけ
- poll: 受けていない知らせがあれば POLLIN、答えていないものがあれば POLLOUT、そのフィルタを使うスレッドがいなくなれば
  (回収されるまでのゾンビも使う人) POLLHUP
- listener を閉じると、待っているものはみな ENOSYS、それからの知らせも ENOSYS (listener がないのと同じ)
- 待つ眠りはシグナルで起き、知らせは消える (ERESTARTSYS: ハンドラが SA_RESTART でなければ EINTR)。RESTART_BEFORE_RECV
  (7.4) なら、受けられる前のものはハンドラのあとでいつもやりなおす (ERESTARTNOINTR: 新しい知らせになる)。
  WAIT_KILLABLE_RECV なら、受けられたあとは SIGKILL でだけ起きる (aios に SIGKILL でだけ起きる眠り proc::sleep_killable を
  足した: Linux の TASK_KILLABLE)
- fork した子も同じフィルタ (知らせの pid は子のスレッド)

`test/seccompnotify.c` (73 行) は Linux 7.4-rc1 と 1 バイトもちがわない。手本の Linux (test/linux.config) に seccomp が
入っていなかったので、CONFIG_SECCOMP と CONFIG_SECCOMP_FILTER を足した (test/sysdiff/ の手本はそれで作りなおして変わらない)。

### Landlock の ABI 12 (Linux 7.4)

aios の landlock は ABI 4 (ファイルと TCP) だった。Linux 7.4-rc1 の security/landlock/ と同じ ABI 12 に
(kernel/src/landlock.rs)。`LANDLOCK_CREATE_RULESET_VERSION` は 12、`_ERRATA` は 15 (直したもの 1 から 4: TCP は IPPROTO_TCP
だけ、scope のシグナルは同じスレッドグループを許す、つながっていないディレクトリ、whiteout (S_IFCHR の 0:0) を作るのは MAKE_REG)。

- `landlock_create_ruleset`: struct landlock_ruleset_attr は 7 つの欄 (56 バイト。handled_access_fs / net、scoped、
  quiet_access_fs / net、quiet_scoped、handled_permissions)。古い大きさ (8 バイトから) も受け、知らない続きが 0 でなければ
  E2BIG、4096 より大きければ E2BIG、attr が NULL なら EFAULT。quiet の欄は見張るものの中だけ (でなければ EINVAL)。fd の名前は
  7.4 と同じ `anon_inode:[landlock-ruleset]`、read と write は EINVAL (前は EBADF)
- `landlock_add_rule`: 印 `LANDLOCK_ADD_RULE_QUIET` (PATH_BENEATH と NET_PORT。quiet の欄がなければ EINVAL、許すものが 0 でも
  よい)。`LANDLOCK_RULE_NAMESPACE` (3。作ってよい CLONE_NEW*) と `LANDLOCK_RULE_CAPABILITY` (4。使ってよい CAP_*): permissions が
  ちょうどその 1 つで、束がそれを見張っていなければ EINVAL、allowed と quiet がどちらも 0 なら ENOMSG、知らないビットはだまって
  捨てる。NET_PORT は束が net を見張っていなければ EINVAL (前は EOPNOTSUPP)。デバイスを開いた fd (/dev/null) も PATH_BENEATH の
  場所になる
- `landlock_restrict_self` の印: LOG_SAME_EXEC_OFF、LOG_NEW_EXEC_ON、LOG_SUBDOMAINS_OFF (fd が -1 でもよい: 層は足さない)、
  TSYNC (同じプロセスのほかのスレッドにも同じ層、呼んだスレッドが no_new_privs ならそれも)、NO_NEW_PRIVS (no_new_privs が
  なくてもかけられ、うまくいけば立てる)。aios に記録 (audit) はないので、LOG_* と quiet は形だけたしかめて覚える
- 止めかた (新しいもの):
  - UDP: bind は BIND_UDP、connect と sendto / sendmsg の送り先は CONNECT_SEND_UDP (connect の AF_UNSPEC はよい)。名前の
    ないソケットは送る・つなぐときに autobind するので、ポート 0 の BIND_UDP も要る (write も sendmsg の hook を通る)。TCP の
    MSG_FASTOPEN は送り先の CONNECT_TCP。名前の長さと family のたしかめも Linux の current_check_access_socket と同じ順
  - RESOLVE_UNIX: 名前のある unix ソケットにつなぐ・送るのは、パスの決まりの RESOLVE_UNIX (ファイルの決まりにも書ける)。相手の
    ソケットを作ったプロセスがその層の中なら見ない (ソケットに作ったプロセスの層を覚える。accept したものは accept したプロセス)
  - IOCTL_DEV と TRUNCATE は開いたときに決める: open で、TRUNCATE と (デバイスなら) IOCTL_DEV の許しを開いたファイルに覚え
    (OpenFile の ll_deny)、あとの ftruncate と ioctl はそれで (渡された fd でも、層のないプロセスでも)。デバイスの ioctl のうち
    FIOCLEX、FIONCLEX、FIONBIO、FIOASYNC、FIOQSIZE、FIFREEZE、FITHAW、FIEMAP、FIGETBSZ、FICLONE なかま、GETFSUUID、
    GETFSSYSFSPATH はいつもよい (FIONREAD などデバイスに回るものはだめ)
  - scope: ABSTRACT_UNIX_SOCKET (抽象名前空間のソケットを作ったプロセスが層の外なら、つなぐ・送るのは EPERM。もうつないで
    いる相手はよい) と SIGNAL (相手が層の外なら kill などは EPERM。同じスレッドグループはよい)。「層の中」は Linux の
    hierarchy と同じく、その段の層が同じものか (restrict_self は新しい層を 1 つ作り、古い層をそのまま並べるので、ある段が同じ
    ならその下もみな同じ)
  - permission: NAMESPACE_USE は unshare と clone の CLONE_NEW* (ユーザーの namespace も)、CAPABILITY_USE は Cred::capable
    (commoncap が許したあとで止めるだけ。LSM の順は landlock、AppArmor) と、aios がユーザーの namespace の中で cap を見ない
    ところ (ほかの namespace を作る、自分の UTS の sethostname)。どちらも EPERM
  - ptrace (Linux の ABI 1 から): /proc/PID の environ や maps などを見るのは、相手が自分の層の中のときだけ
- ついでに直したもの: unshare は、しくじったら何も変えない (ユーザーの namespace だけ作って EPERM のとき、前は作ったものが
  残った)。`ioctl(FIOCLEX / FIONCLEX)` はどの fd でも close-on-exec を (前はデバイスやソケットで ENOTTY)

aibox はいままでどおり (ファイルの権利は TRUNCATE まで、TCP の connect と bind)。`test/landlockabi.c` (127 行: 入口のたしかめ 78 と、
止めかたを子のプロセスで 49。nobody で) は Linux 7.4-rc1 と 1 バイトもちがわない。手本の Linux (test/linux.config) に
landlock (CONFIG_LSM は landlock,apparmor) と、namespace の permission を見るためにユーザーと UTS の namespace を足した
(test/sysdiff/ の手本はそれで作りなおして変わらない)。

### マウントの番号と st_dev (/proc/PID/mountinfo、statx の STATX_MNT_ID)

systemd、findmnt、psutil、コンテナの道具は、マウントを /proc/self/mountinfo で知り、statx の stx_mnt_id と
STATX_ATTR_MOUNT_ROOT で「ここがマウントの根か」を見分ける。aios にはどれもなく、stat の st_dev はどのファイルシステムでも 1
だった (cp や tar は (dev, ino) で同じファイルかを見るので、ちがうファイルシステムの同じ番号のものを同じとみなしうる)。
Linux 7.4-rc1 の fs/proc_namespace.c (show_mountinfo) と fs/stat.c と同じに (kernel/src/vfs.rs):

- st_dev はファイルシステムごと: ブロックデバイスの上のもの (ext4、vfat) はその番号 (ルートの ext4 は 254:2)、ほかは
  Linux の名前のないものと同じく major 0 (minor はファイルシステムの番号から)。ソケット、パイプ、anon_inode、pidfd は
  それぞれの中の dev。statx の stx_dev_major / minor、fdinfo の epoll と inotify の sdev もそれで
- マウントに番号 (21 から。namespace を写すと新しい番号: Linux の copy_tree)。`/proc/PID/mountinfo`: 番号、親 (マウント先を
  ふくむ前のマウント。ルートの親は自分)、major:minor、根 (bind マウントは、もとのファイルシステムの中のパス)、マウント先
  (そのプロセスの chroot の根から。外のものは出さない)、`rw` か `ro` と `relatime`、`-` 種類 もと `rw` か `ro`。名前の空白、
  タブ、改行、\ は 8 進 (`\040`)。伝わり方 (shared: など) は、aios のマウントはいつも private なので出さない
- bind マウントのもとと種類は、もとのファイルシステムのもの (前は もと が "/パス"、種類が "none": /proc/mounts も)
- statx はいつも STATX_MNT_ID と stx_mnt_id (STATX_MNT_ID_UNIQUE を頼めば 2^31 + 番号)、マウントの根なら
  STATX_ATTR_MOUNT_ROOT (attributes_mask にはいつも)。fdinfo の mnt_id もマウントの番号 (開いたときのパスで)
- 大きなロックなしの stat の表 (statc) にも dev とマウントを (1 つの覚えは 64 語のまま: rdev と dev をカーネルの形の
  32 ビットずつで 1 語に、通るディレクトリの上限を 22 から 21 に)。覚えにない名前をルートの ext4 で引いたときは、ルートの
  ext4 が / にしか見えていなければルートのマウント。bind で同じファイルシステムがいくつにも見えているときは、statx は
  ふつうの道へ

`test/mountinfo.c` (75 行: 行の形、親、st_dev と major:minor、statx と fdinfo の番号、ユーザーとマウントの namespace の
中の tmpfs・bind・ro・umount・空白の名前、ほかのプロセスの /proc/PID/mountinfo) は Linux 7.4-rc1 と 1 バイトもちがわない
(`test/ldiff.sh mountinfo @home /tmp`。表の中身は Linux と aios でちがうので、番号はじかに出さず、そろい方を出す)。
まだ: nosuid、nodev、noexec などのマウントの印 (aios は見ていない)、ファイルシステムの印 (tmpfs の size= など)、
listmount / statmount。

### pidfd のことを答える口 (/proc/PID/fdinfo、PIDFD_GET_INFO、pidfd_getfd、pidfd_send_signal)

systemd の pidfd_get_pid は ioctl の PIDFD_GET_INFO で pidfd の指すプロセスを聞き、なければ /proc/self/fdinfo/N の `Pid:` を
読む。aios はどちらもなかった (/proc/PID/fdinfo そのものがなかった)。Linux 7.4-rc1 の fs/pidfs.c、fs/proc/fd.c、
kernel/signal.c、kernel/pid.c と同じに (kernel/src/pidfd.rs を足した。core):

- `/proc/PID/fdinfo/` (040555。一覧は fd/ と同じ) と `fdinfo/N` (0100444。読めるのは ptrace で READ できる人): `pos`、
  `flags` (8 進。fd の FD_CLOEXEC は O_CLOEXEC として)、`mnt_id`、`ino` (fstat の st_ino と同じ) と、ものごとの行: pidfd の
  `Pid:` と `NSpid:` (片づけられたら -1、見えなければ 0)、eventfd の `eventfd-count` (16 けたの 16 進) と `-id`、
  `-semaphore`、timerfd の `clockid`、`ticks`、`settime flags`、`it_value` (のこり)、`it_interval`、epoll の `tfd:` の行
  (EPOLLERR と EPOLLHUP はいつも入る)、inotify の `inotify wd:` の行、AF_UNIX の `scm_fds` (受ける行列の SCM_RIGHTS の数)。
  mnt_id はマウントの番号 (上の「マウントの番号と st_dev」)。inotify の file handle (`fhandle-bytes` など) はまだ
- `flags` と F_GETFL は、open で開いたもの (ファイル、デバイス) に 64 ビットの Linux がいつも足す O_LARGEFILE を見せる
  (しまうものは変えない: 棚の速い open は口をくらべるので)。O_PATH は O_PATH、O_DIRECTORY、O_NOFOLLOW だけ
- `PIDFD_GET_INFO` (`_IOWR(0xFF, 11, struct pidfd_info)`。大きさは 64、72、80、88 の版で、それより小さければ ENOTTY、
  大きければ知らない続きを 0 に): いつも番号 (pid、tgid、ppid) と資格情報、頼まれれば COREDUMP (PR_SET_DUMPABLE からの印:
  1 なら USER、0 なら SKIP) と SUPPORTED_MASK (0xff)。入らない欄の印は落とす。片づけられたあとは EXIT だけ (頼まなければ
  ESRCH): exit_code は wait の status で、片づけたとき (proc の free_slot) に覚える。Linux と同じく、それまでに pidfd を
  開いていたものだけ (pidfd がみな閉じたら忘れる)。ゾンビのあいだはまだ EXIT はない。cgroup がないので CGROUPID は
  入らない (手本の Linux も同じ)。namespace の fd を返す PIDFD_GET_*_NAMESPACE はまだ (ENOTTY)
- pidfd の st_ino はプロセス (スレッド) ごと (aios の pid は使いまわさないので番号そのもの。同じものを指す pidfd は同じ ino)
- `pidfd_getfd` (438): 相手の fd を写して close-on-exec で受けとる。ptrace で ATTACH できる相手だけ (実の uid と gid が
  相手の 3 つの id と同じで dumpable、でなければ CAP_SYS_PTRACE。landlock と AppArmor の ptrace の決まりも)。ゾンビは ESRCH
- `pidfd_send_signal` の flags (THREAD、THREAD_GROUP、PROCESS_GROUP のどれか 1 つ。PROCESS_GROUP は pidfd の指すプロセスの
  番号のグループへ) と siginfo (si_signo がちがえば EINVAL、ほかのプロセスへはカーネルの si_code (0 以上と SI_TKILL) は
  EPERM)。info がなければ THREAD は SI_TKILL、ほかは SI_USER。pidfd のかわりの PIDFD_SELF_THREAD (-10000) と
  PIDFD_SELF_THREAD_GROUP (-10001)。pidfd でない fd は EBADF (前は EINVAL)
- ついでに足したもの: `prctl(PR_GET_DUMPABLE / PR_SET_DUMPABLE)` (持ちもの (mm) ごと、fork で引き継ぎ。いつ変わるかは
  下の「dumpable と /proc/PID の持ち主」)。epoll に poll のできないもの (ディレクトリ、ext4 や tmpfs のふつうのファイル、/dev/null と
  /dev/zero、ブロックデバイス) を入れるのは EPERM (前の aios は入れて、いつも読める・書けると答えていた)

`test/pidfdinfo.c` (176 行: fdinfo を 13 種類の fd で、PIDFD_GET_INFO を自分・子 (動いている、ゾンビ、片づけたあと)・
スレッドと 4 つの大きさで、pidfd_getfd、pidfd_send_signal の flags と siginfo) は Linux 7.4-rc1 と 1 バイトもちがわない。
番号 (pid、ino、mnt_id) と uid はじかに出さず、あてはまるかを出す。

### dumpable と /proc/PID の持ち主

root から uid を変えたプロセスの中を、もとの人が見たり書いたりできないようにするしくみ (Linux の commit_creds、
begin_new_exec、task_dump_owner、__ptrace_may_access)。dumpable は持ちもの (mm) ごと (CLONE_VM の相手と分けあう)。

- 変わるとき: euid か egid が変わると (setuid、setresuid、setreuid と gid のもの。capability も aios では euid で決まる)
  `fs.suid_dumpable` (既定 0) に。setgroups や、同じ値への set*id、ruid と suid だけの変更では変わらない。exec では 1 に
  もどるが、exec の前の euid と uid (egid と gid) がちがう、プログラムかインタプリタ (ld.so) が読めない (Linux の
  would_dump)、setuid / setgid で euid / egid が変わったときは `fs.suid_dumpable`。`PR_SET_DUMPABLE` は 0 と 1 だけ
- /proc/PID の持ち主: そのプロセスの euid と egid。dumpable が 1 でなければ root (dr-xr-xr-x のディレクトリ: /proc/PID、
  task、fdinfo はそのまま)。なので uid を変えたあとは、自分の loginuid や uid_map を書けず、environ (0400) を読めない
  (`prctl(PR_SET_DUMPABLE, 1)` でもどる)。/proc/PID/fd は root のものになっても自分のプロセスなら入れる
  (proc_fd_permission)
- ほかのプロセスの environ、maps、cwd、root、exe、fd、fdinfo、ns の口と pidfd_getfd: 自分の euid と egid が相手の 3 つの
  uid と gid とみな同じで、相手が dumpable なときだけ (でなければ CAP_SYS_PTRACE)。それから landlock と AppArmor の ptrace。
  maps、environ、fdinfo/N は開くときに見る (Linux と同じ。前の aios は読むときに見て、maps は uid を見ていなかった)
- ついでに Linux とそろえたもの: environ と stack は 0400、comm は 0644 で書ける (同じスレッドグループから。はじめの
  15 バイト)、fd/N のリンクの mode は開いた口しだい (読めれば r-x、書ければ -wx)

`test/dumpable.c` (root で: 自分の資格を変える、変わらない・もどすだけのとき、ほかのプロセスから、exec のあと (読めない
プログラム、setuid のもの)、`fs.suid_dumpable` 2) は Linux 7.4-rc1 と同じ (82 行)。fsuid はまだない (setfsuid は euid を
返すだけ) ので、setfsuid で dumpable が変わるところはまだ。

### 出すだけのテストの答えあわせ (test/ldiff.sh)

`test/ldiff.sh [--root] [--linux VER] [--fresh] NAME [ARG...]` は、出すだけのテスト (test/NAME.c) を作って (test/bin/NAME)、
手本の Linux (test/linux-vm.sh exec。既定は mainline) と、いま動いている aios の VM (test/vm.py start --www test。
`--kernel .` なら作業中のソースのカーネルを作って起こしなおしてから) で同時に
動かし、出力をくらべる。同じなら `same: N lines`、ちがえば `diff -u` (- が Linux、+ が aios) と両方の終わりの番号と aios の
stderr の終わり。Linux の答えは build/linux-answers/ にとっておき、プログラムと引数と Linux の版と test/linux.config が
同じなら動かしなおさない (aios だけ直したときは数秒で終わる)。引数の `@home` は ext4 のところ (Linux は /work、aios は
ホーム)。`--root` は Linux の rootexec と aios の sudo -n。前は、Linux の出力から `.config:` の警告と終わりの番号を手で
のぞき、aios の出力を vm.py get で取り出して diff していた。test/linux-vm.sh は、Linux を作るとき (15 分ほど) に、作り
はじめと終わりを stderr で知らせる (前は終わるまで何も出さず、止まっているのか見わけられなかった)。

`test/vm.py start --kernel .|FILE|COMMIT [--snapshot] [--off NAME,...] [--www DIR]` は、aish の `vm start kernel=` と同じ
作りかたと置き場 (build/aish-vm/kernels。コミットは build/aish-vm/wt で作る) でカーネルを作り、動いている VM を止めて
起こしなおす。aish の MCP がつながらないときも、これだけで作業中のカーネルをためせる (前は AIOS_INITRD=none と
CARGO_TARGET_DIR と AIOS_KERNEL と AIOS_QEMU_ARGS=-snapshot を手でそろえていた)。`vm.py job` は、job のプロセスグループに
ゾンビでないものがいるあいだは終わったと言わず、VM がいそがしくて読めなければ busy で答える。

くらべたあと、手本の Linux に足りない機能 (aios にはある) を 1 行で知らせる (`note: the reference Linux lacks what aios
has: ...`)。test/features.c (システムコールが ENOSYS か、namespace を作れるか、ソケットの family、LSM、/proc と /sys の
もの: 76 こを yes / no で) を両方で動かしてくらべる。手本の Linux は tinyconfig に test/linux.config を足したものなので、
テストが使う機能がないと、ENOSYS や EINVAL がテストの答えにかくれて同じに見える (CONFIG_USER_NS がなかったとき、
test/landlockabi.c の namespace の行が、どちらも「止められていない」に見えた)。そのときは test/linux.config に足す。
`test/ldiff.sh features` は、両方の機能の一覧そのものをくらべる (aios にないもの: IPv6、cgroup と time の namespace、
/proc/self/mountinfo、NETLINK_AUDIT など)。

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
`base: mainline` (kernel.org の mainline。いまは 7.4-rc1: rc の tarball は git.kernel.org から、kernel.org は rc の sha256 を
出さないので HTTPS で取るだけ) や `base: 7.4-rc1` で、安定版になる前の新しい Linux とくらべることも、
`base: host` でこのマシンのカーネルにすることもできる。ほかの Linux (たとえば aios(main)) の答えでくらべるなら、そこで
`cc -O2 -o sysdiff test/sysdiff.c && ./sysdiff -n 30 DIR 1-300 > linux.txt` とし、`linux` にそのファイルをわたす。

ホームの ext4 (`fs` の既定) と /tmp の tmpfs (`fs: tmp`) の両方で、30 op と、長い並び (`ops: 80`) の種を見る。いまの aios は
Linux 7.2.11 (最新の安定版) と、ext4 で 500 の種 (30 op) と 3000 の種 (80 op)、tmpfs で 1000 の種 (80 op) のどれもちがわない。
CI の答え (test/sysdiff/、`test/sysdiff-ci.sh golden mainline`) は 7.4-rc1 のもの (下の「Linux 7.4 に合わせる」)。
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
inotify の見張りがいまある (数で見る: 2026-11-03 までは「置かれたことがある」で、見張りを外しても inotify を閉じても切れた
ままだった)、砂場 (landlock)、読むだけのマウントがある、htree のディレクトリ、.. のあるパスや
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

munmap と、アドレス空間を捨てるとき (exit、exec) は、外すページと写しの表の鍵を、はじめにみな Vec に集めていた。
3.4 GB のモデル (ai.gguf) を写した llama-server を止めると、鍵が何十万にもなり、その Vec が 12 MB をヒープにほしがる
(2 MiB の予備にも入らない)。2 GB の aios ではモデルでメモリがいっぱいで取れず、`sudo poweroff` でカーネルが止まった
(memory allocation of 12582912 bytes failed、sys_munmap → shared_keys)。いまは Linux の mmu_gather と同じく少しずつ返す:
PTE を 0 にしたページを 128 枚ためたら TLB を消して返し (Gather)、写しの表は領域ごとの鍵の範囲 (shared_ranges) から
64 ずつ取り出して返す (release_shared_ranges)。どちらもヒープを使わないので、メモリが足りないときも外せる。
同じ形だった MADV_DONTNEED (discard) と、縮めたファイルのページをみなのアドレス空間から外すところ (drop_file_range) も
Gather で。
test/bigunmap.c は 64 MiB の MAP_SHARED に書いて、munmap、半分ずつの munmap、写したままの exit のあとに、みなファイルに
書き戻されたかを見る (まとめる数をなんども越える)。

写しているファイルのページは、写しているあいだ捨てなかった (MAP_SHARED は写しの表が、MAP_PRIVATE で読むだけのものは
ページキャッシュが参照を持ち、swap_out は自分だけの無名のページしか追い出さない)。そのため RAM より大きいファイルを
写して読むと (llama-server が 3.4 GB のモデルを読みこむ)、RAM が写しのページでいっぱいになり、ページが取れなくなった
フォルトはそのスレッドを SIGKILL にしていたので、たまたまスタックをのばした init が殺されてカーネルが止まった
(pid 1: out of memory → init exited with status 9。1G の VM で 1.5 GB を写して読むだけで起きた)。いまは Linux と同じく:
- スワップの前に、写しているファイルのページでしばらく触れていないものを外して捨てる (PageTable::reclaim_file。
  swap_out と同じ clock で、AF の立っているものは落とすだけ、落ちたままのものを外す。外すのは、写しの表にあって
  書いていないページと、書きこみ禁止のまま写したページキャッシュのページだけ。1 回に見る PTE は数をかぎる)。
  外したページは、ほかに写しているものがなければ写しの表 (release_shared) とページキャッシュ (shrink_cache) から捨て、
  ext4 のブロックのキャッシュもだれも持っていない古いものを捨てる (ExtFs::shrink。ふだんは上限をこえたときだけ
  1 つずつ捨てるので、手放したページがそこに残っていた)。次に触れればフォルトで読みなおす。
- それでもページが取れないフォルトは、いちばんメモリを持っているプロセス (init のほか) を SIGKILL で終わらせて
  空け、困ったスレッドは少し待って同じ命令からやりなおす (oom::on_fault。Linux の pagefault_out_of_memory)。
  init は殺さない。空きが足りているのに取れなかったとき、わざと取れなくした (`fail ... page`) なら、いままでどおり
  困ったものだけを殺し、ほかが返したあとなら、だれも殺さずにやりなおす。
- 終わったプロセスのアドレス空間は、終わったときに捨てる (Linux の exit_mm)。前は親が wait するまでゾンビが
  持っていたので、殺したものが終わっても親が片付けるまでメモリが戻らず、そのすきに 2 つめ (親) まで殺していた。
  ゾンビの /proc/PID/stat と status の大きさは 0 (Linux と同じ)。
test/bigmap.c は RAM の 1.5 倍のファイルを MAP_SHARED と MAP_PRIVATE で写して、ページごとの印を 2 回ずつ確かめる。
test/oomfault.c は RAM とスワップより大きい無名のメモリに書く子が SIGKILL で終わり、そのあいだ書きつづける親が
生きのこるのを見る (どちらも時間がかかるので ctests には入れない)。

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

次に Firefox (headless でページを 1 つ) をはかりなおすと、/proc/bkl のいちばん上が write tty (1365 回、773 ms) だった。
Firefox は tty に書いていない: aish の vm の run を bg で動かすと、外の道具がようすを見るために cat・wc・tail・head を
シリアルの端末で何度も動かしていて、その出力だった (1 回 566 µs: QEMU の PL011 に 1 文字ずつ)。前で動かすと
14.7 → 8.2 秒、大きなロックを待った割合も 61.5 → 13.3% になる (bg ではかった数は、はかる道具の分ふくらんでいる)。
そのとき、trace の名前の絞り (`trace on NAME`) が、ロックなしのシステムコールの中から代表のスレッドを探して、プロセスの
表を大きなロックなしでなめていた (`bkl: proc.rs:646 touched without the big lock`。スレッドの多い Firefox で出た)。
絞りに合うかをスレッドごとに覚え (Proc::trace_mark)、絞りを変えたときと名前が変わったとき (exec、PR_SET_NAME) に版を
進める。表をなめるのは大きなロックの中だけで、ロックなしでまだ覚えていないスレッドは自分の名前でくらべる (作られた
ときに名前を受けつぐ)。

前で動かした Firefox で大きなロックを持っていたものには、mremap が 26637 回あった (1 回 4.3 µs)。数えてみると、ほとんどみな
`mremap(addr, 4096, 8192, 0)` (1 ページの領域を、動かさずに 2 ページにできるか) で、答えはみな ENOMEM だった。伸ばせないことは
領域の表を読むだけで分かるので、大きなロックなしで答える (vm.rs の fast_mremap_fail。fast_fault と同じく、表を変える人が
いないときだけ読む: fast_users を増やしてから mutators が 0 か見る)。古い場所に領域がなければ EFAULT、領域の終わりでないか
その先に領域があれば ENOMEM。伸ばせるもの、縮める・動かすもの、知らない形はふつうの道へ。ついでに sys_mremap の順を
Linux 6.18 に合わせた: 大きさ 0 の古い場所は共有でなければ EINVAL (前は EFAULT)、同じ大きさと縮めるのは古い場所が領域を
こえていても答える (縮めるなら、こえたところも外す。前は EFAULT)。test/mremap.c (20、Linux でも aios でも同じ。見張りの
PROT_NONE のページで、Linux がとなりの領域をまとめても答えが変わらないように。3 つのスレッドがたずねつづけるあいだに、
となりを mmap / munmap する) を ctests に。Firefox の /proc/bkl から mremap がなくなり、大きなロックを待った割合は
13.3 → 10〜11% (時間は 8.6〜8.8 秒で、TCG のゆれの中)。

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
はハンドラが動けば EINTR、LOCK_PI はいつもやり直す (Linux の ERESTARTSYS、ERESTART_RESTARTBLOCK、ERESTARTNOINTR)。起こす
側は、行列から出したスレッドだけを起こす (いまはスレッドごとの場所で眠る: 上の「futex の待ちの行列」)。ロックなしの速い道 (PRIVATE の WAKE で待つもの
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
| select-ebadf | select と pselect6 で、nfds までに開いていない fd のビットがあれば、どの番号でも EBADF | fd の表の大きさ (はじめは 64、開いた数で増える) より先のビットは見ずにすてる (fd 40 は EBADF、fd 1000 は 0) | POSIX は、開いていない fd があれば EBADF と決めている。答えが fd の表の大きさ (いままでに開いた数) で変わらない。select(2) の BUGS にある。ロボットは select を使わないので test/chosen.c が確かめる |
| lock-deadlock | fcntl のレコードロック (POSIX どうし) の行きどまりを、輪の長さによらず EDEADLK にする (待っているものの数だけたどる) | 11 たどるとやめる (MAX_DEADLK_ITERATIONS)。13 より多い持ち主の輪は見のがし、みないつまでも待つ | 行きどまりを見つけるのに、長さの決まりはいらない (待っているものは 1 つずつしか待たないので、数だけたどれば止まる)。fcntl(2) の BUGS にある。aios は前は 10 までで、Linux より 1 つ短かった。test/chosen.c が 13 のプロセスの輪で確かめる |
| recvmmsg-timeout | recvmmsg の timeout は呼び出しの締め切り: 受けとりで眠るのはそこまで (ソケットの SO_RCVTIMEO と早いほう)。時間切れで、受けたものがあればその数、なければ EAGAIN。待つのは Linux と同じく 1 つずつ (MSG_WAITFORONE なら 2 つ目から待たない)、残りの時間を timeout に書きもどすのも同じ | datagram を受けたあとにしか timeout を見ない。vlen より少なければシグナルが来るまで待つ (timeout 0.3 秒で 1 つも来なくても) | timeout を渡すのは、そこまでで返ってほしいから。recvmmsg(2) の BUGS にある。前の aios は timeout を見ず、2 つ目からいつも待たなかった (Linux とちがう)。test/chosen.c が 3 つのうち 2 つで確かめる |
| send-notconn | 一度もつないでいない TCP (listen しているもの、connect にしくじったもの、AF_UNSPEC で切ったものも) へ send / write すると ENOTCONN (SIGPIPE はなし)。つないだあとで閉じられたものは Linux と同じ EPIPE と SIGPIPE | どちらも EPIPE と SIGPIPE (sk_stream_wait_connect) | POSIX は、つながっていないソケットは ENOTCONN、もうつながっていないものは EPIPE と分けている。Linux でも AF_UNIX の stream は ENOTCONN で、TCP だけがちがう (send(2) の BUGS)。FreeBSD も ENOTCONN。つないでいないソケットに書くまちがいで、プロセスが SIGPIPE で落ちない。ロボットは Linux で、EPIPE のとき TCP_INFO の送った segment の数 (tcpi_segs_out。dport と同じく、connect にしくじるか AF_UNSPEC で切ると 0 にもどる) が 0 なら ENOTCONN にしてまねる。test/chosen.c も確かめる |
| apparmor-inet | AppArmor の network_v9 の af_inet を名のり、`network inet stream ip=127.0.0.1 port=8080,` や `peer=(ip=... port=...)` の番地の決まりを効かせる (読みかたは apparmor_parser の決まりの形と Ubuntu のカーネルの af_inet.c と同じ。特権の port、deny の決まり、自分の番地の条件のない peer の決まりの 3 つは、Ubuntu の読みかたのまちがいをなおした: 上の「AF_INET」) | af_inet を名のらないので、apparmor_parser が ip= と port= を落として (inet, type) の決まりにする (allow は広く許し、deny は広く止める)。af_inet.c は番地を記録に書くだけ | 書いた決まりのとおりに効く (書いたよりも多く許さない、多く止めない)。パーサも Ubuntu のカーネルもこの決まりを持っていて、Linux の本体にまだ入っていないだけ。ロボットは AppArmor を見ないので、test/apparmor.c が両方でたしかめる (aios は番地の答え、Linux は広がった決まりの答えと、features に af_inet がないこと) |
| audit-field-value | audit のルールの値の決まりを見て EINVAL: LOGINUID_SET は 0 か 1、PERM は 15 まで、FILETYPE は S_IFMT のビットだけ、FIELD_COMPARE は AUDIT_MAX_FIELD_COMPARE まで、SADDR_FAM は AF_MAX より下 | 値をたしかめるところ (audit_field_valid) が、値を入れる前の f->val (0) を見るので、どれも通る。FIELD_COMPARE の知らない番号は、そのルールを合わせるたびに WARN (カーネルのスタックを kmsg に) | Linux のコードが書いてあるとおりのふるまい (値をたしかめるつもりのコードが効いていない)。root がまちがって足したルールが黙って何にも合わない (FILETYPE、PERM) か、カーネルの WARN を出しつづける (FIELD_COMPARE) のを、足すときに知らせる。test/auditrules.c が Linux ではこれをまねてくらべる |

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

#### Linux よりよくできそうなところ (候補)

よい道のさがしかた: Linux 自身が「書いてあるとおりに動かない」とまとめているところを読む。man-pages (6.20) の
BUGS の節 (man2 の 74、man7 の 25 のページ。多くは古い版のことで、もう直っている)、カーネルのソースの TODO と FIXME、
AppArmor のように Linux の本体の外 (Ubuntu のカーネル、apparmor-next) にあるもの。まだ残っているものは、小さな
プログラムで Linux 7.3.1 (test/linux-vm.sh exec) と aios の両方で動かしてたしかめる。上の表に入れたもの
(select-ebadf、lock-deadlock、recvmmsg-timeout、send-notconn、apparmor-inet、pwrite-append) のほかに、2026-11-01 にたしかめたもの:

| もの | Linux 7.3.1 | aios (いま) | どうするか |
|---|---|---|---|
| recvmmsg で、受けたあとのエラー | 次の呼び出しで返すが、そのあいだの ICMP などで上書きされる (recvmmsg(2) の BUGS) | 捨てる (そこまでの数を返す) | ソケットにとっておいて、次に必ず返す |
| mmap でファイルの終わりの先 (同じページ) に書いたもの | ページキャッシュに残り、あとの写しに見える (書き出しまで。mmap(2) の BUGS) | 書き出すとき (msync、munmap、終わり、sync) に 0 にするので、munmap のあとの写しには見えない。ほかのプロセスが写しているあいだは見える | 新しく写すときにも終わりの先を 0 に (POSIX) |
| timerfd の時計 | CLOCK_PROCESS_CPUTIME_ID と CLOCK_THREAD_CPUTIME_ID は EINVAL (timer_create はできる。timerfd_create(2) の BUGS) | Linux と同じ | 使えるように |
| execveat(fd, "", AT_EMPTY_PATH) の #! スクリプト | fd が close-on-exec なら ENOENT なので、fd がスクリプトにもれる (execveat(2) の BUGS) | まだ見ていない | |
| fcntl のロックと CLONE_FILES | ファイルの表を分けあうプロセスどうしで、ない行きどまりを EDEADLK にすることがある (fcntl_locking(2) の BUGS) | まだ見ていない | |
| RLIMIT_CPU | SIGXCPU のハンドラがあると、やわらかい上限を 1 秒ずつ上げる (getrlimit(2) の BUGS) | まだ見ていない | |
| AppArmor の AF_UNSPEC の connect | 7.3 は見ない。apparmor-next は connect の許しを見る (bind の AF_UNSPEC の長さも確かめる) | 7.3 と同じ | Linux の次の版に入ったら合わせる |
| AppArmor の af_inet のラベル | (af_inet がない) | label= と peer=(label=) は区切りだけたどる (Ubuntu も) | ラベルの名前まで見る |

そのままにするもの (Linux のふるまいに頼るプログラムがある、ABI): select が EINTR でも timeout を書きかえる、
sched_setscheduler が前の policy でなく 0 を返す、setfsuid / setfsgid がしくじりを知らせない、nice がスレッドごと、
waitid の infop が NULL でもよい、EPOLLWAKEUP を黙って無視する。

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

## ローカルの AI (llama.cpp)

aios の中で LLM を動かす。推論エンジンは llama.cpp (pkg/c/llama.cpp。CPU だけ、静的)、モデルは GGUF、常駐するのは
`llama-server.service`。どれも aios.img には入れない (大きいので)。aios のモデルは git.syui.ai の ai/os のリリース
`model` (`ai.gguf`。ゼロから作る `ai-origin.gguf` も) にあり、`aimodel` パッケージが取ってくる:

```sh
sudo ap -S aimodel curl                       # llama.cpp も入り、モデルを取ってくる (llama-server は動かさない)
llama cli                                     # 端末で話す (/exit か Ctrl-C で終わる。/clear で最初から。モデルはここで読みこむ)
sudo systemctl enable --now llama-server      # API もほしいとき、いつも動かすなら (llama cli はそれにつなぐ)
curl -s http://127.0.0.1:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"こんにちは"}]}'
aimodel status                                # 置いてあるものと配っているもの (sha256)
sudo aimodel pull                             # 配っているものが変わっていれば取りなおす (動いている llama-server は起こしなおす)
sudo aimodel pull origin                      # ゼロから作った ai-origin.gguf を models/ に置く (llama-server は ai.gguf のまま)
llama completion -m /var/lib/ai/models/ai-origin.gguf -p "Once upon a time" -n 150 -no-cnv   # 書き出しのつづき (英語のお話)
きょうは何する                                 # aish で: 見つからないコマンドの行は、claude がなければ llama-server のアイへ (aish-local)
```

ほかのモデルを手で置くとき (aimodel は入れない。入れると /var/lib/ai/model.gguf はそちらのものになる):

```sh
sudo ap -S llama.cpp curl                     # /opt/c/bin/llama (と llama-server、llama-cli)。curl は API に話すのに
sudo cp MODEL.gguf /var/lib/ai/models/        # 置き場所 (パッケージが作る)
sudo ln -sf models/MODEL.gguf /var/lib/ai/model.gguf
sudo systemctl enable --now llama-server      # いま起こし、次の起動からもひとりでに動く
fetch http://127.0.0.1:8080/health            # {"status":"ok"} (読むだけなら fetch でも)
curl -s http://127.0.0.1:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"こんにちは"}]}'
llama cli                                     # 端末で話す (llama help で、ほかのもの)
llama bench                                   # この機械での速さ (prompt と生成の tokens/s)
```

- `llama` は upstream の 1 本にまとめたもの (serve、cli、completion、bench、quantize、perplexity …)。CPU の命令ごとに
  3 本あり (armv8.0: cortex-a72 や QEMU、armv8.2 + dotprod + fp16: cortex-a76 や Apple M1、armv8.6 + i8mm: Apple M2 から)、
  `/opt/c/bin/llama` が /proc/cpuinfo の Features (上の「CPU の印」) を見て選ぶ。`LLAMA_CPU=armv8.0` で手で選べる。
  ggml の CPU の速さは dotprod と i8mm でほぼ決まる (Q4_0 などは、読みこむときにその CPU の形に並べかえる)。
  `-m` (や `-hf`、`--models-dir`) も `LLAMA_ARG_MODEL` もなければ、`/var/lib/ai/model.gguf` を使う (`llama cli` だけで話せる)。
  `llama cli` は答えを端末にじかに書くので、ファイルやパイプにつなぐときは `--simple-io` をつける
  (`llama cli -p "こんにちは" -st --simple-io > out.txt`。`-st` は 1 回だけ答えて終わる)。
  モデルを言わない `llama cli` は、llama-server.service が動いていればそれにつなぐ (`--server-base http://127.0.0.1:8080`:
  モデルをもう 1 つ読みこまない)。サーバーがまだ読みこんでいるところ (起動したばかりなど) なら、読みおわるまで待つ
  (Ctrl-C でやめる。待っているあいだにサーバーが止まったら、そう言って終わる)。
- `llama-server.service` は base の /etc にあり、`/opt/c/bin/llama` と `/var/lib/ai/model.gguf` の両方が
  あるときだけ起きる (ConditionPathExists)。はじめは有効にしない (aimodel も有効にしない): モデルは起動のたびに
  読みこむので (ai.gguf は 3.4 GB)、メモリの少ない aios (Mac は 2-3 GB) ではほかが動けなくなる (起動したばかりの
  aios で読みこみ、メモリが足りずにカーネルが止まったこともあった)。話すときに `sudo systemctl start llama-server`、
  いつも動かすなら `sudo systemctl enable --now llama-server` (動いているかは `systemctl is-active llama-server`)。
  動いていなければ `llama cli` が自分でモデルを読みこむ。有効にするリンクは base に入れない (入れていたときは、
  disable しても base を上げると戻った)。nobody で、127.0.0.1:8080 に OpenAI と同じ API (`/v1/chat/completions`、
  `/v1/completions`、`/v1/embeddings`、`/v1/models`) を出す。外から使うときは ssh の転送で (`ssh -L 8080:127.0.0.1:8080`)。
- aish から: 見つからなかったコマンドの行 (打ちまちがいや、言葉で書いたこと) は、claude があれば Claude に (aish-claude)、
  なければ llama-server に (aish-local) 渡し、答えを届いたところから出す (Ctrl-C で止まる)。`AISH_AI=local` なら claude が
  あってもこちら。答えるだけで、コマンドは動かさない (手を動かすのは Claude と `aish --mcp`)。15 分以内の 4 つまでの
  やりとりを `~/.cache/aish/local.json` (0600) に残して続けて話す (モデルの読める長さに入りきらなければ、やりとりなしで
  聞きなおす)。llama-server が止まっていれば、ふつうの command not found (モデルを置いてあれば、起こし方を 1 度だけ出す)。
  場所は `AISH_LOCAL_URL` (既定 `http://127.0.0.1:8080`。OpenAI と同じ API ならほかのものでも)、モデルへの前置きは
  `AISH_LOCAL_SYSTEM` (空なら前置きなし。ai-origin のように 256 トークンしか読めないモデルで)、`AISH_LOCAL=0` で止める。
- 設定は `/etc/systemd/system/llama-server.service.d/local.conf` に `[Service]` と `Environment=LLAMA_ARG_CTX_SIZE=8192
  LLAMA_ARG_THREADS=4` など (llama serve --help に出る LLAMA_ARG_*)。モデルを替えたら `systemctl restart llama-server`。
  会話の長さはふつう 4096 (`LLAMA_ARG_CTX_SIZE`。言わなければ llama.cpp はモデルの学んだ長さ、Gemma 4 なら 131072 の
  KV cache を取ろうとして、`failed to fit params` になる)。
  答える前の考えごと (thinking) はしない (`LLAMA_ARG_REASONING=off`。Gemma 4 は 100-200 トークン考えてから答えるので、
  CPU ではそのぶん待つ。考えさせたいときは local.conf で `LLAMA_ARG_REASONING=auto`、`llama cli` なら `--reasoning on`)。
- モデルは mmap で読む (ページキャッシュに入り、メモリが足りなければ捨てて読みなおす)。RAM はモデルの大きさと
  コンテキストの KV cache の分がいる。作るときに Web UI はネットから取らないので、ブラウザの画面はなく API だけ。
  ai.gguf (Gemma 4 E2B、3.4 GB) は、llama-server が動いて 1.8-2.7 GB ほど使う。Mac の aios はふつう 2G なので、
  `AIOS_MEM=3G ./bin/run.sh` で (hvf は highmem=off なので 3G まで)。2G ではモデルを読みなおしつづけて重い。
- モデルを替えるときは、リリース `model` に置きなおす: `bin/gitea.sh model ai.gguf` (`ai.gguf.sha256` もいっしょに置く。
  同じ名前のものは置きかえる)。大きいものは Cloudflare をとおすと 502 になる (31 MB はとおり、279 MB はだめだった) ので、
  サーバーの上で `GITEA_URL=http://127.0.0.1:3000 bin/gitea.sh model ai.gguf` のように Gitea へじかに。git (ai/repo) には
  入れない (publish が ai/repo の写しを持つので、重くなる)。aios では `sudo aimodel pull`。aimodel は `ai.gguf.sha256` で
  変わったかを見て、取ったものも確かめてから入れかえる (読めなければ何も変えない)。パッケージ (aimodel 0.0.1) は
  スクリプトと設定だけで、モデルは入っていない。
- `aimodel pull|status|rm` は NAME を取る: なければ `ai` (`ai.gguf`。`/var/lib/ai/model.gguf` をそれに向けて llama-server を
  起こしなおす)、`origin` は `ai-origin.gguf` (置くだけ。ゼロから作った 15.7M のモデルで、会話ではなく書き出しのつづきを書く。
  一度に見るのは 256 トークンまで)。ほかの名前は `ai-NAME.gguf`。`sudo aimodel rm origin` は ai-origin だけを消す。
- 取ってくるところは `/etc/aimodel.conf` (`URL` と `SUM`。`ai` でないものは、同じところの `ai-NAME.gguf` と
  `ai-NAME.gguf.sha256`)。`SUM` を空にして `API` に Gitea の contents API を書くと、git に置いたもの (blob の sha1 と
  大きさで確かめる) からも取れる。どちらも空なら、置いてあれば取らず、`aimodel pull -f` でいつも取る。
- 入れたときは post_install が `aimodel pull` (取れなくても入れるのはしくじらない。llama-server は動かさない)、
  上げたときは `aimodel pull` (新しいモデルがあれば取り、動いている llama-server だけ起こしなおす。有効かどうかは
  変えない)、消すときは pre_remove が `aimodel rm` (llama-server を止めて、取ってきたモデル
  (ai-origin も) とリンクを消す。手で向けかえたリンクは残す) と `systemctl disable llama-server`。
  aimodel は llama.cpp と base に頼る (depends) ので、`ap -U` で入れても llama.cpp が先に入り、`ap -Syu` では base が先に上がる。

## 改造 (aios src / build / install)

```sh
aios src                       # /usr/src/aios に aios のソース (なければ git clone、あれば git pull)。wheel の人が書ける
                               # (git が要る: sudo ap -S git。aiosd が /etc/gitconfig の safe.directory に足す)
aios build kernel              # AIOS_INITRD=none でビルドし、target/Image (起動できる形) を作る (objcopy の代わりも aios がする)
aios install kernel            # 新しいカーネルを試す (aiosd)。起動できたら /boot/Image になり、前のものは /boot/Image.prev
aios install kernel --revert   # 試すのをやめる (試していなければ /boot/Image と /boot/Image.prev を入れかえる)
aios build pkg NAME|DIR [-o DIR]   # PKGBUILD からパッケージを作る (NAME は /usr/src/aios/pkg/*/NAME)
aios install pkg FILE...       # 作ったパッケージを入れる (aiosd が aipkg -U で)
```

- 前のカーネルは起動の一覧に「aios (previous kernel)」(`/boot/loader/entries/prev-aios.conf`) として出る。
  一覧が出るように、loader.conf の `timeout 0` は 3 秒にする
- 新しいカーネルは、起動できるとわかるまで `/boot/Image` にしない。試すのは systemd-boot と同じ boot counting
  (Automatic Boot Assessment) なので、ローダーは aiboot でも systemd-boot でもよい:
  1. `aios install kernel` で aiosd が新しいものを `/boot/Image.new` に置き、試す entry
     `/boot/loader/entries/aios-new+1.conf` を作る (名前の `+1` が残りの回数。`sort-key aios` があるので一覧の先頭で、
     loader.conf の `default aios*` に当たる。options は aios.conf と同じに `aios.new=1` を足す)
  2. ローダーは数える名前の entry を起動する前に、名前を `aios-new+0-1.conf` に変える (残り 0、済んだ 1)。
     残りが 0 の entry は一覧のうしろに並ぶので、次の起動では default が `aios.conf` (いまの /boot/Image) に当たる
  3. aiosd が動くと (systemd-bless-boot の役): `aios.new=1` で起きていれば、/boot/Image を /boot/Image.prev に、
     /boot/Image.new を /boot/Image にして、試す entry を消す (起動できた)。ほかのもので起きていれば、試すのをやめて
     (entry と /boot/Image.new を消す) わけを残す: `unfinished` (ローダーが試したのに aiosd まで来なかった。止まって
     リセットした)、`load` (UEFI の LoadImage が読めなかった。aiboot がその場で次の entry へ行き、
     `aios.fallback_why=load` を渡す。systemd-boot は起動しなおすので `unfinished` になる)、`chosen` (一覧で
     ほかのものを選んだ。entry の名前が数えられていない)。どれも `/var/log/aiosd.log` に `{"op":"boot",...}` で残り、
     `aios get boot.last` の `result` と `why` で読める
  新しいカーネルが止まったら、電源を入れなおす (リセット) だけで前のものに戻る。/boot/Image は試すあいだも変わらない
- `test/vm-boot.sh [--new] [--sdboot]` (開発のマシン) は、これを aios.img の VM (-snapshot) で確かめる: 起動するもの、
  読めないもの、止まるもの (`AIOS_TEST_HANG=1` でビルドしたカーネル) を入れて再起動し (`test/vm.py reboot
  --reset-after`)、/boot/Image がもとのままか、わけ (why) を見る。`--new` はこのソースの aiboot、aiosd、aios を
  入れてから。`--sdboot` はローダーを本物の systemd-boot (Debian 13 の systemd-boot-efi 257) にして、同じことを確かめる
- aikernel のパッケージを上げると /boot/Image はパッケージのものになる
- `aios build pkg` は bin/mkpkg.sh と同じものを aios の中で作る (user/src/lib/mkpkg.rs)。PKGBUILD の関数
  (pkgver prepare build package) は zsh を sh のふり (`emulate -R sh`) で動かし (aios に bash は入れない。pkg/ の
  PKGBUILD みなを bash と同じに読めることは確かめた。ap の .INSTALL も同じ)、ソース (git+、http(s) は fetch、横のファイル)、sha256、
  .PKGINFO、tar と zstd はこちらでする。zstd のコマンドがなければ ruzstd で縮める (速いが、-19 ほどは縮まない)。
  作業の場所は ~/.cache/aios/build/NAME。root はいらない (ファイルの持ち主は tar の中で root にする)
