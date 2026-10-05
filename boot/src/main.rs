// aiboot: 小さな UEFI のブートローダー
//
// systemd-boot と同じ書き方の設定を ESP から読んで、カーネル (EFI スタブつきの Image) を起動する。
//   /loader/loader.conf         default PATTERN (* が使える), timeout N (秒、menu-force で待ちつづける)
//   /loader/entries/*.conf      title, version, linux PATH, options ... (何行でも)
//   /loader/try                 新しいカーネルを試す回数 (aiosd が aios install kernel で書き、起動できたら消す)。
//                               あれば 1 減らして起動し、0 のときは前のカーネル (prev-aios.conf) を
//                               aios.fallback=1 をつけて起動する (systemd-boot の boot counting を小さくしたもの)
// 項目がなければ /Image をそのまま起動する。timeout があれば番号を選べる一覧を出す
// (数字で選ぶ、Enter で既定、ほかのキーで数えるのを止める)。
// カーネルは LoadImage でメモリから読み込み、options を LoadOptions (UTF-16) に入れて StartImage する。
// aios のカーネルはここからコマンドラインを受けとる (kernel/src/efi.rs)。Linux の EFI スタブも同じ。
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{read_volatile, write_volatile};

type Status = usize;
const SUCCESS: Status = 0;
const LOAD_ERROR: Status = (1 << 63) | 1;
const NOT_FOUND: Status = (1 << 63) | 14;
const LOADER_DATA: usize = 2;

// EFI_SYSTEM_TABLE
const ST_CONIN: usize = 0x30;
const ST_CONOUT: usize = 0x40;
const ST_BOOT_SERVICES: usize = 0x60;
// EFI_BOOT_SERVICES
const BS_ALLOCATE_POOL: usize = 0x40;
const BS_FREE_POOL: usize = 0x48;
const BS_HANDLE_PROTOCOL: usize = 0x98;
const BS_LOAD_IMAGE: usize = 0xc8;
const BS_START_IMAGE: usize = 0xd0;
const BS_STALL: usize = 0xf8;
const BS_SET_WATCHDOG: usize = 0x100;
// EFI_LOADED_IMAGE_PROTOCOL
const LI_DEVICE: usize = 0x18;
const LI_OPTIONS_SIZE: usize = 0x30;
const LI_OPTIONS: usize = 0x38;
// EFI_FILE_PROTOCOL
const F_OPEN: usize = 0x08;
const F_CLOSE: usize = 0x10;
const F_READ: usize = 0x20;
const F_WRITE: usize = 0x28;
const F_FLUSH: usize = 0x50;
const FILE_MODE_READ: usize = 1;
const FILE_MODE_WRITE: usize = 2;
const FILE_DIRECTORY: u64 = 0x10;

/// 5b1b31a1-9562-11d2-8e3f-00a0c969723b
const LOADED_IMAGE_GUID: [u8; 16] = [0xa1, 0x31, 0x1b, 0x5b, 0x62, 0x95, 0xd2, 0x11, 0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
/// 964e5b22-6459-11d2-8e39-00a0c969723b
const SIMPLE_FS_GUID: [u8; 16] = [0x22, 0x5b, 0x4e, 0x96, 0x59, 0x64, 0xd2, 0x11, 0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];

static mut ST: usize = 0;

fn st() -> usize {
    unsafe { ST }
}

fn bs() -> usize {
    rd(st() + ST_BOOT_SERVICES)
}

fn rd(a: usize) -> usize {
    unsafe { read_volatile(a as *const usize) }
}

/// 表の中の関数を呼ぶ
macro_rules! efi {
    ($table:expr, $off:expr, ($($t:ty),*), $($arg:expr),*) => {{
        let f: extern "efiapi" fn($($t),*) -> Status = unsafe { core::mem::transmute(rd($table + $off)) };
        f($($arg),*)
    }};
}

// ---- メモリ (AllocatePool) ----

struct Pool;

unsafe impl GlobalAlloc for Pool {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // AllocatePool は 8 バイト境界。それより大きい境界は使わない
        if l.align() > 8 {
            return core::ptr::null_mut();
        }
        let mut p = 0usize;
        if efi!(bs(), BS_ALLOCATE_POOL, (usize, usize, *mut usize), LOADER_DATA, l.size().max(1), &mut p) != SUCCESS {
            return core::ptr::null_mut();
        }
        p as *mut u8
    }

    unsafe fn dealloc(&self, p: *mut u8, _: Layout) {
        efi!(bs(), BS_FREE_POOL, (usize), p as usize);
    }
}

