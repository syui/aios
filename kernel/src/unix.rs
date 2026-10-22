// AF_UNIX のソケット (Linux の net/unix/af_unix.c と同じ考え)。SOCK_STREAM、SOCK_DGRAM (SOCK_RAW もこれになる)、
// SOCK_SEQPACKET。どのソケットも受ける行列 (メッセージ = Linux の skb の並び) を持ち、送る側は相手の行列に入れる。
//
// - 名前: ファイルシステムの名前は bind でソケットのファイル (S_IFSOCK) を作り、つなぐ相手はその inode で見つける
//   (名前を変えてもつながり、消せばつながらない)。抽象名前空間 (先頭が NUL) の名前は、長さもふくめて名前で、型ごと
//   (Linux の unix_abstract_hash は型をまぜるので、ちがう型どうしは見えない)、分けた NET ごと。autobind は "\0%05x"
// - つなぐ (stream / seqpacket): connect のときに受ける側の口 (embryo) を作って listen しているものの行列に入れる。
//   accept はそれを取り出す。つないだ側は accept の前から送れる (embryo の行列に入る)
// - 送る側の数え: Linux と同じく、メッセージごとの大きさ (skb の truesize: 中身と、kmalloc の箱と、ページの分) を
//   送った側の wmem に足し、読まれたら引く。wmem が sndbuf をこえたら EAGAIN (待つ)、wmem × 4 が sndbuf をこえたら
//   書けない (POLLOUT なし)。大きさは Linux 7.3 (arm64) で測ったとおり
// - SCM_RIGHTS (fd を渡す)、SCM_CREDENTIALS (SO_PASSCRED)、MSG_OOB (stream の 1 バイト)、MSG_PEEK と SO_PEEK_OFF、
//   shutdown、閉じたときの ECONNRESET / EPIPE / ECONNREFUSED、SO_PEERCRED
//
// Linux とちがうところ: MSG_OOB の 1 バイトを (SO_OOBINLINE なしで) 読みとばしたときは、FIONREAD の数からも引く
// (Linux 7.3 の manage_oob は inq_len を減らさず、FIONREAD が 1 多いままになる)
//
// ソケットはそれぞれ自分のロック (UnixCell: パイプの PipeCell と同じ) を持ち、Arc で分ける (送ったメッセージは送った口を
// 持つ: 大きなロックなしで動くところも数を変えられるように)。ふだんは大きなロックの中で動く。つないだ stream の
// ふつうのバイトの read / write は、眠らず起こさずにすめば大きなロックなしで (fast_rw: パイプの fast_rw と同じ形)。
// そのため、眠る人 (sleepers)、poll で見ている人 (watchers)、epoll に入れたか (epolled) をソケットのロックの中で数え、
// 大きなロックなしで入れたり出したりしたほうは、それがあれば起こしを大きなロックを持つ CPU に頼む (smp::defer_wake)。借りたまま眠らない、
// 同じソケットを二重に借りない (止まる)、借りたまま fd (FileRef) を落とさない (落とすと、
// それが最後ならそのソケットを閉じる: release)
use crate::file::{self, FileRef, Kind, Stat};
use crate::proc;
use crate::vfs::InodeRef;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::sync::{Arc, Weak};
use alloc::string::String;
use alloc::vec::Vec;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EAGAIN: i64 = 11;
const EACCES: i64 = 13;
const EFAULT: i64 = 14;
const ENOSPC: i64 = 28;
const EPIPE: i64 = 32;
const EDOM: i64 = 33;
const EMSGSIZE: i64 = 90;
const EPROTOTYPE: i64 = 91;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const EOPNOTSUPP: i64 = 95;
const EADDRINUSE: i64 = 98;
const ECONNRESET: i64 = 104;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ECONNREFUSED: i64 = 111;

pub const SOCK_STREAM: u8 = 1;
pub const SOCK_DGRAM: u8 = 2;
pub const SOCK_SEQPACKET: u8 = 5;
pub const SOCK_NONBLOCK: u64 = 0o4000;
const AF_UNSPEC: u16 = 0;
const AF_UNIX: u16 = 1;

pub const MSG_OOB: u64 = 0x1;
pub const MSG_PEEK: u64 = 0x2;
pub const MSG_CTRUNC: u32 = 0x8;
pub const MSG_TRUNC: u64 = 0x20;
pub const MSG_WAITALL: u64 = 0x100;
pub const MSG_NOSIGNAL: u64 = 0x4000;
pub const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
/// カーネルの中だけのフラグ: Recv の parts (メッセージごとの長さ) を作る (splice でパイプに入れるとき)
pub const MSG_PARTS: u64 = 1 << 62;

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
/// 1 回の sendmsg で渡せる fd の数 (Linux の SCM_MAX_FD)
const MAX_FDS: usize = 253;

/// poll のビット (Linux と同じ)
pub const POLLIN: u32 = 0x1;
pub const POLLPRI: u32 = 0x2;
pub const POLLOUT: u32 = 0x4;
pub const POLLERR: u32 = 0x8;
pub const POLLHUP: u32 = 0x10;
pub const POLLRDNORM: u32 = 0x40;
pub const POLLWRNORM: u32 = 0x100;
pub const POLLWRBAND: u32 = 0x200;
pub const POLLRDHUP: u32 = 0x2000;

/// shutdown のビット (Linux の RCV_SHUTDOWN と SEND_SHUTDOWN)
const RCV: u8 = 1;
const SEND: u8 = 2;
const BOTH: u8 = 3;

/// 入れ物の既定 (Linux の net.core.wmem_default / rmem_default) と、setsockopt で決められる上限 (wmem_max / rmem_max)
/// と下限 (SOCK_MIN_SNDBUF / SOCK_MIN_RCVBUF)
const BUF_DEFAULT: usize = 212992;
const BUF_MAX: usize = 4 << 20;
const MIN_SNDBUF: usize = 4608;
const MIN_RCVBUF: usize = 2304;
/// SOCK_DGRAM の行列の長さ (net.unix.max_dgram_qlen) と、listen の backlog の上限 (net.core.somaxconn)
const DGRAM_QLEN: usize = 10;
pub const SOMAXCONN: u32 = 4096;
/// 資格情報のないメッセージを SO_PASSCRED で受けたときの uid / gid (Linux の overflowuid: INVALID_UID を見せるもの)
const OVERFLOW_ID: u32 = 65534;

#[derive(Clone, Copy, PartialEq)]
enum State {
    /// つながっていない (Linux の TCP_CLOSE)
    Close,
    Listen,
    /// つながっている (dgram は connect したもの、connect されたもの)
    Established,
}

/// (pid は本当の番号, uid, gid)
pub type Cred = (u32, u32, u32);

/// 受ける行列のメッセージ (Linux の skb)
struct Msg {
    id: u64,
    data: Vec<u8>,
    /// stream で読んだところ (と、MSG_OOB で読んだ 1 バイト: Linux の UNIXCB(skb).consumed)。長さは data.len() - used
    used: usize,
    /// いっしょに渡す fd (SCM_RIGHTS)
    fds: Vec<FileRef>,
    /// 送った人。None は資格情報なし
    cred: Option<Cred>,
    /// 送った口 (recvfrom の名前は、受けるときのその口の名前。wmem を返す先)
    sender: Option<UnixRef>,
    truesize: usize,
    /// MSG_PEEK で見た長さ 0 のもの (SO_PEEK_OFF で次は飛ばす: Linux の skb->peeked)
    peeked: bool,
    /// fds のうち AF_UNIX のソケットの数 (INFLIGHT に足したもの)
    unix: usize,
}

impl Msg {
    fn new(data: Vec<u8>, fds: Vec<FileRef>, cred: Option<Cred>, sender: &UnixRef, truesize: usize) -> Msg {
        let unix = fds.iter().filter(|f| matches!(f.borrow().kind, Kind::Unix(_))).count();
        unsafe { INFLIGHT += unix };
        Msg { id: next_id(), data, used: 0, fds, cred, sender: Some(sender.clone()), truesize, peeked: false, unix }
    }

    /// fd も資格情報もない stream のバイト (fast_rw)。INFLIGHT にさわらない
    fn plain(data: Vec<u8>, sender: &UnixRef, truesize: usize) -> Msg {
        Msg { id: next_id(), data, used: 0, fds: Vec::new(), cred: None, sender: Some(sender.clone()), truesize, peeked: false, unix: 0 }
    }

    fn len(&self) -> usize {
        self.data.len() - self.used
    }

    /// 受けた fd を取りだす (もう送られている途中ではない)
    fn take_fds(&mut self) -> Vec<FileRef> {
        unsafe { INFLIGHT -= self.unix };
        self.unix = 0;
        core::mem::take(&mut self.fds)
    }
}

impl Drop for Msg {
    fn drop(&mut self) {
        // 大きなロックなしで落とすもの (fast_rw: fd はない) は INFLIGHT にさわらない (0 を引くのも書きもどしになる)
        if self.unix > 0 {
            unsafe { INFLIGHT -= self.unix };
        }
    }
}

pub struct Unix {
    pub typ: u8,
    state: State,
    /// 閉じた (Linux の SOCK_DEAD)
    dead: bool,
    shutdown: u8,
    /// SO_ERROR (正の errno、0 はなし)。読むと消える (Linux の sock_error)
    err: i64,
    peer: Option<UnixRef>,
    /// 名前 (struct sockaddr_un のバイト: family と sun_path。長さもこれ)
    addr: Option<Rc<Vec<u8>>>,
    /// ファイルシステムの名前の inode (名前を消されても持っておく: Linux の u->path)
    inode: Option<InodeRef>,
    q: VecDeque<Msg>,
    /// listen しているもの: つないできた口 (accept を待っているもの。Linux では受ける行列の skb)
    embryos: VecDeque<UnixRef>,
    /// 行列の長さの上限 (listen の backlog か、dgram の max_dgram_qlen: Linux の sk_max_ack_backlog)
    backlog: usize,
    /// MSG_OOB のメッセージ (行列の中の id)
    oob: Option<u64>,
    /// stream: 読まれていないバイト (FIONREAD。MSG_OOB の 1 バイトもふくむ: Linux の inq_len)
    inq: usize,
    /// 送って、まだ読まれていないメッセージの大きさの和 (Linux の sk_wmem_alloc - 1)
    wmem: usize,
    sndbuf: usize,
    rcvbuf: usize,
    /// SO_PEERCRED
    peercred: Option<Cred>,
    passcred: bool,
    passrights: bool,
    reuse: bool,
    keepalive: bool,
    broadcast: bool,
    oobinline: bool,
    dontroute: bool,
    priority: u32,
    /// SO_LINGER (入っているか、秒。切っても秒は残る: Linux の sk_lingertime)
    linger: bool,
    linger_secs: u64,
    rcvlowat: i32,
    peek_off: i32,
    /// SO_RCVTIMEO / SO_SNDTIMEO (ns、0 はいつまでも)
    rcvtimeo: u64,
    sndtimeo: u64,
    /// ソケットの fd がある (accept されるまでの、つないできた口にはない: Linux の sk_socket)。ないものに送るものには
    /// いつも資格情報をつける
    has_file: bool,
    /// 抽象名前空間を分ける NET (作ったときの)
    net: u64,
    /// fstat に見せるもの (Linux の sockfs の inode: 番号、持ち主、許可 (fchmod で変わり、bind のファイルの許可に
    /// なる)、時刻 (作ったときから。utimensat で変わる))
    ino: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: u64,
    ctime: u64,
    /// 状態が変わるたびに増える数 (epoll の EPOLLET)
    pub generation: u64,
    /// このソケットの印 (key) で眠っている数 (読む、書くのを待つ)。眠る前にロックの中で増やす (fast_rw が起こしを頼むか決める)
    sleepers: u32,
    /// poll / select で見ている数 (file::PollWatch) と、epoll に入れたか
    pub watchers: u32,
    pub epolled: bool,
}

