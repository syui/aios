// AF_UNIX のソケット (SOCK_STREAM)。パスに bind して listen し、connect / accept でつなぐ。
// つながった口は socketpair と同じ Kind::Pair (向かい合わせのパイプ 2 本) になる。
// sendmsg / recvmsg の SCM_RIGHTS で fd を渡せる (Wayland の共有メモリやキーマップ)。
//
// 名前はパスの文字列で覚える (ファイルシステムには置かない)。先頭が NUL の抽象名前空間も同じ表
use crate::file::{self, FileRef, Kind, Pipe};
use crate::proc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

const ENOENT: i64 = 2;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const EADDRINUSE: i64 = 98;
const EISCONN: i64 = 106;
const ECONNREFUSED: i64 = 111;

pub const SOCK_NONBLOCK: u64 = 0o4000;
pub const SOCK_CLOEXEC: u64 = 0o2000000;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_CTRUNC: u32 = 0x8;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
/// 1 回の sendmsg で渡せる fd の数 (Linux の SCM_MAX_FD)
const MAX_FDS: usize = 253;

pub struct Unix {
    /// bind した名前
    path: Option<String>,
    /// listen 中なら、accept を待っている口 (サーバー側の OpenFile)
    backlog: Option<VecDeque<FileRef>>,
    /// listen したプロセス (pid, uid, gid)。つないだ側の SO_PEERCRED はこれ (Linux と同じ)
    owner: (u32, u32, u32),
}

/// いまのプロセスの (pid, uid, gid) (SO_PEERCRED の答え)
pub fn me() -> (u32, u32, u32) {
    let c = crate::cred::current();
    (proc::current().tgid, c.euid, c.egid)
}

/// つながった口の相手 (自分が読むパイプに書くほう)。わからなければ None
pub fn peer_cred(f: &FileRef) -> Option<(u32, u32, u32)> {
    match &f.borrow().kind {
        Kind::Pair(rx, _) => rx.borrow().cred,
        _ => None,
    }
}

/// 口の両側のパイプに、書くほうのプロセスを覚えさせる (a に書くのは wa、b に書くのは wb)
pub fn set_creds(k: &Kind, rx: (u32, u32, u32), tx: (u32, u32, u32)) {
    if let Kind::Pair(r, t) = k {
        r.borrow_mut().cred = Some(rx);
        t.borrow_mut().cred = Some(tx);
    }
}

pub type UnixRef = Rc<RefCell<Unix>>;

type R = Result<i64, i64>;

static mut NAMES: BTreeMap<String, Weak<RefCell<Unix>>> = BTreeMap::new();

fn names() -> &'static mut BTreeMap<String, Weak<RefCell<Unix>>> {
    unsafe { &mut *(&raw mut NAMES) }
}

impl Drop for Unix {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            // 自分の名前なら消す (Weak が死んでいるもの)
            if names().get(p).is_some_and(|w| w.upgrade().is_none()) {
                names().remove(p);
            }
        }
    }
}

fn key(u: &UnixRef) -> usize {
    Rc::as_ptr(u) as usize
}

pub fn new_kind() -> Kind {
    Kind::Unix(Rc::new(RefCell::new(Unix { path: None, backlog: None, owner: (0, 0, 0) })))
}

/// poll 用: listen 中で待っている相手がいれば読める
pub fn readiness(u: &UnixRef) -> (bool, bool, bool) {
    let u = u.borrow();
    (u.backlog.as_ref().is_some_and(|q| !q.is_empty()), false, false)
}

/// struct sockaddr_un から名前 (相対パスは cwd から、抽象名前空間は "@" をつける)
fn read_name(addr: usize, len: usize) -> Result<String, i64> {
    if len < 3 || len > 110 {
        return Err(-EINVAL);
    }
    let mut b = alloc::vec![0u8; len];
    proc::current().pt().copy_in(&mut b, addr).ok_or(-EFAULT)?;
    if u16::from_le_bytes([b[0], b[1]]) != 1 {
        return Err(-EINVAL);
    }
    let raw = &b[2..];
    if raw[0] == 0 {
        return Ok(alloc::format!("@{}", String::from_utf8_lossy(&raw[1..])));
    }
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let p = core::str::from_utf8(&raw[..end]).map_err(|_| -EINVAL)?;
    Ok(crate::vfs::normalize(&proc::current_cwd(), p))
}

fn unix_of(f: &FileRef) -> Option<UnixRef> {
    match &f.borrow().kind {
        Kind::Unix(u) => Some(u.clone()),
        _ => None,
    }
}

pub fn bind(f: &FileRef, addr: usize, len: usize) -> R {
    let u = unix_of(f).ok_or(-EINVAL)?;
    let name = read_name(addr, len)?;
    // 砂場: ファイルシステムの名前なら、そのディレクトリに MAKE_SOCK
    if name.starts_with('/') {
        crate::landlock::check_parent(name.trim_start_matches('/'), crate::landlock::MAKE_SOCK)?;
    }
    if u.borrow().path.is_some() {
        return Err(-EINVAL);
    }
    if names().get(&name).is_some_and(|w| w.upgrade().is_some()) {
        return Err(-EADDRINUSE);
    }
    names().insert(name.clone(), Rc::downgrade(&u));
    u.borrow_mut().path = Some(name);
    Ok(0)
}

pub fn listen(f: &FileRef) -> R {
    let u = unix_of(f).ok_or(-EINVAL)?;
    let mut u = u.borrow_mut();
    if u.path.is_none() {
        return Err(-EINVAL);
    }
    if u.backlog.is_none() {
        u.backlog = Some(VecDeque::new());
    }
    u.owner = me();
    Ok(0)
}

