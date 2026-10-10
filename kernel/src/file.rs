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
pub const O_RDWR: u32 = 2;
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
    PipeRead(Rc<PipeCell>),
    PipeWrite(Rc<PipeCell>),
    /// O_RDWR で開いた FIFO (読み書き両方の口)
    PipeRw(Rc<PipeCell>),
    /// socketpair の片方: rx から読み、tx へ書く
    Pair(Rc<PipeCell>, Rc<PipeCell>),
    Socket(crate::socket::SockRef),
    /// AF_UNIX のソケットで、まだつながっていないもの (bind / listen 中)。つながると Pair になる
    Unix(crate::unix::UnixRef),
    Epoll(crate::epoll::EpollRef),
    EventFd(crate::epoll::EventFdRef),
    /// timerfd_create で作ったもの (期限が来ると読める)
    TimerFd(crate::timerfd::TimerRef),
    /// inotify_init1 で作ったもの (できごとが読める)
    Inotify(crate::inotify::InotifyRef),
    /// pidfd_open で開いたプロセス (終わると読める)
    PidFd(u32),
    /// ディスクか区画 (/dev/vda2 など)。セクタに合わないところは読んでから書く
    Block(crate::block::Part),
    /// 画面 (/dev/fb0、virtio-gpu)
    Fb,
    /// キーボードやマウス (/dev/input/eventN、virtio-input)
    Input(usize),
    /// 音のカードの制御の口 (/dev/snd/controlC0、virtio-sound)
    SndCtl,
    /// 音の再生の口 (/dev/snd/pcmC0D0p)
    SndPcm,
    /// landlock_create_ruleset の決まりの束
    Landlock(crate::landlock::RulesetRef),
}

