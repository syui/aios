// プロセス (いまは init 1 つだけ)
use crate::exec;
use crate::kalloc;
use crate::memlayout::PGSIZE;
use crate::trap::TrapFrame;
use crate::vm::PageTable;

pub const MMAP_BASE: usize = 0x10_0000_0000;

pub struct Proc {
    pub pid: u32,
    pub pagetable: PageTable,
    /// brk の下限 (ELF の末尾) と現在値
    pub heap_start: usize,
    pub brk: usize,
    /// 次に mmap で渡す場所
    pub mmap_next: usize,
    kstack: *mut u8,
}

static mut INIT: Option<Proc> = None;

pub fn current() -> &'static mut Proc {
    unsafe { (*(&raw mut INIT)).as_mut().expect("no current proc") }
}

pub fn user_init() -> ! {
    let argv: [&[u8]; 1] = [b"/init"];
    let envp: [&[u8]; 3] = [b"HOME=/", b"PATH=/bin", b"TERM=vt100"];
    let img = match exec::exec("/init", &argv, &envp) {
        Ok(img) => img,
        Err(e) => panic!("user_init: cannot exec /init ({})", e),
    };

    let kstack = kalloc::alloc().expect("user_init: out of memory");
    let proc = Proc {
        pid: 1,
        pagetable: img.pagetable,
        heap_start: img.brk,
        brk: img.brk,
        mmap_next: MMAP_BASE,
        kstack,
    };
    unsafe { *(&raw mut INIT) = Some(proc) };

    let p = current();
    p.pagetable.activate();

    // カーネルスタックの天辺に TrapFrame を置いて eret で EL0 へ
    let tf = unsafe { &mut *((p.kstack as usize + PGSIZE - size_of::<TrapFrame>()) as *mut TrapFrame) };
    *tf = TrapFrame::zeroed();
    tf.elr = img.entry as u64;
    tf.sp_el0 = img.sp as u64;
    tf.spsr = 0; // EL0t, 割り込み許可
    crate::trap::user_return(tf)
}

pub fn exit(status: i32) -> ! {
    let p = current();
    println!("pid {} exited with status {}", p.pid, status);
    // まだスケジューラが無いので、割り込みを許して眠り続ける
    crate::trap::intr_on();
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
