// 開いたファイル (fd の向こう側)
use crate::tty::{self, TtyRef};
use crate::memlayout::PGSIZE;
use crate::vfs::{self, InodeRef, S_IFMT};
use crate::proc;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

pub const EBADF: i64 = 9;
pub const EINVAL: i64 = 22;
pub const ESPIPE: i64 = 29;
pub const EPIPE: i64 = 32;
pub const ENOTDIR: i64 = 20;

pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
const O_NOCTTY: u32 = 0o400;

const S_IFIFO: u32 = 0o010000;

#[derive(Clone)]
pub enum Kind {
    /// 端末 (コンソールと、疑似端末の子の口)
    Tty(TtyRef),
    /// 疑似端末の親の口 (/dev/ptmx)
    PtyMaster(TtyRef),
    Null,
    Zero,
    Random,
    /// ファイルシステムの inode と、開いたときのパス (dirfd の基準に使う)
    Inode(InodeRef, String),
    PipeRead(Rc<RefCell<Pipe>>),
    PipeWrite(Rc<RefCell<Pipe>>),
    /// O_RDWR で開いた FIFO (読み書き両方の口)
    PipeRw(Rc<RefCell<Pipe>>),
    /// socketpair の片方: rx から読み、tx へ書く
    Pair(Rc<RefCell<Pipe>>, Rc<RefCell<Pipe>>),
    Socket(crate::socket::SockRef),
}

impl Kind {
    /// デバイスファイルを開いたときの中身
    pub fn of_dev(major: u32, minor: u32, flags: u32) -> Result<Kind, i64> {
        const ENXIO: i64 = 6;
        let tty = match (major, minor) {
            (1, 3) => return Ok(Kind::Null),
            (1, 5) => return Ok(Kind::Zero),
            (1, 8) | (1, 9) => return Ok(Kind::Random),
            (5, 0) => tty::controlling().ok_or(-ENXIO)?,
            (5, 1) => tty::console(),
            (5, 2) => return Ok(Kind::PtyMaster(tty::open_ptmx()?)),
            (136, n) => tty::open_slave(n as usize)?,
            _ => return Err(-ENXIO),
        };
        if flags & O_NOCTTY == 0 {
            tty::maybe_acquire(&tty);
        }
        Ok(Kind::Tty(tty))
    }
}

pub struct OpenFile {
    pub kind: Kind,
    pub offset: usize,
    pub flags: u32,
}

pub type FileRef = Rc<RefCell<OpenFile>>;

pub fn new(kind: Kind, flags: u32) -> FileRef {
    Rc::new(RefCell::new(OpenFile { kind, offset: 0, flags }))
}

/// fd から読む。眠るかもしれないものは OpenFile を借りたまま眠らない
/// (同じ OpenFile を共有する他のプロセスが poll や read をできるように)
pub fn read(f: &FileRef, dst: &mut [u8]) -> Result<usize, i64> {
    let stream = {
        let b = f.borrow();
        if !b.readable() {
            return Err(-EBADF);
        }
        b.stream()
    };
    match stream {
        Some((Kind::PipeWrite(_), _)) => Err(-EBADF),
        Some((k, nonblock)) => read_stream(&k, dst, nonblock),
        None => f.borrow_mut().read(dst),
    }
}

pub fn write(f: &FileRef, src: &[u8]) -> Result<usize, i64> {
    let stream = {
        let b = f.borrow();
        if !b.writable() {
            return Err(-EBADF);
        }
        b.stream()
    };
    match stream {
        Some((Kind::PipeRead(_), _)) => Err(-EBADF),
        Some((k, nonblock)) => write_stream(&k, src, nonblock),
        None => f.borrow_mut().write(src),
    }
}

fn read_stream(k: &Kind, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    match k {
        Kind::Tty(t) => tty::read(t, dst, nonblock),
        Kind::PtyMaster(t) => tty::master_read(t, dst, nonblock),
        Kind::PipeRead(p) | Kind::PipeRw(p) | Kind::Pair(p, _) => Pipe::read(p, dst),
        Kind::Socket(s) => s.borrow_mut().read(dst),
        _ => Err(-EBADF),
    }
}

