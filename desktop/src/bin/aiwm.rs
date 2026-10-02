// aiwm: aios のタイル型 Wayland コンポジタ (sway の小さな親戚)
//   画面 (/dev/fb0) とキーボード・マウス (/dev/input/event*) を持ち、
//   $XDG_RUNTIME_DIR/wayland-0 で Wayland のクライアント (aiterm など) を待つ。
//   窓は横に並べる (sway の splith)。設定は ~/.config/aiwm/config か /etc/aiwm/config (sway と同じ書き方)
//
// できる Wayland: wl_compositor, wl_shm, wl_seat (キーボード、ポインタ), wl_output, xdg_wm_base (toplevel)。
// クライアントの絵 (wl_shm のバッファ) は commit のときに写して、すぐ release する
#[path = "../lib/fb.rs"]
mod fb;
#[path = "../lib/input.rs"]
mod input;
#[path = "../lib/keys.rs"]
mod keys;
#[path = "../lib/text.rs"]
mod text;
#[path = "../lib/wl.rs"]
mod wl;

use keys::Mods;
use std::collections::{BTreeMap, HashMap};
use std::os::fd::RawFd;
use std::process::exit;
use wl::{Arg, Conn, Msg};

const BORDER: i32 = 2;
const GAP: i32 = 4;
const FOCUS: u32 = 0xf5c518;
const UNFOCUS: u32 = 0x3b3f4c;
const DESK: u32 = 0x0e1018;

// グローバル (wl_registry で見せるもの): (名前, インターフェース, 版)
const GLOBALS: [(u32, &str, u32); 5] = [
    (1, "wl_compositor", 4),
    (2, "wl_shm", 1),
    (3, "wl_seat", 5),
    (4, "wl_output", 2),
    (5, "xdg_wm_base", 2),
];

enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    Pool(Pool),
    Buffer(Buffer),
    Seat,
    Keyboard,
    Pointer,
    Output,
    WmBase,
    XdgSurface(u32),
    Toplevel(u32),
    Positioner,
}

/// 要求を振り分けるための Obj の種類
#[derive(Clone, Copy)]
enum K {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    Pool,
    Buffer,
    Seat,
    Keyboard,
    Pointer,
    Output,
    WmBase,
    XdgSurface(u32),
    Toplevel(u32),
    Positioner,
}

fn kind(o: &Obj) -> K {
    match o {
        Obj::Display => K::Display,
        Obj::Registry => K::Registry,
        Obj::Callback => K::Callback,
        Obj::Compositor => K::Compositor,
        Obj::Surface => K::Surface,
        Obj::Region => K::Region,
        Obj::Shm => K::Shm,
        Obj::Pool(_) => K::Pool,
        Obj::Buffer(_) => K::Buffer,
        Obj::Seat => K::Seat,
        Obj::Keyboard => K::Keyboard,
        Obj::Pointer => K::Pointer,
        Obj::Output => K::Output,
        Obj::WmBase => K::WmBase,
        Obj::XdgSurface(s) => K::XdgSurface(*s),
        Obj::Toplevel(s) => K::Toplevel(*s),
        Obj::Positioner => K::Positioner,
    }
}

struct Pool {
    fd: RawFd,
    ptr: *const u8,
    size: usize,
}

impl Pool {
    fn map(fd: RawFd, size: usize) -> Option<Pool> {
        let p = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        if p == libc::MAP_FAILED {
            return None;
        }
        Some(Pool { fd, ptr: p as *const u8, size })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut _, self.size);
            libc::close(self.fd);
        }
    }
}

#[derive(Clone)]
struct Buffer {
    pool: u32,
    offset: usize,
    w: usize,
    h: usize,
    stride: usize,
}

#[derive(Default)]
struct Surface {
    /// attach されたバッファ (Some(0) は外す)
    pending: Option<u32>,
    pending_frames: Vec<u32>,
    /// 写した絵
    image: Vec<u32>,
    iw: usize,
    ih: usize,
    xdg: Option<u32>,
    toplevel: Option<u32>,
    title: String,
    /// 最後に送った configure の大きさと、作業中か
    sent: Option<(i32, i32, bool)>,
}

