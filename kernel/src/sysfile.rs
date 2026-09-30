// ファイルまわりのシステムコール
use crate::file::{self, Kind, Pipe, Stat, EBADF, EINVAL};
use crate::path;
use crate::proc::{self, Fd};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const EFAULT: i64 = 14;
const ENOTDIR: i64 = 20;
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const EROFS: i64 = 30;
const ERANGE: i64 = 34;

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;

const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;
const O_CLOEXEC: u64 = 0o2000000;

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const FD_CLOEXEC: u64 = 1;

const TCGETS: u64 = 0x5401;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCGPGRP: u64 = 0x540f;

type R = Result<i64, i64>;

fn user_str(va: usize) -> Result<String, i64> {
    let b = proc::current().pt().copy_in_str(va, 4096).ok_or(-EFAULT)?;
    String::from_utf8(b).map_err(|_| -ENOENT)
}

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn file_of(fd: u64) -> Result<file::FileRef, i64> {
    proc::current().fd(fd).cloned().ok_or(-EBADF)
}

/// dirfd と path から、探索の基準にするディレクトリ (先頭 / なし)
fn base_dir(dirfd: i64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return Ok(proc::current().cwd.clone());
    }
    let f = file_of(dirfd as u64)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Initrd(e) if e.is_dir() => Ok(String::from(e.name)),
        _ => Err(-ENOTDIR),
    }
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
    let n = f.borrow_mut().write(&tmp)?;
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

pub fn openat(dirfd: i64, pathp: usize, flags: u64) -> R {
    let path = user_str(pathp)?;
    let kind = match path.as_str() {
        "/dev/null" => Kind::Null,
        "/dev/console" | "/dev/tty" => Kind::Console,
        _ => {
            if flags & (O_CREAT | O_TRUNC) != 0 || flags & O_ACCMODE != 0 {
                // initrd は読み取り専用
                let base = base_dir(dirfd, &path)?;
                return match path::resolve(&base, &path, true) {
                    Ok(e) if e.is_dir() => Err(-file::EISDIR),
                    Ok(_) => Err(-EROFS),
                    Err(e) if flags & O_CREAT != 0 && e == -ENOENT => Err(-EROFS),
                    Err(e) => Err(e),
                };
            }
            let base = base_dir(dirfd, &path)?;
            let e = path::resolve(&base, &path, true)?;
            if flags & O_DIRECTORY != 0 && !e.is_dir() {
                return Err(-ENOTDIR);
            }
            Kind::Initrd(e)
        }
    };
    let f = file::new(kind, flags as u32);
    let fd = proc::current().add_fd(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

pub fn close(fd: u64) -> R {
    let p = proc::current();
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
    let s = match path.as_str() {
        "/dev/null" => file::new(Kind::Null, 0).borrow().stat(),
        "/dev/console" | "/dev/tty" => file::new(Kind::Console, 0).borrow().stat(),
        _ => {
            let base = base_dir(dirfd, &path)?;
            Stat::of_entry(&path::resolve(&base, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?)
        }
    };
    out(st, &s.to_bytes())?;
    Ok(0)
}

pub fn faccessat(dirfd: i64, pathp: usize) -> R {
    let path = user_str(pathp)?;
    if path.starts_with("/dev/") {
        return Ok(0);
    }
    let base = base_dir(dirfd, &path)?;
    path::resolve(&base, &path, true)?;
    Ok(0)
}

pub fn readlinkat(dirfd: i64, pathp: usize, buf: usize, len: usize) -> R {
    let path = user_str(pathp)?;
    let base = base_dir(dirfd, &path)?;
    let e = path::resolve(&base, &path, false)?;
    if !e.is_symlink() {
        return Err(-EINVAL);
    }
    let n = e.data.len().min(len);
    out(buf, &e.data[..n])?;
    Ok(n as i64)
}

pub fn getdents64(fd: u64, buf: usize, len: usize) -> R {
    let f = file_of(fd)?;
    let mut v = Vec::new();
    f.borrow_mut().getdents(&mut v, len)?;
    out(buf, &v)?;
    Ok(v.len() as i64)
}

pub fn dup(fd: u64) -> R {
    let f = file_of(fd)?;
    let n = proc::current().add_fd(f, false, 0).ok_or(-EMFILE)?;
    Ok(n as i64)
}

pub fn dup3(old: u64, new: u64, flags: u64) -> R {
    if old == new {
        return Err(-EINVAL);
    }
    let f = file_of(old)?;
    let p = proc::current();
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
    let p = proc::current();
    let entry = p.fds.get_mut(fd as usize).and_then(|f| f.as_mut()).ok_or(-EBADF)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let f = entry.file.clone();
            let n = p.add_fd(f, cmd == F_DUPFD_CLOEXEC, arg as usize).ok_or(-EMFILE)?;
            Ok(n as i64)
        }
        F_GETFD => Ok(if entry.cloexec { FD_CLOEXEC as i64 } else { 0 }),
        F_SETFD => {
            entry.cloexec = arg & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => Ok(entry.file.borrow().flags as i64),
        F_SETFL => Ok(0),
        _ => Err(-EINVAL),
    }
}

pub fn pipe2(fds: usize, flags: u64) -> R {
    let (r, w) = Pipe::new();
    let cloexec = flags & O_CLOEXEC != 0;
    let p = proc::current();
    let rfd = p.add_fd(file::new(r, 0), cloexec, 0).ok_or(-EMFILE)?;
    let Some(wfd) = p.add_fd(file::new(w, 1), cloexec, 0) else {
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
            out(arg, &(proc::current().pid as i32).to_le_bytes())?;
            Ok(0)
        }
        _ => Err(-ENOTTY),
    }
}

pub fn getcwd(buf: usize, len: usize) -> R {
    let cwd = proc::current().cwd.clone();
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
    let p = proc::current();
    let e = path::resolve(&p.cwd, &path, true)?;
    if !e.is_dir() {
        return Err(-ENOTDIR);
    }
    p.cwd = String::from(e.name);
    Ok(0)
}