fn write_stream(k: &Kind, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    match k {
        Kind::Tty(t) => tty::write(t, src, nonblock),
        Kind::PtyMaster(t) => tty::master_write(t, src, nonblock),
        Kind::PipeWrite(p) | Kind::PipeRw(p) | Kind::Pair(_, p) => Pipe::write(p, src),
        Kind::Socket(s) => s.borrow_mut().write(src),
        _ => Err(-EBADF),
    }
}

/// Linux (asm-generic) の struct stat
pub struct Stat {
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blocks: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl Stat {
    pub fn of_inode(i: &InodeRef) -> Stat {
        let m = i.meta();
        Stat {
            ino: m.ino,
            mode: m.mode,
            nlink: m.nlink,
            uid: m.uid,
            gid: m.gid,
            rdev: m.rdev,
            size: m.size,
            blocks: m.blocks,
            mtime: m.mtime,
            ctime: m.ctime,
        }
    }

    fn dev(mode: u32, rdev: u64) -> Stat {
        Stat { ino: 0, mode, nlink: 1, uid: 0, gid: 0, rdev, size: 0, blocks: 0, mtime: 0, ctime: 0 }
    }

    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&1u64.to_le_bytes()); // st_dev
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.mode.to_le_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
        b[24..28].copy_from_slice(&self.uid.to_le_bytes());
        b[28..32].copy_from_slice(&self.gid.to_le_bytes());
        b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..60].copy_from_slice(&4096u32.to_le_bytes()); // st_blksize
        b[64..72].copy_from_slice(&self.blocks.to_le_bytes()); // st_blocks
        let times = [(72, self.mtime), (88, self.mtime), (104, self.ctime)];
        for (off, ns) in times {
            b[off..off + 8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
            b[off + 8..off + 16].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
        }
        b
    }
}

impl OpenFile {
    fn readable(&self) -> bool {
        self.flags & O_ACCMODE != O_WRONLY
    }

    fn writable(&self) -> bool {
        self.flags & O_ACCMODE != O_RDONLY
    }

    pub fn read(&mut self, dst: &mut [u8]) -> Result<usize, i64> {
        if !self.readable() {
            return Err(-EBADF);
        }
        match &self.kind {
            Kind::Null => Ok(0),
            Kind::Zero => {
                dst.fill(0);
                Ok(dst.len())
            }
            Kind::Random => {
                for c in dst.chunks_mut(16) {
                    let b = crate::rand::bytes16();
                    c.copy_from_slice(&b[..c.len()]);
                }
                Ok(dst.len())
            }
            Kind::Inode(ino, _) => {
                let n = ino.read_at(self.offset, dst)?;
                self.offset += n;
                Ok(n)
            }
            Kind::PipeWrite(_) => Err(-EBADF),
            k => read_stream(k, dst, self.flags & O_NONBLOCK != 0),
        }
    }

