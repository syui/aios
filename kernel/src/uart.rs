// PL011 UART (qemu virt: 0x0900_0000)
use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const BASE: usize = 0x0900_0000;
const DR: *mut u32 = BASE as *mut u32;
const FR: *const u32 = (BASE + 0x18) as *const u32;
const FR_TXFF: u32 = 1 << 5;

pub fn putc(c: u8) {
    unsafe {
        while read_volatile(FR) & FR_TXFF != 0 {}
        write_volatile(DR, c as u32);
    }
}

pub struct Uart;

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
        let _ = write!($crate::uart::Uart, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{ $crate::print!($($arg)*); $crate::print!("\n"); }};
}
