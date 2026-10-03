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
| `aish-pick` | base | C-r 履歴 / C-f ファイル / C-o よく使うパス / C-j 最近のディレクトリ / C-k `cd ..` / C-p C-p コピー。ツール `history` `dirs` `paths` |
| `aish-edit` | base | ツールだけ: `read` `edit` (ぴったり置きかえ) `write` `grep` `sed` `lines` (行の番号で) `undo`。`aish --mcp` で読む |
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

- `run {cmd, timeout_ms?, stdin?}` → `{status, out, err, ms, pwd}`。いつも同じシェルで動くので、
  `cd` や変数、関数は次の `run` に残る。時間切れなら子と孫を止めて `status: 124, timeout: true`。
  `exit` はその `run` だけを終える
- 設定は対話のときと同じ `/etc/aishrc` と `~/.aishrc`。`AISH_MCP=1` なので `[ -n "$AISH_MCP" ] && ...` で分けられる
- `paths` (aish-pick) は、コマンドの行とツールで使ったファイルとディレクトリを、使った回数 × 新しさ
  (zoxide と同じ) で並べる。覚えるのは `~/.cache/aish/paths` (1 行 1 つの JSON)
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
| `complete` | `line pos cmds vars path home pwd` | `{"start":N,"cands":[{"text":"...","show":"...","dir":false}]}` | Tab。`start` から `pos` までを `text` と置きかえる (`pos` と `start` は文字の数)。1 つなら決め、たくさんなら aish が並べて選ばせる |
| `key` | `widget key line pos pwd home histfile` | 下の「キーの答え」 | `keys` / `bindkey` で結んだキー |
| `preexec` | `line pwd` | `{}` | 行を動かす前 (`aish --mcp` の `run` でも) |
| `precmd` | `status` | `{}` | プロンプトを出す前 (`aish --mcp` では `run` のあと) |
| `chpwd` | `pwd old` | `{}` | `cd` でディレクトリが変わったとき |
| `not_found` | `args line pwd status` | `{"status":N}` (引き受けたとき) | コマンドが見つからなかったとき。`{}` なら aish が "command not found" を出す |
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