    /// 待つかもしれないもの (端末、パイプ、ソケット) なら、その中身の写しと O_NONBLOCK
    fn stream(&self) -> Option<(Kind, bool)> {
        let k = match &self.kind {
            Kind::Tty(_) | Kind::PtyMaster(_) | Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) | Kind::Pair(..) | Kind::Socket(_) => self.kind.clone(),
            _ => return None,
        };
        Some((k, self.flags & O_NONBLOCK != 0))
    }

    pub fn write(&mut self, src: &[u8]) -> Result<usize, i64> {
        if !self.writable() {
            return Err(-EBADF);
        }
        match &self.kind {
            Kind::Null | Kind::Zero | Kind::Random => Ok(src.len()),
            Kind::Inode(ino, _) => {
                if self.flags & O_APPEND != 0 {
                    self.offset = ino.meta().size as usize;
                }
                let n = ino.write_at(self.offset, src)?;
                self.offset += n;
                Ok(n)
            }
            Kind::PipeRead(_) => Err(-EBADF),
            k => write_stream(k, src, self.flags & O_NONBLOCK != 0),
        }
    }

    pub fn stat(&self) -> Stat {
        // デバイスは /dev のノードと同じ ino を見せる (musl の ttyname はそれを比べる)
        if matches!(self.kind, Kind::Tty(_) | Kind::PtyMaster(_) | Kind::Null | Kind::Zero | Kind::Random) {
            if let Ok(i) = vfs::resolve("", &self.describe(), true) {
                return Stat::of_inode(&i);
            }
        }
        match &self.kind {
            Kind::Tty(t) => Stat::dev(vfs::S_IFCHR | 0o620, tty::rdev(t, false)),
            Kind::PtyMaster(t) => Stat::dev(vfs::S_IFCHR | 0o666, tty::rdev(t, true)),
            Kind::Null => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 3),
            Kind::Zero => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 5),
            Kind::Random => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 9),
            Kind::Inode(ino, _) => Stat::of_inode(ino),
            Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) => Stat::dev(S_IFIFO | 0o600, 0),
            Kind::Socket(_) | Kind::Pair(..) => Stat::dev(0o140000 | 0o777, 0),
        }
    }

    /// 何を開いているか (/proc/PID/fd/N の readlink)
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::Tty(t) => tty::name(t, false),
            Kind::PtyMaster(t) => tty::name(t, true),
            Kind::Null => "/dev/null".into(),
            Kind::Zero => "/dev/zero".into(),
            Kind::Random => "/dev/urandom".into(),
            Kind::Inode(_, path) => alloc::format!("/{}", path),
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::format!("pipe:[{}]", Rc::as_ptr(p) as usize & 0xffffff),
            Kind::Pair(p, _) => alloc::format!("socket:[{}]", Rc::as_ptr(p) as usize & 0xffffff),
            Kind::Socket(s) => alloc::format!("socket:[{}]", Rc::as_ptr(s) as usize & 0xffffff),
        }
    }

    /// poll 用: (読める, 書ける, 閉じた/エラー)
    pub fn readiness(&self) -> (bool, bool, bool) {
        match &self.kind {
            Kind::Tty(t) => tty::readiness(t),
            Kind::PtyMaster(t) => tty::master_readiness(t),
            Kind::PipeRead(p) => {
                let p = p.borrow();
                (p.len() > 0 || p.writers == 0, false, p.writers == 0 && p.len() == 0)
            }
            Kind::PipeWrite(p) => {
                let p = p.borrow();
                (false, p.len() < p.cap, p.readers == 0)
            }
            Kind::PipeRw(p) => {
                let p = p.borrow();
                (p.len() > 0, p.len() < p.cap, false)
            }
            Kind::Pair(rx, tx) => {
                let (rx, tx) = (rx.borrow(), tx.borrow());
                (rx.len() > 0 || rx.writers == 0, tx.len() < tx.cap, rx.writers == 0 && rx.len() == 0)
            }
            Kind::Socket(s) => s.borrow().readiness(),
            _ => (true, true, false),
        }
    }

    pub fn lseek(&mut self, off: i64, whence: u32) -> Result<usize, i64> {
        let size = match &self.kind {
            Kind::Inode(ino, _) => ino.meta().size as i64,
            Kind::Null | Kind::Zero | Kind::Random => 0,
            _ => return Err(-ESPIPE),
        };
        const SEEK_DATA: u32 = 3;
        const SEEK_HOLE: u32 = 4;
        const ENXIO: i64 = 6;
        let base = match whence {
            0 => 0,
            1 => self.offset as i64,
            2 => size,
            // 穴はないので、データは末尾まで続き、穴は末尾にだけある
            SEEK_DATA | SEEK_HOLE if off < 0 || off >= size => return Err(-ENXIO),
            SEEK_DATA => 0,
            SEEK_HOLE => return Ok({
                self.offset = size as usize;
                self.offset
            }),
            _ => return Err(-EINVAL),
        };
        let new = base.checked_add(off).filter(|&n| n >= 0).ok_or(-EINVAL)?;
        self.offset = new as usize;
        Ok(self.offset)
    }

    /// linux_dirent64 を詰める。offset は「何番目まで返したか」
    pub fn getdents(&mut self, out: &mut Vec<u8>, max: usize) -> Result<(), i64> {
        let Kind::Inode(dir, path) = &self.kind else { return Err(-ENOTDIR) };
        if !dir.meta().is_dir() {
            return Err(-ENOTDIR);
        }
        let parent = vfs::resolve("", &vfs::normalize(path, ".."), true).unwrap_or_else(|_| dir.clone());
        let mut list = Vec::new();
        list.push((dir.meta().ino, String::from("."), vfs::S_IFDIR));
        list.push((parent.meta().ino, String::from(".."), vfs::S_IFDIR));
        for e in dir.readdir()? {
            list.push((e.ino, e.name, e.mode));
        }
        for (i, (ino, name, mode)) in list.into_iter().enumerate().skip(self.offset) {
            let reclen = (19 + name.len() + 1 + 7) & !7;
            if out.len() + reclen > max {
                if out.is_empty() {
                    return Err(-EINVAL);
                }
                break;
            }
            let start = out.len();
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&((i + 1) as i64).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(dtype(mode));
            out.extend_from_slice(name.as_bytes());
            out.resize(start + reclen, 0);
            self.offset = i + 1;
        }
        Ok(())
    }
}

