// inotify: ファイルとディレクトリの変化を知らせる (Linux の inotify_init1 / inotify_add_watch / inotify_rm_watch)
//
// watch は inode (InodeRef::id) ごと。ディレクトリを見張ると、その中のものの変化も名前つきで届く。
// できごとは VFS の操作 (sysfile.rs と file.rs) が知らせる: 作る、消す、名前を変える、書く、開く、閉じる、属性。
// read は struct inotify_event (wd, mask, cookie, len, name) を並べて返す。名前は 16 バイトにそろえる (Linux と同じ)。
// 同じできごとが続けば 1 つにまとめる (書くたびの IN_MODIFY があふれないように)。
// 見張っている人がいなければ、知らせるところはすぐ戻る
use crate::cell::Bkl;
use crate::file::{self, Kind};
use crate::proc;
use crate::vfs::{self, InodeRef};
use alloc::collections::VecDeque;
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

const EAGAIN: i64 = 11;
const EBADF: i64 = 9;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const ENOSPC: i64 = 28;

pub const IN_ACCESS: u32 = 0x1;
pub const IN_MODIFY: u32 = 0x2;
pub const IN_ATTRIB: u32 = 0x4;
pub const IN_CLOSE_WRITE: u32 = 0x8;
pub const IN_CLOSE_NOWRITE: u32 = 0x10;
pub const IN_OPEN: u32 = 0x20;
pub const IN_MOVED_FROM: u32 = 0x40;
pub const IN_MOVED_TO: u32 = 0x80;
pub const IN_CREATE: u32 = 0x100;
pub const IN_DELETE: u32 = 0x200;
pub const IN_DELETE_SELF: u32 = 0x400;
pub const IN_MOVE_SELF: u32 = 0x800;
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;

const IN_NONBLOCK: u64 = 0o4000;
const IN_CLOEXEC: u64 = 0o2000000;

// ためておくできごとの数 (こえたら IN_Q_OVERFLOW) と 1 つの inotify の watch の数は
// /proc/sys/fs/inotify/max_queued_events と max_user_watches (sysctl.rs)

struct Watch {
    wd: i32,
    /// 見張っている inode (InodeRef::id)
    id: (usize, u64),
    mask: u32,
}

pub struct Inotify {
    watches: Vec<Watch>,
    /// できごと (struct inotify_event の形のバイト列)
    queue: VecDeque<Vec<u8>>,
    next_wd: i32,
    /// ためたできごとの数 (epoll の EPOLLET が新しいできごとを知るため)
    generation: u64,
}

pub type InotifyRef = Rc<RefCell<Inotify>>;

/// IN_ACCESS を待つ見張りが置かれたことがある (それからは、大きなロックなしの read はしない: file::fast_read_file)
pub static ACCESS_WATCHED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 作る・消すのできごと (IN_CREATE、IN_DELETE、IN_ATTRIB、IN_DELETE_SELF) を待つ見張りが置かれたことがある (それからは、
/// 大きなロックなしの作る・消す (fastdir.rs) はしない: できごとはふつうの道が出す)
pub static NAME_WATCHED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 生きている inotify (見張りがあるかを速く知るため)
static LIST: Bkl<Vec<Weak<RefCell<Inotify>>>> = Bkl::new(Vec::new());
static COOKIE: Bkl<u32> = Bkl::new(0);

#[track_caller]
fn list() -> &'static mut Vec<Weak<RefCell<Inotify>>> {
    LIST.get()
}

/// poll で見張るときの印 (poll_keys) にも
pub fn chan(n: &InotifyRef) -> usize {
    Rc::as_ptr(n) as usize
}

/// inotify_init1(flags)
pub fn init1(flags: u64) -> Result<i64, i64> {
    if flags & !(IN_NONBLOCK | IN_CLOEXEC) != 0 {
        return Err(crate::why::inval());
    }
    let n = Rc::new(RefCell::new(Inotify { watches: Vec::new(), queue: VecDeque::new(), next_wd: 1, generation: 0 }));
    let l = list();
    l.retain(|w| w.strong_count() > 0);
    l.push(Rc::downgrade(&n));
    let fl = file::O_RDONLY | if flags & IN_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 };
    let fd = proc::current().files().add(file::new(Kind::Inotify(n), fl), flags & IN_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn of_fd(fd: u64) -> Result<InotifyRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inotify(n) => Ok(n.clone()),
        _ => Err(crate::why::inval()),
    }
}

