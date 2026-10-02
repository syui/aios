// aiterm: aios の端末 (Wayland のクライアント)
//   疑似端末 (/dev/ptmx) でシェル ($SHELL、なければ /bin/aish) を動かし、
//   その出力を aifont で窓に描く。キーは evdev の番号から文字にして送る (配列は XKB_DEFAULT_LAYOUT)
//   aiterm [-e コマンド...]
#[path = "../lib/keys.rs"]
mod keys;
#[path = "../lib/term.rs"]
mod term;
#[path = "../lib/wl.rs"]
mod wl;

use keys::Mods;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::process::exit;
use term::Term;
use wl::{Arg, Conn};

const FONT: &str = "/usr/share/fonts/aifont/aifont.ttf";
const SIZE: f32 = 16.0;
const PAD: usize = 4;
const CURSOR: u32 = 0xf5c518;

struct Glyph {
    w: usize,
    h: usize,
    left: i32,
    /// ベースラインから上の端まで
    top: i32,
    alpha: Vec<u8>,
}

struct Shm {
    pool: u32,
    buffer: u32,
    fd: RawFd,
    ptr: *mut u32,
    size: usize,
}

struct App {
    conn: Conn,
    next_id: u32,
    // グローバル
    compositor: u32,
    shm_global: u32,
    seat: u32,
    wm_base: u32,
    // 窓
    surface: u32,
    xdg: u32,
    toplevel: u32,
    keyboard: u32,
    configured: bool,
    width: usize,
    height: usize,
    shm: Option<Shm>,
    /// 前の frame がまだ終わっていない
    waiting_frame: Option<u32>,
    focused: bool,
    // 文字
    font: fontdue::Font,
    glyphs: HashMap<(char, bool), Glyph>,
    cell_w: usize,
    cell_h: usize,
    ascent: i32,
    term: Term,
    full_redraw: bool,
    /// 前に描いたカーソルの場所 (動いたら、前の行も描きなおす)
    last_cursor: (usize, usize),
    // シェル
    pty: RawFd,
    child: i32,
    mods: Mods,
    layout: String,
    repeat: Option<(u16, u32)>,
    closed: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd: Vec<String> = match args.first().map(String::as_str) {
        Some("-e") => args[1..].to_vec(),
        Some(a) => {
            eprintln!("usage: aiterm [-e command...] ({}?)", a);
            exit(2)
        }
        None => vec![],
    };
    let conn = Conn::connect().unwrap_or_else(|e| {
        eprintln!("aiterm: cannot connect to the Wayland compositor: {} (aiwm の中で動かす)", e);
        exit(1)
    });
    let data = std::fs::read(FONT).unwrap_or_else(|e| {
        eprintln!("aiterm: {}: {} (aipkg -S aifont)", FONT, e);
        exit(1)
    });
    let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).unwrap_or_else(|e| {
        eprintln!("aiterm: {}: {}", FONT, e);
        exit(1)
    });
    let lm = font.horizontal_line_metrics(SIZE).expect("line metrics");
    let cell_w = font.metrics('M', SIZE).advance_width.ceil() as usize;
    let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil() as usize;
    let ascent = lm.ascent.ceil() as i32;
    let mut app = App {
        conn,
        next_id: 2,
        compositor: 0,
        shm_global: 0,
        seat: 0,
        wm_base: 0,
        surface: 0,
        xdg: 0,
        toplevel: 0,
        keyboard: 0,
        configured: false,
        width: 0,
        height: 0,
        shm: None,
        waiting_frame: None,
        focused: false,
        font,
        glyphs: HashMap::new(),
        cell_w: cell_w.max(1),
        cell_h: cell_h.max(1),
        ascent,
        term: Term::new(80, 24),
        full_redraw: true,
        last_cursor: (0, 0),
        pty: -1,
        child: 0,
        mods: Mods::default(),
        layout: keys::layout(),
        repeat: None,
        closed: false,
    };
    app.setup();
    app.spawn(&cmd);
    app.run();
}

