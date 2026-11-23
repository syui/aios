// 開いたファイルの持ち主と、非同期の知らせ (Linux の fs/fcntl.c: f_owner、fasync_helper、kill_fasync、send_sigio、
// send_sigurg)。
//   持ち主 (Fown)  F_SETOWN / F_SETOWN_EX でつけ、F_GETOWN / F_GETOWN_EX / F_GETOWNER_UIDS で読む。F_SETSIG のシグナルも。
//                  開いたファイルごと (dup と fork でいっしょ): OpenFile::fown。はじめて要るときに作る
//                  (file_f_owner_allocate)。番号は大もとの PID の namespace のもの
//   知らせの表     O_ASYNC (F_SETFL、FIOASYNC) で入れて外す。もの (パイプの読む口と書く口、ソケット、inotify ...) ごとに
//                  鍵 (そのものの番地) で引く表。最後に閉じたファイルはいなくなる (Weak)。もの (FileOps) は fasync_key で
//                  鍵を答え、できごとのときに kill (Linux の kill_fasync) を呼ぶ
//   送る           send_sigio: 持ち主 (スレッド、プロセス、グループのみな) に、F_SETSIG のシグナル (si_code は POLL_IN ...、
//                  si_band、si_fd) か、なければ SIGIO (SI_KERNEL)。送ってよいのは持ち主をつけた人の uid / euid が合うとき
//                  (sigio_perm)
// aios とちがうところ: 持ち主は番号で覚える (Linux は struct pid)。持ち主が終わって番号がほかのプロセスに使われると、
// そちらに送る
use crate::cell::Bkl;
use crate::file::{FileRef, OpenFile};
use crate::proc;
use crate::signal::{self, SigInfo};
use alloc::collections::BTreeMap;
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

pub const F_OWNER_TID: i32 = 0;
pub const F_OWNER_PID: i32 = 1;
pub const F_OWNER_PGRP: i32 = 2;

/// si_code (POLL_IN から POLL_HUP) と、そのときの si_band (Linux の band_table)
pub const POLL_IN: i32 = 1;
pub const POLL_OUT: i32 = 2;
pub const POLL_MSG: i32 = 3;
pub const POLL_ERR: i32 = 4;
pub const POLL_PRI: i32 = 5;
pub const POLL_HUP: i32 = 6;
const BAND: [u64; 6] = [0x41, 0x304, 0x441, 0x8, 0x82, 0x18];
/// シグナルが自分の si_code を持つときの si_code (Linux の SI_SIGIO)
pub const SI_SIGIO: i32 = -5;
/// O_ASYNC (arm64 の FASYNC)
pub const FASYNC: u32 = 0o20000;

#[derive(Clone, Copy, PartialEq)]
pub enum PidType {
    /// スレッド (F_OWNER_TID)
    Pid,
    /// プロセス (F_SETOWN の正の番号、F_OWNER_PID)
    Tgid,
    /// プロセスグループ (F_SETOWN の負の番号、F_OWNER_PGRP)
    Pgid,
}

/// struct fown_struct
pub struct Fown {
    /// 大もとの PID の namespace の番号。0 はいない
    pid: Cell<u32>,
    ty: Cell<PidType>,
    /// つけた人の uid と euid (sigio_perm)
    uid: Cell<u32>,
    euid: Cell<u32>,
    /// F_SETSIG (0 は SIGIO)
    signum: Cell<i32>,
}

/// ファイルの持ち主 (なければ作る)
fn fown_of(f: &FileRef) -> Rc<Fown> {
    let mut b = f.borrow_mut();
    b.fown.get_or_insert_with(|| Rc::new(Fown { pid: Cell::new(0), ty: Cell::new(PidType::Tgid), uid: Cell::new(0), euid: Cell::new(0), signum: Cell::new(0) })).clone()
}

fn fown_get(f: &FileRef) -> Option<Rc<Fown>> {
    f.try_borrow().ok().and_then(|b| b.fown.clone())
}

