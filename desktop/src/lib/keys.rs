// キーボード: evdev のキー番号 (KEY_*) から文字や名前へ。配列は us と jp
//   (XKB_DEFAULT_LAYOUT=jp のように選ぶ。aiwm の設定の xkb_layout が子に渡す)
#![allow(dead_code)]

pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_END: u16 = 107;
pub const KEY_DOWN: u16 = 108;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const BTN_LEFT: u16 = 0x110;

/// xkb の修飾キーの印 (wl_keyboard.modifiers と同じ)
pub const MOD_SHIFT: u32 = 1;
pub const MOD_LOCK: u32 = 2;
pub const MOD_CTRL: u32 = 4;
pub const MOD_ALT: u32 = 8;
pub const MOD_LOGO: u32 = 64;

/// 押されている修飾キー
#[derive(Default, Clone, Copy)]
pub struct Mods {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lmeta: bool,
    rmeta: bool,
    pub caps: bool,
}

impl Mods {
    /// キーが押された / 離された。修飾キーなら true
    pub fn update(&mut self, code: u16, pressed: bool) -> bool {
        let slot = match code {
            KEY_LEFTSHIFT => &mut self.lshift,
            KEY_RIGHTSHIFT => &mut self.rshift,
            KEY_LEFTCTRL => &mut self.lctrl,
            KEY_RIGHTCTRL => &mut self.rctrl,
            KEY_LEFTALT => &mut self.lalt,
            KEY_RIGHTALT => &mut self.ralt,
            KEY_LEFTMETA => &mut self.lmeta,
            KEY_RIGHTMETA => &mut self.rmeta,
            KEY_CAPSLOCK => {
                if pressed {
                    self.caps = !self.caps;
                }
                return true;
            }
            _ => return false,
        };
        *slot = pressed;
        true
    }

    pub fn shift(&self) -> bool {
        self.lshift || self.rshift
    }
    pub fn ctrl(&self) -> bool {
        self.lctrl || self.rctrl
    }
    pub fn alt(&self) -> bool {
        self.lalt || self.ralt
    }
    pub fn logo(&self) -> bool {
        self.lmeta || self.rmeta
    }

    /// wl_keyboard.modifiers の (depressed, locked)
    pub fn mask(&self) -> (u32, u32) {
        let mut d = 0;
        if self.shift() {
            d |= MOD_SHIFT;
        }
        if self.ctrl() {
            d |= MOD_CTRL;
        }
        if self.alt() {
            d |= MOD_ALT;
        }
        if self.logo() {
            d |= MOD_LOGO;
        }
        (d, if self.caps { MOD_LOCK } else { 0 })
    }
}

/// us 配列: キー番号 → (そのまま, Shift)
fn us(code: u16) -> Option<(char, char)> {
    Some(match code {
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        26 => ('[', '{'),
        27 => (']', '}'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        _ => {
            let c = letter(code)?;
            (c, c.to_ascii_uppercase())
        }
    })
}

/// jp 配列 (JIS): us と違うところだけ
fn jp(code: u16) -> Option<(char, char)> {
    Some(match code {
        3 => ('2', '"'),
        7 => ('6', '&'),
        8 => ('7', '\''),
        9 => ('8', '('),
        10 => ('9', ')'),
        11 => ('0', '0'),
        12 => ('-', '='),
        13 => ('^', '~'),
        26 => ('@', '`'),
        27 => ('[', '{'),
        39 => (';', '+'),
        40 => (':', '*'),
        43 => (']', '}'),
        89 => ('\\', '_'),
        124 => ('\\', '|'),
        41 => return None,
        _ => return us(code),
    })
}

fn letter(code: u16) -> Option<char> {
    let rows: [(u16, &str); 3] = [(16, "qwertyuiop"), (30, "asdfghjkl"), (44, "zxcvbnm")];
    for (start, s) in rows {
        if code >= start && ((code - start) as usize) < s.len() {
            return s.chars().nth((code - start) as usize);
        }
    }
    None
}

/// 文字になるキーなら、その文字 (Shift と CapsLock を考える)
pub fn char_of(code: u16, mods: &Mods, layout: &str) -> Option<char> {
    let (lo, hi) = if layout == "jp" { jp(code)? } else { us(code)? };
    let upper = if letter(code).is_some() { mods.shift() != mods.caps } else { mods.shift() };
    Some(if upper { hi } else { lo })
}

/// 配列の名前 (XKB_DEFAULT_LAYOUT)
pub fn layout() -> String {
    std::env::var("XKB_DEFAULT_LAYOUT").ok().filter(|l| !l.is_empty()).unwrap_or_else(|| "us".into())
}

/// bindsym で使うキーの名前 → 番号 (sway / xkb の keysym の名前)
pub fn code_of_name(name: &str) -> Option<u16> {
    let n = name.to_ascii_lowercase();
    let named = match n.as_str() {
        "return" | "enter" => Some(KEY_ENTER),
        "escape" => Some(KEY_ESC),
        "backspace" => Some(KEY_BACKSPACE),
        "tab" => Some(KEY_TAB),
        "space" => Some(KEY_SPACE),
        "left" => Some(KEY_LEFT),
        "right" => Some(KEY_RIGHT),
        "up" => Some(KEY_UP),
        "down" => Some(KEY_DOWN),
        "home" => Some(KEY_HOME),
        "end" => Some(KEY_END),
        "delete" => Some(KEY_DELETE),
        "minus" => Some(12),
        "equal" => Some(13),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some(f) = n.strip_prefix('f').and_then(|s| s.parse::<u16>().ok()) {
        return match f {
            1..=10 => Some(58 + f),
            11 => Some(87),
            12 => Some(88),
            _ => None,
        };
    }
    let mut cs = n.chars();
    let c = cs.next()?;
    if cs.next().is_some() {
        return None;
    }
    (1..128).find(|&k| us(k).is_some_and(|(lo, _)| lo == c))
}
