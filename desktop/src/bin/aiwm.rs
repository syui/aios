// aiwm: aios のタイル型 Wayland コンポジタ (sway の小さな親戚)
//   画面 (/dev/fb0) とキーボード・マウス (/dev/input/event*) を持ち、
//   $XDG_RUNTIME_DIR/wayland-0 で Wayland のクライアント (aiterm など) を待つ。
//   窓は横に並べる (sway の splith)。設定は ~/.config/aiwm/config か /etc/aiwm/config (sway と同じ書き方)
//
// できる Wayland: wl_compositor, wl_subcompositor, wl_shm, wl_seat (キーボード、ポインタ), wl_output,
//   xdg_wm_base (toplevel と popup: メニューや候補の一覧)。
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
const BAR_H: i32 = 24;
const BAR_BG: u32 = 0x0b0d14;
const BAR_FONT: f32 = 14.0;
const TAB_H: i32 = 22;

// グローバル (wl_registry で見せるもの): (名前, インターフェース, 版)
const GLOBALS: [(u32, &str, u32); 7] = [
    (1, "wl_compositor", 4),
    (2, "wl_shm", 1),
    (3, "wl_seat", 5),
    (4, "wl_output", 2),
    (5, "xdg_wm_base", 2),
    // クリップボードとドラッグ (まだ中身は運ばない。GTK はこれがないと seat を作らない)
    (6, "wl_data_device_manager", 3),
    // 窓の中の子の窓 (Firefox はページの中身をこれに描く)
    (7, "wl_subcompositor", 1),
];

enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Shm,
    /// バッファは作ったあとプールが消されても使える (Rc で分けあう)
    Pool(std::rc::Rc<Pool>),
    Buffer(Buffer),
    Seat,
    Keyboard,
    Pointer,
    Output,
    WmBase,
    XdgSurface(u32),
    Toplevel(u32),
    Positioner(Positioner),
    /// xdg_popup の surface
    Popup(u32),
    DataManager,
    DataSource,
    DataDevice,
    Subcompositor,
    /// wl_subsurface: 子の surface
    Subsurface(u32),
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
    Popup(u32),
    DataManager,
    DataSource,
    DataDevice,
    Subcompositor,
    Subsurface(u32),
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
        Obj::Positioner(_) => K::Positioner,
        Obj::Popup(s) => K::Popup(*s),
        Obj::DataManager => K::DataManager,
        Obj::DataSource => K::DataSource,
        Obj::DataDevice => K::DataDevice,
        Obj::Subcompositor => K::Subcompositor,
        Obj::Subsurface(s) => K::Subsurface(*s),
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
    pool: std::rc::Rc<Pool>,
    offset: usize,
    w: usize,
    h: usize,
    stride: usize,
    /// wl_shm の形式: 0 = ARGB8888、1 = XRGB8888 (アルファは使わない)
    format: u32,
}

/// xdg_positioner: popup をどこに出すか (親の窓の中の四角 anchor_rect のどこに、どちら向きに)
#[derive(Clone, Copy, Default)]
struct Positioner {
    size: (i32, i32),
    anchor_rect: (i32, i32, i32, i32),
    anchor: u32,
    gravity: u32,
    offset: (i32, i32),
    adjust: u32,
}

impl Positioner {
    /// 親の窓 (window geometry) の中での popup の左上
    fn place(&self) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        let (w, h) = self.size;
        // anchor: 0 none 1 top 2 bottom 3 left 4 right 5 top_left 6 bottom_left 7 top_right 8 bottom_right (gravity も同じ)
        let px = match self.anchor {
            3 | 5 | 6 => ax,
            4 | 7 | 8 => ax + aw,
            _ => ax + aw / 2,
        };
        let py = match self.anchor {
            1 | 5 | 7 => ay,
            2 | 6 | 8 => ay + ah,
            _ => ay + ah / 2,
        };
        let x = match self.gravity {
            3 | 5 | 6 => px - w,
            4 | 7 | 8 => px,
            _ => px - w / 2,
        };
        let y = match self.gravity {
            1 | 5 | 7 => py - h,
            2 | 6 | 8 => py,
            _ => py - h / 2,
        };
        (x + self.offset.0, y + self.offset.1)
    }

    /// 上下をひっくり返したもの (下にはみ出すメニューを上に出す)
    fn flip_y(&self) -> Positioner {
        let f = |v: u32| match v {
            1 => 2,
            2 => 1,
            5 => 6,
            6 => 5,
            7 => 8,
            8 => 7,
            v => v,
        };
        Positioner { anchor: f(self.anchor), gravity: f(self.gravity), offset: (self.offset.0, -self.offset.1), ..*self }
    }
}

#[derive(Clone, Copy)]
struct Popup {
    id: u32,
    /// 親 (toplevel か、ほかの popup) の surface
    parent: u32,
    pos: Positioner,
    /// 親の窓 (window geometry) の中の場所と大きさ (configure で送ったもの)
    x: i32,
    y: i32,
    configured: bool,
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
    sent: Option<(i32, i32, bool, bool, u8)>,
    /// xdg_surface.set_window_geometry: 絵の中の「窓」の場所 (影などを除いたところ)
    geometry: Option<(i32, i32, i32, i32)>,
    /// subsurface なら親の surface と、親の中の場所
    parent: Option<u32>,
    pos: (i32, i32),
    /// 子を持つ surface の重なり順 (下から)。自分自身も入る。空なら自分だけ
    stack: Vec<u32>,
    popup: Option<Popup>,
    /// xdg_toplevel.set_parent の親 (ダイアログ)、set_app_id、set_min_size / set_max_size
    parent_top: Option<u32>,
    app_id: String,
    min_size: (i32, i32),
    max_size: (i32, i32),
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

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// 横に並べる (sway の splith)
    SplitH,
    /// 縦に積む (splitv)
    SplitV,
    /// タブ (tabbed): 作業中の窓だけを大きく、上にタブ
    Tabbed,
}

/// ワークスペース: 窓の並びと、その中の作業中の窓
struct Ws {
    wins: Vec<Win>,
    /// 浮いている窓 (タイルに入れない。ダイアログなど)。下から
    floats: Vec<Win>,
    focus: Option<Win>,
    layout: Layout,
    fullscreen: Option<Win>,
}

impl Ws {
    fn new() -> Ws {
        Ws { wins: vec![], floats: vec![], focus: None, layout: Layout::SplitH, fullscreen: None }
    }
}

enum Action {
    Exec(String),
    Kill,
    Focus(i32),
    Move(i32),
    Workspace(u32),
    MoveTo(u32),
    Layout(Option<Layout>),
    Fullscreen,
    /// floating toggle: 作業中の窓を浮かせる / タイルに戻す
    FloatToggle,
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
    /// バー: None なら出さない。(上か, status_command)
    bar: Option<(bool, Option<String>)>,
    /// floating_modifier: これを押しながらドラッグで浮いた窓を動かす
    float_mod: u32,
    /// for_window [app_id="..."] floating enable の app_id
    float_apps: Vec<String>,
}

