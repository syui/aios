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
    /// なくても動くところのもの (epoll、eventfd、timerfd、inotify、装置: ディスクと区画、画面、キーボードやマウス、音、
    /// ソケット: AF_UNIX、AF_INET の TCP と UDP と ICMP と raw、AF_NETLINK ...): 中身とその操作 (Linux の file の private_data と f_op。file.rs は FileOps の関数で呼び、名前で呼ばない)
    Ops(Rc<dyn FileOps>),
    /// pidfd_open で開いたプロセス (終わると読める)
    PidFd(u32),
    /// seccomp の user notification の listener (SECCOMP_FILTER_FLAG_NEW_LISTENER。知らせが来ると読める)
    SeccompNotify(Rc<dyn crate::security::Listener>),
    /// landlock_create_ruleset の決まりの束 (中は landlock だけが知る)
    Landlock(Rc<dyn core::any::Any>),
}

/// fd を閉じた (Linux の fput から f_op->release のところ)。AF_UNIX なら、送られている途中のソケットの輪が残ったかも
/// しれないので、unix.rs の gc に知らせる (片づけるのはシステムコールの終わり: release_deferred)
pub fn closed(k: &Kind) {
    if let Kind::Ops(o) = k {
        o.on_close();
    }
}

/// システムコールの終わり (と、プロセスが終わって fd の表を落としたとき: force) に、閉じたあとの片づけ (AF_UNIX の gc:
/// aios がえらんだちがい unix-gc-now。main.rs が register_release_deferred で unix.rs の gc_check を置く)
#[inline]
pub fn release_deferred(force: bool) {
    if let Some(f) = RELEASE_DEFERRED.val() {
        f(force);
    }
}

static RELEASE_DEFERRED: crate::cell::Early<Option<fn(bool)>> = crate::cell::Early::new(None);

pub fn register_release_deferred(f: fn(bool)) {
    RELEASE_DEFERRED.set(Some(f));
}

/// 大きなロックなしのソケットの read / write (syscall::fast: main.rs が register_fast_rw_sock で unix.rs の fast_rw を置く)
type FastRw = fn(&proc::Proc, u64, usize, usize, bool) -> Option<i64>;
static FAST_RW_SOCK: crate::cell::Early<Option<FastRw>> = crate::cell::Early::new(None);

pub fn register_fast_rw_sock(f: FastRw) {
    FAST_RW_SOCK.set(Some(f));
}

/// ファイルのロック (Linux の CONFIG_FILE_LOCKING: fs/locks.c)。main.rs が register_locks で locks.rs をつなぐ。つないで
/// いなければ、外すものはなにもせず、fcntl のロック (F_GETLK など) は EINVAL (Linux のないときと同じ。flock は locks.rs が
/// syscall::register で置くので ENOSYS)
pub trait FileLocks: Sync {
    /// 開いたものの最後の参照が閉じた (file は OpenFile の場所): その flock と OFD ロックを外す
    fn release_file(&self, _file: usize) {}
    /// プロセス (ファイルの表 files) が f の fd を 1 つ閉じた: そのファイルの、そのプロセスの POSIX のロックをみな外す
    fn release_posix(&self, _files: usize, _f: &OpenFile) {}
    /// プロセスのファイルの表がなくなる: その POSIX のロックをみな外す
    fn release_owner(&self, _files: usize) {}
    /// fcntl の F_GETLK・F_SETLK・F_SETLKW と OFD のもの
    fn fcntl(&self, _f: &FileRef, _files: usize, _cmd: u64, _arg: usize) -> Result<i64, i64> {
        Err(-EINVAL)
    }
}

struct NoLocks;

impl FileLocks for NoLocks {}

static LOCKS: crate::cell::Early<&'static dyn FileLocks> = crate::cell::Early::new(&NoLocks);

/// 起動のとき (main.rs) にファイルのロックをつなぐ
pub fn register_locks(l: &'static dyn FileLocks) {
    LOCKS.set(l);
}

pub fn locks() -> &'static dyn FileLocks {
    LOCKS.val()
}

/// poll のビット (Linux と同じ値)
pub const POLLIN: u32 = 0x1;
pub const POLLOUT: u32 = 0x4;
pub const POLLERR: u32 = 0x8;
pub const POLLHUP: u32 = 0x10;
pub const POLLRDNORM: u32 = 0x40;
pub const POLLWRNORM: u32 = 0x100;

/// 開いたものの中身の操作 (Linux の file_operations: Kind::Ops)。それぞれ、何も書かなければ名前のない inode
/// (anon_inode) のふるまい
pub trait FileOps: core::any::Any {
    /// /proc/PID/fd/N の readlink
    fn describe(&self) -> String;
    /// CLASSES の番号
    fn class(&self) -> usize {
        CLASSES.len() - 1
    }
    /// 待つかもしれないもの (読み書きは read と write で。O_NONBLOCK を見る)
    fn stream(&self) -> bool {
        false
    }
    fn read(&self, _dst: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        Err(-EBADF)
    }
    fn write(&self, _src: &[u8], _nonblock: bool) -> Result<usize, i64> {
        Err(-EBADF)
    }
    fn stat(&self) -> Stat {
        Stat::dev(0o600, 0)
    }
    /// マウント (OpenFile::mount)
    fn mount(&self) -> (u64, bool) {
        (vfs::ANON_MNT, false)
    }
    /// /proc/PID/fdinfo/N に足すもの
    fn fdinfo(&self) -> String {
        String::new()
    }
    /// 状態が変わるたびに増える数 (epoll の EPOLLET 用)
    fn event_gen(&self) -> u64 {
        0
    }
    /// 調べるための様子 (/proc/PID/stack の fd の一覧。desc は名前、rw は「r?w?h?」)。None なら「名前 r?w?h?」
    fn debug_state(&self, _desc: &str, _rw: &str) -> Option<String> {
        None
    }
    /// 読まれずに残っているもの (/proc/ai/fd)
    fn pending(&self) -> Option<u64> {
        None
    }
    /// FIONREAD (None なら端末の ioctl へ)
    fn fionread(&self) -> Option<Result<usize, i64>> {
        None
    }
    /// (読める, 書ける, 切れた)
    fn readiness(&self) -> (bool, bool, bool) {
        (true, true, false)
    }
    /// poll のビット (None なら readiness の 3 つで)
    fn poll_mask(&self) -> Option<u32> {
        None
    }
    /// poll で待つときの印 (None はわからない)
    fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        None
    }
    /// ロック (flock、fcntl) がかけられるか
    fn lockable(&self) -> bool {
        false
    }
    /// open で開いたもの (装置。F_GETFL に O_LARGEFILE を見せる)
    fn opened(&self) -> bool {
        false
    }
    /// 位置のあるもの (pread、pwrite、lseek、sendfile の位置: Linux の FMODE_LSEEK と FMODE_PREAD。待つことがなく、頼まれた
    /// 分を最後まで運ぶ): read_at、write_at、size で
    fn seekable(&self) -> bool {
        false
    }
    /// splice の位置と sync_file_range (Linux の FMODE_PREAD / FMODE_PWRITE のうち、ディスク)
    fn positional(&self) -> bool {
        false
    }
    /// RWF_NOWAIT で待たずに読み書きできる
    fn nowait(&self) -> bool {
        false
    }
    fn read_at(&self, _pos: usize, _dst: &mut [u8]) -> Result<usize, i64> {
        Err(-ESPIPE)
    }
    fn write_at(&self, _pos: usize, _src: &[u8]) -> Result<usize, i64> {
        Err(-ESPIPE)
    }
    /// 大きさ (lseek の SEEK_END。None は ESPIPE)
    fn size(&self) -> Option<i64> {
        None
    }
    /// epoll に入れられるか (Linux の f_op->poll があるもの)
    fn pollable(&self) -> bool {
        true
    }
    /// ioctl (None ならふつうのもの: FIONREAD、端末 ...)。nonblock は O_NONBLOCK
    fn ioctl(&self, _req: u64, _arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        None
    }
    /// mmap (None は写せない: ENODEV)
    fn mmap(&self, _fr: &FileRef, _off: usize, _len: usize, _prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
        None
    }
    /// 閉じた (最後の OpenFile が落ちた: Linux の f_op->release)
    fn release(&self) {}

    // ---- ソケットのもの (Linux の struct socket と proto_ops。ほかは何もしない) ----

    /// readiness を poll_mask から (読める: IN|HUP|ERR、書ける: OUT、切れた: HUP|ERR。recvmmsg のエラーも ERR に)
    fn mask_readiness(&self) -> bool {
        false
    }
    /// fd を閉じた (OpenFile がまだあっても: close のたびに)
    fn on_close(&self) {}
    /// poll / select が見ている (on) / やめた (大きなロックなしの read/write が起こしを頼むように)
    fn watch(&self, _on: bool) {}
    /// epoll に登録した
    fn mark_epolled(&self) {}
    /// いま中身があるか (パイプとソケット。ほかの流れはあるものとして)
    fn has_data(&self) -> bool {
        true
    }
    /// read で取りだした分だけを埋める (0 で埋めておかなくてよい)
    fn exact_read(&self) -> bool {
        false
    }
    /// 読み手のいない EPIPE の SIGPIPE は自分で (MSG_NOSIGNAL を見る)
    fn own_sigpipe(&self) -> bool {
        false
    }
    /// read と readv (入れ物みなで 1 回受ける: dgram の 1 つのメッセージ)
    fn readv(&self, _f: &FileRef, _iov: &[(usize, usize)], _nowait: bool) -> Option<Result<i64, i64>> {
        None
    }
    /// write と writev (入れ物みなを 1 回で送る)
    fn writev(&self, _f: &FileRef, _iov: &[(usize, usize)], _nowait: bool, _nosignal: bool) -> Option<Result<i64, i64>> {
        None
    }
    /// fchmod (sockfs の inode)
    fn chmod(&self, _mode: u32) -> Option<Result<i64, i64>> {
        None
    }
    /// 持ち主 (F_SETFL の O_NOATIME)
    fn owner(&self) -> Option<u32> {
        None
    }
    /// (持ち主, グループ, 許可) (sockfs の inode: fchown、utimensat)
    fn owner_mode(&self) -> Option<(u32, u32, u32)> {
        None
    }
    fn set_owner_mode(&self, _uid: Option<u32>, _gid: Option<u32>, _mode: u32) {}
    fn touch(&self, _mtime: Option<u64>) {}
    /// O_NONBLOCK が変わった (FIONBIO、F_SETFL)
    fn set_nonblock(&self, _on: bool) {}
    /// splice で位置のないものとして読む (TCP / UDP: Linux の tcp_splice_read と copy_splice_read)
    fn splice_stream(&self) -> bool {
        false
    }
    /// splice でパイプへ (Linux の sock_splice_read)
    fn splice_to_pipe(&self, _f: &FileRef, _pos: i64, _p: &Rc<PipeCell>, _len: usize, _nonblock: bool) -> Option<Result<usize, i64>> {
        None
    }
}

/// 装置のノードのあるマウント (FileOps::mount: /dev の下の path)
pub fn dev_mount(path: &str) -> (u64, bool) {
    vfs::mount_of(path.trim_start_matches('/'))
}

/// 文字デバイスの番号から開くもの (Linux の register_chrdev): (major, 開くもの)。開くものは minor と open の flags から
/// Kind (その minor がなければ None: ENXIO)。装置が起こすとき (modprobe) に置く
type ChrOpen = fn(u32, u32) -> Option<Result<Kind, i64>>;
static CHRDEVS: crate::spinlock::SpinLock<alloc::vec::Vec<(u32, ChrOpen)>> = crate::spinlock::SpinLock::new(alloc::vec::Vec::new());

