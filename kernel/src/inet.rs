// AF_INET の TCP と UDP。答え (返す数、エラー、poll のビット、名前、sockopt) は Linux 7.3 の net/ipv4 (af_inet.c、
// tcp.c、tcp_input.c、tcp_ipv4.c、tcp_minisocks.c、udp.c、datagram.c、inet_connection_sock.c、ip_sockglue.c) と同じ。
//
// Linux とちがうところ: aios の中どうし (127.0.0.0/8、eth0 のアドレス、0.0.0.0 あて) はパケットを作らない。TCP は
// つないだ相手の受ける行列に送ったものをそのまま入れ、UDP は届け先のソケットの行列に入れる (Linux は lo をとおして
// TCP をまるごと動かす: 3 回のあいさつ、ACK、窓、再送のタイマ、Nagle、遅らせた ACK)。見えるもの (状態、エラー、poll、
// FIN と RST の届きかた、TIME_WAIT、listen の行列があふれたときの待ち) は同じにして、時間しだいのもの (Nagle と
// 遅らせた ACK で小さな書きこみがたまる) はなくす。外 (eth0 の向こう) の相手だけ smoltcp で (net.rs)
//
// - ポートの表: bind のぶつかり (SO_REUSEADDR、SO_REUSEPORT、listen しているもの、TIME_WAIT)、ポート 0 は
//   32768〜60999 から (Linux の ip_local_port_range)
// - TCP: listen の行列は backlog + 1 まで (あふれた connect は SYN_SENT のまま、1、2、4 … 秒ごとにやりなおす)。
//   backlog 0 の listen は SYN cookie でつながる (受けた側の MSS が cookie の表のもの)。MSG_OOB は最後の 1 バイトが
//   urgent (SIOCATMARK、SO_OOBINLINE)。閉じたら FIN (読んでいないものがあれば RST)、閉じたあとも FIN_WAIT2 /
//   TIME_WAIT で 60 秒残る (ポートを持ち、そのあいだ相手が送ったら RST)
// - UDP: connect した相手へ届かない (ポートにだれもいない) と次に ECONNREFUSED (Linux の ICMP)、MSG_MORE でためる、
//   IP_RECVERR のエラーの行列
//
// 大きなロックの中で動く (Rc と RefCell)。借りたまま眠らない、2 つのソケットを同時に借りない (自分につないだものは
// 相手が自分)
use crate::cell::Bkl;
use crate::file::{self, FileRef, Kind, Stat};
use crate::net;
use crate::proc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

const EPERM: i64 = 1;
const EAGAIN: i64 = 11;
const EACCES: i64 = 13;
const EFAULT: i64 = 14;
const EPIPE: i64 = 32;
const EDOM: i64 = 33;
const EDESTADDRREQ: i64 = 89;
const EMSGSIZE: i64 = 90;
const ENOPROTOOPT: i64 = 92;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EADDRINUSE: i64 = 98;
const EADDRNOTAVAIL: i64 = 99;
const ENETUNREACH: i64 = 101;
const ECONNABORTED: i64 = 103;
const ECONNRESET: i64 = 104;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ECONNREFUSED: i64 = 111;
const EALREADY: i64 = 114;
const EINPROGRESS: i64 = 115;

pub const MSG_OOB: u64 = 0x1;
pub const MSG_PEEK: u64 = 0x2;
pub const MSG_TRUNC: u64 = 0x20;
pub const MSG_DONTWAIT: u64 = 0x40;
const MSG_WAITALL: u64 = 0x100;
const MSG_ERRQUEUE: u64 = 0x2000;
pub const MSG_NOSIGNAL: u64 = 0x4000;
const MSG_MORE: u64 = 0x8000;

use crate::unix::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI, POLLRDHUP, POLLRDNORM, POLLWRBAND, POLLWRNORM};

const AF_UNSPEC: u16 = 0;
const AF_INET: u16 = 2;
const LOOPBACK: u32 = 0x7f00_0001;

/// shutdown のビット (Linux の RCV_SHUTDOWN と SEND_SHUTDOWN)
const RCV: u8 = 1;
const SEND: u8 = 2;
const BOTH: u8 = 3;

/// TCP の入れ物の既定 (Linux の net.ipv4.tcp_rmem[1] / tcp_wmem[1])、UDP の (net.core.rmem_default / wmem_default)、
/// setsockopt で決められる上限 (net.core.rmem_max / wmem_max) と下限 (SOCK_MIN_SNDBUF / SOCK_MIN_RCVBUF)
const TCP_RCVBUF: usize = 131072;
const TCP_SNDBUF: usize = 16384;
const UDP_BUF: usize = 212992;
const BUF_MAX: usize = 4 << 20;
const MIN_SNDBUF: usize = 4608;
const MIN_RCVBUF: usize = 2304;
/// TCP_MAXSEG: つなぐ前 (TCP_MSS_DEFAULT)。ループバックで知らせる MSS (IPv4 の 65535 から IP と TCP の頭)。つないだ
/// ものは、相手の知らせたもの (TCP_MAXSEG があればそれまで) からタイムスタンプの 12 を引き、相手の窓の半分まで
/// (Linux の tcp_bound_to_half_wnd: 測ったもの。connect した側は 65483 の半分、受けた側と自分につないだものは 65536 の半分)
const MSS_DEFAULT: i32 = 536;
const ADV_MSS: i32 = 65495;
const HALF_WND_CLIENT: i32 = 32741;
const HALF_WND_SERVER: i32 = 32768;
/// TCP の受ける行列にためておける大きさ (送る側はこれより多くは書けない: 待つ / EAGAIN)。SO_RCVBUF を決めたものは
/// その大きさ
const TCP_QUEUE: usize = 4 << 20;
/// ポート 0 のときに選ぶところ (net.ipv4.ip_local_port_range) と、root でなければ bind できないもの
/// (net.ipv4.ip_unprivileged_port_start)
const PORT_LO: u16 = 32768;
const PORT_HI: u16 = 60999;
const PRIV_PORT: u16 = 1024;
pub const SOMAXCONN: u32 = 4096;
/// SYN をやりなおす数 (net.ipv4.tcp_syn_retries)、FIN_WAIT2 と TIME_WAIT の長さ (tcp_fin_timeout、TCP_TIMEWAIT_LEN)
const SYN_RETRIES: i32 = 6;
const FIN_TIMEOUT: u64 = 60;
/// TCP_KEEPIDLE / TCP_KEEPINTVL / TCP_KEEPCNT の既定 (net.ipv4.tcp_keepalive_*)
const KEEPIDLE: i32 = 7200;
const KEEPINTVL: i32 = 75;
const KEEPCNT: i32 = 9;

/// urgent のバイトの状態 (Linux の TCP_URG_VALID / NOTYET / READ)
const URG_VALID: u16 = 0x100;
const URG_NOTYET: u16 = 0x200;
const URG_READ: u16 = 0x400;

const HZ: u64 = crate::timer::HZ;

/// TCP の状態 (番号は Linux と同じ: /proc/net/tcp の st)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    Established = 1,
    SynSent = 2,
    /// aios の中では 3 回のあいさつをすぐにすませるので、この状態は残らない (match のために)
    #[allow(dead_code)]
    SynRecv = 3,
    FinWait1 = 4,
    FinWait2 = 5,
    TimeWait = 6,
    Close = 7,
    CloseWait = 8,
    LastAck = 9,
    Listen = 10,
    Closing = 11,
}

/// ソケットの層の状態 (Linux の struct socket の state)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ss {
    Unconnected,
    Connecting,
    Connected,
    Disconnecting,
}

/// TCP の受ける行列の 1 つ (Linux の skb): seq からのバイト。fin はそのあとの FIN (seq の 1 つぶん)
struct Seg {
    seq: u32,
    data: Vec<u8>,
    fin: bool,
}

impl Seg {
    fn end(&self) -> u32 {
        self.seq.wrapping_add(self.data.len() as u32).wrapping_add(self.fin as u32)
    }
}

/// UDP のデータグラム: 送り元 (アドレス, ポート)、あて先のアドレス (IP_PKTINFO)、数えた大きさ (skb の truesize)
struct Dgram {
    data: Vec<u8>,
    from: (u32, u16),
    to: u32,
    tos: u8,
    ttl: u8,
    truesize: usize,
}

/// IP_RECVERR のエラーの行列のもの (Linux の sock_exterr_skb): 送ったものの中身、errno、ICMP の種類、あて先、
/// 知らせてきたアドレス
struct ErrMsg {
    data: Vec<u8>,
    errno: u32,
    typ: u8,
    code: u8,
    to: (u32, u16),
    offender: u32,
}

pub struct Inet {
    pub tcp: bool,
    /// SO_PROTOCOL
    protocol: u8,
    /// 分けた NET (なければ 0)
    net: u64,
    state: St,
    ss: Ss,
    shutdown: u8,
    /// sk_err (SO_ERROR、次の読み書きで返す)
    err: i64,
    /// FIN を受けた (Linux の SOCK_DONE)
    done: bool,
    /// fd がない (閉じた、または accept されていない: Linux の SOCK_DEAD と sk_socket なし)
    dead: bool,
    has_file: bool,
    /// アドレスとポート (ホストの順): 受けるアドレス (inet_rcv_saddr)、送り元 (inet_saddr)、表にあるポート (inet_num)、
    /// 名前に見せるポート (inet_sport)、相手
    rcv_saddr: u32,
    saddr: u32,
    num: u16,
    sport: u16,
    daddr: u32,
    dport: u16,
    /// bind で決めたもの (SOCK_BINDADDR_LOCK / SOCK_BINDPORT_LOCK) と、setsockopt で決めた入れ物 (SOCK_SNDBUF_LOCK /
    /// SOCK_RCVBUF_LOCK)
    lock_addr: bool,
    lock_port: bool,
    lock_sndbuf: bool,
    lock_rcvbuf: bool,
    /// ポートの表にいる。pseq はそこでの番号 (bind した順)
    hashed: bool,
    pseq: u64,
    /// 時刻の表にいるときの時刻 (0 ならいない)
    tkey: u64,
    // SOL_SOCKET
    reuse: bool,
    reuseport: bool,
    keepalive: bool,
    oobinline: bool,
    broadcast: bool,
    dontroute: bool,
    passcred: bool,
    priority: u32,
    linger: bool,
    linger_secs: u64,
    rcvlowat: i32,
    peek_off: i32,
    rcvtimeo: u64,
    sndtimeo: u64,
    sndbuf: usize,
    rcvbuf: usize,
    // IPPROTO_TCP
    nodelay: bool,
    cork: bool,
    user_mss: i32,
    mss_cache: i32,
    mss_clamp: i32,
    keepidle: i32,
    keepintvl: i32,
    keepcnt: i32,
    syncnt: i32,
    /// TCP_LINGER2 (秒。0 は tcp_fin_timeout、-1 は FIN_WAIT2 を待たずに RST)
    linger2: i32,
    user_timeout: u32,
    notsent_lowat: u32,
    window_clamp: i32,
    // TCP の受ける側 (seq は Linux の rcv_nxt と copied_seq。はじめは 1)
    rq: VecDeque<Seg>,
    rcv_nxt: u32,
    copied_seq: u32,
    urg_data: u16,
    urg_seq: u32,
    /// つないだ相手 (aios の中のもの)。自分につないだものは自分
    peer: Option<Weak<RefCell<Inet>>>,
    /// 相手がこの行列があくのを待っている (読んだら起こす)
    writer_waits: bool,
    /// MSG_MORE と TCP_CORK でためているもの (MSS に満ちるか、ためない送り、TCP_NODELAY、FIN で出す)
    cork_buf: Vec<u8>,
    /// listen: backlog と、つながって accept を待つもの
    max_backlog: u32,
    acceptq: VecDeque<InetRef>,
    /// SYN_SENT: listen の行列があふれていてやりなおす回数と、次の時刻 (tick)
    syn_tries: i32,
    syn_next: u64,
    /// FIN_WAIT2 / TIME_WAIT で残っているものの終わりの時刻 (tick)
    expires: u64,
    // UDP
    dq: VecDeque<Dgram>,
    rmem: usize,
    /// MSG_MORE でためているもの (中身、あて先)
    pending: Option<(Vec<u8>, u32, u16)>,
    errq: VecDeque<ErrMsg>,
    // IPPROTO_IP
    tos: u8,
    uc_ttl: i32,
    mc_ttl: u8,
    mc_loop: bool,
    mc_all: bool,
    recverr: bool,
    pktinfo: bool,
    recvtos: bool,
    recvttl: bool,
    freebind: bool,
    pmtudisc: u8,
    bind_no_port: bool,
    minttl: u8,
    /// 外 (eth0 の向こう) とのもの: smoltcp のソケット (TCP のつながり、UDP の口) と、外からつないでくるものを待つ口
    ext: Option<SocketHandle>,
    ext_listen: Option<SocketHandle>,
    /// fstat に見せるもの (Linux の sockfs の inode)
    ino: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: u64,
    ctime: u64,
    /// 状態が変わるたびに増える数 (epoll の EPOLLET)
    pub generation: u64,
}

pub type InetRef = Rc<RefCell<Inet>>;
type W = Weak<RefCell<Inet>>;
type R = Result<i64, i64>;

// ---- 表 ----

// どれも端から探さない: つながりや TIME_WAIT がいくつあっても、socket、connect、accept、close の手間は変わらない
// (Linux の bhash、listening_hash、tw の timer にあたる)

/// ポートの表: (NET, TCP か, ポート) → そこにいるもの (pseq の順 = bind した順。accept を待つ子や TIME_WAIT も
/// listen しているもののポートにいる: Linux の inet_inherit_port)
static PORTS: Bkl<BTreeMap<(u64, bool, u16), BTreeMap<u64, W>>> = Bkl::new(BTreeMap::new());
static PSEQ: Bkl<u64> = Bkl::new(0);
/// listen しているものだけ: (NET, ポート) → pseq の順。listen をやめたものは lookup_listener で掃く
static LISTENERS: Bkl<BTreeMap<(u64, u16), BTreeMap<u64, W>>> = Bkl::new(BTreeMap::new());
/// fd がなくなっても残っているもの (FIN_WAIT2、TIME_WAIT、LAST_ACK)
static ORPHANS: Bkl<BTreeMap<usize, InetRef>> = Bkl::new(BTreeMap::new());
/// すべて (/proc/net/tcp と udp)。消えたものは new_sock で掃く (数が SWEEP をこえたときだけ: 均せば 1 つに O(1))
static ALL: Bkl<Vec<W>> = Bkl::new(Vec::new());
static SWEEP: Bkl<usize> = Bkl::new(64);
/// 時刻でやることがあるもの: (その時刻, key) の順 (SYN のやりなおし、FIN_WAIT2 / TIME_WAIT の終わり)。tick は
/// 時刻の来たものだけを見る
static TIMED: Bkl<BTreeMap<(u64, usize), W>> = Bkl::new(BTreeMap::new());

#[track_caller]
fn ports() -> &'static mut BTreeMap<(u64, bool, u16), BTreeMap<u64, W>> {
    PORTS.get()
}

#[track_caller]
fn listeners() -> &'static mut BTreeMap<(u64, u16), BTreeMap<u64, W>> {
    LISTENERS.get()
}

#[track_caller]
fn orphans() -> &'static mut BTreeMap<usize, InetRef> {
    ORPHANS.get()
}

#[track_caller]
fn all() -> &'static mut Vec<W> {
    ALL.get()
}

#[track_caller]
fn timed() -> &'static mut BTreeMap<(u64, usize), W> {
    TIMED.get()
}

fn key(u: &InetRef) -> usize {
    Rc::as_ptr(u) as usize
}

fn ticks() -> u64 {
    crate::timer::ticks()
}

fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn after(a: u32, b: u32) -> bool {
    before(b, a)
}

/// 状態が変わった: epoll の数を進め、待っている人 (読む、書く、つなぐ、poll) を起こす
fn notify(u: &InetRef) {
    let _p = crate::prof::span("inet:wake");
    u.borrow_mut().generation += 1;
    proc::wakeup(key(u));
    proc::poll_wake(key(u));
}

fn upgrade(w: &Option<W>) -> Option<InetRef> {
    w.as_ref().and_then(|w| w.upgrade())
}

/// 眠る (SO_RCVTIMEO / SO_SNDTIMEO があればそこまで: 時間切れは EAGAIN)。until は時刻でやることの時刻 (0 は無し)
fn wait(chan: usize, timeo: u64, until: u64) -> Result<(), i64> {
    let deadline = if timeo == 0 { 0 } else { crate::timer::ticks_after(timeo) };
    let d = match (deadline, until) {
        (0, x) | (x, 0) => x,
        (a, b) => a.min(b),
    };
    let woken = proc::sleep_until(chan, d)?;
    if !woken && deadline != 0 && ticks() >= deadline {
        return Err(-EAGAIN);
    }
    Ok(())
}

