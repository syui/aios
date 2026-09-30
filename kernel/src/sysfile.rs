// ファイルまわりのシステムコール
use crate::file::{self, FileRef, Kind, Pipe, Stat, EBADF, EINVAL};
use crate::fs;
use crate::proc::{self, Fd};
use crate::vfs::{self, InodeRef, NewNode, S_IFMT};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const ENXIO: i64 = 6;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const ERANGE: i64 = 34;

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_SYMLINK_FOLLOW: u64 = 0x400;
const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
// arm64 の値 (x86 とは違う)
const O_DIRECTORY: u64 = 0o40000;
const O_NOFOLLOW: u64 = 0o100000;
const O_CLOEXEC: u64 = 0o2000000;

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_SETPIPE_SZ: u64 = 1031;
const F_GETPIPE_SZ: u64 = 1032;
const FD_CLOEXEC: u64 = 1;

const O_NONBLOCK: u32 = 0o4000;

const TCGETS: u64 = 0x5401;
const FIONBIO: u64 = 0x5421;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCGPGRP: u64 = 0x540f;

const UMASK: u32 = 0o022;

type R = Result<i64, i64>;

fn user_str(va: usize) -> Result<String, i64> {
    let b = proc::current().pt().copy_in_str(va, 4096).ok_or(-EFAULT)?;
    String::from_utf8(b).map_err(|_| -ENOENT)
}

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn file_of(fd: u64) -> Result<FileRef, i64> {
    proc::current().files().get(fd).cloned().ok_or(-EBADF)
}

fn inode_of(fd: u64) -> Result<InodeRef, i64> {
    let f = file_of(fd)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, _) => Ok(i.clone()),
        _ => Err(-EINVAL),
    }
}

/// dirfd と path から、探索の基準にするディレクトリ (先頭 / なし)
fn base_dir(dirfd: i64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return Ok(proc::current().files().cwd.clone());
    }
    let f = file_of(dirfd as u64)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, p) if i.meta().is_dir() => Ok(p.clone()),
        _ => Err(-ENOTDIR),
    }
}

/// dirfd + path の inode。path が空で AT_EMPTY_PATH なら dirfd 自身
fn at(dirfd: i64, pathp: usize, flags: u64) -> Result<InodeRef, i64> {
    let path = user_str(pathp)?;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH != 0 {
            return inode_of(dirfd as u64);
        }
        return Err(-ENOENT);
    }
    let base = base_dir(dirfd, &path)?;
    vfs::resolve(&base, &path, flags & AT_SYMLINK_NOFOLLOW == 0)
}

fn parent_at(dirfd: i64, pathp: usize) -> Result<(InodeRef, String), i64> {
    let path = user_str(pathp)?;
    let base = base_dir(dirfd, &path)?;
    vfs::parent_of(&base, &path)
}

pub fn read(fd: u64, buf: usize, len: usize) -> R {
    let f = file_of(fd)?;
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    let n = f.borrow_mut().read(&mut tmp)?;
    out(buf, &tmp[..n])?;
    Ok(n as i64)
}

pub fn write(fd: u64, buf: usize, len: usize) -> R {
    let f = file_of(fd)?;
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    proc::current().pt().copy_in(&mut tmp, buf).ok_or(-EFAULT)?;
    let r = f.borrow_mut().write(&tmp);
    if r == Err(-file::EPIPE) && proc::current().sig_handlers[proc::SIGPIPE as usize] == 0 {
        // 読み手のいないパイプ: SIGPIPE の既定動作で終わる
        drop(f);
        proc::die(proc::SIGPIPE);
    }
    Ok(r? as i64)
}

pub fn pread(fd: u64, buf: usize, len: usize, off: i64) -> R {
    let ino = inode_of(fd).map_err(|_| -29)?; // ESPIPE
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    let n = ino.read_at(off.max(0) as usize, &mut tmp)?;
    out(buf, &tmp[..n])?;
    Ok(n as i64)
}

pub fn pwrite(fd: u64, buf: usize, len: usize, off: i64) -> R {
    let ino = inode_of(fd).map_err(|_| -29)?;
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    proc::current().pt().copy_in(&mut tmp, buf).ok_or(-EFAULT)?;
    let n = ino.write_at(off.max(0) as usize, &tmp)?;
    Ok(n as i64)
}

