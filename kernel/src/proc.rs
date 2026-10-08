// プロセス・スレッドとスケジューラ
//
// カーネルの中では割り込みを止めたまま、大きなロック (smp.rs) を持って動く。切り替えが起きるのは
// EL0 からのタイマ割り込みと、sleep/yield/exit のときだけ。CPU ごとにスケジューラがあり、
// 同じ Proc の表から Runnable のものを取って走らせる (次にどの CPU で走るかは決まっていない)。
// スレッドは mm (アドレス空間) と files を共有する Proc。
use crate::exec::{self, Image};
use crate::file::{self, FileRef, Kind};
use crate::trap::TrapFrame;
use crate::vm::PageTable;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

/// プロセスとスレッドの数の上限 (スレッドも 1 つずつ使う。Firefox のように数十のスレッドを持つものがある)
pub const NPROC: usize = 512;
/// 開けるファイルの数の上限 (RLIMIT_NOFILE のハードの上限)。fd の表は使う分だけ伸びる
pub const NOFILE: usize = 65536;
/// はじめのソフトの上限 (Linux と同じ 1024。setrlimit で NOFILE まで上げられる)
pub const NOFILE_SOFT: usize = 1024;
const KSTACK_SIZE: usize = 16 * 1024;
pub const MMAP_BASE: usize = 0x10_0000_0000;

pub const EINTR: i64 = 4;
pub use crate::signal::{NSIG, SIGKILL};
use crate::signal::{AltStack, PosixTimer, SigInfo, SigTable};

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

/// 複数の Proc から使う持ち物。カーネルの中は大きなロックで 1 つの CPU だけなので、同時には触られない
pub struct Shared<T>(Rc<UnsafeCell<T>>);

impl<T> Shared<T> {
    pub fn new(v: T) -> Self {
        Self(Rc::new(UnsafeCell::new(v)))
    }
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    /// ほかに持っている人がいない (ほかのスレッドと分けていない)
    pub fn private(&self) -> bool {
        Rc::strong_count(&self.0) == 1
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
    /// /proc/PID/exe
    pub exe: String,
    /// 引数と環境の文字列の場所 (arg_start, env_start, env_end)
    pub args: (usize, usize, usize),
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
    /// chroot のルート (先頭 / なし、"" は本当のルート。fork と exec で受けつぐ)
    pub root: String,
    /// RLIMIT_NOFILE のソフトの上限 (fork と exec で受けつぐ)
    pub nofile: usize,
    /// 作るファイルの mode から外すもの (umask。fork で受けつぐ。Linux の fs_struct と同じくスレッドで共有)
    pub umask: u32,
}

impl Files {
    pub fn get(&self, fd: u64) -> Option<&FileRef> {
        self.fds.get(fd as usize)?.as_ref().map(|f| &f.file)
    }

