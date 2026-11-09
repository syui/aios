// モジュール: カーネルに入っているが、起動しただけでは動かさないドライバ (画面、キーボードやマウス、音)
//
// aios のカーネルはあとから中身を読み込めないので、ドライバはぜんぶ中に入っていて、
// modprobe (finit_module) で起こす。/usr/lib/modules/NAME.ko (unix パッケージ) は
// 「aios-module NAME」と書いた札で、カーネルはそれを読んで NAME のドライバを起こす。
// 起動のときに起こすものは /etc/modules-load.d/*.conf に書く (init が読む。systemd と同じ)。
// 起こしたものは /proc/modules に出る。止める (rmmod) のはまだできない (EBUSY)
use crate::cell::Bkl;
use alloc::string::String;

struct Module {
    name: &'static str,
    /// 装置があれば起こして true
    init: fn() -> bool,
    loaded: bool,
}

static MODULES: Bkl<[Module; 3]> = Bkl::new([
    Module { name: "virtio_gpu", init: crate::gpu::init, loaded: false },
    Module { name: "virtio_input", init: input_init, loaded: false },
    Module { name: "virtio_snd", init: crate::sound::init, loaded: false },
]);

fn input_init() -> bool {
    crate::input::init();
    crate::input::count() > 0
}

#[track_caller]
fn modules() -> &'static mut [Module; 3] {
    MODULES.get()
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
    if !crate::proc::current().cred.capable(crate::cred::CAP_SYS_MODULE) {
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
    Ok(0)
}

/// rmmod: 止めることはまだできない
pub fn unload(name: &str) -> Result<i64, i64> {
    if !crate::proc::current().cred.capable(crate::cred::CAP_SYS_MODULE) {
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

/// システムコール (main.rs が syscall::register で置く。Linux の kernel/module/main.c)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[INIT_MODULE, FINIT_MODULE, DELETE_MODULE]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        INIT_MODULE => sys_init_module(a[0] as usize, a[1] as usize),
        FINIT_MODULE => sys_finit_module(a[0]),
        DELETE_MODULE => sys_delete_module(a[0] as usize),
        _ => Err(-38),
    }
}

/// init_module(image, len, params): 札 (module.rs) をメモリで
fn sys_init_module(image: usize, len: usize) -> Result<i64, i64> {
    let mut img = alloc::vec![0u8; len.min(4096)];
    crate::proc::current().pt().copy_in(&mut img, image).ok_or(-14)?;
    crate::module::load(&img)
}

/// finit_module(fd, params, flags): 札のファイル (/usr/lib/modules/NAME.ko) を読む
fn sys_finit_module(fd: u64) -> Result<i64, i64> {
    let f = crate::proc::current().files().get(fd).cloned().ok_or(-9)?;
    let mut img = alloc::vec![0u8; 4096];
    let n = f.borrow_mut().read(&mut img)?;
    img.truncate(n);
    crate::module::load(&img)
}

fn sys_delete_module(name: usize) -> Result<i64, i64> {
    let name = crate::proc::current().pt().copy_in_str(name, 64).ok_or(-14)?;
    crate::module::unload(core::str::from_utf8(&name).map_err(|_| crate::why::inval())?)
}
