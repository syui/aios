// プロセスとスケジューラ (1 CPU)
//
// カーネルの中では割り込みを止めたまま動く。切り替えが起きるのは
// EL0 からのタイマ割り込みと、sleep/yield/exit のときだけ。
use crate::exec::{self, Image};
use crate::trap::TrapFrame;
use crate::vm::PageTable;
use alloc::vec::Vec;

pub const NPROC: usize = 64;
const KSTACK_SIZE: usize = 16 * 1024;
pub const MMAP_BASE: usize = 0x10_0000_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unused,
    Runnable,
    Running,
    Sleeping,
    Zombie,
}

/// swtch で保存する callee-saved レジスタ
#[repr(C)]
#[derive(Clone, Copy)]
struct Context {
    x19_x30: [u64; 12],
    sp: u64,
}

impl Context {
    const ZERO: Self = Self { x19_x30: [0; 12], sp: 0 };
}

/// EL0 の FP/SIMD レジスタ (q0-q31, fpcr, fpsr)
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FpState {
    q: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

impl FpState {
    const ZERO: Self = Self { q: [0; 32], fpcr: 0, fpsr: 0 };
}

pub struct Proc {
    pub state: State,
    pub pid: u32,
    pub ppid: u32,
    pub pagetable: Option<PageTable>,
    /// brk の下限 (ELF の末尾) と現在値
    pub heap_start: usize,
    pub brk: usize,
    /// 次に mmap で渡す場所
    pub mmap_next: usize,
    pub xstatus: i32,
    chan: usize,
    context: Context,
    tpidr: u64,
    fp: FpState,
}

impl Proc {
    const UNUSED: Self = Self {
        state: State::Unused,
        pid: 0,
        ppid: 0,
        pagetable: None,
        heap_start: 0,
        brk: 0,
        mmap_next: 0,
        xstatus: 0,
        chan: 0,
        context: Context::ZERO,
        tpidr: 0,
        fp: FpState::ZERO,
    };

    pub fn pt(&mut self) -> &mut PageTable {
        self.pagetable.as_mut().expect("proc without pagetable")
    }

    fn slot(&self) -> usize {
        (self as *const Proc as usize - (&raw const PROCS) as usize) / size_of::<Proc>()
    }

    fn kstack_top(&self) -> usize {
        (&raw const KSTACKS) as usize + (self.slot() + 1) * KSTACK_SIZE
    }

    /// EL0 から入ってきたときの TrapFrame はいつもカーネルスタックの天辺にある
    pub fn tf(&mut self) -> &mut TrapFrame {
        unsafe { &mut *((self.kstack_top() - size_of::<TrapFrame>()) as *mut TrapFrame) }
    }

    fn load_image(&mut self, img: Image) {
        self.pagetable = Some(img.pagetable);
        self.heap_start = img.brk;
        self.brk = img.brk;
        self.mmap_next = MMAP_BASE;
        let tf = self.tf();
        *tf = TrapFrame::zeroed();
        tf.elr = img.entry as u64;
        tf.sp_el0 = img.sp as u64;
        tf.spsr = 0; // EL0t, 割り込み許可
    }
}

#[repr(C, align(16))]
struct KStacks([[u8; KSTACK_SIZE]; NPROC]);

static mut KSTACKS: KStacks = KStacks([[0; KSTACK_SIZE]; NPROC]);
static mut PROCS: [Proc; NPROC] = [const { Proc::UNUSED }; NPROC];
static mut CURRENT: Option<usize> = None;
static mut SCHEDULER: Context = Context::ZERO;
static mut NEXT_PID: u32 = 1;

fn procs() -> &'static mut [Proc; NPROC] {
    unsafe { &mut *(&raw mut PROCS) }
}

pub fn current() -> &'static mut Proc {
    let i = unsafe { CURRENT }.expect("no current proc");
    &mut procs()[i]
}

