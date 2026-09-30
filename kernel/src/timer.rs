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

pub fn init() {
    crate::gic::enable(IRQ);
    rearm();
}

pub fn tick() {
    // 割り込み中の cpu0 だけが書く
    let t = TICKS.load(Ordering::Relaxed) + 1;
    TICKS.store(t, Ordering::Relaxed);
    if t % HZ == 0 {
        println!("uptime {}s", t / HZ);
    }
    rearm();
}
