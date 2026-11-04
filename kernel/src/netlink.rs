// netlink (AF_NETLINK): カーネルとプロセスが、メッセージ (struct nlmsghdr と属性 rtattr) でやりとりするソケット
// (Linux の net/netlink/af_netlink.c と同じ形と答え)。NETLINK_ROUTE の口とアドレスの一覧 (rtnetlink.c の
// RTM_GETLINK と RTM_GETADDR: musl や glibc の getifaddrs と if_nameindex、node の os.networkInterfaces())、道の表
// (RTM_GETROUTE: ip route get、Firefox の NetlinkService がつながっているかを見る。下の「道の表」) と、
// NETLINK_NETFILTER の nftables (nftables.rs。doc/aios.md の「ファイアウォール (nftables)」)。
//
// - socket(AF_NETLINK, SOCK_RAW か SOCK_DGRAM, NETLINK_ROUTE か NETLINK_NETFILTER)。ほかの型は ESOCKTNOSUPPORT、
//   ほかの protocol は EPROTONOSUPPORT (Linux でその機能を入れずに作ったときと同じ)
// - ACK (netlink_ack): しくじりでなければいつも NLM_F_CAPPED。NETLINK_EXT_ACK なら、しくじりのわけと属性の場所
//   (NLMSGERR_ATTR_MSG と NLMSGERR_ATTR_OFFS) を足して NLM_F_ACK_TLVS
// - ポート (nl_pid): bind で決めるか、送るか bind(0) で自分で (はじめはプロセスの tgid、使われていれば -4097 から下へ)
// - カーネル (nl_pid 0) へのメッセージは、送ったときにみな答える (netlink_rcv_skb と同じ順: NLM_F_REQUEST のないもの
//   と制御のもの (type < 16) は答えない (NLM_F_ACK なら 0 の ACK)、しくじりか NLM_F_ACK なら NLMSG_ERROR)。答えは
//   1 つずつデータグラムにして受ける行列へ (dump は中身と NLMSG_DONE の 2 つ)。受けるのはデータグラムごと (短い入れ物
//   なら切って MSG_TRUNC)
// - 口は lo (1) と eth0 (2): ioctl (netif.rs) と /sys/class/net (sysfs.rs) と同じ番号とフラグ
use crate::cell::Bkl;
use crate::file::{self, FileRef, Kind};
use crate::proc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

const EPERM: i64 = 1;
const EAGAIN: i64 = 11;
const ENODEV: i64 = 19;
const EMSGSIZE: i64 = 90;
const ENOPROTOOPT: i64 = 92;
const EPROTONOSUPPORT: i64 = 93;
const ESOCKTNOSUPPORT: i64 = 94;
const EOPNOTSUPP: i64 = 95;
const EADDRINUSE: i64 = 98;
const ECONNREFUSED: i64 = 111;

pub const AF_NETLINK: u16 = 16;
const SOCK_DGRAM: u32 = 2;
const SOCK_RAW: u32 = 3;
const NETLINK_ROUTE: u32 = 0;
/// Linux の MAX_LINKS (protocol はこれより下)
const MAX_LINKS: u32 = 32;

// nlmsghdr の type と flags (include/uapi/linux/netlink.h)
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_MIN_TYPE: u16 = 16;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_CAPPED: u16 = 0x100;
const NLMSG_HDRLEN: usize = 16;

// rtnetlink (include/uapi/linux/rtnetlink.h)
const RTM_BASE: u16 = 16;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;
/// これより大きい type は EOPNOTSUPP (RTM_MAX: 7.3 は RTM_NEWTUNNEL の次の組のおわり)
const RTM_MAX: u16 = 123;

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_BRIDGE: u8 = 7;

// IFLA_* (if_link.h) と IFA_* (if_addr.h)
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_STATS: u16 = 7;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_CARRIER_CHANGES: u16 = 35;
const IFLA_MIN_MTU: u16 = 50;
const IFLA_MAX_MTU: u16 = 51;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_F_PERMANENT: u32 = 0x80;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_HOST: u8 = 254;

// ソケットのオプション
const SOL_SOCKET: u64 = 1;
const SOL_NETLINK: u64 = 270;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_PASSCRED: u64 = 16;
const SO_SNDBUFFORCE: u64 = 32;
const SO_RCVBUFFORCE: u64 = 33;
const SO_PROTOCOL: u64 = 38;
const SO_DOMAIN: u64 = 39;
const NETLINK_ADD_MEMBERSHIP: u64 = 1;
const NETLINK_DROP_MEMBERSHIP: u64 = 2;
const NETLINK_PKTINFO: u64 = 3;
const NETLINK_BROADCAST_ERROR: u64 = 4;
const NETLINK_NO_ENOBUFS: u64 = 5;
const NETLINK_LISTEN_ALL_NSID: u64 = 8;
const NETLINK_LIST_MEMBERSHIPS: u64 = 9;
const NETLINK_CAP_ACK: u64 = 10;
const NETLINK_EXT_ACK: u64 = 11;
const NETLINK_GET_STRICT_CHK: u64 = 12;
/// NETLINK_ROUTE の multicast の組の数 (RTNLGRP_MAX)
const RTNLGRP_MAX: u32 = 37;
/// 受ける・送る入れ物の大きさのはじめ (net.core.rmem_default と wmem_default)
const BUF_DEFAULT: usize = 212992;

pub const MSG_PEEK: u64 = 0x2;
pub const MSG_TRUNC: u64 = 0x20;
pub const MSG_DONTWAIT: u64 = 0x40;

/// フラグのオプション (NETLINK_PKTINFO から NETLINK_GET_STRICT_CHK)。ビットは opt の番号
#[derive(Default)]
pub struct Netlink {
    pub protocol: u32,
    pub typ: u32,
    /// ポート (0 はまだない)
    portid: u32,
    bound: bool,
    /// 入っている multicast の組 (ビット)
    groups: u64,
    /// connect した相手
    dst_pid: u32,
    dst_groups: u32,
    /// 受ける行列 (データグラムごとに、送ったもののポート: カーネルは 0)
    rq: VecDeque<(u32, Vec<u8>)>,
    rcvbuf: usize,
    sndbuf: usize,
    /// NETLINK_* のフラグのオプション (opt の番号のビット)
    opts: u32,
    passcred: bool,
    ino: usize,
    /// 受ける行列が変わった数 (poll の覚え)
    generation: u64,
}

pub type NetlinkRef = Rc<RefCell<Netlink>>;

/// 使っているポート ((protocol, portid) → ソケット)
static PORTS: Bkl<BTreeMap<(u32, u32), Weak<RefCell<Netlink>>>> = Bkl::new(BTreeMap::new());
/// ほかのプロセスのポートとぶつかったときの次の候補 (Linux の rover)
static ROVER: Bkl<i32> = Bkl::new(-4097);

type R = Result<i64, i64>;

/// socket(AF_NETLINK, typ, protocol) (Linux の netlink_create: 型、protocol の範囲、あるか)
pub fn create(typ: u64, protocol: u64) -> Result<NetlinkRef, i64> {
    let typ = typ as u32;
    if typ != SOCK_RAW && typ != SOCK_DGRAM {
        return Err(-ESOCKTNOSUPPORT);
    }
    let protocol = protocol as u32;
    if protocol >= MAX_LINKS || (protocol != NETLINK_ROUTE && protocol != crate::nftables::NETLINK_NETFILTER) {
        return Err(-EPROTONOSUPPORT);
    }
    let n = Rc::new(RefCell::new(Netlink::default()));
    {
        let mut s = n.borrow_mut();
        (s.protocol, s.typ, s.rcvbuf, s.sndbuf) = (protocol, typ, BUF_DEFAULT, BUF_DEFAULT);
        s.ino = Rc::as_ptr(&n) as usize & 0xffffff;
    }
    Ok(n)
}

