// 切れるところ (カーネルの実験のため): 起動の引数 aios.off=NAME,NAME (kernel.json の extra と quirk の名前) で、
// それがないカーネルのように起こす。システムコールは ENOSYS (Linux でそれを入れずに作ったときと同じ)、装置は起こさない、
// quirk は手当てをしない。どれを切れて、切るとどうなるかは kernel.json の off (test/kernel-json.py が NAMES とくらべる)。
// 起動のときに「parts: off NAME ...」と出す。aish の box (aish-vm) が、切って起こしてテストを動かし、切らないときとくらべる。
// 動かしたままでも切ったり入れたりできる (/proc/ai/ctl の part NAME off|on。aish の part): 使うたびに見るもの
// (システムコール、quirk の手当て) だけ (RUNTIME)。起動のときに起こす装置と、/proc/ai そのもの (aiproc) はできない。
// いまの様子は /proc/ai/parts
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Part {
    Aiproc,
    Apparmor,
    Bcm2835Rng,
    Epoll,
    Inotify,
    Landlock,
    Module,
    Net,
    Ns,
    Seccomp,
    Swap,
    Timerfd,
    Vdso,
    VirtioRng,
    TtyRootFdlink,
    SoundMinPeriod,
    QemuUartBurst,
}

/// Part と同じ順の名前 (kernel.json の name)
const NAMES: [&str; 17] = [
    "aiproc",
    "apparmor",
    "bcm2835_rng",
    "epoll",
    "inotify",
    "landlock",
    "module",
    "net",
    "ns",
    "seccomp",
    "swap",
    "timerfd",
    "vdso",
    "virtio_rng",
    "tty-root-fdlink",
    "sound-min-period",
    "qemu-uart-burst",
];

/// 切ったもの (Part の番号のビット)
static OFF: AtomicU64 = AtomicU64::new(0);

/// 動かしたまま切ったり入れたりしてよいもの (使うたびに off を見るもの)。装置 (bcm2835_rng、net、virtio_rng) は起動の
/// ときに起こすかどうかなので、あとからは変えられない。aiproc を切ると /proc/ai/ctl 自身が消えるのでこれも
const RUNTIME: u64 = bit(Part::Apparmor)
    | bit(Part::Epoll)
    | bit(Part::Inotify)
    | bit(Part::Landlock)
    | bit(Part::Module)
    | bit(Part::Ns)
    | bit(Part::Seccomp)
    | bit(Part::Swap)
    | bit(Part::Timerfd)
    | bit(Part::Vdso)
    | bit(Part::TtyRootFdlink)
    | bit(Part::SoundMinPeriod)
    | bit(Part::QemuUartBurst);

const fn bit(p: Part) -> u64 {
    1 << p as u32
}

/// /proc/ai/ctl の part NAME off|on: 動かしたまま切る・入れる (RUNTIME のものだけ。ほかは EINVAL)
pub fn set(name: &str, on: bool) -> Result<(), i64> {
    let Some(i) = NAMES.iter().position(|n| *n == name) else {
        println!("parts: {}: no such part (kernel.json lists what can be turned off, as off)", name);
        return Err(crate::why::inval());
    };
    if RUNTIME & (1 << i) == 0 {
        println!("parts: {}: cannot be turned {} while running (a device started at boot, or /proc/ai itself): use aios.off=", name, if on { "on" } else { "off" });
        return Err(crate::why::inval());
    }
    if on {
        OFF.fetch_and(!(1 << i), Ordering::Relaxed);
    } else {
        OFF.fetch_or(1 << i, Ordering::Relaxed);
    }
    println!("parts: {} {} (by /proc/ai/ctl)", name, if on { "on" } else { "off" });
    Ok(())
}

/// /proc/ai/parts: 1 行の JSON。切れるものそれぞれの、いま切ってあるか、動かしたまま変えられるか
pub fn json() -> alloc::string::String {
    let off = OFF.load(Ordering::Relaxed);
    let v: alloc::vec::Vec<alloc::string::String> = NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| alloc::format!("{{\"name\":\"{}\",\"off\":{},\"runtime\":{}}}", n, off & (1 << i) != 0, RUNTIME & (1 << i) != 0))
        .collect();
    alloc::format!("{{\"parts\":[{}]}}\n", v.join(","))
}

/// 起動の引数の aios.off を読む (DTB を読んだあと、ドライバを起こす前に)
pub fn init() {
    let Some(arg) = crate::dtb::arg("aios.off") else { return };
    let mut bits = 0u64;
    for name in arg.split(',').filter(|n| !n.is_empty()) {
        match NAMES.iter().position(|n| *n == name) {
            Some(i) => bits |= 1 << i,
            None => println!("parts: aios.off={}: cannot turn it off (kernel.json lists what can, as off)", name),
        }
    }
    OFF.store(bits, Ordering::Relaxed);
    if bits != 0 {
        print!("parts: off");
        for (i, n) in NAMES.iter().enumerate() {
            if bits & (1 << i) != 0 {
                print!(" {}", n);
            }
        }
        println!();
    }
}

/// 切ってあるか
#[inline]
pub fn off(p: Part) -> bool {
    OFF.load(Ordering::Relaxed) & (1 << p as u32) != 0
}
