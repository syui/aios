//! ファイルのロック (Linux の fs/locks.c と同じ決まり)
//!
//! - flock(2): 開いたファイル (OpenFile) ごとに、ファイルまるごと。LOCK_SH は共有、LOCK_EX は専有。dup した fd と
//!   fork した子は同じロックを持つ。開いたファイルの最後の参照が閉じたら外れる。パイプやソケットにもかけられる
//! - fcntl のレコードロック (F_GETLK / F_SETLK / F_SETLKW): 範囲つき。持ち主はプロセス (ファイルの表: スレッドは同じ持ち主)。
//!   同じ持ち主どうしはぶつからず、重なるか隣りあう同じ種類は 1 つにまとめ、種類がちがえば切りかえる (切り分け)。
//!   そのファイルの fd をプロセスがどれか 1 つでも閉じると (dup2 で上書きしても)、そのファイルのロックはみな外れる
//!   (POSIX の決まり)。fork した子には受けつがれない (子のファイルの表は別)
//! - OFD ロック (F_OFD_GETLK / F_OFD_SETLK / F_OFD_SETLKW): 持ち主は開いたファイル。dup した fd と fork した子は同じ持ち主。
//!   最後の参照が閉じたら外れる。F_OFD_GETLK は聞くときの l_pid が 0 でないと EINVAL、答えの l_pid は -1
//!
//! POSIX と OFD は持ち主がちがうので、同じプロセスの中でもぶつかる。待つ (F_SETLKW、LOCK_NB なしの flock) ときは、
//! ほかの持ち主が外すまで眠る。POSIX どうしの行きどまり (自分を待っている人を待つ) は、輪の長さによらず EDEADLK
//! (Linux と同じく、聞く側が OFD なら見ない)。ファイルごとの並びは Linux と同じ: 持ち主ははじめてかけた順、持ち主の中は始まりの順
//! (F_GETLK は、ぶつかるもののうち並びで先のものを答える)
use crate::cell::Bkl;
use crate::file::{FileRef, Kind, OpenFile};
use crate::proc;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;

/// core の入り口 (file::FileLocks。main.rs が file::register_locks で置く)
pub struct Locks;
pub static OPS: Locks = Locks;

impl crate::file::FileLocks for Locks {
    fn release_file(&self, file: usize) {
        release_file(file)
    }
    fn release_posix(&self, files: usize, f: &OpenFile) {
        release_posix(files, f)
    }
    fn release_owner(&self, files: usize) {
        release_owner(files)
    }
    fn fcntl(&self, f: &FileRef, files: usize, cmd: u64, arg: usize) -> Result<i64, i64> {
        fcntl(f, files, cmd, arg)
    }
    fn setlease(&self, fd: i32, f: &FileRef, arg: i64, deleg: bool) -> Result<i64, i64> {
        setlease(fd, f, arg, deleg)
    }
    fn getlease(&self, f: &FileRef, deleg: bool) -> i64 {
        getlease(f, deleg)
    }
    fn break_lease(&self, key: Key, write: bool, nonblock: bool) -> Result<(), i64> {
        break_lease(key, write, nonblock, false)
    }
    fn break_deleg(&self, key: Key) -> Result<(), i64> {
        break_lease(key, true, false, true)
    }
}

/// flock (main.rs が syscall::register で置く。Linux の COND_SYSCALL(flock))
pub const SYSCALLS: &[u64] = &[crate::syscall::nr::FLOCK];

pub fn sys(_n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    let (fd, op) = (a[0], a[1]);
    // LSM (file_lock。AppArmor): LOCK_SH と LOCK_UN は k、LOCK_EX (F_WRLCK) は w も
    const LOCK_EX: u64 = 2;
    const LOCK_NB: u64 = 4;
    if matches!(op & !LOCK_NB, 1 | 2 | 8) {
        crate::sysfile::lsm_fd(crate::security::PathOp::Lock { write: op & !LOCK_NB == LOCK_EX }, fd)?;
    }
    flock(fd, op)
}

