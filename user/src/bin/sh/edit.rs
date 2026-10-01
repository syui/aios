// 対話するときの行の編集 (emacs のようなキー) と履歴
//   ← → C-b C-f      1 文字うごく          M-b M-f        1 語うごく
//   C-a Home C-e End 行の頭 / 終わり      C-h BS / C-d Del   消す (空の行で C-d は終わり)
//   C-u C-k          頭まで / 終わりまで消す   C-w M-BS       前の 1 語を消す
//   ↑ ↓              打ちかけの文字をふくむ履歴をさかのぼる / もどる (zsh の history-substring-search)
//   C-p C-n          履歴をさかのぼる / もどる   C-l            画面を消す
//   C-c              打ちかけの行を捨てる   Enter          決める
// 履歴は $HISTFILE (なければ ~/.sh_history) に、打つたびに足す。$HISTSIZE 行まで (既定 10000)。
// 前と同じ行は足さない。プロンプトの中の ESC [ ... m (色) と \x01 \x02 で囲んだところは幅に数えない
use std::io::Write;

pub enum Input {
    Line(String),
    Eof,
    Interrupt,
}

pub struct Editor {
    pub history: Vec<String>,
    file: Option<String>,
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

/// 少しだけ待って 1 バイト (ESC のあとに続くか)
fn read_byte_soon() -> Option<u8> {
    let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    if unsafe { libc::poll(&mut p, 1, 50) } <= 0 {
        return None;
    }
    read_byte()
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

/// 編集中の行
struct Line {
    buf: Vec<char>,
    pos: usize,
    /// 前に描いたときの、カーソルの行 (プロンプトの頭から)
    row: usize,
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
    pub fn read(&mut self, prompt: &str) -> Input {
        let Some(old) = raw_mode() else {
            // 端末でなければ、ふつうに 1 行
            return match super::read_line() {
                Ok(Some(l)) => Input::Line(l),
                Ok(None) => Input::Eof,
                Err(_) => Input::Interrupt,
            };
        };
        let r = self.edit(prompt);
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &old) };
        r
    }

