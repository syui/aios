// AF_INET の TCP と UDP。答え (返す数、エラー、poll のビット、名前、sockopt) は Linux 7.3 の net/ipv4 (af_inet.c、
// tcp.c、tcp_input.c、tcp_ipv4.c、tcp_minisocks.c、udp.c、datagram.c、inet_connection_sock.c、ip_sockglue.c) と同じ。
//
// AF_INET6 (段階 1: Linux 7.4 の net/ipv6 の af_inet6.c、tcp_ipv6.c、udp.c、datagram.c、ipv6_sockglue.c): ::1 と ::、
// v4 の番地を写したもの (::ffff:a.b.c.d)、IPV6_V6ONLY。Linux と同じく 1 つのソケットに v4 の欄 (rcv_saddr ...) と
// v6 の欄 (rcv6 ...) があり、IPv4 とのやりとり (写した番地、:: で待つものへの IPv4) は v4 の欄でいつもの道を通る。
// IPv6 だけのもの (v6) は v4 の欄が LOOPBACK4_IPV6 (127.0.0.6) で、v6only なので IPv4 からは見えない。ポートの表は
// v4 と v6 で 1 つ (Linux の bhash)。IPv6 の番地は ::1 と eth0 のもの (addrconf.rs: link-local と SLAAC)。自分の番地へは
// IPv4 と同じくパケットを作らずに相手の行列へ、ほかは smoltcp (道は addrconf の表、送り元は ipv6_dev_get_saddr のえらびかた)。
// link-local の名前の scope id は eth0 (aios の口は 1 つ)。
// ファイアウォール (nftables) は ::1 のものも IPv4 と同じく lo の 4 つのフックに通す (IPv6 の頭で)。conntrack はまだ
// IPv6 を見ない
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
//   IP_RECVERR と IPV6_RECVERR のエラーの行列 (ICMP と ICMPv6)
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
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};

const EPERM: i64 = 1;
const EAGAIN: i64 = 11;
const EACCES: i64 = 13;
const ENODEV: i64 = 19;
const EPROTO: i64 = 71;
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
pub const AF_INET6: u16 = 10;
const LOOPBACK: u32 = 0x7f00_0001;
/// ::1
const LOOPBACK6: u128 = 1;
/// IPv6 だけのソケットの v4 の欄 (Linux の LOOPBACK4_IPV6: 127.0.0.6)
const LOOPBACK4_IPV6: u32 = 0x7f00_0006;

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
/// IPv6 のループバック: MSS は 65536 から IPv6 の頭 40 と TCP の 20、connect した側の窓の半分は 65464 の半分
const ADV_MSS6: i32 = 65476;
const HALF_WND_CLIENT6: i32 = 32732;
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
/// delayed ACK の短いほう (TCP_DELACK_MIN: 40 ms。lo の srtt はこれより短いので、pingpong の ato はこれ)
const DELACK_MIN: u64 = HZ / 25;

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
    /// IPv6 のもの: (送り元, あて先)
    v6: Option<(u128, u128)>,
    tos: u8,
    ttl: u8,
    truesize: usize,
    /// 受けた口 (IPV6_PKTINFO の ipi6_ifindex: 自分あては lo、外からのものは eth0)
    ifindex: u32,
}

/// IP_RECVERR と IPV6_RECVERR のエラーの行列のもの (Linux の sock_exterr_skb): 送ったものの中身、errno、ICMP (v6 なら
/// ICMPv6) の種類、あて先、知らせてきた番地、その ICMP を受けた口と番地 (IPv4 の番地は下の 32 ビットに)
struct ErrMsg {
    data: Vec<u8>,
    errno: u32,
    v6: bool,
    typ: u8,
    code: u8,
    /// ee_info (packet too big の MTU)
    info: u32,
    to: (u128, u16),
    offender: u128,
    ifindex: u32,
    local: u128,
    /// ee_origin: SO_EE_ORIGIN_LOCAL (1: 送るときに分かった EMSGSIZE。知らせてきた番地も付帯データの口もない)、
    /// SO_EE_ORIGIN_ICMP (2)、SO_EE_ORIGIN_ICMP6 (3)
    origin: u8,
}

const EE_ORIGIN_LOCAL: u8 = 1;

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
    /// AF_INET か AF_INET6
    pub family: u16,
    /// AF_INET6 の IPv6 の欄 (Linux の sk_v6_rcv_saddr、np->saddr、sk_v6_daddr): bind と connect で決まる (IPv4 の
    /// ものは v4 の番地を写したもの。:: は 0)。v6only は IPV6_V6ONLY (ipv6_only_sock: IPv6 だけの番地に bind しても立つ)。
    /// v6 はいまの bind かつながりが IPv6 だけのもの (v4 の欄は LOOPBACK4_IPV6)
    rcv6: u128,
    saddr6: u128,
    daddr6: u128,
    v6only: bool,
    v6: bool,
    // IPPROTO_IPV6 (IPV6_UNICAST_HOPS は -1 なら 64、IPV6_MULTICAST_HOPS、IPV6_MULTICAST_LOOP、IPV6_TCLASS、
    // IPV6_RECVPKTINFO / RECVTCLASS / RECVHOPLIMIT、IPV6_MTU_DISCOVER、IPV6_RECVERR)
    hops6: i32,
    mc_hops6: i32,
    mc_loop6: bool,
    /// 入っている IPv6 のマルチキャストの組 (np->ipv6_mc_list: 新しいものが先): (組、口)。mc_all6 は IPV6_MULTICAST_ALL
    /// (入っていない組あてのものも受ける)
    mc6: Vec<(u128, i32)>,
    mc_all6: bool,
    /// IPV6_ADDR_PREFERENCES (np->srcprefs: IPV6_PREFER_SRC_TMP 1、PUBLIC 2、COA 4)
    srcprefs: u32,
    tclass: i32,
    rx_pktinfo6: bool,
    rx_tclass: bool,
    rx_hoplimit: bool,
    pmtudisc6: i32,
    recverr6: bool,
    /// IPV6_DONTFRAG、IPV6_RECVPATHMTU と、まだ読んでいない IPV6_PATHMTU (ipv6_local_rxpmtu: (あて先、口、MTU))、
    /// IPV6_MTU の frag_size (0 はなし)
    dontfrag6: bool,
    rxpmtu_on: bool,
    rxpmtu: Option<(u128, u32, u32)>,
    frag_size: u32,
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
    /// 中どうしの TCP で、決まりと conntrack に見せる次の seq (つないだときの ISN + 1 から、データの長さだけ進む。0 は
    /// まだ見せていない)
    ct_seq: u32,
    /// おくらせている ACK (tick。0 はなし): ESTABLISHED で FIN を受けた側は Linux の tcp_fin が pingpong にして、ACK を
    /// delayed ACK (lo では TCP_DELACK_MIN の 40 ms) で出す。そのあいだに自分も閉じれば、FIN が ACK も運ぶ
    ack_due: u64,
    /// ファイアウォールが落としたデータの segment (seq、中身、urgent)。送りなおすまで相手に届かない (Linux の再送)
    fw_hold: VecDeque<(u32, Vec<u8>, bool)>,
    /// 送りなおしの時刻 (tick)、回数、FIN も待っているか
    fw_next: u64,
    fw_tries: u32,
    fw_fin: bool,
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

/// 眠る (SO_RCVTIMEO / SO_SNDTIMEO か、recvmmsg の締め切りがあればそこまで: 時間切れは EAGAIN)。until は時刻で
/// やることの時刻 (0 は無し)
fn wait(chan: usize, timeo: u64, until: u64) -> Result<(), i64> {
    let deadline = proc::recv_deadline(if timeo == 0 { 0 } else { crate::timer::ticks_after(timeo) });
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
        family: AF_INET,
        rcv6: 0,
        saddr6: 0,
        daddr6: 0,
        v6only: false,
        v6: false,
        hops6: -1,
        mc_hops6: 1,
        mc_loop6: true,
        mc6: Vec::new(),
        mc_all6: true,
        srcprefs: 0,
        tclass: 0,
        rx_pktinfo6: false,
        rx_tclass: false,
        rx_hoplimit: false,
        pmtudisc6: 1,
        recverr6: false,
        dontfrag6: false,
        rxpmtu_on: false,
        rxpmtu: None,
        frag_size: 0,
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
        ct_seq: 0,
        ack_due: 0,
        fw_hold: VecDeque::new(),
        fw_next: 0,
        fw_tries: 0,
        fw_fin: false,
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
/// landlock が見るソケットか (Linux の sk_is_tcp / sk_is_udp: SOCK_STREAM の IPPROTO_TCP、SOCK_DGRAM の IPPROTO_UDP) と、
/// もう口が決まっているか (inet_num。UDP は決まっていなければ送る・つなぐときに autobind する)
pub fn ll_kind(u: &InetRef) -> (Option<bool>, bool) {
    let s = u.borrow();
    let kind = match (s.tcp, s.protocol) {
        (true, 6) => Some(true),
        (false, 17) => Some(false),
        _ => None,
    };
    (kind, s.num != 0)
}

pub fn create(typ: u64, protocol: u64) -> InetRef {
    let tcp = typ == 1;
    let c = proc::current_cred_ref();
    new_sock_as(tcp, if protocol != 0 { protocol as u8 } else if tcp { 6 } else { 17 }, c.fsuid, c.fsgid, crate::ns::net().unwrap_or(0))
}

/// socket(AF_INET6, ...): IPV6_V6ONLY ははじめ 0 (net.ipv6.bindv6only)
pub fn create6(typ: u64, protocol: u64) -> InetRef {
    let u = create(typ, protocol);
    u.borrow_mut().family = AF_INET6;
    u
}

pub fn family(u: &InetRef) -> u16 {
    u.borrow().family
}

pub fn file_of(u: InetRef, flags: u64) -> FileRef {
    file::new(Kind::Ops(Rc::new(InetFile(u))), file::O_RDWR | if flags & 0o4000 != 0 { file::O_NONBLOCK } else { 0 })
}

// ---- AppArmor (af_inet.c。v9 の決まりなら番地まで見る (apparmor.rs の inet_perm)。番地は記録にも) ----

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

/// いまのプロセスの決まりで、このソケットへの op。自分の番地は今の名前 (getsockname と同じ)、bind は bind する名前
/// (saddr= と src=)、相手のあることは相手の名前 (daddr= と dest=)
pub fn aa_perm(u: &InetRef, op: &str, request: u32, what: crate::apparmor::InetOp) -> Result<(), i64> {
    use crate::apparmor::{InetOp, inet_addr_audit};
    let (typ, proto, sk, local) = {
        let s = u.borrow();
        let sk = inet_addr_audit(Some(&name_of(&s, s.rcv_saddr, if s.v6 { s.rcv6 } else { mapped(s.rcv_saddr) }, s.sport)), "laddr", "lport")
            + &inet_addr_audit(Some(&name_of(&s, s.daddr, peer6(&s), s.dport)), "faddr", "fport");
        let local = name_of(&s, if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr }, local6(&s), s.sport);
        (if s.tcp { 1 } else { 2 }, s.protocol as u32, sk, local)
    };
    let (bind, remote) = match &what {
        InetOp::Bind(a) => (Some(*a), None),
        InetOp::Remote(r) => (None, *r),
        _ => (None, None),
    };
    let pre = || {
        let mut s = sk.clone();
        s.push_str(&inet_addr_audit(Some(bind.unwrap_or(&local)), "saddr", "src"));
        s.push_str(&inet_addr_audit(remote, "daddr", "dest"));
        crate::apparmor::NetAudit { pre: s, post: String::new() }
    };
    crate::apparmor::inet_perm(op, request, typ, proto, &local, what, &pre)
}

// ---- アドレス ----

/// eth0 のアドレス (なければ 0)
fn eth() -> u32 {
    u32::from_be_bytes(net::addr().octets())
}

/// aios の中のアドレスか (Linux の RTN_LOCAL: 127.0.0.0/8 と、分けていない NET の eth0 のアドレス)
fn is_local(a: u32, netns: u64) -> bool {
    a >> 24 == 127 || (netns == 0 && net::is_eth_addr(a))
}

fn is_multicast(a: u32) -> bool {
    a >> 28 == 0xe
}

fn is_broadcast(a: u32) -> bool {
    if a == u32::MAX {
        return true;
    }
    let bcast = |base: u32, p: u32| p < 31 && a == base | (u32::MAX >> p);
    net::cidr().is_some_and(|c| bcast(u32::from_be_bytes(c.address().octets()), c.prefix_len() as u32))
        || net::extra4().iter().any(|x| bcast(x.addr, x.prefix as u32) || (x.brd != 0 && a == x.brd))
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
    if netns == 0 && net::is_eth_addr(daddr) {
        // local の道の prefsrc (secondary の番地へは primary から)
        return Ok((daddr, if saddr != 0 { saddr } else { net::src4(daddr) }, true));
    }
    if netns != 0 || net::get().is_none() || eth() == 0 {
        return Err(-ENETUNREACH);
    }
    // 送り元は道の prefsrc (netlink で足した番地があれば、あて先の網のもの)
    Ok((daddr, if saddr != 0 { saddr } else { net::src4(daddr) }, false))
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

// ---- IPv6 のアドレス (128 ビットの数。:: は 0、::1 は 1) ----

/// v4 の番地を写したもの (::ffff:a.b.c.d)
fn mapped(a: u32) -> u128 {
    0xffff_0000_0000u128 | a as u128
}

fn is_mapped(a: u128) -> bool {
    a >> 32 == 0xffff
}

fn v6_multicast(a: u128) -> bool {
    a >> 120 == 0xff
}

/// scope id がいるもの (__ipv6_addr_needs_scope_id: fe80::/10 と、multicast の link-local・interface-local)
fn v6_needs_scope(a: u128) -> bool {
    a >> 118 == 0x3fa || (v6_multicast(a) && matches!((a >> 112) & 0xf, 1 | 2))
}

/// aios の中の IPv6 のアドレスか (::1 と eth0 のもの)
fn is_local6(a: u128) -> bool {
    a == LOOPBACK6 || crate::addrconf::is_mine(a)
}

/// IPv6 のあて先へのみち (ip6_route_output と ip6_dst_lookup_tail): (あて先, 送り元, 自分あてか)。自分の番地へは
/// lo (送り元はあて先そのもの)、ほかは addrconf の道の表 (なければ ENETUNREACH) で、送り元は決めていなければ
/// ipv6_dev_get_saddr。scope のいる番地の口が eth0 でなければ ENETUNREACH。分けた NET の中は ::1 だけ
fn route6(netns: u64, daddr: u128, saddr: u128, scope: u32, prefs: u32) -> Result<(u128, u128, bool), i64> {
    if v6_needs_scope(daddr) && scope != 0 && scope != crate::netlink::ETH_INDEX as u32 {
        return Err(-ENETUNREACH);
    }
    if daddr == LOOPBACK6 || (netns == 0 && is_local6(daddr)) {
        return Ok((daddr, if saddr != 0 { saddr } else { daddr }, true));
    }
    if netns != 0 || crate::addrconf::lookup(daddr).is_none() {
        return Err(-ENETUNREACH);
    }
    let src = if saddr != 0 { saddr } else { crate::addrconf::source_for_prefs(daddr, prefs).ok_or(-ENETUNREACH)? };
    Ok((daddr, src, false))
}

/// sockaddr_in6 (28 バイト: family、port、flowinfo、addr、scope_id)。scope のいる番地 (link-local) の scope id は eth0
/// (ipv6_iface_scope_id: Linux はソケットの口か、受けた口。aios の口は 1 つ)
fn sockaddr6(a: u128, port: u16) -> [u8; 28] {
    let mut b = [0u8; 28];
    b[..2].copy_from_slice(&AF_INET6.to_le_bytes());
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[8..24].copy_from_slice(&a.to_be_bytes());
    if v6_needs_scope(a) {
        b[24..28].copy_from_slice(&(crate::netlink::ETH_INDEX as u32).to_le_bytes());
    }
    b
}

/// sockaddr_in6 を読む: (family, port, addr, scope_id)。短いところは 0
fn parse6(a: &[u8]) -> (u16, u16, u128, u32) {
    let fam = if a.len() >= 2 { u16::from_le_bytes([a[0], a[1]]) } else { 0 };
    let port = if a.len() >= 4 { u16::from_be_bytes([a[2], a[3]]) } else { 0 };
    let addr = if a.len() >= 24 { u128::from_be_bytes(a[8..24].try_into().unwrap()) } else { 0 };
    let scope = if a.len() >= 28 { u32::from_le_bytes(a[24..28].try_into().unwrap()) } else { 0 };
    (fam, port, addr, scope)
}

/// 自分の名前の IPv6 の番地 (inet6_getname: sk_v6_rcv_saddr、:: なら np->saddr。IPv4 のものは写した番地)
fn local6(s: &Inet) -> u128 {
    if s.v6 {
        return if s.rcv6 != 0 { s.rcv6 } else { s.saddr6 };
    }
    let a = if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr };
    if a == 0 && !is_mapped(s.rcv6) { 0 } else { mapped(a) }
}