#[global_allocator]
static POOL: Pool = Pool;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    say("aiboot: panic\n");
    loop {
        core::hint::spin_loop();
    }
}

// ---- 文字 ----

fn utf16(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

fn say(s: &str) {
    let conout = rd(st() + ST_CONOUT);
    if conout == 0 {
        return;
    }
    let mut v: Vec<u16> = Vec::new();
    for c in s.encode_utf16() {
        if c == b'\n' as u16 {
            v.push(b'\r' as u16);
        }
        v.push(c);
    }
    v.push(0);
    efi!(conout, 0x08, (usize, *const u16), conout, v.as_ptr());
}

/// キーが押されていれば、その文字 (特別なキーは 0)
fn key() -> Option<u16> {
    let conin = rd(st() + ST_CONIN);
    let mut k = [0u16; 2];
    (efi!(conin, 0x08, (usize, *mut u16), conin, k.as_mut_ptr()) == SUCCESS).then_some(k[1])
}

// ---- ファイル (EFI_FILE_PROTOCOL) ----

struct File(usize);

impl File {
    fn open(&self, path: &str) -> Option<File> {
        let name = utf16(&path.replace('/', "\\"));
        let mut h = 0usize;
        let r = efi!(self.0, F_OPEN, (usize, *mut usize, *const u16, usize, usize), self.0, &mut h, name.as_ptr(), FILE_MODE_READ, 0);
        (r == SUCCESS && h != 0).then_some(File(h))
    }

    /// 読み書きで開く (あるファイルだけ)
    fn open_rw(&self, path: &str) -> Option<File> {
        let name = utf16(&path.replace('/', "\\"));
        let mut h = 0usize;
        let r = efi!(self.0, F_OPEN, (usize, *mut usize, *const u16, usize, usize), self.0, &mut h, name.as_ptr(), FILE_MODE_READ | FILE_MODE_WRITE, 0);
        (r == SUCCESS && h != 0).then_some(File(h))
    }

    /// 頭から書く (前より短いと後ろが残るので、同じ長さで書く)
    fn write(&self, data: &[u8]) -> bool {
        let mut n = data.len();
        efi!(self.0, F_WRITE, (usize, *mut usize, *const u8), self.0, &mut n, data.as_ptr()) == SUCCESS
            && n == data.len()
            && efi!(self.0, F_FLUSH, (usize), self.0) == SUCCESS
    }

    fn read(&self, buf: &mut [u8]) -> Option<usize> {
        let mut n = buf.len();
        (efi!(self.0, F_READ, (usize, *mut usize, *mut u8), self.0, &mut n, buf.as_mut_ptr()) == SUCCESS).then_some(n)
    }

    fn read_all(&self) -> Option<Vec<u8>> {
        let mut v = Vec::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                return Some(v);
            }
            v.extend_from_slice(&buf[..n]);
        }
    }

    /// ディレクトリの中のファイルの名前
    fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1024];
        while let Some(n) = self.read(&mut buf) {
            if n < 82 {
                break;
            }
            let attr = u64::from_le_bytes(buf[72..80].try_into().unwrap());
            let name: Vec<u16> = buf[80..n].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
            if attr & FILE_DIRECTORY == 0 {
                out.push(String::from_utf16_lossy(&name));
            }
        }
        out
    }
}

impl Drop for File {
    fn drop(&mut self) {
        efi!(self.0, F_CLOSE, (usize), self.0);
    }
}

fn protocol(handle: usize, guid: &[u8; 16]) -> Option<usize> {
    let mut p = 0usize;
    (efi!(bs(), BS_HANDLE_PROTOCOL, (usize, *const u8, *mut usize), handle, guid.as_ptr(), &mut p) == SUCCESS && p != 0).then_some(p)
}

fn read_text(root: &File, path: &str) -> Option<String> {
    root.open(path)?.read_all().map(|v| String::from_utf8_lossy(&v).into_owned())
}