/// 番号 (大もと) のもの (struct pid) があるか: スレッドかプロセス (ゾンビも)、グループ、セッション (Linux の find_vpid)
fn pid_exists(g: u32) -> bool {
    proc::find_any(g).is_some() || !proc::leaders_in_pgrp_any(g).is_empty() || proc::find_where(|p| p.sid == g).is_some()
}

/// 呼んだ人の namespace の番号から大もとの番号 (なければ None)
fn resolve(who: u32) -> Option<u32> {
    crate::ns::to_global(who).filter(|&g| pid_exists(g))
}

/// 持ち主をつける (Linux の __f_setown)。force でなければ、まだいないときだけ
pub fn set(f: &FileRef, pid: u32, ty: PidType, force: bool) {
    let o = fown_of(f);
    remember_owner(f);
    if force || o.pid.get() == 0 {
        o.pid.set(pid);
        o.ty.set(ty);
        if pid != 0 {
            let c = crate::cred::current();
            o.uid.set(c.uid);
            o.euid.set(c.euid);
        }
    }
}

/// F_SETOWN: 正ならプロセス、負ならグループ、0 は外す
pub fn setown(f: &FileRef, who: i32) -> Result<i64, i64> {
    let (ty, who) = if who < 0 {
        if who == i32::MIN {
            return Err(crate::why::inval());
        }
        (PidType::Pgid, -who as u32)
    } else {
        (PidType::Tgid, who as u32)
    };
    let pid = if who != 0 { resolve(who).ok_or(-3i64)? } else { 0 };
    set(f, pid, ty, true);
    Ok(0)
}

/// 持ち主がいま生きているか (Linux の pid_task: ゾンビもいる)
fn alive(pid: u32, ty: PidType) -> bool {
    match ty {
        PidType::Pid => proc::find_any(pid).is_some(),
        PidType::Tgid => proc::find_leader_any(pid).is_some(),
        PidType::Pgid => !proc::leaders_in_pgrp_any(pid).is_empty(),
    }
}

/// F_GETOWN: いなければ 0、グループは負
pub fn getown(f: &FileRef) -> i64 {
    let Some(o) = fown_get(f) else { return 0 };
    let (pid, ty) = (o.pid.get(), o.ty.get());
    if pid == 0 || !alive(pid, ty) {
        return 0;
    }
    let l = crate::ns::local_or_0(pid) as i64;
    if ty == PidType::Pgid { -l } else { l }
}

/// F_SETOWN_EX (struct f_owner_ex: type、pid)
pub fn setown_ex(f: &FileRef, typ: i32, who: i32) -> Result<i64, i64> {
    let ty = match typ {
        F_OWNER_TID => PidType::Pid,
        F_OWNER_PID => PidType::Tgid,
        F_OWNER_PGRP => PidType::Pgid,
        _ => return Err(crate::why::inval()),
    };
    let pid = if who != 0 { u32::try_from(who).ok().and_then(resolve).ok_or(-3i64)? } else { 0 };
    set(f, pid, ty, true);
    Ok(0)
}

/// F_GETOWN_EX: (type, pid)。持ち主がまだなければスレッド (F_OWNER_TID) の 0
pub fn getown_ex(f: &FileRef) -> (i32, i32) {
    let Some(o) = fown_get(f) else { return (F_OWNER_TID, 0) };
    let (pid, ty) = (o.pid.get(), o.ty.get());
    let n = if pid != 0 && alive(pid, ty) { crate::ns::local_or_0(pid) as i32 } else { 0 };
    let t = match ty {
        PidType::Pid => F_OWNER_TID,
        PidType::Tgid => F_OWNER_PID,
        PidType::Pgid => F_OWNER_PGRP,
    };
    (t, n)
}

/// F_GETOWNER_UIDS: 持ち主をつけた人の uid と euid (Linux の CHECKPOINT_RESTORE)
pub fn owner_uids(f: &FileRef) -> (u32, u32) {
    fown_get(f).map(|o| (crate::ns::show_uid(o.uid.get()), crate::ns::show_uid(o.euid.get()))).unwrap_or((0, 0))
}

