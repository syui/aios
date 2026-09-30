// 割り込みコントローラ。DTB を見て、どちらかを使う
//   GICv2 (qemu virt、ラズパイ4 の GIC-400)
//   BCM2836 (ラズパイ2/3): コアごとの local intc (タイマなど) + ARM control の IC (周辺機器)
//
// 割り込みの番号はこのカーネルの中では GIC の INTID に合わせる:
//   タイマ (EL1 物理タイマ) = 30、GIC の SPI n = 32 + n
//   BCM2835 の周辺機器 (DTB の <bank irq>) = 64 + bank * 32 + irq
use crate::dtb;
use crate::memlayout::p2v;
use core::ptr::{read_volatile, write_volatile};

pub const SPURIOUS: u32 = 1023;
pub const TIMER: u32 = 30;

// GICv2
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

// BCM2836 local intc (コア 0)
const LOCAL_TIMER_CTL0: usize = 0x40;
const LOCAL_IRQ_SRC0: usize = 0x60;
const SRC_CNTPNS: u32 = 1 << 1;
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
    Bcm { local: usize, arm: usize },
}

static mut CTRL: Ctrl = Ctrl::Gic { d: 0, c: 0 };

fn ctrl() -> Ctrl {
    unsafe { CTRL }
}

fn rd(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}

fn wr(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}

pub fn init() {
    let gic = dtb::reg_of("arm,cortex-a15-gic", 0)
        .map(|d| (d, dtb::reg_of("arm,cortex-a15-gic", 1)))
        .or_else(|| dtb::reg_of("arm,gic-400", 0).map(|d| (d, dtb::reg_of("arm,gic-400", 1))));
    let c = match (gic, dtb::reg_of("brcm,bcm2836-l1-intc", 0), dtb::reg_of("brcm,bcm2836-armctrl-ic", 0)) {
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
        Ctrl::Bcm { .. } => {
            println!("irq: bcm2836 local intc + bcm2835 armctrl");
        }
    }
}

/// DTB の interrupts (その機器のもの) をこのカーネルの番号に
pub fn from_dt(compatible: &str) -> Option<u32> {
    match ctrl() {
        Ctrl::Gic { .. } => {
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
            unsafe { write_volatile((d + GICD_IPRIORITYR + i) as *mut u8, 0) };
            if i >= 32 {
                // SPI は cpu0 に届ける
                unsafe { write_volatile((d + GICD_ITARGETSR + i) as *mut u8, 1) };
            }
            wr(d + GICD_ISENABLER + (i / 32) * 4, 1 << (i % 32));
        }
        Ctrl::Bcm { local, arm } => {
            if id == TIMER {
                wr(local + LOCAL_TIMER_CTL0, rd(local + LOCAL_TIMER_CTL0) | SRC_CNTPNS);
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

/// いま来ている割り込みの番号 (なければ SPURIOUS)
pub fn claim() -> u32 {
    match ctrl() {
        Ctrl::Gic { c, .. } => rd(c + GICC_IAR) & 0x3ff,
        Ctrl::Bcm { local, arm } => {
            let src = rd(local + LOCAL_IRQ_SRC0);
            if src & SRC_CNTPNS != 0 {
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
    if let Ctrl::Gic { c, .. } = ctrl() {
        wr(c + GICC_EOIR, id);
    }
}
