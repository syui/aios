// シグナル: 送る、ためる、EL0 へ戻るときにハンドラを呼ぶ (Linux arm64 と同じ rt_sigframe)
use crate::proc::{self, Proc, Shared};
use crate::trap::TrapFrame;
use alloc::vec::Vec;

pub const NSIG: usize = 65;

pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGSEGV: i32 = 11;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGURG: i32 = 23;
pub const SIGWINCH: i32 = 28;
pub const SIGSYS: i32 = 31;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

const SA_NOCLDWAIT: u64 = 0x2;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SA_RESTORER: u64 = 0x0400_0000;

const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;

// si_code
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TIMER: i32 = -2;
pub const SI_TKILL: i32 = -6;
pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
const SA_NOCLDSTOP: u64 = 0x1;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;

type R = Result<i64, i64>;

#[derive(Clone, Copy)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    pub const DFL: SigAction = SigAction { handler: SIG_DFL, flags: 0, restorer: 0, mask: 0 };
}

pub type SigTable = [SigAction; NSIG];

pub fn new_table() -> Shared<SigTable> {
    Shared::new([SigAction::DFL; NSIG])
}

pub fn copy_table(t: &Shared<SigTable>) -> Shared<SigTable> {
    Shared::new(*t.get())
}

/// ためておくシグナルの付帯情報 (siginfo のうち使うもの)
#[derive(Clone, Copy)]
pub struct SigInfo {
    pub code: i32,
    pub pid: u32,
    pub uid: u32,
    pub addr: u64,
    pub status: i32,
    pub value: u64,
    pub timerid: i32,
}

impl SigInfo {
    pub const ZERO: SigInfo = SigInfo { code: 0, pid: 0, uid: 0, addr: 0, status: 0, value: 0, timerid: 0 };

    pub fn from(code: i32) -> SigInfo {
        let (pid, uid) = proc::current_ids();
        SigInfo { code, pid, uid, ..SigInfo::ZERO }
    }
}

#[derive(Clone, Copy)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
    pub flags: i32,
}

impl AltStack {
    pub const NONE: AltStack = AltStack { sp: 0, size: 0, flags: SS_DISABLE };

    fn on(&self, sp: u64) -> bool {
        self.flags & SS_DISABLE == 0 && sp > self.sp && sp <= self.sp + self.size
    }
}

/// POSIX タイマー (timer_create)
#[derive(Clone, Copy)]
pub struct PosixTimer {
    pub id: i32,
    pub realtime: bool,
    pub signo: i32,
    /// SIGEV_SIGNAL (0), SIGEV_NONE (1), SIGEV_THREAD_ID (4)
    pub notify: i32,
    pub tid: u32,
    pub value: u64,
    /// 次に鳴る tick (0 なら止まっている) と、くり返しの間隔 (tick)
    pub deadline: u64,
    pub interval: u64,
    pub overrun: i32,
    /// CPU の時計のタイマ (Linux の posix-cpu-timers): その時計と、鳴る CPU 時間 (ns、0 なら止まっている) と間隔 (ns)。
    /// 見るのは tick ごと (Linux も tick ごと)
    pub cpu: Option<crate::timer::Clock>,
    pub cpu_deadline: u64,
    pub cpu_interval: u64,
}

/// 落ちたプロセスの知らせ (シリアルにも出る) は 5 秒に 10 こまで (Linux の printk_ratelimit: 5 * HZ に 10)。ロボットの子のように、
/// わざと自分に SIGABRT を送って死ぬものが続くと、シリアルが詰まって遅くなり、CI のログもそれで埋まっていた。
/// 出さなかった数は、次に出せるときに 1 行で
static REPORT_START: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SUPPRESSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const REPORT_BURST: u32 = 10;
const REPORT_INTERVAL_NS: u64 = 5_000_000_000;

fn report_allowed() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let now = crate::timer::uptime_ns();
    let start = REPORT_START.load(Relaxed);
    if start == 0 || now.saturating_sub(start) >= REPORT_INTERVAL_NS {
        REPORT_START.store(now.max(1), Relaxed);
        REPORTED.store(0, Relaxed);
        let missed = SUPPRESSED.swap(0, Relaxed);
        if missed > 0 {
            println!("signal: {} reports of processes killed by a signal suppressed", missed);
        }
    }
    if REPORTED.fetch_add(1, Relaxed) < REPORT_BURST {
        true
    } else {
        SUPPRESSED.fetch_add(1, Relaxed);
        false
    }
}

const fn bit(sig: i32) -> u64 {
    1 << (sig - 1)
}

const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

enum Default {
    Ignore,
    Terminate,
    Stop,
}

/// 止めるシグナル (SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU)
const STOP_MASK: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);

fn default_action(sig: i32) -> Default {
    match sig {
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH => Default::Ignore,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Default::Stop,
        _ => Default::Terminate,
    }
}

pub fn action(p: &Proc, sig: i32) -> SigAction {
    p.sigacts.as_ref().map_or(SigAction::DFL, |t| t.get()[sig as usize])
}

/// 送っても何も起きないか (無視、または既定が無視)
fn ignored(p: &Proc, sig: i32) -> bool {
    let a = action(p, sig);
    a.handler == SIG_IGN || (a.handler == SIG_DFL && matches!(default_action(sig), Default::Ignore))
}

/// ブロックしているか無視しているか (端末が SIGTTIN / SIGTTOU の代わりに EIO を返す)
pub fn blocked_or_ignored(p: &Proc, sig: i32) -> bool {
    p.sig_mask & bit(sig) != 0 || action(p, sig).handler == SIG_IGN
}

/// いま受け取れるシグナルがあるか (sleep を中断する)
pub fn deliverable(p: &Proc) -> bool {
    (p.sig_pending | p.sig_shared) & !p.sig_mask != 0
}

/// フォールトなど、来たら先に受けとるもの (Linux の SYNCHRONOUS_MASK)
const SYNCHRONOUS: u64 = bit(SIGSEGV) | bit(SIGBUS) | bit(SIGILL) | bit(SIGTRAP) | bit(SIGFPE) | bit(SIGSYS);

