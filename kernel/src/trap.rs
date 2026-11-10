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

// ベクタは 5 とおり (cpuvuln.rs が起動のときに選ぶ: Linux の __bp_harden_el1_vectors と同じく、ユーザー (EL0) から
// 入るところにだけ Spectre-BHB の手当てを、最初の間接分岐より前に): 手当てなし、分岐の履歴を消すループ (回数は
// cpuvuln::BHB_K)、CLRBHB の命令、ファームウェアの SMCCC_ARCH_WORKAROUND_3 (hvc / smc)。x0 と x1 は先に置いてあるので
// 使ってよい。ファームウェアは x0〜x3 を変えるので、x2 と x3 も置いてから呼び、あとでもどす
core::arch::global_asm!(
    r#"
.macro VENTRY kind
    .balign 0x80
    sub     sp, sp, #272
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       trap_common
.endm

.macro VENTRY_BHB kind, how
    .balign 0x80
    sub     sp, sp, #272
    stp     x0, x1, [sp, #0]
    .if \how == 1
    adrp    x0, {bhb_k}
    ldr     w0, [x0, :lo12:{bhb_k}]
1:
    b       . + 4
    subs    w0, w0, #1
    b.ne    1b
    dsb     nsh
    isb
    .elseif \how == 2
    hint    #22
    isb
    .else
    stp     x2, x3, [sp, #16]
    movz    w0, #0x3fff
    movk    w0, #0x8000, lsl #16
    .if \how == 3
    hvc     #0
    .else
    smc     #0
    .endif
    ldp     x2, x3, [sp, #16]
    .endif
    mov     x1, #\kind
    b       trap_common
.endm

.macro VTABLE name, how
.balign 0x800
.global \name
\name:
    VENTRY 0
    VENTRY 1
    VENTRY 2
    VENTRY 3
    VENTRY 4
    VENTRY 5
    VENTRY 6
    VENTRY 7
    .if \how == 0
    VENTRY 8
    VENTRY 9
    VENTRY 10
    VENTRY 11
    VENTRY 12
    VENTRY 13
    VENTRY 14
    VENTRY 15
    .else
    VENTRY_BHB 8, \how
    VENTRY_BHB 9, \how
    VENTRY_BHB 10, \how
    VENTRY_BHB 11, \how
    VENTRY_BHB 12, \how
    VENTRY_BHB 13, \how
    VENTRY_BHB 14, \how
    VENTRY_BHB 15, \how
    .endif
.endm

.section .text
    VTABLE vectors, 0
    VTABLE vectors_bhb_loop, 1
    VTABLE vectors_bhb_insn, 2
    VTABLE vectors_bhb_hvc, 3
    VTABLE vectors_bhb_smc, 4

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
"#,
    bhb_k = sym crate::cpuvuln::BHB_K,
);

// ベクタの並び: [EL1t, EL1h, EL0 64bit, EL0 32bit] x [sync, irq, fiq, serror]
const EL1H_SYNC: u64 = 4;
const EL1H_IRQ: u64 = 5;
pub const EL0_SYNC: u64 = 8;
const EL0_IRQ: u64 = 9;
const EL0_FIQ: u64 = 10;

const EC_UNKNOWN: u64 = 0x00;
const EC_SYS64: u64 = 0x18;
pub const EC_SVC64: u64 = 0x15;
const EC_IABT_LOW: u64 = 0x20;
const EC_DABT_LOW: u64 = 0x24;
const EC_BRK64: u64 = 0x3c;
const EC_PC_ALIGN: u64 = 0x22;
const EC_SP_ALIGN: u64 = 0x26;
const EC_FP: u64 = 0x2c;
const EC_BREAKPT_LOW: u64 = 0x30;
const EC_SOFTSTP_LOW: u64 = 0x32;

/// この CPU のベクタ (VBAR_EL1) を置く。Spectre-BHB の手当てのあるものは cpuvuln.rs が起動のときに選ぶ (選んだあとに
/// もう 1 度呼ぶ。ほかの CPU は起きるときに)
pub fn init() {
    unsafe extern "C" {
        static vectors: u8;
        static vectors_bhb_loop: u8;
        static vectors_bhb_insn: u8;
        static vectors_bhb_hvc: u8;
        static vectors_bhb_smc: u8;
    }
    let v = match crate::cpuvuln::bhb_how() {
        crate::cpuvuln::BHB_LOOP => &raw const vectors_bhb_loop,
        crate::cpuvuln::BHB_INSN => &raw const vectors_bhb_insn,
        crate::cpuvuln::BHB_FW_HVC => &raw const vectors_bhb_hvc,
        crate::cpuvuln::BHB_FW_SMC => &raw const vectors_bhb_smc,
        _ => &raw const vectors,
    };
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) v,
        );
    }
}