pub type UnixRef = Arc<UnixCell>;

/// ソケットの中身のロック (file.rs の PipeCell と同じ)。借りるのはいつも 1 人 (borrow も borrow_mut も)。持ったまま眠らないこと。
/// 同じ CPU が二重に取ったら止める
pub struct UnixCell {
    owner: core::sync::atomic::AtomicUsize,
    inner: crate::spinlock::SpinLock<Unix>,
}

pub struct UnixGuard<'a> {
    g: crate::spinlock::Guard<'a, Unix>,
    owner: &'a core::sync::atomic::AtomicUsize,
}

impl UnixCell {
    fn new(u: Unix) -> Self {
        Self { owner: core::sync::atomic::AtomicUsize::new(0), inner: crate::spinlock::SpinLock::new(u) }
    }

    pub fn borrow_mut(&self) -> UnixGuard<'_> {
        use core::sync::atomic::Ordering::Relaxed;
        let me = crate::smp::id() + 1;
        if self.owner.load(Relaxed) == me {
            panic!("unix: cpu{} locks a socket twice", me - 1);
        }
        let g = self.inner.lock();
        self.owner.store(me, Relaxed);
        UnixGuard { g, owner: &self.owner }
    }

    pub fn borrow(&self) -> UnixGuard<'_> {
        self.borrow_mut()
    }
}

impl Drop for UnixGuard<'_> {
    fn drop(&mut self) {
        self.owner.store(0, core::sync::atomic::Ordering::Relaxed);
    }
}

impl core::ops::Deref for UnixGuard<'_> {
    type Target = Unix;
    fn deref(&self) -> &Unix {
        &self.g
    }
}

impl core::ops::DerefMut for UnixGuard<'_> {
    fn deref_mut(&mut self) -> &mut Unix {
        &mut self.g
    }
}

type R = Result<i64, i64>;

// ---- 名前の表 ----

static mut ABSTRACT: BTreeMap<(u64, u8, Vec<u8>), Weak<UnixCell>> = BTreeMap::new();
static mut BSD: BTreeMap<(usize, u64), Weak<UnixCell>> = BTreeMap::new();
static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
static mut NEXT_INO: u64 = 1;
/// メッセージの中にある (送られている途中の) AF_UNIX のソケットの fd の数。0 なら gc はしない
static mut INFLIGHT: usize = 0;
/// ソケットの fd が閉じた・ソケットを閉じた (システムコールの終わりに gc を見る)
static mut GC_WANTED: bool = false;
/// すべてのソケット (gc がメッセージの中の fd をたどる)。消えたものは gc と new_sock で掃く
static mut ALL: Vec<Weak<UnixCell>> = Vec::new();
static mut ALL_LIVE: usize = 0;

/// いま生きている AF_UNIX のソケットの数 (/proc/net/sockstat の sockets: used)
pub fn live() -> usize {
    unsafe { &*(&raw const ALL) }.iter().filter(|w| w.strong_count() > 0).count()
}

fn abstract_names() -> &'static mut BTreeMap<(u64, u8, Vec<u8>), Weak<UnixCell>> {
    unsafe { &mut *(&raw mut ABSTRACT) }
}

fn bsd_names() -> &'static mut BTreeMap<(usize, u64), Weak<UnixCell>> {
    unsafe { &mut *(&raw mut BSD) }
}

/// ソケットの inode の番号 (AF_UNIX と AF_INET で 1 つの並び: Linux の sockfs)
pub fn new_ino() -> u64 {
    unsafe {
        NEXT_INO += 1;
        NEXT_INO
    }
}

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1
}

fn key(u: &UnixRef) -> usize {
    Arc::as_ptr(u) as usize
}

fn is(w: &Weak<UnixCell>, u: &UnixRef) -> bool {
    core::ptr::eq(w.as_ptr(), Arc::as_ptr(u))
}

fn same(a: &Option<UnixRef>, b: &UnixRef) -> bool {
    a.as_ref().is_some_and(|x| Arc::ptr_eq(x, b))
}

/// 状態が変わった: epoll の数を進め、待っている人 (読む、書く、つなぐ、poll) を起こす
fn notify(u: &UnixRef) {
    u.borrow_mut().generation += 1;
    proc::wakeup(key(u));
    proc::poll_wake(key(u));
}

/// いまのプロセスの (pid, euid, egid) (SO_PEERCRED: Linux の cred_to_ucred は euid)
pub fn me() -> Cred {
    let c = proc::current_cred_ref();
    (proc::current().tgid, c.euid, c.egid)
}

/// いまのプロセスの (pid, uid, gid) (メッセージの資格情報: Linux の current_uid_gid は本当の uid)
fn me_real() -> Cred {
    let c = proc::current_cred_ref();
    (proc::current().tgid, c.uid, c.gid)
}

fn new_sock(typ: u8) -> UnixRef {
    let c = proc::current_cred_ref();
    let ino = new_ino();
    let u = Arc::new(UnixCell::new(Unix {
        typ,
        state: State::Close,
        dead: false,
        shutdown: 0,
        err: 0,
        peer: None,
        addr: None,
        inode: None,
        q: VecDeque::new(),
        embryos: VecDeque::new(),
        backlog: DGRAM_QLEN,
        oob: None,
        inq: 0,
        wmem: 0,
        sndbuf: BUF_DEFAULT,
        rcvbuf: BUF_DEFAULT,
        peercred: None,
        passcred: false,
        passrights: true,
        reuse: false,
        keepalive: false,
        broadcast: false,
        oobinline: false,
        dontroute: false,
        priority: 0,
        linger: false,
        linger_secs: 0,
        rcvlowat: 1,
        peek_off: -1,
        rcvtimeo: 0,
        sndtimeo: 0,
        has_file: true,
        net: crate::ns::net().unwrap_or(0),
        ino,
        uid: c.euid,
        gid: c.egid,
        mode: 0o777,
        mtime: crate::timer::epoch_ns(),
        ctime: crate::timer::epoch_ns(),
        generation: 0,
        sleepers: 0,
        watchers: 0,
        epolled: false,
    }));
    let all = unsafe { &mut *(&raw mut ALL) };
    if all.len() >= 64 && all.len() >= 2 * unsafe { ALL_LIVE } {
        all.retain(|w| w.strong_count() > 0);
        unsafe { ALL_LIVE = all.len() };
    }
    all.push(Arc::downgrade(&u));
    u
}

// ---- skb の大きさ (Linux 7.3 arm64 で測ったもの) ----

/// kmalloc の頭の大きさ h のときの truesize: 中身は 64 にそろえて skb_shared_info (320) を足し、640 までは小さな頭の
/// 箱、8192 までは 2 のべきの箱、そのうえはページの 2 のべき。それに struct sk_buff (256)
fn head_truesize(h: usize) -> usize {
    let sz = h.div_ceil(64) * 64 + 320;
    let a = if sz <= 640 {
        640
    } else if sz <= 8192 {
        sz.next_power_of_two()
    } else {
        sz.div_ceil(4096).next_power_of_two() * 4096
    };
    a + 256
}

/// dgram / seqpacket の len バイトのメッセージ: 頭に入らない分 (SKB_MAX_ALLOC 16064 をこえるところ) はページ
fn dgram_truesize(len: usize) -> usize {
    let data = if len > 16064 { (len - 16064).min(17 * 4096).div_ceil(4096) * 4096 } else { 0 };
    head_truesize(len - data) + data
}

/// stream の 1 つのメッセージの長さの上限 (sndbuf の半分 - 64、頭 (3776) とページ (32 KiB) まで)
fn stream_chunk(sndbuf: usize, left: usize) -> usize {
    left.min((sndbuf / 2).saturating_sub(64).max(1)).min(3776 + 32768)
}

/// stream の size バイトのメッセージ: 3776 をこえる分はページ
fn stream_truesize(size: usize) -> usize {
    let data = size.min(size.saturating_sub(3776).div_ceil(4096) * 4096);
    head_truesize(size - data) + data.div_ceil(4096) * 4096
}

// ---- 名前 ----

/// sockaddr_un のパスの名前 (NUL まで。Linux の unix_mkname_bsd) と、ふつうの長さ (family + パス + NUL)
fn bsd_name(a: &[u8]) -> (String, Vec<u8>) {
    let raw = &a[2..];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    let mut b = Vec::with_capacity(end + 3);
    b.extend_from_slice(&a[..2]);
    b.extend_from_slice(&raw[..end]);
    b.push(0);
    (String::from_utf8_lossy(&raw[..end]).into_owned(), b)
}

fn family(a: &[u8]) -> u16 {
    if a.len() < 2 { 0xffff } else { u16::from_le_bytes([a[0], a[1]]) }
}

/// Linux の unix_validate_addr: 長さが sun_family より長く sockaddr_un より短く、family が AF_UNIX
fn validate(a: &[u8]) -> Result<(), i64> {
    if a.len() <= 2 || a.len() > 110 || family(a) != AF_UNIX {
        return Err(crate::why::inval());
    }
    Ok(())
}

/// つなぐ / 送る相手 (Linux の unix_find_other)。パスの名前は inode で (書けること、ソケットのファイルであること、
/// それに bind したものがいること、型が同じこと)。抽象名前空間は型ごとの表で
fn find_other(a: &[u8], typ: u8, net: u64) -> Result<UnixRef, i64> {
    if a[2] != 0 {
        let (path, _) = bsd_name(a);
        let i = crate::sysfile::find_sock(&path)?;
        if i.meta().mode & crate::vfs::S_IFMT != crate::vfs::S_IFSOCK {
            return Err(-ECONNREFUSED);
        }
        let s = bsd_names().get(&i.id()).and_then(|w| w.upgrade()).ok_or(-ECONNREFUSED)?;
        if s.borrow().typ != typ {
            return Err(-EPROTOTYPE);
        }
        return Ok(s);
    }
    abstract_names().get(&(net, typ, a.to_vec())).and_then(|w| w.upgrade()).ok_or(-ECONNREFUSED)
}

/// SO_PASSCRED で名前がない (送る前、つなぐ前に autobind する。Linux の unix_may_passcred)
fn needs_autobind(u: &UnixRef) -> bool {
    let s = u.borrow();
    s.passcred && s.addr.is_none()
}

/// 名前のないもの: 抽象名前空間の "\0%05x" (Linux の unix_autobind。もう名前があれば何もしない)
fn autobind(u: &UnixRef) -> Result<(), i64> {
    if u.borrow().addr.is_some() {
        return Ok(());
    }
    let (net, typ) = {
        let s = u.borrow();
        (s.net, s.typ)
    };
    let mut r = [0u8; 4];
    crate::rand::fill(&mut r);
    let last = u32::from_le_bytes(r) & 0xfffff;
    let mut n = last;
    loop {
        n = (n + 1) & 0xfffff;
        let mut a = Vec::with_capacity(8);
        a.extend_from_slice(&AF_UNIX.to_le_bytes());
        a.push(0);
        a.extend_from_slice(alloc::format!("{:05x}", n).as_bytes());
        let k = (net, typ, a.clone());
        if !abstract_names().get(&k).is_some_and(|w| w.upgrade().is_some()) {
            abstract_names().insert(k, Arc::downgrade(u));
            u.borrow_mut().addr = Some(Rc::new(a));
            return Ok(());
        }
        if n == last {
            return Err(-ENOSPC);
        }
    }
}

// ---- 作る ----