pub fn file_of(n: NetlinkRef, flags: u64) -> FileRef {
    file::new(Kind::Netlink(n), file::O_RDWR | if flags & 0o4000 != 0 { file::O_NONBLOCK } else { 0 })
}

/// fd が netlink のソケットなら
pub fn of_fd(fd: u64) -> Option<(FileRef, NetlinkRef)> {
    let f = proc::current().files().get(fd)?.clone();
    let n = match &f.borrow().kind {
        Kind::Netlink(n) => n.clone(),
        _ => return None,
    };
    Some((f, n))
}

/// 閉じたら、ポートを返す
impl Drop for Netlink {
    fn drop(&mut self) {
        if self.bound {
            PORTS.get().remove(&(self.protocol, self.portid));
            // 持ち主の表を消す (NETLINK_URELEASE)
            if self.protocol == crate::nftables::NETLINK_NETFILTER {
                crate::nftables::release(self.portid);
            }
        }
    }
}

fn ports() -> &'static mut BTreeMap<(u32, u32), Weak<RefCell<Netlink>>> {
    PORTS.get()
}

fn taken(protocol: u32, portid: u32) -> bool {
    ports().get(&(protocol, portid)).is_some_and(|w| w.strong_count() > 0)
}

fn insert(n: &NetlinkRef, portid: u32) {
    let protocol = n.borrow().protocol;
    ports().insert((protocol, portid), Rc::downgrade(n));
    let mut s = n.borrow_mut();
    s.portid = portid;
    s.bound = true;
}

/// 自分でポートを決める (netlink_autobind: はじめはプロセスの tgid、使われていれば負の数を下へ)
fn autobind(n: &NetlinkRef) {
    if n.borrow().bound {
        return;
    }
    let protocol = n.borrow().protocol;
    let mut portid = proc::current().tgid;
    while taken(protocol, portid) {
        let r = ROVER.get();
        portid = *r as u32;
        *r = if *r == i32::MIN { -4097 } else { *r - 1 };
    }
    insert(n, portid);
}

/// sockaddr_nl (family、pad、nl_pid、nl_groups) を読む。短いか family がちがえば EINVAL
fn sockaddr(a: &[u8]) -> Result<(u32, u32), i64> {
    if a.len() < 12 || u16::from_le_bytes([a[0], a[1]]) != AF_NETLINK {
        return Err(crate::why::inval());
    }
    Ok((u32::from_le_bytes(a[4..8].try_into().unwrap()), u32::from_le_bytes(a[8..12].try_into().unwrap())))
}

