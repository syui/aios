// 対話するときの行の編集 (emacs のようなキー) と履歴。ここにあるのは土台だけで、
// 補完の候補、グレーの候補、キーに結んだ機能はプラグイン (plugin.rs) が出す
//   ← → C-b             1 文字うごく (→ と End は、行の終わりならグレーの候補を決める)
//   M-b M-f             1 語うごく       C-a Home / C-e End   行の頭 / 終わり
//   C-h BS / C-d Del    消す (空の行で C-d は終わり)
//   C-u                 頭まで消す       C-k   終わりまで消す
//   C-w M-BS            前の 1 語を消す  C-l   画面を消す
//   ↑ ↓                 打ちかけの文字をふくむ履歴をさかのぼる / もどる (zsh の history-substring-search)
//   C-p C-n             履歴をさかのぼる / もどる
//   Tab                 補完 (候補は complete のプラグイン)。決まらなければ候補を出し、
//                       もう一度 Tab で選ぶ (Tab / S-Tab / 矢印、Enter で決める)
//   C-c                 打ちかけの行を捨てる    Enter 決める
// bindkey で結んだキーは、上のものより先にプラグインの機能を呼ぶ ("C-p C-p" のように 2 つ続けても)。
// 打っている行に続くもの (suggest のプラグイン) は、グレーで出す。
// 履歴は $HISTFILE (なければ ~/.aish_history) に、打つたびに足す。$HISTSIZE 行まで (既定 10000)。
// 前と同じ行は足さない。プロンプトの中の ESC [ ... m (色) と \x01 \x02 で囲んだところは幅に数えない
use super::plugin::Plugins;
use serde_json::json;
use std::io::Write;

pub enum Input {
    Line(String),
    /// 履歴に残さずに動かすもの (プラグインの機能の run と silent)
    Silent(String),
    Eof,
    Interrupt,
}

/// 補完などのためにプラグインへ渡す、シェルの様子
pub struct Ctx {
    /// 組み込み、alias、関数の名前
    pub cmds: Vec<String>,
    /// シェルの変数の名前
    pub vars: Vec<String>,
    pub home: String,
    pub path: String,
    pub pwd: String,
}

pub struct Editor {
    pub history: Vec<String>,
    pub file: Option<String>,
    size: usize,
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

/// 見える幅 (色の ESC [ ... 文字 と、\x01 ... \x02 は数えない)
pub fn str_width(s: &str) -> usize {
    let mut w = 0;
    let mut cs = s.chars().peekable();
    let mut hidden = false;
    while let Some(c) = cs.next() {
        match c {
            '\x01' => hidden = true,
            '\x02' => hidden = false,
            '\x1b' => {
                if cs.peek() == Some(&'[') {
                    cs.next();
                    for d in cs.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                } else {
                    cs.next();
                }
            }
            _ if hidden => {}
            _ => w += char_width(c),
        }
    }
    w
}

/// 見える幅 w までに切る (色の並びはそのまま)
fn clip(s: &str, w: usize) -> String {
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

fn term_cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

fn read_byte() -> Option<u8> {
    let mut c = 0u8;
    loop {
        let n = unsafe { libc::read(0, &mut c as *mut u8 as *mut libc::c_void, 1) };
        if n == 1 {
            return Some(c);
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
}

/// もう届いている入力があるか
fn input_ready() -> bool {
    let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut p, 1, 0) > 0 }
}

/// ms だけ待って 1 バイト (ESC のあとに続くか、C-p C-p か)
fn read_byte_within(ms: i32) -> Option<u8> {
    let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut p, 1, ms) } <= 0 {
        return None;
    }
    read_byte()
}