    /// minfd 以上で空いている一番小さい fd に置く
    pub fn add(&mut self, file: FileRef, cloexec: bool, minfd: usize) -> Option<usize> {
        let i = (minfd..self.nofile).find(|&i| self.fds.get(i).is_none_or(|f| f.is_none()))?;
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
pub struct FpState {
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

impl FpState {
    const ZERO: Self = Self { q: [0; 32], fpcr: 0, fpsr: 0 };
}

pub struct Proc {
    pub state: State,
    /// 走っている (最後に走った) CPU
    pub cpu: usize,
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
    /// シグナルのハンドラ (スレッドグループで共有)
    pub sigacts: Option<Shared<SigTable>>,
    /// たまっているシグナルと、ブロックしているシグナル (ビット sig-1)
    pub sig_pending: u64,
    pub sig_mask: u64,
    pub sig_info: [SigInfo; NSIG],
    /// sigsuspend で一時的に変える前のマスク
    pub saved_mask: Option<u64>,
    /// rt_sigtimedwait で待っているシグナル (ブロックされていても起こす)
    pub sigwait: u64,
    pub altstack: AltStack,
    /// プロセスグループとセッション
    pub pgid: u32,
    pub sid: u32,
    /// システムコールに入ったときの x0 (やり直し用)
    pub orig_x0: u64,
    /// setitimer の (期限, 間隔) と POSIX タイマー (代表スレッドに置く)
    pub itimer: (u64, u64),
    pub timers: Vec<PosixTimer>,
    /// 親が SIGCHLD を無視しているので、終わったらすぐ片付ける
    autoreap: bool,
    /// ユーザーとグループ
    pub cred: crate::cred::Cred,
    /// プログラムの名前 (/proc/PID/stat や ps に出る。最大 15 バイト)
    pub comm: [u8; 16],
    /// 代表でないスレッド。親は wait せず、終わったらスケジューラが片付ける
    thread: bool,
    /// 終了時に 0 を書いて futex で起こす場所 (CLONE_CHILD_CLEARTID)
    pub clear_tid: usize,
    /// ジョブ制御 (代表スレッドだけが使う): 止められているか、
    /// wait (WUNTRACED / WCONTINUED) にまだ知らせていない停止のシグナル / 再開
    pub stopped: bool,
    pub stop_report: i32,
    pub cont_report: bool,
    /// ユーザーモードで動いた時間 (tick)。代表スレッドは終わったスレッドの分も持つ
    pub utime: u64,
    /// 最近走った tick (走るたびに 1 足し、1 秒ごとに半分にする)。少ないものから走らせる
    pub recent: u32,
    /// sched_yield で順番をゆずった (次の pick で後ろに回す。選ばれたら戻す)
    pub yielded: bool,
    /// いま走りはじめてから来たタイマの割り込みの数。タイムスライス (sysctl kernel.sched_timeslice_ms) に
    /// 着いたらゆずる
    slice: u32,
    /// 回収した子 (とその子孫) の utime
    pub cutime: u64,
    /// 作った tick (/proc/PID/stat の starttime)
    pub start: u64,
    /// PID の namespace (ns.rs。None ははじめのもの) と、その中の番号
    pub pid_ns: Option<alloc::rc::Rc<crate::ns::Pid>>,
    pub vpid: u32,
    chan: usize,
    /// 最後に呼んだシステムコールの番号と最初の 2 つの引数 (/proc/threads で見る)
    pub last_sys: (u64, u64, u64),
    /// 最後の例外 (pc、アドレス、ESR の EC)。シグナルで落ちたときに出す
    pub last_fault: (u64, u64, u64),
    /// 最後に落ちたときの ESR (調べもの用: WnR、CM など)
    pub last_esr: u64,
    pub last_lr: u64,
    /// そのときの x0, x1, x19 (落ちたときに、文字列なら出す)
    pub last_regs: [u64; 3],
    /// 最後に落ちたときの sp (調べもの用)
    pub last_sp: u64,
    /// この tick になったら起こす (0 なら無し)
    wake_at: u64,
    /// poll で眠っているとき、起こしてほしいものの印 (None は何でも)
    poll_keys: Option<Vec<usize>>,
    context: Context,
    tpidr: u64,
    fp: FpState,
}

impl Proc {
    const UNUSED: Self = Self {
        state: State::Unused,
        cpu: 0,
        pid: 0,
        tgid: 0,
        ppid: 0,
        mm: None,
        files: None,
        xstatus: 0,
        killed: false,
        group_exit: None,
        sigacts: None,
        sig_pending: 0,
        sig_mask: 0,
        sig_info: [SigInfo::ZERO; NSIG],
        saved_mask: None,
        sigwait: 0,
        altstack: AltStack::NONE,
        pgid: 0,
        sid: 0,
        orig_x0: 0,
        itimer: (0, 0),
        timers: Vec::new(),
        autoreap: false,
        cred: crate::cred::Cred::ROOT,
        comm: [0; 16],
        thread: false,
        clear_tid: 0,
        stopped: false,
        stop_report: 0,
        cont_report: false,
        utime: 0,
        start: 0,
        pid_ns: None,
        vpid: 0,
        recent: 0,
        yielded: false,
        slice: 0,
        cutime: 0,
        chan: 0,
        last_sys: (0, 0, 0),
        last_fault: (0, 0, 0),
        last_esr: 0,
        last_lr: 0,
        last_regs: [0; 3],
        last_sp: 0,
        wake_at: 0,
        poll_keys: None,
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
    pub fn set_comm(&mut self, name: &[u8]) {
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len()).min(15);
        self.comm = [0; 16];
        self.comm[..n].copy_from_slice(&name[..n]);
    }

    pub fn comm(&self) -> &str {
        let n = self.comm.iter().position(|&c| c == 0).unwrap_or(16);
        core::str::from_utf8(&self.comm[..n]).unwrap_or("?")
    }

    pub fn tf(&mut self) -> &mut TrapFrame {
        unsafe { &mut *((self.kstack_top() - size_of::<TrapFrame>()) as *mut TrapFrame) }
    }

    fn load_image(&mut self, img: Image) {
        self.mm = Some(Shared::new(Mm { pt: img.pagetable, heap_start: img.brk, brk: img.brk, mmap_next: MMAP_BASE, exe: img.exe, args: img.args }));
        let tf = self.tf();
        *tf = TrapFrame::zeroed();
        tf.elr = img.entry as u64;
        tf.sp_el0 = img.sp as u64;
        tf.spsr = 0; // EL0t, 割り込み許可
    }

    /// 同じスレッドグループの他の Proc (リーダーも)
    pub fn siblings(&self) -> impl Iterator<Item = &'static mut Proc> {
        let (tgid, pid) = (self.tgid, self.pid);
        live().filter(move |p| p.state != State::Unused && p.tgid == tgid && p.pid != pid)
    }
}

#[repr(C, align(16))]
struct KStacks([[u8; KSTACK_SIZE]; NPROC]);

static mut KSTACKS: KStacks = KStacks([[0; KSTACK_SIZE]; NPROC]);
static mut PROCS: [Proc; NPROC] = [const { Proc::UNUSED }; NPROC];
/// CPU ごと: いま走らせている Proc の添字と、スケジューラの文脈
static mut CURRENT: [Option<usize>; crate::smp::MAXCPU] = [None; crate::smp::MAXCPU];
static mut SCHEDULER: [Context; crate::smp::MAXCPU] = [Context::ZERO; crate::smp::MAXCPU];

fn cur() -> Option<usize> {
    unsafe { CURRENT[crate::smp::id()] }
}

fn set_cur(v: Option<usize>) {
    unsafe { CURRENT[crate::smp::id()] = v };
}

fn sched_ctx() -> *mut Context {
    unsafe { &raw mut SCHEDULER[crate::smp::id()] }
}
static mut NEXT_PID: u32 = 1;

fn procs() -> &'static mut [Proc; NPROC] {
    unsafe { &mut *(&raw mut PROCS) }
}

// 使っているかもしれないスロットの印 (1 ビットが 1 つ)。見て回るところ (起こす、選ぶ、探す) が NPROC ぜんぶを
// 見ないように。alloc_proc でつけ、free_slot で消す (つけたまま Unused のものは、見るときに state でとばす)
const WORDS: usize = NPROC / 64;
static mut LIVE: [u64; WORDS] = [0; WORDS];

fn slot_of(p: &Proc) -> usize {
    (p as *const Proc as usize - (&raw const PROCS) as usize) / size_of::<Proc>()
}

/// スロットを空ける
fn free_slot(p: &mut Proc) {
    let i = slot_of(p);
    unsafe { LIVE[i / 64] &= !(1u64 << (i % 64)) };
    *p = Proc::UNUSED;
}

struct Bits {
    base: usize,
    bits: u64,
}

impl Iterator for Bits {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        if self.bits == 0 {
            return None;
        }
        let b = self.bits.trailing_zeros() as usize;
        self.bits &= self.bits - 1;
        Some(self.base + b)
    }
}

/// 印のついたスロットの番号を start から順に (おしまいまで行ったら 0 から start の手前まで)
fn live_from(start: usize) -> impl Iterator<Item = usize> {
    let start = start % NPROC;
    (0..=WORDS).flat_map(move |k| {
        let w = (start / 64 + k) % WORDS;
        let mut bits = unsafe { LIVE[w] };
        let low = (1u64 << (start % 64)) - 1;
        if k == 0 {
            bits &= !low;
        } else if k == WORDS {
            bits &= low;
        }
        Bits { base: w * 64, bits }
    })
}

/// 使っている Proc (Unused でないもの)
fn live() -> impl Iterator<Item = &'static mut Proc> {
    live_from(0).map(|i| &mut procs()[i]).filter(|p| p.state != State::Unused)
}

