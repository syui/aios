// 割り込みコントローラ。DTB を見て、どれかを使う
//   GICv2 (qemu virt、ラズパイ4 の GIC-400)
//   GICv3 (qemu virt,gic-version=3。Mac の Hypervisor (HVF) で動かすときはこちら):
//         distributor と CPU ごとの redistributor はメモリ、CPU インターフェースはシステムレジスタ (ICC_*)
//   BCM2836 (ラズパイ2/3): コアごとの local intc (タイマなど) + ARM control の IC (周辺機器)
//
// 割り込みの番号はこのカーネルの中では GIC の INTID に合わせる:
//   タイマ (EL1 仮想タイマ) = 27、GIC の SPI n = 32 + n
//   BCM2835 の周辺機器 (DTB の <bank irq>) = 64 + bank * 32 + irq
// タイマは CPU ごと (GIC の PPI は CPU ごとにある。BCM2836 はコアごとのレジスタ)。
// 機器の割り込みはすべて cpu0 に届ける。
use crate::dtb;
use crate::memlayout::p2v;
use crate::mmio;

pub const SPURIOUS: u32 = 1023;
pub const TIMER: u32 = 27;
/// CPU から CPU への割り込み (GIC の SGI 0、BCM2836 のメールボックス 0)
pub const IPI: u32 = 0;

// GICv2
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_SGIR: usize = 0xf00;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

// BCM2836 local intc (コア n は + 4 * n)
const LOCAL_TIMER_CTL0: usize = 0x40;
const LOCAL_MBOX_CTL0: usize = 0x50;
const LOCAL_IRQ_SRC0: usize = 0x60;
const LOCAL_MBOX0_SET: usize = 0x80;
const LOCAL_MBOX0_CLR: usize = 0xc0;
const SRC_MBOX0: u32 = 1 << 4;

/// BCM2836 のコアの番号 (MPIDR の Aff0)
fn core_no() -> usize {
    let m: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) m) };
    (m & 0xff) as usize
}
/// コアのタイマの割り込みの元: 仮想タイマ (CNTVIRQ)
const SRC_CNTV: u32 = 1 << 3;
const SRC_GPU: u32 = 1 << 8;
// BCM2835 ARM control の IC
const PENDING_1: usize = 0x04;
const PENDING_2: usize = 0x08;
const ENABLE_1: usize = 0x10;
const ENABLE_2: usize = 0x14;
const ENABLE_BASIC: usize = 0x18;

#[derive(Clone, Copy)]
enum Ctrl {
    Gic { d: usize, c: usize },
    /// GICv3: distributor と redistributor の並び
    Gic3 { d: usize, r: usize },
    Bcm { local: usize, arm: usize },
}

static mut CTRL: Ctrl = Ctrl::Gic { d: 0, c: 0 };

fn ctrl() -> Ctrl {
    unsafe { CTRL }
}

fn rd(a: usize) -> u32 {
    mmio::r32(a)
}

fn wr(a: usize, v: u32) {
    mmio::w32(a, v)
}

// ---- GICv3 ----

const GICD_IGROUPR: usize = 0x080;
const GICD_IROUTER: usize = 0x6000;
const GICR_TYPER: usize = 0x08;
const GICR_WAKER: usize = 0x14;
/// redistributor の SGI と PPI の口 (RD_base + 64 KiB)
const GICR_SGI: usize = 0x10000;
const GICR_FRAME: usize = 0x20000;

/// この CPU の redistributor (GICR_TYPER の affinity が MPIDR と同じもの)
fn my_redist(r: usize) -> usize {
    let m: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) m) };
    let aff = (m & 0xff_ffff) | ((m >> 32) & 0xff) << 24;
    let mut f = r;
    for _ in 0..64 {
        let t = mmio::r64(f + GICR_TYPER);
        if (t >> 32) == aff {
            return f;
        }
        if t & (1 << 4) != 0 {
            break; // 最後
        }
        f += GICR_FRAME;
    }
    r
}