/// UTF-8 の 1 文字の残りを読む
fn read_char(first: u8) -> String {
    let n = if first >= 0xf0 { 3 } else if first >= 0xe0 { 2 } else if first >= 0xc0 { 1 } else { 0 };
    let mut bytes = vec![first];
    for _ in 0..n {
        if let Some(d) = read_byte() {
            bytes.push(d);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// キー (ESC で始まる並びはまとめて 1 つ)
enum Key {
    Byte(u8),
    Text(String),
    Csi(Vec<u8>),
    Alt(u8),
    Esc,
}

fn read_key() -> Option<Key> {
    let c = read_byte()?;
    Some(match c {
        0x1b => match read_byte_within(50) {
            Some(b'[') | Some(b'O') => {
                let mut seq = Vec::new();
                while let Some(d) = read_byte_within(50) {
                    seq.push(d);
                    if (0x40..=0x7e).contains(&d) {
                        break;
                    }
                }
                Key::Csi(seq)
            }
            Some(d) => Key::Alt(d),
            None => Key::Esc,
        },
        c if c >= 0x80 => Key::Text(read_char(c)),
        c => Key::Byte(c),
    })
}

/// ms だけ待ってキーを 1 つ (2 つ続けるキーの 2 つ目)
fn read_key_within(ms: i32) -> Option<Key> {
    let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut p, 1, ms) } <= 0 {
        return None;
    }
    read_key()
}

/// キーの名前 (bindkey の書き方): "C-r" "M-f" "Tab" "Enter" "Esc" "BS" ...。名前のないものは None
fn key_name(k: &Key) -> Option<String> {
    Some(match k {
        Key::Byte(0x09) => "Tab".into(),
        Key::Byte(0x0d) => "Enter".into(),
        Key::Byte(0x7f) => "BS".into(),
        Key::Byte(0) => "C-@".into(),
        Key::Byte(c) if *c < 0x20 => format!("C-{}", (c + 0x60) as char),
        Key::Alt(c) if *c >= 0x20 && *c < 0x7f => format!("M-{}", *c as char),
        Key::Esc => "Esc".into(),
        _ => return None,
    })
}

/// 端末を 1 文字ずつ、エコーなしに (戻すときの設定を返す)
fn raw_mode() -> Option<libc::termios> {
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut old) } != 0 {
        return None;
    }
    let mut raw = old;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw.c_iflag &= !(libc::ICRNL | libc::IXON);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &raw) };
    Some(old)
}

fn print_flush(s: &str) {
    let mut e = std::io::stderr();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

/// 編集中の行
struct Line {
    buf: Vec<char>,
    pos: usize,
    /// 前に描いたときの、カーソルの行 (プロンプトの頭から)
    row: usize,
    /// 前に描いたときの、最後の行 (プロンプトの頭から。下に出したものはのぞく)
    end_row: usize,
}

impl Line {
    fn text(&self) -> String {
        self.buf.iter().collect()
    }

    fn set(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.pos = self.buf.len();
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            self.buf.insert(self.pos, c);
            self.pos += 1;
        }
    }
}

/// 補完の候補
#[derive(Clone)]
struct Cand {
    /// 語と置きかえるもの
    text: String,
    /// 一覧に出すもの
    show: String,
    /// ディレクトリ (決めたときに空白を足さない)
    dir: bool,
}

/// Tab の様子
enum Menu {
    None,
    /// 候補を出した (次の Tab で選びはじめる): (語の頭, 候補)
    Listed(usize, Vec<Cand>),
    /// 選んでいる: (語の頭, 候補, 選んでいる番号, 選ぶ前の行)
    Select(usize, Vec<Cand>, usize, Vec<char>),
}

impl Editor {
    pub fn new() -> Editor {
        Editor { history: Vec::new(), file: None, size: 10000 }
    }

    /// 履歴のファイルを読む
    pub fn load(&mut self, file: Option<String>, size: usize) {
        self.size = size.max(1);
        if let Some(f) = &file
            && let Ok(t) = std::fs::read_to_string(f)
        {
            self.history = t.lines().filter(|l| !l.is_empty()).map(String::from).collect();
            let n = self.history.len();
            if n > self.size {
                self.history.drain(..n - self.size);
            }
        }
        self.file = file;
    }