/// inotify_add_watch(fd, path, mask) → wd
pub fn add_watch(fd: u64, path: &str, mask: u32) -> Result<i64, i64> {
    let n = of_fd(fd)?;
    if mask & IN_ALL_EVENTS == 0 || (mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0) {
        return Err(crate::why::inval());
    }
    let cwd = proc::current().files().cwd.clone();
    let (_, ino) = vfs::lookup(&cwd, path, mask & IN_DONT_FOLLOW == 0)?;
    if mask & IN_ONLYDIR != 0 && !ino.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    // 見張るには読めること
    crate::cred::current().check(&ino.meta(), crate::cred::R)?;
    let id = ino.id();
    let keep = mask & (IN_ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
    // 大きなロックなしの read は IN_ACCESS を、pwrite は IN_MODIFY を出さない: これからはふつうの道で
    // (いまある watch に足すときも)
    if keep & IN_ACCESS != 0 {
        ACCESS_WATCHED.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    if keep & IN_MODIFY != 0 {
        crate::vm::write_permits_revoke();
    }
    if keep & (IN_CREATE | IN_DELETE | IN_ATTRIB | IN_DELETE_SELF) != 0 {
        NAME_WATCHED.store(true, core::sync::atomic::Ordering::SeqCst);
    }
    // 大きなロックなしの open / close (file の棚) は IN_OPEN と IN_CLOSE を出さない: これからはふつうの道で。
    // 棚にしまってあるものは、ここで閉じる (見張りが置かれる前なので、できごとはない)
    if keep & (IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE) != 0 {
        crate::file::OPEN_WATCHED.store(true, core::sync::atomic::Ordering::SeqCst);
        crate::file::shelf_flush_all();
    }
    // いま大きなロックなしで作っている・消しているもの (fastdir.rs) が終わるのを待つ (ext4 のロック): あとから来るものは、
    // ロックを取ってから印を見るので、ふつうの道へ行く
    drop(crate::extfs::fs_lock());
    let mut n = n.borrow_mut();
    if let Some(w) = n.watches.iter_mut().find(|w| w.id == id) {
        if mask & IN_MASK_CREATE != 0 {
            return Err(-EEXIST);
        }
        w.mask = if mask & IN_MASK_ADD != 0 { w.mask | keep } else { keep };
        return Ok(w.wd as i64);
    }
    if n.watches.len() >= crate::sysctl::INOTIFY_MAX_WATCHES.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(-ENOSPC);
    }
    let wd = n.next_wd;
    n.next_wd += 1;
    n.watches.push(Watch { wd, id, mask: keep });
    Ok(wd as i64)
}

/// inotify_rm_watch(fd, wd)
pub fn rm_watch(fd: u64, wd: i32) -> Result<i64, i64> {
    let n = of_fd(fd)?;
    let found = {
        let mut b = n.borrow_mut();
        let i = b.watches.iter().position(|w| w.wd == wd).ok_or_else(|| crate::why::inval())?;
        b.watches.remove(i);
        true
    };
    if found {
        push(&n, wd, IN_IGNORED, 0, None);
    }
    Ok(0)
}

/// できごとを 1 つためる (直前と同じなら、まとめる)
fn push(n: &InotifyRef, wd: i32, mask: u32, cookie: u32, name: Option<&str>) {
    let mut ev = Vec::with_capacity(16);
    ev.extend_from_slice(&wd.to_le_bytes());
    ev.extend_from_slice(&mask.to_le_bytes());
    ev.extend_from_slice(&cookie.to_le_bytes());
    let len = name.map_or(0, |s| (s.len() + 1).next_multiple_of(16));
    ev.extend_from_slice(&(len as u32).to_le_bytes());
    if let Some(s) = name {
        ev.extend_from_slice(s.as_bytes());
        ev.resize(16 + len, 0);
    }
    {
        let mut b = n.borrow_mut();
        if b.queue.back() == Some(&ev) {
            return;
        }
        if b.queue.len() >= crate::sysctl::INOTIFY_MAX_QUEUED.load(core::sync::atomic::Ordering::Relaxed) {
            // あふれた: 最後に 1 つだけ IN_Q_OVERFLOW (wd は -1)
            let mut o = Vec::with_capacity(16);
            o.extend_from_slice(&(-1i32).to_le_bytes());
            o.extend_from_slice(&IN_Q_OVERFLOW.to_le_bytes());
            o.extend_from_slice(&[0; 8]);
            if b.queue.back() != Some(&o) {
                b.queue.push_back(o);
            }
            return;
        }
        b.queue.push_back(ev);
        b.generation += 1;
    }
    proc::wakeup(chan(n));
    proc::poll_wake(chan(n));
}

/// 見張っている inotify があるか (なければ知らせるところは何もしない)
pub fn active() -> bool {
    list().iter().any(|w| w.upgrade().is_some_and(|n| !n.borrow().watches.is_empty()))
}

/// IN_MODIFY を待っている watch があるか (ext4 が大きなロックなしの pwrite を許すまえに: vm::write_permit)
pub fn modify_watched() -> bool {
    wanted(IN_MODIFY, (0, 0)).0
}

/// ev を待っている watch があるか、と、そのうち id 以外 (親ディレクトリかもしれない) のものがあるか
fn wanted(ev: u32, id: (usize, u64)) -> (bool, bool) {
    let (mut any, mut others) = (false, false);
    for w in list().iter() {
        let Some(n) = w.upgrade() else { continue };
        for w in n.borrow().watches.iter().filter(|w| w.mask & ev != 0) {
            any = true;
            others |= w.id != id;
        }
    }
    (any, others)
}

/// id の inode を見張るものに、できごと (mask。IN_ISDIR はついていてよい) を届ける。name はディレクトリの中のもの
fn deliver(id: (usize, u64), mask: u32, cookie: u32, name: Option<&str>) {
    let ev = mask & IN_ALL_EVENTS;
    for w in list().clone() {
        let Some(n) = w.upgrade() else { continue };
        let hits: Vec<(i32, bool)> = n.borrow().watches.iter().filter(|w| w.id == id && w.mask & ev != 0).map(|w| (w.wd, w.mask & IN_ONESHOT != 0)).collect();
        for (wd, oneshot) in hits {
            push(&n, wd, mask, cookie, name);
            if oneshot {
                n.borrow_mut().watches.retain(|w| w.wd != wd);
                push(&n, wd, IN_IGNORED, 0, None);
            }
        }
    }
}

/// ディレクトリの中の name で起きたこと (作る、消す、名前を変える)
pub fn dir_event(dir: &InodeRef, name: &str, mask: u32, cookie: u32) {
    if active() {
        deliver(dir.id(), mask, cookie, Some(name));
    }
}

/// inode そのものに起きたこと (見張っているのがその inode のとき。名前なし)
pub fn self_event(ino: &InodeRef, mask: u32) {
    if active() {
        deliver(ino.id(), mask, 0, None);
    }
}

/// 開いたファイルに起きたこと (書く、開く、閉じる、属性): その inode と、入っているディレクトリ (名前つき) に。
/// path は開いたときのパス (先頭 / なし)
pub fn file_event(path: &str, ino: &InodeRef, mask: u32) {
    // 見張っている人がいても、このできごと (読むたびの IN_ACCESS など) をだれも待っていなければすぐ戻る。
    // 親ディレクトリを探す (パスをたどる) のは、ほかのものを見張っている (ディレクトリかもしれない) ときだけ
    let id = ino.id();
    let (wanted, others) = wanted(mask & IN_ALL_EVENTS, id);
    if !wanted {
        return;
    }
    let mask = if ino.meta().is_dir() { mask | IN_ISDIR } else { mask };
    deliver(id, mask, 0, None);
    if others
        && let Ok((dir, name)) = vfs::parent_of("", &alloc::format!("/{}", path))
        && !name.is_empty()
    {
        deliver(dir.id(), mask, 0, Some(&name));
    }
}

/// inode がなくなった (リンクが 0 か、ディレクトリを消した): IN_DELETE_SELF、それから watch を外して IN_IGNORED
pub fn gone(ino: &InodeRef) {
    if !active() {
        return;
    }
    let id = ino.id();
    deliver(id, IN_DELETE_SELF, 0, None);
    for w in list().clone() {
        let Some(n) = w.upgrade() else { continue };
        let wds: Vec<i32> = n.borrow().watches.iter().filter(|w| w.id == id).map(|w| w.wd).collect();
        n.borrow_mut().watches.retain(|w| w.id != id);
        for wd in wds {
            push(&n, wd, IN_IGNORED, 0, None);
        }
    }
}

/// rename の MOVED_FROM と MOVED_TO をつなぐ番号
pub fn cookie() -> u32 {
    let c = COOKIE.get();
    *c = c.wrapping_add(1).max(1);
    *c
}

pub fn read(n: &InotifyRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    loop {
        {
            let mut b = n.borrow_mut();
            if let Some(first) = b.queue.front() {
                if dst.len() < first.len() {
                    return Err(crate::why::inval());
                }
                let mut off = 0;
                while let Some(ev) = b.queue.front() {
                    if off + ev.len() > dst.len() {
                        break;
                    }
                    dst[off..off + ev.len()].copy_from_slice(ev);
                    off += ev.len();
                    b.queue.pop_front();
                }
                return Ok(off);
            }
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        proc::sleep(chan(n))?;
    }
}

pub fn readiness(n: &InotifyRef) -> (bool, bool, bool) {
    (!n.borrow().queue.is_empty(), false, false)
}

pub fn generation(n: &InotifyRef) -> u64 {
    n.borrow().generation
}

/// FIONREAD: 読めるバイト数
pub fn pending(n: &InotifyRef) -> usize {
    n.borrow().queue.iter().map(|e| e.len()).sum()
}