core::arch::global_asm!(
    r#"
.section .text
// swtch(old: *mut Context, new: *const Context)
.global swtch
swtch:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9, [x0, #96]
    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x9, [x1, #96]
    mov     sp, x9
    ret

// 新しいプロセスは swtch からここへ戻り、TrapFrame を戻して EL0 へ
.global forkret
forkret:
    b       trap_ret

.arch_extension fp
.arch_extension simd
// fp_save(st: *mut FpState)
.global fp_save
fp_save:
    stp     q0, q1, [x0, #0]
    stp     q2, q3, [x0, #32]
    stp     q4, q5, [x0, #64]
    stp     q6, q7, [x0, #96]
    stp     q8, q9, [x0, #128]
    stp     q10, q11, [x0, #160]
    stp     q12, q13, [x0, #192]
    stp     q14, q15, [x0, #224]
    stp     q16, q17, [x0, #256]
    stp     q18, q19, [x0, #288]
    stp     q20, q21, [x0, #320]
    stp     q22, q23, [x0, #352]
    stp     q24, q25, [x0, #384]
    stp     q26, q27, [x0, #416]
    stp     q28, q29, [x0, #448]
    stp     q30, q31, [x0, #480]
    mrs     x1, fpcr
    mrs     x2, fpsr
    add     x0, x0, #512
    stp     x1, x2, [x0]
    ret

// fp_load(st: *const FpState)
.global fp_load
fp_load:
    ldp     q0, q1, [x0, #0]
    ldp     q2, q3, [x0, #32]
    ldp     q4, q5, [x0, #64]
    ldp     q6, q7, [x0, #96]
    ldp     q8, q9, [x0, #128]
    ldp     q10, q11, [x0, #160]
    ldp     q12, q13, [x0, #192]
    ldp     q14, q15, [x0, #224]
    ldp     q16, q17, [x0, #256]
    ldp     q18, q19, [x0, #288]
    ldp     q20, q21, [x0, #320]
    ldp     q22, q23, [x0, #352]
    ldp     q24, q25, [x0, #384]
    ldp     q26, q27, [x0, #416]
    ldp     q28, q29, [x0, #448]
    ldp     q30, q31, [x0, #480]
    add     x0, x0, #512
    ldp     x1, x2, [x0]
    msr     fpcr, x1
    msr     fpsr, x2
    ret
"#
);

unsafe extern "C" {
    fn swtch(old: *mut Context, new: *const Context);
    fn forkret();
    fn fp_save(st: *mut FpState);
    fn fp_load(st: *const FpState);
}

/// 空きスロットを取り、forkret から EL0 へ戻れるようにする
fn alloc_proc() -> Option<&'static mut Proc> {
    let p = procs().iter_mut().find(|p| p.state == State::Unused)?;
    *p = Proc::UNUSED;
    unsafe {
        p.pid = NEXT_PID;
        NEXT_PID += 1;
    }
    p.context.x19_x30[11] = forkret as *const () as u64; // x30 (lr)
    p.context.sp = (p.kstack_top() - size_of::<TrapFrame>()) as u64;
    Some(p)
}

pub fn user_init() {
    let argv = [b"/init".to_vec()];
    let envp = [b"HOME=/".to_vec(), b"PATH=/bin".to_vec(), b"TERM=vt100".to_vec()];
    let img = match exec::exec("/init", &argv, &envp) {
        Ok(img) => img,
        Err(e) => panic!("user_init: cannot exec /init ({})", e),
    };
    let p = alloc_proc().expect("user_init: no proc slot");
    p.load_image(img);
    p.state = State::Runnable;
}

/// 実行できるプロセスを順番に走らせ続ける
pub fn scheduler() -> ! {
    loop {
        let mut ran = false;
        for i in 0..NPROC {
            let p = &mut procs()[i];
            if p.state != State::Runnable {
                continue;
            }
            p.state = State::Running;
            unsafe {
                CURRENT = Some(i);
                p.pt().activate();
                fp_load(&p.fp);
                core::arch::asm!("msr tpidr_el0, {}", in(reg) p.tpidr);
                swtch(&raw mut SCHEDULER, &p.context);
                CURRENT = None;
            }
            ran = true;
        }
        if !ran {
            // することがないので割り込みを待つ
            crate::trap::intr_on();
            unsafe { core::arch::asm!("wfi") };
            crate::trap::intr_off();
        }
    }
}

/// スケジューラへ戻る。state は呼ぶ側が変えておく
fn sched() {
    let p = current();
    unsafe {
        fp_save(&mut p.fp);
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) p.tpidr);
        swtch(&mut p.context, &raw const SCHEDULER);
    }
}

pub fn yield_now() {
    current().state = State::Runnable;
    sched();
}

pub fn sleep(chan: usize) {
    let p = current();
    p.chan = chan;
    p.state = State::Sleeping;
    sched();
    current().chan = 0;
}

pub fn wakeup(chan: usize) {
    for p in procs().iter_mut() {
        if p.state == State::Sleeping && p.chan == chan {
            p.state = State::Runnable;
        }
    }
}

fn find(pid: u32) -> Option<&'static mut Proc> {
    procs().iter_mut().find(|p| p.state != State::Unused && p.pid == pid)
}

pub fn exit(status: i32) -> ! {
    let p = current();
    if p.pid == 1 {
        panic!("init exited with status {}", status);
    }
    for c in procs().iter_mut() {
        if c.state != State::Unused && c.ppid == p.pid {
            c.ppid = 1;
        }
    }
    p.xstatus = status;
    p.state = State::Zombie;
    if let Some(parent) = find(p.ppid) {
        wakeup(parent as *mut Proc as usize);
    }
    wakeup(find(1).map(|i| i as *mut Proc as usize).unwrap_or(0));
    sched();
    unreachable!("zombie ran");
}

const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_THREAD: u64 = 0x0001_0000;

/// fork / clone。CLONE_VM でもアドレス空間はコピーする (vfork と同じ振る舞いになる)
pub fn clone(flags: u64, stack: usize, tls: u64) -> Result<u32, i64> {
    const EAGAIN: i64 = 11;
    const ENOMEM: i64 = 12;
    const EINVAL: i64 = 22;
    if flags & CLONE_THREAD != 0 {
        return Err(-EINVAL);
    }
    let parent = current();
    let pt = parent.pt().fork().ok_or(-ENOMEM)?;
    let child = alloc_proc().ok_or(-EAGAIN)?;
    child.pagetable = Some(pt);
    child.ppid = parent.pid;
    child.heap_start = parent.heap_start;
    child.brk = parent.brk;
    child.mmap_next = parent.mmap_next;
    unsafe {
        fp_save(&mut child.fp);
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) child.tpidr);
    }
    if flags & CLONE_SETTLS != 0 {
        child.tpidr = tls;
    }
    let ptf = parent.tf() as *const TrapFrame;
    let ctf = child.tf();
    unsafe { core::ptr::copy_nonoverlapping(ptf, ctf, 1) };
    ctf.x[0] = 0;
    if stack != 0 {
        ctf.sp_el0 = stack as u64;
    }
    child.state = State::Runnable;
    Ok(child.pid)
}

pub fn execve(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), i64> {
    let img = exec::exec(path, argv, envp)?;
    let p = current();
    let old = p.pagetable.take();
    p.load_image(img);
    p.pt().activate();
    drop(old);
    Ok(())
}

const WNOHANG: u64 = 1;

/// 子の終了を待つ。(pid, status) を返す
pub fn wait(pid: i64, options: u64) -> Result<(u32, i32), i64> {
    const ECHILD: i64 = 10;
    let me = current();
    loop {
        let mut have = false;
        for c in procs().iter_mut() {
            if c.state == State::Unused || c.ppid != me.pid || (pid > 0 && c.pid as i64 != pid) {
                continue;
            }
            have = true;
            if c.state == State::Zombie {
                let r = (c.pid, c.xstatus);
                c.pagetable = None;
                c.state = State::Unused;
                return Ok(r);
            }
        }
        if !have {
            return Err(-ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok((0, 0));
        }
        sleep(me as *mut Proc as usize);
    }
}