/// 来ているもののうち allowed の中で次に受けとるものを取り出す (Linux の dequeue_signal と next_signal): スレッドに
/// 来たもの (sig_pending) をプロセスに来たもの (sig_shared) より先に。それぞれの中では同期のもの (フォールト) を先に、
/// あとは番号の小さいものから
fn dequeue(p: &mut Proc, allowed: u64) -> Option<(i32, SigInfo)> {
    for shared in [false, true] {
        let set = if shared { p.sig_shared } else { p.sig_pending } & allowed;
        if set != 0 {
            let s = if set & SYNCHRONOUS != 0 { set & SYNCHRONOUS } else { set };
            let sig = s.trailing_zeros() as i32 + 1;
            return Some(if shared {
                p.sig_shared &= !bit(sig);
                (sig, p.sig_shared_info[sig as usize])
            } else {
                p.sig_pending &= !bit(sig);
                (sig, p.sig_info[sig as usize])
            });
        }
    }
    None
}

/// 終わるスレッド from が預かっていた、プロセスに来たシグナルを to (代表) に。どちらにもあるものは to のものを残す
pub fn hand_over(from: &mut Proc, to: &mut Proc) {
    let move_ = from.sig_shared & !to.sig_shared;
    for sig in 1..NSIG as i32 {
        if move_ & bit(sig) != 0 {
            to.sig_shared_info[sig as usize] = from.sig_shared_info[sig as usize];
        }
    }
    to.sig_shared |= move_;
    from.sig_shared = 0;
    if move_ & !to.sig_mask != 0 {
        proc::interrupt(to);
    } else if move_ != 0 {
        proc::wakeup(fd_chan(to.tgid));
        proc::poll_wake(fd_chan(to.tgid));
    }
}

/// たまっている sig を捨てる (スレッドに来たものもプロセスに来たものも)
fn discard(p: &mut Proc, mask: u64) {
    p.sig_pending &= !mask;
    p.sig_shared &= !mask;
}

/// /proc/PID/status のシグナルの行 (SigQ、SigPnd、ShdPnd、SigBlk、SigIgn、SigCgt: Linux の task_sig)。leader は見る
/// スレッド、shared はスレッドグループのみなのプロセスに来たもの。SigQ の上限は getrlimit と同じく決めない (最大)
pub fn status_text(p: &Proc, shared: u64, queued: u32) -> alloc::string::String {
    let (mut ign, mut cgt) = (0u64, 0u64);
    for sig in 1..NSIG as i32 {
        match action(p, sig).handler {
            SIG_DFL => {}
            SIG_IGN => ign |= bit(sig),
            _ => cgt |= bit(sig),
        }
    }
    alloc::format!(
        "SigQ:\t{}/{}\nSigPnd:\t{:016x}\nShdPnd:\t{:016x}\nSigBlk:\t{:016x}\nSigIgn:\t{:016x}\nSigCgt:\t{:016x}\n",
        queued,
        u64::MAX,
        p.sig_pending,
        shared,
        p.sig_mask,
        ign,
        cgt
    )
}

// ---- 送る ----

/// スレッド t に送る
pub fn send_thread(t: &mut Proc, sig: i32, info: SigInfo) {
    queue(t, sig, info, false);
}

/// t にためる (shared ならプロセスに来たものとして: send_group が選んだスレッドに預ける)
fn queue(t: &mut Proc, sig: i32, info: SigInfo, shared: bool) {
    // カーネルのスレッド (proc::kthread) は受けとらない (Linux の kthread と同じ: 送れるが何も起きない)
    if t.kthread {
        return;
    }
    // init は、ハンドラを登録したシグナルしか受け取らない (SIGKILL も効かない)
    if t.tgid == 1 && action(t, sig).handler == SIG_DFL {
        return;
    }
    if sig == SIGKILL {
        proc::kill_group(t.tgid, sig);
        return;
    }
    // SIGCONT は無視されていても、止まっているグループを動かす。
    // 止めるシグナルが来たら、たまっている SIGCONT は捨てる (逆も)
    if sig == SIGCONT {
        continue_group(t.tgid);
    } else if bit(sig) & STOP_MASK != 0 {
        discard(t, bit(SIGCONT));
    }
    let b = bit(sig);
    // ブロックされていない無視されるシグナルは捨てる
    if t.sig_mask & b == 0 && ignored(t, sig) {
        return;
    }
    // もう来ているなら、はじめのものの情報を残す (Linux の legacy_queue: ふつうのシグナルは 2 つめを捨てる)。スレッドに
    // 来たものとプロセスに来たものは別にたまる
    if shared {
        if t.sig_shared & b == 0 {
            t.sig_shared_info[sig as usize] = info;
        }
        t.sig_shared |= b;
    } else {
        if t.sig_pending & b == 0 {
            t.sig_info[sig as usize] = info;
        }
        t.sig_pending |= b;
    }
    GEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if t.sig_mask & b == 0 || t.sigwait & b != 0 {
        proc::interrupt(t);
    } else {
        // ブロックしたまま来たもの: signalfd を読む・待つもの (read は wakeup、poll と epoll は poll_wake) を起こす
        proc::wakeup(fd_chan(t.tgid));
        proc::poll_wake(fd_chan(t.tgid));
    }
}

/// シグナルが来るたびに進む版 (signalfd の epoll の event_gen)
pub static GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

static FD_CHAN: u8 = 0;

/// プロセス (tgid) にブロックしたシグナルが来るのを待つチャンネル (signalfd。ほかのものとぶつかっても、起きた側が
/// 見なおすだけ)
pub fn fd_chan(tgid: u32) -> usize {
    (&raw const FD_CHAN as usize) ^ ((tgid as usize) << 16)
}

/// いまのスレッドに来ている mask のシグナルを 1 つとり出す (番号の小さいものから。signalfd の read)
pub fn take(mask: u64) -> Option<(i32, SigInfo)> {
    dequeue(proc::current(), mask & !UNBLOCKABLE)
}

/// いまのスレッドに mask のシグナルが来ているか
pub fn pending_in(mask: u64) -> bool {
    let p = proc::current();
    (p.sig_pending | p.sig_shared) & mask & !UNBLOCKABLE != 0
}

