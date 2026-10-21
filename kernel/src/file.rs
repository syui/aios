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
/// 場所だけ指す fd (読み書きはできない。Linux の FMODE_PATH)
pub const O_PATH: u32 = 0o10000000;
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
    /// AF_INET の ICMP (ping) と raw (smoltcp: socket.rs)
    Socket(crate::socket::SockRef),
    /// AF_UNIX のソケット (socketpair も: unix.rs)
    Unix(crate::unix::UnixRef),
    /// AF_INET の TCP と UDP (inet.rs)
    Inet(crate::inet::InetRef),
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
    /// ディレクトリ: getdents で読んだ一覧。続きはここから返す
    dirents: Option<Rc<Dirents>>,
    /// ふつうのファイルを読んでいる途中 (read_inode_file。ディスクを眠って待つことがある)。同じ開いたファイルを
    /// 読むほかの人は、終わるまで待つ (オフセットを同時に進めない)
    reading: bool,
}

pub type FileRef = Rc<RefCell<OpenFile>>;

/// 開いたディレクトリの一覧 (getdents の続き)
pub struct Dirents {
    /// (読む位置, inode, 名前, mode)。位置の順
    list: Vec<(u64, u64, String, u32)>,
    /// ext4 (dir_index) のハッシュの位置 (Linux と同じ): 終わりは i64::MAX。ディレクトリが変わったら、いまの
    /// 位置から先を読みなおす (位置は名前で決まるのでずれない)。でなければ位置は何番めか (0 が .、1 が ..)。
    /// こちらは最初の getdents (offset 0) で読んだものを使い続ける (読みながら消しても番号がずれず、
    /// 読み飛ばさない。rewinddir で 0 に戻れば読みなおす)
    hashed: bool,
    /// 読んだときのディレクトリの版 (vfs::dir_gen)
    generation: u64,
}

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
    write_opt2(f, src, dontwait, None)
}

/// append: Some(true) なら終わりに (pwritev2 の RWF_APPEND)、Some(false) なら O_APPEND でも位置に (RWF_NOAPPEND)、
/// None なら開いたときの O_APPEND のとおり
pub fn write_opt2(f: &FileRef, src: &[u8], dontwait: bool, append: Option<bool>) -> Result<usize, i64> {
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
        None => f.borrow_mut().write_append(src, append),
    }
}