fn sockaddr_of(pid: u32, groups: u32) -> [u8; 12] {
    let mut a = [0u8; 12];
    a[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
    a[4..8].copy_from_slice(&pid.to_le_bytes());
    a[8..12].copy_from_slice(&groups.to_le_bytes());
    a
}

/// bind (netlink_bind: 名前を確かめ、もうポートがあれば同じものだけ、0 なら自分で決める。multicast の組に入るのは、
/// NETLINK_ROUTE ならだれでも (NL_CFG_F_NONROOT_RECV)。組は 32 までなのでみな)
pub fn bind(n: &NetlinkRef, a: &[u8]) -> R {
    let (pid, groups) = sockaddr(a)?;
    let (bound, cur, protocol) = {
        let s = n.borrow();
        (s.bound, s.portid, s.protocol)
    };
    if bound {
        if pid != cur {
            return Err(crate::why::inval());
        }
    } else if pid == 0 {
        autobind(n);
    } else {
        if taken(protocol, pid) {
            return Err(-EADDRINUSE);
        }
        insert(n, pid);
    }
    let mut s = n.borrow_mut();
    s.groups = (s.groups & !0xffff_ffff) | groups as u64;
    Ok(0)
}

/// connect (netlink_connect: AF_UNSPEC は相手を外す。ほかは相手のポートと組を覚え、まだなら自分のポートを決める)
pub fn connect(n: &NetlinkRef, a: &[u8]) -> R {
    if a.len() >= 2 && a[0] == 0 && a[1] == 0 {
        let mut s = n.borrow_mut();
        s.dst_pid = 0;
        s.dst_groups = 0;
        return Ok(0);
    }
    let (pid, groups) = sockaddr(a)?;
    if groups != 0 && !proc::current().cred.capable(crate::cred::CAP_NET_ADMIN) {
        return Err(-EPERM);
    }
    autobind(n);
    let mut s = n.borrow_mut();
    s.dst_pid = pid;
    s.dst_groups = groups;
    Ok(0)
}

/// getsockname (まだポートがなければ 0) と getpeername (connect した相手)
pub fn name(n: &NetlinkRef, peer: bool) -> [u8; 12] {
    let s = n.borrow();
    if peer { sockaddr_of(s.dst_pid, s.dst_groups) } else { sockaddr_of(s.portid, s.groups as u32) }
}

/// 送る (netlink_sendmsg)。to は名前 (なければ connect した相手)。カーネル (ポート 0) へは、みな答えてから返る
pub fn send(n: &NetlinkRef, data: &[u8], to: Option<&[u8]>, flags: u64) -> R {
    const MSG_OOB: u64 = 1;
    if flags & MSG_OOB != 0 {
        return Err(-EOPNOTSUPP);
    }
    let (dst, groups) = match to {
        Some(a) => {
            let (pid, g) = sockaddr(a)?;
            // NETLINK_ROUTE はほかのソケットへ送るのも組へ送るのも CAP_NET_ADMIN (NL_CFG_F_NONROOT_SEND がない)
            if (pid != 0 || g != 0) && !proc::current().cred.capable(crate::cred::CAP_NET_ADMIN) {
                return Err(-EPERM);
            }
            (pid, g)
        }
        None => {
            let s = n.borrow();
            (s.dst_pid, s.dst_groups)
        }
    };
    autobind(n);
    if data.len() > n.borrow().sndbuf.saturating_sub(32) {
        return Err(-EMSGSIZE);
    }
    // 組へ (いまは組を待つソケットへ届けることはない) と、ほかのソケットへ
    if groups != 0 && dst == 0 {
        return Ok(data.len() as i64);
    }
    if dst != 0 {
        let protocol = n.borrow().protocol;
        let Some(peer) = ports().get(&(protocol, dst)).and_then(|w| w.upgrade()) else { return Err(-ECONNREFUSED) };
        let me = n.borrow().portid;
        deliver(&peer, me, data.to_vec());
        return Ok(data.len() as i64);
    }
    let me = n.borrow().portid;
    for d in rcv(n, data, me) {
        deliver(n, 0, d);
    }
    Ok(data.len() as i64)
}

/// 受ける行列に入れて起こす (入れ物があふれたら捨てる: Linux は ENOBUFS を次に返す)
fn deliver(n: &NetlinkRef, from: u32, d: Vec<u8>) {
    {
        let mut s = n.borrow_mut();
        let used: usize = s.rq.iter().map(|x| x.1.len()).sum();
        if used > 0 && used + d.len() > s.rcvbuf {
            return;
        }
        s.rq.push_back((from, d));
        s.generation += 1;
    }
    proc::wakeup(chan(n));
}

/// 受けたもの: 入れ物に入れるバイト、返す長さ、msg_flags、送ったものの名前 (sockaddr_nl)
pub struct Recv {
    pub data: Vec<u8>,
    pub len: usize,
    pub flags: u32,
    pub from: [u8; 12],
}

/// 受ける (netlink_recvmsg: データグラムを 1 つ。入れ物が短ければ切って MSG_TRUNC、flags に MSG_TRUNC なら本当の長さを返す)
pub fn recv(n: &NetlinkRef, room: usize, flags: u64, nonblock: bool) -> Result<Recv, i64> {
    loop {
        {
            let mut s = n.borrow_mut();
            if let Some((from, d)) = s.rq.front() {
                let len = d.len();
                let k = len.min(room);
                let data = d[..k].to_vec();
                let from = sockaddr_of(*from, 0);
                if flags & MSG_PEEK == 0 {
                    s.rq.pop_front();
                    s.generation += 1;
                }
                let tflag = if k < len { MSG_TRUNC as u32 } else { 0 };
                return Ok(Recv { data, len: if flags & MSG_TRUNC != 0 { len } else { k }, flags: tflag, from });
            }
        }
        if nonblock || flags & MSG_DONTWAIT != 0 {
            return Err(-EAGAIN);
        }
        proc::sleep(chan(n))?;
    }
}

/// read (名前も付帯データもない recvmsg)
pub fn read(n: &NetlinkRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    let r = recv(n, dst.len(), 0, nonblock)?;
    dst[..r.data.len()].copy_from_slice(&r.data);
    Ok(r.len)
}

/// write (名前のない sendmsg)
pub fn write(n: &NetlinkRef, src: &[u8]) -> Result<usize, i64> {
    send(n, src, None, 0).map(|k| k as usize)
}

pub fn generation(n: &NetlinkRef) -> u64 {
    n.borrow().generation
}

pub fn chan(n: &NetlinkRef) -> usize {
    Rc::as_ptr(n) as usize
}

/// 読める・書ける・こわれた
pub fn readiness(n: &NetlinkRef) -> (bool, bool, bool) {
    (!n.borrow().rq.is_empty(), true, false)
}

/// 次に読める長さ (FIONREAD)
pub fn pending(n: &NetlinkRef) -> u64 {
    n.borrow().rq.front().map_or(0, |d| d.1.len() as u64)
}

pub fn describe(n: &NetlinkRef) -> String {
    alloc::format!("socket:[{}]", n.borrow().ino)
}

pub fn ino(n: &NetlinkRef) -> u64 {
    n.borrow().ino as u64
}

/// setsockopt (SOL_SOCKET の入れ物の大きさなど、SOL_NETLINK のオプション)
pub fn setsockopt(n: &NetlinkRef, level: u64, opt: u64, val: &[u8]) -> R {
    let int = || -> Result<u32, i64> {
        if val.len() < 4 {
            return Err(crate::why::inval());
        }
        Ok(u32::from_le_bytes(val[..4].try_into().unwrap()))
    };
    match level {
        SOL_SOCKET => {
            let v = int()? as i32;
            let mut s = n.borrow_mut();
            // Linux の sock_setsockopt: 2 倍にして、最小 (SOCK_MIN_RCVBUF 2304、SOCK_MIN_SNDBUF 4608) より小さくしない
            match opt {
                SO_RCVBUF | SO_RCVBUFFORCE => s.rcvbuf = ((v.max(0) as usize).min(BUF_DEFAULT) * 2).max(2304),
                SO_SNDBUF | SO_SNDBUFFORCE => s.sndbuf = ((v.max(0) as usize).min(BUF_DEFAULT) * 2).max(4608),
                SO_PASSCRED => s.passcred = v != 0,
                _ => return Err(-ENOPROTOOPT),
            }
            Ok(0)
        }
        SOL_NETLINK => {
            let v = int()?;
            match opt {
                NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
                    if v == 0 || v > RTNLGRP_MAX {
                        return Err(crate::why::inval());
                    }
                    autobind(n);
                    let mut s = n.borrow_mut();
                    if opt == NETLINK_ADD_MEMBERSHIP {
                        s.groups |= 1 << (v - 1);
                    } else {
                        s.groups &= !(1 << (v - 1));
                    }
                }
                NETLINK_PKTINFO | NETLINK_BROADCAST_ERROR | NETLINK_NO_ENOBUFS | NETLINK_LISTEN_ALL_NSID | NETLINK_CAP_ACK | NETLINK_EXT_ACK | NETLINK_GET_STRICT_CHK => {
                    let mut s = n.borrow_mut();
                    if v != 0 {
                        s.opts |= 1 << opt;
                    } else {
                        s.opts &= !(1 << opt);
                    }
                }
                _ => return Err(-ENOPROTOOPT),
            }
            Ok(0)
        }
        _ => Err(-ENOPROTOOPT),
    }
}

/// getsockopt。答えのバイト
pub fn getsockopt(n: &NetlinkRef, level: u64, opt: u64) -> Result<Vec<u8>, i64> {
    let s = n.borrow();
    let int = |v: u32| v.to_le_bytes().to_vec();
    match (level, opt) {
        (SOL_SOCKET, SO_TYPE) => Ok(int(s.typ)),
        (SOL_SOCKET, SO_PROTOCOL) => Ok(int(s.protocol)),
        (SOL_SOCKET, SO_DOMAIN) => Ok(int(AF_NETLINK as u32)),
        (SOL_SOCKET, SO_ERROR) => Ok(int(0)),
        (SOL_SOCKET, SO_RCVBUF) => Ok(int(s.rcvbuf as u32)),
        (SOL_SOCKET, SO_SNDBUF) => Ok(int(s.sndbuf as u32)),
        (SOL_SOCKET, SO_PASSCRED) => Ok(int(s.passcred as u32)),
        (SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS) => Ok((s.groups as u32).to_le_bytes().to_vec()),
        (SOL_NETLINK, NETLINK_PKTINFO | NETLINK_BROADCAST_ERROR | NETLINK_NO_ENOBUFS | NETLINK_LISTEN_ALL_NSID | NETLINK_CAP_ACK | NETLINK_EXT_ACK | NETLINK_GET_STRICT_CHK) => Ok(int((s.opts >> opt) & 1)),
        (SOL_SOCKET | SOL_NETLINK, _) => Err(-ENOPROTOOPT),
        _ => Err(-EOPNOTSUPP),
    }
}

// ---- メッセージを作る ----

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// nlmsghdr と中身 (4 バイトにそろえる)
fn put_msg(out: &mut Vec<u8>, typ: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) {
    let len = NLMSG_HDRLEN + payload.len();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + align4(len) - len, 0);
}

/// rtattr (長さ、type、中身。4 バイトにそろえる)
fn attr(out: &mut Vec<u8>, typ: u16, data: &[u8]) {
    let len = 4 + data.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(out.len() + align4(len) - len, 0);
}

fn attr_u32(out: &mut Vec<u8>, typ: u16, v: u32) {
    attr(out, typ, &v.to_le_bytes());
}

fn attr_str(out: &mut Vec<u8>, typ: u16, s: &str) {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    attr(out, typ, &b);
}

/// NLMSG_ERROR (netlink_ack: エラーとともとの頭。しくじりなら、NETLINK_CAP_ACK でなければもとのメッセージまるごと)
fn put_ack(out: &mut Vec<u8>, req: &[u8], err: i64, cap_ack: bool, pid: u32) {
    out.extend_from_slice(&ack(req, err, cap_ack, false, None, None, pid));
}