fn iovecs(iov: usize, cnt: usize) -> Result<Vec<(usize, usize)>, i64> {
    if cnt > 1024 {
        return Err(-EINVAL);
    }
    let mut v = Vec::with_capacity(cnt);
    for i in 0..cnt {
        let mut b = [0u8; 16];
        proc::current().pt().copy_in(&mut b, iov + i * 16).ok_or(-EFAULT)?;
        v.push((
            u64::from_le_bytes(b[..8].try_into().unwrap()) as usize,
            u64::from_le_bytes(b[8..].try_into().unwrap()) as usize,
        ));
    }
    Ok(v)
}

pub fn writev(fd: u64, iov: usize, cnt: usize) -> R {
    let mut total = 0;
    for (base, len) in iovecs(iov, cnt)? {
        let n = write(fd, base, len)?;
        total += n;
        if (n as usize) < len {
            break;
        }
    }
    Ok(total)
}

pub fn readv(fd: u64, iov: usize, cnt: usize) -> R {
    let mut total = 0;
    for (base, len) in iovecs(iov, cnt)? {
        if len == 0 {
            continue;
        }
        let n = read(fd, base, len)?;
        total += n;
        if (n as usize) < len {
            break;
        }
    }
    Ok(total)
}

pub fn openat(dirfd: i64, pathp: usize, flags: u64, mode: u64) -> R {
    let path = user_str(pathp)?;
    let base = base_dir(dirfd, &path)?;
    let follow = flags & O_NOFOLLOW == 0;
    let (full, ino) = match vfs::lookup(&base, &path, follow) {
        Ok(found) => {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                return Err(-EEXIST);
            }
            found
        }
        Err(e) if e == -ENOENT && flags & O_CREAT != 0 => {
            let (parent, name) = vfs::parent_of(&base, &path)?;
            let ino = parent.create(&name, mode as u32 & 0o7777 & !UMASK, NewNode::File)?;
            (vfs::normalize(&base, &path), ino)
        }
        Err(e) => return Err(e),
    };
    let accmode = flags as u32 & file::O_ACCMODE;
    let kind = match ino.meta().mode & S_IFMT {
        vfs::S_IFDIR => {
            if accmode != file::O_RDONLY {
                return Err(-EISDIR);
            }
            None
        }
        _ if flags & O_DIRECTORY != 0 => return Err(-ENOTDIR),
        vfs::S_IFCHR => {
            let (ma, mi) = fs::dev_of(&ino).unwrap();
            Some(Kind::of_dev(ma, mi).ok_or(-ENXIO)?)
        }
        vfs::S_IFIFO => {
            let p = fifo_pipe(&ino);
            let (r, w) = match accmode {
                file::O_RDONLY => (true, false),
                file::O_WRONLY => (false, true),
                _ => (true, true),
            };
            Some(Pipe::open_fifo(&p, r, w, flags as u32 & O_NONBLOCK != 0)?)
        }
        vfs::S_IFLNK => return Err(-40), // ELOOP (O_NOFOLLOW)
        _ => {
            if flags & O_TRUNC != 0 && accmode != file::O_RDONLY {
                ino.truncate(0)?;
            }
            None
        }
    };
    let kind = kind.unwrap_or(Kind::Inode(ino, full));
    let f = file::new(kind, flags as u32);
    let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

pub fn close(fd: u64) -> R {
    let p = proc::current().files();
    match p.fds.get_mut(fd as usize) {
        Some(f @ Some(_)) => {
            *f = None;
            Ok(0)
        }
        _ => Err(-EBADF),
    }
}

pub fn lseek(fd: u64, off: i64, whence: u64) -> R {
    let f = file_of(fd)?;
    let n = f.borrow_mut().lseek(off, whence as u32)?;
    Ok(n as i64)
}

pub fn fstat(fd: u64, st: usize) -> R {
    let f = file_of(fd)?;
    let s = f.borrow().stat();
    out(st, &s.to_bytes())?;
    Ok(0)
}

pub fn newfstatat(dirfd: i64, pathp: usize, st: usize, flags: u64) -> R {
    let path = user_str(pathp)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return fstat(dirfd as u64, st);
    }
    let ino = at(dirfd, pathp, flags)?;
    out(st, &Stat::of_inode(&ino).to_bytes())?;
    Ok(0)
}