fn read_stream(k: &Kind, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    match k {
        Kind::Tty(t) => tty::read(t, dst, nonblock),
        Kind::PtyMaster(t) => tty::master_read(t, dst, nonblock),
        Kind::PipeRead(p) | Kind::PipeRw(p) => Pipe::read_ex(p, dst, false, nonblock),
        Kind::Unix(u) => crate::unix::read(u, dst, nonblock),
        Kind::Inet(u) => crate::inet::read(u, dst, nonblock),
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
        Kind::PipeWrite(p) | Kind::PipeRw(p) => {
            let n = Pipe::write(p, src, nonblock)?;
            // 名前つきのパイプ (FIFO) は、書けたら時刻を変える (Linux の fifo_pipe_write)。pipe(2) のものは変えない
            // (Linux 6.7 から: anon_pipe_write)
            if n > 0 && p.fifo.load(core::sync::atomic::Ordering::Relaxed) {
                p.touch();
            }
            Ok(n)
        }
        Kind::Unix(u) => crate::unix::write(u, src, nonblock),
        Kind::Inet(u) => crate::inet::write(u, src, nonblock),
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

    pub fn dev(mode: u32, rdev: u64) -> Stat {
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
    /// statx の答え。mask は頼まれたもの。mg (ext4 と tmpfs: Linux の multigrain の時刻) なら、mtime も ctime も
    /// 頼まれていなければ、その 2 つは出さない (Linux の fill_mg_cmtime)
    pub fn to_statx(&self, mask: u32, mg: bool) -> [u8; 256] {
        const MTIME: u32 = 0x40;
        const CTIME: u32 = 0x80;
        let cm = !mg || mask & (MTIME | CTIME) != 0;
        let mut b = [0u8; 256];
        // stx_mask = STATX_BASIC_STATS (と、なければ mtime / ctime をのぞく)
        b[0..4].copy_from_slice(&(if cm { 0x7ffu32 } else { 0x7ff & !(MTIME | CTIME) }).to_le_bytes());
        b[4..8].copy_from_slice(&4096u32.to_le_bytes());
        b[16..20].copy_from_slice(&self.nlink.to_le_bytes());
        b[20..24].copy_from_slice(&crate::ns::show_uid(self.uid).to_le_bytes());
        b[24..28].copy_from_slice(&crate::ns::show_gid(self.gid).to_le_bytes());
        b[28..30].copy_from_slice(&(self.mode as u16).to_le_bytes());
        b[32..40].copy_from_slice(&self.ino.to_le_bytes());
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b[48..56].copy_from_slice(&self.blocks.to_le_bytes());
        // atime, (btime なし), ctime, mtime
        let times: &[(usize, u64)] = if cm { &[(64, self.mtime), (96, self.ctime), (112, self.mtime)] } else { &[(64, self.mtime)] };
        for &(off, ns) in times {
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
    /// 読める口か (O_RDONLY か O_RDWR。Linux の FMODE_READ: O_PATH と、O_ACCMODE (3) で開いたもの (ioctl だけ) は読めない)
    pub fn readable(&self) -> bool {
        self.flags & O_PATH == 0 && matches!(self.flags & O_ACCMODE, O_RDONLY | O_RDWR)
    }

    /// 書ける口か (O_WRONLY か O_RDWR)
    pub fn writable(&self) -> bool {
        self.flags & O_PATH == 0 && matches!(self.flags & O_ACCMODE, O_WRONLY | O_RDWR)
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
            // /dev/random と /dev/urandom: rand の ChaCha20 (どちらも待たない)
            Kind::Random => {
                crate::rand::fill(dst);
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
            Kind::Tty(_) | Kind::PtyMaster(_) | Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) | Kind::Unix(_) | Kind::Inet(_) | Kind::Socket(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::Input(_) => self.kind.clone(),
            _ => return None,
        };
        Some((k, self.flags & O_NONBLOCK != 0))
    }

    /// append: write_opt2 と同じ (None なら開いたときの O_APPEND のとおり)
    pub fn write_append(&mut self, src: &[u8], append: Option<bool>) -> Result<usize, i64> {
        if !self.writable() {
            return Err(-EBADF);
        }
        match &self.kind {
            Kind::Null | Kind::Zero => Ok(src.len()),
            // 池にまぜる (だれが書いてもよい。数えはしない: Linux と同じ)
            Kind::Random => {
                crate::rand::mix(src);
                Ok(src.len())
            }
            Kind::Inode(ino, path) => {
                // 0 バイトは何もしない (終わりのずっと先にいても伸ばさない。O_APPEND でも動かない: Linux の
                // generic_write_checks)
                if src.is_empty() {
                    return Ok(0);
                }
                if append.unwrap_or(self.flags & O_APPEND != 0) {
                    self.offset = ino.meta().size as usize;
                }
                vfs::write_sealed(ino, self.offset, src.len())?;
                vfs::drop_privs(ino)?;
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
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                use core::sync::atomic::Ordering::Relaxed;
                Stat { ino: Rc::as_ptr(p) as usize as u64 & 0xffffff, uid: p.uid.load(Relaxed), gid: p.gid.load(Relaxed), mtime: p.mtime.load(Relaxed), ctime: p.ctime.load(Relaxed), ..Stat::dev(S_IFIFO | p.perm.load(Relaxed), 0) }
            }
            Kind::Socket(_) => Stat::dev(0o140000 | 0o777, 0),
            Kind::Unix(u) => crate::unix::stat(u),
            Kind::Inet(u) => crate::inet::stat(u),
            // 名前のない inode (anon_inode)
            Kind::Epoll(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::PidFd(_) | Kind::Landlock(_) => Stat::dev(0o600, 0),
        }
    }

    /// 状態が変わるたびに増える数 (epoll の EPOLLET 用)。数えていないものは 0
    pub fn event_gen(&self) -> u64 {
        match &self.kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => p.borrow().generation,
            Kind::EventFd(e) => crate::epoll::generation(e),
            Kind::TimerFd(t) => crate::timerfd::generation(t),
            Kind::Inotify(n) => crate::inotify::generation(n),
            Kind::Tty(t) | Kind::PtyMaster(t) => t.borrow().generation.get(),
            Kind::Socket(_) => crate::net::generation(),
            Kind::Unix(u) => u.borrow().generation,
            Kind::Inet(u) => u.borrow().generation + crate::net::generation(),
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
            Kind::Socket(s) => alloc::format!("socket:[{}]", Rc::as_ptr(s) as usize & 0xffffff),
            Kind::Unix(u) => crate::unix::describe(u),
            Kind::Inet(u) => crate::inet::describe(u),
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
            Kind::Unix(u) => alloc::format!("{} {} {}", self.describe(), rw, crate::unix::debug(u)),
            Kind::Inet(u) => alloc::format!("{} {} {}", self.describe(), rw, crate::inet::debug(u)),
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
            Kind::PipeRead(p) | Kind::PipeRw(p) => p.borrow().len() as u64,
            Kind::Unix(u) => crate::unix::pending(u),
            Kind::Inet(u) => crate::inet::pending(u),
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
                (false, !p.full(), p.readers == 0)
            }
            Kind::PipeRw(p) => {
                let p = p.borrow();
                (p.len() > 0, !p.full(), false)
            }
            Kind::Socket(s) => s.borrow().readiness(),
            Kind::Unix(u) => {
                use crate::unix::*;
                let m = poll_mask(u);
                (m & (POLLIN | POLLHUP | POLLERR) != 0, m & POLLOUT != 0, m & (POLLHUP | POLLERR) != 0)
            }
            Kind::Inet(u) => {
                use crate::unix::{POLLERR, POLLHUP, POLLIN, POLLOUT};
                let m = crate::inet::poll_mask(u);
                (m & (POLLIN | POLLHUP | POLLERR) != 0, m & POLLOUT != 0, m & (POLLHUP | POLLERR) != 0)
            }
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

    /// poll のビット (Linux の f_op->poll と同じもの: POLLIN、POLLOUT、POLLERR、POLLHUP、POLLPRI、POLLRDHUP ...)。
    /// AF_UNIX のソケット、パイプ、いつでも読み書きできるもの (Linux の DEFAULT_POLLMASK) だけ。ほかは None
    /// (readiness の 3 つで)
    pub fn poll_mask(&self) -> Option<u32> {
        use crate::unix::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
        Some(match &self.kind {
            Kind::Unix(u) => crate::unix::poll_mask(u),
            Kind::Inet(u) => crate::inet::poll_mask(u),
            // Linux の pipe_poll: 読む口は中身があれば IN、書く口がなければ HUP。書く口は箱があれば OUT、読む口が
            // なければ ERR
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                let p = p.borrow();
                let mut m = 0;
                if self.readable() {
                    if p.len() > 0 {
                        m |= POLLIN | POLLRDNORM;
                    }
                    if p.writers == 0 {
                        m |= POLLHUP;
                    }
                }
                if self.writable() {
                    if !p.full() {
                        m |= POLLOUT | POLLWRNORM;
                    }
                    if p.readers == 0 {
                        m |= POLLERR;
                    }
                }
                m
            }
            Kind::Inode(..) | Kind::Null | Kind::Zero | Kind::Random | Kind::Block(_) => POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM,
            _ => return None,
        })
    }

    /// poll で待つときの印 (変わったときに poll_wake されるもの)。None はわからない (何かあれば起こす)
    pub fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        Some(match &self.kind {
            Kind::Tty(t) | Kind::PtyMaster(t) => alloc::vec![t.borrow().poll_key()],
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::vec![Rc::as_ptr(p) as usize],
            Kind::Unix(u) => crate::unix::poll_keys(u),
            Kind::Inet(u) => crate::inet::poll_keys(u),
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
        // ディレクトリ: ext4 は htree のハッシュの位置なので終わりは i64::MAX (ext4_dir_llseek)、ほか (tmpfs) は
        // SEEK_SET と SEEK_CUR だけ (dcache_dir_lseek)
        if let Kind::Inode(ino, _) = &self.kind
            && ino.meta().is_dir()
        {
            const EXT4_SUPER_MAGIC: u64 = 0xef53;
            let st = ino.statfs();
            let ext4 = u64::from_le_bytes(st[0..8].try_into().unwrap()) == EXT4_SUPER_MAGIC;
            // 消した (rmdir) ext4 のディレクトリは大きさ 0 のふつうのもの (ext4_rmdir が i_size を 0 にし、索引の
            // ないディレクトリとして ext4_llseek): 終わりは 0、SEEK_DATA / SEEK_HOLE は ENXIO、位置は s_maxbytes まで
            if ext4 && ino.meta().nlink == 0 {
                let maxbytes = u32::MAX as i64 * i64::from_le_bytes(st[8..16].try_into().unwrap()).max(1024);
                let new = match whence {
                    0 => Some(off),
                    1 if off == 0 => return Ok(self.offset),
                    1 => (self.offset as i64).checked_add(off),
                    2 => Some(off),
                    3 | 4 => return Err(-6),
                    _ => None,
                };
                let new = new.filter(|&n| n >= 0 && n <= maxbytes).ok_or(-EINVAL)?;
                self.offset = new as usize;
                return Ok(self.offset);
            }
            let new = match whence {
                0 => Some(off),
                1 => (self.offset as i64).checked_add(off),
                2 if ext4 => i64::MAX.checked_add(off),
                // 終わり (i64::MAX) から先と負は ENXIO (generic_file_llseek_size は u64 にしてくらべる)
                3 if ext4 => return if off < 0 || off == i64::MAX { Err(-6) } else { self.offset = off as usize; Ok(self.offset) },
                4 if ext4 => return if off < 0 || off == i64::MAX { Err(-6) } else { self.offset = i64::MAX as usize; Ok(self.offset) },
                _ => None,
            };
            let new = new.filter(|&n| n >= 0).ok_or(-EINVAL)?;
            self.offset = new as usize;
            return Ok(self.offset);
        }
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
        // 穴を知っているもの (ext4 の extent)
        if (whence == SEEK_DATA || whence == SEEK_HOLE)
            && off >= 0
            && off < size
            && let Kind::Inode(ino, _) = &self.kind
            && let Some(r) = ino.seek_data(off as u64, whence == SEEK_HOLE)
        {
            self.offset = r? as usize;
            return Ok(self.offset);
        }
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

    /// linux_dirent64 を詰める。offset は次に読む位置 (Dirents)
    pub fn getdents(&mut self, out: &mut Vec<u8>, max: usize) -> Result<(), i64> {
        let Kind::Inode(dir, path) = &self.kind else { return Err(-ENOTDIR) };
        if !dir.meta().is_dir() {
            return Err(-ENOTDIR);
        }
        // 開いたまま消された (rmdir) ディレクトリは ENOENT (Linux の iterate_dir の IS_DEADDIR)
        if dir.meta().nlink == 0 {
            return Err(-vfs::ENOENT);
        }
        // tmpfs: 全部をコピーせずに、名前の位置 (offset) から (Linux 6.18 の tmpfs と同じ位置と順)
        let (d, p) = (dir.clone(), path.clone());
        if let Some(r) = self.getdents_stream(d, p, out, max) {
            return r;
        }
        let Kind::Inode(dir, path) = &self.kind else { return Err(-ENOTDIR) };
        // ハッシュの位置なら、ディレクトリが変わったら読みなおす (Linux と同じく、まだ読んでいない位置に入った名前も
        // 見える)。大きなものは読みなおさずに、返す名前がまだあるかだけ確かめる (rm -rf のように消しながら読むと、
        // 毎回全部を読むことになるので)
        const RELOAD_MAX: usize = 512;
        let changed = |d: &Dirents| d.hashed && d.generation != vfs::dir_gen(dir.id());
        let fresh = match &self.dirents {
            Some(d) if self.offset != 0 => changed(d) && d.list.len() <= RELOAD_MAX,
            _ => true,
        };
        if fresh {
            let generation = vfs::dir_gen(dir.id());
            let d = match dir.readdir_cookies() {
                Some(r) => Dirents { list: r?.into_iter().map(|(p, e)| (p, e.ino, e.name, e.mode)).collect(), hashed: true, generation },
                None => {
                    let parent = vfs::resolve("", &vfs::normalize(path, ".."), true).unwrap_or_else(|_| dir.clone());
                    let mut list = Vec::new();
                    list.push((0, dir.meta().ino, String::from("."), vfs::S_IFDIR));
                    list.push((1, parent.meta().ino, String::from(".."), vfs::S_IFDIR));
                    for (i, e) in dir.readdir()?.into_iter().enumerate() {
                        list.push((i as u64 + 2, e.ino, e.name, e.mode));
                    }
                    Dirents { list, hashed: false, generation }
                }
            };
            self.dirents = Some(Rc::new(d));
        }
        let d = self.dirents.clone().unwrap();
        let check = changed(&d);
        // 終わりの位置 (ハッシュなら Linux の EXT4_HTREE_EOF_64BIT)
        let end = if d.hashed { i64::MAX as u64 } else { d.list.len() as u64 };
        let pos = self.offset as u64;
        if pos >= end {
            return Ok(());
        }
        let first = d.list.partition_point(|e| e.0 < pos);
        // 終わりまで返したら (残りが消えていたものだけでも) 位置は終わり
        let mut all = true;
        for k in first..d.list.len() {
            let (_, ino, name, mode) = &d.list[k];
            let (ino, mode) = match check {
                false => (*ino, *mode),
                true => match dir.lookup(name) {
                    Ok(i) => (i.meta().ino, i.meta().mode),
                    Err(_) => continue,
                },
            };
            let next = d.list.get(k + 1).map_or(end, |e| e.0);
            let reclen = (19 + name.len() + 1 + 7) & !7;
            if out.len() + reclen > max {
                if out.is_empty() {
                    return Err(-EINVAL);
                }
                all = false;
                break;
            }
            let start = out.len();
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&(next as i64).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(dtype(mode));
            out.extend_from_slice(name.as_bytes());
            out.resize(start + reclen, 0);
            self.offset = next as usize;
        }
        if all {
            self.offset = end as usize;
        }
        Ok(())
    }
}

impl OpenFile {
    fn getdents_stream(&mut self, dir: InodeRef, path: String, out: &mut Vec<u8>, max: usize) -> Option<Result<(), i64>> {
        use crate::tmpfs::DIR_OFFSET_EOD;
        // 使えるか (空の f で読んでみる)
        dir.readdir_offsets(DIR_OFFSET_EOD, &mut |_, _, _, _| false)?.ok()?;
        // Linux の filldir64 と同じく、項目の d_off は次の項目の位置 (入れるときに前のものを書きかえる)、最後の
        // ものは読み終えたときの位置
        let mut prev: Option<usize> = None;
        let mut put = |out: &mut Vec<u8>, ino: u64, name: &str, mode: u32, pos: u64| -> bool {
            let reclen = (19 + name.len() + 1 + 7) & !7;
            if out.len() + reclen > max {
                return false;
            }
            if let Some(p) = prev {
                out[p + 8..p + 16].copy_from_slice(&(pos as i64).to_le_bytes());
            }
            let start = out.len();
            prev = Some(start);
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&0i64.to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(dtype(mode));
            out.extend_from_slice(name.as_bytes());
            out.resize(start + reclen, 0);
            true
        };
        // . が位置 0、.. が 1 (dir_emit_dots)。名前はそれぞれの offset、読み終えたら DIR_OFFSET_EOD
        let mut pos = self.offset as u64;
        let mut full = false;
        if pos == 0 {
            full = !put(out, dir.meta().ino, ".", vfs::S_IFDIR, 0);
            if !full {
                pos = 1;
            }
        }
        if pos == 1 && !full {
            let parent = vfs::resolve("", &vfs::normalize(&path, ".."), true).unwrap_or_else(|_| dir.clone());
            full = !put(out, parent.meta().ino, "..", vfs::S_IFDIR, 1);
            if !full {
                pos = 2;
            }
        }
        if !full && pos != DIR_OFFSET_EOD {
            let r = dir.readdir_offsets(pos, &mut |off, name, ino, mode| {
                pos = off;
                if put(out, ino, name, mode, off) {
                    true
                } else {
                    full = true;
                    false
                }
            })?;
            if let Err(e) = r {
                return Some(Err(e));
            }
            if !full {
                pos = DIR_OFFSET_EOD;
            }
        }
        if let Some(p) = prev {
            out[p + 8..p + 16].copy_from_slice(&(pos as i64).to_le_bytes());
        }
        self.offset = pos as usize;
        // 1 つも入らなかった: 次の項目がバッファより大きい
        if out.is_empty() && full {
            return Some(Err(-EINVAL));
        }
        Some(Ok(()))
    }
}

fn dtype(mode: u32) -> u8 {
    match mode & S_IFMT {
        0o010000 => 1,  // DT_FIFO
        0o020000 => 2,  // DT_CHR
        0o040000 => 4,  // DT_DIR
        0o060000 => 6,  // DT_BLK
        0o100000 => 8,  // DT_REG
        0o120000 => 10, // DT_LNK
        0o140000 => 12, // DT_SOCK
        _ => 0,
    }
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        // flock のロックと OFD ロックは OpenFile ごと (その場所で見分ける): 最後の参照が閉じたら外す
        crate::locks::release_file(self as *const OpenFile as usize);
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
            Kind::Unix(u) => crate::unix::release(u),
            Kind::Inet(u) => crate::inet::release(u),
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
/// 大きなロックなしのファイルの read (fast_read_file) の数: [0] うまくいったもの、ほかはふつうの道へ行ったわけ
/// (1 大きさ、2 IN_ACCESS の見張り、3 fd の表を分けている、4 開いたファイルを分けている、5 ext4 のファイルでない、
/// 6 ページキャッシュにない)。/proc/bkl に出す
pub static FAST_READ: [core::sync::atomic::AtomicU64; 7] = [const { core::sync::atomic::AtomicU64::new(0) }; 7];
pub const PIPE_MAX: usize = 1024 * 1024;

/// パイプの中身: Linux の pipe_buffer と同じく、ページ (4KiB) ひとつずつの箱の列。箱の数はパイプの大きさ / 4KiB
/// まで (Linux の max_usage。POLLOUT は箱があいていること)。write は、書く長さの 4KiB の端数が前の箱にまるごと
/// 入るときだけそこへ足し (merge)、ほかは新しい箱へ (anon_pipe_write)。なので 4KiB (PIPE_BUF) までの write は
/// 分かれない。splice で入れたもの (ファイルのページのかけら) は足さない箱。読みきった箱はすぐ捨てる (pipe_read)
struct PageQueue {
    /// (ページ, 読むところ, 書いたところ, 足してよいか)
    pages: alloc::collections::VecDeque<(*mut u8, usize, usize, bool)>,
    len: usize,
    /// 読みきった箱のページを 1 つとっておき、次の箱に使う (Linux の pipe->tmp_page。小さな write と read を
    /// くりかえすたびに、ページをとって返さないように)
    spare: Option<*mut u8>,
}

impl PageQueue {
    const fn new() -> Self {
        PageQueue { pages: alloc::collections::VecDeque::new(), len: 0, spare: None }
    }

    /// 新しい箱のページ (書いたところしか読まないので、0 にしなくてよい)
    fn page(&mut self) -> Option<*mut u8> {
        self.spare.take().or_else(crate::kalloc::alloc_dirty)
    }

    /// 読みきった箱のページを返す
    fn release(&mut self, p: *mut u8) {
        if self.spare.is_none() {
            self.spare = Some(p);
        } else {
            crate::kalloc::free(p);
        }
    }

    /// 箱が slots こまでのパイプに n バイトを write したら、いま入るバイト数。merge は前の箱に足してよいか
    /// (write の初めだけ)
    fn write_room(&self, n: usize, slots: usize, merge: bool) -> usize {
        let free = slots.saturating_sub(self.pages.len());
        let chars = n % PGSIZE;
        let merged = if merge && chars != 0 && self.pages.back().is_some_and(|b| b.3 && b.2 + chars <= PGSIZE) { chars } else { 0 };
        merged + (n - merged).min(free * PGSIZE)
    }

    /// write の 1 回分: 入るだけ入れて (write_room と同じだけ)、入れたバイト数。ページがとれなければ、何も入れて
    /// いなければ ENOMEM
    fn push_write(&mut self, src: &[u8], slots: usize, merge: bool) -> Result<usize, i64> {
        let mut done = 0;
        let chars = src.len() % PGSIZE;
        if merge
            && chars != 0
            && let Some(b) = self.pages.back_mut()
            && b.3
            && b.2 + chars <= PGSIZE
        {
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), b.0.add(b.2), chars) };
            b.2 += chars;
            done = chars;
        }
        while done < src.len() && self.pages.len() < slots {
            let Some(p) = self.page() else {
                if done == 0 {
                    return Err(-12); // ENOMEM
                }
                break;
            };
            let k = PGSIZE.min(src.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(src[done..].as_ptr(), p, k) };
            self.pages.push_back((p, 0, k, true));
            done += k;
        }
        self.len += done;
        Ok(done)
    }

    /// splice で入れるかけら (ファイルの 1 ページの中のもの。空でなく 4KiB まで) を、足さない箱ひとつに
    fn push_piece(&mut self, src: &[u8]) -> Result<(), i64> {
        let p = self.page().ok_or(-12i64)?; // ENOMEM
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), p, src.len()) };
        self.pages.push_back((p, 0, src.len(), false));
        self.len += src.len();
        Ok(())
    }

    /// 先頭から n バイトを捨てる (pop で写したあと)
    fn skip(&mut self, mut n: usize) {
        n = n.min(self.len);
        self.len -= n;
        while n > 0 {
            let (p, r, w, _) = self.pages[0];
            let k = (w - r).min(n);
            self.pages[0].1 += k;
            n -= k;
            if r + k == w {
                self.release(p);
                self.pages.pop_front();
            }
        }
    }

    /// 先頭から読む。consume なら取り除く
    fn pop(&mut self, dst: &mut [u8], consume: bool) -> usize {
        let mut done = 0;
        let mut i = 0;
        while done < dst.len() && i < self.pages.len() {
            let (p, r, w, _) = self.pages[i];
            let k = (w - r).min(dst.len() - done);
            unsafe { core::ptr::copy_nonoverlapping(p.add(r), dst[done..].as_mut_ptr(), k) };
            done += k;
            if consume {
                self.pages[i].1 += k;
                if r + k == w {
                    self.release(p);
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
        for (p, _, _, _) in self.pages.drain(..) {
            crate::kalloc::free(p);
        }
        if let Some(p) = self.spare.take() {
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
    let r = fast_read_file_inner(me, fd, va, len);
    FAST_READ[r.err().unwrap_or(0)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    r.ok()
}

/// 大きなロックなしの lseek (syscall::fast): ページキャッシュを通るファイル (ext4、tmpfs) の SEEK_SET、SEEK_CUR と、
/// 大きさをページキャッシュが知っているときの SEEK_END。fd の表と開いたファイルをだれとも分けていないときだけ
/// (fast_read_file と同じ。Linux も分けていなければ f_pos_lock を取らない)。位置だけを変えるので、ディレクトリでも同じ
/// (消した ext4 のディレクトリは s_maxbytes までなので、4 TiB より先はふつうの道で)。ほかは None でふつうの道へ
pub fn fast_lseek(me: &proc::Proc, fd: u64, off: i64, whence: u64) -> Option<i64> {
    let files = me.files.as_ref()?;
    if !files.private() {
        return None;
    }
    let f = files.get().get(fd)?;
    if Rc::strong_count(f) != 1 {
        return None;
    }
    let of = unsafe { &mut *f.as_ptr() };
    let Kind::Inode(ino, _) = &of.kind else { return None };
    if of.flags & O_PATH != 0 || !ino.page_cacheable() {
        return None;
    }
    let base = match whence {
        0 => 0,
        1 => of.offset as i64,
        2 => crate::vm::cached_size(ino.id())? as i64,
        _ => return None,
    };
    let new = base.checked_add(off).filter(|&n| (0..1 << 42).contains(&n))?;
    of.offset = new as usize;
    Some(new)
}

/// 開いていない fd の close (EBADF) を大きなロックなしで: fd の表をだれとも分けていないときだけ見る
pub fn fast_close_badf(me: &proc::Proc, fd: u64) -> bool {
    me.files.as_ref().is_some_and(|files| files.private() && files.get().get(fd).is_none())
}

/// 大きなロックなしの /dev/null・/dev/zero・/dev/urandom の read / write (syscall::fast)。書くものは捨てるだけ
/// (ユーザーのメモリも読まない。Linux と同じ)、/dev/null は読むと 0、/dev/zero は 0 で埋める (/dev/urandom の
/// 読みはふつうの道)。開いたファイルの種類と読み書きの口はあとで変わらないので、借りずに見る
pub fn fast_dev(me: &proc::Proc, fd: u64, va: usize, len: usize, write: bool) -> Option<i64> {
    let files = me.files.as_ref()?;
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let f = files.get().get(fd)?;
    let of = unsafe { &*f.as_ptr() };
    if !matches!(of.kind, Kind::Null | Kind::Zero | Kind::Random) {
        return None;
    }
    if write {
        // 書けない口なら EBADF をふつうの道で
        return of.writable().then_some(len as i64);
    }
    if !of.readable() {
        return None;
    }
    match of.kind {
        Kind::Null => Some(0),
        Kind::Zero if len <= 64 * 1024 => {
            const Z: [u8; 4096] = [0; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(Z.len());
                // 写っていないページがあれば、ふつうの道でやりなおす (そこまで書いた 0 はそのままでよい)
                if !me.pt().copy_out_nofault(va + done, &Z[..n]) {
                    return None;
                }
                done += n;
            }
            Some(len as i64)
        }
        _ => None,
    }
}

/// fast_read_file の中身。Err はふつうの道へ行ったわけ (FAST_READ の番号)
fn fast_read_file_inner(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Result<i64, usize> {
    const MAX: usize = 256 * 1024;
    if len == 0 || len > MAX {
        return Err(1);
    }
    if crate::inotify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(2);
    }
    let files = me.files.as_ref().ok_or(3usize)?;
    if !files.private() {
        return Err(3);
    }
    let f = files.get().get(fd).ok_or(5usize)?;
    if Rc::strong_count(f) != 1 {
        return Err(4);
    }
    // この fd の開いたファイルは、このスレッドのほかにだれも持っていない (借りずに見て、オフセットを進める)
    let of = unsafe { &mut *f.as_ptr() };
    if !of.readable() {
        return Err(5);
    }
    let Kind::Inode(ino, _) = &of.kind else { return Err(5) };
    if !ino.page_cacheable() {
        return Err(5);
    }
    let n = crate::vm::read_fast(me.pt(), ino.id(), of.offset, va, len).ok_or(6usize)?;
    of.offset += n;
    Ok(n as i64)
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
    if !of.readable() {
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
/// ほかは None でふつうの道へ (flags のまちがい、O_PATH、読めない口・書けない口、ふつうのファイルでないものも、
/// ふつうの道が Linux の順でエラーにする)
pub fn fast_splice(me: &proc::Proc, fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> Option<i64> {
    if len == 0 || off_out != 0 || flags & !0x0f != 0 {
        return None;
    }
    let len = len.min(64 * 1024);
    let files = me.files.as_ref()?;
    let private = files.private();
    let _gate = if private { None } else { Some(files.get().gate.read()?) };
    let fi = files.get().get(fd_in)?;
    let fo = files.get().get(fd_out)?;
    let (ofi, ofo) = unsafe { (&mut *fi.as_ptr(), &*fo.as_ptr()) };
    if (ofi.flags | ofo.flags) & O_PATH != 0 || !ofi.readable() || !ofo.writable() {
        return None;
    }
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
            // ino.meta() は見ない: inode の中身は大きなロックで守られている (ほかの CPU が書いているかもしれない)。
            // ページキャッシュにはふつうのファイルのページしか入らない (vm::read_cached と mmap) ので、ディレクトリなどは
            // with_cached が None でふつうの道へ (fast_pread と同じ)
            if !ino.page_cacheable() || crate::inotify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) {
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
            // 読み手がいない (EPIPE と SIGPIPE) か箱がない (待つか EAGAIN) なら、ファイルの終わりでもふつうの道
            // (Linux の wait_for_space は読む前)
            {
                let pp = p.borrow();
                if pp.readers == 0 || pp.full() {
                    return None;
                }
            }
            let mut woke = false;
            let n = crate::vm::with_cached(ino.id(), off, len, |parts| {
                let total: usize = parts.iter().map(|x| x.len()).sum();
                let mut pp = p.borrow_mut();
                // ファイルのページのかけらごとに箱ひとつ (Linux の splice と同じ)。全部入るときだけ。箱にはそのときの
                // 中身を写す (aios がえらんだちがい (splice-snapshot): sysfile::splice_file_to_pipe と同じ)
                if pp.readers == 0 || pp.splice_room(off as u64, total) < total {
                    return false;
                }
                if off_in != 0 && !me.pt().copy_out_nofault(off_in, &((off + total) as i64).to_le_bytes()) {
                    return false;
                }
                let mut pos = off;
                for x in parts {
                    let mut i = 0;
                    while i < x.len() {
                        let k = (PGSIZE - pos % PGSIZE).min(x.len() - i);
                        if pp.data.push_piece(&x[i..i + k]).is_err() {
                            // ページがとれなかった: 入れた分はそのまま (大きなロックの道でも同じく途中までになる)
                            break;
                        }
                        i += k;
                        pos += k;
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
    /// fstat に見せる持ち主 (作った人) と許可 (fchmod で変わる)。Linux の pipefs の inode と同じ
    pub uid: core::sync::atomic::AtomicU32,
    pub gid: core::sync::atomic::AtomicU32,
    pub perm: core::sync::atomic::AtomicU32,
    /// 時刻 (ns)。FIFO は書くと変わり (Linux の fifo_pipe_write の file_update_time)、utimensat で決められる
    pub mtime: core::sync::atomic::AtomicU64,
    pub ctime: core::sync::atomic::AtomicU64,
    /// 名前つきのパイプ (FIFO) のもの (sysfile::fifo_pipe)
    pub fifo: core::sync::atomic::AtomicBool,
}

pub struct PipeGuard<'a> {
    g: crate::spinlock::Guard<'a, Pipe>,
    owner: &'a core::sync::atomic::AtomicUsize,
}

impl PipeCell {
    pub fn new(p: Pipe) -> Self {
        use core::sync::atomic::AtomicU32;
        use core::sync::atomic::AtomicU64;
        let c = crate::cred::current();
        let t = crate::timer::epoch_ns();
        Self { owner: core::sync::atomic::AtomicUsize::new(0), inner: crate::spinlock::SpinLock::new(p), uid: AtomicU32::new(c.euid), gid: AtomicU32::new(c.egid), perm: AtomicU32::new(0o600), mtime: AtomicU64::new(t), ctime: AtomicU64::new(t), fifo: core::sync::atomic::AtomicBool::new(false) }
    }

    /// 中身が変わった: mtime と ctime をいまに
    pub fn touch(&self) {
        use core::sync::atomic::Ordering::Relaxed;
        let t = crate::timer::epoch_ns();
        self.mtime.store(t, Relaxed);
        self.ctime.store(t, Relaxed);
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
        let q = PageQueue::new();
        let p = Rc::new(PipeCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 1, writers: 1, r_opened: 1, w_opened: 1, generation: 0, wrote: 0, taken: 0, sleepers: 0, watchers: 0, epolled: false }));
        (Kind::PipeRead(p.clone()), Kind::PipeWrite(p))
    }

    pub fn len(&self) -> usize {
        self.data.len
    }

    /// 読む口の数 (sysfile の write_flags: 入れ物を写す前に EPIPE を見るため)
    pub fn readers(&self) -> usize {
        self.readers
    }

    /// 箱の数 (Linux の max_usage: パイプの大きさ / 4KiB)
    pub fn slots(&self) -> usize {
        (self.cap / PGSIZE).max(1)
    }

    /// 使っている箱の数
    pub fn used_slots(&self) -> usize {
        self.data.pages.len()
    }

    /// 箱がみなうまっている (Linux の pipe_full。write は端数を前の箱に足せるときのほか、入らない)
    pub fn full(&self) -> bool {
        self.used_slots() >= self.slots()
    }

    /// n バイトの write (の初め) が、いま入るバイト数
    pub fn write_room(&self, n: usize) -> usize {
        self.data.write_room(n, self.slots(), true)
    }

    /// splice で、ファイルの位置 pos から len バイトまでを入れるとき (Linux の filemap_splice_read と
    /// shmem_file_splice_read): 長さはあいた箱 × 4KiB まで、ファイルの 1 ページの中のかけらごとに箱ひとつ。
    /// 入るバイト数
    pub fn splice_room(&self, pos: u64, len: usize) -> usize {
        let free = self.slots().saturating_sub(self.used_slots());
        let mut len = len.min(free * PGSIZE);
        let (mut done, mut pos, mut n) = (0, pos, 0);
        while len > 0 && n < free {
            let k = (PGSIZE - (pos % PGSIZE as u64) as usize).min(len);
            done += k;
            len -= k;
            pos += k as u64;
            n += 1;
        }
        done
    }

    /// splice でパイプに入れる前に、箱があくのを待つ (Linux の wait_for_space): 読み手がいなければ EPIPE
    /// (SIGPIPE は呼ぶほうで)、いっぱいなら nonblock は EAGAIN、ほかは待つ
    pub fn wait_space(p: &Rc<PipeCell>, nonblock: bool) -> Result<(), i64> {
        loop {
            let pp = p.borrow_mut();
            if pp.readers == 0 {
                return Err(-EPIPE);
            }
            if !pp.full() {
                return Ok(());
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// splice で入れる: ファイルの位置 pos から読んだ data を、ファイルの 1 ページの中のかけらごとに足さない箱へ
    /// (あいている箱の分だけ)。入れたバイト数。読み手を起こす
    pub fn splice_in(p: &Rc<PipeCell>, mut pos: u64, data: &[u8]) -> usize {
        let mut done = 0;
        {
            let mut pp = p.borrow_mut();
            while done < data.len() && !pp.full() {
                let k = (PGSIZE - (pos % PGSIZE as u64) as usize).min(data.len() - done);
                if pp.data.push_piece(&data[done..done + k]).is_err() {
                    break;
                }
                done += k;
                pos += k as u64;
            }
            if done > 0 {
                pp.generation += 1;
                pp.wrote += done as u64;
            }
        }
        if done > 0 {
            Pipe::wake(p);
        }
        done
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

    /// 誰も開いていない FIFO 用
    pub fn empty() -> Rc<PipeCell> {
        let q = PageQueue::new();
        Rc::new(PipeCell::new(Pipe { data: q, cap: PIPE_SIZE, readers: 0, writers: 0, r_opened: 0, w_opened: 0, generation: 0, wrote: 0, taken: 0, sleepers: 0, watchers: 0, epolled: false }))
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
            if pp.readers == 0 || pp.write_room(len) < len {
                return Err(8);
            }
            let slots = pp.slots();
            pp.data.push_write(&tmp, slots, true).map_err(|_| 9usize)?;
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
        let _p = crate::prof::span("file:pipe_read");
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

    /// splice でパイプから書くとき (Linux の splice_from_pipe_next と iter_file_splice_write): 中身が来るまで待ち
    /// (書き手がいなければ空。nonblock は EAGAIN)、先頭から max バイトまでを読み減らさずに写す。書けた分は、あとで
    /// discard で読み減らす
    pub fn peek_wait(p: &Rc<PipeCell>, max: usize, nonblock: bool) -> Result<Vec<u8>, i64> {
        loop {
            let mut pp = p.borrow_mut();
            if pp.data.len > 0 {
                let n = max.min(pp.data.len);
                // pop が n バイトみな写す (0 で埋めない)
                let mut v = Vec::with_capacity(n);
                unsafe { v.set_len(n) };
                pp.data.pop(&mut v, false);
                return Ok(v);
            }
            if pp.writers == 0 {
                return Ok(Vec::new());
            }
            if nonblock {
                return Err(-11); // EAGAIN
            }
            Pipe::sleep(p, pp)?;
        }
    }

    /// パイプ a からパイプ b へ len バイトまで (Linux の splice_pipe_to_pipe): 箱ごとそのまま動かす (足してよいかの
    /// 印もそのまま)。len が箱より短ければ、その分だけを足さない新しい箱に写し、a の箱は残りを持つ。はじめに、a が空なら
    /// 待ち (書き手がいなければそのまま。nonblock は EAGAIN)、b がいっぱいなら待つ (読み手がいなければ EPIPE。nonblock
    /// は EAGAIN)。b の読み手がいなければ EPIPE (SIGPIPE は呼ぶほうで)、a が空で書き手がいなければ 0
    pub fn splice_pipe(a: &Rc<PipeCell>, b: &Rc<PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
        const EAGAIN: i64 = 11;
        'retry: loop {
            // ipipe_prep
            loop {
                let pa = a.borrow_mut();
                if pa.data.len > 0 || pa.writers == 0 {
                    break;
                }
                if nonblock {
                    return Err(-EAGAIN);
                }
                Pipe::sleep(a, pa)?;
            }
            // opipe_prep
            loop {
                let pb = b.borrow_mut();
                if !pb.full() {
                    break;
                }
                if pb.readers == 0 {
                    return Err(-EPIPE);
                }
                if nonblock {
                    return Err(-EAGAIN);
                }
                Pipe::sleep(b, pb)?;
            }
            // 2 つのロックは場所の順に (Linux の pipe_double_lock)
            let (mut pa, mut pb) = if (Rc::as_ptr(a) as usize) < (Rc::as_ptr(b) as usize) {
                let x = a.borrow_mut();
                (x, b.borrow_mut())
            } else {
                let y = b.borrow_mut();
                (a.borrow_mut(), y)
            };
            let (mut done, mut left, mut freed) = (0, len, false);
            loop {
                if pb.readers == 0 {
                    if done == 0 {
                        return Err(-EPIPE);
                    }
                    break;
                }
                if pa.data.len == 0 && pa.writers == 0 {
                    break;
                }
                if pa.data.len == 0 || pb.full() {
                    if done > 0 {
                        break;
                    }
                    if nonblock {
                        return Err(-EAGAIN);
                    }
                    drop(pa);
                    drop(pb);
                    continue 'retry;
                }
                let (pg, r, w, merge) = pa.data.pages[0];
                let k = w - r;
                if left >= k {
                    pa.data.pages.pop_front();
                    pa.data.len -= k;
                    pb.data.pages.push_back((pg, r, w, merge));
                    pb.data.len += k;
                    freed = true;
                    done += k;
                    left -= k;
                } else {
                    let Some(np) = pb.data.page() else {
                        if done == 0 {
                            return Err(-12); // ENOMEM
                        }
                        break;
                    };
                    unsafe { core::ptr::copy_nonoverlapping(pg.add(r), np, left) };
                    pb.data.pages.push_back((np, 0, left, false));
                    pb.data.len += left;
                    pa.data.pages[0].1 += left;
                    pa.data.len -= left;
                    done += left;
                    left = 0;
                }
                if left == 0 {
                    break;
                }
            }
            if done > 0 {
                pa.taken += done as u64;
                pb.wrote += done as u64;
                pb.generation += 1;
            }
            drop(pa);
            drop(pb);
            if done > 0 {
                Pipe::wake(b);
            }
            if freed {
                Pipe::wake(a);
            }
            return Ok(done);
        }
    }

    /// tee: パイプ a の中身を読み減らさずに、パイプ b へ len バイトまで写す (Linux の link_pipe): a の箱ごとに、
    /// 足さない箱を b に作って写す (len で切れる箱は、その分だけ)。はじめに a が空なら待ち (書き手がいなければそのまま
    /// 0。nonblock は EAGAIN)、b がいっぱいなら待つ (読み手がいなければ EPIPE。nonblock は EAGAIN)。写せたバイト数
    pub fn tee_pipe(a: &Rc<PipeCell>, b: &Rc<PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
        const EAGAIN: i64 = 11;
        loop {
            let pa = a.borrow_mut();
            if pa.data.len > 0 || pa.writers == 0 {
                break;
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            Pipe::sleep(a, pa)?;
        }
        loop {
            let pb = b.borrow_mut();
            if !pb.full() {
                break;
            }
            if pb.readers == 0 {
                return Err(-EPIPE);
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            Pipe::sleep(b, pb)?;
        }
        let (pa, mut pb) = if (Rc::as_ptr(a) as usize) < (Rc::as_ptr(b) as usize) {
            let x = a.borrow_mut();
            (x, b.borrow_mut())
        } else {
            let y = b.borrow_mut();
            (a.borrow_mut(), y)
        };
        let (mut done, mut left, mut i) = (0, len, 0);
        while left > 0 {
            if pb.readers == 0 {
                if done == 0 {
                    return Err(-EPIPE);
                }
                break;
            }
            if i >= pa.data.pages.len() || pb.full() {
                break;
            }
            let (pg, r, w, _) = pa.data.pages[i];
            let k = (w - r).min(left);
            let Some(np) = pb.data.page() else {
                if done == 0 {
                    return Err(-12); // ENOMEM
                }
                break;
            };
            unsafe { core::ptr::copy_nonoverlapping(pg.add(r), np, k) };
            pb.data.pages.push_back((np, 0, k, false));
            pb.data.len += k;
            done += k;
            left -= k;
            i += 1;
        }
        if done > 0 {
            pb.wrote += done as u64;
            pb.generation += 1;
        }
        drop(pa);
        drop(pb);
        if done > 0 {
            Pipe::wake(b);
        }
        Ok(done)
    }

    /// nonblock なら、書けるだけ書いて、1 バイトも書けなければ EAGAIN。入れ方は Linux の anon_pipe_write
    /// (PageQueue): 初めに端数を前の箱に足せたら足し、あとは箱があくだけ
    fn write(p: &Rc<PipeCell>, src: &[u8], nonblock: bool) -> Result<usize, i64> {
        let _p = crate::prof::span("file:pipe_write");
        let mut done = 0;
        let mut first = true;
        while done < src.len() {
            let before = done;
            {
                let mut pp = p.borrow_mut();
                if pp.readers == 0 {
                    return if done > 0 { Ok(done) } else { Err(-EPIPE) };
                }
                let k = pp.data.write_room(src.len() - done, pp.slots(), first);
                if k > 0 {
                    let slots = pp.slots();
                    let k = match pp.data.push_write(&src[done..], slots, first) {
                        Ok(k) => k,
                        Err(e) if done == 0 => return Err(e),
                        Err(_) => return Ok(done),
                    };
                    pp.generation += 1;
                    pp.wrote += k as u64;
                    done += k;
                }
                first = false;
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
                if pp.readers > 0 && pp.full()
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

/// pread の、ふつうのファイル (sysfile が、借りずに読む) でないもの: ディスクと画面は pos から、/dev/null と
/// /dev/zero と乱数は位置によらない。開いたファイルの位置は動かさない。位置のないものは ESPIPE
pub fn pread_dev(f: &FileRef, pos: usize, dst: &mut [u8]) -> Result<usize, i64> {
    match &f.borrow().kind {
        Kind::Block(p) => blk_read(p, pos, dst),
        Kind::Fb => Ok(crate::gpu::read(pos, dst)),
        Kind::Null => Ok(0),
        Kind::Zero => {
            dst.fill(0);
            Ok(dst.len())
        }
        Kind::Random => {
            crate::rand::fill(dst);
            Ok(dst.len())
        }
        _ => Err(-ESPIPE),
    }
}

/// pwrite の、ふつうのファイルでないもの (pread_dev と同じ)
pub fn pwrite_dev(f: &FileRef, pos: usize, src: &[u8]) -> Result<usize, i64> {
    match &f.borrow().kind {
        Kind::Block(p) => blk_write(p, pos, src),
        Kind::Fb => {
            let n = crate::gpu::write(pos, src);
            if n == 0 && !src.is_empty() { Err(-28) } else { Ok(n) }
        }
        Kind::Null | Kind::Zero => Ok(src.len()),
        Kind::Random => {
            crate::rand::mix(src);
            Ok(src.len())
        }
        _ => Err(-ESPIPE),
    }
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
