// aiboot: 小さな UEFI のブートローダー
//
// systemd-boot と同じ書き方の設定を ESP から読んで、カーネル (EFI スタブつきの Image) を起動する。
//   /loader/loader.conf         default PATTERN (* が使える), timeout N (秒、menu-force で待ちつづける)
//   /loader/entries/*.conf      title, version, sort-key, machine-id, linux PATH, options ... (何行でも)
// 並べかたと既定は systemd-boot と同じ: 残りが 0 の entry はうしろ、sort-key のあるものが前 (sort-key、machine-id、
// version の新しい順)、あとは名前 (id) の新しい順 (systemd の strverscmp_improved)。default は並べたなかで最初に
// 当たるもの (なければ先頭)。
// boot counting も systemd-boot と同じ (Automatic Boot Assessment): 名前が NAME+LEFT[-DONE].conf の entry は、
// 起動する前に NAME+(LEFT-1)-(DONE+1).conf に名前を変える。LEFT が 0 のものはうしろに並ぶので、default が
// 前のものに当たる。起動できたら OS が名前から数えを外す (aios では aiosd。Linux では systemd-bless-boot)。
// 読むだけの印のある entry は数えない。数える entry が読めなかったら、次の entry に aios.fallback_why=load をつける
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
use core::cmp::Ordering;
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
const F_GET_INFO: usize = 0x40;
const F_SET_INFO: usize = 0x48;
const F_FLUSH: usize = 0x50;
const FILE_MODE_READ: usize = 1;
const FILE_MODE_WRITE: usize = 2;
const FILE_READ_ONLY: u64 = 0x01;
const FILE_DIRECTORY: u64 = 0x10;

