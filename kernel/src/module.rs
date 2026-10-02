// モジュール: カーネルに入っているが、起動しただけでは動かさないドライバ (画面、キーボードやマウス)
//
// aios のカーネルはあとから中身を読み込めないので、ドライバはぜんぶ中に入っていて、
// modprobe (finit_module) で起こす。/usr/lib/modules/NAME.ko (unix パッケージ) は
// 「aios-module NAME」と書いた札で、カーネルはそれを読んで NAME のドライバを起こす。
// 起動のときに起こすものは /etc/modules-load.d/*.conf に書く (init が読む。systemd と同じ)。
// 起こしたものは /proc/modules に出る。止める (rmmod) のはまだできない (EBUSY)
use alloc::string::String;

struct Module {
    name: &'static str,
    /// 装置があれば起こして true
    init: fn() -> bool,
    loaded: bool,
}

static mut MODULES: [Module; 2] = [
    Module { name: "virtio_gpu", init: crate::gpu::init, loaded: false },
    Module { name: "virtio_input", init: input_init, loaded: false },
];

fn input_init() -> bool {
    crate::input::init();
    crate::input::count() > 0
}

fn modules() -> &'static mut [Module; 2] {
    unsafe { &mut *(&raw mut MODULES) }
}

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBUSY: i64 = 16;
const EEXIST: i64 = 17;
const ENODEV: i64 = 19;
const ENOEXEC: i64 = 8;

/// 札 (「aios-module NAME」) からモジュールの名前
fn name_of(image: &[u8]) -> Result<&str, i64> {
    let s = core::str::from_utf8(image).map_err(|_| -ENOEXEC)?;
    let line = s.lines().find(|l| !l.trim().is_empty() && !l.starts_with('#')).ok_or(-ENOEXEC)?;
    line.trim().strip_prefix("aios-module").map(str::trim).filter(|n| !n.is_empty()).ok_or(-ENOEXEC)
}

/// 札の中身でモジュールを起こす (root だけ)
pub fn load(image: &[u8]) -> Result<i64, i64> {
    if crate::proc::current().cred.euid != 0 {
        return Err(-EPERM);
    }
    let name = name_of(image)?;
    let m = modules().iter_mut().find(|m| m.name == name).ok_or(-ENOENT)?;
    if m.loaded {
        return Err(-EEXIST);
    }
    if !(m.init)() {
        println!("module {}: no device (virtio:{})", name, crate::virtio::list());
        return Err(-ENODEV);
    }
    m.loaded = true;
    crate::fs::add_device_nodes();
    Ok(0)
}

/// rmmod: 止めることはまだできない
pub fn unload(name: &str) -> Result<i64, i64> {
    if crate::proc::current().cred.euid != 0 {
        return Err(-EPERM);
    }
    match modules().iter().find(|m| m.name == name) {
        Some(m) if m.loaded => Err(-EBUSY),
        _ => Err(-ENOENT),
    }
}

/// /proc/modules (Linux と同じ形: 名前 大きさ 使っている数 依存 状態 場所)
pub fn proc_modules() -> String {
    let mut s = String::new();
    for m in modules().iter().filter(|m| m.loaded) {
        s.push_str(&alloc::format!("{} 0 0 - Live 0x0000000000000000\n", m.name));
    }
    s
}