// ---- 設定 ----

/// 前のカーネルのエントリ (aiosd が aios install kernel で作る)
const PREV_ENTRY: &str = "prev-aios.conf";

struct Entry {
    id: String,
    title: String,
    linux: String,
    options: String,
}

fn parse_entry(id: &str, text: &str) -> Option<Entry> {
    let mut e = Entry { id: id.into(), title: String::new(), linux: String::new(), options: String::new() };
    let mut version = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once(char::is_whitespace).map(|(k, v)| (k, v.trim())).unwrap_or((line, ""));
        match k {
            "title" => e.title = v.into(),
            "version" => version = v.into(),
            "linux" | "efi" => e.linux = v.into(),
            "options" => {
                if !e.options.is_empty() {
                    e.options.push(' ');
                }
                e.options.push_str(v);
            }
            "initrd" => say("aiboot: initrd is not supported, ignored\n"),
            _ => {}
        }
    }
    if e.linux.is_empty() {
        return None;
    }
    if e.title.is_empty() {
        e.title = id.trim_end_matches(".conf").into();
    }
    if !version.is_empty() {
        e.title = alloc::format!("{} ({})", e.title, version);
    }
    Some(e)
}

/// * だけの簡単なパターン
fn glob(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((a, b)) => {
            s.starts_with(a) && {
                let rest = &s[a.len()..];
                (0..=rest.len()).filter(|&i| rest.is_char_boundary(i)).any(|i| glob(b, &rest[i..]))
            }
        }
    }
}

fn is_default(pat: &str, e: &Entry) -> bool {
    glob(pat, &e.id) || glob(pat, e.id.trim_end_matches(".conf"))
}

/// 一覧を出して選んでもらう。timeout は秒 (None は待ちつづける)
fn menu(entries: &[Entry], def: usize, timeout: Option<usize>) -> usize {
    say("\naiboot\n");
    for (i, e) in entries.iter().enumerate().take(9) {
        say(&alloc::format!("  {} {}. {}\n", if i == def { '*' } else { ' ' }, i + 1, e.title));
    }
    let mut left = timeout.map(|t| t * 10);
    if let Some(t) = timeout {
        say(&alloc::format!("boot {} in {} s (1-{}: choose, Enter: boot, other keys: wait)\n", def + 1, t, entries.len().min(9)));
    }
    loop {
        if let Some(c) = key() {
            match c {
                0x0d => return def,
                c if (b'1' as u16..=b'9' as u16).contains(&c) && ((c - b'1' as u16) as usize) < entries.len() => return (c - b'1' as u16) as usize,
                _ => {
                    if left.is_some() {
                        say("waiting\n");
                    }
                    left = None;
                }
            }
        }
        if let Some(l) = left.as_mut() {
            if *l == 0 {
                return def;
            }
            *l -= 1;
        }
        efi!(bs(), BS_STALL, (usize), 100_000);
    }
}

