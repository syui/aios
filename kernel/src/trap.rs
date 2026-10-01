// 例外ベクタ (VBAR_EL1) とトラップ処理
use crate::{irq, proc, signal, syscall, timer};

#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub sp_el0: u64,
}

impl TrapFrame {
    pub const fn zeroed() -> Self {
        Self { x: [0; 31], elr: 0, spsr: 0, sp_el0: 0 }
    }
}

core::arch::global_asm!(
    r#"
.macro VENTRY kind
    .balign 0x80
    sub     sp, sp, #272
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       trap_common
.endm

.section .text
.balign 0x800
.global vectors
vectors:
    VENTRY 0
    VENTRY 1
    VENTRY 2
    VENTRY 3
    VENTRY 4
    VENTRY 5
    VENTRY 6
    VENTRY 7
    VENTRY 8
    VENTRY 9
    VENTRY 10
    VENTRY 11
    VENTRY 12
    VENTRY 13
    VENTRY 14
    VENTRY 15

trap_common:
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    mrs     x2, elr_el1
    stp     x30, x2, [sp, #240]
    mrs     x2, spsr_el1
    mrs     x3, sp_el0
    stp     x2, x3, [sp, #256]

    mov     x0, sp
    bl      trap_handler

.global trap_ret
trap_ret:
    ldp     x2, x3, [sp, #256]
    msr     spsr_el1, x2
    msr     sp_el0, x3
    ldp     x30, x2, [sp, #240]
    msr     elr_el1, x2
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldp     x0, x1, [sp, #0]
    add     sp, sp, #272
    eret
"#
);

// ベクタの並び: [EL1t, EL1h, EL0 64bit, EL0 32bit] x [sync, irq, fiq, serror]
const EL1H_SYNC: u64 = 4;
const EL1H_IRQ: u64 = 5;
const EL0_SYNC: u64 = 8;
const EL0_IRQ: u64 = 9;

const EC_SVC64: u64 = 0x15;
const EC_IABT_LOW: u64 = 0x20;
const EC_DABT_LOW: u64 = 0x24;
const EC_BRK64: u64 = 0x3c;
const EC_PC_ALIGN: u64 = 0x22;
const EC_SP_ALIGN: u64 = 0x26;
const EC_FP: u64 = 0x2c;

pub fn init() {
    unsafe extern "C" {
        static vectors: u8;
    }
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) &raw const vectors,
        );
    }
}

fn esr_far() -> (u64, u64) {
    let esr: u64;
    let far: u64;
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr);
        core::arch::asm!("mrs {}, far_el1", out(reg) far);
    }
    (esr, far)
}

pub fn intr_on() {
    unsafe { core::arch::asm!("msr daifclr, #2") };
}

pub fn intr_off() {
    unsafe { core::arch::asm!("msr daifset, #2") };
}

#[unsafe(no_mangle)]
extern "C" fn trap_handler(tf: &mut TrapFrame, kind: u64) {
    match kind {
        EL1H_SYNC => {
            let (esr, far) = esr_far();
            let ec = esr >> 26;
            if ec == EC_BRK64 {
                println!("trap: brk #{} at {:#x}", esr & 0xffff, tf.elr);
                tf.elr += 4;
                return;
            }
            panic!(
                "kernel sync exception: esr={:#x} (ec={:#x}) elr={:#x} far={:#x}",
                esr, ec, tf.elr, far
            );
        }
        EL0_SYNC => {
            let (esr, far) = esr_far();
            match esr >> 26 {
                EC_SVC64 => {
                    let intr = syscall::dispatch(tf);
                    proc::check_killed();
                    signal::deliver(tf, intr);
                }
                ec => {
                    // ページがまだない / 共有している (コピーオンライト): 用意して、同じ命令からやりなおす
                    // DFSC/IFSC: 0b0001xx 変換、0b0010xx アクセスフラグ、0b0011xx 権限
                    let fsc = esr & 0x3f;
                    let mut fault = None;
                    if (ec == EC_IABT_LOW || ec == EC_DABT_LOW) && (4..=15).contains(&fsc) {
                        let write = ec == EC_DABT_LOW && esr & (1 << 6) != 0;
                        match proc::current().pt().fault(far as usize, write, ec == EC_IABT_LOW) {
                            Ok(()) => {
                                proc::check_killed();
                                signal::deliver(tf, None);
                                return;
                            }
                            Err(e) => fault = Some(e),
                        }
                    }
                    // 例外はシグナルにして、今のスレッドに必ず届ける
                    let (sig, code, addr) = match ec {
                        EC_IABT_LOW | EC_DABT_LOW => match fault {
                            Some(crate::vm::FaultErr::NoMem) => {
                                println!("pid {}: out of memory at {:#x}", proc::current().pid, far);
                                (signal::SIGKILL, 0, far)
                            }
                            Some(crate::vm::FaultErr::Access) => (signal::SIGSEGV, signal::SEGV_ACCERR, far),
                            _ => (signal::SIGSEGV, signal::SEGV_MAPERR, far),
                        },
                        EC_PC_ALIGN | EC_SP_ALIGN => (signal::SIGBUS, 1, far),
                        EC_BRK64 => (signal::SIGTRAP, 1, tf.elr),
                        EC_FP => (signal::SIGFPE, 0, tf.elr),
                        _ => (signal::SIGILL, 1, tf.elr),
                    };
                    let _ = esr;
                    signal::force(sig, signal::SigInfo { code, addr, ..signal::SigInfo::ZERO });
                    proc::check_killed();
                    signal::deliver(tf, None);
                }
            }
        }
        EL1H_IRQ | EL0_IRQ => {
            let id = irq::claim();
            match id {
                timer::IRQ => timer::tick(),
                id if id == crate::uart::irq() => crate::uart::intr(),
                irq::SPURIOUS => return,
                id if Some(id) == crate::net::irq() => crate::net::intr(),
                _ => println!("irq: unexpected {}", id),
            }
            irq::complete(id);
            // EL0 で走っていたなら順番をゆずる
            if kind == EL0_IRQ {
                if id == timer::IRQ {
                    proc::yield_now();
                }
                proc::check_killed();
                signal::deliver(tf, None);
            }
        }
        _ => panic!("unexpected exception kind {} elr={:#x}", kind, tf.elr),
    }
}
