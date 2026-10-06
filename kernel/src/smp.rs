// 複数 CPU
//
// カーネルは大きなロック (BKL) 1 つで守る: カーネルの中を走れるのはいつも 1 つの CPU だけで、
// ユーザーのプログラムはすべての CPU で同時に動く (Linux 2.0 のころと同じ)。
//   EL0 から例外で入ったらロックを取り、EL0 へ戻る直前に放す (trap.rs、forkret)。
//   スケジューラはロックを持ったまま回し、することがなければ放して割り込みを待つ。
// これでいままでの 1 CPU 向けのコード (static mut、RefCell) はそのまま使える。
// ロックなしで通すもの: 自分のことを読むだけのシステムコール (syscall::fast) と、自分だけの無名の
// 領域のページフォルト (vm.rs fast_fault。表を変える側は mutating() でそれを止めて待つ)。
//
// 2 つめからの CPU は DTB の cpu ノードの enable-method で起こす:
//   psci        PSCI の CPU_ON (QEMU virt、UEFI)
//   spin-table  cpu-release-addr に入口を書いて sev (ラズパイのファームウェア)
// 起きた CPU は boot.rs と同じページ表で MMU を入れ、自分のスタックでスケジューラに入る。
// TPIDR_EL1 に CPU の番号を入れておく。
use crate::memlayout::{p2v, v2p};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const MAXCPU: usize = 8;
const STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stacks([[u8; STACK_SIZE]; MAXCPU]);

#[unsafe(no_mangle)]
static mut secondary_stacks: Stacks = Stacks([[0; STACK_SIZE]; MAXCPU]);

/// 動いている CPU の数
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// CPU の番号ごとの MPIDR の Aff0 (CPU 間の割り込みの宛先)
static mut TARGET: [usize; MAXCPU] = [0; MAXCPU];

/// 仕事がなくて眠っている CPU (ビット)
static IDLE: AtomicUsize = AtomicUsize::new(0);

/// この CPU は眠る / 起きた (スケジューラが、大きなロックを持ったまま呼ぶ)
pub fn set_idle(on: bool) {
    let bit = 1 << id();
    if on {
        IDLE.fetch_or(bit, Ordering::AcqRel);
    } else {
        IDLE.fetch_and(!bit, Ordering::AcqRel);
    }
}

/// 眠っている CPU を起こす (動けるプロセスができた)。sev ではなく割り込みで:
/// Mac の Hypervisor.framework (HVF) では wfe が眠らず sev も届かないので、ひまな CPU は wfi で眠っている
pub fn wake_idle() {
    let idle = IDLE.load(Ordering::Acquire) & !(1 << id());
    for cpu in 0..online() {
        if idle & (1 << cpu) != 0 {
            crate::irq::send_ipi(unsafe { TARGET[cpu] });
        }
    }
}

/// ほかの CPU に、EL0 から戻ってくるように知らせる (殺された、シグナルが来た)
pub fn kick(cpu: usize) {
    if cpu != id() && cpu < online() {
        crate::irq::send_ipi(unsafe { TARGET[cpu] });
    }
}

/// いまの CPU の番号 (0 から)
pub fn id() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mrs {}, tpidr_el1", out(reg) v) };
    v
}

pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

// ---- 大きなロック ----

/// 持っている CPU の番号 + 1 (0 なら空き)
static BKL: AtomicUsize = AtomicUsize::new(0);

pub fn lock() {
    let me = id() + 1;
    debug_assert!(BKL.load(Ordering::Relaxed) != me, "BKL: cpu{} locks twice", me - 1);
    let t0 = crate::timer::uptime_ns();
    while BKL.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let t1 = crate::timer::uptime_ns();
    let c = &STATS.cpu[me - 1];
    c.wait.fetch_add(t1 - t0, Ordering::Relaxed);
    c.locks.fetch_add(1, Ordering::Relaxed);
    c.since.store(t1, Ordering::Relaxed);
}

pub fn unlock() {
    let c = &STATS.cpu[id()];
    c.hold.fetch_add(crate::timer::uptime_ns().saturating_sub(c.since.load(Ordering::Relaxed)), Ordering::Relaxed);
    BKL.store(0, Ordering::Release);
}