pub fn register_chrdev(major: u32, open: ChrOpen) {
    let mut l = CHRDEVS.lock();
    l.retain(|(m, _)| *m != major);
    l.push((major, open));
}

/// ブロックデバイスの番号から開くもの (Linux の register_blkdev: block.rs が置く)。なければ ENXIO
static BLKDEV: crate::spinlock::SpinLock<Option<fn(u32, u32) -> Option<Kind>>> = crate::spinlock::SpinLock::new(None);

pub fn register_blkdev(open: fn(u32, u32) -> Option<Kind>) {
    *BLKDEV.lock() = Some(open);
}

/// ブロックデバイスのノード (major, minor) を開く
pub fn open_blkdev(major: u32, minor: u32) -> Result<Kind, i64> {
    const ENXIO: i64 = 6;
    let open = (*BLKDEV.lock()).ok_or(-ENXIO)?;
    open(major, minor).ok_or(-ENXIO)
}

impl Kind {
    /// 位置のあるもの (ふつうのファイル、/dev/null と /dev/zero と乱数、ディスクと画面: pread、pwrite、lseek、sendfile の
    /// 位置。待つことがなく、頼まれた分を最後まで運ぶ)
    pub fn seekable(&self) -> bool {
        match self {
            Kind::Inode(..) | Kind::Null | Kind::Zero | Kind::Random => true,
            Kind::Ops(o) => o.seekable(),
            _ => false,
        }
    }

    /// splice の位置と sync_file_range のできるもの (ふつうのファイルとディスク)
    pub fn positional(&self) -> bool {
        match self {
            Kind::Inode(..) => true,
            Kind::Ops(o) => o.positional(),
            _ => false,
        }
    }

    /// Kind::Ops の中身が T なら、それを借りて (Rc を写さない: 大きなロックなしの道からも)
    // [lockless]
    pub fn ops_ref<T: FileOps>(&self) -> Option<&T> {
        match self {
            Kind::Ops(o) => (&**o as &dyn core::any::Any).downcast_ref::<T>(),
            _ => None,
        }
    }

    /// Kind::Ops の中身が T なら、その Rc (それぞれが fd から自分のものを取りだす: Linux の file->private_data)
    pub fn ops<T: FileOps>(&self) -> Option<Rc<T>> {
        match self {
            Kind::Ops(o) => (o.clone() as Rc<dyn core::any::Any>).downcast::<T>().ok(),
            _ => None,
        }
    }
}

/// 開いたものの種類の名前 (Kind::class の番号。/proc/bkl が fd のシステムコールを分ける)
pub const CLASSES: [&str; 10] = ["file", "proc", "pipe", "unix", "inet", "eventfd", "tty", "dev", "timerfd", "other"];

/// いまのプロセスの fd の開いたものの種類 (CLASSES の番号。大きなロックを持って。fd がなければ other)
pub fn class_of(fd: u64) -> usize {
    let other = CLASSES.len() - 1;
    let Some(files) = proc::current().files.as_ref() else { return other };
    files.get().get(fd).and_then(|f| f.try_borrow().ok().map(|o| o.kind.class())).unwrap_or(other)
}

impl Kind {
    /// CLASSES の番号
    pub fn class(&self) -> usize {
        match self {
            Kind::Inode(_, p) => {
                let p = p.trim_start_matches('/');
                let top = p.split('/').next().unwrap_or("");
                if top == "proc" || top == "sys" { 1 } else { 0 }
            }
            Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) => 2,
            Kind::Tty(_) | Kind::PtyMaster(_) => 6,
            Kind::Null | Kind::Zero | Kind::Random => 7,
            Kind::PidFd(_) | Kind::Landlock(_) | Kind::SeccompNotify(_) => 9,
            Kind::Ops(o) => o.class(),
        }
    }

    /// デバイスファイルを開いたときの中身
    pub fn of_dev(major: u32, minor: u32, flags: u32) -> Result<Kind, i64> {
        const ENXIO: i64 = 6;
        let tty = match (major, minor) {
            (1, 3) => return Ok(Kind::Null),
            (1, 5) => return Ok(Kind::Zero),
            (1, 8) | (1, 9) => return Ok(Kind::Random),
            (5, 0) => {
                let t = tty::controlling().ok_or(-ENXIO)?;
                tty::ref_slave(&t);
                t
            }
            (5, 1) => tty::console(),
            (5, 2) => return Ok(Kind::PtyMaster(tty::open_ptmx()?)),
            (136, n) => tty::open_slave(n as usize)?,
            // 装置 (画面、キーボードやマウス、音 ...: register_chrdev で置いたもの)
            _ => {
                let open = CHRDEVS.lock().iter().find(|(m, _)| *m == major).map(|&(_, f)| f);
                return open.and_then(|f| f(minor, flags)).unwrap_or(Err(-ENXIO));
            }
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
    /// 閉じたら棚 (Shelf) にしまってよい: ext4 のふつうのファイルを、読むだけの口で、ほかの印なしに開いたもの
    /// (sysfile::open_found がつける)
    pub reuse: bool,
    /// flock か OFD ロックを置いたことがある (閉じるときに外す): 棚にはしまわない
    pub locked: bool,
    /// LSM (landlock) が開いたときに覚えたもの: 許されなかった TRUNCATE と IOCTL_DEV (デバイスだけ)。あとの ftruncate と
    /// ioctl はこれで決める (security::file_truncate と file_ioctl。Linux の landlock_file の allowed_access。渡された fd
    /// でも、層のないプロセスでも同じ)。0 でなければ棚にしまわない
    pub lsm_deny: u32,
    /// ソケット: recvmmsg が 1 つ目より後でしくじったときのエラー (負の errno、0 はなし)。次の受けとり (recv、recvmsg、
    /// recvmmsg、read) か SO_ERROR で返して消す。aios がえらんだちがい (recvmmsg-error): Linux はソケットの sk_err に書く
    /// ので、次の send や ICMP のエラーや SO_ERROR が持っていき、受けとる側に返らないことがある (recvmmsg(2) の BUGS)
    pub recv_err: i64,
    /// 開いた (作った) プロセスの LSM のラベル (なければ None: Linux の file->f_security、AppArmor の aa_file_ctx の
    /// label)。ソケットをほかのラベルのプロセスが読み書きするときに見なおす (security::file_permission)
    pub label: Option<crate::security::Label>,
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

/// pidfd の印 (Linux の PIDFD_THREAD = O_EXCL): そのスレッドを指す pidfd (open のときだけの印なので、ほかのファイルには残らない)
pub const PIDFD_THREAD: u32 = 0o200;

pub fn new(kind: Kind, flags: u32) -> FileRef {
    if let Kind::Inode(ino, _) = &kind {
        ino.opened();
    }
    Rc::new(RefCell::new(OpenFile { kind, offset: 0, flags, dirents: None, reading: false, reuse: false, locked: false, lsm_deny: 0, recv_err: 0, label: crate::cred::current().label.clone() }))
}

/// 閉じたふつうのファイルの棚 (fd の表ごと)。aios のしかけ (Linux にはない): 読むだけで開いた ext4 のふつうのファイルを
/// 閉じるとき、ほかにだれも持っていず (dup も fork もしていない)、ロックも IN_OPEN / IN_CLOSE の見張りもなければ、
/// OpenFile を捨てずにここへしまう (大きなロックなしの close: fast_close)。同じパスを同じ口でまた開くとき、statc の表が、
/// そのパスはいまも同じ inode をさし、呼んだ人が読めると答えれば、棚から出していちばん小さい空いた fd に置く
/// (大きなロックなしの open: fast_open)。どちらも Rc の参照の数は変えない (動かすだけ)。棚のものを捨てるのは大きなロックの
/// 中だけ: ファイルを消して inode のリンクが 0 になったとき (extfs の unlink_node: 消したファイルの場所がすぐ返るように)、
/// umount、IN_OPEN / IN_CLOSE の見張りを置くとき、exec と exit (表といっしょに)。マウントが変わったあとのものは
/// (statc の版がちがう) 使わない
/// 2 つめは、大きなロックなしの open がとっておいた fd の番号 (+1。0 は空き。棚のロックの中で変える: Slot)
pub struct Shelf(crate::spinlock::SpinLock<Vec<Shelved>>, [core::sync::atomic::AtomicUsize; RESERVE_MAX]);

/// 1 つの表で同時にとっておける fd の数 (ほかのスレッドの大きなロックなしの open が開いている途中のもの。こえればふつうの道へ)
const RESERVE_MAX: usize = 4;

impl Default for Shelf {
    fn default() -> Self {
        Self(crate::spinlock::SpinLock::new(Vec::new()), [const { core::sync::atomic::AtomicUsize::new(0) }; RESERVE_MAX])
    }
}

impl Shelf {
    /// fd がほかのスレッドの大きなロックなしの open にとっておかれている (Linux の、alloc_fd したが fd_install していない
    /// もの)。ふつうの道 (proc::Files の add、put_at) は門の中で見る: add はとばし、dup2 などは EBUSY
    pub fn reserved(&self, fd: usize) -> bool {
        self.1.iter().any(|r| r.load(core::sync::atomic::Ordering::Relaxed) == fd + 1)
    }
}

/// 大きなロックなしの open がとっておいた fd (reserve。Linux の alloc_fd)。置くのは place (fd_install)。置かずに落とせば返す。
/// とっておくのと置くのは、それぞれ門の中で (ほかのスレッドと表を分けているとき)。あいだ (ext4 で引いて開く) は門の外:
/// ほかのスレッドの close や dup2 (ふつうの道) を待たせない。とっておいた fd には、ふつうの道は置かない
pub struct Slot {
    files: *const proc::Shared<proc::Files>,
    k: usize,
    pub fd: usize,
}

impl Drop for Slot {
    // [lockless]
    fn drop(&mut self) {
        let t = unsafe { (*self.files).get() };
        let _l = t.shelf.0.lock();
        t.shelf.1[self.k].store(0, core::sync::atomic::Ordering::Relaxed);
    }
}

/// いちばん小さい空いた fd (nofile まで。表を伸ばすのはふつうの道で) をとっておく
pub fn reserve(files: &proc::Shared<proc::Files>) -> Option<Slot> {
    use core::sync::atomic::Ordering::Relaxed;
    let t = files.get();
    let _g = if files.private() { None } else { Some(t.gate.read()?) };
    let _l = t.shelf.0.lock();
    let k = t.shelf.1.iter().position(|r| r.load(Relaxed) == 0)?;
    let fd = (0..t.fds.len().min(t.nofile)).find(|&i| t.fds[i].is_none() && !t.shelf.reserved(i))?;
    t.shelf.1[k].store(fd + 1, Relaxed);
    Some(Slot { files, k, fd })
}

/// とっておいた fd に置く (棚のロックの中で: 呼ぶ人が門と棚のロックを持っている)。fd を返す
fn place_locked(fds: &mut [Option<proc::Fd>], shelf: &Shelf, slot: Slot, file: FileRef, cloexec: bool) -> usize {
    let fd = slot.fd;
    fds[fd] = Some(proc::Fd { file, cloexec });
    shelf.1[slot.k].store(0, core::sync::atomic::Ordering::Relaxed);
    core::mem::forget(slot);
    fd
}

/// 門に入る (ほかのスレッドと表を分けているとき)。表を変えている人 (大きなロックの中の close、dup2 など。すぐ終わる) が
/// いれば、終わるのを待つ (とっておいた fd に置くのは、やめられない)
fn gate_wait(files: &proc::Shared<proc::Files>) -> Option<crate::proc::GateRead<'_>> {
    if files.private() {
        return None;
    }
    loop {
        if let Some(g) = files.get().gate.read() {
            return Some(g);
        }
        core::hint::spin_loop();
    }
}

/// 大きなロックなしの open (fastdir::fast_open) が新しく開いたファイルを、とっておいた fd に置く。fd を返す
pub fn place(slot: Slot, file: FileRef, cloexec: bool) -> usize {
    let files = unsafe { &*slot.files };
    let _g = gate_wait(files);
    let t = files.get();
    let _l = t.shelf.0.lock();
    place_locked(&mut t.fds, &t.shelf, slot, file, cloexec)
}

struct Shelved {
    file: FileRef,
    /// しまったときの statc の全体の版
    epoch: u64,
}

impl Shelf {
    /// 棚のロック (ほかのプロセスの表を読むとき: proc::Files::get_other)
    pub fn hold(&self) -> impl Sized + '_ {
        self.0.lock()
    }
}

