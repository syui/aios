// プロセス・スレッドとスケジューラ
//
// カーネルの中では割り込みを止めたまま、大きなロック (smp.rs) を持って動く。切り替えが起きるのは
// EL0 からのタイマ割り込みと、sleep/yield/exit のときだけ。CPU ごとにスケジューラがあり、
// 同じ Proc の表から Runnable のものを取って走らせる (次にどの CPU で走るかは決まっていない)。
// スレッドは mm (アドレス空間) と files を共有する Proc。
use crate::cell::{Bkl, PerCpu, Raw};
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

/// カーネルのスタックの全部のバイト (/proc/ai/mem)
pub fn kstack_bytes() -> usize {
    NPROC * KSTACK_SIZE
}
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
    // [lockless]
    pub fn private(&self) -> bool {
        Rc::strong_count(&self.0) == 1
    }

    pub fn id(&self) -> usize {
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
    /// 動かしているプログラム (ELF) の inode (書くために開くと ETXTBSY: executing)
    pub exe_id: (usize, u64),
    /// 引数と環境の文字列の場所 (arg_start, env_start, env_end)
    pub args: (usize, usize, usize),
    /// PR_GET_DUMPABLE / PR_SET_DUMPABLE (0 作らない、1 作る、2 root の権利で)。exec で 1 (setuid / setgid のもので
    /// id が変わったら 0: Linux の suid_dumpable の既定)。fork で引き継ぐ。PIDFD_GET_INFO の coredump の印と、ptrace の許し
    pub dumpable: u8,
}

impl Mm {
    /// 作る (exe_id の箱を数える: maybe_executing)
    fn shared(m: Mm) -> Shared<Mm> {
        if let Some(b) = exe_box(m.exe_id) {
            b.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        Shared::new(m)
    }
}

impl Drop for Mm {
    fn drop(&mut self) {
        if let Some(b) = exe_box(self.exe_id) {
            b.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
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
    /// 大きなロックなしの read / write (file::Pipe::fast_rw) が、ほかのスレッドと分けている表を見るための門
    pub gate: Gate,
    /// 閉じたふつうのファイルの棚 (file::Shelf。大きなロックなしの open / close が使う。fork と exec では受けつがない)
    pub shelf: crate::file::Shelf,
    /// この表 (POSIX のロックの持ち主) が fcntl のレコードロックを置いたことがある: 閉じるたびに外すものがあるかもしれない
    /// ので、大きなロックなしの close (file::fast_close) はしない
    pub posix_locked: bool,
}

impl Drop for Files {
    fn drop(&mut self) {
        // プロセス (スレッドの組) の終わり: その POSIX のレコードロックをみな外す
        crate::file::locks().release_owner(self as *const Files as usize);
    }
}

/// 表を大きなロックなしで見ている数 (users) と、変えている数 (mutators)。見る側は users を増やしてから
/// mutators を見て、0 でなければやめる。変える側 (大きなロックの中) は mutators を増やしてから users が 0 に
/// なるのを待つ。どちらかがかならず相手に気づくので、見ているあいだに fd が閉じられて OpenFile が消えたり、
/// 表が伸びて場所が変わったりしない (vm.rs の fast_fault と同じ形)
#[derive(Default)]
pub struct Gate {
    users: core::sync::atomic::AtomicUsize,
    mutators: core::sync::atomic::AtomicUsize,
}

impl Clone for Gate {
    // 写した表は別の表なので、数は 0 から
    fn clone(&self) -> Self {
        Gate::default()
    }
}

/// 変えているあいだ (落とすと mutators を戻す)
pub struct GateWrite(*const Gate);

impl Drop for GateWrite {
    fn drop(&mut self) {
        unsafe { (*self.0).mutators.fetch_sub(1, core::sync::atomic::Ordering::SeqCst) };
    }
}

/// 大きなロックなしで見ているあいだ (落とすと users を戻す)
pub struct GateRead<'a>(&'a Gate);

impl Drop for GateRead<'_> {
    fn drop(&mut self) {
        self.0.users.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

impl Gate {
    /// 変える (大きなロックの中で)。見ている人がいなくなるまで待つ
    fn write(&self) -> GateWrite {
        use core::sync::atomic::{Ordering::SeqCst, fence};
        self.mutators.fetch_add(1, SeqCst);
        fence(SeqCst);
        while self.users.load(SeqCst) != 0 {
            core::hint::spin_loop();
        }
        GateWrite(self)
    }

    /// 大きなロックなしで見る。だれかが変えている途中なら None (ふつうの道へ)
    pub fn read(&self) -> Option<GateRead<'_>> {
        use core::sync::atomic::{Ordering::SeqCst, fence};
        self.users.fetch_add(1, SeqCst);
        fence(SeqCst);
        if self.mutators.load(SeqCst) != 0 {
            self.users.fetch_sub(1, SeqCst);
            return None;
        }
        Some(GateRead(self))
    }
}

impl Files {
    /// fd i を v にする (表を伸ばすことも)。もとのものを返す。表を変えるのはこれと add だけ (gate のため)
    pub fn set(&mut self, i: usize, v: Option<Fd>) -> Option<Fd> {
        let old = {
            let _w = self.gate.write();
            if self.fds.len() <= i {
                if v.is_none() {
                    return None;
                }
                self.fds.resize(i + 1, None);
            }
            core::mem::replace(&mut self.fds[i], v)
        };
        // fd を閉じた (上書きも): そのファイルの、このプロセスの POSIX のレコードロックはみな外れる (locks.rs)
        if let Some(fd) = &old {
            crate::file::locks().release_posix(self as *const Files as usize, &fd.file.borrow());
        }
        // 閉じたあとの片づけ (AF_UNIX なら、送られている途中の輪が残ったかもしれない: file.rs から unix.rs の gc。
        // システムコールの終わりに)
        if let Some(fd) = &old {
            crate::file::closed(&fd.file.borrow().kind);
        }
        old
    }

    pub fn get(&self, fd: u64) -> Option<&FileRef> {
        self.fds.get(fd as usize)?.as_ref().map(|f| &f.file)
    }

    /// ほかのプロセスの表から写す (procfs、pidfd_getfd)。持ち主は大きなロックなしで fd を置いたり棚へしまったり
    /// (file::fast_open、fast_close) するので、棚のロックの中で
    pub fn get_other(&self, fd: u64) -> Option<(FileRef, bool)> {
        let _s = self.shelf.hold();
        self.fds.get(fd as usize)?.as_ref().map(|f| (f.file.clone(), f.cloexec))
    }

    /// minfd 以上で空いている一番小さい fd に置く
    pub fn add(&mut self, file: FileRef, cloexec: bool, minfd: usize) -> Option<usize> {
        let i = (minfd..self.nofile).find(|&i| self.fds.get(i).is_none_or(|f| f.is_none()))?;
        self.set(i, Some(Fd { file, cloexec }));
        Some(i)
    }

