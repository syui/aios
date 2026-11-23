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

// できごとの番号と見張りの数は core の入り口 (vfs::notify。Linux の fsnotify) のもの
pub use crate::vfs::notify::{ACCESS_WATCHED, IN_ACCESS, IN_ATTRIB, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE, IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_ISDIR, IN_MODIFY, IN_MOVED_FROM, IN_MOVED_TO, IN_OPEN, NAME_WATCHED};
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
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

/// /proc/PID/fdinfo の行 (Linux の inotify_show_fdinfo): 見張りごとに wd、inode、dev、待つできごと。file handle (Linux の
/// CONFIG_EXPORTFS の fhandle-bytes など) は、aios に name_to_handle_at がないので出さない
pub fn fdinfo(n: &InotifyRef) -> alloc::string::String {
    const USER_MASK: u32 = 0xfff | 0x0400_0000 | 0x8000_0000;
    let mut s = alloc::string::String::new();
    for w in n.borrow().watches.iter() {
        let (ma, mi) = crate::vfs::fs_dev(w.id.0);
        s.push_str(&alloc::format!("inotify wd:{:x} ino:{:x} sdev:{:x} mask:{:x} ignored_mask:0 \n", w.wd, w.id.1, ma << 20 | mi, w.mask & USER_MASK));
    }
    s
}

/// ためておくできごとの数。こえたら IN_Q_OVERFLOW (/proc/sys は sysctl.rs)
pub static INOTIFY_MAX_QUEUED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(16384);
/// 1 つの inotify の watch の数
pub static INOTIFY_MAX_WATCHES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(8192);

const NAME_BITS: u32 = IN_CREATE | IN_DELETE | IN_ATTRIB | IN_DELETE_SELF;
const OPEN_BITS: u32 = IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE;

/// 見張り 1 つ (mask) のぶん、ACCESS_WATCHED、NAME_WATCHED、file::OPEN_WATCHED、file::MODIFY_WATCHED を足す (add) か引く。前は「一度でも置かれた」の
/// 印で、見張りを外しても inotify を閉じても、大きなロックなしの道は切れたままだった (先に inotify を使ったテストのあとで、
/// test/createrace.c の fast-path が落ちた)。足すのは見張りを置く前、引くのは外したあと。数なので、大きなロックの外で
/// inotify が捨てられても (Drop) よい
fn count(mask: u32, add: bool) {
    use core::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let f = |a: &AtomicUsize, bits: u32| {
        if mask & bits != 0 {
            if add {
                a.fetch_add(1, SeqCst);
            } else {
                a.fetch_sub(1, SeqCst);
            }
        }
    };
    f(&ACCESS_WATCHED, IN_ACCESS);
    f(&NAME_WATCHED, NAME_BITS);
    f(&crate::file::OPEN_WATCHED, OPEN_BITS);
    f(&crate::file::MODIFY_WATCHED, IN_MODIFY);
}

impl Drop for Inotify {
    fn drop(&mut self) {
        for w in &self.watches {
            count(w.mask, false);
        }
    }
}

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
    let fd = proc::current().files().add(file::new(Kind::Ops(Rc::new(InotifyFile(n))), fl), flags & IN_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn of_fd(fd: u64) -> Result<InotifyRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let f = f.borrow();
    match f.kind.ops::<InotifyFile>() {
        Some(n) => Ok(n.0.clone()),
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
    // 同じ inode の見張りがあれば、その印をかえる (IN_MASK_ADD なら足す)
    let old = n.borrow().watches.iter().find(|w| w.id == id).map(|w| w.mask);
    if old.is_some() && mask & IN_MASK_CREATE != 0 {
        return Err(-EEXIST);
    }
    if old.is_none() && n.borrow().watches.len() >= INOTIFY_MAX_WATCHES.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(-ENOSPC);
    }
    let new = match old {
        Some(m) if mask & IN_MASK_ADD != 0 => m | keep,
        _ => keep,
    };
    // 大きなロックなしの read は IN_ACCESS を、pwrite は IN_MODIFY を、作る・消すはそのできごとを、open / close (file の棚) は
    // IN_OPEN と IN_CLOSE を出さない: 見張りの数を先に足して、これからはふつうの道で
    if new & IN_MODIFY != 0 {
        crate::vm::write_permits_revoke();
    }
    count(new, true);
    // 棚にしまってあるものは、ここで閉じる (見張りが置かれる前なので、できごとはない)
    if new & OPEN_BITS != 0 {
        crate::file::shelf_flush_all();
    }
    // いま大きなロックなしで作っている・消しているもの (fastdir.rs) が終わるのを待つ (ext4 のロック): あとから来るものは、
    // ロックを取ってから数を見るので、ふつうの道へ行く
    drop(crate::smp::fs_lock());
    let mut n = n.borrow_mut();
    if let Some(w) = n.watches.iter_mut().find(|w| w.id == id) {
        count(w.mask, false);
        w.mask = new;
        return Ok(w.wd as i64);
    }
    let wd = n.next_wd;
    n.next_wd += 1;
    n.watches.push(Watch { wd, id, mask: new });
    Ok(wd as i64)
}