struct Client {
    conn: Conn,
    objs: HashMap<u32, Obj>,
    surfaces: HashMap<u32, Surface>,
    keyboards: Vec<u32>,
    pointers: Vec<u32>,
    dead: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Win {
    client: usize,
    surface: u32,
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

enum Action {
    Exec(String),
    Kill,
    Focus(i32),
    Move(i32),
    Exit,
}

struct Binding {
    mods: u32,
    code: u16,
    action: Action,
}

struct Config {
    binds: Vec<Binding>,
    layout: String,
    autostart: Vec<String>,
    bg: u32,
}

struct Wm {
    fb: fb::Fb,
    text: Option<text::Text>,
    listen: RawFd,
    inputs: input::Inputs,
    clients: BTreeMap<usize, Client>,
    next_client: usize,
    wins: Vec<Win>,
    focus: Option<Win>,
    serial: u32,
    mods: Mods,
    /// bindsym で使ったキー (離したときもクライアントに送らない)
    eaten: Vec<u16>,
    ptr: (i32, i32),
    ptr_shown: bool,
    ptr_win: Option<Win>,
    abs_max: (i32, i32),
    config: Config,
    /// 描きなおす行 [y0, y1)
    dirty: Option<(i32, i32)>,
    /// 次に画面を出したあと done を送る frame コールバック
    frames: Vec<(usize, u32)>,
    keymap_fd: RawFd,
    env: Vec<(String, String)>,
    quit: bool,
}

fn main() {
    let fb = fb::Fb::open().unwrap_or_else(|e| {
        eprintln!("aiwm: /dev/fb0: {} (sudo modprobe virtio_gpu、窓は AIOS_DISPLAY=1)", e);
        exit(1)
    });
    let inputs = input::Inputs::open();
    if inputs.is_empty() {
        eprintln!("aiwm: no /dev/input/event* (sudo modprobe virtio_input)");
    }
    let config = load_config();
    let (dir, listen) = open_socket().unwrap_or_else(|e| {
        eprintln!("aiwm: {}", e);
        exit(1)
    });
    let keymap_fd = unsafe { libc::memfd_create(c"aiwm-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    let mut env = vec![("XDG_RUNTIME_DIR".to_string(), dir.clone()), ("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string())];
    if !config.layout.is_empty() {
        env.push(("XKB_DEFAULT_LAYOUT".into(), config.layout.clone()));
    }
    let (w, h) = (fb.width as i32, fb.height as i32);
    let mut wm = Wm {
        fb,
        text: text::Text::load(text::FONT).ok(),
        listen,
        abs_max: input_abs_max(&inputs),
        inputs,
        clients: BTreeMap::new(),
        next_client: 1,
        wins: vec![],
        focus: None,
        serial: 1,
        mods: Mods::default(),
        eaten: vec![],
        ptr: (w / 2, h / 2),
        ptr_shown: false,
        ptr_win: None,
        config,
        dirty: Some((0, h)),
        frames: vec![],
        keymap_fd,
        env,
        quit: false,
    };
    eprintln!("aiwm: {}x{}, WAYLAND_DISPLAY={}/wayland-0", w, h, dir);
    for cmd in wm.config.autostart.clone() {
        wm.spawn(&cmd);
    }
    wm.run();
    wm.fb.fill(0);
    wm.fb.present();
    let _ = std::fs::remove_file(format!("{}/wayland-0", dir));
}

/// $XDG_RUNTIME_DIR (なければ /run/user/UID、だめなら /tmp/runtime-UID) に wayland-0 を作って listen
fn open_socket() -> Result<(String, RawFd), String> {
    let uid = unsafe { libc::getuid() };
    let dirs: Vec<String> = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => vec![d],
        _ => vec![format!("/run/user/{}", uid), format!("/tmp/runtime-{}", uid)],
    };
    let mut last = String::new();
    for dir in dirs {
        if std::fs::create_dir_all(&dir).is_err() {
            last = format!("{}: cannot create", dir);
            continue;
        }
        unsafe { libc::chmod(format!("{}\0", dir).as_ptr() as *const _, 0o700) };
        let path = format!("{}/wayland-0", dir);
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
        let (a, len) = wl::sockaddr(&path).map_err(|e| e.to_string())?;
        if unsafe { libc::bind(fd, &a as *const _ as *const libc::sockaddr, len) } != 0 || unsafe { libc::listen(fd, 16) } != 0 {
            last = format!("{}: {} (another aiwm?)", path, std::io::Error::last_os_error());
            unsafe { libc::close(fd) };
            continue;
        }
        return Ok((dir, fd));
    }
    Err(last)
}

fn input_abs_max(_inputs: &input::Inputs) -> (i32, i32) {
    // virtio-tablet は 0..32767
    (32767, 32767)
}

// ---- 設定 ----

const DEFAULT_CONFIG: &str = include_str!("../../etc/aiwm/config");

fn load_config() -> Config {
    let home = std::env::var("HOME").unwrap_or_default();
    let text = [format!("{}/.config/aiwm/config", home), "/etc/aiwm/config".to_string()]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());
    parse_config(&text)
}

