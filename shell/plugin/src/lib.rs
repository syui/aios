//! aish-plugin: aish のプラグインを書くための SDK
//!
//! プラグインは aish とは別のプログラム。aish は `plugin NAME` (~/.aishrc) でプラグインを起こし、
//! 標準入力と標準出力をパイプでつなぎっぱなしにして、1 行 1 つの JSON で話す (README.md に全部)。
//!
//! ```no_run
//! use aish_plugin::{json, Spec, Value};
//! fn main() {
//!     let spec = Spec { name: "hello", hooks: &["prompt"], keys: &[], tools: &[] };
//!     aish_plugin::run(spec, |ev, v| match ev {
//!         "prompt" => json!({ "prompt": format!("{} $ ", v["pwd"].as_str().unwrap_or("")) }),
//!         _ => json!({}),
//!     });
//! }
//! ```
//!
//! キーに結びつけた機能 (`key`) が呼ばれているあいだは、端末はプラグインのもの:
//! [`Tty`] で /dev/tty に描いて読み、[`pick`] で絞りこんで選ばせることができる。
//! aish は打ちかけの行の下の行の頭にカーソルを置いてから呼ぶので、終わったらそこへ戻して返すこと
//!
//! 同じ機能を、端末なしで JSON だけで呼べるようにもできる (`tools`)。人はキーで、
//! Claude は `aish --mcp` (MCP のツール) で、同じプラグインを使う。答えは JSON のオブジェクト
//! (しくじったら [`error`])

pub use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// プロトコルの版 (hello の version)。2 で tools が増えた
pub const VERSION: u64 = 2;

/// 端末なしで呼べる機能 (aish --mcp で MCP のツールになる)
pub struct Tool {
    /// 名前 (英数字と _ -)
    pub name: &'static str,
    /// 何をするか (Claude が読む)
    pub desc: &'static str,
    /// 引数 (hello で JSON Schema にして送る: [`schema`])
    pub args: &'static [Arg],
}

/// 引数の型 (JSON Schema の type。配列なら items も)
#[derive(Clone, Copy)]
pub enum Ty {
    Str,
    Int,
    Num,
    Bool,
    /// 文字列の配列
    Strs,
    /// 整数の配列
    Ints,
    /// オブジェクト (中の形は決めない: 名前 → 値)
    Obj,
    /// オブジェクトの配列 (中の引数)
    Objs(&'static [Arg]),
    /// 決まった文字列のどれか
    OneOf(&'static [&'static str]),
}

/// ツールの引数 1 つ。const で書く:
/// `const VM: &[Arg] = &[arg("op", Str, "start / stop").req(), arg("bg", Bool, "うしろで")];`
#[derive(Clone, Copy)]
pub struct Arg {
    pub name: &'static str,
    pub ty: Ty,
    /// 何か (Claude が読む。dir のような、みなが同じ意味のものは空でもよい)
    pub desc: &'static str,
    pub required: bool,
}

/// 引数 (なくてもよいもの。いるものは .req())
pub const fn arg(name: &'static str, ty: Ty, desc: &'static str) -> Arg {
    Arg { name, ty, desc, required: false }
}

impl Arg {
    /// いる引数に
    pub const fn req(self) -> Arg {
        Arg { required: true, ..self }
    }
}

/// 引数の並びを JSON Schema (type: object) に
pub fn schema(args: &[Arg]) -> Value {
    let mut props = serde_json::Map::new();
    let mut req = Vec::new();
    for a in args {
        let mut p = match a.ty {
            Ty::Str => json!({ "type": "string" }),
            Ty::Int => json!({ "type": "integer" }),
            Ty::Num => json!({ "type": "number" }),
            Ty::Bool => json!({ "type": "boolean" }),
            Ty::Strs => json!({ "type": "array", "items": { "type": "string" } }),
            Ty::Ints => json!({ "type": "array", "items": { "type": "integer" } }),
            Ty::Obj => json!({ "type": "object" }),
            Ty::Objs(inner) => json!({ "type": "array", "items": schema(inner) }),
            Ty::OneOf(v) => json!({ "type": "string", "enum": v }),
        };
        if !a.desc.is_empty() {
            p["description"] = json!(a.desc);
        }
        props.insert(a.name.to_string(), p);
        if a.required {
            req.push(a.name);
        }
    }
    let mut s = json!({ "type": "object", "properties": props });
    if !req.is_empty() {
        s["required"] = json!(req);
    }
    s
}