/// 新しいソケット (持ち主と NET を決めて: タイマから作るもの (受ける口、TIME_WAIT) は、いまのプロセスのものでない)
fn new_sock_as(tcp: bool, protocol: u8, uid: u32, gid: u32, netns: u64) -> InetRef {
    let now = crate::timer::coarse_epoch_ns();
    let u = Rc::new(RefCell::new(Inet {
        tcp,
        protocol,
        net: netns,
        state: St::Close,
        ss: Ss::Unconnected,
        shutdown: 0,
        err: 0,
        done: false,
        dead: false,
        has_file: true,
        rcv_saddr: 0,
        saddr: 0,
        num: 0,
        sport: 0,
        daddr: 0,
        dport: 0,
        lock_addr: false,
        lock_port: false,
        lock_sndbuf: false,
        lock_rcvbuf: false,
        hashed: false,
        pseq: 0,
        tkey: 0,
        reuse: false,
        reuseport: false,
        keepalive: false,
        oobinline: false,
        broadcast: false,
        dontroute: false,
        passcred: false,
        priority: 0,
        linger: false,
        linger_secs: 0,
        rcvlowat: 1,
        peek_off: -1,
        rcvtimeo: 0,
        sndtimeo: 0,
        sndbuf: if tcp { TCP_SNDBUF } else { UDP_BUF },
        rcvbuf: if tcp { TCP_RCVBUF } else { UDP_BUF },
        nodelay: false,
        cork: false,
        user_mss: 0,
        mss_cache: MSS_DEFAULT,
        mss_clamp: MSS_DEFAULT,
        keepidle: 0,
        keepintvl: 0,
        keepcnt: 0,
        syncnt: 0,
        linger2: 0,
        user_timeout: 0,
        notsent_lowat: 0,
        window_clamp: 0,
        rq: VecDeque::new(),
        rcv_nxt: 1,
        copied_seq: 1,
        urg_data: 0,
        urg_seq: 0,
        peer: None,
        writer_waits: false,
        cork_buf: Vec::new(),
        max_backlog: 0,
        acceptq: VecDeque::new(),
        syn_tries: 0,
        syn_next: 0,
        expires: 0,
        dq: VecDeque::new(),
        rmem: 0,
        pending: None,
        errq: VecDeque::new(),
        tos: 0,
        uc_ttl: -1,
        mc_ttl: 1,
        mc_loop: true,
        mc_all: true,
        recverr: false,
        pktinfo: false,
        recvtos: false,
        recvttl: false,
        freebind: false,
        pmtudisc: 1,
        bind_no_port: false,
        minttl: 0,
        ext: None,
        ext_listen: None,
        ino: crate::unix::new_ino(),
        uid,
        gid,
        mode: 0o777,
        mtime: now,
        ctime: now,
        generation: 0,
    }));
    let a = all();
    let sweep = SWEEP.get();
    if a.len() >= *sweep {
        a.retain(|w| w.strong_count() > 0);
        *sweep = (a.len() * 2).max(64);
    }
    a.push(Rc::downgrade(&u));
    u
}

/// socket(AF_INET, SOCK_STREAM | SOCK_DGRAM, protocol): 型と protocol は呼ぶほうで見た (socket.rs の inet_check)
pub fn create(typ: u64, protocol: u64) -> InetRef {
    let tcp = typ == 1;
    let c = proc::current_cred_ref();
    new_sock_as(tcp, if protocol != 0 { protocol as u8 } else if tcp { 6 } else { 17 }, c.euid, c.egid, crate::ns::net().unwrap_or(0))
}

pub fn file_of(u: InetRef, flags: u64) -> FileRef {
    file::new(Kind::Inet(u), file::O_RDWR | if flags & 0o4000 != 0 { file::O_NONBLOCK } else { 0 })
}

// ---- AppArmor (af_inet.c。network_v8 なので (inet, type, protocol) の決まりで見る。番地は記録だけ) ----

/// 番地の確かめ (af_inet.c の map_addr: 名前がなければよい、AF_INET は 16 バイト、AF_INET6 は 24 バイトなければ EINVAL、
/// ほかの family は EAFNOSUPPORT)。決まりのあるプロセスだけ
pub fn aa_map_addr(a: Option<&[u8]>) -> Result<(), i64> {
    let Some(a) = a.filter(|a| a.len() >= 2) else { return Ok(()) };
    match u16::from_le_bytes([a[0], a[1]]) {
        2 if a.len() < 16 => Err(crate::why::inval()),
        2 => Ok(()),
        10 if a.len() < 24 => Err(crate::why::inval()),
        10 => Ok(()),
        _ => Err(-97),
    }
}

/// いまのプロセスの決まりで、このソケットへの op。bind は自分の名前を a に (saddr= と src=)、ほかは今の名前 (つないで
/// いれば相手の名前を remote に: daddr= と dest=)
pub fn aa_perm(u: &InetRef, op: &str, request: u32, bind: Option<&[u8]>, remote: Option<&[u8]>) -> Result<(), i64> {
    use crate::apparmor::inet_addr_audit;
    let (typ, proto, sk, local) = {
        let s = u.borrow();
        let sk = inet_addr_audit(Some(&sockaddr(s.rcv_saddr, s.sport)), "laddr", "lport") + &inet_addr_audit(Some(&sockaddr(s.daddr, s.dport)), "faddr", "fport");
        let local = sockaddr(if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr }, s.sport);
        (if s.tcp { 1 } else { 2 }, s.protocol as u32, sk, local)
    };
    let pre = || {
        let mut s = sk.clone();
        s.push_str(&inet_addr_audit(Some(bind.unwrap_or(&local)), "saddr", "src"));
        s.push_str(&inet_addr_audit(remote, "daddr", "dest"));
        crate::apparmor::NetAudit { pre: s, post: String::new() }
    };
    crate::apparmor::net_perm(op, request, 2, typ, proto, &pre)
}

// ---- アドレス ----

/// eth0 のアドレス (なければ 0)
fn eth() -> u32 {
    u32::from_be_bytes(net::addr().octets())
}

/// aios の中のアドレスか (Linux の RTN_LOCAL: 127.0.0.0/8 と、分けていない NET の eth0 のアドレス)
fn is_local(a: u32, netns: u64) -> bool {
    a >> 24 == 127 || (netns == 0 && a != 0 && a == eth())
}

fn is_multicast(a: u32) -> bool {
    a >> 28 == 0xe
}

fn is_broadcast(a: u32) -> bool {
    if a == u32::MAX {
        return true;
    }
    net::cidr().is_some_and(|c| {
        let p = c.prefix_len() as u32;
        let base = u32::from_be_bytes(c.address().octets());
        p < 31 && a == base | (u32::MAX >> p)
    })
}

/// あて先へのみち (Linux の ip_route_output): (あて先, 送り元, aios の中か)。あて先 0.0.0.0 は自分 (送り元か
/// 127.0.0.1)。外へ出られなければ ENETUNREACH
fn route(netns: u64, daddr: u32, saddr: u32) -> Result<(u32, u32, bool), i64> {
    if daddr == 0 {
        let d = if saddr != 0 { saddr } else { LOOPBACK };
        return Ok((d, if saddr != 0 { saddr } else { LOOPBACK }, true));
    }
    if daddr >> 24 == 127 {
        return Ok((daddr, if saddr != 0 { saddr } else { LOOPBACK }, true));
    }
    if netns == 0 && daddr == eth() && daddr != 0 {
        return Ok((daddr, if saddr != 0 { saddr } else { daddr }, true));
    }
    if netns != 0 || net::get().is_none() || eth() == 0 {
        return Err(-ENETUNREACH);
    }
    Ok((daddr, if saddr != 0 { saddr } else { eth() }, false))
}

/// sockaddr_in (16 バイト)
fn sockaddr(a: u32, port: u16) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..2].copy_from_slice(&AF_INET.to_le_bytes());
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[4..8].copy_from_slice(&a.to_be_bytes());
    b
}

fn parse(a: &[u8]) -> (u16, u16, u32) {
    let fam = u16::from_le_bytes([a[0], a[1]]);
    let port = if a.len() >= 4 { u16::from_be_bytes([a[2], a[3]]) } else { 0 };
    let addr = if a.len() >= 8 { u32::from_be_bytes([a[4], a[5], a[6], a[7]]) } else { 0 };
    (fam, port, addr)
}

fn ipv4(a: u32) -> Ipv4Address {
    Ipv4Address::from_bits(a)
}

// ---- ポートの表 ----

/// その口のポートにいるもの (bind した順。消えたものは掃く)
fn owners(netns: u64, tcp: bool, port: u16) -> Vec<InetRef> {
    let k = (netns, tcp, port);
    let Some(v) = ports().get_mut(&k) else { return Vec::new() };
    v.retain(|_, w| w.strong_count() > 0);
    let out: Vec<InetRef> = v.values().filter_map(|w| w.upgrade()).collect();
    if v.is_empty() {
        ports().remove(&k);
    }
    out
}

/// そのポートにだれもいないか (空いているポートを探すとき: 中身は写さない)
fn port_free(netns: u64, tcp: bool, port: u16) -> bool {
    let k = (netns, tcp, port);
    let Some(v) = ports().get_mut(&k) else { return true };
    if v.values().any(|w| w.strong_count() > 0) {
        return false;
    }
    ports().remove(&k);
    true
}

fn hash_port(u: &InetRef, port: u16) {
    let (k, q) = {
        let mut s = u.borrow_mut();
        let q = {
            let n = PSEQ.get();
            *n += 1;
            *n
        };
        s.num = port;
        s.sport = port;
        s.hashed = true;
        s.pseq = q;
        ((s.net, s.tcp, port), q)
    };
    ports().entry(k).or_default().insert(q, Rc::downgrade(u));
}

/// 表から出す (inet_num は 0 に。名前に見せるポートは TCP ではそのまま: Linux の inet_put_port)
fn put_port(u: &InetRef) {
    let k = {
        let mut s = u.borrow_mut();
        if !s.hashed {
            return;
        }
        s.hashed = false;
        let k = ((s.net, s.tcp, s.num), s.pseq);
        s.num = 0;
        s.pseq = 0;
        k
    };
    if let Some(v) = ports().get_mut(&k.0) {
        v.remove(&k.1);
        if v.is_empty() {
            ports().remove(&k.0);
        }
    }
}

/// bind をしらべるときの自分
#[derive(Clone, Copy)]
struct Me {
    tcp: bool,
    net: u64,
    rcv_saddr: u32,
    reuse: bool,
    reuseport: bool,
    uid: u32,
}

fn me_of(s: &Inet) -> Me {
    Me { tcp: s.tcp, net: s.net, rcv_saddr: s.rcv_saddr, reuse: s.reuse, reuseport: s.reuseport, uid: s.uid }
}

/// アドレスが重なるか (どちらかが 0.0.0.0 か、同じ: Linux の inet_rcv_saddr_equal の match_wildcard)
fn addr_overlap(a: u32, b: u32) -> bool {
    a == 0 || b == 0 || a == b
}

/// port に bind するとぶつかるか。TCP は Linux の inet_bind_conflict (relax、reuseport_ok): どちらも SO_REUSEADDR で
/// 相手が listen していなければよい、どちらも SO_REUSEPORT で同じ持ち主 (か相手が TIME_WAIT) ならよい。UDP は
/// udp_lib_lport_inuse: どちらも SO_REUSEADDR ならよい、新しいものから見て、はじめに重なったものがどちらも
/// SO_REUSEPORT で同じ持ち主ならよい
fn conflicts(me: &Me, u: &InetRef, port: u16) -> bool {
    let list = owners(me.net, me.tcp, port);
    if me.tcp {
        return list.iter().filter(|o| !Rc::ptr_eq(o, u)).any(|o| {
            let o = o.borrow();
            if !addr_overlap(me.rcv_saddr, o.rcv_saddr) {
                return false;
            }
            if me.reuse && o.reuse && o.state != St::Listen {
                return false;
            }
            !(me.reuseport && o.reuseport && (o.state == St::TimeWait || o.uid == me.uid))
        });
    }
    for o in list.iter().rev().filter(|o| !Rc::ptr_eq(o, u)) {
        let o = o.borrow();
        if (!o.reuse || !me.reuse) && addr_overlap(me.rcv_saddr, o.rcv_saddr) {
            return !(o.reuseport && me.reuseport && o.uid == me.uid);
        }
    }
    false
}

/// ポートを取る (Linux の get_port: 0 なら空いているものを。ぶつかれば EADDRINUSE)
fn get_port(u: &InetRef, snum: u16) -> Result<(), i64> {
    let me = me_of(&u.borrow());
    if snum != 0 {
        if conflicts(&me, u, snum) {
            return Err(-EADDRINUSE);
        }
        if !u.borrow().hashed {
            hash_port(u, snum);
        }
        return Ok(());
    }
    let range = (PORT_HI - PORT_LO) as u32 + 1;
    let mut r = [0u8; 4];
    crate::rand::fill(&mut r);
    let start = u32::from_le_bytes(r) % range;
    for i in 0..range {
        let p = PORT_LO + ((start + i) % range) as u16;
        if port_free(me.net, me.tcp, p) {
            hash_port(u, p);
            return Ok(());
        }
    }
    Err(-EADDRINUSE)
}

/// 自分で bind する (Linux の inet_autobind: UDP の送る前と connect の前)
fn autobind(u: &InetRef) -> Result<(), i64> {
    if u.borrow().num != 0 {
        return Ok(());
    }
    get_port(u, 0).map_err(|_| -EAGAIN)?;
    ext_udp_bind(u);
    Ok(())
}

// ---- bind / listen ----

/// bind (Linux の __inet_bind): 長さ、family (AF_UNSPEC は 0.0.0.0 だけ)、自分のアドレスか (でなければ
/// EADDRNOTAVAIL。IP_FREEBIND ならよい)、1024 より下は root だけ、もう bind していれば EINVAL、ポート
pub fn bind(u: &InetRef, a: &[u8]) -> R {
    if a.len() < 16 {
        return Err(crate::why::inval());
    }
    let (fam, port, addr) = parse(a);
    if fam != AF_INET && !(fam == AF_UNSPEC && addr == 0) {
        return Err(-EAFNOSUPPORT);
    }
    let (netns, freebind) = {
        let s = u.borrow();
        (s.net, s.freebind)
    };
    let special = is_multicast(addr) || is_broadcast(addr);
    if !(addr == 0 || is_local(addr, netns) || special || freebind) {
        return Err(-EADDRNOTAVAIL);
    }
    if port != 0 && port < PRIV_PORT && !proc::current_cred_ref().capable(crate::cred::CAP_NET_BIND_SERVICE) {
        return Err(-EACCES);
    }
    {
        let mut s = u.borrow_mut();
        if s.state != St::Close || s.num != 0 {
            return Err(crate::why::inval());
        }
        s.rcv_saddr = addr;
        s.saddr = if special { 0 } else { addr };
    }
    let no_port = u.borrow().bind_no_port;
    if port != 0 || !no_port {
        if let Err(e) = get_port(u, port) {
            let mut s = u.borrow_mut();
            s.rcv_saddr = 0;
            s.saddr = 0;
            return Err(e);
        }
    }
    {
        let mut s = u.borrow_mut();
        if s.rcv_saddr != 0 {
            s.lock_addr = true;
        }
        if port != 0 {
            s.lock_port = true;
        }
        s.sport = s.num;
        s.daddr = 0;
        s.dport = 0;
    }
    ext_udp_bind(u);
    Ok(0)
}

/// listen (Linux の inet_listen と inet_csk_listen_start): TCP だけ、つないでいないもの。はじめての listen は
/// ポートを取りなおす (listen するものとして、ぶつかりをしらべる。bind していなければ空いているものを)
pub fn listen(u: &InetRef, backlog: u64) -> R {
    let backlog = if (backlog as u32) > SOMAXCONN { SOMAXCONN } else { backlog as u32 };
    {
        let s = u.borrow();
        if !s.tcp {
            return Err(-EOPNOTSUPP);
        }
        if s.ss != Ss::Unconnected || !matches!(s.state, St::Close | St::Listen) {
            return Err(crate::why::inval());
        }
    }
    u.borrow_mut().max_backlog = backlog;
    if u.borrow().state == St::Listen {
        return Ok(0);
    }
    u.borrow_mut().state = St::Listen;
    let num = u.borrow().num;
    if let Err(e) = get_port(u, num) {
        u.borrow_mut().state = St::Close;
        return Err(e);
    }
    let (k, q) = {
        let mut s = u.borrow_mut();
        s.sport = s.num;
        ((s.net, s.num), s.pseq)
    };
    listeners().entry(k).or_default().insert(q, Rc::downgrade(u));
    ext_listen(u);
    notify(u);
    Ok(0)
}