const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EDEADLK: i64 = 35;
const EOVERFLOW: i64 = 75;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

/// ロックをかけるもの (ファイル) の鍵: inode なら (ファイルシステム, inode)。パイプは両方の口で 1 つ。ほかは開いたファイルごと
pub type Key = (usize, u64);

/// レコードロックの持ち主: プロセス (ファイルの表の場所) か、開いたファイル (OFD)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Owner {
    Posix(usize),
    Ofd(usize),
}

/// レコードロック 1 つ: [start, end] (end は i64::MAX まで: 終わりなし)
struct Rec {
    owner: Owner,
    /// 持ち主のプロセス (F_GETLK の l_pid。OFD は -1 と答える)
    pid: u32,
    wr: bool,
    start: i64,
    end: i64,
}

/// flock のロック 1 つ: 開いたファイルごと
struct Flock {
    file: usize,
    excl: bool,
}

/// ファイル (鍵) ごとのレコードロックの並び (Linux の flc_posix と同じ順)
static RECS: Bkl<BTreeMap<Key, Vec<Rec>>> = Bkl::new(BTreeMap::new());
static FLOCKS: Bkl<BTreeMap<Key, Vec<Flock>>> = Bkl::new(BTreeMap::new());
/// 待っている人 (持ち主) と、待たれている持ち主 (行きどまりを見る)
static WAITING: Bkl<Vec<(Owner, Owner)>> = Bkl::new(Vec::new());

#[track_caller]
fn recs() -> &'static mut BTreeMap<Key, Vec<Rec>> {
    RECS.get()
}

#[track_caller]
fn flocks() -> &'static mut BTreeMap<Key, Vec<Flock>> {
    FLOCKS.get()
}

#[track_caller]
fn waiting() -> &'static mut Vec<(Owner, Owner)> {
    WAITING.get()
}

/// 待っている人をみな起こす (ロックが外れた、変わった)
fn chan() -> usize {
    (&raw const RECS) as usize
}

/// 開いたファイルのロックの鍵。ないもの (ロックのかけられないもの) はない
fn key(f: &OpenFile) -> Option<Key> {
    Some(match &f.kind {
        Kind::Inode(i, _) => i.id(),
        Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => (usize::MAX, Rc::as_ptr(p) as usize as u64),
        Kind::PidFd(_) | Kind::Landlock(_) => return None,
        Kind::Ops(o) if !o.lockable() => return None,
        _ => (usize::MAX - 1, f as *const OpenFile as usize as u64),
    })
}

fn ptr_of(f: &OpenFile) -> usize {
    f as *const OpenFile as usize
}

/// 開いたファイルの最後の参照が閉じられた: その flock のロックと OFD ロックを外す
fn release_file(file: usize) {
    let mut changed = false;
    for (_, v) in flocks().iter_mut() {
        let n = v.len();
        v.retain(|l| l.file != file);
        changed |= v.len() != n;
    }
    for (_, v) in recs().iter_mut() {
        let n = v.len();
        v.retain(|r| r.owner != Owner::Ofd(file));
        changed |= v.len() != n;
    }
    if changed {
        tidy();
        proc::wakeup(chan());
    }
    // リースも外れる (Linux の locks_remove_lease)
    if crate::file::LEASES.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        let mut gone = false;
        leases().retain(|_, v| {
            let n = v.len();
            v.retain(|l| l.ptr != file);
            gone |= v.len() != n;
            !v.is_empty()
        });
        if gone {
            lease_count();
            proc::wakeup(lease_chan());
        }
    }
}

/// プロセス (ファイルの表 files) が、f の fd を 1 つ閉じた: そのファイルの POSIX のロックをみな外す
fn release_posix(files: usize, f: &OpenFile) {
    let Some(key) = key(f) else { return };
    if let Some(v) = recs().get_mut(&key) {
        let n = v.len();
        v.retain(|r| r.owner != Owner::Posix(files));
        if v.len() != n {
            tidy();
            proc::wakeup(chan());
        }
    }
}