struct Wm {
    fb: fb::Fb,
    text: Option<text::Text>,
    listen: RawFd,
    inputs: input::Inputs,
    clients: BTreeMap<usize, Client>,
    next_client: usize,
    /// ワークスペース (番号 → 中身) と、見えているもの
    spaces: BTreeMap<u32, Ws>,
    cur: u32,
    focus: Option<Win>,
    serial: u32,
    mods: Mods,
    /// bindsym で使ったキー (離したときもクライアントに送らない)
    eaten: Vec<u16>,
    ptr: (i32, i32),
    ptr_shown: bool,
    ptr_win: Option<Win>,
    /// 出ている popup (下から)
    popups: Vec<Win>,
    /// 浮いている窓の「窓」(window geometry) の左上
    float_at: HashMap<(usize, u32), (i32, i32)>,
    /// floating_modifier + ドラッグで動かしている窓と、押したところからのずれ
    drag: Option<(Win, i32, i32)>,
    abs_max: (i32, i32),
    config: Config,
    /// 描きなおす行 [y0, y1)
    dirty: Option<(i32, i32)>,
    /// 次に画面を出したあと done を送る frame コールバック
    frames: Vec<(usize, u32)>,
    /// xkb のキーマップ (memfd、NUL まで) と大きさ
    keymap_fd: RawFd,
    keymap_size: u32,
    env: Vec<(String, String)>,
    quit: bool,
    /// バーの右に出すもの (status_command の最後の行。なければ時計)
    status: String,
    status_fd: Option<RawFd>,
    status_buf: Vec<u8>,
    clock: String,
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
    let (keymap_fd, keymap_size) = make_keymap(&config.layout);
    let mut env = vec![("XDG_RUNTIME_DIR".to_string(), dir.clone()), ("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string())];
    if !config.layout.is_empty() {
        env.push(("XKB_DEFAULT_LAYOUT".into(), config.layout.clone()));
    }
    // GTK (firefox など、[c] のもの) は /opt/c/share の schema やアイコンを XDG_DATA_DIRS で探す
    if std::env::var_os("XDG_DATA_DIRS").is_none() {
        env.push(("XDG_DATA_DIRS".into(), "/opt/c/share:/usr/local/share:/usr/share".into()));
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
        spaces: BTreeMap::from([(1, Ws::new())]),
        cur: 1,
        focus: None,
        serial: 1,
        mods: Mods::default(),
        eaten: vec![],
        ptr: (w / 2, h / 2),
        ptr_shown: false,
        ptr_win: None,
        popups: vec![],
        float_at: HashMap::new(),
        drag: None,
        config,
        dirty: Some((0, h)),
        frames: vec![],
        keymap_fd,
        keymap_size,
        env,
        quit: false,
        status: String::new(),
        status_fd: None,
        status_buf: vec![],
        clock: String::new(),
    };
    if let Some((_, Some(cmd))) = wm.config.bar.clone() {
        wm.status_fd = wm.spawn_status(&cmd);
    }
    eprintln!("aiwm: {}x{}, WAYLAND_DISPLAY={}/wayland-0", w, h, dir);
    for cmd in wm.config.autostart.clone() {
        wm.spawn(&cmd);
    }
    wm.run();
    wm.fb.fill(0);
    wm.fb.present();
    let _ = std::fs::remove_file(format!("{}/wayland-0", dir));
}

/// キーボードの配列 (xkb_layout) の xkb キーマップを memfd に書く: (fd, NUL までの大きさ)。
/// 中身は desktop/share/xkb の、xkbcli compile-keymap で作ったもの (us と jp)
fn make_keymap(layout: &str) -> (RawFd, u32) {
    const US: &str = include_str!("../../share/xkb/us.xkb");
    const JP: &str = include_str!("../../share/xkb/jp.xkb");
    let text = match layout {
        "jp" => JP,
        "us" | "" => US,
        other => {
            eprintln!("aiwm: no xkb keymap for layout '{}', using us", other);
            US
        }
    };
    let fd = unsafe { libc::memfd_create(c"aiwm-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    let mut data = text.as_bytes().to_vec();
    data.push(0);
    if fd < 0 || unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) } != data.len() as isize {
        eprintln!("aiwm: cannot make the keymap: {}", std::io::Error::last_os_error());
        return (fd, 0);
    }
    (fd, data.len() as u32)
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
        if let Err(e) = std::fs::create_dir_all(&dir) {
            last = format!("{}: {}", dir, e);
            continue;
        }
        unsafe { libc::chmod(format!("{}\0", dir).as_ptr() as *const _, 0o700) };
        let path = format!("{}/wayland-0", dir);
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
        if fd < 0 {
            return Err(format!(
                "unix socket: {} (the kernel is too old: sudo aipkg -Syu unix, then reboot)",
                std::io::Error::last_os_error()
            ));
        }
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
    let mut c = Config { binds: vec![], layout: String::new(), autostart: vec![], bg: DESK, bar: None, float_mod: keys::MOD_ALT, float_apps: vec![] };
    let mut in_bar = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // bar { ... }
        if in_bar {
            let bar = c.bar.get_or_insert((true, None));
            let ws: Vec<&str> = line.split_whitespace().collect();
            match ws.as_slice() {
                ["}"] => in_bar = false,
                ["position", pos] => bar.0 = *pos != "bottom",
                ["status_command", rest @ ..] => bar.1 = Some(rest.join(" ")),
                _ => eprintln!("aiwm: config: bar: ignored: {}", line),
            }
            continue;
        }
        if line == "bar {" || line == "bar{" {
            in_bar = true;
            c.bar.get_or_insert((true, None));
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
                    ["workspace", "number", n] | ["workspace", n] if n.parse::<u32>().is_ok() => Action::Workspace(n.parse().unwrap()),
                    ["move", "container" | "window", "to", "workspace", "number", n] | ["move", "container" | "window", "to", "workspace", n]
                        if n.parse::<u32>().is_ok() =>
                    {
                        Action::MoveTo(n.parse().unwrap())
                    }
                    ["layout", "splith"] => Action::Layout(Some(Layout::SplitH)),
                    ["layout", "splitv"] => Action::Layout(Some(Layout::SplitV)),
                    ["layout", "tabbed" | "stacking"] => Action::Layout(Some(Layout::Tabbed)),
                    ["layout", "toggle", "split"] => Action::Layout(None),
                    ["fullscreen"] | ["fullscreen", "toggle"] => Action::Fullscreen,
                    ["floating", "toggle"] => Action::FloatToggle,
                    ["exit"] => Action::Exit,
                    _ => {
                        eprintln!("aiwm: config: unknown command {}", cmd.join(" "));
                        continue;
                    }
                };
                c.binds.push(Binding { mods, code, action });
            }
            ["exec", rest @ ..] => c.autostart.push(rest.join(" ")),
            ["floating_modifier", m, ..] => match parse_combo(&format!("{}+a", m)) {
                Some((mods, _)) => c.float_mod = mods,
                None => eprintln!("aiwm: config: unknown modifier {}", m),
            },
            ["for_window", crit, "floating", "enable"] => {
                // [app_id="firefox"] だけ
                match crit.strip_prefix("[app_id=").and_then(|r| r.strip_suffix(']')) {
                    Some(id) => c.float_apps.push(id.trim_matches('"').to_string()),
                    None => eprintln!("aiwm: config: for_window: only [app_id=\"...\"]: {}", crit),
                }
            }
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
            let status_at = pfds.len();
            if let Some(fd) = self.status_fd {
                pfds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
            }
            // 描くものがあれば、前の画面から 16ms たったら描く。時計があれば 1 秒ごとに見る
            let idle = if self.config.bar.is_some() && self.status.is_empty() { 1000 } else { -1 };
            let timeout = if self.dirty.is_some() { (16 - wl::now_ms().wrapping_sub(last_frame) as i32).clamp(0, 16) } else { idle };
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
            if pfds.get(status_at).is_some_and(|p| p.revents != 0) {
                self.read_status();
            }
            self.tick();
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
        if self.focus.is_some_and(|f| f.client == id) {
            self.focus = None;
        }
        if self.ptr_win.is_some_and(|f| f.client == id) {
            self.ptr_win = None;
        }
        self.frames.retain(|(c, _)| *c != id);
        if self.popups.iter().any(|p| p.client == id) {
            self.popups.retain(|p| p.client != id);
            self.mark_all();
        }
        let gone: Vec<Win> = self.spaces.values().flat_map(|s| s.wins.iter().chain(s.floats.iter()).copied()).filter(|w| w.client == id).collect();
        for w in gone {
            self.unmap(w);
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
                    6 => Obj::DataManager,
                    7 => Obj::Subcompositor,
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
            // wl_data_device_manager: create_data_source (0)、get_data_device (1)
            (K::DataManager, 0) => {
                new_obj!(Obj::DataSource);
            }
            (K::DataManager, 1) => {
                new_obj!(Obj::DataDevice);
            }
            (K::DataManager, _) => {}
            // wl_data_source: offer (0)、destroy (1)、set_actions (2)
            (K::DataSource, 1) => self.destroy(cid, m.id),
            (K::DataSource, _) => {}
            // wl_data_device: start_drag (0)、set_selection (1)、release (2)
            (K::DataDevice, 2) => self.destroy(cid, m.id),
            (K::DataDevice, _) => {}
            (K::Region, _) => {}
            // xdg_positioner: set_size (1)、set_anchor_rect (2)、set_anchor (3)、set_gravity (4)、
            // set_constraint_adjustment (5)、set_offset (6)
            (K::Positioner, op) => {
                let v: Vec<i32> = match op {
                    1 | 6 => vec![m.int(), m.int()],
                    2 => vec![m.int(), m.int(), m.int(), m.int()],
                    3..=5 => vec![m.int()],
                    _ => vec![],
                };
                let Some(Obj::Positioner(p)) = c.objs.get_mut(&m.id) else { return };
                match op {
                    1 => p.size = (v[0], v[1]),
                    2 => p.anchor_rect = (v[0], v[1], v[2], v[3]),
                    3 => p.anchor = v[0] as u32,
                    4 => p.gravity = v[0] as u32,
                    5 => p.adjust = v[0] as u32,
                    6 => p.offset = (v[0], v[1]),
                    _ => {}
                }
            }
            // xdg_popup: destroy (0)、grab (1)
            (K::Popup(sid), 0) => {
                self.destroy(cid, m.id);
                self.close_popup(cid, sid);
            }
            (K::Popup(_), _) => {}
            // wl_subcompositor: destroy (0)、get_subsurface (1)
            (K::Subcompositor, 0) => self.destroy(cid, m.id),
            (K::Subcompositor, 1) => {
                let nid = m.uint();
                let (sid, pid) = (m.uint(), m.uint());
                if sid == pid || !c.surfaces.contains_key(&sid) || !c.surfaces.contains_key(&pid) {
                    c.dead = true;
                    return;
                }
                c.objs.insert(nid, Obj::Subsurface(sid));
                let s = c.surfaces.get_mut(&sid).unwrap();
                s.parent = Some(pid);
                s.pos = (0, 0);
                let p = c.surfaces.get_mut(&pid).unwrap();
                if p.stack.is_empty() {
                    p.stack.push(pid);
                }
                p.stack.push(sid);
            }
            (K::Subcompositor, _) => {}
            // wl_subsurface: destroy (0)、set_position (1)、place_above (2)、place_below (3)、set_sync (4)、set_desync (5)。
            // commit はいつもすぐ見せる (desync と同じ)
            (K::Subsurface(sid), 0) => {
                self.destroy(cid, m.id);
                self.detach_sub(cid, sid);
            }
            (K::Subsurface(sid), 1) => {
                let pos = (m.int(), m.int());
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.pos = pos;
                }
                self.mark_surface(cid, sid);
            }
            (K::Subsurface(sid), op @ (2 | 3)) => {
                let sib = m.uint();
                let Some(pid) = c.surfaces.get(&sid).and_then(|s| s.parent) else { return };
                let Some(p) = c.surfaces.get_mut(&pid) else { return };
                p.stack.retain(|&x| x != sid);
                let i = p.stack.iter().position(|&x| x == sib).unwrap_or(p.stack.len() - 1);
                p.stack.insert(if op == 2 { i + 1 } else { i }, sid);
                self.mark_surface(cid, sid);
            }
            (K::Subsurface(_), _) => {}
            (K::Shm, 0) => {
                let nid = m.uint();
                let fd = c.conn.take_fd();
                let size = m.int().max(0) as usize;
                match fd.and_then(|fd| Pool::map(fd, size)) {
                    Some(p) => {
                        c.objs.insert(nid, Obj::Pool(std::rc::Rc::new(p)));
                    }
                    None => c.dead = true,
                }
            }
            (K::Pool, 0) => {
                let nid = m.uint();
                let (offset, w, h, stride, format) = (m.int(), m.int(), m.int(), m.int(), m.uint());
                let Some(Obj::Pool(pool)) = c.objs.get(&m.id) else { return };
                let pool = pool.clone();
                if offset < 0 || w <= 0 || h <= 0 || stride < w * 4 {
                    c.dead = true;
                    return;
                }
                c.objs.insert(nid, Obj::Buffer(Buffer { pool, offset: offset as usize, w: w as usize, h: h as usize, stride: stride as usize, format }));
            }
            (K::Pool, 1) => self.destroy(cid, m.id),
            (K::Pool, 2) => {
                let size = m.int().max(0) as usize;
                let Some(Obj::Pool(p)) = c.objs.get(&m.id) else { return };
                let fd = unsafe { libc::dup(p.fd) };
                if let Some(np) = Pool::map(fd, size) {
                    c.objs.insert(m.id, Obj::Pool(std::rc::Rc::new(np)));
                }
            }
            (K::Buffer, 0) => self.destroy(cid, m.id),
            (K::Surface, 0) => {
                let sid = m.id;
                self.destroy(cid, sid);
                self.detach_sub(cid, sid);
                let c = self.clients.get_mut(&cid).unwrap();
                if let Some(s) = c.surfaces.remove(&sid) {
                    // 子は親をなくす (もう見えない)
                    for k in s.stack {
                        if let Some(ks) = c.surfaces.get_mut(&k).filter(|_| k != sid) {
                            ks.parent = None;
                        }
                    }
                }
                self.unmap(Win { client: cid, surface: sid });
                self.close_popup(cid, sid);
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
                // 1 = XKB_V1 (クライアントは libxkbcommon で読む)。0 なら「キーマップなし」
                let format = if self.keymap_size > 0 { 1 } else { 0 };
                c.conn.send(k, 0, &[Arg::U(format), Arg::Fd(self.keymap_fd), Arg::U(self.keymap_size)]);
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
                new_obj!(Obj::Positioner(Positioner::default()));
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
            (K::XdgSurface(sid), 2) => {
                // get_popup: 親 (xdg_surface、なくてもよい) と positioner。configure は最初の commit のあとで送る
                let nid = m.uint();
                let (parent, posid) = (m.uint(), m.uint());
                let pos = match c.objs.get(&posid) {
                    Some(Obj::Positioner(p)) => *p,
                    _ => Positioner::default(),
                };
                let psid = match c.objs.get(&parent) {
                    Some(Obj::XdgSurface(p)) => *p,
                    _ => 0,
                };
                if std::env::var_os("AIWM_DEBUG").is_some() {
                    eprintln!("aiwm: popup {} (surface {}) parent {} size {:?} anchor {:?} {} gravity {} offset {:?} adjust {}", nid, sid, psid, pos.size, pos.anchor_rect, pos.anchor, pos.gravity, pos.offset, pos.adjust);
                }
                c.objs.insert(nid, Obj::Popup(sid));
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.popup = Some(Popup { id: nid, parent: psid, pos, x: 0, y: 0, configured: false });
                }
                self.popups.push(Win { client: cid, surface: sid });
            }
            (K::XdgSurface(sid), 3) => {
                let g = (m.int(), m.int(), m.int(), m.int());
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.geometry = (g.2 > 0 && g.3 > 0).then_some(g);
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
            (K::Toplevel(sid), 1) => {
                // set_parent: 親の xdg_toplevel (0 なら親なし)
                let pid = m.uint();
                let psid = match c.objs.get(&pid) {
                    Some(Obj::Toplevel(p)) => Some(*p),
                    _ => None,
                };
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.parent_top = psid;
                }
            }
            (K::Toplevel(sid), 2) => {
                let t = m.string();
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.title = t;
                }
            }
            (K::Toplevel(sid), 3) => {
                let t = m.string();
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    s.app_id = t;
                }
            }
            (K::Toplevel(sid), op @ (7 | 8)) => {
                // set_max_size (7)、set_min_size (8)
                let wh = (m.int(), m.int());
                if let Some(s) = c.surfaces.get_mut(&sid) {
                    if op == 7 {
                        s.max_size = wh;
                    } else {
                        s.min_size = wh;
                    }
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
                {
                    let p = &b.pool;
                    if b.offset + b.stride * (b.h - 1) + b.w * 4 <= p.size {
                        s.image.resize(b.w * b.h, 0);
                        for y in 0..b.h {
                            let src = unsafe { std::slice::from_raw_parts(p.ptr.add(b.offset + y * b.stride) as *const u32, b.w) };
                            s.image[y * b.w..(y + 1) * b.w].copy_from_slice(src);
                        }
                        if b.format != 0 {
                            s.image.iter_mut().for_each(|p| *p |= 0xff00_0000);
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
        if let Some(p) = s.popup {
            if std::env::var_os("AIWM_DEBUG").is_some() {
                eprintln!("aiwm: popup surface {} commit {}x{} configured {}", sid, s.iw, s.ih, p.configured);
            }
            if !p.configured {
                self.configure_popup(cid, sid);
            }
            self.mark_all();
            // 出てきた popup がポインタの下なら、そちらに enter
            if self.ptr_shown {
                self.pointer_moved(self.ptr);
            }
            return;
        }
        if is_top && self.ws_of(w).is_none() && self.wants_float(w) {
            // ダイアログなど: 浮かせる。場所は絵が来てから (place_float)
            let ws = self.cur_ws();
            ws.floats.push(w);
            ws.fullscreen = None;
            self.set_focus(Some(w));
            self.relayout();
        } else if is_top && self.ws_of(w).is_none() {
            // 新しい窓: 今のワークスペースの作業中の窓のうしろに入れて、作業中にする
            let focus = self.focus;
            let ws = self.cur_ws();
            let at = focus.and_then(|f| ws.wins.iter().position(|x| *x == f)).map_or(ws.wins.len(), |i| i + 1);
            ws.wins.insert(at, w);
            ws.fullscreen = None;
            self.set_focus(Some(w));
            self.relayout();
        } else {
            if self.is_float(w) && !self.float_at.contains_key(&(cid, sid)) {
                // 最初の絵: 大きすぎれば縮めてもらい (relayout)、真ん中に置く
                self.relayout();
                self.place_float(w);
            }
            if self.is_float(w) {
                // 浮いた窓は大きさが変わるかもしれない (前の大きさのところも描きなおす)
                self.mark_all();
            } else {
                self.mark_surface(cid, sid);
            }
        }
    }

    /// 新しい窓を浮かせるか: 親がある (ダイアログ)、大きさが決まっている、for_window で決めた app_id
    fn wants_float(&self, w: Win) -> bool {
        let Some(s) = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)) else { return false };
        s.parent_top.is_some() || (s.min_size.0 > 0 && s.min_size == s.max_size) || self.config.float_apps.iter().any(|a| *a == s.app_id)
    }

    fn is_float(&self, w: Win) -> bool {
        self.spaces.values().any(|s| s.floats.contains(&w))
    }

    /// 浮いた窓の大きさ (window geometry か絵の大きさ)
    fn float_size(&self, w: Win) -> (i32, i32) {
        let Some(s) = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)) else { return (1, 1) };
        let (gw, gh) = s.geometry.map_or((s.iw as i32, s.ih as i32), |g| (g.2, g.3));
        (gw.max(1), gh.max(1))
    }

    /// 浮いた窓を、親の窓 (なければ窓を並べるところ) の真ん中に置く。絵がまだなければ置かない
    fn place_float(&mut self, w: Win) {
        let Some(s) = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)) else { return };
        if s.iw == 0 {
            return;
        }
        let parent = s.parent_top.and_then(|p| self.rect_of(Win { client: w.client, surface: p }));
        let a = parent.unwrap_or_else(|| self.area());
        let (fw, fh) = self.float_size(w);
        let x = (a.x + (a.w - fw) / 2).clamp(0, (self.width() - fw).max(0));
        let y = (a.y + (a.h - fh) / 2).clamp(self.bar_h(), (self.height() - fh).max(self.bar_h()));
        self.float_at.insert((w.client, w.surface), (x, y));
    }

    /// surface (子の窓でも) が属している窓 (いちばん上の親)
    fn root_of(&self, cid: usize, mut sid: u32) -> Win {
        let c = &self.clients[&cid];
        for _ in 0..32 {
            match c.surfaces.get(&sid).and_then(|s| s.parent) {
                Some(p) => sid = p,
                None => break,
            }
        }
        Win { client: cid, surface: sid }
    }

    /// surface の絵が変わった: その窓を描きなおす
    fn mark_surface(&mut self, cid: usize, sid: u32) {
        if let Some(r) = self.rect_of(self.root_of(cid, sid)) {
            self.mark(r.y, r.y + r.h);
        }
    }

    /// popup を置く場所を決めて configure を送る。画面からはみ出すなら上下を返し、それでもだめなら画面の中へずらす
    fn configure_popup(&mut self, cid: usize, sid: u32) {
        let Some(p) = self.clients[&cid].surfaces.get(&sid).and_then(|s| s.popup) else { return };
        let origin = self.geom_origin(cid, p.parent).unwrap_or((0, 0));
        let (mut w, mut h) = (p.pos.size.0.max(1), p.pos.size.1.max(1));
        let (sw, sh) = (self.width(), self.height());
        let (mut x, mut y) = p.pos.place();
        // constraint_adjustment: slide_x 1, slide_y 2, flip_x 4, flip_y 8, resize_x 16, resize_y 32。
        // 上下にはみ出すなら: 返して入ればそれ、だめなら (resize_y なら) 入るところまで縮める、それでもだめならずらす
        if origin.1 + y + h > sh || origin.1 + y < 0 {
            let (_, fy) = p.pos.flip_y().place();
            if p.pos.adjust & 8 != 0 && origin.1 + fy >= 0 && origin.1 + fy + h <= sh {
                y = fy;
            } else if p.pos.adjust & 32 != 0 {
                let top = (origin.1 + y).max(0);
                if sh - top >= 64 {
                    y = top - origin.1;
                    h = sh - top;
                }
            }
        }
        if p.pos.adjust & 16 != 0 && w > sw {
            w = sw;
        }
        x = (origin.0 + x).clamp(0, (sw - w).max(0)) - origin.0;
        y = (origin.1 + y).clamp(0, (sh - h).max(0)) - origin.1;
        let serial = self.next_serial();
        let c = self.clients.get_mut(&cid).unwrap();
        let Some(s) = c.surfaces.get_mut(&sid) else { return };
        let Some(pp) = s.popup.as_mut() else { return };
        pp.x = x;
        pp.y = y;
        pp.configured = true;
        let (pid, xdg) = (pp.id, s.xdg);
        c.conn.send(pid, 0, &[Arg::I(x), Arg::I(y), Arg::I(w), Arg::I(h)]);
        if let Some(xdg) = xdg {
            c.conn.send(xdg, 0, &[Arg::U(serial)]);
        }
    }

    /// popup が消えた (destroy されたか、surface ごとなくなった)
    fn close_popup(&mut self, cid: usize, sid: u32) {
        let w = Win { client: cid, surface: sid };
        if !self.popups.contains(&w) {
            return;
        }
        self.popups.retain(|p| *p != w);
        if let Some(s) = self.clients.get_mut(&cid).and_then(|c| c.surfaces.get_mut(&sid)) {
            s.popup = None;
        }
        if self.ptr_win == Some(w) {
            self.ptr_win = None;
        }
        self.mark_all();
        if self.ptr_shown {
            self.pointer_moved(self.ptr);
        }
    }

    /// 出ている popup をみんな閉じてもらう (外をクリックしたとき)。上のものから popup_done
    fn dismiss_popups(&mut self) {
        for w in self.popups.clone().into_iter().rev() {
            if let Some(c) = self.clients.get_mut(&w.client) {
                if let Some(id) = c.surfaces.get(&w.surface).and_then(|s| s.popup).map(|p| p.id) {
                    c.conn.send(id, 1, &[]);
                }
            }
        }
    }

    /// xdg_surface の「窓」(window geometry) の左上が、画面のどこにあるか
    fn geom_origin(&self, cid: usize, sid: u32) -> Option<(i32, i32)> {
        self.geom_origin_n(cid, sid, 0)
    }

    fn geom_origin_n(&self, cid: usize, sid: u32, depth: u32) -> Option<(i32, i32)> {
        let s = self.clients.get(&cid)?.surfaces.get(&sid)?;
        if let Some(p) = s.popup {
            if depth > 16 {
                return None;
            }
            let (ox, oy) = self.geom_origin_n(cid, p.parent, depth + 1)?;
            return Some((ox + p.x, oy + p.y));
        }
        let r = self.rect_of(Win { client: cid, surface: sid })?;
        Some((r.x, r.y))
    }

    /// surface の絵の左上 (0, 0) が画面のどこか (window geometry の分だけずらす)
    fn surface_origin(&self, w: Win) -> Option<(i32, i32)> {
        let (x, y) = self.geom_origin(w.client, w.surface)?;
        let (gx, gy) = self.geom_off(w);
        Some((x - gx, y - gy))
    }

    /// (x, y) にあるもの: popup (上から) か窓と、その surface の左上
    fn target_at(&self, x: i32, y: i32) -> Option<(Win, (i32, i32))> {
        for &w in self.popups.iter().rev() {
            let Some(s) = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)) else { continue };
            let Some((ox, oy)) = self.geom_origin(w.client, w.surface) else { continue };
            let (gw, gh) = s.geometry.map_or((s.iw as i32, s.ih as i32), |g| (g.2, g.3));
            if (Rect { x: ox, y: oy, w: gw, h: gh }).contains(x, y) {
                return Some((w, self.surface_origin(w)?));
            }
        }
        let (w, r) = self.win_at(x, y)?;
        let (gx, gy) = self.geom_off(w);
        Some((w, (r.x - gx, r.y - gy)))
    }

    /// subsurface をやめる (親の重なり順から外す)
    fn detach_sub(&mut self, cid: usize, sid: u32) {
        self.mark_surface(cid, sid);
        let c = self.clients.get_mut(&cid).unwrap();
        let Some(pid) = c.surfaces.get_mut(&sid).and_then(|s| s.parent.take()) else { return };
        if let Some(p) = c.surfaces.get_mut(&pid) {
            p.stack.retain(|&x| x != sid);
        }
    }

    fn cur_ws(&mut self) -> &mut Ws {
        self.spaces.entry(self.cur).or_insert_with(Ws::new)
    }

    fn ws_of(&self, w: Win) -> Option<u32> {
        self.spaces.iter().find(|(_, s)| s.wins.contains(&w) || s.floats.contains(&w)).map(|(n, _)| *n)
    }

    fn unmap(&mut self, w: Win) {
        let Some(n) = self.ws_of(w) else { return };
        self.float_at.remove(&(w.client, w.surface));
        if self.drag.is_some_and(|d| d.0 == w) {
            self.drag = None;
        }
        let ws = self.spaces.get_mut(&n).unwrap();
        if let Some(i) = ws.floats.iter().position(|x| *x == w) {
            // 浮いた窓: 次はいちばん上の浮いた窓か、タイルの窓へ
            ws.floats.remove(i);
            if ws.focus == Some(w) {
                ws.focus = ws.floats.last().copied().or(ws.wins.last().copied());
            }
        } else {
            let i = ws.wins.iter().position(|x| *x == w).unwrap();
            ws.wins.remove(i);
            if ws.focus == Some(w) {
                ws.focus = if ws.wins.is_empty() { ws.floats.last().copied() } else { Some(ws.wins[i.min(ws.wins.len() - 1)]) };
            }
        }
        if ws.fullscreen == Some(w) {
            ws.fullscreen = None;
        }
        let next = ws.focus;
        let empty = ws.wins.is_empty() && ws.floats.is_empty();
        if n == self.cur {
            if self.focus == Some(w) {
                self.focus = None;
            }
            self.set_focus(next);
        } else if empty {
            self.spaces.remove(&n);
        }
        if self.ptr_win == Some(w) {
            self.ptr_win = None;
        }
        self.relayout();
    }

    // ---- 並べ方 ----

    fn bar_h(&self) -> i32 {
        if self.config.bar.is_some() { BAR_H } else { 0 }
    }

    /// 窓を並べるところ (バーを除く)
    fn area(&self) -> Rect {
        let (sw, sh, bh) = (self.width(), self.height(), self.bar_h());
        let top = self.config.bar.as_ref().is_none_or(|b| b.0);
        Rect { x: GAP, y: if top { bh } else { 0 } + GAP, w: sw - 2 * GAP, h: sh - bh - 2 * GAP }
    }

    /// ワークスペース ws の窓 w (枠の内側) の場所。タブで隠れている窓も大きさは返す
    fn tile_rect(&self, ws: &Ws, w: Win) -> Option<Rect> {
        let i = ws.wins.iter().position(|x| *x == w)? as i32;
        if ws.fullscreen == Some(w) {
            return Some(Rect { x: 0, y: 0, w: self.width(), h: self.height() });
        }
        let a = self.area();
        let n = ws.wins.len() as i32;
        let outer = match ws.layout {
            Layout::SplitH => {
                let x0 = a.x + (a.w + GAP) * i / n;
                let x1 = a.x + (a.w + GAP) * (i + 1) / n - GAP;
                Rect { x: x0, y: a.y, w: x1 - x0, h: a.h }
            }
            Layout::SplitV => {
                let y0 = a.y + (a.h + GAP) * i / n;
                let y1 = a.y + (a.h + GAP) * (i + 1) / n - GAP;
                Rect { x: a.x, y: y0, w: a.w, h: y1 - y0 }
            }
            Layout::Tabbed => Rect { x: a.x, y: a.y + TAB_H, w: a.w, h: a.h - TAB_H },
        };
        Some(Rect { x: outer.x + BORDER, y: outer.y + BORDER, w: (outer.w - 2 * BORDER).max(1), h: (outer.h - 2 * BORDER).max(1) })
    }

    /// 今見えている窓の場所 (ほかのワークスペースや、タブ・全画面で隠れているものは None)
    fn rect_of(&self, w: Win) -> Option<Rect> {
        let ws = self.spaces.get(&self.cur)?;
        if ws.floats.contains(&w) {
            if ws.fullscreen.is_some() {
                return None;
            }
            let (x, y) = *self.float_at.get(&(w.client, w.surface))?;
            let (fw, fh) = self.float_size(w);
            return Some(Rect { x, y, w: fw, h: fh });
        }
        if !ws.wins.contains(&w) {
            return None;
        }
        if ws.fullscreen.is_some_and(|f| f != w) {
            return None;
        }
        if ws.layout == Layout::Tabbed && ws.fullscreen.is_none() && ws.focus != Some(w) {
            return None;
        }
        self.tile_rect(ws, w)
    }

    /// 大きさや状態が変わった窓に configure を送る
    fn relayout(&mut self) {
        let mut todo = vec![];
        for ws in self.spaces.values() {
            for &w in &ws.wins {
                if let Some(r) = self.tile_rect(ws, w) {
                    todo.push((w, r, self.focus == Some(w), ws.fullscreen == Some(w), ws.layout));
                }
            }
        }
        // 浮いた窓: 大きさは窓にまかせる (0x0)。ただし窓を並べるところより大きければ、そこまでにしてもらう
        let a = self.area();
        for ws in self.spaces.values() {
            for &w in &ws.floats {
                let (fw, fh) = self.float_size(w);
                // 一度縮めてもらった大きさは送りつづける (0x0 にもどすと、また大きくなる)
                let prev = self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)).and_then(|s| s.sent).filter(|k| k.0 > 0).map(|k| (k.0, k.1));
                let r = match prev {
                    Some((pw, ph)) => Rect { x: 0, y: 0, w: pw, h: ph },
                    None if fw > a.w || fh > a.h => Rect { x: 0, y: 0, w: fw.min(a.w), h: fh.min(a.h) },
                    None => Rect { x: 0, y: 0, w: 0, h: 0 },
                };
                todo.push((w, r, self.focus == Some(w), false, ws.layout));
            }
        }
        for (w, r, active, full, layout) in todo {
            let serial = self.next_serial();
            let Some(c) = self.clients.get_mut(&w.client) else { continue };
            let Some(s) = c.surfaces.get_mut(&w.surface) else { continue };
            let key = (r.w, r.h, active, full, layout as u8);
            if s.sent == Some(key) {
                continue;
            }
            s.sent = Some(key);
            let (Some(top), Some(xdg)) = (s.toplevel, s.xdg) else { continue };
            // 状態: fullscreen (2)、activated (4)、tiled left/right/top/bottom (5..8)
            let floating = self.spaces.values().any(|ws| ws.floats.contains(&w));
            let mut sts: Vec<u32> = if full {
                vec![2]
            } else if floating {
                vec![]
            } else {
                vec![5, 6, 7, 8]
            };
            if active {
                sts.push(4);
            }
            let states: Vec<u8> = sts.iter().flat_map(|v| v.to_le_bytes()).collect();
            c.conn.send(top, 0, &[Arg::I(r.w), Arg::I(r.h), Arg::A(&states)]);
            c.conn.send(xdg, 0, &[Arg::U(serial)]);
        }
        self.mark_all();
    }

    /// キーボードの作業中の窓 (今のワークスペースの窓か None)
    fn set_focus(&mut self, w: Option<Win>) {
        if let Some(n) = w {
            let ws = self.cur_ws();
            ws.focus = Some(n);
            // 浮いた窓はいちばん上へ
            if let Some(i) = ws.floats.iter().position(|x| *x == n) {
                let f = ws.floats.remove(i);
                ws.floats.push(f);
            }
        }
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

    /// ワークスペース n を見せる (空になった前のワークスペースは消す)
    fn switch_to(&mut self, n: u32) {
        if n == self.cur {
            return;
        }
        let old = self.cur;
        self.set_focus(None);
        self.cur = n;
        let f = self.cur_ws().focus;
        if self.spaces.get(&old).is_some_and(|s| s.wins.is_empty() && s.floats.is_empty()) {
            self.spaces.remove(&old);
        }
        self.set_focus(f);
        self.relayout();
        if self.ptr_shown {
            let p = self.ptr;
            self.pointer_moved(p);
        }
    }

    /// 作業中の窓をワークスペース n へ
    fn move_to(&mut self, n: u32) {
        let Some(w) = self.focus else { return };
        if n == self.cur {
            return;
        }
        let ws = self.cur_ws();
        let float = ws.floats.contains(&w);
        if float {
            ws.floats.retain(|x| *x != w);
            ws.focus = ws.floats.last().copied().or(ws.wins.last().copied());
        } else {
            let i = ws.wins.iter().position(|x| *x == w).unwrap();
            ws.wins.remove(i);
            ws.focus = if ws.wins.is_empty() { ws.floats.last().copied() } else { Some(ws.wins[i.min(ws.wins.len() - 1)]) };
        }
        if ws.fullscreen == Some(w) {
            ws.fullscreen = None;
        }
        let next = ws.focus;
        let dst = self.spaces.entry(n).or_insert_with(Ws::new);
        if float {
            dst.floats.push(w);
        } else {
            dst.wins.push(w);
        }
        dst.focus = Some(w);
        self.set_focus(next);
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
                let ws = self.cur_ws();
                if let Some(i) = ws.focus.and_then(|f| ws.wins.iter().position(|x| *x == f)) {
                    let n = ws.wins.len() as i32;
                    let w = ws.wins[(i as i32 + d).rem_euclid(n) as usize];
                    if ws.fullscreen.is_some() {
                        ws.fullscreen = Some(w);
                    }
                    self.set_focus(Some(w));
                }
            }
            Action::Move(d) => {
                let d = *d;
                let ws = self.cur_ws();
                if let Some(i) = ws.focus.and_then(|f| ws.wins.iter().position(|x| *x == f)) {
                    let j = i as i32 + d;
                    if j >= 0 && (j as usize) < ws.wins.len() {
                        ws.wins.swap(i, j as usize);
                        self.relayout();
                    }
                }
            }
            Action::Workspace(n) => {
                let n = *n;
                self.switch_to(n);
            }
            Action::MoveTo(n) => {
                let n = *n;
                self.move_to(n);
            }
            Action::Layout(l) => {
                let l = *l;
                let ws = self.cur_ws();
                ws.layout = match (l, ws.layout) {
                    (Some(l), _) => l,
                    (None, Layout::SplitH) => Layout::SplitV,
                    (None, _) => Layout::SplitH,
                };
                self.relayout();
            }
            Action::Fullscreen => {
                let ws = self.cur_ws();
                ws.fullscreen = if ws.fullscreen.is_some() { None } else { ws.focus };
                self.relayout();
            }
            Action::FloatToggle => {
                let Some(w) = self.focus else { return };
                let r = self.rect_of(w);
                let ws = self.cur_ws();
                if let Some(i) = ws.floats.iter().position(|x| *x == w) {
                    ws.floats.remove(i);
                    ws.wins.push(w);
                    self.float_at.remove(&(w.client, w.surface));
                } else if let Some(i) = ws.wins.iter().position(|x| *x == w) {
                    ws.wins.remove(i);
                    ws.floats.push(w);
                    if ws.fullscreen == Some(w) {
                        ws.fullscreen = None;
                    }
                    // 今の場所から少し内側に (大きさは窓が決めなおす)
                    if let Some(r) = r {
                        self.float_at.insert((w.client, w.surface), (r.x + r.w / 8, r.y + r.h / 8));
                    }
                }
                self.relayout();
            }
            Action::Exit => self.quit = true,
        }
    }

    fn win_at(&self, x: i32, y: i32) -> Option<(Win, Rect)> {
        let ws = self.spaces.get(&self.cur)?;
        ws.floats.iter().rev().chain(ws.wins.iter()).find_map(|w| self.rect_of(*w).filter(|r| r.contains(x, y)).map(|r| (*w, r)))
    }

    fn pointer_moved(&mut self, old: (i32, i32)) {
        self.ptr_shown = true;
        self.mark(old.1 - 1, old.1 + CURSOR_H + 1);
        self.mark(self.ptr.1 - 1, self.ptr.1 + CURSOR_H + 1);
        if let Some((w, dx, dy)) = self.drag {
            let (fw, _) = self.float_size(w);
            let x = (self.ptr.0 - dx).clamp(-fw + 32, self.width() - 32);
            let y = (self.ptr.1 - dy).clamp(self.bar_h(), self.height() - 32);
            self.float_at.insert((w.client, w.surface), (x, y));
            self.mark_all();
            return;
        }
        let hit = self.target_at(self.ptr.0, self.ptr.1);
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
            if let Some((n, o)) = hit {
                if let Some(c) = self.clients.get_mut(&n.client) {
                    for p in c.pointers.clone() {
                        c.conn.send(p, 0, &[Arg::U(serial), Arg::O(n.surface), Arg::F((self.ptr.0 - o.0) as f64), Arg::F((self.ptr.1 - o.1) as f64)]);
                        c.conn.send(p, 5, &[]);
                        // enter のあとに motion も (GTK は motion で「上にいる」を決めるものがある)
                        c.conn.send(p, 2, &[Arg::U(wl::now_ms()), Arg::F((self.ptr.0 - o.0) as f64), Arg::F((self.ptr.1 - o.1) as f64)]);
                        c.conn.send(p, 5, &[]);
                    }
                }
            }
            self.ptr_win = now;
        } else if let Some((n, o)) = hit {
            if let Some(c) = self.clients.get_mut(&n.client) {
                for p in c.pointers.clone() {
                    c.conn.send(p, 2, &[Arg::U(wl::now_ms()), Arg::F((self.ptr.0 - o.0) as f64), Arg::F((self.ptr.1 - o.1) as f64)]);
                    c.conn.send(p, 5, &[]);
                }
            }
        }
    }

    fn button(&mut self, code: u16, value: i32) {
        if value == 2 {
            return;
        }
        // floating_modifier + 左ボタン: 浮いた窓をつかんで動かす (クライアントには送らない)
        if self.drag.is_some() {
            if value == 0 {
                self.drag = None;
            }
            return;
        }
        let fm = self.config.float_mod;
        if value == 1 && code == keys::BTN_LEFT && fm != 0 && self.mods.mask().0 & fm == fm {
            let (x, y) = self.ptr;
            if let Some((w, r)) = self.win_at(x, y).filter(|(w, _)| self.is_float(*w)) {
                self.drag = Some((w, x - r.x, y - r.y));
                self.set_focus(Some(w));
                return;
            }
        }
        if value == 1 {
            let (x, y) = self.ptr;
            // popup の外を押した: popup を閉じてもらい、この押したのは使わない (メニューの外のクリック)
            if !self.popups.is_empty() && !self.target_at(x, y).is_some_and(|(w, _)| self.popups.contains(&w)) {
                self.dismiss_popups();
                return;
            }
            if let Some(w) = self.ptr_win.filter(|w| self.popups.contains(w)) {
                let serial = self.next_serial();
                if let Some(c) = self.clients.get_mut(&w.client) {
                    for p in c.pointers.clone() {
                        c.conn.send(p, 3, &[Arg::U(serial), Arg::U(wl::now_ms()), Arg::U(code as u32), Arg::U(value as u32)]);
                        c.conn.send(p, 5, &[]);
                    }
                }
                return;
            }
            if let Some(n) = self.bar_hit(x, y) {
                self.switch_to(n);
                return;
            }
            if let Some(w) = self.tab_hit(x, y) {
                self.set_focus(Some(w));
                return;
            }
            if let Some((w, _)) = self.win_at(x, y) {
                self.set_focus(Some(w));
            }
        }
        if let Some(w) = self.ptr_win {
            let serial = self.next_serial();
            if std::env::var_os("AIWM_DEBUG").is_some() {
                eprintln!("aiwm: button {} {} -> client {} surface {} at {:?}", code, value, w.client, w.surface, self.ptr);
            }
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
        let (wins, layout, full) = match self.spaces.get(&self.cur) {
            Some(ws) => (ws.wins.clone(), ws.layout, ws.fullscreen),
            None => (vec![], Layout::SplitH, None),
        };
        if wins.is_empty() {
            self.draw_hint(y0, y1);
        }
        if layout == Layout::Tabbed && full.is_none() && !wins.is_empty() {
            self.draw_tabs(&wins, y0, y1);
        }
        for w in wins {
            let Some(r) = self.rect_of(w) else { continue };
            if full != Some(w) {
                let color = if self.focus == Some(w) { FOCUS } else { UNFOCUS };
                let outer = Rect { x: r.x - BORDER, y: r.y - BORDER, w: r.w + 2 * BORDER, h: r.h + 2 * BORDER };
                self.fill_rect(outer, color, y0, y1);
            }
            self.fill_rect(r, term_bg(), y0, y1);
            let Some(c) = self.clients.get(&w.client) else { continue };
            let Some(s) = c.surfaces.get(&w.surface) else { continue };
            // 絵のうち窓として見せるところ (set_window_geometry があれば、影などを除いた部分)。
            // 子の窓もまとめて、その外は切る
            let (iw, ih) = (s.iw as i32, s.ih as i32);
            let (gx, gy, gw, gh) = match s.geometry {
                Some((x, y, w, h)) => (x, y, w, h),
                None => (0, 0, iw, ih),
            };
            let clip = Rect { x: r.x, y: r.y.max(y0), w: r.w.min(gw), h: (r.y + r.h.min(gh)).min(y1) - r.y.max(y0) };
            let px = unsafe { std::slice::from_raw_parts_mut(self.fb.pixels().as_mut_ptr(), self.fb.pixels().len()) };
            draw_tree(px, stride, c, w.surface, r.x - gx, r.y - gy, clip, 0);
        }
        // 浮いた窓 (下から)。枠をつけて、窓の形で切る
        let floats = self.spaces.get(&self.cur).map_or(vec![], |ws| ws.floats.clone());
        for w in floats {
            let Some(r) = self.rect_of(w) else { continue };
            let color = if self.focus == Some(w) { FOCUS } else { UNFOCUS };
            let outer = Rect { x: r.x - BORDER, y: r.y - BORDER, w: r.w + 2 * BORDER, h: r.h + 2 * BORDER };
            self.fill_rect(outer, color, y0, y1);
            self.fill_rect(r, term_bg(), y0, y1);
            let Some(c) = self.clients.get(&w.client) else { continue };
            let (gx, gy) = self.geom_off(w);
            let top = r.y.max(y0).max(0);
            let clip = Rect { x: r.x.max(0), y: top, w: (r.x + r.w).min(sw as i32) - r.x.max(0), h: (r.y + r.h).min(y1) - top };
            if clip.w <= 0 || clip.h <= 0 {
                continue;
            }
            let px = unsafe { std::slice::from_raw_parts_mut(self.fb.pixels().as_mut_ptr(), self.fb.pixels().len()) };
            draw_tree(px, stride, c, w.surface, r.x - gx, r.y - gy, clip, 0);
        }
        if full.is_none() {
            self.draw_bar(y0, y1);
        }
        // popup (メニューなど) はいちばん上に
        for w in self.popups.clone() {
            let Some((ox, oy)) = self.surface_origin(w) else { continue };
            let Some(c) = self.clients.get(&w.client) else { continue };
            let clip = Rect { x: 0, y: y0, w: sw as i32, h: y1 - y0 };
            let px = unsafe { std::slice::from_raw_parts_mut(self.fb.pixels().as_mut_ptr(), self.fb.pixels().len()) };
            draw_tree(px, stride, c, w.surface, ox, oy, clip, 0);
        }
        if self.ptr_shown {
            self.draw_cursor(y0, y1);
        }
        self.fb.present_rows(y0 as usize, y1 as usize);
    }

    /// 窓の絵の中で、窓が始まるところ (set_window_geometry の x, y)。ポインタの場所をずらすのに使う
    fn geom_off(&self, w: Win) -> (i32, i32) {
        self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)).and_then(|s| s.geometry).map_or((0, 0), |g| (g.0, g.1))
    }

    fn title_of(&self, w: Win) -> String {
        self.clients.get(&w.client).and_then(|c| c.surfaces.get(&w.surface)).map(|s| s.title.clone()).unwrap_or_default()
    }

    /// バーの場所
    fn bar_rect(&self) -> Option<Rect> {
        let (top, _) = self.config.bar.as_ref()?;
        let y = if *top { 0 } else { self.height() - BAR_H };
        Some(Rect { x: 0, y, w: self.width(), h: BAR_H })
    }

    /// バーのワークスペースの札: (番号, 場所)
    fn bar_labels(&self) -> Vec<(u32, Rect)> {
        let (Some(b), Some(t)) = (self.bar_rect(), self.text.as_ref()) else { return vec![] };
        let mut x = 0;
        let mut out = vec![];
        for &n in self.spaces.keys() {
            let w = t.width(&n.to_string(), BAR_FONT) + 16;
            out.push((n, Rect { x, y: b.y, w, h: b.h }));
            x += w + 1;
        }
        out
    }

    fn bar_hit(&self, x: i32, y: i32) -> Option<u32> {
        self.bar_labels().into_iter().find(|(_, r)| r.contains(x, y)).map(|(n, _)| n)
    }

    fn draw_bar(&mut self, y0: i32, y1: i32) {
        let Some(b) = self.bar_rect() else { return };
        if b.y >= y1 || b.y + b.h <= y0 {
            return;
        }
        self.fill_rect(b, BAR_BG, y0, y1);
        let labels = self.bar_labels();
        let Some(t) = self.text.take() else { return };
        let base = b.y + BAR_H - 7;
        for (n, r) in &labels {
            let cur = *n == self.cur;
            self.fill_rect(*r, if cur { FOCUS } else { 0x262a36 }, y0, y1);
            t.draw(&mut self.fb, &n.to_string(), r.x + 8, base, BAR_FONT, if cur { 0x101218 } else { 0xc8d0e0 });
        }
        let left = labels.last().map_or(0, |(_, r)| r.x + r.w) + 12;
        // 真ん中: 作業中の窓の名前
        if let Some(f) = self.focus {
            let title = self.title_of(f);
            t.draw(&mut self.fb, &title, left, base, BAR_FONT, 0xc8d0e0);
        }
        // 右: status_command の行か時計
        let right = if self.status.is_empty() { self.clock.clone() } else { self.status.clone() };
        let w = t.width(&right, BAR_FONT);
        t.draw(&mut self.fb, &right, b.w - w - 10, base, BAR_FONT, 0xc8d0e0);
        self.text = Some(t);
    }

    /// タブの並び (Tabbed のとき、窓の上)
    fn tab_rects(&self, wins: &[Win]) -> Vec<(Win, Rect)> {
        let a = self.area();
        let n = wins.len().max(1) as i32;
        wins.iter().enumerate().map(|(i, w)| {
            let x0 = a.x + a.w * i as i32 / n;
            let x1 = a.x + a.w * (i as i32 + 1) / n;
            (*w, Rect { x: x0, y: a.y, w: x1 - x0 - 1, h: TAB_H - 1 })
        }).collect()
    }

    fn tab_hit(&self, x: i32, y: i32) -> Option<Win> {
        let ws = self.spaces.get(&self.cur)?;
        if ws.layout != Layout::Tabbed || ws.fullscreen.is_some() {
            return None;
        }
        self.tab_rects(&ws.wins).into_iter().find(|(_, r)| r.contains(x, y)).map(|(w, _)| w)
    }

    fn draw_tabs(&mut self, wins: &[Win], y0: i32, y1: i32) {
        let tabs = self.tab_rects(wins);
        let Some(t) = self.text.take() else { return };
        for (w, r) in tabs {
            let on = self.focus == Some(w);
            self.fill_rect(r, if on { FOCUS } else { 0x262a36 }, y0, y1);
            let title = self.title_of(w);
            t.draw(&mut self.fb, &title, r.x + 8, r.y + TAB_H - 7, BAR_FONT, if on { 0x101218 } else { 0xc8d0e0 });
        }
        self.text = Some(t);
    }

    /// 時計をすすめる (変わったらバーを描きなおす)
    fn tick(&mut self) {
        if self.config.bar.is_none() || !self.status.is_empty() {
            return;
        }
        let now = clock();
        if now != self.clock {
            self.clock = now;
            if let Some(b) = self.bar_rect() {
                self.mark(b.y, b.y + b.h);
            }
        }
    }

    /// status_command を動かし、その標準出力を読む fd
    fn spawn_status(&self, cmd: &str) -> Option<RawFd> {
        use std::os::fd::IntoRawFd;
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd).stdout(std::process::Stdio::piped());
        for (k, v) in &self.env {
            c.env(k, v);
        }
        let child = c.spawn().map_err(|e| eprintln!("aiwm: status_command: {}", e)).ok()?;
        let fd = child.stdout?.into_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        Some(fd)
    }

    fn read_status(&mut self) {
        let Some(fd) = self.status_fd else { return };
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            if n == 0 {
                unsafe { libc::close(fd) };
                self.status_fd = None;
            }
            return;
        }
        self.status_buf.extend_from_slice(&buf[..n as usize]);
        if let Some(end) = self.status_buf.iter().rposition(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(&self.status_buf[..end]).to_string();
            self.status_buf.drain(..=end);
            if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                self.status = line.trim().to_string();
                if let Some(b) = self.bar_rect() {
                    self.mark(b.y, b.y + b.h);
                }
            }
        }
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