// ---- TCP: つなぐ ----

/// 時刻でやることがあるものとして覚える (tick が見る): いまの状態の時刻 (SYN_SENT ならやりなおし、fd のない
/// FIN_WAIT2 / TIME_WAIT なら終わり) で表に入れなおす。どれでもなければ表から出す
fn arm(u: &InetRef) {
    let mut s = u.borrow_mut();
    let at = match s.state {
        St::SynSent => s.syn_next,
        St::TimeWait | St::FinWait2 if s.dead => s.expires,
        _ => 0,
    };
    if s.tkey == at {
        return;
    }
    if s.tkey != 0 {
        timed().remove(&(s.tkey, key(u)));
    }
    s.tkey = at;
    if at != 0 {
        timed().insert((at, key(u)), Rc::downgrade(u));
    }
}

fn disarm(u: &InetRef) {
    let mut s = u.borrow_mut();
    if s.tkey != 0 {
        timed().remove(&(s.tkey, key(u)));
        s.tkey = 0;
    }
}

/// fd がなくなっても残すもの
fn keep_orphan(u: &InetRef) {
    orphans().insert(key(u), u.clone());
}

fn drop_orphan(u: &InetRef) {
    orphans().remove(&key(u));
}

/// 4 つ組 (自分のアドレスとポート、相手のアドレスとポート) がもう使われているか (Linux の __inet_check_established:
/// つながったもの、TIME_WAIT。TIME_WAIT は 1 秒たっていれば使いなおせる: tcp_tw_reuse の 2 はループバックだけ)
fn tuple_used(u: &InetRef, saddr: u32, sport: u16, daddr: u32, dport: u16) -> bool {
    let (netns, tcp) = {
        let s = u.borrow();
        (s.net, s.tcp)
    };
    for o in owners(netns, tcp, sport) {
        if Rc::ptr_eq(&o, u) {
            continue;
        }
        let (hit, tw, exp) = {
            let x = o.borrow();
            let hit = x.rcv_saddr == saddr && x.daddr == daddr && x.dport == dport && !matches!(x.state, St::Listen | St::Close);
            (hit, x.state == St::TimeWait, x.expires)
        };
        if !hit {
            continue;
        }
        if tw && ticks() + (FIN_TIMEOUT - 1) * HZ >= exp {
            kill(&o);
            continue;
        }
        return true;
    }
    false
}

/// listen しているもの (Linux の __inet_lookup_listener: そのアドレスのものを先に、なければ 0.0.0.0 のもの。
/// 同じなら新しいもの)
fn lookup_listener(netns: u64, daddr: u32, dport: u16) -> Option<InetRef> {
    let k = (netns, dport);
    let v = listeners().get_mut(&k)?;
    // listen をやめたもの、ポートを持ちなおしたものは掃く
    v.retain(|&q, w| {
        w.upgrade().is_some_and(|o| {
            let o = o.borrow();
            o.state == St::Listen && o.hashed && o.pseq == q && o.num == dport
        })
    });
    let list: Vec<InetRef> = v.values().filter_map(|w| w.upgrade()).collect();
    if v.is_empty() {
        listeners().remove(&k);
    }
    let pick = |exact: bool| list.iter().rev().find(|o| if exact { o.borrow().rcv_saddr == daddr } else { o.borrow().rcv_saddr == 0 }).cloned();
    pick(true).or_else(|| pick(false))
}

/// つながった (Linux の tcp_finish_connect / tcp_init_transfer): MSS (相手の知らせたもの、自分の TCP_MAXSEG、窓の
/// 半分) と、入れ物を広げる (Linux の tcp_sndbuf_expand: 2 × cwnd 10 × MSS の skb の大きさ。決めたものはそのまま)
fn established(s: &mut Inet, peer_mss: i32, half_wnd: i32) {
    s.state = St::Established;
    let mut clamp = peer_mss;
    if s.user_mss != 0 && s.user_mss < clamp {
        clamp = s.user_mss;
    }
    s.mss_clamp = clamp;
    s.mss_cache = (clamp - 12).min(half_wnd);
    if !s.lock_sndbuf {
        let per = (clamp.max(s.mss_cache) as usize + 576).next_power_of_two() + 256;
        let want = (2 * 10 * per).min(4194304);
        if s.sndbuf < want {
            s.sndbuf = want;
        }
    }
}

/// 相手から SYN でない segment (データ、ACK、FIN) が来た: 窓の大きさが知らされる (Linux の tcp_ack_update_window:
/// 窓が max_window をこえたら tcp_sync_mss)。ループバックの窓は 65483 を 128 にそろえた 65536 なので、半分は 32768
/// (SYN-ACK の窓は 65483 で、つないだほうは 32741 から)
fn wnd_seen(u: &InetRef) {
    let mut s = u.borrow_mut();
    if s.ext.is_none() && s.state != St::SynSent {
        s.mss_cache = s.mss_cache.max((s.mss_clamp - 12).min(HALF_WND_SERVER));
    }
}

/// こちらが SYN で知らせる MSS (Linux の tcp_mss_clamp: TCP_MAXSEG があればそれまで)
fn adv_mss(user_mss: i32) -> i32 {
    if user_mss != 0 && user_mss < ADV_MSS { user_mss } else { ADV_MSS }
}

/// SYN を送る (connect とやりなおし): 自分につないだもの、listen しているものの行列 (あふれていれば落とされる:
/// SYN_SENT のまま、やりなおしの時刻を決める)、いなければ RST (ECONNREFUSED)
fn syn(u: &InetRef) {
    let _p = crate::prof::span("inet:syn");
    let (netns, saddr, sport, daddr, dport, my_mss, my_ttl) = {
        let s = u.borrow();
        (s.net, s.saddr, s.num, s.daddr, s.dport, s.user_mss, if s.uc_ttl < 0 { 64 } else { s.uc_ttl })
    };
    // 自分につないだ (同時に開く: SYN を受けて SYN_RECV、SYN-ACK を受けて ESTABLISHED)
    if saddr == daddr && sport == dport {
        let mut s = u.borrow_mut();
        established(&mut s, adv_mss(my_mss), HALF_WND_SERVER);
        s.peer = Some(Rc::downgrade(u));
        s.syn_tries = 0;
        drop(s);
        notify(u);
        return;
    }
    let Some(l) = lookup_listener(netns, daddr, dport) else {
        tcp_reset(u);
        return;
    };
    let full = {
        let l = l.borrow();
        l.acceptq.len() as u32 > l.max_backlog
    };
    if full {
        let mut s = u.borrow_mut();
        let rto = 1u64 << s.syn_tries.min(10);
        s.syn_next = ticks() + rto * HZ;
        drop(s);
        arm(u);
        return;
    }
    // listen しているものを写して受ける口を作る (Linux の sk_clone_lock: 決めたものはみな引きつぐ)
    let c = {
        let l = l.borrow();
        let c = new_sock_as(true, l.protocol, l.uid, l.gid, l.net);
        {
            let mut x = c.borrow_mut();
            // accept されるまで fd はないが、閉じたもの (SOCK_DEAD) ではない
            x.has_file = false;
            x.reuse = l.reuse;
            x.reuseport = l.reuseport;
            x.keepalive = l.keepalive;
            x.oobinline = l.oobinline;
            x.broadcast = l.broadcast;
            x.dontroute = l.dontroute;
            x.passcred = l.passcred;
            x.nodelay = l.nodelay;
            x.cork = l.cork;
            x.linger = l.linger;
            x.lock_sndbuf = l.lock_sndbuf;
            x.lock_rcvbuf = l.lock_rcvbuf;
            x.recverr = l.recverr;
            x.pktinfo = l.pktinfo;
            x.recvtos = l.recvtos;
            x.recvttl = l.recvttl;
            x.freebind = l.freebind;
            x.mc_loop = l.mc_loop;
            x.priority = l.priority;
            x.linger_secs = l.linger_secs;
            x.rcvlowat = l.rcvlowat;
            x.rcvtimeo = l.rcvtimeo;
            x.sndtimeo = l.sndtimeo;
            x.sndbuf = l.sndbuf;
            x.rcvbuf = l.rcvbuf;
            x.user_mss = l.user_mss;
            x.keepidle = l.keepidle;
            x.keepintvl = l.keepintvl;
            x.keepcnt = l.keepcnt;
            x.syncnt = l.syncnt;
            x.linger2 = l.linger2;
            x.user_timeout = l.user_timeout;
            x.notsent_lowat = l.notsent_lowat;
            x.tos = l.tos;
            x.uc_ttl = l.uc_ttl;
            // IP_MULTICAST_TTL は受けた ACK の TTL (Linux の tcp_v4_syn_recv_sock)
            x.mc_ttl = my_ttl as u8;
            x.pmtudisc = l.pmtudisc;
            x.minttl = l.minttl;
            x.uid = l.uid;
            x.gid = l.gid;
            x.rcv_saddr = daddr;
            x.saddr = daddr;
            x.lock_addr = l.lock_addr;
            x.lock_port = l.lock_port;
            x.daddr = saddr;
            x.dport = sport;
            x.ss = Ss::Connected;
            established(&mut x, adv_mss(my_mss), HALF_WND_SERVER);
            // listen する前の shutdown の印も写る (Linux の sk_clone_lock は sk_shutdown もそのまま写す)
            x.shutdown = l.shutdown;
            x.peer = Some(Rc::downgrade(u));
        }
        c
    };
    let lport = l.borrow().num;
    hash_port(&c, lport);
    {
        let theirs = adv_mss(l.borrow().user_mss);
        let mut s = u.borrow_mut();
        established(&mut s, theirs, HALF_WND_CLIENT);
        s.peer = Some(Rc::downgrade(&c));
        s.syn_tries = 0;
    }
    // 写った SEND の shutdown は、つながったところで FIN を送る (Linux の tcp_rcv_state_process の TCP_SYN_RECV)
    if c.borrow().shutdown & SEND != 0 {
        let fin = close_state(&mut c.borrow_mut());
        if fin {
            send_fin(&c);
        }
    }
    l.borrow_mut().acceptq.push_back(c);
    notify(&l);
    notify(u);
}

/// connect (Linux の __inet_stream_connect と tcp_v4_connect)。aios の中のあいてには、そのまま SYN を届けて答えも
/// 受けとる: 待たないものは、つながっても断られても EINPROGRESS (Linux は答えを、connect が返るときに受ける)
pub fn connect(f: &FileRef, u: &InetRef, a: &[u8]) -> R {
    let nonblock = f.borrow().flags & file::O_NONBLOCK != 0;
    if !u.borrow().tcp {
        return udp_connect(u, a);
    }
    if a.len() < 2 {
        return Err(crate::why::inval());
    }
    let (fam, _, _) = parse(a);
    if fam == AF_UNSPEC {
        let r = tcp_disconnect(u);
        u.borrow_mut().ss = if r.is_err() { Ss::Disconnecting } else { Ss::Unconnected };
        return Ok(0);
    }
    let ss = u.borrow().ss;
    let mut err = match ss {
        Ss::Connected => return Err(-EISCONN),
        Ss::Disconnecting => return Err(crate::why::inval()),
        Ss::Connecting => -EALREADY,
        Ss::Unconnected => {
            if u.borrow().state != St::Close {
                return Err(-EISCONN);
            }
            tcp_v4_connect(u, a)?;
            u.borrow_mut().ss = Ss::Connecting;
            -EINPROGRESS
        }
    };
    if ss == Ss::Unconnected && nonblock {
        return Err(err);
    }
    loop {
        ext_sync(u);
        let (st, timeo, next) = {
            let s = u.borrow();
            (s.state, s.sndtimeo, s.syn_next)
        };
        if !matches!(st, St::SynSent | St::SynRecv) {
            break;
        }
        if nonblock {
            return Err(err);
        }
        let chan = if u.borrow().ext.is_some() { net::chan() } else { key(u) };
        match wait(chan, timeo, next) {
            Ok(()) => {}
            Err(e) if e == -EAGAIN => return Err(err),
            Err(e) => return Err(e),
        }
        err = -EALREADY;
    }
    if u.borrow().state == St::Close {
        let e = take_err(u);
        u.borrow_mut().ss = Ss::Unconnected;
        let _ = tcp_disconnect(u);
        return Err(if e != 0 { e } else { -ECONNABORTED });
    }
    u.borrow_mut().ss = Ss::Connected;
    let _ = err;
    Ok(0)
}

/// sk_err を取り出す (Linux の sock_error: 0 にする)
fn take_err(u: &InetRef) -> i64 {
    let mut s = u.borrow_mut();
    let e = s.err;
    s.err = 0;
    e
}

fn tcp_v4_connect(u: &InetRef, a: &[u8]) -> Result<(), i64> {
    if a.len() < 16 {
        return Err(crate::why::inval());
    }
    let (fam, dport, daddr) = parse(a);
    if fam != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let (netns, saddr0) = {
        let s = u.borrow();
        (s.net, s.saddr)
    };
    if is_multicast(daddr) || is_broadcast(daddr) {
        return Err(-ENETUNREACH);
    }
    let (daddr, src, local) = route(netns, daddr, saddr0)?;
    {
        let mut s = u.borrow_mut();
        if s.saddr == 0 {
            s.saddr = src;
            s.rcv_saddr = src;
        } else {
            s.rcv_saddr = s.saddr;
        }
        s.dport = dport;
        s.daddr = daddr;
        s.state = St::SynSent;
        s.syn_tries = 0;
        s.syn_next = 0;
        s.err = 0;
    }
    // ポート (Linux の inet_hash_connect: なければ空いているもの、あれば 4 つ組がほかと同じでないこと)
    let r = if u.borrow().num == 0 {
        get_port(u, 0).map_err(|_| -EADDRNOTAVAIL)
    } else {
        let (s1, p1) = {
            let s = u.borrow();
            (s.saddr, s.num)
        };
        if tuple_used(u, s1, p1, daddr, dport) { Err(-EADDRNOTAVAIL) } else { Ok(()) }
    };
    if let Err(e) = r {
        connect_failed(u);
        return Err(e);
    }
    {
        let mut s = u.borrow_mut();
        s.sport = s.num;
        // SYN を送るときの MSS (Linux の tcp_connect_init: mss_clamp は TCP_MAXSEG か 536、timestamp の 12 を引く)
        s.mss_cache = if s.user_mss != 0 { s.user_mss } else { MSS_DEFAULT } - 12;
    }
    if !local {
        if let Err(e) = ext_connect(u) {
            connect_failed(u);
            return Err(e);
        }
        return Ok(());
    }
    syn(u);
    Ok(())
}

/// tcp_v4_connect がしくじった: CLOSE にもどす (bind で決めていないアドレスとポートは手放す)
fn connect_failed(u: &InetRef) {
    let lock_port = {
        let mut s = u.borrow_mut();
        s.state = St::Close;
        if !s.lock_addr {
            s.saddr = 0;
            s.rcv_saddr = 0;
        }
        s.dport = 0;
        s.lock_port
    };
    if !lock_port {
        put_port(u);
    }
}

