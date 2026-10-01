// ARM generic timer (EL1 physical timer, PPI 14 = INTID 30)
use core::sync::atomic::{AtomicU64, Ordering};

pub const IRQ: u32 = crate::irq::TIMER;
pub const HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn freq() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    // ファームウェアが設定していなければ、ラズパイの水晶 (19.2 MHz)
    if f == 0 { 19_200_000 } else { f }
}

fn rearm() {
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", in(reg) freq() / HZ);
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) 1u64);
    }
}

/// 起動したときの時刻 (UNIX 秒)。PL031 RTC があればそこから、なければ (ラズパイには
/// RTC がない) カーネルを作った時刻から始める
static BOOT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let rtc = if crate::dtb::present() { crate::dtb::reg_of("arm,pl031", 0).map(|(a, _)| a as usize) } else { Some(0x0901_0000) };
    let epoch = match rtc {
        Some(pa) => (unsafe { core::ptr::read_volatile(crate::memlayout::p2v(pa) as *const u32) }) as u64,
        None => env!("AIOS_BUILD_EPOCH").parse().unwrap_or(0),
    };
    BOOT_EPOCH.store(epoch.saturating_sub(uptime_ns() / 1_000_000_000), Ordering::Relaxed);
    crate::irq::enable(IRQ);
    crate::vdso::allow_counter();
    rearm();
}

/// 起動してからの時間 (ns)
pub fn uptime_ns() -> u64 {
    let cnt: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt) };
    let f = freq();
    (cnt / f) * 1_000_000_000 + (cnt % f) * 1_000_000_000 / f
}

/// 起動したときの UNIX 秒 (vDSO の vvar に入れる)
pub fn boot_epoch() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed)
}

pub fn epoch_ns() -> u64 {
    BOOT_EPOCH.load(Ordering::Relaxed) * 1_000_000_000 + uptime_ns()
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// 2 つめからの CPU: 自分のタイマを動かす
pub fn init_cpu() {
    crate::irq::enable(IRQ);
    crate::vdso::allow_counter();
    rearm();
}

/// タイマの割り込み (CPU ごと)。時刻を進めるなどは cpu0 だけ
pub fn tick() {
    crate::proc::account_tick();
    if crate::smp::id() != 0 {
        rearm();
        return;
    }
    let now = TICKS.load(Ordering::Relaxed) + 1;
    TICKS.store(now, Ordering::Relaxed);
    crate::proc::wake_expired(now);
    crate::signal::tick(now);
    // TCP の再送などのため、ときどき回す
    if now % 5 == 0 {
        crate::net::poll();
    }
    rearm();
}