/// 時計の文字 (年-月-日 時:分)
/// surface とその子の窓を、下から順に (ox, oy) を左上にして描く。clip の外は描かない。
/// 透明 (アルファ 0) のところは描かず、半透明は重ねる (premultiplied)
#[allow(clippy::too_many_arguments)]
fn draw_tree(px: &mut [u32], stride: usize, c: &Client, sid: u32, ox: i32, oy: i32, clip: Rect, depth: u32) {
    let Some(s) = c.surfaces.get(&sid) else { return };
    let own = [sid];
    let order: &[u32] = if s.stack.is_empty() { &own } else { &s.stack };
    for &k in order {
        if k != sid {
            if depth < 8 {
                if let Some(ks) = c.surfaces.get(&k) {
                    draw_tree(px, stride, c, k, ox + ks.pos.0, oy + ks.pos.1, clip, depth + 1);
                }
            }
            continue;
        }
        let (iw, ih) = (s.iw as i32, s.ih as i32);
        let x0 = ox.max(clip.x);
        let x1 = (ox + iw).min(clip.x + clip.w);
        let ya = oy.max(clip.y);
        let yb = (oy + ih).min(clip.y + clip.h);
        if x0 >= x1 || ya >= yb {
            continue;
        }
        for y in ya..yb {
            let src = &s.image[((y - oy) as usize) * s.iw + (x0 - ox) as usize..][..(x1 - x0) as usize];
            let dst = &mut px[y as usize * stride + x0 as usize..][..(x1 - x0) as usize];
            for (d, &p) in dst.iter_mut().zip(src) {
                let a = p >> 24;
                if a == 255 {
                    *d = p;
                } else if a != 0 {
                    let inv = 255 - a;
                    let ch = |sh: u32| (((p >> sh) & 255) + (((*d >> sh) & 255) * inv) / 255).min(255) << sh;
                    *d = ch(16) | ch(8) | ch(0);
                }
            }
        }
    }
}

fn clock() -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
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