/// socket(AF_UNIX, type, protocol): type は SOCK_TYPE_MASK の分 (フラグと範囲は呼ぶほうで見た)
pub fn create(typ: u64, protocol: u64) -> Result<UnixRef, i64> {
    // Linux の unix_create: protocol は 0 か PF_UNIX、SOCK_RAW は SOCK_DGRAM に
    if protocol != 0 && protocol != 1 {
        return Err(-EPROTONOSUPPORT);
    }
    let t = match typ {
        1 => SOCK_STREAM,
        2 | 3 => SOCK_DGRAM,
        5 => SOCK_SEQPACKET,
        _ => return Err(-ESOCKTNOSUPPORT),
    };
    Ok(new_sock(t))
}

pub fn file_of(u: UnixRef, flags: u64) -> FileRef {
    file::new(Kind::Unix(u), file::O_RDWR | if flags & SOCK_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 })
}

/// socketpair: 向かい合わせにつなぐ (どちらの SO_PEERCRED も自分)
pub fn pair(a: &UnixRef, b: &UnixRef) {
    let cred = me();
    for (x, y) in [(a, b), (b, a)] {
        let mut s = x.borrow_mut();
        s.peer = Some(y.clone());
        s.state = State::Established;
        s.peercred = Some(cred);
    }
}

// ---- bind / listen / connect / accept ----

pub fn bind(u: &UnixRef, a: &[u8]) -> R {
    // 長さが sun_family だけなら autobind
    if a.len() == 2 && family(a) == AF_UNIX {
        autobind(u)?;
        return Ok(0);
    }
    validate(a)?;
    if a[2] != 0 {
        // Linux の unix_bind_bsd: 名前のファイルを先に作り (あれば EADDRINUSE)、もう名前があれば消して EINVAL
        let (path, name) = bsd_name(a);
        let mode = u.borrow().mode;
        let (ino, parent, last) = crate::sysfile::bind_sock(&path, mode)?;
        if u.borrow().addr.is_some() {
            crate::sysfile::unbind_sock(&parent, &last);
            return Err(crate::why::inval());
        }
        bsd_names().insert(ino.id(), Arc::downgrade(u));
        let mut s = u.borrow_mut();
        s.addr = Some(Rc::new(name));
        s.inode = Some(ino);
        return Ok(0);
    }
    let (net, typ) = {
        let s = u.borrow();
        if s.addr.is_some() {
            return Err(crate::why::inval());
        }
        (s.net, s.typ)
    };
    let k = (net, typ, a.to_vec());
    if abstract_names().get(&k).is_some_and(|w| w.upgrade().is_some()) {
        return Err(-EADDRINUSE);
    }
    abstract_names().insert(k, Arc::downgrade(u));
    u.borrow_mut().addr = Some(Rc::new(a.to_vec()));
    Ok(0)
}

/// listen (backlog は somaxconn までにしたもの)
pub fn listen(u: &UnixRef, backlog: u32) -> R {
    {
        let mut s = u.borrow_mut();
        if s.typ != SOCK_STREAM && s.typ != SOCK_SEQPACKET {
            return Err(-EOPNOTSUPP);
        }
        if s.addr.is_none() {
            return Err(crate::why::inval());
        }
        if s.state != State::Close && s.state != State::Listen {
            return Err(crate::why::inval());
        }
        s.backlog = backlog as usize;
        s.state = State::Listen;
        // つないでくるものが写す資格情報
        s.peercred = Some(me());
    }
    notify(u);
    Ok(0)
}

/// 眠る (SO_RCVTIMEO / SO_SNDTIMEO があればそこまで: 時間切れは EAGAIN)
fn wait(chan: usize, timeo: u64) -> Result<(), i64> {
    if timeo == 0 {
        return proc::sleep(chan);
    }
    let deadline = crate::timer::ticks_after(timeo);
    if proc::sleep_until(chan, deadline)? { Ok(()) } else { Err(-EAGAIN) }
}

pub fn connect(f: &FileRef, u: &UnixRef, a: &[u8]) -> R {
    let nonblock = f.borrow().flags & file::O_NONBLOCK != 0;
    let typ = u.borrow().typ;
    if typ == SOCK_DGRAM {
        return dgram_connect(u, a);
    }
    validate(a)?;
    if needs_autobind(u) {
        autobind(u)?;
    }
    let net = u.borrow().net;
    loop {
        let other = find_other(a, typ, net)?;
        {
            let o = other.borrow();
            if o.state != State::Listen || o.shutdown & RCV != 0 {
                return Err(-ECONNREFUSED);
            }
        }
        let full = {
            let o = other.borrow();
            o.embryos.len() > o.backlog
        };
        if full {
            if nonblock {
                return Err(-EAGAIN);
            }
            let timeo = u.borrow().sndtimeo;
            wait(key(&other), timeo)?;
            continue;
        }
        match u.borrow().state {
            State::Close => {}
            State::Established => return Err(-EISCONN),
            State::Listen => return Err(crate::why::inval()),
        }
        join(u, &other, typ);
        return Ok(0);
    }
}

/// つなぐ: 受ける側の口 (embryo) を作り、listen しているもの (other) の行列に入れる
fn join(u: &UnixRef, other: &UnixRef, typ: u8) {
    let e = new_sock(typ);
    {
        let o = other.borrow();
        let mut es = e.borrow_mut();
        es.peer = Some(u.clone());
        es.state = State::Established;
        es.peercred = Some(me());
        es.addr = o.addr.clone();
        es.inode = o.inode.clone();
        es.passcred = o.passcred;
        es.passrights = o.passrights;
        es.has_file = false;
        es.net = o.net;
    }
    {
        let mut s = u.borrow_mut();
        s.peercred = other.borrow().peercred;
        s.state = State::Established;
        s.peer = Some(e.clone());
    }
    other.borrow_mut().embryos.push_back(e);
    notify(other);
    notify(u);
}

fn dgram_connect(u: &UnixRef, a: &[u8]) -> R {
    if a.len() < 2 {
        return Err(crate::why::inval());
    }
    let other = if family(a) != AF_UNSPEC {
        validate(a)?;
        if needs_autobind(u) {
            autobind(u)?;
        }
        let net = u.borrow().net;
        let other = find_other(a, SOCK_DGRAM, net)?;
        // Linux の unix_may_send: 相手がほかにつないでいれば EPERM
        if other.borrow().peer.as_ref().is_some_and(|p| !Arc::ptr_eq(p, u)) {
            return Err(-EPERM);
        }
        u.borrow_mut().state = State::Established;
        other.borrow_mut().state = State::Established;
        Some(other)
    } else {
        None
    };
    let old = u.borrow_mut().peer.take();
    u.borrow_mut().peer = other.clone();
    if let Some(old) = old {
        if other.is_none() {
            u.borrow_mut().state = State::Close;
        }
        if !other.as_ref().is_some_and(|o| Arc::ptr_eq(o, &old)) {
            disconnected(u, &old);
            if old.borrow().peer.is_none() {
                old.borrow_mut().state = State::Close;
            }
        }
        notify(&old);
    }
    notify(u);
    Ok(0)
}

/// dgram の相手が変わった・いなくなった (Linux の unix_dgram_disconnected): 行列を捨て、前の相手がこちらにつないで
/// いれば ECONNRESET を知らせる
fn disconnected(u: &UnixRef, old: &UnixRef) {
    let msgs: Vec<Msg> = u.borrow_mut().q.drain(..).collect();
    if msgs.is_empty() {
        return;
    }
    u.borrow_mut().inq = 0;
    free(msgs);
    notify(u);
    let back = {
        let o = old.borrow();
        !o.dead && same(&o.peer, u)
    };
    if back {
        old.borrow_mut().err = ECONNRESET;
        notify(old);
    }
}

/// accept: つないできた口を 1 つ (Linux の unix_accept と skb_recv_datagram: 自分のエラー、つないできたもの、
/// 待たないなら EAGAIN、読むのをやめていれば EINVAL)
pub fn accept(f: &FileRef, u: &UnixRef) -> Result<UnixRef, i64> {
    let nonblock = f.borrow().flags & file::O_NONBLOCK != 0;
    loop {
        {
            let mut s = u.borrow_mut();
            if s.typ != SOCK_STREAM && s.typ != SOCK_SEQPACKET {
                return Err(-EOPNOTSUPP);
            }
            if s.state != State::Listen {
                return Err(crate::why::inval());
            }
            if s.err != 0 {
                let e = s.err;
                s.err = 0;
                return Err(-e);
            }
            if let Some(e) = s.embryos.pop_front() {
                drop(s);
                e.borrow_mut().has_file = true;
                notify(u);
                return Ok(e);
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            if s.shutdown & RCV != 0 {
                return Err(crate::why::inval());
            }
        }
        let timeo = u.borrow().rcvtimeo;
        wait(key(u), timeo)?;
    }
}

/// getsockname / getpeername の名前 (struct sockaddr のバイト)。名前のないものは family だけ (長さ 2)。相手の名前は、
/// 相手が閉じたあとも (Linux の unix_peer_get)
pub fn name(u: &UnixRef, peer: bool) -> Result<Vec<u8>, i64> {
    // 相手は自分のこともある (自分につないだ dgram): 自分を放してから相手を借りる
    let addr = if peer {
        let p = u.borrow().peer.clone().ok_or(-ENOTCONN)?;
        p.borrow().addr.clone()
    } else {
        u.borrow().addr.clone()
    };
    Ok(match addr {
        Some(a) => (*a).clone(),
        None => AF_UNIX.to_le_bytes().to_vec(),
    })
}

// ---- 送る ----

/// sendmsg の付帯データ (Linux の scm_cookie): SCM_RIGHTS の fd と、SCM_CREDENTIALS
pub struct Scm {
    pub fds: Vec<FileRef>,
    pub cred: Option<Cred>,
}

impl Scm {
    pub fn none() -> Scm {
        Scm { fds: Vec::new(), cred: None }
    }
}

/// 付帯データのバイトを読む (Linux の __scm_send)。長さのおかしいもの、知らない SOL_SOCKET のものは EINVAL、閉じた fd
/// は EBADF、ほかの人の資格情報は EPERM、ない pid は ESRCH。SOL_SOCKET でないものは見ない
pub fn scm_parse(b: &[u8]) -> Result<Scm, i64> {
    let mut scm = Scm::none();
    let mut at = 0;
    while at + 16 <= b.len() {
        let clen = u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(b[at + 8..at + 12].try_into().unwrap());
        let typ = i32::from_le_bytes(b[at + 12..at + 16].try_into().unwrap());
        // Linux の CMSG_OK
        if clen < 16 || clen > b.len() - at {
            return Err(crate::why::inval());
        }
        if level == SOL_SOCKET {
            match typ {
                SCM_RIGHTS => {
                    let n = (clen - 16) / 4;
                    if n > MAX_FDS || scm.fds.len() + n > MAX_FDS {
                        return Err(crate::why::inval());
                    }
                    for c in b[at + 16..at + 16 + n * 4].chunks_exact(4) {
                        let fd = i32::from_le_bytes(c.try_into().unwrap());
                        if fd < 0 {
                            return Err(-file::EBADF);
                        }
                        let f = proc::current().files().get(fd as u64).cloned().ok_or(-file::EBADF)?;
                        scm.fds.push(f);
                    }
                }
                SCM_CREDENTIALS => {
                    if clen != 16 + 12 {
                        return Err(crate::why::inval());
                    }
                    let w = |o: usize| u32::from_le_bytes(b[at + 16 + o..at + 20 + o].try_into().unwrap());
                    let (pid, uid, gid) = (w(0), w(4), w(8));
                    // Linux の scm_check_creds: uid と gid は見える番号、pid は自分、uid と gid は自分のもの
                    // (root はどれでも)
                    if uid == u32::MAX || gid == u32::MAX {
                        return Err(crate::why::inval());
                    }
                    let uid = crate::ns::take_uid(uid)?;
                    let gid = crate::ns::take_gid(gid)?;
                    let c = proc::current_cred_ref();
                    let local = crate::ns::local_or_0(proc::current().tgid);
                    let root = c.euid == 0;
                    let uid_ok = uid == c.uid || uid == c.euid || uid == c.suid || root;
                    let gid_ok = gid == c.gid || gid == c.egid || gid == c.sgid || root;
                    if !((pid == local || root) && uid_ok && gid_ok) {
                        return Err(-EPERM);
                    }
                    let g = crate::ns::to_global(pid).filter(|&g| g != 0 && crate::proc::find_any(g).is_some()).ok_or(-ESRCH)?;
                    scm.cred = Some((g, uid, gid));
                }
                _ => return Err(crate::why::inval()),
            }
        }
        at += (clen + 7) & !7;
    }
    Ok(scm)
}

/// 送る中身 (Linux の msg_iter): カーネルの中のバイトか、ユーザーの入れ物 (iovec の (場所, 長さ))。送るときに少しずつ
/// 写す (さわれないところは EFAULT)
pub enum Src<'a> {
    Kernel(&'a [u8]),
    User(&'a [(usize, usize)]),
}

struct Iter<'a> {
    src: Src<'a>,
    at: usize,
}

impl Iter<'_> {
    /// 次の n バイト
    fn take(&mut self, n: usize) -> Result<Vec<u8>, i64> {
        let start = self.at;
        self.at += n;
        match self.src {
            Src::Kernel(b) => Ok(b[start..start + n].to_vec()),
            Src::User(iov) => {
                let mut v = crate::heap::try_vec(n, false)?;
                let (mut off, mut done) = (start, 0);
                let pt = proc::current().pt();
                for &(base, l) in iov {
                    if done == n {
                        break;
                    }
                    if off >= l {
                        off -= l;
                        continue;
                    }
                    let k = (l - off).min(n - done);
                    pt.copy_in(&mut v[done..done + k], base + off).ok_or(-EFAULT)?;
                    done += k;
                    off = 0;
                }
                Ok(v)
            }
        }
    }
}

