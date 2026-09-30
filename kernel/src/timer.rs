// ARM generic timer (EL1 physical timer, PPI 14 = INTID 30)
use core::sync::atomic::{AtomicU64, Ordering};

pub const IRQ: u32 = 30;
pub const HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

fn freq() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    f
}

fn rearm() {
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", in(reg) freq() / HZ);
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) 1u64);
    }
}

/// 起動したときの時刻 (UNIX 秒, PL031 RTC から)
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let epoch = unsafe { core::ptr::read_volatile(crate::memlayout::RTC as *const u32) };
    BOOT_EPOCH.store(epoch as u64 - uptime_ns() / 1_000_000_000, Ordering::Relaxed);
    crate::gic::enable(IRQ);
    rearm();
}

/// 起動してからの時間 (ns)
pub fn uptime_ns() -> u64 {
    let cnt: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt) };
    let f = freq();
    (cnt / f) * 1_000_000_000 + (cnt % f) * 1_000_000_000 / f
}

pub fn epoch_ns() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed) * 1_000_000_000 + uptime_ns()
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn tick() {
    // 割り込み中の cpu0 だけが書く
    let now = TICKS.load(Ordering::Relaxed) + 1;
    TICKS.store(now, Ordering::Relaxed);
    crate::proc::wake_expired(now);
    rearm();
}