pub fn current() -> &'static mut Proc {
    let i = cur().expect("no current proc");
    &mut procs()[i]
}

/// いまのプロセスの資格情報。まだプロセスがなければ root
pub fn current_cred() -> crate::cred::Cred {
    match cur() {
        Some(i) => procs()[i].cred.clone(),
        None => crate::cred::Cred::ROOT,
    }
}

/// exec が使う cwd。user_init のときはまだ current がないのでルート
pub fn current_cwd() -> String {
    match cur() {
        Some(i) => procs()[i].files().cwd.clone(),
        None => String::new(),
    }
}

/// chroot のルート。current がないときは本当のルート
pub fn current_root() -> String {
    match cur() {
        Some(i) => procs()[i].files().root.clone(),
        None => String::new(),
    }
}

pub fn nprocs() -> usize {
    live().filter(|p| p.state != State::Unused).count()
}

/// 使われているアドレス空間ごとに 1 回 f (スレッドで共有しているものも 1 回)
pub fn each_pagetable(mut f: impl FnMut(&mut PageTable)) {
    let mut seen: Vec<usize> = Vec::new();
    for p in live() {
        let Some(mm) = p.mm.as_ref().filter(|_| p.state != State::Unused) else { continue };
        if seen.contains(&mm.id()) {
            continue;
        }
        seen.push(mm.id());
        f(&mut mm.get().pt);
    }
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

// 新しいプロセスは swtch からここへ戻り、大きなロックを放し、TrapFrame を戻して EL0 へ
.global forkret
forkret:
    bl      forkret_unlock
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
    let Some(p) = procs().iter_mut().find(|p| p.state == State::Unused) else {
        println!("proc: no free slot (NPROC = {})", NPROC);
        return None;
    };
    *p = Proc::UNUSED;
    let i = slot_of(p);
    unsafe { LIVE[i / 64] |= 1u64 << (i % 64) };
    unsafe {
        p.pid = NEXT_PID;
        NEXT_PID += 1;
    }
    p.tgid = p.pid;
    p.start = crate::timer::ticks();
    p.context.x19_x30[11] = forkret as *const () as u64; // x30 (lr)
    p.context.sp = (p.kstack_top() - size_of::<TrapFrame>()) as u64;
    Some(p)
}

pub fn user_init() {
    // Linux と同じく、カーネルのコマンドラインの init= で最初のプログラムを選べる (init=/bin/sh など)
    let init = crate::dtb::arg("init").unwrap_or("/init");
    let argv = [init.as_bytes().to_vec()];
    let envp = [b"HOME=/".to_vec(), b"PATH=/usr/bin:/bin".to_vec(), b"TERM=vt100".to_vec()];
    let img = match exec::exec(init, &argv, &envp) {
        Ok(img) => img,
        Err(e) => panic!("user_init: cannot exec {} ({})", init, e),
    };
    if init != "/init" {
        println!("init: {}", init);
    }
    let p = alloc_proc().expect("user_init: no proc slot");
    p.load_image(img);
    let mut files = Files { fds: Vec::new(), cwd: String::new(), root: String::new(), nofile: NOFILE_SOFT, umask: 0o022 };
    let console = file::new(Kind::Tty(crate::tty::console()), 2);
    for _ in 0..3 {
        files.add(console.clone(), false, 0);
    }
    p.files = Some(Shared::new(files));
    p.sigacts = Some(crate::signal::new_table());
    p.set_comm(init.rsplit('/').next().unwrap_or("init").as_bytes());
    p.pgid = p.pid;
    p.sid = p.pid;
    p.state = State::Runnable;
}

#[unsafe(no_mangle)]
extern "C" fn forkret_unlock() {
    crate::smp::unlock();
}