/// 例外の理由。1 つの例外で 1 度だけ読んで渡す (QEMU の TCG では ESR_EL1 / FAR_EL1 を読むたびに、HCR_EL2 の
/// TRVM を確かめる関数を呼ぶ: 1 回 20 ns ほど。システムコールで 6 回読んでいた)
pub fn esr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) v) };
    v
}

/// フォルトした番地 (データと命令のフォルトのときだけ読む)
fn far() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, far_el1", out(reg) v) };
    v
}

pub fn intr_on() {
    unsafe { core::arch::asm!("msr daifclr, #2") };
}

pub fn intr_off() {
    unsafe { core::arch::asm!("msr daifset, #2") };
}

/// 例外の入口: 大きなロックを取ってから処理し、戻る前に放す (smp.rs)。
/// EL1 の割り込みはスケジューラが割り込みを待っているとき (ロックを放している) だけ来る
#[unsafe(no_mangle)]
pub extern "C" fn trap_handler(tf: &mut TrapFrame, kind: u64) {
    if kind == EL1H_SYNC {
        return handle(tf, kind, None, esr());
    }
    let esr = if kind == EL0_SYNC { esr() } else { 0 };
    // Spectre v2 のファームウェアの手当て (cpuvuln.rs): ユーザーからカーネルのアドレス (ビット 55) で落ちたとき、
    // 割りこみを開ける前に分岐予測をきれいにする (Linux の el0_ia、el0_pc、el0_breakpt、el0_softstp、el0_interrupt)
    if (kind == EL0_SYNC || kind == EL0_IRQ || kind == EL0_FIQ) && crate::cpuvuln::bp_fw_on() {
        let kernel = |a: u64| a & (1 << 55) != 0;
        let hit = match (kind, esr >> 26) {
            (EL0_SYNC, EC_IABT_LOW) => kernel(far()),
            (EL0_SYNC, EC_PC_ALIGN | EC_BREAKPT_LOW | EC_SOFTSTP_LOW) => kernel(tf.elr),
            (EL0_SYNC, _) => false,
            _ => kernel(tf.elr),
        };
        if hit {
            crate::cpuvuln::bp_harden();
        }
    }
    // 自分のことを読むだけのシステムコールと、自分だけの無名の領域のページフォルトはロックなしで (smp.rs)
    if kind == EL0_SYNC {
        let ec = esr >> 26;
        if ec == EC_SVC64 && syscall::fast(tf) {
            crate::smp::fs_check_released();
            return;
        }
        // データと命令のフォルト (変換、アクセスフラグ、権限)。WnR は、キャッシュを整える命令 (CM) なら読んだことに (handle と同じ)
        if (ec == EC_DABT_LOW || ec == EC_IABT_LOW) && (4..=15).contains(&(esr & 0x3f)) {
            let write = ec == EC_DABT_LOW && esr & (1 << 6) != 0 && esr & (1 << 8) == 0;
            if proc::current().pt().fast_fault(far() as usize, write, ec == EC_IABT_LOW) {
                crate::smp::FAST_FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                crate::smp::FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }
    // コンソールの送信の割り込み (輪から次の文字を出すだけ) もロックなしで
    let mut claimed = None;
    if kind == EL1H_IRQ || kind == EL0_IRQ {
        let raw = irq::claim();
        if raw & 0x3ff == crate::uart::irq() && crate::uart::intr_fast() {
            irq::complete(raw);
            return;
        }
        claimed = Some(raw);
    }
    // ここからカーネルの時間 (proc の sys_ns)。速い道は上でもどっている
    let from_user = kind == EL0_SYNC || kind == EL0_IRQ;
    if from_user {
        proc::kernel_enter();
    }
    let t0 = crate::smp::lock();
    // ロックを持っていた時間を、原因ごとに数える (/proc/bkl)。svc の番号は handle が x0 を書く前に。
    // 時計を読むのは、測る回 (smp::SAMPLE のくじ) の取ったときと終わりの 2 回だけ (取った時刻と放す時刻にも使う)
    let cause = match kind {
        EL0_SYNC if esr >> 26 == EC_SVC64 => crate::smp::Cause::sys(tf.x[8], tf.x[0]),
        EL0_SYNC => crate::smp::Cause::Fault,
        _ => crate::smp::Cause::Irq,
    };
    crate::smp::seg_begin(&cause, t0);
    let sw = crate::smp::SWITCHES.load(core::sync::atomic::Ordering::Relaxed);
    // ロックなしの munmap が外したファイルの写しの Rc を捨てる (割り込みではなく、プロセスの中で)
    if kind == EL0_SYNC {
        crate::vm::drop_unmapped();
    }
    handle(tf, kind, claimed, esr);
    crate::proc::drop_prefork();
    crate::smp::fs_check_released();
    let slept = crate::smp::SWITCHES.load(core::sync::atomic::Ordering::Relaxed) != sw;
    let t1 = crate::smp::timed_now();
    crate::smp::account(cause, t1.saturating_sub(t0), slept, t1);
    crate::smp::unlock_at(t1);
    crate::vm::free_deferred();
    if from_user {
        proc::kernel_leave();
    }
}

/// EL0 の MRS で ID レジスタ (op0 = 3, op1 = 0, CRn = 0) を読んだものに、Linux と同じく答える
/// (Linux はいつも答えるので AT_HWCAP の CPUID を見ずに読むものがある: zig の CPU しらべ)。
/// MIDR はそのまま、MPIDR は Linux と同じ 1 << 31、ほかの ID レジスタはカーネルが使わせているところだけ (SVE、SME、MTE、
/// ポインタ認証などは 0)、名前のないものは 0。AT_HWCAP と AT_HWCAP2 (exec.rs の hwcaps) と同じものを見せる。答えたら次の命令へ
fn emulate_mrs(tf: &mut TrapFrame) -> bool {
    let mut w = [0u8; 4];
    if proc::current().pt().copy_in(&mut w, tf.elr as usize).is_none() {
        return false;
    }
    let insn = u32::from_le_bytes(w);
    // MRS (L = 1)、op0 = 3
    if insn & 0xfff8_0000 != 0xd538_0000 {
        return false;
    }
    let (op1, crn, crm, op2, rt) = ((insn >> 16) & 7, (insn >> 12) & 0xf, (insn >> 8) & 0xf, (insn >> 5) & 7, (insn & 0x1f) as usize);
    if op1 != 0 || crn != 0 {
        return false;
    }
    macro_rules! rd {
        ($r:literal) => {{
            let v: u64;
            unsafe { core::arch::asm!(concat!("mrs {}, ", $r), out(reg) v) };
            v
        }};
    }
    // 4 ビットの欄のうち、shift のものだけ残す
    let keep = |v: u64, shifts: &[u32]| shifts.iter().fold(0, |a, &s| a | (v & (0xf << s)));
    let v = match (crm, op2) {
        (0, 0) => rd!("midr_el1"),
        (0, 5) => 1 << 31,
        // ID_AA64PFR0: EL2 と EL3、FP、AdvSIMD、DIT。EL0 と EL1 は AArch64 だけ (1): aios は AArch32 のプログラムを動かさない
        // (Linux の COMPAT のないものと同じ)
        (4, 0) => keep(rd!("id_aa64pfr0_el1"), &[8, 12, 16, 20, 48]) | 0x11,
        // ID_AA64PFR1: SSBS だけ (Linux も BT と MTE は使わせているときだけ)
        (4, 1) => keep(rd!("id_aa64pfr1_el1"), &[4]),
        (6, 0) => rd!("id_aa64isar0_el1"),
        // ID_AA64ISAR1: ポインタ認証 (APA API GPA GPI) を外す
        (6, 1) => rd!("id_aa64isar1_el1") & !(0xff << 4 | 0xff << 24),
        // ID_AA64ISAR2: RPRES、BC、RPRFM、CSSC だけ (WFxT と MOPS とポインタ認証は外す)
        (6, 2) => keep(rd!("s3_0_c0_c6_2"), &[4, 20, 48, 52]),
        (7, 0) => rd!("id_aa64mmfr0_el1"),
        (7, 1) => rd!("id_aa64mmfr1_el1"),
        (7, 2) => rd!("id_aa64mmfr2_el1"),
        _ => 0,
    };
    if rt != 31 {
        tf.x[rt] = v;
    }
    tf.elr += 4;
    true
}

/// esr は trap_handler が読んだもの (EL0 と EL1 の同期例外のとき)
fn handle(tf: &mut TrapFrame, kind: u64, claimed: Option<u32>, esr: u64) {
    match kind {
        EL1H_SYNC => {
            let far = far();
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
            // 空きが少なければ、使っていないページをスワップへ
            crate::vm::balance();
            match esr >> 26 {
                EC_SVC64 => {
                    let intr = syscall::dispatch(tf);
                    // 書き出しが追いつかないほど書いた: 書き出しのスレッドを待つ
                    if proc::current().dirtied {
                        crate::vfs::balance_dirty();
                    }
                    // 起こしたものにゆずる (自分で起こしたとき)
                    if proc::take_resched() {
                        proc::yield_now();
                    }
                    proc::check_killed();
                    signal::deliver(tf, intr);
                }
                ec => {
                    let far = far();
                    // ページがまだない / 共有している (コピーオンライト): 用意して、同じ命令からやりなおす
                    // DFSC/IFSC: 0b0001xx 変換、0b0010xx アクセスフラグ、0b0011xx 権限
                    let fsc = esr & 0x3f;
                    let mut fault = None;
                    if (ec == EC_IABT_LOW || ec == EC_DABT_LOW) && (4..=15).contains(&fsc) {
                        // WnR (書こうとした)。ただしキャッシュを整える命令 (DC CVAU など。CM = ISS の bit 8) は、
                        // 読むだけでも WnR = 1 で来る。JIT (LLVM の __clear_cache) が実行だけの領域のまだないページに
                        // DC CVAU すると、書いたと見て SIGSEGV にしてしまうので、読んだものとして扱う
                        let cm = ec == EC_DABT_LOW && esr & (1 << 8) != 0;
                        let write = ec == EC_DABT_LOW && esr & (1 << 6) != 0 && !cm;
                        crate::smp::FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        let mut r = proc::current().pt().fault(far as usize, write, ec == EC_IABT_LOW);
                        // 足りなければ自分のページもスワップへ追い出して、もう一度
                        if matches!(r, Err(crate::vm::FaultErr::NoMem)) && crate::vm::reclaim(crate::vm::RECLAIM_BATCH, 0) > 0 {
                            r = proc::current().pt().fault(far as usize, write, ec == EC_IABT_LOW);
                        }
                        // それでもなければ、いちばん大きいものを殺して空ける (oom::on_fault)。殺したのが自分でなければ、
                        // それが終わるのを少し待って、同じ命令からやりなおす (init も殺さずに待つ)
                        if matches!(r, Err(crate::vm::FaultErr::NoMem)) && crate::oom::on_fault() {
                            let _ = proc::sleep_until(0, crate::timer::ticks_after(10_000_000));
                            proc::check_killed();
                            signal::deliver(tf, None);
                            return;
                        }
                        match r {
                            Ok(()) => {
                                proc::check_killed();
                                signal::deliver(tf, None);
                                return;
                            }
                            Err(e) => fault = Some(e),
                        }
                    }
                    // EL0 から ID レジスタを読んだ (MRS): Linux と同じく見せてよい値で答える (zig cc は CPU を調べるのに読む)
                    if (ec == EC_UNKNOWN || ec == EC_SYS64) && emulate_mrs(tf) {
                        proc::check_killed();
                        signal::deliver(tf, None);
                        return;
                    }
                    // 例外はシグナルにして、今のスレッドに必ず届ける
                    let (sig, code, addr) = match ec {
                        EC_IABT_LOW | EC_DABT_LOW => match fault {
                            Some(crate::vm::FaultErr::NoMem) => {
                                println!("pid {}: out of memory at {:#x}", proc::current().pid, far);
                                (signal::SIGKILL, 0, far)
                            }
                            Some(crate::vm::FaultErr::Access) => (signal::SIGSEGV, signal::SEGV_ACCERR, far),
                            // 写したファイルの終わりより先 (BUS_ADRERR)
                            Some(crate::vm::FaultErr::Bus) => (signal::SIGBUS, 2, far),
                            _ => (signal::SIGSEGV, signal::SEGV_MAPERR, far),
                        },
                        EC_PC_ALIGN | EC_SP_ALIGN => (signal::SIGBUS, 1, far),
                        EC_BRK64 => (signal::SIGTRAP, 1, tf.elr),
                        EC_FP => (signal::SIGFPE, 0, tf.elr),
                        _ => (signal::SIGILL, 1, tf.elr),
                    };
                    proc::current().last_fault = (tf.elr, far, ec);
                    proc::current().last_esr = esr;
                    proc::current().last_lr = tf.x[30];
                    proc::current().last_regs = [tf.x[0], tf.x[1], tf.x[19]];
                    proc::current().last_sp = tf.sp_el0;
                    // SIGILL を自分で受けるもの (OpenSSL が CPU の命令を試すときなど) は、ふつうのことなので書かない
                    if sig == signal::SIGILL && signal::action(proc::current(), sig).handler == signal::SIG_DFL {
                        // 何の命令で止まったか (調べもの用)
                        let mut w = [0u8; 4];
                        let insn = proc::current().pt().copy_in(&mut w, tf.elr as usize).map(|_| u32::from_le_bytes(w));
                        let p = proc::current();
                        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
                        println!(
                            "pid {} ({}): SIGILL ec {:#x} esr {:#x} pc {:#x} insn {:x?}",
                            p.pid,
                            core::str::from_utf8(&p.comm[..n]).unwrap_or("?"),
                            ec,
                            esr,
                            tf.elr,
                            insn
                        );
                    }
                    signal::force(sig, signal::SigInfo { code, addr, ..signal::SigInfo::ZERO });
                    proc::check_killed();
                    signal::deliver(tf, None);
                }
            }
        }
        EL1H_IRQ | EL0_IRQ => {
            let raw = claimed.unwrap_or_else(irq::claim);
            let id = raw & 0x3ff;
            crate::debug::trace_irq(id);
            match id {
                timer::IRQ => timer::tick(),
                // ほかの CPU から: EL0 へ戻る前に、殺されたか、シグナルが来たかを見るだけ
                irq::IPI => {}
                id if id == crate::uart::irq() => crate::uart::intr(),
                irq::SPURIOUS => return,
                // 装置のもの (net、input、sound、virtio-blk ...: それぞれが起こすときに irq::request で置いた)
                id if irq::handle(id) => {}
                _ => println!("irq: unexpected {}", id),
            }
            irq::complete(raw);
            // EL0 で走っていて、タイムスライスを使いきったなら順番をゆずる
            if kind == EL0_IRQ {
                // 起こされたものにゆずる印がついていても (ほかの CPU からの IPI か、この CPU の割り込みで起こした)
                if (id == timer::IRQ && proc::slice_expired()) | proc::take_resched() {
                    proc::yield_now();
                }
                proc::check_killed();
                signal::deliver(tf, None);
            }
        }
        _ => panic!("unexpected exception kind {} elr={:#x}", kind, tf.elr),
    }
}