/// signalfd の read の形 (struct signalfd_siginfo、128 バイト)。どの欄を埋めるかは Linux の signalfd_copyinfo と
/// siginfo_layout と同じく si_code で決める: カーネルの出すもの (0 < code < SI_KERNEL) はシグナルごと、SI_TIMER は
/// タイマー、ほかの負のもの (sigqueue、tgkill など) は pid・uid と値、のこり (kill) は pid と uid
pub fn signalfd_info(sig: i32, i: &SigInfo) -> [u8; 128] {
    let mut b = [0u8; 128];
    put(&mut b, 0, &(sig as u32).to_le_bytes());
    put(&mut b, 8, &i.code.to_le_bytes());
    let pid = crate::ns::local_or_0(i.pid).to_le_bytes();
    match i.code {
        c if c > SI_USER && c < SI_KERNEL => match sig {
            SIGCHLD => {
                put(&mut b, 12, &pid);
                put(&mut b, 16, &i.uid.to_le_bytes());
                put(&mut b, 40, &i.status.to_le_bytes());
            }
            // seccomp の TRAP: si_errno (timerid に)、ssi_syscall、ssi_call_addr、ssi_arch
            SIGSYS => {
                put(&mut b, 4, &i.timerid.to_le_bytes());
                put(&mut b, 84, &i.status.to_le_bytes());
                put(&mut b, 88, &i.addr.to_le_bytes());
                put(&mut b, 96, &crate::syscall::AUDIT_ARCH_AARCH64.to_le_bytes());
            }
            SIGSEGV | SIGBUS | SIGILL | SIGFPE | SIGTRAP => put(&mut b, 72, &i.addr.to_le_bytes()),
            _ => {
                put(&mut b, 12, &pid);
                put(&mut b, 16, &i.uid.to_le_bytes());
            }
        },
        SI_TIMER => {
            put(&mut b, 24, &i.timerid.to_le_bytes());
            put(&mut b, 44, &(i.value as i32).to_le_bytes());
            put(&mut b, 48, &i.value.to_le_bytes());
        }
        c if c < 0 => {
            put(&mut b, 12, &pid);
            put(&mut b, 16, &i.uid.to_le_bytes());
            put(&mut b, 44, &(i.value as i32).to_le_bytes());
            put(&mut b, 48, &i.value.to_le_bytes());
        }
        _ => {
            put(&mut b, 12, &pid);
            put(&mut b, 16, &i.uid.to_le_bytes());
        }
    }
    b
}

/// seccomp の TRAP: 自分のスレッドに SIGSYS を送る。ブロックか無視していたら受け取れないので終わる (Linux と同じ)
pub fn force_sigsys(nr: u64, pc: u64, errno: u16) {
    let p = proc::current();
    if p.sig_mask & bit(SIGSYS) != 0 || ignored(p, SIGSYS) {
        proc::die(SIGSYS);
    }
    // SYS_SECCOMP (1)
    let info = SigInfo { code: 1, addr: pc, status: nr as i32, timerid: errno as i32, ..SigInfo::from(1) };
    send_thread(p, SIGSYS, info);
}

/// プロセス (スレッドグループ) に送る。受け取れるスレッドを選ぶ
pub fn send_group(tgid: u32, sig: i32, info: SigInfo) -> Result<(), i64> {
    let b = bit(sig);
    let mut threads = proc::threads_of(tgid);
    if threads.is_empty() {
        return Err(-ESRCH);
    }
    let pick = threads.iter().position(|t| t.pid == tgid && t.sig_mask & b == 0).or_else(|| threads.iter().position(|t| t.sig_mask & b == 0)).unwrap_or(0);
    // プロセスに来たもの (Linux の shared_pending): 選んだスレッドに預け、スレッドに来たものよりあとで受けとる
    queue(threads.swap_remove(pick), sig, info, true);
    Ok(())
}

/// 終わった子のことを親 (代表スレッド) に SIGCHLD で知らせる。代表がブロックしていない無視されるシグナル (ハンドラの
/// ない SIGCHLD はふつうこれ) なら、send_group も代表を選んで捨てるので、スレッドを集めずに捨てる (子が終わるたびに)
pub fn send_chld(parent: &Proc, info: SigInfo) {
    let b = bit(SIGCHLD);
    if parent.state != proc::State::Zombie && parent.sig_mask & b == 0 && ignored(parent, SIGCHLD) {
        return;
    }
    let _ = send_group(parent.tgid, SIGCHLD, info);
}

/// 送り主が送ってよいか (Linux の check_kill_permission: uid が合うか CAP_KILL、でなければ EPERM。それから
/// security_task_kill: landlock の scope (相手が層の外なら EPERM。同じスレッドグループはよい)、それから LSM のフック
/// (AppArmor の signal の決まりで、だめなら EACCES))
fn can_signal(target: &Proc, sig: i32) -> Result<(), i64> {
    // audit が見る (auditd に送られるもの: AUDIT_SIGNAL_INFO)
    crate::security::audit_signal_info(target.tgid, sig);
    let me = proc::current();
    let (a, b) = (&me.cred, &target.cred);
    if !(a.euid == b.suid || a.euid == b.uid || a.uid == b.suid || a.uid == b.uid || a.capable(crate::cred::CAP_KILL)) {
        return Err(-EPERM);
    }
    crate::security::task_kill(target, sig)
}

/// 例外など、今のスレッドに必ず届けるもの (ブロックや無視は外す)
pub fn force(sig: i32, info: SigInfo) {
    let p = proc::current();
    let b = bit(sig);
    let a = action(p, sig);
    if p.sig_mask & b != 0 || a.handler == SIG_IGN {
        if let Some(t) = p.sigacts.as_ref() {
            t.get()[sig as usize].handler = SIG_DFL;
        }
        p.sig_mask &= !b;
    }
    p.sig_pending |= b;
    p.sig_info[sig as usize] = info;
}

/// プロセスグループ全体に送る
pub fn send_pgrp(pgid: u32, sig: i32, info: SigInfo) -> usize {
    let mut n = 0;
    for tgid in proc::leaders_in_pgrp(pgid) {
        if send_group(tgid, sig, info).is_ok() {
            n += 1;
        }
    }
    n
}

/// 親に子の停止 / 再開を知らせる (SIGCHLD と wait の起床)
fn notify_parent(tgid: u32, code: i32, sig: i32) {
    let Some(child) = proc::find_leader(tgid) else { return };
    let (ppid, uid) = (child.ppid, child.cred.uid);
    let Some(parent) = proc::find_leader(ppid) else { return };
    let quiet = code == CLD_STOPPED && action(parent, SIGCHLD).flags & SA_NOCLDSTOP != 0;
    proc::wakeup(parent as *mut Proc as usize);
    if !quiet {
        let _ = send_group(ppid, SIGCHLD, SigInfo { code, pid: tgid, uid, status: sig, ..SigInfo::ZERO });
    }
}