/// 相手の IPv6 の番地
fn peer6(s: &Inet) -> u128 {
    if s.v6 {
        s.daddr6
    } else if s.daddr == 0 {
        0
    } else {
        mapped(s.daddr)
    }
}

/// ソケットの名前 (family のもの)
fn name_of(s: &Inet, a4: u32, a6: u128, port: u16) -> Vec<u8> {
    if s.family == AF_INET6 { sockaddr6(a6, port).to_vec() } else { sockaddr(a4, port).to_vec() }
}

/// lo の決まりに通すパケットの送り元とあて先 (IPv6 のつながりなら v6 の欄)。rev は相手からのもの
fn ends(s: &Inet, rev: bool) -> crate::nfhook::Ends {
    use crate::nfhook::Ends;
    match (s.v6, rev) {
        (true, false) => Ends::V6(s.saddr6, s.daddr6),
        (true, true) => Ends::V6(s.daddr6, s.saddr6),
        (false, false) => Ends::V4(s.saddr, s.daddr),
        (false, true) => Ends::V4(s.daddr, s.saddr),
    }
}

/// bind で決めていないアドレスを手放す (Linux の inet_reset_saddr: v6 の欄も)
fn reset_saddr(s: &mut Inet) {
    s.saddr = 0;
    s.rcv_saddr = 0;
    s.rcv6 = 0;
    s.saddr6 = 0;
    s.v6 = false;
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
    family: u16,
    rcv6: u128,
    v6only: bool,
    reuse: bool,
    reuseport: bool,
    uid: u32,
}

fn me_of(s: &Inet) -> Me {
    Me { tcp: s.tcp, net: s.net, rcv_saddr: s.rcv_saddr, family: s.family, rcv6: s.rcv6, v6only: s.v6only, reuse: s.reuse, reuseport: s.reuseport, uid: s.uid }
}

/// IPv6 の番地の種類 (ipv6_addr_type のうち bind に要るもの)
#[derive(PartialEq, Clone, Copy)]
enum T6 {
    Any,
    Mapped,
    Native,
}

fn t6(a: u128) -> T6 {
    if a == 0 {
        T6::Any
    } else if is_mapped(a) {
        T6::Mapped
    } else {
        T6::Native
    }
}