/// accept (Linux の inet_csk_accept): listen していなければ EINVAL、なければ待つ (SO_RCVTIMEO まで) か EAGAIN
pub fn accept(f: &FileRef, u: &InetRef) -> Result<InetRef, i64> {
    let _p = crate::prof::span("inet:accept");
    let nonblock = f.borrow().flags & file::O_NONBLOCK != 0;
    if !u.borrow().tcp {
        return Err(-EOPNOTSUPP);
    }
    loop {
        ext_accept(u);
        let (st, c, timeo) = {
            let mut s = u.borrow_mut();
            (s.state, s.acceptq.pop_front(), s.rcvtimeo)
        };
        if st != St::Listen {
            return Err(crate::why::inval());
        }
        if let Some(c) = c {
            {
                let mut x = c.borrow_mut();
                x.dead = false;
                x.has_file = true;
                x.ss = Ss::Connected;
            }
            drop_orphan(&c);
            notify(u);
            return Ok(c);
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        let chan = if u.borrow().ext_listen.is_some() { net::chan() } else { key(u) };
        wait(chan, timeo, 0)?;
    }
}

// ---- TCP: 受ける ----

/// 相手から届いたもの (Linux の tcp_urg、tcp_check_urg、tcp_data_queue)。urg なら最後のバイトが urgent
fn rx_data(s: &mut Inet, data: Vec<u8>, urg: bool) {
    let seq = s.rcv_nxt;
    let len = data.len() as u32;
    if urg && len > 0 {
        check_urg(s, seq.wrapping_add(len - 1));
    }
    if s.urg_data == URG_NOTYET {
        let off = s.urg_seq.wrapping_sub(seq) as usize;
        if off < data.len() {
            s.urg_data = URG_VALID | data[off] as u16;
        }
    }
    if len == 0 {
        return;
    }
    s.rq.push_back(Seg { seq, data, fin: false });
    s.rcv_nxt = s.rcv_nxt.wrapping_add(len);
}

/// 新しい urgent の場所 (Linux の tcp_check_urg)
fn check_urg(s: &mut Inet, ptr: u32) {
    if after(s.copied_seq, ptr) || before(ptr, s.rcv_nxt) {
        return;
    }
    if s.urg_data != 0 && !after(ptr, s.urg_seq) {
        return;
    }
    // 前の urgent のバイトを読むところだったら、それはとばす
    if s.urg_seq == s.copied_seq && s.urg_data != 0 && !s.oobinline && s.copied_seq != s.rcv_nxt {
        s.copied_seq = s.copied_seq.wrapping_add(1);
        if let Some(seg) = s.rq.front() {
            if !before(s.copied_seq, seg.end()) {
                s.rq.pop_front();
            }
        }
    }
    s.urg_data = URG_NOTYET;
    s.urg_seq = ptr;
}

/// FIN が届いた (Linux の tcp_fin)。FIN_WAIT2 は TIME_WAIT に (fd があれば、それは CLOSE になり、TIME_WAIT は別に残る)
fn rx_fin(u: &InetRef) {
    let st = {
        let mut s = u.borrow_mut();
        let seq = s.rcv_nxt;
        s.rq.push_back(Seg { seq, data: Vec::new(), fin: true });
        s.rcv_nxt = seq.wrapping_add(1);
        s.shutdown |= RCV;
        s.done = true;
        s.state
    };
    match st {
        St::SynRecv | St::Established => u.borrow_mut().state = St::CloseWait,
        St::FinWait1 => u.borrow_mut().state = St::Closing,
        St::FinWait2 => time_wait(u),
        _ => {}
    }
    notify(u);
}

/// TIME_WAIT に (Linux の tcp_time_wait)。fd のないものはそのまま TIME_WAIT で残り、fd のあるものは CLOSE になって、
/// 写しが TIME_WAIT で残る (ポートを持つ)
fn time_wait(u: &InetRef) {
    let dead = u.borrow().dead;
    let until = ticks() + FIN_TIMEOUT * HZ;
    if dead {
        {
            let mut s = u.borrow_mut();
            s.state = St::TimeWait;
            s.expires = until;
            s.peer = None;
        }
        keep_orphan(u);
        arm(u);
        return;
    }
    let tw = {
        let s = u.borrow();
        let tw = new_sock_as(true, s.protocol, s.uid, s.gid, s.net);
        {
            let mut t = tw.borrow_mut();
            t.state = St::TimeWait;
            t.dead = true;
            t.has_file = false;
            t.rcv_saddr = s.rcv_saddr;
            t.saddr = s.saddr;
            t.daddr = s.daddr;
            t.dport = s.dport;
            t.reuse = s.reuse;
            t.reuseport = s.reuseport;
            t.expires = until;
        }
        tw
    };
    let port = u.borrow().num;
    if port != 0 {
        hash_port(&tw, port);
    }
    keep_orphan(&tw);
    arm(&tw);
    tcp_done(u);
}

/// つながりの終わり (Linux の tcp_done): CLOSE、両方の shutdown。fd のないものは消す
fn tcp_done(u: &InetRef) {
    let (dead, lock_port) = {
        let mut s = u.borrow_mut();
        s.state = St::Close;
        s.shutdown = BOTH;
        s.peer = None;
        s.syn_next = 0;
        (s.dead, s.lock_port)
    };
    if !lock_port {
        put_port(u);
    }
    if dead {
        destroy(u);
    } else {
        notify(u);
    }
}

/// 消す (表とのこりものから出す)
fn destroy(u: &InetRef) {
    put_port(u);
    disarm(u);
    {
        let mut s = u.borrow_mut();
        s.rq.clear();
        s.acceptq.clear();
        s.peer = None;
        s.state = St::Close;
    }
    ext_close(u);
    drop_orphan(u);
}

/// TIME_WAIT のものなどを消す
fn kill(u: &InetRef) {
    destroy(u);
}

/// RST が届いた (Linux の tcp_reset): SYN_SENT は ECONNREFUSED、CLOSE_WAIT は EPIPE、ほかは ECONNRESET
fn tcp_reset(u: &InetRef) {
    let st = u.borrow().state;
    let err = match st {
        St::SynSent => ECONNREFUSED,
        St::CloseWait => EPIPE,
        St::Close => return,
        St::TimeWait | St::FinWait2 if u.borrow().dead => {
            kill(u);
            return;
        }
        _ => ECONNRESET,
    };
    u.borrow_mut().err = -err;
    tcp_done(u);
}

/// 相手に RST を送る
fn send_reset(u: &InetRef) {
    let peer = upgrade(&u.borrow().peer);
    if let Some(p) = peer {
        if !Rc::ptr_eq(&p, u) {
            tcp_reset(&p);
        }
    }
}

/// 相手に FIN を送る (相手が受けて ACK を返す: こちらは FIN_WAIT1 → FIN_WAIT2、LAST_ACK → CLOSE)
fn send_fin(u: &InetRef) {
    let peer = upgrade(&u.borrow().peer);
    let ext = u.borrow().ext;
    if let Some(h) = ext {
        tcp_sock(h).close();
        net::poll();
        return;
    }
    if let Some(p) = &peer {
        rx_fin(p);
        wnd_seen(p);
        wnd_seen(u);
    }
    // ACK が届く (自分につないだものは FIN も自分に届いている)
    let st = u.borrow().state;
    match st {
        St::FinWait1 => {
            {
                let mut s = u.borrow_mut();
                s.state = St::FinWait2;
                s.shutdown |= SEND;
            }
            notify(u);
        }
        St::LastAck | St::Closing => {
            if st == St::Closing {
                time_wait(u);
            } else {
                tcp_done(u);
            }
        }
        _ => {}
    }
}

/// FIN を送る状態の移りかた (Linux の tcp_close_state)。FIN を送るなら true
fn close_state(s: &mut Inet) -> bool {
    let (next, fin) = match s.state {
        St::Established | St::SynRecv => (St::FinWait1, true),
        St::CloseWait => (St::LastAck, true),
        St::SynSent | St::TimeWait | St::Close | St::Listen => (St::Close, false),
        other => (other, false),
    };
    s.state = next;
    fin
}

/// つながりを切る (Linux の tcp_disconnect: connect の AF_UNSPEC、SO_LINGER 0 の close、listen しているものの子)。
/// つながっていたら RST を送り、自分も ECONNRESET
fn tcp_disconnect(u: &InetRef) -> Result<(), i64> {
    let old = u.borrow().state;
    let lock_port = u.borrow().lock_port;
    if old != St::Close {
        u.borrow_mut().state = St::Close;
        if !lock_port {
            put_port(u);
        }
    }
    if old == St::Listen {
        listen_stop(u);
    } else if matches!(old, St::Established | St::CloseWait | St::FinWait1 | St::FinWait2 | St::SynRecv) {
        send_reset(u);
        u.borrow_mut().err = -ECONNRESET;
    } else if old == St::SynSent {
        u.borrow_mut().err = -ECONNRESET;
    }
    ext_close(u);
    {
        let mut s = u.borrow_mut();
        s.rq.clear();
        s.copied_seq = s.rcv_nxt;
        s.urg_data = 0;
        s.peek_off = -1;
        s.dport = 0;
        if !s.lock_addr {
            s.saddr = 0;
            s.rcv_saddr = 0;
        }
        s.shutdown = 0;
        s.done = false;
        s.user_mss = 0;
        s.window_clamp = 0;
        s.peer = None;
        s.syn_next = 0;
        s.syn_tries = 0;
    }
    notify(u);
    Ok(())
}

/// listen をやめる (Linux の inet_csk_listen_stop): accept されていない子はつながりを切る (相手に RST)
fn listen_stop(u: &InetRef) {
    let q: Vec<InetRef> = u.borrow_mut().acceptq.drain(..).collect();
    for c in q {
        let _ = tcp_disconnect(&c);
        destroy(&c);
    }
    if let Some(h) = u.borrow_mut().ext_listen.take() {
        if let Some(n) = net::get() {
            n.sockets.remove(h);
        }
    }
}

// ---- TCP: 読む ----

/// 受けたもの
pub struct Recv {
    pub data: Vec<u8>,
    /// 返す数 (MSG_TRUNC のデータグラムは入れ物より大きいことがある)
    pub ret: usize,
    /// 送り元の名前 (TCP はなし: msg_namelen は 0)
    pub from: Option<[u8; 16]>,
    /// msg_flags
    pub flags: u32,
    /// 付帯データ (level, type, 中身)
    pub cmsgs: Vec<(i32, i32, Vec<u8>)>,
}

impl Recv {
    fn new() -> Recv {
        Recv { data: Vec::new(), ret: 0, from: None, flags: 0, cmsgs: Vec::new() }
    }
}

/// MSG_OOB (Linux の tcp_recv_urg)
fn tcp_recv_urg(s: &mut Inet, len: usize, flags: u64) -> Result<Recv, i64> {
    if s.oobinline || s.urg_data == 0 || s.urg_data == URG_READ {
        return Err(crate::why::inval());
    }
    if s.state == St::Close && !s.done {
        return Err(-ENOTCONN);
    }
    let mut r = Recv::new();
    if s.urg_data & URG_VALID != 0 {
        let c = s.urg_data as u8;
        if flags & MSG_PEEK == 0 {
            s.urg_data = URG_READ;
        }
        r.flags |= MSG_OOB as u32;
        if len > 0 {
            if flags & MSG_TRUNC == 0 {
                r.data.push(c);
            }
            r.ret = 1;
        } else {
            r.flags |= MSG_TRUNC as u32;
        }
        return Ok(r);
    }
    if s.state == St::Close || s.shutdown & RCV != 0 {
        return Ok(r);
    }
    Err(-EAGAIN)
}

/// FIONREAD (Linux の tcp_inq): urgent のバイトが先にあれば、その手前まで。FIN の分は数えない
fn tcp_inq(s: &Inet) -> i32 {
    if matches!(s.state, St::SynSent | St::SynRecv) {
        return 0;
    }
    if s.oobinline || s.urg_data == 0 || before(s.urg_seq, s.copied_seq) || !before(s.urg_seq, s.rcv_nxt) {
        let mut n = s.rcv_nxt.wrapping_sub(s.copied_seq) as i32;
        if n != 0 && s.done {
            n -= 1;
        }
        n
    } else {
        s.urg_seq.wrapping_sub(s.copied_seq) as i32
    }
}

fn rcvlowat(s: &Inet, waitall: bool, len: usize) -> usize {
    let v = if waitall { len } else { (s.rcvlowat.max(0) as usize).min(len) };
    v.max(1)
}

/// TCP の recvmsg (Linux の tcp_recvmsg_locked): 読めるだけ (urgent のバイトの手前で止まる。SO_OOBINLINE でなければ
/// とばす)。読めるものがなければ FIN (0)、エラー、shutdown (0)、つながっていない (ENOTCONN)、EAGAIN か待つ
fn tcp_recv(u: &InetRef, len: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    let _p = crate::prof::span("inet:tcp_recv");
    if flags & MSG_ERRQUEUE != 0 {
        return recv_error(u, len);
    }
    if u.borrow().ext.is_some() {
        return ext_recv(u, len, flags, nonblock);
    }
    let peek = flags & MSG_PEEK != 0;
    let mut r = Recv::new();
    {
        let mut s = u.borrow_mut();
        if s.state == St::Listen {
            return Err(-ENOTCONN);
        }
        if flags & MSG_OOB != 0 {
            return tcp_recv_urg(&mut s, len, flags);
        }
    }
    let (mut peek_seq, peek_offset, target, used_before, cap_before) = {
        let s = u.borrow();
        let off = if peek && s.peek_off >= 0 { s.peek_off as u32 } else { 0 };
        let cap = if s.lock_rcvbuf { s.rcvbuf } else { TCP_QUEUE.max(s.rcvbuf) };
        (s.copied_seq.wrapping_add(off), off, rcvlowat(&s, flags & MSG_WAITALL != 0, len), s.rcv_nxt.wrapping_sub(s.copied_seq) as usize, cap)
    };
    let mut copied = 0usize;
    let mut left = len;
    let mut urg_hole = 0u32;
    let mut consumed = false;
    loop {
        let mut s = u.borrow_mut();
        let seq = if peek { peek_seq } else { s.copied_seq };
        if s.urg_data != 0 && s.urg_seq == seq && copied > 0 {
            break;
        }
        // seq をふくむもの
        let mut found = None;
        let mut fin_at = None;
        for (i, seg) in s.rq.iter().enumerate() {
            let off = seq.wrapping_sub(seg.seq) as usize;
            if off < seg.data.len() {
                found = Some((i, off));
                break;
            }
            if seg.fin && off == seg.data.len() {
                fin_at = Some(i);
                break;
            }
        }
        if let Some((i, mut off)) = found {
            let seglen = s.rq[i].data.len();
            let mut used = (seglen - off).min(left);
            let mut seq = seq;
            let mut skip = false;
            if s.urg_data != 0 {
                let urg_off = s.urg_seq.wrapping_sub(seq) as usize;
                if urg_off < used {
                    if urg_off == 0 {
                        if !s.oobinline {
                            seq = seq.wrapping_add(1);
                            urg_hole += 1;
                            off += 1;
                            used -= 1;
                            skip = used == 0;
                        }
                    } else {
                        used = urg_off;
                    }
                }
            }
            if !skip {
                if flags & MSG_TRUNC == 0 {
                    let d = &s.rq[i].data[off..off + used];
                    r.data.extend_from_slice(d);
                }
                seq = seq.wrapping_add(used as u32);
                copied += used;
                left -= used;
                if peek {
                    if s.peek_off >= 0 {
                        s.peek_off += used as i32;
                    }
                } else if s.peek_off >= 0 {
                    s.peek_off = (s.peek_off - used as i32).max(0);
                }
            }
            if peek {
                peek_seq = seq;
            } else {
                s.copied_seq = seq;
                consumed = true;
            }
            if s.urg_data != 0 && after(s.copied_seq, s.urg_seq) {
                s.urg_data = 0;
            }
            let done_seg = used + off >= seglen;
            if done_seg {
                // FIN まで読んだ (MSG_PEEK は FIN を残す)
                if s.rq[i].fin {
                    if !peek {
                        s.copied_seq = s.copied_seq.wrapping_add(1);
                        s.rq.remove(i);
                    }
                    break;
                }
                if !peek {
                    s.rq.remove(i);
                }
            }
            if left == 0 {
                break;
            }
            continue;
        }
        if let Some(i) = fin_at {
            if !peek {
                s.copied_seq = s.copied_seq.wrapping_add(1);
                s.rq.remove(i);
                consumed = true;
            }
            break;
        }
        // もうない
        if copied >= target {
            break;
        }
        let timeo_zero = nonblock || flags & MSG_DONTWAIT != 0;
        if copied > 0 {
            if timeo_zero || s.state == St::Close || s.err != 0 || s.shutdown & RCV != 0 {
                break;
            }
        } else {
            if s.done {
                break;
            }
            if s.err != 0 {
                let e = s.err;
                s.err = 0;
                return Err(e);
            }
            if s.shutdown & RCV != 0 {
                break;
            }
            if s.state == St::Close {
                return Err(-ENOTCONN);
            }
            if timeo_zero {
                return Err(-EAGAIN);
            }
        }
        let timeo = s.rcvtimeo;
        let wake_writer = consumed && s.writer_waits;
        if wake_writer {
            s.writer_waits = false;
        }
        drop(s);
        if wake_writer {
            wake_peer(u);
        }
        consumed = false;
        match wait(key(u), timeo, 0) {
            Ok(()) => {}
            Err(e) => {
                if copied > 0 {
                    break;
                }
                return Err(e);
            }
        }
        if peek {
            let s = u.borrow();
            if peek_seq.wrapping_sub(peek_offset).wrapping_sub(copied as u32).wrapping_sub(urg_hole) != s.copied_seq {
                peek_seq = s.copied_seq.wrapping_add(peek_offset);
            }
        }
    }
    // 読んであいたら、書く相手を起こす (待っているもの、poll で POLLOUT を待っているもの)
    let wake = {
        let mut s = u.borrow_mut();
        let w = consumed && (s.writer_waits || used_before * 3 > cap_before * 2);
        if w {
            s.writer_waits = false;
        }
        w
    };
    if wake {
        wake_peer(u);
    }
    r.ret = copied;
    Ok(r)
}

/// 書く相手 (行列があくのを待っている) を起こす
fn wake_peer(u: &InetRef) {
    let peer = upgrade(&u.borrow().peer);
    if let Some(p) = peer {
        notify(&p);
    }
}

// ---- TCP: 書く ----

/// 送るものの取り出し (ユーザーの iovec か、カーネルの中のもの)
pub use crate::unix::Src;

fn take(src: &Src, at: usize, n: usize) -> Result<Vec<u8>, i64> {
    match src {
        Src::Kernel(b) => Ok(b[at..at + n].to_vec()),
        Src::User(iov) => {
            let mut v = crate::heap::try_vec(n, false)?;
            let (mut off, mut done) = (at, 0);
            let pt = proc::current().pt();
            for &(base, l) in iov.iter() {
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

fn sigpipe(flags: u64) {
    if flags & MSG_NOSIGNAL == 0 {
        let info = crate::signal::SigInfo::from(crate::signal::SI_KERNEL);
        crate::signal::send_thread(proc::current(), crate::signal::SIGPIPE, info);
    }
}

/// Linux の sk_stream_error: EPIPE はたまっているエラーを先に、そして SIGPIPE (MSG_NOSIGNAL でなければ)
fn stream_error(u: &InetRef, flags: u64, err: i64) -> i64 {
    let mut e = err;
    if e == -EPIPE {
        let x = take_err(u);
        if x != 0 {
            e = x;
        }
    }
    if e == -EPIPE {
        sigpipe(flags);
    }
    e
}

/// 相手の行列のあき (自分につないだものは自分の行列)
fn room(u: &InetRef) -> Option<usize> {
    let p = upgrade(&u.borrow().peer)?;
    let s = p.borrow();
    let cap = if s.lock_rcvbuf { s.rcvbuf } else { TCP_QUEUE.max(s.rcvbuf) };
    let used = s.rcv_nxt.wrapping_sub(s.copied_seq) as usize;
    Some(cap.saturating_sub(used))
}

/// 相手 (aios の中) に届ける。相手が閉じていて読まない (FIN_WAIT2、TIME_WAIT で fd がない) なら RST が返る。
/// 相手が FIN を送ったあと (FIN_WAIT1 / FIN_WAIT2) で RCV も shutdown していれば、相手はこわれて (ECONNRESET) RST が返る
/// (Linux の tcp_rcv_state_process: RFC 1122 の MUST)
fn deliver(u: &InetRef, data: Vec<u8>, urg: bool) {
    let _p = crate::prof::span("inet:deliver");
    let Some(p) = upgrade(&u.borrow().peer) else { return };
    let (gone, abort) = {
        let s = p.borrow();
        let gone = s.dead && (s.shutdown & RCV != 0 || matches!(s.state, St::FinWait2 | St::TimeWait | St::Close));
        (gone, s.shutdown & RCV != 0 && matches!(s.state, St::FinWait1 | St::FinWait2))
    };
    if gone || abort {
        // 閉じたあとに届いたもの: 相手は消え、RST が返る
        if !Rc::ptr_eq(&p, u) {
            if gone {
                kill(&p);
            } else {
                tcp_reset(&p);
            }
        }
        tcp_reset(u);
        return;
    }
    rx_data(&mut p.borrow_mut(), data, urg);
    // データの segment と、その ACK がたがいの窓を知らせる
    wnd_seen(&p);
    wnd_seen(u);
    notify(&p);
}

/// TCP の sendmsg (Linux の tcp_sendmsg_locked): つながるのを待つ (つながっていなければ EPIPE)、付帯データの
/// エラー、自分のエラーか SEND の shutdown で EPIPE、相手の行列があくまで待つ
fn tcp_send(u: &InetRef, src: &Src, len: usize, ctl: &[u8], flags: u64, nonblock: bool) -> R {
    let _p = crate::prof::span("inet:tcp_send");
    let ctl_err = sock_cmsg_check(ctl, true).err();
    let nb = nonblock || flags & MSG_DONTWAIT != 0;
    loop {
        ext_sync(u);
        let (st, timeo, next) = {
            let s = u.borrow();
            (s.state, s.sndtimeo, s.syn_next)
        };
        if matches!(st, St::Established | St::CloseWait) {
            break;
        }
        let e = take_err(u);
        if e != 0 {
            return Err(stream_error(u, flags, e));
        }
        if !matches!(st, St::SynSent | St::SynRecv) {
            return Err(stream_error(u, flags, -EPIPE));
        }
        if nb {
            return Err(-EAGAIN);
        }
        let chan = if u.borrow().ext.is_some() { net::chan() } else { key(u) };
        wait(chan, timeo, next)?;
    }
    if let Some(e) = ctl_err {
        return Err(e);
    }
    {
        let s = u.borrow();
        if s.err != 0 || s.shutdown & SEND != 0 {
            drop(s);
            return Err(stream_error(u, flags, -EPIPE));
        }
    }
    if len == 0 {
        return Ok(0);
    }
    if u.borrow().ext.is_some() {
        return ext_send(u, src, len, flags, nb);
    }
    let oob = flags & MSG_OOB != 0;
    // ためる (Linux の TCP_NAGLE_CORK: MSS に満たない終わりは送らない。urgent のものはためない)
    let hold = (flags & MSG_MORE != 0 || u.borrow().cork) && !oob;
    let mut done = 0;
    while done < len {
        let Some(space) = room(u) else {
            // 相手がいない (切れた)
            return if done > 0 { Ok(done as i64) } else { Err(stream_error(u, flags, -EPIPE)) };
        };
        if space == 0 {
            if done > 0 && nb {
                break;
            }
            if nb {
                return Err(-EAGAIN);
            }
            let peer = upgrade(&u.borrow().peer);
            if let Some(p) = peer {
                p.borrow_mut().writer_waits = true;
            }
            let timeo = u.borrow().sndtimeo;
            if let Err(e) = wait(key(u), timeo, 0) {
                return if done > 0 { Ok(done as i64) } else { Err(e) };
            }
            let s = u.borrow();
            if s.err != 0 || s.shutdown & SEND != 0 || !matches!(s.state, St::Established | St::CloseWait) {
                drop(s);
                return if done > 0 { Ok(done as i64) } else { Err(stream_error(u, flags, -EPIPE)) };
            }
            continue;
        }
        let corked = u.borrow().cork_buf.len();
        let n = (len - done).min(space.saturating_sub(corked).max(1)).min(1 << 20);
        let chunk = take(src, done, n)?;
        done += n;
        if hold || corked > 0 {
            let mss = u.borrow().mss_cache.max(1) as usize;
            let out = {
                let mut s = u.borrow_mut();
                s.cork_buf.extend_from_slice(&chunk);
                if !hold {
                    Some(core::mem::take(&mut s.cork_buf))
                } else if s.cork_buf.len() >= mss {
                    let keep = s.cork_buf.len() % mss;
                    let at = s.cork_buf.len() - keep;
                    let rest = s.cork_buf.split_off(at);
                    Some(core::mem::replace(&mut s.cork_buf, rest))
                } else {
                    None
                }
            };
            if let Some(b) = out {
                deliver(u, b, oob && done == len);
            }
        } else {
            deliver(u, chunk, oob && done == len);
        }
        if u.borrow().state == St::Close {
            break;
        }
    }
    Ok(done as i64)
}

/// 付帯データ (Linux の sock_cmsg_send と ip_cmsg_send): cmsghdr の形、SOL_SOCKET の知らないものは EINVAL
/// (SCM_RIGHTS と SCM_CREDENTIALS は AF_UNIX のものなので見ない)、IPPROTO_IP (UDP だけ) の IP_TTL / IP_TOS /
/// IP_PKTINFO。(ttl, tos) を返す
fn sock_cmsg_check(ctl: &[u8], tcp: bool) -> Result<(Option<u8>, Option<u8>), i64> {
    const SOL_IP: i32 = 0;
    const SOL_SOCKET: i32 = 1;
    let (mut ttl, mut tos) = (None, None);
    let mut off = 0usize;
    while off + 16 <= ctl.len() {
        let clen = u64::from_le_bytes(ctl[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(ctl[off + 8..off + 12].try_into().unwrap());
        let typ = i32::from_le_bytes(ctl[off + 12..off + 16].try_into().unwrap());
        if clen < 16 || clen > ctl.len() - off {
            return Err(crate::why::inval());
        }
        let data = &ctl[off + 16..off + clen];
        let int = || if data.len() >= 4 { Some(i32::from_le_bytes(data[..4].try_into().unwrap())) } else { None };
        if level == SOL_SOCKET {
            match typ {
                1 | 2 => {}
                // SO_PRIORITY
                12 => {
                    if clen != 20 {
                        return Err(crate::why::inval());
                    }
                    if int().unwrap_or(0) as u32 > 6 && !crate::cred::net_prio_ok(proc::current_cred_ref()) {
                        return Err(-EPERM);
                    }
                }
                // SO_MARK
                36 => {
                    if !crate::cred::net_prio_ok(proc::current_cred_ref()) {
                        return Err(-EPERM);
                    }
                    if clen != 20 {
                        return Err(crate::why::inval());
                    }
                }
                // SO_TIMESTAMPING
                37 | 65 => {
                    if clen != 20 {
                        return Err(crate::why::inval());
                    }
                }
                _ => return Err(crate::why::inval()),
            }
        } else if level == SOL_IP && !tcp {
            match typ {
                // IP_TTL
                2 => {
                    if clen != 20 {
                        return Err(crate::why::inval());
                    }
                    let v = int().unwrap_or(0);
                    if !(1..=255).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    ttl = Some(v as u8);
                }
                // IP_TOS
                1 => {
                    let v = if clen == 20 { int().unwrap_or(0) } else if clen == 17 { data[0] as i32 } else { return Err(crate::why::inval()) };
                    if !(0..=255).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    tos = Some(v as u8);
                }
                // IP_PKTINFO
                8 => {
                    if clen != 28 {
                        return Err(crate::why::inval());
                    }
                }
                // IP_RETOPTS
                7 => {}
                _ => return Err(crate::why::inval()),
            }
        }
        off += clen.div_ceil(8) * 8;
    }
    Ok((ttl, tos))
}

// ---- UDP ----

/// データグラムの数えかた (Linux 7.3 arm64 の skb の truesize): 頭 (中身 + UDP 8 + IP 20 + 16) が 16064 に入れば
/// kmalloc の箱 (640 まではそれ用の箱、そのうえは 2 のべき) + struct sk_buff (256)、入らなければ小さな頭 (896) と
/// ページの分
fn udp_truesize(len: usize) -> usize {
    if len + 28 + 16 >= 16064 {
        return 896 + len;
    }
    let sz = (len + 44).div_ceil(64) * 64 + 320;
    let a = if sz <= 640 { 640 } else { sz.next_power_of_two() };
    a + 256
}

/// 届け先 (Linux の __udp4_lib_lookup: そのアドレスに bind したものを先に、なければ 0.0.0.0 のもの。connect した
/// ものは相手が同じときだけ。点の多いもの、同じなら新しいもの)
fn udp_lookup(netns: u64, saddr: u32, sport: u16, daddr: u32, dport: u16) -> Option<InetRef> {
    let v = ports().get(&(netns, false, dport))?;
    let score = |o: &Inet, want: u32| -> i32 {
        if o.rcv_saddr != want {
            return -1;
        }
        let mut sc = 2;
        if o.daddr != 0 {
            if o.daddr != saddr {
                return -1;
            }
            sc += 4;
        }
        if o.dport != 0 {
            if o.dport != sport {
                return -1;
            }
            sc += 4;
        }
        sc
    };
    for want in [daddr, 0] {
        let mut best: Option<(i32, InetRef)> = None;
        for o in v.values().rev().filter_map(|w| w.upgrade()) {
            let sc = score(&o.borrow(), want);
            if sc > 0 && best.as_ref().is_none_or(|(b, _)| sc > *b) {
                best = Some((sc, o));
            }
        }
        if let Some((_, o)) = best {
            return Some(o);
        }
    }
    None
}

/// UDP の connect (Linux の inet_dgram_connect と __ip4_datagram_connect): AF_UNSPEC は相手を外す。先に autobind。
/// 送り元のアドレスが決まっていなければ、みちの送り元に
fn udp_connect(u: &InetRef, a: &[u8]) -> R {
    if a.len() < 2 {
        return Err(crate::why::inval());
    }
    let (fam, port, addr) = parse(a);
    if fam == AF_UNSPEC {
        udp_disconnect(u);
        return Ok(0);
    }
    autobind(u)?;
    if a.len() < 16 {
        return Err(crate::why::inval());
    }
    if fam != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let (netns, saddr0, bcast) = {
        let s = u.borrow();
        (s.net, s.saddr, s.broadcast)
    };
    let (daddr, src) = if is_multicast(addr) {
        (addr, if saddr0 != 0 { saddr0 } else { eth() })
    } else {
        let (d, s, _) = route(netns, addr, saddr0)?;
        (d, s)
    };
    if is_broadcast(daddr) && !bcast {
        return Err(-EACCES);
    }
    let mut s = u.borrow_mut();
    s.daddr = daddr;
    s.dport = port;
    if s.saddr == 0 {
        s.saddr = src;
    }
    if s.rcv_saddr == 0 {
        s.rcv_saddr = src;
    }
    s.state = St::Established;
    Ok(0)
}

/// 相手を外す (Linux の __udp_disconnect: bind で決めていないアドレスとポートも手放す)
fn udp_disconnect(u: &InetRef) {
    let unhash = {
        let mut s = u.borrow_mut();
        s.state = St::Close;
        s.daddr = 0;
        s.dport = 0;
        if !s.lock_addr {
            s.saddr = 0;
            s.rcv_saddr = 0;
        }
        !s.lock_port
    };
    if unhash {
        put_port(u);
        ext_close(u);
        u.borrow_mut().sport = 0;
    }
}

/// UDP の sendmsg (Linux の udp_sendmsg): 長さ、MSG_OOB、ためているもの (MSG_MORE) につなげる、あて先 (なければ
/// connect したもの: なければ EDESTADDRREQ)、付帯データ、みち、たまっているエラーと SEND の shutdown (EPIPE)
fn udp_send(u: &InetRef, src: &Src, len: usize, to: Option<&[u8]>, ctl: &[u8], flags: u64) -> R {
    let _p = crate::prof::span("inet:udp_send");
    if len > 0xffff {
        return Err(-EMSGSIZE);
    }
    if flags & MSG_OOB != 0 {
        return Err(-EOPNOTSUPP);
    }
    let corkreq = flags & MSG_MORE != 0;
    let pending = u.borrow().pending.as_ref().map(|p| (p.1, p.2, p.0.len()));
    let (daddr, dport, ttl, tos) = if let Some((d, p, _)) = pending {
        (d, p, None, None)
    } else {
        let (d, p) = match to {
            Some(a) => {
                if a.len() < 16 {
                    return Err(crate::why::inval());
                }
                let (fam, port, addr) = parse(a);
                if fam != AF_INET && fam != AF_UNSPEC {
                    return Err(-EAFNOSUPPORT);
                }
                if port == 0 {
                    return Err(crate::why::inval());
                }
                (addr, port)
            }
            None => {
                let s = u.borrow();
                if s.state != St::Established {
                    return Err(-EDESTADDRREQ);
                }
                (s.daddr, s.dport)
            }
        };
        let (ttl, tos) = sock_cmsg_check(ctl, false)?;
        (d, p, ttl, tos)
    };
    let (netns, saddr0, bcast) = {
        let s = u.borrow();
        (s.net, s.saddr, s.broadcast)
    };
    let (daddr, saddr, local) = if is_multicast(daddr) || daddr == u32::MAX {
        if !bcast && daddr == u32::MAX {
            return Err(-EACCES);
        }
        (daddr, if saddr0 != 0 { saddr0 } else { eth() }, false)
    } else {
        route(netns, daddr, saddr0)?
    };
    if is_broadcast(daddr) && !bcast {
        return Err(-EACCES);
    }
    // 送る入れ物を取る (Linux の sock_alloc_send_pskb: たまっているエラー、SEND の shutdown)
    let e = take_err(u);
    if e != 0 {
        return Err(e);
    }
    if u.borrow().shutdown & SEND != 0 {
        return Err(-EPIPE);
    }
    let already = pending.map_or(0, |p| p.2);
    if already + len + 8 > 0xffff - 20 {
        u.borrow_mut().pending = None;
        return Err(-EMSGSIZE);
    }
    let data = take(src, 0, len)?;
    if corkreq {
        let mut s = u.borrow_mut();
        match s.pending.as_mut() {
            Some(p) => p.0.extend_from_slice(&data),
            None => s.pending = Some((data, daddr, dport)),
        }
        return Ok(len as i64);
    }
    let data = match u.borrow_mut().pending.take() {
        Some((mut p, _, _)) => {
            p.extend_from_slice(&data);
            p
        }
        None => data,
    };
    if !local {
        ext_udp_send(u, &data, daddr, dport, ttl)?;
        return Ok(len as i64);
    }
    let (sport, my_tos, my_ttl) = {
        let s = u.borrow();
        (s.sport, s.tos, s.uc_ttl)
    };
    let ttl = ttl.unwrap_or(if my_ttl < 0 { 64 } else { my_ttl as u8 });
    let tos = tos.unwrap_or(my_tos);
    match udp_lookup(netns, saddr, sport, daddr, dport) {
        Some(r) => {
            let ts = udp_truesize(data.len());
            let ok = {
                let mut x = r.borrow_mut();
                let full = x.rmem + ts > x.rcvbuf && x.rmem != 0;
                if !full {
                    x.rmem += ts;
                    x.dq.push_back(Dgram { data, from: (saddr, sport), to: daddr, tos, ttl, truesize: ts });
                    true
                } else {
                    false
                }
            };
            if ok {
                notify(&r);
            }
        }
        // だれもいない: ICMP の port unreachable が返る (Linux の __udp4_lib_err)。connect した相手そのものか、
        // IP_RECVERR なら、エラーを覚える
        None => icmp_unreach(u, &data, daddr, dport, saddr),
    }
    Ok(len as i64)
}

/// ICMP の port unreachable を受けた (Linux の __udp4_lib_err と ip_icmp_error)
fn icmp_unreach(u: &InetRef, data: &[u8], daddr: u32, dport: u16, saddr: u32) {
    let deliver = {
        let s = u.borrow();
        // 自分あてか (送り元と相手で、このソケットが受けるものか)
        let mine = s.rcv_saddr == 0 || s.rcv_saddr == saddr;
        let peer_ok = s.daddr == 0 || (s.daddr == daddr && (s.dport == 0 || s.dport == dport));
        mine && peer_ok && (s.recverr || s.state == St::Established)
    };
    if !deliver {
        return;
    }
    {
        let mut s = u.borrow_mut();
        if s.recverr {
            // ICMP にのるのは 576 バイトまで (IP と ICMP の頭、送ったものの IP と UDP の頭をのぞいた中身)
            let keep = data.len().min(576 - 20 - 8 - 20 - 8);
            s.errq.push_back(ErrMsg { data: data[..keep].to_vec(), errno: ECONNREFUSED as u32, typ: 3, code: 3, to: (daddr, dport), offender: daddr });
        }
        s.err = -ECONNREFUSED;
    }
    notify(u);
}

/// IP_RECVERR の付帯データ (struct sock_extended_err と、知らせてきたアドレスの sockaddr_in)
fn exterr(m: &ErrMsg) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(&m.errno.to_le_bytes());
    // ee_origin (SO_EE_ORIGIN_ICMP)、ee_type、ee_code、ee_pad
    b.extend_from_slice(&[2, m.typ, m.code, 0]);
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&sockaddr(m.offender, 0));
    b
}

/// MSG_ERRQUEUE (Linux の ip_recv_error): なければ EAGAIN。読んだら sk_err を次のものの errno に
fn recv_error(u: &InetRef, len: usize) -> Result<Recv, i64> {
    let mut s = u.borrow_mut();
    let Some(m) = s.errq.pop_front() else { return Err(-EAGAIN) };
    let mut r = Recv::new();
    let n = m.data.len().min(len);
    if n < m.data.len() {
        r.flags |= MSG_TRUNC as u32;
    }
    r.data = m.data[..n].to_vec();
    r.ret = n;
    r.from = Some(sockaddr(m.to.0, m.to.1));
    r.flags |= MSG_ERRQUEUE as u32;
    // IP_RECVERR (SOL_IP 0、11)
    r.cmsgs.push((0, 11, exterr(&m)));
    s.err = s.errq.front().map_or(0, |x| -(x.errno as i64));
    Ok(r)
}

/// UDP の recvmsg (Linux の udp_recvmsg と __skb_recv_udp): たまっているエラーが先、はじめのデータグラム (MSG_PEEK
/// と SO_PEEK_OFF)、入れ物より大きければ MSG_TRUNC (返す数は、MSG_TRUNC を渡せばデータグラムの長さ)
fn udp_recv(u: &InetRef, len: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    let _p = crate::prof::span("inet:udp_recv");
    if flags & MSG_ERRQUEUE != 0 {
        return recv_error(u, len);
    }
    let peek = flags & MSG_PEEK != 0;
    loop {
        ext_udp_pull(u);
        let mut s = u.borrow_mut();
        if s.err != 0 {
            let e = s.err;
            s.err = 0;
            return Err(e);
        }
        // SO_PEEK_OFF: MSG_PEEK はその先から (いくつものデータグラムをまたぐ)
        let mut off = if peek && s.peek_off >= 0 { s.peek_off as usize } else { 0 };
        let mut idx = None;
        for (i, d) in s.dq.iter().enumerate() {
            if peek && off > 0 && off >= d.data.len() {
                off -= d.data.len();
                continue;
            }
            idx = Some(i);
            break;
        }
        if let Some(i) = idx {
            let (pktinfo, recvtos, recvttl) = (s.pktinfo, s.recvtos, s.recvttl);
            let d = &s.dq[i];
            let ulen = d.data.len();
            let mut r = Recv::new();
            let copied = len.min(ulen.saturating_sub(off));
            if copied < ulen - off.min(ulen) {
                r.flags |= MSG_TRUNC as u32;
            }
            r.data = d.data[off..off + copied].to_vec();
            r.from = Some(sockaddr(d.from.0, d.from.1));
            if pktinfo {
                let mut b = Vec::with_capacity(12);
                b.extend_from_slice(&1u32.to_le_bytes());
                b.extend_from_slice(&d.to.to_be_bytes());
                b.extend_from_slice(&d.to.to_be_bytes());
                r.cmsgs.push((0, 8, b));
            }
            if recvttl {
                r.cmsgs.push((0, 2, (d.ttl as i32).to_le_bytes().to_vec()));
            }
            if recvtos {
                r.cmsgs.push((0, 1, alloc::vec![d.tos]));
            }
            r.ret = if flags & MSG_TRUNC != 0 { ulen } else { copied };
            if peek {
                if s.peek_off >= 0 {
                    s.peek_off += r.ret as i32;
                }
            } else {
                let d = s.dq.remove(i).unwrap();
                s.rmem -= d.truesize;
                if s.peek_off >= 0 {
                    s.peek_off = (s.peek_off - r.ret as i32).max(0);
                }
            }
            return Ok(r);
        }
        if nonblock || flags & MSG_DONTWAIT != 0 {
            return Err(-EAGAIN);
        }
        if s.shutdown & RCV != 0 {
            return Ok(Recv::new());
        }
        let timeo = s.rcvtimeo;
        let chan = if s.ext.is_some() { net::chan() } else { key(u) };
        drop(s);
        wait(chan, timeo, 0)?;
    }
}

// ---- 送る・受ける (socket.rs から) ----

/// sendmsg / sendto / write
pub fn send(u: &InetRef, src: Src, len: usize, to: Option<&[u8]>, ctl: &[u8], flags: u64, nonblock: bool) -> R {
    if u.borrow().tcp {
        // TCP はあて先を見ない (connect したところへ)
        let r = tcp_send(u, &src, len, ctl, flags, nonblock);
        return r;
    }
    // UDP: 送る前に autobind (Linux の inet_send_prepare)
    autobind(u)?;
    udp_send(u, &src, len, to, ctl, flags)
}

/// recvmsg / recvfrom / read
pub fn recv(u: &InetRef, len: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    if u.borrow().tcp { tcp_recv(u, len, flags, nonblock) } else { udp_recv(u, len, flags, nonblock) }
}

pub fn read(u: &InetRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    // read の長さ 0 は何もしない (Linux の sock_read_iter)
    if dst.is_empty() {
        return Ok(0);
    }
    let r = recv(u, dst.len(), 0, nonblock)?;
    dst[..r.data.len()].copy_from_slice(&r.data);
    Ok(r.data.len().min(r.ret))
}

pub fn write(u: &InetRef, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    send(u, Src::Kernel(src), src.len(), None, &[], 0, nonblock).map(|n| n as usize)
}

/// shutdown (Linux の inet_shutdown と tcp_shutdown): how は 0〜2。つながっていなければ ENOTCONN (それでも shutdown
/// の印はつく)。listen しているものは SHUT_RD で listen をやめ、SYN_SENT のものはつなぐのをやめる
pub fn shutdown(u: &InetRef, how: u64) -> R {
    let how = (how as u32 as i32).wrapping_add(1);
    if how & !3 != 0 || how == 0 {
        return Err(crate::why::inval());
    }
    let how = how as u8;
    {
        let mut s = u.borrow_mut();
        if s.ss == Ss::Connecting {
            s.ss = if matches!(s.state, St::SynSent | St::SynRecv | St::Close) { Ss::Disconnecting } else { Ss::Connected };
        }
    }
    let st = u.borrow().state;
    let mut err = 0;
    match st {
        St::Listen => {
            if how & RCV != 0 {
                let r = tcp_disconnect(u);
                u.borrow_mut().ss = if r.is_err() { Ss::Disconnecting } else { Ss::Unconnected };
            }
        }
        St::SynSent => {
            let r = tcp_disconnect(u);
            u.borrow_mut().ss = if r.is_err() { Ss::Disconnecting } else { Ss::Unconnected };
        }
        _ => {
            if st == St::Close {
                err = -ENOTCONN;
            }
            let tcp = {
                let mut s = u.borrow_mut();
                s.shutdown |= how;
                s.tcp
            };
            if tcp && how & SEND != 0 && matches!(st, St::Established | St::CloseWait) {
                flush_cork(u);
                let fin = close_state(&mut u.borrow_mut());
                if fin {
                    send_fin(u);
                }
            }
        }
    }
    notify(u);
    if err != 0 { Err(err) } else { Ok(0) }
}

/// MSG_MORE と TCP_CORK でためたもの (TCP) を送る
fn flush_cork(u: &InetRef) {
    let b = core::mem::take(&mut u.borrow_mut().cork_buf);
    if !b.is_empty() {
        deliver(u, b, false);
    }
}

// ---- poll と ioctl ----

/// poll のビット (Linux の tcp_poll と udp_poll / datagram_poll)
pub fn poll_mask(u: &InetRef) -> u32 {
    ext_sync(u);
    ext_accept(u);
    ext_udp_pull(u);
    let tcp = u.borrow().tcp;
    if !tcp {
        let s = u.borrow();
        let mut m = 0;
        if s.err != 0 || !s.errq.is_empty() {
            m |= POLLERR;
        }
        if s.shutdown & RCV != 0 {
            m |= POLLRDHUP | POLLIN | POLLRDNORM;
        }
        if s.shutdown == BOTH {
            m |= POLLHUP;
        }
        if !s.dq.is_empty() {
            m |= POLLIN | POLLRDNORM;
        }
        return m | POLLOUT | POLLWRNORM | POLLWRBAND;
    }
    let writable = writable(u);
    let s = u.borrow();
    if s.state == St::Listen {
        return if s.acceptq.is_empty() { 0 } else { POLLIN | POLLRDNORM };
    }
    let mut m = 0;
    if s.shutdown == BOTH || s.state == St::Close {
        m |= POLLHUP;
    }
    if s.shutdown & RCV != 0 {
        m |= POLLIN | POLLRDNORM | POLLRDHUP;
    }
    if !matches!(s.state, St::SynSent | St::SynRecv) {
        let mut target = s.rcvlowat.max(1) as i64;
        if s.urg_data != 0 && s.urg_seq == s.copied_seq && !s.oobinline {
            target += 1;
        }
        let avail = s.rcv_nxt.wrapping_sub(s.copied_seq) as i32 as i64;
        if avail > 0 && avail >= target {
            m |= POLLIN | POLLRDNORM;
        }
        if s.ext.is_some() {
            if let Some(h) = s.ext {
                let t = tcp_sock(h);
                if t.can_recv() || (!t.may_recv() && t.state() != tcp::State::SynSent) {
                    m |= POLLIN | POLLRDNORM;
                }
            }
        }
        if s.shutdown & SEND == 0 {
            if writable {
                m |= POLLOUT | POLLWRNORM;
            }
        } else {
            m |= POLLOUT | POLLWRNORM;
        }
        if s.urg_data & URG_VALID != 0 {
            m |= POLLPRI;
        }
    }
    if s.err != 0 || !s.errq.is_empty() {
        m |= POLLERR;
    }
    m
}

/// 書けるか (Linux の __sk_stream_is_writeable: 入れ物の 2/3 まで。相手の行列で数える)
fn writable(u: &InetRef) -> bool {
    let ext = u.borrow().ext;
    if let Some(h) = ext {
        return tcp_sock(h).can_send() || !tcp_sock(h).may_send();
    }
    let Some(p) = upgrade(&u.borrow().peer) else { return true };
    let s = p.borrow();
    let cap = if s.lock_rcvbuf { s.rcvbuf } else { TCP_QUEUE.max(s.rcvbuf) };
    let used = s.rcv_nxt.wrapping_sub(s.copied_seq) as usize;
    used * 3 <= cap * 2
}

/// poll で待つときの印: 自分 (外とのものは net::chan() も)
pub fn poll_keys(u: &InetRef) -> Vec<usize> {
    let mut v = alloc::vec![key(u)];
    let s = u.borrow();
    if s.ext.is_some() || s.ext_listen.is_some() {
        v.push(net::chan());
    }
    v
}

/// FIONREAD (SIOCINQ): TCP は読めるバイト (listen しているものは EINVAL)、UDP ははじめのデータグラムの長さ
pub fn inq(u: &InetRef) -> Result<i32, i64> {
    ext_sync(u);
    ext_udp_pull(u);
    let s = u.borrow();
    if !s.tcp {
        return Ok(s.dq.front().map_or(0, |d| d.data.len() as i32));
    }
    if s.state == St::Listen {
        return Err(crate::why::inval());
    }
    if let Some(h) = s.ext {
        return Ok(tcp_sock(h).recv_queue() as i32);
    }
    Ok(tcp_inq(&s))
}

/// SIOCOUTQ: 送って ACK の来ていないもの (aios の中は届いたときに ACK も来ている)
pub fn outq(u: &InetRef) -> Result<i32, i64> {
    let s = u.borrow();
    if s.tcp && s.state == St::Listen {
        return Err(crate::why::inval());
    }
    if let Some(h) = s.ext {
        if s.tcp {
            return Ok(tcp_sock(h).send_queue() as i32);
        }
    }
    Ok(0)
}

/// SIOCATMARK (TCP だけ: 次に読むところが urgent のバイト)
pub fn atmark(u: &InetRef) -> Result<i32, i64> {
    let s = u.borrow();
    if !s.tcp {
        return Err(-25); // ENOTTY
    }
    Ok((s.urg_data != 0 && s.urg_seq == s.copied_seq) as i32)
}

/// /proc/ai/fd などの、読まれずに残っているもの
pub fn pending(u: &InetRef) -> u64 {
    let s = u.borrow();
    if !s.tcp {
        return s.dq.iter().map(|d| d.data.len() as u64).sum();
    }
    // FIN の分 (rcv_nxt に 1 つ入っている) は読めるバイトではない (Linux の tcp_inq と同じ。相手が閉じたあとの
    // 読みは、入れ物にさわれなくても 0)
    let n = s.rcv_nxt.wrapping_sub(s.copied_seq) as u64;
    if n != 0 && s.done { n - 1 } else { n }
}

// ---- 名前 ----

/// getsockname (peer = false) と getpeername / accept の名前 (peer = 1 は getpeername: CLOSE と SYN_SENT は
/// ENOTCONN。2 は accept: 状態を見ない)
pub fn name(u: &InetRef, peer: u8) -> Result<[u8; 16], i64> {
    let s = u.borrow();
    if peer != 0 {
        if s.dport == 0 || (matches!(s.state, St::Close | St::SynSent) && peer == 1) {
            return Err(-ENOTCONN);
        }
        return Ok(sockaddr(s.daddr, s.dport));
    }
    let a = if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr };
    Ok(sockaddr(a, s.sport))
}

// ---- sockopt ----

const SOL_SOCKET: i32 = 1;
const IPPROTO_IP: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;

fn timeval(ns: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
    b.extend_from_slice(&(((ns % 1_000_000_000) / 1000) as i64).to_le_bytes());
    b
}

/// tcp_rmem[2] (SO_RCVLOWAT の上限は、その半分。Linux 7.3 で 2 GiB のマシン)
const TCP_RMEM_MAX: usize = 16282880;

/// getsockopt。len は入れ物の長さ (返すものはそこまでに切る)
pub fn getsockopt(u: &InetRef, level: i32, opt: i32, len: i32) -> Result<Vec<u8>, i64> {
    if len < 0 {
        return Err(crate::why::inval());
    }
    let int = |v: i64| (v as i32).to_le_bytes().to_vec();
    let tcp = u.borrow().tcp;
    let mut v = match level {
        SOL_SOCKET => {
            let mut s = u.borrow_mut();
            match opt {
                1 => int(0),
                2 => int(s.reuse as i64),
                3 => int(if s.tcp { 1 } else { 2 }),
                4 => {
                    let e = s.err;
                    s.err = 0;
                    int(-e)
                }
                5 => int(s.dontroute as i64),
                6 => int(s.broadcast as i64),
                7 => int(s.sndbuf as i64),
                8 => int(s.rcvbuf as i64),
                9 => int(s.keepalive as i64),
                10 => int(s.oobinline as i64),
                11 => int(0),
                12 => int(s.priority as i64),
                13 => {
                    let mut b = int(s.linger as i64);
                    b.extend_from_slice(&int(s.linger_secs.min(i32::MAX as u64) as i64));
                    b
                }
                14 => int(0),
                15 => int(s.reuseport as i64),
                16 => return Err(-EOPNOTSUPP),
                17 => {
                    let mut b = Vec::with_capacity(12);
                    b.extend_from_slice(&0u32.to_le_bytes());
                    b.extend_from_slice(&u32::MAX.to_le_bytes());
                    b.extend_from_slice(&u32::MAX.to_le_bytes());
                    b
                }
                18 => int(s.rcvlowat as i64),
                19 => int(1),
                20 | 66 => timeval(s.rcvtimeo),
                21 | 67 => timeval(s.sndtimeo),
                30 => int((s.state == St::Listen) as i64),
                38 => int(s.protocol as i64),
                39 => int(2),
                42 => int(s.peek_off as i64),
                _ => return Err(-ENOPROTOOPT),
            }
        }
        IPPROTO_TCP if tcp => {
            let s = u.borrow();
            let val = match opt {
                // TCP_MAXSEG: CLOSE と LISTEN は決めたもの
                2 => {
                    if s.user_mss != 0 && matches!(s.state, St::Close | St::Listen) {
                        s.user_mss
                    } else {
                        s.mss_cache
                    }
                }
                1 => s.nodelay as i32,
                3 => s.cork as i32,
                4 => if s.keepidle != 0 { s.keepidle } else { KEEPIDLE },
                5 => if s.keepintvl != 0 { s.keepintvl } else { KEEPINTVL },
                6 => if s.keepcnt != 0 { s.keepcnt } else { KEEPCNT },
                7 => if s.syncnt != 0 { s.syncnt } else { SYN_RETRIES },
                8 => {
                    if s.linger2 < 0 {
                        -1
                    } else if s.linger2 != 0 {
                        s.linger2
                    } else {
                        FIN_TIMEOUT as i32
                    }
                }
                9 => 0,
                10 => s.window_clamp,
                12 => 1,
                18 => s.user_timeout as i32,
                25 => s.notsent_lowat as i32,
                _ => return Err(-ENOPROTOOPT),
            };
            let n = (len as usize).min(4);
            let mut b = int(val as i64);
            b.truncate(n);
            return Ok(b);
        }
        IPPROTO_UDP if !tcp => return Err(-ENOPROTOOPT),
        IPPROTO_IP => {
            let s = u.borrow();
            let val: i32 = match opt {
                1 => s.tos as i32,
                2 => if s.uc_ttl < 0 { 64 } else { s.uc_ttl },
                3 => 0,
                8 => s.pktinfo as i32,
                10 => s.pmtudisc as i32,
                11 => s.recverr as i32,
                12 => s.recvttl as i32,
                13 => s.recvtos as i32,
                // IP_MTU: つながっていなければ ENOTCONN (ループバックは 65535)
                14 => {
                    let connected = if s.tcp { !matches!(s.state, St::Close | St::Listen | St::SynSent) } else { s.state == St::Established };
                    if !connected {
                        return Err(-ENOTCONN);
                    }
                    if s.ext.is_some() { 1500 } else { 65535 }
                }
                15 => s.freebind as i32,
                21 => s.minttl as i32,
                24 => s.bind_no_port as i32,
                33 => s.mc_ttl as i32,
                34 => s.mc_loop as i32,
                49 => s.mc_all as i32,
                4 => {
                    // IP_OPTIONS: オプションはない (長さ 0)
                    return Ok(Vec::new());
                }
                _ => return Err(-ENOPROTOOPT),
            };
            // 長さが 1〜3 で 0〜255 なら 1 バイト (Linux の do_ip_getsockopt の copyval)
            if (1..4).contains(&len) && (0..=255).contains(&val) {
                return Ok(alloc::vec![val as u8]);
            }
            let mut b = int(val as i64);
            b.truncate((len as usize).min(4));
            return Ok(b);
        }
        _ => return Err(-EOPNOTSUPP),
    };
    v.truncate(len as usize);
    Ok(v)
}

pub fn setsockopt(u: &InetRef, level: i32, opt: i32, val: &[u8]) -> R {
    let tcp = u.borrow().tcp;
    match level {
        SOL_SOCKET => sol_socket_set(u, opt, val),
        IPPROTO_TCP if tcp => {
            if val.len() < 4 {
                return Err(crate::why::inval());
            }
            let v = i32::from_le_bytes(val[..4].try_into().unwrap());
            let mut s = u.borrow_mut();
            match opt {
                1 => {
                    s.nodelay = v != 0;
                    drop(s);
                    if v != 0 {
                        flush_cork(u);
                    }
                }
                2 => {
                    if v != 0 && !(88..=32767).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    s.user_mss = v;
                }
                3 => {
                    s.cork = v != 0;
                    drop(s);
                    if v == 0 {
                        flush_cork(u);
                    }
                }
                4 => {
                    if !(1..=32767).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    s.keepidle = v;
                }
                5 => {
                    if !(1..=32767).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    s.keepintvl = v;
                }
                6 => {
                    if !(1..=127).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    s.keepcnt = v;
                }
                7 => {
                    if !(1..=127).contains(&v) {
                        return Err(crate::why::inval());
                    }
                    s.syncnt = v;
                }
                8 => s.linger2 = if v < 0 { -1 } else { v.min(120) },
                9 => {}
                10 => {
                    if v == 0 {
                        if s.state != St::Close {
                            return Err(crate::why::inval());
                        }
                        s.window_clamp = 0;
                    } else {
                        s.window_clamp = v.max(MIN_RCVBUF as i32 / 2);
                    }
                }
                12 => {}
                18 => {
                    if v < 0 {
                        return Err(crate::why::inval());
                    }
                    s.user_timeout = v as u32;
                }
                25 => s.notsent_lowat = v as u32,
                _ => return Err(-ENOPROTOOPT),
            }
            Ok(0)
        }
        IPPROTO_IP => ip_set(u, opt, val),
        // Linux の ip_setsockopt: IP でない level は ENOPROTOOPT (getsockopt は EOPNOTSUPP)
        _ => Err(-ENOPROTOOPT),
    }
}

fn sol_socket_set(u: &InetRef, opt: i32, val: &[u8]) -> R {
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
            1 => {
                if b && !proc::current_cred_ref().capable(crate::cred::CAP_NET_ADMIN) {
                    return Err(-EACCES);
                }
            }
            2 => s.reuse = b,
            15 => s.reuseport = b,
            5 => s.dontroute = b,
            6 => s.broadcast = b,
            7 => {
                let n = (v as u32 as usize).min(BUF_MAX).min(i32::MAX as usize / 2);
                s.lock_sndbuf = true;
                s.sndbuf = (n * 2).max(MIN_SNDBUF);
            }
            8 => {
                let n = (v as u32 as usize).min(BUF_MAX).min(i32::MAX as usize / 2);
                s.lock_rcvbuf = true;
                s.rcvbuf = (n * 2).max(MIN_RCVBUF);
            }
            9 => s.keepalive = b,
            10 => s.oobinline = b,
            11 => {}
            12 => {
                if !(0..=6).contains(&v) && !crate::cred::net_prio_ok(proc::current_cred_ref()) {
                    return Err(-EPERM);
                }
                s.priority = v as u32;
            }
            13 => {
                if val.len() < 8 {
                    return Err(crate::why::inval());
                }
                s.linger = b;
                if b {
                    s.linger_secs = i32::from_le_bytes(val[4..8].try_into().unwrap()) as i64 as u64;
                }
            }
            14 => {}
            16 => return Err(-EOPNOTSUPP),
            18 => {
                let v = if v < 0 { i32::MAX } else { v };
                if s.tcp {
                    // Linux の tcp_set_rcvlowat: 上限は rcvbuf (決めたもの) か tcp_rmem[2] の半分。rcvbuf を広げる
                    let cap = if s.lock_rcvbuf { s.rcvbuf / 2 } else { TCP_RMEM_MAX / 2 } as i32;
                    let v = v.min(cap);
                    s.rcvlowat = if v == 0 { 1 } else { v };
                    if !s.lock_rcvbuf {
                        let space = v as usize * 2;
                        if space > s.rcvbuf {
                            s.rcvbuf = space;
                        }
                    }
                } else {
                    s.rcvlowat = if v == 0 { 1 } else { v };
                }
            }
            20 | 66 => s.rcvtimeo = to_ns(val)?,
            21 | 67 => s.sndtimeo = to_ns(val)?,
            42 => s.peek_off = v,
            _ => return Err(-ENOPROTOOPT),
        }
    }
    notify(u);
    Ok(0)
}

/// IPPROTO_IP の setsockopt (Linux の do_ip_setsockopt: 長さ 1〜3 なら 1 バイトの値)
fn ip_set(u: &InetRef, opt: i32, val: &[u8]) -> R {
    let v: i32 = if val.len() >= 4 {
        i32::from_le_bytes(val[..4].try_into().unwrap())
    } else if !val.is_empty() {
        val[0] as i32
    } else {
        0
    };
    let mut s = u.borrow_mut();
    match opt {
        8 => s.pktinfo = v != 0,
        12 => s.recvttl = v != 0,
        13 => s.recvtos = v != 0,
        6 | 7 => {}
        11 => {
            s.recverr = v != 0;
            if v == 0 {
                s.errq.clear();
            }
        }
        15 => {
            if val.is_empty() {
                return Err(crate::why::inval());
            }
            s.freebind = v != 0;
        }
        3 => return Err(-ENOPROTOOPT),
        34 => {
            if val.is_empty() {
                return Err(crate::why::inval());
            }
            s.mc_loop = v != 0;
        }
        49 => {
            if val.is_empty() || (v != 0 && v != 1) {
                return Err(crate::why::inval());
            }
            s.mc_all = v != 0;
        }
        24 => s.bind_no_port = v != 0,
        2 => {
            if val.is_empty() {
                return Err(crate::why::inval());
            }
            if v != -1 && !(1..=255).contains(&v) {
                return Err(crate::why::inval());
            }
            s.uc_ttl = v;
        }
        21 => {
            if val.is_empty() {
                return Err(crate::why::inval());
            }
            if !(0..=255).contains(&v) {
                return Err(crate::why::inval());
            }
            s.minttl = v as u8;
        }
        33 => {
            if s.tcp {
                return Err(crate::why::inval());
            }
            if val.is_empty() {
                return Err(crate::why::inval());
            }
            let v = if v == -1 { 1 } else { v };
            if !(0..=255).contains(&v) {
                return Err(crate::why::inval());
            }
            s.mc_ttl = v as u8;
        }
        10 => {
            if !(0..=5).contains(&v) {
                return Err(crate::why::inval());
            }
            s.pmtudisc = v as u8;
        }
        1 => {
            // TCP は ECN の 2 ビットを変えない
            let mut t = v as u8;
            if s.tcp {
                t = (t & !3) | (s.tos & 3);
            }
            s.tos = t;
        }
        _ => return Err(-ENOPROTOOPT),
    }
    Ok(0)
}

// ---- 閉じる ----

/// 最後の fd が閉じた (Linux の inet_release)
pub fn release(u: &InetRef) {
    {
        let mut s = u.borrow_mut();
        s.has_file = false;
        s.dead = true;
    }
    if u.borrow().tcp {
        tcp_close(u);
    } else {
        put_port(u);
        ext_close(u);
        let mut s = u.borrow_mut();
        s.dq.clear();
        s.errq.clear();
        s.pending = None;
        s.state = St::Close;
    }
}

/// Linux の __tcp_close: listen していれば子を切る。読んでいないものがあれば RST、SO_LINGER 0 なら切る、
/// ほかは FIN。FIN_WAIT2 で残る (60 秒か TCP_LINGER2。そのあいだに相手が送ったら RST、FIN なら TIME_WAIT)
fn tcp_close(u: &InetRef) {
    let _p = crate::prof::span("inet:close");
    let st = {
        let mut s = u.borrow_mut();
        s.shutdown = BOTH;
        s.state
    };
    if st == St::Listen {
        u.borrow_mut().state = St::Close;
        listen_stop(u);
        destroy(u);
        return;
    }
    let unread = {
        let s = u.borrow();
        s.rq.iter().any(|seg| {
            let end = if seg.fin { seg.end().wrapping_sub(1) } else { seg.end() };
            after(end, s.copied_seq)
        })
    };
    u.borrow_mut().rq.clear();
    if u.borrow().state == St::Close {
        destroy(u);
        return;
    }
    if u.borrow().ext.is_some() {
        destroy(u);
        return;
    }
    let (linger, secs, linger2) = {
        let s = u.borrow();
        (s.linger, s.linger_secs, s.linger2)
    };
    if unread {
        u.borrow_mut().state = St::Close;
        send_reset(u);
        destroy(u);
        return;
    }
    if linger && secs == 0 {
        let _ = tcp_disconnect(u);
        destroy(u);
        return;
    }
    flush_cork(u);
    let fin = close_state(&mut u.borrow_mut());
    if fin {
        send_fin(u);
    }
    let st = u.borrow().state;
    match st {
        St::FinWait2 => {}
        St::Close => {
            destroy(u);
            return;
        }
        St::TimeWait => return,
        _ => {
            keep_orphan(u);
            return;
        }
    }
    if linger2 < 0 {
        u.borrow_mut().state = St::Close;
        send_reset(u);
        destroy(u);
        return;
    }
    let t = if linger2 > 0 { linger2 as u64 } else { FIN_TIMEOUT };
    u.borrow_mut().expires = ticks() + t * HZ;
    keep_orphan(u);
    arm(u);
}

// ---- 時刻 (timer.rs から 5 tick ごと) ----

/// SYN のやりなおし (1、2、4 … 秒。TCP_SYNCNT 回やりなおしてだめなら ETIMEDOUT) と、FIN_WAIT2 / TIME_WAIT の終わり
pub fn tick() {
    let now = ticks();
    loop {
        let Some((&(at, k), _)) = timed().first_key_value() else { return };
        if at > now {
            return;
        }
        let w = timed().remove(&(at, k));
        let Some(u) = w.and_then(|w| w.upgrade()) else { continue };
        let (st, dead, next, exp, tries, syncnt) = {
            let mut s = u.borrow_mut();
            s.tkey = 0;
            (s.state, s.dead, s.syn_next, s.expires, s.syn_tries, if s.syncnt != 0 { s.syncnt } else { SYN_RETRIES })
        };
        match st {
            St::SynSent if next != 0 && now >= next => {
                if tries >= syncnt {
                    u.borrow_mut().err = -ETIMEDOUT;
                    tcp_done(&u);
                } else {
                    {
                        let mut s = u.borrow_mut();
                        s.syn_tries += 1;
                        s.syn_next = 0;
                    }
                    syn(&u);
                    notify(&u);
                }
            }
            St::TimeWait | St::FinWait2 if dead && exp != 0 && now >= exp => kill(&u),
            _ => {}
        }
        // まだ先の時刻があれば入れなおす (時刻が動いたもの、やりなおしの SYN がまた落とされたもの)
        arm(&u);
    }
}

// ---- fstat など (Linux の sockfs の inode) ----

pub fn stat(u: &InetRef) -> Stat {
    let s = u.borrow();
    Stat { ino: s.ino, uid: s.uid, gid: s.gid, mtime: s.mtime, ctime: s.ctime, ..Stat::dev(0o140000 | s.mode, 0) }
}

pub fn touch(u: &InetRef, mtime: Option<u64>) {
    let mut s = u.borrow_mut();
    if let Some(t) = mtime {
        s.mtime = t;
    }
    s.ctime = crate::timer::stamp_ns();
}

pub fn owner(u: &InetRef) -> u32 {
    u.borrow().uid
}

pub fn chmod(u: &InetRef, mode: u32) -> R {
    let c = proc::current_cred_ref();
    let mut s = u.borrow_mut();
    if c.euid != s.uid && !c.capable(crate::cred::CAP_FOWNER) {
        return Err(-EPERM);
    }
    s.mode = mode & 0o7777;
    Ok(0)
}

pub fn owner_mode(u: &InetRef) -> (u32, u32, u32) {
    let s = u.borrow();
    (s.uid, s.gid, s.mode)
}

pub fn set_owner_mode(u: &InetRef, uid: Option<u32>, gid: Option<u32>, mode: u32) {
    let mut s = u.borrow_mut();
    if let Some(n) = uid {
        s.uid = n;
    }
    if let Some(g) = gid {
        s.gid = g;
    }
    s.mode = mode & 0o7777;
}

pub fn describe(u: &InetRef) -> String {
    alloc::format!("socket:[{}]", u.borrow().ino)
}

fn dotted(a: u32) -> String {
    alloc::format!("{}.{}.{}.{}", a >> 24, (a >> 16) & 255, (a >> 8) & 255, a & 255)
}

pub fn debug(u: &InetRef) -> String {
    let s = u.borrow();
    alloc::format!(
        "{} {:?} {}:{} -> {}:{} rq {} dq {} acceptq {} shutdown {} err {}{}",
        if s.tcp { "tcp" } else { "udp" },
        s.state,
        dotted(s.rcv_saddr),
        s.sport,
        dotted(s.daddr),
        s.dport,
        s.rcv_nxt.wrapping_sub(s.copied_seq),
        s.dq.len(),
        s.acceptq.len(),
        s.shutdown,
        s.err,
        if s.ext.is_some() { " ext" } else { "" }
    )
}

/// /proc/net/tcp と /proc/net/udp (Linux と同じ形。ss と netstat が読む)。inode は fstat のもの (fd のないもの、
/// TIME_WAIT は 0)
pub fn proc_net(want_tcp: bool) -> String {
    let hex = |a: u32| alloc::format!("{:08X}", a.swap_bytes());
    let mut out = String::from(if want_tcp {
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
    } else {
        "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"
    });
    let netns = crate::ns::net().unwrap_or(0);
    let mut i = 0;
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let s = u.borrow();
        if s.tcp != want_tcp || s.net != netns || (!s.hashed && s.num == 0) {
            continue;
        }
        let local = if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr };
        let inode = if s.has_file { s.ino } else { 0 };
        // Linux の tw (TIME_WAIT と fd のない FIN_WAIT2) は持ち主を覚えていない
        let uid = if is_tw(&s) { 0 } else { s.uid };
        let line = if want_tcp {
            let rx = if s.state == St::Listen { s.acceptq.len() as u32 } else { s.rcv_nxt.wrapping_sub(s.copied_seq) };
            alloc::format!(
                "{:4}: {}:{:04X} {}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5}        0 {} 1 0000000000000000 100 0 0 10 0\n",
                i,
                hex(local),
                s.sport,
                hex(s.daddr),
                s.dport,
                s.state as u8,
                0,
                rx,
                uid,
                inode
            )
        } else {
            alloc::format!(
                "{:5}: {}:{:04X} {}:{:04X} {:02X} 00000000:{:08X} 00:00000000 00000000 {:5}        0 {} 2 0000000000000000 0\n",
                i,
                hex(local),
                s.sport,
                hex(s.daddr),
                s.dport,
                s.state as u8,
                s.rmem,
                s.uid,
                inode
            )
        };
        out.push_str(&line);
        i += 1;
    }
    out
}