// ---- 大きなロックの統計 (/proc/bkl。ロックを細かくするとき、どこから分けるかを決めるため) ----

const NSYS: usize = 512;

struct CpuStat {
    /// ロックを待っていた時間 (ns)
    wait: AtomicU64,
    /// ロックを持っていた時間 (ns)
    hold: AtomicU64,
    locks: AtomicU64,
    /// いま持ちはじめた時刻
    since: AtomicU64,
}

struct Stats {
    cpu: [CpuStat; MAXCPU],
    /// システムコールの番号ごとの (回数, ロックを持っていた時間)。途中で眠ったもの (wait4 など) はのぞく
    sys: [(AtomicU64, AtomicU64); NSYS],
    fault: (AtomicU64, AtomicU64),
    irq: (AtomicU64, AtomicU64),
    /// 途中で眠ったので数えなかったもの
    slept: AtomicU64,
    /// 数えはじめた時刻 (書くと 0 からやりなおす)
    start: AtomicU64,
}

const Z: AtomicU64 = AtomicU64::new(0);
static STATS: Stats = Stats {
    cpu: [const { CpuStat { wait: Z, hold: Z, locks: Z, since: Z } }; MAXCPU],
    sys: [const { (Z, Z) }; NSYS],
    fault: (Z, Z),
    irq: (Z, Z),
    slept: Z,
    start: Z,
};

/// スケジューラでほかのプロセスへ切りかえた回数 (ロックを持ったまま眠ったかを見る)
pub static SWITCHES: AtomicU64 = AtomicU64::new(0);

/// 大きなロックなしで片づけたページフォルト (vm.rs fast_fault)
pub static FAST_FAULTS: AtomicU64 = AtomicU64::new(0);

/// 例外 1 つぶんの、ロックを持っていた時間を数える (trap.rs)。sys はシステムコールの番号
pub enum Cause {
    Sys(u64),
    Fault,
    Irq,
}