/// fork や exec で表を写すとき、棚は空から (しまったものは、その表のもの)
impl Clone for Shelf {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// 表が落ちる (exit、exec で前の表を手放す): しまってあったものもいっしょに落ちる
impl Drop for Shelf {
    fn drop(&mut self) {
        let n = self.0.lock().len();
        SHELVED.fetch_sub(n, core::sync::atomic::Ordering::Relaxed);
    }
}

/// 1 つの表の棚にしまう数の上限
const SHELF_MAX: usize = 8;

/// 棚にあるものの数 (みなの表の。0 なら捨てるときに表をなめない)
static SHELVED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// reuse の印のついた OpenFile の数 (表にあるものも棚にあるものも。0 なら、ファイルを消すときに表をなめない)
pub static REUSABLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// REUSABLE を、inode の id のハッシュで 256 の箱に分けたもの。ファイルを消すとき (shelf_forget)、その inode の箱が 0 なら
/// 表も棚もなめない (ほかのファイルが棚にあっても: 消すたびにみなのプロセスの fd をなめていた)
static REUSE_IDS: [core::sync::atomic::AtomicUsize; 256] = [const { core::sync::atomic::AtomicUsize::new(0) }; 256];

fn reuse_box(of: &OpenFile) -> Option<&'static core::sync::atomic::AtomicUsize> {
    match &of.kind {
        Kind::Inode(ino, _) => Some(id_box(ino.id())),
        _ => None,
    }
}

