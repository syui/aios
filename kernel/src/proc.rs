// プロセス・スレッドとスケジューラ (1 CPU)
//
// カーネルの中では割り込みを止めたまま動く。切り替えが起きるのは
// EL0 からのタイマ割り込みと、sleep/yield/exit のときだけ。
// スレッドは mm (アドレス空間) と files を共有する Proc。
use crate::exec::{self, Image};
use crate::file::{self, FileRef, Kind};
use crate::trap::TrapFrame;
use crate::vm::PageTable;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

pub const NPROC: usize = 64;
pub const NOFILE: usize = 256;
const KSTACK_SIZE: usize = 16 * 1024;
pub const MMAP_BASE: usize = 0x10_0000_0000;

pub const EINTR: i64 = 4;
pub const NSIG: usize = 65;
pub const SIG_IGN: u64 = 1;
pub const SIGKILL: i32 = 9;
pub const SIGPIPE: i32 = 13;

/// exit(code) の wait status
fn exited(code: i32) -> i32 {
    (code & 0xff) << 8
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unused,
    Runnable,
    Running,
    Sleeping,
    Zombie,
}

/// 複数の Proc から使う持ち物。1 CPU で割り込みを止めているので、同時には触られない
pub struct Shared<T>(Rc<UnsafeCell<T>>);