/// 次に走らせるもの: 実行できるもののうち recent が一番少ないもの (同じなら start から順に見て先のもの)。
/// ふだん眠っているもの (aiwm、シェル、入力を待つもの) は recent が少ないので、起きるとすぐ走る。
/// CPU を使い続けるもの (llvmpipe、ビルド) どうしは、recent が増えては減るので順番に回る
fn pick(start: usize) -> Option<usize> {
    let mut best: Option<(u32, usize)> = None;
    for i in live_from(start) {
        let p = &mut procs()[i];
        if p.state == State::Zombie && (p.thread || p.autoreap) {
            // 終わったスレッドは誰も wait しないのでここで片付ける
            free_slot(p);
            continue;
        }
        if p.state != State::Runnable {
            continue;
        }
        // ゆずったものは 1 秒ぶん後ろに (スピンして待つ相手を先に走らせる)
        let key = p.recent + if p.yielded { crate::timer::HZ as u32 } else { 0 };
        if best.is_none_or(|(r, _)| key < r) {
            best = Some((key, i));
            if key == 0 {
                break;
            }
        }
    }
    best.map(|(_, i)| i)
}

/// 1 秒ごと (cpu0 の tick) に recent を半分にする。5 秒ごとに load average を進める
pub fn decay_recent() {
    for p in live() {
        p.recent /= 2;
    }
    static SECS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if SECS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 5 == 4 {
        calc_load();
    }
}

const MAXCPU: usize = 16;

/// CPU ごとの、プロセスが走っていた tick (/proc/stat。残りは休んでいた)
pub static BUSY: [core::sync::atomic::AtomicU64; MAXCPU] = [const { core::sync::atomic::AtomicU64::new(0) }; MAXCPU];

/// load average (1, 5, 15 分。Linux と同じ 11 ビットの固定小数点、5 秒ごとに指数で平均)
pub static LOAD: [core::sync::atomic::AtomicU64; 3] = [const { core::sync::atomic::AtomicU64::new(0) }; 3];

/// いちばん新しく渡した PID
pub fn last_pid() -> u32 {
    unsafe { NEXT_PID - 1 }
}

/// スレッドの数 (/proc/loadavg)
pub fn nr_threads() -> usize {
    live().filter(|p| p.state != State::Unused).count()
}

/// 走っているか、走れるスレッドの数
pub fn nr_running() -> usize {
    live().filter(|p| matches!(p.state, State::Running | State::Runnable)).count()
}

fn calc_load() {
    use core::sync::atomic::Ordering::Relaxed;
    const FIXED_1: u64 = 1 << 11;
    const EXP: [u64; 3] = [1884, 2014, 2037];
    let active = nr_running() as u64 * FIXED_1;
    for (l, e) in LOAD.iter().zip(EXP) {
        let old = l.load(Relaxed);
        let mut new = old * e + active * (FIXED_1 - e);
        if active >= old {
            new += FIXED_1 - 1;
        }
        l.store(new / FIXED_1, Relaxed);
    }
}

/// 次の pick を始める場所 (同じ recent のものを順番に回すため。CPU みんなで使う)
static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 実行できるプロセスを選んで走らせ続ける (CPU ごと。大きなロックを持って呼ぶ)
pub fn scheduler() -> ! {
    use core::sync::atomic::Ordering::Relaxed;
    loop {
        let mut ran = false;
        if let Some(i) = pick(NEXT.load(Relaxed)) {
            NEXT.store((i + 1) % NPROC, Relaxed);
            let p = &mut procs()[i];
            p.state = State::Running;
            p.yielded = false;
            p.slice = 0;
            // 前の印は、選びなおしたので要らない
            take_resched();
            p.cpu = crate::smp::id();
            unsafe {
                set_cur(Some(i));
                p.pt().activate();
                fp_load(&p.fp);
                core::arch::asm!("msr tpidr_el0, {}", in(reg) p.tpidr);
                swtch(sched_ctx(), &p.context);
                set_cur(None);
                // 終わったプロセスのページ表はほかの CPU が片付けるかもしれないので、外しておく
                crate::vm::deactivate();
            }
            ran = true;
        }
        if !ran {
            // 書き残しがあれば書いてから (1 秒ごと、cpu0 で)、ロックを放して割り込みを待つ
            if crate::smp::id() == 0 {
                crate::vfs::idle_sync();
            }
            // 眠ると印をつけてから確かめる (そのあとに起こす CPU は、印を見て割り込みを送る)
            crate::smp::set_idle(true);
            if live().any(|p| p.state == State::Runnable) {
                crate::smp::set_idle(false);
                continue;
            }
            crate::smp::unlock();
            crate::timer::idle(true);
            // 割り込みを止めたまま wfi で眠る (止めていても、来ている割り込みがあれば起きる)。
            // 先に割り込みを開けると、そのすきに来た IPI をここで受けてしまい、そのあとの wfi で
            // 仕事があるのに眠りこむ (ほかの CPU が起こしたのに起きない)
            unsafe { core::arch::asm!("dsb sy", "wfi") };
            // 起こした割り込みをここで受ける
            crate::trap::intr_on();
            unsafe { core::arch::asm!("isb") };
            crate::trap::intr_off();
            crate::timer::idle(false);
            crate::smp::set_idle(false);
            crate::smp::lock();
        }
    }
}

/// スケジューラへ戻る。state は呼ぶ側が変えておく
fn sched() {
    debug_assert!(crate::smp::holding(), "sched without the big kernel lock");
    crate::smp::SWITCHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let p = current();
    unsafe {
        fp_save(&mut p.fp);
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) p.tpidr);
        swtch(&mut p.context, sched_ctx());
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
    if p.killed || crate::signal::deliverable(p) {
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
    if p.killed || crate::signal::deliverable(p) {
        return Err(-EINTR);
    }
    Ok(!timed_out)
}