/// 5b1b31a1-9562-11d2-8e3f-00a0c969723b
const LOADED_IMAGE_GUID: [u8; 16] = [0xa1, 0x31, 0x1b, 0x5b, 0x62, 0x95, 0xd2, 0x11, 0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
/// 09576e92-6d3f-11d2-8e39-00a0c969723b (EFI_FILE_INFO)
const FILE_INFO_GUID: [u8; 16] = [0x92, 0x6e, 0x57, 0x09, 0x3f, 0x6d, 0xd2, 0x11, 0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
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

    /// 同じディレクトリの中で名前を変える (EFI_FILE_INFO の FileName を変えて SetInfo)。
    /// 読むだけの印があれば変えずに Some(false) (systemd-boot と同じく、数えないしるし)
    fn rename(&self, name: &str) -> Option<bool> {
        let mut buf = vec![0u8; 1024];
        let mut n = buf.len();
        if efi!(self.0, F_GET_INFO, (usize, *const u8, *mut usize, *mut u8), self.0, FILE_INFO_GUID.as_ptr(), &mut n, buf.as_mut_ptr()) != SUCCESS || n < 80 {
            return None;
        }
        if u64::from_le_bytes(buf[72..80].try_into().unwrap()) & FILE_READ_ONLY != 0 {
            return Some(false);
        }
        buf.truncate(80);
        for c in utf16(name) {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        let size = buf.len();
        buf[0..8].copy_from_slice(&(size as u64).to_le_bytes());
        (efi!(self.0, F_SET_INFO, (usize, *const u8, usize, *const u8), self.0, FILE_INFO_GUID.as_ptr(), size, buf.as_ptr()) == SUCCESS
            && efi!(self.0, F_FLUSH, (usize), self.0) == SUCCESS)
            .then_some(true)
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

struct Entry {
    /// いまのファイルの名前 (数えるときに変わる)
    file: String,
    /// 名前から数え (+LEFT-DONE) を外して小文字にしたもの (systemd-boot の id。default と並べるのに使う)
    id: String,
    title: String,
    linux: String,
    options: String,
    sort_key: String,
    machine_id: String,
    version: String,
    /// boot counting: 残りと済んだ回数と、次の名前 (数えない entry は None)
    tries: Option<(u32, u32, String)>,
    /// この起動で試して読めなかった (残りが 0 のものと同じく、うしろに並べる)
    failed: bool,
}

/// systemd-boot の boot counting の名前: 最後の + のあとが LEFT[-DONE] で、.conf で終わる
fn parse_tries(file: &str) -> Option<(String, (u32, u32, String))> {
    let plus = file.rfind('+')?;
    let num = |s: &str| -> Option<(u32, usize)> {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        let v = s[..n].parse::<u32>().ok().filter(|&v| v <= i32::MAX as u32)?;
        Some((v, n))
    };
    let rest = &file[plus + 1..];
    let (left, n) = num(rest)?;
    let mut rest = &rest[n..];
    let mut done = 0;
    if let Some(r) = rest.strip_prefix('-') {
        let (d, m) = num(r)?;
        done = d;
        rest = &r[m..];
    }
    if !rest.eq_ignore_ascii_case(".conf") {
        return None;
    }
    let next = alloc::format!("{}{}-{}.conf", &file[..=plus], left.saturating_sub(1), (done + 1).min(i32::MAX as u32));
    Some((alloc::format!("{}.conf", &file[..plus]).to_ascii_lowercase(), (left, done, next)))
}

fn parse_entry(file: &str, text: &str) -> Option<Entry> {
    let (id, tries) = match parse_tries(file) {
        Some((id, t)) => (id, Some(t)),
        None => (file.to_ascii_lowercase(), None),
    };
    let mut e = Entry {
        file: file.into(),
        id,
        title: String::new(),
        linux: String::new(),
        options: String::new(),
        sort_key: String::new(),
        machine_id: String::new(),
        version: String::new(),
        tries,
        failed: false,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once(char::is_whitespace).map(|(k, v)| (k, v.trim())).unwrap_or((line, ""));
        match k {
            "title" => e.title = v.into(),
            "version" => e.version = v.into(),
            "sort-key" => e.sort_key = v.into(),
            "machine-id" => e.machine_id = v.into(),
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
        e.title = e.id.trim_end_matches(".conf").into();
    }
    if !e.version.is_empty() {
        e.title = alloc::format!("{} ({})", e.title, e.version);
    }
    Some(e)
}

/// systemd の strverscmp_improved: 版のくらべかた (- . ^ で区切る。~ は前のもの、- のあるほうが古い、
/// 数は字より新しい、数は値で、字は辞書順で、長いほうが新しい)。Greater が新しい
fn verscmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let valid = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'~' | b'-' | b'^' | b'.');
    let (mut i, mut j) = (0, 0);
    loop {
        while at(a, i) != 0 && !valid(at(a, i)) {
            i += 1;
        }
        while at(b, j) != 0 && !valid(at(b, j)) {
            j += 1;
        }
        if at(a, i) == b'~' || at(b, j) == b'~' {
            let r = (at(a, i) != b'~').cmp(&(at(b, j) != b'~'));
            if r.is_ne() {
                return r;
            }
            i += 1;
            j += 1;
        }
        if at(a, i) == 0 || at(b, j) == 0 {
            return at(a, i).cmp(&at(b, j));
        }
        for sep in *b"-^." {
            if at(a, i) == sep || at(b, j) == sep {
                let r = (at(a, i) != sep).cmp(&(at(b, j) != sep));
                if r.is_ne() {
                    return r;
                }
                i += 1;
                j += 1;
            }
        }
        let run = |s: &[u8], k: usize, f: fn(&u8) -> bool| k + s.get(k..).unwrap_or(&[]).iter().take_while(|c| f(c)).count();
        let (ae, be);
        if at(a, i).is_ascii_digit() || at(b, j).is_ascii_digit() {
            ae = run(a, i, u8::is_ascii_digit);
            be = run(b, j, u8::is_ascii_digit);
            let r = (ae != i).cmp(&(be != j));
            if r.is_ne() {
                return r;
            }
            while at(a, i) == b'0' {
                i += 1;
            }
            while at(b, j) == b'0' {
                j += 1;
            }
            let r = (ae - i).cmp(&(be - j)).then_with(|| a[i..ae].cmp(&b[j..be]));
            if r.is_ne() {
                return r;
            }
        } else {
            ae = run(a, i, u8::is_ascii_alphabetic);
            be = run(b, j, u8::is_ascii_alphabetic);
            let m = (ae - i).min(be - j);
            let r = a[i..i + m].cmp(&b[j..j + m]).then_with(|| (ae - i).cmp(&(be - j)));
            if r.is_ne() {
                return r;
            }
        }
        i = ae;
        j = be;
    }
}

/// systemd-boot の boot_entry_compare と同じ並べかた (Less が前)
fn compare(a: &Entry, b: &Entry) -> Ordering {
    let bad = |e: &Entry| e.failed || matches!(e.tries, Some((0, _, _)));
    let none = |e: &Entry| e.sort_key.is_empty();
    bad(a)
        .cmp(&bad(b))
        .then_with(|| none(a).cmp(&none(b)))
        .then_with(|| {
            if none(a) {
                return Ordering::Equal;
            }
            a.sort_key.cmp(&b.sort_key).then_with(|| a.machine_id.cmp(&b.machine_id)).then_with(|| verscmp(&b.version, &a.version))
        })
        .then_with(|| verscmp(&b.id, &a.id))
        .then_with(|| match (&a.tries, &b.tries) {
            (Some((al, ad, _)), Some((bl, bd, _))) => bl.cmp(al).then_with(|| ad.cmp(bd)),
            _ => Ordering::Equal,
        })
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

/// default のパターンに当たるか (id は小文字なので、パターンも小文字にして渡す。.conf はあってもなくてもよい)
fn is_default(pat: &str, e: &Entry) -> bool {
    glob(pat, &e.id) || glob(pat, e.id.trim_end_matches(".conf"))
}

/// boot counting: 起動する前に名前の数を進める (systemd-boot の boot_entry_bump_counters と同じ)
fn bump(root: &File, e: &Entry) {
    let Some((left, _, next)) = &e.tries else { return };
    match root.open_rw(&alloc::format!("/loader/entries/{}", e.file)).and_then(|f| f.rename(next)) {
        Some(true) => say(&alloc::format!("aiboot: boot counting: {} -> {} ({} left after this)\n", e.file, next, left.saturating_sub(1))),
        Some(false) => say(&alloc::format!("aiboot: {} is read-only, not counting\n", e.file)),
        None => say(&alloc::format!("aiboot: cannot rename {} to {}\n", e.file, next)),
    }
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

    // entries (systemd-boot と同じ順に並べる)
    let mut entries: Vec<Entry> = Vec::new();
    if let Some(dir) = root.open("/loader/entries") {
        for n in dir.list().into_iter().filter(|n| n.to_ascii_lowercase().ends_with(".conf")) {
            if let Some(e) = read_text(&root, &alloc::format!("/loader/entries/{}", n)).and_then(|t| parse_entry(&n, &t)) {
                entries.push(e);
            }
        }
    }
    entries.sort_by(compare);
    if entries.is_empty() {
        say("aiboot: no entries in /loader/entries, booting /Image\n");
        return boot(image, &root, "/Image", "");
    }
    let default = default.to_ascii_lowercase();
    let def = if default.is_empty() { 0 } else { entries.iter().position(|e| is_default(&default, e)).unwrap_or(0) };
    let mut pick = if timeout == Some(0) { def } else { menu(&entries, def, timeout) };
    // 読めなければ、それをうしろに並べなおして、default をもう一度選ぶ (なければ、まだ試していない先頭のもの)。
    // systemd-boot は起動しなおし、残りの減った entry がうしろに並ぶので、同じものに行きつく。
    // 数える entry (新しいカーネルを試している) が読めなかったときは、次のものに aios.fallback_why=load をつける
    // (aiosd が /var/log/aiosd.log と aios get boot.last に残す)
    let mut why = "";
    loop {
        let e = &mut entries[pick];
        bump(&root, e);
        if why.is_empty() {
            boot(image, &root, &e.linux, &e.options);
        } else {
            boot(image, &root, &e.linux, alloc::format!("{} aios.fallback_why={}", e.options, why).trim_start());
        }
        // ここに来たら、いまの項目は読めなかった (boot はうまくいけば戻らない)
        if e.tries.is_some() {
            why = "load";
        }
        e.failed = true;
        entries.sort_by(compare);
        let Some(next) = entries.iter().position(|e| !e.failed && is_default(&default, e)).or_else(|| entries.iter().position(|e| !e.failed)) else {
            break;
        };
        pick = next;
    }
    LOAD_ERROR
}
