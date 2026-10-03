//! aish-plugin: aish のプラグインを書くための SDK
//!
//! プラグインは aish とは別のプログラム。aish は `plugin NAME` (~/.aishrc) でプラグインを起こし、
//! 標準入力と標準出力をパイプでつなぎっぱなしにして、1 行 1 つの JSON で話す (README.md に全部)。
//!
//! ```no_run
//! use aish_plugin::{json, Spec, Value};
//! fn main() {
//!     let spec = Spec { name: "hello", hooks: &["prompt"], keys: &[] };
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

pub use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// プロトコルの版 (hello の version)
pub const VERSION: u64 = 1;

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
            json!({ "name": spec.name, "version": VERSION, "hooks": spec.hooks, "keys": keys })
        } else {
            let r = f(&ev, &v);
            if r.is_object() { r } else { json!({}) }
        };
        if writeln!(out, "{}", reply).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
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
    let mut tty = Tty::open().ok()?;
    let mut query = String::new();
    let mut cur = 0usize;
    let rows = 12;
    let r = loop {
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();
        let hits: Vec<&String> = items
            .iter()
            .filter(|it| {
                let low = it.to_lowercase();
                words.iter().all(|w| low.contains(w.as_str()))
            })
            .collect();
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