/// chan で眠っている Proc を起こし、起こした数を返す
pub fn wakeup(chan: usize) -> usize {
    wake_where(|p| p.chan == chan)
}

/// 眠っているもののうち f に合うものを起こす。眠っている CPU があれば起こし、なければ
/// 走っているもののうち一番 CPU を使っている (recent) ものにゆずらせる (preempt)
fn wake_where(f: impl Fn(&Proc) -> bool) -> usize {
    let mut n = 0;
    let mut woken = u32::MAX;
    let mut busiest: Option<(u32, usize)> = None;
    for p in live() {
        if p.state == State::Sleeping && f(p) {
            p.state = State::Runnable;
            woken = woken.min(p.recent);
            n += 1;
        } else if p.state == State::Running && busiest.is_none_or(|(r, _)| p.recent > r) {
            busiest = Some((p.recent, p.cpu));
        }
    }
    if n > 0 && !crate::smp::wake_idle() {
        if let Some((r, cpu)) = busiest {
            preempt(woken, r, cpu);
        }
    }
    n
}

/// CPU ごとの「EL0 へ戻るときにゆずる」印 (起こされたものを、タイムスライスの終わりまで待たせない)
static RESCHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 起こしたもの (recent が woken) が、cpu で走っているもの (recent が running) より 2 tick 以上
/// 使っていなければ、cpu にゆずらせる。ふだん眠っているもの (音、入力、aiwm) が、計算し続けるものに
/// 割りこめる。sysctl kernel.sched_wakeup_preempt で止められる
fn preempt(woken: u32, running: u32, cpu: usize) {
    use core::sync::atomic::Ordering;
    if crate::sysctl::SCHED_WAKEUP_PREEMPT.load(Ordering::Relaxed) == 0 || running < woken.saturating_add(2) {
        return;
    }
    RESCHED.fetch_or(1 << cpu, Ordering::AcqRel);
    crate::smp::kick(cpu);
    crate::smp::PREEMPTS.fetch_add(1, Ordering::Relaxed);
}

/// この CPU にゆずる印がついていたら消して true (EL0 へ戻る前に見る)
pub fn take_resched() -> bool {
    let bit = 1 << crate::smp::id();
    RESCHED.load(core::sync::atomic::Ordering::Relaxed) & bit != 0 && RESCHED.fetch_and(!bit, core::sync::atomic::Ordering::AcqRel) & bit != 0
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
    println!("ticks {} current {:?}", crate::timer::ticks(), cur());
}

static POLL: u8 = 0;

/// poll で待っている人が眠る channel。何かの状態が変わったらここを起こす
pub fn poll_chan() -> usize {
    (&raw const POLL) as usize
}

/// poll / select で眠る: keys (見張っているものの印) のどれかが poll_wake されるか、
/// 何でも起こす wakeup(poll_chan) か、deadline まで
pub fn poll_sleep(keys: Option<Vec<usize>>, deadline: u64) -> Result<bool, i64> {
    current().poll_keys = keys;
    let r = sleep_until(poll_chan(), deadline);
    current().poll_keys = None;
    r
}

/// key (パイプや端末など) が変わった: それを見張って poll で眠っているものだけを起こす
pub fn poll_wake(key: usize) {
    let chan = poll_chan();
    wake_where(|p| p.chan == chan && p.poll_keys.as_ref().is_none_or(|k| k.contains(&key)));
}

/// タイマから: 期限の来た Proc を起こす
pub fn wake_expired(now: u64) {
    wake_where(|p| p.wake_at != 0 && p.wake_at <= now);
}

fn kill_proc(p: &mut Proc) {
    p.killed = true;
    interrupt(p);
}