    /// 履歴に足す (空の行と、前と同じ行は足さない)
    pub fn add(&mut self, line: &str) {
        let line = line.trim_end_matches('\n');
        if line.trim().is_empty() || self.history.last().is_some_and(|l| l == line) || line.contains('\n') {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > self.size {
            self.history.remove(0);
        }
        if let Some(f) = &self.file
            && let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f)
        {
            let _ = writeln!(fh, "{}", line);
        }
    }

    /// プロンプトを出して 1 行読む
    pub fn read(&mut self, prompt: &str, ctx: &Ctx, pl: &mut Plugins) -> Input {
        let Some(old) = raw_mode() else {
            // 端末でなければ、ふつうに 1 行
            return match super::read_line() {
                Ok(Some(l)) => Input::Line(l),
                Ok(None) => Input::Eof,
                Err(_) => Input::Interrupt,
            };
        };
        let r = self.edit(prompt, ctx, pl);
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &old) };
        r
    }

    /// 打っている行に続くもの (グレーで出す。suggest のプラグイン)
    fn suggestion(&self, l: &Line, pl: &mut Plugins) -> Option<String> {
        if l.buf.is_empty() || l.pos != l.buf.len() || !pl.wants("suggest") {
            return None;
        }
        let r = pl.ask("suggest", json!({ "line": l.text() }))?;
        r["suggest"].as_str().filter(|s| !s.is_empty() && !s.contains('\n')).map(String::from)
    }

    /// キーに結んだ機能を呼ぶ。端末はプラグインに渡す (打ちかけの行の下の行の頭にカーソルを置いて)。
    /// 行を決めて返すもの (run) があれば Some
    fn call_widget(&mut self, prompt: &str, l: &mut Line, ctx: &Ctx, pl: &mut Plugins, b: usize) -> Option<Input> {
        self.draw(prompt, l, None, &[]);
        let down = l.end_row.saturating_sub(l.row);
        let mut out = String::new();
        if down > 0 {
            out.push_str(&format!("\x1b[{}B", down));
        }
        out.push_str("\r\n");
        print_flush(&out);
        l.row = l.end_row + 1;
        let ev = json!({
            "line": l.text(),
            "pos": l.pos,
            "pwd": ctx.pwd,
            "home": ctx.home,
            "histfile": self.file.clone().unwrap_or_default(),
        });
        let r = pl.key(b, ev).unwrap_or(json!({}));
        if let Some(t) = r["line"].as_str() {
            l.set(t);
            if let Some(p) = r["pos"].as_u64() {
                l.pos = (p as usize).min(l.buf.len());
            }
        }
        if let Some(t) = r["insert"].as_str() {
            l.insert(t);
        }
        if let Some(cmd) = r["run"].as_str() {
            self.draw(prompt, l, None, &[]);
            print_flush("\r\n");
            let cmd = format!("{}\n", cmd.trim_end_matches('\n'));
            return Some(if r["silent"].as_bool().unwrap_or(false) { Input::Silent(cmd) } else { Input::Line(cmd) });
        }
        if r["accept"].as_bool().unwrap_or(false) {
            l.pos = l.buf.len();
            self.draw(prompt, l, None, &[]);
            print_flush("\r\n");
            return Some(Input::Line(l.text() + "\n"));
        }
        None
    }

    fn edit(&mut self, prompt: &str, ctx: &Ctx, pl: &mut Plugins) -> Input {
        let mut l = Line { buf: Vec::new(), pos: 0, row: 0, end_row: 0 };
        // 履歴を見ている位置 (history.len() は打ちかけの行) と、さかのぼる前の打ちかけ
        let mut hi = self.history.len();
        let mut saved: Vec<char> = Vec::new();
        let mut search: Option<String> = None;
        let mut menu = Menu::None;
        // 2 つ続けるキーで、1 つ目のあとに来たちがうキー (次にふつうに読む)
        let mut pending: Option<Key> = None;
        // 先打ち: 編集を始める前に届いていた入力 (コマンドが動いている間に打ったものや、貼りつけ)。
        // その間の端末はふつうのモードで Enter (\r) が \n になっているので、\n も Enter とする (C-j ではなく)
        let mut typeahead = input_ready();
        self.draw(prompt, &mut l, None, &[]);
        loop {
            if typeahead && pending.is_none() && !input_ready() {
                typeahead = false;
            }
            let Some(mut key) = pending.take().or_else(read_key) else { return Input::Eof };
            if typeahead && matches!(key, Key::Byte(b'\n')) {
                key = Key::Byte(b'\r');
            }
            // 補完の候補を選んでいるとき: 動かすキーか、決めてから続けるか
            if let Menu::Select(start, cands, i, before) = &mut menu {
                let n = cands.len();
                let mv = match &key {
                    Key::Byte(0x09) | Key::Byte(0x0e) => Some((*i + 1) % n),
                    Key::Csi(s) if s == b"B" || s == b"C" => Some((*i + 1) % n),
                    Key::Byte(0x10) => Some((*i + n - 1) % n),
                    Key::Csi(s) if s == b"Z" || s == b"A" || s == b"D" => Some((*i + n - 1) % n),
                    _ => None,
                };
                if let Some(j) = mv {
                    *i = j;
                    apply_cand(&mut l, *start, &cands[j], before);
                    let below = menu_lines(cands, Some(j));
                    self.draw(prompt, &mut l, None, &below);
                    continue;
                }
                let cancel = matches!(key, Key::Esc | Key::Byte(0x07));
                if cancel {
                    // 選ぶ前の行へ (カーソルは語の終わり)
                    l.buf = before.clone();
                    l.pos = *start;
                    while l.pos < l.buf.len() && !is_break(&l.buf, l.pos) {
                        l.pos += 1;
                    }
                } else if !cands[*i].dir {
                    l.insert(" ");
                }
                let done = cancel || matches!(key, Key::Byte(b'\r'));
                menu = Menu::None;
                if done {
                    self.draw(prompt, &mut l, None, &[]);
                    continue;
                }
                // ほかのキーは、決めてからそのまま続ける (空白を足したので、空白のキーはのぞく)
                if matches!(key, Key::Byte(b' ')) {
                    key = Key::Byte(0xff);
                }
            }
            // bindkey で結んだキー: プラグインの機能 (2 つ続けるものは、1 つ目のあと少し待つ)
            if let Some(name) = key_name(&key) {
                let two = pl.binds.iter().any(|b| b.keys.len() == 2 && b.keys[0] == name);
                let mut hit = None;
                if two {
                    match read_key_within(400) {
                        Some(k2) => {
                            let n2 = key_name(&k2);
                            hit = pl.binds.iter().position(|b| b.keys.len() == 2 && b.keys[0] == name && Some(&b.keys[1]) == n2.as_ref());
                            if hit.is_none() {
                                pending = Some(k2);
                            }
                        }
                        None => {}
                    }
                }
                if hit.is_none() && pending.is_none() {
                    hit = pl.binds.iter().position(|b| b.keys.len() == 1 && b.keys[0] == name);
                }
                if let Some(b) = hit {
                    menu = Menu::None;
                    if let Some(done) = self.call_widget(prompt, &mut l, ctx, pl, b) {
                        return done;
                    }
                    search = None;
                    hi = self.history.len();
                    let sug = self.suggestion(&l, pl);
                    self.draw(prompt, &mut l, sug.as_deref(), &[]);
                    continue;
                }
            }
            let mut nav = false;
            match key {
                Key::Byte(b'\r') => {
                    l.pos = l.buf.len();
                    self.draw(prompt, &mut l, None, &[]);
                    print_flush("\r\n");
                    return Input::Line(l.text() + "\n");
                }
                Key::Byte(0x03) => {
                    l.pos = l.buf.len();
                    self.draw(prompt, &mut l, None, &[]);
                    print_flush("^C\r\n");
                    return Input::Interrupt;
                }
                Key::Byte(0x04) => {
                    if l.buf.is_empty() {
                        print_flush("\r\n");
                        return Input::Eof;
                    }
                    if l.pos < l.buf.len() {
                        l.buf.remove(l.pos);
                    }
                }
                Key::Byte(0x01) => l.pos = 0,
                Key::Byte(0x05) => self.end_or_accept(&mut l, pl),
                Key::Byte(0x02) => l.pos = l.pos.saturating_sub(1),
                Key::Byte(0x08) | Key::Byte(0x7f) => {
                    if l.pos > 0 {
                        l.pos -= 1;
                        l.buf.remove(l.pos);
                    }
                }
                Key::Byte(0x0b) => l.buf.truncate(l.pos),
                Key::Byte(0x15) => {
                    l.buf.drain(..l.pos);
                    l.pos = 0;
                }
                Key::Byte(0x17) | Key::Alt(0x7f) | Key::Alt(0x08) => kill_word_back(&mut l),
                Key::Byte(0x0c) => {
                    print_flush("\x1b[H\x1b[2J");
                    l.row = 0;
                }
                Key::Byte(0x10) => {
                    self.step(&mut l, &mut hi, &mut saved, None, true);
                    nav = true;
                }
                Key::Byte(0x0e) => {
                    self.step(&mut l, &mut hi, &mut saved, None, false);
                    nav = true;
                }
                Key::Byte(0x09) => {
                    menu = match std::mem::replace(&mut menu, Menu::None) {
                        Menu::Listed(start, cands) => {
                            let before = l.buf.clone();
                            apply_cand(&mut l, start, &cands[0], &before);
                            let below = menu_lines(&cands, Some(0));
                            self.draw(prompt, &mut l, None, &below);
                            Menu::Select(start, cands, 0, before)
                        }
                        _ => self.tab(&mut l, ctx, pl),
                    };
                    if let Menu::Select(..) = menu {
                        continue;
                    }
                    let below = match &menu {
                        Menu::Listed(_, c) => menu_lines(c, None),
                        _ => Vec::new(),
                    };
                    let sug = self.suggestion(&l, pl);
                    self.draw(prompt, &mut l, sug.as_deref(), &below);
                    continue;
                }
                Key::Csi(s) => match s.as_slice() {
                    b"A" | b"B" => {
                        // ↑ ↓: 打ちかけの文字をふくむ履歴
                        if hi == self.history.len() {
                            search = Some(l.text());
                        }
                        let q = search.clone().unwrap_or_default();
                        self.step(&mut l, &mut hi, &mut saved, Some(&q), s == b"A");
                        nav = true;
                    }
                    b"C" => {
                        if l.pos == l.buf.len() {
                            self.end_or_accept(&mut l, pl);
                        } else {
                            l.pos += 1;
                        }
                    }
                    b"D" => l.pos = l.pos.saturating_sub(1),
                    b"H" | b"1~" | b"7~" => l.pos = 0,
                    b"F" | b"4~" | b"8~" => self.end_or_accept(&mut l, pl),
                    b"3~" => {
                        if l.pos < l.buf.len() {
                            l.buf.remove(l.pos);
                        }
                    }
                    _ => continue,
                },
                Key::Alt(b'b') => l.pos = word_left(&l.buf, l.pos),
                Key::Alt(b'f') => l.pos = word_right(&l.buf, l.pos),
                Key::Text(t) => l.insert(&t),
                Key::Byte(0xff) => {}
                Key::Byte(c) if (0x20..0x7f).contains(&c) => l.insert(&(c as char).to_string()),
                _ => continue,
            }
            if !nav {
                search = None;
                hi = self.history.len();
            }
            menu = Menu::None;
            let sug = self.suggestion(&l, pl);
            self.draw(prompt, &mut l, sug.as_deref(), &[]);
        }
    }

    /// End / C-e / →: 行の終わりへ。終わりにいればグレーの候補を決める
    fn end_or_accept(&self, l: &mut Line, pl: &mut Plugins) {
        if l.pos == l.buf.len()
            && let Some(s) = self.suggestion(l, pl)
        {
            l.insert(&s);
        }
        l.pos = l.buf.len();
    }

    /// Tab: 補完する。決まらなければ候補を出す
    fn tab(&self, l: &mut Line, ctx: &Ctx, pl: &mut Plugins) -> Menu {
        let (start, cands) = complete(l, ctx, pl);
        let word: String = l.buf[start..l.pos].iter().collect();
        match cands.len() {
            0 => Menu::None,
            1 => {
                let before = l.buf.clone();
                apply_cand(l, start, &cands[0], &before);
                if !cands[0].dir {
                    l.insert(" ");
                }
                Menu::None
            }
            _ => {
                // みんなに共通する頭まで進める (大文字小文字は区別しない)
                let first: Vec<char> = cands[0].text.chars().collect();
                let mut n = first.len();
                for c in &cands[1..] {
                    let o: Vec<char> = c.text.chars().collect();
                    n = n.min(first.iter().zip(o.iter()).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count());
                }
                if n > word.chars().count() {
                    let common: String = first[..n].iter().collect();
                    let before = l.buf.clone();
                    apply_cand(l, start, &Cand { text: common, show: String::new(), dir: true }, &before);
                    return Menu::None;
                }
                Menu::Listed(start, cands)
            }
        }
    }

    /// 履歴を 1 つ動く。q があれば、それをふくむものだけ (大文字小文字は区別しない)
    fn step(&self, l: &mut Line, hi: &mut usize, saved: &mut Vec<char>, q: Option<&str>, up: bool) {
        let n = self.history.len();
        if *hi == n {
            *saved = l.buf.clone();
        }
        let q = q.map(|s| s.to_lowercase());
        let ok = |s: &String| q.as_ref().is_none_or(|q| s.to_lowercase().contains(q.as_str()));
        let mut i = *hi;
        loop {
            if up {
                if i == 0 {
                    return;
                }
                i -= 1;
            } else {
                i += 1;
                if i >= n {
                    *hi = n;
                    l.buf = saved.clone();
                    l.pos = l.buf.len();
                    return;
                }
            }
            // 今と同じものは飛ばす
            if ok(&self.history[i]) && self.history[i].chars().collect::<Vec<_>>() != l.buf {
                break;
            }
        }
        *hi = i;
        l.set(&self.history[i].clone());
    }

    /// プロンプトと行 (とグレーの候補、下に出す行) を描きなおして、カーソルを pos へ
    fn draw(&self, prompt: &str, l: &mut Line, sug: Option<&str>, below: &[String]) {
        let cols = term_cols();
        let mut out = String::new();
        // 前に描いた頭へ
        if l.row > 0 {
            out.push_str(&format!("\x1b[{}A", l.row));
        }
        out.push_str("\r\x1b[J");
        out.extend(prompt.chars().filter(|&c| c != '\x01' && c != '\x02'));
        // プロンプトが何行かあれば、最後の行の幅から
        let pn = prompt.matches('\n').count();
        let pw = str_width(prompt.rsplit('\n').next().unwrap_or(prompt));
        out.push_str(&l.text());
        let mut total = pw + l.buf.iter().map(|&c| char_width(c)).sum::<usize>();
        if let Some(s) = sug {
            out.push_str("\x1b[90m");
            out.push_str(s);
            out.push_str("\x1b[0m");
            total += str_width(s);
        }
        let at = pw + l.buf[..l.pos].iter().map(|&c| char_width(c)).sum::<usize>();
        // 行の終わりちょうどで折り返したら、カーソルを次の行の頭へ
        if total > 0 && total % cols == 0 {
            out.push_str("\r\n");
        }
        let mut cur_row = total / cols;
        l.end_row = pn + cur_row;
        for b in below {
            out.push_str("\r\n");
            out.push_str(&clip(b, cols.saturating_sub(1)));
            out.push_str("\x1b[0m");
            cur_row += 1;
        }
        let (row, col) = (at / cols, at % cols);
        if cur_row > row {
            out.push_str(&format!("\x1b[{}A", cur_row - row));
        }
        out.push('\r');
        if col > 0 {
            out.push_str(&format!("\x1b[{}C", col));
        }
        l.row = pn + row;
        print_flush(&out);
    }
}