/// 送るのに、入れ物 (wmem < sndbuf) があくまで待つ (Linux の sock_alloc_send_pskb)。自分のエラー、SEND の shutdown が先
fn wait_wmem(u: &UnixRef, nonblock: bool) -> Result<(), i64> {
    loop {
        let timeo = {
            let mut s = u.borrow_mut();
            if s.err != 0 {
                let e = s.err;
                s.err = 0;
                return Err(-e);
            }
            if s.shutdown & SEND != 0 {
                return Err(-EPIPE);
            }
            if s.wmem < s.sndbuf {
                return Ok(());
            }
            if nonblock {
                return Err(-EAGAIN);
            }
            // 眠る前に数える (大きなロックなしで読んだ人が、wmem を返したときに起こしを頼むように)
            s.sleepers += 1;
            s.sndtimeo
        };
        let r = wait(key(u), timeo);
        u.borrow_mut().sleepers -= 1;
        r?;
    }
}

/// 送るメッセージの資格情報 (Linux の unix_maybe_add_creds): どちらかが SO_PASSCRED か、相手がまだ accept されて
/// いなければ、いまのプロセスのもの
fn add_creds(cred: Option<Cred>, u: &UnixRef, other: &UnixRef) -> Option<Cred> {
    if cred.is_some() {
        return cred;
    }
    // 相手は自分のこともある (自分に送る dgram): 1 つずつ借りる
    let mine = u.borrow().passcred;
    let need = mine || {
        let o = other.borrow();
        o.passcred || !o.has_file
    };
    need.then(me_real)
}

/// 送る (sendto、sendmsg、write)。len バイトを src から、to は相手の名前 (長さ 0 は名前なし)。nonblock は O_NONBLOCK か
/// MSG_DONTWAIT
/// ctl は付帯データのバイト (読むのは sendmsg のはじめ: Linux の scm_send。SOCK_SEQPACKET はつながっているかを見てから)
pub fn send(u: &UnixRef, src: Src, len: usize, to: Option<&[u8]>, ctl: &[u8], flags: u64, nonblock: bool) -> R {
    let _p = crate::prof::span("unix:send");
    let to = to.filter(|t| !t.is_empty());
    let mut it = Iter { src, at: 0 };
    let typ = u.borrow().typ;
    match typ {
        SOCK_STREAM => stream_send(u, &mut it, len, to, scm_parse(ctl)?, flags, nonblock),
        SOCK_SEQPACKET => {
            // Linux の unix_seqpacket_sendmsg: 自分のエラー、つながっていなければ ENOTCONN、名前は見ない
            {
                let mut s = u.borrow_mut();
                if s.err != 0 {
                    let e = s.err;
                    s.err = 0;
                    return Err(-e);
                }
                if s.state != State::Established {
                    return Err(-ENOTCONN);
                }
            }
            dgram_send(u, &mut it, len, None, scm_parse(ctl)?, flags, nonblock)
        }
        _ => dgram_send(u, &mut it, len, to, scm_parse(ctl)?, flags, nonblock),
    }
}

fn sigpipe(flags: u64) {
    if flags & MSG_NOSIGNAL == 0 {
        let info = crate::signal::SigInfo::from(crate::signal::SI_KERNEL);
        crate::signal::send_thread(proc::current(), crate::signal::SIGPIPE, info);
    }
}

fn stream_send(u: &UnixRef, it: &mut Iter, len: usize, to: Option<&[u8]>, scm: Scm, flags: u64, nonblock: bool) -> R {
    let mut len = len;
    let oob = flags & MSG_OOB != 0;
    if oob {
        if len == 0 {
            return Err(-EOPNOTSUPP);
        }
        len -= 1;
    }
    if to.is_some() {
        return Err(if u.borrow().state == State::Established { -EISCONN } else { -EOPNOTSUPP });
    }
    let other = u.borrow().peer.clone().ok_or(-ENOTCONN)?;
    if u.borrow().shutdown & SEND != 0 {
        sigpipe(flags);
        return Err(-EPIPE);
    }
    let Scm { mut fds, cred } = scm;
    let mut sent = 0;
    let mut fail: Option<i64> = None;
    while sent < len {
        if let Err(e) = wait_wmem(u, nonblock) {
            fail = Some(e);
            break;
        }
        let size = stream_chunk(u.borrow().sndbuf, len - sent);
        let c = add_creds(cred, u, &other);
        let r = it.take(size).and_then(|data| deliver(u, &other, data, core::mem::take(&mut fds), c, stream_truesize(size), false));
        if let Err(e) = r {
            if e == -EPIPE && sent == 0 {
                sigpipe(flags);
            }
            fail = Some(e);
            break;
        }
        sent += size;
    }
    if fail.is_none() && oob {
        // 終わりの 1 バイトは MSG_OOB (Linux の queue_oob。こちらの EPIPE は SIGPIPE なし)
        let r = wait_wmem(u, nonblock).and_then(|_| {
            let c = add_creds(cred, u, &other);
            let data = it.take(1)?;
            deliver(u, &other, data, core::mem::take(&mut fds), c, stream_truesize(1), true)
        });
        match r {
            Ok(()) => sent += 1,
            Err(e) => fail = Some(e),
        }
    }
    match fail {
        Some(e) if sent == 0 => Err(e),
        _ => Ok(sent as i64),
    }
}

/// 相手 (other) の行列にメッセージを入れる (stream)。相手が閉じたか読むのをやめていれば EPIPE、fd を受けないなら EPERM
fn deliver(u: &UnixRef, other: &UnixRef, data: Vec<u8>, fds: Vec<FileRef>, cred: Option<Cred>, truesize: usize, oob: bool) -> Result<(), i64> {
    let _p = crate::prof::span("unix:deliver");
    let bad = {
        let o = other.borrow();
        if o.dead || o.shutdown & RCV != 0 {
            Some(-EPIPE)
        } else if !fds.is_empty() && !o.passrights {
            Some(-EPERM)
        } else {
            None
        }
    };
    if let Some(e) = bad {
        drop(fds);
        return Err(e);
    }
    let n = data.len();
    let m = Msg::new(data, fds, cred, u, truesize);
    let id = m.id;
    {
        let mut o = other.borrow_mut();
        o.q.push_back(m);
        o.inq += n;
        if oob {
            o.oob = Some(id);
        }
    }
    u.borrow_mut().wmem += truesize;
    notify(other);
    Ok(())
}

fn dgram_send(u: &UnixRef, it: &mut Iter, len: usize, to: Option<&[u8]>, scm: Scm, flags: u64, nonblock: bool) -> R {
    if flags & MSG_OOB != 0 {
        return Err(-EOPNOTSUPP);
    }
    if let Some(t) = to {
        validate(t)?;
    }
    if needs_autobind(u) {
        autobind(u)?;
    }
    if len + 32 > u.borrow().sndbuf {
        return Err(-EMSGSIZE);
    }
    wait_wmem(u, nonblock)?;
    let data = it.take(len)?;
    let Scm { fds, cred } = scm;
    let (typ, net) = {
        let s = u.borrow();
        (s.typ, s.net)
    };
    let mut other = match to {
        Some(t) => find_other(t, typ, net)?,
        None => u.borrow().peer.clone().ok_or(-ENOTCONN)?,
    };
    let cred = add_creds(cred, u, &other);
    loop {
        // Linux の unix_may_send: 相手がほかにつないでいれば EPERM
        if other.borrow().peer.as_ref().is_some_and(|p| !Arc::ptr_eq(p, u)) {
            return Err(-EPERM);
        }
        if other.borrow().dead {
            if typ == SOCK_SEQPACKET {
                return Err(-EPIPE);
            }
            if same(&u.borrow().peer, &other) {
                {
                    let mut s = u.borrow_mut();
                    s.peer = None;
                    s.state = State::Close;
                }
                disconnected(u, &other);
                return Err(-ECONNREFUSED);
            }
            match to {
                None => return Err(-ECONNRESET),
                Some(t) => {
                    other = find_other(t, typ, net)?;
                    continue;
                }
            }
        }
        {
            let o = other.borrow();
            if o.shutdown & RCV != 0 {
                return Err(-EPIPE);
            }
            if !fds.is_empty() && !o.passrights {
                return Err(-EPERM);
            }
        }
        // 相手の行列がいっぱい (相手がこちらにつないでいなければ: Linux の unix_recvq_full)
        let full = !Arc::ptr_eq(&other, u) && {
            let o = other.borrow();
            !same(&o.peer, u) && o.q.len() > o.backlog
        };
        if full {
            if nonblock {
                return Err(-EAGAIN);
            }
            let timeo = u.borrow().sndtimeo;
            wait(key(&other), timeo)?;
            continue;
        }
        break;
    }
    let ts = dgram_truesize(len);
    let m = Msg::new(data, fds, cred, u, ts);
    other.borrow_mut().q.push_back(m);
    u.borrow_mut().wmem += ts;
    notify(&other);
    Ok(len as i64)
}

/// 読まれた・捨てたメッセージ: 送った口の wmem を返す。そちらが入れ物を待っていた (wmem が sndbuf をこえていた) か
/// 書けなかった (POLLOUT なし) ときだけ起こす (Linux の sock_wfree と sk_write_space。返事を待って眠っている送り手を、
/// 読まれるたびに起こさない)。fd はここで落ちる
fn free(msgs: impl IntoIterator<Item = Msg>) {
    for mut m in msgs {
        if let Some(s) = m.sender.take() {
            let was_full = {
                let mut b = s.borrow_mut();
                let before = b.wmem;
                b.wmem = before.saturating_sub(m.truesize);
                before >= b.sndbuf || (before + 1) * 4 > b.sndbuf
            };
            if was_full {
                notify(&s);
            }
        }
        drop(m);
    }
}