/// F_SETSIG (0 から 64。0 は SIGIO。Linux の valid_signal)
pub fn setsig(f: &FileRef, sig: i32) -> Result<i64, i64> {
    if !(0..=64).contains(&sig) {
        return Err(crate::why::inval());
    }
    fown_of(f).signum.set(sig);
    remember_owner(f);
    Ok(0)
}

pub fn getsig(f: &FileRef) -> i64 {
    fown_get(f).map(|o| o.signum.get() as i64).unwrap_or(0)
}

// ---- 知らせの表 ----

struct Entry {
    file: Weak<RefCell<OpenFile>>,
    fd: i32,
}

/// 鍵 (ものの番地) ごとの表
#[derive(Default)]
struct List {
    entries: Vec<Entry>,
    /// 書けなくなった (Linux の SOCKWQ_ASYNC_NOSPACE): 書けるようになったら 1 度だけ POLL_OUT
    nospace: bool,
}

static LISTS: Bkl<BTreeMap<usize, List>> = Bkl::new(BTreeMap::new());

/// ソケットの開いたファイル (Linux の sock->file: ソケットごとに 1 つ)。持ち主をつけたときに覚える: SIGURG は
/// O_ASYNC がなくても持ち主に送る (send_sigurg)
static OWNERS: Bkl<BTreeMap<usize, Weak<RefCell<OpenFile>>>> = Bkl::new(BTreeMap::new());

fn remember_owner(f: &FileRef) {
    let key = match &f.borrow().kind {
        crate::file::Kind::Ops(o) => o.fasync_key(),
        _ => None,
    };
    if let Some(k) = key {
        let owners = OWNERS.get();
        owners.retain(|_, w| w.strong_count() > 0);
        owners.insert(k, Rc::downgrade(f));
    }
}

/// O_ASYNC を入れる / 外す (Linux の fasync_helper)。ファイルの O_ASYNC の印もここで (表にいるかと印をいつも合わせる)。
/// 変えたら true
pub fn helper(key: usize, fd: i32, f: &FileRef, on: bool) -> bool {
    let lists = LISTS.get();
    let me = Rc::as_ptr(f);
    if !on {
        let Some(l) = lists.get_mut(&key) else { return false };
        let Some(i) = l.entries.iter().position(|e| core::ptr::eq(e.file.as_ptr(), me)) else { return false };
        l.entries.remove(i);
        if l.entries.is_empty() {
            lists.remove(&key);
        }
        f.borrow_mut().flags &= !FASYNC;
        return true;
    }
    let l = lists.entry(key).or_default();
    l.entries.retain(|e| e.file.strong_count() > 0);
    if let Some(e) = l.entries.iter_mut().find(|e| core::ptr::eq(e.file.as_ptr(), me)) {
        e.fd = fd;
        return false;
    }
    l.entries.push(Entry { file: Rc::downgrade(f), fd });
    f.borrow_mut().flags |= FASYNC;
    true
}

/// 鍵の表にだれかいるか (パイプのロックなしの道が見る数のため)
pub fn listed(key: usize) -> bool {
    LISTS.get().get(&key).is_some_and(|l| l.entries.iter().any(|e| e.file.strong_count() > 0))
}

/// 表のみなの持ち主に知らせる (Linux の kill_fasync)。sig は SIGIO か SIGURG、band は POLL_IN ...
pub fn kill(key: usize, sig: i32, band: i32) {
    let lists = LISTS.get();
    if lists.is_empty() {
        return;
    }
    let Some(l) = lists.get_mut(&key) else { return };
    l.entries.retain(|e| e.file.strong_count() > 0);
    let to: Vec<(Rc<Fown>, i32)> = l.entries.iter().filter_map(|e| e.file.upgrade().and_then(|f| fown_get(&f)).map(|o| (o, e.fd))).collect();
    if l.entries.is_empty() {
        lists.remove(&key);
    }
    for (o, fd) in to {
        // SIGURG は F_SETSIG をしたものにだけ (ほかは send_sigurg が送る)
        if !(sig == signal::SIGURG && o.signum.get() == 0) {
            send_sigio(&o, fd, band);
        }
    }
}