impl Kind {
    /// デバイスファイルを開いたときの中身
    pub fn of_dev(major: u32, minor: u32, flags: u32) -> Result<Kind, i64> {
        const ENXIO: i64 = 6;
        let tty = match (major, minor) {
            (1, 3) => return Ok(Kind::Null),
            (1, 5) => return Ok(Kind::Zero),
            (1, 8) | (1, 9) => return Ok(Kind::Random),
            (29, 0) if crate::gpu::get().is_some() => return Ok(Kind::Fb),
            (13, n) if n >= 64 && ((n - 64) as usize) < crate::input::count() => return Ok(Kind::Input((n - 64) as usize)),
            (116, 0) if crate::sound::present() => return Ok(Kind::SndCtl),
            (116, 16) if crate::sound::present() => {
                crate::sound::open_pcm()?;
                return Ok(Kind::SndPcm);
            }
            (5, 0) => {
                let t = tty::controlling().ok_or(-ENXIO)?;
                tty::ref_slave(&t);
                t
            }
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
    /// ディレクトリ: 最初の getdents (offset 0) で読んだ一覧。続きはここから返す
    /// (読みながら消しても番号がずれず、読み飛ばさない。rewinddir で offset が 0 に戻れば読みなおす)
    dirents: Option<Rc<Vec<(u64, String, u32)>>>,
    /// ふつうのファイルを読んでいる途中 (read_inode_file。ディスクを眠って待つことがある)。同じ開いたファイルを
    /// 読むほかの人は、終わるまで待つ (オフセットを同時に進めない)
    reading: bool,
}

pub type FileRef = Rc<RefCell<OpenFile>>;

pub fn new(kind: Kind, flags: u32) -> FileRef {
    Rc::new(RefCell::new(OpenFile { kind, offset: 0, flags, dirents: None, reading: false }))
}

/// fd から読む。眠るかもしれないものは OpenFile を借りたまま眠らない
/// (同じ OpenFile を共有する他のプロセスが poll や read をできるように)
pub fn read(f: &FileRef, dst: &mut [u8]) -> Result<usize, i64> {
    read_opt(f, dst, false)
}

pub fn write(f: &FileRef, src: &[u8]) -> Result<usize, i64> {
    write_opt(f, src, false)
}

/// dontwait なら O_NONBLOCK がなくても待たない (recv/send の MSG_DONTWAIT)
pub fn read_opt(f: &FileRef, dst: &mut [u8], dontwait: bool) -> Result<usize, i64> {
    let stream = {
        let b = f.borrow();
        if !b.readable() {
            return Err(-EBADF);
        }
        b.stream()
    };
    match stream {
        Some((Kind::PipeWrite(_), _)) => Err(-EBADF),
        Some((k, nonblock)) => read_stream(&k, dst, nonblock || dontwait),
        None => {
            // ページキャッシュを通すふつうのファイル (ext4): 借りたままディスクを待たない (眠ることがある)
            let inode = {
                let b = f.borrow();
                match &b.kind {
                    Kind::Inode(ino, path) if ino.page_cacheable() => Some((ino.clone(), path.clone())),
                    _ => None,
                }
            };
            match inode {
                Some((ino, path)) => read_inode_file(f, &ino, &path, dst),
                None => f.borrow_mut().read(dst),
            }
        }
    }
}

/// ふつうのファイルを、開いたファイル f のオフセットから読む。ディスクを待つあいだ眠ることがある
/// (そのあいだ大きなロックはほかの CPU が使える)。同じ開いたファイルを読むほかの人とは順番に
fn read_inode_file(f: &FileRef, ino: &InodeRef, path: &str, dst: &mut [u8]) -> Result<usize, i64> {
    let chan = Rc::as_ptr(f) as *const u8 as usize;
    while f.borrow().reading {
        proc::sleep(chan)?;
    }
    let off = {
        let mut b = f.borrow_mut();
        b.reading = true;
        b.offset
    };
    let r = read_sleepable(ino, off, dst);
    {
        let mut b = f.borrow_mut();
        b.reading = false;
        if let Ok(n) = r {
            b.offset = off + n;
        }
    }
    proc::wakeup(chan);
    if let Ok(n) = r
        && n > 0
    {
        crate::inotify::file_event(path, ino, crate::inotify::IN_ACCESS);
    }
    r
}

/// ページキャッシュを通して読む。キャッシュにないところはディスクを眠って待つ (proc::io_sleepable)。
/// 眠っているあいだにファイルシステムが書きかえられたら (vfs::ERETRY) 頭から読みなおし、何度もなら眠らずに
/// やりなおした数 (/proc/bkl)
pub static IO_RETRIES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn read_sleepable(ino: &InodeRef, off: usize, dst: &mut [u8]) -> Result<usize, i64> {
    for _ in 0..8 {
        match proc::io_sleepable(|| crate::vm::read_cached(ino, off, dst)) {
            Err(e) if e == -crate::vfs::ERETRY => {
                IO_RETRIES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                continue;
            }
            r => return r,
        }
    }
    crate::vm::read_cached(ino, off, dst)
}

pub fn write_opt(f: &FileRef, src: &[u8], dontwait: bool) -> Result<usize, i64> {
    let stream = {
        let b = f.borrow();
        if !b.writable() {
            return Err(-EBADF);
        }
        b.stream()
    };
    match stream {
        Some((Kind::PipeRead(_), _)) => Err(-EBADF),
        Some((k, nonblock)) => write_stream(&k, src, nonblock || dontwait),
        None => f.borrow_mut().write(src),
    }
}

fn read_stream(k: &Kind, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    match k {
        Kind::Tty(t) => tty::read(t, dst, nonblock),
        Kind::PtyMaster(t) => tty::master_read(t, dst, nonblock),
        Kind::PipeRead(p) | Kind::PipeRw(p) | Kind::Pair(p, _) => Pipe::read_ex(p, dst, false, nonblock),
        Kind::Socket(s) => s.borrow_mut().read(dst),
        Kind::EventFd(e) => crate::epoll::read(e, dst, nonblock),
        Kind::TimerFd(t) => crate::timerfd::read(t, dst, nonblock),
        Kind::Inotify(n) => crate::inotify::read(n, dst, nonblock),
        Kind::Input(n) => crate::input::read(*n, dst, nonblock),
        _ => Err(-EBADF),
    }
}

fn write_stream(k: &Kind, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    match k {
        Kind::Tty(t) => tty::write(t, src, nonblock),
        Kind::PtyMaster(t) => tty::master_write(t, src, nonblock),
        Kind::PipeWrite(p) | Kind::PipeRw(p) | Kind::Pair(_, p) => Pipe::write(p, src, nonblock),
        Kind::Socket(s) => s.borrow_mut().write(src),
        Kind::EventFd(e) => crate::epoll::write(e, src, nonblock),
        Kind::TimerFd(_) => Err(-EINVAL),
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
        // ユーザーの namespace の中なら、中の番号で見せる
        b[24..28].copy_from_slice(&crate::ns::show_uid(self.uid).to_le_bytes());
        b[28..32].copy_from_slice(&crate::ns::show_gid(self.gid).to_le_bytes());
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

    /// struct statx (STATX_BASIC_STATS だけ)
    pub fn to_statx(&self) -> [u8; 256] {
        let mut b = [0u8; 256];
        b[0..4].copy_from_slice(&0x7ffu32.to_le_bytes()); // stx_mask = STATX_BASIC_STATS
        b[4..8].copy_from_slice(&4096u32.to_le_bytes());
        b[16..20].copy_from_slice(&self.nlink.to_le_bytes());
        b[20..24].copy_from_slice(&crate::ns::show_uid(self.uid).to_le_bytes());
        b[24..28].copy_from_slice(&crate::ns::show_gid(self.gid).to_le_bytes());
        b[28..30].copy_from_slice(&(self.mode as u16).to_le_bytes());
        b[32..40].copy_from_slice(&self.ino.to_le_bytes());
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b[48..56].copy_from_slice(&self.blocks.to_le_bytes());
        // atime, (btime なし), ctime, mtime
        for (off, ns) in [(64, self.mtime), (96, self.ctime), (112, self.mtime)] {
            b[off..off + 8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
            b[off + 8..off + 12].copy_from_slice(&((ns % 1_000_000_000) as u32).to_le_bytes());
        }
        // st_rdev と同じ番号 (makedev の形) を major / minor に
        let d = self.rdev;
        let major = ((d >> 32) & 0xffff_f000) | ((d >> 8) & 0xfff);
        let minor = ((d >> 12) & 0xffff_ff00) | (d & 0xff);
        b[128..132].copy_from_slice(&(major as u32).to_le_bytes());
        b[132..136].copy_from_slice(&(minor as u32).to_le_bytes());
        b[140..144].copy_from_slice(&1u32.to_le_bytes()); // st_dev = 1
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
            Kind::Inode(ino, path) => {
                // ext4 のふつうのファイルはページキャッシュを通す (次からロックなしで読めるように)
                let n = if ino.page_cacheable() { crate::vm::read_cached(ino, self.offset, dst)? } else { ino.read_at(self.offset, dst)? };
                self.offset += n;
                if n > 0 {
                    crate::inotify::file_event(path, ino, crate::inotify::IN_ACCESS);
                }
                Ok(n)
            }
            Kind::Block(p) => {
                let n = blk_read(p, self.offset, dst)?;
                self.offset += n;
                Ok(n)
            }
            Kind::Fb => {
                let n = crate::gpu::read(self.offset, dst);
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
            Kind::Tty(_) | Kind::PtyMaster(_) | Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) | Kind::Pair(..) | Kind::Socket(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::Input(_) => self.kind.clone(),
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
            Kind::Inode(ino, path) => {
                if self.flags & O_APPEND != 0 {
                    self.offset = ino.meta().size as usize;
                }
                vfs::write_sealed(ino, self.offset, src.len())?;
                let n = ino.write_at(self.offset, src)?;
                self.offset += n;
                crate::inotify::file_event(path, ino, crate::inotify::IN_MODIFY);
                Ok(n)
            }
            Kind::Block(p) => {
                let n = blk_write(p, self.offset, src)?;
                self.offset += n;
                Ok(n)
            }
            Kind::Fb => {
                let n = crate::gpu::write(self.offset, src);
                self.offset += n;
                if n == 0 && !src.is_empty() {
                    return Err(-28); // ENOSPC (画面の終わり)
                }
                Ok(n)
            }
            Kind::PipeRead(_) => Err(-EBADF),
            k => write_stream(k, src, self.flags & O_NONBLOCK != 0),
        }
    }

    pub fn stat(&self) -> Stat {
        // デバイスは /dev のノードと同じ ino を見せる (musl の ttyname はそれを比べる)
        if matches!(self.kind, Kind::Tty(_) | Kind::PtyMaster(_) | Kind::Null | Kind::Zero | Kind::Random | Kind::Block(_) | Kind::Fb | Kind::Input(_) | Kind::SndCtl | Kind::SndPcm) {
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
            Kind::Fb => Stat::dev(vfs::S_IFCHR | 0o666, 29 << 8),
            Kind::Input(n) => Stat::dev(vfs::S_IFCHR | 0o666, 13 << 8 | (64 + *n as u64)),
            Kind::SndCtl => Stat::dev(vfs::S_IFCHR | 0o666, 116 << 8),
            Kind::SndPcm => Stat::dev(vfs::S_IFCHR | 0o666, 116 << 8 | 16),
            Kind::Inode(ino, _) => Stat::of_inode(ino),
            Kind::Block(p) => {
                let (ma, mi) = crate::block::dev_of_part(p);
                Stat::dev(vfs::S_IFBLK | 0o660, ((ma as u64) << 8) | mi as u64)
            }
            // ino はパイプごとにちがう番号 (/proc/PID/fd の pipe:[N] と同じ)。diff <(a) <(b) などは
            // dev と ino が同じなら同じファイルとみなす
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => Stat { ino: Rc::as_ptr(p) as usize as u64 & 0xffffff, ..Stat::dev(S_IFIFO | 0o600, 0) },
            Kind::Socket(_) | Kind::Pair(..) | Kind::Unix(_) => Stat::dev(0o140000 | 0o777, 0),
            // 名前のない inode (anon_inode)
            Kind::Epoll(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::PidFd(_) | Kind::Landlock(_) => Stat::dev(0o600, 0),
        }
    }

    /// 状態が変わるたびに増える数 (epoll の EPOLLET 用)。数えていないものは 0
    pub fn event_gen(&self) -> u64 {
        match &self.kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => p.borrow().generation,
            Kind::Pair(rx, tx) => rx.borrow().generation.wrapping_add(tx.borrow().generation),
            Kind::EventFd(e) => crate::epoll::generation(e),
            Kind::TimerFd(t) => crate::timerfd::generation(t),
            Kind::Inotify(n) => crate::inotify::generation(n),
            Kind::Tty(t) | Kind::PtyMaster(t) => t.borrow().generation.get(),
            Kind::Socket(_) => crate::net::generation(),
            Kind::Unix(u) => u.borrow().generation,
            _ => 0,
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
            Kind::Fb => "/dev/fb0".into(),
            Kind::Input(n) => alloc::format!("/dev/input/event{}", n),
            Kind::SndCtl => "/dev/snd/controlC0".into(),
            Kind::SndPcm => "/dev/snd/pcmC0D0p".into(),
            Kind::Inode(_, path) => alloc::format!("/{}", path),
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::format!("pipe:[{}]", Rc::as_ptr(p) as usize & 0xffffff),
            Kind::Pair(p, _) => alloc::format!("socket:[{}]", Rc::as_ptr(p) as usize & 0xffffff),
            Kind::Socket(s) => alloc::format!("socket:[{}]", Rc::as_ptr(s) as usize & 0xffffff),
            Kind::Unix(u) => alloc::format!("socket:[{}]", Rc::as_ptr(u) as usize & 0xffffff),
            Kind::Epoll(_) => "anon_inode:[eventpoll]".into(),
            Kind::EventFd(_) => "anon_inode:[eventfd]".into(),
            Kind::TimerFd(_) => "anon_inode:[timerfd]".into(),
            Kind::Inotify(_) => "anon_inode:inotify".into(),
            Kind::PidFd(_) => "anon_inode:[pidfd]".into(),
            Kind::Landlock(_) => "anon_inode:landlock-ruleset".into(),
            Kind::Block(p) => crate::block::part_name(p),
        }
    }

    /// 調べるための様子 (/proc/PID/stack の fd の一覧)
    pub fn debug_state(&self) -> String {
        let (r, w, h) = self.readiness();
        let rw = alloc::format!("r{}w{}h{}", r as u8, w as u8, h as u8);
        match &self.kind {
            Kind::Pair(rx, tx) => {
                let (rx, tx) = (rx.borrow(), tx.borrow());
                let types = |p: &Pipe| p.types.iter().map(|(t, (n, at))| alloc::format!("{}:{}@{}", t, n, at)).collect::<Vec<_>>().join(" ");
                let heads = |p: &Pipe| {
                    p.heads.iter().map(|h| {
                        let u = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
                        alloc::format!("{:x}/{}/{}", u(8), u(0), u(4) as i32)
                    }).collect::<Vec<_>>().join(" ")
                };
                alloc::format!(
                    "{} {} rx={} (writers {}, gen {}, bytes {}) tx={} (readers {}, gen {}, bytes {})\n    in: {}\n    out: {}\n    intypes: {}\n    outtypes: {}",
                    self.describe(), rw, rx.len(), rx.writers, rx.generation, rx.wrote, tx.len(), tx.readers, tx.generation, tx.wrote, heads(&rx), heads(&tx), types(&rx), types(&tx)
                )
            }
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                let p = p.borrow();
                alloc::format!("{} {} len={} r={} w={}", self.describe(), rw, p.len(), p.readers, p.writers)
            }
            Kind::Epoll(e) => alloc::format!("epoll{}", e.borrow().debug()),
            _ => alloc::format!("{} {}", self.describe(), rw),
        }
    }

    /// 読まれずに残っているもの (/proc/ai/fd): パイプと socketpair はバイト、TCP / UDP は届いている分、
    /// eventfd は数、inotify はバイト。ないもの (ふつうのファイルなど) は None
    pub fn pending(&self) -> Option<u64> {
        Some(match &self.kind {
            Kind::PipeRead(p) | Kind::PipeRw(p) | Kind::Pair(p, _) => p.borrow().len() as u64,
            Kind::Socket(s) => s.borrow().available() as u64,
            Kind::EventFd(e) => crate::epoll::count(e),
            Kind::Inotify(n) => crate::inotify::pending(n) as u64,
            _ => return None,
        })
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
            Kind::Unix(u) => crate::unix::readiness(u),
            Kind::EventFd(e) => crate::epoll::readiness(e),
            Kind::TimerFd(t) => crate::timerfd::readiness(t),
            Kind::Inotify(n) => crate::inotify::readiness(n),
            Kind::PidFd(pid) => (proc::has_exited(*pid), false, proc::has_exited(*pid)),
            Kind::Epoll(e) => (e.borrow_mut().readable(), false, false),
            Kind::Input(n) => (crate::input::readable(*n), false, false),
            Kind::SndPcm => crate::sound::readiness(),
            _ => (true, true, false),
        }
    }

    /// poll で待つときの印 (変わったときに poll_wake されるもの)。None はわからない (何かあれば起こす)
    pub fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        Some(match &self.kind {
            Kind::Tty(t) | Kind::PtyMaster(t) => alloc::vec![t.borrow().poll_key()],
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::vec![Rc::as_ptr(p) as usize],
            Kind::Pair(rx, tx) => alloc::vec![Rc::as_ptr(rx) as usize, Rc::as_ptr(tx) as usize],
            Kind::Unix(u) => alloc::vec![Rc::as_ptr(u) as usize],
            Kind::TimerFd(t) => alloc::vec![crate::timerfd::chan(t)],
            // eventfd、inotify、TCP / UDP のソケット (ネットワークで何か変わると net::poll が net::chan() を起こす)。
            // 印がないと、どこかのパイプに書くたびに起こされる (Firefox の gmain が 1 秒に何百回も起きていた)
            Kind::EventFd(e) => alloc::vec![crate::epoll::chan(e)],
            Kind::Inotify(n) => alloc::vec![crate::inotify::chan(n)],
            Kind::Socket(_) => alloc::vec![crate::net::chan()],
            Kind::PidFd(_) => alloc::vec![proc::pidfd_key()],
            // いつでも読み書きできる (待たない)
            Kind::Null | Kind::Zero | Kind::Random | Kind::Inode(..) | Kind::Block(_) | Kind::Fb => alloc::vec![],
            Kind::Input(n) => alloc::vec![crate::input::chan(*n)],
            Kind::SndCtl => alloc::vec![],
            Kind::SndPcm => alloc::vec![crate::sound::chan()],
            _ => return None,
        })
    }

    pub fn lseek(&mut self, off: i64, whence: u32) -> Result<usize, i64> {
        let size = match &self.kind {
            Kind::Inode(ino, _) => ino.meta().size as i64,
            Kind::Null | Kind::Zero | Kind::Random => 0,
            Kind::Block(p) => blk_size(p) as i64,
            Kind::Fb => crate::gpu::get().map_or(0, |g| g.size()) as i64,
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
        let list = match &self.dirents {
            Some(l) if self.offset != 0 => l.clone(),
            _ => {
                let parent = vfs::resolve("", &vfs::normalize(path, ".."), true).unwrap_or_else(|_| dir.clone());
                let mut list = Vec::new();
                list.push((dir.meta().ino, String::from("."), vfs::S_IFDIR));
                list.push((parent.meta().ino, String::from(".."), vfs::S_IFDIR));
                for e in dir.readdir()? {
                    list.push((e.ino, e.name, e.mode));
                }
                let l = Rc::new(list);
                self.dirents = Some(l.clone());
                l
            }
        };
        for (i, (ino, name, mode)) in list.iter().enumerate().skip(self.offset) {
            let (ino, mode) = (*ino, *mode);
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
            Kind::SndPcm => crate::sound::close_pcm(),
            Kind::Inode(ino, path) => {
                let ev = if self.writable() { crate::inotify::IN_CLOSE_WRITE } else { crate::inotify::IN_CLOSE_NOWRITE };
                crate::inotify::file_event(path, ino, ev);
            }
            Kind::PipeRead(p) => {
                { let mut pp = p.borrow_mut(); pp.readers -= 1; pp.generation += 1; }
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::poll_wake(Rc::as_ptr(p) as usize);
            }
            Kind::PipeWrite(p) => {
                { let mut pp = p.borrow_mut(); pp.writers -= 1; pp.generation += 1; }
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::poll_wake(Rc::as_ptr(p) as usize);
            }
            Kind::PipeRw(p) => {
                let mut pp = p.borrow_mut();
                pp.readers -= 1;
                pp.writers -= 1;
                pp.generation += 1;
                drop(pp);
                proc::wakeup(Rc::as_ptr(p) as usize);
                proc::poll_wake(Rc::as_ptr(p) as usize);
            }
            Kind::Pair(rx, tx) => {
                { let mut pp = rx.borrow_mut(); pp.readers -= 1; pp.generation += 1; }
                { let mut pp = tx.borrow_mut(); pp.writers -= 1; pp.generation += 1; }
                proc::wakeup(Rc::as_ptr(rx) as usize);
                proc::wakeup(Rc::as_ptr(tx) as usize);
                proc::poll_wake(Rc::as_ptr(rx) as usize);
                proc::poll_wake(Rc::as_ptr(tx) as usize);
            }
            Kind::Tty(t) if matches!(t.borrow().dev, tty::Dev::Pty(_)) => tty::close_slave(t),
            Kind::PtyMaster(t) => tty::close_master(t),
            _ => {}
        }
    }
}

/// パイプの大きさ (Linux と同じ既定値と、F_SETPIPE_SZ で広げられる上限)
pub const PIPE_SIZE: usize = 64 * 1024;

/// 大きなロックなしの read/write (Pipe::fast_rw) の数: [0] はうまくいったもの、ほかはふつうの道へ行ったわけ
/// (2 大きさ、3 fd の表をほかのスレッドが変えている途中、4 パイプでない、5 6 7 ... は fast_rw_inner を見よ)。/proc/bkl に出す
pub static FAST_RW: [core::sync::atomic::AtomicU64; 12] = [const { core::sync::atomic::AtomicU64::new(0) }; 12];
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
                // 書いたところ (p.2 まで) しか読まないので、0 にしなくてよい
                let p = crate::kalloc::alloc_dirty().ok_or(-12i64)?; // ENOMEM
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

    /// 全部入れるか、何も入れない (ページが足りなければ Err で、中身は変えない)
    fn push_all(&mut self, src: &[u8]) -> Result<(), i64> {
        let room = self.pages.back().map_or(0, |p| PGSIZE - p.2);
        let need = src.len().saturating_sub(room).div_ceil(PGSIZE);
        let mut fresh = Vec::with_capacity(need);
        for _ in 0..need {
            match crate::kalloc::alloc_dirty() {
                Some(p) => fresh.push(p),
                None => {
                    for p in fresh {
                        crate::kalloc::free(p);
                    }
                    return Err(-12); // ENOMEM
                }
            }
        }
        let mut done = 0;
        let mut fresh = fresh.into_iter();
        while done < src.len() {
            if self.pages.back().is_none_or(|p| p.2 == PGSIZE) {
                self.pages.push_back((fresh.next().unwrap(), 0, 0));
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

    /// 先頭から n バイトを捨てる (pop で写したあと)
    fn skip(&mut self, mut n: usize) {
        n = n.min(self.len);
        self.len -= n;
        while n > 0 {
            let (p, r, w) = self.pages[0];
            let k = (w - r).min(n);
            self.pages[0].1 += k;
            n -= k;
            if self.pages[0].1 == PGSIZE {
                crate::kalloc::free(p);
                self.pages.pop_front();
            }
        }
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
    /// 書かれたり口が閉じたりするたびに増える (epoll の EPOLLET が「新しいこと」を見分ける)
    pub generation: u64,
    /// これまでに書かれた / 読まれたバイト数 (SCM_RIGHTS の fd がどのバイトについてきたか)
    pub wrote: u64,
    pub taken: u64,
    /// 分けた NET の中の TCP (unix.rs の inet): このパイプに書く口のポート (getsockname / getpeername)
    pub inet: Option<u16>,
    /// sendmsg の SCM_RIGHTS で送られた fd (wrote のどこから始まるデータについてきたか)
    pub rights: alloc::collections::VecDeque<(u64, Vec<FileRef>)>,
    /// 調べるための記録: 最近の書きこみの頭 20 バイト (IPC のメッセージの見出し)
    pub heads: alloc::collections::VecDeque<[u8; 20]>,
    /// 調べるための記録: 書きこみの頭の IPC のメッセージの型ごとの (回数, 最後に書いたのは何回目の書きこみか)
    pub types: alloc::collections::BTreeMap<u32, (u32, u64)>,
    /// AF_UNIX: このパイプに書くほうのプロセス (pid, uid, gid)。読むほうの SO_PEERCRED (unix.rs)
    pub cred: Option<(u32, u32, u32)>,
    /// このパイプで眠っているもの (読む・書く・FIFO を開く) の数。大きなロックなしの read/write (fast_rw) は、
    /// 0 でなければ起こしを頼む
    sleepers: usize,
    /// poll / select で、いま待っている (待つかもしれない) 数 (PollWatch)。0 でなければ fast_rw は起こしを頼む
    watchers: usize,
    /// epoll に登録されたことがある (いつ待たれるかわからないので、fast_rw はいつも起こしを頼む)
    pub epolled: bool,
}

/// 大きなロックなしの read (syscall::fast): ext4 のふつうのファイルで、読むところがページキャッシュに全部あるとき。
/// fd の表をほかのスレッドと、開いたファイル (オフセット) をほかの fd やプロセスと分けていない、
/// IN_ACCESS を待つ inotify の見張りがない、ときだけ。ほかは None でふつうの道へ
pub fn fast_read_file(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Option<i64> {
    const MAX: usize = 256 * 1024;
    if len == 0 || len > MAX || crate::inotify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let files = me.files.as_ref()?;
    if !files.private() {
        return None;
    }
    let f = files.get().get(fd)?;
    if Rc::strong_count(f) != 1 {
        return None;
    }
    // この fd の開いたファイルは、このスレッドのほかにだれも持っていない (借りずに見て、オフセットを進める)
    let of = unsafe { &mut *f.as_ptr() };
    if of.flags & O_ACCMODE == O_WRONLY {
        return None;
    }
    let Kind::Inode(ino, _) = &of.kind else { return None };
    if !ino.page_cacheable() {
        return None;
    }
    let n = crate::vm::read_fast(me.pt(), ino.id(), of.offset, va, len)?;
    of.offset += n;
    Some(n as i64)
}

/// 大きなロックなしの pread64 (syscall::fast): ext4 のふつうのファイルで、読むところがページキャッシュに全部
/// あるとき。オフセットを動かさないので、fd の表をほかのスレッドと分けていても (門を通って) 読める
pub fn fast_pread(me: &proc::Proc, fd: u64, va: usize, len: usize, off: i64) -> Option<i64> {
    const MAX: usize = 256 * 1024;
    if len == 0 || len > MAX || off < 0 || crate::inotify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let files = me.files.as_ref()?;
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let f = files.get().get(fd)?;
    // ほかのプロセス (fork で分けた) が大きなロックの中で借りているかもしれないので、借りずに見る。
    // 開いたファイルの種類と口 (flags の読み書き) はあとで変わらない
    let of = unsafe { &*f.as_ptr() };
    if of.flags & O_ACCMODE == O_WRONLY {
        return None;
    }
    let Kind::Inode(ino, _) = &of.kind else { return None };
    if !ino.page_cacheable() {
        return None;
    }
    crate::vm::read_fast(me.pt(), ino.id(), off as usize, va, len).map(|n| n as i64)
}

/// 大きなロックなしの splice (syscall::fast)。uutils の cat は、ファイル → (自分で作った) パイプ → 出力 と
/// 2 回 splice する。眠らずにすむものだけ:
///   パイプ → /dev/null: パイプのロックだけで捨てる (中身があるとき)
///   ext4 のファイル → パイプ: 読むところがページキャッシュに全部あり、パイプに全部入るとき (半分だけ
///   入れることはしない)。オフセットは off_in (ユーザーのメモリ) か、だれとも分けていない開いたファイルの
/// ほかは None でふつうの道へ
pub fn fast_splice(me: &proc::Proc, fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize) -> Option<i64> {
    if len == 0 || off_out != 0 {
        return None;
    }
    let len = len.min(64 * 1024);
    let files = me.files.as_ref()?;
    let private = files.private();
    let _gate = if private { None } else { Some(files.get().gate.read()?) };
    let fi = files.get().get(fd_in)?;
    let fo = files.get().get(fd_out)?;
    let (ofi, ofo) = unsafe { (&mut *fi.as_ptr(), &*fo.as_ptr()) };
    let wake = |p: &Rc<PipeCell>| {
        FAST_RW[6].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::smp::defer_wake(Rc::as_ptr(p) as usize);
    };
    match (&ofi.kind, &ofo.kind) {
        (Kind::PipeRead(p), Kind::Null) if off_in == 0 => {
            let mut pp = p.borrow_mut();
            if pp.data.len == 0 {
                return None;
            }
            let k = len.min(pp.data.len);
            pp.data.skip(k);
            pp.taken += k as u64;
            let w = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
            drop(pp);
            if w {
                wake(p);
            }
            Some(k as i64)
        }
        (Kind::Inode(ino, _), Kind::PipeWrite(p)) => {
            if ofi.flags & O_ACCMODE == O_WRONLY || !ino.page_cacheable() || crate::inotify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            // オフセット: off_in があればそこ (あとで書きもどす)、なければ開いたファイルの (だれとも分けていないとき)
            let off = if off_in != 0 {
                let mut b = [0u8; 8];
                if !me.pt().copy_in_nofault(off_in, &mut b) {
                    return None;
                }
                let o = i64::from_le_bytes(b);
                if o < 0 {
                    return None;
                }
                o as usize
            } else {
                if !private || Rc::strong_count(fi) != 1 {
                    return None;
                }
                ofi.offset
            };
            let mut woke = false;
            let n = crate::vm::with_cached(ino.id(), off, len, |parts| {
                let total: usize = parts.iter().map(|x| x.len()).sum();
                let mut pp = p.borrow_mut();
                if pp.readers == 0 || pp.cap.saturating_sub(pp.data.len) < total {
                    return false;
                }
                if off_in != 0 && !me.pt().copy_out_nofault(off_in, &((off + total) as i64).to_le_bytes()) {
                    return false;
                }
                for x in parts {
                    if pp.data.push_all(x).is_err() {
                        // ページがとれなかった: 入れた分はそのまま (大きなロックの道でも同じく途中までになる)
                        break;
                    }
                }
                pp.generation += 1;
                pp.wrote += total as u64;
                woke = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
                true
            })?;
            if off_in == 0 {
                ofi.offset = off + n;
            }
            if woke {
                wake(p);
            }
            Some(n as i64)
        }
        _ => None,
    }
}

/// パイプ (key はその PipeCell の場所) で眠っているもの、poll / epoll で待っているものを起こす (大きなロックを持って)
pub fn wake_key(key: usize) {
    proc::wakeup(key);
    proc::poll_wake(key);
}

/// poll / select が待っているあいだ、見ているパイプに印をつける (watchers)。大きなロックなしの read/write
/// (fast_rw) は、印のあるパイプでは起こしを頼む。readiness を見る前につけること
pub struct PollWatch(Vec<Rc<PipeCell>>);

impl PollWatch {
    pub fn new<'a>(files: impl Iterator<Item = &'a FileRef>) -> Self {
        let mut v = Vec::new();
        for f in files {
            match &f.borrow().kind {
                Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                    p.borrow_mut().watchers += 1;
                    v.push(p.clone());
                }
                _ => {}
            }
        }
        PollWatch(v)
    }
}

impl Drop for PollWatch {
    fn drop(&mut self) {
        for p in &self.0 {
            p.borrow_mut().watchers -= 1;
        }
    }
}

/// epoll に登録した (Pipe::epolled)
pub fn mark_epolled(f: &FileRef) {
    if let Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) = &f.borrow().kind {
        p.borrow_mut().epolled = true;
    }
}

/// パイプの中身のロック。ふだんは大きなロックの中で使うが、大きなロックなしの read/write (Pipe::fast_rw)
/// とも分けあう。持ったまま眠らないこと。同じ CPU が二重に取ったら (持ったまま眠ったときも) 止める
pub struct PipeCell {
    owner: core::sync::atomic::AtomicUsize,
    inner: crate::spinlock::SpinLock<Pipe>,
}

pub struct PipeGuard<'a> {
    g: crate::spinlock::Guard<'a, Pipe>,
    owner: &'a core::sync::atomic::AtomicUsize,
}

impl PipeCell {
    pub fn new(p: Pipe) -> Self {
        Self { owner: core::sync::atomic::AtomicUsize::new(0), inner: crate::spinlock::SpinLock::new(p) }
    }

    pub fn borrow_mut(&self) -> PipeGuard<'_> {
        use core::sync::atomic::Ordering::Relaxed;
        let me = crate::smp::id() + 1;
        if self.owner.load(Relaxed) == me {
            panic!("pipe: cpu{} locks a pipe twice", me - 1);
        }
        let g = self.inner.lock();
        self.owner.store(me, Relaxed);
        PipeGuard { g, owner: &self.owner }
    }

    pub fn borrow(&self) -> PipeGuard<'_> {
        self.borrow_mut()
    }
}