fn find(pid: u32) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.pid == pid)
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
    crate::ns::init_exited(p);
    clear_child_tid(p);
    if p.thread {
        // 使った時間は代表スレッドに持たせる
        let t = core::mem::take(&mut p.utime);
        if let Some(l) = find(p.tgid) {
            l.utime += t;
        }
        let p = current();
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
    for c in live() {
        if c.state != State::Unused && c.ppid == p.tgid && !c.thread {
            c.ppid = 1;
        }
    }
    p.files = None;
    p.timers.clear();
    p.xstatus = p.group_exit.unwrap_or(status);
    p.state = State::Zombie;
    if let Some(parent) = find(p.ppid) {
        if crate::signal::parent_reaps_automatically(parent) {
            p.autoreap = true;
        } else {
            let (code, st) = if p.xstatus & 0x7f != 0 { (crate::signal::CLD_KILLED, p.xstatus & 0x7f) } else { (crate::signal::CLD_EXITED, (p.xstatus >> 8) & 0xff) };
            let info = SigInfo { code, pid: p.tgid, uid: p.cred.uid, status: st, ..SigInfo::ZERO };
            let ppid = p.ppid;
            let _ = crate::signal::send_group(ppid, crate::signal::SIGCHLD, info);
        }
        if let Some(parent) = find(current().ppid) {
            wakeup(parent as *mut Proc as usize);
        }
    }
    if let Some(init) = find(1) {
        wakeup(init as *mut Proc as usize);
    }
    // pidfd を poll / epoll しているもの
    wakeup(poll_chan());
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
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// fork / スレッド作成。CLONE_THREAD でない CLONE_VM (vfork) はアドレス空間をコピーする
/// unshare: 自分のものを分ける。CLONE_NEW* (ns.rs) と CLONE_FILES (ファイルの表)。CLONE_FS、CLONE_SYSVSEM は
/// もう分かれているので何もしない
pub fn unshare(flags: u64) -> Result<i64, i64> {
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_SYSVSEM: u64 = 0x0004_0000;
    if flags & !(crate::ns::SUPPORTED | CLONE_FILES | CLONE_FS | CLONE_SYSVSEM) != 0 {
        return Err(-22);
    }
    let p = current();
    crate::ns::renew(&mut p.cred, flags)?;
    if flags & CLONE_FILES != 0 {
        let f = Shared::new(p.files().clone());
        p.files = Some(f);
    }
    Ok(0)
}

pub fn clone(flags: u64, stack: usize, ptid: usize, tls: u64, ctid: usize) -> Result<u32, i64> {
    const EAGAIN: i64 = 11;
    const ENOMEM: i64 = 12;
    const EINVAL: i64 = 22;
    let thread = flags & CLONE_THREAD != 0;
    if thread && flags & CLONE_VM == 0 {
        return Err(-EINVAL);
    }
    let parent = current();
    // CLONE_NEW*: 子の namespace を新しく (ns.rs。スレッドには作れない)
    if thread && flags & crate::ns::SUPPORTED != 0 {
        return Err(-EINVAL);
    }
    let mut cred = parent.cred.clone();
    crate::ns::renew(&mut cred, flags)?;
    let mm = if thread {
        parent.mm.clone().unwrap()
    } else {
        let m = parent.mm();
        let pt = m.pt.fork().ok_or(-ENOMEM)?;
        Shared::new(Mm { pt, heap_start: m.heap_start, brk: m.brk, mmap_next: m.mmap_next, exe: m.exe.clone(), args: m.args })
    };
    let files = if flags & CLONE_FILES != 0 && thread {
        parent.files.clone().unwrap()
    } else {
        Shared::new(parent.files().clone())
    };
    let child = alloc_proc().ok_or(-EAGAIN)?;
    child.mm = Some(mm);
    child.files = Some(files);
    child.sigacts = if flags & CLONE_SIGHAND != 0 {
        parent.sigacts.clone()
    } else {
        parent.sigacts.as_ref().map(|t| crate::signal::copy_table(t))
    };
    child.sig_mask = parent.sig_mask;
    child.pgid = parent.pgid;
    child.sid = parent.sid;
    if !thread {
        child.altstack = parent.altstack;
    }
    // PID の namespace: スレッドは親と同じ、プロセスは親がこれから作る子のもの (unshare / CLONE_NEWPID)
    child.pid_ns = if thread { parent.pid_ns.clone() } else { cred.ns.pid.clone().or_else(|| parent.pid_ns.clone()) };
    if let Some(ns) = &child.pid_ns {
        child.vpid = ns.alloc();
    }
    child.cred = cred;
    child.comm = parent.comm;
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
    crate::smp::wake_idle();
    Ok(tid)
}

pub fn execve(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), i64> {
    let img = exec::exec(path, argv, envp)?;
    let p = current();
    // setuid / setgid のプログラムなら euid / egid (と保存された id) が変わる。no_new_privs なら変えない
    // (砂場の中から sudo で外へ出られないように)
    if let Some(u) = img.setuid.filter(|_| !p.cred.no_new_privs) {
        p.cred.euid = u;
    }
    if let Some(g) = img.setgid.filter(|_| !p.cred.no_new_privs) {
        p.cred.egid = g;
    }
    p.cred.suid = p.cred.euid;
    p.cred.sgid = p.cred.egid;
    p.set_comm(path.rsplit('/').next().unwrap_or(path).as_bytes());
    // 他のスレッドは消える
    for t in p.siblings() {
        kill_proc(t);
    }
    // 呼ばれるハンドラは既定に戻す (無視はそのまま)。他のスレッドとは切り離す
    if let Some(t) = p.sigacts.as_ref() {
        let mut table = *t.get();
        for a in table.iter_mut() {
            if a.handler != crate::signal::SIG_IGN {
                *a = crate::signal::SigAction::DFL;
            }
        }
        p.sigacts = Some(Shared::new(table));
    }
    p.altstack = AltStack::NONE;
    p.timers.clear();
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
const WUNTRACED: u64 = 2;
const WCONTINUED: u64 = 8;
/// waitid の WNOWAIT: 知らせるだけで、回収しない (状態もそのまま)
pub const WNOWAIT: u64 = 0x100_0000;
/// 終わった子を知らせる (wait4 はいつも。waitid は WEXITED のときだけ)
pub const WEXITED: u64 = 4;

/// 子の終了 (と WUNTRACED なら停止、WCONTINUED なら再開) を待つ。(pid, status) を返す。
/// pid: > 0 はその子、0 は同じプロセスグループ、-1 はどれでも、< -1 はグループ -pid
/// 子を待つ。返す番号は、待つほうから見たもの (PID の namespace の中なら中の番号。片づける前に読む)
pub fn wait(pid: i64, options: u64) -> Result<(u32, i32), i64> {
    const ECHILD: i64 = 10;
    let me = current();
    let (my_tgid, my_pgid) = (me.tgid, me.pgid);
    loop {
        let mut have = false;
        for c in live() {
            if c.state == State::Unused || c.thread || c.ppid != my_tgid {
                continue;
            }
            let wanted = match pid {
                p if p > 0 => c.pid as i64 == p,
                0 => c.pgid == my_pgid,
                -1 => true,
                p => c.pgid as i64 == -p,
            };
            if !wanted {
                continue;
            }
            have = true;
            let keep = options & WNOWAIT != 0;
            if c.state == State::Zombie && options & WEXITED != 0 {
                let r = (crate::ns::to_local(c.pid).unwrap_or(c.pid), c.xstatus);
                if !keep {
                    let t = c.utime + c.cutime;
                    free_slot(c);
                    if let Some(me) = find(my_tgid) {
                        me.cutime += t;
                    }
                }
                return Ok(r);
            }
            if options & WUNTRACED != 0 && c.stop_report != 0 {
                let sig = c.stop_report;
                if !keep {
                    c.stop_report = 0;
                }
                return Ok((crate::ns::to_local(c.pid).unwrap_or(c.pid), (sig << 8) | 0x7f));
            }
            if options & WCONTINUED != 0 && c.cont_report {
                if !keep {
                    c.cont_report = false;
                }
                return Ok((crate::ns::to_local(c.pid).unwrap_or(c.pid), 0xffff));
            }
        }
        if !have {
            return Err(-ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok((0, 0));
        }
        // 親が待つのは代表スレッドのアドレス
        let me = current();
        let leader = find(me.tgid).map_or(me as *mut Proc as usize, |l| l as *mut Proc as usize);
        sleep(leader)?;
    }
}

/// タイマの割り込みから: いま動いているプロセスに 1 tick つける。
/// カーネルの中では割り込みを止めているので、動いていたのはユーザーモード
pub fn account_tick() {
    let c = crate::smp::id().min(MAXCPU - 1);
    if let Some(i) = cur() {
        BUSY[c].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let p = &mut procs()[i];
        p.utime += 1;
        p.recent = p.recent.saturating_add(1);
        p.slice = p.slice.saturating_add(1);
    }
}

/// タイマの割り込みから: いま動いているものがタイムスライスを使いきったか (ゆずるか)
pub fn slice_expired() -> bool {
    let ticks = crate::sysctl::SCHED_TIMESLICE_TICKS.load(core::sync::atomic::Ordering::Relaxed) as u32;
    cur().is_some_and(|i| procs()[i].slice >= ticks)
}

/// sched_yield: 順番をゆずる (同じくらい走ったものより後ろに)
pub fn yield_voluntary() {
    current().yielded = true;
    yield_now();
}

/// スレッドグループ全体の utime (tick)
pub fn group_utime(tgid: u32) -> u64 {
    live().filter(|p| p.state != State::Unused && p.tgid == tgid).map(|p| p.utime).sum()
}

/// pidfd: そのプロセスが終わった (ゾンビか、もういない) か
pub fn has_exited(pid: u32) -> bool {
    !live().any(|p| p.pid == pid && !p.thread && p.state != State::Unused && p.state != State::Zombie)
}

/// 止められたスレッドグループの再開を待つ (SIGCONT か SIGKILL まで)。
/// 止まっている間に来た他のシグナルは、再開してから届ける
pub fn stop_while_stopped() {
    loop {
        let p = current();
        if p.killed {
            return;
        }
        let Some(l) = find(p.tgid) else { return };
        if !l.stopped {
            return;
        }
        p.chan = stop_chan(l);
        p.state = State::Sleeping;
        sched();
        current().chan = 0;
    }
}

pub fn stop_chan(leader: &Proc) -> usize {
    leader as *const Proc as usize + 1
}

/// 眠っているスレッドをシグナルで起こす (sleep は EINTR で戻る)
pub fn interrupt(t: &mut Proc) {
    match t.state {
        State::Sleeping => {
            t.state = State::Runnable;
            crate::smp::wake_idle();
        }
        // ほかの CPU のユーザーモードで走っている: すぐに戻ってきてもらう
        State::Running => crate::smp::kick(t.cpu),
        _ => {}
    }
}

/// スレッドグループ全体を終わらせる (SIGKILL)
pub fn kill_group(tgid: u32, sig: i32) {
    if let Some(leader) = find(tgid) {
        leader.group_exit.get_or_insert(sig & 0x7f);
    }
    for t in live().filter(|p| p.state != State::Unused && p.state != State::Zombie && p.tgid == tgid) {
        kill_proc(t);
    }
}

pub fn threads_of(tgid: u32) -> Vec<&'static mut Proc> {
    live().filter(|p| p.state != State::Unused && p.state != State::Zombie && p.tgid == tgid).collect()
}

/// 終わったもの (ゾンビ) も
pub fn find_any(pid: u32) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.pid == pid)
}