// ---- 受ける ----

pub struct Recv {
    /// 受けたバイト
    pub data: Vec<u8>,
    /// 返す数 (MSG_TRUNC の dgram は、入れ物より大きいことがある)
    pub ret: usize,
    /// 送った口の名前 (なければ None: msg_namelen は 0)
    pub from: Option<Vec<u8>>,
    /// 付帯データを返す (Linux の scm_recv_unix。MSG_OOB で受けたときはない)
    pub scm: bool,
    pub fds: Vec<FileRef>,
    /// SO_PASSCRED なら資格情報 (このプロセスから見た pid、uid、gid)
    pub cred: Option<Cred>,
    /// msg_flags (MSG_TRUNC、MSG_OOB)
    pub flags: u32,
    /// data のうちメッセージごとの長さ (MSG_PARTS のときだけ。splice でパイプに入れるとき、メッセージごとに箱を分ける:
    /// Linux の skb_splice_bits)
    pub parts: Vec<usize>,
}

impl Recv {
    fn new() -> Recv {
        Recv { data: Vec::new(), ret: 0, from: None, scm: true, fds: Vec::new(), cred: None, flags: 0, parts: Vec::new() }
    }
}

/// 資格情報の見せかた (なければ pid 0、uid と gid は overflowuid)
fn show_cred(c: Option<Cred>) -> Cred {
    match c {
        Some((pid, uid, gid)) => (crate::ns::local_or_0(pid), crate::ns::show_uid(uid), crate::ns::show_gid(gid)),
        None => (0, OVERFLOW_ID, OVERFLOW_ID),
    }
}

/// 送った口の名前 (me は受ける口 u の中身: 自分に送ったものは、借りているそれから)
fn sender_name(m: &Msg, me: &Unix, u: &UnixRef) -> Option<Vec<u8>> {
    let s = m.sender.as_ref()?;
    let a = if Arc::ptr_eq(s, u) { me.addr.clone() } else { s.borrow().addr.clone() };
    a.map(|a| (*a).clone())
}

/// SO_PASSCRED か (付帯データに資格情報を入れる)
pub fn passcred(u: &UnixRef) -> bool {
    u.borrow().passcred
}

/// 受ける (recvfrom、recvmsg、read)。max は入れ物の大きさ
pub fn recv(u: &UnixRef, max: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    let _p = crate::prof::span("unix:recv");
    let typ = u.borrow().typ;
    match typ {
        SOCK_STREAM => stream_recv(u, max, flags, nonblock),
        SOCK_SEQPACKET => {
            if u.borrow().state != State::Established {
                return Err(-ENOTCONN);
            }
            dgram_recv(u, max, flags, nonblock)
        }
        _ => dgram_recv(u, max, flags, nonblock),
    }
}

/// Linux の __unix_dgram_recvmsg
fn dgram_recv(u: &UnixRef, max: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    if flags & MSG_OOB != 0 {
        return Err(-EOPNOTSUPP);
    }
    let peek = flags & MSG_PEEK != 0;
    loop {
        let mut s = u.borrow_mut();
        if s.err != 0 {
            let e = s.err;
            s.err = 0;
            return Err(-e);
        }
        // SO_PEEK_OFF: MSG_PEEK なら、そこから (見たメッセージは飛ばす: Linux の __skb_try_recv_from_queue)
        let mut skip = 0usize;
        let mut at = None;
        if peek && s.peek_off >= 0 {
            let mut off = s.peek_off as usize;
            for (i, m) in s.q.iter().enumerate() {
                if off >= m.len() && (off > 0 || m.peeked) {
                    off -= m.len();
                    continue;
                }
                at = Some(i);
                break;
            }
            skip = off;
        } else if !s.q.is_empty() {
            at = Some(0);
        }
        let Some(i) = at else {
            if nonblock {
                // 待たない SOCK_SEQPACKET は、相手が閉じていれば 0 (EOF)
                if s.typ == SOCK_SEQPACKET && s.shutdown & RCV != 0 {
                    return Ok(Recv::new());
                }
                return Err(-EAGAIN);
            }
            if s.shutdown & RCV != 0 {
                return Ok(Recv::new());
            }
            if s.typ == SOCK_SEQPACKET && s.state != State::Established {
                return Err(-ENOTCONN);
            }
            let timeo = s.rcvtimeo;
            drop(s);
            wait(key(u), timeo)?;
            continue;
        };
        let passcred = s.passcred;
        let from = sender_name(&s.q[i], &s, u);
        let m = &mut s.q[i];
        let len = m.len();
        let size = max.min(len - skip);
        let mut out = Recv::new();
        out.data = m.data[m.used + skip..m.used + skip + size].to_vec();
        if flags & MSG_PARTS != 0 {
            out.parts.push(size);
        }
        if size < len - skip {
            out.flags |= MSG_TRUNC as u32;
        }
        out.ret = if flags & MSG_TRUNC != 0 { len - skip } else { size };
        out.from = from;
        if passcred {
            out.cred = Some(show_cred(m.cred));
        }
        if peek {
            if len == 0 {
                m.peeked = true;
            }
            out.fds = m.fds.clone();
            if s.peek_off >= 0 {
                s.peek_off += size as i32;
            }
            return Ok(out);
        }
        // この行列がいっぱいで待っていた送る人がいるかもしれない (Linux の unix_recvq_full)
        let was_full = s.q.len() > s.backlog;
        let mut m = s.q.remove(i).unwrap();
        out.fds = m.take_fds();
        if s.peek_off >= 0 {
            s.peek_off = (s.peek_off - len as i32).max(0);
        }
        drop(s);
        free([m]);
        if was_full {
            notify(u);
        }
        return Ok(out);
    }
}

/// 読みおえて行列から外したメッセージ (借りるのをやめてから free する)。ふつうは 1 つなので、1 つめはヒープを使わない
#[derive(Default)]
struct Freed {
    one: Option<Msg>,
    more: Vec<Msg>,
}

impl Freed {
    fn push(&mut self, m: Msg) {
        if self.one.is_none() {
            self.one = Some(m);
        } else {
            self.more.push(m);
        }
    }

    fn take(&mut self) -> Freed {
        core::mem::take(self)
    }
}

impl IntoIterator for Freed {
    type Item = Msg;
    type IntoIter = core::iter::Chain<core::option::IntoIter<Msg>, alloc::vec::IntoIter<Msg>>;
    fn into_iter(self) -> Self::IntoIter {
        self.one.into_iter().chain(self.more)
    }
}

/// MSG_OOB の印と、読みおえた長さ 0 のもの (Linux の manage_oob)。i は行列の頭。読むもの (なければ None)
fn manage_oob(s: &mut Unix, mut i: usize, peek: bool, copied: bool, dropped: &mut Freed) -> Option<usize> {
    let is_oob = |s: &Unix, i: usize| Some(s.q[i].id) == s.oob;
    if s.q[i].len() > 0 && !is_oob(s, i) {
        return Some(i);
    }
    if s.q[i].len() == 0 {
        if copied && (s.oob.is_none() || is_oob(s, i)) {
            return None;
        } else if peek {
            i += 1;
        } else {
            dropped.push(s.q.remove(i).unwrap());
        }
        if i >= s.q.len() {
            return None;
        }
    }
    if !is_oob(s, i) {
        return Some(i);
    }
    if copied {
        return None;
    }
    if !peek {
        s.oob = None;
        if !s.oobinline {
            // 読みとばす MSG_OOB の 1 バイト (FIONREAD からも引く: Linux は引かない)
            let m = s.q.remove(i).unwrap();
            s.inq -= m.len();
            dropped.push(m);
            return if s.q.is_empty() { None } else { Some(0) };
        }
    } else if !s.oobinline {
        i += 1;
        if i >= s.q.len() {
            return None;
        }
    }
    Some(i)
}

/// Linux の unix_stream_read_generic
fn stream_recv(u: &UnixRef, max: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    if u.borrow().state != State::Established {
        return Err(crate::why::inval());
    }
    if flags & MSG_OOB != 0 {
        return recv_urg(u, max, flags);
    }
    let peek = flags & MSG_PEEK != 0;
    let target = {
        let s = u.borrow();
        let v = if flags & MSG_WAITALL != 0 { max } else { (s.rcvlowat.max(0) as usize).min(max) };
        v.max(1)
    };
    let mut out = Recv::new();
    let mut size = max;
    let mut err = 0i64;
    // SO_PASSCRED なら、はじめのメッセージの資格情報を見せ、ちがう人のものはつなげない
    let mut creds: Option<Option<Cred>> = None;
    let mut dropped = Freed::default();
    'redo: loop {
        let mut skip = if peek { u.borrow().peek_off.max(0) as usize } else { 0 };
        loop {
            let mut s = u.borrow_mut();
            let mut cur = if s.q.is_empty() { None } else { Some(0) };
            // Linux の again:
            loop {
                if let Some(i) = cur {
                    cur = manage_oob(&mut s, i, peek, !out.data.is_empty(), &mut dropped);
                    if cur.is_none() && (!out.data.is_empty() || max == 0) {
                        break 'redo;
                    }
                }
                let Some(mut i) = cur else {
                    if out.data.len() >= target {
                        break 'redo;
                    }
                    if s.err != 0 {
                        err = s.err;
                        s.err = 0;
                        break 'redo;
                    }
                    if s.shutdown & RCV != 0 {
                        break 'redo;
                    }
                    if nonblock {
                        err = EAGAIN;
                        break 'redo;
                    }
                    let timeo = s.rcvtimeo;
                    // 眠る前に数える (大きなロックなしで入れた人が起こしを頼むように)
                    s.sleepers += 1;
                    drop(s);
                    free(dropped.take());
                    let r = wait(key(u), timeo);
                    u.borrow_mut().sleepers -= 1;
                    if let Err(e) = r {
                        err = -e;
                        break 'redo;
                    }
                    continue 'redo;
                };
                // SO_PEEK_OFF の分を飛ばす
                let mut gone = false;
                while skip >= s.q[i].len() {
                    skip -= s.q[i].len();
                    i += 1;
                    if i >= s.q.len() {
                        gone = true;
                        break;
                    }
                }
                if gone {
                    cur = None;
                    continue;
                }
                let passcred = s.passcred;
                let m = &s.q[i];
                match creds {
                    Some(c) if c != m.cred => break 'redo,
                    None if passcred => creds = Some(m.cred),
                    _ => {}
                }
                out.from = sender_name(m, &s, u);
                let chunk = (m.len() - skip).min(size);
                let from = m.used + skip;
                // まだ何も受けていなくて、メッセージをまるごと読むなら、写さずにその入れ物を渡す (64 KiB ずつの stream の
                // read は、送った人の入れ物 → ユーザーの 2 回だけ写す。Linux の skb → ユーザーと同じ数)。OOB のものは残す
                let whole = !peek && out.data.is_empty() && from == 0 && chunk == m.data.len() && s.oob != Some(m.id);
                if !whole {
                    out.data.extend_from_slice(&m.data[from..from + chunk]);
                }
                if flags & MSG_PARTS != 0 {
                    out.parts.push(chunk);
                }
                size -= chunk;
                if !peek {
                    if s.peek_off >= 0 {
                        s.peek_off = (s.peek_off - chunk as i32).max(0);
                    }
                    s.inq -= chunk;
                    let m = &mut s.q[i];
                    if whole {
                        out.data = core::mem::take(&mut m.data);
                    } else {
                        m.used += chunk;
                    }
                    let got = !m.fds.is_empty();
                    if got {
                        out.fds = m.take_fds();
                    }
                    if m.len() > 0 {
                        break 'redo;
                    }
                    dropped.push(s.q.remove(i).unwrap());
                    if got {
                        break 'redo;
                    }
                    break;
                }
                let got = !s.q[i].fds.is_empty();
                if got {
                    out.fds = s.q[i].fds.clone();
                }
                if s.peek_off >= 0 {
                    s.peek_off += chunk as i32;
                }
                if got {
                    break 'redo;
                }
                skip = 0;
                if i + 1 < s.q.len() {
                    cur = Some(i + 1);
                    continue;
                }
                break 'redo;
            }
            // 1 つ読みおえた (Linux の while (size))
            if size == 0 {
                break 'redo;
            }
        }
    }
    // stream の送り手は自分の wmem で待つので、ここで自分は起こさない (free が送り手を起こす)
    free(dropped);
    if out.data.is_empty() && err != 0 {
        return Err(-err);
    }
    if let Some(c) = creds {
        out.cred = Some(show_cred(c));
    }
    out.ret = out.data.len();
    Ok(out)
}