fn id_box(id: (usize, u64)) -> &'static core::sync::atomic::AtomicUsize {
    &REUSE_IDS[((id.1 ^ (id.0 as u64).rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize]
}

/// 大きなロックの中で: 閉じたら棚にしまってよい印をつける (sysfile の open_found)
pub fn reuse_on(f: &FileRef) {
    use core::sync::atomic::Ordering::Relaxed;
    let mut b = f.borrow_mut();
    if !b.reuse {
        b.reuse = true;
        REUSABLE.fetch_add(1, Relaxed);
        if let Some(c) = reuse_box(&b) {
            c.fetch_add(1, Relaxed);
        }
    }
}

/// reuse の印を落とす (数も)
fn reuse_off(of: &mut OpenFile) {
    use core::sync::atomic::Ordering::Relaxed;
    if of.reuse {
        of.reuse = false;
        REUSABLE.fetch_sub(1, Relaxed);
        if let Some(c) = reuse_box(of) {
            c.fetch_sub(1, Relaxed);
        }
    }
}

/// IN_OPEN か IN_CLOSE を待つ見張りの数 (大きなロックなしの open / close はできごとを出さないので、0 でなければふつうの道で。
/// inotify.rs の count)
pub static OPEN_WATCHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 大きなロックなしの書きたし (fast_write_file と ExtFs::fast_append) の数: [0] 書いた、ふつうの道へ: [1] ファイルの形
/// (大きさの終わりでない、ふつうのファイルでない、フラグ、inode がキャッシュにない)、[2] 書き出すころ、[3] 最後の extent の
/// つづきにならない・ブロックがキャッシュにない (append_fits)、[4] 見張りか copy-append のあいだ、ほか。/proc/bkl に出す
pub static FAST_WRITE: [core::sync::atomic::AtomicU64; 5] = [const { core::sync::atomic::AtomicU64::new(0) }; 5];

/// 大きなロックの中で O_APPEND の終わりを読んでから書くもの (splice.rs の copy-append) の数。0 でなければ大きなロックなしの
/// 書きたし (Inode::fast_append) はふつうの道へ (読んだ終わりのあとに書きたされると、その上に書いてしまう)
pub static SLOW_APPENDS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// SLOW_APPENDS を持つしるし (落とすと引く)。足してから ext4 のロックをひとりで取って放し、いま書きたしているものが
/// 出るのを待つ (書きたしは ext4 のロックをいっしょに持ってから数を見る)。O_APPEND の終わりを読む前に
pub struct SlowAppend(());

impl SlowAppend {
    pub fn new() -> SlowAppend {
        SLOW_APPENDS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        drop(crate::smp::fs_lock());
        SlowAppend(())
    }
}

impl Drop for SlowAppend {
    fn drop(&mut self) {
        SLOW_APPENDS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

/// IN_MODIFY を待つ見張りの数 (大きなロックなしの書きたし fast_write_file は、0 でなければふつうの道へ: できごとを出す。
/// inotify.rs の count)
pub static MODIFY_WATCHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 大きなロックなしの open / close の数: [0] 棚から開いた、[1] ない (ENOENT) と答えた、[2] 棚にしまった、
/// ふつうの道へ: [3] statc の表にない・古い、[4] 棚になく、新しくも開けない、[5] 開けない形 (フラグ、fd の表、見張り)、
/// [6] しまえない (分けている、ロック、棚がいっぱい)。[7] 棚になく、ext4 の名前と inode から新しく開いた。/proc/bkl に出す
pub static FAST_OPEN: [core::sync::atomic::AtomicU64; 8] = [const { core::sync::atomic::AtomicU64::new(0) }; 8];

// [lockless]
fn note(i: usize) {
    FAST_OPEN[i].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// 大きなロックなしの openat (syscall::fast): O_RDONLY か O_WRONLY か O_RDWR (と O_CLOEXEC / O_LARGEFILE / O_NOCTTY)
/// だけで (O_NOFOLLOW は「ない」の答えだけ: git は .gitignore をディレクトリごとに O_NOFOLLOW で開く)、絶対のパスか、いまのディレクトリか開いているディレクトリ (statc の dir_base) からのもの。statc の表が
/// 「ない」と答えれば ENOENT (ふつうの道と同じわけを /proc/ai/why に)、ふつうのファイルで呼んだ人が読めて (書けて)、
/// 読むだけの口で、同じパスと inode と口のものが棚にあれば、それを置く。棚になく (書く口はいつも)、リンクをたどらずに
/// 着くものなら、ext4 の名前と inode から新しく開く (fastdir::fast_open)。ほかは None (ふつうの道へ)
pub fn fast_open(me: &proc::Proc, dirfd: i64, pathp: usize, flags: u64) -> Option<i64> {
    use core::sync::atomic::Ordering::Relaxed;
    const O_LARGEFILE: u64 = 0o400000;
    const O_CLOEXEC: u64 = 0o2000000;
    const OPENAT: u64 = 56;
    const O_DIRECTORY: u64 = 0o40000;
    const O_NOFOLLOW: u64 = 0o100000;
    let acc = flags & O_ACCMODE as u64;
    let nofollow = flags & O_NOFOLLOW != 0;
    // O_DIRECTORY (opendir) は読むだけの口で、ディレクトリなら (ほかはふつうの道が EISDIR や ENOTDIR を)
    let dir = flags & O_DIRECTORY != 0;
    if flags & !(O_ACCMODE as u64 | O_LARGEFILE | O_CLOEXEC | O_NOCTTY as u64 | O_DIRECTORY | O_NOFOLLOW) != 0 || acc == O_ACCMODE as u64 || (dir && acc != O_RDONLY as u64) || me.has_cred_work(proc::CRED_SANDBOX) || OPEN_WATCHED.load(Relaxed) != 0 {
        note(5);
        return None;
    }
    // fd の表はほかのスレッドと分けていてもよい: 表と cwd を見るところ (statc と、fd をとっておく・置くところ) は門の中で
    let files = me.files.as_ref()?;
    let mut norm = [0u8; crate::statc::PATH];
    let Some((ans, len)) = crate::statc::open_lookup(me, dirfd, pathp, !nofollow, &mut norm) else {
        note(3);
        return None;
    };
    let (st, plain, fresh, base) = match ans {
        crate::statc::Open::Missing(looked) => {
            note(1);
            return Some(crate::statc::missing(me, OPENAT, &norm[..len], looked));
        }
        // O_NOFOLLOW であるもの (リンクなら ELOOP、ほかは開いた口の flags に残す) は、ふつうの道で
        crate::statc::Open::Found(..) if nofollow => {
            note(5);
            return None;
        }
        crate::statc::Open::Found(st, plain, fresh, base) => (st, plain, fresh, base),
    };
    // 読むだけの口で表の stat が古い (inode を書いたあと) なら、stat と同じく、いまの inode のものに書きなおす
    // (statc::open_now)。書きなおさないと、新しく開く道がいつもうまくいって、だれも覚えなおさず、棚を見ない
    // (同じファイルを開きなおすたびに新しく開き、閉じたものは棚をふさいでいた)
    let (st, fresh) = if fresh || acc != O_RDONLY as u64 {
        (st, fresh)
    } else {
        match crate::statc::open_now(me, &norm[..len], st, base) {
            Some(now) => (now, true),
            None => (st, false),
        }
    };
    // ここからふつうの道へ渡すものは、表の答えをいま確かめたもの (ふつうの道は覚えなおさない)
    let pass = |i: usize| {
        note(i);
        crate::statc::seen_set(me.pid, pathp);
        None
    };
    // 読めなければ (書けなければ)、ふつうの道が EACCES とそのわけを (stat が古ければ、新しく開く道が ext4 の中でも)
    let m = vfs::Meta { mode: st.mode, uid: st.uid, gid: st.gid, ..vfs::Meta::default() };
    let perm = match acc as u32 {
        O_RDONLY => crate::cred::R,
        O_WRONLY => crate::cred::W,
        _ => crate::cred::R | crate::cred::W,
    };
    if st.mode & S_IFMT != if dir { vfs::S_IFDIR } else { vfs::S_IFREG } || !me.cred.may(&m, perm, false) {
        return pass(5);
    }
    // いちばん小さい空いた fd をとっておく (表を伸ばすのはふつうの道で)
    let Some(slot) = reserve(files) else {
        return pass(5);
    };
    let key = &norm[..len];
    // 書く口は棚を見ない (棚は読むだけの口のもの)。stat が古いとき (inode を書いたあと) も: 棚から出すときの許しは、この
    // stat で確かめたもの。どちらもふつうの道は覚えなおしてよいので、seen_set はしない。ディレクトリも (棚はふつうのファイルのもの)
    if acc != O_RDONLY as u64 || !fresh || dir {
        if plain && let Some(fd) = crate::fastdir::fast_open(me, slot, key, st.ino, flags, base) {
            note(7);
            return Some(fd);
        }
        note(4);
        return None;
    }
    let want = (flags & O_LARGEFILE) as u32;
    let epoch = crate::statc::epoch();
    // 表に置くのも棚のロックの中で (大きなロックの中の shelf_forget が、表と棚をいっしょに見る)。門は棚のロックの前に
    let gate = gate_wait(files);
    let t = files.get();
    let mut shelf = t.shelf.0.lock();
    let i = shelf.iter().position(|s| {
        let of = unsafe { &*s.file.as_ptr() };
        s.epoch == epoch && of.flags == want && matches!(&of.kind, Kind::Inode(ino, path) if ino.id().1 == st.ino && path.as_bytes() == key)
    });
    let Some(i) = i else {
        drop(shelf);
        drop(gate);
        // 棚になければ、ext4 の名前と inode から新しく (Linux の open と同じく。パスは表の鍵: リンクをたどらずに着いたもの)
        if plain && let Some(fd) = crate::fastdir::fast_open(me, slot, key, st.ino, flags, base) {
            note(7);
            return Some(fd);
        }
        return pass(4);
    };
    let file = shelf.swap_remove(i).file;
    // 棚のものは自分だけのもの (しまうときに確かめ、棚にあるあいだはだれも見つけられない): 借りずに開きなおしの形に
    let of = unsafe { &mut *file.as_ptr() };
    of.offset = 0;
    of.dirents = None;
    of.reading = false;
    let fd = place_locked(&mut t.fds, &t.shelf, slot, file, flags & O_CLOEXEC != 0);
    drop(shelf);
    drop(gate);
    SHELVED.fetch_sub(1, Relaxed);
    note(0);
    Some(fd as i64)
}

/// 大きなロックなしの作る道 (fastdir.rs) が作ったファイルを、空いている fd に置く (表ごとのスピンロックの中で: 大きなロックの
/// 中の shelf_forget が、表と棚をいっしょに見る)
pub fn fast_install(t: &mut proc::Files, fd: usize, file: FileRef, cloexec: bool) {
    let _shelf = t.shelf.0.lock();
    t.fds[fd] = Some(proc::Fd { file, cloexec });
}

/// 大きなロックなしの close (syscall::fast): 閉じる fd が、自分だけが持っている棚にしまってよいファイル (OpenFile::reuse) で、
/// ロック (flock / OFD、この表の POSIX のロック) も IN_OPEN / IN_CLOSE の見張りもなければ、表から外して棚に。
/// OpenFile は落とさないので、落とすときの後片づけ (ロックを外す、IN_CLOSE) はいらない。ほかは None (ふつうの道へ)
// [lockless]
pub fn fast_close(me: &proc::Proc, fd: u64) -> Option<i64> {
    use core::sync::atomic::Ordering::Relaxed;
    let files = me.files.as_ref()?;
    if OPEN_WATCHED.load(Relaxed) != 0 || crate::debug::FAIL_ON.load(Relaxed) {
        return None;
    }
    if !files.private() {
        return fast_close_shared(files, fd);
    }
    let t = files.get();
    let slot = t.fds.get_mut(fd as usize)?;
    let f = &slot.as_ref()?.file;
    let of = unsafe { &*f.as_ptr() };
    if !of.reuse || of.locked || t.posix_locked || Rc::strong_count(f) != 1 {
        note(6);
        return None;
    }
    let mut shelf = t.shelf.0.lock();
    // reuse と、自分だけが持っていることは棚のロックの中でもう一度 (ファイルを消すとき、shelf_forget がこのロックの中で
    // 落とす。ほかのプロセスの表を読む procfs や pidfd_getfd は、このロックの中で写す: Files::get_other)
    if shelf.len() >= SHELF_MAX || !unsafe { (*slot.as_ref()?.file.as_ptr()).reuse } || Rc::strong_count(&slot.as_ref()?.file) != 1 {
        drop(shelf);
        note(6);
        return None;
    }
    let file = slot.take()?.file;
    shelf.push(Shelved { file, epoch: crate::statc::epoch() });
    drop(shelf);
    SHELVED.fetch_add(1, Relaxed);
    note(2);
    Some(0)
}

/// スレッドで分けた表の大きなロックなしの close の数: [0] 閉じた、ふつうの道へ: [1] 同じ表のスレッドが大きなロックを
/// 持っている、[2] ほかの人が表を変えている途中、[3] 閉じられない形 (読むだけの口のふつうのファイルでない、ロック、
/// 開いていない fd)、[4] 落とすのを待つものがたまった。/proc/bkl に出す
pub static FAST_CLOSE_SHARED: [core::sync::atomic::AtomicU64; 5] = [const { core::sync::atomic::AtomicU64::new(0) }; 5];

/// スレッドで分けた表から大きなロックなしで外した OpenFile (Linux の fput の task_work と delayed_fput)。Rc の数は
/// 大きなロックの中でしか変えないので、ここに移して (数は変えずに)、次に大きなロックを取ったシステムコールのはじめに
/// 落とす (drop_closed)。外したときから、だれも表から引けないので、落とすのが遅れて見えるのは、ファイルの後片づけ
/// (消したファイルの場所が返る) が少しあとになることだけ (IN_CLOSE の見張りがあれば、はじめから閉じない)
static CLOSED: crate::spinlock::SpinLock<Closed> = crate::spinlock::SpinLock::new(Closed(Vec::new()));

/// CLOSED の中身。ほかの CPU へ渡る Rc だが、Rc の数を変える (落とす) のは大きなロックの中の drop_closed だけで、
/// 入れる側 (fast_close_shared) は表から移すだけ (数を変えない)
struct Closed(Vec<FileRef>);
// SAFETY: 上のとおり、Rc の数は大きなロックの中でしか変えない
unsafe impl Send for Closed {}
static CLOSED_N: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// 落とすのを待つものの上限 (大きなロックを取らずに close しつづけても、これ以上はためない)
const CLOSED_MAX: usize = 256;

/// スレッドで分けた表の close (fast_close から): 読むだけの口の ext4 のふつうのファイル (reuse の印) を、表から外して
/// CLOSED に移す。表を読む人とは:
/// - 大きなロックの中で読む人 (同じ表のスレッド。file_of で OpenFile を写す: 門を通らない) とは、表の箱
///   (smp::FILES_WRITING) に印を立ててから、大きなロックの持ち主の表 (smp::bkl_files) が自分の表なら入らない。大きなロックの
///   側は、持ち主の表を置いてから箱の印が消えるのを待つ (vm::PageTable::write_enter と同じ形)
/// - 大きなロックなしで読む人 (門の users) とは、門を書く側で通って (proc::Gate::try_write: 変える人は 1 人ずつ。大きな
///   ロックの中の set や、ほかのプロセスの表に置く seccomp の addfd とも)、見ている人がいなくなるのを待つ
/// - ほかのプロセスの表を読む人 (procfs、pidfd_getfd: Files::get_other) とは、棚のロックの中で外す
// [lockless]
fn fast_close_shared(files: &proc::Shared<proc::Files>, fd: u64) -> Option<i64> {
    use core::sync::atomic::Ordering::{Relaxed, SeqCst};
    let note = |i: usize| {
        FAST_CLOSE_SHARED[i].fetch_add(1, Relaxed);
        None
    };
    if CLOSED_N.load(Relaxed) >= CLOSED_MAX {
        return note(4);
    }
    let id = files.id();
    let b = crate::smp::files_box(id);
    b.fetch_add(1, SeqCst);
    core::sync::atomic::fence(SeqCst);
    // 同じ表のスレッドが大きなロックを持っていれば、印を下ろして、放すのを少し待って (vm::spin_until) 1 度だけやりなおす
    // (rg -j4 は、ほかのスレッドがはじめて開くファイルで大きなロックを持つあいだに閉じることが多い。すぐふつうの道へ
    // 行くと、その大きなロックに並ぶ)
    if crate::smp::bkl_files() == id {
        b.fetch_sub(1, SeqCst);
        if !crate::vm::spin_until(|| crate::smp::bkl_files() != id) {
            return note(1);
        }
        b.fetch_add(1, SeqCst);
        core::sync::atomic::fence(SeqCst);
    }
    let r = if crate::smp::bkl_files() == id {
        note(1)
    } else {
        let t = files.get();
        match t.gate.try_write() {
            None => note(2),
            Some(w) => {
                let taken = {
                    let _s = t.shelf.0.lock();
                    let slot = t.fds.get_mut(fd as usize);
                    // 借りずに見る (ほかのスレッドが大きなロックなしで読んでいたかもしれないが、もう門の外)
                    let ok = slot.as_ref().and_then(|s| s.as_ref()).is_some_and(|s| {
                        let of = unsafe { &*s.file.as_ptr() };
                        of.reuse && !of.locked && !t.posix_locked
                    });
                    if ok { slot.and_then(|s| s.take()) } else { None }
                };
                drop(w);
                match taken {
                    Some(f) => {
                        CLOSED.lock().0.push(f.file);
                        CLOSED_N.fetch_add(1, Relaxed);
                        FAST_CLOSE_SHARED[0].fetch_add(1, Relaxed);
                        Some(0)
                    }
                    None => note(3),
                }
            }
        }
    };
    b.fetch_sub(1, SeqCst);
    r
}

/// 大きなロックの中で (システムコールのはじめ): fast_close_shared が外した OpenFile を落とす (最後の参照なら、そこで
/// 閉じたあとの片づけ)。落とすのは CLOSED のロックの外で
pub fn drop_closed() {
    use core::sync::atomic::Ordering::Relaxed;
    if CLOSED_N.load(Relaxed) == 0 {
        return;
    }
    let v = core::mem::take(&mut CLOSED.lock().0);
    CLOSED_N.fetch_sub(v.len(), Relaxed);
    drop(v);
}

/// 大きなロックの中で: 棚から、which が真のものを捨てる (みなの表の)。捨てた OpenFile はそこで落ちる。open_too なら、
/// 表で開いているもののうち which が真のものも、もう棚にしまわない (reuse を落とす。棚のロックの中で: fast_close は
/// しまう前にそのロックの中で reuse を見なおす)
fn shelf_drop(open_too: bool, mut which: impl FnMut(&OpenFile) -> bool) {
    use core::sync::atomic::Ordering::Relaxed;
    if SHELVED.load(Relaxed) == 0 && (!open_too || REUSABLE.load(Relaxed) == 0) {
        return;
    }
    let mut out: Vec<FileRef> = Vec::new();
    proc::each_files(|t| {
        let mut shelf = t.shelf.0.lock();
        if open_too {
            for fd in t.fds.iter().flatten() {
                // 借りずに (持ち主が大きなロックなしで見ているかもしれない。こちらは印を落とすだけ)
                let of = unsafe { &mut *fd.file.as_ptr() };
                if of.reuse && which(of) {
                    reuse_off(of);
                }
            }
        }
        let mut i = 0;
        while i < shelf.len() {
            if which(&shelf[i].file.borrow()) {
                out.push(shelf.swap_remove(i).file);
            } else {
                i += 1;
            }
        }
    });
    SHELVED.fetch_sub(out.len(), Relaxed);
    // 落とすのは棚のロックの外で (OpenFile と inode の後片づけは大きなロックの中の仕事)
    drop(out);
}

/// 大きなロックの中で: f を閉じても棚にはしまわない (epoll に入れたものなど、落ちることを見ている人がいる)
pub fn no_reuse(f: &FileRef) {
    reuse_off(&mut f.borrow_mut());
}

/// inode id のリンクが 0 になる (消した、上書きした): 棚のものを捨てる (ほかに開いている人がいなければ、そこで片づく)。
/// いま開いているもの (消したあとも読める) も、閉じたら棚にはしまわない (しまうと、消したファイルの場所が返らない)
pub fn shelf_forget(id: (usize, u64)) {
    if id_box(id).load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return;
    }
    shelf_drop(true, |of| matches!(&of.kind, Kind::Inode(ino, _) if ino.id() == id));
}

/// 棚をみな空に (umount、IN_OPEN / IN_CLOSE の見張りを置くとき)
pub fn shelf_flush_all() {
    shelf_drop(false, |_| true);
}

/// 大きなロックの中の close から: 自分の棚の、マウントが変わる前のもの (もう使えない) を捨てる。いっぱいなら、
/// いちばん古いものも (名前を変えたものなど、もう開かれないものが棚をふさがないように)
pub fn shelf_prune(t: &mut proc::Files) {
    use core::sync::atomic::Ordering::Relaxed;
    let epoch = crate::statc::epoch();
    // 棚の Vec はその場で (前は閉じるたびに Vec を 3 つ作りなおし、棚の場所も手放していた)。落とすのは棚のロックの外で
    let (oldest, stale) = {
        let mut shelf = t.shelf.0.lock();
        let fresh = shelf.iter().all(|s| s.epoch == epoch);
        if shelf.len() < SHELF_MAX && fresh {
            return;
        }
        let stale: Vec<FileRef> = if fresh { Vec::new() } else { shelf.extract_if(.., |s| s.epoch != epoch).map(|s| s.file).collect() };
        let oldest = (shelf.len() >= SHELF_MAX).then(|| shelf.remove(0).file);
        (oldest, stale)
    };
    SHELVED.fetch_sub(stale.len() + oldest.is_some() as usize, Relaxed);
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
        let mut b = f.borrow_mut();
        if !b.readable() {
            return Err(-EBADF);
        }
        // recvmmsg がとっておいたエラー (recv_err)
        if b.recv_err != 0 {
            return Err(core::mem::take(&mut b.recv_err));
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
        crate::vfs::notify::file_event(path, ino, crate::vfs::notify::IN_ACCESS);
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
        Kind::Ops(o) => o.read(dst, nonblock),
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
        Kind::Ops(o) => o.write(src, nonblock),
        _ => Err(-EBADF),
    }
}

/// Linux (asm-generic) の struct stat
#[derive(Clone, Copy)]
pub struct Stat {
    /// st_dev (stat の形: vfs::encode_dev)。ファイルシステムごと (vfs::fs_dev)
    pub dev: u64,
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
        Stat::of_meta(&i.meta(), i.id().0)
    }

    /// fs はファイルシステムの番号 (InodeRef::id の .0。st_dev になる)
    // [lockless]
    pub fn of_meta(m: &vfs::Meta, fs: usize) -> Stat {
        Stat {
            dev: vfs::st_dev(fs),
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

    /// ファイルシステムのないもの (anon_inode の dev。ソケットとパイプは呼ぶほうが dev を)
    pub fn dev(mode: u32, rdev: u64) -> Stat {
        Stat { dev: vfs::encode_dev(vfs::ANON_DEV), ino: 0, mode, nlink: 1, uid: 0, gid: 0, rdev, size: 0, blocks: 0, mtime: 0, ctime: 0 }
    }

    /// st_dev をカーネルの形 (major << 20 | minor) に (fdinfo の sdev)
    pub fn kdev(&self) -> u32 {
        let (ma, mi) = vfs::decode_dev(self.dev);
        ma << 20 | mi
    }

    // [lockless]
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&self.dev.to_le_bytes());
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

    /// struct statx (STATX_BASIC_STATS と、マウントの番号)
    /// statx の答え。mask は頼まれたもの。mg (ext4 と tmpfs: Linux の multigrain の時刻) なら、mtime も ctime も
    /// 頼まれていなければ、その 2 つは出さない (Linux の fill_mg_cmtime)。mnt はそのものがあるマウント (番号、その根か:
    /// vfs::mount_of)。stx_mnt_id はいつも (STATX_MNT_ID_UNIQUE を頼まれればその番号で)、根なら STATX_ATTR_MOUNT_ROOT
    /// (Linux の vfs_statx_path)
    // [lockless]
    pub fn to_statx(&self, mask: u32, mg: bool, mnt: (u64, bool)) -> [u8; 256] {
        const MTIME: u32 = 0x40;
        const CTIME: u32 = 0x80;
        const MNT_ID: u32 = 0x1000;
        const MNT_ID_UNIQUE: u32 = 0x4000;
        const ATTR_MOUNT_ROOT: u64 = 0x2000;
        let cm = !mg || mask & (MTIME | CTIME) != 0;
        let mut b = [0u8; 256];
        // stx_mask = STATX_BASIC_STATS (と、なければ mtime / ctime をのぞく) とマウントの番号
        let unique = mask & MNT_ID_UNIQUE != 0;
        let got = (if cm { 0x7ffu32 } else { 0x7ff & !(MTIME | CTIME) }) | if unique { MNT_ID_UNIQUE } else { MNT_ID };
        b[0..4].copy_from_slice(&got.to_le_bytes());
        b[8..16].copy_from_slice(&(if mnt.1 { ATTR_MOUNT_ROOT } else { 0 }).to_le_bytes());
        b[56..64].copy_from_slice(&ATTR_MOUNT_ROOT.to_le_bytes());
        b[144..152].copy_from_slice(&(if unique { vfs::unique_mnt_id(mnt.0) } else { mnt.0 }).to_le_bytes());
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
        let (dma, dmi) = vfs::decode_dev(self.dev);
        b[136..140].copy_from_slice(&dma.to_le_bytes());
        b[140..144].copy_from_slice(&dmi.to_le_bytes());
        b
    }
}

impl OpenFile {
    /// 読める口か (O_RDONLY か O_RDWR。Linux の FMODE_READ: O_PATH と、O_ACCMODE (3) で開いたもの (ioctl だけ) は読めない)
    // [lockless]
    pub fn readable(&self) -> bool {
        self.flags & O_PATH == 0 && matches!(self.flags & O_ACCMODE, O_RDONLY | O_RDWR)
    }

    /// 書ける口か (O_WRONLY か O_RDWR)
    // [lockless]
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
                    crate::vfs::notify::file_event(path, ino, crate::vfs::notify::IN_ACCESS);
                }
                Ok(n)
            }
            Kind::Ops(o) if o.seekable() => {
                let n = o.read_at(self.offset, dst)?;
                self.offset += n;
                Ok(n)
            }
            Kind::PipeWrite(_) => Err(-EBADF),
            // landlock の決まりの束: 読み書きできる口だが、読めない (Linux の fop_dummy_read)
            Kind::Landlock(_) => Err(crate::why::inval()),
            k => read_stream(k, dst, self.flags & O_NONBLOCK != 0),
        }
    }

    /// 待つかもしれないもの (端末、パイプ、ソケット) なら、その中身の写しと O_NONBLOCK
    fn stream(&self) -> Option<(Kind, bool)> {
        let k = match &self.kind {
            Kind::Tty(_) | Kind::PtyMaster(_) | Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) => self.kind.clone(),
            Kind::Ops(o) if o.stream() => self.kind.clone(),
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
                let append = append.unwrap_or(self.flags & O_APPEND != 0);
                if append {
                    self.offset = ino.meta().size as usize;
                }
                vfs::write_sealed(ino, self.offset, src.len())?;
                vfs::drop_privs(ino)?;
                // O_APPEND の書きはじめはファイルシステムが書くときに決める (大きなロックなしの書きたしが、そのあいだに
                // 終わりを伸ばしているかもしれない)
                let n = if append {
                    let (n, at) = ino.write_append(src)?;
                    self.offset = at;
                    n
                } else {
                    ino.write_at(self.offset, src)?
                };
                self.offset += n;
                crate::vfs::notify::file_event(path, ino, crate::vfs::notify::IN_MODIFY);
                Ok(n)
            }
            Kind::Ops(o) if o.seekable() => {
                let n = o.write_at(self.offset, src)?;
                self.offset += n;
                Ok(n)
            }
            Kind::PipeRead(_) => Err(-EBADF),
            Kind::Landlock(_) => Err(crate::why::inval()),
            k => write_stream(k, src, self.flags & O_NONBLOCK != 0),
        }
    }

    pub fn stat(&self) -> Stat {
        // デバイスは /dev のノードと同じ ino を見せる (musl の ttyname はそれを比べる)
        if matches!(self.kind, Kind::Tty(_) | Kind::PtyMaster(_) | Kind::Null | Kind::Zero | Kind::Random) || matches!(&self.kind, Kind::Ops(o) if o.opened()) {
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
            // ino はパイプごとにちがう番号 (/proc/PID/fd の pipe:[N] と同じ)。diff <(a) <(b) などは
            // dev と ino が同じなら同じファイルとみなす
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                use core::sync::atomic::Ordering::Relaxed;
                Stat { dev: vfs::encode_dev(vfs::PIPE_DEV), ino: Rc::as_ptr(p) as usize as u64 & 0xffffff, uid: p.uid.load(Relaxed), gid: p.gid.load(Relaxed), mtime: p.mtime.load(Relaxed), ctime: p.ctime.load(Relaxed), ..Stat::dev(S_IFIFO | p.perm.load(Relaxed), 0) }
            }
            // pidfd はプロセス (スレッド) ごとの inode (Linux の pidfs: 同じものを指す pidfd は同じ ino。aios の pid は
            // 使いまわさないので番号そのもの)
            Kind::PidFd(pid) => Stat { dev: vfs::encode_dev(vfs::PIDFS_DEV), ino: *pid as u64, ..Stat::dev(0o600, 0) },
            // 名前のない inode (anon_inode)
            Kind::Landlock(_) | Kind::SeccompNotify(_) => Stat::dev(0o600, 0),
            Kind::Ops(o) => o.stat(),
        }
    }

    /// Linux の f_flags (F_GETFL と /proc/PID/fdinfo の flags)。open で開いたもの (ファイル、デバイス) には、64 ビットの
    /// カーネルがいつも足す O_LARGEFILE を見せる (aios はしまわない: 棚の速い open が口をくらべるので)。O_PATH は O_PATH、
    /// O_DIRECTORY、O_NOFOLLOW だけ (Linux の O_PATH_FLAGS)
    pub fn linux_flags(&self) -> u32 {
        const O_LARGEFILE: u32 = 0o400000;
        const O_DIRECTORY: u32 = 0o40000;
        const O_NOFOLLOW: u32 = 0o100000;
        if self.flags & O_PATH != 0 {
            return self.flags & (O_PATH | O_DIRECTORY | O_NOFOLLOW);
        }
        let opened = matches!(self.kind, Kind::Inode(..) | Kind::Tty(_) | Kind::PtyMaster(_) | Kind::Null | Kind::Zero | Kind::Random) || matches!(&self.kind, Kind::Ops(o) if o.opened());
        self.flags | if opened { O_LARGEFILE } else { 0 }
    }

    /// このファイルのあるマウント (番号、その根か: statx と fdinfo の mnt_id)。開いたときのパスの、いまのプロセスの
    /// マウントの表で (vfs::mount_of)。名前のないもの (パイプ、ソケット、anon_inode、pidfd) は Linux の中のマウントの番号
    pub fn mount(&self) -> (u64, bool) {
        match &self.kind {
            Kind::Inode(_, path) => vfs::mount_of(path.trim_start_matches('/')),
            Kind::PidFd(_) => (vfs::PIDFS_MNT, false),
            Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) => (vfs::PIPE_MNT, false),
            Kind::Landlock(_) | Kind::SeccompNotify(_) => (vfs::ANON_MNT, false),
            Kind::Ops(o) => o.mount(),
            // デバイスは /dev のノードのあるところ
            _ => vfs::mount_of(self.describe().trim_start_matches('/')),
        }
    }

    /// /proc/PID/fdinfo/N の中身 (Linux の seq_show と、ものごとの show_fdinfo)。cloexec はその fd の FD_CLOEXEC
    pub fn fdinfo(&self, cloexec: bool) -> String {
        const O_CLOEXEC: u32 = 0o2000000;
        let flags = self.linux_flags() | if cloexec { O_CLOEXEC } else { 0 };
        let mut s = alloc::format!("pos:\t{}\nflags:\t0{:o}\nmnt_id:\t{}\nino:\t{}\n", self.offset, flags, self.mount().0, self.stat().ino);
        match &self.kind {
            Kind::PidFd(pid) => s.push_str(&crate::pidfd::fdinfo(*pid, self.flags & PIDFD_THREAD != 0)),
            Kind::Ops(o) => s.push_str(&o.fdinfo()),
            _ => {}
        }
        s
    }

    /// 状態が変わるたびに増える数 (epoll の EPOLLET 用)。数えていないものは 0
    pub fn event_gen(&self) -> u64 {
        match &self.kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => p.borrow().generation,
            Kind::Ops(o) => o.event_gen(),
            Kind::Tty(t) | Kind::PtyMaster(t) => t.borrow().generation.get(),
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
            Kind::Inode(_, path) => alloc::format!("/{}", path),
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::format!("pipe:[{}]", Rc::as_ptr(p) as usize & 0xffffff),
            Kind::Ops(o) => o.describe(),
            Kind::PidFd(_) => "anon_inode:[pidfd]".into(),
            Kind::SeccompNotify(_) => "anon_inode:seccomp notify".into(),
            Kind::Landlock(_) => "anon_inode:[landlock-ruleset]".into(),
        }
    }

    /// 読まれずに残っているもの (/proc/ai/fd): パイプと socketpair はバイト、TCP / UDP は届いている分、
    /// eventfd は数、inotify はバイト。ないもの (ふつうのファイルなど) は None
    pub fn pending(&self) -> Option<u64> {
        Some(match &self.kind {
            Kind::PipeRead(p) | Kind::PipeRw(p) => p.borrow().len() as u64,
            Kind::Ops(o) => return o.pending(),
            _ => return None,
        })
    }

    /// recvmmsg がとっておいたエラーがあれば POLLERR (Linux の sock_poll の sk_err)
    fn recv_err_mask(&self) -> u32 {
        if self.recv_err != 0 { POLLERR } else { 0 }
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
            // ソケット (AF_UNIX、TCP / UDP) は poll のビットから
            Kind::Ops(o) if o.mask_readiness() => {
                let m = o.poll_mask().unwrap_or(0) | self.recv_err_mask();
                (m & (POLLIN | POLLHUP | POLLERR) != 0, m & POLLOUT != 0, m & (POLLHUP | POLLERR) != 0)
            }
            // 終わったら読める、片づけられたら POLLHUP も (PIDFD_THREAD のものは O_EXCL の印: そのスレッドを見る)
            Kind::PidFd(pid) => {
                let (exited, reaped) = proc::pidfd_state(*pid, self.flags & PIDFD_THREAD != 0);
                (exited, false, reaped)
            }
            Kind::Ops(o) => o.readiness(),
            Kind::SeccompNotify(l) => l.readiness(),
            _ => (true, true, false),
        }
    }

    /// poll のビット (Linux の f_op->poll と同じもの: POLLIN、POLLOUT、POLLERR、POLLHUP、POLLPRI、POLLRDHUP ...)。
    /// AF_UNIX のソケット、パイプ、いつでも読み書きできるもの (Linux の DEFAULT_POLLMASK) だけ。ほかは None
    /// (readiness の 3 つで)
    pub fn poll_mask(&self) -> Option<u32> {
        Some(match &self.kind {
            // ソケット (AF_UNIX、TCP / UDP: recvmmsg がとっておいたエラーも)
            Kind::Ops(o) if o.mask_readiness() => o.poll_mask().unwrap_or(0) | self.recv_err_mask(),
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
            Kind::Inode(..) | Kind::Null | Kind::Zero | Kind::Random => POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM,
            Kind::Ops(o) => return o.poll_mask(),
            _ => return None,
        })
    }

    /// poll で待つときの印 (変わったときに poll_wake されるもの)。None はわからない (何かあれば起こす)
    pub fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        Some(match &self.kind {
            Kind::Tty(t) | Kind::PtyMaster(t) => alloc::vec![t.borrow().poll_key()],
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => alloc::vec![Rc::as_ptr(p) as usize],
            // eventfd、inotify、ソケット (FileOps::poll_keys)。印がないと、どこかのパイプに書くたびに起こされる (Firefox の
            // gmain が 1 秒に何百回も起きていた)
            Kind::PidFd(_) => alloc::vec![proc::pidfd_key()],
            Kind::SeccompNotify(l) => alloc::vec![l.chan()],
            // いつでも読み書きできる (待たない)
            Kind::Null | Kind::Zero | Kind::Random | Kind::Inode(..) => alloc::vec![],
            Kind::Ops(o) => return o.poll_keys(),
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
                let new = new.filter(|&n| n >= 0 && n <= maxbytes).ok_or_else(|| crate::why::inval())?;
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
            let new = new.filter(|&n| n >= 0).ok_or_else(|| crate::why::inval())?;
            self.offset = new as usize;
            return Ok(self.offset);
        }
        let size = match &self.kind {
            Kind::Inode(ino, _) => ino.meta().size as i64,
            Kind::Null | Kind::Zero | Kind::Random => 0,
            Kind::Ops(o) => o.size().ok_or(-ESPIPE)?,
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
            _ => return Err(crate::why::inval()),
        };
        let new = base.checked_add(off).filter(|&n| n >= 0).ok_or_else(|| crate::why::inval())?;
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
                    return Err(crate::why::inval());
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
            return Some(Err(crate::why::inval()));
        }
        Some(Ok(()))
    }
}