/// スレッドグループを止める (既定の動作が Stop のシグナルを受けたとき)
fn stop_group(tgid: u32, sig: i32) {
    let Some(l) = proc::find_leader(tgid) else { return };
    if l.stopped {
        return;
    }
    proc::set_stopped(l, true);
    l.stop_report = sig;
    l.cont_report = false;
    // 他のスレッドも、ユーザーに戻る前に止まるよう起こす
    let me = proc::current().pid;
    for th in proc::threads_of(tgid) {
        if th.pid != me {
            proc::interrupt(th);
        }
    }
    notify_parent(tgid, CLD_STOPPED, sig);
}

/// 止まっているスレッドグループを動かす (SIGCONT)
fn continue_group(tgid: u32) {
    for th in proc::threads_of(tgid) {
        discard(th, STOP_MASK);
    }
    let Some(l) = proc::find_leader(tgid) else { return };
    if !l.stopped {
        return;
    }
    proc::set_stopped(l, false);
    l.stop_report = 0;
    l.cont_report = true;
    proc::wakeup(proc::stop_chan(l));
    notify_parent(tgid, CLD_CONTINUED, SIGCONT);
}

/// SIGCHLD を無視していれば、子はゾンビにならずに消える
pub fn parent_reaps_automatically(parent: &Proc) -> bool {
    let a = action(parent, SIGCHLD);
    a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0
}

// ---- 届ける ----

/// 待ちが割り込まれたシステムコールを、戻る前にやり直させる印。restartable は Linux の ERESTARTSYS (ハンドラが
/// 動くなら SA_RESTART のときだけ)、always は ERESTARTNOINTR (ハンドラが動いてもいつも。futex の LOCK_PI)
pub struct Restart {
    pub restartable: bool,
    pub always: bool,
}

const FRAME_INFO: usize = 128;
const UC_MCONTEXT: usize = 176;
const MC_RESERVED: usize = 288;
const RESERVED: usize = 4096;
const FRAME: usize = FRAME_INFO + UC_MCONTEXT + MC_RESERVED + RESERVED + 16;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: usize = 528;

fn put(buf: &mut [u8], off: usize, v: &[u8]) {
    buf[off..off + v.len()].copy_from_slice(v);
}

fn siginfo_bytes(sig: i32, i: &SigInfo) -> [u8; FRAME_INFO] {
    let mut b = [0u8; FRAME_INFO];
    put(&mut b, 0, &sig.to_le_bytes());
    put(&mut b, 8, &i.code.to_le_bytes());
    match sig {
        SIGSEGV | SIGBUS | SIGILL | SIGFPE | SIGTRAP => put(&mut b, 16, &i.addr.to_le_bytes()),
        // seccomp の TRAP: si_errno、si_call_addr、si_syscall、si_arch
        SIGSYS => {
            put(&mut b, 4, &i.timerid.to_le_bytes());
            put(&mut b, 16, &i.addr.to_le_bytes());
            put(&mut b, 24, &i.status.to_le_bytes());
            put(&mut b, 28, &crate::syscall::AUDIT_ARCH_AARCH64.to_le_bytes());
        }
        SIGCHLD => {
            put(&mut b, 16, &crate::ns::local_or_0(i.pid).to_le_bytes());
            put(&mut b, 20, &i.uid.to_le_bytes());
            put(&mut b, 24, &i.status.to_le_bytes());
        }
        _ if i.code == SI_TIMER => {
            put(&mut b, 16, &i.timerid.to_le_bytes());
            put(&mut b, 24, &i.value.to_le_bytes());
        }
        _ => {
            put(&mut b, 16, &crate::ns::local_or_0(i.pid).to_le_bytes());
            put(&mut b, 20, &i.uid.to_le_bytes());
            put(&mut b, 24, &i.value.to_le_bytes());
        }
    }
    b
}

/// ハンドラのための rt_sigframe をユーザーのスタックに積み、レジスタを向ける
fn setup_frame(p: &mut Proc, tf: &mut TrapFrame, sig: i32, info: &SigInfo, a: &SigAction) -> Result<(), ()> {
    let mut sp = tf.sp_el0;
    if a.flags & SA_ONSTACK != 0 && p.altstack.flags & SS_DISABLE == 0 && !p.altstack.on(sp) {
        sp = p.altstack.sp + p.altstack.size;
    }
    let sp = (sp - FRAME as u64) & !15;
    let mut f = alloc::vec![0u8; FRAME];
    f[..FRAME_INFO].copy_from_slice(&siginfo_bytes(sig, info));
    let uc = FRAME_INFO;
    // uc_stack
    put(&mut f, uc + 16, &p.altstack.sp.to_le_bytes());
    let ss_flags = if p.altstack.on(sp + 1) { SS_ONSTACK } else { p.altstack.flags };
    put(&mut f, uc + 24, &ss_flags.to_le_bytes());
    put(&mut f, uc + 32, &p.altstack.size.to_le_bytes());
    let old_mask = p.saved_mask.take().unwrap_or(p.sig_mask);
    put(&mut f, uc + 40, &old_mask.to_le_bytes());
    // uc_mcontext
    let mc = uc + UC_MCONTEXT;
    put(&mut f, mc, &info.addr.to_le_bytes());
    for (i, r) in tf.x.iter().enumerate() {
        put(&mut f, mc + 8 + i * 8, &r.to_le_bytes());
    }
    put(&mut f, mc + 256, &tf.sp_el0.to_le_bytes());
    put(&mut f, mc + 264, &tf.elr.to_le_bytes());
    put(&mut f, mc + 272, &tf.spsr.to_le_bytes());
    // __reserved: fpsimd_context と終わりの印
    let r = mc + MC_RESERVED;
    let fp = proc::fp_snapshot();
    put(&mut f, r, &FPSIMD_MAGIC.to_le_bytes());
    put(&mut f, r + 4, &(FPSIMD_SIZE as u32).to_le_bytes());
    put(&mut f, r + 8, &(fp.fpsr as u32).to_le_bytes());
    put(&mut f, r + 12, &(fp.fpcr as u32).to_le_bytes());
    for (i, q) in fp.q.iter().enumerate() {
        put(&mut f, r + 16 + i * 16, &q.to_le_bytes());
    }
    // frame record (fp, lr)
    let rec = FRAME - 16;
    put(&mut f, rec, &tf.x[29].to_le_bytes());
    put(&mut f, rec + 8, &tf.x[30].to_le_bytes());
    p.pt().copy_out(sp as usize, &f).ok_or(())?;

    tf.x[0] = sig as u64;
    tf.x[1] = sp;
    tf.x[2] = sp + uc as u64;
    tf.x[29] = sp + rec as u64;
    // 戻り先: SA_RESTORER のもの (musl)、なければ vDSO の __kernel_rt_sigreturn (aarch64 の glibc。前は 0 で、
    // ハンドラから戻ると番地 0 で SIGSEGV になった)
    tf.x[30] = if a.flags & SA_RESTORER != 0 { a.restorer } else { crate::vdso::sigtramp().unwrap_or(0) };
    tf.sp_el0 = sp;
    tf.elr = a.handler;
    let mut m = p.sig_mask | a.mask;
    if a.flags & SA_NODEFER == 0 {
        m |= bit(sig);
    }
    p.sig_mask = m & !UNBLOCKABLE;
    if a.flags & SA_RESETHAND != 0 {
        if let Some(t) = p.sigacts.as_ref() {
            t.get()[sig as usize] = SigAction::DFL;
        }
    }
    Ok(())
}