/// プラグインが何をするか (hello への答え)
pub struct Spec {
    /// 名前 (`plugin` の一覧に出る)
    pub name: &'static str,
    /// 受けとるフック: hello のあと、ここにあるものだけ送られてくる。
    /// "prompt" "suggest" "complete" "key" "preexec" "precmd" "chpwd" "not_found"
    pub hooks: &'static [&'static str],
    /// 既定のキー: (キー, 機能の名前)。キーは "C-r"、"M-f"、"C-p C-p" (2 つ続けて) のように書く。
    /// ~/.aishrc の bindkey で変えられる
    pub keys: &'static [(&'static str, &'static str)],
    /// 端末なしで呼べる機能。呼ばれると ev が "tool" で、v["name"] と v["args"] がくる
    pub tools: &'static [Tool],
}

/// プラグインの本体: 標準入力から 1 行ずつ JSON を読み、f の答えを 1 行の JSON で返す。
/// f は (イベントの名前, イベント全体) を受けとる。hello も f に渡す (答えは使わない) ので、
/// そこで histfile などを覚えておける。標準入力が閉じたら (aish が終わったら) 戻る
pub fn run(spec: Spec, mut f: impl FnMut(&str, &Value) -> Value) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let ev = v["ev"].as_str().unwrap_or("").to_string();
        let reply = if ev == "hello" {
            f("hello", &v);
            let keys: serde_json::Map<String, Value> = spec.keys.iter().map(|(k, w)| (k.to_string(), json!(w))).collect();
            let tools: Vec<Value> = spec
                .tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.desc, "input": schema(t.args) }))
                .collect();
            json!({ "name": spec.name, "version": VERSION, "hooks": spec.hooks, "keys": keys, "tools": tools })
        } else {
            let r = f(&ev, &v);
            if r.is_object() { r } else { json!({}) }
        };
        if writeln!(out, "{}", reply).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

/// 1 行の JSON を読む (読めなければ None)
pub fn parse(s: &str) -> Option<Value> {
    serde_json::from_str(s).ok()
}

/// tool の答え: しくじった
pub fn error(msg: impl std::fmt::Display) -> Value {
    json!({ "error": msg.to_string() })
}

/// イベントの文字列のフィールド (なければ空)
pub fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// シェルの語として入れるときに、特別な文字の前に \ をつける (~ は頭ならそのまま)
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if " \t'\"\\$`&|;<>()*?[]#!{}".contains(c) || (c == '~' && i > 0) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// シェルの語にする (いらなければそのまま、いれば '...' で囲む)
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "/._-~+,:@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// HOME の下なら ~ で短く
pub fn tilde(path: &str, home: &str) -> String {
    if !home.is_empty() && (path == home || path.starts_with(&format!("{}/", home))) { format!("~{}", &path[home.len()..]) } else { path.to_string() }
}

/// 文字の画面の幅 (全角は 2、合わせる文字は 0)
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u == 0 || (0x300..=0x36f).contains(&u) || (0x200b..=0x200f).contains(&u) || (0xfe00..=0xfe0f).contains(&u) {
        return 0;
    }
    let wide = [
        (0x1100, 0x115f),
        (0x2e80, 0x303e),
        (0x3041, 0x33ff),
        (0x3400, 0x4dbf),
        (0x4e00, 0x9fff),
        (0xa000, 0xa4cf),
        (0xac00, 0xd7a3),
        (0xf900, 0xfaff),
        (0xfe30, 0xfe4f),
        (0xff00, 0xff60),
        (0xffe0, 0xffe6),
        (0x1f300, 0x1f64f),
        (0x1f900, 0x1f9ff),
        (0x20000, 0x3fffd),
    ];
    if wide.iter().any(|&(a, b)| (a..=b).contains(&u)) { 2 } else { 1 }
}