/// inotify_rm_watch(fd, wd)
pub fn rm_watch(fd: u64, wd: i32) -> Result<i64, i64> {
    let n = of_fd(fd)?;
    let found = {
        let mut b = n.borrow_mut();
        let i = b.watches.iter().position(|w| w.wd == wd).ok_or_else(|| crate::why::inval())?;
        let w = b.watches.remove(i);
        count(w.mask, false);
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
        if b.queue.len() >= INOTIFY_MAX_QUEUED.load(core::sync::atomic::Ordering::Relaxed) {
            // あふれた: 最後に 1 つだけ IN_Q_OVERFLOW (wd は -1)
            let mut o = Vec::with_capacity(16);
            o.extend_from_slice(&(-1i32).to_le_bytes());
            o.extend_from_slice(&IN_Q_OVERFLOW.to_le_bytes());
            o.extend_from_slice(&[0; 8]);
            if b.queue.back() == Some(&o) {
                return;
            }
            b.queue.push_back(o);
        } else {
            b.queue.push_back(ev);
        }
        b.generation += 1;
    }
    proc::wakeup(chan(n));
    proc::poll_wake(chan(n));
    // O_ASYNC (Linux の fsnotify_add_event の kill_fasync。まとめたものはのぞく)
    crate::fasync::kill(chan(n), crate::signal::SIGIO, crate::fasync::POLL_IN);
}

/// 見張っている inotify があるか (なければ知らせるところは何もしない)
pub fn active() -> bool {
    !dnotify().is_empty() || list().iter().any(|w| w.upgrade().is_some_and(|n| !n.borrow().watches.is_empty()))
}

/// IN_MODIFY を待っている watch があるか (ext4 が大きなロックなしの pwrite を許すまえに: vm::write_permit)
pub fn modify_watched() -> bool {
    wanted(IN_MODIFY, (0, 0)).0
}