/// プロセスのファイルの表がなくなる (終わり): その POSIX のロックをみな外す
fn release_owner(files: usize) {
    let mut changed = false;
    for (_, v) in recs().iter_mut() {
        let n = v.len();
        v.retain(|r| r.owner != Owner::Posix(files));
        changed |= v.len() != n;
    }
    if changed {
        tidy();
        proc::wakeup(chan());
    }
}

fn tidy() {
    recs().retain(|_, v| !v.is_empty());
    flocks().retain(|_, v| !v.is_empty());
}

/// (ロックの数, 待っている数): /proc/ai/fs
pub fn stats() -> (usize, usize) {
    (recs().values().map(|v| v.len()).sum::<usize>() + flocks().values().map(|v| v.len()).sum::<usize>(), waiting().len())
}

/// struct flock (arm64 / x86_64: l_type i16, l_whence i16, l_start i64, l_len i64, l_pid i32) の範囲を [start, end] に
/// (Linux の flock64_to_posix_lock)
fn range(whence: i16, l_start: i64, l_len: i64, pos: i64, size: i64) -> Result<(i64, i64), i64> {
    let base = match whence {
        0 => 0,
        1 => pos,
        2 => size,
        _ => return Err(crate::why::inval()),
    };
    if l_start > i64::MAX - base {
        return Err(-EOVERFLOW);
    }
    let mut start = base + l_start;
    if start < 0 {
        return Err(crate::why::inval());
    }
    let end = if l_len > 0 {
        if l_len - 1 > i64::MAX - start {
            return Err(-EOVERFLOW);
        }
        start + (l_len - 1)
    } else if l_len < 0 {
        if start + l_len < 0 {
            return Err(crate::why::inval());
        }
        let e = start - 1;
        start += l_len;
        e
    } else {
        i64::MAX
    };
    Ok((start, end))
}

fn overlap(r: &Rec, start: i64, end: i64) -> bool {
    r.end >= start && r.start <= end
}

/// 行きどまり: me が待とうとしている blocker (の持ち主) が、めぐって me を待っているか (Linux の posix_locks_deadlock)。
/// aios がえらんだちがい (lock-deadlock): 輪の長さによらず見つける (Linux は 11 たどるとやめるので、13 より多い持ち主の
/// 輪は見のがし、みないつまでも待つ: fcntl(2) の BUGS)。待っているものの数だけたどれば、me のない輪でも止まる
fn deadlock(me: Owner, mut blocker: Owner) -> bool {
    for _ in 0..=waiting().len() {
        let Some(&(_, next)) = waiting().iter().find(|(w, _)| *w == blocker) else { return false };
        if next == me {
            return true;
        }
        blocker = next;
    }
    false
}