/// アドレスが重なるか (Linux の inet_rcv_saddr_equal で match_wildcard: v4 どうしは 0.0.0.0 か同じ。AF_INET6 は
/// ipv6_rcv_saddr_equal: どちらも写した番地なら v4 として、:: は相手が v6only の写した番地でなければ重なる、ほかは同じ番地)
fn addr_overlap(me: &Me, o: &Inet) -> bool {
    let v4 = |a: u32, b: u32| a == b || a == 0 || b == 0;
    if me.family != AF_INET6 {
        return !o.v6only && v4(me.rcv_saddr, o.rcv_saddr);
    }
    let (a1, a2) = (t6(me.rcv6), if o.family == AF_INET6 { t6(o.rcv6) } else { T6::Mapped });
    if a1 == T6::Mapped && a2 == T6::Mapped {
        return !o.v6only && v4(me.rcv_saddr, o.rcv_saddr);
    }
    if a1 == T6::Any && a2 == T6::Any {
        return true;
    }
    if a2 == T6::Any && !(o.v6only && a1 == T6::Mapped) {
        return true;
    }
    if a1 == T6::Any && !(me.v6only && a2 == T6::Mapped) {
        return true;
    }
    o.family == AF_INET6 && o.rcv6 == me.rcv6
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
            if !addr_overlap(me, &o) {
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
        if (!o.reuse || !me.reuse) && addr_overlap(me, &o) {
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
    if u.borrow().family == AF_INET6 {
        return bind6(u, a);
    }
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

/// AF_INET6 の bind (Linux の inet6_bind と __inet6_bind): 長さ (SIN6_LEN_RFC2133 の 24)、family、TCP に multicast は
/// EINVAL、1024 より下は root だけ、もう bind していれば EINVAL。写した番地は v6only なら EINVAL、v4 のたしかめ。IPv6 の
/// 番地は scope のいるもの (fe80::) は口がなければ EINVAL、自分のもの (::1) でなければ EADDRNOTAVAIL。IPv6 だけの
/// 番地に bind したら v6only になる
fn bind6(u: &InetRef, a: &[u8]) -> R {
    if a.len() < 24 {
        return Err(crate::why::inval());
    }
    let (fam, port, addr, scope) = parse6(a);
    if fam != AF_INET6 {
        return Err(-EAFNOSUPPORT);
    }
    let (tcp, netns, freebind, v6only) = {
        let s = u.borrow();
        (s.tcp, s.net, s.freebind, s.v6only)
    };
    if v6_multicast(addr) && tcp {
        return Err(crate::why::inval());
    }
    if port != 0 && port < PRIV_PORT && !proc::current_cred_ref().capable(crate::cred::CAP_NET_BIND_SERVICE) {
        return Err(-EACCES);
    }
    {
        let s = u.borrow();
        if s.state != St::Close || s.num != 0 {
            return Err(crate::why::inval());
        }
    }
    let native = addr != 0 && !is_mapped(addr);
    let v4 = if is_mapped(addr) {
        if v6only {
            return Err(crate::why::inval());
        }
        let v4 = addr as u32;
        if !(v4 == 0 || is_local(v4, netns) || is_multicast(v4) || is_broadcast(v4) || freebind) {
            return Err(-EADDRNOTAVAIL);
        }
        v4
    } else if native {
        if v6_needs_scope(addr) {
            // 口 (scope id) がなければ EINVAL。口があっても、aios の口には link-local の番地がない
            if scope == 0 {
                return Err(crate::why::inval());
            }
            if !crate::netlink::links().iter().any(|l| l.index as u32 == scope) {
                return Err(-ENODEV);
            }
        }
        if !v6_multicast(addr) && !(is_local6(addr) || freebind) {
            return Err(-EADDRNOTAVAIL);
        }
        LOOPBACK4_IPV6
    } else {
        0
    };
    {
        let mut s = u.borrow_mut();
        s.rcv_saddr = v4;
        s.saddr = v4;
        s.rcv6 = addr;
        if !v6_multicast(addr) {
            s.saddr6 = addr;
        }
        if native {
            s.v6only = true;
            s.v6 = true;
        }
    }
    let no_port = u.borrow().bind_no_port;
    if port != 0 || !no_port {
        if let Err(e) = get_port(u, port) {
            let mut s = u.borrow_mut();
            s.v6only = v6only;
            reset_saddr(&mut s);
            return Err(e);
        }
    }
    {
        let mut s = u.borrow_mut();
        if addr != 0 {
            s.lock_addr = true;
        }
        if port != 0 {
            s.lock_port = true;
        }
        s.sport = s.num;
        s.daddr = 0;
        s.dport = 0;
    }
    if !native {
        ext_udp_bind(u);
    }
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
    let mut at = match s.state {
        St::SynSent => s.syn_next,
        St::TimeWait | St::FinWait2 if s.dead => s.expires,
        _ => 0,
    };
    // ファイアウォールが落としたデータの送りなおし
    if s.fw_next != 0 && (at == 0 || s.fw_next < at) {
        at = s.fw_next;
    }
    // おくらせている ACK
    if s.ack_due != 0 && (at == 0 || s.ack_due < at) {
        at = s.ack_due;
    }
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
    let (netns, tcp, v6, s6, d6) = {
        let s = u.borrow();
        (s.net, s.tcp, s.v6, s.rcv6, s.daddr6)
    };
    for o in owners(netns, tcp, sport) {
        if Rc::ptr_eq(&o, u) {
            continue;
        }
        let (hit, tw, exp) = {
            let x = o.borrow();
            let same = if v6 { x.v6 && x.rcv6 == s6 && x.daddr6 == d6 } else { !x.v6 && x.rcv_saddr == saddr && x.daddr == daddr };
            let hit = same && x.dport == dport && !matches!(x.state, St::Listen | St::Close);
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
    // v6only の AF_INET6 は IPv4 を受けない。同じなら AF_INET を先に (compute_score)、そのなかで新しいもの
    let pick = |exact: bool| {
        let ok = |o: &&InetRef| {
            let o = o.borrow();
            !o.v6only && if exact { o.rcv_saddr == daddr } else { o.rcv_saddr == 0 }
        };
        let v: Vec<&InetRef> = list.iter().rev().filter(ok).collect();
        v.iter().find(|o| o.borrow().family == AF_INET).or(v.first()).map(|o| (*o).clone())
    };
    pick(true).or_else(|| pick(false))
}

/// IPv6 のあて先に listen しているもの (inet6_lookup_listener: AF_INET6 だけ、その番地のものを先に、なければ ::)
fn lookup_listener6(netns: u64, daddr: u128, dport: u16) -> Option<InetRef> {
    let k = (netns, dport);
    let v = listeners().get(&k)?;
    let list: Vec<InetRef> = v
        .iter()
        .filter_map(|(&q, w)| w.upgrade().filter(|o| {
            let o = o.borrow();
            o.state == St::Listen && o.hashed && o.pseq == q && o.num == dport && o.family == AF_INET6
        }))
        .collect();
    let pick = |want: u128| list.iter().rev().find(|o| o.borrow().rcv6 == want).cloned();
    pick(daddr).or_else(|| pick(0))
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

/// IPv6 のループバックで知らせる MSS
fn adv_mss6(user_mss: i32) -> i32 {
    if user_mss != 0 && user_mss < ADV_MSS6 { user_mss } else { ADV_MSS6 }
}

/// SYN を送る (connect とやりなおし): 自分につないだもの、listen しているものの行列 (あふれていれば落とされる:
/// SYN_SENT のまま、やりなおしの時刻を決める)、いなければ RST (ECONNREFUSED)。in_call は connect のシステムコールの
/// 中か (ファイアウォールが落としたわけを、そのシステムコールに残す。tick のやりなおしでは残さない)
fn syn(u: &InetRef, in_call: bool) {
    let _p = crate::prof::span("inet:syn");
    let (netns, saddr, sport, daddr, dport, my_mss, my_ttl, v6, saddr6, daddr6) = {
        let s = u.borrow();
        (s.net, s.saddr, s.num, s.daddr, s.dport, s.user_mss, if s.uc_ttl < 0 { 64 } else { s.uc_ttl }, s.v6, s.saddr6, s.daddr6)
    };
    // IPv6 のループバックの MSS と窓
    let (adv, half_client) = if v6 { (adv_mss6 as fn(i32) -> i32, HALF_WND_CLIENT6) } else { (adv_mss as fn(i32) -> i32, HALF_WND_CLIENT) };
    // ファイアウォール (lo の 4 つのフック): Linux が lo に流す SYN と同じもの。落ちれば届かなかった SYN と同じく
    // やりなおす (reject の RST や ICMP が届けば、それで終わる)。IPv6 のものは IPv6 の頭で
    let fw = netns == 0 && crate::nfhook::active();
    let isn = if fw { crate::rand::u32() } else { 0 };
    let (fwd, back) = {
        let s = u.borrow();
        (ends(&s, false), ends(&s, true))
    };
    if fw && !fw_tcp(Some(u), fwd, sport, dport, crate::nfhook::SYN, isn, 0, adv(my_mss) as u16, in_call) {
        if u.borrow().state == St::SynSent {
            syn_lost(u);
        }
        return;
    }
    // 自分につないだ (同時に開く: SYN を受けて SYN_RECV、SYN-ACK を受けて ESTABLISHED)
    if (if v6 { saddr6 == daddr6 } else { saddr == daddr }) && sport == dport {
        let mut s = u.borrow_mut();
        established(&mut s, adv(my_mss), HALF_WND_SERVER);
        s.peer = Some(Rc::downgrade(u));
        s.syn_tries = 0;
        drop(s);
        notify(u);
        return;
    }
    let found = if v6 { lookup_listener6(netns, daddr6, dport) } else { lookup_listener(netns, daddr, dport) };
    let Some(l) = found else {
        // だれもいない: カーネルが返す RST (tcp_v4_send_reset) も決まりを通る。落ちれば SYN をやりなおす
        if fw && !fw_tcp(None, back, dport, sport, crate::nfhook::RST | crate::nfhook::ACK, 0, isn.wrapping_add(1), 0, false) {
            syn_lost(u);
            return;
        }
        tcp_reset(u);
        return;
    };
    let full = {
        let l = l.borrow();
        l.acceptq.len() as u32 > l.max_backlog
    };
    if full {
        syn_lost(u);
        return;
    }
    // SYN-ACK (listen しているほうから)。落ちれば SYN をやりなおす (返すものは listen しているほうへ: 見ない)
    let sisn = if fw { crate::rand::u32() } else { 0 };
    if fw {
        let theirs = adv(l.borrow().user_mss) as u16;
        if !fw_tcp(None, back, dport, sport, crate::nfhook::SYN | crate::nfhook::ACK, sisn, isn.wrapping_add(1), theirs, false) {
            syn_lost(u);
            return;
        }
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
            // AF_INET6 で待つものの子は AF_INET6 (IPv4 のつながりなら名前は写した番地)。IPv6 の設定も引きつぐ
            x.family = l.family;
            x.v6only = l.v6only;
            x.v6 = v6;
            x.rcv6 = if v6 { daddr6 } else { mapped(daddr) };
            x.saddr6 = x.rcv6;
            x.daddr6 = if v6 { saddr6 } else { mapped(saddr) };
            x.hops6 = l.hops6;
            x.mc_hops6 = l.mc_hops6;
            x.mc_loop6 = l.mc_loop6;
            x.tclass = l.tclass;
            x.rx_pktinfo6 = l.rx_pktinfo6;
            x.rx_tclass = l.rx_tclass;
            x.rx_hoplimit = l.rx_hoplimit;
            x.pmtudisc6 = l.pmtudisc6;
            x.recverr6 = l.recverr6;
            x.dontfrag6 = l.dontfrag6;
            x.rxpmtu_on = l.rxpmtu_on;
            x.frag_size = l.frag_size;
            x.ss = Ss::Connected;
            established(&mut x, adv(my_mss), HALF_WND_SERVER);
            // listen する前の shutdown の印も写る (Linux の sk_clone_lock は sk_shutdown もそのまま写す)
            x.shutdown = l.shutdown;
            x.peer = Some(Rc::downgrade(u));
        }
        c
    };
    let lport = l.borrow().num;
    hash_port(&c, lport);
    {
        let theirs = adv(l.borrow().user_mss);
        let mut s = u.borrow_mut();
        established(&mut s, theirs, half_client);
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
    // 3 つ目の ACK (SYN_RECV → ESTABLISHED、ASSURED)。このあとの FIN や RST も ct_seg で
    if fw {
        u.borrow_mut().ct_seq = isn.wrapping_add(1);
        c.borrow_mut().ct_seq = sisn.wrapping_add(1);
        ct_seg(u, crate::nfhook::ACK);
    }
    l.borrow_mut().acceptq.push_back(c);
    notify(&l);
    notify(u);
}

/// 中どうしの TCP の、データのない segment (3 つ目の ACK、閉じるときの FIN と ACK、RST) を Linux が lo に流すのと
/// 同じ形 (timestamp のオプションつきの 52 バイト、窓 512) で lo の決まりと conntrack に通す (u から相手へ)。落とされても
/// 止めない (つながりはもうできている。データの segment は通さない)
fn ct_seg(u: &InetRef, flags: u8) {
    use crate::nfhook;
    // ACK を運ぶ segment なら、おくらせていた ACK はいらない
    if flags & nfhook::ACK != 0 {
        u.borrow_mut().ack_due = 0;
    }
    if !nfhook::active() {
        return;
    }
    let (e, sport, dport, seq, tos, ttl, peer, sk, prio) = {
        let s = u.borrow();
        (ends(&s, false), s.sport, s.dport, s.ct_seq, ip_tos(&s), ip_ttl(&s), upgrade(&s.peer), (s.uid, s.gid), s.priority)
    };
    if seq == 0 {
        return;
    }
    let ack = peer.as_ref().map_or(0, |p| p.borrow().ct_seq);
    let fl = if flags & nfhook::RST != 0 && ack == 0 { flags & !nfhook::ACK } else { flags };
    let ts = (crate::timer::uptime_ns() / 1_000_000) as u32;
    let mut opt = [1u8, 1, 8, 10, 0, 0, 0, 0, 0, 0, 0, 0];
    opt[4..8].copy_from_slice(&ts.to_be_bytes());
    opt[8..12].copy_from_slice(&ts.to_be_bytes());
    let mut pkt = nfhook::tcp_ends(e, sport, dport, seq, ack, fl, 512, &opt, tos, ttl, 0, &[]);
    let _ = nfhook::lo(&mut pkt, Some(sk), 0, prio);
    if flags & nfhook::FIN != 0 {
        u.borrow_mut().ct_seq = seq.wrapping_add(1);
    }
}

/// SYN が届かなかった (listen の行列があふれた、ファイアウォールが落とした): SYN_SENT のまま、やりなおす時刻を決める
fn syn_lost(u: &InetRef) {
    let mut s = u.borrow_mut();
    let rto = 1u64 << s.syn_tries.min(10);
    s.syn_next = ticks() + rto * HZ;
    drop(s);
    arm(u);
}

// ---- ファイアウォール (nftables: nfhook.rs と nftables.rs) ----

/// 中どうしの TCP の 1 つのパケット (SYN、SYN-ACK、RST) を、Linux が lo に流すものと同じ形で lo の決まりに通す。
/// 通れば true。落ちて返すもの (reject の RST か ICMP) が届けば、送った側 (from) にそれを当てる
#[allow(clippy::too_many_arguments)]
fn fw_tcp(from: Option<&InetRef>, e: crate::nfhook::Ends, sport: u16, dport: u16, flags: u8, seq: u32, ack: u32, mss: u16, in_call: bool) -> bool {
    use crate::nfhook;
    let v6 = matches!(e, nfhook::Ends::V6(..));
    let (tos, ttl, sk, prio) = match from {
        Some(u) => {
            let s = u.borrow();
            (ip_tos(&s), ip_ttl(&s), Some((s.uid, s.gid)), s.priority)
        }
        None => (0, 64, None, 0),
    };
    let ts = (crate::timer::uptime_ns() / 1_000_000) as u32;
    let mut pkt = if flags & nfhook::SYN != 0 {
        // SYN は窓 65495、SYN-ACK は 65483 (lo の MTU と既定の入れ物での Linux の値。IPv6 は 65476 と 65464)。IP の id は
        // SYN が乱数、ほかは 0
        let opts = nfhook::syn_opts(mss, ts, if flags & nfhook::ACK != 0 { ts } else { 0 });
        let (win, id) = match (flags & nfhook::ACK != 0, v6) {
            (true, false) => (65483, 0),
            (false, false) => (65495, crate::rand::u32() as u16),
            (true, true) => (65464, 0),
            (false, true) => (65476, 0),
        };
        nfhook::tcp_ends(e, sport, dport, seq, ack, flags, win, &opts, tos, ttl, id, &[])
    } else {
        nfhook::tcp_ends(e, sport, dport, seq, ack, flags, 0, &[], tos, ttl, 0, &[])
    };
    let reply = match nfhook::lo(&mut pkt, sk, 0, prio) {
        nfhook::Path::Pass => return true,
        nfhook::Path::Out(r, why) | nfhook::Path::In(r, why) => {
            if in_call {
                crate::why::firewall(&why.text());
            }
            r
        }
    };
    if let (Some(u), Some(r)) = (from, reply) {
        if r[0] >> 4 == 6 {
            // ICMPv6 の到達不能 (tcp_v6_err) か RST
            match nfhook::icmp6_of(&r) {
                Some(code) => tcp_icmp(u, nfhook::icmp6_errno(code).0),
                None if r.get(6) == Some(&58) => {}
                None => tcp_reset(u),
            }
            return false;
        }
        match nfhook::icmp_of(&r) {
            Some((3, code)) => tcp_icmp(u, nfhook::icmp_errno(code).0),
            Some(_) => {}
            None => tcp_reset(u),
        }
    }
    false
}

/// lo の決まりに通すパケットの TOS と TTL (IPv6 なら traffic class と hop limit)
fn ip_tos(s: &Inet) -> u8 {
    if s.v6 { s.tclass as u8 } else { s.tos }
}

fn ip_ttl(s: &Inet) -> u8 {
    let t = if s.v6 { s.hops6 } else { s.uc_ttl };
    if t < 0 { 64 } else { t as u8 }
}

/// ICMP の到達不能を受けた TCP (Linux の tcp_v4_err): つなぐところ (SYN_SENT、SYN_RECV) ならそのエラーで終わる。
/// つながったものには、やわらかいエラー (sk_err_soft) なので何もしない
fn tcp_icmp(u: &InetRef, errno: i64) {
    let (st, ext) = {
        let s = u.borrow();
        (s.state, s.ext)
    };
    if !matches!(st, St::SynSent | St::SynRecv) {
        return;
    }
    // smoltcp の abort と close は相手が決まったまま CLOSED にするので RST を送るが、Linux はつなぐ途中 (SYN_SENT) で
    // やめるときは何も送らない (tcp_done): 写しの smoltcp に足した forget で
    if let Some(h) = ext {
        let t = tcp_sock(h);
        if t.state() == tcp::State::SynSent {
            t.forget();
        } else {
            t.abort();
        }
    }
    u.borrow_mut().err = -errno;
    tcp_done(u);
}

/// smoltcp が出す TCP のソケットの持ち主 (meta skuid / skgid)。ip は IPv4 の頭から。listen しているものの SYN-ACK
/// は、その持ち主
pub fn ext_owner(ip: &[u8]) -> Option<(u32, u32)> {
    if *ip.first()? >> 4 == 6 {
        return ext_owner6(ip);
    }
    let ihl = (*ip.first()? & 15) as usize * 4;
    if ip.len() < ihl + 4 || ip[9] != 6 {
        return None;
    }
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let (sa, da) = (be32(&ip[12..16]), be32(&ip[16..20]));
    let (sp, dp) = (u16::from_be_bytes([ip[ihl], ip[ihl + 1]]), u16::from_be_bytes([ip[ihl + 2], ip[ihl + 3]]));
    let mut listener = None;
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let Ok(s) = u.try_borrow() else { continue };
        if !s.tcp {
            continue;
        }
        if s.ext.is_some() && s.sport == sp && s.daddr == da && s.dport == dp {
            return Some((s.uid, s.gid));
        }
        if s.ext_listen.is_some() && s.num == sp && (s.rcv_saddr == 0 || s.rcv_saddr == sa) {
            listener = Some((s.uid, s.gid));
        }
    }
    listener
}

/// ext_owner の IPv6 (smoltcp が出す TCP)
fn ext_owner6(ip: &[u8]) -> Option<(u32, u32)> {
    let (thoff, proto, _) = crate::nfhook::v6_l4(ip)?;
    if proto != 6 || ip.len() < thoff + 4 {
        return None;
    }
    let sa = u128::from_be_bytes(ip[8..24].try_into().unwrap());
    let da = u128::from_be_bytes(ip[24..40].try_into().unwrap());
    let (sp, dp) = (u16::from_be_bytes([ip[thoff], ip[thoff + 1]]), u16::from_be_bytes([ip[thoff + 2], ip[thoff + 3]]));
    let mut listener = None;
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let Ok(s) = u.try_borrow() else { continue };
        if !s.tcp {
            continue;
        }
        if s.ext.is_some() && s.v6 && s.sport == sp && s.daddr6 == da && s.dport == dp {
            return Some((s.uid, s.gid));
        }
        if s.ext_listen.is_some() && s.num == sp && (s.rcv6 == 0 || s.rcv6 == sa) {
            listener = Some((s.uid, s.gid));
        }
    }
    listener
}

/// 受けたパケット (eth0 から、自分あて。IP の頭から) の early demux (ip_rcv_finish_core の tcp_v4_early_demux /
/// udp_v4_early_demux と IPv6 のもの): TCP はつながり (SYN_SENT も: ehash) のソケット、UDP は connect して 4 つが
/// みな合うソケットの持ち主 (fsuid, fsgid)。listen しているものと断片には見ない。LOCAL_IN の meta skuid / skgid
pub fn demux_owner(ip: &[u8]) -> Option<(u32, u32)> {
    let v6 = *ip.first()? >> 4 == 6;
    let (thoff, proto) = if v6 {
        let (off, proto, frag) = crate::nfhook::v6_l4(ip)?;
        if frag != 0 {
            return None;
        }
        (off, proto)
    } else {
        let ihl = (*ip.first()? & 15) as usize * 4;
        // MF か offset のある断片
        if ip.len() < 20 || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
            return None;
        }
        (ihl, ip[9])
    };
    if (proto != 6 && proto != 17) || ip.len() < thoff + 4 {
        return None;
    }
    let (sp, dp) = (u16::from_be_bytes([ip[thoff], ip[thoff + 1]]), u16::from_be_bytes([ip[thoff + 2], ip[thoff + 3]]));
    let (sa, da) = if v6 {
        (u128::from_be_bytes(ip[8..24].try_into().unwrap()), u128::from_be_bytes(ip[24..40].try_into().unwrap()))
    } else {
        (u32::from_be_bytes(ip[12..16].try_into().unwrap()) as u128, u32::from_be_bytes(ip[16..20].try_into().unwrap()) as u128)
    };
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let Ok(s) = u.try_borrow() else { continue };
        let (peer, me) = if v6 {
            if !s.v6 {
                continue;
            }
            (s.daddr6, if s.rcv6 != 0 { s.rcv6 } else { s.saddr6 })
        } else {
            if s.v6 || (s.family == AF_INET6 && s.v6only) {
                continue;
            }
            (s.daddr as u128, if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr } as u128)
        };
        let hit = if proto == 6 {
            s.tcp && s.ext.is_some() && s.sport == dp && peer == sa && s.dport == sp
        } else {
            !s.tcp && s.state == St::Established && s.num == dp && peer == sa && s.dport == sp && me == da
        };
        if hit {
            return Some((s.uid, s.gid));
        }
    }
    None
}

/// ファイアウォールが返した ICMP の到達不能 (自分あて。IPv4 の頭から) を、送ったソケットに (tcp_v4_err、__udp4_lib_err)
pub fn icmp_error(r: &[u8]) {
    if r.len() < 28 + 20 + 8 || r[20] != 3 {
        return;
    }
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let (code, offender) = (r[21], be32(&r[12..16]));
    let inner = &r[28..];
    let ihl = (inner[0] & 15) as usize * 4;
    if inner.len() < ihl + 8 {
        return;
    }
    let (sa, da, proto) = (be32(&inner[12..16]), be32(&inner[16..20]), inner[9]);
    let (sp, dp) = (u16::from_be_bytes([inner[ihl], inner[ihl + 1]]), u16::from_be_bytes([inner[ihl + 2], inner[ihl + 3]]));
    match proto {
        6 => {
            let u = all().iter().filter_map(|w| w.upgrade()).find(|u| u.try_borrow().is_ok_and(|s| s.tcp && s.saddr == sa && s.sport == sp && s.daddr == da && s.dport == dp && s.state != St::Listen));
            if let Some(u) = u {
                tcp_icmp(&u, crate::nfhook::icmp_errno(code).0);
            }
        }
        17 => {
            if let Some(u) = udp_lookup(0, da, dp, sa, sp)
                && u.try_borrow_mut().is_ok()
            {
                icmp_err_udp(&u, &inner[ihl + 8..], da, dp, sa, code, offender, crate::netlink::ETH_INDEX as u32);
            }
        }
        _ => {}
    }
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
            if u.borrow().family == AF_INET6 {
                tcp_v6_connect(u, a)?;
            } else {
                tcp_v4_connect(u, a)?;
            }
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
    syn(u, true);
    Ok(())
}

/// AF_INET6 の TCP の connect (Linux の tcp_v6_connect): 長さと family、:: は ::1 (写した番地に bind していれば
/// 127.0.0.1)、multicast は ENETUNREACH、scope のいるものは口がなければ EINVAL。写した番地は v6only なら ENETUNREACH、
/// でなければ IPv4 の connect (名前は写した番地)。IPv6 は、写した番地に bind していれば EAFNOSUPPORT
/// (ip6_dst_lookup_tail)、みち (::1 だけ)、ポート、SYN
fn tcp_v6_connect(u: &InetRef, a: &[u8]) -> Result<(), i64> {
    if a.len() < 24 {
        return Err(crate::why::inval());
    }
    let (fam, dport, mut daddr, scope) = parse6(a);
    if fam != AF_INET6 {
        return Err(-EAFNOSUPPORT);
    }
    let (rcv6, v6only) = {
        let s = u.borrow();
        (s.rcv6, s.v6only)
    };
    if daddr == 0 {
        daddr = if is_mapped(rcv6) { mapped(LOOPBACK) } else { LOOPBACK6 };
    }
    if v6_multicast(daddr) {
        return Err(-ENETUNREACH);
    }
    if v6_needs_scope(daddr) && scope == 0 {
        return Err(crate::why::inval());
    }
    if is_mapped(daddr) {
        if v6only {
            return Err(-ENETUNREACH);
        }
        tcp_v4_connect(u, &sockaddr(daddr as u32, dport))?;
        let mut s = u.borrow_mut();
        s.rcv6 = mapped(s.rcv_saddr);
        s.saddr6 = s.rcv6;
        s.daddr6 = mapped(s.daddr);
        return Ok(());
    }
    if is_mapped(rcv6) {
        return Err(-EAFNOSUPPORT);
    }
    let (netns, prefs) = (u.borrow().net, u.borrow().srcprefs);
    let (daddr, src, local) = route6(netns, daddr, rcv6, scope, prefs)?;
    {
        let mut s = u.borrow_mut();
        if s.rcv6 == 0 {
            s.rcv6 = src;
        }
        s.saddr6 = s.rcv6;
        s.daddr6 = daddr;
        s.v6 = true;
        s.saddr = LOOPBACK4_IPV6;
        s.rcv_saddr = LOOPBACK4_IPV6;
        s.daddr = LOOPBACK4_IPV6;
        s.dport = dport;
        s.state = St::SynSent;
        s.syn_tries = 0;
        s.syn_next = 0;
        s.err = 0;
    }
    let r = if u.borrow().num == 0 {
        get_port(u, 0).map_err(|_| -EADDRNOTAVAIL)
    } else {
        let p1 = u.borrow().num;
        if tuple_used(u, LOOPBACK4_IPV6, p1, LOOPBACK4_IPV6, dport) { Err(-EADDRNOTAVAIL) } else { Ok(()) }
    };
    if let Err(e) = r {
        connect_failed(u);
        return Err(e);
    }
    {
        let mut s = u.borrow_mut();
        s.sport = s.num;
        s.mss_cache = if s.user_mss != 0 { s.user_mss } else { MSS_DEFAULT } - 12;
    }
    if !local {
        if let Err(e) = ext_connect(u) {
            connect_failed(u);
            return Err(e);
        }
        return Ok(());
    }
    syn(u, true);
    Ok(())
}

/// tcp_v4_connect / tcp_v6_connect がしくじった: CLOSE にもどす (bind で決めていないアドレスとポートは手放す)
fn connect_failed(u: &InetRef) {
    let lock_port = {
        let mut s = u.borrow_mut();
        s.state = St::Close;
        if !s.lock_addr {
            reset_saddr(&mut s);
        }
        s.daddr6 = 0;
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
            t.family = s.family;
            t.rcv6 = s.rcv6;
            t.saddr6 = s.saddr6;
            t.daddr6 = s.daddr6;
            t.v6only = s.v6only;
            t.v6 = s.v6;
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
        ct_seg(u, crate::nfhook::RST | crate::nfhook::ACK);
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
    // ファイアウォールが落としたデータが残っていれば、FIN はそのあと (送りなおしが終わってから)
    if !u.borrow().fw_hold.is_empty() {
        u.borrow_mut().fw_fin = true;
        return;
    }
    if let Some(p) = &peer {
        // conntrack には FIN と、相手の ACK を (ESTABLISHED の相手はおくらせる: ack_due)
        ct_seg(u, crate::nfhook::FIN | crate::nfhook::ACK);
        if !Rc::ptr_eq(p, u) {
            if p.borrow().state == St::Established && crate::nfhook::active() {
                p.borrow_mut().ack_due = ticks() + DELACK_MIN;
                arm(p);
            } else {
                ct_seg(p, crate::nfhook::ACK);
            }
        }
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
            reset_saddr(&mut s);
        }
        s.daddr6 = 0;
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
    /// 送り元の名前 (TCP はなし: msg_namelen は 0)。sockaddr_in か sockaddr_in6
    pub from: Option<Vec<u8>>,
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
    // ファイアウォールが落として送りなおしを待つものも、入れ物を使っている
    let held: usize = if Rc::ptr_eq(&p, u) { 0 } else { u.borrow().fw_hold.iter().map(|x| x.1.len()).sum() };
    let s = p.borrow();
    let cap = if s.lock_rcvbuf { s.rcvbuf } else { TCP_QUEUE.max(s.rcvbuf) };
    let used = s.rcv_nxt.wrapping_sub(s.copied_seq) as usize;
    Some(cap.saturating_sub(used).saturating_sub(held))
}

/// 相手 (aios の中) に届ける。ファイアウォールがあれば、データの segment (MSS ずつ) を Linux が lo に流すのと同じ形で
/// 決まりに通し (相手の ACK も)、落ちたものとそのあとのものは送りなおしの列へ (fw_hold: 200 ms から倍に、
/// 120 秒まで。15 回で ETIMEDOUT: Linux の tcp_retries2)
fn deliver(u: &InetRef, data: Vec<u8>, urg: bool) {
    if !crate::nfhook::active() || data.is_empty() {
        return deliver_now(u, data, urg);
    }
    let Some(p) = upgrade(&u.borrow().peer) else { return deliver_now(u, data, urg) };
    if Rc::ptr_eq(&p, u) {
        return deliver_now(u, data, urg);
    }
    // ファイアウォールが入る前につないだもの: ここから数える (conntrack は途中から拾う)
    for x in [u, &p] {
        let mut s = x.borrow_mut();
        if s.ct_seq == 0 {
            s.ct_seq = crate::rand::u32() | 1;
        }
    }
    let mss = (u.borrow().mss_cache.max(536) as usize).min(65483);
    let n = data.len();
    let mut at = 0;
    while at < n {
        let end = (at + mss).min(n);
        let seg = data[at..end].to_vec();
        let surg = urg && end == n;
        let seq = {
            let mut s = u.borrow_mut();
            let q = s.ct_seq;
            s.ct_seq = q.wrapping_add(seg.len() as u32);
            q
        };
        if !u.borrow().fw_hold.is_empty() || !fw_data(u, seq, &seg, surg) {
            if u.borrow().state == St::Close {
                return;
            }
            fw_hold(u, seq, seg, surg);
        } else {
            deliver_now(u, seg, surg);
            ct_seg(&p, crate::nfhook::ACK);
            if u.borrow().state == St::Close {
                return;
            }
        }
        at = end;
    }
}

/// データの segment を lo の決まりに通す (PSH ACK、timestamp つき、窓 512)。通れば true。reject の RST が届けば
/// つながりはこわれる (ECONNRESET)。ICMP はつながったものにはやわらかいエラーなので何もしない
fn fw_data(u: &InetRef, seq: u32, seg: &[u8], urg: bool) -> bool {
    // データの segment も ACK を運ぶ
    u.borrow_mut().ack_due = 0;
    use crate::nfhook;
    let (e, sport, dport, tos, ttl, peer, sk, prio) = {
        let s = u.borrow();
        (ends(&s, false), s.sport, s.dport, ip_tos(&s), ip_ttl(&s), upgrade(&s.peer), (s.uid, s.gid), s.priority)
    };
    let ack = peer.as_ref().map_or(0, |p| p.borrow().ct_seq);
    let ts = (crate::timer::uptime_ns() / 1_000_000) as u32;
    let mut opt = [1u8, 1, 8, 10, 0, 0, 0, 0, 0, 0, 0, 0];
    opt[4..8].copy_from_slice(&ts.to_be_bytes());
    opt[8..12].copy_from_slice(&ts.to_be_bytes());
    let flags = nfhook::PSH | nfhook::ACK | if urg { 0x20 } else { 0 };
    // 中身も (payload の決まりのため)
    let mut pkt = nfhook::tcp_ends(e, sport, dport, seq, ack, flags, 512, &opt, tos, ttl, 0, seg);
    let reply = match nfhook::lo(&mut pkt, Some(sk), 0, prio) {
        nfhook::Path::Pass => return true,
        nfhook::Path::Out(r, _) | nfhook::Path::In(r, _) => r,
    };
    if let Some(r) = reply
        && (if r[0] >> 4 == 6 { r.get(6) != Some(&58) } else { nfhook::icmp_of(&r).is_none() })
    {
        tcp_reset(u);
    }
    false
}

/// 落ちた segment を送りなおしの列に (はじめのものなら時計をかける)
fn fw_hold(u: &InetRef, seq: u32, seg: Vec<u8>, urg: bool) {
    {
        let mut s = u.borrow_mut();
        if s.fw_hold.is_empty() {
            s.fw_tries = 0;
            s.fw_next = ticks() + HZ / 5;
        }
        s.fw_hold.push_back((seq, seg, urg));
    }
    arm(u);
}

/// 送りなおし (tick から): はじめのものから決まりに通し、通ったものは届ける。また落ちれば待ちを倍に、15 回で ETIMEDOUT
fn fw_retransmit(u: &InetRef) {
    let peer = upgrade(&u.borrow().peer);
    loop {
        let front = u.borrow().fw_hold.front().cloned();
        let Some((seq, seg, urg)) = front else { break };
        if !fw_data(u, seq, &seg, urg) {
            if u.borrow().state == St::Close {
                return;
            }
            let give_up = {
                let mut s = u.borrow_mut();
                s.fw_tries += 1;
                s.fw_tries > 15
            };
            if give_up {
                {
                    let mut s = u.borrow_mut();
                    s.fw_hold.clear();
                    s.fw_next = 0;
                    s.fw_fin = false;
                    s.err = -ETIMEDOUT;
                }
                tcp_done(u);
                return;
            }
            {
                let mut s = u.borrow_mut();
                let rto = (HZ / 5) << s.fw_tries.min(10);
                s.fw_next = ticks() + rto.min(120 * HZ);
            }
            arm(u);
            return;
        }
        u.borrow_mut().fw_hold.pop_front();
        deliver_now(u, seg, urg);
        if let Some(p) = &peer {
            ct_seg(p, crate::nfhook::ACK);
        }
        if u.borrow().state == St::Close {
            return;
        }
    }
    // みな届いた: 待っていた FIN も
    let fin = {
        let mut s = u.borrow_mut();
        s.fw_next = 0;
        s.fw_tries = 0;
        core::mem::take(&mut s.fw_fin)
    };
    arm(u);
    notify(u);
    if fin {
        send_fin(u);
    }
}

/// いますぐ相手に届ける。相手が閉じていて読まない (FIN_WAIT2、TIME_WAIT で fd がない) なら RST が返る。
/// 相手が FIN を送ったあと (FIN_WAIT1 / FIN_WAIT2) で RCV も shutdown していれば、相手はこわれて (ECONNRESET) RST が返る
/// (Linux の tcp_rcv_state_process: RFC 1122 の MUST)
fn deliver_now(u: &InetRef, data: Vec<u8>, urg: bool) {
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
/// エラー、自分のエラーか SEND の shutdown で EPIPE、相手の行列があくまで待つ。aios がえらんだちがい (send-notconn):
/// 一度もつないでいない (listen しているものも、connect にしくじったものも、AF_UNSPEC で切ったものも) なら ENOTCONN
/// (POSIX。AF_UNIX の stream は Linux でも ENOTCONN)。Linux は EPIPE と SIGPIPE (send(2) の BUGS)
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
            // 相手のポートは connect で決まり、しくじるか切ると 0 にもどる (閉じられたものは残る)
            if u.borrow().dport == 0 {
                return Err(-ENOTCONN);
            }
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
        // v6only の AF_INET6 は IPv4 を受けない
        if o.rcv_saddr != want || o.v6only {
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

/// IPv6 のあて先の UDP (udp6_lib_lookup の compute_score: AF_INET6 だけ、その番地のものを先に、なければ ::。
/// connect したものは相手が合うこと)
fn udp_lookup6(netns: u64, saddr: u128, sport: u16, daddr: u128, dport: u16) -> Option<InetRef> {
    let v = ports().get(&(netns, false, dport))?;
    let score = |o: &Inet, want: u128| -> i32 {
        if o.family != AF_INET6 || o.rcv6 != want {
            return -1;
        }
        let mut sc = 2;
        if o.dport != 0 {
            if o.dport != sport {
                return -1;
            }
            sc += 4;
        }
        if o.daddr6 != 0 {
            if o.daddr6 != saddr {
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

fn udp_connect(u: &InetRef, a: &[u8]) -> R {
    if u.borrow().family == AF_INET6 { udp6_connect(u, a) } else { udp_connect4(u, a) }
}

/// AF_INET6 の UDP の connect (Linux の __ip6_datagram_connect): AF_UNSPEC は相手を外す。先に autobind。sockaddr_in
/// も受ける (v6only なら EAFNOSUPPORT)。:: は ::1、写した番地は IPv4 の connect (v6only なら ENETUNREACH)。IPv6 は
/// 写した番地に bind していれば EAFNOSUPPORT、みち (::1 だけ)
fn udp6_connect(u: &InetRef, a: &[u8]) -> R {
    if a.len() < 2 {
        return Err(crate::why::inval());
    }
    let fam = u16::from_le_bytes([a[0], a[1]]);
    if fam == AF_UNSPEC {
        udp_disconnect(u);
        return Ok(0);
    }
    autobind(u)?;
    let (v6only, rcv6) = {
        let s = u.borrow();
        (s.v6only, s.rcv6)
    };
    let v4 = |u: &InetRef, a: &[u8]| -> R {
        udp_connect4(u, a)?;
        let mut s = u.borrow_mut();
        s.daddr6 = mapped(s.daddr);
        if s.saddr6 == 0 || s.saddr6 == mapped(0) {
            s.saddr6 = mapped(s.saddr);
        }
        if s.rcv6 == 0 || s.rcv6 == mapped(0) {
            s.rcv6 = mapped(s.rcv_saddr);
        }
        Ok(0)
    };
    if fam == AF_INET {
        if v6only {
            return Err(-EAFNOSUPPORT);
        }
        return v4(u, a);
    }
    if a.len() < 24 {
        return Err(crate::why::inval());
    }
    if fam != AF_INET6 {
        return Err(-EAFNOSUPPORT);
    }
    let (_, port, mut daddr, scope) = parse6(a);
    if daddr == 0 {
        daddr = if is_mapped(rcv6) { mapped(LOOPBACK) } else { LOOPBACK6 };
    }
    if is_mapped(daddr) {
        if v6only {
            return Err(-ENETUNREACH);
        }
        return v4(u, &sockaddr(daddr as u32, port));
    }
    if v6_needs_scope(daddr) && scope == 0 {
        return Err(crate::why::inval());
    }
    if is_mapped(rcv6) {
        return Err(-EAFNOSUPPORT);
    }
    let (netns, prefs) = (u.borrow().net, u.borrow().srcprefs);
    let (daddr, src, _) = route6(netns, daddr, rcv6, scope, prefs)?;
    let mut s = u.borrow_mut();
    s.daddr6 = daddr;
    s.dport = port;
    if s.rcv6 == 0 {
        s.rcv6 = src;
        s.rcv_saddr = LOOPBACK4_IPV6;
    }
    if s.saddr6 == 0 {
        s.saddr6 = src;
        s.saddr = LOOPBACK4_IPV6;
    }
    s.daddr = LOOPBACK4_IPV6;
    s.v6 = true;
    s.state = St::Established;
    Ok(0)
}

/// UDP の connect (Linux の inet_dgram_connect と __ip4_datagram_connect): AF_UNSPEC は相手を外す。先に autobind。
/// 送り元のアドレスが決まっていなければ、みちの送り元に
fn udp_connect4(u: &InetRef, a: &[u8]) -> R {
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
        s.daddr6 = 0;
        if !s.lock_addr {
            reset_saddr(&mut s);
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
    if u.borrow().family == AF_INET6 { udp6_send(u, src, len, to, ctl, flags) } else { udp_send4(u, src, len, to, ctl, flags) }
}

/// AF_INET6 の UDP の sendmsg (Linux の udpv6_sendmsg): あて先は sockaddr_in6 (24 バイトから)、sockaddr_in (IPv4 へ)、
/// AF_UNSPEC (connect したものへ)。写した番地と IPv4 は udp_sendmsg (v6only なら ENETUNREACH)。IPv6 は :: が ::1、
/// ポート 0 は EINVAL、みち (::1 だけ)、autobind、届ける (だれもいなければ ICMPv6 の port unreachable: connect した相手
/// なら次に ECONNREFUSED)
/// ソケットが IPv6 のマルチキャストの組に入る (ipv6_sock_mc_join): マルチキャストでなければ EINVAL、同じ組 (口が 0 か
/// 同じ口) にもう入っていれば EADDRINUSE。口が 0 なら道から (ff00::/8 は eth0)、ない口は ENODEV。組の表は netns 0 の
/// 口のもの (ほかの netns では ENODEV)
fn mc6_join(u: &InetRef, ifindex: i32, g: u128) -> Result<(), i64> {
    if !v6_multicast(g) {
        return Err(crate::why::inval());
    }
    let netns = u.borrow().net;
    if u.borrow().mc6.iter().any(|&(x, i)| x == g && (ifindex == 0 || i == ifindex)) {
        return Err(-EADDRINUSE);
    }
    let dev = if netns != 0 {
        0
    } else if ifindex == 0 {
        if crate::addrconf::lookup(g).is_some() { crate::netlink::ETH_INDEX } else { 0 }
    } else if crate::netlink::links().iter().any(|l| l.index == ifindex) {
        ifindex
    } else {
        0
    };
    if dev == 0 {
        return Err(-ENODEV);
    }
    crate::netlink::mc6_sync();
    crate::addrconf::mc_inc(dev, g);
    crate::netlink::mc6_sync();
    u.borrow_mut().mc6.insert(0, (g, dev));
    Ok(())
}

/// 組を出る (ipv6_sock_mc_drop): 口が 0 なら、その組のうち新しく入ったもの。入っていなければ EADDRNOTAVAIL
fn mc6_drop(u: &InetRef, ifindex: i32, g: u128) -> Result<(), i64> {
    if !v6_multicast(g) {
        return Err(crate::why::inval());
    }
    let pos = u.borrow().mc6.iter().position(|&(x, i)| x == g && (ifindex == 0 || i == ifindex)).ok_or(-EADDRNOTAVAIL)?;
    let (_, dev) = u.borrow_mut().mc6.remove(pos);
    crate::netlink::mc6_sync();
    crate::addrconf::mc_dec(dev, g);
    crate::netlink::mc6_sync();
    Ok(())
}

/// 閉じるときに入っていた組をみな出る (ipv6_sock_mc_close: 新しいものから)
fn mc6_close(u: &InetRef) {
    let v = core::mem::take(&mut u.borrow_mut().mc6);
    for (g, dev) in v {
        crate::netlink::mc6_sync();
        crate::addrconf::mc_dec(dev, g);
        crate::netlink::mc6_sync();
    }
}

/// マルチキャストの UDP を受ける (__udp6_lib_mcast_deliver と __udp_v6_is_mcast_sock): 同じポートの AF_INET6 のソケット
/// のうち、bind した番地が :: かその組で、つないだ相手が合い、その組に入っている (入っていなければ IPV6_MULTICAST_ALL
/// の) もの、みなに。ifindex は受けた口 (自分が送ったものの折りかえしも eth0)。だれもいなくても ICMPv6 は返さない
#[allow(clippy::too_many_arguments)]
fn udp6_mcast_deliver(netns: u64, src: u128, sport: u16, dst: u128, dport: u16, data: &[u8], hops: u8, tclass: u8, ifindex: i32) {
    let Some(v) = ports().get(&(netns, false, dport)) else { return };
    let socks: Vec<InetRef> = v.values().filter_map(|w| w.upgrade()).collect();
    for r in socks {
        let ts = udp_truesize(data.len());
        let ok = {
            let mut x = r.borrow_mut();
            let want = x.family == AF_INET6
                && !x.tcp
                && !x.dead
                && (x.rcv6 == 0 || x.rcv6 == dst)
                && (x.dport == 0 || x.dport == sport)
                && (x.daddr6 == 0 || x.daddr6 == src)
                && (x.mc_all6 || x.mc6.iter().any(|m| m.0 == dst));
            let full = x.rmem + ts > x.rcvbuf && x.rmem != 0;
            if want && !full {
                x.rmem += ts;
                x.dq.push_back(Dgram { data: data.to_vec(), from: (LOOPBACK4_IPV6, sport), to: LOOPBACK4_IPV6, v6: Some((src, dst)), tos: tclass, ttl: hops, truesize: ts, ifindex: ifindex as u32 });
            }
            want && !full
        };
        if ok {
            notify(&r);
        }
    }
}

/// eth0 から来たマルチキャストの UDP (IPv6 の頭から。eth0 が入っている組あてのもの: net.rs が smoltcp に渡さずに)。
/// チェックサムがおかしいもの (0 も) は捨てる
pub fn udp6_mcast_rx(ip: &[u8]) {
    if ip.len() < 48 {
        return;
    }
    let plen = u16::from_be_bytes([ip[4], ip[5]]) as usize;
    let ulen = u16::from_be_bytes([ip[44], ip[45]]) as usize;
    if ulen < 8 || ulen > plen || 40 + plen > ip.len() || ip[46..48] == [0, 0] {
        return;
    }
    let udp = &ip[40..40 + ulen];
    let mut sum = crate::nfhook::csum_add(0, &ip[8..40]);
    sum = crate::nfhook::csum_add(sum, &(ulen as u32).to_be_bytes());
    sum = crate::nfhook::csum_add(sum, &[0, 0, 0, 17]);
    if crate::nfhook::csum_fold(crate::nfhook::csum_add(sum, udp)) != 0 {
        return;
    }
    let src = u128::from_be_bytes(ip[8..24].try_into().unwrap());
    let dst = u128::from_be_bytes(ip[24..40].try_into().unwrap());
    let tclass = ((u16::from_be_bytes([ip[0], ip[1]]) >> 4) & 0xff) as u8;
    let (sport, dport) = (u16::from_be_bytes([udp[0], udp[1]]), u16::from_be_bytes([udp[2], udp[3]]));
    udp6_mcast_deliver(0, src, sport, dst, dport, &udp[8..], ip[7], tclass, crate::netlink::ETH_INDEX);
}

/// sendmsg の付帯データの IPV6_DONTFRAG (ip6_datagram_send_ctl: int でなければ EINVAL)。ほかは見ない
fn cmsg6_dontfrag(ctl: &[u8]) -> Result<Option<bool>, i64> {
    let mut df = None;
    let mut off = 0usize;
    while off + 16 <= ctl.len() {
        let clen = u64::from_le_bytes(ctl[off..off + 8].try_into().unwrap()) as usize;
        let level = i32::from_le_bytes(ctl[off + 8..off + 12].try_into().unwrap());
        let typ = i32::from_le_bytes(ctl[off + 12..off + 16].try_into().unwrap());
        if clen < 16 || clen > ctl.len() - off {
            return Err(crate::why::inval());
        }
        if (level, typ) == (IPPROTO_IPV6, 62) {
            if clen != 20 {
                return Err(crate::why::inval());
            }
            df = Some(i32::from_le_bytes(ctl[off + 16..off + 20].try_into().unwrap()) != 0);
        }
        off += clen.div_ceil(8) * 8;
    }
    Ok(df)
}

/// 送るときに分かったエラーをエラーの行列に (ipv6_local_error / ip_local_error: IPV6_RECVERR / IP_RECVERR のときだけ。
/// sk_err は変えない)
fn local_error(u: &InetRef, v6: bool, to: (u128, u16), info: u32) {
    let mut s = u.borrow_mut();
    if if v6 { s.recverr6 } else { s.recverr } {
        s.errq.push_back(ErrMsg { data: Vec::new(), errno: EMSGSIZE as u32, v6, typ: 0, code: 0, info, to, offender: 0, ifindex: 0, local: 0, origin: EE_ORIGIN_LOCAL });
        drop(s);
        notify(u);
    }
}

/// IPv6 の UDP のデータグラムが MTU に入るか (__ip6_append_data): MTU は ip6_setup_cork の fragsize (IPV6_MTU_DISCOVER
/// が PROBE からは口の MTU、ほかは path MTU。自分あては lo の mtu。IPV6_MTU の frag_size がそれより小さければそれ)。
/// DONTFRAG (ソケットか付帯データ) で入らなければ IPV6_PATHMTU を残して EMSGSIZE、DO・PROBE・INTERFACE で入らないか、
/// 断片にしても IPv6 の 65535 に入らなければ EMSGSIZE (どれも IPV6_RECVERR なら SO_EE_ORIGIN_LOCAL、ee_info は MTU)。
/// 返すのは eth0 の断片のたのみ (断片にするかを決める MTU、断片の大きさの MTU)。いつもどおりなら (0, 0)
fn udp6_mtu_check(u: &InetRef, len: usize, daddr: u128, dport: u16, local: bool, cmsg_df: Option<bool>) -> Result<(u32, u32), i64> {
    let (pmtud, frag, df) = {
        let s = u.borrow();
        (s.pmtudisc6, s.frag_size, cmsg_df.unwrap_or(s.dontfrag6))
    };
    let dev_mtu = if local { crate::addrconf::CONF.get()[crate::addrconf::CONF_LO].mtu as u32 } else { 1500 };
    let limit = if local || pmtud >= 3 { dev_mtu } else { crate::addrconf::path_mtu(daddr) };
    let mtu = if frag != 0 && frag < limit { frag } else { limit };
    let ulen = len + 8;
    if df && ulen > mtu as usize - 40 {
        {
            let mut s = u.borrow_mut();
            if s.rxpmtu_on {
                s.rxpmtu = Some((daddr, if v6_needs_scope(daddr) { crate::netlink::ETH_INDEX as u32 } else { 0 }, mtu));
            }
        }
        local_error(u, true, (daddr, dport), mtu);
        return Err(-EMSGSIZE);
    }
    let ignore_df = pmtud < 2 || pmtud == 5;
    let maxnonfrag = if ignore_df { 40 + 0xffff } else { mtu as usize };
    if ulen > maxnonfrag - 40 {
        local_error(u, true, (daddr, dport), mtu);
        return Err(-EMSGSIZE);
    }
    let dflt = crate::addrconf::path_mtu(daddr);
    Ok(if local || (limit == dflt && mtu == dflt) { (0, 0) } else { (limit, mtu) })
}

fn udp6_send(u: &InetRef, src: &Src, len: usize, to: Option<&[u8]>, ctl: &[u8], flags: u64) -> R {
    let (v6only, saddr6, rcv6, pending) = {
        let s = u.borrow();
        (s.v6only, s.saddr6, s.rcv6, s.pending.is_some())
    };
    let mut dest: Option<(u128, u16)> = None;
    let mut scope = 0;
    if let Some(a) = to {
        if a.len() < 2 {
            return Err(crate::why::inval());
        }
        match u16::from_le_bytes([a[0], a[1]]) {
            AF_INET6 => {
                if a.len() < 24 {
                    return Err(crate::why::inval());
                }
                let (_, port, mut d, sc) = parse6(a);
                if d == 0 && is_mapped(saddr6) {
                    d = mapped(LOOPBACK);
                }
                if v6_needs_scope(d) && sc == 0 {
                    return Err(crate::why::inval());
                }
                scope = sc;
                dest = Some((d, port));
            }
            AF_INET => return if v6only { Err(-ENETUNREACH) } else { udp_send4(u, src, len, to, ctl, flags) },
            AF_UNSPEC => {}
            _ => return Err(crate::why::inval()),
        }
    }
    let (daddr, dport) = match dest {
        Some(d) => d,
        None if pending => return udp_send4(u, src, len, None, ctl, flags),
        None => {
            let s = u.borrow();
            if s.state != St::Established {
                return Err(-EDESTADDRREQ);
            }
            (peer6(&s), s.dport)
        }
    };
    if is_mapped(daddr) {
        if v6only {
            return Err(-ENETUNREACH);
        }
        let to4 = sockaddr(daddr as u32, dport);
        return udp_send4(u, src, len, if dest.is_some() { Some(&to4) } else { None }, ctl, flags);
    }
    if flags & MSG_OOB != 0 {
        return Err(-EOPNOTSUPP);
    }
    if dest.is_some() && dport == 0 {
        return Err(crate::why::inval());
    }
    let cmsg_df = cmsg6_dontfrag(ctl)?;
    let daddr = if daddr == 0 { LOOPBACK6 } else { daddr };
    if is_mapped(rcv6) {
        return Err(-EAFNOSUPPORT);
    }
    let netns = u.borrow().net;
    let prefs = u.borrow().srcprefs;
    let (daddr, src6, local) = route6(netns, daddr, if rcv6 != 0 { rcv6 } else { saddr6 }, scope, prefs)?;
    let (limit, chunk) = udp6_mtu_check(u, len, daddr, dport, local, cmsg_df)?;
    autobind(u)?;
    let e = take_err(u);
    if e != 0 {
        return Err(e);
    }
    if u.borrow().shutdown & SEND != 0 {
        return Err(-EPIPE);
    }
    let data = take(src, 0, len)?;
    let (netns, sport, hops, tclass, sk, prio) = {
        let s = u.borrow();
        (s.net, s.sport, if s.hops6 < 0 { 64 } else { s.hops6 as u8 }, s.tclass as u8, (s.uid, s.gid), s.priority)
    };
    // ファイアウォール (自分あては lo の 4 つのフック、外へは LOCAL_OUT と POST_ROUTING。IPv6 の頭で)。出す側で落ちたら
    // EPERM。返ってきた ICMPv6 の到達不能は connect したもので致命のものなら次に (__udp6_lib_err)
    let fw = netns == 0 && crate::nfhook::active();
    if fw {
        use crate::nfhook;
        let hops = if local {
            hops
        } else if v6_multicast(daddr) {
            u.borrow().mc_hops6 as u8
        } else {
            u.borrow().hops6.try_into().unwrap_or(crate::addrconf::hop_limit())
        };
        let mut pkt = nfhook::udp_ends(nfhook::Ends::V6(src6, daddr), sport, dport, &data, tclass, hops, 0, false);
        let path = if local { nfhook::lo(&mut pkt, Some(sk), 0, prio) } else { nfhook::out_eth(&mut pkt, Some(sk), 0, prio) };
        let (reply, out, why) = match path {
            nfhook::Path::Pass => (None, false, None),
            nfhook::Path::Out(r, w) => (r, true, Some(w)),
            nfhook::Path::In(r, w) => (r, false, Some(w)),
        };
        if let Some(r) = reply.as_deref()
            && let Some(code) = nfhook::icmp6_of(r)
        {
            udp6_icmp(u, &data, src6, daddr, dport, 1, code, u128::from_be_bytes(r[8..24].try_into().unwrap()), 1, 0);
        }
        if let Some(w) = why {
            if out {
                crate::why::firewall(&w.text());
                return Err(-EPERM);
            }
            return Ok(len as i64);
        }
    }
    // マルチキャスト (ip6_finish_output2): hop limit は IPV6_MULTICAST_HOPS。IPV6_MULTICAST_LOOP で eth0 がその組に
    // 入っていれば自分にも (dev_loopback_xmit: 受けた口は eth0。決まりには通さない)。interface-local (ff01::) は外に出さない
    if v6_multicast(daddr) {
        let (lp, mh) = {
            let s = u.borrow();
            (s.mc_loop6, s.mc_hops6 as u8)
        };
        if lp && crate::addrconf::mc_has(crate::netlink::ETH_INDEX, daddr) {
            udp6_mcast_deliver(netns, src6, sport, daddr, dport, &data, mh, tclass, crate::netlink::ETH_INDEX);
        }
        if (daddr >> 112) & 0xf > 1 {
            if (limit, chunk) != (0, 0) {
                net::frag_hint(src6, daddr, sport, dport, limit, chunk);
            }
            ext_udp_send6(u, &data, src6, daddr, dport, Some(mh))?;
        }
        return Ok(len as i64);
    }
    // 外へ (eth0): smoltcp の UDP のソケットから
    if !local {
        if (limit, chunk) != (0, 0) {
            net::frag_hint(src6, daddr, sport, dport, limit, chunk);
        }
        let hops = u.borrow().hops6;
        ext_udp_send6(u, &data, src6, daddr, dport, if hops < 0 { None } else { Some(hops as u8) })?;
        return Ok(len as i64);
    }
    match udp_lookup6(netns, src6, sport, daddr, dport) {
        Some(r) => {
            let ts = udp_truesize(data.len());
            let ok = {
                let mut x = r.borrow_mut();
                let full = x.rmem + ts > x.rcvbuf && x.rmem != 0;
                if !full {
                    x.rmem += ts;
                    x.dq.push_back(Dgram { data, from: (LOOPBACK4_IPV6, sport), to: LOOPBACK4_IPV6, v6: Some((src6, daddr)), tos: tclass, ttl: hops, truesize: ts, ifindex: 1 });
                }
                !full
            };
            if ok {
                notify(&r);
            }
        }
        None => {
            // だれもいない: カーネルの ICMPv6 の port unreachable (ファイアウォールがあれば、それも lo の道を通る)
            if fw {
                use crate::nfhook;
                let mut pkt = nfhook::udp_ends(nfhook::Ends::V6(src6, daddr), sport, dport, &data, tclass, hops, 0, false);
                if let Some(mut icmp) = nfhook::kernel_unreach6(&mut pkt, 4)
                    && matches!(nfhook::lo(&mut icmp, None, 0, 0), nfhook::Path::Pass)
                {
                    udp6_icmp(u, &data, src6, daddr, dport, 1, 4, daddr, 1, 0);
                }
            } else {
                // icmp6_send の送り元は、自分あての番地 (ここではもとのあて先)
                udp6_icmp(u, &data, src6, daddr, dport, 1, 4, daddr, 1, 0);
            }
        }
    }
    Ok(len as i64)
}

/// ICMPv6 のエラーの errno と、致命か (icmpv6_err_convert: 到達不能は code ごと、time exceeded は EHOSTUNREACH、
/// parameter problem は EPROTO)。ほかの種類 (packet too big など) は None
fn icmp6_err(typ: u8, code: u8) -> Option<(i64, bool)> {
    match typ {
        1 => Some(crate::nfhook::icmp6_errno(code)),
        3 => Some((113, false)),
        4 => Some((71, true)),
        _ => None,
    }
}

/// ICMPv6 のエラーを受けた UDP (__udp6_lib_err と ipv6_icmp_error)。data は送ったものの UDP の後ろ、src6 は送り元、
/// offender は ICMPv6 を送ってきた番地、ifindex はそれを受けた口、info は packet too big の MTU。IPV6_RECVERR ならいつも
/// (つないでいなくても) 行列に入れて sk_err にも、でなければ致命のもので connect したものだけ sk_err に。packet too big
/// は EMSGSIZE で、IPV6_MTU_DISCOVER が DONT でなければ致命 (INTERFACE と OMIT は見ない: ip6_sk_accept_pmtu)
#[allow(clippy::too_many_arguments)]
fn udp6_icmp(u: &InetRef, data: &[u8], src6: u128, daddr: u128, dport: u16, typ: u8, code: u8, offender: u128, ifindex: u32, info: u32) {
    let pmtudisc = u.borrow().pmtudisc6;
    let (errno, fatal) = if typ == 2 {
        if pmtudisc >= 4 {
            return;
        }
        (EMSGSIZE, pmtudisc != 0)
    } else {
        let Some(e) = icmp6_err(typ, code) else { return };
        e
    };
    let deliver = {
        let s = u.borrow();
        let connected = s.state == St::Established;
        let peer_ok = !connected || (s.daddr6 == daddr && s.dport == dport);
        peer_ok && (s.recverr6 || (fatal && connected))
    };
    if !deliver {
        return;
    }
    {
        let mut s = u.borrow_mut();
        if s.recverr6 {
            // ICMPv6 のエラーにのるのは 1280 バイトまで (IPv6 と ICMPv6 の頭、送ったものの IPv6 と UDP の頭をのぞいた中身)
            let keep = data.len().min(1280 - 40 - 8 - 40 - 8);
            s.errq.push_back(ErrMsg { data: data[..keep].to_vec(), errno: errno as u32, v6: true, typ, code, info, to: (daddr, dport), offender, ifindex, local: src6, origin: 3 });
        }
        s.err = -errno;
    }
    notify(u);
}

fn udp_send4(u: &InetRef, src: &Src, len: usize, to: Option<&[u8]>, ctl: &[u8], flags: u64) -> R {
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
    // __ip_append_data: IP_MTU_DISCOVER が DO・PROBE・INTERFACE なら MTU (自分あては lo の 65536、ほかは eth0 の 1500) に、
    // ほかは断片にしても 65535 に入らなければ EMSGSIZE (IP_RECVERR なら SO_EE_ORIGIN_LOCAL、名前のポートはつないだ相手の)
    let already = pending.map_or(0, |p| p.2);
    let (pmtud, cport) = {
        let s = u.borrow();
        (s.pmtudisc, if s.state == St::Established { s.dport } else { 0 })
    };
    let mtu: usize = if local { 65536 } else { 1500 };
    let maxnonfrag = if pmtud < 2 || pmtud == 5 { 0xffff } else { mtu };
    if already + len + 8 > maxnonfrag - 20 {
        u.borrow_mut().pending = None;
        local_error(u, false, (daddr as u128, cport), mtu as u32);
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
    let fw = netns == 0 && crate::nfhook::active();
    if !local {
        if fw {
            let (sport, my_tos, my_ttl) = {
                let s = u.borrow();
                (s.sport, s.tos, s.uc_ttl)
            };
            let t = ttl.unwrap_or(if my_ttl < 0 { 64 } else { my_ttl as u8 });
            let src = if saddr >> 24 == 127 || saddr == 0 { net::src4(daddr) } else { saddr };
            if !fw_udp(u, src, sport, daddr, dport, &data, tos.unwrap_or(my_tos), t, false)? {
                return Ok(len as i64);
            }
        }
        ext_udp_send(u, &data, daddr, dport, ttl)?;
        return Ok(len as i64);
    }
    let (sport, my_tos, my_ttl) = {
        let s = u.borrow();
        (s.sport, s.tos, s.uc_ttl)
    };
    let ttl = ttl.unwrap_or(if my_ttl < 0 { 64 } else { my_ttl as u8 });
    let tos = tos.unwrap_or(my_tos);
    if fw && !fw_udp(u, saddr, sport, daddr, dport, &data, tos, ttl, true)? {
        return Ok(len as i64);
    }
    match udp_lookup(netns, saddr, sport, daddr, dport) {
        Some(r) => {
            let ts = udp_truesize(data.len());
            let ok = {
                let mut x = r.borrow_mut();
                let full = x.rmem + ts > x.rcvbuf && x.rmem != 0;
                if !full {
                    x.rmem += ts;
                    x.dq.push_back(Dgram { data, from: (saddr, sport), to: daddr, v6: None, tos, ttl, truesize: ts, ifindex: 1 });
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
        // IP_RECVERR なら、エラーを覚える。ファイアウォールがあれば、その ICMP も lo の決まりを通る
        None => {
            if fw {
                let pkt = crate::nfhook::udp(saddr, sport, daddr, dport, &data, tos, ttl, 0, true);
                if let Some(mut icmp) = crate::nfhook::kernel_unreach(&pkt, 3)
                    && matches!(crate::nfhook::lo(&mut icmp, None, 0, 0), crate::nfhook::Path::Pass)
                {
                    icmp_unreach(u, &data, daddr, dport, saddr);
                }
            } else {
                icmp_unreach(u, &data, daddr, dport, saddr);
            }
        }
    }
    Ok(len as i64)
}

/// ICMP の port unreachable を受けた (Linux の __udp4_lib_err と ip_icmp_error)
fn icmp_unreach(u: &InetRef, data: &[u8], daddr: u32, dport: u16, saddr: u32) {
    icmp_err_udp(u, data, daddr, dport, saddr, 3, daddr, 1);
}

/// ICMP の到達不能 (code) を受けた UDP。IP_RECVERR ならいつも、でなければ致命のもの (icmp_err_convert の fatal) で
/// connect したものだけ、エラーを覚える。offender は ICMP を送ってきたアドレス
#[allow(clippy::too_many_arguments)]
fn icmp_err_udp(u: &InetRef, data: &[u8], daddr: u32, dport: u16, saddr: u32, code: u8, offender: u32, ifindex: u32) {
    let (errno, fatal) = crate::nfhook::icmp_errno(code);
    let deliver = {
        let s = u.borrow();
        // 自分あてか (送り元と相手で、このソケットが受けるものか)
        let mine = s.rcv_saddr == 0 || s.rcv_saddr == saddr;
        let peer_ok = s.daddr == 0 || (s.daddr == daddr && (s.dport == 0 || s.dport == dport));
        mine && peer_ok && (s.recverr || (fatal && s.state == St::Established))
    };
    if !deliver {
        return;
    }
    {
        let mut s = u.borrow_mut();
        if s.recverr {
            // ICMP にのるのは 576 バイトまで (IP と ICMP の頭、送ったものの IP と UDP の頭をのぞいた中身)
            let keep = data.len().min(576 - 20 - 8 - 20 - 8);
            s.errq.push_back(ErrMsg { data: data[..keep].to_vec(), errno: errno as u32, v6: false, typ: 3, code, info: 0, to: (daddr as u128, dport), offender: offender as u128, ifindex, local: saddr as u128, origin: 2 });
        }
        s.err = -errno;
    }
    notify(u);
}

/// UDP のデータグラムを決まりに通す (中どうしは lo の 4 つ、外へは eth0 の LOCAL_OUT と POST_ROUTING)。Err は出す側で
/// 落ちた (sendmsg は EPERM)、Ok(false) は受ける側で落ちた (sendmsg はうまくいったことに)。返すもの (reject の ICMP) は
/// 送ったソケットに
#[allow(clippy::too_many_arguments)]
fn fw_udp(u: &InetRef, saddr: u32, sport: u16, daddr: u32, dport: u16, data: &[u8], tos: u8, ttl: u8, local: bool) -> Result<bool, i64> {
    use crate::nfhook;
    let (sk, prio, df, connected) = {
        let s = u.borrow();
        (Some((s.uid, s.gid)), s.priority, s.pmtudisc != 0, s.state == St::Established)
    };
    let id = if df && !connected { 0 } else { crate::rand::u32() as u16 };
    let mut pkt = nfhook::udp(saddr, sport, daddr, dport, data, tos, ttl, id, df);
    let path = if local { nfhook::lo(&mut pkt, sk, 0, prio) } else { nfhook::out_eth(&mut pkt, sk, 0, prio) };
    let (reply, out, why) = match path {
        nfhook::Path::Pass => return Ok(true),
        nfhook::Path::Out(r, w) => (r, true, w),
        nfhook::Path::In(r, w) => (r, false, w),
    };
    if let Some(r) = reply
        && let Some((3, code)) = nfhook::icmp_of(&r)
    {
        let offender = u32::from_be_bytes([r[12], r[13], r[14], r[15]]);
        icmp_err_udp(u, data, daddr, dport, saddr, code, offender, 1);
    }
    if out {
        crate::why::firewall(&why.text());
        return Err(-EPERM);
    }
    Ok(false)
}

/// AF_INET6 のソケットに見せる番地 (IPv4 のものは v4 を写した ::ffff:a.b.c.d)
fn err_addr6(m: &ErrMsg, a: u128) -> u128 {
    if m.v6 { a } else { mapped(a as u32) }
}

/// IP_RECVERR / IPV6_RECVERR の付帯データ (struct sock_extended_err と、知らせてきた番地の sockaddr_in か sockaddr_in6)
fn exterr(m: &ErrMsg, fam6: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(44);
    b.extend_from_slice(&m.errno.to_le_bytes());
    // ee_origin (SO_EE_ORIGIN_ICMP6 か SO_EE_ORIGIN_ICMP)、ee_type、ee_code、ee_pad、ee_info (packet too big の MTU)、
    // ee_data (0)
    b.extend_from_slice(&[m.origin, m.typ, m.code, 0]);
    b.extend_from_slice(&m.info.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    // SO_EE_ORIGIN_LOCAL は知らせてきた番地がない (AF_UNSPEC)
    if m.origin == EE_ORIGIN_LOCAL {
        b.resize(b.len() + if fam6 { 28 } else { 16 }, 0);
    } else if fam6 {
        b.extend_from_slice(&sockaddr6(err_addr6(m, m.offender), 0));
    } else {
        b.extend_from_slice(&sockaddr(m.offender as u32, 0));
    }
    b
}

/// MSG_ERRQUEUE (Linux の ip_recv_error と ipv6_recv_error): なければ EAGAIN。名前は送ったあて先、付帯データは
/// IP_PKTINFO / IPV6_PKTINFO (その ICMP を受けた口と番地。たのんでいれば) と IP_RECVERR / IPV6_RECVERR。読んだら
/// sk_err を次のものの errno に
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
    r.flags |= MSG_ERRQUEUE as u32;
    let local = m.origin == EE_ORIGIN_LOCAL;
    if s.family == AF_INET6 {
        let mut name = sockaddr6(err_addr6(&m, m.to.0), m.to.1);
        if local {
            // ipv6_recv_error の scope は受けた口から (送るときのものは 0)
            name[24..28].fill(0);
        }
        r.from = Some(name.to_vec());
        if s.rx_pktinfo6 && !local {
            let mut b = Vec::with_capacity(20);
            b.extend_from_slice(&err_addr6(&m, m.local).to_be_bytes());
            b.extend_from_slice(&m.ifindex.to_le_bytes());
            r.cmsgs.push((41, 50, b));
        }
        r.cmsgs.push((41, 25, exterr(&m, true)));
    } else {
        r.from = Some(sockaddr(m.to.0 as u32, m.to.1).to_vec());
        if s.pktinfo && !local {
            // in_pktinfo: 口、ipi_spec_dst (エラーでは 0)、そのあて先
            let mut b = Vec::with_capacity(12);
            b.extend_from_slice(&m.ifindex.to_le_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&(m.local as u32).to_be_bytes());
            r.cmsgs.push((0, 8, b));
        }
        r.cmsgs.push((0, 11, exterr(&m, false)));
    }
    // sock_dequeue_err_skb: 次が ICMP のものならその errno を sk_err に、ICMP のものを読んで次がなければ 0 に (送るときの
    // エラーは sk_err を変えない)
    let icmp_next = s.errq.front().is_some_and(|x| x.origin != EE_ORIGIN_LOCAL);
    if icmp_next {
        s.err = -(s.errq.front().unwrap().errno as i64);
    } else if !local {
        s.err = 0;
    }
    Ok(r)
}

/// UDP の recvmsg (Linux の udp_recvmsg と __skb_recv_udp): たまっているエラーが先、はじめのデータグラム (MSG_PEEK
/// と SO_PEEK_OFF)、入れ物より大きければ MSG_TRUNC (返す数は、MSG_TRUNC を渡せばデータグラムの長さ)
fn udp_recv(u: &InetRef, len: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    let _p = crate::prof::span("inet:udp_recv");
    if flags & MSG_ERRQUEUE != 0 {
        return recv_error(u, len);
    }
    // IPV6_RECVPATHMTU のもの (ipv6_recv_rxpmtu): 中身は 0 バイト、名前はあて先 (port 0)、付帯データ IPV6_PATHMTU
    {
        let mut s = u.borrow_mut();
        if s.rxpmtu_on
            && let Some((addr, scope, mtu)) = s.rxpmtu.take()
        {
            let mut name = [0u8; 28];
            name[..2].copy_from_slice(&AF_INET6.to_le_bytes());
            name[8..24].copy_from_slice(&addr.to_be_bytes());
            name[24..28].copy_from_slice(&scope.to_le_bytes());
            let mut info = name.to_vec();
            info.extend_from_slice(&mtu.to_le_bytes());
            let mut r = Recv::new();
            r.from = Some(name.to_vec());
            r.cmsgs.push((41, 61, info));
            return Ok(r);
        }
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
            r.from = Some(if s.family == AF_INET6 {
                sockaddr6(d.v6.map_or(mapped(d.from.0), |v| v.0), d.from.1).to_vec()
            } else {
                sockaddr(d.from.0, d.from.1).to_vec()
            });
            let (rx6, rxhl, rxtc) = (s.rx_pktinfo6, s.rx_hoplimit, s.rx_tclass);
            let (pktinfo, recvtos, recvttl) = if d.v6.is_some() { (false, false, false) } else { (pktinfo, recvtos, recvttl) };
            if pktinfo {
                let mut b = Vec::with_capacity(12);
                b.extend_from_slice(&d.ifindex.to_le_bytes());
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
            // SOL_IPV6 (ip6_datagram_recv_common_ctl と specific_ctl): IPV6_PKTINFO (in6_pktinfo: あて先と口)、
            // IPV6_HOPLIMIT、IPV6_TCLASS
            if rx6 {
                let mut b = Vec::with_capacity(20);
                b.extend_from_slice(&d.v6.map_or(mapped(d.to), |v| v.1).to_be_bytes());
                b.extend_from_slice(&d.ifindex.to_le_bytes());
                r.cmsgs.push((41, 50, b));
            }
            if rxhl && d.v6.is_some() {
                r.cmsgs.push((41, 52, (d.ttl as i32).to_le_bytes().to_vec()));
            }
            if rxtc && d.v6.is_some() {
                r.cmsgs.push((41, 67, (d.tos as i32).to_le_bytes().to_vec()));
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
    // 中どうし: ファイアウォールが落として送りなおしを待つもの (Linux の SIOCOUTQ はまだ ACK のないもの)
    Ok(s.fw_hold.iter().map(|x| x.1.len()).sum::<usize>() as i32)
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
pub fn name(u: &InetRef, peer: u8) -> Result<Vec<u8>, i64> {
    let s = u.borrow();
    if peer != 0 {
        if s.dport == 0 || (matches!(s.state, St::Close | St::SynSent) && peer == 1) {
            return Err(-ENOTCONN);
        }
        return Ok(name_of(&s, s.daddr, peer6(&s), s.dport));
    }
    let a = if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr };
    Ok(name_of(&s, a, local6(&s), s.sport))
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
    // AF_INET6: SOL_IPV6 (do_ipv6_getsockopt)。SOL_IP は IPv4 のもの、ほかの level は ENOPROTOOPT
    if u.borrow().family == AF_INET6 {
        // IPV6_PATHMTU: struct ip6_mtuinfo (番地は 0、つないだあて先への path MTU。つないでいなければ ENOTCONN)
        if level == IPPROTO_IPV6 && opt == 61 {
            if len < 32 {
                return Err(crate::why::inval());
            }
            let s = u.borrow();
            if s.state != St::Established || s.tcp {
                return Err(-ENOTCONN);
            }
            let mut v = alloc::vec![0u8; 32];
            v[28..32].copy_from_slice(&crate::addrconf::path_mtu(if s.v6 { s.daddr6 } else { mapped(s.daddr) }).to_le_bytes());
            return Ok(v);
        }
        if level == IPPROTO_IPV6 {
            return ipv6_get(u, opt);
        }
        if !matches!(level, SOL_SOCKET | IPPROTO_IP | IPPROTO_TCP | IPPROTO_UDP) {
            return Err(-ENOPROTOOPT);
        }
    }
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
                // SO_PASSPIDFD、SO_PASSPIDFD_THREAD、SO_PEERPIDFD_THREAD は AF_UNIX だけ。SO_PEERPIDFD は相手の pid がない
                // (Linux 7.4 の sk_getsockopt)
                76 | 86 | 87 => return Err(-EOPNOTSUPP),
                77 => return Err(-61),
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
                39 => int(s.family as i64),
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
    if u.borrow().family == AF_INET6 && level == IPPROTO_IPV6 {
        return ipv6_set(u, opt, val);
    }
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

const IPPROTO_IPV6: i32 = 41;

/// SOL_IPV6 の getsockopt (do_ipv6_getsockopt のうち aios が持つもの)
fn ipv6_get(u: &InetRef, opt: i32) -> Result<Vec<u8>, i64> {
    let s = u.borrow();
    let v: i64 = match opt {
        // IPV6_V6ONLY
        26 => s.v6only as i64,
        // IPV6_UNICAST_HOPS (-1 なら口の hop_limit: 64)、IPV6_MULTICAST_HOPS、IPV6_MULTICAST_LOOP
        16 => if s.hops6 < 0 { 64 } else { s.hops6 as i64 },
        18 => s.mc_hops6 as i64,
        19 => s.mc_loop6 as i64,
        29 => s.mc_all6 as i64,
        60 => s.rxpmtu_on as i64,
        62 => s.dontfrag6 as i64,
        // IPV6_ADDR_PREFERENCES: TMP か PUBLIC か PUBTMP_DEFAULT (0x100)、COA か HOME (0x400)
        72 => {
            let p = s.srcprefs;
            (if p & 1 != 0 {
                1
            } else if p & 2 != 0 {
                2
            } else {
                0x100
            }) | if p & 4 != 0 { 4 } else { 0x400 }
        }
        // IPV6_MULTICAST_IF、IPV6_UNICAST_IF
        17 | 76 => 0,
        // IPV6_TCLASS、IPV6_RECVPKTINFO、IPV6_RECVHOPLIMIT、IPV6_RECVTCLASS、IPV6_MTU_DISCOVER、IPV6_RECVERR
        67 => s.tclass as i64,
        49 => s.rx_pktinfo6 as i64,
        51 => s.rx_hoplimit as i64,
        66 => s.rx_tclass as i64,
        23 => s.pmtudisc6 as i64,
        // IPV6_MTU: つないだもののあて先への path MTU (つないでいなければ ENOTCONN)
        24 => {
            if s.state != St::Established {
                return Err(-ENOTCONN);
            }
            crate::addrconf::path_mtu(if s.v6 { s.daddr6 } else { mapped(s.daddr) }) as i64
        }
        25 => s.recverr6 as i64,
        // IPV6_FREEBIND
        78 => s.freebind as i64,
        _ => return Err(-ENOPROTOOPT),
    };
    Ok((v as i32).to_le_bytes().to_vec())
}

/// SOL_IPV6 の setsockopt (do_ipv6_setsockopt のうち aios が持つもの。ほかは ENOPROTOOPT)
fn ipv6_set(u: &InetRef, opt: i32, val: &[u8]) -> R {
    let int = || -> Result<i32, i64> { if val.len() < 4 { Err(crate::why::inval()) } else { Ok(i32::from_le_bytes(val[..4].try_into().unwrap())) } };
    // IPV6_ADD_MEMBERSHIP / DROP_MEMBERSHIP (struct ipv6_mreq: 組と口。TCP は EPROTO) と MCAST_JOIN_GROUP / LEAVE_GROUP
    // (struct group_req: 口と、8 からの sockaddr_storage。AF_INET6 でなければ EADDRNOTAVAIL。TCP も入れる)
    if matches!(opt, 20 | 21 | 42 | 45) {
        let (g, ifindex) = if opt <= 21 {
            if val.len() < 20 {
                return Err(crate::why::inval());
            }
            if u.borrow().tcp {
                return Err(-EPROTO);
            }
            (u128::from_be_bytes(val[..16].try_into().unwrap()), i32::from_le_bytes(val[16..20].try_into().unwrap()))
        } else {
            if val.len() < 136 {
                return Err(crate::why::inval());
            }
            if u16::from_le_bytes([val[8], val[9]]) != AF_INET6 {
                return Err(-EADDRNOTAVAIL);
            }
            (u128::from_be_bytes(val[16..32].try_into().unwrap()), i32::from_le_bytes(val[..4].try_into().unwrap()))
        };
        return if opt == 20 || opt == 42 { mc6_join(u, ifindex, g) } else { mc6_drop(u, ifindex, g) }.map(|_| 0);
    }
    let mut s = u.borrow_mut();
    match opt {
        // IPV6_V6ONLY: bind したら変えられない
        26 => {
            let v = int()?;
            if s.num != 0 {
                return Err(crate::why::inval());
            }
            s.v6only = v != 0;
        }
        16 => {
            let v = int()?;
            if !(-1..=255).contains(&v) {
                return Err(crate::why::inval());
            }
            s.hops6 = v;
        }
        18 => {
            if s.tcp {
                return Err(-ENOPROTOOPT);
            }
            let v = int()?;
            if !(-1..=255).contains(&v) {
                return Err(crate::why::inval());
            }
            s.mc_hops6 = if v == -1 { 1 } else { v };
        }
        19 => {
            let v = int()?;
            if v != 0 && v != 1 {
                return Err(crate::why::inval());
            }
            s.mc_loop6 = v != 0;
        }
        // IPV6_MULTICAST_ALL
        29 => s.mc_all6 = int()? != 0,
        // IPV6_RECVPATHMTU、IPV6_DONTFRAG
        60 => s.rxpmtu_on = int()? != 0,
        62 => s.dontfrag6 = int()? != 0,
        // IPV6_MTU (frag_size): 0 か 1280 から
        24 => {
            let v = int()?;
            if v != 0 && v < 1280 {
                return Err(crate::why::inval());
            }
            s.frag_size = v as u32;
        }
        // IPV6_ADDR_PREFERENCES (__ip6_sock_set_addr_preferences): TMP と PUBLIC と PUBTMP_DEFAULT はどれか 1 つ、HOME と
        // COA、CGA と NONCGA もどちらか (でなければ EINVAL)
        72 => {
            let v = int()? as u32;
            let mut mask = u32::MAX;
            let mut pref = 0;
            match v & 0x103 {
                1 | 2 => {
                    pref |= v & 3;
                    mask &= !3;
                }
                0x100 => mask &= !3,
                0 => {}
                _ => return Err(crate::why::inval()),
            }
            match v & 0x404 {
                0x400 => mask &= !4,
                4 => pref |= 4,
                0 => {}
                _ => return Err(crate::why::inval()),
            }
            if v & 0x808 == 0x808 {
                return Err(crate::why::inval());
            }
            s.srcprefs = (s.srcprefs & mask) | pref;
        }
        // IPV6_TCLASS: -1 は 0、TCP は ECN の 2 ビットを変えない
        67 => {
            let mut v = int()?;
            if !(-1..=0xff).contains(&v) {
                return Err(crate::why::inval());
            }
            if v == -1 {
                v = 0;
            }
            if s.tcp {
                v = (v & !3) | (s.tclass & 3);
            }
            s.tclass = v;
        }
        49 => s.rx_pktinfo6 = int()? != 0,
        51 => s.rx_hoplimit = int()? != 0,
        66 => s.rx_tclass = int()? != 0,
        23 => {
            let v = int()?;
            if !(0..=5).contains(&v) {
                return Err(crate::why::inval());
            }
            s.pmtudisc6 = v;
        }
        25 => {
            s.recverr6 = int()? != 0;
            if !s.recverr6 {
                s.errq.clear();
            }
        }
        78 => s.freebind = int()? != 0,
        _ => return Err(-ENOPROTOOPT),
    }
    Ok(0)
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
            // SO_PASSPIDFD と SO_PASSPIDFD_THREAD は AF_UNIX だけ (Linux 7.4 の sk_setsockopt)
            76 | 86 => return Err(-EOPNOTSUPP),
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
    mc6_close(u);
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
                    syn(&u, false);
                    notify(&u);
                }
            }
            St::TimeWait | St::FinWait2 if dead && exp != 0 && now >= exp => kill(&u),
            _ => {}
        }
        let fw_due = {
            let s = u.borrow();
            s.fw_next != 0 && now >= s.fw_next
        };
        if fw_due {
            fw_retransmit(&u);
        }
        let ack_due = {
            let s = u.borrow();
            s.ack_due != 0 && now >= s.ack_due
        };
        if ack_due {
            ct_seg(&u, crate::nfhook::ACK);
        }
        // まだ先の時刻があれば入れなおす (時刻が動いたもの、やりなおしの SYN がまた落とされたもの)
        arm(&u);
    }
}

// ---- fstat など (Linux の sockfs の inode) ----

pub fn stat(u: &InetRef) -> Stat {
    let s = u.borrow();
    Stat { dev: crate::vfs::encode_dev(crate::vfs::SOCK_DEV), ino: s.ino, uid: s.uid, gid: s.gid, mtime: s.mtime, ctime: s.ctime, ..Stat::dev(0o140000 | s.mode, 0) }
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
    if c.fsuid != s.uid && !c.capable(crate::cred::CAP_FOWNER) {
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
    proc_net_of(want_tcp, false)
}

/// /proc/net/tcp6 と udp6: AF_INET6 のもの (番地は 32 ビットずつ、Linux の s6_addr32 をホストの順で)
pub fn proc_net6(want_tcp: bool) -> String {
    proc_net_of(want_tcp, true)
}

fn proc_net_of(want_tcp: bool, v6: bool) -> String {
    let hex4 = |a: u32| alloc::format!("{:08X}", a.swap_bytes());
    let hex6 = |a: u128| (0..4).map(|i| alloc::format!("{:08X}", ((a >> (96 - 32 * i)) as u32).swap_bytes())).collect::<String>();
    let mut out = String::from(match (want_tcp, v6) {
        (true, false) => "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        (false, false) => "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n",
        (true, true) => "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        (false, true) => "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n",
    });
    let netns = crate::ns::net().unwrap_or(0);
    let mut i = 0;
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let s = u.borrow();
        if s.tcp != want_tcp || s.net != netns || (!s.hashed && s.num == 0) || (s.family == AF_INET6) != v6 {
            continue;
        }
        let (hex_l, hex_r) = if v6 {
            (hex6(local6(&s)), hex6(peer6(&s)))
        } else {
            (hex4(if s.rcv_saddr != 0 { s.rcv_saddr } else { s.saddr }), hex4(s.daddr))
        };
        let inode = if s.has_file { s.ino } else { 0 };
        // Linux の tw (TIME_WAIT と fd のない FIN_WAIT2) は持ち主を覚えていない
        let uid = if is_tw(&s) { 0 } else { s.uid };
        let line = if want_tcp {
            let rx = if s.state == St::Listen { s.acceptq.len() as u32 } else { s.rcv_nxt.wrapping_sub(s.copied_seq) };
            alloc::format!(
                "{:4}: {}:{:04X} {}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5}        0 {} 1 0000000000000000 100 0 0 10 0\n",
                i,
                hex_l,
                s.sport,
                hex_r,
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
                hex_l,
                s.sport,
                hex_r,
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
    if v6 {
        return out;
    }
    // tcp4_seq_show と udp4_seq_show は 1 行を 149 (TMPSZ - 1) と 127 文字に空白でそろえる (seq_setwidth と seq_pad)。
    // tcp6 と udp6 はそろえない
    let width = if want_tcp { 149 } else { 127 };
    out.lines().map(|l| alloc::format!("{:<width$}\n", l, width = width)).collect()
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
    /// AF_INET6 の TCP と UDP の inuse (/proc/net/sockstat6。Linux は prot ごとに数える: ほかの数は v4 と v6 で 1 つ)
    pub inuse6: usize,
    pub udp6: usize,
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
        let six = s.family == AF_INET6;
        if !s.tcp {
            c.udp_all += 1;
            if s.hashed {
                if six {
                    c.udp6 += 1;
                } else {
                    c.udp += 1;
                }
            }
            continue;
        }
        if is_tw(&s) {
            c.tw += 1;
            continue;
        }
        c.alloc += 1;
        if s.state != St::Close {
            if six {
                c.inuse6 += 1;
            } else {
                c.inuse += 1;
            }
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

/// 外から届きうるか (分けていない NET で、0.0.0.0 か eth0 のアドレスに bind したもの。IPv6 は :: か eth0 の IPv6 の
/// 番地に bind したもの)
fn reachable_from_outside(s: &Inet) -> bool {
    if s.net != 0 || net::get().is_none() {
        return false;
    }
    if s.v6 || (s.family == AF_INET6 && s.v6only) {
        return s.rcv6 == 0 || (s.rcv6 != LOOPBACK6 && crate::addrconf::is_mine(s.rcv6));
    }
    net::is_eth_addr(s.rcv_saddr) || s.rcv_saddr == 0
}

/// smoltcp の番地
fn ip6(a: u128) -> IpAddress {
    IpAddress::Ipv6(Ipv6Address::from_bits(a))
}

fn ext_connect(u: &InetRef) -> Result<(), i64> {
    let n = net::get().ok_or(-ENETUNREACH)?;
    let sock = tcp::Socket::new(tcp::SocketBuffer::new(alloc::vec![0; EXT_RX]), tcp::SocketBuffer::new(alloc::vec![0; EXT_TX]));
    let h = n.sockets.add(sock);
    let (daddr, dport, sport, v6, saddr6, daddr6) = {
        let s = u.borrow();
        (s.daddr, s.dport, s.num, s.v6, s.saddr6, s.daddr6)
    };
    // IPv6 は送り元も決めて (route6 がえらんだもの)
    let local = IpListenEndpoint { addr: v6.then(|| ip6(saddr6)), port: sport };
    let ep = if v6 { IpEndpoint::new(ip6(daddr6), dport) } else { IpEndpoint::new(IpAddress::Ipv4(ipv4(daddr)), dport) };
    if n.sockets.get_mut::<tcp::Socket>(h).connect(n.iface.context(), ep, local).is_err() {
        n.sockets.remove(h);
        return Err(crate::why::inval());
    }
    {
        let mut s = u.borrow_mut();
        s.ext = Some(h);
        if !s.v6 && (s.saddr == 0 || s.saddr >> 24 == 127) {
            let src = net::src4(daddr);
            s.saddr = src;
            s.rcv_saddr = src;
        }
    }
    net::poll();
    Ok(())
}

/// smoltcp が決めたこちらのアドレスを (IPv4 なら v4 の欄、IPv6 なら v6 の欄)
fn set_local(s: &mut Inet, a: IpAddress) {
    match a {
        IpAddress::Ipv4(a) => {
            s.saddr = u32::from_be_bytes(a.octets());
            s.rcv_saddr = s.saddr;
        }
        IpAddress::Ipv6(a) => {
            s.saddr6 = a.to_bits();
            s.rcv6 = s.saddr6;
        }
    }
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
                // RST (ICMP のエラーで止めたものは、そのエラーのまま)
                if s.err == 0 {
                    s.err = -ECONNREFUSED;
                }
                s.state = St::Close;
                s.shutdown = BOTH;
                true
            }
            (St::SynSent, _) => {
                s.state = St::Established;
                s.mss_cache = if s.v6 { 1440 } else { 1460 } - 12;
                if !s.lock_sndbuf {
                    s.sndbuf = s.sndbuf.max(87040);
                }
                if let Some(ep) = t.local_endpoint() {
                    set_local(&mut s, ep.addr);
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
    // 送るほうは IPv6 のいちばん大きいデータグラム (65527) が入るように
    let sock = udp::Socket::new(udp::PacketBuffer::new(meta(), alloc::vec![0; 16 * 1024]), udp::PacketBuffer::new(meta(), alloc::vec![0; 64 * 1024]));
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
        let ts = udp_truesize(k);
        let mut s = u.borrow_mut();
        // IPv4 のものは v6only でないもの、IPv6 のものは AF_INET6 のものだけ (同じポートの smoltcp のソケットは両方を受ける)
        let d = match meta.endpoint.addr {
            IpAddress::Ipv4(a) if !(s.family == AF_INET6 && s.v6only) => {
                let to = meta.local_address.and_then(|l| if let IpAddress::Ipv4(x) = l { Some(u32::from_be_bytes(x.octets())) } else { None }).unwrap_or_else(eth);
                Dgram { data: buf, from: (u32::from_be_bytes(a.octets()), meta.endpoint.port), to, v6: None, tos: 0, ttl: 64, truesize: ts, ifindex: crate::netlink::ETH_INDEX as u32 }
            }
            IpAddress::Ipv6(a) if s.family == AF_INET6 => {
                let to = meta.local_address.and_then(|l| if let IpAddress::Ipv6(x) = l { Some(x.to_bits()) } else { None }).unwrap_or(s.rcv6);
                Dgram { data: buf, from: (LOOPBACK4_IPV6, meta.endpoint.port), to: LOOPBACK4_IPV6, v6: Some((a.to_bits(), to)), tos: 0, ttl: 64, truesize: ts, ifindex: crate::netlink::ETH_INDEX as u32 }
            }
            _ => continue,
        };
        // connect したものは、その相手からのものだけ
        if s.state == St::Established && s.dport != 0 && (s.dport != meta.endpoint.port || (d.v6.is_some() && s.daddr6 != d.v6.unwrap().0)) {
            continue;
        }
        if s.rmem + ts > s.rcvbuf && s.rmem != 0 {
            continue;
        }
        s.rmem += ts;
        s.dq.push_back(d);
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

/// IPv6 の外への UDP (udpv6_sendmsg の ip6_dst_lookup のあと): 送り元を決めて smoltcp から
fn ext_udp_send6(u: &InetRef, data: &[u8], src: u128, daddr: u128, dport: u16, hops: Option<u8>) -> Result<(), i64> {
    if u.borrow().ext.is_none() {
        ext_udp_bind(u);
    }
    let Some(h) = u.borrow().ext else { return Err(-ENETUNREACH) };
    let meta = smoltcp::socket::udp::UdpMetadata { endpoint: IpEndpoint::new(ip6(daddr), dport), local_address: Some(ip6(src)), meta: Default::default() };
    udp_sock(h).set_hop_limit(hops);
    udp_sock(h).send_slice(data, meta).map_err(|_| -EAGAIN)?;
    net::poll();
    Ok(())
}

/// eth0 から来た (と、自分あてに送られた) ICMPv6 のエラー (icmpv6_rcv → icmpv6_notify → tcp_v6_err / __udp6_lib_err):
/// 中のもとのパケットの組 (自分の番地とポート、相手の番地とポート) のソケットに。TCP はつなぐ途中ならどのエラーでも
/// 止める。UDP はそれを受けるはずのソケット (つないでいないものも) に、IPV6_RECVERR なら行列に (udp6_icmp)。packet too
/// big (2) は、もとのあて先の path MTU を (addrconf::pmtu_update) 下げて、UDP に EMSGSIZE を (TCP の MSS は smoltcp が
/// 変えられないので、そのまま)
pub fn icmp6_error(ip: &[u8]) {
    if ip.len() < 48 + 40 + 4 || ip[6] != 58 {
        return;
    }
    let (ty, code) = (ip[40], ip[41]);
    let info = u32::from_be_bytes(ip[44..48].try_into().unwrap());
    let errno = match icmp6_err(ty, code) {
        Some((e, _)) => e,
        None if ty == 2 => EMSGSIZE,
        None => return,
    };
    let offender = u128::from_be_bytes(ip[8..24].try_into().unwrap());
    let inner = &ip[48..];
    let Some((thoff, proto, _)) = crate::nfhook::v6_l4(inner) else { return };
    if inner.len() < thoff + 4 || !matches!(proto, 6 | 17) {
        return;
    }
    let src = u128::from_be_bytes(inner[8..24].try_into().unwrap());
    let dst = u128::from_be_bytes(inner[24..40].try_into().unwrap());
    let sport = u16::from_be_bytes([inner[thoff], inner[thoff + 1]]);
    let dport = u16::from_be_bytes([inner[thoff + 2], inner[thoff + 3]]);
    if ty == 2 {
        crate::addrconf::pmtu_update(dst, info);
    }
    if proto == 17 {
        if let Some(u) = udp_lookup6(0, dst, dport, src, sport)
            && u.try_borrow_mut().is_ok()
        {
            let payload = inner.get(thoff + 8..).unwrap_or(&[]);
            // 自分あてに送られたもの (送り元が自分の番地) は lo で受けた
            let ifindex = if crate::addrconf::is_mine(offender) { 1 } else { crate::netlink::ETH_INDEX as u32 };
            udp6_icmp(&u, payload, src, dst, dport, ty, code, offender, ifindex, info);
        }
        return;
    }
    if ty == 2 {
        return;
    }
    for w in all().iter() {
        let Some(u) = w.upgrade() else { continue };
        let hit = {
            let s = u.borrow();
            s.net == 0 && s.v6 && s.tcp && s.sport == sport && s.saddr6 == src && s.daddr6 == dst && s.dport == dport
        };
        if hit {
            tcp_icmp(&u, errno);
            return;
        }
    }
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
            x.nodelay = l.nodelay;
            x.keepalive = l.keepalive;
            x.family = l.family;
            x.v6only = l.v6only;
            if let Some(ep) = remote {
                x.dport = ep.port;
                match ep.addr {
                    IpAddress::Ipv4(a) => x.daddr = u32::from_be_bytes(a.octets()),
                    IpAddress::Ipv6(a) => {
                        // IPv6 のつながり: v4 の欄は LOOPBACK4_IPV6
                        x.v6 = true;
                        x.daddr6 = a.to_bits();
                        x.daddr = LOOPBACK4_IPV6;
                        x.saddr = LOOPBACK4_IPV6;
                        x.rcv_saddr = LOOPBACK4_IPV6;
                        x.mss_cache = 1440 - 12;
                    }
                }
            }
            if let Some(ep) = local {
                set_local(&mut x, ep.addr);
            }
            if !x.v6 {
                x.rcv6 = mapped(x.rcv_saddr);
                x.saddr6 = x.rcv6;
                x.daddr6 = mapped(x.daddr);
            }
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

/// AF_INET の TCP と UDP のソケットの fd の中身 (file::Kind::Ops。Linux の socket_file_ops と inet_stream_ops)
pub struct InetFile(pub InetRef);

impl file::FileOps for InetFile {
    fn describe(&self) -> alloc::string::String {
        describe(&self.0)
    }
    fn class(&self) -> usize {
        4
    }
    fn stream(&self) -> bool {
        true
    }
    fn read(&self, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        read(&self.0, dst, nonblock)
    }
    fn write(&self, src: &[u8], nonblock: bool) -> Result<usize, i64> {
        write(&self.0, src, nonblock)
    }
    fn stat(&self) -> Stat {
        stat(&self.0)
    }
    fn mount(&self) -> (u64, bool) {
        (crate::vfs::SOCK_MNT, false)
    }
    fn event_gen(&self) -> u64 {
        self.0.borrow().generation + crate::net::generation()
    }
    fn debug_state(&self, desc: &str, rw: &str) -> Option<alloc::string::String> {
        Some(alloc::format!("{} {} {}", desc, rw, debug(&self.0)))
    }
    fn pending(&self) -> Option<u64> {
        Some(pending(&self.0))
    }
    fn mask_readiness(&self) -> bool {
        true
    }
    fn poll_mask(&self) -> Option<u32> {
        Some(poll_mask(&self.0))
    }
    fn poll_keys(&self) -> Option<Vec<usize>> {
        Some(poll_keys(&self.0))
    }
    fn lockable(&self) -> bool {
        true
    }
    fn nowait(&self) -> bool {
        true
    }
    fn release(&self) {
        release(&self.0);
    }
    fn has_data(&self) -> bool {
        pending(&self.0) > 0
    }
    fn own_sigpipe(&self) -> bool {
        true
    }
    fn splice_stream(&self) -> bool {
        true
    }
    fn readv(&self, f: &file::FileRef, iov: &[(usize, usize)], nowait: bool) -> Option<Result<i64, i64>> {
        Some(crate::socket::inet_readv(f, &self.0, iov, nowait))
    }
    fn writev(&self, f: &file::FileRef, iov: &[(usize, usize)], nowait: bool, nosignal: bool) -> Option<Result<i64, i64>> {
        Some(crate::socket::inet_writev(f, &self.0, iov, nowait, nosignal))
    }
    fn chmod(&self, mode: u32) -> Option<Result<i64, i64>> {
        Some(chmod(&self.0, mode))
    }
    fn owner(&self) -> Option<u32> {
        Some(owner(&self.0))
    }
    fn owner_mode(&self) -> Option<(u32, u32, u32)> {
        Some(owner_mode(&self.0))
    }
    fn set_owner_mode(&self, uid: Option<u32>, gid: Option<u32>, mode: u32) {
        set_owner_mode(&self.0, uid, gid, mode);
    }
    fn touch(&self, mtime: Option<u64>) {
        touch(&self.0, mtime);
    }
    /// ネットワークのインターフェース (ip、networkd) と、SIOCOUTQ、SIOCATMARK、FIONREAD (Linux の tcp_ioctl と udp_ioctl)
    fn ioctl(&self, req: u64, arg: usize, _nonblock: bool) -> Option<Result<i64, i64>> {
        const SIOCOUTQ: u64 = 0x5411;
        const SIOCATMARK: u64 = 0x8905;
        const FIONREAD: u64 = 0x541b;
        let out = |b: &[u8]| crate::proc::current().pt().copy_out(arg, b).ok_or(-14).map(|_| 0);
        if crate::netif::handles(req) {
            return Some(crate::netif::ioctl(req, arg));
        }
        let v = match req {
            SIOCOUTQ => outq(&self.0),
            SIOCATMARK => atmark(&self.0),
            FIONREAD => inq(&self.0),
            _ => return None,
        };
        Some(v.and_then(|v| out(&v.to_le_bytes())))
    }
    /// TCP のソケット (Linux の tcp_mmap: 書くもの、実行するものは EPERM。ページはなく、さわると SIGBUS。Linux は
    /// getsockopt の TCP_ZEROCOPY_RECEIVE で受けたものをここに入れる)
    fn mmap(&self, fr: &file::FileRef, off: usize, _len: usize, prot: u64) -> Option<Result<crate::vm::Backing, i64>> {
        if !self.0.borrow().tcp {
            return None;
        }
        const PROT_WRITE: u64 = 2;
        const PROT_EXEC: u64 = 4;
        const EPERM: i64 = 1;
        Some(if prot & (PROT_WRITE | PROT_EXEC) != 0 {
            Err(-EPERM)
        } else {
            Ok(crate::vm::Backing::Pages { pages: Rc::new(Vec::new()), off, hold: Some(fr.clone()) })
        })
    }
}