pub fn faccessat(dirfd: i64, pathp: usize) -> R {
    at(dirfd, pathp, 0)?;
    Ok(0)
}

pub fn readlinkat(dirfd: i64, pathp: usize, buf: usize, len: usize) -> R {
    let ino = at(dirfd, pathp, AT_SYMLINK_NOFOLLOW)?;
    let t = ino.readlink()?;
    let n = t.len().min(len);
    out(buf, &t.as_bytes()[..n])?;
    Ok(n as i64)
}

pub fn getdents64(fd: u64, buf: usize, len: usize) -> R {
    let f = file_of(fd)?;
    let mut v = Vec::new();
    f.borrow_mut().getdents(&mut v, len)?;
    out(buf, &v)?;
    Ok(v.len() as i64)
}

pub fn mkdirat(dirfd: i64, pathp: usize, mode: u64) -> R {
    let (parent, name) = parent_at(dirfd, pathp)?;
    parent.create(&name, mode as u32 & 0o7777 & !UMASK, NewNode::Dir)?;
    Ok(0)
}

pub fn mknodat(dirfd: i64, pathp: usize, mode: u64, dev: u64) -> R {
    let (parent, name) = parent_at(dirfd, pathp)?;
    let mode = mode as u32;
    let node = match mode & S_IFMT {
        vfs::S_IFIFO => NewNode::Fifo,
        vfs::S_IFCHR => NewNode::Dev((dev >> 8) as u32 & 0xfff, (dev & 0xff) as u32),
        0 | vfs::S_IFREG => NewNode::File,
        _ => return Err(-EINVAL),
    };
    parent.create(&name, mode & 0o7777 & !UMASK, node)?;
    Ok(0)
}

pub fn unlinkat(dirfd: i64, pathp: usize, flags: u64) -> R {
    let (parent, name) = parent_at(dirfd, pathp)?;
    parent.unlink(&name, flags & AT_REMOVEDIR != 0)?;
    Ok(0)
}

pub fn symlinkat(targetp: usize, dirfd: i64, pathp: usize) -> R {
    let target = user_str(targetp)?;
    let (parent, name) = parent_at(dirfd, pathp)?;
    parent.create(&name, 0o777, NewNode::Symlink(target))?;
    Ok(0)
}

pub fn linkat(olddir: i64, oldp: usize, newdir: i64, newp: usize, flags: u64) -> R {
    let follow = if flags & AT_SYMLINK_FOLLOW != 0 { 0 } else { AT_SYMLINK_NOFOLLOW };
    let ino = at(olddir, oldp, follow | (flags & AT_EMPTY_PATH))?;
    if ino.meta().is_dir() {
        return Err(-vfs::EPERM);
    }
    let (parent, name) = parent_at(newdir, newp)?;
    parent.link(&name, &ino)?;
    Ok(0)
}

pub fn renameat(olddir: i64, oldp: usize, newdir: i64, newp: usize, flags: u64) -> R {
    const RENAME_NOREPLACE: u64 = 1;
    if flags & !RENAME_NOREPLACE != 0 {
        return Err(-EINVAL);
    }
    let (op, oname) = parent_at(olddir, oldp)?;
    let (np, nname) = parent_at(newdir, newp)?;
    if op.id().0 != np.id().0 {
        return Err(-vfs::EXDEV);
    }
    if flags & RENAME_NOREPLACE != 0 && np.lookup(&nname).is_ok() {
        return Err(-EEXIST);
    }
    op.rename(&oname, &np, &nname)?;
    Ok(0)
}

pub fn ftruncate(fd: u64, len: i64) -> R {
    if len < 0 {
        return Err(-EINVAL);
    }
    inode_of(fd)?.truncate(len as usize)?;
    Ok(0)
}

pub fn truncate(pathp: usize, len: i64) -> R {
    if len < 0 {
        return Err(-EINVAL);
    }
    at(AT_FDCWD, pathp, 0)?.truncate(len as usize)?;
    Ok(0)
}

pub fn fchmod(fd: u64, mode: u64) -> R {
    inode_of(fd)?.set_mode(mode as u32)?;
    Ok(0)
}

pub fn fchmodat(dirfd: i64, pathp: usize, mode: u64) -> R {
    at(dirfd, pathp, 0)?.set_mode(mode as u32)?;
    Ok(0)
}