/// 見える幅 w までに切る (色の ESC [ ... はそのまま、幅に数えない)
pub fn clip(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '\x1b' {
            out.push(c);
            if cs.peek() == Some(&'[') {
                out.push(cs.next().unwrap());
                for d in cs.by_ref() {
                    out.push(d);
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        let cw = char_width(c);
        if n + cw > w {
            break;
        }
        n += cw;
        out.push(c);
    }
    out
}

/// 端末のキー (ESC で始まる並びはまとめて 1 つ)
#[derive(Debug, PartialEq)]
pub enum Key {
    /// 制御文字か ASCII (0x0d = Enter、0x09 = Tab、0x7f = BS、C-a = 0x01 ...)
    Byte(u8),
    /// ASCII でない 1 文字
    Text(String),
    /// ESC [ の続き ("A" = ↑、"B" = ↓、"C" = →、"D" = ←、"Z" = S-Tab、"3~" = Del ...)
    Csi(String),
    /// Alt (ESC + 文字)
    Alt(u8),
    Esc,
}

/// 制御する端末 (/dev/tty)。aish が 1 文字ずつ・エコーなしの形にしてから key を呼ぶので、そのまま読める
pub struct Tty {
    f: std::fs::File,
}

impl Tty {
    pub fn open() -> std::io::Result<Tty> {
        Ok(Tty { f: std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")? })
    }

    pub fn write(&mut self, s: &str) {
        let _ = self.f.write_all(s.as_bytes());
        let _ = self.f.flush();
    }

    /// 端末の幅
    pub fn cols(&self) -> usize {
        use std::os::fd::AsRawFd;
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
    }

    /// ms だけ待って 1 バイト (負なら待ちつづける)
    fn byte(&mut self, ms: i32) -> Option<u8> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        loop {
            let r = unsafe { libc::poll(&mut p, 1, ms) };
            if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if r <= 0 {
                return None;
            }
            let mut b = [0u8];
            return match self.f.read(&mut b) {
                Ok(1) => Some(b[0]),
                _ => None,
            };
        }
    }

    /// まだ読んでいないキーがあるか (打ちつづけているあいだは重い仕事をあとにする)
    pub fn pending(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 0) > 0 }
    }

    /// キーを 1 つ読む (閉じたら None)
    pub fn key(&mut self) -> Option<Key> {
        let c = self.byte(-1)?;
        Some(match c {
            0x1b => match self.byte(50) {
                Some(b'[') | Some(b'O') => {
                    let mut seq = String::new();
                    while let Some(d) = self.byte(50) {
                        seq.push(d as char);
                        if (0x40..=0x7e).contains(&d) {
                            break;
                        }
                    }
                    Key::Csi(seq)
                }
                Some(d) => Key::Alt(d),
                None => Key::Esc,
            },
            c if c >= 0x80 => {
                let n = if c >= 0xf0 { 3 } else if c >= 0xe0 { 2 } else if c >= 0xc0 { 1 } else { 0 };
                let mut b = vec![c];
                for _ in 0..n {
                    if let Some(d) = self.byte(50) {
                        b.push(d);
                    }
                }
                Key::Text(String::from_utf8_lossy(&b).into_owned())
            }
            c => Key::Byte(c),
        })
    }

    /// 端末のクリップボードへ (OSC 52)
    pub fn copy(&mut self, text: &str) {
        self.write(&format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
    }
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for ch in b.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= ch.len() {
                out.push(T[(n >> (18 - 6 * k)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// 絞りこんで選ぶ (いまのカーソルの行から下に出す)。空白で区切った語をすべてふくむもの (大文字小文字は
/// 区別しない) が残る。↑↓ C-p C-n Tab で動き、Enter で決める。Esc C-c C-g でやめる (None)。
/// 終わったらカーソルを始めの場所に戻し、出したものは消す
pub fn pick(label: &str, items: &[String]) -> Option<String> {
    pick_live(label, |query| {
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
        items
            .iter()
            .filter(|it| {
                let low = it.to_lowercase();
                words.iter().all(|w| low.contains(w.as_str()))
            })
            .cloned()
            .collect()
    })
}

/// 打つたびに候補を作りなおして選ぶ (find は問いから候補を作る。rg で探すときなど)。キーは pick と同じ
pub fn pick_live(label: &str, mut find: impl FnMut(&str) -> Vec<String>) -> Option<String> {
    let mut tty = Tty::open().ok()?;
    let mut query = String::new();
    let mut cur = 0usize;
    let rows = 12;
    // 問いが変わったときだけ作りなおす
    let mut asked: Option<String> = None;
    let mut hits: Vec<String> = Vec::new();
    let r = loop {
        // 続けて打っているあいだは作りなおさない (遅いマシンで、打つたびの rg がたまらないように)
        if asked.as_deref() != Some(query.as_str()) && !tty.pending() {
            hits = find(&query);
            asked = Some(query.clone());
        }
        cur = cur.min(hits.len().saturating_sub(1));
        let top = cur.saturating_sub(rows - 1);
        let cols = tty.cols().saturating_sub(1);
        let mut out = format!("\r\x1b[J\x1b[33m{}>\x1b[0m {} ({})", label, query, hits.len());
        let mut n = 0;
        for (k, h) in hits.iter().enumerate().skip(top).take(rows) {
            let line = clip(&format!("  {}", h), cols);
            if k == cur {
                out.push_str(&format!("\r\n\x1b[7m{}\x1b[0m", line));
            } else {
                out.push_str(&format!("\r\n{}", line));
            }
            n += 1;
        }
        if n > 0 {
            out.push_str(&format!("\x1b[{}A", n));
        }
        // カーソルは問いの終わりに
        out.push_str(&format!("\r\x1b[{}C", label.chars().count() + 2 + query.chars().map(char_width).sum::<usize>()));
        tty.write(&out);
        let Some(key) = tty.key() else { break None };
        match key {
            Key::Byte(b'\r') | Key::Byte(b'\n') => break hits.get(cur).map(|s| s.to_string()),
            Key::Esc | Key::Byte(0x03) | Key::Byte(0x07) => break None,
            Key::Byte(0x0e) | Key::Byte(0x09) => cur = (cur + 1).min(hits.len().saturating_sub(1)),
            Key::Byte(0x10) => cur = cur.saturating_sub(1),
            Key::Csi(s) if s == "B" => cur = (cur + 1).min(hits.len().saturating_sub(1)),
            Key::Csi(s) if s == "A" => cur = cur.saturating_sub(1),
            Key::Byte(0x7f) | Key::Byte(0x08) => {
                query.pop();
                cur = 0;
            }
            Key::Byte(0x15) => {
                query.clear();
                cur = 0;
            }
            Key::Text(t) => {
                query.push_str(&t);
                cur = 0;
            }
            Key::Byte(c) if c >= 0x20 => {
                query.push(c as char);
                cur = 0;
            }
            _ => {}
        }
    };
    tty.write("\r\x1b[J");
    r
}

/// ripgrep (rg) の答え
pub struct Rg {
    /// --json の行のうち match と context ({"type": ..., "data": {path, line_number, lines, ...}})
    pub items: Vec<Value>,
    /// match が max を超えたので途中でやめた
    pub truncated: bool,
    /// rg の標準エラー (パターンのまちがいなど)
    pub err: String,
    /// rg の終わりのステータス (0 見つかった、1 なかった、2 しくじった。途中でやめたら 0)
    pub status: i32,
}

/// rg --json ARGS を dir で動かす。match が max を超えたら止める。rg がなければ None
pub fn rg_json(dir: &str, args: &[String], max: usize) -> Option<Rg> {
    use std::io::Read;
    let mut child = std::process::Command::new("rg")
        .arg("--json")
        .args(args)
        .current_dir(if dir.is_empty() { "." } else { dir })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    // 標準エラーは別に読む (たくさん出ても rg が止まらないように)
    let errs = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            s
        })
    });
    let mut items = Vec::new();
    let mut matches = 0;
    let mut truncated = false;
    for line in std::io::BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        let Some(v) = parse(&line) else { continue };
        match v["type"].as_str() {
            Some("match") => {
                if matches >= max {
                    truncated = true;
                    break;
                }
                matches += 1;
                items.push(v);
            }
            Some("context") => items.push(v),
            _ => {}
        }
    }
    if truncated {
        let _ = child.kill();
    }
    let st = child.wait();
    let err = errs.and_then(|t| t.join().ok()).unwrap_or_default();
    let st = st.ok().and_then(|s| s.code()).unwrap_or(0);
    Some(Rg { items, truncated, err, status: if truncated { 0 } else { st } })
}

/// rg --files (.gitignore と隠しファイルをのぞいた、dir の下のファイル)。max まで。rg がなければ None
pub fn rg_files(dir: &str, max: usize) -> Option<Vec<String>> {
    let mut child = std::process::Command::new("rg")
        .arg("--files")
        .current_dir(if dir.is_empty() { "." } else { dir })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let out = child.stdout.take()?;
    let files: Vec<String> = std::io::BufReader::new(out).lines().map_while(Result::ok).take(max).collect();
    let _ = child.kill();
    let _ = child.wait();
    Some(files)
}

/// rg の JSON の文字 ({"text": ...} か {"bytes": base64})
pub fn rg_text(v: &Value) -> String {
    v["text"].as_str().map(String::from).unwrap_or_default()
}

/// 使ったパスの順位 (aish-pick が ~/.cache/aish/paths に書く) の点: 使った回数 × 新しさ
/// (1 時間以内 4 倍、1 日 2 倍、1 週間 0.5 倍、それより前 0.25 倍。z と同じ)
pub fn frecency(rank: f64, time: u64, now: u64) -> f64 {
    let age = now.saturating_sub(time);
    let w = if age < 3600 {
        4.0
    } else if age < 86400 {
        2.0
    } else if age < 604800 {
        0.5
    } else {
        0.25
    };
    rank * w
}

/// いまの時刻 (秒)
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// パス → 点 (aish-pick の paths。ほかのプラグインが「よく使うものを先に」並べるのに使う)
pub fn path_scores(home: &str) -> std::collections::HashMap<String, f64> {
    let t = now();
    let text = std::fs::read_to_string(format!("{}/.cache/aish/paths", home)).unwrap_or_default();
    text.lines()
        .filter_map(parse)
        .filter_map(|v| Some((v["path"].as_str()?.to_string(), frecency(v["rank"].as_f64().unwrap_or(1.0), v["time"].as_u64().unwrap_or(0), t))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Ty::*;
    use super::*;

    // const で入れ子も書ける (edit の edits のようなもの)
    const EDIT: &[Arg] = &[
        arg("path", Str, "ファイル").req(),
        arg("edits", Objs(&[arg("old", Str, "").req(), arg("new", Str, "").req(), arg("all", Bool, "")]), "いくつも"),
        arg("kind", OneOf(&["file", "dir"]), "どちら"),
        arg("only", Ints, "番号"),
        arg("headers", Obj, "名前 → 値"),
        arg("dir", Str, ""),
    ];

    #[test]
    fn schemas() {
        let s = schema(EDIT);
        assert_eq!(s["type"], "object");
        assert_eq!(s["required"], json!(["path"]));
        assert_eq!(s["properties"]["path"], json!({ "type": "string", "description": "ファイル" }));
        assert_eq!(s["properties"]["edits"]["items"]["required"], json!(["old", "new"]));
        assert_eq!(s["properties"]["edits"]["items"]["properties"]["all"], json!({ "type": "boolean" }));
        assert_eq!(s["properties"]["kind"]["enum"], json!(["file", "dir"]));
        assert_eq!(s["properties"]["only"]["items"], json!({ "type": "integer" }));
        assert_eq!(s["properties"]["headers"], json!({ "type": "object", "description": "名前 → 値" }));
        // 説明のないもの (dir) は description なし、いるものがなければ required なし
        assert!(s["properties"]["dir"].get("description").is_none());
        assert!(schema(&[]).get("required").is_none());
    }
}
