// プロセス (いまは init 1 つだけ)
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use crate::trap::TrapFrame;
use crate::vm::{PageTable, Perm};

pub const USER_TEXT: usize = 0x40_0000;
pub const USER_STACK_TOP: usize = 0x40_0000_0000;

pub struct Proc {
    pub pid: u32,
    pub pagetable: PageTable,
    kstack: *mut u8,
}

static mut INIT: Option<Proc> = None;

pub fn current() -> &'static mut Proc {
    unsafe { (*(&raw mut INIT)).as_mut().expect("no current proc") }
}

// 最初のユーザープログラム。位置独立で USER_TEXT に写される。
core::arch::global_asm!(
    r#"
.section .rodata
.balign 4
.global initcode
.global initcode_end
initcode:
    mov     x0, #1
    adr     x1, 1f
    mov     x2, #(2f - 1f)
    mov     x8, #64
    svc     #0
    mov     x0, #0
    mov     x8, #94
    svc     #0
0:  b       0b
1:  .ascii  "hello from EL0 (aios init)\n"
2:
.balign 4
initcode_end:
"#
);

pub fn user_init() -> ! {
    unsafe extern "C" {
        static initcode: u8;
        static initcode_end: u8;
    }
    let code = unsafe {
        let start = &raw const initcode;
        let len = (&raw const initcode_end).offset_from(start) as usize;
        core::slice::from_raw_parts(start, len)
    };
    assert!(code.len() <= PGSIZE);

    let mut pt = PageTable::new().expect("user_init: out of memory");
    let text = kalloc::alloc().expect("user_init: out of memory");
    unsafe { core::ptr::copy_nonoverlapping(code.as_ptr(), text, code.len()) };
    pt.map(USER_TEXT, v2p(text as usize), Perm::RX).unwrap();
    let stack = kalloc::alloc().expect("user_init: out of memory");
    pt.map(USER_STACK_TOP - PGSIZE, v2p(stack as usize), Perm::RW).unwrap();

    let kstack = kalloc::alloc().expect("user_init: out of memory");
    unsafe { *(&raw mut INIT) = Some(Proc { pid: 1, pagetable: pt, kstack }) };

    let p = current();
    p.pagetable.activate();

    // カーネルスタックの天辺に TrapFrame を置いて eret で EL0 へ
    let tf = unsafe { &mut *((p.kstack as usize + PGSIZE - size_of::<TrapFrame>()) as *mut TrapFrame) };
    *tf = TrapFrame::zeroed();
    tf.elr = USER_TEXT as u64;
    tf.sp_el0 = USER_STACK_TOP as u64;
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