/// カーネルを読み込んで、options を渡して起動する (戻ってきたら失敗)
fn boot(image: usize, root: &File, path: &str, options: &str) -> Status {
    say(&alloc::format!("aiboot: {} {}\n", path, options));
    let Some(data) = root.open(path).and_then(|f| f.read_all()) else {
        say(&alloc::format!("aiboot: cannot read {}\n", path));
        return NOT_FOUND;
    };
    let mut child = 0usize;
    let r = efi!(bs(), BS_LOAD_IMAGE, (u8, usize, usize, *const u8, usize, *mut usize), 0, image, 0, data.as_ptr(), data.len(), &mut child);
    drop(data);
    if r != SUCCESS {
        say(&alloc::format!("aiboot: LoadImage {} failed ({:#x})\n", path, r));
        return r;
    }
    if !options.is_empty()
        && let Some(li) = protocol(child, &LOADED_IMAGE_GUID)
    {
        // 起動したあとも読めるように、LoadOptions は放しておく
        let opts = utf16(options).leak();
        unsafe {
            write_volatile((li + LI_OPTIONS_SIZE) as *mut u32, (opts.len() * 2) as u32);
            write_volatile((li + LI_OPTIONS) as *mut usize, opts.as_ptr() as usize);
        }
    }
    let r = efi!(bs(), BS_START_IMAGE, (usize, *mut usize, usize), child, core::ptr::null_mut(), 0);
    say(&alloc::format!("aiboot: {} returned ({:#x})\n", path, r));
    r
}

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: usize, st: usize) -> Status {
    unsafe { ST = st };
    // 一覧で待っている間に、ファームウェアの番犬 (5 分) にリセットされないように
    efi!(bs(), BS_SET_WATCHDOG, (usize, u64, usize, usize), 0, 0, 0, 0);
    let Some(root) = protocol(image, &LOADED_IMAGE_GUID)
        .map(|li| rd(li + LI_DEVICE))
        .and_then(|dev| protocol(dev, &SIMPLE_FS_GUID))
        .and_then(|fs| {
            let mut h = 0usize;
            (efi!(fs, 0x08, (usize, *mut usize), fs, &mut h) == SUCCESS).then_some(File(h))
        })
    else {
        say("aiboot: cannot open the boot volume\n");
        return LOAD_ERROR;
    };

    // loader.conf
    let mut default = String::new();
    let mut timeout = Some(0);
    if let Some(conf) = read_text(&root, "/loader/loader.conf") {
        for line in conf.lines() {
            let mut w = line.split_whitespace();
            match (w.next(), w.next()) {
                (Some("default"), Some(v)) => default = v.into(),
                (Some("timeout"), Some("menu-force")) => timeout = None,
                (Some("timeout"), Some(v)) => timeout = v.parse().ok().or(Some(0)),
                _ => {}
            }
        }
    }

    // entries
    let mut entries: Vec<Entry> = Vec::new();
    if let Some(dir) = root.open("/loader/entries") {
        let mut names: Vec<String> = dir.list().into_iter().filter(|n| n.to_ascii_lowercase().ends_with(".conf")).collect();
        names.sort();
        for n in names {
            if let Some(e) = read_text(&root, &alloc::format!("/loader/entries/{}", n)).and_then(|t| parse_entry(&n, &t)) {
                entries.push(e);
            }
        }
    }
    if entries.is_empty() {
        say("aiboot: no entries in /loader/entries, booting /Image\n");
        return boot(image, &root, "/Image", "");
    }
    let mut def = if default.is_empty() { 0 } else { entries.iter().position(|e| is_default(&default, e)).unwrap_or(0) };

    // boot counting: /loader/try が 0 なら、新しいカーネルは前に起動しなかった
    // 無いファイルを開くとファームウェア (edk2 の FAT) が例外で止まることがあるので、先に一覧で確かめる
    let has_try = root.open("/loader").is_some_and(|d| d.list().iter().any(|n| n.eq_ignore_ascii_case("try")));
    let trying = if has_try { read_text(&root, "/loader/try").map(|t| t.trim().parse::<u32>().unwrap_or(0)) } else { None };
    if let Some(left) = trying {
        if left == 0 {
            if let Some(i) = entries.iter().position(|e| e.id == PREV_ENTRY) {
                say("aiboot: the new kernel did not finish booting; booting the previous kernel\n");
                def = i;
            }
        } else {
            // 数字の桁は変わらないように (9 → 8 など、1 桁で使う)
            let ok = root.open_rw("/loader/try").is_some_and(|f| f.write(alloc::format!("{}", left - 1).as_bytes()));
            say(&alloc::format!("aiboot: trying the new kernel ({} more {})\n", left - 1, if ok { "after this" } else { "- cannot write /loader/try" }));
        }
    }
    let mut pick = if timeout == Some(0) { def } else { menu(&entries, def, timeout) };
    // 起動できなければ、ほかの項目を順に
    for _ in 0..entries.len() {
        let e = &entries[pick];
        if trying.is_some() && e.id == PREV_ENTRY {
            // 新しいカーネルを試しているのに前のもので起動する (自動で、一覧で選んだ、新しいものが読めなかった):
            // aiosd が /boot/Image を前のものに戻す
            let options = if e.options.is_empty() { "aios.fallback=1".into() } else { alloc::format!("{} aios.fallback=1", e.options) };
            boot(image, &root, &e.linux, &options);
        } else {
            boot(image, &root, &e.linux, &e.options);
        }
        pick = (pick + 1) % entries.len();
    }
    LOAD_ERROR
}