/// NLMSGERR_ATTR_MSG と NLMSGERR_ATTR_OFFS (NETLINK_EXT_ACK)
const NLMSGERR_ATTR_MSG: u16 = 1;
const NLMSGERR_ATTR_OFFS: u16 = 2;
const NLM_F_ACK_TLVS: u16 = 0x200;

/// netlink_ack: NLMSG_ERROR のデータグラム。もとの頭 (しくじりで NETLINK_CAP_ACK でなければ、もとのメッセージまるごと:
/// でなければ NLM_F_CAPPED)。NETLINK_EXT_ACK なら、しくじりのわけ (msg) とどの属性か (off: メッセージの頭から) を
/// うしろに足して NLM_F_ACK_TLVS
pub fn ack(req: &[u8], err: i64, cap_ack: bool, ext_ack: bool, off: Option<usize>, msg: Option<&str>, pid: u32) -> Vec<u8> {
    let seq = u32::from_le_bytes(req[8..12].try_into().unwrap());
    let reqlen = (u32::from_le_bytes(req[0..4].try_into().unwrap()) as usize).min(req.len());
    let whole = err != 0 && !cap_ack;
    let mut flags = if whole { 0 } else { NLM_F_CAPPED };
    let mut p = Vec::new();
    p.extend_from_slice(&(err as i32).to_le_bytes());
    p.extend_from_slice(&req[..if whole { reqlen } else { NLMSG_HDRLEN }]);
    if ext_ack {
        let mut tlv = Vec::new();
        if let Some(m) = msg {
            attr_str(&mut tlv, NLMSGERR_ATTR_MSG, m);
        }
        if let Some(o) = off.filter(|&o| err != 0 && o < reqlen) {
            attr_u32(&mut tlv, NLMSGERR_ATTR_OFFS, o as u32);
        }
        if !tlv.is_empty() {
            flags |= NLM_F_ACK_TLVS;
            p.resize(align4(p.len()), 0);
            p.extend_from_slice(&tlv);
        }
    }
    let mut out = Vec::new();
    put_msg(&mut out, NLMSG_ERROR, flags, seq, pid, &p);
    out
}

// ---- カーネルの受け口 (netlink_rcv_skb と rtnetlink_rcv_msg) ----

/// data のメッセージをみな受けて、答えのデータグラムを返す (答えも ACK も 1 つずつ別のデータグラム: Linux の
/// netlink_unicast と同じ)。me は送ったソケットのポート
fn rcv(n: &NetlinkRef, data: &[u8], me: u32) -> Vec<Vec<u8>> {
    let cap_ack = n.borrow().opts & (1 << NETLINK_CAP_ACK) != 0;
    let strict = n.borrow().opts & (1 << NETLINK_GET_STRICT_CHK) != 0;
    // nftables (nfnetlink の nf_tables)
    if n.borrow().protocol == crate::nftables::NETLINK_NETFILTER {
        let ext_ack = n.borrow().opts & (1 << NETLINK_EXT_ACK) != 0;
        return crate::nftables::rcv(data, me, cap_ack, ext_ack);
    }
    let mut out = Vec::new();
    let mut at = 0;
    while data.len() - at >= NLMSG_HDRLEN {
        let h = &data[at..];
        let len = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        if len < NLMSG_HDRLEN || h.len() < len {
            break;
        }
        let msg = &h[..len];
        let typ = u16::from_le_bytes([msg[4], msg[5]]);
        let flags = u16::from_le_bytes([msg[6], msg[7]]);
        let (mut err, mut dumped) = (0, false);
        if flags & NLM_F_REQUEST != 0 && typ >= NLMSG_MIN_TYPE {
            (err, dumped) = route(msg, typ, flags, me, strict, &mut out);
        }
        // dump は NLMSG_DONE で終わるので ACK はない (netlink_dump_start が -EINTR を返して skip)
        if !dumped && (flags & NLM_F_ACK != 0 || err != 0) {
            let mut a = Vec::new();
            put_ack(&mut a, msg, err, cap_ack, me);
            out.push(a);
        }
        at += align4(len).min(data.len() - at);
    }
    out
}

/// rtnetlink_rcv_msg: type の範囲、短いものは答えない、GET のほかは CAP_NET_ADMIN、dump か 1 つか。(負の errno か 0、
/// dump したか)。strict は NETLINK_GET_STRICT_CHK
fn route(msg: &[u8], typ: u16, flags: u16, me: u32, strict: bool, out: &mut Vec<Vec<u8>>) -> (i64, bool) {
    let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
    let r = route_msg(msg, typ, dump, me, strict, out);
    (r, r == 0 && dump && matches!(typ, RTM_GETLINK | RTM_GETADDR | RTM_GETROUTE))
}

/// out に答えのデータグラムを足す。dump は、中身があれば中身と NLMSG_DONE の 2 つ (Linux の netlink_dump は、dump が
/// 中身を返すとまた呼び、なにもなくなったところで DONE を次のデータグラムに)、なければ DONE だけ。
/// 家族: その家族の口がなければ PF_UNSPEC の口 (rtnl_get_link の fallback)。aios は IPv6 を入れずに作った Linux と同じで、
/// RTM_GETLINK は PF_BRIDGE (rtnl_bridge_getlink: ブリッジがないので空) のほかはみな口の一覧、RTM_GETADDR と
/// RTM_GETROUTE の dump は AF_INET のほかは rtnl_dump_all (PF_PACKET をとばして AF_INET のもの)。だから AF_PACKET
/// (Firefox の NetlinkService) でも AF_INET のアドレスと道が来る。1 つの RTM_GETROUTE は AF_INET だけ (ほかは
/// PF_UNSPEC に doit がなく EOPNOTSUPP)
fn route_msg(msg: &[u8], typ: u16, dump: bool, me: u32, strict: bool, out: &mut Vec<Vec<u8>>) -> i64 {
    if typ > RTM_MAX {
        return -EOPNOTSUPP;
    }
    let body = &msg[NLMSG_HDRLEN..];
    // struct rtgenmsg (1 バイト) より短いものは、だまって捨てる
    if body.is_empty() {
        return 0;
    }
    let kind = (typ - RTM_BASE) & 3;
    if kind != 2 && !proc::current().cred.capable(crate::cred::CAP_NET_ADMIN) {
        return -EPERM;
    }
    let seq = u32::from_le_bytes(msg[8..12].try_into().unwrap());
    let family = body[0];
    match (typ, dump) {
        (RTM_GETLINK, true) => {
            let mut d = Vec::new();
            if family != AF_BRIDGE {
                for l in links() {
                    put_link(&mut d, &l, NLM_F_MULTI, seq, me);
                }
            }
            done(out, d, seq, me);
            0
        }
        (RTM_GETLINK, false) => {
            // struct ifinfomsg (16 バイト) の ifi_index か、IFLA_IFNAME
            if body.len() < 16 {
                return crate::why::inval();
            }
            let index = i32::from_le_bytes(body[4..8].try_into().unwrap());
            let name = find_attr(&body[16..], IFLA_IFNAME).map(|v| String::from_utf8_lossy(v.split(|&c| c == 0).next().unwrap_or(&[])).into_owned());
            if index <= 0 && name.is_none() {
                return crate::why::inval();
            }
            let Some(l) = links().into_iter().find(|l| if index > 0 { l.index == index } else { Some(l.name) == name.as_deref() }) else { return -ENODEV };
            let mut d = Vec::new();
            put_link(&mut d, &l, 0, seq, me);
            out.push(d);
            0
        }
        (RTM_GETADDR, true) => {
            let mut d = Vec::new();
            for a in addrs() {
                put_addr(&mut d, &a, NLM_F_MULTI, seq, me);
            }
            done(out, d, seq, me);
            0
        }
        (RTM_GETROUTE, true) => {
            dump_routes(body, strict, seq, me, out);
            0
        }
        (RTM_GETROUTE, false) if family == AF_INET => get_route(body, strict, seq, me, out),
        _ => -EOPNOTSUPP,
    }
}