/// この CPU の GICv3: redistributor を起こし、CPU インターフェース (ICC_*) を使えるように
fn gic3_cpu(r: usize) {
    let rd_ = my_redist(r);
    // ProcessorSleep を落として、ChildrenAsleep が消えるのを待つ
    wr(rd_ + GICR_WAKER, rd(rd_ + GICR_WAKER) & !(1 << 1));
    while rd(rd_ + GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }
    // SGI と PPI はみんな group 1
    wr(rd_ + GICR_SGI + GICD_IGROUPR, !0);
    unsafe {
        core::arch::asm!(
            "msr S3_0_C12_C12_5, {sre}", // ICC_SRE_EL1: システムレジスタで
            "isb",
            "msr S3_0_C4_C6_0, {pmr}",   // ICC_PMR_EL1: すべての優先度を通す
            "msr S3_0_C12_C12_7, {one}", // ICC_IGRPEN1_EL1: group 1 を受ける
            "isb",
            sre = in(reg) 0x7u64,
            pmr = in(reg) 0xffu64,
            one = in(reg) 1u64,
        );
    }
}

/// 2 つめからの CPU: GIC の CPU インターフェース (タイマは timer::init_cpu が有効にする)
pub fn init_cpu() {
    match ctrl() {
        Ctrl::Gic { c, .. } => {
            wr(c + GICC_PMR, 0xff);
            wr(c + GICC_CTLR, 1);
        }
        Ctrl::Gic3 { r, .. } => gic3_cpu(r),
        Ctrl::Bcm { .. } => {}
    }
    enable_ipi();
}

/// この CPU で CPU 間の割り込みを受ける
pub fn enable_ipi() {
    match ctrl() {
        Ctrl::Gic { .. } | Ctrl::Gic3 { .. } => enable(IPI),
        Ctrl::Bcm { local, .. } => {
            let r = local + LOCAL_MBOX_CTL0 + 4 * core_no();
            wr(r, rd(r) | 1);
        }
    }
}

/// CPU (GIC の CPU インターフェースの番号 / BCM2836 のコアの番号) に割り込みを送る
pub fn send_ipi(target: usize) {
    unsafe { core::arch::asm!("dsb ishst") };
    match ctrl() {
        Ctrl::Gic { d, .. } => wr(d + GICD_SGIR, (1 << (16 + target)) | IPI),
        // ICC_SGI1R_EL1: INTID と、宛先 (Aff1 と Aff0 の並び)
        Ctrl::Gic3 { .. } => {
            let v = ((IPI as u64) << 24) | (((target >> 4) as u64 & 0xff) << 16) | (1u64 << (target & 0xf));
            unsafe { core::arch::asm!("msr S3_0_C12_C11_5, {}", "isb", in(reg) v) };
        }
        Ctrl::Bcm { local, .. } => wr(local + LOCAL_MBOX0_SET + 0x10 * target, 1),
    }
}

pub fn init() {
    let gic = dtb::reg_of("arm,cortex-a15-gic", 0)
        .map(|d| (d, dtb::reg_of("arm,cortex-a15-gic", 1)))
        .or_else(|| dtb::reg_of("arm,gic-400", 0).map(|d| (d, dtb::reg_of("arm,gic-400", 1))));
    let gic3 = dtb::reg_of("arm,gic-v3", 0).zip(dtb::reg_of("arm,gic-v3", 1));
    let c = match (gic, dtb::reg_of("brcm,bcm2836-l1-intc", 0), dtb::reg_of("brcm,bcm2836-armctrl-ic", 0)) {
        _ if gic3.is_some() => {
            let ((d, _), (r, _)) = gic3.unwrap();
            Ctrl::Gic3 { d: p2v(d as usize), r: p2v(r as usize) }
        }
        (Some(((d, _), Some((c, _)))), _, _) => Ctrl::Gic { d: p2v(d as usize), c: p2v(c as usize) },
        (_, Some((l, _)), Some((a, _))) => Ctrl::Bcm { local: p2v(l as usize), arm: p2v(a as usize) },
        // DTB がなければ qemu virt
        _ => Ctrl::Gic { d: p2v(0x0800_0000), c: p2v(0x0801_0000) },
    };
    unsafe { CTRL = c };
    match c {
        Ctrl::Gic { d, c } => {
            wr(d + GICD_CTLR, 1);
            wr(c + GICC_PMR, 0xff);
            wr(c + GICC_CTLR, 1);
        }
        Ctrl::Gic3 { d, r } => {
            // ARE (affinity で宛先を決める) と group 1 / 0 を有効に
            wr(d + GICD_CTLR, (1 << 4) | (1 << 1) | 1);
            while rd(d + GICD_CTLR) & (1 << 31) != 0 {
                core::hint::spin_loop();
            }
            gic3_cpu(r);
            println!("irq: gicv3");
        }
        Ctrl::Bcm { .. } => {
            println!("irq: bcm2836 local intc + bcm2835 armctrl");
        }
    }
}

