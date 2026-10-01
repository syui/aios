// PL011 UART。場所と割り込みは DTB から (なければ qemu virt の PA 0x0900_0000, SPI 1)
use crate::spinlock::SpinLock;
use core::fmt;
use crate::mmio;

const DR: usize = 0x00;
const FR: usize = 0x18;
const IMSC: usize = 0x38;
const ICR: usize = 0x44;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const INT_RX: u32 = 1 << 4;
const INT_RT: u32 = 1 << 6;

static mut BASE: usize = 0;
static mut IRQ: u32 = 33;

fn reg(off: usize) -> usize {
    unsafe { BASE + off }
}

/// 何よりも先に (dtb::init のすぐ後): 出力の場所を決める
pub fn early_init() {
    let pa = crate::dtb::reg_of("arm,pl011", 0).map_or(0x0900_0000, |(a, _)| a as usize);
    unsafe { BASE = crate::memlayout::p2v(pa) };
}

pub fn irq() -> u32 {
    unsafe { IRQ }
}

/// 受信の割り込みを有効にする (irq::init の後)
pub fn init() {
    if let Some(i) = crate::irq::from_dt("arm,pl011") {
        unsafe { IRQ = i };
    }
    mmio::w32(reg(IMSC), INT_RX | INT_RT);
    crate::irq::enable(irq());
}

/// 受信した文字をすべてコンソールへ渡す
pub fn intr() {
    // 先に下げてから読む (読んだ後に下げると、その間に来た文字の割り込みを消してしまう)
    mmio::w32(reg(ICR), INT_RX | INT_RT);
    while mmio::r32(reg(FR)) & FR_RXFE == 0 {
        let c = mmio::r32(reg(DR)) as u8;
        crate::console::intr(c);
    }
}

pub fn putc(c: u8) {
    unsafe {
        if BASE == 0 {
            return;
        }
        while mmio::r32(reg(FR)) & FR_TXFF != 0 {}
        mmio::w32(reg(DR), c as u32);
    }
}

pub struct Uart;

pub static LOCK: SpinLock<()> = SpinLock::new(());

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                putc(b'\r');
            }
            putc(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _g = $crate::uart::LOCK.lock();
        let _ = write!($crate::uart::Uart, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