/// EL0 へ戻る前に: たまっているシグナルを処理する。
/// interrupted はシステムコールが待ちを割り込まれて EINTR を返したところ
/// ユーザーへ戻るたびに通る。届けるものも、やり直すものも、戻すマスクもなければ (ふだん) 見るだけ
#[inline]
pub fn deliver(tf: &mut TrapFrame, interrupted: Option<Restart>) {
    let p = proc::current();
    if (p.sig_pending | p.sig_shared) & !p.sig_mask == 0 && interrupted.is_none() && !p.no_restart && p.saved_mask.is_none() && !proc::any_stopped() {
        return;
    }
    deliver_now(tf, interrupted);
}

#[cold]
fn deliver_now(tf: &mut TrapFrame, interrupted: Option<Restart>) {
    // 他のスレッドがグループを止めたなら、ここで止まる
    proc::stop_while_stopped();
    let p = proc::current();
    // 他のスレッドの execve を引きついだばかり: 割り込まれたのは前のプログラムのシステムコールなので、やり直さない
    let interrupted = if core::mem::take(&mut p.no_restart) { None } else { interrupted };
    let mut handled = false;
    loop {
        let Some((sig, info)) = dequeue(p, !p.sig_mask) else { break };
        let a = action(p, sig);
        match a.handler {
            SIG_IGN => continue,
            SIG_DFL => match default_action(sig) {
                Default::Ignore => continue,
                Default::Terminate => {
                    // 異常で落ちたもの (Linux が core を吐く種類) は知らせる (調べもの用)。5 秒に 10 こまで
                    if matches!(sig, 4..=8 | 11 | 31) && report_allowed() {
                        // SIGILL 4, SIGTRAP 5, SIGABRT 6, SIGBUS 7, SIGFPE 8, SIGSEGV 11, SIGSYS 31
                        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
                        let (pc, addr, ec) = p.last_fault;
                        // カーネルがフォールトで出したもの (si_code が正: SEGV_MAPERR、BUS_ADRERR、ILL_ILLOPC など) だけ、落ちた
                        // ところをくわしく。プロセスが送ったもの (kill、tkill、abort の raise: 0 以下) には落ちたところがないので、
                        // だれが送ったかと、abort の文 (x0 などの文字列) だけ
                        let fault = info.code > 0;
                        if let Some((name, off)) = p.mm.as_ref().filter(|_| fault).and_then(|_| p.mm().pt.name_at(pc as usize)) {
                            println!("pid {}: pc {:#x} is {} + {:#x}", p.pid, pc, name, off);
                        }
                        if p.mm.as_ref().is_some() {
                            for (r, v) in ["x0", "x1", "x19"].iter().zip(p.last_regs) {
                                let mut b = [0u8; 160];
                                if v > 0x1000 && p.mm().pt.copy_in(&mut b, v as usize).is_some() {
                                    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                                    if n >= 4 && b[..n].iter().all(|&c| (0x20..0x7f).contains(&c) || c == b'\n') {
                                        println!("pid {}: {} -> \"{}\"", p.pid, r, core::str::from_utf8(&b[..n]).unwrap_or(""));
                                    }
                                }
                            }
                        }
                        if let Some((name, off)) = p.mm.as_ref().filter(|_| fault).and_then(|_| p.mm().pt.name_at(p.last_lr as usize)) {
                            println!("pid {}: lr {:#x} is {} + {:#x}", p.pid, p.last_lr, name, off);
                        }
                        if fault && p.mm.as_ref().is_some() {
                            // 落ちたアドレスとスタックがどこか (スタックのあふれと、外を読んだのを見分ける)
                            let pt = &p.mm().pt;
                            println!("pid {}: addr {:#x} is {}", p.pid, addr, pt.region_text(addr as usize));
                            println!("pid {}: sp {:#x} is {}", p.pid, p.last_sp, pt.region_text(p.last_sp as usize));
                            let e = p.last_esr;
                            if matches!(e >> 26, 0x20 | 0x24) {
                                // データ / 命令のアボート: ISS の WnR (書いた)、CM (キャッシュの命令)、FSC (なぜ)
                                println!("pid {}: esr {:#x} (WnR {} CM {} FSC {:#x})", p.pid, e, (e >> 6) & 1, (e >> 8) & 1, e & 0x3f);
                            }
                            // そのページにかかわった最近の mmap / mprotect / munmap (-N は N 回前)
                            for l in pt.hist_text(addr as usize).iter().rev().take(12).rev() {
                                println!("pid {}: addr page: {}", p.pid, l);
                            }
                        }
                        let comm = core::str::from_utf8(&p.comm[..n]).unwrap_or("?");
                        if fault {
                            println!(
                                "pid {} ({}): killed by signal {} (fault: pc {:#x} addr {:#x} ec {:#x}, last syscall {})",
                                p.pid, comm, sig, pc, addr, ec, p.last_sys.0
                            );
                        } else if info.pid == p.tgid {
                            println!("pid {} ({}): killed by signal {} sent by itself (last syscall {})", p.pid, comm, sig, p.last_sys.0);
                        } else {
                            println!("pid {} ({}): killed by signal {} sent by pid {} (last syscall {})", p.pid, comm, sig, info.pid, p.last_sys.0);
                        }
                    }
                    proc::die(sig)
                }
                Default::Stop => {
                    // 孤児のプロセスグループでは、SIGSTOP のほかの止めるシグナルは捨てる (Linux の get_signal)
                    if sig != SIGSTOP && proc::pgrp_orphaned(p.pgid) {
                        continue;
                    }
                    stop_group(p.tgid, sig);
                    proc::stop_while_stopped();
                    continue;
                }
            },
            _ => {
                if let Some(r) = &interrupted {
                    if r.always || (r.restartable && a.flags & SA_RESTART != 0) {
                        restart(p, tf);
                    }
                }
                if setup_frame(p, tf, sig, &info, &a).is_err() {
                    proc::die(SIGSEGV);
                }
                handled = true;
                break;
            }
        }
    }
    // ハンドラを呼ばなかったなら、sigsuspend / ppoll などで一時的に変えたマスクを戻す
    if !handled {
        if let Some(m) = p.saved_mask.take() {
            p.sig_mask = m;
        }
    }
    // ハンドラが呼ばれずに割り込みが終わったなら、黙ってやり直す
    if !handled {
        if let Some(r) = interrupted {
            if r.restartable {
                restart(p, tf);
            }
        }
    }
}