impl App {
    fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// グローバルを集めて、窓を作る
    fn setup(&mut self) {
        let reg = self.new_id();
        self.conn.send(1, 1, &[Arg::O(reg)]);
        let sync = self.new_id();
        self.conn.send(1, 0, &[Arg::O(sync)]);
        let mut globals = vec![];
        self.roundtrip(sync, |_, m| {
            if m.id == reg && m.op == 0 {
                let name = m.uint();
                let iface = m.string();
                let ver = m.uint();
                globals.push((name, iface, ver));
            }
        });
        for (name, iface, ver) in globals {
            let want = match iface.as_str() {
                "wl_compositor" => 4,
                "wl_shm" => 1,
                "wl_seat" => 5,
                "xdg_wm_base" => 2,
                _ => continue,
            };
            let id = self.new_id();
            self.conn.send(reg, 0, &[Arg::U(name), Arg::S(&iface), Arg::U(ver.min(want)), Arg::O(id)]);
            match iface.as_str() {
                "wl_compositor" => self.compositor = id,
                "wl_shm" => self.shm_global = id,
                "wl_seat" => self.seat = id,
                _ => self.wm_base = id,
            }
        }
        if self.compositor == 0 || self.shm_global == 0 || self.wm_base == 0 {
            eprintln!("aiterm: the compositor lacks wl_compositor / wl_shm / xdg_wm_base");
            exit(1);
        }
        self.surface = self.new_id();
        self.conn.send(self.compositor, 0, &[Arg::O(self.surface)]);
        self.xdg = self.new_id();
        self.conn.send(self.wm_base, 2, &[Arg::O(self.xdg), Arg::O(self.surface)]);
        self.toplevel = self.new_id();
        self.conn.send(self.xdg, 1, &[Arg::O(self.toplevel)]);
        self.conn.send(self.toplevel, 2, &[Arg::S("aiterm")]);
        self.conn.send(self.toplevel, 3, &[Arg::S("aiterm")]);
        if self.seat != 0 {
            self.keyboard = self.new_id();
            self.conn.send(self.seat, 1, &[Arg::O(self.keyboard)]);
        }
        self.conn.send(self.surface, 6, &[]);
        let _ = self.conn.flush();
    }