pub fn account(cause: Cause, ns: u64, slept: bool) {
    if slept {
        STATS.slept.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let slot = match cause {
        Cause::Sys(n) if (n as usize) < NSYS => &STATS.sys[n as usize],
        Cause::Sys(_) => return,
        Cause::Fault => &STATS.fault,
        Cause::Irq => &STATS.irq,
    };
    slot.0.fetch_add(1, Ordering::Relaxed);
    slot.1.fetch_add(ns, Ordering::Relaxed);
}

/// 数えなおす (/proc/bkl に書く)
pub fn stats_reset() {
    for c in &STATS.cpu {
        c.wait.store(0, Ordering::Relaxed);
        c.hold.store(0, Ordering::Relaxed);
        c.locks.store(0, Ordering::Relaxed);
    }
    for (n, t) in STATS.sys.iter().chain([&STATS.fault, &STATS.irq]) {
        n.store(0, Ordering::Relaxed);
        t.store(0, Ordering::Relaxed);
    }
    STATS.slept.store(0, Ordering::Relaxed);
    FAST_FAULTS.store(0, Ordering::Relaxed);
    STATS.start.store(crate::timer::uptime_ns(), Ordering::Relaxed);
}

/// /proc/bkl
pub fn stats() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    let ms = |ns: u64| ns as f64 / 1e6;
    let span = crate::timer::uptime_ns().saturating_sub(STATS.start.load(Ordering::Relaxed)).max(1);
    let pct = |ns: u64| ns as f64 * 100.0 / span as f64;
    let mut s = String::new();
    s.push_str(&format!("# 大きなロック (BKL)。{:.1} 秒のあいだ (echo > /proc/bkl で数えなおす)
", span as f64 / 1e9));
    s.push_str("cpu      wait (待った)          hold (持っていた)        locks
");
    let (mut tw, mut th) = (0, 0);
    for (i, c) in STATS.cpu.iter().enumerate().take(online()) {
        let (w, h) = (c.wait.load(Ordering::Relaxed), c.hold.load(Ordering::Relaxed));
        tw += w;
        th += h;
        s.push_str(&format!("{:<4} {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%  {:>10}
", i, ms(w), pct(w), ms(h), pct(h), c.locks.load(Ordering::Relaxed)));
    }
    s.push_str(&format!("all  {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%   (1 CPU = 100%)

", ms(tw), pct(tw), ms(th), pct(th)));
    // 持っていた時間の長いもの (眠らなかったものだけ)
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for (i, (n, t)) in STATS.sys.iter().enumerate() {
        let n = n.load(Ordering::Relaxed);
        if n > 0 {
            let name = crate::syscall::name(i as u64).map_or(format!("sys{}", i), |x| x.to_ascii_lowercase());
            rows.push((name, n, t.load(Ordering::Relaxed)));
        }
    }
    for (name, slot) in [("(page fault)", &STATS.fault), ("(irq)", &STATS.irq)] {
        let n = slot.0.load(Ordering::Relaxed);
        if n > 0 {
            rows.push((name.into(), n, slot.1.load(Ordering::Relaxed)));
        }
    }
    rows.sort_by_key(|r| core::cmp::Reverse(r.2));
    s.push_str("hold の長いもの            回数       合計       平均
");
    for (name, n, t) in rows.iter().take(25) {
        s.push_str(&format!("{:<20} {:>10} {:>9.1} ms {:>8.1} us
", name, n, ms(*t), *t as f64 / 1e3 / *n as f64));
    }
    s.push_str(&format!("(途中で眠ったので数えなかったもの: {}。ロックなしで片づけたページフォルト: {})
", STATS.slept.load(Ordering::Relaxed), FAST_FAULTS.load(Ordering::Relaxed)));
    s
}

/// この CPU がロックを持っているか
pub fn holding() -> bool {
    BKL.load(Ordering::Relaxed) == id() + 1
}

// ---- 2 つめからの CPU ----

core::arch::global_asm!(
    r#"
.equ S_MAIR, 0xff00
.equ S_TCR, (25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (2 << 32))

.section .text
// 物理アドレスで、MMU なしで来る。x0 = CPU の番号
.global secondary_entry
secondary_entry:
    msr     daifset, #0xf
    mov     x21, x0
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    1f
    // EL2 なら EL1 へ (boot.rs と同じ)
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33ff
    msr     cptr_el2, x0
    msr     hstr_el2, xzr
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, 1f
    msr     elr_el2, x0
    eret
1:  ldr     x0, =0x30d00800
    msr     sctlr_el1, x0
    isb
    // cpu0 が作ったページ表をそのまま使う
    adrp    x0, boot_l1_lo
    add     x0, x0, :lo12:boot_l1_lo
    msr     ttbr0_el1, x0
    adrp    x0, boot_l1_hi
    add     x0, x0, :lo12:boot_l1_hi
    msr     ttbr1_el1, x0
    ldr     x0, =S_MAIR
    msr     mair_el1, x0
    ldr     x0, =S_TCR
    msr     tcr_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)
    orr     x0, x0, #(1 << 2)
    orr     x0, x0, #(1 << 12)
    // EL0 にも Linux と同じく許す: DC ZVA (14、memset)、CTR_EL0 を読む (15)、
    // キャッシュの掃除 DC CVAU / IC IVAU (26、JIT が書いた命令を流すのに使う)
    orr     x0, x0, #(1 << 14)
    orr     x0, x0, #(1 << 15)
    orr     x0, x0, #(1 << 26)
    msr     sctlr_el1, x0
    isb
    ldr     x0, =2f
    br      x0
2:  // ここから上位アドレス
    ldr     x1, =secondary_stacks
    add     x2, x21, #1
    lsl     x2, x2, #14
    add     x1, x1, x2
    mov     sp, x1
    msr     tpidr_el1, x21
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    mov     x0, x21
    bl      secondary_main
3:  wfe
    b       3b
"#
);

unsafe extern "C" {
    fn secondary_entry();
}

#[unsafe(no_mangle)]
extern "C" fn secondary_main(cpu: usize) -> ! {
    crate::trap::init();
    crate::irq::init_cpu();
    crate::timer::init_cpu();
    lock();
    ONLINE.fetch_add(1, Ordering::AcqRel);
    println!("smp: cpu{} online", cpu);
    crate::proc::scheduler()
}

fn psci_cpu_on(mpidr: u64, entry: u64, ctx: u64) -> i64 {
    const CPU_ON: u64 = 0xc400_0003;
    let r: i64;
    unsafe {
        match crate::dtb::psci_method() {
            Some("smc") => core::arch::asm!("smc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
            _ => core::arch::asm!("hvc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
        }
    }
    r
}

/// spin-table: release addr に入口の物理アドレスを書いて起こす。x0 には番号が入らないので、
/// 入口を CPU ごとの小さな踏み台にする
fn spin_table_on(release: u64, cpu: usize) {
    let tramp = TRAMPOLINES[cpu] as *const () as usize;
    let va = p2v(release as usize) as *mut u64;
    unsafe {
        core::ptr::write_volatile(va, v2p(tramp) as u64);
        // MMU を入れる前の CPU が読むので、キャッシュから RAM へ
        core::arch::asm!("dc civac, {}", "dsb sy", "sev", in(reg) va);
    }
}

// spin-table の踏み台: x0 に番号を入れて secondary_entry へ
core::arch::global_asm!(
    r#"
.section .text
.global smp_tramp0
smp_tramp0: mov x0, #0
    b secondary_entry
.global smp_tramp1
smp_tramp1: mov x0, #1
    b secondary_entry
.global smp_tramp2
smp_tramp2: mov x0, #2
    b secondary_entry
.global smp_tramp3
smp_tramp3: mov x0, #3
    b secondary_entry
.global smp_tramp4
smp_tramp4: mov x0, #4
    b secondary_entry
.global smp_tramp5
smp_tramp5: mov x0, #5
    b secondary_entry
.global smp_tramp6
smp_tramp6: mov x0, #6
    b secondary_entry
.global smp_tramp7
smp_tramp7: mov x0, #7
    b secondary_entry
"#
);

unsafe extern "C" {
    fn smp_tramp0();
    fn smp_tramp1();
    fn smp_tramp2();
    fn smp_tramp3();
    fn smp_tramp4();
    fn smp_tramp5();
    fn smp_tramp6();
    fn smp_tramp7();
}

static TRAMPOLINES: [unsafe extern "C" fn(); MAXCPU] = [smp_tramp0, smp_tramp1, smp_tramp2, smp_tramp3, smp_tramp4, smp_tramp5, smp_tramp6, smp_tramp7];

/// DTB の CPU を起こす (cpu0 から、ロックを持って)。maxcpus=N / nosmp で減らせる
pub fn start() {
    let max = if crate::dtb::arg("nosmp").is_some() || crate::dtb::bootargs().is_some_and(|a| a.split_whitespace().any(|w| w == "nosmp")) {
        1
    } else {
        crate::dtb::arg("maxcpus").and_then(|n| n.parse().ok()).unwrap_or(MAXCPU).clamp(1, MAXCPU)
    };
    let me: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) me) };
    let me = me & 0xff_00ff_ffff;
    unsafe { TARGET[0] = (me & 0xff) as usize };
    let mut n = 1;
    let mut started = 0;
    crate::dtb::each_cpu(|mpidr, method, release| {
        if mpidr == me || n >= max {
            return;
        }
        let cpu = n;
        n += 1;
        unsafe { TARGET[cpu] = (mpidr & 0xff) as usize };
        match (method, release) {
            (Some("spin-table"), Some(rel)) => {
                spin_table_on(rel, cpu);
                started += 1;
            }
            _ => {
                let entry = v2p(secondary_entry as *const () as usize) as u64;
                let r = psci_cpu_on(mpidr, entry, cpu as u64);
                if r == 0 {
                    started += 1;
                } else {
                    println!("smp: cannot start cpu {:#x} ({})", mpidr, r);
                }
            }
        }
    });
    if started == 0 {
        return;
    }
    // 起きた CPU はロックを待っているので、いったん放して待つ
    let want = 1 + started;
    unlock();
    for _ in 0..50_000_000 {
        if online() >= want {
            break;
        }
        core::hint::spin_loop();
    }
    lock();
    println!("smp: {} cpus", online());
}
