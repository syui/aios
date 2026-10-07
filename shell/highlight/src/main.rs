// aish-highlight: 打っている行に色をつける (zsh-syntax-highlighting の main と同じ既定の色)
//   あるコマンド (組み込み、alias、関数、PATH にあるもの) は緑、ないものは太い赤、sudo などの前置きは緑の下線、
//   予約語 (if for ...) は黄、文字列は黄 ("..." の中の $変数 はシアン)、あるファイルは下線、* ? は青、コメントは灰。
//   aish は行が変わるたびに highlight で聞く。答えは (始め, 終わり, SGR) の並び (文字の番号)
use aish_plugin::{Spec, Value, json, s};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// 調べた答えを覚えておく長さ (打つたびに PATH のディレクトリを全部 stat しないように)
const KEEP: Duration = Duration::from_secs(5);

thread_local! {
    /// (PATH, 名前) → (コマンドがあるか, 調べた時刻)、(pwd, 語) → (ファイルがあるか, 時刻)
    static CMDS: RefCell<HashMap<(String, String), (bool, Instant)>> = RefCell::new(HashMap::new());
    static FILES: RefCell<HashMap<(String, String), (bool, Instant)>> = RefCell::new(HashMap::new());
}

/// map に新しい答えがあればそれを、なければ f で調べて覚える
fn cached(map: &'static std::thread::LocalKey<RefCell<HashMap<(String, String), (bool, Instant)>>>, key: (String, String), f: impl FnOnce() -> bool) -> bool {
    if let Some(v) = map.with(|m| m.borrow().get(&key).filter(|(_, t)| t.elapsed() < KEEP).map(|(v, _)| *v)) {
        return v;
    }
    let v = f();
    map.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(key, (v, Instant::now()));
    });
    v
}

const RESERVED: &[&str] = &["if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until", "do", "done", "in", "function", "time", "{", "}", "!", "[[", "]]", "coproc"];
/// このあとの語もコマンドの場所
const PRECMD: &[&str] = &["sudo", "exec", "command", "builtin", "nice", "nohup", "env", "time", "aibox", "doas", "strace", "timeout"];
/// 予約語のうち、このあとがコマンドの場所になるもの
const OPENS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "{", "!", "time"];

const GREEN: &str = "32";
const RED: &str = "1;31";
const PRE: &str = "32;4";
const YELLOW: &str = "33";
const CYAN: &str = "36";
const UNDER: &str = "4";
const BLUE: &str = "34";
const GRAY: &str = "90";

struct Ctx<'a> {
    cmds: Vec<&'a str>,
    path: &'a str,
    pwd: &'a str,
    home: &'a str,
}