/// /proc/net/sockstat と snmp の数 (Linux の数えかた)
#[derive(Default)]
pub struct Counts {
    /// TCP: 表にいるもの (listen、つないでいる、つながった。bind しただけと TIME_WAIT はのぞく: sock_prot_inuse)
    pub inuse: usize,
    /// fd がなくなって、まだ終わっていないもの (FIN_WAIT1、CLOSING、LAST_ACK: tcp_orphan_count)
    pub orphan: usize,
    /// TIME_WAIT と、fd のない FIN_WAIT2 (Linux はどちらも小さな tw になる: tcp_time_wait)
    pub tw: usize,
    /// tw でない TCP のソケットすべて (tcp_sockets_allocated)
    pub alloc: usize,
    pub estab: usize,
    /// UDP: bind したもの
    pub udp: usize,
    /// UDP のソケットすべて (sockets: used に)
    pub udp_all: usize,
}

/// Linux なら小さな tw になっているもの: TIME_WAIT と、fd のない FIN_WAIT2 (aios ではどちらもふつうのソケット。
/// FIN_WAIT2 ののこる時間が TIME_WAIT より長いものは、Linux でもふつうのソケットのまま: tcp_close)
fn is_tw(s: &Inet) -> bool {
    s.state == St::TimeWait || (s.state == St::FinWait2 && s.dead && s.linger2 <= FIN_TIMEOUT as i32)
}

