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
//! ほかの持ち主が外すまで眠る。POSIX どうしの行きどまり (自分を待っている人を待つ) は EDEADLK (Linux と同じく、
//! 聞く側が OFD なら見ない)。ファイルごとの並びは Linux と同じ: 持ち主ははじめてかけた順、持ち主の中は始まりの順
//! (F_GETLK は、ぶつかるもののうち並びで先のものを答える)
use crate::cell::Bkl;
use crate::file::{FileRef, Kind, OpenFile};
use crate::proc;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;

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
pub fn key(f: &OpenFile) -> Option<Key> {
    Some(match &f.kind {
        Kind::Inode(i, _) => i.id(),
        Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => (usize::MAX, Rc::as_ptr(p) as usize as u64),
        Kind::Epoll(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::PidFd(_) | Kind::Landlock(_) => return None,
        _ => (usize::MAX - 1, f as *const OpenFile as usize as u64),
    })
}

fn ptr_of(f: &OpenFile) -> usize {
    f as *const OpenFile as usize
}

/// 開いたファイルの最後の参照が閉じられた: その flock のロックと OFD ロックを外す
pub fn release_file(file: usize) {
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
}

/// プロセス (ファイルの表 files) が、鍵 key のファイルの fd を 1 つ閉じた: そのファイルの POSIX のロックをみな外す
pub fn release_posix(files: usize, key: Key) {
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
pub fn release_owner(files: usize) {
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

/// 行きどまり: me が待とうとしている blocker (の持ち主) が、めぐって me を待っているか (Linux の posix_locks_deadlock)
fn deadlock(me: Owner, mut blocker: Owner) -> bool {
    for _ in 0..10 {
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
pub fn fcntl(file: &FileRef, files: usize, cmd: u64, arg: usize) -> Result<i64, i64> {
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
pub fn flock(fd: u64, op: u64) -> Result<i64, i64> {
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