/// dump の中身 d (空でなければ 1 つのデータグラム) と、NLMSG_DONE のデータグラム
fn done(out: &mut Vec<Vec<u8>>, d: Vec<u8>, seq: u32, me: u32) {
    done_with(out, d, seq, me, 0, 0);
}

/// done に、DONE のフラグ (cb->answer_flags: NLM_F_DUMP_FILTERED) と dump のしくじり (dump_done_errno) を
fn done_with(out: &mut Vec<Vec<u8>>, d: Vec<u8>, seq: u32, me: u32, flags: u16, err: i64) {
    if !d.is_empty() {
        out.push(d);
    }
    let mut e = Vec::new();
    put_msg(&mut e, NLMSG_DONE, NLM_F_MULTI | flags, seq, me, &(err as i32).to_le_bytes());
    out.push(e);
}

/// rtattr の並びから type のものの中身
fn find_attr(mut a: &[u8], typ: u16) -> Option<&[u8]> {
    while a.len() >= 4 {
        let len = u16::from_le_bytes([a[0], a[1]]) as usize;
        if len < 4 || len > a.len() {
            return None;
        }
        if u16::from_le_bytes([a[2], a[3]]) == typ {
            return Some(&a[4..len]);
        }
        a = &a[align4(len).min(a.len())..];
    }
    None
}

// ---- 口とアドレス (netif.rs の ioctl と sysfs.rs と同じもの) ----

pub struct Link {
    pub index: i32,
    pub name: &'static str,
    /// ARPHRD_LOOPBACK か ARPHRD_ETHER
    pub typ: u16,
    /// dev_get_flags (IFF_RUNNING と IFF_LOWER_UP も)
    pub flags: u32,
    pub mtu: u32,
    pub addr: [u8; 6],
    pub brd: [u8; 6],
    /// (受けたバイト, 受けたフレーム, 出したバイト, 出したフレーム)
    pub stats: [u64; 4],
}

pub const LO_INDEX: i32 = 1;
pub const ETH_INDEX: i32 = 2;
/// IFF_UP | IFF_LOOPBACK | IFF_RUNNING と IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST (SIOCGIFFLAGS の 16 ビット)
pub const LO_FLAGS: u16 = 0x49;
pub const ETH_FLAGS: u16 = 0x1043;
/// IFF_LOWER_UP (netlink の ifi_flags だけ)
const IFF_LOWER_UP: u32 = 0x10000;

/// 口の並び: lo と、ネットワークの装置があれば eth0
pub fn links() -> Vec<Link> {
    let mut v = alloc::vec![Link { index: LO_INDEX, name: "lo", typ: 772, flags: LO_FLAGS as u32 | IFF_LOWER_UP, mtu: 65536, addr: [0; 6], brd: [0; 6], stats: crate::net::stats(true) }];
    if crate::net::get().is_some() {
        v.push(Link { index: ETH_INDEX, name: crate::net::IFNAME, typ: 1, flags: ETH_FLAGS as u32 | IFF_LOWER_UP, mtu: 1500, addr: crate::net::mac().unwrap_or_default(), brd: [0xff; 6], stats: crate::net::stats(false) });
    }
    v
}