// [lockless]
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
        reuse_off(self);
        // flock のロックと OFD ロックは OpenFile ごと (その場所で見分ける): 最後の参照が閉じたら外す
        locks().release_file(self as *const OpenFile as usize);
        match &self.kind {
            Kind::Ops(o) => o.release(),
            Kind::PidFd(pid) => crate::pidfd::closed(*pid),
            Kind::Inode(ino, path) => {
                let ev = if self.writable() { crate::vfs::notify::IN_CLOSE_WRITE } else { crate::vfs::notify::IN_CLOSE_NOWRITE };
                crate::vfs::notify::file_event(path, ino, ev);
                ino.released();
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
            Kind::Tty(t) if matches!(t.borrow().dev, tty::Dev::Pty(_)) => tty::close_slave(t),
            Kind::PtyMaster(t) => tty::close_master(t),
            _ => {}
        }
    }
}

/// パイプの大きさ (Linux と同じ既定値と、F_SETPIPE_SZ で広げられる上限)
pub const PIPE_SIZE: usize = 64 * 1024;

/// 大きなロックなしの read/write (Pipe::fast_rw) の数: [0] はうまくいったもの、ほかはふつうの道へ行ったわけ
/// (2 大きさ、3 fd の表をほかのスレッドが変えている途中、4 パイプでない、5 6 ... は fast_rw_inner を見よ)。7 は、うまくいった
/// もののうち空のパイプで終わり (0) か EAGAIN を返したもの。/proc/bkl に出す
pub static FAST_RW: [core::sync::atomic::AtomicU64; 12] = [const { core::sync::atomic::AtomicU64::new(0) }; 12];
/// 大きなロックなしのファイルの read (fast_read_file) の数: [0] うまくいったもの、ほかはふつうの道へ行ったわけ
/// (1 大きさか、わざとしくじらせる決まりがある、2 IN_ACCESS の見張り、3 fd の表をほかのスレッドが変えている途中、4 ほかの人がその開いたファイルの
/// 位置を使っている途中 (pos_slow)、5 ext4 のファイルでない、6 ページキャッシュにない)。/proc/bkl に出す
pub static FAST_READ: [core::sync::atomic::AtomicU64; 7] = [const { core::sync::atomic::AtomicU64::new(0) }; 7];
pub const PIPE_MAX: usize = 1024 * 1024;