fn id_arg(v: u64) -> Option<u32> {
    (v as u32 != u32::MAX).then_some(v as u32)
}

pub fn fchown(fd: u64, uid: u64, gid: u64) -> R {
    inode_of(fd)?.set_owner(id_arg(uid), id_arg(gid))?;
    Ok(0)
}

pub fn fchownat(dirfd: i64, pathp: usize, uid: u64, gid: u64, flags: u64) -> R {
    at(dirfd, pathp, flags)?.set_owner(id_arg(uid), id_arg(gid))?;
    Ok(0)
}

pub fn utimensat(dirfd: i64, pathp: usize, times: usize, flags: u64) -> R {
    const UTIME_NOW: u64 = (1 << 30) - 1;
    const UTIME_OMIT: u64 = (1 << 30) - 2;
    let ino = if pathp == 0 { inode_of(dirfd as u64)? } else { at(dirfd, pathp, flags)? };
    let mtime = if times == 0 {
        Some(crate::timer::epoch_ns())
    } else {
        let mut b = [0u8; 32];
        proc::current().pt().copy_in(&mut b, times).ok_or(-EFAULT)?;
        let sec = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let nsec = u64::from_le_bytes(b[24..32].try_into().unwrap());
        match nsec {
            UTIME_OMIT => None,
            UTIME_NOW => Some(crate::timer::epoch_ns()),
            _ => Some(sec * 1_000_000_000 + nsec),
        }
    };
    if let Some(t) = mtime {
        ino.set_mtime(t)?;
    }
    Ok(0)
}

pub fn statfs(pathp: usize, buf: usize) -> R {
    let ino = at(AT_FDCWD, pathp, 0)?;
    out(buf, &ino.statfs())?;
    Ok(0)
}

pub fn fstatfs(fd: u64, buf: usize) -> R {
    let f = file_of(fd)?;
    let b = match &f.borrow().kind {
        Kind::Inode(i, _) => i.statfs(),
        _ => vfs::root().statfs(),
    };
    out(buf, &b)?;
    Ok(0)
}

pub fn dup(fd: u64) -> R {
    let f = file_of(fd)?;
    let n = proc::current().files().add(f, false, 0).ok_or(-EMFILE)?;
    Ok(n as i64)
}

pub fn dup3(old: u64, new: u64, flags: u64) -> R {
    if old == new {
        return Err(-EINVAL);
    }
    let f = file_of(old)?;
    let p = proc::current().files();
    let new = new as usize;
    if new >= proc::NOFILE {
        return Err(-EBADF);
    }
    if p.fds.len() <= new {
        p.fds.resize(new + 1, None);
    }
    p.fds[new] = Some(Fd { file: f, cloexec: flags & O_CLOEXEC != 0 });
    Ok(new as i64)
}

pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> R {
    let p = proc::current().files();
    let entry = p.fds.get_mut(fd as usize).and_then(|f| f.as_mut()).ok_or(-EBADF)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let f = entry.file.clone();
            let n = p.add(f, cmd == F_DUPFD_CLOEXEC, arg as usize).ok_or(-EMFILE)?;
            Ok(n as i64)
        }
        F_GETFD => Ok(if entry.cloexec { FD_CLOEXEC as i64 } else { 0 }),
        F_SETFD => {
            entry.cloexec = arg & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => Ok(entry.file.borrow().flags as i64),
        F_SETFL => {
            // 変えられるのは O_APPEND と O_NONBLOCK だけ
            let mut f = entry.file.borrow_mut();
            f.flags = (f.flags & !(file::O_APPEND | O_NONBLOCK)) | (arg as u32 & (file::O_APPEND | O_NONBLOCK));
            if let Kind::Socket(s) = &f.kind {
                s.borrow_mut().nonblock = arg as u32 & O_NONBLOCK != 0;
            }
            Ok(0)
        }
        F_GETPIPE_SZ | F_SETPIPE_SZ => match &entry.file.borrow().kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                let mut p = p.borrow_mut();
                if cmd == F_SETPIPE_SZ {
                    let want = (arg as usize).clamp(4096, usize::MAX).next_power_of_two();
                    if want > file::PIPE_MAX {
                        return Err(-1); // EPERM
                    }
                    if want < p.len() {
                        return Err(-16); // EBUSY
                    }
                    p.cap = want;
                }
                Ok(p.cap as i64)
            }
            _ => Err(-EBADF),
        },
        _ => Err(-EINVAL),
    }
}