pub fn find_where(f: impl Fn(&Proc) -> bool) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.state != State::Zombie && f(p))
}

pub fn find_thread(tid: u32) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.state != State::Zombie && p.pid == tid)
}

pub fn find_leader(tgid: u32) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.state != State::Zombie && p.pid == tgid && !p.thread)
}

pub fn all_leaders() -> Vec<u32> {
    live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.thread).map(|p| p.tgid).collect()
}

pub fn all_leader_procs() -> Vec<&'static mut Proc> {
    live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.thread).collect()
}

pub fn leaders_in_pgrp(pgid: u32) -> Vec<u32> {
    live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.thread && p.pgid == pgid).map(|p| p.tgid).collect()
}

pub fn current_leader() -> &'static mut Proc {
    let tgid = current().tgid;
    find_leader(tgid).unwrap_or_else(current)
}

/// (tgid, 実 uid): siginfo の送り主
pub fn current_ids() -> (u32, u32) {
    match cur() {
        Some(i) => (procs()[i].tgid, procs()[i].cred.uid),
        None => (0, 0),
    }
}

/// いまの EL0 の FP/SIMD レジスタ (ハードウェアにあるもの) を写す / 戻す
pub fn fp_snapshot() -> FpState {
    let mut f = FpState::ZERO;
    unsafe { fp_save(&mut f) };
    f
}