impl Drop for PipeGuard<'_> {
    fn drop(&mut self) {
        self.owner.store(0, core::sync::atomic::Ordering::Relaxed);
    }
}

impl core::ops::Deref for PipeGuard<'_> {
    type Target = Pipe;
    fn deref(&self) -> &Pipe {
        &self.g
    }
}

impl core::ops::DerefMut for PipeGuard<'_> {
    fn deref_mut(&mut self) -> &mut Pipe {
        &mut self.g
    }
}

impl Pipe {
    pub fn new() -> (Kind, Kind) {
        let q = PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        let p = Rc::new(PipeCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 1, writers: 1, r_opened: 1, w_opened: 1, generation: 0, wrote: 0, taken: 0, rights: alloc::collections::VecDeque::new(), heads: alloc::collections::VecDeque::new(), types: alloc::collections::BTreeMap::new(), cred: None, inet: None, sleepers: 0, watchers: 0, epolled: false }));
        (Kind::PipeRead(p.clone()), Kind::PipeWrite(p))
    }

    pub fn len(&self) -> usize {
        self.data.len
    }

    /// FIFO を開く。相手 (読み手なら書き手) が来るまで待つ (nonblock なら待たない)
    pub fn open_fifo(p: &Rc<PipeCell>, read: bool, write: bool, nonblock: bool) -> Result<Kind, i64> {
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
                let pp = p.borrow_mut();
                let (now, opened) = if read { (pp.writers, pp.w_opened) } else { (pp.readers, pp.r_opened) };
                if now > 0 || opened != seen {
                    break;
                }
                Pipe::sleep(p, pp)?;
            }
        }
        Ok(kind)
    }

    /// socketpair: 向かい合わせにつないだ 2 本のパイプ
    pub fn pair() -> (Kind, Kind) {
        let q = || PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        let mk = || Rc::new(PipeCell::new(Pipe { data: q(), cap: PIPE_SIZE, readers: 1, writers: 1, r_opened: 1, w_opened: 1, generation: 0, wrote: 0, taken: 0, rights: alloc::collections::VecDeque::new(), heads: alloc::collections::VecDeque::new(), types: alloc::collections::BTreeMap::new(), cred: None, inet: None, sleepers: 0, watchers: 0, epolled: false }));
        let (a, b) = (mk(), mk());
        (Kind::Pair(a.clone(), b.clone()), Kind::Pair(b, a))
    }

    /// 誰も開いていない FIFO 用
    pub fn empty() -> Rc<PipeCell> {
        let q = PageQueue { pages: alloc::collections::VecDeque::new(), len: 0 };
        Rc::new(PipeCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 0, writers: 0, r_opened: 0, w_opened: 0, generation: 0, wrote: 0, taken: 0, rights: alloc::collections::VecDeque::new(), heads: alloc::collections::VecDeque::new(), types: alloc::collections::BTreeMap::new(), cred: None, inet: None, sleepers: 0, watchers: 0, epolled: false }))
    }

    /// 大きなロックなしの read / write (syscall::fast)。パイプで、眠らずにすむときだけ。ほかは None で
    /// ふつうの道へ: fd の表をほかのスレッドが変えている途中、読むのに中身がない、書くのに全部は入らない・
    /// 読み手がいない、ユーザーのメモリが写っていない (ページフォルトになる)。
    /// 眠っている人や poll / epoll で待っている人がいれば、起こすのは大きなロックを持つ CPU に頼む (smp::defer_wake)
    pub fn fast_rw(me: &proc::Proc, fd: u64, buf: usize, len: usize, write: bool) -> Option<i64> {
        let r = Pipe::fast_rw_inner(me, fd, buf, len, write);
        FAST_RW[r.err().unwrap_or(0)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        r.ok()
    }

    /// うまくいけば Ok、ふつうの道へ行くなら Err(わけ: FAST_RW の番号)
    fn fast_rw_inner(me: &proc::Proc, fd: u64, buf: usize, len: usize, write: bool) -> Result<i64, usize> {
        if len == 0 || len > PIPE_SIZE {
            return Err(2);
        }
        let files = me.files.as_ref().ok_or(3usize)?;
        // ほかのスレッドと分けている表なら、門 (proc::Gate) を通って見る: 見ているあいだは、ほかの CPU が
        // fd を閉じたり表を伸ばしたりしない (変える側が待つ)。だれかが変えている途中ならふつうの道へ
        let _gate = if files.private() { None } else { Some(files.get().gate.read().ok_or(3usize)?) };
        let f = files.get().get(fd).ok_or(4usize)?;
        // ほかのプロセス (fork で分けた) が大きなロックの中で借りているかもしれないので、借りずに見る。
        // パイプの口の種類はあとで変わらない
        let of = unsafe { &*f.as_ptr() };
        let p = match (&of.kind, write) {
            (Kind::PipeRead(p), false) | (Kind::PipeWrite(p), true) => p,
            _ => return Err(4),
        };
        let mut tmp = Vec::with_capacity(len);
        unsafe { tmp.set_len(len) };
        if write {
            if !me.pt().copy_in_nofault(buf, &mut tmp) {
                return Err(5);
            }
            let mut pp = p.borrow_mut();
            if pp.readers == 0 || pp.cap.saturating_sub(pp.data.len) < len {
                return Err(8);
            }
            pp.data.push_all(&tmp).map_err(|_| 9usize)?;
            pp.generation += 1;
            pp.wrote += len as u64;
            let wake = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
            drop(pp);
            // 待っている人がいれば、起こすのは大きなロックを持っている CPU に頼む (入れたあとで)
            if wake {
                FAST_RW[6].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                crate::smp::defer_wake(Rc::as_ptr(p) as usize);
            }
            Ok(len as i64)
        } else {
            let mut pp = p.borrow_mut();
            if pp.data.len == 0 {
                return Err(10);
            }
            let n = len.min(pp.data.len);
            pp.data.pop(&mut tmp[..n], false);
            // 写せたときだけ取り除く (ページがなければ、何も変えずにふつうの道へ)
            if !me.pt().copy_out_nofault(buf, &tmp[..n]) {
                return Err(11);
            }
            pp.data.skip(n);
            pp.taken += n as u64;
            let wake = pp.sleepers > 0 || pp.watchers > 0 || pp.epolled;
            drop(pp);
            if wake {
                FAST_RW[6].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                crate::smp::defer_wake(Rc::as_ptr(p) as usize);
            }
            Ok(n as i64)
        }
    }

    /// 眠る (pp を持ったまま決めたこと: 中身がない、いっぱい ... のあとで)。眠っている数を pp の中で増やしてから
    /// 放すので、大きなロックなしの read/write (fast_rw) は、起こさずに進めてしまうことがない
    fn sleep(p: &Rc<PipeCell>, mut pp: PipeGuard<'_>) -> Result<(), i64> {
        pp.sleepers += 1;
        drop(pp);
        let r = proc::sleep(Rc::as_ptr(p) as usize);
        p.borrow_mut().sleepers -= 1;
        r
    }

    fn wake(p: &Rc<PipeCell>) {
        wake_key(Rc::as_ptr(p) as usize);
    }

    /// 読む。peek なら取り除かない (tee 用)
    pub fn read_ex(p: &Rc<PipeCell>, dst: &mut [u8], peek: bool, nonblock: bool) -> Result<usize, i64> {
        loop {
            let mut pp = p.borrow_mut();
            if pp.data.len > 0 {
                let n = pp.data.pop(dst, !peek);
                if !peek {
                    pp.taken += n as u64;
                }
                drop(pp);
                Pipe::wake(p);
                return Ok(n);
            }
            if pp.writers == 0 {
                return Ok(0);
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// read_ex と同じく待つが、写さずに n バイトまで捨てる (splice の読み減らしと、/dev/null へ)
    pub fn discard(p: &Rc<PipeCell>, n: usize, nonblock: bool) -> Result<usize, i64> {
        loop {
            let mut pp = p.borrow_mut();
            if pp.data.len > 0 {
                let k = n.min(pp.data.len);
                pp.data.skip(k);
                pp.taken += k as u64;
                drop(pp);
                Pipe::wake(p);
                return Ok(k);
            }
            if pp.writers == 0 {
                return Ok(0);
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// nonblock なら、書けるだけ書いて、1 バイトも書けなければ EAGAIN
    fn write(p: &Rc<PipeCell>, src: &[u8], nonblock: bool) -> Result<usize, i64> {
        let mut done = 0;
        while done < src.len() {
            let before = done;
            {
                let mut pp = p.borrow_mut();
                if pp.readers == 0 {
                    return if done > 0 { Ok(done) } else { Err(-EPIPE) };
                }
                let room = pp.cap.saturating_sub(pp.data.len);
                let k = room.min(src.len() - done);
                if k > 0 {
                    if done == 0 && src.len() >= 20 {
                        if pp.heads.len() >= 32 {
                            pp.heads.pop_front();
                        }
                        pp.heads.push_back(src[..20].try_into().unwrap());
                        // IPC でないもの (ふつうのパイプ) では型がばらばらなので、128 種類まで
                        let t = u32::from_le_bytes(src[4..8].try_into().unwrap());
                        let nth = pp.generation;
                        if pp.types.len() < 128 || pp.types.contains_key(&t) {
                            let e = pp.types.entry(t).or_insert((0, 0));
                            e.0 += 1;
                            e.1 = nth;
                        }
                    }
                    pp.data.push(&src[done..done + k])?;
                    pp.generation += 1;
                    pp.wrote += k as u64;
                    done += k;
                }
            }
            // 入れたときだけ起こす (いっぱいで入れられなかった書き手どうしが、起こしあって回りつづけないように)
            if done > before {
                Pipe::wake(p);
            }
            if done < src.len() {
                if nonblock {
                    const EAGAIN: i64 = 11;
                    return if done > 0 { Ok(done) } else { Err(-EAGAIN) };
                }
                // いっぱいのまま (起こしたあとに読まれていなければ) 眠る
                let pp = p.borrow_mut();
                if pp.readers > 0 && pp.data.len >= pp.cap
                    && let Err(e) = Pipe::sleep(p, pp)
                {
                    return if done > 0 { Ok(done) } else { Err(e) };
                }
            }
        }
        Ok(done)
    }
}

// ---- ブロックデバイス ----

const SECTOR: usize = crate::block::SECTOR;
/// 1 回の読み書きの大きさ
const BLK_CHUNK: usize = 64 * 1024;

/// 区画のバイト数
pub fn blk_size(p: &crate::block::Part) -> usize {
    p.len as usize * SECTOR
}

fn blk_read(p: &crate::block::Part, off: usize, dst: &mut [u8]) -> Result<usize, i64> {
    let n = dst.len().min(blk_size(p).saturating_sub(off));
    let mut sec = [0u8; SECTOR];
    let mut done = 0;
    while done < n {
        let pos = off + done;
        let (s, skip) = ((pos / SECTOR) as u64, pos % SECTOR);
        if skip == 0 && n - done >= SECTOR {
            let len = ((n - done) / SECTOR * SECTOR).min(BLK_CHUNK);
            crate::block::read_part(p, s, &mut dst[done..done + len])?;
            done += len;
        } else {
            crate::block::read_part(p, s, &mut sec)?;
            let len = (SECTOR - skip).min(n - done);
            dst[done..done + len].copy_from_slice(&sec[skip..skip + len]);
            done += len;
        }
    }
    Ok(n)
}

fn blk_write(p: &crate::block::Part, off: usize, src: &[u8]) -> Result<usize, i64> {
    const ENOSPC: i64 = 28;
    let n = src.len().min(blk_size(p).saturating_sub(off));
    if n == 0 && !src.is_empty() {
        return Err(-ENOSPC);
    }
    let mut sec = [0u8; SECTOR];
    let mut done = 0;
    while done < n {
        let pos = off + done;
        let (s, skip) = ((pos / SECTOR) as u64, pos % SECTOR);
        if skip == 0 && n - done >= SECTOR {
            let len = ((n - done) / SECTOR * SECTOR).min(BLK_CHUNK);
            crate::block::write_part(p, s, &src[done..done + len])?;
            done += len;
        } else {
            // セクタの一部: 読んでから書く
            crate::block::read_part(p, s, &mut sec)?;
            let len = (SECTOR - skip).min(n - done);
            sec[skip..skip + len].copy_from_slice(&src[done..done + len]);
            crate::block::write_part(p, s, &sec)?;
            done += len;
        }
    }
    Ok(n)
}
