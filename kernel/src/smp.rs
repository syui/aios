// 複数 CPU
//
// カーネルは大きなロック (BKL) 1 つで守る: カーネルの中を走れるのはいつも 1 つの CPU だけで、
// ユーザーのプログラムはすべての CPU で同時に動く (Linux 2.0 のころと同じ)。
//   EL0 から例外で入ったらロックを取り、EL0 へ戻る直前に放す (trap.rs、forkret)。
//   スケジューラはロックを持ったまま回し、することがなければ放して割り込みを待つ。
// これでいままでの 1 CPU 向けのコード (static mut、RefCell) はそのまま使える。
//
// 2 つめからの CPU は DTB の cpu ノードの enable-method で起こす:
//   psci        PSCI の CPU_ON (QEMU virt、UEFI)
//   spin-table  cpu-release-addr に入口を書いて sev (ラズパイのファームウェア)
// 起きた CPU は boot.rs と同じページ表で MMU を入れ、自分のスタックでスケジューラに入る。
// TPIDR_EL1 に CPU の番号を入れておく。
use crate::memlayout::{p2v, v2p};
use core::sync::atomic::{AtomicUsize, Ordering};

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
    while BKL.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}

pub fn unlock() {
    BKL.store(0, Ordering::Release);
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