fn dtype(mode: u32) -> u8 {
    match mode & S_IFMT {
        0o010000 => 1,  // DT_FIFO
        0o020000 => 2,  // DT_CHR
        0o040000 => 4,  // DT_DIR
        0o100000 => 8,  // DT_REG
        0o120000 => 10, // DT_LNK
        _ => 0,
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        // flock のロックは OpenFile ごと (その場所で見分ける)
        crate::sysfile::release_locks(self as *const OpenFile as usize);
        match &self.kind {
            Kind::PipeRead(p) => {
                p.borrow_mut().readers -= 1;
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::wakeup(proc::poll_chan());
            }
            Kind::PipeWrite(p) => {
                p.borrow_mut().writers -= 1;
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::wakeup(proc::poll_chan());
            }
            Kind::PipeRw(p) => {
                let mut pp = p.borrow_mut();
                pp.readers -= 1;
                pp.writers -= 1;
                drop(pp);
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::wakeup(proc::poll_chan());
            }
            Kind::Pair(rx, tx) => {
                rx.borrow_mut().readers -= 1;
                tx.borrow_mut().writers -= 1;
                proc::wakeup(Rc::as_ptr(rx) as usize);
                proc::wakeup(Rc::as_ptr(tx) as usize);
                proc::wakeup(proc::poll_chan());
            }
            Kind::Tty(t) if matches!(t.borrow().dev, tty::Dev::Pty(_)) => tty::close_slave(t),
            Kind::PtyMaster(t) => tty::close_master(t),
            _ => {}
        }
    }
}

/// パイプの大きさ (Linux と同じ既定値と、F_SETPIPE_SZ で広げられる上限)
pub const PIPE_SIZE: usize = 64 * 1024;
pub const PIPE_MAX: usize = 1024 * 1024;

/// ページ (4KiB) をつないだ FIFO
struct PageQueue {
    /// (ページ, 読むところ, 書いたところ)
    pages: alloc::collections::VecDeque<(*mut u8, usize, usize)>,
    len: usize,
}

impl PageQueue {
    fn push(&mut self, src: &[u8]) -> Result<(), i64> {
        let mut done = 0;
        while done < src.len() {
            let need_new = self.pages.back().is_none_or(|p| p.2 == PGSIZE);
            if need_new {
                let p = crate::kalloc::alloc().ok_or(-12i64)?; // ENOMEM
                self.pages.push_back((p, 0, 0));
            }
            let last = self.pages.back_mut().unwrap();
            let k = (PGSIZE - last.2).min(src.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(src[done..].as_ptr(), last.0.add(last.2), k) };
            last.2 += k;
            done += k;
        }
        self.len += src.len();
        Ok(())
    }

    /// 先頭から読む。consume なら取り除く
    fn pop(&mut self, dst: &mut [u8], consume: bool) -> usize {
        let mut done = 0;
        let mut i = 0;
        while done < dst.len() && i < self.pages.len() {
            let (p, r, w) = self.pages[i];
            let k = (w - r).min(dst.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(p.add(r), dst[done..].as_mut_ptr(), k) };
            done += k;
            if consume {
                self.pages[i].1 += k;
                if self.pages[i].1 == PGSIZE {
                    crate::kalloc::free(p);
                    self.pages.pop_front();
                    continue;
                }
            }
            i += 1;
        }
        if consume {
            self.len -= done;
        }
        done
    }
}

impl Drop for PageQueue {
    fn drop(&mut self) {
        for (p, _, _) in self.pages.drain(..) {
            crate::kalloc::free(p);
        }
    }
}

pub struct Pipe {
    data: PageQueue,
    pub cap: usize,
    readers: usize,
    writers: usize,
    /// FIFO が読み/書きで開かれた回数 (相手がすぐ閉じても、来たことはわかる)
    r_opened: u64,
    w_opened: u64,
}