/// 数える
pub fn counts() -> Counts {
    let mut c = Counts::default();
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let s = u.borrow();
        if !s.tcp {
            c.udp_all += 1;
            if s.hashed {
                c.udp += 1;
            }
            continue;
        }
        if is_tw(&s) {
            c.tw += 1;
            continue;
        }
        c.alloc += 1;
        if s.state != St::Close {
            c.inuse += 1;
            if s.dead {
                c.orphan += 1;
            }
        }
        if matches!(s.state, St::Established | St::CloseWait) {
            c.estab += 1;
        }
    }
    c
}

// ---- 外 (smoltcp) ----

const EXT_RX: usize = 64 * 1024;
const EXT_TX: usize = 32 * 1024;

fn tcp_sock(h: SocketHandle) -> &'static mut tcp::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<tcp::Socket>(h)
}

fn udp_sock(h: SocketHandle) -> &'static mut udp::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<udp::Socket>(h)
}

/// 外から届きうるか (分けていない NET で、0.0.0.0 か eth0 のアドレスに bind したもの)
fn reachable_from_outside(s: &Inet) -> bool {
    s.net == 0 && net::get().is_some() && (s.rcv_saddr == 0 || (s.rcv_saddr == eth() && eth() != 0))
}

fn ext_connect(u: &InetRef) -> Result<(), i64> {
    let n = net::get().ok_or(-ENETUNREACH)?;
    let sock = tcp::Socket::new(tcp::SocketBuffer::new(alloc::vec![0; EXT_RX]), tcp::SocketBuffer::new(alloc::vec![0; EXT_TX]));
    let h = n.sockets.add(sock);
    let (daddr, dport, sport) = {
        let s = u.borrow();
        (s.daddr, s.dport, s.num)
    };
    let local = IpListenEndpoint { addr: None, port: sport };
    let ep = IpEndpoint::new(IpAddress::Ipv4(ipv4(daddr)), dport);
    if n.sockets.get_mut::<tcp::Socket>(h).connect(n.iface.context(), ep, local).is_err() {
        n.sockets.remove(h);
        return Err(crate::why::inval());
    }
    {
        let mut s = u.borrow_mut();
        s.ext = Some(h);
        if s.saddr == 0 || s.saddr >> 24 == 127 {
            s.saddr = eth();
            s.rcv_saddr = eth();
        }
    }
    net::poll();
    Ok(())
}