/// fcntl の F_GETLK (5) / F_SETLK (6) / F_SETLKW (7) と F_OFD_GETLK (36) / F_OFD_SETLK (37) / F_OFD_SETLKW (38)。
/// files はプロセスのファイルの表 (POSIX の持ち主)、arg はユーザーの struct flock
fn fcntl(file: &FileRef, files: usize, cmd: u64, arg: usize) -> Result<i64, i64> {
    let mut buf = [0u8; 32];
    proc::current().pt().copy_in(&mut buf, arg).ok_or(-EFAULT)?;
    let l_type = i16::from_le_bytes([buf[0], buf[1]]);
    let l_whence = i16::from_le_bytes([buf[2], buf[3]]);
    let l_start = i64::from_le_bytes(buf[8..16].try_into().unwrap());
    let l_len = i64::from_le_bytes(buf[16..24].try_into().unwrap());
    let l_pid = i32::from_le_bytes(buf[24..28].try_into().unwrap());
    let ofd = cmd >= 36;
    let getlk = cmd == 5 || cmd == 36;
    let wait = cmd == 7 || cmd == 38;
    let (key, fptr, readable, writable, pos, size) = {
        let b = file.borrow();
        let mode = b.flags & crate::file::O_ACCMODE;
        let size = match &b.kind {
            Kind::Inode(i, _) => i.meta().size as i64,
            _ => 0,
        };
        (key(&b).ok_or_else(|| crate::why::inval())?, ptr_of(&b), mode == crate::file::O_RDONLY || mode == crate::file::O_RDWR, mode == crate::file::O_WRONLY || mode == crate::file::O_RDWR, b.offset as i64, size)
    };
    let owner = if ofd { Owner::Ofd(fptr) } else { Owner::Posix(files) };
    // ロックを置く (外すのも): 閉じるときに外すものがあるかもしれないので、大きなロックなしの close (棚) はもうしない
    if !getlk {
        if ofd {
            file.borrow_mut().locked = true;
        } else {
            proc::current().files().posix_locked = true;
        }
    }
    if getlk {
        // Linux の fcntl_getlk の順: 種類 (OFD なら F_UNLCK も)、範囲、OFD の l_pid
        if !ofd && l_type != F_RDLCK && l_type != F_WRLCK {
            return Err(crate::why::inval());
        }
        let (start, end) = range(l_whence, l_start, l_len, pos, size)?;
        // 種類は範囲のあとで (flock64_to_posix_lock の assign_type): OFD は F_UNLCK も聞ける (自分のロックを答える)
        if !matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK) {
            return Err(crate::why::inval());
        }
        if ofd && l_pid != 0 {
            return Err(crate::why::inval());
        }
        let hit = recs().get(&key).and_then(|v| {
            v.iter().find(|r| {
                overlap(r, start, end)
                    && if l_type == F_UNLCK { r.owner == owner } else { r.owner != owner && (r.wr || l_type == F_WRLCK) }
            })
        });
        match hit {
            Some(r) => {
                buf[0..2].copy_from_slice(&(if r.wr { F_WRLCK } else { F_RDLCK }).to_le_bytes());
                buf[2..4].copy_from_slice(&0i16.to_le_bytes());
                buf[8..16].copy_from_slice(&r.start.to_le_bytes());
                buf[16..24].copy_from_slice(&(if r.end == i64::MAX { 0 } else { r.end - r.start + 1 }).to_le_bytes());
                let pid: i32 = if matches!(r.owner, Owner::Ofd(_)) { -1 } else { r.pid as i32 };
                buf[24..28].copy_from_slice(&pid.to_le_bytes());
            }
            None => buf[0..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
        }
        proc::current().pt().copy_out(arg, &buf).ok_or(-EFAULT)?;
        return Ok(0);
    }
    // F_SETLK / F_SETLKW (Linux の fcntl_setlk の順): 範囲と種類、口 (F_RDLCK は読める fd、F_WRLCK は書ける fd)、OFD の l_pid
    let (start, end) = range(l_whence, l_start, l_len, pos, size)?;
    if !matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK) {
        return Err(crate::why::inval());
    }
    if (l_type == F_RDLCK && !readable) || (l_type == F_WRLCK && !writable) {
        return Err(-EBADF);
    }
    if ofd && l_pid != 0 {
        return Err(crate::why::inval());
    }
    let pid = proc::current().tgid;
    if l_type != F_UNLCK {
        let wr = l_type == F_WRLCK;
        loop {
            let blocker = recs().get(&key).and_then(|v| v.iter().find(|r| r.owner != owner && overlap(r, start, end) && (r.wr || wr)).map(|r| r.owner));
            let Some(b) = blocker else { break };
            if !wait {
                return Err(-EAGAIN);
            }
            if !ofd && deadlock(owner, b) {
                return Err(-EDEADLK);
            }
            waiting().push((owner, b));
            let r = proc::sleep(chan());
            waiting().retain(|w| *w != (owner, b));
            r?;
        }
    }
    apply(key, owner, pid, l_type, start, end);
    Ok(0)
}