/// ev を待っている watch があるか、と、そのうち id 以外 (親ディレクトリかもしれない) のものがあるか
fn wanted(ev: u32, id: (usize, u64)) -> (bool, bool) {
    let (mut any, mut others) = (false, false);
    // F_NOTIFY (ディレクトリを見張る: 中のもののできごとは親に)
    for d in dnotify().iter().filter(|d| dn_bits(ev) & d.mask != 0) {
        any = true;
        others |= d.id != id;
    }
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
    dn_deliver(id, mask, name.is_some());
    for w in list().clone() {
        let Some(n) = w.upgrade() else { continue };
        let hits: Vec<(i32, bool)> = n.borrow().watches.iter().filter(|w| w.id == id && w.mask & ev != 0).map(|w| (w.wd, w.mask & IN_ONESHOT != 0)).collect();
        for (wd, oneshot) in hits {
            push(&n, wd, mask, cookie, name);
            if oneshot {
                n.borrow_mut().watches.retain(|w| {
                    let keep = w.wd != wd;
                    if !keep {
                        count(w.mask, false);
                    }
                    keep
                });
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
        n.borrow_mut().watches.retain(|w| {
            let keep = w.id != id;
            if !keep {
                count(w.mask, false);
            }
            keep
        });
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

/// システムコール (main.rs が syscall::register で置く。Linux の fs/notify/inotify/inotify_user.c)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[INOTIFY_INIT1, INOTIFY_ADD_WATCH, INOTIFY_RM_WATCH]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        INOTIFY_INIT1 => init1(a[0]),
        INOTIFY_ADD_WATCH => {
            let b = crate::proc::current().pt().copy_in_str(a[1] as usize, 4096).ok_or(-14)?; // EFAULT
            add_watch(a[0], &alloc::string::String::from_utf8(b).map_err(|_| -2)?, a[2] as u32) // ENOENT
        }
        INOTIFY_RM_WATCH => rm_watch(a[0], a[1] as i32),
        _ => Err(-38),
    }
}

/// inotify の fd の中身 (file::Kind::Ops。Linux の inotify_fops)
pub struct InotifyFile(pub InotifyRef);

impl file::FileOps for InotifyFile {
    fn describe(&self) -> alloc::string::String {
        "anon_inode:inotify".into()
    }
    fn fasync_key(&self) -> Option<usize> {
        Some(chan(&self.0))
    }
    fn stream(&self) -> bool {
        true
    }
    fn read(&self, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        read(&self.0, dst, nonblock)
    }
    fn fdinfo(&self) -> alloc::string::String {
        fdinfo(&self.0)
    }
    fn event_gen(&self) -> u64 {
        generation(&self.0)
    }
    fn pending(&self) -> Option<u64> {
        Some(pending(&self.0) as u64)
    }
    fn fionread(&self) -> Option<Result<usize, i64>> {
        Some(Ok(pending(&self.0)))
    }
    fn readiness(&self) -> (bool, bool, bool) {
        readiness(&self.0)
    }
    fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        Some(alloc::vec![chan(&self.0)])
    }
}

/// core の入り口 (vfs::notify) につなぐもの (main.rs)
pub struct Ops;

pub static OPS: Ops = Ops;

impl crate::vfs::notify::Notify for Ops {
    fn active(&self) -> bool {
        active()
    }
    fn dir_event(&self, dir: &InodeRef, name: &str, mask: u32, cookie: u32) {
        dir_event(dir, name, mask, cookie);
    }
    fn self_event(&self, ino: &InodeRef, mask: u32) {
        self_event(ino, mask);
    }
    fn file_event(&self, path: &str, ino: &InodeRef, mask: u32) {
        file_event(path, ino, mask);
    }
    fn gone(&self, ino: &InodeRef) {
        gone(ino);
    }
    fn cookie(&self) -> u32 {
        cookie()
    }
    fn modify_watched(&self) -> bool {
        modify_watched()
    }
    fn dirnotify(&self, fd: i32, f: &file::FileRef, files: usize, arg: u32) -> Result<i64, i64> {
        dirnotify(fd, f, files, arg)
    }
    fn closed(&self, files: usize, file: usize) {
        dn_closed(files, file)
    }
}

// ---- dnotify (Linux の fs/notify/dnotify: fcntl の F_NOTIFY) ----
//
// ディレクトリの開いたものに F_NOTIFY で DN_* を置くと、その中のもの (と、そのディレクトリ自身) にできごとがあれば、
// ファイルの持ち主に知らせる (F_SETSIG のシグナルなら POLL_MSG と si_fd: fasync.rs)。DN_MULTISHOT がなければ 1 度で外れる。
// 置いたプロセス (ファイルの表) がその fd を閉じると外れる (dup したものを閉じても: Linux の dnotify_flush)

pub const DN_ACCESS: u32 = 0x1;
pub const DN_MODIFY: u32 = 0x2;
pub const DN_CREATE: u32 = 0x4;
pub const DN_DELETE: u32 = 0x8;
pub const DN_RENAME: u32 = 0x10;
pub const DN_ATTRIB: u32 = 0x20;
pub const DN_MULTISHOT: u32 = 0x8000_0000;

/// dir-notify-enable (sysctl.rs)
pub static DIR_NOTIFY_ENABLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

struct Dn {
    file: Weak<RefCell<file::OpenFile>>,
    ptr: usize,
    /// 置いたプロセスのファイルの表
    files: usize,
    fd: i32,
    mask: u32,
    /// 見張るディレクトリ
    id: (usize, u64),
}

static DNOTIFY: Bkl<Vec<Dn>> = Bkl::new(Vec::new());

fn dnotify() -> &'static mut Vec<Dn> {
    DNOTIFY.get()
}

/// inotify のできごとから DN_* (Linux の convert_arg の逆)
fn dn_bits(ev: u32) -> u32 {
    let mut b = 0;
    if ev & IN_ACCESS != 0 {
        b |= DN_ACCESS;
    }
    if ev & IN_MODIFY != 0 {
        b |= DN_MODIFY;
    }
    if ev & IN_ATTRIB != 0 {
        b |= DN_ATTRIB;
    }
    if ev & (IN_CREATE | IN_MOVED_TO) != 0 {
        b |= DN_CREATE;
    }
    if ev & (IN_DELETE | IN_MOVED_FROM) != 0 {
        b |= DN_DELETE;
    }
    if ev & IN_MOVED_FROM != 0 {
        b |= DN_RENAME;
    }
    b
}

