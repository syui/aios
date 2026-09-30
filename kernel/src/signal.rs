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
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

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
}

const fn bit(sig: i32) -> u64 {
    1 << (sig - 1)
}

const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

enum Default {
    Ignore,
    Terminate,
}

fn default_action(sig: i32) -> Default {
    match sig {
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH => Default::Ignore,
        // 止める (SIGSTOP など) はまだないので無視する
        19..=22 => Default::Ignore,
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

/// いま受け取れるシグナルがあるか (sleep を中断する)
pub fn deliverable(p: &Proc) -> bool {
    p.sig_pending & !p.sig_mask != 0
}

// ---- 送る ----

/// スレッド t に送る
pub fn send_thread(t: &mut Proc, sig: i32, info: SigInfo) {
    // init は、ハンドラを登録したシグナルしか受け取らない (SIGKILL も効かない)
    if t.tgid == 1 && action(t, sig).handler == SIG_DFL {
        return;
    }
    if sig == SIGKILL {
        proc::kill_group(t.tgid, sig);
        return;
    }
    let b = bit(sig);
    // ブロックされていない無視されるシグナルは捨てる
    if t.sig_mask & b == 0 && ignored(t, sig) {
        return;
    }
    t.sig_pending |= b;
    t.sig_info[sig as usize] = info;
    if t.sig_mask & b == 0 || t.sigwait & b != 0 {
        proc::interrupt(t);
    }
}

/// プロセス (スレッドグループ) に送る。受け取れるスレッドを選ぶ
pub fn send_group(tgid: u32, sig: i32, info: SigInfo) -> Result<(), i64> {
    let b = bit(sig);
    let mut threads = proc::threads_of(tgid);
    if threads.is_empty() {
        return Err(-ESRCH);
    }
    let pick = threads.iter().position(|t| t.pid == tgid && t.sig_mask & b == 0).or_else(|| threads.iter().position(|t| t.sig_mask & b == 0)).unwrap_or(0);
    send_thread(threads.swap_remove(pick), sig, info);
    Ok(())
}

/// 送り主が送ってよいか (root か、uid が合うか)
fn can_signal(target: &Proc) -> bool {
    let me = proc::current();
    let (a, b) = (&me.cred, &target.cred);
    a.euid == 0 || a.uid == b.uid || a.uid == b.suid || a.euid == b.uid || a.euid == b.suid
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

/// SIGCHLD を無視していれば、子はゾンビにならずに消える
pub fn parent_reaps_automatically(parent: &Proc) -> bool {
    let a = action(parent, SIGCHLD);
    a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0
}

// ---- 届ける ----

/// 待ちが割り込まれたシステムコールを、戻る前にやり直させる印
pub struct Restart {
    pub restartable: bool,
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
        SIGCHLD => {
            put(&mut b, 16, &i.pid.to_le_bytes());
            put(&mut b, 20, &i.uid.to_le_bytes());
            put(&mut b, 24, &i.status.to_le_bytes());
        }
        _ if i.code == SI_TIMER => {
            put(&mut b, 16, &i.timerid.to_le_bytes());
            put(&mut b, 24, &i.value.to_le_bytes());
        }
        _ => {
            put(&mut b, 16, &i.pid.to_le_bytes());
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
    tf.x[30] = if a.flags & SA_RESTORER != 0 { a.restorer } else { 0 };
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
pub fn deliver(tf: &mut TrapFrame, interrupted: Option<Restart>) {
    let p = proc::current();
    let mut handled = false;
    loop {
        let ready = p.sig_pending & !p.sig_mask;
        if ready == 0 {
            break;
        }
        let sig = ready.trailing_zeros() as i32 + 1;
        p.sig_pending &= !bit(sig);
        let info = p.sig_info[sig as usize];
        let a = action(p, sig);
        match a.handler {
            SIG_IGN => continue,
            SIG_DFL => match default_action(sig) {
                Default::Ignore => continue,
                Default::Terminate => proc::die(sig),
            },
            _ => {
                if let Some(r) = &interrupted {
                    if r.restartable && a.flags & SA_RESTART != 0 {
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

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn read_u64(va: usize) -> Result<u64, i64> {
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(u64::from_le_bytes(b))
}

pub fn rt_sigaction(sig: usize, act: usize, oact: usize) -> R {
    if sig == 0 || sig >= NSIG {
        return Err(-EINVAL);
    }
    let p = proc::current();
    let table = p.sigacts.as_ref().ok_or(-EINVAL)?.get();
    let old = table[sig];
    if act != 0 {
        if sig == SIGKILL as usize || sig == SIGSTOP as usize {
            return Err(-EINVAL);
        }
        // struct k_sigaction { handler, flags, restorer, mask }
        let mut b = [0u8; 32];
        p.pt().copy_in(&mut b, act).ok_or(-EFAULT)?;
        let v = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        table[sig] = SigAction { handler: v(0), flags: v(1), restorer: v(2), mask: v(3) & !UNBLOCKABLE };
        // 無視になったものは、たまっていても捨てる
        if ignored(p, sig as i32) {
            p.sig_pending &= !bit(sig as i32);
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
            _ => return Err(-EINVAL),
        } & !UNBLOCKABLE;
    }
    if oset != 0 {
        out(oset, &old.to_le_bytes())?;
    }
    Ok(0)
}

pub fn rt_sigpending(set: usize) -> R {
    let p = proc::current();
    out(set, &(p.sig_pending & p.sig_mask).to_le_bytes())?;
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
        crate::timer::ticks() + (ns * crate::timer::HZ).div_ceil(1_000_000_000).max(if ns == 0 { 0 } else { 1 })
    };
    loop {
        let p = proc::current();
        let hit = p.sig_pending & set;
        if hit != 0 {
            let sig = hit.trailing_zeros() as i32 + 1;
            p.sig_pending &= !bit(sig);
            if info != 0 {
                out(info, &siginfo_bytes(sig, &p.sig_info[sig as usize]))?;
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
    // EL0 のまま、条件フラグだけ戻す
    tf.spsr = u(mc + 272) & 0xf000_0000;
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

/// kill(pid, sig): pid > 0 はそのプロセス、0 は自分のグループ、-1 は送れる全員、< -1 はグループ -pid
pub fn kill(pid: i64, sig: i32) -> R {
    if !(0..NSIG as i32).contains(&sig) {
        return Err(-EINVAL);
    }
    let info = SigInfo::from(SI_USER);
    let targets: Vec<u32> = if pid > 0 {
        vec_of(pid as u32)
    } else if pid == 0 {
        proc::leaders_in_pgrp(proc::current().pgid)
    } else if pid == -1 {
        proc::all_leaders().into_iter().filter(|&t| t != 1 && t != proc::current().tgid).collect()
    } else {
        proc::leaders_in_pgrp((-pid) as u32)
    };
    let mut sent = 0;
    let mut denied = false;
    for tgid in targets {
        let Some(leader) = proc::find_leader(tgid) else { continue };
        if !can_signal(leader) {
            denied = true;
            continue;
        }
        if sig != 0 {
            send_group(tgid, sig, info)?;
        }
        sent += 1;
    }
    if sent > 0 {
        Ok(0)
    } else if denied {
        Err(-EPERM)
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
pub fn tgkill(tgid: u32, tid: u32, sig: i32) -> R {
    if !(0..NSIG as i32).contains(&sig) {
        return Err(-EINVAL);
    }
    let t = proc::find_thread(tid).ok_or(-ESRCH)?;
    if tgid != 0 && t.tgid != tgid {
        return Err(-ESRCH);
    }
    if !can_signal(t) {
        return Err(-EPERM);
    }
    if sig != 0 {
        send_thread(t, sig, SigInfo::from(SI_TKILL));
    }
    Ok(0)
}

// ---- タイマー ----

fn read_timespec(va: usize) -> Result<u64, i64> {
    let mut b = [0u8; 16];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(u64::from_le_bytes(b[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[8..].try_into().unwrap()))
}

fn ns_to_ticks(ns: u64) -> u64 {
    (ns * crate::timer::HZ).div_ceil(1_000_000_000)
}

fn ticks_to_ns(t: u64) -> u64 {
    t * 1_000_000_000 / crate::timer::HZ
}

fn timespec(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    b
}

/// setitimer(ITIMER_REAL): 期限が来たら SIGALRM
pub fn setitimer(which: u64, new: usize, old: usize) -> R {
    if which != 0 {
        return Err(-EINVAL); // ITIMER_VIRTUAL/PROF はない
    }
    let now = crate::timer::ticks();
    let leader = proc::current_leader();
    if old != 0 {
        getitimer(which, old)?;
    }
    if new != 0 {
        // struct itimerval { timeval interval; timeval value; } (usec)
        let mut b = [0u8; 32];
        proc::current().pt().copy_in(&mut b, new).ok_or(-EFAULT)?;
        let tv = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap()) * 1000;
        let (interval, value) = (ns_to_ticks(tv(0)), ns_to_ticks(tv(16)));
        leader.itimer = if value == 0 { (0, 0) } else { (now + value, interval) };
    }
    Ok(0)
}

pub fn getitimer(which: u64, cur: usize) -> R {
    if which != 0 {
        return Err(-EINVAL);
    }
    let (deadline, interval) = proc::current_leader().itimer;
    let left = if deadline == 0 { 0 } else { ticks_to_ns(deadline.saturating_sub(crate::timer::ticks())) };
    let tv = |ns: u64| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        b
    };
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&tv(ticks_to_ns(interval)));
    b[16..].copy_from_slice(&tv(left));
    out(cur, &b)?;
    Ok(0)
}

pub fn timer_create(clock: u64, sev: usize, idp: usize) -> R {
    const SIGEV_SIGNAL: i32 = 0;
    const SIGEV_NONE: i32 = 1;
    const SIGEV_THREAD_ID: i32 = 4;
    let (mut signo, mut notify, mut tid) = (SIGALRM, SIGEV_SIGNAL, 0u32);
    let mut value = 0u64;
    if sev != 0 {
        // struct sigevent { sigval value; int signo; int notify; int tid (union) }
        let mut b = [0u8; 20];
        proc::current().pt().copy_in(&mut b, sev).ok_or(-EFAULT)?;
        value = u64::from_le_bytes(b[0..8].try_into().unwrap());
        signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
        notify = i32::from_le_bytes(b[12..16].try_into().unwrap());
        tid = u32::from_le_bytes(b[16..20].try_into().unwrap());
        if !matches!(notify, SIGEV_SIGNAL | SIGEV_NONE | SIGEV_THREAD_ID) || (notify != SIGEV_NONE && !(1..NSIG as i32).contains(&signo)) {
            return Err(-EINVAL);
        }
    }
    let leader = proc::current_leader();
    let id = (0..).find(|i| !leader.timers.iter().any(|t| t.id == *i)).unwrap();
    if value == 0 && sev == 0 {
        value = id as u64;
    }
    leader.timers.push(PosixTimer { id, realtime: clock == 0, signo, notify, tid, value, deadline: 0, interval: 0, overrun: 0 });
    out(idp, &id.to_le_bytes())?;
    Ok(0)
}

fn timer_of(id: i32) -> Result<&'static mut PosixTimer, i64> {
    proc::current_leader().timers.iter_mut().find(|t| t.id == id).ok_or(-EINVAL)
}

pub fn timer_settime(id: i32, flags: u64, new: usize, old: usize) -> R {
    const TIMER_ABSTIME: u64 = 1;
    if old != 0 {
        timer_gettime(id, old)?;
    }
    let interval = read_timespec(new)?;
    let value = read_timespec(new + 16)?;
    let t = timer_of(id)?;
    let now = crate::timer::ticks();
    t.interval = ns_to_ticks(interval);
    t.deadline = if value == 0 {
        0
    } else if flags & TIMER_ABSTIME != 0 {
        let cur = if t.realtime { crate::timer::epoch_ns() } else { crate::timer::uptime_ns() };
        now + ns_to_ticks(value.saturating_sub(cur)).max(1)
    } else {
        now + ns_to_ticks(value).max(1)
    };
    t.overrun = 0;
    Ok(0)
}

pub fn timer_gettime(id: i32, cur: usize) -> R {
    let t = *timer_of(id)?;
    let left = if t.deadline == 0 { 0 } else { ticks_to_ns(t.deadline.saturating_sub(crate::timer::ticks())) };
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&timespec(ticks_to_ns(t.interval)));
    b[16..].copy_from_slice(&timespec(left));
    out(cur, &b)?;
    Ok(0)
}

pub fn timer_getoverrun(id: i32) -> R {
    Ok(timer_of(id)?.overrun as i64)
}

pub fn timer_delete(id: i32) -> R {
    let l = proc::current_leader();
    let before = l.timers.len();
    l.timers.retain(|t| t.id != id);
    if l.timers.len() == before { Err(-EINVAL) } else { Ok(0) }
}

/// タイマ割り込みから: 期限の来た itimer と POSIX タイマーを鳴らす
pub fn tick(now: u64) {
    for p in proc::all_leader_procs() {
        let tgid = p.tgid;
        let (deadline, interval) = p.itimer;
        if deadline != 0 && deadline <= now {
            p.itimer = if interval == 0 { (0, 0) } else { (now + interval, interval) };
            let _ = send_group(tgid, SIGALRM, SigInfo { code: SI_KERNEL, ..SigInfo::ZERO });
        }
        let mut fire: Vec<PosixTimer> = Vec::new();
        for t in p.timers.iter_mut() {
            if t.deadline != 0 && t.deadline <= now {
                fire.push(*t);
                t.deadline = if t.interval == 0 { 0 } else { now + t.interval };
            }
        }
        for t in fire {
            let info = SigInfo { code: SI_TIMER, timerid: t.id, value: t.value, ..SigInfo::ZERO };
            match t.notify {
                4 => {
                    if let Some(th) = proc::find_thread(t.tid) {
                        send_thread(th, t.signo, info);
                    }
                }
                0 => {
                    let _ = send_group(tgid, t.signo, info);
                }
                _ => {}
            }
        }
    }
}

// ---- プロセスグループとセッション ----

pub fn setpgid(pid: u32, pgid: u32) -> R {
    let me = proc::current();
    let target = if pid == 0 { me.tgid } else { pid };
    let pgid = if pgid == 0 { target } else { pgid };
    let t = proc::find_leader(target).ok_or(-ESRCH)?;
    if t.tgid != proc::current().tgid && t.ppid != proc::current().tgid {
        return Err(-ESRCH);
    }
    if t.sid == t.tgid {
        return Err(-EPERM); // セッションリーダーは動かせない
    }
    for th in proc::threads_of(target) {
        th.pgid = pgid;
    }
    Ok(0)
}

pub fn getpgid(pid: u32) -> R {
    let t = if pid == 0 { proc::current_leader() } else { proc::find_leader(pid).ok_or(-ESRCH)? };
    Ok(t.pgid as i64)
}

pub fn getsid(pid: u32) -> R {
    let t = if pid == 0 { proc::current_leader() } else { proc::find_leader(pid).ok_or(-ESRCH)? };
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
