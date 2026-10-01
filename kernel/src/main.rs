#![no_std]
#![no_main]

extern crate alloc;

mod boot;
#[macro_use]
mod uart;
mod console;
mod tty;
mod block;
mod cred;
mod dtb;
mod efi;
mod epoll;
mod exec;
mod extfs;
mod file;
mod fs;
mod irq;
mod heap;
mod initrd;
mod kalloc;
mod memlayout;
mod net;
mod proc;
mod sd;
mod smp;
mod procfs;
mod rand;
mod signal;
mod socket;
mod spinlock;
mod syscall;
mod sysfile;
mod timer;
mod tmpfs;
mod trap;
mod vfat;
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
    // CPU の番号 (smp::id) は 0
    unsafe { core::arch::asm!("msr tpidr_el1, xzr") };
    // 出力の場所 (UART) は DTB で決まるので、何よりも先に DTB を読む
    let has_dtb = dtb::init();
    uart::early_init();
    println!();
    println!("aios {} (aarch64)", env!("AIOS_RELEASE"));
    println!("hello from EL{}", current_el());

    trap::init();
    if has_dtb {
        if let Some((base, size)) = dtb::memory() {
            memlayout::set_ram(base as usize, size as usize);
        }
    }
    kalloc::init();
    heap::init();
    println!("kalloc: {} pages free", kalloc::nfree());
    println!("initrd: {} entries", initrd::count());
    dtb::summary();

    irq::init();
    irq::enable_ipi();
    timer::init();
    uart::init();
    fs::init();
    net::init();

    // ここからは大きなロックを持って (smp.rs)
    smp::lock();
    proc::user_init();
    smp::start();
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
