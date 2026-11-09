# aish のプラグイン

aish の本体は、POSIX のシェルと行の編集の土台と履歴だけ。補完、グレーの候補、キーに結ぶ機能、プロンプトなどは
**プラグイン**が出す。プラグインは aish とは別のプログラムで、aish はそれを起こして標準入力と標準出力を
パイプでつなぎっぱなしにし、**1 行 1 つの JSON** で話す。

```sh
plugin aish-complete          # 読む (~/.aishrc に書く)。$AISH_PLUGIN_PATH (なければ
                              # ~/.local/lib/aish/plugins:/usr/lib/aish/plugins) と PATH から探す
plugin                        # つないでいるものの一覧
bindkey C-g pick:dir          # キーとプラグインの機能を結ぶ (PLUGIN:WIDGET)
bindkey -r C-g                # 外す
bindkey                       # 一覧
```

| プラグイン | パッケージ | すること |
|---|---|---|
| `aish-complete` | base | Tab の補完 (コマンド、ファイル、`$変数`) |
| `aish-suggest` | base | 打っている行に続く履歴をグレーで出す |
| `aish-highlight` | base | 打っている行の色 (zsh-syntax-highlighting の既定の色): あるコマンド (組み込み、alias、関数、PATH) は緑、ないものは太い赤、sudo などの前置きは緑の下線、予約語と文字列は黄、`"..."` の中の `$変数` はシアン、あるファイルは下線、`* ?` は青、コメントは灰 |
| `aish-time` | base | 長くかかったコマンドの時間と、しくじったときの終了コードを、終わったあとに 1 行で (`-- 12.3s, exit 1`。zsh の REPORTTIME)。`plugin aish-time 秒` で時間を出すしきい値 (既定 3 秒) |
| `aish-pick` | base | C-r 履歴 / C-f ファイル / C-g rg で探して開く / C-o よく使うパス / C-j 最近のディレクトリ / C-k `cd ..` / C-p C-p コピー。ツール `history` `dirs` `paths` |
| `aish-edit` | base | ツールだけ: `read` (`paths: ["a.rs", "b.rs:10-40"]` でいくつもまとめて。ディレクトリなら中の名前の一覧) `edit` (ぴったり置きかえ。`edits` でいくつも、ぜんぶかなにもしないか。`edits` の中の `path` でいくつものファイルを 1 回で、`undo` 1 回でみな戻る。答えに変えた行とまわり 2 行を番号つきで) `write` `grep` (ripgrep があればそれで。よく使うファイルが先で、見つけた行に番号 `n`。文字として何もなく、`a|b` や `\.` `\(` のような正規表現の形なら、正規表現として探しなおして `as_regex` に書く) `hit` (n 番のまわり) `each` (見つけた行だけ置きかえる) `sed` (`each` と `sed` は `subs` で何組も。`grep` `sed` `each` は `word: true` で単語ぴったり) `lines` (行の番号で) `undo`。`aish --mcp` で読む |
| `aish-map` | base | 探さなくていいように: ツール `where` (名前から定義の場所。ぴったり → 前が同じ → 含む、よく使うファイルが先。`body: true` でいちばん上の定義の中身も) `outline` (ファイルの中の定義)。M-. で選んで `$EDITOR +行 ファイル`。rg が要る |
| `aish-fix` | base | vim の quickfix: ツール `build` (うしろでビルドして、エラーを番号 `n` つきで `n path:line: error[E..]: ...`。cargo には `--message-format=json` を足して rustc の診断をそのまま読み (`cargo test` / `run` / `bench` は、`.cargo/config.toml` の `[build] target` がこのマシンで動かせない CPU で runner もなければ、このマシンの `--target` も足す。aish のワークスペースは aios の aarch64 musl が既定なので、x86_64 で test すると exec format error になっていた)、cc や zig は `path:line:col: error:` の行を拾う。`cargo test` で落ちたテストも `test failed: NAME: assert の説明` の 1 行に (場所は panicked at から。fix の全文のスタックトレースは自分のコードの段だけ)、まとめの `test result:` は `tests` に。`wait_ms` を過ぎたら `running`) `errors` (待って同じ形で。`kill` で止める) `fix` (n 番の全文とまわりのソース。rustc の直し方があれば `suggestions` に番号 k つきで出し、`apply: k` でファイルにあてる。穴あき (HasPlaceholders) のものと、ビルドのあとで変えたファイルにはあてない。`all: true` ならすべての診断の確かな (MachineApplicable) 直し方をまとめて。cargo clippy --fix のように)。M-e でビルドして選んで `$EDITOR +行 ファイル` |
| `aish-sys` | base | aios の様子: ツール `health` (いまの見たて: `aios health --json`。1 つずつ ok / info / warn / crit と、次に使う道具) `sys` (まとめ) `procs` (CPU / メモリの順) `kmsg` (カーネルのメッセージ、/proc/kmsg) `log` (サービスのログ) `bkl` (コマンドを動かして大きなロックを測る) `strace` (コマンドを動かしてシステムコールを kmsg から) `threads` (/proc/ai/threads: 眠っているスレッドが何をどれだけ待っているか) `hang` (プロセスがなぜ止まっているかの見立て。pid も name もなければ、みなからあやしいものを探す) `stack` (呼び出しの並び。.eh_frame でたどり、名前のないところは近くの文字列) `watch` (aiwatch が見つけたもの: 起こしが消えた、起こしすぎ、大きなロックの混雑) `fw` (ファイアウォール: /proc/ai/fw の基の鎖と決まりごとの当たった数と式 (nft --debug=netlink の形)、落としたもの (どのパケットをどのフックのどの決まりが)、conntrack。`trace` でパケットを流さずにいまの決まりの道を試す (`in eth0 tcp 10.0.2.2:40000 10.0.2.15:22 syn`)、`nft: true` で nft -a list ruleset も)。M-s でまとめを出す |
| `aish-pkg` | base | パッケージを最新に: ツール `pkg_check` `pkg_edit` `pkg_build` `pkg_fetch` `pkg_test` `pkg_scan` `pkg_rebuild` `pkg_update` `pkg_push`、M-p で check、`aish-pkg` のコマンドでも。くわしくは doc/aios.md の「パッケージの手入れ (aish-pkg)」 |
| `aish-claude` | base | 見つからなかったコマンドの行を `claude -p` に渡し、答えを端末に出す (claude があれば。人が打つときだけ。`AISH_CLAUDE=0` で止める。`AISH_AI=local` なら aish-local にゆずる)。aios の Claude Code は `/etc/claude-code/managed-mcp.json` で `aish --mcp` を使う |
| `aish-local` | base | aish-claude のうしろで、claude が答えなかったとき (ない、`AISH_CLAUDE=0`、`AISH_AI=local`) に、見つからなかったコマンドの行を llama-server (このマシンの AI。`AISH_LOCAL_URL`、既定 `http://127.0.0.1:8080`) の `/v1/chat/completions` に渡し、答えを届いたところから端末に出す (Ctrl-C で止まる)。答えるだけでコマンドは動かさない。15 分以内の 4 つまでのやりとりを `~/.cache/aish/local.json` に残して続けて話す。動いていなければふつうの command not found (`/var/lib/ai/model.gguf` があれば起こし方を 1 度だけ)。前置きは `AISH_LOCAL_SYSTEM` (空なら前置きなし)、`AISH_LOCAL=0` で止める |
| `aish-wait` | base | ツールだけ: `wait` (プロセスが終わる、ファイルに文字が出る、ポートが開く、まで)。`aish --mcp` で読む |
| `aish-fetch` | base | ツールだけ: `fetch` (url か file を curl で。JSON なら `path` (`.a.b[0].c`、`.items[].name`、`.a[-1]`) で取り出し、`fields` (`["name","head.msg"]`) で配列の 1 つずつを `name=x  head.msg=y` の 1 行に。`method` `body` `headers` も)。`curl \| python3 -c json` のかわり。`aish --mcp` で読む |
| `aish-git` | base | ツールだけ: `git` (ブランチと HEAD、変えたファイル、remote と `git config aish.mirror` のミラーごとの進み・おくれ (`ls-remote` なので fetch しない)、push していないコミットの署名 (`gpg` / `ssh` / `none` と鍵) とコミットした人。設定 (`user.email`、`user.signingkey`、`gpg.format`) とちがう署名や人、remote のほうが進んでいるものは `problems` に。push の前に) `ci` (push したコミットの GitHub Actions の run を終わるまで待つ。結果とかかった時間、落ちたものはどの job のどの step か。1 回に 50 秒までで、まだなら `running: true`。API は 15 秒に 1 度。`workflow: "unix"` でその run だけを待つ)。`aish --mcp` で読む |
| `aish-vm` | (開発のマシン) | ツールだけ。aios を作るマシン (Linux) で、QEMU の VM を相手にする (`bin/aish-mcp.sh` の aish が読む): `vm` (start: カーネルを選んで起こす。パス / `.` で作業中のソースから / git のコミット。stop status run (`bg: true` で中でうしろで動かしてすぐ答え、`job` (`id`、`wait_ms`、`kill`) で様子と、前に見せたところから先の出力。50 秒より長い ap -S firefox なども。中では /tmp/.vmjob/ID.sh をサブシェルで . で読み、出力・終わりの番号・pid をとっておく。止めるときはグループごと) put get (中のファイルを外へ: gzip と base64 を 1 行にしてシリアルで。sha256 を確かめる。中で撮った png を Read で見るなど) shot keys (`@alt-b` などと文字) log (grep) mon。`test/vm.py` を呼ぶ) `try` (C / C++ を `zig cc` で aarch64 の静的なプログラムにして、VM の中で動かして出力と終了コード) `ab` (2 つのカーネルで同じコマンドを n 回ずつ: 時間のまんなか・範囲と大きなロックの割合、範囲が重ならなければ clear) `bisect` (good と bad のあいだを git bisect: コミットごとにカーネルを作って VM で test。0 good、125 skip、ほか bad) `box` (カーネルの砂場: kernel.json の off のあるもの (extra、quirk と core の ns) を起動の引数 `aios.off=` で切って起こし、test (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときと FAIL の行をくらべる。`each` で 1 つずつ全部。`vm` の start にも `off`) `bench` (ファイルまわりの速さ: test/fsbench.c を n 回、op ごとの µs のまんなかと、prof をつけた 1 回の /proc/ai/prof の part ごとの割合。`against` で 2 つのカーネルをくらべる。結果は build/aish-vm/bench/ にも)。ab・bisect・box の test はサブシェルで (exit で中のシェルごと終わらないように)。start・ab・bisect・box・bench はうしろで動き、もういちど呼ぶと様子か結果。ab・bisect・box・bench は動いている VM を止めて `-snapshot` で起こしなおす。カーネルは `build/aish-vm/` に作ってとっておく |
| `aish-powerline` | base | powerline のプロンプト (`~/.aishrc` の `plugin aish-powerline` で読む。アイコンと区切りは aifont か Nerd Font の文字) |

## 書き方

Rust なら SDK (`plugin/`、クレート名 `aish-plugin`) を使うと、イベントに答える関数を 1 つ書くだけ:

```rust
use aish_plugin::{json, s, Spec};

fn main() {
    let spec = Spec { name: "hello", hooks: &["prompt"], keys: &[("C-x", "greet")], tools: &[] };
    aish_plugin::run(spec, |ev, v| match ev {
        "prompt" => json!({ "prompt": format!("{} > ", s(v, "pwd")) }),
        "key" => json!({ "insert": "hello" }),
        _ => json!({}),
    });
}
```

`shell/` に新しいクレートを足して `cargo build --release -p NAME` すると、
`target/aarch64-unknown-linux-musl/release/NAME` ができる。aios の `~/.local/lib/aish/plugins/` に置いて
`plugin NAME`。プラグインは JSON を流しこめば一人で試せる:

```sh
printf '%s\n' '{"ev":"hello","version":1}' '{"ev":"prompt","pwd":"/tmp"}' | aish-powerline
```

ほかの言語でもよい (標準入力から 1 行読み、1 行の JSON を書いて flush するだけ)。

## 人はキーで、Claude は MCP で

プラグインの機能には顔が 2 つある。人は `keys` (端末で絞りこんで選ぶ)、Claude は `tools`
(端末なしで、JSON を渡して JSON が返る)。`aish --mcp` は aish を MCP のサーバーにして、
組み込みの `run` と、読んだプラグインの `tools` を MCP のツールとして見せる。

```sh
claude mcp add aish -- aish --mcp     # Claude Code から
```

このリポジトリの `.mcp.json` は `bin/aish-mcp.sh` を起こす: aios の中ならそこの aish を、開発の Linux なら
このリポジトリの aish とプラグインをそのマシン向けにビルドして (`shell/mcp.rc` を読む)。`aish --mcp RC...` で
設定を足せる。

ツールはシェルからも呼べる: `tool NAME KEY=VALUE...` (または `tool NAME '{JSON}'`。`tool` だけなら一覧)。
本文は標準出力、行の数などの残りは JSON の 1 行で標準エラーへ。パイプや `$(...)` の中でも動く
(そのときはプラグインを一度だけ別に起こす)。ビルドしなおしたあと、MCP の客がツールの新しい引数をまだ知らないときも
`run` から使える。

- `run {cmd, bg: true}` はうしろで動かしてすぐ `{job, pid}` を答える (シェルを fork した子なので、`cd` などはその中だけ)。
  `job {id, wait_ms?, kill?}` で様子と出力 (`{done, status, out, err, ms}`。id がなければ一覧)。重いビルドのあいだも
  ほかのツールが使える。終わったら、job で読むまでどのツールの答えにも 1 度だけ `jobs_done` (`job 1 ended (status 0, 6m 45s): ...`)
  がつく (MCP の客は知らせでは起きないので、job を何度も呼ばなくても、ほかのことをしているあいだに気づける)。子はプロトコルとプラグインとの口を閉じる (持ったままだと、サーバーが終わってもつなぎ先は
  閉じたと気づかずに、どのツールも 60 秒待って時間切れになり、プラグインも残った)
- 答えの `content` の text は読みやすい形: `out` と `text` (read、hit) はエスケープせずにそのまま、`err` は `[err]` のあと、
  grep はファイルごとに、パスの行のあとに 1 行に 1 つ (`n line: text`)、read の `paths` と edit のいくつものファイル (`files`) は
  ファイルごとに、パスと残り (`{"lines":12}` や `{"error":...}`) の 1 行のあとに中身、ほかは終わりに 1 行の JSON (なにもなければ出さない)。`aish --mcp --json` なら、いままでどおり
  JSON の text と `structuredContent` (ほかのプログラムがつなぐとき。Claude Code は structuredContent があると
  そちらを見せるので、ふだんは付けない)
- 出力が長い (12000 バイトより) ときは、頭と終わりだけを行の切れ目で返し、まん中は `... (N lines, M bytes cut ...) ...` になる。
  答えの `out_id` を `out {id, grep?, regex?, from?, to?, err?}` に渡すと、もう一度動かさずに、切ったところを行の番号つきで
  探したり読んだりできる (メモリーに最近の 8 つだけ。ディスクには書かない)
- `run {cmd, timeout_ms?, stdin?}` → `{status, out, err, ms, pwd}`。いつも同じシェルで動くので、
  `cd` や変数、関数は次の `run` に残る。時間切れ (既定 50 秒。Claude Code はツールの答えを 60 秒しか待たないので、長いものは `bg` で) なら子と孫を止めて `status: 124, timeout: true`。
  `timeout_ms` も 50 秒より長くはできない (`AISH_MCP_MAX_MS` で変えられる。客があきらめたあとも run が続くと、あとの要求も
  みな待たされて時間切れになる)。`exit` はその `run` だけを終える
- サーバーが終わるわけ (つなぎ先が閉じた、読めなかった) とパニックは `~/.cache/aish/mcp.log` に (パニックと読めなかったときは、
  もとの標準エラーにも。run のあいだは標準エラーが memfd なので、ふつうの知らせは見えない。release は panic = "abort")。
  UTF-8 でない行は parse error と答えて続ける
- aish かプラグインをビルドしなおすと、次にツールを使ったときに、aish が同じつながりのまま新しいビルドに入れかわる
  (自分を exec しなおし、`notifications/tools/list_changed` を送る。`/mcp` でつなぎなおさなくてよい)。いまのディレクトリは残り、
  シェルの変数は空に戻る (答えに `reloaded`)。ビルドの途中 (cargo が動いている)、うしろのジョブが動いているか、
  終わっても job で読んでいないときは待つ (入れかわるとその出力がなくなる)。設定にない、`run` の中の `plugin NAME` で読んだものも
  起こしなおす。`run` の中の `plugin NAME` でツールが変わったときも `notifications/tools/list_changed` を送る (すぐ使える)
- `check` はつながりの様子: aish の版と起きてからの時間、プラグインが生きているか (止まったわけ)、起きたあとに
  ビルドしなおしたもの (`/mcp` でつなぎなおすと新しくなる)。`AISH_SRC` (`bin/aish-mcp.sh` がリポジトリを入れる)
  があれば、ソースがバイナリより新しいもの (ビルドが要る) と、うしろで動いている cargo も。`problems` が空なら `ok`
- 設定は対話のときと同じ `/etc/aishrc` と `~/.aishrc`。`AISH_MCP=1` なので `[ -n "$AISH_MCP" ] && ...` で分けられる
- `paths` (aish-pick) は、コマンドの行とツールで使ったファイルとディレクトリを、使った回数 × 新しさ
  (zoxide と同じ) で並べる。覚えるのは `~/.cache/aish/paths` (1 行 1 つの JSON)
- ripgrep (`rg`) があれば、`grep` ツールは `rg --json` で探す (.gitignore を読み、`glob` `hidden` も使える。
  答えの `engine` が `rg`)。C-f は `rg --files`、C-g は打ちながら rg で探して `$EDITOR +行 ファイル` にする。
  SDK の `rg_json` / `rg_files` / `pick_live` でほかのプラグインからも使える
- 出力はメモリー (memfd) に受けて答えに入れるだけ、`aish-edit` の取り消しの写しもメモリーだけ。
  ディスクには何も残らないので、リポジトリやイメージにまざらない (`bin/mkdisk.sh` も、ホームの
  `.cache` と履歴、aipkg のキャッシュをイメージに入れない)

## プロトコル (版 2)

aish がイベント `{"ev": NAME, ...}` を 1 行で送り、プラグインは**かならず 1 行の JSON オブジェクト**で答える
(何もしないなら `{}`)。答えが 3 秒来ないか、JSON のオブジェクトでなければ、aish はそのプラグインを止めて外す
(`key` と `not_found` は人と話すかもしれないので待ちつづける)。ただし打っているあいだのもの (`highlight` と
`suggest`) は 0.1 秒、`prompt` は 1 秒、知らせるだけのもの (`preexec` `precmd` `chpwd`) は 0.1 秒だけ待ち、
まにあわなければその答えは使わずに先へ進む (プラグインは止めない。遅れた答えは、次に送る前に読み捨てる。
`highlight` と `suggest` は、前の答えがまだ来ていなければ送らない)。マシンがいそがしくてプラグインがすぐ
答えられなくても、打つのは止まらない (前は 1 文字ごとに答えを 3 秒まで待ち、まにあわなければ止めていた)。
遅れた答えが 30 秒来なければ止める。プラグインの標準エラーは端末へ出る。
Ctrl-C (SIGINT) は無視するようにして起こされる。aish が終わると標準入力が閉じるので、そこで終わる。

### hello (最初に 1 回)

```json
{"ev":"hello","version":2,"shell":"aish","args":["plugin のあとの引数"],"home":"/home/ai","histfile":"/home/ai/.aish_history"}
```
答え:
```json
{"name":"pick","version":2,"hooks":["key","chpwd"],"keys":{"C-r":"history","C-p C-p":"copy"},
 "tools":[{"name":"history","description":"...","input":{"type":"object","properties":{"query":{"type":"string"}}}}]}
```
- `hooks`: 受けとるフック。このあと、ここにあるものだけが送られてくる
- `keys`: 既定のキー → 機能 (`widget`) の名前。`bindkey` であとから変えられる。キーは `C-r` `M-f` `Tab`
  `Enter` `Esc` `BS` で、`"C-p C-p"` のように空白で 2 つ続けたもの (1 つ目のあと 0.4 秒待つ) も書ける。
  aish の行の編集のキーより先に効く
- `tools` (版 2): 端末なしで呼べる機能。`name` (英数字と `_` `-`)、`description` (Claude が読む)、
  `input` (引数の JSON Schema)

### フック

| ev | 送るもの | 答え | いつ |
|---|---|---|---|
| `prompt` | `pwd home user host status root ssh jobs` | `{"prompt":"..."}` | プロンプトを出すとき。最初に答えたものを使う (なければ `$PS1`) |
| `suggest` | `line` | `{"suggest":"続き"}` | 1 文字打つたびに (カーソルが行の終わりのとき)。→ / End / C-e で決まる |
| `highlight` | `line cmds path pwd home` | `{"spans":[[始め,終わり,"SGR"],...]}` | 行が変わるたびに。文字の番号で、重ならない並び。SGR は `32` や `1;31` のような数字と `;` だけ |
| `complete` | `line pos cmds vars path home pwd` | `{"start":N,"cands":[{"text":"...","show":"...","dir":false}]}` | Tab。`start` から `pos` までを `text` と置きかえる (`pos` と `start` は文字の数)。1 つなら決め、たくさんなら aish が並べて選ばせる |
| `key` | `widget key line pos pwd home histfile` | 下の「キーの答え」 | `keys` / `bindkey` で結んだキー |
| `preexec` | `line pwd` | `{}` | 行を動かす前 (`aish --mcp` の `run` でも) |
| `precmd` | `status` | `{}` | プロンプトを出す前 (`aish --mcp` では `run` のあと) |
| `chpwd` | `pwd old` | `{}` | `cd` でディレクトリが変わったとき |
| `not_found` | `args line pwd status env` (いまの環境。export したものと `VAR=x cmd` の VAR) | `{"status":N}` (引き受けたとき) | コマンドが見つからなかったとき。`{}` なら aish が "command not found" を出す |
| `tool` | `name args pwd home histfile` | JSON のオブジェクト。しくじったら `{"error":"..."}` | `aish --mcp` でツールが呼ばれたとき (hooks に書かなくても来る)。答えを待ちつづける |

`prompt` `suggest` `complete` `not_found` は、フックを持つプラグインに順に聞き、最初の空でない答えを使う。
ほかは持っているものみなに知らせる。

### キーの答え

| フィールド | すること |
|---|---|
| `line` (`pos`) | 行をこれにする (カーソルは `pos`、なければ行の終わり) |
| `insert` | カーソルのところに入れる |
| `run` (`silent`) | このコマンドを動かす (`silent` なら履歴に残さない) |
| `accept` | いまの行を決める (Enter と同じ) |

`key` が呼ばれているあいだは、端末はプラグインのもの。aish は打ちかけの行の下の行の頭にカーソルを置き、
端末を 1 文字ずつ・エコーなしにしてから呼ぶ。プラグインは `/dev/tty` を開いて描き・読み (SDK の `Tty`、
絞りこんで選ぶ `pick`)、**終わったらカーソルを始めの場所 (その行の頭) に戻し、出したものを消して**答える。
そのあと aish が行を描きなおす。