/// RTM_NEWLINK (rtnl_fill_ifinfo の属性の順)
fn put_link(out: &mut Vec<u8>, l: &Link, flags: u16, seq: u32, me: u32) {
    let lo = l.index == LO_INDEX;
    let mut p = Vec::new();
    // struct ifinfomsg: family、pad、type、index、flags、change
    p.extend_from_slice(&[AF_UNSPEC, 0]);
    p.extend_from_slice(&l.typ.to_le_bytes());
    p.extend_from_slice(&l.index.to_le_bytes());
    p.extend_from_slice(&l.flags.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    attr_str(&mut p, IFLA_IFNAME, l.name);
    attr_u32(&mut p, IFLA_TXQLEN, 1000);
    // IF_OPER_UNKNOWN (lo) と IF_OPER_UP
    attr(&mut p, IFLA_OPERSTATE, &[if lo { 0 } else { 6 }]);
    attr(&mut p, IFLA_LINKMODE, &[0]);
    attr_u32(&mut p, IFLA_MTU, l.mtu);
    attr_u32(&mut p, IFLA_MIN_MTU, if lo { 0 } else { 68 });
    attr_u32(&mut p, IFLA_MAX_MTU, if lo { 0 } else { 65535 });
    attr_u32(&mut p, IFLA_GROUP, 0);
    attr_u32(&mut p, IFLA_PROMISCUITY, 0);
    attr_u32(&mut p, IFLA_NUM_TX_QUEUES, 1);
    attr_u32(&mut p, IFLA_NUM_RX_QUEUES, 1);
    attr(&mut p, IFLA_CARRIER, &[1]);
    attr_u32(&mut p, IFLA_CARRIER_CHANGES, if lo { 0 } else { 1 });
    attr(&mut p, IFLA_ADDRESS, &l.addr);
    attr(&mut p, IFLA_BROADCAST, &l.brd);
    // struct rtnl_link_stats64 (25 の u64) と rtnl_link_stats (24 の u32): rx_packets、tx_packets、rx_bytes、tx_bytes、...
    let [rxb, rxp, txb, txp] = l.stats;
    let mut s64 = [0u64; 25];
    s64[..4].copy_from_slice(&[rxp, txp, rxb, txb]);
    attr(&mut p, IFLA_STATS64, &s64.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
    attr(&mut p, IFLA_STATS, &s64[..24].iter().flat_map(|&v| (v as u32).to_le_bytes()).collect::<Vec<u8>>());
    put_msg(out, RTM_NEWLINK, flags, seq, me, &p);
}

pub struct Addr {
    pub index: i32,
    pub label: &'static str,
    pub addr: [u8; 4],
    pub prefix: u8,
    pub scope: u8,
    pub brd: Option<[u8; 4]>,
}

/// IPv4 のアドレス: lo の 127.0.0.1/8 と、eth0 にあれば
pub fn addrs() -> Vec<Addr> {
    let mut v = alloc::vec![Addr { index: LO_INDEX, label: "lo", addr: [127, 0, 0, 1], prefix: 8, scope: RT_SCOPE_HOST, brd: None }];
    if crate::net::get().is_some()
        && let Some(c) = crate::net::cidr()
    {
        let a = c.address().octets();
        let p = c.prefix_len();
        let m = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
        let b = (u32::from_be_bytes(a) | !m).to_be_bytes();
        v.push(Addr { index: ETH_INDEX, label: crate::net::IFNAME, addr: a, prefix: p, scope: RT_SCOPE_UNIVERSE, brd: Some(b) });
    }
    v
}

/// RTM_NEWADDR (inet_fill_ifaddr の属性の順)
fn put_addr(out: &mut Vec<u8>, a: &Addr, flags: u16, seq: u32, me: u32) {
    let mut p = Vec::new();
    // struct ifaddrmsg: family、prefixlen、flags、scope、index
    p.extend_from_slice(&[AF_INET, a.prefix, IFA_F_PERMANENT as u8, a.scope]);
    p.extend_from_slice(&(a.index as u32).to_le_bytes());
    attr(&mut p, IFA_ADDRESS, &a.addr);
    attr(&mut p, IFA_LOCAL, &a.addr);
    if let Some(b) = a.brd {
        attr(&mut p, IFA_BROADCAST, &b);
    }
    attr_str(&mut p, IFA_LABEL, a.label);
    attr_u32(&mut p, IFA_FLAGS, IFA_F_PERMANENT);
    // struct ifa_cacheinfo: いつまでも (INFINITY_LIFE_TIME) と、作った時刻 (1/100 秒)
    let mut ci = Vec::new();
    ci.extend_from_slice(&u32::MAX.to_le_bytes());
    ci.extend_from_slice(&u32::MAX.to_le_bytes());
    ci.extend_from_slice(&0u32.to_le_bytes());
    ci.extend_from_slice(&0u32.to_le_bytes());
    attr(&mut p, IFA_CACHEINFO, &ci);
    put_msg(out, RTM_NEWADDR, flags, seq, me, &p);
}

// ---- 道の表 (RTM_GETROUTE: Linux の fib_frontend.c、fib_semantics.c、fib_trie.c と route.c) ----
//
// 道は口のアドレスとゲートウェイから作る (Linux の fib_add_ifaddr と、ip=dhcp の ipconfig が足す既定の道と同じもの)。
// IP_MULTIPLE_TABLES のない Linux と同じく、local (255) の表は main (254) と 1 つの trie を分けあう: dump は main の
// ものを番地の順に、そのあと local のものを。さがす (fib_lookup) のは両方をまとめて、いちばん長い prefix

const RTA_DST: u16 = 1;
const RTA_SRC: u16 = 2;
const RTA_IIF: u16 = 3;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;
const RTA_CACHEINFO: u16 = 12;
const RTA_TABLE: u16 = 15;
const RTA_MARK: u16 = 16;
const RTA_UID: u16 = 25;
const RTA_IP_PROTO: u16 = 27;
const RTA_SPORT: u16 = 28;
const RTA_DPORT: u16 = 29;
/// RTA_MAX (7.4: RTA_FLOWLABEL)
const RTA_MAX: u16 = 31;
const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTN_MULTICAST: u8 = 5;
const RT_SCOPE_LINK: u8 = 253;
const RTPROT_KERNEL: u8 = 2;
const RTPROT_BOOT: u8 = 3;
const RT_TABLE_MAIN: u8 = 254;
const RT_TABLE_LOCAL: u8 = 255;
const RTM_F_NOTIFY: u32 = 0x100;
const RTM_F_CLONED: u32 = 0x200;
const RTM_F_PREFIX: u32 = 0x800;
const RTM_F_LOOKUP_TABLE: u32 = 0x1000;
const RTM_F_FIB_MATCH: u32 = 0x2000;
const RTCF_NOTIFY: u32 = 0x0001_0000;
const RTCF_MULTICAST: u32 = 0x2000_0000;
const RTCF_BROADCAST: u32 = 0x1000_0000;
const RTCF_LOCAL: u32 = 0x8000_0000;
const NLM_F_DUMP_FILTERED: u16 = 0x20;
const ENETUNREACH: i64 = 101;
const EHOSTUNREACH: i64 = 113;
const LOOPBACK: u32 = 0x7f00_0001;
const LBCAST: u32 = 0xffff_ffff;

/// 1 つの道 (fib_alias と fib_info)。番地はホストの順の u32 (0 はなし)
#[derive(Clone, PartialEq)]
pub struct Route {
    pub table: u8,
    pub typ: u8,
    pub scope: u8,
    pub proto: u8,
    pub dst: u32,
    pub len: u8,
    pub prefsrc: u32,
    pub gw: u32,
    pub oif: i32,
}

fn mask(len: u8) -> u32 {
    if len == 0 { 0 } else { u32::MAX << (32 - len as u32) }
}

fn is_multicast(a: u32) -> bool {
    a & 0xf000_0000 == 0xe000_0000
}

/// 道の一覧 (trie の葉の順: 番地、そのなかで長い prefix、同じなら表の番号の大きいもの)
pub fn routes() -> Vec<Route> {
    let mut v: Vec<Route> = Vec::new();
    let mut add = |r: Route| {
        // fib_table_insert: まったく同じもの (NLM_F_APPEND でも) は足さない
        if !v.contains(&r) {
            v.push(r);
        }
    };
    for a in addrs() {
        let lo = a.index == LO_INDEX;
        let addr = u32::from_be_bytes(a.addr);
        let m = mask(a.prefix);
        let prefix = addr & m;
        // fib_magic: proto kernel、prefsrc は口のアドレス、LOCAL は scope host でほかは link、UNICAST だけ main
        let magic = |typ: u8, dst: u32, len: u8| Route {
            table: if typ == RTN_UNICAST { RT_TABLE_MAIN } else { RT_TABLE_LOCAL },
            typ,
            scope: if typ == RTN_LOCAL { RT_SCOPE_HOST } else { RT_SCOPE_LINK },
            proto: RTPROT_KERNEL,
            dst,
            len,
            prefsrc: addr,
            gw: 0,
            oif: a.index,
        };
        add(magic(RTN_LOCAL, addr, 32));
        if let Some(b) = a.brd.map(u32::from_be_bytes).filter(|&b| b != 0 && b != LBCAST) {
            add(magic(RTN_BROADCAST, b, 32));
        }
        if prefix != 0 && (prefix != addr || a.prefix < 32) {
            add(magic(if lo { RTN_LOCAL } else { RTN_UNICAST }, prefix, a.prefix));
            if a.prefix < 31 {
                add(magic(RTN_BROADCAST, prefix | !m, 32));
            }
        }
    }
    // ipconfig (ip_rt_ioctl の SIOCADDRT): 既定の道は proto boot、scope universe、prefsrc なし
    if let Some(gw) = crate::net::get().and(crate::net::gateway()) {
        add(Route { table: RT_TABLE_MAIN, typ: RTN_UNICAST, scope: RT_SCOPE_UNIVERSE, proto: RTPROT_BOOT, dst: 0, len: 0, prefsrc: 0, gw: u32::from_be_bytes(gw.octets()), oif: ETH_INDEX });
    }
    v.sort_by_key(|r| (r.dst, 32 - r.len, 255 - r.table));
    v
}

/// fib_table_lookup: いちばん長い prefix (同じなら葉の順で先のもの)。oif があれば、その口のものだけ
fn lookup(rs: &[Route], dst: u32, oif: i32) -> Option<&Route> {
    rs.iter().filter(|r| dst & mask(r.len) == r.dst && (oif == 0 || r.oif == oif)).min_by_key(|r| 32 - r.len)
}

/// 口が持つアドレスか (__ip_dev_find: 口のアドレスか、local の表で RTN_LOCAL)
fn is_local(rs: &[Route], a: u32) -> bool {
    lookup(rs, a, 0).is_some_and(|r| r.typ == RTN_LOCAL)
}

/// inet_select_addr: dev の (dst と同じ網のものを先に) scope が scope 以下のアドレス。なければほかの口から
/// (in_dev_select_addr: scope link のものはとらない)
fn select_addr(dev: i32, dst: u32, scope: u8) -> u32 {
    let all = addrs();
    let ok = |a: &&Addr| a.scope <= scope;
    let mine: Vec<&Addr> = all.iter().filter(|a| a.index == dev).filter(ok).collect();
    if let Some(a) = mine.iter().find(|a| dst == 0 || dst & mask(a.prefix) == u32::from_be_bytes(a.addr) & mask(a.prefix)).or(mine.first()) {
        return u32::from_be_bytes(a.addr);
    }
    all.iter().find(|a| a.scope != RT_SCOPE_LINK && a.scope <= scope).map_or(0, |a| u32::from_be_bytes(a.addr))
}

/// 出ていく道 (rtable)
struct Out {
    typ: u8,
    /// RTCF_*
    flags: u32,
    oif: i32,
    saddr: u32,
    /// rt_uses_gateway のときのゲートウェイ
    gw: u32,
    /// fib_lookup で当たった道 (FIB_MATCH が出すもの)。ないこともある
    fi: Option<Route>,
}

/// ip_route_output_key_hash_rcu と __mkroute_output (ほかの VRF と multicast の group は見ない)
fn output(dst: u32, src: u32, oif: i32) -> Result<Out, i64> {
    let rs = routes();
    let devs = links();
    let (mut daddr, mut saddr) = (dst, src);
    let mut typ = 0u8;
    let mut flags = 0u32;
    let mut fi: Option<Route> = None;
    let dev;
    'make: {
        if saddr != 0 {
            if is_multicast(saddr) || saddr == LBCAST {
                return Err(crate::why::inval());
            }
            if oif == 0 && (is_multicast(daddr) || daddr == LBCAST) {
                // saddr の口から (vic や vat のための hack)
                let Some(r) = lookup(&rs, saddr, 0).filter(|r| r.typ == RTN_LOCAL) else { return Err(-ENETUNREACH) };
                dev = r.oif;
                break 'make;
            }
            if !is_local(&rs, saddr) {
                return Err(-ENETUNREACH);
            }
        }
        if oif != 0 {
            if !devs.iter().any(|l| l.index == oif) {
                return Err(-ENODEV);
            }
            if daddr & 0xffff_ff00 == 0xe000_0000 || daddr == LBCAST {
                if saddr == 0 {
                    saddr = select_addr(oif, 0, RT_SCOPE_LINK);
                }
                dev = oif;
                break 'make;
            }
            if saddr == 0 {
                if is_multicast(daddr) {
                    saddr = select_addr(oif, 0, RT_SCOPE_UNIVERSE);
                } else if daddr == 0 {
                    saddr = select_addr(oif, 0, RT_SCOPE_HOST);
                }
            }
        }
        if daddr == 0 {
            daddr = if saddr != 0 { saddr } else { LOOPBACK };
            saddr = daddr;
            typ = RTN_LOCAL;
            flags |= RTCF_LOCAL;
            dev = LO_INDEX;
            break 'make;
        }
        let Some(r) = lookup(&rs, daddr, oif) else {
            if oif != 0 {
                // 表がまちがっていても、oif があれば同じ網にいるとみなす
                if saddr == 0 {
                    saddr = select_addr(oif, 0, RT_SCOPE_LINK);
                }
                typ = RTN_UNICAST;
                dev = oif;
                break 'make;
            }
            return Err(-ENETUNREACH);
        };
        typ = r.typ;
        fi = Some(r.clone());
        if r.typ == RTN_LOCAL {
            if saddr == 0 {
                saddr = if r.prefsrc != 0 { r.prefsrc } else { daddr };
            }
            flags |= RTCF_LOCAL;
            dev = LO_INDEX;
            break 'make;
        }
        dev = r.oif;
        if saddr == 0 {
            saddr = if r.prefsrc != 0 { r.prefsrc } else { select_addr(r.oif, r.gw, r.scope) };
        }
    }
    // __mkroute_output
    if saddr >> 24 == 127 && dev != LO_INDEX {
        return Err(crate::why::inval());
    }
    let mut nh = fi.clone();
    if daddr == LBCAST {
        typ = RTN_BROADCAST;
        nh = None;
    } else if is_multicast(daddr) {
        typ = RTN_MULTICAST;
    } else if daddr >> 24 == 0 {
        return Err(crate::why::inval());
    }
    if dev == LO_INDEX {
        flags |= RTCF_LOCAL;
    }
    if typ == RTN_BROADCAST {
        flags |= RTCF_BROADCAST | RTCF_LOCAL;
    } else if typ == RTN_MULTICAST {
        // ip_check_mc_rcu: aios の口は multicast の group に入らないので、RTCF_LOCAL はつけない
        flags |= RTCF_MULTICAST;
        if nh.as_ref().is_some_and(|r| r.len < 4) {
            nh = None;
        }
    }
    let gw = nh.as_ref().map_or(0, |r| r.gw);
    Ok(Out { typ, flags, oif: dev, saddr, gw, fi })
}

/// rtattr を type ごとに (後のものが勝つ。長さのおかしいところで終わり)
fn attrs_of(mut a: &[u8]) -> BTreeMap<u16, &[u8]> {
    let mut m = BTreeMap::new();
    while a.len() >= 4 {
        let len = u16::from_le_bytes([a[0], a[1]]) as usize;
        if len < 4 || len > a.len() {
            break;
        }
        m.insert(u16::from_le_bytes([a[2], a[3]]) & 0x3fff, &a[4..len]);
        a = &a[align4(len).min(a.len())..];
    }
    m
}

fn u32_of(a: Option<&&[u8]>) -> u32 {
    a.filter(|v| v.len() >= 4).map_or(0, |v| u32::from_le_bytes(v[..4].try_into().unwrap()))
}

fn addr_of(a: Option<&&[u8]>) -> u32 {
    a.filter(|v| v.len() >= 4).map_or(0, |v| u32::from_be_bytes(v[..4].try_into().unwrap()))
}

/// inet_rtm_getroute (ip route get): rtmsg の頭と RTA_DST・RTA_SRC・RTA_OIF・RTA_MARK・RTA_UID。答えは rt_fill_info
/// (FIB_MATCH なら当たった道を fib_dump_info で)
fn get_route(body: &[u8], strict: bool, seq: u32, me: u32, out: &mut Vec<Vec<u8>>) -> i64 {
    // struct rtmsg: family、dst_len、src_len、tos、table、protocol、scope、type、flags (u32)
    if body.len() < 12 {
        return crate::why::inval();
    }
    let (dst_len, src_len, table, proto, scope, rtype) = (body[1], body[2], body[4], body[5], body[6], body[7]);
    let rflags = u32::from_le_bytes(body[8..12].try_into().unwrap());
    let a = attrs_of(&body[12..]);
    if strict {
        // inet_rtm_valid_getroute_req
        if (src_len != 0 && src_len != 32) || (dst_len != 0 && dst_len != 32) || table != 0 || proto != 0 || scope != 0 || rtype != 0 {
            return crate::why::inval();
        }
        if rflags & !(RTM_F_NOTIFY | RTM_F_LOOKUP_TABLE | RTM_F_FIB_MATCH) != 0 {
            return crate::why::inval();
        }
        if a.keys().any(|&k| k > RTA_MAX) || (a.contains_key(&RTA_SRC) && src_len == 0) || (a.contains_key(&RTA_DST) && dst_len == 0) {
            return crate::why::inval();
        }
        if a.keys().any(|k| !matches!(*k, RTA_IIF | RTA_OIF | RTA_SRC | RTA_DST | RTA_IP_PROTO | RTA_SPORT | RTA_DPORT | RTA_MARK | RTA_UID)) {
            return crate::why::inval();
        }
    }
    let src = addr_of(a.get(&RTA_SRC));
    let dst = addr_of(a.get(&RTA_DST));
    let mark = u32_of(a.get(&RTA_MARK));
    if a.contains_key(&RTA_IIF) && u32_of(a.get(&RTA_IIF)) != 0 {
        // 入ってくる道 (ip_route_input_rcu) はまだない
        return -EOPNOTSUPP;
    }
    let uid = if a.contains_key(&RTA_UID) { u32_of(a.get(&RTA_UID)) } else { proc::current().cred.uid };
    let rt = match output(dst, src, u32_of(a.get(&RTA_OIF)) as i32) {
        Ok(rt) => rt,
        Err(e) => return e,
    };
    // RTM_F_LOOKUP_TABLE: 当たった表 (local も main の trie なので 254。さがさなかったら 0)
    let table_id = if rflags & RTM_F_LOOKUP_TABLE != 0 && rt.fi.is_none() { 0 } else { RT_TABLE_MAIN };
    let mut d = Vec::new();
    if rflags & RTM_F_FIB_MATCH != 0 {
        let Some(mut f) = rt.fi else { return -EHOSTUNREACH };
        f.table = table_id;
        f.typ = rt.typ;
        put_route(&mut d, &f, 0, seq, me);
        out.push(d);
        return 0;
    }
    let mut flags = rt.flags;
    if rflags & RTM_F_NOTIFY != 0 {
        flags |= RTCF_NOTIFY;
    }
    // rtm_tos は当たった道の dscp (aios の道はみな 0)、protocol は RTPROT_UNSPEC
    let mut p = Vec::new();
    p.extend_from_slice(&[AF_INET, 32, if src != 0 { 32 } else { 0 }, 0, table_id, 0, RT_SCOPE_UNIVERSE, rt.typ]);
    let rtm_flags = (flags & !0xffff) | RTM_F_CLONED | if flags & RTCF_NOTIFY != 0 { RTM_F_NOTIFY } else { 0 };
    p.extend_from_slice(&rtm_flags.to_le_bytes());
    attr_u32(&mut p, RTA_TABLE, table_id as u32);
    attr(&mut p, RTA_DST, &dst.to_be_bytes());
    if src != 0 {
        attr(&mut p, RTA_SRC, &src.to_be_bytes());
    }
    attr_u32(&mut p, RTA_OIF, rt.oif as u32);
    if rt.saddr != src {
        attr(&mut p, RTA_PREFSRC, &rt.saddr.to_be_bytes());
    }
    if rt.gw != 0 {
        attr(&mut p, RTA_GATEWAY, &rt.gw.to_be_bytes());
    }
    if mark != 0 {
        attr_u32(&mut p, RTA_MARK, mark);
    }
    attr_u32(&mut p, RTA_UID, uid);
    // struct rta_cacheinfo: clntref、lastuse、expires、error、used、id、ts、tsage
    let mut ci = [0u32; 8];
    ci[0] = 1;
    attr(&mut p, RTA_CACHEINFO, &ci.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
    put_msg(&mut d, RTM_NEWROUTE, 0, seq, me, &p);
    out.push(d);
    0
}

/// inet_dump_fib: strict (ip_valid_fib_dump_req) なら頭と RTA_TABLE・RTA_OIF で選ぶ (NLM_F_DUMP_FILTERED)。
/// しくじりは DONE の errno に
fn dump_routes(body: &[u8], strict: bool, seq: u32, me: u32, out: &mut Vec<Vec<u8>>) {
    let (mut rflags, mut table, mut proto, mut rtype, mut dev) = (0u32, 0u32, 0u8, 0u8, 0i32);
    let (mut routes_too, mut filter_set) = (true, false);
    if strict {
        let e = (|| {
            if body.len() < 12 {
                return Err(crate::why::inval());
            }
            if body[1] != 0 || body[2] != 0 || body[3] != 0 || body[6] != 0 {
                return Err(crate::why::inval());
            }
            rflags = u32::from_le_bytes(body[8..12].try_into().unwrap());
            if rflags & !(RTM_F_CLONED | RTM_F_PREFIX) != 0 {
                return Err(crate::why::inval());
            }
            // RTM_F_CLONED なら例外 (fnhe) だけ: aios にはない
            routes_too = rflags & RTM_F_CLONED == 0;
            (proto, rtype, table) = (body[5], body[7], body[4] as u32);
            let a = attrs_of(&body[12..]);
            for (&k, v) in a.iter() {
                match k {
                    RTA_TABLE => table = u32_of(Some(v)),
                    RTA_OIF => {
                        dev = u32_of(Some(v)) as i32;
                        if !links().iter().any(|l| l.index == dev) {
                            return Err(-ENODEV);
                        }
                    }
                    _ => return Err(crate::why::inval()),
                }
            }
            filter_set = rflags != 0 || proto != 0 || rtype != 0 || table != 0 || dev != 0;
            Ok(())
        })();
        if let Err(e) = e {
            done_with(out, Vec::new(), seq, me, 0, e);
            return;
        }
    } else if body.len() >= 12 {
        rflags = u32::from_le_bytes(body[8..12].try_into().unwrap()) & (RTM_F_PREFIX | RTM_F_CLONED);
    }
    let answer = if filter_set { NLM_F_DUMP_FILTERED } else { 0 };
    // strict でないとき: 例外も道も出す。strict なら片方だけなので NLM_F_DUMP_FILTERED
    let flags = NLM_F_MULTI | if strict { NLM_F_DUMP_FILTERED } else { 0 };
    let mut d = Vec::new();
    if rflags & RTM_F_PREFIX == 0 && routes_too {
        // 表をえらぶと、その表だけ (fib_get_table: 255 のほかはみな main)
        let tables: &[u8] = match table {
            0 => &[RT_TABLE_MAIN, RT_TABLE_LOCAL],
            255 => &[RT_TABLE_LOCAL],
            _ => &[RT_TABLE_MAIN],
        };
        let rs = routes();
        for &t in tables {
            for r in rs.iter().filter(|r| r.table == t) {
                if (rtype != 0 && r.typ != rtype) || (proto != 0 && r.proto != proto) || (dev != 0 && r.oif != dev) {
                    continue;
                }
                put_route(&mut d, r, flags, seq, me);
            }
        }
    }
    done_with(out, d, seq, me, answer, 0);
}

/// RTM_NEWROUTE (fib_dump_info の属性の順)
fn put_route(out: &mut Vec<u8>, r: &Route, flags: u16, seq: u32, me: u32) {
    let mut p = Vec::new();
    p.extend_from_slice(&[AF_INET, r.len, 0, 0, r.table, r.proto, r.scope, r.typ]);
    p.extend_from_slice(&0u32.to_le_bytes());
    attr_u32(&mut p, RTA_TABLE, r.table as u32);
    if r.len != 0 {
        attr(&mut p, RTA_DST, &r.dst.to_be_bytes());
    }
    if r.prefsrc != 0 {
        attr(&mut p, RTA_PREFSRC, &r.prefsrc.to_be_bytes());
    }
    if r.gw != 0 {
        attr(&mut p, RTA_GATEWAY, &r.gw.to_be_bytes());
    }
    attr_u32(&mut p, RTA_OIF, r.oif as u32);
    put_msg(out, RTM_NEWROUTE, flags, seq, me, &p);
}