    /// fd i に置く (あったものは閉じる: dup2 と同じ。Linux の replace_fd: RLIMIT_NOFILE をこえれば EBADF)
    pub fn put_at(&mut self, i: usize, file: FileRef, cloexec: bool) -> Result<usize, i64> {
        if i >= self.nofile {
            return Err(-9);
        }
        self.set(i, Some(Fd { file, cloexec }));
        Ok(i)
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

/// Proc::cred_work の印
pub const CRED_SECCOMP: u8 = 1;
pub const CRED_SANDBOX: u8 = 2;

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
    /// cred の seccomp と landlock の層 (sandbox) があるかの印 (CRED_SECCOMP、CRED_SANDBOX)。この 2 つはほかのスレッドが
    /// 書くことがある (TSYNC) ので、大きなロックなしの道 (syscall::fast、file::fast_open、fastdir) は cred を読まずにこれを
    /// 見る (Linux の TIF_SECCOMP と同じく atomic に立てる)。cred のそれを書いたら sync_cred_work で
    pub cred_work: core::sync::atomic::AtomicU8,
    /// プログラムの名前 (/proc/PID/stat や ps に出る。最大 15 バイト)
    pub comm: [u8; 16],
    /// 投機的なストアの追いこし (SSB) の印 (prctl の PR_SET_SPECULATION_CTRL: cpuvuln::SSB_*)。fork で受けつぐ
    pub spec_ssb: u8,
    /// trace の名前の絞りに合うかの覚え: (絞りの版 << 1 | 合う)。版が trace::FILTER_GEN と同じあいだ使う (trace::wanted)
    pub trace_mark: u64,
    /// 代表でないスレッド。親は wait せず、終わったらスケジューラが片付ける
    thread: bool,
    /// カーネルの中だけで動くスレッド (kthread: 書き出しの flush)。ユーザー空間へ戻らず、シグナルを受けない
    pub kthread: bool,
    /// 書き出しのスレッドが追いつかないほど書いた: システムコールの終わりで待つ (vfs::balance_dirty)
    pub dirtied: bool,
    /// 取れなくても耐える区間 (heap::fallible) の深さ: この中でヒープが取れなければ null (ENOMEM)。予備は使わない
    pub fallible: u32,
    /// 終了時に 0 を書いて futex で起こす場所 (CLONE_CHILD_CLEARTID)
    pub clear_tid: usize,
    /// set_robust_list の頭 (struct robust_list_head のユーザーの番地。0 はなし)。終わるときと exec で
    /// futex::exit_robust_list がたどる。fork の子は 0 から (Linux の futex_init_task)
    pub robust_list: usize,
    /// システムコールの監査の入れもの (audit.rs の Ctx: audit を一度でも on にしたあとに作ったスレッドだけ)
    pub audit: Option<alloc::boxed::Box<dyn core::any::Any>>,
    /// はじめて走るとき (forkret)、大きなロックを放してから自分の tid を書く場所 (CLONE_CHILD_SETTID。Linux の
    /// schedule_tail と同じ。fork の子では書くと COW になるので、前は親の clone が大きなロックを持ったまま写していた)
    set_child_tid: usize,
    /// CLONE_VFORK で作られた子: exec か終わりまで待っている親の pid (なければ 0)
    pub vfork_parent: u32,
    /// 代表でないスレッドが execve した (代表スレッドに置く): 読みこんだプログラムと新しい comm。
    /// 代表スレッドが check_killed でそれになって続ける (Linux と同じく PID はそのまま、親から見て終わらない)
    exec_takeover: Option<(Image, String)>,
    /// fork の前半: 大きなロックの外 (syscall::fast) で PTE を写しておいた表。その例外の中で clone が使うか、
    /// 使わなければ trap.rs が捨てる (drop_prefork)
    pub prefork: Option<crate::vm::Prefork>,
    /// execve を引きついだばかり: 割り込まれた前のシステムコールをやり直さない (signal::deliver が見る)
    pub no_restart: bool,
    /// ジョブ制御 (代表スレッドだけが使う): 止められているか、
    /// wait (WUNTRACED / WCONTINUED) にまだ知らせていない停止のシグナル / 再開
    pub stopped: bool,
    pub stop_report: i32,
    pub cont_report: bool,
    /// fork してから exec した (Linux の PF_FORKNOEXEC が消えた): 親はもう setpgid で動かせない (EACCES)
    pub execed: bool,
    /// ユーザーモードで動いた時間 (tick)。代表スレッドは終わったスレッドの分も持つ
    pub utime: u64,
    /// CPU で走った時間 (ns: ユーザーもカーネルも。Linux の sum_exec_runtime)。スケジューラが走らせるたびに足す。
    /// CLOCK_THREAD_CPUTIME_ID と CPU の時計 (timer.rs の clock_ns)
    pub cpu_ns: u64,
    /// いま走りはじめた時刻 (uptime の ns。走っていなければ 0)
    pub ran_at: u64,
    /// 代表スレッドだけ: 終わったスレッドの cpu_ns (CLOCK_PROCESS_CPUTIME_ID に足す)
    pub cpu_dead_ns: u64,
    /// cpu_ns のうちカーネルで走った時間 (ns: Linux の stime。utime は cpu_ns - sys_ns)。EL0 からの例外の入口から、EL0 に
    /// もどるか切りかわるまで (trap.rs の kernel_enter / kernel_leave)
    pub sys_ns: u64,
    /// いまカーネルで走りはじめた時刻 (uptime の ns。ユーザーで走っているか、走っていなければ 0)
    pub kern_at: u64,
    /// 代表スレッドだけ: 終わったスレッドの sys_ns
    pub sys_dead_ns: u64,
    /// 代表スレッドだけ: 回収した子 (とその子孫) の走った時間とカーネルの時間 (ns: RUSAGE_CHILDREN、tms_cutime / cstime)
    pub child_cpu_ns: u64,
    pub child_sys_ns: u64,
    /// 最近走った tick (走るたびに 1 足し、1 秒ごとに半分にする)。少ないものから走らせる
    pub recent: u32,
    /// 走らせた回数 (/proc/threads。調べもの用)
    pub nrun: u64,
    /// 眠りはじめた tick (/proc/ai/threads の slept_s。どれだけ眠りつづけているか)
    pub slept_at: u64,
    /// 0 でなければ、いまは「ディスクを待つあいだ眠ってよい」読むだけの道の中 (file::read の ext4 のファイル、
    /// pread)。extfs の with_block がキャッシュにないブロックを読むとき、回らずに眠る (io_sleepable)
    pub io_sleep: u32,
    /// 0 でなければ、いまは「書き出し (ext4 の flush) がディスクを眠って待ってよい」ところ (fsync、sync: システムコールの
    /// いちばん外。何も借りていない)。flush_sleepable
    pub flush_sleep: u32,
    /// シグナルでは起こさない眠りの中 (sleep_nointr: ディスクの要求の終わりを待つ。Linux の TASK_UNINTERRUPTIBLE、ps の D)
    pub nointr: bool,
    /// SIGKILL でだけ起きる眠りの中 (sleep_killable: seccomp の WAIT_KILLABLE_RECV。Linux の TASK_KILLABLE)
    pub killable: bool,
    /// ディスクを読まない区間 (preadv2 の RWF_NOWAIT): キャッシュになければ EAGAIN (extfs の with_block)
    pub noio: u32,
    /// 新しく読んだものをキャッシュに残さない区間 (RWF_DONTCACHE)
    pub dontcache: u32,
    /// 起こされた時刻 (ns、走りだしたら 0)。起こされてから走るまで (/proc/bkl の「起こされてから走るまで」)
    woken_ns: u64,
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
    pub chan: usize,
    /// 最後に呼んだシステムコールの番号と最初の 2 つの引数 (/proc/threads で見る)
    pub last_sys: (u64, u64, u64),
    /// いまのシステムコールがしくじったわけ (why.rs。/proc/ai/why)
    pub why: crate::why::Note,
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
    pub wake_at: u64,
    /// ソケットの受けとりで眠るときの締め切り (tick、0 なら無し。recvmmsg の timeout: socket::mmsg がおく)
    pub recv_deadline: u64,
    /// 眠りの待ち行列のつなぎ (chan のもの、期限つきのもの)
    wq: Link,
    tm: Link,
    /// 走れるものの並びのつなぎ (RUNQ)
    rq: Link,
    /// 子 (fork したプロセス。スレッドは入らない) の並びの先頭と末尾のスロット。代表スレッドが持つ (Linux の
    /// children)。wait4 と、終わるときに子を init にわたすのは、これだけを見る (前は生きているものをみな見ていた)
    kids: (u16, u16),
    /// 親 (代表スレッド) の子の並びのつなぎ (Linux の sibling と real_parent)
    sib: Sib,
    /// poll で眠っているとき、起こしてほしいものの印 (None は何でも)
    pub poll_keys: Option<Vec<usize>>,
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
        cred_work: core::sync::atomic::AtomicU8::new(0),
        comm: [0; 16],
        spec_ssb: 0,
        trace_mark: 0,
        thread: false,
        kthread: false,
        dirtied: false,
        fallible: 0,
        clear_tid: 0,
        robust_list: 0,
        audit: None,
        set_child_tid: 0,
        vfork_parent: 0,
        exec_takeover: None,
        prefork: None,
        no_restart: false,
        stopped: false,
        stop_report: 0,
        cont_report: false,
        execed: false,
        utime: 0,
        cpu_ns: 0,
        ran_at: 0,
        cpu_dead_ns: 0,
        sys_ns: 0,
        kern_at: 0,
        sys_dead_ns: 0,
        child_cpu_ns: 0,
        child_sys_ns: 0,
        start: 0,
        pid_ns: None,
        vpid: 0,
        recent: 0,
        nrun: 0,
        slept_at: 0,
        io_sleep: 0,
        nointr: false,
        killable: false,
        flush_sleep: 0,
        noio: 0,
        dontcache: 0,
        woken_ns: 0,
        yielded: false,
        slice: 0,
        cutime: 0,
        chan: 0,
        last_sys: (0, 0, 0),
        why: crate::why::NONE,
        last_fault: (0, 0, 0),
        last_esr: 0,
        last_lr: 0,
        last_regs: [0; 3],
        last_sp: 0,
        wake_at: 0,
        recv_deadline: 0,
        wq: Link::NONE,
        tm: Link::NONE,
        rq: Link::NONE,
        kids: (NONE, NONE),
        sib: Sib::NONE,
        poll_keys: None,
        context: Context::ZERO,
        tpidr: 0,
        fp: FpState::ZERO,
    };

    // [lockless]
    pub fn mm(&self) -> &mut Mm {
        self.mm.as_ref().expect("proc without mm").get()
    }

    // [lockless]
    pub fn pt(&self) -> &mut PageTable {
        &mut self.mm().pt
    }

    /// スレッドグループの代表 (スレッドでない) か
    // [lockless]
    pub fn is_leader(&self) -> bool {
        !self.thread && self.pid == self.tgid
    }

    pub fn files(&self) -> &mut Files {
        self.files.as_ref().expect("proc without files").get()
    }

    /// futex の channel を作るための、アドレス空間ごとの値
    // [lockless]
    pub fn mm_id(&self) -> usize {
        self.mm.as_ref().map_or(0, |m| m.id())
    }

    fn slot(&self) -> usize {
        (self as *const Proc as usize - PROCS.ptr() as usize) / size_of::<Proc>()
    }

    fn kstack_top(&self) -> usize {
        KSTACKS.ptr() as usize + (self.slot() + 1) * KSTACK_SIZE
    }

    pub fn tf_ref(&self) -> &TrapFrame {
        unsafe { &*((self.kstack_top() - size_of::<TrapFrame>()) as *const TrapFrame) }
    }

    /// EL0 から入ってきたときの TrapFrame はいつもカーネルスタックの天辺にある
    pub fn set_comm(&mut self, name: &[u8]) {
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len()).min(15);
        self.comm = [0; 16];
        self.comm[..n].copy_from_slice(&name[..n]);
        crate::debug::trace_name(self.pid, &self.comm);
        // 代表の名前が変わったかもしれない: trace の絞りの覚えを古くする
        crate::debug::trace_names_changed();
    }

    pub fn comm(&self) -> &str {
        let n = self.comm.iter().position(|&c| c == 0).unwrap_or(16);
        core::str::from_utf8(&self.comm[..n]).unwrap_or("?")
    }

    pub fn tf(&mut self) -> &mut TrapFrame {
        unsafe { &mut *((self.kstack_top() - size_of::<TrapFrame>()) as *mut TrapFrame) }
    }

    fn load_image(&mut self, img: Image) {
        self.execed = true;
        self.mm = Some(Mm::shared(Mm { pt: img.pagetable, heap_start: img.brk, brk: img.brk, mmap_next: MMAP_BASE, exe: img.exe, exe_id: img.exe_id, args: img.args, dumpable: 1 }));
        let tf = self.tf();
        *tf = TrapFrame::zeroed();
        tf.elr = img.entry as u64;
        tf.sp_el0 = img.sp as u64;
        tf.spsr = 0; // EL0t, 割り込み許可
        // PR_SPEC_DISABLE_NOEXEC は exec で外れる。PSTATE.SSBS は印から
        crate::cpuvuln::exec(&mut self.spec_ssb);
        let ssb = self.spec_ssb;
        crate::cpuvuln::task_spsr(&mut self.tf().spsr, ssb);
    }