/// 候補を語と置きかえる: before (置きかえる前の行) の start から語の終わりまでを c.text に
fn apply_cand(l: &mut Line, start: usize, c: &Cand, before: &[char]) {
    let mut end = start;
    while end < before.len() && !is_break(before, end) {
        end += 1;
    }
    let mut buf: Vec<char> = before[..start].to_vec();
    buf.extend(c.text.chars());
    let pos = buf.len();
    buf.extend_from_slice(&before[end..]);
    l.buf = buf;
    l.pos = pos;
}

/// 候補の一覧 (列にならべる。sel は選んでいるもの)
fn menu_lines(cands: &[Cand], sel: Option<usize>) -> Vec<String> {
    let cols = term_cols();
    let w = cands.iter().map(|c| str_width(&c.show)).max().unwrap_or(1) + 2;
    let ncol = (cols.saturating_sub(1) / w).max(1);
    let nrow = cands.len().div_ceil(ncol);
    // 多すぎれば、選んでいるところのまわりの 10 行だけ
    let max_rows = 10;
    let sel_row = sel.map_or(0, |s| s % nrow);
    let top = if nrow > max_rows { sel_row.saturating_sub(max_rows - 1) } else { 0 };
    let end = nrow.min(top + max_rows);
    let mut out = Vec::new();
    for r in top..end {
        let mut line = String::new();
        for c in 0..ncol {
            let k = c * nrow + r;
            let Some(cand) = cands.get(k) else { break };
            let pad = w - str_width(&cand.show);
            if Some(k) == sel {
                line.push_str(&format!("\x1b[7m{}\x1b[0m{}", cand.show, " ".repeat(pad)));
            } else if cand.dir {
                line.push_str(&format!("\x1b[34m{}\x1b[0m{}", cand.show, " ".repeat(pad)));
            } else {
                line.push_str(&format!("{}{}", cand.show, " ".repeat(pad)));
            }
        }
        out.push(line);
    }
    if nrow > max_rows {
        out.push(format!("\x1b[90m({} rows, {} candidates)\x1b[0m", nrow, cands.len()));
    }
    out
}