pub fn fp_restore(f: &FpState) {
    unsafe { fp_load(f) };
}

/// /proc/threads: すべてのスレッドの pid、tgid、状態、待っているもの、最後のシステムコール (調べもの用)
/// /proc/PID/stack (調べもの用): プロセスのスレッドごとに、EL0 の pc と lr、フレームポインタをたどった戻り先、
/// それからスタック (sp から 32 KiB) の中で写したファイル (実行できるところ) を指す値を「ファイル+位置」で
pub fn stacks_text(tgid: u32) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for i in 0..NPROC {
        let p = &procs()[i];
        if p.state == State::Unused || p.state == State::Zombie || p.tgid != tgid {
            continue;
        }
        let tf = p.tf_ref();
        let (pc, lr, fp, sp) = (tf.elr, tf.x[30], tf.x[29], tf.sp_el0);
        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
        let st = match p.state {
            State::Running => "run",
            State::Runnable => "ready",
            State::Sleeping => "sleep",
            _ => "other",
        };
        let _ = writeln!(out, "thread {} {} sys {} chan {:x} {} utime {}", p.pid, core::str::from_utf8(&p.comm[..n]).unwrap_or("?"), p.last_sys.0, p.chan, st, p.utime);
        let pt = p.pt();
        let name = |pt: &mut crate::vm::PageTable, va: u64| -> Option<alloc::string::String> {
            let (_, v) = pt.find(va as usize)?;
            if v.prot & 4 == 0 {
                return None;
            }
            let (f, off) = pt.name_at(va as usize)?;
            Some(alloc::format!("{}+{:#x}", f.rsplit('/').next().unwrap_or(&f), off))
        };
        // futex で待っているなら、そのアドレスと、待つときの値と、いまの値
        if p.last_sys.0 == 98 && p.state == State::Sleeping {
            let mut w = [0u8; 4];
            let now = pt.copy_in(&mut w, tf.x[0] as usize).map(|_| u32::from_le_bytes(w) as i64).unwrap_or(-1);
            let _ = writeln!(out, "  futex {:#x} op {} val {} now {}", tf.x[0], tf.x[1], tf.x[2] as u32, now);
        }
        for (what, va) in [("pc", pc), ("lr", lr)] {
            let _ = writeln!(out, "  {} {:#x} {}", what, va, name(pt, va).unwrap_or_default());
        }
        // フレームポインタ: [fp] = 前の fp、[fp + 8] = 戻り先
        let mut f = fp;
        for _ in 0..48 {
            if f == 0 || f & 7 != 0 {
                break;
            }
            let mut b = [0u8; 16];
            if pt.copy_in(&mut b, f as usize).is_none() {
                break;
            }
            let next = u64::from_le_bytes(b[..8].try_into().unwrap());
            let ret = u64::from_le_bytes(b[8..].try_into().unwrap());
            let _ = writeln!(out, "  fp {:#x} {}", ret, name(pt, ret).unwrap_or_default());
            if next <= f {
                break;
            }
            f = next;
        }
        // スタックの中の、コードを指す値
        let mut buf = alloc::vec![0u8; 32 * 1024];
        let mut got = 0;
        while got < buf.len() {
            let chunk = 4096 - ((sp as usize + got) & 4095);
            let chunk = chunk.min(buf.len() - got);
            if pt.copy_in(&mut buf[got..got + chunk], sp as usize + got).is_none() {
                break;
            }
            got += chunk;
        }
        let mut shown = 0;
        for w in buf[..got].chunks_exact(8) {
            let va = u64::from_le_bytes(w.try_into().unwrap());
            if let Some(nm) = name(pt, va) {
                let _ = writeln!(out, "  scan {:#x} {}", va, nm);
                shown += 1;
                if shown >= 80 {
                    break;
                }
            }
        }
    }
    for (tid, a, op, n) in crate::syscall::futex_wakes(tgid) {
        let _ = writeln!(out, "wake by {} {:#x} op {} woke {}", tid, a, op, n);
    }
    // 開いている fd の様子 (ソケットに残っているバイト、epoll の見張り)
    if let Some(l) = find_leader(tgid)
        && let Some(files) = l.files.as_ref()
    {
        for (n, fd) in files.get().fds.iter().enumerate() {
            if let Some(fd) = fd {
                let _ = writeln!(out, "fd {} {}", n, fd.file.borrow().debug_state());
            }
        }
    }
    out
}

pub fn threads_text() -> alloc::string::String {
    let mut out = alloc::format!("ticks {}\n  PID  TGID ST CHAN             SYSCALL ARG0             ARG1                 WAKE NAME\n", crate::timer::ticks());
    for p in live() {
        if p.state == State::Unused {
            continue;
        }
        let st = match p.state {
            State::Running | State::Runnable => 'R',
            State::Sleeping => 'S',
            State::Zombie => 'Z',
            State::Unused => 'X',
        };
        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
        out.push_str(&alloc::format!(
            "{:5} {:5} {}  {:16x} {:7} {:16x} {:16x} {:8} {}\n",
            p.pid,
            p.tgid,
            st,
            p.chan,
            p.last_sys.0,
            p.last_sys.1,
            p.last_sys.2,
            p.wake_at,
            core::str::from_utf8(&p.comm[..n]).unwrap_or("?")
        ));
    }
    out
}