fn restart(p: &Proc, tf: &mut TrapFrame) {
    tf.elr -= 4;
    tf.x[0] = p.orig_x0;
}

// ---- システムコール ----

use crate::proc::put_user as out;

fn read_u64(va: usize) -> Result<u64, i64> {
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(u64::from_le_bytes(b))
}

pub fn rt_sigaction(sig: usize, act: usize, oact: usize) -> R {
    if sig == 0 || sig >= NSIG {
        return Err(crate::why::inval());
    }
    let p = proc::current();
    let table = p.sigacts.as_ref().ok_or_else(|| crate::why::inval())?.get();
    let old = table[sig];
    if act != 0 {
        if sig == SIGKILL as usize || sig == SIGSTOP as usize {
            return Err(crate::why::inval());
        }
        // struct k_sigaction { handler, flags, restorer, mask }
        let mut b = [0u8; 32];
        p.pt().copy_in(&mut b, act).ok_or(-EFAULT)?;
        let v = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        table[sig] = SigAction { handler: v(0), flags: v(1), restorer: v(2), mask: v(3) & !UNBLOCKABLE };
        // 無視になったものは、たまっていても捨てる (Linux と同じく、スレッドグループのみなの分も)
        if ignored(p, sig as i32) {
            for t in proc::threads_of(p.tgid) {
                discard(t, bit(sig as i32));
            }
        }
    }
    if oact != 0 {
        let mut b = [0u8; 32];
        put(&mut b, 0, &old.handler.to_le_bytes());
        put(&mut b, 8, &old.flags.to_le_bytes());
        put(&mut b, 16, &old.restorer.to_le_bytes());
        put(&mut b, 24, &old.mask.to_le_bytes());
        out(oact, &b)?;
    }
    Ok(0)
}

pub fn rt_sigprocmask(how: u64, set: usize, oset: usize) -> R {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    let p = proc::current();
    let old = p.sig_mask;
    if set != 0 {
        let s = read_u64(set)?;
        p.sig_mask = match how {
            SIG_BLOCK => old | s,
            SIG_UNBLOCK => old & !s,
            SIG_SETMASK => s,
            _ => return Err(crate::why::inval()),
        } & !UNBLOCKABLE;
    }
    if oset != 0 {
        out(oset, &old.to_le_bytes())?;
    }
    Ok(0)
}

pub fn rt_sigpending(set: usize) -> R {
    let p = proc::current();
    out(set, &((p.sig_pending | p.sig_shared) & p.sig_mask).to_le_bytes())?;
    Ok(0)
}

/// マスクを一時的にかえて、シグナルが来るまで眠る
pub fn rt_sigsuspend(set: usize) -> R {
    let new = read_u64(set)? & !UNBLOCKABLE;
    let p = proc::current();
    p.saved_mask = Some(p.sig_mask);
    p.sig_mask = new;
    loop {
        if deliverable(proc::current()) {
            return Err(-EINTR);
        }
        proc::sleep_until(0, 0).ok();
    }
}

/// ppoll / pselect6 / epoll_pwait の sigmask: 待つあいだだけマスクを set にする (0 ならそのまま)。
/// 前のマスクは saved_mask にとっておき、システムコールのあとの deliver が戻す
/// (シグナルで起きたなら、ハンドラから戻るとき (sigreturn) に戻る)
pub fn wait_mask(set: usize) -> Result<(), i64> {
    if set == 0 {
        return Ok(());
    }
    let new = read_u64(set)? & !UNBLOCKABLE;
    let p = proc::current();
    p.saved_mask = Some(p.saved_mask.unwrap_or(p.sig_mask));
    p.sig_mask = new;
    Ok(())
}

/// pselect6 の 6 番目の引数 ({ sigset_t *ss; size_t len; }) から sigset のアドレスを
pub fn pselect_mask(arg: usize) -> Result<usize, i64> {
    if arg == 0 { Ok(0) } else { read_u64(arg).map(|v| v as usize) }
}

