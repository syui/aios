#![no_std]
#![no_main]

extern crate alloc;

mod aiproc;
mod apparmor;
mod audit;
mod auditfilter;
mod securityfs;
mod efirt;
mod efivarfs;
mod cpuvuln;
mod trace;
mod fail;
mod oom;
mod bcm2835_rng;
mod boot;
mod mmio;
#[macro_use]
mod uart;
mod kms;
mod kmsg;
mod landlock;
mod timerfd;
mod signalfd;
mod console;
mod tty;
mod block;
mod cred;
mod security;
mod debug;
mod drm;
mod edid;
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
mod lsm_syscalls;
mod lockstat;
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
mod addrconf;
mod net;
mod netif;
mod netlink;
mod nftables;
mod nfhook;
mod conntrack;
mod ctnetlink;
mod parts;
mod pidfd;
mod posix_timers;
mod proc;
mod sd;
mod smp;
mod procfs;
mod ns;
mod seccomp;
mod sysfs;
mod sysrq;
mod rand;
mod signal;
mod socket;
mod spinlock;
mod splice;
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
mod whylog;

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
    // UEFI から起動したなら、そのコマンドライン (EFI スタブの efi.rs が覚えたもの) を DTB の bootargs より先に
    dtb::set_cmdline(efi::cmdline());
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
    // UEFI のランタイムサービスの RAM はよけて (Linux の efi_init と memblock)
    efi::reserve_runtime();
    let early = kalloc::init(heap::boot_extra());
    heap::init(early);
    // 大きなロックなしの stat の表の大きさを RAM から (ほかの CPU とユーザーの前に)
    statc::init();
    println!("kalloc: {} pages free", kalloc::nfree());
    // UEFI のランタイムサービス (efivarfs が使う)
    efirt::init();
    // CPU のぜい弱性 (Spectre / Meltdown) と、SSBS の手当て
    cpuvuln::init();
    println!("initrd: {} entries", initrd::count());
    dtb::summary();
    // 実験のために切ったところ (aios.off=)
    parts::init();
    // LSM をつなぐ (Linux の security_init と CONFIG_LSM="landlock,apparmor": capability は cred.rs の has_cap でいつも先)。
    // 切ったもの (aios.off=) もつなぐ: 動かしたまま入れなおせるように、切ったかはそれぞれがフックの中で見る
    security::register(&landlock::LSM);
    security::register(&apparmor::LSM);
    // seccomp (LSM ではない: Linux の CONFIG_SECCOMP)。切ったもの (aios.off=seccomp) もつなぐ (切るのはシステムコールだけ)
    security::register_seccomp(&seccomp::OPS);
    // audit (LSM ではない: Linux の CONFIG_AUDITSYSCALL)。起動の記録 (audit::init) はいままでどおりあとで
    security::register_audit(&audit::OPS);
    // スワップ (Linux の CONFIG_SWAP)。回収は vm.rs (vmscan)、区画とスロットは swap.rs
    vm::register_swap(&swap::OPS);
    syscall::register(swap::SYSCALLS, swap::sys);
    // なくても動くところが持つシステムコール (Linux の COND_SYSCALL: syscall.rs の表に。置かなければ ENOSYS)
    syscall::register(epoll::SYSCALLS, epoll::sys);
    syscall::register(timerfd::SYSCALLS, timerfd::sys);
    syscall::register(signalfd::SYSCALLS, signalfd::sys);
    syscall::register(inotify::SYSCALLS, inotify::sys);
    // ファイルのロック (Linux の CONFIG_FILE_LOCKING): flock と、fcntl のロック・閉じたときに外すもの (file::FileLocks)
    file::register_locks(&locks::OPS);
    syscall::register(locks::SYSCALLS, locks::sys);
    // LSM のシステムコール (lsm_*: Linux の security/lsm_syscalls.c、landlock_*: security/landlock/syscalls.c)
    syscall::register(lsm_syscalls::SYSCALLS, lsm_syscalls::sys);
    syscall::register(landlock::SYSCALLS, landlock::sys);
    // splice、tee、sendfile、copy_file_range と、大きなロックなしの splice
    syscall::register(splice::SYSCALLS, splice::sys);
    syscall::register_fast_splice(splice::fast);
    // ファイルの出来事の知らせ (Linux の fsnotify: vfs::notify) を inotify へ
    vfs::notify::register(&inotify::OPS);
    // AF_UNIX: fd を閉じたあとの gc (システムコールの終わり) と、大きなロックなしの read / write (syscall::fast)
    file::register_release_deferred(unix::gc_check);
    file::register_fast_rw_sock(unix::fast_rw);
    syscall::register(socket::SYSCALLS, socket::sys);
    syscall::register(module::SYSCALLS, module::sys);
    // 調べもの (Linux の Kernel hacking: tracepoint と fault-inject、aios の prof) と、しくじったわけをためるもの (whylog)。
    // core は debug.rs と why.rs の入り口を呼び、ここでつなぐ (止まっているあいだは ON を見るだけ)
    debug::register_trace(&trace::OPS);
    debug::register_prof(&prof::OPS);
    debug::register_fail(&fail::OPS);
    why::register_log(&whylog::OPS);
    debug::register_sysrq(sysrq::handle);

    irq::init();
    irq::enable_ipi();
    timer::init();
    // itimer と POSIX タイマー (Linux の CONFIG_POSIX_TIMERS)、timerfd の期限 (tick ごと。Linux ではそれぞれの hrtimer)
    syscall::register(posix_timers::SYSCALLS, posix_timers::sys);
    timer::every(1, posix_timers::tick);
    timer::every(1, timerfd::tick);
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
    audit::init();
    // 装置のないファイルシステム (Linux の register_filesystem: mount(2) と、起動のときの /proc や /sys が引く)
    vfs::register_filesystem("proc", || Ok(procfs::new_root()));
    vfs::register_filesystem("sysfs", || Ok(sysfs::new_root()));
    vfs::register_filesystem("securityfs", || Ok(securityfs::new_root()));
    vfs::register_filesystem("efivarfs", efivarfs::new_root);
    // /dev/stdin や /proc/self/fd/N で、名前のないものを開くとき [quirk tty-root-fdlink] (proc の magic link)
    sysfile::register_fd_link(procfs::fd_link_file);
    // ルート: ディスクがあれば (block::mount_root: Linux の mount_root)、なければ initramfs
    fs::init(block::mount_root());
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