/// 書けなくなった (いっぱいで EAGAIN か、眠る前: Linux の sock_alloc_send_pskb の SOCKWQ_ASYNC_NOSPACE)
pub fn nospace(key: usize) {
    if let Some(l) = LISTS.get().get_mut(&key) {
        l.nospace = true;
    }
}

/// 書けるようになった: 前に書けなくなっていたら 1 度だけ POLL_OUT (Linux の sock_wake_async の SOCK_WAKE_SPACE)
pub fn write_space(key: usize) {
    let was = LISTS.get().get_mut(&key).is_some_and(|l| core::mem::take(&mut l.nospace));
    if was {
        kill(key, signal::SIGIO, POLL_OUT);
    }
}

/// 帯域外のデータが来た (Linux の sk_send_sigurg): ソケットの持ち主に SIGURG、いれば O_ASYNC の表に POLL_PRI
pub fn sigurg(key: usize) {
    let f = {
        let owners = OWNERS.get();
        if owners.is_empty() {
            return;
        }
        owners.get(&key).and_then(|w| w.upgrade())
    };
    if let Some(f) = f
        && send_sigurg(&f)
    {
        kill(key, signal::SIGURG, POLL_PRI);
    }
}

/// 自分の si_code を持つシグナル (Linux の sig_specific_sicodes)
fn specific(sig: i32) -> bool {
    matches!(sig, signal::SIGILL | signal::SIGFPE | signal::SIGSEGV | signal::SIGBUS | signal::SIGTRAP | signal::SIGCHLD | signal::SIGIO | signal::SIGSYS)
}

/// si_code と si_band と si_fd の形か (Linux の siginfo_layout の SIL_POLL)
pub fn poll_layout(sig: i32, code: i32) -> bool {
    code == SI_SIGIO || (POLL_IN..=POLL_HUP).contains(&code) && (sig == signal::SIGIO || !specific(sig))
}

/// 送ってよいか (Linux の sigio_perm)
fn perm(p: &proc::Proc, o: &Fown) -> bool {
    let (e, u) = (o.euid.get(), o.uid.get());
    e == 0 || e == p.cred.suid || e == p.cred.uid || u == p.cred.suid || u == p.cred.uid
}

/// 持ち主のスレッドかプロセスかグループのみなに (Linux の send_sigio: スレッドならそのスレッドだけ、プロセスなら
/// その番号のスレッドのいるプロセス、グループならグループのプロセスみな)。f は (受けるもの, スレッドだけか)
fn each(o: &Fown, mut f: impl FnMut(&mut proc::Proc, bool)) {
    let pid = o.pid.get();
    if pid == 0 {
        return;
    }
    match o.ty.get() {
        PidType::Pid => {
            if let Some(t) = proc::find_thread(pid) {
                f(t, true);
            }
        }
        PidType::Tgid => {
            if let Some(t) = proc::find_any(pid) {
                f(t, false);
            }
        }
        PidType::Pgid => {
            for tgid in proc::leaders_in_pgrp(pid) {
                if let Some(l) = proc::find_leader(tgid) {
                    f(l, false);
                }
            }
        }
    }
}

fn deliver(t: &mut proc::Proc, thread: bool, sig: i32, info: SigInfo) {
    if thread {
        signal::send_thread(t, sig, info);
    } else {
        // ゾンビ (スレッドがもういない) なら届かない
        let _ = signal::send_group(t.tgid, sig, info);
    }
}

/// F_SETSIG のシグナル (si_code は reason、si_band、si_fd) か SIGIO を送る
pub fn send_sigio(o: &Fown, fd: i32, reason: i32) {
    let signum = o.signum.get();
    each(o, |t, thread| {
        if !perm(t, o) {
            return;
        }
        if signum != 0 {
            let code = if signum != signal::SIGIO && specific(signum) { SI_SIGIO } else { reason };
            let band = BAND.get((reason - POLL_IN) as usize).copied().unwrap_or(!0);
            deliver(t, thread, signum, SigInfo { code, addr: band, status: fd, ..SigInfo::ZERO });
        } else {
            deliver(t, thread, signal::SIGIO, SigInfo { code: signal::SI_KERNEL, ..SigInfo::ZERO });
        }
    });
}

