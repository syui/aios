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
| `aish-edit` | base | ツールだけ: `read` `edit` (ぴったり置きかえ。`edits` でいくつも、ぜんぶかなにもしないか。答えに変えた行とまわり 2 行を番号つきで) `write` `grep` (ripgrep があればそれで。よく使うファイルが先で、見つけた行に番号 `n`) `hit` (n 番のまわり) `each` (見つけた行だけ置きかえる) `sed` (`each` と `sed` は `subs` で何組も。`grep` `sed` `each` は `word: true` で単語ぴったり) `lines` (行の番号で) `undo`。`aish --mcp` で読む |
| `aish-map` | base | 探さなくていいように: ツール `where` (名前から定義の場所。ぴったり → 前が同じ → 含む、よく使うファイルが先。`body: true` でいちばん上の定義の中身も) `outline` (ファイルの中の定義)。M-. で選んで `$EDITOR +行 ファイル`。rg が要る |
| `aish-fix` | base | vim の quickfix: ツール `build` (うしろでビルドして、エラーを番号 `n` つきで `n path:line: error[E..]: ...`。cargo には `--message-format=json` を足して rustc の診断をそのまま読み、cc や zig は `path:line:col: error:` の行を拾う。`cargo test` で落ちたテストも `test failed: NAME: assert の説明` の 1 行に (場所は panicked at から。fix の全文のスタックトレースは自分のコードの段だけ)、まとめの `test result:` は `tests` に。`wait_ms` を過ぎたら `running`) `errors` (待って同じ形で。`kill` で止める) `fix` (n 番の全文とまわりのソース。rustc の直し方があれば `suggestions` に番号 k つきで出し、`apply: k` でファイルにあてる。穴あき (HasPlaceholders) のものと、ビルドのあとで変えたファイルにはあてない。`all: true` ならすべての診断の確かな (MachineApplicable) 直し方をまとめて。cargo clippy --fix のように)。M-e でビルドして選んで `$EDITOR +行 ファイル` |
| `aish-sys` | base | aios の様子: ツール `sys` (まとめ) `procs` (CPU / メモリの順) `kmsg` (カーネルのメッセージ、/proc/kmsg) `log` (サービスのログ) `bkl` (コマンドを動かして大きなロックを測る) `strace` (コマンドを動かしてシステムコールを kmsg から) `threads` (/proc/ai/threads: 眠っているスレッドが何をどれだけ待っているか) `hang` (プロセスがなぜ止まっているかの見立て) `stack` (呼び出しの並び。.eh_frame でたどり、名前のないところは近くの文字列)。M-s でまとめを出す |
| `aish-pkg` | base | パッケージを最新に: ツール `pkg_check` (配布元の最新といまの pkgver。どこを見るか `pkg/upstream.json` は PKGBUILD の git と Arch の `.nvchecker.toml` から作る。`#commit=` のものはリポジトリの HEAD と、PKGBUILD の `_latest_url` と `_latest_regex` があればそのページと、download.gnome.org のものはその cache.json とくらべる) `pkg_edit` (PKGBUILD の pkgver・pkgrel・sha256sums と `.aios.json` を書きかえる) `pkg_build` (bin/mkpkg.sh で `repo/aarch64/KIND/` に作り、aios.db を作りなおす) `pkg_test` (版、ELF が aarch64 か、bin/ を qemu-aarch64 か aarch64 で --version) `pkg_push` (ai/repo とくらべて bin/gitea.sh repo で送る。pkg_test を通っていないものや、ai/repo のほうが新しいものがあれば止まる)。build と push はうしろで動き、もういちど呼ぶと様子か結果。M-p で check を出す。一覧は `pkg/pkg.json` (1 つに 1 行で name type src now latest)。`.aios.json` の pkg は、check・edit・build のたびに pkg/ の PKGBUILD にそろえる (新しいものを足し、版を直し、なくなったものを消す。npm と uv は見ない)。`aish-pkg check` / `edit NAME [VER]` / `build NAME` / `test NAME` / `push` とコマンドでも |
| `aish-claude` | base | 見つからなかったコマンドの行を `claude -p` に渡し、答えを端末に出す (claude があれば。人が打つときだけ。`AISH_CLAUDE=0` で止める)。aios の Claude Code は `/etc/claude-code/managed-mcp.json` で `aish --mcp` を使う |
| `aish-wait` | base | ツールだけ: `wait` (プロセスが終わる、ファイルに文字が出る、ポートが開く、まで)。`aish --mcp` で読む |
| `aish-powerline` | aish-powerline | powerline のプロンプト |

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
  ほかのツールが使える
- 答えの `content` の text は読みやすい形: `out` と `text` (read、hit) はエスケープせずにそのまま、`err` は `[err]` のあと、
  grep はファイルごとに、パスの行のあとに 1 行に 1 つ (`n line: text`)、ほかは終わりに 1 行の JSON (なにもなければ出さない)。`aish --mcp --json` なら、いままでどおり
  JSON の text と `structuredContent` (ほかのプログラムがつなぐとき。Claude Code は structuredContent があると
  そちらを見せるので、ふだんは付けない)
- 出力が長い (12000 バイトより) ときは、頭と終わりだけを行の切れ目で返し、まん中は `... (N lines, M bytes cut ...) ...` になる。
  答えの `out_id` を `out {id, grep?, regex?, from?, to?, err?}` に渡すと、もう一度動かさずに、切ったところを行の番号つきで
  探したり読んだりできる (メモリーに最近の 8 つだけ。ディスクには書かない)
- `run {cmd, timeout_ms?, stdin?}` → `{status, out, err, ms, pwd}`。いつも同じシェルで動くので、
  `cd` や変数、関数は次の `run` に残る。時間切れ (既定 50 秒。Claude Code はツールの答えを 60 秒しか待たないので、長いものは `bg` で) なら子と孫を止めて `status: 124, timeout: true`。
  `exit` はその `run` だけを終える
- aish かプラグインをビルドしなおすと、次にツールを使ったときに、aish が同じつながりのまま新しいビルドに入れかわる
  (自分を exec しなおし、`notifications/tools/list_changed` を送る。`/mcp` でつなぎなおさなくてよい)。いまのディレクトリは残り、
  シェルの変数は空に戻る (答えに `reloaded`)。ビルドの途中 (cargo が動いている)、うしろのジョブが動いているときは待つ
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
(`key` と `not_found` は人と話すかもしれないので待ちつづける)。プラグインの標準エラーは端末へ出る。
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
