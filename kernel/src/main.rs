#![no_std]
#![no_main]

extern crate alloc;

mod boot;
#[macro_use]
mod uart;
mod exec;
mod gic;
mod heap;
mod initrd;
mod kalloc;
mod memlayout;
mod proc;
mod rand;
mod spinlock;
mod syscall;
mod timer;
mod trap;
mod vm;

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
    kalloc::init();
    heap::init();
    println!("kalloc: {} pages free", kalloc::nfree());
    println!("initrd: {} entries", initrd::count());

    gic::init();
    timer::init();

    proc::user_init();
    proc::scheduler()
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
