// 端末の中身: 文字の升目と、エスケープシーケンス (xterm の よく使うところ) の読み取り
//   feed() でシェルからの出力を入れると升目が変わる。reply にはシェルへ返す答え (カーソルの位置など)
#![allow(dead_code)]

pub const FG: u32 = 0xd8dee9;
pub const BG: u32 = 0x161821;

/// 16 色 (暗い 8 色と明るい 8 色)
const PALETTE: [u32; 16] = [
    0x1d1f28, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd8dee9, //
    0x5c6370, 0xf07c85, 0xa8d389, 0xf5c518, 0x71bfff, 0xd688ed, 0x66c6d2, 0xffffff,
];

fn color256(n: u16) -> u32 {
    match n {
        0..=15 => PALETTE[n as usize],
        16..=231 => {
            let n = n - 16;
            let v = |k: u16| if k == 0 { 0 } else { 55 + 40 * k as u32 };
            v(n / 36) << 16 | v(n / 6 % 6) << 8 | v(n % 6)
        }
        _ => {
            let g = 8 + 10 * (n as u32 - 232);
            g << 16 | g << 8 | g
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct Cell {
    pub c: char,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub ul: bool,
}

const BLANK: Cell = Cell { c: ' ', fg: FG, bg: BG, bold: false, ul: false };

/// 2 マスの字 (日本語など) の右半分に置く印 (描かない)
pub const WIDE_CONT: char = '\0';

/// 端末で何マス使うか (East Asian Wide / Fullwidth と絵文字は 2、結合文字は 0)
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if matches!(u, 0x0300..=0x036f | 0x200b..=0x200f | 0x20d0..=0x20ff | 0xfe00..=0xfe0f | 0xe0100..=0xe01ef) {
        return 0;
    }
    let wide = matches!(u,
        0x1100..=0x115f | 0x231a..=0x231b | 0x2329..=0x232a | 0x23e9..=0x23ec | 0x23f0 | 0x23f3 | 0x25fd..=0x25fe
        | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267f | 0x2693 | 0x26a1 | 0x26aa..=0x26ab | 0x26bd..=0x26be
        | 0x26c4..=0x26c5 | 0x26ce | 0x26d4 | 0x26ea | 0x26f2..=0x26f3 | 0x26f5 | 0x26fa | 0x26fd | 0x2705
        | 0x270a..=0x270b | 0x2728 | 0x274c | 0x274e | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27b0 | 0x27bf
        | 0x2b1b..=0x2b1c | 0x2b50 | 0x2b55 | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf | 0xa960..=0xa97f | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe10..=0xfe19 | 0xfe30..=0xfe6f
        | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f004 | 0x1f0cf | 0x1f18e | 0x1f191..=0x1f19a | 0x1f200..=0x1f251
        | 0x1f300..=0x1f64f | 0x1f680..=0x1f6ff | 0x1f900..=0x1f9ff | 0x1fa70..=0x1faff | 0x20000..=0x3fffd);
    if wide { 2 } else { 1 }
}

#[derive(Clone, Copy)]
struct Pen {
    fg: u32,
    bg: u32,
    bold: bool,
    ul: bool,
    reverse: bool,
}

const PEN: Pen = Pen { fg: FG, bg: BG, bold: false, ul: false, reverse: false };

enum State {
    Ground,
    Esc,
    /// ESC ( などのあとの 1 文字を読みとばす
    Skip,
    Csi,
    Osc,
    /// OSC の中の ESC (ESC \ で終わる)
    OscEsc,
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    grid: Vec<Cell>,
    /// 代わりの画面 (?1049h) に入っている間の、元の画面
    saved_grid: Option<Vec<Cell>>,
    pub cx: usize,
    pub cy: usize,
    /// 右端に書いたあと (次の文字で折り返す)
    wrap: bool,
    saved: (usize, usize, Pen),
    pen: Pen,
    top: usize,
    bottom: usize,
    pub cursor_visible: bool,
    /// アプリケーションのカーソルキー (?1h: 矢印を ESC O A で送る)
    pub app_cursor: bool,
    state: State,
    params: String,
    utf8: Vec<u8>,
    /// 変わった行 (描きなおすところ)
    pub dirty: Vec<bool>,
    /// シェルへ返すもの
    pub reply: Vec<u8>,
    pub title: Option<String>,
    osc: String,
    /// 上へ流れて消えた行 (スクロールバック。古いものから。代わりの画面のあいだはためない)
    history: std::collections::VecDeque<Vec<Cell>>,
    /// いま見ているところ: 何行ぶん上を見ているか (0 なら今の画面)
    pub view: usize,
}

/// スクロールバックにためる行の数
const HISTORY: usize = 5000;

impl Term {
    pub fn new(cols: usize, rows: usize) -> Term {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Term {
            cols,
            rows,
            grid: vec![BLANK; cols * rows],
            saved_grid: None,
            cx: 0,
            cy: 0,
            wrap: false,
            saved: (0, 0, PEN),
            pen: PEN,
            top: 0,
            bottom: rows - 1,
            cursor_visible: true,
            app_cursor: false,
            state: State::Ground,
            params: String::new(),
            utf8: Vec::new(),
            dirty: vec![true; rows],
            reply: Vec::new(),
            title: None,
            osc: String::new(),
            history: std::collections::VecDeque::new(),
            view: 0,
        }
    }

    pub fn cell(&self, x: usize, y: usize) -> Cell {
        self.grid[y * self.cols + x]
    }

    /// 見ている画面の (x, y) (view 行ぶん上なら、スクロールバックの行も)
    pub fn view_cell(&self, x: usize, y: usize) -> Cell {
        if self.view == 0 {
            return self.cell(x, y);
        }
        let i = self.history.len() + y - self.view;
        match self.history.get(i) {
            // 幅を変える前の行は、足りなければ空白
            Some(row) => row.get(x).copied().unwrap_or(BLANK),
            None => self.cell(x, i - self.history.len()),
        }
    }

    /// 見るところを delta 行動かす (正で上 = 古いほう)。変わったら true
    pub fn scroll_view(&mut self, delta: isize) -> bool {
        let v = (self.view as isize + delta).clamp(0, self.history.len() as isize) as usize;
        if v == self.view {
            return false;
        }
        self.view = v;
        self.dirty.fill(true);
        true
    }

    /// スクロールバックの行の数
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// 大きさを変える (中身は左上にそろえて残す)
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        // 行が減るなら、カーソルが見えるように上を捨てる
        let drop = (self.cy + 1).saturating_sub(rows);
        let mut g = vec![BLANK; cols * rows];
        for y in 0..rows.min(self.rows - drop) {
            for x in 0..cols.min(self.cols) {
                g[y * cols + x] = self.grid[(y + drop) * self.cols + x];
            }
        }
        self.grid = g;
        self.saved_grid = None;
        self.cols = cols;
        self.rows = rows;
        self.cy -= drop;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.top = 0;
        self.bottom = rows - 1;
        self.wrap = false;
        self.dirty = vec![true; rows];
        self.view = 0;
    }

    fn blank(&self) -> Cell {
        Cell { bg: if self.pen.reverse { self.pen.fg } else { self.pen.bg }, ..BLANK }
    }

    fn put(&mut self, c: char) {
        let width = char_width(c);
        if width == 0 {
            // 結合文字は前の字に重ねる代わりに捨てる (升目は 1 字 1 マスなので)
            return;
        }
        if self.wrap {
            self.wrap = false;
            self.cx = 0;
            self.newline();
        }
        // 2 マスの字が右端に入らなければ、次の行へ (右端は空けておく)
        if width == 2 && self.cx + 1 >= self.cols {
            let blank = self.blank();
            let i = self.cy * self.cols + self.cx;
            self.grid[i] = blank;
            self.dirty[self.cy] = true;
            self.cx = 0;
            self.newline();
        }
        let p = self.pen;
        let (fg, bg) = if p.reverse { (p.bg, p.fg) } else { (p.fg, p.bg) };
        let row = self.cy * self.cols;
        // 2 マスの字の半分だけを上書きするなら、もう半分は空白に
        if self.grid[row + self.cx].c == WIDE_CONT && self.cx > 0 {
            self.grid[row + self.cx - 1] = Cell { c: ' ', ..self.grid[row + self.cx - 1] };
        }
        let end = self.cx + width;
        if end < self.cols && self.grid[row + end].c == WIDE_CONT {
            self.grid[row + end] = Cell { c: ' ', ..self.grid[row + end] };
        }
        self.grid[row + self.cx] = Cell { c, fg, bg, bold: p.bold, ul: p.ul };
        if width == 2 {
            self.grid[row + self.cx + 1] = Cell { c: WIDE_CONT, fg, bg, bold: p.bold, ul: p.ul };
        }
        self.dirty[self.cy] = true;
        if self.cx + width >= self.cols {
            self.cx = self.cols - 1;
            self.wrap = true;
        } else {
            self.cx += width;
        }
    }

    fn newline(&mut self) {
        if self.cy == self.bottom {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    /// [top, bottom] を n 行上へ (下に空いた行)
    fn scroll_up(&mut self, n: usize) {
        let (t, b, w) = (self.top, self.bottom, self.cols);
        let n = n.min(b - t + 1);
        // 画面の一番上から流れていく行はスクロールバックへ (代わりの画面 (vim など) と、範囲を決めた上下のスクロールはのぞく)
        if t == 0 && self.saved_grid.is_none() {
            for y in 0..n {
                self.history.push_back(self.grid[y * w..(y + 1) * w].to_vec());
                if self.history.len() > HISTORY {
                    self.history.pop_front();
                } else if self.view > 0 {
                    // 見ているところが流れないように
                    self.view += 1;
                }
            }
            self.view = self.view.min(self.history.len());
        }
        self.grid.copy_within((t + n) * w..(b + 1) * w, t * w);
        let blank = self.blank();
        self.grid[(b + 1 - n) * w..(b + 1) * w].fill(blank);
        for y in t..=b {
            self.dirty[y] = true;
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (t, b, w) = (self.top, self.bottom, self.cols);
        let n = n.min(b - t + 1);
        self.grid.copy_within(t * w..(b + 1 - n) * w, (t + n) * w);
        let blank = self.blank();
        self.grid[t * w..(t + n) * w].fill(blank);
        for y in t..=b {
            self.dirty[y] = true;
        }
    }

    fn clear(&mut self, from: usize, to: usize) {
        let blank = self.blank();
        let to = to.min(self.grid.len());
        if from >= to {
            return;
        }
        self.grid[from..to].fill(blank);
        for y in from / self.cols..=(to - 1) / self.cols {
            self.dirty[y] = true;
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Ground => {
                if !self.utf8.is_empty() || b >= 0x80 {
                    self.utf8.push(b);
                    match std::str::from_utf8(&self.utf8) {
                        Ok(s) => {
                            let c = s.chars().next().unwrap_or('?');
                            self.utf8.clear();
                            self.put(c);
                        }
                        Err(e) if e.error_len().is_some() || self.utf8.len() >= 4 => {
                            self.utf8.clear();
                            self.put('\u{fffd}');
                        }
                        Err(_) => {}
                    }
                    return;
                }
                self.control_or_put(b);
            }
            State::Esc => {
                self.state = State::Ground;
                match b {
                    b'[' => {
                        self.params.clear();
                        self.state = State::Csi;
                    }
                    b']' => {
                        self.osc.clear();
                        self.state = State::Osc;
                    }
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.state = State::Skip,
                    b'7' => self.saved = (self.cx, self.cy, self.pen),
                    b'8' => {
                        (self.cx, self.cy, self.pen) = self.saved;
                        self.wrap = false;
                    }
                    b'D' => self.newline(),
                    b'E' => {
                        self.cx = 0;
                        self.newline();
                    }
                    b'M' => {
                        if self.cy == self.top {
                            self.scroll_down(1);
                        } else if self.cy > 0 {
                            self.cy -= 1;
                        }
                    }
                    b'c' => {
                        let (c, r) = (self.cols, self.rows);
                        *self = Term::new(c, r);
                    }
                    _ => {}
                }
            }
            State::Skip => self.state = State::Ground,
            State::Csi => {
                if (0x30..=0x3f).contains(&b) || b == b' ' {
                    if self.params.len() < 64 {
                        self.params.push(b as char);
                    }
                } else if (0x40..=0x7e).contains(&b) {
                    self.state = State::Ground;
                    let p = std::mem::take(&mut self.params);
                    self.csi(&p, b);
                } else if b == 0x1b {
                    self.state = State::Esc;
                } else {
                    // CSI の中の制御文字はそのまま効く
                    self.control_or_put(b);
                }
            }
            State::Osc => match b {
                0x07 => self.osc_end(),
                0x1b => self.state = State::OscEsc,
                _ => {
                    if self.osc.len() < 256 {
                        self.osc.push(b as char);
                    }
                }
            },
            State::OscEsc => self.osc_end(),
        }
    }

    fn osc_end(&mut self) {
        self.state = State::Ground;
        let s = std::mem::take(&mut self.osc);
        if let Some(t) = s.strip_prefix("0;").or_else(|| s.strip_prefix("2;")) {
            self.title = Some(t.to_string());
        }
    }

    fn control_or_put(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Esc,
            b'\r' => {
                self.cx = 0;
                self.wrap = false;
            }
            b'\n' | 0x0b | 0x0c => {
                self.wrap = false;
                self.newline();
            }
            0x08 => {
                self.wrap = false;
                self.cx = self.cx.saturating_sub(1);
            }
            b'\t' => {
                self.cx = ((self.cx / 8 + 1) * 8).min(self.cols - 1);
            }
            0x07 | 0x00 | 0x0e | 0x0f => {}
            0x20..=0x7e => self.put(b as char),
            _ => {}
        }
    }

    fn csi(&mut self, p: &str, f: u8) {
        let private = p.starts_with('?');
        let body = p.trim_start_matches(['?', '>', '=', ' ']);
        let nums: Vec<u16> = body.split(';').map(|s| s.parse().unwrap_or(0)).collect();
        let arg = |i: usize, d: u16| nums.get(i).copied().filter(|&v| v != 0).unwrap_or(d) as usize;
        let (w, h) = (self.cols, self.rows);
        if f != b'm' {
            self.wrap = false;
        }
        match f {
            b'A' => self.cy = self.cy.saturating_sub(arg(0, 1)).max(if self.cy >= self.top { self.top } else { 0 }),
            b'B' | b'e' => self.cy = (self.cy + arg(0, 1)).min(if self.cy <= self.bottom { self.bottom } else { h - 1 }),
            b'C' | b'a' => self.cx = (self.cx + arg(0, 1)).min(w - 1),
            b'D' => self.cx = self.cx.saturating_sub(arg(0, 1)),
            b'E' => {
                self.cy = (self.cy + arg(0, 1)).min(h - 1);
                self.cx = 0;
            }
            b'F' => {
                self.cy = self.cy.saturating_sub(arg(0, 1));
                self.cx = 0;
            }
            b'G' | b'`' => self.cx = (arg(0, 1) - 1).min(w - 1),
            b'd' => self.cy = (arg(0, 1) - 1).min(h - 1),
            b'H' | b'f' => {
                self.cy = (arg(0, 1) - 1).min(h - 1);
                self.cx = (arg(1, 1) - 1).min(w - 1);
            }
            b'J' => {
                let here = self.cy * w + self.cx;
                match arg(0, 0) {
                    0 => self.clear(here, w * h),
                    1 => self.clear(0, here + 1),
                    _ => self.clear(0, w * h),
                }
            }
            b'K' => {
                let row = self.cy * w;
                match arg(0, 0) {
                    0 => self.clear(row + self.cx, row + w),
                    1 => self.clear(row, row + self.cx + 1),
                    _ => self.clear(row, row + w),
                }
            }
            b'L' | b'M' if (self.top..=self.bottom).contains(&self.cy) => {
                let t = self.top;
                self.top = self.cy;
                if f == b'L' {
                    self.scroll_down(arg(0, 1));
                } else {
                    self.scroll_up(arg(0, 1));
                }
                self.top = t;
                self.cx = 0;
            }
            b'S' => self.scroll_up(arg(0, 1)),
            b'T' => self.scroll_down(arg(0, 1)),
            b'P' => {
                let n = arg(0, 1).min(w - self.cx);
                let row = self.cy * w;
                self.grid.copy_within(row + self.cx + n..row + w, row + self.cx);
                self.clear(row + w - n, row + w);
            }
            b'@' => {
                let n = arg(0, 1).min(w - self.cx);
                let row = self.cy * w;
                self.grid.copy_within(row + self.cx..row + w - n, row + self.cx + n);
                self.clear(row + self.cx, row + self.cx + n);
            }
            b'X' => {
                let row = self.cy * w;
                self.clear(row + self.cx, row + (self.cx + arg(0, 1)).min(w));
            }
            b'r' if !private => {
                let t = arg(0, 1) - 1;
                let b = arg(1, h as u16).min(h) - 1;
                if t < b {
                    self.top = t;
                    self.bottom = b;
                }
                self.cx = 0;
                self.cy = 0;
            }
            b's' if !private => self.saved = (self.cx, self.cy, self.pen),
            b'u' if !private => (self.cx, self.cy, self.pen) = self.saved,
            b'm' => self.sgr(&nums),
            b'n' if !private && arg(0, 0) == 6 => {
                self.reply.extend_from_slice(format!("\x1b[{};{}R", self.cy + 1, self.cx + 1).as_bytes());
            }
            b'n' if !private && arg(0, 0) == 5 => self.reply.extend_from_slice(b"\x1b[0n"),
            b'c' if !p.starts_with('>') => self.reply.extend_from_slice(b"\x1b[?6c"),
            b'h' | b'l' if private => {
                let on = f == b'h';
                for &n in &nums {
                    match n {
                        1 => self.app_cursor = on,
                        25 => self.cursor_visible = on,
                        1049 | 47 | 1047 => self.alt_screen(on),
                        _ => {}
                    }
                }
                self.dirty[self.cy] = true;
            }
            _ => {}
        }
        self.dirty[self.cy.min(h - 1)] = true;
    }

    fn alt_screen(&mut self, on: bool) {
        self.view = 0;
        if on && self.saved_grid.is_none() {
            self.saved = (self.cx, self.cy, self.pen);
            self.saved_grid = Some(std::mem::replace(&mut self.grid, vec![BLANK; self.cols * self.rows]));
        } else if !on {
            if let Some(g) = self.saved_grid.take() {
                self.grid = g;
                (self.cx, self.cy, self.pen) = self.saved;
            }
        }
        self.dirty.fill(true);
    }

    fn sgr(&mut self, nums: &[u16]) {
        let mut i = 0;
        while i < nums.len() {
            let n = nums[i];
            match n {
                0 => self.pen = PEN,
                1 => self.pen.bold = true,
                4 => self.pen.ul = true,
                7 => self.pen.reverse = true,
                22 => self.pen.bold = false,
                24 => self.pen.ul = false,
                27 => self.pen.reverse = false,
                30..=37 => self.pen.fg = PALETTE[(n - 30) as usize],
                39 => self.pen.fg = FG,
                40..=47 => self.pen.bg = PALETTE[(n - 40) as usize],
                49 => self.pen.bg = BG,
                90..=97 => self.pen.fg = PALETTE[(n - 90 + 8) as usize],
                100..=107 => self.pen.bg = PALETTE[(n - 100 + 8) as usize],
                38 | 48 => {
                    let c = match nums.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            nums.get(i).map(|&v| color256(v))
                        }
                        Some(2) => {
                            i += 4;
                            let g = |k: usize| *nums.get(i + k - 3).unwrap_or(&0) as u32 & 255;
                            Some(g(1) << 16 | g(2) << 8 | g(3))
                        }
                        _ => None,
                    };
                    if let Some(c) = c {
                        if n == 38 {
                            self.pen.fg = c;
                        } else {
                            self.pen.bg = c;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}