/// 持ち主 owner の鍵 key のロックに [start, end] を typ でかける (F_UNLCK は外す): 重なるか隣りあう同じ種類はまとめ、
/// ちがう種類は切り分ける (Linux の posix_lock_inode)。持ち主の並びの場所は変えない (なくなって、またかけたら終わりに)
fn apply(key: Key, owner: Owner, pid: u32, typ: i16, start: i64, end: i64) {
    let v = recs().entry(key).or_default();
    // 持ち主のものを取り出す (並びの場所を覚えて)
    let first = v.iter().position(|r| r.owner == owner);
    let mut mine: Vec<Rec> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        if v[i].owner == owner {
            mine.push(v.remove(i));
        } else {
            i += 1;
        }
    }
    let mut out: Vec<Rec> = Vec::new();
    let (mut ns, mut ne) = (start, end);
    let wr = typ == F_WRLCK;
    for r in mine {
        if typ != F_UNLCK && r.wr == wr {
            // 同じ種類: 重なるか隣りあえば 1 つに
            if r.end >= ns.saturating_sub(1) && r.start.saturating_sub(1) <= ne {
                ns = ns.min(r.start);
                ne = ne.max(r.end);
                continue;
            }
            out.push(r);
        } else {
            // ちがう種類 (外すのも): 重なるところを切り取る
            if r.end < start || r.start > end {
                out.push(r);
                continue;
            }
            if r.start < start {
                out.push(Rec { owner, pid: r.pid, wr: r.wr, start: r.start, end: start - 1 });
            }
            if r.end > end {
                out.push(Rec { owner, pid: r.pid, wr: r.wr, start: end + 1, end: r.end });
            }
        }
    }
    if typ != F_UNLCK {
        out.push(Rec { owner, pid, wr, start: ns, end: ne });
    }
    out.sort_by_key(|r| r.start);
    let at = first.unwrap_or(v.len()).min(v.len());
    for (k, r) in out.into_iter().enumerate() {
        v.insert(at + k, r);
    }
    tidy();
    proc::wakeup(chan());
}

/// flock(fd, op): LOCK_SH / LOCK_EX / LOCK_UN (| LOCK_NB)。Linux の順: LOCK_MAND は何もしないで 0、op がだめなら EINVAL、
/// fd がないか O_PATH なら EBADF (fdget は O_PATH をとばす)、読みも書きもできない口 (O_ACCMODE が 3) は EBADF (LOCK_UN はよい)
fn flock(fd: u64, op: u64) -> Result<i64, i64> {
    const LOCK_SH: u64 = 1;
    const LOCK_EX: u64 = 2;
    const LOCK_NB: u64 = 4;
    const LOCK_UN: u64 = 8;
    const LOCK_MAND: u64 = 32;
    if op & LOCK_MAND != 0 {
        return Ok(0);
    }
    let (excl, un) = match op & !LOCK_NB {
        LOCK_SH => (false, false),
        LOCK_EX => (true, false),
        LOCK_UN => (false, true),
        _ => return Err(crate::why::inval()),
    };
    let f = proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let (key, me, usable) = {
        let b = f.borrow();
        if b.flags & crate::file::O_PATH != 0 {
            return Err(-EBADF);
        }
        let mode = b.flags & crate::file::O_ACCMODE;
        (key(&b).ok_or_else(|| crate::why::inval())?, ptr_of(&b), mode != crate::file::O_ACCMODE)
    };
    if !un && !usable {
        return Err(-EBADF);
    }
    // 閉じるときに外すものがあるかもしれない: 大きなロックなしの close (棚) はもうしない
    f.borrow_mut().locked = true;
    loop {
        let v = flocks().entry(key).or_default();
        if let Some(i) = v.iter().position(|l| l.file == me) {
            if !un && v[i].excl == excl {
                return Ok(0);
            }
            // 切りかえる (外してから取りなおす。取れなければ外れたまま: Linux と同じ)
            v.remove(i);
            proc::wakeup(chan());
        }
        if un {
            tidy();
            return Ok(0);
        }
        if !v.iter().any(|l| l.file != me && (excl || l.excl)) {
            v.push(Flock { file: me, excl });
            return Ok(0);
        }
        if op & LOCK_NB != 0 {
            tidy();
            return Err(-EAGAIN);
        }
        proc::sleep(chan())?;
    }
}

