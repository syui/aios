// PL011 UART (qemu virt: PA 0x0900_0000)
use crate::memlayout::UART0 as BASE;
use crate::spinlock::SpinLock;
use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const DR: *mut u32 = BASE as *mut u32;
const FR: *const u32 = (BASE + 0x18) as *const u32;
const IMSC: *mut u32 = (BASE + 0x38) as *mut u32;
const ICR: *mut u32 = (BASE + 0x44) as *mut u32;
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;
const INT_RX: u32 = 1 << 4;
const INT_RT: u32 = 1 << 6;

/// PL011 は SPI 1
pub const IRQ: u32 = 33;

pub fn init() {
    unsafe { write_volatile(IMSC, INT_RX | INT_RT) };
    crate::gic::enable(IRQ);
}

/// 受信した文字をすべてコンソールへ渡す
pub fn intr() {
    unsafe {
        // 先に下げてから読む (読んだ後に下げると、その間に来た文字の割り込みを消してしまう)
        write_volatile(ICR, INT_RX | INT_RT);
        while read_volatile(FR) & FR_RXFE == 0 {
            let c = read_volatile(DR) as u8;
            crate::console::intr(c);
        }
    }
}

pub fn putc(c: u8) {
    unsafe {
        while read_volatile(FR) & FR_TXFF != 0 {}
        write_volatile(DR, c as u32);
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