/// パイプの中身: Linux の pipe_buffer と同じく、ページ (4KiB) ひとつずつの箱の列。箱の数はパイプの大きさ / 4KiB
/// まで (Linux の max_usage。POLLOUT は箱があいていること)。write は、書く長さの 4KiB の端数が前の箱にまるごと
/// 入るときだけそこへ足し (merge)、ほかは新しい箱へ (anon_pipe_write)。なので 4KiB (PIPE_BUF) までの write は
/// 分かれない。splice で入れたもの (ファイルのページのかけら) は足さない箱。読みきった箱はすぐ捨てる (pipe_read)
pub struct PageQueue {
    /// (ページ, 読むところ, 書いたところ, 足してよいか)
    pub pages: alloc::collections::VecDeque<(*mut u8, usize, usize, bool)>,
    pub len: usize,
    /// 読みきった箱のページを 1 つとっておき、次の箱に使う (Linux の pipe->tmp_page。小さな write と read を
    /// くりかえすたびに、ページをとって返さないように)
    pub spare: Option<*mut u8>,
}

impl PageQueue {
    const fn new() -> Self {
        PageQueue { pages: alloc::collections::VecDeque::new(), len: 0, spare: None }
    }

    /// 新しい箱のページ (書いたところしか読まないので、0 にしなくてよい)
    // [lockless]
    pub fn page(&mut self) -> Option<*mut u8> {
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
    // [lockless]
    fn write_room(&self, n: usize, slots: usize, merge: bool) -> usize {
        let free = slots.saturating_sub(self.pages.len());
        let chars = n % PGSIZE;
        let merged = if merge && chars != 0 && self.pages.back().is_some_and(|b| b.3 && b.2 + chars <= PGSIZE) { chars } else { 0 };
        merged + (n - merged).min(free * PGSIZE)
    }

    /// write の 1 回分: 入るだけ入れて (write_room と同じだけ)、入れたバイト数。ページがとれなければ、何も入れて
    /// いなければ ENOMEM
    // [lockless]
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

    /// 先頭から n バイトを捨てる (pop で写したあと)
    pub fn skip(&mut self, mut n: usize) {
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
    pub fn pop(&mut self, dst: &mut [u8], consume: bool) -> usize {
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
    pub data: PageQueue,
    pub cap: usize,
    pub readers: usize,
    pub writers: usize,
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
    pub sleepers: usize,
    /// poll / select で、いま待っている (待つかもしれない) 数 (PollWatch)。0 でなければ fast_rw は起こしを頼む
    pub watchers: usize,
    /// epoll に登録されたことがある (いつ待たれるかわからないので、fast_rw はいつも起こしを頼む)
    pub epolled: bool,
}

/// 大きなロックなしの read (syscall::fast): ext4 のふつうのファイルで、読むところがページキャッシュに全部あるとき。
/// fd の表をほかのスレッドと、開いたファイル (オフセット) をほかの fd やプロセスと分けていない、
/// IN_ACCESS を待つ inotify の見張りがない、ときだけ。ほかは None でふつうの道へ
// [lockless]
pub fn fast_read_file(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Option<i64> {
    let r = fast_read_file_inner(me, fd, va, len);
    FAST_READ[r.err().unwrap_or(0)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    r.ok()
}

/// 大きなロックなしの lseek (syscall::fast): ページキャッシュを通るファイル (ext4、tmpfs) の SEEK_SET、SEEK_CUR と、
/// 大きさをページキャッシュが知っているときの SEEK_END。fd の表と開いたファイルをだれとも分けていないときだけ
/// (fast_read_file と同じ。Linux も分けていなければ f_pos_lock を取らない)。位置だけを変えるので、ディレクトリでも同じ
/// (消した ext4 のディレクトリは s_maxbytes までなので、4 TiB より先はふつうの道で)。ほかは None でふつうの道へ
// [lockless]
pub fn fast_lseek(me: &proc::Proc, fd: u64, off: i64, whence: u64) -> Option<i64> {
    let files = me.files.as_ref()?;
    if !files.private() {
        return None;
    }
    let f = files.get().get(fd)?;
    if Rc::strong_count(f) != 1 {
        return None;
    }
    // ほかのプロセスが pidfd_getfd で写したばかりかもしれない: 位置の札を持って
    let _pos = pos_fast(f)?;
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

/// 大きなロックなしの fcntl の F_GETFD、F_SETFD、F_GETFL (syscall::fast。Rust の std は開くたびに F_SETFD を呼ぶ):
/// fd の表の印 (FD_CLOEXEC) と、開いたファイルの口を見るだけ (Linux も files->file_lock と、ロックなしの f_flags)。
/// ほかのスレッドと表を分けていれば門を通る (見ているあいだ、ふつうの道は fd を閉じたり置きかえたりしない)。F_GETFL は
/// ファイル (Kind::Inode) だけ (ソケットなどの O_LARGEFILE は FileOps に聞くので、ふつうの道で)。開いていない fd、ほかの
/// cmd はふつうの道へ
// [lockless]
pub fn fast_fcntl(me: &proc::Proc, fd: u64, cmd: u64, arg: u64) -> Option<i64> {
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const FD_CLOEXEC: u64 = 1;
    const O_LARGEFILE: u32 = 0o400000;
    if !matches!(cmd, F_GETFD | F_SETFD | F_GETFL) || crate::debug::FAIL_ON.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let files = me.files.as_ref()?;
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let slot = files.get().fds.get_mut(fd as usize)?.as_mut()?;
    match cmd {
        F_GETFD => Some(if slot.cloexec { FD_CLOEXEC as i64 } else { 0 }),
        F_SETFD => {
            slot.cloexec = arg & FD_CLOEXEC != 0;
            Some(0)
        }
        _ => {
            // 借りずに (ほかのプロセスと分けている OpenFile かもしれない。口の印を読むだけ)
            let of = unsafe { &*slot.file.as_ptr() };
            if !matches!(of.kind, Kind::Inode(..)) || of.flags & O_PATH != 0 {
                return None;
            }
            Some((of.flags | O_LARGEFILE) as i64)
        }
    }
}

/// 大きなロックなしの getcwd (syscall::fast): "/" と fd の表の cwd と NUL を写す (表をほかのスレッドと分けていれば門の
/// 中で読む: chdir は門を通して変える)。chroot の中 (ルートからのパスに読みかえる)、入れ物が小さい (ERANGE)、写せない
/// ページはふつうの道へ
// [lockless]
pub fn fast_getcwd(me: &proc::Proc, buf: usize, len: usize) -> Option<i64> {
    let files = me.files.as_ref()?;
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let t = files.get();
    if !t.root.is_empty() {
        return None;
    }
    let cwd = t.cwd.as_bytes();
    let n = cwd.len() + 2;
    if n > len {
        return None;
    }
    let pt = me.pt();
    (pt.copy_out_nofault(buf, b"/") && pt.copy_out_nofault(buf + 1, cwd) && pt.copy_out_nofault(buf + 1 + cwd.len(), &[0])).then_some(n as i64)
}

/// 開いていない fd の close (EBADF) を大きなロックなしで: fd の表をだれとも分けていないときだけ見る
// [lockless]
pub fn fast_close_badf(me: &proc::Proc, fd: u64) -> bool {
    me.files.as_ref().is_some_and(|files| files.private() && files.get().get(fd).is_none())
}

/// 装置やソケットの mmap (Linux の f_op->mmap: syscall.rs の mmap から)。画面 (/dev/fb0) はフレームバッファのページを
/// そのまま、音の再生の口はリングバッファのページ、TCP のソケットはページのない写し (Linux の tcp_mmap: 書くもの、
/// 実行するものは EPERM。さわると SIGBUS。Linux は getsockopt の TCP_ZEROCOPY_RECEIVE で受けたものをここに入れる)。
/// 写せない種類は None (ENODEV)
pub fn mmap_dev(kind: &Kind, fr: &FileRef, off: usize, len: usize, prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
    match kind {
        Kind::Ops(o) => o.mmap(fr, off, len, prot),
        _ => None,
    }
}

/// 大きなロックなしのソケット (AF_UNIX) の read / write (syscall::fast。unix.rs の fast_rw)
#[inline]
// [lockless]
pub fn fast_rw_sock(me: &proc::Proc, fd: u64, va: usize, len: usize, write: bool) -> Option<i64> {
    FAST_RW_SOCK.val()?(me, fd, va, len, write)
}

/// 大きなロックなしの /dev/null・/dev/zero・/dev/urandom の read / write (syscall::fast)。書くものは捨てるだけ
/// (ユーザーのメモリも読まない。Linux と同じ)、/dev/null は読むと 0、/dev/zero は 0 で埋める (/dev/urandom の
/// 読みはふつうの道)。開いたファイルの種類と読み書きの口はあとで変わらないので、借りずに見る
// [lockless]
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
// [lockless]
fn fast_read_file_inner(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Result<i64, usize> {
    const MAX: usize = 256 * 1024;
    // わざとしくじらせる決まり (fail.rs) があるあいだは通らない (ふつうの道で決まりが効くように)
    if len == 0 || len > MAX || crate::debug::FAIL_ON.load(core::sync::atomic::Ordering::Relaxed) {
        return Err(1);
    }
    if crate::vfs::notify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        return Err(2);
    }
    let files = me.files.as_ref().ok_or(3usize)?;
    // ほかのスレッドと分けている表は門を通って見る (見ているあいだ、fd は閉じられず、表も動かない)
    let _gate = if files.private() { None } else { Some(files.get().gate.read().ok_or(3usize)?) };
    let f = files.get().get(fd).ok_or(5usize)?;
    // 開いたファイルの位置の札 (Linux の f_pos_lock): ほかの CPU がこの開いたファイルの位置を使っている途中
    // (ほかのスレッドや、fork で分けたプロセスの read、lseek など) ならふつうの道へ
    let _pos = pos_fast(f).ok_or(4usize)?;
    // ほかのプロセスが大きなロックの中で借りているかもしれないので、借りずに見る (位置は札を持つ人だけが使う)
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

/// 開いたファイルの位置 (offset) の札 (Linux の f_pos_lock と fdget_pos)。開いたファイルの場所のハッシュで選ぶ。
/// 大きなロックなしの read (fast_read_file) は fast を取ってから slow を見て、0 でなければやめる (ふつうの道へ)。
/// 位置を使うシステムコール (syscall::dispatch の pos_guard: read、write、lseek、getdents64 など) は、ふつうの
/// ファイルとディレクトリなら slow を増やしてから fast が空くのを待つ (大きなロックなしの read は眠らず、すぐ終わる)。
/// どちらかがかならず相手に気づくので (proc::Gate と同じ形)、同じ開いたファイルの位置を 2 人が同時に進めない。
/// ふつうの道どうしは大きなロックと read_inode_file の reading で順に。ハッシュがぶつかれば、ふつうの道へ行くだけ
#[repr(align(64))]
struct PosSlot {
    fast: core::sync::atomic::AtomicBool,
    slow: core::sync::atomic::AtomicU32,
}

static POS: [PosSlot; 64] = [const { PosSlot { fast: core::sync::atomic::AtomicBool::new(false), slow: core::sync::atomic::AtomicU32::new(0) } }; 64];

// [lockless]
fn pos_slot(f: &FileRef) -> &'static PosSlot {
    let a = Rc::as_ptr(f) as usize;
    &POS[((a >> 6) ^ (a >> 12)) % POS.len()]
}

/// 位置を使うシステムコールのあいだ (落とすと slow を戻す)
pub struct PosSlow(&'static PosSlot);

impl Drop for PosSlow {
    fn drop(&mut self) {
        self.0.slow.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

/// 位置を使うシステムコール (大きなロックの中) のはじめに: 終わるまで、大きなロックなしの read にこの開いたファイルを
/// 読ませない
pub fn pos_slow(f: &FileRef) -> PosSlow {
    use core::sync::atomic::{Ordering::SeqCst, fence};
    let s = pos_slot(f);
    s.slow.fetch_add(1, SeqCst);
    fence(SeqCst);
    while s.fast.load(SeqCst) {
        core::hint::spin_loop();
    }
    PosSlow(s)
}

/// 大きなロックなしの read が位置を使っているあいだ (落とすと fast を戻す)
pub struct PosFast(&'static PosSlot);

impl Drop for PosFast {
    // [lockless]
    fn drop(&mut self) {
        self.0.fast.store(false, core::sync::atomic::Ordering::Release);
    }
}

/// 大きなロックなしの道 (read、splice、lseek、write) が開いたファイルの位置を使う前に。取れなければ None (ふつうの道へ)
// [lockless]
pub fn pos_fast(f: &FileRef) -> Option<PosFast> {
    use core::sync::atomic::{Ordering::{Relaxed, Release, SeqCst}, fence};
    let s = pos_slot(f);
    // ほかの大きなロックなしの道が持っているなら、少し待つ (すぐ終わる。ふつうの道へ行くと、そのあいだほかの人も
    // みなふつうの道へ行き、行列になる)。ふつうの道が持っていれば待たない (眠るかもしれない)
    let mut spins = 0;
    while s.fast.compare_exchange_weak(false, true, SeqCst, Relaxed).is_err() {
        spins += 1;
        if spins > 4096 || s.slow.load(Relaxed) != 0 {
            return None;
        }
        core::hint::spin_loop();
    }
    fence(SeqCst);
    if s.slow.load(SeqCst) != 0 {
        s.fast.store(false, Release);
        return None;
    }
    Some(PosFast(s))
}

/// 大きなロックなしの getdents64 の数: [0] 答えた、ふつうの道へ: [1] 口の形 (O_PATH、ルートの ext4 でない、IN_ACCESS の
/// 見張り、fail)、[2] ext4 が読めない (ハッシュの順に読まない、キャッシュにない、消した、ディレクトリでない)、[3] 位置を
/// 使っている途中、[4] はじめの項目が入らない (EINVAL) かユーザーのページが写っていない。/proc/bkl に出す
pub static FAST_DENTS: [core::sync::atomic::AtomicU64; 5] = [const { core::sync::atomic::AtomicU64::new(0) }; 5];

/// 大きなロックなしの getdents64 (syscall::fast): ルートの ext4 のディレクトリを、ふつうの道 (OpenFile::getdents) と
/// 同じ位置と並び (ハッシュの順。d_off は次の項目の位置、終わりは i64::MAX) で。開いた口の位置の札を取って (ほかの人が
/// この口の位置を使っていればふつうの道へ)、写せたぶんだけ位置を進める。git status、du、rg はディレクトリごとに 2 度
/// ずつ呼ぶ (2 度目は 0 で終わり)。ふつうの道の覚え (OpenFile の dirents) にはさわらない: 位置はハッシュなので、どちらで
/// 読みついでも同じ並び
// [lockless]
pub fn fast_getdents(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Option<i64> {
    let r = fast_getdents_inner(me, fd, va, len);
    FAST_DENTS[r.err().unwrap_or(0)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    r.ok()
}

/// fast_getdents の中身。Err はふつうの道へ行ったわけ (FAST_DENTS の番号)
// [lockless]
fn fast_getdents_inner(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Result<i64, usize> {
    use core::sync::atomic::Ordering::Relaxed;
    const END: u64 = i64::MAX as u64;
    if len == 0 || crate::debug::FAIL_ON.load(Relaxed) || crate::vfs::notify::ACCESS_WATCHED.load(Relaxed) != 0 {
        return Err(1);
    }
    let files = me.files.as_ref().ok_or(1usize)?;
    // ほかのスレッドと分けている表は門を通って見る (見ているあいだ、fd は閉じられず、表も動かない)
    let _gate = if files.private() { None } else { Some(files.get().gate.read().ok_or(1usize)?) };
    let f = files.get().get(fd).ok_or(1usize)?;
    let _pos = pos_fast(f).ok_or(3usize)?;
    // ほかのプロセスが大きなロックの中で借りているかもしれないので、借りずに見る (位置は札を持つ人だけが使う)
    let of = unsafe { &mut *f.as_ptr() };
    if of.flags & O_PATH != 0 {
        return Err(1);
    }
    let Kind::Inode(ino, _) = &of.kind else { return Err(1) };
    let (fs, dir) = ino.id();
    let pos = of.offset as u64;
    let mut names = Vec::new();
    let list = crate::fastdir::readdir(fs, dir, pos, len, &mut names).ok_or(2usize)?;
    let mut out: Vec<u8> = Vec::with_capacity(len.min(32 * 1024));
    // 終わりまで返したら位置は終わり
    let mut next_pos = END;
    for (k, &(_, ino, mode, at, nlen)) in list.iter().enumerate() {
        let name = &names[at as usize..at as usize + nlen as usize];
        let next = list.get(k + 1).map_or(END, |e| e.0);
        let reclen = (19 + name.len() + 1 + 7) & !7;
        if out.len() + reclen > len {
            if out.is_empty() {
                return Err(4);
            }
            next_pos = list[k].0;
            break;
        }
        let start = out.len();
        out.extend_from_slice(&(ino as u64).to_le_bytes());
        out.extend_from_slice(&(next as i64).to_le_bytes());
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push(dtype(mode));
        out.extend_from_slice(name);
        out.resize(start + reclen, 0);
    }
    if !out.is_empty() && !me.pt().copy_out_nofault(va, &out) {
        return Err(4);
    }
    // 読み終えていれば (位置がもう終わりでも) 終わりに
    of.offset = next_pos as usize;
    Ok(out.len() as i64)
}

/// 大きなロックなしの pread64 (syscall::fast): ext4 のふつうのファイルで、読むところがページキャッシュに全部
/// あるとき。オフセットを動かさないので、fd の表をほかのスレッドと分けていても (門を通って) 読める
// [lockless]
pub fn fast_pread(me: &proc::Proc, fd: u64, va: usize, len: usize, off: i64) -> Option<i64> {
    const MAX: usize = 256 * 1024;
    if len == 0 || len > MAX || off < 0 || crate::vfs::notify::ACCESS_WATCHED.load(core::sync::atomic::Ordering::Relaxed) != 0 || crate::debug::FAIL_ON.load(core::sync::atomic::Ordering::Relaxed) {
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

/// 大きなロックなしの pwrite64 (syscall::fast): ext4 のファイルの、ページキャッシュにある (ブロックのキャッシュのページ
/// そのもの) ところを、大きさを変えずに、inode の変わらない秒のうちに書きかえるだけのもの (vm::write_fast)。
/// O_DSYNC / O_SYNC / O_DIRECT で開いたもの、ほかはふつうの道へ (Linux と同じ順でエラーにする)
// [lockless]
pub fn fast_pwrite(me: &proc::Proc, fd: u64, va: usize, len: usize, off: i64) -> Option<i64> {
    const MAX: usize = 256 * 1024;
    const O_DSYNC: u32 = 0o10000;
    const O_DIRECT: u32 = 0o200000;
    if len == 0 || len > MAX || off < 0 || crate::debug::FAIL_ON.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let files = me.files.as_ref()?;
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let f = files.get().get(fd)?;
    // fast_pread と同じく借りずに見る (開いたファイルの種類と口はあとで変わらない)
    let of = unsafe { &*f.as_ptr() };
    if !of.writable() || of.flags & (O_DSYNC | O_DIRECT) != 0 {
        return None;
    }
    let Kind::Inode(ino, _) = &of.kind else { return None };
    if !ino.page_cacheable() {
        return None;
    }
    crate::vm::write_fast(me.pt(), ino.id(), off as usize, va, len).map(|n| n as i64)
}

/// 大きなロックなしの write (syscall::fast): ふつうのファイルの終わりへの書きたし (Inode::fast_append: ext4 の、最後の
/// extent がそのまま伸びるもの)。fd の表が自分だけで、開いたファイルを持っているのが自分だけ (dup も fork もしていない:
/// 位置を変えるので)、1 バイトから FAST_WRITE_MAX まで。中身はロックを取る前にカーネルの入れ物に写す (ページが写って
/// いなければふつうの道へ)。O_DSYNC / O_SYNC / O_DIRECT、開いたときの LSM のラベルのあるもの (file_permission)、IN_MODIFY
/// の見張りがあるとき (ふつうの道ができごとを出す)、わざとしくじらせる決まりのあるとき (fail.rs) は、ふつうの道へ
// [lockless]
pub fn fast_write_file(me: &proc::Proc, fd: u64, va: usize, len: usize) -> Option<i64> {
    const FAST_WRITE_MAX: usize = 64 * 1024;
    const O_DSYNC: u32 = 0o10000;
    const O_DIRECT: u32 = 0o200000;
    if len == 0 || len > FAST_WRITE_MAX || crate::debug::FAIL_ON.load(core::sync::atomic::Ordering::Relaxed) || MODIFY_WATCHED.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        return None;
    }
    let files = me.files.as_ref().filter(|f| f.private())?;
    let f = files.get().get(fd)?;
    if Rc::strong_count(f) != 1 {
        return None;
    }
    // 表も開いたファイルも自分だけなので、借りずに見て、位置を書く (ほかの大きなロックなしの道と同じ。ほかのプロセスが
    // pidfd_getfd で写したばかりかもしれないので、位置の札を持って)
    let _pos = pos_fast(f)?;
    let of = unsafe { &mut *f.as_ptr() };
    if !of.writable() || of.flags & (O_PATH | O_DSYNC | O_DIRECT) != 0 || of.label.is_some() {
        return None;
    }
    let Kind::Inode(ino, _) = &of.kind else { return None };
    let mut data = crate::heap::try_vec(len, false).ok()?;
    if !me.pt().copy_in_nofault(va, &mut data) {
        return None;
    }
    let off = if of.flags & O_APPEND != 0 { None } else { Some(of.offset) };
    let (n, end) = ino.fast_append(off, &data)?;
    FAST_WRITE[0].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    of.offset = end;
    Some(n as i64)
}

/// パイプ (key はその PipeCell の場所) で眠っているもの、poll / epoll で待っているものを起こす (大きなロックを持って)
pub fn wake_key(key: usize) {
    proc::wakeup(key);
    proc::poll_wake(key);
}

/// poll / select が待っているあいだ、見ているパイプに印をつける (watchers)。大きなロックなしの read/write
/// (fast_rw) は、印のあるパイプでは起こしを頼む。readiness を見る前につけること
/// AF_UNIX のソケットも (unix::fast_rw)
pub struct PollWatch(Vec<Rc<PipeCell>>, Vec<Rc<dyn FileOps>>);

impl PollWatch {
    pub fn new<'a>(files: impl Iterator<Item = &'a FileRef>) -> Self {
        let (mut v, mut us) = (Vec::new(), Vec::new());
        for f in files {
            match &f.borrow().kind {
                Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                    p.borrow_mut().watchers += 1;
                    v.push(p.clone());
                }
                Kind::Ops(o) => {
                    o.watch(true);
                    us.push(o.clone());
                }
                _ => {}
            }
        }
        PollWatch(v, us)
    }
}

impl Drop for PollWatch {
    fn drop(&mut self) {
        for p in &self.0 {
            p.borrow_mut().watchers -= 1;
        }
        for o in &self.1 {
            o.watch(false);
        }
    }
}

/// epoll に登録した (Pipe::epolled と、AF_UNIX のソケットの epolled)
pub fn mark_epolled(f: &FileRef) {
    match &f.borrow().kind {
        Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => p.borrow_mut().epolled = true,
        Kind::Ops(o) => o.mark_epolled(),
        _ => {}
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
        let t = crate::timer::coarse_epoch_ns();
        Self { owner: core::sync::atomic::AtomicUsize::new(0), inner: crate::spinlock::SpinLock::new(p), uid: AtomicU32::new(c.fsuid), gid: AtomicU32::new(c.fsgid), perm: AtomicU32::new(0o600), mtime: AtomicU64::new(t), ctime: AtomicU64::new(t), fifo: core::sync::atomic::AtomicBool::new(false) }
    }

    /// 中身が変わった: mtime と ctime をいまに (Linux の pipefs と同じく粗い時計で: 読み書きのたびに時計を読まない)
    pub fn touch(&self) {
        use core::sync::atomic::Ordering::Relaxed;
        let t = crate::timer::coarse_epoch_ns();
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
    // [lockless]
    pub fn slots(&self) -> usize {
        (self.cap / PGSIZE).max(1)
    }

    /// 使っている箱の数
    // [lockless]
    pub fn used_slots(&self) -> usize {
        self.data.pages.len()
    }

    /// 箱がみなうまっている (Linux の pipe_full。write は端数を前の箱に足せるときのほか、入らない)
    // [lockless]
    pub fn full(&self) -> bool {
        self.used_slots() >= self.slots()
    }

    /// n バイトの write (の初め) が、いま入るバイト数
    // [lockless]
    pub fn write_room(&self, n: usize) -> usize {
        self.data.write_room(n, self.slots(), true)
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
    // [lockless]
    pub fn fast_rw(me: &proc::Proc, fd: u64, buf: usize, len: usize, write: bool) -> Option<i64> {
        let r = Pipe::fast_rw_inner(me, fd, buf, len, write);
        FAST_RW[r.err().unwrap_or(0)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        r.ok()
    }

    /// うまくいけば Ok、ふつうの道へ行くなら Err(わけ: FAST_RW の番号)
    // [lockless]
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
        if write {
            let mut tmp = Vec::with_capacity(len);
            unsafe { tmp.set_len(len) };
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
                // 空でも、書き手がいなければ終わり (0)、O_NONBLOCK なら EAGAIN: どちらも眠らず、何も変えない (Linux の
                // pipe_read と同じ)。イベントループは起こしの印のパイプを EAGAIN まで読むので、それがみな大きなロックを
                // 取っていた (Firefox で 3000 回、1 回 160 µs)。待つものだけふつうの道で眠る
                if pp.writers == 0 || unsafe { core::ptr::read_volatile(&of.flags) } & O_NONBLOCK != 0 {
                    FAST_RW[7].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return Ok(if pp.writers == 0 { 0 } else { -11 }); // EAGAIN
                }
                return Err(10);
            }
            let n = len.min(pp.data.len);
            let mut tmp = Vec::with_capacity(n);
            unsafe { tmp.set_len(n) };
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
    pub fn sleep(p: &Rc<PipeCell>, mut pp: PipeGuard<'_>) -> Result<(), i64> {
        pp.sleepers += 1;
        drop(pp);
        let r = proc::sleep(Rc::as_ptr(p) as usize);
        p.borrow_mut().sleepers -= 1;
        r
    }

    pub fn wake(p: &Rc<PipeCell>) {
        wake_key(Rc::as_ptr(p) as usize);
    }

    /// 読む。peek なら取り除かない (tee 用)
    pub fn read_ex(p: &Rc<PipeCell>, dst: &mut [u8], peek: bool, nonblock: bool) -> Result<usize, i64> {
        let _p = crate::debug::span("file:pipe_read");
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

    /// nonblock なら、書けるだけ書いて、1 バイトも書けなければ EAGAIN。入れ方は Linux の anon_pipe_write
    /// (PageQueue): 初めに端数を前の箱に足せたら足し、あとは箱があくだけ
    fn write(p: &Rc<PipeCell>, src: &[u8], nonblock: bool) -> Result<usize, i64> {
        let _p = crate::debug::span("file:pipe_write");
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

/// pread の、ふつうのファイル (sysfile が、借りずに読む) でないもの: ディスクと画面は pos から、/dev/null と
/// /dev/zero と乱数は位置によらない。開いたファイルの位置は動かさない。位置のないものは ESPIPE
pub fn pread_dev(f: &FileRef, pos: usize, dst: &mut [u8]) -> Result<usize, i64> {
    match &f.borrow().kind {
        Kind::Ops(o) if o.seekable() => o.read_at(pos, dst),
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
        Kind::Ops(o) if o.seekable() => o.write_at(pos, src),
        Kind::Null | Kind::Zero => Ok(src.len()),
        Kind::Random => {
            crate::rand::mix(src);
            Ok(src.len())
        }
        _ => Err(-ESPIPE),
    }
}