    fn edit(&mut self, prompt: &str) -> Input {
        let mut l = Line { buf: Vec::new(), pos: 0, row: 0 };
        // 履歴を見ている位置 (history.len() は打ちかけの行) と、さかのぼる前の打ちかけ
        let mut hi = self.history.len();
        let mut saved: Vec<char> = Vec::new();
        let mut search: Option<String> = None;
        let mut out = String::new();
        self.draw(prompt, &mut l, &mut out);
        loop {
            let Some(c) = read_byte() else { return Input::Eof };
            let mut redraw = true;
            // 履歴をたどるキーか (ほかのキーなら、たどるのをやめる)
            let mut nav = false;
            match c {
                b'\r' | b'\n' => {
                    l.pos = l.buf.len();
                    self.draw(prompt, &mut l, &mut out);
                    print_flush("\r\n");
                    let s: String = l.buf.iter().collect();
                    return Input::Line(s + "\n");
                }
                0x03 => {
                    l.pos = l.buf.len();
                    self.draw(prompt, &mut l, &mut out);
                    print_flush("^C\r\n");
                    return Input::Interrupt;
                }
                0x04 => {
                    if l.buf.is_empty() {
                        print_flush("\r\n");
                        return Input::Eof;
                    }
                    if l.pos < l.buf.len() {
                        l.buf.remove(l.pos);
                    }
                }
                0x01 => l.pos = 0,
                0x05 => l.pos = l.buf.len(),
                0x02 => l.pos = l.pos.saturating_sub(1),
                0x06 => l.pos = (l.pos + 1).min(l.buf.len()),
                0x08 | 0x7f => {
                    if l.pos > 0 {
                        l.pos -= 1;
                        l.buf.remove(l.pos);
                    }
                }
                0x0b => {
                    l.buf.truncate(l.pos);
                }
                0x15 => {
                    l.buf.drain(..l.pos);
                    l.pos = 0;
                }
                0x17 => kill_word_back(&mut l),
                0x0c => {
                    print_flush("\x1b[H\x1b[2J");
                    l.row = 0;
                }
                0x10 | 0x0e => {
                    // C-p / C-n: 履歴を順に
                    let up = c == 0x10;
                    self.step(&mut l, &mut hi, &mut saved, None, up);
                    nav = true;
                }
                0x1b => match read_byte_soon() {
                    Some(b'[') | Some(b'O') => {
                        let mut seq = Vec::new();
                        while let Some(d) = read_byte_soon() {
                            seq.push(d);
                            if (0x40..=0x7e).contains(&d) {
                                break;
                            }
                        }
                        match seq.as_slice() {
                            b"A" | b"B" => {
                                // ↑ ↓: 打ちかけの文字をふくむ履歴
                                if hi == self.history.len() {
                                    search = Some(l.buf.iter().collect());
                                }
                                let q = search.clone().unwrap_or_default();
                                self.step(&mut l, &mut hi, &mut saved, Some(&q), seq == b"A");
                                nav = true;
                            }
                            b"C" => l.pos = (l.pos + 1).min(l.buf.len()),
                            b"D" => l.pos = l.pos.saturating_sub(1),
                            b"H" | b"1~" | b"7~" => l.pos = 0,
                            b"F" | b"4~" | b"8~" => l.pos = l.buf.len(),
                            b"3~" => {
                                if l.pos < l.buf.len() {
                                    l.buf.remove(l.pos);
                                }
                            }
                            _ => redraw = false,
                        }
                    }
                    Some(b'b') => l.pos = word_left(&l.buf, l.pos),
                    Some(b'f') => l.pos = word_right(&l.buf, l.pos),
                    Some(0x7f) | Some(0x08) => kill_word_back(&mut l),
                    _ => redraw = false,
                },
                c if c >= 0x20 => {
                    // UTF-8 の続きも読む
                    let n = if c >= 0xf0 { 3 } else if c >= 0xe0 { 2 } else if c >= 0xc0 { 1 } else { 0 };
                    let mut bytes = vec![c];
                    for _ in 0..n {
                        if let Some(d) = read_byte() {
                            bytes.push(d);
                        }
                    }
                    for ch in String::from_utf8_lossy(&bytes).chars() {
                        l.buf.insert(l.pos, ch);
                        l.pos += 1;
                    }
                }
                _ => redraw = false,
            }
            if !nav && redraw {
                search = None;
                hi = self.history.len();
            }
            if redraw {
                self.draw(prompt, &mut l, &mut out);
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
        l.buf = self.history[i].chars().collect();
        l.pos = l.buf.len();
    }

    /// プロンプトと行を描きなおして、カーソルを pos へ
    fn draw(&self, prompt: &str, l: &mut Line, out: &mut String) {
        let cols = term_cols();
        out.clear();
        // 前に描いた頭へ
        if l.row > 0 {
            out.push_str(&format!("\x1b[{}A", l.row));
        }
        out.push_str("\r\x1b[J");
        out.extend(prompt.chars().filter(|&c| c != '\x01' && c != '\x02'));
        // プロンプトが何行かあれば、最後の行の幅から
        let pn = prompt.matches('\n').count();
        let pw = str_width(prompt.rsplit('\n').next().unwrap_or(prompt));
        let text: String = l.buf.iter().collect();
        out.push_str(&text);
        let total = pw + l.buf.iter().map(|&c| char_width(c)).sum::<usize>();
        let at = pw + l.buf[..l.pos].iter().map(|&c| char_width(c)).sum::<usize>();
        // 行の終わりちょうどで折り返したら、カーソルを次の行の頭へ
        if total > 0 && total % cols == 0 {
            out.push_str("\r\n");
        }
        let end_row = total / cols;
        let (row, col) = (at / cols, at % cols);
        if end_row > row {
            out.push_str(&format!("\x1b[{}A", end_row - row));
        }
        out.push('\r');
        if col > 0 {
            out.push_str(&format!("\x1b[{}C", col));
        }
        l.row = pn + row;
        print_flush(out);
    }
}

fn print_flush(s: &str) {
    let mut e = std::io::stderr();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
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
