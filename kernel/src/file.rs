// 開いたファイル (fd の向こう側)
use crate::console;
use crate::initrd::{self, Entry, S_IFDIR};
use crate::proc;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

pub const EBADF: i64 = 9;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ESPIPE: i64 = 29;
pub const EPIPE: i64 = 32;

const S_IFCHR: u32 = 0o020000;
const S_IFIFO: u32 = 0o010000;

pub enum Kind {
    Console,
    Null,
    Initrd(Entry),
    PipeRead(Rc<RefCell<Pipe>>),
    PipeWrite(Rc<RefCell<Pipe>>),
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
    pub rdev: u64,
    pub size: u64,
    pub mtime: u64,
}

impl Stat {
    pub fn of_entry(e: &Entry) -> Stat {
        Stat {
            ino: e.ino as u64,
            mode: e.mode,
            nlink: if e.is_dir() { 2 } else { 1 },
            rdev: 0,
            size: e.data.len() as u64,
            mtime: e.mtime as u64,
        }
    }

    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&1u64.to_le_bytes()); // st_dev
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.mode.to_le_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
        // uid, gid = 0
        b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..60].copy_from_slice(&4096u32.to_le_bytes()); // st_blksize
        b[64..72].copy_from_slice(&self.size.div_ceil(512).to_le_bytes()); // st_blocks
        for t in [72, 88, 104] {
            b[t..t + 8].copy_from_slice(&self.mtime.to_le_bytes());
        }
        b
    }
}

impl OpenFile {
    pub fn read(&mut self, dst: &mut [u8]) -> Result<usize, i64> {
        match &self.kind {
            Kind::Console => console::read(dst),
            Kind::Null => Ok(0),
            Kind::Initrd(e) => {
                if e.is_dir() {
                    return Err(-EISDIR);
                }
                let data = e.data;
                let n = dst.len().min(data.len().saturating_sub(self.offset));
                dst[..n].copy_from_slice(&data[self.offset..self.offset + n]);
                self.offset += n;
                Ok(n)
            }
            Kind::PipeRead(p) => Pipe::read(p, dst),
            Kind::PipeWrite(_) => Err(-EBADF),
        }
    }

    pub fn write(&mut self, src: &[u8]) -> Result<usize, i64> {
        match &self.kind {
            Kind::Console => {
                console::write(src);
                Ok(src.len())
            }
            Kind::Null => Ok(src.len()),
            Kind::PipeWrite(p) => Pipe::write(p, src),
            Kind::Initrd(_) | Kind::PipeRead(_) => Err(-EBADF),
        }
    }

    pub fn stat(&self) -> Stat {
        match &self.kind {
            Kind::Console => Stat { ino: 0, mode: S_IFCHR | 0o620, nlink: 1, rdev: (5 << 8) | 1, size: 0, mtime: 0 },
            Kind::Null => Stat { ino: 0, mode: S_IFCHR | 0o666, nlink: 1, rdev: (1 << 8) | 3, size: 0, mtime: 0 },
            Kind::Initrd(e) => Stat::of_entry(e),
            Kind::PipeRead(_) | Kind::PipeWrite(_) => Stat { ino: 0, mode: S_IFIFO | 0o600, nlink: 1, rdev: 0, size: 0, mtime: 0 },
        }
    }

    pub fn lseek(&mut self, off: i64, whence: u32) -> Result<usize, i64> {
        let size = match &self.kind {
            Kind::Initrd(e) => e.data.len() as i64,
            Kind::Null => 0,
            _ => return Err(-ESPIPE),
        };
        let base = match whence {
            0 => 0,
            1 => self.offset as i64,
            2 => size,
            _ => return Err(-EINVAL),
        };
        let new = base.checked_add(off).filter(|&n| n >= 0).ok_or(-EINVAL)?;
        self.offset = new as usize;
        Ok(self.offset)
    }

    /// linux_dirent64 を詰める。offset は「何番目まで返したか」
    pub fn getdents(&mut self, out: &mut Vec<u8>, max: usize) -> Result<(), i64> {
        let Kind::Initrd(dir) = &self.kind else { return Err(-EINVAL) };
        if !dir.is_dir() {
            return Err(-crate::file::ENOTDIR);
        }
        let parent = crate::path::normalize(dir.name, "..");
        let parent_ino = initrd::lookup(&parent).map(|e| e.ino).unwrap_or(1);
        let dots = [(dir.ino, ".", S_IFDIR), (parent_ino, "..", S_IFDIR)];
        let list = dots
            .into_iter()
            .chain(initrd::children(dir.name).map(|e| (e.ino, e.name.rsplit('/').next().unwrap(), e.mode)));
        for (i, (ino, name, mode)) in list.enumerate().skip(self.offset) {
            let reclen = (19 + name.len() + 1 + 7) & !7;
            if out.len() + reclen > max {
                if out.is_empty() {
                    return Err(-EINVAL);
                }
                break;
            }
            let start = out.len();
            out.extend_from_slice(&(ino as u64).to_le_bytes());
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

pub const ENOTDIR: i64 = 20;

fn dtype(mode: u32) -> u8 {
    match mode & initrd::S_IFMT {
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

const PIPE_SIZE: usize = 4096;

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