/// set のどれかがたまるまで待って、それを取り出す
pub fn rt_sigtimedwait(set: usize, info: usize, timeout: usize) -> R {
    const EAGAIN_: i64 = EAGAIN;
    let set = read_u64(set)? & !UNBLOCKABLE;
    let deadline = if timeout == 0 {
        0
    } else {
        let mut b = [0u8; 16];
        proc::current().pt().copy_in(&mut b, timeout).ok_or(-EFAULT)?;
        let ns = u64::from_le_bytes(b[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[8..].try_into().unwrap());
        crate::timer::ticks_after(ns)
    };
    loop {
        let p = proc::current();
        if let Some((sig, si)) = dequeue(p, set) {
            if info != 0 {
                out(info, &siginfo_bytes(sig, &si))?;
            }
            return Ok(sig as i64);
        }
        if timeout != 0 && crate::timer::ticks() >= deadline {
            return Err(-EAGAIN_);
        }
        p.sigwait = set;
        let r = proc::sleep_until(0, deadline);
        proc::current().sigwait = 0;
        r?;
    }
}

/// ハンドラから戻る: rt_sigframe の ucontext を戻す
pub fn rt_sigreturn(tf: &mut TrapFrame) -> R {
    let p = proc::current();
    let sp = tf.sp_el0 as usize;
    let mut f = alloc::vec![0u8; FRAME - 16];
    if p.pt().copy_in(&mut f, sp).is_none() {
        // die は戻らないので、先に片づける (残すともれる)
        drop(f);
        proc::die(SIGSEGV);
    }
    let uc = FRAME_INFO;
    let mc = uc + UC_MCONTEXT;
    let u = |o: usize| u64::from_le_bytes(f[o..o + 8].try_into().unwrap());
    p.sig_mask = u(uc + 40) & !UNBLOCKABLE;
    for i in 0..31 {
        tf.x[i] = u(mc + 8 + i * 8);
    }
    tf.sp_el0 = u(mc + 256);
    tf.elr = u(mc + 264);
    // EL0 のまま、条件フラグと SSBS だけ戻す (SSBS はユーザーも MSR で変えられる。Linux の valid_user_regs も残す)
    tf.spsr = u(mc + 272) & (0xf000_0000 | 1 << 12);
    let r = mc + MC_RESERVED;
    if u32::from_le_bytes(f[r..r + 4].try_into().unwrap()) == FPSIMD_MAGIC {
        let mut fp = proc::fp_snapshot();
        fp.fpsr = u32::from_le_bytes(f[r + 8..r + 12].try_into().unwrap()) as u64;
        fp.fpcr = u32::from_le_bytes(f[r + 12..r + 16].try_into().unwrap()) as u64;
        for i in 0..32 {
            fp.q[i] = u128::from_le_bytes(f[r + 16 + i * 16..r + 32 + i * 16].try_into().unwrap());
        }
        proc::fp_restore(&fp);
    }
    Ok(tf.x[0] as i64)
}

pub fn sigaltstack(ss: usize, old: usize) -> R {
    let p = proc::current();
    let user_sp = p.tf().sp_el0;
    if old != 0 {
        let a = p.altstack;
        let mut b = [0u8; 24];
        put(&mut b, 0, &a.sp.to_le_bytes());
        let flags = if a.on(user_sp) { SS_ONSTACK } else { a.flags };
        put(&mut b, 8, &flags.to_le_bytes());
        put(&mut b, 16, &a.size.to_le_bytes());
        out(old, &b)?;
    }
    if ss != 0 {
        let mut b = [0u8; 24];
        p.pt().copy_in(&mut b, ss).ok_or(-EFAULT)?;
        let flags = i32::from_le_bytes(b[8..12].try_into().unwrap());
        if p.altstack.on(user_sp) {
            return Err(-EPERM);
        }
        p.altstack = if flags & SS_DISABLE != 0 {
            AltStack::NONE
        } else {
            AltStack { sp: u64::from_le_bytes(b[0..8].try_into().unwrap()), size: u64::from_le_bytes(b[16..24].try_into().unwrap()), flags: 0 }
        };
    }
    Ok(0)
}

/// kill(pid, sig): pid > 0 はそのプロセス、0 は自分のグループ、-1 は送れる全員、< -1 はグループ -pid。
/// Linux の順: 相手がいなければ ESRCH、いればシグナルの番号 (EINVAL)、それから許可 (EPERM)
pub fn kill(pid: i64, sig: i32) -> R {
    let bad_sig = !(0..NSIG as i32).contains(&sig);
    let info = SigInfo::from(SI_USER);
    // まだ回収されていないもの (ゾンビ) もいる: 送れて (0)、何も起きない (Linux と同じ)
    let targets: Vec<u32> = if pid > 0 {
        vec_of(pid as u32)
    } else if pid == 0 {
        proc::leaders_in_pgrp_any(proc::current().pgid)
    } else if pid == -1 {
        // PID の namespace の中からは、中のものだけ
        proc::all_leaders().into_iter().filter(|&t| t != 1 && t != proc::current().tgid && crate::ns::to_local(t).is_some()).collect()
    } else {
        proc::leaders_in_pgrp_any((-pid) as u32)
    };
    let mut sent = 0;
    let mut denied = None;
    for tgid in targets {
        let Some(leader) = proc::find_leader_any(tgid) else { continue };
        if bad_sig {
            return Err(crate::why::inval());
        }
        if let Err(e) = can_signal(leader, sig) {
            denied = Some(e);
            continue;
        }
        if sig != 0 && leader.state != proc::State::Zombie {
            send_group(tgid, sig, info)?;
        }
        sent += 1;
    }
    if sent > 0 {
        Ok(0)
    } else if let Some(e) = denied {
        Err(e)
    } else {
        Err(-ESRCH)
    }
}

fn vec_of(t: u32) -> Vec<u32> {
    let mut v = Vec::new();
    v.push(t);
    v
}

/// tkill / tgkill: スレッドに送る (tgid が 0 なら確かめない)
/// Linux の do_send_specific の順: スレッドがいなければ ESRCH、シグナルの番号 (EINVAL)、許可 (EPERM)。
/// 番号が 0 以下なのは syscall.rs が先に EINVAL にする
pub fn tgkill(tgid: u32, tid: u32, sig: i32) -> R {
    send_specific(tgid, tid, sig, SigInfo::from(SI_TKILL))
}

/// rt_sigqueueinfo (tgid が 0: pid のプロセスへ) と rt_tgsigqueueinfo (tgid の中のスレッド pid へ)。siginfo_t を
/// ユーザーから読む (si_signo は sig にする)。カーネルの印 (si_code が 0 以上と SI_TKILL) を名のれるのは、自分の
/// スレッドへだけ (EPERM。Linux の do_rt_sigqueueinfo)。番号は送るほうの PID の namespace のもの
pub fn rt_sigqueueinfo(tgid: i64, pid: i64, sig: i32, uinfo: usize) -> R {
    let mut b = [0u8; 128];
    proc::current().pt().copy_in(&mut b, uinfo).ok_or(-EFAULT)?;
    if tgid != 0 && (tgid <= 0 || pid <= 0) {
        return Err(crate::why::inval());
    }
    let w = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let code = w(8);
    let me = proc::current().pid;
    let target = if pid > 0 { crate::ns::to_global(pid as u32) } else { None };
    if (code >= 0 || code == SI_TKILL) && target != Some(me) {
        return Err(-EPERM);
    }
    let value = u64::from_le_bytes(b[24..32].try_into().unwrap());
    let info = match code {
        SI_TIMER => SigInfo { code, timerid: w(16), value, ..SigInfo::ZERO },
        c if c > SI_USER && c < SI_KERNEL => match sig {
            SIGCHLD => SigInfo { code, pid: crate::ns::to_global(w(16) as u32).unwrap_or(0), uid: w(20) as u32, status: w(24), ..SigInfo::ZERO },
            SIGSEGV | SIGBUS | SIGILL | SIGFPE | SIGTRAP => SigInfo { code, addr: u64::from_le_bytes(b[16..24].try_into().unwrap()), ..SigInfo::ZERO },
            SIGSYS => SigInfo { code, timerid: w(4), addr: u64::from_le_bytes(b[16..24].try_into().unwrap()), status: w(24), ..SigInfo::ZERO },
            _ => SigInfo { code, pid: crate::ns::to_global(w(16) as u32).unwrap_or(0), uid: w(20) as u32, ..SigInfo::ZERO },
        },
        _ => SigInfo { code, pid: crate::ns::to_global(w(16) as u32).unwrap_or(0), uid: w(20) as u32, value, ..SigInfo::ZERO },
    };
    let target = target.ok_or(-ESRCH)?;
    if tgid != 0 {
        let g = crate::ns::to_global(tgid as u32).ok_or(-ESRCH)?;
        return send_specific(g, target, sig, info);
    }
    send_pid_info(target, false, sig, info)
}

fn send_specific(tgid: u32, tid: u32, sig: i32, info: SigInfo) -> R {
    let t = proc::find_thread(tid).ok_or(-ESRCH)?;
    if tgid != 0 && t.tgid != tgid {
        return Err(-ESRCH);
    }
    if !(0..NSIG as i32).contains(&sig) {
        return Err(crate::why::inval());
    }
    can_signal(t, sig)?;
    if sig != 0 {
        send_thread(t, sig, info);
    }
    Ok(0)
}

/// pidfd_send_signal (Linux の kill_pid_info_type): pid の Proc (回収されるまでのゾンビも) に、thread ならそのスレッド
/// だけ、でなければそのスレッドグループに info で。いなければ ESRCH、番号 (EINVAL)、許可 (EPERM) の順。ゾンビには 0
pub fn send_pid_info(pid: u32, thread: bool, sig: i32, info: SigInfo) -> R {
    let t = proc::find_any(pid).ok_or(-ESRCH)?;
    if !(0..NSIG as i32).contains(&sig) {
        return Err(crate::why::inval());
    }
    can_signal(t, sig)?;
    if sig == 0 || t.state == proc::State::Zombie {
        return Ok(0);
    }
    if thread {
        send_thread(t, sig, info);
        Ok(0)
    } else {
        let tgid = t.tgid;
        send_group(tgid, sig, info).map(|_| 0)
    }
}

/// プロセスグループ pgid のみなに info で (Linux の __kill_pgrp_info: ひとつでも送れれば 0、だれもいなければ ESRCH、
/// みな許されなければ最後のわけ)
pub fn send_pgrp_info(pgid: u32, sig: i32, info: SigInfo) -> R {
    if !(0..NSIG as i32).contains(&sig) {
        return Err(crate::why::inval());
    }
    let mut res = Err(-ESRCH);
    let mut sent = false;
    for tgid in proc::leaders_in_pgrp_any(pgid) {
        let Some(l) = proc::find_leader_any(tgid) else { continue };
        match can_signal(l, sig) {
            Ok(()) => {
                if sig != 0 && l.state != proc::State::Zombie {
                    send_group(tgid, sig, info)?;
                }
                sent = true;
            }
            Err(e) => res = Err(e),
        }
    }
    if sent { Ok(0) } else { res }
}

// ---- プロセスグループとセッション ----

/// setpgid(pid, pgid): Linux の順で (負の pgid は呼ぶ前に EINVAL: syscall.rs)。自分か自分の子 (まだ回収していないものも)
/// でなければ ESRCH、子がちがうセッションなら EPERM、exec したあとの子は EACCES、セッションリーダーは EPERM、
/// pgid が自分の番号でなければ、同じセッションにそのグループがなければ EPERM
pub fn setpgid(pid: u32, pgid: u32) -> R {
    const EACCES: i64 = 13;
    let me = proc::current_leader();
    let (my_tgid, my_sid) = (me.tgid, me.sid);
    let target = if pid == 0 { my_tgid } else { pid };
    let pgid = if pgid == 0 { target } else { pgid };
    let t = proc::find_leader_any(target).ok_or(-ESRCH)?;
    if t.tgid != my_tgid {
        if t.ppid != my_tgid {
            return Err(-ESRCH);
        }
        if t.sid != my_sid {
            return Err(-EPERM);
        }
        if t.execed {
            return Err(-EACCES);
        }
    }
    if t.sid == t.tgid {
        return Err(-EPERM); // セッションリーダーは動かせない
    }
    if pgid != target && !proc::leaders_in_pgrp_any(pgid).into_iter().any(|g| proc::find_leader_any(g).is_some_and(|l| l.sid == my_sid)) {
        return Err(-EPERM);
    }
    for th in proc::threads_of(target) {
        th.pgid = pgid;
    }
    if let Some(l) = proc::find_leader_any(target) {
        l.pgid = pgid;
    }
    Ok(0)
}

/// getpgid / getsid: まだ回収されていないもの (ゾンビ) も答える
pub fn getpgid(pid: u32) -> R {
    let t = if pid == 0 { proc::current_leader() } else { proc::find_leader_any(pid).ok_or(-ESRCH)? };
    Ok(t.pgid as i64)
}

pub fn getsid(pid: u32) -> R {
    let t = if pid == 0 { proc::current_leader() } else { proc::find_leader_any(pid).ok_or(-ESRCH)? };
    Ok(t.sid as i64)
}

pub fn setsid() -> R {
    let me = proc::current();
    let tgid = me.tgid;
    if proc::leaders_in_pgrp(tgid).iter().any(|&t| t != tgid) || me.pgid == tgid {
        return Err(-EPERM);
    }
    for th in proc::threads_of(tgid) {
        th.pgid = tgid;
        th.sid = tgid;
    }
    Ok(tgid as i64)
}