/// つなぐ: 向かい合わせのパイプを作り、片方を相手の backlog へ、もう片方をこの口にする
pub fn connect(f: &FileRef, addr: usize, len: usize) -> R {
    if unix_of(f).is_none() {
        return Err(-EISCONN);
    }
    let name = read_name(addr, len)?;
    let l = names().get(&name).and_then(|w| w.upgrade()).ok_or(-ENOENT)?;
    let (mine, theirs) = Pipe::pair();
    {
        let mut lb = l.borrow_mut();
        // 自分が読むほうに書くのは listen したプロセス、相手が読むほうに書くのは自分
        set_creds(&mine, lb.owner, me());
        let q = lb.backlog.as_mut().ok_or(-ECONNREFUSED)?;
        q.push_back(file::new(theirs, file::O_RDWR));
    }
    proc::wakeup(key(&l));
    proc::poll_wake(key(&l));
    f.borrow_mut().kind = mine;
    Ok(0)
}

pub fn accept(f: &FileRef, flags: u64) -> R {
    let u = unix_of(f).ok_or(-EINVAL)?;
    loop {
        let next = {
            let mut ub = u.borrow_mut();
            ub.backlog.as_mut().ok_or(-EINVAL)?.pop_front()
        };
        if let Some(c) = next {
            if flags & SOCK_NONBLOCK != 0 {
                c.borrow_mut().flags |= file::O_NONBLOCK;
            }
            let fd = proc::current().files().add(c, flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
            return Ok(fd as i64);
        }
        if f.borrow().flags & file::O_NONBLOCK != 0 {
            return Err(-EAGAIN);
        }
        proc::sleep(key(&u))?;
    }
}

/// getsockname / getpeername: 名前は返さず、AF_UNIX だということだけ
pub fn write_family(addr: usize, lenp: usize) -> R {
    if addr == 0 || lenp == 0 {
        return Ok(0);
    }
    let pt = proc::current().pt();
    let mut l = [0u8; 4];
    pt.copy_in(&mut l, lenp).ok_or(-EFAULT)?;
    if u32::from_le_bytes(l) >= 2 {
        pt.copy_out(addr, &1u16.to_le_bytes()).ok_or(-EFAULT)?;
    }
    pt.copy_out(lenp, &2u32.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(0)
}

// ---- SCM_RIGHTS ----

/// msghdr の付帯データ (control, controllen) から送る fd を集める
pub fn take_rights(control: usize, len: usize) -> Result<Vec<FileRef>, i64> {
    let mut out = Vec::new();
    if control == 0 || len < 16 {
        return Ok(out);
    }
    let mut b = alloc::vec![0u8; len.min(4096)];
    proc::current().pt().copy_in(&mut b, control).ok_or(-EFAULT)?;
    let mut at = 0;
    while at + 16 <= b.len() {
        let clen = u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(b[at + 8..at + 12].try_into().unwrap());
        let typ = i32::from_le_bytes(b[at + 12..at + 16].try_into().unwrap());
        if clen < 16 || at + clen > b.len() {
            break;
        }
        if level == SOL_SOCKET && typ == SCM_RIGHTS {
            for c in b[at + 16..at + clen].chunks_exact(4) {
                let fd = i32::from_le_bytes(c.try_into().unwrap());
                let f = proc::current().files().get(fd as u64).cloned().ok_or(-file::EBADF)?;
                out.push(f);
                if out.len() > MAX_FDS {
                    return Err(-EINVAL);
                }
            }
        }
        at += (clen + 7) & !7;
    }
    Ok(out)
}

/// 送るデータの始まりに fd をつける (tx はこの口が書くパイプ)
pub fn attach(tx: &Rc<RefCell<Pipe>>, fds: Vec<FileRef>) {
    if !fds.is_empty() {
        let mut p = tx.borrow_mut();
        let at = p.wrote;
        p.rights.push_back((at, fds));
    }
}

/// 読んだバイト [start, end) についてきた fd を、このプロセスの fd にして付帯データに書く。
/// 書ききれなければ MSG_CTRUNC (fd は閉じる)。戻り値は (controllen, flags)
pub fn deliver(rx: &Rc<RefCell<Pipe>>, end: u64, control: usize, space: usize, flags: u64) -> Result<(usize, u32), i64> {
    let mut fds = Vec::new();
    {
        let mut p = rx.borrow_mut();
        while p.rights.front().is_some_and(|(at, _)| *at < end) {
            fds.extend(p.rights.pop_front().unwrap().1);
        }
    }
    if fds.is_empty() {
        return Ok((0, 0));
    }
    let need = 16 + 4 * fds.len();
    if control == 0 || space < need {
        return Ok((0, MSG_CTRUNC));
    }
    let files = proc::current().files();
    let mut nums = Vec::new();
    for f in fds {
        match files.add(f, flags & MSG_CMSG_CLOEXEC != 0, 0) {
            Some(fd) => nums.push(fd as i32),
            None => break,
        }
    }
    let len = 16 + 4 * nums.len();
    let mut b = Vec::with_capacity(len);
    b.extend_from_slice(&(len as u64).to_le_bytes());
    b.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    b.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
    for n in &nums {
        b.extend_from_slice(&n.to_le_bytes());
    }
    proc::current().pt().copy_out(control, &b).ok_or(-EFAULT)?;
    Ok((((len + 7) & !7).min(space), 0))
}