/// MSG_OOB で受ける (Linux の unix_stream_recv_urg): OOB の 1 バイト。なければ (と SO_OOBINLINE なら) EINVAL。
/// 名前も付帯データもない。入れ物がなければ、読んだうえで EFAULT
fn recv_urg(u: &UnixRef, max: usize, flags: u64) -> Result<Recv, i64> {
    let peek = flags & MSG_PEEK != 0;
    let mut dropped = Freed::default();
    let byte = {
        let mut s = u.borrow_mut();
        let id = match s.oob {
            Some(id) if !s.oobinline => id,
            _ => return Err(crate::why::inval()),
        };
        let Some(mut i) = s.q.iter().position(|m| m.id == id) else { return Err(crate::why::inval()) };
        let byte = s.q[i].data.get(s.q[i].used).copied();
        if !peek {
            s.oob = None;
            s.inq -= 1;
            if i > 0 && s.q[i - 1].len() == 0 {
                dropped.push(s.q.remove(i - 1).unwrap());
                i -= 1;
            }
            s.q[i].used += 1;
        }
        byte
    };
    free(dropped);
    if !peek {
        notify(u);
    }
    if max == 0 {
        return Err(-EFAULT);
    }
    let mut out = Recv::new();
    out.data = byte.map(|b| alloc::vec![b]).unwrap_or_default();
    out.ret = out.data.len();
    out.scm = false;
    out.flags = MSG_OOB as u32;
    Ok(out)
}

// ---- 大きなロックなしの read / write ----

/// fast_rw の数 (/proc/bkl): 書いた、読んだ、ふつうの道へ
pub static FAST: [core::sync::atomic::AtomicU64; 3] = [const { core::sync::atomic::AtomicU64::new(0) }; 3];

/// 大きなロックなしの read / write (syscall::fast): つないだ SOCK_STREAM の、fd も資格情報も MSG_OOB もないバイトで、
/// 眠らずにすむときだけ。ほかは None (ふつうの道へ: 送れる余地がない、読むものが SO_RCVLOWAT より少ない、SO_PASSCRED、
/// 相手がまだ accept されていない、閉じた、エラーがある、ユーザーのメモリが写っていない ...)。答えはふつうの道と同じ
pub fn fast_rw(me: &proc::Proc, fd: u64, buf: usize, len: usize, write: bool) -> Option<i64> {
    use core::sync::atomic::Ordering::Relaxed;
    let r = fast_rw_inner(me, fd, buf, len, write);
    FAST[if r.is_none() { 2 } else { write as usize }].fetch_add(1, Relaxed);
    r
}

fn fast_rw_inner(me: &proc::Proc, fd: u64, buf: usize, len: usize, write: bool) -> Option<i64> {
    if len == 0 || len > 3776 + 32768 || crate::fail::ON.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let files = me.files.as_ref()?;
    // ほかのスレッドと分けている表なら門を通って (file::Pipe::fast_rw と同じ)
    let _gate = if files.private() { None } else { Some(files.get().gate.read()?) };
    let f = files.get().get(fd)?;
    // ほかのプロセスが大きなロックの中で借りているかもしれないので、借りずに見る (口の種類はあとで変わらない)
    let of = unsafe { &*f.as_ptr() };
    let Kind::Unix(u) = &of.kind else { return None };
    if write { fast_send(me, u, buf, len) } else { fast_recv(me, u, buf, len) }
}

/// 起こしを頼むか (眠っている人、poll の人、epoll)
fn waited(s: &Unix) -> bool {
    s.sleepers > 0 || s.watchers > 0 || s.epolled
}

fn fast_send(me: &proc::Proc, u: &UnixRef, buf: usize, len: usize) -> Option<i64> {
    // 写すのはロックの前に (ページがなければ、何も変えずにふつうの道へ)
    let mut data = crate::heap::try_vec(len, false).ok()?;
    if !me.pt().copy_in_nofault(buf, &mut data) {
        return None;
    }
    // 自分: 送れるか (stream_send と wait_wmem と add_creds の見るもの)。送るものの大きさを先に wmem に足しておく
    // (相手が大きなロックなしで読んで返すのが、足すより先にならないように)
    let (peer, ts) = {
        let mut s = u.borrow_mut();
        if s.typ != SOCK_STREAM || s.state != State::Established || s.err != 0 || s.shutdown & SEND != 0 || s.passcred || s.wmem >= s.sndbuf || stream_chunk(s.sndbuf, len) != len {
            return None;
        }
        let peer = s.peer.clone()?;
        let ts = stream_truesize(len);
        s.wmem += ts;
        (peer, ts)
    };
    // 相手: 受けられるか (deliver と add_creds の見るもの)。こちらにつないでいる相手だけ (行列のものの送り手は相手の peer)
    let wake = {
        let mut o = peer.borrow_mut();
        if o.dead || o.shutdown & RCV != 0 || o.passcred || !o.has_file || !same(&o.peer, u) {
            None
        } else {
            o.q.push_back(Msg::plain(data, u, ts));
            o.inq += len;
            o.generation += 1;
            Some(waited(&o))
        }
    };
    let Some(wake) = wake else {
        // 足した分をもどす。そのあいだに wmem がいっぱいに見えて眠った人がいれば起こしを頼む
        let w = {
            let mut s = u.borrow_mut();
            s.wmem -= ts;
            waited(&s)
        };
        if w {
            crate::smp::defer_wake(key(u));
        }
        return None;
    };
    if wake {
        crate::smp::defer_wake(key(&peer));
    }
    Some(len as i64)
}

fn fast_recv(me: &proc::Proc, u: &UnixRef, buf: usize, len: usize) -> Option<i64> {
    let mut tmp = crate::heap::try_vec(len, false).ok()?;
    let (gone, n) = {
        let mut s = u.borrow_mut();
        if s.typ != SOCK_STREAM || s.state != State::Established || s.passcred || s.oob.is_some() {
            return None;
        }
        // 読むだけあるか (stream_recv の target)。読むメッセージはどれも fd のないもので、相手が送ったもの (それを落としても
        // 送り手の最後の参照にはならない: こちらの peer が持っている)。fd のあるものに届くなら、ふつうの道 (そこまで読む)
        let target = (s.rcvlowat.max(0) as usize).min(len).max(1);
        if s.inq < target {
            return None;
        }
        let mut n = 0;
        for m in &s.q {
            if n == len {
                break;
            }
            if !m.fds.is_empty() || m.len() == 0 || !m.sender.as_ref().is_some_and(|x| same(&s.peer, x)) {
                return None;
            }
            let k = m.len().min(len - n);
            tmp[n..n + k].copy_from_slice(&m.data[m.used..m.used + k]);
            n += k;
        }
        if n < target || !me.pt().copy_out_nofault(buf, &tmp[..n]) {
            return None;
        }
        // 写せたので、読んだところを外す
        let mut gone = Vec::new();
        let mut left = n;
        while left > 0 {
            let m = &mut s.q[0];
            let k = m.len().min(left);
            m.used += k;
            left -= k;
            if m.len() == 0 {
                gone.push(s.q.pop_front().unwrap());
            }
        }
        s.inq -= n;
        if s.peek_off >= 0 {
            s.peek_off = (s.peek_off - n as i32).max(0);
        }
        (gone, n)
    };
    for mut m in gone {
        // free と同じ: 送った口の wmem を返す。待っていた (いっぱいだった) なら起こしを頼む
        if let Some(sd) = m.sender.take() {
            let wake = {
                let mut b = sd.borrow_mut();
                let before = b.wmem;
                b.wmem = before.saturating_sub(m.truesize);
                let full = before >= b.sndbuf || (before + 1) * 4 > b.sndbuf;
                if full {
                    b.generation += 1;
                }
                full && waited(&b)
            };
            if wake {
                crate::smp::defer_wake(key(&sd));
            }
        }
    }
    Some(n as i64)
}

/// read: 0 バイトはなにもせずに 0 (Linux の「SYS5 の決まり」)
pub fn read(u: &UnixRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if dst.is_empty() {
        return Ok(0);
    }
    let r = recv(u, dst.len(), 0, nonblock)?;
    dst[..r.data.len()].copy_from_slice(&r.data);
    Ok(r.data.len())
}

/// write (SOCK_SEQPACKET の MSG_EOR は unix では見ない)
pub fn write(u: &UnixRef, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    send(u, Src::Kernel(src), src.len(), None, &[], 0, nonblock).map(|n| n as usize)
}

// ---- shutdown ----

pub fn shutdown(u: &UnixRef, how: u64) -> R {
    if how > 2 {
        return Err(crate::why::inval());
    }
    let mode = how as u8 + 1;
    let (peer, typ) = {
        let mut s = u.borrow_mut();
        s.shutdown |= mode;
        (s.peer.clone(), s.typ)
    };
    notify(u);
    if let Some(p) = peer {
        if typ == SOCK_STREAM || typ == SOCK_SEQPACKET {
            let mut pm = 0;
            if mode & RCV != 0 {
                pm |= SEND;
            }
            if mode & SEND != 0 {
                pm |= RCV;
            }
            p.borrow_mut().shutdown |= pm;
            notify(&p);
        }
    }
    Ok(0)
}

// ---- 閉じる ----

/// 最後の fd が閉じた: Linux の unix_release_sock
pub fn release(u: &UnixRef) {
    release_sock(u, false);
    unsafe { GC_WANTED = true };
}

// ---- gc (Linux の net/unix/garbage.c) ----

/// fd の表からソケットの fd が外れた (閉じた、上書きした)
pub fn closed() {
    unsafe { GC_WANTED = true };
}

/// システムコールの終わり (と、プロセスが終わって fd の表を落としたとき: force) に: 送られている途中のソケットが
/// あれば gc
#[inline]
pub fn gc_check(force: bool) {
    unsafe {
        if !(GC_WANTED || force) {
            return;
        }
        GC_WANTED = false;
        if INFLIGHT == 0 {
            return;
        }
    }
    gc();
}