// ---- リース (Linux の fs/locks.c の lease: F_SETLEASE / F_GETLEASE、F_SETDELEG / F_GETDELEG) ----
//
// ふつうのファイルの開いたもの (OpenFile) ごとに 1 つ。読むリース (F_RDLCK) は、書く口がどこにもないとき。書くリース
// (F_WRLCK) は、ほかに開いたものがないとき。ほかの人が開く (書く口か、書くリースなら読む口も) か truncate すると、リースを
// 持つものに F_SETSIG のシグナル (POLL_MSG、si_fd: fasync.rs) を送り、外すか下げるまで (lease-break-time 秒まで) 待つ
// (O_NONBLOCK なら EWOULDBLOCK)。時間が来たら、外すものは外し、下げるものは下げる。閉じたら外れる。
// delegation (F_SETDELEG) はディレクトリにも (読むものだけ: Linux の generic_setlease)。delegation は、名前と属性を変える
// もの (中に作る・消す・名前を変える、link、chmod、chown、utimes、xattr: sysfile.rs の break_deleg) でもこわれる
// (Linux の try_break_deleg。F_SETLEASE のリースはこわれない)

/// lease-break-time (秒) と leases-enable (sysctl.rs)
pub static LEASE_BREAK_TIME: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(45);
pub static LEASES_ENABLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

struct Lease {
    file: alloc::rc::Weak<core::cell::RefCell<OpenFile>>,
    ptr: usize,
    typ: i16,
    /// F_SETDELEG で置いたもの (Linux の FL_DELEG)
    deleg: bool,
    /// 外すのを待っている (Linux の FL_UNLOCK_PENDING): Some(締め切りの tick。0 は時間切れなし)
    unlock: Option<u64>,
    /// 読むリースに下げるのを待っている (FL_DOWNGRADE_PENDING)
    downgrade: Option<u64>,
}

static LEASES: Bkl<BTreeMap<Key, Vec<Lease>>> = Bkl::new(BTreeMap::new());

fn leases() -> &'static mut BTreeMap<Key, Vec<Lease>> {
    LEASES.get()
}

/// リースが変わった (外した、下げた、時間切れ): 開くのを待っている人を起こす
fn lease_chan() -> usize {
    (&raw const LEASES) as usize
}

/// リースのシグナルの表の鍵 (fasync.rs): 開いたファイルの場所 + 3 (ほかのものの鍵とぶつからない)
fn lease_fasync(ptr: usize) -> usize {
    ptr + 3
}

fn lease_count() {
    let n: usize = leases().values().map(|v| v.len()).sum();
    crate::file::LEASES.store(n, core::sync::atomic::Ordering::Relaxed);
}

/// 外す (Linux の lease_modify の F_UNLCK): 持ち主と F_SETSIG を消し、シグナルの表から外す
fn lease_unlock(l: &Lease) {
    if let Some(f) = l.file.upgrade() {
        crate::fasync::set(&f, 0, crate::fasync::PidType::Tgid, true);
        let _ = crate::fasync::setsig(&f, 0);
        crate::fasync::helper(lease_fasync(l.ptr), 0, &f, false);
    }
}

/// 時間切れのものを下げる・外す (Linux の time_out_leases)
fn time_out_leases(key: Key) {
    let now = crate::timer::ticks();
    let past = |t: Option<u64>| t.is_some_and(|t| t != 0 && now > t);
    let Some(v) = leases().get_mut(&key) else { return };
    let mut gone = Vec::new();
    let mut changed = false;
    let mut i = 0;
    while i < v.len() {
        if past(v[i].downgrade) {
            v[i].typ = F_RDLCK;
            v[i].downgrade = None;
            changed = true;
        }
        if past(v[i].unlock) {
            gone.push(v.remove(i));
            changed = true;
            continue;
        }
        i += 1;
    }
    if v.is_empty() {
        leases().remove(&key);
    }
    for l in &gone {
        lease_unlock(l);
    }
    if changed {
        lease_count();
        proc::wakeup(lease_chan());
    }
}