/// 帯域外のデータ (TCP の urgent、AF_UNIX の MSG_OOB): 持ち主がいれば SIGURG を送って true (Linux の send_sigurg)。
/// true なら、呼んだ側は kill(key, SIGURG, POLL_PRI) も
pub fn send_sigurg(f: &FileRef) -> bool {
    let Some(o) = fown_get(f) else { return false };
    if o.pid.get() == 0 {
        return false;
    }
    each(&o, |t, thread| {
        if perm(t, &o) {
            deliver(t, thread, signal::SIGURG, SigInfo { code: signal::SI_KERNEL, ..SigInfo::ZERO });
        }
    });
    true
}

// ---- O_ASYNC (Linux の f_op->fasync) ----

/// パイプの鍵: 読む口の表と書く口の表
fn pipe_keys(p: &Rc<crate::file::PipeCell>) -> (usize, usize) {
    let k = Rc::as_ptr(p) as *const u8 as usize;
    (k, k + 1)
}

/// パイプの読む口の表に知らせる (書いた、書く口がみな閉じた: Linux の pipe_write、pipe_release)
pub fn pipe_readers(p: &Rc<crate::file::PipeCell>) {
    kill(pipe_keys(p).0, signal::SIGIO, POLL_IN);
}

/// パイプの書く口の表に知らせる (読んだ、読む口がみな閉じた: Linux の pipe_read、pipe_release)
pub fn pipe_writers(p: &Rc<crate::file::PipeCell>) {
    kill(pipe_keys(p).1, signal::SIGIO, POLL_OUT);
}

/// パイプの表にだれかいるか (ロックなしの読み書きはしない)
pub fn pipe_listed(p: &Rc<crate::file::PipeCell>) -> bool {
    let (r, w) = pipe_keys(p);
    listed(r) || listed(w)
}

/// /dev/random と /dev/urandom の表 (Linux の random_fasync。乱数はいつも用意できているので知らせることはない)
static RANDOM_KEY: u8 = 0;

/// O_ASYNC を on / off に (F_SETFL、FIOASYNC)。fasync のないもの (ふつうのファイル、eventfd ...) なら None: O_ASYNC の印も
/// 変えない
pub fn toggle(fd: i32, f: &FileRef, on: bool) -> Option<()> {
    use crate::file::Kind;
    let mut pipe = None;
    let mut ops = None;
    let mut tty: Option<u32> = None;
    let keys: Vec<usize> = match &f.borrow().kind {
        Kind::PipeRead(p) => {
            pipe = Some(p.clone());
            alloc::vec![pipe_keys(p).0]
        }
        Kind::PipeWrite(p) => {
            pipe = Some(p.clone());
            alloc::vec![pipe_keys(p).1]
        }
        Kind::PipeRw(p) => {
            pipe = Some(p.clone());
            let (r, w) = pipe_keys(p);
            alloc::vec![r, w]
        }
        Kind::Random => alloc::vec![&RANDOM_KEY as *const u8 as usize],
        Kind::Tty(t) => {
            tty = Some(t.borrow().pgrp);
            alloc::vec![crate::tty::fasync_key(t, false)]
        }
        Kind::PtyMaster(t) => {
            tty = Some(0);
            alloc::vec![crate::tty::fasync_key(t, true)]
        }
        Kind::Ops(o) => {
            ops = Some(o.clone());
            alloc::vec![o.fasync_key()?]
        }
        _ => return None,
    };
    let mut added = false;
    for &k in &keys {
        added |= helper(k, fd, f, on) && on;
    }
    // 端末は、まだ持ち主がなければ前にいるグループ (なければ自分のプロセス) を持ち主に (Linux の __tty_fasync)
    if let Some(pgrp) = tty
        && added
    {
        if pgrp != 0 {
            set(f, pgrp, PidType::Pgid, false);
        } else {
            set(f, proc::current().tgid, PidType::Tgid, false);
        }
    }
    remember_owner(f);
    if let Some(o) = ops {
        o.fasync_changed(listed(keys[0]));
    }
    if let Some(p) = pipe {
        let l = pipe_listed(&p);
        p.borrow_mut().fasync = l;
    }
    Some(())
}