/// aios がえらんだちがい (unix-gc-now): ソケットの fd を閉じたシステムコールの終わりに、すぐ見る (gc_check)。Linux は
/// ほかの AF_UNIX のソケットが閉じたときに、うしろ (kworker) で片づけるので、それまで輪の中のソケットは生きている。
/// メッセージの中にしかない (どの fd の表からもたどれない) ソケットの輪を片づける: 自分の fd を自分の行列に入れた
/// ソケットや、たがいの fd を持ちあうもの。ソケットの fd (OpenFile) の参照がみなメッセージの中のもの (候補) で、
/// 候補でないソケットの行列からも、そこからたどれる候補の行列からも持たれていないものの行列を捨てる。listen している
/// ものの行列には、まだ accept されていない口の行列も入れる (Linux の scan_children)
fn gc() {
    let all = unsafe { &mut *(&raw mut ALL) };
    all.retain(|w| w.strong_count() > 0);
    unsafe { ALL_LIVE = all.len() };
    let socks: Vec<UnixRef> = all.iter().filter_map(|w| w.upgrade()).collect();
    // ソケット s の行列 (と accept 待ちの口の行列) にある AF_UNIX の fd: (OpenFile の場所, その中のソケット)
    let held = |s: &UnixRef| -> Vec<(usize, UnixRef)> {
        let b = s.borrow();
        let mut v = Vec::new();
        let mut scan = |q: &VecDeque<Msg>| {
            for m in q {
                for f in &m.fds {
                    if let Kind::Unix(t) = &f.borrow().kind {
                        v.push((Rc::as_ptr(f) as usize, t.clone()));
                    }
                }
            }
        };
        scan(&b.q);
        for e in &b.embryos {
            scan(&e.borrow().q);
        }
        v
    };
    // fd ごとの、メッセージの中の数と、ぜんぶの参照の数
    let mut refs: BTreeMap<usize, (usize, usize, UnixRef)> = BTreeMap::new();
    for s in &socks {
        if !s.borrow().has_file {
            continue;
        }
        let b = s.borrow();
        let mut scan = |q: &VecDeque<Msg>| {
            for m in q {
                for f in &m.fds {
                    if let Kind::Unix(t) = &f.borrow().kind {
                        let e = refs.entry(Rc::as_ptr(f) as usize).or_insert((0, Rc::strong_count(f), t.clone()));
                        e.0 += 1;
                    }
                }
            }
        };
        scan(&b.q);
        for e in &b.embryos {
            scan(&e.borrow().q);
        }
    }
    // 候補: 参照がみなメッセージの中のもの
    let cand: BTreeMap<usize, UnixRef> = refs.iter().filter(|(_, (n, all, _))| n == all).map(|(&k, (_, _, t))| (k, t.clone())).collect();
    if cand.is_empty() {
        return;
    }
    let mut from_cand: BTreeMap<usize, usize> = BTreeMap::new();
    for t in cand.values() {
        for (k, _) in held(t) {
            *from_cand.entry(k).or_insert(0) += 1;
        }
    }
    // 候補でないものから持たれている候補と、そこからたどれる候補は生きている
    let mut live: Vec<usize> = cand.keys().copied().filter(|k| refs[k].0 > from_cand.get(k).copied().unwrap_or(0)).collect();
    let mut seen: alloc::collections::BTreeSet<usize> = live.iter().copied().collect();
    while let Some(k) = live.pop() {
        for (c, _) in held(&cand[&k]) {
            if cand.contains_key(&c) && seen.insert(c) {
                live.push(c);
            }
        }
    }
    // のこりの候補の行列を捨てる (fd が落ち、そのソケットが閉じる)
    let mut drop_msgs: Vec<Msg> = Vec::new();
    for (k, t) in &cand {
        if seen.contains(k) {
            continue;
        }
        let mut b = t.borrow_mut();
        drop_msgs.extend(b.q.drain(..));
        b.inq = 0;
        b.oob = None;
        let es: Vec<UnixRef> = b.embryos.iter().cloned().collect();
        drop(b);
        for e in es {
            drop_msgs.extend(e.borrow_mut().q.drain(..));
        }
    }
    drop(socks);
    free(drop_msgs);
}

/// embryo: accept されずに、listen しているものが閉じた口 (相手には ECONNRESET)
fn release_sock(u: &UnixRef, embryo: bool) {
    let (peer, msgs, embryos, was, typ) = {
        let mut s = u.borrow_mut();
        if s.dead {
            return;
        }
        // 名前の表から外す (自分のものなら)
        if let Some(a) = &s.addr {
            if a.len() > 2 && a[2] == 0 {
                let k = (s.net, s.typ, (**a).clone());
                if abstract_names().get(&k).is_some_and(|w| is(w, u)) {
                    abstract_names().remove(&k);
                }
            }
        }
        if let Some(i) = &s.inode {
            let id = i.id();
            if bsd_names().get(&id).is_some_and(|w| is(w, u)) {
                bsd_names().remove(&id);
            }
        }
        s.dead = true;
        s.shutdown = BOTH;
        let was = s.state;
        s.state = State::Close;
        s.oob = None;
        s.inode = None;
        let peer = s.peer.take();
        // 読まれていないものがあったか (頭の長さ 0 のもの (読んだ MSG_OOB の印) はとばして、その次を見る)
        let unread = match s.q.front() {
            None => false,
            Some(m) if m.len() == 0 => s.q.len() > 1,
            Some(_) => true,
        };
        let msgs: Vec<Msg> = s.q.drain(..).collect();
        let embryos: Vec<UnixRef> = s.embryos.drain(..).collect();
        s.inq = 0;
        (peer.map(|p| (p, unread)), msgs, embryos, was, s.typ)
    };
    if let Some((p, unread)) = peer {
        if typ == SOCK_STREAM || typ == SOCK_SEQPACKET {
            let mut ps = p.borrow_mut();
            ps.shutdown = BOTH;
            if unread || embryo {
                ps.err = ECONNRESET;
            }
        }
        notify(&p);
    }
    free(msgs);
    if was == State::Listen {
        for e in embryos {
            release_sock(&e, true);
        }
    }
    notify(u);
}

// ---- 様子 ----

/// poll のビット (Linux の unix_poll と unix_dgram_poll)
pub fn poll_mask(u: &UnixRef) -> u32 {
    let s = u.borrow();
    let mut m = 0;
    let writable = s.state != State::Listen && (s.wmem + 1) * 4 <= s.sndbuf;
    let readable = !s.q.is_empty() || !s.embryos.is_empty();
    if s.err != 0 {
        m |= POLLERR;
    }
    if s.shutdown == BOTH {
        m |= POLLHUP;
    }
    if s.shutdown & RCV != 0 {
        m |= POLLRDHUP | POLLIN | POLLRDNORM;
    }
    if readable {
        m |= POLLIN | POLLRDNORM;
    }
    if s.typ == SOCK_STREAM {
        if s.oob.is_some() {
            m |= POLLPRI;
        }
        if s.state == State::Close {
            m |= POLLHUP;
        }
        if writable {
            m |= POLLOUT | POLLWRNORM | POLLWRBAND;
        }
        return m;
    }
    if s.typ == SOCK_SEQPACKET && s.state == State::Close {
        m |= POLLHUP;
    }
    let mut w = writable;
    if w {
        // dgram: つないだ相手がこちらにつないでいなくて、その行列がいっぱいなら書けない
        if let Some(o) = &s.peer {
            if !Arc::ptr_eq(o, u) {
                let ob = o.borrow();
                if !same(&ob.peer, u) && ob.q.len() > ob.backlog {
                    w = false;
                }
            }
        }
    }
    if w {
        m |= POLLOUT | POLLWRNORM | POLLWRBAND;
    }
    m
}

/// poll で待つときの印: 自分と、つないだ相手 (dgram はその行列があくと書ける)
pub fn poll_keys(u: &UnixRef) -> Vec<usize> {
    let mut v = alloc::vec![key(u)];
    if let Some(p) = &u.borrow().peer {
        v.push(key(p));
    }
    v
}

/// FIONREAD (SIOCINQ): stream は読まれていないバイト、seqpacket はメッセージの和、dgram ははじめのメッセージ。
/// listen しているものは EINVAL
pub fn inq(u: &UnixRef) -> Result<usize, i64> {
    let s = u.borrow();
    if s.state == State::Listen {
        return Err(crate::why::inval());
    }
    Ok(match s.typ {
        SOCK_STREAM => s.inq,
        SOCK_SEQPACKET => s.q.iter().map(|m| m.len()).sum(),
        _ => s.q.front().map_or(0, |m| m.data.len()),
    })
}

/// SIOCOUTQ: 送って読まれていない大きさ
pub fn outq(u: &UnixRef) -> usize {
    u.borrow().wmem
}

/// SIOCATMARK: 次に読むところが MSG_OOB の印か (stream だけ)
pub fn atmark(u: &UnixRef) -> Result<bool, i64> {
    let s = u.borrow();
    if s.typ != SOCK_STREAM {
        return Err(-EOPNOTSUPP);
    }
    let Some(first) = s.q.front() else { return Ok(false) };
    let next = s.q.get(1).map(|m| m.id);
    Ok(Some(first.id) == s.oob || (first.len() == 0 && (s.oob.is_none() || next == s.oob)))
}

/// /proc/ai/fd などの、読まれずに残っているバイト
pub fn pending(u: &UnixRef) -> u64 {
    u.borrow().q.iter().map(|m| m.len() as u64).sum()
}

pub fn stat(u: &UnixRef) -> Stat {
    let s = u.borrow();
    Stat { ino: s.ino, uid: s.uid, gid: s.gid, mtime: s.mtime, ctime: s.ctime, ..Stat::dev(0o140000 | s.mode, 0) }
}

/// utimensat: mtime を変え (None はそのまま)、ctime はいまに
pub fn touch(u: &UnixRef, mtime: Option<u64>) {
    let mut s = u.borrow_mut();
    if let Some(t) = mtime {
        s.mtime = t;
    }
    s.ctime = crate::timer::epoch_ns();
}

/// fcntl の O_NOATIME などで見る持ち主
pub fn owner(u: &UnixRef) -> u32 {
    u.borrow().uid
}

/// fchmod: 持ち主か root (Linux の sockfs の inode)
pub fn chmod(u: &UnixRef, mode: u32) -> R {
    let c = proc::current_cred_ref();
    let mut s = u.borrow_mut();
    if c.euid != 0 && c.euid != s.uid {
        return Err(-EPERM);
    }
    s.mode = mode & 0o7777;
    Ok(0)
}

/// fchown のための (uid, gid, 許可)
pub fn owner_mode(u: &UnixRef) -> (u32, u32, u32) {
    let s = u.borrow();
    (s.uid, s.gid, s.mode)
}

pub fn set_owner_mode(u: &UnixRef, uid: Option<u32>, gid: Option<u32>, mode: u32) {
    let mut s = u.borrow_mut();
    if let Some(n) = uid {
        s.uid = n;
    }
    if let Some(g) = gid {
        s.gid = g;
    }
    s.mode = mode & 0o7777;
}

pub fn describe(u: &UnixRef) -> String {
    alloc::format!("socket:[{}]", u.borrow().ino)
}

pub fn debug(u: &UnixRef) -> String {
    let s = u.borrow();
    let st = match s.state {
        State::Close => "close",
        State::Listen => "listen",
        State::Established => "established",
    };
    let name = s.addr.as_ref().map(|a| String::from_utf8_lossy(&a[2..]).replace('\0', "@")).unwrap_or_default();
    alloc::format!(
        "unix type {} {} name {:?} queue {} ({} bytes) embryos {} wmem {} sndbuf {} shutdown {} err {}{}",
        s.typ,
        st,
        name,
        s.q.len(),
        s.q.iter().map(|m| m.len()).sum::<usize>(),
        s.embryos.len(),
        s.wmem,
        s.sndbuf,
        s.shutdown,
        s.err,
        if s.peer.is_some() { " peer" } else { "" }
    )
}