fn parse_config(text: &str) -> Config {
    let mut vars: Vec<(String, String)> = vec![];
    let mut c = Config { binds: vec![], layout: String::new(), autostart: vec![], bg: DESK };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut line = line.to_string();
        // 長い名前から置きかえる ($mod と $mod2 など)
        let mut vs = vars.clone();
        vs.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        for (k, v) in &vs {
            line = line.replace(k.as_str(), v);
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["set", k, v @ ..] => vars.push((k.to_string(), v.join(" "))),
            ["bindsym", combo, cmd @ ..] => {
                let Some((mods, code)) = parse_combo(combo) else {
                    eprintln!("aiwm: config: unknown key {}", combo);
                    continue;
                };
                let action = match cmd {
                    ["exec", rest @ ..] => Action::Exec(rest.join(" ")),
                    ["kill"] => Action::Kill,
                    ["focus", "left" | "up" | "prev"] => Action::Focus(-1),
                    ["focus", "right" | "down" | "next"] => Action::Focus(1),
                    ["move", "left" | "up"] => Action::Move(-1),
                    ["move", "right" | "down"] => Action::Move(1),
                    ["exit"] => Action::Exit,
                    _ => {
                        eprintln!("aiwm: config: unknown command {}", cmd.join(" "));
                        continue;
                    }
                };
                c.binds.push(Binding { mods, code, action });
            }
            ["exec", rest @ ..] => c.autostart.push(rest.join(" ")),
            ["input", _, "xkb_layout", l] => c.layout = l.to_string(),
            ["output", _, "bg", color, ..] => {
                if let Ok(v) = u32::from_str_radix(color.trim_start_matches('#'), 16) {
                    c.bg = v;
                }
            }
            _ => eprintln!("aiwm: config: ignored: {}", line),
        }
    }
    c
}

fn parse_combo(s: &str) -> Option<(u32, u16)> {
    let parts: Vec<&str> = s.split('+').collect();
    let (key, mods) = parts.split_last()?;
    let mut m = 0;
    for p in mods {
        m |= match p.to_ascii_lowercase().as_str() {
            "shift" => keys::MOD_SHIFT,
            "ctrl" | "control" => keys::MOD_CTRL,
            "mod1" | "alt" => keys::MOD_ALT,
            "mod4" | "super" | "logo" => keys::MOD_LOGO,
            _ => return None,
        };
    }
    Some((m, keys::code_of_name(key)?))
}

// ---- 本体 ----

impl Wm {
    fn width(&self) -> i32 {
        self.fb.width as i32
    }
    fn height(&self) -> i32 {
        self.fb.height as i32
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    fn spawn(&self, cmd: &str) {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(format!("exec {}", cmd));
        for (k, v) in &self.env {
            c.env(k, v);
        }
        if let Err(e) = c.spawn() {
            eprintln!("aiwm: exec {}: {}", cmd, e);
        }
    }

    fn mark(&mut self, y0: i32, y1: i32) {
        let (y0, y1) = (y0.max(0), y1.min(self.height()));
        if y0 >= y1 {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some((a, b)) => (a.min(y0), b.max(y1)),
            None => (y0, y1),
        });
    }

    fn mark_all(&mut self) {
        let h = self.height();
        self.mark(0, h);
    }