/// DTB の interrupts (その機器のもの) をこのカーネルの番号に
pub fn from_dt(compatible: &str) -> Option<u32> {
    match ctrl() {
        Ctrl::Gic { .. } | Ctrl::Gic3 { .. } => {
            // <種類 番号 flags>: 種類 0 = SPI, 1 = PPI
            let kind = dtb::irq_cell(compatible, 0)?;
            let n = dtb::irq_cell(compatible, 1)?;
            Some(if kind == 1 { 16 + n } else { 32 + n })
        }
        Ctrl::Bcm { .. } => {
            let bank = dtb::irq_cell(compatible, 0)?;
            let n = dtb::irq_cell(compatible, 1)?;
            Some(64 + bank * 32 + n)
        }
    }
}

pub fn enable(id: u32) {
    match ctrl() {
        Ctrl::Gic { d, .. } => {
            let i = id as usize;
            mmio::w8(d + GICD_IPRIORITYR + i, 0);
            if i >= 32 {
                // SPI は cpu0 に届ける
                mmio::w8(d + GICD_ITARGETSR + i, 1);
            }
            wr(d + GICD_ISENABLER + (i / 32) * 4, 1 << (i % 32));
        }
        Ctrl::Gic3 { d, r } => {
            let i = id as usize;
            if i < 32 {
                // SGI と PPI はこの CPU の redistributor で
                let s = my_redist(r) + GICR_SGI;
                mmio::w8(s + GICD_IPRIORITYR + i, 0);
                wr(s + GICD_ISENABLER, 1 << i);
            } else {
                // SPI は group 1、cpu0 (affinity 0) へ
                wr(d + GICD_IGROUPR + (i / 32) * 4, rd(d + GICD_IGROUPR + (i / 32) * 4) | 1 << (i % 32));
                mmio::w8(d + GICD_IPRIORITYR + i, 0);
                mmio::w64(d + GICD_IROUTER + i * 8, 0);
                wr(d + GICD_ISENABLER + (i / 32) * 4, 1 << (i % 32));
            }
        }
        Ctrl::Bcm { local, arm } => {
            if id == TIMER {
                let r = local + LOCAL_TIMER_CTL0 + 4 * core_no();
                wr(r, rd(r) | SRC_CNTV);
            } else if id >= 64 {
                let (bank, n) = ((id - 64) / 32, (id - 64) % 32);
                let reg = match bank {
                    0 => ENABLE_BASIC,
                    1 => ENABLE_1,
                    _ => ENABLE_2,
                };
                wr(arm + reg, 1 << n);
            }
        }
    }
}

/// いま来ている割り込み。番号は & 0x3ff (なければ SPURIOUS)。complete にはこの値のまま渡す
/// (GIC の SGI は送り元の CPU の番号も入っていて、EOIR にもそれが要る)
pub fn claim() -> u32 {
    match ctrl() {
        Ctrl::Gic { c, .. } => rd(c + GICC_IAR),
        Ctrl::Gic3 { .. } => {
            let v: u64;
            unsafe { core::arch::asm!("mrs {}, S3_0_C12_C12_0", out(reg) v) }; // ICC_IAR1_EL1
            v as u32
        }
        Ctrl::Bcm { local, arm } => {
            let core = core_no();
            let src = rd(local + LOCAL_IRQ_SRC0 + 4 * core);
            if src & SRC_MBOX0 != 0 {
                wr(local + LOCAL_MBOX0_CLR + 0x10 * core, !0);
                return IPI;
            }
            if src & SRC_CNTV != 0 {
                return TIMER;
            }
            if src & SRC_GPU != 0 {
                let p1 = rd(arm + PENDING_1) & rd(arm + ENABLE_1);
                if p1 != 0 {
                    return 64 + 32 + p1.trailing_zeros();
                }
                let p2 = rd(arm + PENDING_2) & rd(arm + ENABLE_2);
                if p2 != 0 {
                    return 64 + 64 + p2.trailing_zeros();
                }
            }
            SPURIOUS
        }
    }
}

pub fn complete(id: u32) {
    match ctrl() {
        Ctrl::Gic { c, .. } => wr(c + GICC_EOIR, id),
        Ctrl::Gic3 { .. } => unsafe { core::arch::asm!("msr S3_0_C12_C12_1, {}", "isb", in(reg) id as u64) }, // ICC_EOIR1_EL1
        Ctrl::Bcm { .. } => {}
    }
}