/// /proc/net/unix (Linux の unix_seq_show と同じ形: Num RefCount Protocol Flags Type St Inode Path)。Num は Linux も
/// ふつうの人には 0 (kptr_restrict)。Flags の 00010000 は listen しているもの、St は 01 がつながっていない、03 が
/// つながっている。抽象名前空間の名前は @ から (NUL は @)。accept されていない口も出す (名前は listen しているもの)
pub fn proc_net() -> String {
    let mut out = String::from("Num       RefCount Protocol Flags    Type St Inode Path\n");
    let all = unsafe { &*(&raw const ALL) };
    for w in all {
        let Some(u) = w.upgrade() else { continue };
        let s = u.borrow();
        if s.dead {
            continue;
        }
        let flags = if s.state == State::Listen { 0x10000 } else { 0 };
        let st = if s.state == State::Established && s.peer.is_some() { 3 } else { 1 };
        let path = match &s.addr {
            Some(a) if a.len() > 2 && a[2] == 0 => alloc::format!(" @{}", String::from_utf8_lossy(&a[3..]).replace('\0', "@")),
            Some(a) if a.len() > 2 => alloc::format!(" {}", String::from_utf8_lossy(&a[2..a.len() - 1])),
            _ => String::new(),
        };
        out.push_str(&alloc::format!("{:016x}: {:08X} {:08X} {:08X} {:04X} {:02X} {:5}{}\n", 0, w.strong_count() + 1, 0, flags, s.typ, st, s.ino, path));
    }
    out
}

// ---- getsockopt / setsockopt ----

const SO_DEBUG: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_DONTROUTE: i32 = 5;
const SO_BROADCAST: i32 = 6;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_KEEPALIVE: i32 = 9;
const SO_OOBINLINE: i32 = 10;
const SO_PRIORITY: i32 = 12;
const SO_LINGER: i32 = 13;
const SO_REUSEPORT: i32 = 15;
const SO_PASSCRED: i32 = 16;
const SO_PEERCRED: i32 = 17;
const SO_RCVLOWAT: i32 = 18;
const SO_SNDLOWAT: i32 = 19;
const SO_RCVTIMEO_OLD: i32 = 20;
const SO_SNDTIMEO_OLD: i32 = 21;
const SO_ACCEPTCONN: i32 = 30;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;
const SO_PEEK_OFF: i32 = 42;
const SO_RCVTIMEO_NEW: i32 = 66;
const SO_SNDTIMEO_NEW: i32 = 67;
const SO_PASSRIGHTS: i32 = 83;

fn timeval(ns: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
    b.extend_from_slice(&((ns % 1_000_000_000 / 1000) as i64).to_le_bytes());
    b
}

/// getsockopt (len は入れ物の長さ)。答えのバイト (長さは min(len, 答えの長さ): Linux の sk_getsockopt)
pub fn getsockopt(u: &UnixRef, level: i32, opt: i32, len: i32) -> Result<Vec<u8>, i64> {
    if level != SOL_SOCKET {
        return Err(-EOPNOTSUPP);
    }
    if len < 0 {
        return Err(crate::why::inval());
    }
    let mut s = u.borrow_mut();
    let int = |v: i64| (v as i32).to_le_bytes().to_vec();
    let mut v = match opt {
        SO_DEBUG => int(0),
        SO_BROADCAST => int(s.broadcast as i64),
        SO_REUSEADDR => int(s.reuse as i64),
        SO_REUSEPORT => int(0),
        SO_KEEPALIVE => int(s.keepalive as i64),
        SO_OOBINLINE => int(s.oobinline as i64),
        SO_DONTROUTE => int(s.dontroute as i64),
        SO_SNDBUF => int(s.sndbuf as i64),
        SO_RCVBUF => int(s.rcvbuf as i64),
        SO_TYPE => int(s.typ as i64),
        SO_PROTOCOL => int(0),
        SO_DOMAIN => int(1),
        SO_ERROR => {
            let e = s.err;
            s.err = 0;
            int(e)
        }
        SO_PRIORITY => int(s.priority as i64),
        SO_LINGER => {
            let mut b = int(s.linger as i64);
            b.extend_from_slice(&int(s.linger_secs.min(i32::MAX as u64) as i64));
            b
        }
        SO_RCVLOWAT => int(s.rcvlowat as i64),
        SO_SNDLOWAT => int(1),
        SO_PASSCRED => int(s.passcred as i64),
        SO_PASSRIGHTS => int(s.passrights as i64),
        SO_ACCEPTCONN => int((s.state == State::Listen) as i64),
        SO_PEEK_OFF => int(s.peek_off as i64),
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => timeval(s.rcvtimeo),
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => timeval(s.sndtimeo),
        SO_PEERCRED => {
            // 相手がいないものは pid 0、uid と gid は -1 (Linux の cred_to_ucred)
            let (pid, uid, gid) = match s.peercred {
                Some((p, u, g)) => (crate::ns::local_or_0(p), crate::ns::show_uid(u), crate::ns::show_gid(g)),
                None => (0, u32::MAX, u32::MAX),
            };
            let mut b = Vec::with_capacity(12);
            b.extend_from_slice(&pid.to_le_bytes());
            b.extend_from_slice(&uid.to_le_bytes());
            b.extend_from_slice(&gid.to_le_bytes());
            b
        }
        _ => return Err(-ENOPROTOOPT),
    };
    v.truncate(len as usize);
    Ok(v)
}

pub fn setsockopt(u: &UnixRef, level: i32, opt: i32, val: &[u8]) -> R {
    if level != SOL_SOCKET {
        return Err(-EOPNOTSUPP);
    }
    if val.len() < 4 {
        return Err(crate::why::inval());
    }
    let v = i32::from_le_bytes(val[..4].try_into().unwrap());
    let b = v != 0;
    let to_ns = |val: &[u8]| -> Result<u64, i64> {
        if val.len() < 16 {
            return Err(crate::why::inval());
        }
        let sec = i64::from_le_bytes(val[..8].try_into().unwrap());
        let usec = i64::from_le_bytes(val[8..16].try_into().unwrap());
        if !(0..1_000_000).contains(&usec) {
            return Err(-EDOM);
        }
        if sec < 0 {
            return Ok(1);
        }
        Ok((sec as u64).saturating_mul(1_000_000_000) + usec as u64 * 1000)
    };
    {
        let mut s = u.borrow_mut();
        match opt {
            SO_PEEK_OFF => s.peek_off = v,
            SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => s.rcvtimeo = to_ns(val)?,
            SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => s.sndtimeo = to_ns(val)?,
            SO_PRIORITY => {
                if v > 6 && proc::current_cred_ref().euid != 0 {
                    return Err(-EPERM);
                }
                s.priority = v as u32;
            }
            SO_DEBUG => {
                if b && proc::current_cred_ref().euid != 0 {
                    return Err(-EACCES);
                }
            }
            SO_REUSEADDR => s.reuse = b,
            // SO_REUSEPORT は AF_INET だけ (0 はよい: Linux の sk_setsockopt)
            SO_REUSEPORT => {
                if b {
                    return Err(-EOPNOTSUPP);
                }
            }
            SO_DONTROUTE => s.dontroute = b,
            SO_BROADCAST => s.broadcast = b,
            SO_SNDBUF => {
                let n = (v as u32 as usize).min(BUF_MAX).min(i32::MAX as usize / 2);
                s.sndbuf = (n * 2).max(MIN_SNDBUF);
            }
            SO_RCVBUF => {
                let n = (v as u32 as usize).min(BUF_MAX).min(i32::MAX as usize / 2);
                s.rcvbuf = (n * 2).max(MIN_RCVBUF);
            }
            SO_KEEPALIVE => s.keepalive = b,
            SO_OOBINLINE => s.oobinline = b,
            SO_LINGER => {
                if val.len() < 8 {
                    return Err(crate::why::inval());
                }
                s.linger = b;
                if b {
                    // l_linger は unsigned long にして秒
                    s.linger_secs = i32::from_le_bytes(val[4..8].try_into().unwrap()) as i64 as u64;
                }
            }
            SO_RCVLOWAT => s.rcvlowat = if v < 0 { i32::MAX } else if v == 0 { 1 } else { v },
            SO_PASSCRED => s.passcred = b,
            SO_PASSRIGHTS => s.passrights = b,
            _ => return Err(-ENOPROTOOPT),
        }
    }
    // 入れ物が大きくなったら、待っている書く人を起こす
    notify(u);
    Ok(0)
}

// ---- 受けた fd と資格情報を付帯データに (Linux の scm_recv_unix) ----

/// 付帯データ: 入れ物の中のどこに何を書くか (資格情報と fd の 2 つまで)、使った長さ (msg_controllen に返すもの)、
/// msg_flags (MSG_CTRUNC)
pub struct ScmOut {
    pub writes: [Option<(usize, Vec<u8>)>; 2],
    pub used: usize,
    pub flags: u32,
}

/// cmsghdr の頭 (cmsg_len、cmsg_level、cmsg_type) を b の先に書く
fn cmsg_head(b: &mut [u8], len: usize, typ: i32) {
    b[..8].copy_from_slice(&(len as u64).to_le_bytes());
    b[8..12].copy_from_slice(&SOL_SOCKET.to_le_bytes());
    b[12..16].copy_from_slice(&typ.to_le_bytes());
}

/// space は入れ物の長さ (msg_control が NULL なら None)。fd はこのプロセスの fd にする (入らなかったものは閉じる)
pub fn scm_out(r: &mut Recv, passcred: bool, space: Option<usize>, cloexec: bool) -> ScmOut {
    let mut out = ScmOut { writes: [None, None], used: 0, flags: 0 };
    let fds = core::mem::take(&mut r.fds);
    if !r.scm {
        return out;
    }
    let Some(room) = space else {
        if passcred || !fds.is_empty() {
            out.flags |= MSG_CTRUNC;
        }
        return out;
    };
    let mut left = room;
    // SCM_CREDENTIALS (Linux の put_cmsg: 入らなければ切って MSG_CTRUNC)
    if passcred {
        let (pid, uid, gid) = r.cred.unwrap_or((0, OVERFLOW_ID, OVERFLOW_ID));
        if left < 16 {
            out.flags |= MSG_CTRUNC;
        } else {
            let full = 16 + 12;
            let cmlen = full.min(left);
            if left < full {
                out.flags |= MSG_CTRUNC;
            }
            let mut b = alloc::vec![0u8; full];
            cmsg_head(&mut b, cmlen, SCM_CREDENTIALS);
            b[16..20].copy_from_slice(&pid.to_le_bytes());
            b[20..24].copy_from_slice(&uid.to_le_bytes());
            b[24..28].copy_from_slice(&gid.to_le_bytes());
            b.truncate(cmlen);
            out.writes[0] = Some((out.used, b));
            let step = 32.min(left);
            out.used += step;
            left -= step;
        }
    }
    // SCM_RIGHTS (Linux の scm_detach_fds: 入るだけ、fd の表がいっぱいならそこまで)
    if !fds.is_empty() {
        let fdmax = if left <= 16 { 0 } else { (left - 16) / 4 };
        let mut b = Vec::with_capacity(16 + 4 * fdmax.min(fds.len()));
        b.resize(16, 0);
        let files = proc::current().files();
        for f in fds.iter().take(fdmax) {
            match files.add(f.clone(), cloexec, 0) {
                Some(fd) => b.extend_from_slice(&(fd as i32).to_le_bytes()),
                None => break,
            }
        }
        let n = (b.len() - 16) / 4;
        if n > 0 {
            let cmlen = b.len();
            cmsg_head(&mut b, cmlen, SCM_RIGHTS);
            out.writes[1] = Some((out.used, b));
            out.used += ((cmlen + 7) & !7).min(left);
        }
        if n < fds.len() || fdmax == 0 {
            out.flags |= MSG_CTRUNC;
        }
    }
    out
}