    /// 同じスレッドグループの他の Proc (リーダーも)
    /// cred の seccomp と sandbox を書いたあと (大きなロックの中で): cred_work をそれに合わせる
    pub fn sync_cred_work(&self) {
        let w = if self.cred.seccomp.is_some() { CRED_SECCOMP } else { 0 } | if self.cred.sandbox.is_some() { CRED_SANDBOX } else { 0 };
        self.cred_work.store(w, core::sync::atomic::Ordering::Release);
    }

    /// cred_work の印 (大きなロックなしの道から)
    #[inline]
    // [lockless]
    pub fn has_cred_work(&self, bit: u8) -> bool {
        self.cred_work.load(core::sync::atomic::Ordering::Acquire) & bit != 0
    }

    pub fn siblings(&self) -> impl Iterator<Item = &'static mut Proc> {
        let (tgid, pid) = (self.tgid, self.pid);
        live().filter(move |p| p.state != State::Unused && p.tgid == tgid && p.pid != pid)
    }
}

#[repr(C, align(16))]
struct KStacks([[u8; KSTACK_SIZE]; NPROC]);

/// スレッドごとのカーネルのスタック (番地を切りかえと例外の入口に渡すだけ。中はそのスレッドが使う)
static KSTACKS: Raw<KStacks> = Raw::new(KStacks([[0; KSTACK_SIZE]; NPROC]));
/// プロセスとスレッドの表 (procs())
static PROCS: Raw<[Proc; NPROC]> = Raw::new([const { Proc::UNUSED }; NPROC]);
/// CPU ごと: いま走らせている Proc の添字と、スケジューラの文脈
static CURRENT: PerCpu<Option<usize>> = PerCpu::new([None; crate::smp::MAXCPU]);
static SCHEDULER: PerCpu<Context> = PerCpu::new([Context::ZERO; crate::smp::MAXCPU]);

// [lockless]
pub fn cur() -> Option<usize> {
    *CURRENT.get()
}

/// いまのスレッドの表の番号 (0..NPROC。起動のときやアイドルなら None。prof.rs がスレッドごとに数えるのに)
// [lockless]
pub fn cur_index() -> Option<usize> {
    cur()
}

/// いまのスレッドの番号 (起動のときやアイドルなら 0。割り込みの中からも呼べる: trace.rs)
pub fn current_pid_or_zero() -> u32 {
    cur().map_or(0, |i| procs()[i].pid)
}

/// いまのプロセスの番号 (tgid。起動のときやアイドルなら 0: fail.rs)
pub fn current_tgid_or_zero() -> u32 {
    cur().map_or(0, |i| procs()[i].tgid)
}

/// いまのプロセス (代表スレッド) の名前が prefix で始まるか (起動のときやアイドルなら false: fail.rs)
pub fn current_comm_starts_with(prefix: &[u8]) -> bool {
    cur().is_some_and(|i| {
        let p = &procs()[i];
        let leader = if p.thread { find_leader(p.tgid) } else { None };
        leader.map_or(&p.comm[..], |l| &l.comm[..]).starts_with(prefix)
    })
}

/// いまのスレッドは、取れなくても耐える区間 (heap::fallible) の中か (起動のときやアイドルなら、耐えない)
pub fn in_fallible() -> bool {
    cur().is_some_and(|i| procs()[i].fallible > 0)
}

// [lockless]
pub fn fallible_enter() {
    if let Some(i) = cur() {
        procs()[i].fallible += 1;
    }
}

pub fn fallible_leave() {
    if let Some(i) = cur() {
        let p = &mut procs()[i];
        p.fallible = p.fallible.saturating_sub(1);
    }
}

/// 生きているプロセスとスレッドの fd の表をみな (分けているものは何度も。file の棚を捨てるときに)
pub fn each_files(mut f: impl FnMut(&mut Files)) {
    for p in live() {
        if p.state != State::Unused
            && let Some(files) = &p.files
        {
            f(files.get());
        }
    }
}

/// 生きているプロセス (代表スレッド) をみな (Vec を使わない: oom.rs は取れないときに呼ばれる)
pub fn each_leader(mut f: impl FnMut(&mut Proc)) {
    for p in live() {
        if p.state != State::Unused && p.state != State::Zombie && !p.thread {
            f(p);
        }
    }
}

fn set_cur(v: Option<usize>) {
    *CURRENT.get() = v;
}

fn sched_ctx() -> *mut Context {
    SCHEDULER.ptr()
}
static NEXT_PID: Bkl<u32> = Bkl::new(1);

/// 表は大きなロックが守る。ロックなしで触ってよいのは、いまのスレッドの Proc (current()) のうち、そのスレッドの
/// ほかにはロックを持っても変えないところ (fd の表を持つ Rc、アドレス空間、資格情報を読むなど: syscall::fast)。
/// Proc ごとに守り手がちがうので Bkl にはしない (Bkl にすると、速い道の current() がみな unlocked に出る)
// [lockless]
pub fn procs() -> &'static mut [Proc; NPROC] {
    unsafe { &mut *PROCS.ptr() }
}

// 使っているかもしれないスロットの印 (1 ビットが 1 つ)。見て回るところ (起こす、選ぶ、探す) が NPROC ぜんぶを
// 見ないように。alloc_proc でつけ、free_slot で消す (つけたまま Unused のものは、見るときに state でとばす)
const WORDS: usize = NPROC / 64;
static LIVE: Bkl<[u64; WORDS]> = Bkl::new([0; WORDS]);

fn slot_of(p: &Proc) -> usize {
    (p as *const Proc as usize - PROCS.ptr() as usize) / size_of::<Proc>()
}

/// fork の前半 (syscall::fast の prefork) を、その例外の中で clone が使わなかった (clone が早くにしくじった、わざと
/// しくじらせた): 捨てる (trap.rs。次の fork が古い写しを使わないように)
#[inline]
pub fn drop_prefork() {
    if let Some(i) = cur() {
        let p = &mut procs()[i];
        if p.prefork.is_some() {
            p.prefork = None;
        }
    }
}

/// スロットを空ける
fn free_slot(p: &mut Proc) {
    set_stopped(p, false);
    // seccomp の listener に、フィルタを使う人が減ったことを (使う人がいなくなれば POLLHUP)
    crate::security::seccomp_filter_release(&p.cred);
    // pidfd があれば、終わったときの status を (PIDFD_GET_INFO の PIDFD_INFO_EXIT)
    crate::pidfd::reaped(p.pid, p.xstatus);
    let i = slot_of(p);
    kid_unlink(i);
    // 子はふつう終わるときに init にわたしている (そのあとに、残っていたスレッドが fork したものも)
    reparent_kids(i);
    wq_leave(i);
    unlink(i, |p| &mut p.rq);
    LIVE.get()[i / 64] &= !(1u64 << (i % 64));
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
        let mut bits = LIVE.get()[w];
        let low = (1u64 << (start % 64)) - 1;
        if k == 0 {
            bits &= !low;
        } else if k == WORDS {
            bits &= low;
        }
        Bits { base: w * 64, bits }
    })
}

// ---- 眠りの待ち行列 ----
// Linux の wait queue のように、眠っているものだけを並べておく。前は起こすたびに、生きている Proc をみな見ていた
// (スレッドの多い Firefox で wakeup が 1 回 50 µs、タイマの割り込みのたびにも)。眠るときに自分を入れ (wq_enter)、
// 起きて走りだしたとき (sched から戻ったところ) に自分で出る (wq_leave)。起こす側は出さない (起こし方がいくつも
// ある: wakeup、シグナル、期限、kill)。なので並びには、起こされてまだ走っていないものもいる (state を見てとばす)

const NONE: u16 = u16::MAX;

/// 並びのつなぎ (Proc の添字。NONE で終わり)。head は入っている並び (HEADS の番号。NONE なら入っていない)
#[derive(Clone, Copy)]
struct Link {
    head: u16,
    next: u16,
    prev: u16,
}

impl Link {
    const NONE: Link = Link { head: NONE, next: NONE, prev: NONE };
}

/// chan のハッシュごとの、そこで眠っているものの並び (Proc::wq でつなぐ)。そのあとの TIMED は期限つきで眠っている
/// ものの並び (Proc::tm でつなぐ。タイマの wake_expired が見る)、RUNQ は走れるもの (Runnable) の並び (Proc::rq で
/// つなぐ。pick が見る: Linux の runqueue。前は pick も、生きている Proc をみな見ていた)
const NWQ: usize = 256;
const TIMED: usize = NWQ;
const RUNQ: usize = NWQ + 1;
static HEADS: Bkl<[u16; NWQ + 2]> = Bkl::new([NONE; NWQ + 2]);

