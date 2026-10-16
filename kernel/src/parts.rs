// 切れるところ (カーネルの実験のため): 起動の引数 aios.off=NAME,NAME (kernel.json の extra と quirk の名前) で、
// それがないカーネルのように起こす。システムコールは ENOSYS (Linux でそれを入れずに作ったときと同じ)、装置は起こさない、
// quirk は手当てをしない。どれを切れて、切るとどうなるかは kernel.json の off (test/kernel-json.py が NAMES とくらべる)。
// 起動のときに「parts: off NAME ...」と出す。aish の box (aish-vm) が、切って起こしてテストを動かし、切らないときとくらべる
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Part {
    Aiproc,
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
const NAMES: [&str; 16] = [
    "aiproc",
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