/// smoltcp の TCP の状態をこちらに映す
fn ext_sync(u: &InetRef) {
    let (h, st) = {
        let s = u.borrow();
        if !s.tcp {
            return;
        }
        let Some(h) = s.ext else { return };
        (h, s.state)
    };
    net::poll();
    let t = tcp_sock(h);
    let ts = t.state();
    let changed = {
        let mut s = u.borrow_mut();
        match (st, ts) {
            (St::SynSent, tcp::State::SynSent | tcp::State::SynReceived) => false,
            (St::SynSent, tcp::State::Closed) => {
                s.err = -ECONNREFUSED;
                s.state = St::Close;
                s.shutdown = BOTH;
                true
            }
            (St::SynSent, _) => {
                s.state = St::Established;
                s.mss_cache = 1460 - 12;
                if !s.lock_sndbuf {
                    s.sndbuf = s.sndbuf.max(87040);
                }
                if let Some(ep) = t.local_endpoint() {
                    let IpAddress::Ipv4(a) = ep.addr;
                    s.saddr = u32::from_be_bytes(a.octets());
                    s.rcv_saddr = s.saddr;
                }
                true
            }
            (St::Established | St::FinWait1 | St::FinWait2 | St::CloseWait, _) if !t.may_recv() && s.shutdown & RCV == 0 => {
                s.shutdown |= RCV;
                s.done = true;
                if s.state == St::Established {
                    s.state = St::CloseWait;
                }
                true
            }
            (St::Established | St::CloseWait, tcp::State::Closed) => {
                s.state = St::Close;
                s.shutdown = BOTH;
                true
            }
            _ => false,
        }
    };
    if changed {
        notify(u);
    }
}

