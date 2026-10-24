#![no_std]
#![no_main]

extern crate alloc;

mod aiproc;
mod trace;
mod fail;
mod oom;
mod bcm2835_rng;
mod boot;
mod mmio;
#[macro_use]
mod uart;
mod kmsg;
mod landlock;
mod timerfd;
mod console;
mod tty;
mod block;
mod cred;
mod crypto;
mod dtb;
mod efi;
mod epoll;
mod inotify;
mod exec;
mod extfs;
mod fastdir;
mod file;
mod locks;
mod fs;
mod futex;
mod irq;
mod heap;
mod tlsf;
mod mem;
mod initrd;
mod kalloc;
mod memlayout;
mod prof;
mod inet;
mod net;
mod netif;
mod parts;
mod proc;
mod sd;
mod smp;
mod procfs;
mod ns;
mod seccomp;
mod sysfs;
mod rand;
mod signal;
mod socket;
mod spinlock;
mod cell;
mod statc;
mod swap;
mod syscall;
mod sysctl;
mod sysfile;
mod timer;
mod tmpfs;
mod unix;
mod trap;
mod vfat;
mod vfs;
mod virtio;
mod gpu;
mod input;
mod sound;
mod module;
mod virtio_blk;
mod virtio_net;
mod virtio_rng;
mod vdso;
mod vm;
mod why;

use core::panic::PanicInfo;

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
    (el >> 2) & 0b11
}

#[unsafe(no_mangle)]
// AIOS_TEST_HANG のものは途中で止まるので、そのあとはたどりつかない
#[cfg_attr(aios_test_hang, allow(unreachable_code, unused_variables))]
pub extern "C" fn kmain() -> ! {
    // CPU の番号 (smp::id) は 0
    unsafe { core::arch::asm!("msr tpidr_el1, xzr") };
    // 出力の場所 (UART) は DTB で決まるので、何よりも先に DTB を読む
    let has_dtb = dtb::init();
    uart::early_init();
    println!();
    println!("aios {} (aarch64)", env!("AIOS_RELEASE"));
    println!("hello from EL{}", current_el());
    // AIOS_TEST_HANG=1 でビルドしたもの (test/vm-boot.sh) だけ: aiosd まで来ない新しいカーネルのふりをする
    #[cfg(aios_test_hang)]
    {
        println!("AIOS_TEST_HANG: stopping here on purpose");
        loop {
            core::hint::spin_loop();
        }
    }

    trap::init();
    if has_dtb {
        if let Some((base, size)) = dtb::memory() {
            memlayout::set_ram(base as usize, size as usize);
        }
    }
    let early = kalloc::init(heap::boot_extra());
    heap::init(early);
    println!("kalloc: {} pages free", kalloc::nfree());
    println!("initrd: {} entries", initrd::count());
    dtb::summary();
    // 実験のために切ったところ (aios.off=)
    parts::init();

    irq::init();
    irq::enable_ipi();
    timer::init();
    vdso::init();
    uart::init();
    // 乱数: 起動のときのもと (UEFI がくれたもの、DTB の rng-seed、CPU の RNDR) と、乱数の装置 (virtio-rng かラズパイのもの)。
    // 使う人 (ネット、exec) より前に
    efi::take_seed(|b| rand::seed("efi-rng", b));
    rand::init();
    if parts::off(parts::Part::VirtioRng) || !virtio_rng::init() {
        if !parts::off(parts::Part::Bcm2835Rng) {
            bcm2835_rng::init();
        }
    }
    rand::summary();
    fs::init();
    if !parts::off(parts::Part::Net) {
        net::init();
    }

    // ここからは大きなロックを持って (smp.rs)
    smp::lock();
    proc::user_init();
    vfs::start_flusher();
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
    backtrace();
    halt()
}

/// フレームポインタ (x29) をたどって、呼んだところの番地を出す (.cargo/config.toml の force-frame-pointers)。
/// addr2line -e カーネルの ELF -f -C 番地 ... で関数と行になる
fn backtrace() {
    let mut fp: usize;
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp) };
    let lo = memlayout::p2v(memlayout::ram_base());
    let hi = lo + memlayout::ram_size();
    // ヒープを使わない (ヒープが足りなくて panic したときにも出せるように)
    print!("backtrace:");
    for _ in 0..32 {
        if fp % 16 != 0 || fp < lo || fp + 16 > hi {
            break;
        }
        let (next, lr) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        if lr == 0 {
            break;
        }
        print!(" {:#x}", lr);
        if next <= fp {
            break;
        }
        fp = next;
    }
    println!();
}