pub fn pipe2(fds: usize, flags: u64) -> R {
    let (r, w) = Pipe::new();
    let cloexec = flags & O_CLOEXEC != 0;
    let p = proc::current().files();
    let rfd = p.add(file::new(r, file::O_RDONLY), cloexec, 0).ok_or(-EMFILE)?;
    let Some(wfd) = p.add(file::new(w, file::O_WRONLY), cloexec, 0) else {
        p.fds[rfd] = None;
        return Err(-EMFILE);
    };
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&(rfd as i32).to_le_bytes());
    b[4..].copy_from_slice(&(wfd as i32).to_le_bytes());
    if let Err(e) = out(fds, &b) {
        p.fds[rfd] = None;
        p.fds[wfd] = None;
        return Err(e);
    }
    Ok(0)
}

pub fn ioctl(fd: u64, req: u64, arg: usize) -> R {
    let f = file_of(fd)?;
    if let (FIONBIO, Kind::Socket(s)) = (req, &f.borrow().kind) {
        let mut v = [0u8; 4];
        proc::current().pt().copy_in(&mut v, arg).ok_or(-EFAULT)?;
        s.borrow_mut().nonblock = u32::from_le_bytes(v) != 0;
        return Ok(0);
    }
    if !matches!(f.borrow().kind, Kind::Console) {
        return Err(-ENOTTY);
    }
    match req {
        TCGETS => {
            // 端末であることだけ伝える (中身は 0 の termios)
            out(arg, &[0u8; 60])?;
            Ok(0)
        }
        TIOCGWINSZ => {
            let mut ws = [0u8; 8];
            ws[0..2].copy_from_slice(&24u16.to_le_bytes());
            ws[2..4].copy_from_slice(&80u16.to_le_bytes());
            out(arg, &ws)?;
            Ok(0)
        }
        TIOCGPGRP => {
            out(arg, &(proc::current().tgid as i32).to_le_bytes())?;
            Ok(0)
        }
        _ => Err(-ENOTTY),
    }
}

pub fn getcwd(buf: usize, len: usize) -> R {
    let cwd = proc::current().files().cwd.clone();
    let mut s = Vec::with_capacity(cwd.len() + 2);
    s.push(b'/');
    s.extend_from_slice(cwd.as_bytes());
    s.push(0);
    if s.len() > len {
        return Err(-ERANGE);
    }
    out(buf, &s)?;
    Ok(s.len() as i64)
}

pub fn chdir(pathp: usize) -> R {
    let path = user_str(pathp)?;
    let files = proc::current().files();
    let (full, ino) = vfs::lookup(&files.cwd, &path, true)?;
    if !ino.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    files.cwd = full;
    Ok(0)
}

pub fn fchdir(fd: u64) -> R {
    let f = file_of(fd)?;
    let path = match &f.borrow().kind {
        Kind::Inode(i, p) if i.meta().is_dir() => p.clone(),
        _ => return Err(-ENOTDIR),
    };
    proc::current().files().cwd = path;
    Ok(0)
}