fn wq_of(chan: usize) -> usize {
    ((chan as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize
}

fn link(i: usize, head: usize, l: fn(&mut Proc) -> &mut Link) {
    let heads = HEADS.get();
    let first = heads[head];
    *l(&mut procs()[i]) = Link { head: head as u16, next: first, prev: NONE };
    if first != NONE {
        l(&mut procs()[first as usize]).prev = i as u16;
    }
    heads[head] = i as u16;
}

fn unlink(i: usize, l: fn(&mut Proc) -> &mut Link) {
    let me = *l(&mut procs()[i]);
    if me.head == NONE {
        return;
    }
    if me.prev != NONE {
        l(&mut procs()[me.prev as usize]).next = me.next;
    } else {
        HEADS.get()[me.head as usize] = me.next;
    }
    if me.next != NONE {
        l(&mut procs()[me.next as usize]).prev = me.prev;
    }
    *l(&mut procs()[i]) = Link::NONE;
}

// ---- 子の並び ----
// 親 (代表スレッド) ごとの、fork した子の並び (Linux の children と sibling)。古い子から (wait4 は Linux と同じく古い
// 子から見る)。clone でつなぎ、free_slot で外し、親が終わるときは init の並びの末尾へ同じ順でうつす

/// 子の並びのつなぎ: parent は親のスロット、prev / next は兄弟のスロット (NONE で終わり)
#[derive(Clone, Copy)]
struct Sib {
    parent: u16,
    prev: u16,
    next: u16,
}

impl Sib {
    const NONE: Sib = Sib { parent: NONE, prev: NONE, next: NONE };
}

/// child を parent (代表スレッドのスロット) の子の並びの末尾に
fn kid_link(parent: usize, child: usize) {
    let ps = procs();
    let tail = ps[parent].kids.1;
    ps[child].sib = Sib { parent: parent as u16, prev: tail, next: NONE };
    if tail != NONE {
        ps[tail as usize].sib.next = child as u16;
    } else {
        ps[parent].kids.0 = child as u16;
    }
    ps[parent].kids.1 = child as u16;
}

/// child を親の子の並びから外す (入っていなければ何もしない)
fn kid_unlink(child: usize) {
    let ps = procs();
    let s = ps[child].sib;
    if s.parent == NONE {
        return;
    }
    let parent = s.parent as usize;
    if s.prev != NONE {
        ps[s.prev as usize].sib.next = s.next;
    } else {
        ps[parent].kids.0 = s.next;
    }
    if s.next != NONE {
        ps[s.next as usize].sib.prev = s.prev;
    } else {
        ps[parent].kids.1 = s.prev;
    }
    ps[child].sib = Sib::NONE;
}

/// me (スロット) の子をみな init にわたす (Linux の forget_original_parent): init の子の並びの末尾へ同じ順で。子の
/// スレッドの ppid も 1 に (getppid。Linux は子のスレッドみなの real_parent を変える)。終わっている (ゾンビの) 子は、
/// Linux と同じく init に SIGCHLD を送って、wait で眠っている init を起こす (ゾンビがいなければ init は起こさない)
fn reparent_kids(me: usize) {
    let ps = procs();
    let first = ps[me].kids.0;
    if first == NONE {
        return;
    }
    ps[me].kids = (NONE, NONE);
    let tgid = ps[me].tgid;
    for c in live() {
        if c.ppid == tgid && c.tgid != tgid {
            c.ppid = 1;
        }
    }
    let init = find(1).map(|p| slot_of(p));
    let mut zombie = false;
    let mut i = first;
    while i != NONE {
        let c = i as usize;
        let next = ps[c].sib.next;
        ps[c].sib = Sib::NONE;
        if let Some(init) = init {
            kid_link(init, c);
            if procs()[c].state == State::Zombie && !procs()[c].autoreap {
                zombie = true;
                if !crate::signal::parent_reaps_automatically(&procs()[init]) {
                    sigchld(&procs()[c], &procs()[init]);
                }
            }
        }
        i = next;
    }
    if let Some(init) = init.filter(|_| zombie) {
        wakeup(&mut procs()[init] as *mut Proc as usize);
    }
}

/// 終わった子 c のことを親 (代表スレッド) に SIGCHLD で知らせる
fn sigchld(c: &Proc, parent: &Proc) {
    let (code, st) = if c.xstatus & 0x7f != 0 { (crate::signal::CLD_KILLED, c.xstatus & 0x7f) } else { (crate::signal::CLD_EXITED, (c.xstatus >> 8) & 0xff) };
    let info = SigInfo { code, pid: c.tgid, uid: c.cred.uid, status: st, ..SigInfo::ZERO };
    crate::signal::send_chld(parent, info);
}

/// いまのスレッドを、chan (と期限があれば TIMED) の並びに入れる (Sleeping にしてから、sched の前に)
fn wq_enter() {
    let Some(i) = cur() else { return };
    link(i, wq_of(procs()[i].chan), |p| &mut p.wq);
    if procs()[i].wake_at != 0 {
        link(i, TIMED, |p| &mut p.tm);
    }
}

/// いまのスレッドが並びから出る (起きて sched から戻ったところで)
fn wq_exit() {
    if let Some(i) = cur() {
        wq_leave(i);
    }
}

/// i を並びから出る (wq_exit と、片づけるスロット)
fn wq_leave(i: usize) {
    unlink(i, |p| &mut p.wq);
    unlink(i, |p| &mut p.tm);
}

/// 走れるようにする (Runnable にして RUNQ に入れる)。Runnable にするのは、みなここから
fn make_runnable(p: &mut Proc) {
    p.state = State::Runnable;
    if p.rq.head == NONE {
        link(slot_of(p), RUNQ, |p| &mut p.rq);
    }
}

/// CPU ごとの、いま走らせている Proc の添字 + 1 (0 は何も)。起こしたものにゆずらせる相手を、Proc をみな見ずに選ぶ
static ON_CPU: [core::sync::atomic::AtomicUsize; crate::smp::MAXCPU] = [const { core::sync::atomic::AtomicUsize::new(0) }; crate::smp::MAXCPU];

/// 使っている Proc (Unused でないもの)
pub fn live() -> impl Iterator<Item = &'static mut Proc> {
    live_from(0).map(|i| &mut procs()[i]).filter(|p| p.state != State::Unused)
}

/// いまのプロセスのユーザーのメモリへ写す・から写す (Linux の copy_to_user と copy_from_user)。写せなければ EFAULT
pub fn put_user(va: usize, b: &[u8]) -> Result<(), i64> {
    current().pt().copy_out(va, b).ok_or(-14)
}

pub fn get_user(va: usize, b: &mut [u8]) -> Result<(), i64> {
    current().pt().copy_in(b, va).ok_or(-14)
}

// [lockless]
pub fn current() -> &'static mut Proc {
    let i = cur().expect("no current proc");
    &mut procs()[i]
}

/// いまのスレッド (起動のときやアイドルなら None)
// [lockless]
pub fn try_current() -> Option<&'static mut Proc> {
    cur().map(|i| &mut procs()[i])
}

/// いまのプロセスの資格情報。まだプロセスがなければ root
/// いまのプロセスの資格情報を写さずに (起動中でまだプロセスがなければ root)
// [lockless]
pub fn current_cred_ref() -> &'static crate::cred::Cred {
    // だれも書かない (Cred は Rc を持つので Sync でなく、ただの static にはできない。ROOT の Rc はみな None)
    static ROOT: crate::cell::Early<crate::cred::Cred> = crate::cell::Early::new(crate::cred::Cred::ROOT);
    match cur() {
        Some(i) => &procs()[i].cred,
        None => ROOT.get(),
    }
}

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