    fn run(&mut self) {
        let mut last_frame = 0u32;
        while !self.quit {
            // 待つもの: listen、クライアント、入力
            let mut pfds = vec![libc::pollfd { fd: self.listen, events: libc::POLLIN, revents: 0 }];
            let ids: Vec<usize> = self.clients.keys().copied().collect();
            for id in &ids {
                let c = &self.clients[id];
                let ev = libc::POLLIN | if c.conn.pending_out() { libc::POLLOUT } else { 0 };
                pfds.push(libc::pollfd { fd: c.conn.fd, events: ev, revents: 0 });
            }
            let in_fds = self.inputs.fds();
            for fd in &in_fds {
                pfds.push(libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 });
            }
            // 描くものがあれば、前の画面から 16ms たったら描く
            let timeout = if self.dirty.is_some() { (16 - wl::now_ms().wrapping_sub(last_frame) as i32).clamp(0, 16) } else { -1 };
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, timeout) };
            if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                eprintln!("aiwm: poll: {}", std::io::Error::last_os_error());
                break;
            }
            if pfds[0].revents & libc::POLLIN != 0 {
                self.accept();
            }
            for (i, id) in ids.iter().enumerate() {
                let r = pfds[1 + i].revents;
                if r != 0 {
                    self.client_ready(*id);
                }
            }
            for (i, _) in in_fds.iter().enumerate() {
                if pfds[1 + ids.len() + i].revents & libc::POLLIN != 0 {
                    for e in self.inputs.read(i) {
                        self.input(e);
                    }
                }
            }
            if self.dirty.is_some() && wl::now_ms().wrapping_sub(last_frame) >= 16 {
                self.render();
                last_frame = wl::now_ms();
                let frames = std::mem::take(&mut self.frames);
                for (cid, cb) in frames {
                    if let Some(c) = self.clients.get_mut(&cid) {
                        c.conn.send(cb, 0, &[Arg::U(last_frame)]);
                        c.conn.send(1, 1, &[Arg::U(cb)]);
                        c.objs.remove(&cb);
                    }
                }
            }
            for c in self.clients.values_mut() {
                if c.conn.flush().is_err() {
                    c.dead = true;
                }
            }
            let dead: Vec<usize> = self.clients.iter().filter(|(_, c)| c.dead).map(|(k, _)| *k).collect();
            for id in dead {
                self.drop_client(id);
            }
            // 終わった子を片づける
            while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        }
    }

    fn accept(&mut self) {
        loop {
            let fd = unsafe { libc::accept4(self.listen, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
            if fd < 0 {
                return;
            }
            let mut objs = HashMap::new();
            objs.insert(1, Obj::Display);
            let id = self.next_client;
            self.next_client += 1;
            self.clients.insert(id, Client { conn: Conn::new(fd), objs, surfaces: HashMap::new(), keyboards: vec![], pointers: vec![], dead: false });
        }
    }

    fn client_ready(&mut self, id: usize) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        match c.conn.recv() {
            Ok(true) => {}
            _ => {
                c.dead = true;
                return;
            }
        }
        loop {
            let Some(c) = self.clients.get_mut(&id) else { return };
            if c.dead {
                return;
            }
            let Some(m) = c.conn.next() else { return };
            self.request(id, m);
        }
    }

    fn drop_client(&mut self, id: usize) {
        self.clients.remove(&id);
        let before = self.wins.len();
        self.wins.retain(|w| w.client != id);
        if self.focus.is_some_and(|f| f.client == id) {
            self.focus = None;
        }
        if self.ptr_win.is_some_and(|f| f.client == id) {
            self.ptr_win = None;
        }
        self.frames.retain(|(c, _)| *c != id);
        if self.wins.len() != before {
            self.relayout();
        }
    }

    // ---- 要求 ----

    fn request(&mut self, cid: usize, mut m: Msg) {
        let serial_now = self.serial;
        let c = self.clients.get_mut(&cid).unwrap();
        let Some(obj) = c.objs.get(&m.id).map(kind) else {
            // 消したばかりのものへの要求は捨てる
            return;
        };
        macro_rules! new_obj {
            ($o:expr) => {{
                let nid = m.uint();
                c.objs.insert(nid, $o);
                nid
            }};
        }
        match (obj, m.op) {
            (K::Display, 0) => {
                // sync
                let cb = m.uint();
                c.conn.send(cb, 0, &[Arg::U(serial_now)]);
                c.conn.send(1, 1, &[Arg::U(cb)]);
            }
            (K::Display, 1) => {
                let reg = new_obj!(Obj::Registry);
                for (name, iface, ver) in GLOBALS {
                    c.conn.send(reg, 0, &[Arg::U(name), Arg::S(iface), Arg::U(ver)]);
                }
            }
            (K::Registry, 0) => {
                let name = m.uint();
                let _iface = m.string();
                let _ver = m.uint();
                let o = match name {
                    1 => Obj::Compositor,
                    2 => Obj::Shm,
                    3 => Obj::Seat,
                    4 => Obj::Output,
                    5 => Obj::WmBase,
                    _ => {
                        c.dead = true;
                        return;
                    }
                };
                let nid = new_obj!(o);
                match name {
                    2 => {
                        c.conn.send(nid, 0, &[Arg::U(0)]);
                        c.conn.send(nid, 0, &[Arg::U(1)]);
                    }
                    3 => {
                        c.conn.send(nid, 0, &[Arg::U(3)]);
                        c.conn.send(nid, 1, &[Arg::S("seat0")]);
                    }
                    4 => {
                        let (w, h) = (self.fb.width as i32, self.fb.height as i32);
                        c.conn.send(nid, 0, &[Arg::I(0), Arg::I(0), Arg::I(w * 254 / 960), Arg::I(h * 254 / 960), Arg::I(0), Arg::S("aios"), Arg::S("virtio-gpu"), Arg::I(0)]);
                        c.conn.send(nid, 1, &[Arg::U(3), Arg::I(w), Arg::I(h), Arg::I(60000)]);
                        c.conn.send(nid, 3, &[Arg::I(1)]);
                        c.conn.send(nid, 2, &[]);
                    }
                    _ => {}
                }
            }
            (K::Compositor, 0) => {
                let sid = new_obj!(Obj::Surface);
                c.surfaces.insert(sid, Surface::default());
            }
            (K::Compositor, 1) => {
                new_obj!(Obj::Region);
            }
            (K::Region, 0) | (K::Callback, _) | (K::Positioner, 0) => self.destroy(cid, m.id),
            (K::Region, _) | (K::Positioner, _) => {}
            (K::Shm, 0) => {
                let nid = m.uint();
                let fd = c.conn.take_fd();
                let size = m.int().max(0) as usize;
                match fd.and_then(|fd| Pool::map(fd, size)) {
                    Some(p) => {
                        c.objs.insert(nid, Obj::Pool(p));
                    }
                    None => c.dead = true,
                }
            }
            (K::Pool, 0) => {
                let nid = m.uint();
                let (offset, w, h, stride, _format) = (m.int(), m.int(), m.int(), m.int(), m.uint());
                let pool = m.id;
                if offset < 0 || w <= 0 || h <= 0 || stride < w * 4 {
                    c.dead = true;
                    return;
                }
                c.objs.insert(nid, Obj::Buffer(Buffer { pool, offset: offset as usize, w: w as usize, h: h as usize, stride: stride as usize }));
            }
            (K::Pool, 1) => self.destroy(cid, m.id),
            (K::Pool, 2) => {
                let size = m.int().max(0) as usize;
                let Some(Obj::Pool(p)) = c.objs.get(&m.id) else { return };
                let fd = unsafe { libc::dup(p.fd) };
                if let Some(np) = Pool::map(fd, size) {
                    c.objs.insert(m.id, Obj::Pool(np));
                }
            }
            (K::Buffer, 0) => self.destroy(cid, m.id),
            (K::Surface, 0) => {
                let sid = m.id;
                self.destroy(cid, sid);
                let c = self.clients.get_mut(&cid).unwrap();
                c.surfaces.remove(&sid);
                self.unmap(Win { client: cid, surface: sid });
            }
            (K::Surface, 1) => {
                let b = m.uint();
                if let Some(s) = c.surfaces.get_mut(&m.id) {
                    s.pending = Some(b);
                }
            }
            (K::Surface, 3) => {
                let cb = new_obj!(Obj::Callback);
                if let Some(s) = c.surfaces.get_mut(&m.id) {
                    s.pending_frames.push(cb);
                }
            }
            (K::Surface, 6) => self.commit(cid, m.id),
            (K::Surface, _) => {}
            (K::Seat, 0) => {
                let p = new_obj!(Obj::Pointer);
                c.pointers.push(p);
            }
            (K::Seat, 1) => {
                let k = new_obj!(Obj::Keyboard);
                c.keyboards.push(k);
                c.conn.send(k, 0, &[Arg::U(0), Arg::Fd(self.keymap_fd), Arg::U(0)]);
                c.conn.send(k, 5, &[Arg::I(25), Arg::I(600)]);
                // すでに作業中の窓なら、すぐ enter
                if let Some(f) = self.focus.filter(|f| f.client == cid) {
                    let s = self.next_serial();
                    let (d, l) = self.mods.mask();
                    let c = self.clients.get_mut(&cid).unwrap();
                    c.conn.send(k, 1, &[Arg::U(s), Arg::O(f.surface), Arg::A(&[])]);
                    c.conn.send(k, 4, &[Arg::U(s), Arg::U(d), Arg::U(0), Arg::U(l), Arg::U(0)]);
                }
            }
            (K::Seat, 3) => self.destroy(cid, m.id),
            (K::Seat, _) => {}
            (K::Keyboard, 0) => {
                let id = m.id;
                c.keyboards.retain(|&k| k != id);
                self.destroy(cid, id);
            }
            (K::Pointer, 1) => {
                let id = m.id;
                c.pointers.retain(|&k| k != id);
                self.destroy(cid, id);
            }
            (K::Pointer, _) | (K::Keyboard, _) => {}
            (K::Output, 0) => self.destroy(cid, m.id),
            (K::Output, _) => {}
            (K::WmBase, 0) => self.destroy(cid, m.id),
            (K::WmBase, 1) => {
                new_obj!(Obj::Positioner);
            }
            (K::WmBase, 2) => {
                let nid = m.uint();
                let sid = m.uint();
                c.objs.insert(nid, Obj::XdgSurface(sid));
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.xdg = Some(nid);
                }
            }
            (K::WmBase, _) => {}
            (K::XdgSurface(_), 0) => self.destroy(cid, m.id),
            (K::XdgSurface(sid), 1) => {
                let nid = m.uint();
                c.objs.insert(nid, Obj::Toplevel(sid));
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.toplevel = Some(nid);
                }
            }
            (K::XdgSurface(_), _) => {}
            (K::Toplevel(sid), 0) => {
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.toplevel = None;
                    s.sent = None;
                }
                self.destroy(cid, m.id);
                self.unmap(Win { client: cid, surface: sid });
            }
            (K::Toplevel(sid), 2) => {
                let t = m.string();
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.title = t;
                }
            }
            (K::Toplevel(_), _) => {}
            #[allow(unreachable_patterns)]
            _ => {
                eprintln!("aiwm: client {}: unknown request {} on {}", cid, m.op, m.id);
            }
        }
    }

    /// クライアントが消したもの: delete_id を返す
    fn destroy(&mut self, cid: usize, id: u32) {
        let c = self.clients.get_mut(&cid).unwrap();
        c.objs.remove(&id);
        if id < 0xff00_0000 {
            c.conn.send(1, 1, &[Arg::U(id)]);
        }
    }

    fn commit(&mut self, cid: usize, sid: u32) {
        let c = self.clients.get_mut(&cid).unwrap();
        let Some(s) = c.surfaces.get_mut(&sid) else { return };
        for cb in s.pending_frames.drain(..) {
            self.frames.push((cid, cb));
        }
        if let Some(bid) = s.pending.take() {
            if bid == 0 {
                s.image.clear();
                s.iw = 0;
                s.ih = 0;
            } else if let Some(Obj::Buffer(b)) = c.objs.get(&bid) {
                if let Some(Obj::Pool(p)) = c.objs.get(&b.pool) {
                    if b.offset + b.stride * (b.h - 1) + b.w * 4 <= p.size {
                        s.image.resize(b.w * b.h, 0);
                        for y in 0..b.h {
                            let src = unsafe { std::slice::from_raw_parts(p.ptr.add(b.offset + y * b.stride) as *const u32, b.w) };
                            s.image[y * b.w..(y + 1) * b.w].copy_from_slice(src);
                        }
                        s.iw = b.w;
                        s.ih = b.h;
                    }
                }
                c.conn.send(bid, 0, &[]);
            }
        }
        let is_top = s.toplevel.is_some();
        let w = Win { client: cid, surface: sid };
        if is_top && !self.wins.contains(&w) {
            // 新しい窓: 作業中にして並べなおす (ここで初めての configure を送る)
            let at = self.focus.and_then(|f| self.wins.iter().position(|x| *x == f)).map_or(self.wins.len(), |i| i + 1);
            self.wins.insert(at, w);
            self.set_focus(Some(w));
            self.relayout();
        } else if let Some(r) = self.rect_of(w) {
            self.mark(r.y, r.y + r.h);
        }
        if !is_top {
            // 役割のない surface (カーソルなど) は、frame だけすぐ返す
        }
    }

    fn unmap(&mut self, w: Win) {
        if let Some(i) = self.wins.iter().position(|x| *x == w) {
            self.wins.remove(i);
            if self.focus == Some(w) {
                let next = if self.wins.is_empty() { None } else { Some(self.wins[i.min(self.wins.len() - 1)]) };
                self.focus = None;
                self.set_focus(next);
            }
            if self.ptr_win == Some(w) {
                self.ptr_win = None;
            }
            self.relayout();
        }
    }

    // ---- 並べ方 ----

    /// 窓 (枠の内側) の場所
    fn rect_of(&self, w: Win) -> Option<Rect> {
        let i = self.wins.iter().position(|x| *x == w)?;
        let n = self.wins.len() as i32;
        let (sw, sh) = (self.width(), self.height());
        let x0 = GAP + (sw - GAP) * i as i32 / n;
        let x1 = (sw - GAP) * (i as i32 + 1) / n;
        Some(Rect { x: x0 + BORDER, y: GAP + BORDER, w: (x1 - x0 - 2 * BORDER).max(1), h: (sh - 2 * GAP - 2 * BORDER).max(1) })
    }

    /// 大きさや作業中かが変わった窓に configure を送る
    fn relayout(&mut self) {
        for w in self.wins.clone() {
            let Some(r) = self.rect_of(w) else { continue };
            let active = self.focus == Some(w);
            let serial = self.next_serial();
            let Some(c) = self.clients.get_mut(&w.client) else { continue };
            let Some(s) = c.surfaces.get_mut(&w.surface) else { continue };
            if s.sent == Some((r.w, r.h, active)) {
                continue;
            }
            s.sent = Some((r.w, r.h, active));
            let (Some(top), Some(xdg)) = (s.toplevel, s.xdg) else { continue };
            // 状態: activated (4)、tiled left/right/top/bottom (5..8)
            let mut states = vec![];
            for st in if active { vec![4u32, 5, 6, 7, 8] } else { vec![5, 6, 7, 8] } {
                states.extend_from_slice(&st.to_le_bytes());
            }
            c.conn.send(top, 0, &[Arg::I(r.w), Arg::I(r.h), Arg::A(&states)]);
            c.conn.send(xdg, 0, &[Arg::U(serial)]);
        }
        self.mark_all();
    }

    fn set_focus(&mut self, w: Option<Win>) {
        if self.focus == w {
            return;
        }
        let serial = self.next_serial();
        if let Some(old) = self.focus {
            if let Some(c) = self.clients.get_mut(&old.client) {
                for k in c.keyboards.clone() {
                    c.conn.send(k, 2, &[Arg::U(serial), Arg::O(old.surface)]);
                }
            }
        }
        self.focus = w;
        if let Some(new) = w {
            let (d, l) = self.mods.mask();
            if let Some(c) = self.clients.get_mut(&new.client) {
                for k in c.keyboards.clone() {
                    c.conn.send(k, 1, &[Arg::U(serial), Arg::O(new.surface), Arg::A(&[])]);
                    c.conn.send(k, 4, &[Arg::U(serial), Arg::U(d), Arg::U(0), Arg::U(l), Arg::U(0)]);
                }
            }
        }
        self.relayout();
    }

    // ---- 入力 ----

    fn input(&mut self, e: input::Event) {
        const EV_SYN: u16 = 0;
        const EV_REL: u16 = 2;
        const EV_ABS: u16 = 3;
        match e.typ {
            input::EV_KEY if e.code >= 0x100 => self.button(e.code, e.value),
            input::EV_KEY => self.key(e.code, e.value),
            EV_ABS => {
                let (w, h) = (self.width(), self.height());
                let old = self.ptr;
                match e.code {
                    0 => self.ptr.0 = (e.value as i64 * (w - 1) as i64 / self.abs_max.0 as i64) as i32,
                    1 => self.ptr.1 = (e.value as i64 * (h - 1) as i64 / self.abs_max.1 as i64) as i32,
                    _ => return,
                }
                self.pointer_moved(old);
            }
            EV_REL => {
                let old = self.ptr;
                match e.code {
                    0 => self.ptr.0 = (self.ptr.0 + e.value).clamp(0, self.width() - 1),
                    1 => self.ptr.1 = (self.ptr.1 + e.value).clamp(0, self.height() - 1),
                    _ => return,
                }
                self.pointer_moved(old);
            }
            EV_SYN => {}
            _ => {}
        }
    }

    fn key(&mut self, code: u16, value: i32) {
        // value: 1 押した、0 離した、2 押しっぱなし (クライアントが自分でくり返すので送らない)
        if value == 2 {
            return;
        }
        let pressed = value == 1;
        if self.mods.update(code, pressed) {
            let (d, l) = self.mods.mask();
            if let Some(f) = self.focus {
                let serial = self.next_serial();
                if let Some(c) = self.clients.get_mut(&f.client) {
                    for k in c.keyboards.clone() {
                        c.conn.send(k, 4, &[Arg::U(serial), Arg::U(d), Arg::U(0), Arg::U(l), Arg::U(0)]);
                    }
                }
            }
        } else if pressed {
            let (d, _) = self.mods.mask();
            if let Some(i) = self.config.binds.iter().position(|b| b.code == code && b.mods == d) {
                self.eaten.push(code);
                self.run_action(i);
                return;
            }
        } else if let Some(i) = self.eaten.iter().position(|&k| k == code) {
            self.eaten.remove(i);
            return;
        }
        if let Some(f) = self.focus {
            let serial = self.next_serial();
            if let Some(c) = self.clients.get_mut(&f.client) {
                for k in c.keyboards.clone() {
                    c.conn.send(k, 3, &[Arg::U(serial), Arg::U(wl::now_ms()), Arg::U(code as u32), Arg::U(pressed as u32)]);
                }
            }
        }
    }

    fn run_action(&mut self, i: usize) {
        match &self.config.binds[i].action {
            Action::Exec(cmd) => {
                let cmd = cmd.clone();
                self.spawn(&cmd);
            }
            Action::Kill => {
                if let Some(f) = self.focus {
                    if let Some(c) = self.clients.get_mut(&f.client) {
                        if let Some(top) = c.surfaces.get(&f.surface).and_then(|s| s.toplevel) {
                            c.conn.send(top, 1, &[]);
                        }
                    }
                }
            }
            Action::Focus(d) => {
                let d = *d;
                if let Some(i) = self.focus.and_then(|f| self.wins.iter().position(|x| *x == f)) {
                    let n = self.wins.len() as i32;
                    let j = (i as i32 + d).rem_euclid(n) as usize;
                    let w = self.wins[j];
                    self.set_focus(Some(w));
                }
            }
            Action::Move(d) => {
                let d = *d;
                if let Some(i) = self.focus.and_then(|f| self.wins.iter().position(|x| *x == f)) {
                    let j = i as i32 + d;
                    if j >= 0 && (j as usize) < self.wins.len() {
                        self.wins.swap(i, j as usize);
                        self.relayout();
                    }
                }
            }
            Action::Exit => self.quit = true,
        }
    }

    fn win_at(&self, x: i32, y: i32) -> Option<(Win, Rect)> {
        self.wins.iter().find_map(|w| self.rect_of(*w).filter(|r| r.contains(x, y)).map(|r| (*w, r)))
    }

    fn pointer_moved(&mut self, old: (i32, i32)) {
        self.ptr_shown = true;
        self.mark(old.1 - 1, old.1 + CURSOR_H + 1);
        self.mark(self.ptr.1 - 1, self.ptr.1 + CURSOR_H + 1);
        let hit = self.win_at(self.ptr.0, self.ptr.1);
        let now = hit.map(|(w, _)| w);
        let serial = self.next_serial();
        if now != self.ptr_win {
            if let Some(o) = self.ptr_win {
                if let Some(c) = self.clients.get_mut(&o.client) {
                    for p in c.pointers.clone() {
                        c.conn.send(p, 1, &[Arg::U(serial), Arg::O(o.surface)]);
                        c.conn.send(p, 5, &[]);
                    }
                }
            }
            if let Some((n, r)) = hit {
                if let Some(c) = self.clients.get_mut(&n.client) {
                    for p in c.pointers.clone() {
                        c.conn.send(p, 0, &[Arg::U(serial), Arg::O(n.surface), Arg::F((self.ptr.0 - r.x) as f64), Arg::F((self.ptr.1 - r.y) as f64)]);
                        c.conn.send(p, 5, &[]);
                    }
                }
            }
            self.ptr_win = now;
        } else if let Some((n, r)) = hit {
            if let Some(c) = self.clients.get_mut(&n.client) {
                for p in c.pointers.clone() {
                    c.conn.send(p, 2, &[Arg::U(wl::now_ms()), Arg::F((self.ptr.0 - r.x) as f64), Arg::F((self.ptr.1 - r.y) as f64)]);
                    c.conn.send(p, 5, &[]);
                }
            }
        }
    }

    fn button(&mut self, code: u16, value: i32) {
        if value == 2 {
            return;
        }
        if value == 1 {
            if let Some((w, _)) = self.win_at(self.ptr.0, self.ptr.1) {
                self.set_focus(Some(w));
            }
        }
        if let Some(w) = self.ptr_win {
            let serial = self.next_serial();
            if let Some(c) = self.clients.get_mut(&w.client) {
                for p in c.pointers.clone() {
                    c.conn.send(p, 3, &[Arg::U(serial), Arg::U(wl::now_ms()), Arg::U(code as u32), Arg::U(value as u32)]);
                    c.conn.send(p, 5, &[]);
                }
            }
        }
    }

    // ---- 描く ----

    fn render(&mut self) {
        let Some((y0, y1)) = self.dirty.take() else { return };
        let (sw, stride) = (self.fb.width, self.fb.stride);
        let bg = self.config.bg;
        {
            let px = self.fb.pixels();
            for y in y0..y1 {
                px[y as usize * stride..y as usize * stride + sw].fill(bg);
            }
        }
        if self.wins.is_empty() {
            self.draw_hint(y0, y1);
        }
        for w in self.wins.clone() {
            let Some(r) = self.rect_of(w) else { continue };
            let color = if self.focus == Some(w) { FOCUS } else { UNFOCUS };
            // 枠
            let outer = Rect { x: r.x - BORDER, y: r.y - BORDER, w: r.w + 2 * BORDER, h: r.h + 2 * BORDER };
            self.fill_rect(outer, color, y0, y1);
            self.fill_rect(r, term_bg(), y0, y1);
            let Some(s) = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)) else { continue };
            let (iw, ih) = (s.iw as i32, s.ih as i32);
            let px = unsafe { std::slice::from_raw_parts_mut(self.fb.pixels().as_mut_ptr(), self.fb.pixels().len()) };
            for y in r.y.max(y0)..(r.y + r.h.min(ih)).min(y1) {
                let sy = (y - r.y) as usize;
                let n = r.w.min(iw) as usize;
                let dst = y as usize * stride + r.x as usize;
                px[dst..dst + n].copy_from_slice(&s.image[sy * s.iw..sy * s.iw + n]);
            }
        }
        if self.ptr_shown {
            self.draw_cursor(y0, y1);
        }
        self.fb.present_rows(y0 as usize, y1 as usize);
    }

    fn fill_rect(&mut self, r: Rect, rgb: u32, y0: i32, y1: i32) {
        let (sw, sh, stride) = (self.width(), self.height(), self.fb.stride);
        let (x0, x1) = (r.x.max(0), (r.x + r.w).min(sw));
        if x0 >= x1 {
            return;
        }
        let px = self.fb.pixels();
        for y in r.y.max(y0).max(0)..(r.y + r.h).min(y1).min(sh) {
            px[y as usize * stride + x0 as usize..y as usize * stride + x1 as usize].fill(rgb);
        }
    }

    fn draw_hint(&mut self, y0: i32, y1: i32) {
        let Some(t) = self.text.take() else { return };
        let h = self.height();
        let key = self
            .config
            .binds
            .iter()
            .find(|b| matches!(&b.action, Action::Exec(c) if c.starts_with("aiterm")))
            .map(|b| combo_name(b.mods, b.code))
            .unwrap_or_default();
        let lines = [("aiwm".to_string(), 48.0, FOCUS), (if key.is_empty() { String::new() } else { format!("{}  terminal", key) }, 18.0, 0x8090b0)];
        let mut y = h / 2;
        for (s, size, color) in lines {
            if y - 60 < y1 && y + 20 > y0 {
                t.center(&mut self.fb, &s, y, size, color);
            }
            y += 40;
        }
        self.text = Some(t);
    }

    fn draw_cursor(&mut self, y0: i32, y1: i32) {
        let (px0, py0) = self.ptr;
        for (dy, row) in CURSOR.iter().enumerate() {
            let y = py0 + dy as i32;
            if y < y0 || y >= y1 {
                continue;
            }
            for (dx, ch) in row.bytes().enumerate() {
                let c = match ch {
                    b'#' => 0x000000,
                    b'.' => 0xffffff,
                    _ => continue,
                };
                self.fb.blend(px0 + dx as i32, y, c, 255);
            }
        }
    }
}

fn term_bg() -> u32 {
    0x161821
}

fn combo_name(mods: u32, code: u16) -> String {
    let mut s = String::new();
    for (m, n) in [(keys::MOD_LOGO, "Super"), (keys::MOD_CTRL, "Ctrl"), (keys::MOD_ALT, "Alt"), (keys::MOD_SHIFT, "Shift")] {
        if mods & m != 0 {
            s.push_str(n);
            s.push('+');
        }
    }
    s.push_str(match code {
        keys::KEY_ENTER => "Enter",
        _ => "?",
    });
    s
}

const CURSOR_H: i32 = 17;
const CURSOR: [&str; 17] = [
    "#", "##", "#.#", "#..#", "#...#", "#....#", "#.....#", "#......#", "#.......#", "#........#", "#.....####", "#..#..#", "#.# #..#", "##  #..#", "#    #..#", "     #..#",
    "      ##",
];