/// 語の区切り (空白、; | & < > ( )。\ で消したものはのぞく)
fn is_break(b: &[char], i: usize) -> bool {
    " \t;|&<>()".contains(b[i]) && !(i > 0 && b[i - 1] == '\\')
}

/// 補完: (置きかえる語の頭, 候補)。候補は complete のプラグインが出す
fn complete(l: &Line, ctx: &Ctx, pl: &mut Plugins) -> (usize, Vec<Cand>) {
    if !pl.wants("complete") {
        return (l.pos, Vec::new());
    }
    let ev = json!({ "line": l.text(), "pos": l.pos, "cmds": ctx.cmds, "vars": ctx.vars, "path": ctx.path, "home": ctx.home, "pwd": ctx.pwd });
    let Some(r) = pl.ask("complete", ev) else { return (l.pos, Vec::new()) };
    let start = (r["start"].as_u64().unwrap_or(l.pos as u64) as usize).min(l.pos);
    let cands = r["cands"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let text = c["text"].as_str()?.to_string();
                    let show = c["show"].as_str().map_or_else(|| text.clone(), String::from);
                    Some(Cand { text, show, dir: c["dir"].as_bool().unwrap_or(false) })
                })
                .collect()
        })
        .unwrap_or_default();
    (start, cands)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_left(b: &[char], mut p: usize) -> usize {
    while p > 0 && !is_word(b[p - 1]) {
        p -= 1;
    }
    while p > 0 && is_word(b[p - 1]) {
        p -= 1;
    }
    p
}

fn word_right(b: &[char], mut p: usize) -> usize {
    while p < b.len() && !is_word(b[p]) {
        p += 1;
    }
    while p < b.len() && is_word(b[p]) {
        p += 1;
    }
    p
}

/// C-w: 前の 1 語 (空白で区切る) を消す
fn kill_word_back(l: &mut Line) {
    let mut p = l.pos;
    while p > 0 && l.buf[p - 1] == ' ' {
        p -= 1;
    }
    while p > 0 && l.buf[p - 1] != ' ' {
        p -= 1;
    }
    l.buf.drain(p..l.pos);
    l.pos = p;
}