/// いまのプロセスのマウントの namespace (はじめのものなら None。起動中も)
pub fn current_mnt() -> Option<Rc<crate::vfs::MountNs>> {
    cur().and_then(|i| procs()[i].cred.ns.mnt.clone())
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

/// ファイル id をプログラムとして動かしているプロセスがあるか (書くために開く、切りつめるのは ETXTBSY: Linux の
/// deny_write_access。ファイルのページは 1 枚なので、書けば動いているコードが変わる)
pub fn executing(id: (usize, u64)) -> bool {
    live().any(|p| p.state != State::Unused && p.state != State::Zombie && p.mm.as_ref().is_some_and(|m| m.get().exe_id == id))
}

/// 動かしているプログラムの inode の数を、id のハッシュで 256 の箱に分けたもの (アドレス空間 Mm を作るときに足し、
/// 落とすときに引く。exe_id のないカーネルのスレッドは数えない)
static EXE_BOXES: [core::sync::atomic::AtomicU32; 256] = [const { core::sync::atomic::AtomicU32::new(0) }; 256];

fn exe_box(id: (usize, u64)) -> Option<&'static core::sync::atomic::AtomicU32> {
    (id != (0, 0)).then(|| &EXE_BOXES[((id.1 ^ (id.0 as u64).rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize])
}

/// 大きなロックなしの書く open (fastdir::fast_open): id をプログラムとして動かしているかもしれないか (箱が 0 でなければ。
/// そのときはふつうの道が executing で確かめる。Linux の i_writecount のように、プロセスをなめずに数を見るだけ)。
/// exec が、いま見たあとで箱を足すなら、開いたあとに動かしたのと同じ (書く口が開いていても exec はできる)
pub fn maybe_executing(id: (usize, u64)) -> bool {
    exe_box(id).is_some_and(|b| b.load(core::sync::atomic::Ordering::Relaxed) != 0)
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
    // 空きは LIVE の 0 のビット (free_slot が Proc::UNUSED に戻してから消す。一度も使っていないものは、はじめから
    // UNUSED で 0)。なので書きなおさない (前は NPROC の表を頭から見て、取ったものを UNUSED で書きなおしていた:
    // Proc は数 KiB)。印がついたまま Unused のもの (取ってすぐしくじったもの) は、空きがないときに表を見て使う
    let live = LIVE.get();
    let free = (0..WORDS).find_map(|w| (!live[w] != 0).then(|| w * 64 + (!live[w]).trailing_zeros() as usize));
    let p = match free.filter(|&i| i < NPROC && procs()[i].state == State::Unused) {
        Some(i) => &mut procs()[i],
        None => match procs().iter_mut().find(|p| p.state == State::Unused) {
            Some(p) => {
                *p = Proc::UNUSED;
                p
            }
            None => {
                println!("proc: no free slot (NPROC = {})", NPROC);
                return None;
            }
        },
    };
    let i = slot_of(p);
    crate::debug::prof_thread_start(i);
    LIVE.get()[i / 64] |= 1u64 << (i % 64);
    p.pid = *NEXT_PID.get();
    *NEXT_PID.get() += 1;
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
    let mut files = Files { fds: Vec::new(), cwd: String::new(), root: String::new(), nofile: NOFILE_SOFT, umask: 0o022, gate: Gate::default(), shelf: Default::default(), posix_locked: false };
    let console = file::new(Kind::Tty(crate::tty::console()), 2);
    for _ in 0..3 {
        files.add(console.clone(), false, 0);
    }
    p.files = Some(Shared::new(files));
    p.sigacts = Some(crate::signal::new_table());
    p.set_comm(init.rsplit('/').next().unwrap_or("init").as_bytes());
    p.pgid = p.pid;
    p.sid = p.pid;
    make_runnable(p);
}

/// カーネルの中だけで動くスレッドを作る (Linux の kthread_run)。main は swtch の ret からそのまま (forkret を通らない)、
/// スケジューラから受けとった大きなロックを持って走り、眠るあいだは放す (眠るシステムコールと同じ)。ユーザー空間へは
/// 戻らない。アドレス空間は空、fd はない。シグナルは受けない (signal::send_thread。Linux の kthread と同じく、送れても
/// 何も起きない)。親はいない (ppid 0: init の wait には出てこない。Linux では kthreadd の子)
pub fn kthread(name: &[u8], main: extern "C" fn() -> !) -> Option<u32> {
    let pt = PageTable::new()?;
    let p = alloc_proc()?;
    p.kthread = true;
    p.mm = Some(Mm::shared(Mm { pt, heap_start: 0, brk: 0, mmap_next: MMAP_BASE, exe: String::new(), exe_id: (0, 0), args: (0, 0, 0), dumpable: 1 }));
    p.files = Some(Shared::new(Files { fds: Vec::new(), cwd: String::new(), root: String::new(), nofile: NOFILE_SOFT, umask: 0o022, gate: Gate::default(), shelf: Default::default(), posix_locked: false }));
    p.sigacts = Some(crate::signal::new_table());
    p.set_comm(name);
    p.context.x19_x30[11] = main as usize as u64; // x30 (lr)
    let pid = p.pid;
    make_runnable(p);
    Some(pid)
}

#[unsafe(no_mangle)]
extern "C" fn forkret_unlock() {
    crate::smp::unlock();
    crate::vm::free_deferred();
    set_child_tid();
    // はじめてユーザーへ: 走りはじめ (スケジューラ) からのカーネルの時間を閉じる
    kernel_leave();
}

/// CLONE_CHILD_SETTID: はじめてユーザーへ戻る前に、自分の tid を書く (大きなロックなしで)。fork の子のそのページは
/// 親と分けている (COW) ので、ロックなしのフォールト (vm::PageTable::fast_fault) で写してから。それでも書けないもの
/// (ファイルの写し、スワップへ出したページ、だれかが表を変えている途中) だけ大きなロックを取る。書けなくても
/// しくじらない (Linux の schedule_tail の put_user と同じ)
fn set_child_tid() {
    let Some(i) = cur() else { return };
    let p = &mut procs()[i];
    let at = core::mem::take(&mut p.set_child_tid);
    if at == 0 || p.mm.is_none() {
        return;
    }
    let tid = p.pid.to_le_bytes();
    let pt = p.pt();
    if pt.copy_out_nofault(at, &tid) || (pt.fast_fault(at, true, false) && pt.copy_out_nofault(at, &tid)) {
        return;
    }
    crate::smp::lock();
    let _ = current().pt().copy_out(at, &tid);
    crate::smp::unlock();
}

/// 次に走らせるもの: 実行できるもののうち recent が一番少ないもの (同じなら start から順に見て先のもの)。
/// ふだん眠っているもの (aiwm、シェル、入力を待つもの) は recent が少ないので、起きるとすぐ走る。
/// CPU を使い続けるもの (llvmpipe、ビルド) どうしは、recent が増えては減るので順番に回る
/// 走れるもの (RUNQ) だけを見て、選んだものは RUNQ から出す
fn pick(start: usize) -> Option<usize> {
    // (recent, start からの順, 添字)
    let mut best: Option<(u32, usize, usize)> = None;
    let mut i = HEADS.get()[RUNQ];
    while i != NONE {
        let k = i as usize;
        let p = &procs()[k];
        i = p.rq.next;
        // ゆずったものは 1 秒ぶん後ろに (スピンして待つ相手を先に走らせる)
        let key = p.recent + if p.yielded { crate::timer::HZ as u32 } else { 0 };
        let order = (k + NPROC - start) % NPROC;
        if best.is_none_or(|(r, o, _)| (key, order) < (r, o)) {
            best = Some((key, order, k));
        }
    }
    let (_, _, k) = best?;
    unlink(k, |p| &mut p.rq);
    Some(k)
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
    *NEXT_PID.get() - 1
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
        let t0 = crate::timer::uptime_ns();
        if let Some(i) = pick(NEXT.load(Relaxed)) {
            NEXT.store((i + 1) % NPROC, Relaxed);
            let p = &mut procs()[i];
            p.state = State::Running;
            p.nrun += 1;
            crate::debug::trace_switch(p.pid, &p.comm);
            if p.woken_ns != 0 {
                crate::smp::dev_wait(3, p.woken_ns);
                p.woken_ns = 0;
            }
            p.yielded = false;
            p.slice = 0;
            // 前の印は、選びなおしたので要らない
            take_resched();
            p.cpu = crate::smp::id();
            unsafe {
                set_cur(Some(i));
                ON_CPU[crate::smp::id()].store(i + 1, Relaxed);
                p.pt().activate_lazy();
                fp_load(&p.fp);
                // ユーザーに戻るときの PSTATE.SSBS は、切りかえのたびにスレッドの印から (Linux の __switch_to)
                if !p.kthread {
                    let ssb = p.spec_ssb;
                    crate::cpuvuln::task_spsr(&mut p.tf().spsr, ssb);
                }
                core::arch::asm!("msr tpidr_el0, {}", in(reg) p.tpidr);
                crate::smp::dev_wait(2, t0);
                // 切りかえはいつもカーネルの中なので、走りはじめはカーネルの時間から
                let t = crate::timer::uptime_ns().max(1);
                (p.ran_at, p.kern_at) = (t, t);
                swtch(sched_ctx(), &p.context);
                // 走った時間とカーネルの時間を足す (終わったスレッドは、終わるときに代表へわたして 0 にしている)
                let q = &mut procs()[i];
                let now = crate::timer::uptime_ns();
                if q.ran_at != 0 {
                    q.cpu_ns += now.saturating_sub(q.ran_at);
                    q.ran_at = 0;
                }
                if q.kern_at != 0 {
                    q.sys_ns += now.saturating_sub(q.kern_at);
                    q.kern_at = 0;
                }
                set_cur(None);
                ON_CPU[crate::smp::id()].store(0, Relaxed);
                // 終わったスレッドのページ表はほかの CPU が片付けるかもしれないので、外しておく。生きているものは
                // 載せたまま (次に同じアドレス空間のスレッドを選べば、載せなおさない。ページ表は、それを使う
                // スレッドがみな終わるまで捨てられず、捨てるのは大きなロックの中: このループのあいだはない)
                if matches!(procs()[i].state, State::Zombie | State::Unused) {
                    crate::vm::deactivate();
                }
                // 終わったスレッドは誰も wait しないので、ここで片付ける (このスタックから出たあと、ページ表を外したあと)
                let p = &mut procs()[i];
                if p.state == State::Zombie && (p.thread || p.autoreap) {
                    free_slot(p);
                }
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
            if HEADS.get()[RUNQ] != NONE {
                crate::smp::set_idle(false);
                continue;
            }
            // 眠るあいだに、ほかの CPU が (大きなロックを取って) いま載せているページ表を捨てるかもしれない
            crate::vm::deactivate();
            crate::smp::unlock();
            crate::vm::free_deferred();
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
    let seg = crate::smp::seg_pause();
    let off = crate::debug::prof_off_cpu();
    // ext4 のロックは眠るあいだ手放す (大きなロックと同じく、ほかがそのあいだに ext4 に入れるように)
    let fs = crate::smp::fs_unlock_for_sleep();
    unsafe {
        fp_save(&mut p.fp);
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) p.tpidr);
        swtch(&mut p.context, sched_ctx());
    }
    crate::smp::fs_relock_after_sleep(fs);
    crate::debug::prof_on_cpu(off);
    crate::smp::seg_resume(seg);
}

pub fn yield_now() {
    make_runnable(current());
    sched();
}

/// chan で起こされるまで眠る。killed なら Err(-EINTR)
pub fn sleep(chan: usize) -> Result<(), i64> {
    sleep_until(chan, 0).map(|_| ())
}

/// chan で起こされるまで眠る。シグナルでは起きない (Linux の TASK_UNINTERRUPTIBLE)。出したディスクの要求の終わりを
/// 待つところ (virtio_blk): シグナルで戻ると、終わりを見とどける人がいなくなる (前は sleep が EINTR ですぐ戻り、
/// 大きなロックを持ったまま回りつづけ、終わりを受ける割り込みもタイマもロックを待って、機械ごと止まった)。
/// 届いたシグナルは、システムコールから戻るときに受ける。起きたあとも、待っていたものがまだなら呼ぶ側がまた眠る
pub fn sleep_nointr(chan: usize) {
    let p = current();
    p.chan = chan;
    p.wake_at = 0;
    p.nointr = true;
    p.state = State::Sleeping;
    p.slept_at = crate::timer::ticks();
    crate::debug::trace_sleep(chan);
    wq_enter();
    sched();
    wq_exit();
    let p = current();
    p.chan = 0;
    p.nointr = false;
}

/// chan で起こされるか deadline (tick, 0 なら無し) まで眠る。時間切れなら Ok(false)
/// chan で起こされるまで眠る。ふつうのシグナルでは起きず、SIGKILL (killed) でだけ EINTR で起きる (Linux の
/// TASK_KILLABLE: seccomp の WAIT_KILLABLE_RECV)
pub fn sleep_killable(chan: usize) -> Result<(), i64> {
    let p = current();
    if p.killed {
        return Err(-EINTR);
    }
    p.chan = chan;
    p.wake_at = 0;
    p.killable = true;
    p.state = State::Sleeping;
    p.slept_at = crate::timer::ticks();
    crate::debug::trace_sleep(chan);
    wq_enter();
    sched();
    wq_exit();
    let p = current();
    p.chan = 0;
    p.killable = false;
    if p.killed { Err(-EINTR) } else { Ok(()) }
}

pub fn sleep_until(chan: usize, deadline: u64) -> Result<bool, i64> {
    let p = current();
    if p.killed || crate::signal::deliverable(p) {
        return Err(-EINTR);
    }
    p.chan = chan;
    p.wake_at = deadline;
    p.state = State::Sleeping;
    p.slept_at = crate::timer::ticks();
    crate::debug::trace_sleep(chan);
    wq_enter();
    sched();
    wq_exit();
    let p = current();
    p.chan = 0;
    let timed_out = p.wake_at != 0 && crate::timer::ticks() >= p.wake_at;
    p.wake_at = 0;
    if p.killed || crate::signal::deliverable(p) {
        return Err(-EINTR);
    }
    Ok(!timed_out)
}

/// ソケットの受けとりで眠るときの期限: d (tick、0 は無し。SO_RCVTIMEO から) と、いまのスレッドの受けとりの締め切り
/// (recv_deadline) の早いほう
pub fn recv_deadline(d: u64) -> u64 {
    match (current().recv_deadline, d) {
        (0, x) | (x, 0) => x,
        (a, b) => a.min(b),
    }
}

/// chan で眠っている Proc を起こし、起こした数を返す
pub fn wakeup(chan: usize) -> usize {
    let _p = crate::debug::span("proc:wakeup");
    wake_chan(chan, |_| true)
}

/// chan で眠っているもののうち f に合うものを起こす (chan の並びだけを見る)
fn wake_chan(chan: usize, f: impl Fn(&Proc) -> bool) -> usize {
    wake_list(wq_of(chan), |p| &mut p.wq, |p| p.chan == chan && f(p))
}

/// 並び head で眠っているもののうち f に合うものを起こす。眠っている CPU があれば起こし、なければ
/// 走っているもののうち一番 CPU を使っている (recent) ものにゆずらせる (preempt)
fn wake_list(head: usize, l: fn(&mut Proc) -> &mut Link, f: impl Fn(&Proc) -> bool) -> usize {
    let mut n = 0;
    let mut woken = u32::MAX;
    let mut i = HEADS.get()[head];
    while i != NONE {
        let p = &mut procs()[i as usize];
        // 起こしても並びは変えない (出るのは起きたほう) ので、次はいつでも読める
        i = l(p).next;
        if p.state == State::Sleeping && f(p) {
            make_runnable(p);
            p.woken_ns = crate::timer::uptime_ns();
            crate::debug::trace_wake(p.pid, p.chan);
            woken = woken.min(p.recent);
            n += 1;
        }
    }
    if n > 0 && !crate::smp::wake_idle_n(n) {
        // 走っているもの (CPU ごとに 1 つ) のうち一番 CPU を使っているもの
        let mut busiest: Option<(u32, usize)> = None;
        for (cpu, on) in ON_CPU.iter().enumerate().take(crate::smp::online()) {
            let k = on.load(core::sync::atomic::Ordering::Relaxed);
            if k != 0 {
                let p = &procs()[k - 1];
                if p.state == State::Running && busiest.is_none_or(|(r, _)| p.recent > r) {
                    busiest = Some((p.recent, cpu));
                }
            }
        }
        if let Some((r, cpu)) = busiest {
            preempt(woken, r, cpu);
        }
    }
    n
}

/// タイムスライス (tick)。走りはじめてからタイマの割り込みがこれだけ来たら、ほかに順番をゆずる (/proc/sys は sysctl.rs)
pub static SCHED_TIMESLICE_TICKS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);
/// 起こされたもの (あまり CPU を使っていない) が、走っているもの (使っている) に割りこむか
pub static SCHED_WAKEUP_PREEMPT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

/// CPU ごとの「EL0 へ戻るときにゆずる」印 (起こされたものを、タイムスライスの終わりまで待たせない)
static RESCHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 起こしたもの (recent が woken) が、cpu で走っているもの (recent が running) より 2 tick 以上
/// 使っていなければ、cpu にゆずらせる。ふだん眠っているもの (音、入力、aiwm) が、計算し続けるものに
/// 割りこめる。sysctl kernel.sched_wakeup_preempt で止められる
fn preempt(woken: u32, running: u32, cpu: usize) {
    use core::sync::atomic::Ordering;
    if SCHED_WAKEUP_PREEMPT.load(Ordering::Relaxed) == 0 || running < woken.saturating_add(2) {
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

/// pidfd を poll で見張るときの印: どれかのプロセスが終わると起こす
pub fn pidfd_key() -> usize {
    static K: u8 = 0;
    &raw const K as usize
}

/// key (パイプや端末など) が変わった: それを見張って poll で眠っているものだけを起こす
pub fn poll_wake(key: usize) {
    let chan = poll_chan();
    wake_chan(chan, |p| p.poll_keys.as_ref().is_none_or(|k| k.contains(&key)));
}

/// tid のスレッドが chan で眠っていれば起こす (futex: 待つものはみな同じ chan で眠り、起こすものを選ぶ)
pub fn wake_tid(tid: u32, chan: usize) -> bool {
    wake_chan(chan, |p| p.pid == tid) > 0
}

/// タイマから: 期限の来た Proc を起こす
pub fn wake_expired(now: u64) {
    wake_list(TIMED, |p| &mut p.tm, |p| p.wake_at != 0 && p.wake_at <= now);
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
            crate::futex::wake_exit(addr);
        }
    }
}

/// いまのスレッドだけを終える。代表スレッドならプロセスの終了になる
pub fn exit(code: i32) -> ! {
    exit_status(exited(code))
}

fn exit_status(status: i32) -> ! {
    let p = current();
    crate::debug::trace_exit(p.pid, status as i64);
    // 監査: exit や exit_group の中なら、その記録 (__audit_free)
    if p.audit.is_some() {
        crate::security::audit_free();
    }
    crate::ns::init_exited(p);
    // 持ったまま終わる robust futex に OWNER_DIED (Linux の futex_exit_release は mm_release の clear_child_tid より先)
    crate::futex::exit_robust_list();
    clear_child_tid(p);
    crate::futex::exit(p.pid);
    vfork_done(p);
    if p.thread {
        // 使った時間は代表スレッドに持たせる
        let t = core::mem::take(&mut p.utime);
        let (c, s) = (cpu_ns(p), sys_ns(p));
        (p.cpu_ns, p.ran_at, p.sys_ns, p.kern_at) = (0, 0, 0, 0);
        if let Some(l) = find(p.tgid) {
            l.utime += t;
            l.cpu_dead_ns += c;
            l.sys_dead_ns += s;
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
    let _p = crate::debug::span("proc:exit_kids");
    reparent_kids(slot_of(p));
    drop(_p);
    let _p = crate::debug::span("proc:exit_files");
    p.files = None;
    crate::file::release_deferred(true);
    p.timers.clear();
    drop(_p);
    // アドレス空間はここで捨てる (Linux の exit_mm と同じ)。前は親が wait するまでゾンビが持っていたので、OOM で
    // 殺したものが終わっても、親が片付けるまでメモリが戻らず、そのあいだに次のものまで殺していた。ゾンビの
    // /proc/PID/stat と status の大きさは 0 になる (Linux と同じ)。外してから捨てる (exec と同じ。もうユーザーの
    // アドレスには触らない)。ほかのスレッドがまだ使っていれば、捨てるのは最後のスレッド。自分だけの表なら、ページを
    // 返すのは、この CPU が大きなロックを放してから (vm::PageTable::release_deferred)
    let _p = crate::debug::span("proc:exit_mm");
    crate::vm::deactivate();
    if let Some(m) = p.mm.as_ref().filter(|m| m.private()) {
        m.get().pt.release_deferred();
    }
    p.mm = None;
    drop(_p);
    let _p = crate::debug::span("proc:exit_notify");
    p.xstatus = p.group_exit.unwrap_or(status);
    p.state = State::Zombie;
    // 親は子の並びのつなぎから (表を探さない)。init を起こすのは、ゾンビの子をわたしたときだけ (reparent_kids)
    let parent = match p.sib.parent {
        NONE => find(p.ppid),
        s => Some(&mut procs()[s as usize]),
    };
    if let Some(parent) = parent {
        if crate::signal::parent_reaps_automatically(parent) {
            p.autoreap = true;
        } else {
            sigchld(p, parent);
        }
        wakeup(parent as *mut Proc as usize);
    }
    // pidfd を poll / epoll しているもの (印は pidfd_key。印なしで眠っているものも起きる)
    poll_wake(pidfd_key());
    drop(_p);
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
/// ユーザーへ戻るたびに通る。殺されていなければ (ふだん) 見るだけ
#[inline]
pub fn check_killed() {
    if current().killed {
        killed_now();
    }
}

#[cold]
fn killed_now() {
    let p = current();
    // 他のスレッドが execve した: このスレッド (代表) がそのプログラムになって続ける (SIGKILL が来ていなければ)
    if let Some((img, comm)) = p.exec_takeover.take().filter(|_| p.group_exit.is_none()) {
        p.killed = false;
        finish_exec(p, img, &comm);
        p.no_restart = true;
        return;
    }
    let status = if p.thread { 0 } else { p.group_exit.unwrap_or(SIGKILL) };
    exit_status(status);
}

pub const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_VFORK: u64 = 0x0000_4000;
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
    // しくじったら何も変えない (ユーザーの namespace だけ作って、ほかで EPERM のときも)
    let mut cred = p.cred.clone();
    crate::ns::renew(&mut cred, flags)?;
    p.cred = cred;
    p.sync_cred_work();
    if flags & CLONE_FILES != 0 {
        let f = Shared::new(p.files().clone());
        p.files = Some(f);
    }
    Ok(0)
}

pub fn clone(flags: u64, stack: usize, ptid: usize, tls: u64, ctid: usize) -> Result<u32, i64> {
    const EAGAIN: i64 = 11;
    const ENOMEM: i64 = 12;
    let thread = flags & CLONE_THREAD != 0;
    if thread && flags & CLONE_VM == 0 {
        return Err(crate::why::inval());
    }
    let parent = current();
    // CLONE_NEW*: 子の namespace を新しく (ns.rs。スレッドには作れない)
    if thread && flags & crate::ns::SUPPORTED != 0 {
        return Err(crate::why::inval());
    }
    let mut cred = parent.cred.clone();
    crate::ns::renew(&mut cred, flags)?;
    // CLONE_VM: スレッドでなくても同じアドレス空間 (vfork と glibc の posix_spawn は、子が exec するまで親のメモリで
    // 動き、exec できなかったわけをそこに書いて知らせる)。子が exec すれば新しいものに切りかわる
    let mm = if flags & CLONE_VM != 0 {
        parent.mm.clone().unwrap()
    } else {
        let _p = crate::debug::span("proc:fork_pt");
        let pre = parent.prefork.take();
        let m = parent.mm();
        let pt = match pre {
            Some(pre) => m.pt.fork_from(pre),
            None => m.pt.fork(),
        };
        let pt = pt.ok_or(-ENOMEM)?;
        Mm::shared(Mm { pt, heap_start: m.heap_start, brk: m.brk, mmap_next: m.mmap_next, exe: m.exe.clone(), exe_id: m.exe_id, args: m.args, dumpable: m.dumpable })
    };
    let files = if flags & CLONE_FILES != 0 {
        parent.files.clone().unwrap()
    } else {
        let _p = crate::debug::span("proc:fork_files");
        Shared::new(parent.files().clone())
    };
    let _p = crate::debug::span("proc:fork_slot");
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
    child.sync_cred_work();
    child.comm = parent.comm;
    child.spec_ssb = parent.spec_ssb;
    child.thread = thread;
    if thread {
        child.tgid = parent.tgid;
        child.ppid = parent.ppid;
    } else {
        child.ppid = parent.tgid;
        // 子の並びは代表スレッドに
        let leader = if parent.thread { find(parent.tgid).map(|l| slot_of(l)) } else { Some(slot_of(parent)) };
        if let Some(l) = leader {
            kid_link(l, slot_of(child));
        }
    }
    // 監査の入れもの (TASK の組のルールは子の pid と cred で見る)
    child.audit = crate::security::audit_alloc(child);
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
        child.set_child_tid = ctid;
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        child.clear_tid = ctid;
    }
    // CLONE_VFORK: 子が exec するか終わるまで親は待つ (子は親のスタックの上で動くので、先に親が動くと壊れる)
    let vfork = flags & CLONE_VFORK != 0;
    if vfork {
        child.vfork_parent = parent.pid;
    }
    let chan = vfork_chan(child);
    drop(_p);
    let _p = crate::debug::span("proc:fork_run");
    make_runnable(child);
    crate::smp::wake_idle();
    drop(_p);
    if vfork {
        // シグナルではやめない (止められたときだけ)。届いているシグナルがあると眠れないので、そのあいだはゆずりながら
        let me = current().pid;
        while find(tid).is_some_and(|c| c.vfork_parent == me) && !current().killed {
            if sleep(chan).is_err() {
                yield_now();
            }
        }
    }
    Ok(tid)
}

/// vfork の親が眠る channel (子の Proc の番地から。wait のもの (Proc の番地) とも futex (奇数) ともちがう)
fn vfork_chan(child: &Proc) -> usize {
    child as *const Proc as usize + 2
}

/// vfork の子が exec したか終わった: 待っている親を起こす
fn vfork_done(p: &mut Proc) {
    if p.vfork_parent != 0 {
        p.vfork_parent = 0;
        wakeup(vfork_chan(p));
    }
}

pub fn execve(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), i64> {
    execve_target(exec::Target::path(path)?, argv, envp)
}

/// execve と execveat: 開いたプログラム (t) に入れかわる。comm は t.comm()
pub fn execve_target(t: exec::Target, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<(), i64> {
    let comm = String::from(t.comm());
    let img = exec::exec_target(t, argv, envp)?;
    let p = current();
    // 代表でないスレッドが execve した (zig cc -MM は働き手のスレッドから clang に execve する):
    // Linux では他のスレッドが消えて、このスレッドが代表の PID で新しいプログラムになる。ここでは読みこんだものを
    // 代表スレッドにわたして、代表スレッドがそれになる (親から見て、プロセスは終わらずに execve したように見える)
    if p.thread {
        if let Some(l) = find(p.tgid).filter(|l| l.state != State::Zombie && !l.killed) {
            l.exec_takeover = Some((img, comm));
            for t in p.siblings() {
                kill_proc(t);
            }
            // このスレッドはシステムコールから戻るところ (check_killed) で終わる。ここで exit すると、呼んだ側
            // (sys_execve) がコピーした引数と環境 (zig cc は 1000 を超える) が片づけられずに残る
            p.killed = true;
            return Ok(());
        }
    }
    finish_exec(p, img, &comm);
    Ok(())
}

/// execve の続き: 読みこんだプログラムに p を入れかえる (他のスレッドは消える)。comm は新しい名前
fn finish_exec(p: &mut Proc, img: Image, comm: &str) {
    // dumpable (Linux の begin_new_exec): ふつうは 1 にもどる。読めないプログラムか、exec の前の euid と uid (egid と gid)
    // がちがえば fs.suid_dumpable。setuid / setgid で euid / egid が変わっても (下の commit_creds)
    let suid_dumpable = crate::exec::SUID_DUMPABLE.load(core::sync::atomic::Ordering::Relaxed) as u8;
    let mut dumpable = if img.nondump || p.cred.euid != p.cred.uid || p.cred.egid != p.cred.gid { suid_dumpable } else { 1 };
    let (old_euid, old_egid, old_fsuid, old_fsgid) = (p.cred.euid, p.cred.egid, p.cred.fsuid, p.cred.fsgid);
    // setuid / setgid のプログラムなら euid / egid (と保存された id) が変わる。no_new_privs なら変えない
    // (砂場の中から sudo で外へ出られないように)
    if let Some(u) = img.setuid.filter(|_| !p.cred.no_new_privs && p.cred.ns.user.is_none()) {
        p.cred.euid = u;
    }
    if let Some(g) = img.setgid.filter(|_| !p.cred.no_new_privs && p.cred.ns.user.is_none()) {
        p.cred.egid = g;
    }
    p.cred.suid = p.cred.euid;
    p.cred.sgid = p.cred.egid;
    // fs の id は euid と egid に、capability は作りなおす (Linux の cap_bprm_creds_from_file: root (uid か euid が 0) は
    // permitted がみな、euid が 0 なら effective も。ほかは空)
    p.cred.fsuid = p.cred.euid;
    p.cred.fsgid = p.cred.egid;
    p.cred.cap_perm = p.cred.uid == 0 || p.cred.euid == 0;
    p.cred.fs_caps = p.cred.euid == 0;
    p.set_comm(comm.as_bytes());
    crate::security::audit_proctitle_free(p);
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
    // LSM: exec で決まったラベルに (AppArmor の onexec は使いおわり、change_hat の前と token も消える: bprm_committed_creds)。
    // 新しいラベルでプログラムを写せなければ、Linux と同じく戻れないところの先で SIGSEGV (security::bprm_mmap_file)
    p.cred.label = img.bprm.label.clone();
    crate::security::bprm_committed_creds(&mut p.cred);
    let fatal = img.bprm.fatal;
    // 前のプログラムの robust futex (Linux の futex_exec_release: 前のアドレス空間で、外す前に)
    crate::futex::exit_robust_list();
    let old = p.mm.take();
    p.load_image(img);
    if p.cred.euid != old_euid || p.cred.egid != old_egid || p.cred.fsuid != old_fsuid || p.cred.fsgid != old_fsgid {
        dumpable = suid_dumpable;
    }
    p.mm().dumpable = dumpable;
    if fatal {
        crate::signal::send_thread(p, crate::signal::SIGSEGV, crate::signal::SigInfo::from(crate::signal::SI_KERNEL));
    }
    p.pt().activate();
    // 前のアドレス空間 (fork した写し) のページを返すのも、exit と同じく大きなロックを放してから
    if let Some(m) = old.as_ref().filter(|m| m.private()) {
        m.get().pt.release_deferred();
    }
    drop(old);
    // vfork の子: 親のメモリを離れたので、親は動ける
    vfork_done(p);
    // files を他と共有していたなら切り離す
    let files = p.files().clone();
    p.files = Some(Shared::new(files));
    // 大きい番号から閉じる (Linux 7.4 の close_cloexec_files)
    for f in p.files().fds.iter_mut().rev() {
        if f.as_ref().is_some_and(|f| f.cloexec) {
            *f = None;
        }
    }
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
    // 子の並びは代表スレッドが持つ (親が待つのも代表スレッドのアドレス)
    let leader = if me.thread { slot_of(find(my_tgid).ok_or(-ECHILD)?) } else { slot_of(me) };
    loop {
        let mut have = false;
        // Linux と同じく古い子から (子の並びは fork した順、init にわたされたものはそのあと)
        let mut i = procs()[leader].kids.0;
        while i != NONE {
            let c = &mut procs()[i as usize];
            i = c.sib.next;
            let wanted = match pid {
                p if p > 0 => c.pid as i64 == p,
                0 => c.pgid == my_pgid,
                -1 => true,
                p => c.pgid as i64 == -p,
            };
            if !wanted {
                continue;
            }
            // 終わった子 (ゾンビ) は、WEXITED で待つときだけ待つ相手 (WSTOPPED / WCONTINUED だけなら、ほかにいなければ ECHILD:
            // Linux の wait_task_zombie)
            if c.state == State::Zombie && options & WEXITED == 0 {
                continue;
            }
            have = true;
            let keep = options & WNOWAIT != 0;
            if c.state == State::Zombie && options & WEXITED != 0 {
                let r = (crate::ns::to_local(c.pid).unwrap_or(c.pid), c.xstatus);
                if !keep {
                    let _p = crate::debug::span("proc:reap");
                    let t = c.utime + c.cutime;
                    // 子の走った時間 (Linux の wait_task_zombie: 子の分と、子が回収した孫の分)
                    let (u, s) = user_sys_ns(c, false);
                    let (cc, cs) = (u + s + c.child_cpu_ns, s + c.child_sys_ns);
                    free_slot(c);
                    procs()[leader].cutime += t;
                    procs()[leader].child_cpu_ns += cc;
                    procs()[leader].child_sys_ns += cs;
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
        sleep(&mut procs()[leader] as *mut Proc as usize)?;
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
    let ticks = SCHED_TIMESLICE_TICKS.load(core::sync::atomic::Ordering::Relaxed) as u32;
    cur().is_some_and(|i| procs()[i].slice >= ticks)
}

/// sched_yield: 順番をゆずる (同じくらい走ったものより後ろに)
pub fn yield_voluntary() {
    current().yielded = true;
    yield_now();
}

/// スレッドグループ全体の utime (tick)
/// スレッドが CPU で走った時間 (ns)。いま走っていれば、走りはじめてからの分も
// [lockless]
pub fn cpu_ns(p: &Proc) -> u64 {
    let ran = p.ran_at;
    p.cpu_ns + if ran != 0 { crate::timer::uptime_ns().saturating_sub(ran) } else { 0 }
}

/// プロセス (スレッドグループ) が CPU で走った時間 (ns): 生きているスレッドと、終わったスレッドの分
pub fn group_cpu_ns(tgid: u32) -> u64 {
    let dead = find_leader_any(tgid).map_or(0, |l| l.cpu_dead_ns);
    dead + live().filter(|p| p.state != State::Unused && p.tgid == tgid).map(|p| cpu_ns(p)).sum::<u64>()
}

/// EL0 からの例外 (大きなロックの道と割り込み) の入口: ここからカーネルの時間 (sys_ns)。大きなロックなしの速い道
/// (getpid などのシステムコール、ページフォルト) は数えない (ユーザーの時間に入る): 短く、QEMU (TCG) では時計を
/// 読むのに 200 ns ほどかかり、getppid が 4 割遅くなったので。Linux の VIRT_CPU_ACCOUNTING_GEN のように出入りで測る
// [lockless]
pub fn kernel_enter() {
    if let Some(p) = try_current() {
        p.kern_at = crate::timer::uptime_ns().max(1);
    }
}

/// EL0 へもどる (例外の出口、新しいスレッドのはじめ): カーネルにいた時間を sys_ns に
// [lockless]
pub fn kernel_leave() {
    if let Some(p) = try_current() {
        if p.kern_at != 0 {
            p.sys_ns += crate::timer::uptime_ns().saturating_sub(p.kern_at);
            p.kern_at = 0;
        }
    }
}

/// スレッドがカーネルで走った時間 (ns)。いまカーネルで走っていれば、その分も
pub fn sys_ns(p: &Proc) -> u64 {
    let k = p.kern_at;
    p.sys_ns + if k != 0 && p.ran_at != 0 { crate::timer::uptime_ns().saturating_sub(k) } else { 0 }
}

/// (ユーザーの時間, カーネルの時間) ns。スレッドか (でなければスレッドグループの、終わったスレッドの分も)
pub fn user_sys_ns(p: &Proc, thread: bool) -> (u64, u64) {
    let (cpu, sys) = if thread {
        (cpu_ns(p), sys_ns(p))
    } else {
        let tgid = p.tgid;
        let l = find_leader_any(tgid);
        let (dc, ds) = l.map_or((0, 0), |l| (l.cpu_dead_ns, l.sys_dead_ns));
        let (c, s) = live().filter(|q| q.state != State::Unused && q.tgid == tgid).fold((0, 0), |(c, s), q| (c + cpu_ns(q), s + sys_ns(q)));
        (dc + c, ds + s)
    };
    let sys = sys.min(cpu);
    (cpu - sys, sys)
}

/// pidfd のようす (Linux の pidfd_poll): (終わった, 片づけられた)。thread なら PIDFD_THREAD の pidfd で、pid はそのスレッドの
/// 番号 (スレッドは終わるとすぐ片づくので、両方とも true になる)。プロセスのものは代表がゾンビなら終わった、もういなければ
/// 片づけられた
pub fn pidfd_state(pid: u32, thread: bool) -> (bool, bool) {
    let p = live().find(|p| p.pid == pid && p.state != State::Unused && (thread || !p.thread));
    match p {
        None => (true, true),
        Some(p) => (p.state == State::Zombie, false),
    }
}

/// 止められたスレッドグループの再開を待つ (SIGCONT か SIGKILL まで)。
/// 止まっている間に来た他のシグナルは、再開してから届ける
/// 止まっているスレッドグループの数 (set_stopped が変える。大きなロックの中で)。0 なら、ユーザーへ戻るたびに
/// グループの代表を表から探さない (Linux もスレッドごとの印 JOBCTL_STOP_PENDING を見るだけで、探さない)
static STOPPED_GROUPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// グループの代表の stopped を変える (止まっているグループの数も)
pub fn set_stopped(l: &mut Proc, on: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    if l.stopped != on {
        l.stopped = on;
        if on {
            STOPPED_GROUPS.fetch_add(1, Relaxed);
        } else {
            STOPPED_GROUPS.fetch_sub(1, Relaxed);
        }
    }
}

/// 止まっているスレッドグループがあるか (なければ stop_while_stopped は何もしない)
#[inline]
pub fn any_stopped() -> bool {
    STOPPED_GROUPS.load(core::sync::atomic::Ordering::Relaxed) != 0
}

pub fn stop_while_stopped() {
    loop {
        if !any_stopped() {
            return;
        }
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
        wq_enter();
        sched();
        wq_exit();
        current().chan = 0;
    }
}

pub fn stop_chan(leader: &Proc) -> usize {
    leader as *const Proc as usize + 1
}

/// 眠っているスレッドをシグナルで起こす (sleep は EINTR で戻る。sleep_nointr の中なら起こさない: 戻ってから受ける)
pub fn interrupt(t: &mut Proc) {
    match t.state {
        State::Sleeping if !t.nointr && (!t.killable || t.killed) => {
            make_runnable(t);
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

/// 資格情報が f に合う Proc (スレッドも、回収されるまでのゾンビも) の数 (seccomp の listener: フィルタを使う人)
pub fn count_live(f: impl Fn(&crate::cred::Cred) -> bool) -> usize {
    live().filter(|p| p.state != State::Unused && f(&p.cred)).count()
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

/// find_leader と同じだが、終わってまだ回収されていないもの (ゾンビ) も。Linux では回収されるまでいる (kill は 0、
/// getpgid も答える、/proc/PID もある)
pub fn find_leader_any(tgid: u32) -> Option<&'static mut Proc> {
    live().find(|p| p.state != State::Unused && p.pid == tgid && !p.thread)
}

/// leaders_in_pgrp と同じだが、ゾンビも
pub fn leaders_in_pgrp_any(pgid: u32) -> Vec<u32> {
    live().filter(|p| p.state != State::Unused && !p.thread && p.pgid == pgid).map(|p| p.tgid).collect()
}

/// プロセスグループ pgid が孤児か (Linux の will_become_orphaned_pgrp): グループの中に、親がちがうグループで同じセッションの
/// ものが 1 つもない。終わったもの (ゾンビ) と、親が init のものは数えない。孤児のグループには、端末からの止めるシグナル
/// (SIGTSTP、SIGTTIN、SIGTTOU) の既定の動作をしない (だれも続けさせられないので)
pub fn pgrp_orphaned(pgid: u32) -> bool {
    for p in live() {
        if p.state == State::Unused || p.state == State::Zombie || p.thread || p.pgid != pgid || p.ppid <= 1 {
            continue;
        }
        if let Some(parent) = find_leader_any(p.ppid)
            && parent.pgid != pgid
            && parent.sid == p.sid
        {
            return false;
        }
    }
    true
}

pub fn leaders_in_pgrp(pgid: u32) -> Vec<u32> {
    live().filter(|p| p.state != State::Unused && p.state != State::Zombie && !p.thread && p.pgid == pgid).map(|p| p.tgid).collect()
}

pub fn current_leader() -> &'static mut Proc {
    let p = current();
    // 自分が代表なら、表を探さない
    if p.is_leader() {
        return p;
    }
    let tgid = p.tgid;
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

/// いまのプロセスが io_sleepable の中か (プロセスがなければ false)
// [lockless]
pub fn io_sleep_ok() -> bool {
    cur().is_some_and(|i| procs()[i].io_sleep > 0)
}

/// f のあいだ、ディスクを待つときに眠ってよい (読むだけの道で、RefCell を借りたまま呼ばないところ)
pub fn io_sleepable<R>(f: impl FnOnce() -> R) -> R {
    current().io_sleep += 1;
    let r = f();
    current().io_sleep -= 1;
    r
}

/// f のあいだ、書き出し (ext4 の flush) はディスクを眠って待つ (ほかの CPU は大きなロックと ext4 のロックを使える)。
/// 呼ぶのは、RefCell も ext4 の中のものも持っていないところ (fsync、sync、RWF_SYNC の書きのあと、reboot)
pub fn flush_sleepable<R>(f: impl FnOnce() -> R) -> R {
    current().flush_sleep += 1;
    let r = f();
    current().flush_sleep -= 1;
    r
}

/// 書き出しのはじめ: flush_sleepable の印を取って 0 にする (書き出しの中で呼ぶ書き出し: MAP_SHARED のページを書く
/// ときの maybe_flush などは、借りているものがあるので眠らせない)。flush_sleep_restore で戻す
pub fn flush_sleep_take() -> u32 {
    match cur() {
        Some(i) => core::mem::take(&mut procs()[i].flush_sleep),
        None => 0,
    }
}

pub fn flush_sleep_restore(n: u32) {
    if let Some(i) = cur() {
        procs()[i].flush_sleep = n;
    }
}

/// いまのプロセスは noio の中か: ディスクを読まない (キャッシュになければ EAGAIN。preadv2 の RWF_NOWAIT)
// [lockless]
pub fn noio_on() -> bool {
    cur().is_some_and(|i| procs()[i].noio > 0)
}

// [lockless]
pub fn noio<R>(f: impl FnOnce() -> R) -> R {
    current().noio += 1;
    let r = f();
    current().noio -= 1;
    r
}

/// 同じく dontcache の中か: 新しく読んだものをキャッシュに残さない (RWF_DONTCACHE。前からあったものは残る)
// [lockless]
pub fn dontcache_on() -> bool {
    cur().is_some_and(|i| procs()[i].dontcache > 0)
}

pub fn dontcache<R>(f: impl FnOnce() -> R) -> R {
    current().dontcache += 1;
    let r = f();
    current().dontcache -= 1;
    r
}