const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// ppoll(fds, nfds, timeout) (シグナルマスクは無視)
pub fn ppoll(fds: usize, nfds: usize, tmo: usize) -> R {
    if nfds > proc::NOFILE {
        return Err(-EINVAL);
    }
    let mut raw = vec![0u8; nfds * 8];
    proc::current().pt().copy_in(&mut raw, fds).ok_or(-EFAULT)?;
    let deadline = if tmo == 0 {
        None
    } else {
        let mut ts = [0u8; 16];
        proc::current().pt().copy_in(&mut ts, tmo).ok_or(-EFAULT)?;
        let ns = u64::from_le_bytes(ts[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(ts[8..].try_into().unwrap());
        Some(crate::timer::ticks() + (ns * crate::timer::HZ).div_ceil(1_000_000_000))
    };
    loop {
        let mut count = 0;
        for i in 0..nfds {
            let e = &mut raw[i * 8..i * 8 + 8];
            let fd = i32::from_le_bytes(e[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(e[4..6].try_into().unwrap());
            let mut rev = 0i16;
            if fd >= 0 {
                match proc::current().files().get(fd as u64) {
                    None => rev = POLLNVAL,
                    Some(f) => {
                        let (r, w, hup) = f.borrow().readiness();
                        if r {
                            rev |= events & POLLIN;
                        }
                        if w {
                            rev |= events & POLLOUT;
                        }
                        if hup {
                            rev |= POLLHUP | (events & POLLOUT != 0).then_some(POLLERR).unwrap_or(0);
                        }
                    }
                }
            }
            e[6..8].copy_from_slice(&rev.to_le_bytes());
            if rev != 0 {
                count += 1;
            }
        }
        let expired = deadline.is_some_and(|d| crate::timer::ticks() >= d);
        if count > 0 || expired {
            out(fds, &raw)?;
            return Ok(count);
        }
        proc::sleep_until(proc::poll_chan(), deadline.unwrap_or(0))?;
    }
}

/// FIFO の inode ごとのパイプ (誰かが開いている間だけ生きている)
static mut FIFOS: alloc::vec::Vec<((usize, u64), alloc::rc::Weak<core::cell::RefCell<Pipe>>)> = alloc::vec::Vec::new();

fn fifo_pipe(ino: &InodeRef) -> alloc::rc::Rc<core::cell::RefCell<Pipe>> {
    let fifos = unsafe { &mut *(&raw mut FIFOS) };
    fifos.retain(|(_, w)| w.strong_count() > 0);
    if let Some(p) = fifos.iter().find(|(id, _)| *id == ino.id()).and_then(|(_, w)| w.upgrade()) {
        return p;
    }
    let p = Pipe::empty();
    fifos.push((ino.id(), alloc::rc::Rc::downgrade(&p)));
    p
}

fn pipe_of(fd: u64) -> Option<alloc::rc::Rc<core::cell::RefCell<Pipe>>> {
    let f = file_of(fd).ok()?;
    let f = f.borrow();
    match &f.kind {
        Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => Some(p.clone()),
        _ => None,
    }
}

const SPLICE_F_NONBLOCK: u64 = 2;

/// splice: in から out へ len まで移す (中身はいったんカーネルでコピーする)
pub fn splice(fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> R {
    if pipe_of(fd_in).is_none() && pipe_of(fd_out).is_none() {
        return Err(-EINVAL);
    }
    let mut tmp = vec![0u8; len.min(64 * 1024)];
    let n = match (pipe_of(fd_in), off_in) {
        (Some(p), _) => Pipe::read_ex(&p, &mut tmp, false, flags & SPLICE_F_NONBLOCK != 0)?,
        (None, 0) => file_of(fd_in)?.borrow_mut().read(&mut tmp)?,
        (None, at) => {
            let off = read_off(at)?;
            let n = inode_of(fd_in)?.read_at(off as usize, &mut tmp)?;
            out(at, &(off + n as i64).to_le_bytes())?;
            n
        }
    };
    if n == 0 {
        return Ok(0);
    }
    if off_out != 0 {
        let off = read_off(off_out)?;
        inode_of(fd_out)?.write_at(off as usize, &tmp[..n])?;
        out(off_out, &(off + n as i64).to_le_bytes())?;
        return Ok(n as i64);
    }
    let f = file_of(fd_out)?;
    let mut done = 0;
    while done < n {
        done += f.borrow_mut().write(&tmp[done..n])?;
    }
    Ok(n as i64)
}

fn read_off(va: usize) -> Result<i64, i64> {
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(i64::from_le_bytes(b))
}

/// tee: パイプの中身を読み減らさずに、別のパイプへ写す
pub fn tee(fd_in: u64, fd_out: u64, len: usize, flags: u64) -> R {
    let (Some(pin), Some(pout)) = (pipe_of(fd_in), pipe_of(fd_out)) else { return Err(-EINVAL) };
    let room = {
        let o = pout.borrow();
        o.cap.saturating_sub(o.len())
    };
    let mut tmp = vec![0u8; len.min(room).min(64 * 1024)];
    if tmp.is_empty() {
        return Err(-11); // EAGAIN
    }
    let n = Pipe::read_ex(&pin, &mut tmp, true, flags & SPLICE_F_NONBLOCK != 0)?;
    let f = file_of(fd_out)?;
    let w = f.borrow_mut().write(&tmp[..n])?;
    Ok(w as i64)
}
