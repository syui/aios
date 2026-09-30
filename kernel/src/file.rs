// 開いたファイル (fd の向こう側)
use crate::console;
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

const S_IFIFO: u32 = 0o010000;

pub enum Kind {
    Console,
    Null,
    Zero,
    Random,
    /// ファイルシステムの inode と、開いたときのパス (dirfd の基準に使う)
    Inode(InodeRef, String),
    PipeRead(Rc<RefCell<Pipe>>),
    PipeWrite(Rc<RefCell<Pipe>>),
}

impl Kind {
    /// デバイスファイルを開いたときの中身
    pub fn of_dev(major: u32, minor: u32) -> Option<Kind> {
        Some(match (major, minor) {
            (1, 3) => Kind::Null,
            (1, 5) => Kind::Zero,
            (1, 8) | (1, 9) => Kind::Random,
            (5, 0) | (5, 1) => Kind::Console,
            _ => return None,
        })
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
            Kind::Console => console::read(dst),
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
            Kind::PipeRead(p) => Pipe::read(p, dst),
            Kind::PipeWrite(_) => Err(-EBADF),
        }
    }

    pub fn write(&mut self, src: &[u8]) -> Result<usize, i64> {
        if !self.writable() {
            return Err(-EBADF);
        }
        match &self.kind {
            Kind::Console => {
                console::write(src);
                Ok(src.len())
            }
            Kind::Null | Kind::Zero | Kind::Random => Ok(src.len()),
            Kind::Inode(ino, _) => {
                if self.flags & O_APPEND != 0 {
                    self.offset = ino.meta().size as usize;
                }
                let n = ino.write_at(self.offset, src)?;
                self.offset += n;
                Ok(n)
            }
            Kind::PipeWrite(p) => Pipe::write(p, src),
            Kind::PipeRead(_) => Err(-EBADF),
        }
    }

    pub fn stat(&self) -> Stat {
        match &self.kind {
            Kind::Console => Stat::dev(vfs::S_IFCHR | 0o620, (5 << 8) | 1),
            Kind::Null => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 3),
            Kind::Zero => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 5),
            Kind::Random => Stat::dev(vfs::S_IFCHR | 0o666, (1 << 8) | 9),
            Kind::Inode(ino, _) => Stat::of_inode(ino),
            Kind::PipeRead(_) | Kind::PipeWrite(_) => Stat::dev(S_IFIFO | 0o600, 0),
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
        match &self.kind {
            Kind::PipeRead(p) => {
                p.borrow_mut().readers -= 1;
                proc::wakeup(Rc::as_ptr(p) as usize);
            }
            Kind::PipeWrite(p) => {
                p.borrow_mut().writers -= 1;
                proc::wakeup(Rc::as_ptr(p) as usize);
            }
            _ => {}
        }
    }
}

pub const PIPE_SIZE: usize = 4096;

pub struct Pipe {
    buf: [u8; PIPE_SIZE],
    r: usize,
    w: usize,
    readers: usize,
    writers: usize,
}

impl Pipe {
    pub fn new() -> (Kind, Kind) {
        let p = Rc::new(RefCell::new(Pipe { buf: [0; PIPE_SIZE], r: 0, w: 0, readers: 1, writers: 1 }));
        (Kind::PipeRead(p.clone()), Kind::PipeWrite(p))
    }

    fn read(p: &Rc<RefCell<Pipe>>, dst: &mut [u8]) -> Result<usize, i64> {
        let chan = Rc::as_ptr(p) as usize;
        loop {
            {
                let mut pp = p.borrow_mut();
                if pp.r != pp.w {
                    let mut n = 0;
                    while n < dst.len() && pp.r != pp.w {
                        dst[n] = pp.buf[pp.r % PIPE_SIZE];
                        pp.r += 1;
                        n += 1;
                    }
                    drop(pp);
                    proc::wakeup(chan);
                    return Ok(n);
                }
                if pp.writers == 0 {
                    return Ok(0);
                }
            }
            proc::sleep(chan)?;
        }
    }

    fn write(p: &Rc<RefCell<Pipe>>, src: &[u8]) -> Result<usize, i64> {
        let chan = Rc::as_ptr(p) as usize;
        let mut done = 0;
        while done < src.len() {
            {
                let mut pp = p.borrow_mut();
                if pp.readers == 0 {
                    return if done > 0 { Ok(done) } else { Err(-EPIPE) };
                }
                while done < src.len() && pp.w - pp.r < PIPE_SIZE {
                    let w = pp.w;
                    pp.buf[w % PIPE_SIZE] = src[done];
                    pp.w += 1;
                    done += 1;
                }
            }
            proc::wakeup(chan);
            if done < src.len() {
                if let Err(e) = proc::sleep(chan) {
                    return if done > 0 { Ok(done) } else { Err(e) };
                }
            }
        }
        Ok(done)
    }
}