impl Ctx<'_> {
    fn expand(&self, w: &str) -> String {
        match w.strip_prefix('~') {
            Some(r) if r.is_empty() || r.starts_with('/') => format!("{}{}", self.home, r),
            _ => w.to_string(),
        }
    }

    fn file(&self, w: &str) -> std::path::PathBuf {
        let w = self.expand(w);
        if w.starts_with('/') { w.into() } else { Path::new(self.pwd).join(w) }
    }

    /// コマンドとして動かせるか
    fn is_cmd(&self, w: &str) -> bool {
        if self.cmds.contains(&w) {
            return true;
        }
        let key = (format!("{}\0{}", self.path, if w.contains('/') { self.pwd } else { "" }), w.to_string());
        cached(&CMDS, key, || {
            if w.contains('/') {
                return executable(&self.file(w));
            }
            self.path.split(':').filter(|d| !d.is_empty()).any(|d| executable(&Path::new(d).join(w)))
        })
    }
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// 行を語に分けて色をつける
fn highlight(line: &str, c: &Ctx) -> Vec<(usize, usize, &'static str)> {
    let ch: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut cmd_pos = true;
    while i < ch.len() {
        let c0 = ch[i];
        if c0.is_whitespace() {
            i += 1;
            continue;
        }
        // 区切り: | || & && ; ( ) は、そのあとがコマンドの場所
        if matches!(c0, '|' | '&' | ';' | '(' | ')') {
            i += 1;
            while i < ch.len() && matches!(ch[i], '|' | '&' | ';') {
                i += 1;
            }
            cmd_pos = c0 != ')';
            continue;
        }
        // リダイレクト (> >> < 2> &>) とそのあとの語はファイル
        if matches!(c0, '<' | '>') {
            i += 1;
            while i < ch.len() && matches!(ch[i], '<' | '>' | '&' | '|') {
                i += 1;
            }
            continue;
        }
        if c0 == '#' {
            out.push((i, ch.len(), GRAY));
            break;
        }
        // 1 語: 引用符の中は区切らない
        let start = i;
        let mut word = String::new();
        let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
        let mut quoted = false;
        while i < ch.len() && !ch[i].is_whitespace() && !matches!(ch[i], '|' | '&' | ';' | '<' | '>' | '(' | ')') {
            match ch[i] {
                '\\' => {
                    if i + 1 < ch.len() {
                        word.push(ch[i + 1]);
                    }
                    i += 2;
                }
                q @ ('\'' | '"') => {
                    quoted = true;
                    let qs = i;
                    i += 1;
                    while i < ch.len() && ch[i] != q {
                        if q == '"' && ch[i] == '$' {
                            let vs = i;
                            i += 1;
                            if i < ch.len() && ch[i] == '{' {
                                while i < ch.len() && ch[i] != '}' && ch[i] != '"' {
                                    i += 1;
                                }
                                i = (i + 1).min(ch.len());
                            } else {
                                while i < ch.len() && (ch[i].is_alphanumeric() || ch[i] == '_') {
                                    i += 1;
                                }
                            }
                            spans.push((vs, i, CYAN));
                            continue;
                        }
                        if q == '"' && ch[i] == '\\' {
                            i += 1;
                        }
                        if i < ch.len() {
                            word.push(ch[i]);
                        }
                        i += 1;
                    }
                    i = (i + 1).min(ch.len());
                    // 文字列は黄 ($変数のところはシアンのまま。あとから入れたものが先)
                    spans.push((qs, i, YELLOW));
                }
                g @ ('*' | '?') => {
                    spans.push((i, i + 1, BLUE));
                    word.push(g);
                    i += 1;
                }
                x => {
                    word.push(x);
                    i += 1;
                }
            }
        }
        let end = i;
        if cmd_pos {
            // VAR=x はコマンドの前の代入
            if !quoted && word.find('=').is_some_and(|k| k > 0 && word[..k].chars().all(|x| x.is_alphanumeric() || x == '_')) {
                continue;
            }
            if !quoted && RESERVED.contains(&word.as_str()) {
                out.push((start, end, YELLOW));
                cmd_pos = OPENS.contains(&word.as_str());
                continue;
            }
            if !quoted && PRECMD.contains(&word.as_str()) && c.is_cmd(&word) {
                out.push((start, end, PRE));
                continue;
            }
            out.push((start, end, if c.is_cmd(&word) { GREEN } else { RED }));
            cmd_pos = false;
            continue;
        }
        // 引数: あるファイルなら下線。文字列と * ? の色はそのうえに
        if !word.is_empty() && !word.starts_with('-') && !word.contains(['*', '?']) && cached(&FILES, (c.pwd.to_string(), word.clone()), || c.file(&word).symlink_metadata().is_ok()) {
            out.push((start, end, UNDER));
        }
        // 引用符の中の $変数 は、文字列 (黄) のあとに置いて上書きする
        spans.sort_by_key(|s| (s.2 == CYAN) as u8);
        out.extend(spans);
    }
    out
}

/// 重なったところは、あとのものを優先して、重ならない並びに直す (aish は始めと終わりで色を切りかえる)
fn flatten(len: usize, spans: &[(usize, usize, &'static str)]) -> Vec<Value> {
    let mut color: Vec<Option<&str>> = vec![None; len];
    for &(a, b, sgr) in spans {
        for c in color.iter_mut().take(b.min(len)).skip(a) {
            *c = Some(sgr);
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < len {
        let Some(sgr) = color[i] else {
            i += 1;
            continue;
        };
        let a = i;
        while i < len && color[i] == Some(sgr) {
            i += 1;
        }
        out.push(json!([a, i, sgr]));
    }
    out
}

fn main() {
    let spec = Spec { name: "highlight", hooks: &["highlight"], keys: &[], tools: &[] };
    aish_plugin::run(spec, |ev, v| {
        if ev != "highlight" {
            return json!({});
        }
        let line = s(v, "line");
        let cmds: Vec<&str> = v["cmds"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let c = Ctx { cmds, path: s(v, "path"), pwd: s(v, "pwd"), home: s(v, "home") };
        let spans = highlight(line, &c);
        json!({ "spans": flatten(line.chars().count(), &spans) })
    });
}
