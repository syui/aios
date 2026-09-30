#![no_std]
#![no_main]

mod boot;
#[macro_use]
mod uart;
mod gic;
mod timer;
mod trap;

use core::panic::PanicInfo;

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
    (el >> 2) & 0b11
}

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    println!();
    println!("aios {} (aarch64)", env!("CARGO_PKG_VERSION"));
    println!("hello from EL{}", current_el());

    trap::init();
    unsafe { core::arch::asm!("brk #1") };

    gic::init();
    timer::init();
    trap::intr_on();

    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("panic: {}", info);
    halt()
}