fn ext_recv(u: &InetRef, len: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    let nb = nonblock || flags & MSG_DONTWAIT != 0;
    let peek = flags & MSG_PEEK != 0;
    loop {
        ext_sync(u);
        let Some(h) = u.borrow().ext else { return Err(-ENOTCONN) };
        let t = tcp_sock(h);
        if t.can_recv() {
            let mut buf = alloc::vec![0u8; len.min(EXT_RX)];
            let k = if peek { t.peek_slice(&mut buf) } else { t.recv_slice(&mut buf) }.map_err(|_| -ECONNRESET)?;
            buf.truncate(k);
            if !peek {
                net::poll();
            }
            let mut r = Recv::new();
            r.ret = if flags & MSG_TRUNC != 0 { k } else { buf.len() };
            r.data = buf;
            return Ok(r);
        }
        if !t.may_recv() {
            return Ok(Recv::new());
        }
        let e = take_err(u);
        if e != 0 {
            return Err(e);
        }
        if nb {
            return Err(-EAGAIN);
        }
        let timeo = u.borrow().rcvtimeo;
        wait(net::chan(), timeo, 0)?;
    }
}

fn ext_send(u: &InetRef, src: &Src, len: usize, flags: u64, nb: bool) -> R {
    let mut done = 0;
    while done < len {
        let Some(h) = u.borrow().ext else { return Err(-ENOTCONN) };
        let t = tcp_sock(h);
        if !t.may_send() {
            return if done > 0 { Ok(done as i64) } else { Err(stream_error(u, flags, -EPIPE)) };
        }
        if t.can_send() {
            let n = (len - done).min(EXT_TX);
            let data = take(src, done, n)?;
            let k = t.send_slice(&data).map_err(|_| -EPIPE)?;
            done += k;
            net::poll();
            continue;
        }
        if nb {
            return if done > 0 { Ok(done as i64) } else { Err(-EAGAIN) };
        }
        let timeo = u.borrow().sndtimeo;
        if let Err(e) = wait(net::chan(), timeo, 0) {
            return if done > 0 { Ok(done as i64) } else { Err(e) };
        }
    }
    Ok(done as i64)
}

/// smoltcp のものを手放す (TCP は FIN を送り終わるまで net.rs が持つ)
fn ext_close(u: &InetRef) {
    let (h, l, tcp) = {
        let mut s = u.borrow_mut();
        (s.ext.take(), s.ext_listen.take(), s.tcp)
    };
    let Some(n) = net::get() else { return };
    if let Some(h) = h {
        if tcp {
            net::orphan(h);
        } else {
            n.sockets.remove(h);
        }
    }
    if let Some(h) = l {
        n.sockets.remove(h);
    }
}

/// UDP の外との口: bind したポートで smoltcp の UDP のソケットを
fn ext_udp_bind(u: &InetRef) {
    let (ok, port, ttl) = {
        let s = u.borrow();
        (!s.tcp && s.ext.is_none() && s.num != 0 && reachable_from_outside(&s), s.num, s.uc_ttl)
    };
    if !ok {
        return;
    }
    let Some(n) = net::get() else { return };
    let meta = || alloc::vec![udp::PacketMetadata::EMPTY; 16];
    let sock = udp::Socket::new(udp::PacketBuffer::new(meta(), alloc::vec![0; 16 * 1024]), udp::PacketBuffer::new(meta(), alloc::vec![0; 16 * 1024]));
    let h = n.sockets.add(sock);
    if udp_sock(h).bind(IpListenEndpoint { addr: None, port }).is_err() {
        n.sockets.remove(h);
        return;
    }
    udp_sock(h).set_hop_limit(if ttl > 0 { Some(ttl as u8) } else { None });
    u.borrow_mut().ext = Some(h);
}

/// 外から届いた UDP をこちらの行列に
fn ext_udp_pull(u: &InetRef) {
    let h = {
        let s = u.borrow();
        if s.tcp {
            return;
        }
        let Some(h) = s.ext else { return };
        h
    };
    net::poll();
    let mut got = false;
    loop {
        let sock = udp_sock(h);
        if !sock.can_recv() {
            break;
        }
        let mut buf = alloc::vec![0u8; 65536];
        let Ok((k, meta)) = sock.recv_slice(&mut buf) else { break };
        buf.truncate(k);
        let IpAddress::Ipv4(a) = meta.endpoint.addr;
        let from = (u32::from_be_bytes(a.octets()), meta.endpoint.port);
        let ts = udp_truesize(k);
        let mut s = u.borrow_mut();
        if s.rmem + ts > s.rcvbuf && s.rmem != 0 {
            continue;
        }
        s.rmem += ts;
        s.dq.push_back(Dgram { data: buf, from, to: eth(), tos: 0, ttl: 64, truesize: ts });
        got = true;
    }
    if got {
        notify(u);
    }
}

fn ext_udp_send(u: &InetRef, data: &[u8], daddr: u32, dport: u16, ttl: Option<u8>) -> Result<(), i64> {
    if u.borrow().ext.is_none() {
        ext_udp_bind(u);
    }
    let Some(h) = u.borrow().ext else { return Ok(()) };
    let ep = IpEndpoint::new(IpAddress::Ipv4(ipv4(daddr)), dport);
    if let Some(t) = ttl {
        udp_sock(h).set_hop_limit(Some(t));
    }
    udp_sock(h).send_slice(data, ep).map_err(|_| -EAGAIN)?;
    net::poll();
    Ok(())
}

/// listen したものが外からも受けられるように、smoltcp の待つ口を
fn ext_listen(u: &InetRef) {
    let (ok, port) = {
        let s = u.borrow();
        (s.ext_listen.is_none() && reachable_from_outside(&s), s.num)
    };
    if !ok {
        return;
    }
    let Some(n) = net::get() else { return };
    let sock = tcp::Socket::new(tcp::SocketBuffer::new(alloc::vec![0; EXT_RX]), tcp::SocketBuffer::new(alloc::vec![0; EXT_TX]));
    let h = n.sockets.add(sock);
    if n.sockets.get_mut::<tcp::Socket>(h).listen(IpListenEndpoint { addr: None, port }).is_err() {
        n.sockets.remove(h);
        return;
    }
    u.borrow_mut().ext_listen = Some(h);
}

/// 外からつながったものを accept の行列に (smoltcp の待つ口は 1 つずつなので、つながったら新しく作る)
fn ext_accept(u: &InetRef) {
    let h = {
        let s = u.borrow();
        if s.state != St::Listen {
            return;
        }
        let Some(h) = s.ext_listen else { return };
        h
    };
    net::poll();
    let t = tcp_sock(h);
    if !t.is_active() || t.state() == tcp::State::Listen {
        return;
    }
    let (local, remote) = (t.local_endpoint(), t.remote_endpoint());
    let c = {
        let l = u.borrow();
        let c = new_sock_as(true, l.protocol, l.uid, l.gid, l.net);
        {
            let mut x = c.borrow_mut();
            x.has_file = false;
            x.ext = Some(h);
            x.ss = Ss::Connected;
            x.state = St::Established;
            x.mss_cache = 1460 - 12;
            x.sndbuf = 87040;
            if let Some(ep) = local {
                let IpAddress::Ipv4(a) = ep.addr;
                x.saddr = u32::from_be_bytes(a.octets());
                x.rcv_saddr = x.saddr;
            }
            if let Some(ep) = remote {
                let IpAddress::Ipv4(a) = ep.addr;
                x.daddr = u32::from_be_bytes(a.octets());
                x.dport = ep.port;
            }
            x.nodelay = l.nodelay;
            x.keepalive = l.keepalive;
        }
        c
    };
    let port = u.borrow().num;
    hash_port(&c, port);
    u.borrow_mut().ext_listen = None;
    ext_listen(u);
    u.borrow_mut().acceptq.push_back(c);
    notify(u);
}