/// いま求める型 (Linux の target_leasetype): 外すのを待っていれば F_UNLCK、下げるのを待っていれば F_RDLCK
fn target(l: &Lease) -> i16 {
    if l.unlock.is_some() {
        F_UNLCK
    } else if l.downgrade.is_some() {
        F_RDLCK
    } else {
        l.typ
    }
}

/// 開いたもののうち、key のファイルの (読むだけの口の数, 書く口の数)。me (リースを置く口) はのぞく (Linux の
/// i_readcount と i_writecount)
fn open_counts(key: Key, me: usize) -> (usize, usize) {
    let mut seen: Vec<usize> = Vec::new();
    let (mut r, mut w) = (0, 0);
    proc::each_open_file(|f| {
        let p = ptr_of(f);
        if p == me || seen.contains(&p) || self::key(f) != Some(key) || !matches!(f.kind, Kind::Inode(..)) {
            return;
        }
        seen.push(p);
        if f.writable() {
            w += 1;
        } else if f.readable() {
            r += 1;
        }
    });
    (r, w)
}

/// F_GETLEASE / F_GETDELEG (Linux の __fcntl_getlease)
fn getlease(f: &FileRef, deleg: bool) -> i64 {
    let (k, me) = {
        let b = f.borrow();
        (key(&b), ptr_of(&b))
    };
    let Some(k) = k else { return F_UNLCK as i64 };
    time_out_leases(k);
    leases().get(&k).and_then(|v| v.iter().find(|l| l.ptr == me)).filter(|l| l.deleg == deleg).map_or(F_UNLCK, target) as i64
}

/// F_SETLEASE / F_SETDELEG (Linux の fcntl_setlease、vfs_setlease、generic_setlease)
fn setlease(fd: i32, f: &FileRef, arg: i64, deleg: bool) -> Result<i64, i64> {
    const EACCES: i64 = 13;
    const EINVAL: i64 = 22;
    const CAP_LEASE: u32 = 28;
    let (k, me, is_dir, uid, ok_kind, writable) = {
        let b = f.borrow();
        let (dir, uid, inode) = match &b.kind {
            Kind::Inode(i, _) => {
                let m = i.meta();
                (m.is_dir(), Some(m.uid), m.mode & crate::vfs::S_IFMT == crate::vfs::S_IFREG || m.is_dir())
            }
            _ => (false, None, false),
        };
        (key(&b), ptr_of(&b), dir, uid.or_else(|| Some(b.stat().uid)), inode, b.writable())
    };
    // リースはディレクトリには置けない (fcntl_setlease)。delegation は置ける (generic_setlease: 読むものだけ)
    if is_dir && !deleg {
        return Err(-EINVAL);
    }
    // 型 (lease_init の assign_type)、持ち主か CAP_LEASE (vfs_setlease)、ふつうのファイル (generic_setlease)
    if !matches!(arg as i16, F_RDLCK | F_WRLCK | F_UNLCK) || arg as i16 as i64 != arg {
        return Err(-EINVAL);
    }
    let c = crate::cred::current();
    if uid != Some(c.fsuid) && !c.capable(CAP_LEASE) {
        return Err(-EACCES);
    }
    let Some(k) = k.filter(|_| ok_kind) else { return Err(-EINVAL) };
    let arg = arg as i16;
    if is_dir && arg == F_WRLCK {
        return Err(-EINVAL);
    }
    if arg == F_UNLCK {
        let Some(v) = leases().get_mut(&k) else { return Err(-EAGAIN) };
        let Some(i) = v.iter().position(|l| l.ptr == me) else { return Err(-EAGAIN) };
        let l = v.remove(i);
        if v.is_empty() {
            leases().remove(&k);
        }
        lease_unlock(&l);
        lease_count();
        proc::wakeup(lease_chan());
        return Ok(0);
    }
    time_out_leases(k);
    // ほかに開いている口 (Linux の lease_open_conflict)
    let (r, w) = open_counts(k, me);
    if arg == F_RDLCK && (w > 0 || writable) {
        return Err(-EAGAIN);
    }
    if arg == F_WRLCK && (w > 0 || r > 0) {
        return Err(-EAGAIN);
    }
    let v = leases().entry(k).or_default();
    let mut mine = None;
    for (i, l) in v.iter().enumerate() {
        if l.ptr == me {
            mine = Some(i);
            continue;
        }
        // ほかにリースがあれば書くリースはだめ、外すのを待っているものがあれば新しいものもだめ
        if arg == F_WRLCK || l.unlock.is_some() {
            if v.is_empty() {
                leases().remove(&k);
            }
            return Err(-EAGAIN);
        }
    }
    if let Some(i) = mine {
        // 型を変える (Linux の lease_modify): 読むリースにしたら下げる待ちは終わり
        let l = &mut v[i];
        l.typ = arg;
        if arg == F_RDLCK {
            l.downgrade = None;
        }
        proc::wakeup(lease_chan());
    } else {
        if LEASES_ENABLE.load(core::sync::atomic::Ordering::Relaxed) == 0 {
            if v.is_empty() {
                leases().remove(&k);
            }
            return Err(-EINVAL);
        }
        v.push(Lease { file: Rc::downgrade(f), ptr: me, typ: arg, deleg, unlock: None, downgrade: None });
    }
    // シグナルの表に入れ、持ち主がまだなければ自分のプロセスに (Linux の lease_setup)
    crate::fasync::helper(lease_fasync(me), fd, f, true);
    crate::fasync::set(f, proc::current().tgid, crate::fasync::PidType::Tgid, false);
    lease_count();
    Ok(0)
}