/// DN_* を、ロックなしの道を止める数のための inotify のできごとに
fn in_bits(dn: u32) -> u32 {
    let mut b = 0;
    if dn & DN_ACCESS != 0 {
        b |= IN_ACCESS;
    }
    if dn & DN_MODIFY != 0 {
        b |= IN_MODIFY;
    }
    if dn & DN_ATTRIB != 0 {
        b |= IN_ATTRIB;
    }
    if dn & DN_CREATE != 0 {
        b |= IN_CREATE | IN_MOVED_TO;
    }
    if dn & (DN_DELETE | DN_RENAME) != 0 {
        b |= IN_DELETE | IN_MOVED_FROM;
    }
    b
}

fn dn_count() {
    crate::vfs::notify::DNOTIFY.store(dnotify().len(), core::sync::atomic::Ordering::Relaxed);
}

/// 見張るディレクトリ id に届いたできごと: 中のもの (child) か、ディレクトリ自身 (IN_ISDIR) のものだけ (Linux の
/// dnotify_handle_event)。名前を変えたもの (IN_MOVED_FROM) は、DN_RENAME と DN_DELETE の 2 つのできごと (Linux の
/// FS_RENAME と FS_MOVED_FROM)
fn dn_deliver(id: (usize, u64), mask: u32, child: bool) {
    if dnotify().is_empty() || !(child || mask & IN_ISDIR != 0) {
        return;
    }
    let ev = mask & IN_ALL_EVENTS;
    let events: Vec<u32> = if ev & IN_MOVED_FROM != 0 { alloc::vec![DN_RENAME, DN_DELETE] } else { alloc::vec![dn_bits(ev)] };
    for e in events {
        let mut send = Vec::new();
        dnotify().retain(|d| {
            if d.id != id || d.mask & e == 0 {
                return true;
            }
            send.push((d.file.clone(), d.fd));
            if d.mask & DN_MULTISHOT != 0 {
                return true;
            }
            count(in_bits(d.mask), false);
            false
        });
        dn_count();
        for (f, fd) in send {
            if let Some(f) = f.upgrade() {
                crate::fasync::send_file(&f, fd, crate::fasync::POLL_MSG);
            }
        }
    }
}

/// fcntl の F_NOTIFY (Linux の fcntl_dirnotify)
pub fn dirnotify(fd: i32, f: &file::FileRef, files: usize, arg: u32) -> Result<i64, i64> {
    if DIR_NOTIFY_ENABLE.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return Err(crate::why::inval());
    }
    let ptr = Rc::as_ptr(f) as usize;
    // 0 (DN_MULTISHOT だけも) は外す
    if arg & !DN_MULTISHOT == 0 {
        dn_closed(files, ptr);
        return Ok(0);
    }
    let id = match &f.borrow().kind {
        Kind::Inode(i, _) if i.meta().is_dir() => i.id(),
        _ => return Err(-20), // ENOTDIR
    };
    // 持ち主がまだなければ自分のプロセスに (Linux の __f_setown の force 0)
    crate::fasync::set(f, proc::current().tgid, crate::fasync::PidType::Tgid, false);
    // 同じ表の同じファイルなら DN_* を足す (Linux の attach_dn)
    let v = dnotify();
    if let Some(d) = v.iter_mut().find(|d| d.files == files && d.ptr == ptr) {
        count(in_bits(d.mask), false);
        d.fd = fd;
        d.mask |= arg;
        count(in_bits(d.mask), true);
    } else {
        v.push(Dn { file: Rc::downgrade(f), ptr, files, fd, mask: arg, id });
        count(in_bits(arg), true);
    }
    dn_count();
    Ok(0)
}

/// fd を閉じた (file が 0 ならファイルの表がなくなった): その F_NOTIFY を外す
fn dn_closed(files: usize, file: usize) {
    dnotify().retain(|d| {
        let gone = d.files == files && (file == 0 || d.ptr == file) || d.file.strong_count() == 0;
        if gone {
            count(in_bits(d.mask), false);
        }
        !gone
    });
    dn_count();
}