    /// sync の done が来るまで、来たものを f に渡す
    fn roundtrip(&mut self, sync: u32, mut f: impl FnMut(&mut Conn, &mut wl::Msg)) {
        let _ = self.conn.flush();
        loop {
            while let Some(mut m) = self.conn.next() {
                if m.id == sync && m.op == 0 {
                    return;
                }
                f(&mut self.conn, &mut m);
            }
            let mut p = libc::pollfd { fd: self.conn.fd, events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, -1) };
            if !self.conn.recv().unwrap_or(false) {
                eprintln!("aiterm: the compositor closed the connection");
                exit(1);
            }
        }
    }

    // ---- シェル ----

    fn spawn(&mut self, cmd: &[String]) {
        unsafe {
            let m = libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if m < 0 {
                eprintln!("aiterm: /dev/ptmx: {}", std::io::Error::last_os_error());
                exit(1);
            }
            let mut n: libc::c_int = 0;
            libc::ioctl(m, libc::TIOCSPTLCK, &mut n);
            libc::ioctl(m, libc::TIOCGPTN, &mut n);
            let slave = CString::new(format!("/dev/pts/{}", n)).unwrap();
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/aish".into());
            let argv: Vec<CString> = if cmd.is_empty() {
                vec![CString::new(shell.clone()).unwrap()]
            } else {
                vec![CString::new("/bin/sh").unwrap(), CString::new("-c").unwrap(), CString::new(cmd.join(" ")).unwrap()]
            };
            let prog = argv[0].clone();
            let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            let pid = libc::fork();
            if pid == 0 {
                libc::setsid();
                let s = libc::open(slave.as_ptr(), libc::O_RDWR);
                if s < 0 {
                    libc::_exit(127);
                }
                libc::ioctl(s, libc::TIOCSCTTY, 0);
                libc::dup2(s, 0);
                libc::dup2(s, 1);
                libc::dup2(s, 2);
                if s > 2 {
                    libc::close(s);
                }
                libc::execv(prog.as_ptr(), ptrs.as_ptr());
                libc::_exit(127);
            }
            libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK);
            self.pty = m;
            self.child = pid;
        }
    }

    fn set_winsize(&self) {
        let ws = libc::winsize { ws_row: self.term.rows as u16, ws_col: self.term.cols as u16, ws_xpixel: self.width as u16, ws_ypixel: self.height as u16 };
        unsafe { libc::ioctl(self.pty, libc::TIOCSWINSZ, &ws) };
    }

    fn write_pty(&self, b: &[u8]) {
        let mut done = 0;
        while done < b.len() {
            let n = unsafe { libc::write(self.pty, b[done..].as_ptr() as *const _, b.len() - done) };
            if n <= 0 {
                let e = std::io::Error::last_os_error().raw_os_error();
                if e == Some(libc::EAGAIN) || e == Some(libc::EINTR) {
                    let mut p = libc::pollfd { fd: self.pty, events: libc::POLLOUT, revents: 0 };
                    unsafe { libc::poll(&mut p, 1, 100) };
                    continue;
                }
                return;
            }
            done += n as usize;
        }
    }

    // ---- 回す ----

    fn run(&mut self) {
        while !self.closed {
            let mut pfds = [
                libc::pollfd { fd: self.conn.fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.pty, events: libc::POLLIN, revents: 0 },
            ];
            let timeout = match self.repeat {
                Some((_, at)) => (at.wrapping_sub(wl::now_ms()) as i32).clamp(0, 1000),
                None => -1,
            };
            unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout) };
            if pfds[0].revents != 0 {
                if !self.conn.recv().unwrap_or(false) {
                    break;
                }
                while let Some(m) = self.conn.next() {
                    self.event(m);
                }
            }
            if pfds[1].revents != 0 {
                let mut buf = [0u8; 16384];
                let n = unsafe { libc::read(self.pty, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n > 0 {
                    self.term.feed(&buf[..n as usize]);
                    if !self.term.reply.is_empty() {
                        let r = std::mem::take(&mut self.term.reply);
                        self.write_pty(&r);
                    }
                    if let Some(t) = self.term.title.take() {
                        self.conn.send(self.toplevel, 2, &[Arg::S(&t)]);
                    }
                } else if n == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                    // シェルが終わった
                    break;
                }
            }
            if let Some((code, at)) = self.repeat {
                if wl::now_ms().wrapping_sub(at) as i32 >= 0 {
                    self.repeat = Some((code, wl::now_ms() + 40));
                    self.send_key(code);
                }
            }
            self.draw();
            if self.conn.flush().is_err() {
                break;
            }
        }
        if self.child > 0 {
            unsafe { libc::kill(self.child, libc::SIGHUP) };
        }
    }

    fn event(&mut self, mut m: wl::Msg) {
        let id = m.id;
        if id == 1 {
            if m.op == 0 {
                let (_, code, msg) = (m.uint(), m.uint(), m.string());
                eprintln!("aiterm: protocol error {}: {}", code, msg);
                exit(1);
            }
            return;
        }
        if id == self.wm_base && m.op == 0 {
            let s = m.uint();
            self.conn.send(self.wm_base, 3, &[Arg::U(s)]);
        } else if id == self.toplevel && m.op == 0 {
            let (w, h) = (m.int(), m.int());
            if w > 0 && h > 0 {
                self.width = w as usize;
                self.height = h as usize;
            } else if self.width == 0 {
                self.width = 80 * self.cell_w + 2 * PAD;
                self.height = 24 * self.cell_h + 2 * PAD;
            }
            let states = m.array();
            self.focused = states.chunks(4).any(|c| u32::from_le_bytes(c.try_into().unwrap()) == 4);
            self.term.dirty[self.term.cy] = true;
        } else if id == self.toplevel && m.op == 1 {
            self.closed = true;
        } else if id == self.xdg && m.op == 0 {
            let serial = m.uint();
            self.conn.send(self.xdg, 4, &[Arg::U(serial)]);
            self.configured = true;
            self.resize();
        } else if Some(id) == self.waiting_frame && m.op == 0 {
            self.waiting_frame = None;
        } else if self.shm.as_ref().is_some_and(|s| s.buffer == id) {
            // release: 写してもらったのでまた使える
        } else if id == self.keyboard {
            self.keyboard_event(m);
        }
    }

    fn keyboard_event(&mut self, mut m: wl::Msg) {
        match m.op {
            0 => {
                // keymap: fd は使わない (配列は XKB_DEFAULT_LAYOUT)
                let _ = m.uint();
                if let Some(fd) = self.conn.take_fd() {
                    unsafe { libc::close(fd) };
                }
            }
            1 => {
                let caps = self.mods.caps;
                self.mods = Mods::default();
                self.mods.caps = caps;
            }
            2 => self.repeat = None,
            3 => {
                let (_serial, _time, key, state) = (m.uint(), m.uint(), m.uint() as u16, m.uint());
                let pressed = state == 1;
                if self.mods.update(key, pressed) {
                    return;
                }
                if pressed {
                    self.send_key(key);
                    self.repeat = Some((key, wl::now_ms() + 500));
                } else if self.repeat.is_some_and(|(k, _)| k == key) {
                    self.repeat = None;
                }
            }
            4 => {
                let (_s, _dep, _lat, locked) = (m.uint(), m.uint(), m.uint(), m.uint());
                self.mods.caps = locked & keys::MOD_LOCK != 0;
            }
            _ => {}
        }
    }

    /// キーをシェルへのバイトにして送る
    fn send_key(&mut self, key: u16) {
        let app = self.term.app_cursor;
        let arrow = |c: char| if app { format!("\x1bO{}", c) } else { format!("\x1b[{}", c) };
        let seq: Option<String> = match key {
            keys::KEY_ENTER => Some("\r".into()),
            keys::KEY_BACKSPACE => Some(if self.mods.ctrl() { "\x08" } else { "\x7f" }.into()),
            keys::KEY_TAB => Some(if self.mods.shift() { "\x1b[Z" } else { "\t" }.into()),
            keys::KEY_ESC => Some("\x1b".into()),
            keys::KEY_UP => Some(arrow('A')),
            keys::KEY_DOWN => Some(arrow('B')),
            keys::KEY_RIGHT => Some(arrow('C')),
            keys::KEY_LEFT => Some(arrow('D')),
            keys::KEY_HOME => Some("\x1b[H".into()),
            keys::KEY_END => Some("\x1b[F".into()),
            keys::KEY_INSERT => Some("\x1b[2~".into()),
            keys::KEY_DELETE => Some("\x1b[3~".into()),
            keys::KEY_PAGEUP => Some("\x1b[5~".into()),
            keys::KEY_PAGEDOWN => Some("\x1b[6~".into()),
            _ => None,
        };
        let mut out = Vec::new();
        if let Some(s) = seq {
            if self.mods.alt() && s.len() == 1 {
                out.push(0x1b);
            }
            out.extend_from_slice(s.as_bytes());
        } else if let Some(c) = keys::char_of(key, &self.mods, &self.layout) {
            if self.mods.alt() {
                out.push(0x1b);
            }
            if self.mods.ctrl() {
                let b = match c {
                    'a'..='z' | 'A'..='Z' => Some(c.to_ascii_lowercase() as u8 & 0x1f),
                    ' ' | '@' | '2' => Some(0),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '^' | '6' => Some(0x1e),
                    '_' | '-' | '7' => Some(0x1f),
                    '/' => Some(0x1f),
                    _ => None,
                };
                match b {
                    Some(b) => out.push(b),
                    None => return,
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        } else {
            return;
        }
        self.write_pty(&out);
    }

    // ---- 描く ----

    /// 窓の大きさが決まった / 変わった: 共有メモリを作りなおし、升目の数をなおす
    fn resize(&mut self) {
        let (w, h) = (self.width.max(self.cell_w + 2 * PAD), self.height.max(self.cell_h + 2 * PAD));
        if self.shm.as_ref().is_some_and(|s| s.size == w * h * 4) && self.width == w {
            return;
        }
        if let Some(old) = self.shm.take() {
            self.conn.send(old.buffer, 0, &[]);
            self.conn.send(old.pool, 1, &[]);
            unsafe {
                libc::munmap(old.ptr as *mut _, old.size);
                libc::close(old.fd);
            }
        }
        let size = w * h * 4;
        let (fd, ptr) = wl::shm(size).unwrap_or_else(|e| {
            eprintln!("aiterm: shared memory: {}", e);
            exit(1)
        });
        let pool = self.new_id();
        self.conn.send(self.shm_global, 0, &[Arg::O(pool), Arg::Fd(fd), Arg::I(size as i32)]);
        let buffer = self.new_id();
        self.conn.send(pool, 0, &[Arg::O(buffer), Arg::I(0), Arg::I(w as i32), Arg::I(h as i32), Arg::I(w as i32 * 4), Arg::U(1)]);
        self.shm = Some(Shm { pool, buffer, fd, ptr: ptr as *mut u32, size });
        self.width = w;
        self.height = h;
        let cols = (w - 2 * PAD) / self.cell_w;
        let rows = (h - 2 * PAD) / self.cell_h;
        self.term.resize(cols, rows);
        if self.pty >= 0 {
            self.set_winsize();
        }
        self.full_redraw = true;
    }

    fn glyph(&mut self, c: char, bold: bool) -> &Glyph {
        let font = &self.font;
        self.glyphs.entry((c, bold)).or_insert_with(|| {
            let (m, alpha) = font.rasterize(c, SIZE);
            let alpha = if bold {
                // 太字: 横に 1 画素ずらして重ねる
                let mut b = alpha.clone();
                for y in 0..m.height {
                    for x in 1..m.width {
                        let i = y * m.width + x;
                        b[i] = b[i].max(alpha[i - 1]);
                    }
                }
                b
            } else {
                alpha
            };
            Glyph { w: m.width, h: m.height, left: m.xmin, top: m.height as i32 + m.ymin, alpha }
        })
    }

    fn draw(&mut self) {
        if !self.configured || self.waiting_frame.is_some() || self.shm.is_none() {
            return;
        }
        let cur = (self.term.cx, self.term.cy);
        if cur != self.last_cursor {
            if self.last_cursor.1 < self.term.rows {
                self.term.dirty[self.last_cursor.1] = true;
            }
            self.term.dirty[cur.1] = true;
            self.last_cursor = cur;
        }
        let full = std::mem::take(&mut self.full_redraw);
        if !full && !self.term.dirty.iter().any(|&d| d) {
            return;
        }
        let (w, h) = (self.width, self.height);
        let px = {
            let s = self.shm.as_ref().unwrap();
            unsafe { std::slice::from_raw_parts_mut(s.ptr, w * h) }
        };
        if full {
            px.fill(term::BG);
        }
        let (cw, ch) = (self.cell_w, self.cell_h);
        let mut y_min = usize::MAX;
        let mut y_max = 0;
        for row in 0..self.term.rows {
            if !full && !self.term.dirty[row] {
                continue;
            }
            self.term.dirty[row] = false;
            let y0 = PAD + row * ch;
            y_min = y_min.min(y0);
            y_max = y_max.max(y0 + ch);
            for col in 0..self.term.cols {
                let cell = self.term.cell(col, row);
                let x0 = PAD + col * cw;
                let cursor = self.term.cursor_visible && row == self.term.cy && col == self.term.cx;
                let (mut fg, mut bg) = (cell.fg, cell.bg);
                if cursor && self.focused {
                    (fg, bg) = (term::BG, CURSOR);
                }
                for y in y0..y0 + ch {
                    px[y * w + x0..y * w + x0 + cw].fill(bg);
                }
                if cursor && !self.focused {
                    // 作業中でなければ枠だけ
                    for x in x0..x0 + cw {
                        px[y0 * w + x] = CURSOR;
                        px[(y0 + ch - 1) * w + x] = CURSOR;
                    }
                    for y in y0..y0 + ch {
                        px[y * w + x0] = CURSOR;
                        px[y * w + x0 + cw - 1] = CURSOR;
                    }
                }
                if cell.c != ' ' {
                    let base = y0 as i32 + self.ascent;
                    let g = self.glyph(cell.c, cell.bold);
                    let gx = x0 as i32 + g.left;
                    let gy = base - g.top;
                    for yy in 0..g.h {
                        let y = gy + yy as i32;
                        if y < y0 as i32 || y >= (y0 + ch) as i32 {
                            continue;
                        }
                        for xx in 0..g.w {
                            let x = gx + xx as i32;
                            if x < 0 || x as usize >= w {
                                continue;
                            }
                            let a = g.alpha[yy * g.w + xx] as u32;
                            if a == 0 {
                                continue;
                            }
                            let p = &mut px[y as usize * w + x as usize];
                            *p = mix(fg, *p, a);
                        }
                    }
                }
                if cell.ul {
                    let y = y0 + ch - 2;
                    px[y * w + x0..y * w + x0 + cw].fill(fg);
                }
            }
        }
        if full {
            (y_min, y_max) = (0, h);
        }
        if y_min >= y_max {
            return;
        }
        let s = self.shm.as_ref().unwrap();
        let (buffer, surface) = (s.buffer, self.surface);
        self.conn.send(surface, 1, &[Arg::O(buffer), Arg::I(0), Arg::I(0)]);
        self.conn.send(surface, 9, &[Arg::I(0), Arg::I(y_min as i32), Arg::I(w as i32), Arg::I((y_max - y_min) as i32)]);
        let cb = self.new_id();
        self.conn.send(surface, 3, &[Arg::O(cb)]);
        self.waiting_frame = Some(cb);
        self.conn.send(surface, 6, &[]);
    }
}

fn mix(fg: u32, bg: u32, a: u32) -> u32 {
    let m = |s: u32, d: u32| (s * a + d * (255 - a)) / 255;
    m(fg >> 16 & 255, bg >> 16 & 255) << 16 | m(fg >> 8 & 255, bg >> 8 & 255) << 8 | m(fg & 255, bg & 255)
}
