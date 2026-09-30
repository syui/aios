#![no_std]
#![no_main]

extern crate alloc;

mod boot;
#[macro_use]
mod uart;
mod console;
mod tty;
mod cred;
mod exec;
mod extfs;
mod file;
mod fs;
mod gic;
mod heap;
mod initrd;
mod kalloc;
mod memlayout;
mod net;
mod proc;
mod rand;
mod signal;
mod socket;
mod spinlock;
mod syscall;
mod sysfile;
mod timer;
mod tmpfs;
mod trap;
mod vfs;
mod virtio;
mod virtio_blk;
mod virtio_net;
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
    uart::init();
    fs::init();
    net::init();

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