impl Pipe {
    pub fn new() -> (Kind, Kind) {
        let q = PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        let p = Rc::new(RefCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 1, writers: 1, r_opened: 1, w_opened: 1 }));
        (Kind::PipeRead(p.clone()), Kind::PipeWrite(p))
    }

    pub fn len(&self) -> usize {
        self.data.len
    }

    /// FIFO を開く。相手 (読み手なら書き手) が来るまで待つ (nonblock なら待たない)
    pub fn open_fifo(p: &Rc<RefCell<Pipe>>, read: bool, write: bool, nonblock: bool) -> Result<Kind, i64> {
        const ENXIO: i64 = 6;
        // 待ち始めたときの相手の開いた回数
        let seen = {
            let mut pp = p.borrow_mut();
            if write && !read && nonblock && pp.readers == 0 {
                return Err(-ENXIO);
            }
            if read {
                pp.readers += 1;
                pp.r_opened += 1;
            }
            if write {
                pp.writers += 1;
                pp.w_opened += 1;
            }
            if read { pp.w_opened } else { pp.r_opened }
        };
        Pipe::wake(p);
        let kind = match (read, write) {
            (true, true) => Kind::PipeRw(p.clone()),
            (true, false) => Kind::PipeRead(p.clone()),
            _ => Kind::PipeWrite(p.clone()),
        };
        if !(read && write) && !nonblock {
            // 相手がいるか、待っている間に一度でも来たら進む。
            // 割り込まれたら kind が落ちて数は戻る
            loop {
                let pp = p.borrow();
                let (now, opened) = if read { (pp.writers, pp.w_opened) } else { (pp.readers, pp.r_opened) };
                if now > 0 || opened != seen {
                    break;
                }
                drop(pp);
                proc::sleep(Rc::as_ptr(p) as usize)?;
            }
        }
        Ok(kind)
    }

    /// socketpair: 向かい合わせにつないだ 2 本のパイプ
    pub fn pair() -> (Kind, Kind) {
        let q = || PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        let mk = || Rc::new(RefCell::new(Pipe { data: q(), cap: PIPE_SIZE, readers: 1, writers: 1, r_opened: 1, w_opened: 1 }));
        let (a, b) = (mk(), mk());
        (Kind::Pair(a.clone(), b.clone()), Kind::Pair(b, a))
    }

    /// 誰も開いていない FIFO 用
    pub fn empty() -> Rc<RefCell<Pipe>> {
        let q = PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        Rc::new(RefCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 0, writers: 0, r_opened: 0, w_opened: 0 }))
    }

    fn wake(p: &Rc<RefCell<Pipe>>) {
        proc::wakeup(Rc::as_ptr(p) as usize);
        proc::wakeup(proc::poll_chan());
    }

    /// 読む。peek なら取り除かない (tee 用)
    pub fn read_ex(p: &Rc<RefCell<Pipe>>, dst: &mut [u8], peek: bool, nonblock: bool) -> Result<usize, i64> {
        loop {
            {
                let mut pp = p.borrow_mut();
                if pp.data.len > 0 {
                    let n = pp.data.pop(dst, !peek);
                    drop(pp);
                    Pipe::wake(p);
                    return Ok(n);
                }
                if pp.writers == 0 {
                    return Ok(0);
                }
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            proc::sleep(Rc::as_ptr(p) as usize)?;
        }
    }

    fn read(p: &Rc<RefCell<Pipe>>, dst: &mut [u8]) -> Result<usize, i64> {
        Pipe::read_ex(p, dst, false, false)
    }

    fn write(p: &Rc<RefCell<Pipe>>, src: &[u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < src.len() {
            {
                let mut pp = p.borrow_mut();
                if pp.readers == 0 {
                    return if done > 0 { Ok(done) } else { Err(-EPIPE) };
                }
                let room = pp.cap.saturating_sub(pp.data.len);
                let k = room.min(src.len() - done);
                if k > 0 {
                    pp.data.push(&src[done..done + k])?;
                    done += k;
                }
            }
            Pipe::wake(p);
            if done < src.len() {
                if let Err(e) = proc::sleep(Rc::as_ptr(p) as usize) {
                    return if done > 0 { Ok(done) } else { Err(e) };
                }
            }
        }
        Ok(done)
    }
}