/// 開く・切りつめる前に、ぶつかるリースをこわす (Linux の __break_lease)。write は書く口 (truncate も)。deleg なら
/// delegation だけ (Linux の break_deleg: 名前と属性を変えるもの)
fn break_lease(k: Key, write: bool, nonblock: bool, deleg: bool) -> Result<(), i64> {
    const EWOULDBLOCK: i64 = 11;
    time_out_leases(k);
    let conflict = |l: &Lease| (!deleg || l.deleg) && (write || l.typ == F_WRLCK);
    let Some(v) = leases().get_mut(&k) else { return Ok(()) };
    if !v.iter().any(|l| conflict(l)) {
        return Ok(());
    }
    let secs = LEASE_BREAK_TIME.load(core::sync::atomic::Ordering::Relaxed) as u64;
    let until = if secs > 0 { (crate::timer::ticks() + secs * crate::timer::HZ).max(1) } else { 0 };
    let mut tell = Vec::new();
    for l in v.iter_mut() {
        if !conflict(l) {
            continue;
        }
        if write {
            if l.unlock.is_some() {
                continue;
            }
            l.unlock = Some(until);
        } else {
            if l.unlock.is_some() || l.downgrade.is_some() {
                continue;
            }
            l.downgrade = Some(until);
        }
        tell.push(l.ptr);
    }
    // 知らせる (Linux の lease_break_callback: kill_fasync の POLL_MSG)
    for p in tell {
        crate::fasync::kill(lease_fasync(p), crate::signal::SIGIO, crate::fasync::POLL_MSG);
    }
    loop {
        // まだぶつかるもの (外すのを待っているものの、締め切り)
        let wait = leases().get(&k).and_then(|v| v.iter().find(|l| conflict(l)).map(|l| l.unlock.or(l.downgrade).unwrap_or(0)));
        let Some(deadline) = wait else { return Ok(()) };
        if nonblock {
            return Err(-EWOULDBLOCK);
        }
        if deadline == 0 {
            proc::sleep(lease_chan())?;
        } else {
            proc::sleep_until(lease_chan(), deadline)?;
        }
        time_out_leases(k);
    }
}