impl<T> Shared<T> {
    pub fn new(v: T) -> Self {
        Self(Rc::new(UnsafeCell::new(v)))
    }
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    fn id(&self) -> usize {
        Rc::as_ptr(&self.0) as usize
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// アドレス空間
pub struct Mm {
    pub pt: PageTable,
    /// brk の下限 (ELF の末尾) と現在値
    pub heap_start: usize,
    pub brk: usize,
    /// 次に mmap で渡す場所
    pub mmap_next: usize,
}

#[derive(Clone)]
pub struct Fd {
    pub file: FileRef,
    pub cloexec: bool,
}

/// fd テーブルとカレントディレクトリ (先頭 / なし)
#[derive(Clone)]
pub struct Files {
    pub fds: Vec<Option<Fd>>,
    pub cwd: String,
}

impl Files {
    pub fn get(&self, fd: u64) -> Option<&FileRef> {
        self.fds.get(fd as usize)?.as_ref().map(|f| &f.file)
    }

    /// minfd 以上で空いている一番小さい fd に置く
    pub fn add(&mut self, file: FileRef, cloexec: bool, minfd: usize) -> Option<usize> {
        let i = (minfd..NOFILE).find(|&i| self.fds.get(i).is_none_or(|f| f.is_none()))?;
        if self.fds.len() <= i {
            self.fds.resize(i + 1, None);
        }
        self.fds[i] = Some(Fd { file, cloexec });
        Some(i)
    }
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
    /// スレッド ID。プロセスの代表スレッドでは tgid と同じ
    pub pid: u32,
    pub tgid: u32,
    pub ppid: u32,
    pub mm: Option<Shared<Mm>>,
    pub files: Option<Shared<Files>>,
    /// wait4 が返す status (終了コード << 8、またはシグナル番号)
    pub xstatus: i32,
    /// 終わるように言われた。sleep から戻ったら確かめる
    pub killed: bool,
    /// スレッドグループ全体の終了 status (代表スレッドに置く)
    group_exit: Option<i32>,
    /// シグナルごとのハンドラ (SIG_DFL = 0, SIG_IGN = 1)。まだ呼び出しはしない
    pub sig_handlers: [u64; NSIG],
    /// ユーザーとグループ
    pub cred: crate::cred::Cred,
    /// 代表でないスレッド。親は wait せず、終わったらスケジューラが片付ける
    thread: bool,
    /// 終了時に 0 を書いて futex で起こす場所 (CLONE_CHILD_CLEARTID)
    pub clear_tid: usize,
    chan: usize,
    /// この tick になったら起こす (0 なら無し)
    wake_at: u64,
    context: Context,
    tpidr: u64,
    fp: FpState,
}

impl Proc {
    const UNUSED: Self = Self {
        state: State::Unused,
        pid: 0,
        tgid: 0,
        ppid: 0,
        mm: None,
        files: None,
        xstatus: 0,
        killed: false,
        group_exit: None,
        sig_handlers: [0; NSIG],
        cred: crate::cred::Cred::ROOT,
        thread: false,
        clear_tid: 0,
        chan: 0,
        wake_at: 0,
        context: Context::ZERO,
        tpidr: 0,
        fp: FpState::ZERO,
    };

    pub fn mm(&self) -> &mut Mm {
        self.mm.as_ref().expect("proc without mm").get()
    }

    pub fn pt(&self) -> &mut PageTable {
        &mut self.mm().pt
    }

    pub fn files(&self) -> &mut Files {
        self.files.as_ref().expect("proc without files").get()
    }

    /// futex の channel を作るための、アドレス空間ごとの値
    pub fn mm_id(&self) -> usize {
        self.mm.as_ref().map_or(0, |m| m.id())
    }

    fn slot(&self) -> usize {
        (self as *const Proc as usize - (&raw const PROCS) as usize) / size_of::<Proc>()
    }

    fn kstack_top(&self) -> usize {
        (&raw const KSTACKS) as usize + (self.slot() + 1) * KSTACK_SIZE
    }

    fn tf_ref(&self) -> &TrapFrame {
        unsafe { &*((self.kstack_top() - size_of::<TrapFrame>()) as *const TrapFrame) }
    }

    /// EL0 から入ってきたときの TrapFrame はいつもカーネルスタックの天辺にある
    pub fn tf(&mut self) -> &mut TrapFrame {
        unsafe { &mut *((self.kstack_top() - size_of::<TrapFrame>()) as *mut TrapFrame) }
    }

    fn load_image(&mut self, img: Image) {
        self.mm = Some(Shared::new(Mm { pt: img.pagetable, heap_start: img.brk, brk: img.brk, mmap_next: MMAP_BASE }));
        let tf = self.tf();
        *tf = TrapFrame::zeroed();
        tf.elr = img.entry as u64;
        tf.sp_el0 = img.sp as u64;
        tf.spsr = 0; // EL0t, 割り込み許可
    }

    /// 同じスレッドグループの他の Proc
    fn siblings(&self) -> impl Iterator<Item = &'static mut Proc> {
        let (tgid, pid) = (self.tgid, self.pid);
        procs().iter_mut().filter(move |p| p.state != State::Unused && p.tgid == tgid && p.pid != pid)
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

/// いまのプロセスの資格情報。まだプロセスがなければ root
pub fn current_cred() -> crate::cred::Cred {
    match unsafe { CURRENT } {
        Some(i) => procs()[i].cred.clone(),
        None => crate::cred::Cred::ROOT,
    }
}

/// exec が使う cwd。user_init のときはまだ current がないのでルート
pub fn current_cwd() -> String {
    match unsafe { CURRENT } {
        Some(i) => procs()[i].files().cwd.clone(),
        None => String::new(),
    }
}

pub fn nprocs() -> usize {
    procs().iter().filter(|p| p.state != State::Unused).count()
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
    p.tgid = p.pid;
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
    let mut files = Files { fds: Vec::new(), cwd: String::new() };
    let console = file::new(Kind::Console, 2);
    for _ in 0..3 {
        files.add(console.clone(), false, 0);
    }
    p.files = Some(Shared::new(files));
    p.state = State::Runnable;
}

/// 実行できるプロセスを順番に走らせ続ける
pub fn scheduler() -> ! {
    loop {
        let mut ran = false;
        for i in 0..NPROC {
            let p = &mut procs()[i];
            if p.state == State::Zombie && p.thread {
                // 終わったスレッドは誰も wait しないのでここで片付ける
                *p = Proc::UNUSED;
                continue;
            }
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

/// chan で起こされるまで眠る。killed なら Err(-EINTR)
pub fn sleep(chan: usize) -> Result<(), i64> {
    sleep_until(chan, 0).map(|_| ())
}

/// chan で起こされるか deadline (tick, 0 なら無し) まで眠る。時間切れなら Ok(false)
pub fn sleep_until(chan: usize, deadline: u64) -> Result<bool, i64> {
    let p = current();
    if p.killed {
        return Err(-EINTR);
    }
    p.chan = chan;
    p.wake_at = deadline;
    p.state = State::Sleeping;
    sched();
    let p = current();
    p.chan = 0;
    let timed_out = p.wake_at != 0 && crate::timer::ticks() >= p.wake_at;
    p.wake_at = 0;
    if p.killed {
        return Err(-EINTR);
    }
    Ok(!timed_out)
}

/// chan で眠っている Proc を起こし、起こした数を返す
pub fn wakeup(chan: usize) -> usize {
    let mut n = 0;
    for p in procs().iter_mut() {
        if p.state == State::Sleeping && p.chan == chan {
            p.state = State::Runnable;
            n += 1;
        }
    }
    n
}

/// Ctrl-P: プロセスの一覧 (デバッグ用)
pub fn dump() {
    println!();
    for (i, p) in procs().iter().enumerate() {
        if p.state == State::Unused {
            continue;
        }
        let st = match p.state {
            State::Runnable => "runnable",
            State::Running => "running",
            State::Sleeping => "sleeping",
            State::Zombie => "zombie",
            State::Unused => "unused",
        };
        let tf = p.tf_ref();
        println!("[{}] pid {} tgid {} ppid {} {} chan {:#x} wake_at {} pc {:#x} x8 {} x0 {:#x}", i, p.pid, p.tgid, p.ppid, st, p.chan, p.wake_at, tf.elr, tf.x[8], tf.x[0]);
    }
    println!("ticks {} current {:?}", crate::timer::ticks(), unsafe { CURRENT });
}

static POLL: u8 = 0;

/// poll で待っている人が眠る channel。何かの状態が変わったらここを起こす
pub fn poll_chan() -> usize {
    (&raw const POLL) as usize
}

/// タイマから: 期限の来た Proc を起こす
pub fn wake_expired(now: u64) {
    for p in procs().iter_mut() {
        if p.state == State::Sleeping && p.wake_at != 0 && p.wake_at <= now {
            p.state = State::Runnable;
        }
    }
}

fn kill_proc(p: &mut Proc) {
    p.killed = true;
    if p.state == State::Sleeping {
        p.state = State::Runnable;
    }
}

fn find(pid: u32) -> Option<&'static mut Proc> {
    procs().iter_mut().find(|p| p.state != State::Unused && p.pid == pid)
}

/// clear_tid に 0 を書いて、それを待つ futex を起こす
fn clear_child_tid(p: &mut Proc) {
    if p.clear_tid != 0 {
        let addr = p.clear_tid;
        p.clear_tid = 0;
        if p.pt().copy_out(addr, &0u32.to_le_bytes()).is_some() {
            wakeup(futex_chan(p, addr));
        }
    }
}

pub fn futex_chan(p: &Proc, uaddr: usize) -> usize {
    p.mm_id() ^ (uaddr << 1) ^ 1
}

/// いまのスレッドだけを終える。代表スレッドならプロセスの終了になる
pub fn exit(code: i32) -> ! {
    exit_status(exited(code))
}

fn exit_status(status: i32) -> ! {
    let p = current();
    clear_child_tid(p);
    if p.thread {
        p.files = None;
        p.state = State::Zombie;
        sched();
        unreachable!("zombie thread ran");
    }
    // 代表スレッド: 残りのスレッドも止める
    for t in p.siblings() {
        kill_proc(t);
    }
    if p.tgid == 1 {
        panic!("init exited with status {}", status);
    }
    for c in procs().iter_mut() {
        if c.state != State::Unused && c.ppid == p.tgid && !c.thread {
            c.ppid = 1;
        }
    }
    p.files = None;
    p.xstatus = p.group_exit.unwrap_or(status);
    p.state = State::Zombie;
    if let Some(parent) = find(p.ppid) {
        wakeup(parent as *mut Proc as usize);
    }
    if let Some(init) = find(1) {
        wakeup(init as *mut Proc as usize);
    }
    sched();
    unreachable!("zombie ran");
}

/// スレッドグループ全体を終える
pub fn exit_group(code: i32) -> ! {
    group_exit_status(exited(code))
}

/// シグナルで終わる (既定の動作)
pub fn die(sig: i32) -> ! {
    group_exit_status(sig & 0x7f)
}

fn group_exit_status(status: i32) -> ! {
    let p = current();
    if p.thread {
        if let Some(leader) = find(p.tgid) {
            leader.group_exit.get_or_insert(status);
            kill_proc(leader);
        }
        for t in p.siblings() {
            kill_proc(t);
        }
    }
    exit_status(status)
}

/// EL0 へ戻る前に: 終わるように言われていたら終わる
pub fn check_killed() {
    let p = current();
    if p.killed {
        let status = if p.thread { 0 } else { p.group_exit.unwrap_or(SIGKILL) };
        exit_status(status);
    }
}

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// fork / スレッド作成。CLONE_THREAD でない CLONE_VM (vfork) はアドレス空間をコピーする
pub fn clone(flags: u64, stack: usize, ptid: usize, tls: u64, ctid: usize) -> Result<u32, i64> {
    const EAGAIN: i64 = 11;
    const ENOMEM: i64 = 12;
    const EINVAL: i64 = 22;
    let thread = flags & CLONE_THREAD != 0;
    if thread && flags & CLONE_VM == 0 {
        return Err(-EINVAL);
    }
    let parent = current();
    let mm = if thread {
        parent.mm.clone().unwrap()
    } else {
        let m = parent.mm();
        let pt = m.pt.fork().ok_or(-ENOMEM)?;
        Shared::new(Mm { pt, heap_start: m.heap_start, brk: m.brk, mmap_next: m.mmap_next })
    };
    let files = if flags & CLONE_FILES != 0 && thread {
        parent.files.clone().unwrap()
    } else {
        Shared::new(parent.files().clone())
    };
    let child = alloc_proc().ok_or(-EAGAIN)?;
    child.mm = Some(mm);
    child.files = Some(files);
    child.sig_handlers = parent.sig_handlers;
    child.cred = parent.cred.clone();
    child.thread = thread;
    if thread {
        child.tgid = parent.tgid;
        child.ppid = parent.ppid;
    } else {
        child.ppid = parent.tgid;
    }
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
    let tid = child.pid;
    if flags & CLONE_PARENT_SETTID != 0 {
        parent.pt().copy_out(ptid, &tid.to_le_bytes());
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        child.pt().copy_out(ctid, &tid.to_le_bytes());
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        child.clear_tid = ctid;
    }
    child.state = State::Runnable;
    Ok(tid)
}

pub fn execve(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), i64> {
    let img = exec::exec(path, argv, envp)?;
    let p = current();
    // setuid / setgid のプログラムなら euid / egid (と保存された id) が変わる
    if let Some(u) = img.setuid {
        p.cred.euid = u;
    }
    if let Some(g) = img.setgid {
        p.cred.egid = g;
    }
    p.cred.suid = p.cred.euid;
    p.cred.sgid = p.cred.egid;
    // 他のスレッドは消える
    for t in p.siblings() {
        kill_proc(t);
    }
    for h in p.sig_handlers.iter_mut() {
        if *h != SIG_IGN {
            *h = 0;
        }
    }
    let old = p.mm.take();
    p.load_image(img);
    p.pt().activate();
    drop(old);
    // files を他と共有していたなら切り離す
    let files = p.files().clone();
    p.files = Some(Shared::new(files));
    for f in p.files().fds.iter_mut() {
        if f.as_ref().is_some_and(|f| f.cloexec) {
            *f = None;
        }
    }
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
            if c.state == State::Unused || c.thread || c.ppid != me.tgid || (pid > 0 && c.pid as i64 != pid) {
                continue;
            }
            have = true;
            if c.state == State::Zombie {
                let r = (c.pid, c.xstatus);
                *c = Proc::UNUSED;
                return Ok(r);
            }
        }
        if !have {
            return Err(-ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok((0, 0));
        }
        // 親が待つのは代表スレッドのアドレス
        let leader = find(me.tgid).map_or(me as *mut Proc as usize, |l| l as *mut Proc as usize);
        sleep(leader)?;
    }
}

/// tid 宛てのシグナル。まだハンドラは呼べないので、無視されていなければグループごと終える
pub fn kill_thread(tid: u32, sig: i32) -> Result<(), i64> {
    const ESRCH: i64 = 3;
    let me = current();
    let target = find(tid).ok_or(-ESRCH)?;
    // root でなければ、実 uid か実効 uid が相手の実 uid か保存された uid と同じときだけ
    let (a, b) = (&me.cred, &target.cred);
    if a.euid != 0 && a.uid != b.uid && a.uid != b.suid && a.euid != b.uid && a.euid != b.suid {
        return Err(-crate::cred::EPERM);
    }
    if sig <= 0 || sig as usize >= NSIG {
        return if sig == 0 { Ok(()) } else { Err(-22) };
    }
    if target.sig_handlers[sig as usize] == SIG_IGN || sig == 17 || sig == 28 {
        // 無視、または既定で無視される SIGCHLD / SIGWINCH
        return Ok(());
    }
    if target.tgid == me.tgid {
        die(sig);
    }
    let leader = find(target.tgid).ok_or(-ESRCH)?;
    leader.group_exit.get_or_insert(sig & 0x7f);
    kill_proc(leader);
    for t in procs().iter_mut().filter(|p| p.state != State::Unused && p.tgid == leader.tgid) {
        kill_proc(t);
    }
    Ok(())
}
