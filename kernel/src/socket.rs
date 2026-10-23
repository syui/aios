// BSD ソケットのシステムコール。AF_UNIX は unix.rs、AF_INET の TCP と UDP は inet.rs、ICMP の echo (ping) と raw は
// ここで smoltcp のソケットを使う
use crate::cell::Bkl;
use crate::file::{self, FileRef, Kind};
use crate::inet::InetRef;
use crate::net;
use crate::proc;
use crate::unix::UnixRef;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{icmp, raw};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion, Ipv4Address};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EMFILE: i64 = 24;
const EPERM: i64 = 1;
const EACCES: i64 = 13;
const ENOTSOCK: i64 = 88;
const ESOCKTNOSUPPORT: i64 = 94;
const ECONNABORTED: i64 = 103;
const ENOBUFS: i64 = 105;
const EPROTONOSUPPORT: i64 = 93;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EDESTADDRREQ: i64 = 89;
const EADDRINUSE: i64 = 98;
const ENETDOWN: i64 = 100;
const ENOTCONN: i64 = 107;
const ENETUNREACH: i64 = 101;

const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_PEEK: u64 = 0x2;
const MSG_CTRUNC: u32 = 0x8;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;
const IPPROTO_ICMP: u64 = 1;
/// 1 回の読み書きの上限 (Linux の MAX_RW_COUNT)
const MAX_RW: usize = 0x7fff_f000;

#[derive(PartialEq)]
enum Proto {
    /// ICMP の echo (Linux の ping ソケット: SOCK_DGRAM + IPPROTO_ICMP。aios ではだれでも ping できる: Linux の
    /// net.ipv4.ping_group_range を 0 2147483647 にしたもの)
    Icmp,
    /// raw の ICMP (SOCK_RAW + IPPROTO_ICMP、root だけ)。受けるのは IP ヘッダーからの ICMP ぜんぶ (traceroute が
    /// 途中のルーターの「時間切れ」を受ける)。送るものには IP ヘッダーをつける
    Raw,
}

pub struct Socket {
    proto: Proto,
    handle: Option<SocketHandle>,
    /// bind した場所 (ICMP は id)
    local: Option<IpListenEndpoint>,
    /// connect 先
    peer: Option<IpEndpoint>,
    pub nonblock: bool,
    /// /proc/PID/fd の socket:[N] の N (add_fd で決まる)
    ino: usize,
    /// setsockopt IP_TTL (なければ 64)
    ttl: Option<u8>,
    /// 分けた NET (ns.rs) の中: どこへも送れない
    isolated: bool,
}

pub type SockRef = Rc<RefCell<Socket>>;

static NEXT_ID: Bkl<u16> = Bkl::new(49152);

/// ICMP の echo の id (空いているもの)
fn next_id() -> u16 {
    let p = *NEXT_ID.get();
    *NEXT_ID.get() = if p == 65535 { 49152 } else { p + 1 };
    p
}

fn n() -> Result<&'static mut net::Net, i64> {
    net::get().ok_or(-ENETDOWN)
}

fn raw_sock(h: SocketHandle) -> &'static mut raw::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<raw::Socket>(h)
}

fn icmp_sock(h: SocketHandle) -> &'static mut icmp::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<icmp::Socket>(h)
}

/// インターネットのチェックサム (RFC 1071)
fn inet_checksum(b: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for c in b.chunks(2) {
        sum += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// 眠って待つ。nonblock なら EAGAIN
fn wait(nonblock: bool) -> Result<(), i64> {
    if nonblock {
        return Err(-EAGAIN);
    }
    proc::sleep(net::chan())?;
    net::poll();
    Ok(())
}

impl Drop for Socket {
    fn drop(&mut self) {
        let Some(h) = self.handle.take() else { return };
        if let Some(n) = net::get() {
            n.sockets.remove(h);
        }
    }
}

impl Socket {
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, i64> {
        self.recv(buf, false, false).map(|(n, _)| n)
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, i64> {
        self.send(buf, None, false)
    }

    /// peek なら読んだものを取りださない (MSG_PEEK)
    fn recv(&mut self, buf: &mut [u8], dontwait: bool, peek: bool) -> Result<(usize, Option<IpEndpoint>), i64> {
        let nb = self.nonblock || dontwait;
        net::poll();
        match self.proto {
            // IP ヘッダーからぜんぶ。送り元は IP ヘッダーの中
            Proto::Raw => {
                let h = self.bind_raw()?;
                loop {
                    let s = raw_sock(h);
                    if s.can_recv() {
                        let k = if peek { s.peek_slice(buf) } else { s.recv_slice(buf) }.map_err(|_| crate::why::inval())?;
                        let from = (k >= 20).then(|| IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(buf[12], buf[13], buf[14], buf[15])), 0));
                        return Ok((k, from));
                    }
                    wait(nb)?;
                }
            }
            // ICMP の echo reply (IP ヘッダーなし、ICMP ヘッダーから)
            Proto::Icmp => {
                let h = self.bind_icmp()?;
                loop {
                    let s = icmp_sock(h);
                    if s.can_recv() {
                        let (k, from) = s.recv_slice(buf).map_err(|_| crate::why::inval())?;
                        return Ok((k, Some(IpEndpoint::new(from, 0))));
                    }
                    wait(nb)?;
                }
            }
        }
    }

    fn send(&mut self, buf: &[u8], to: Option<IpEndpoint>, _dontwait: bool) -> Result<usize, i64> {
        if self.isolated {
            return Err(-ENETUNREACH);
        }
        match self.proto {
            // ICMP のメッセージに IP ヘッダーをつけて送る (Linux の raw で IP_HDRINCL なしと同じ)
            Proto::Raw => {
                let to = to.or(self.peer).ok_or(-EDESTADDRREQ)?;
                let IpAddress::Ipv4(dst) = to.addr;
                let h = self.bind_raw()?;
                let total = 20 + buf.len();
                if total > 65535 {
                    return Err(crate::why::inval());
                }
                let mut pkt = alloc::vec![0u8; total];
                pkt[0] = 0x45;
                pkt[2..4].copy_from_slice(&(total as u16).to_be_bytes());
                pkt[8] = self.ttl.unwrap_or(64);
                pkt[9] = 1; // ICMP
                pkt[12..16].copy_from_slice(&net::addr().octets());
                pkt[16..20].copy_from_slice(&dst.octets());
                let sum = inet_checksum(&pkt[..20]);
                pkt[10..12].copy_from_slice(&sum.to_be_bytes());
                pkt[20..].copy_from_slice(buf);
                raw_sock(h).send_slice(&pkt).map_err(|_| -EAGAIN)?;
                net::poll();
                Ok(buf.len())
            }
            // ICMP の echo request: Linux と同じく id をこのソケットの番号にして、チェックサムを計算しなおす
            Proto::Icmp => {
                let to = to.or(self.peer).ok_or(-EDESTADDRREQ)?;
                if buf.len() < 8 || buf[0] != 8 {
                    return Err(crate::why::inval());
                }
                let h = self.bind_icmp()?;
                let id = self.local.map_or(0, |l| l.port);
                let mut pkt = buf.to_vec();
                pkt[4..6].copy_from_slice(&id.to_be_bytes());
                pkt[2..4].copy_from_slice(&[0, 0]);
                let sum = inet_checksum(&pkt);
                pkt[2..4].copy_from_slice(&sum.to_be_bytes());
                icmp_sock(h).send_slice(&pkt, to.addr).map_err(|_| -EAGAIN)?;
                net::poll();
                Ok(buf.len())
            }
        }
    }

    fn bind_raw(&mut self) -> Result<SocketHandle, i64> {
        if let Some(h) = self.handle {
            return Ok(h);
        }
        let meta = || vec![raw::PacketMetadata::EMPTY; 16];
        let sock = raw::Socket::new(IpVersion::Ipv4, IpProtocol::Icmp, raw::PacketBuffer::new(meta(), vec![0; 16 * 1024]), raw::PacketBuffer::new(meta(), vec![0; 16 * 1024]));
        let h = n()?.sockets.add(sock);
        self.handle = Some(h);
        Ok(h)
    }

    /// setsockopt IP_TTL: 送るものの TTL (bind していれば、すぐ smoltcp のソケットに)
    fn set_ttl(&mut self, ttl: Option<u8>) {
        self.ttl = ttl;
        if let (Some(h), Proto::Icmp) = (self.handle, &self.proto) {
            icmp_sock(h).set_hop_limit(ttl);
        }
    }

    /// ICMP も送る前に自動で bind する (id は空いている番号)
    fn bind_icmp(&mut self) -> Result<SocketHandle, i64> {
        if let Some(h) = self.handle {
            return Ok(h);
        }
        let meta = || vec![icmp::PacketMetadata::EMPTY; 16];
        let mut sock = icmp::Socket::new(icmp::PacketBuffer::new(meta(), vec![0; 16 * 1024]), icmp::PacketBuffer::new(meta(), vec![0; 16 * 1024]));
        let id = self.local.map(|l| l.port).filter(|&p| p != 0).unwrap_or_else(next_id);
        sock.bind(icmp::Endpoint::Ident(id)).map_err(|_| -EADDRINUSE)?;
        sock.set_hop_limit(self.ttl);
        let h = n()?.sockets.add(sock);
        self.local = Some(IpListenEndpoint { addr: None, port: id });
        self.handle = Some(h);
        Ok(h)
    }

    /// FIONREAD: 次のデータグラムの大きさ
    pub fn available(&self) -> usize {
        net::poll();
        let Some(h) = self.handle else { return 0 };
        match self.proto {
            Proto::Icmp => usize::from(icmp_sock(h).can_recv()),
            Proto::Raw => raw_sock(h).peek().map_or(0, |d| d.len()),
        }
    }

    /// (読める, 書ける, 閉じた)
    pub fn readiness(&self) -> (bool, bool, bool) {
        net::poll();
        let Some(h) = self.handle else { return (false, true, false) };
        match self.proto {
            Proto::Icmp => (icmp_sock(h).can_recv(), true, false),
            Proto::Raw => (raw_sock(h).can_recv(), true, false),
        }
    }
}

// ---- sockaddr_in ----

fn read_addr(va: usize, len: usize) -> Result<IpEndpoint, i64> {
    if len < 8 {
        return Err(crate::why::inval());
    }
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    if u16::from_le_bytes([b[0], b[1]]) as u64 != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([b[2], b[3]]);
    Ok(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(b[4], b[5], b[6], b[7])), port))
}

fn endpoint_bytes(ep: Option<IpEndpoint>) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    if let Some(ep) = ep {
        b[2..4].copy_from_slice(&ep.port.to_be_bytes());
        let IpAddress::Ipv4(a) = ep.addr;
        b[4..8].copy_from_slice(&a.octets());
    }
    b
}

// ---- 共通 ----

type R = Result<i64, i64>;

/// 0 で埋めない受け渡しの場所 (読んだ・写した分だけを使う)
fn uninit(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    unsafe { v.set_len(n) };
    v
}

/// ソケットのシステムコールの fd (Linux の fdget: ない fd と O_PATH のものは EBADF)
fn fd_file(fd: u64) -> Result<FileRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-file::EBADF)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-file::EBADF);
    }
    Ok(f)
}

/// どのソケットか
enum Sock {
    Unix(FileRef, UnixRef),
    Inet(FileRef, InetRef),
    Smol(SockRef),
}

/// fd のソケット (ない fd は EBADF、ソケットでなければ ENOTSOCK)
fn sock(fd: u64) -> Result<Sock, i64> {
    let f = fd_file(fd)?;
    let k = match &f.borrow().kind {
        Kind::Unix(u) => Ok(Ok(u.clone())),
        Kind::Inet(u) => Ok(Err(Ok(u.clone()))),
        Kind::Socket(s) => Ok(Err(Err(s.clone()))),
        _ => Err(-ENOTSOCK),
    }?;
    Ok(match k {
        Ok(u) => Sock::Unix(f, u),
        Err(Ok(u)) => Sock::Inet(f, u),
        Err(Err(s)) => Sock::Smol(s),
    })
}

/// ユーザーの名前 (Linux の move_addr_to_kernel: 長さは 0 から sockaddr_storage (128) まで)
fn addr_in(va: usize, len: usize) -> Result<Vec<u8>, i64> {
    let len = len as u32 as i32;
    if !(0..=128).contains(&len) {
        return Err(crate::why::inval());
    }
    let mut b = vec![0u8; len as usize];
    if len > 0 {
        proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    }
    Ok(b)
}

/// 名前を返す (Linux の move_addr_to_user): *lenp までを写し、*lenp には本当の長さ
fn addr_out(va: usize, lenp: usize, name: &[u8]) -> Result<(), i64> {
    let pt = proc::current().pt();
    let mut l = [0u8; 4];
    pt.copy_in(&mut l, lenp).ok_or(-EFAULT)?;
    let len = i32::from_le_bytes(l).min(name.len() as i32);
    if len < 0 {
        return Err(crate::why::inval());
    }
    if len > 0 {
        pt.copy_out(va, &name[..len as usize]).ok_or(-EFAULT)?;
    }
    pt.copy_out(lenp, &(name.len() as u32).to_le_bytes()).ok_or(-EFAULT)?;
    Ok(())
}

fn nonblock_of(f: &FileRef, flags: u64) -> bool {
    flags & MSG_DONTWAIT != 0 || f.borrow().flags & file::O_NONBLOCK != 0
}

/// 入れ物 (iovec) の先頭 need バイトに書けるか (ページごとに 1 バイトを読んで書きもどす)
fn writable(iov: &[(usize, usize)], need: usize) -> bool {
    let pt = proc::current().pt();
    let mut left = need;
    for &(base, len) in iov {
        if left == 0 {
            break;
        }
        let n = len.min(left);
        let mut at = base;
        while at < base + n {
            let mut b = [0u8; 1];
            if pt.copy_in(&mut b, at).is_none() || pt.copy_out(at, &b).is_none() {
                return false;
            }
            at = (at / crate::memlayout::PGSIZE + 1) * crate::memlayout::PGSIZE;
        }
        if pt.copy_in(&mut [0u8; 1], base + n - 1).is_none() {
            return false;
        }
        left -= n;
    }
    true
}

/// 受けたものを入れ物 (iovec) へ
fn copy_out_iov(iov: &[(usize, usize)], data: &[u8]) -> Result<(), i64> {
    let pt = proc::current().pt();
    let mut done = 0;
    for &(base, len) in iov {
        if done == data.len() {
            break;
        }
        let n = len.min(data.len() - done);
        pt.copy_out(base, &data[done..done + n]).ok_or(-EFAULT)?;
        done += n;
    }
    Ok(())
}

/// AF_UNIX から入れ物 (iovec) へ受ける。stream は、読めるものがあるのに入れ物にさわれなければ、読まずに EFAULT
/// (Linux の unix_stream_read_actor は写せなければその skb を残す)。dgram は受けてから写す (写せなければ、その
/// メッセージはなくなって EFAULT: Linux の __unix_dgram_recvmsg)
fn unix_recv(u: &UnixRef, iov: &[(usize, usize)], flags: u64, nonblock: bool) -> Result<crate::unix::Recv, i64> {
    let total = iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    let stream = u.borrow().typ == crate::unix::SOCK_STREAM;
    if stream && flags & crate::unix::MSG_OOB == 0 && total > 0 {
        let need = total.min(crate::unix::pending(u) as usize);
        if need > 0 && !writable(iov, need) {
            return Err(-EFAULT);
        }
    }
    let r = crate::unix::recv(u, total, flags, nonblock)?;
    copy_out_iov(iov, &r.data)?;
    Ok(r)
}

/// AF_INET から入れ物へ受ける。TCP は、入れ物にさわれなければ読まずに EFAULT (Linux の tcp_recvmsg は写せなかった
/// ところから先を残す)。UDP は受けてから写す (写せなければ、そのデータグラムはなくなって EFAULT)
fn inet_recv(u: &InetRef, iov: &[(usize, usize)], flags: u64, nonblock: bool) -> Result<crate::inet::Recv, i64> {
    let total = iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    if u.borrow().tcp && flags & (crate::inet::MSG_OOB | crate::inet::MSG_TRUNC) == 0 && total > 0 {
        let need = total.min(crate::inet::pending(u) as usize);
        if need > 0 && !writable(iov, need) {
            return Err(-EFAULT);
        }
    }
    let r = crate::inet::recv(u, total, flags, nonblock)?;
    copy_out_iov(iov, &r.data)?;
    Ok(r)
}

/// read / readv / preadv2 (位置 -1) で AF_UNIX のソケットから (Linux の sock_read_iter: 長さ 0 なら何もせずに 0。
/// 届いた fd は閉じる)
pub fn unix_readv(f: &FileRef, u: &UnixRef, iov: &[(usize, usize)], nowait: bool) -> R {
    if iov.iter().all(|x| x.1 == 0) {
        return Ok(0);
    }
    let r = unix_recv(u, iov, 0, nowait || nonblock_of(f, 0))?;
    Ok(r.ret as i64)
}

/// write / writev / pwritev2 (位置 -1) で AF_UNIX のソケットへ (Linux の sock_write_iter: 長さ 0 でも送る。
/// SOCK_STREAM の EPIPE は SIGPIPE、RWF_NOSIGNAL なら送らない)
pub fn unix_writev(f: &FileRef, u: &UnixRef, iov: &[(usize, usize)], nowait: bool, nosignal: bool) -> R {
    let total = iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    let flags = if nosignal { crate::unix::MSG_NOSIGNAL } else { 0 };
    crate::unix::send(u, crate::unix::Src::User(iov), total, None, &[], flags, nowait || nonblock_of(f, 0))
}

/// read / readv / preadv2 で AF_INET のソケットから (長さ 0 なら何もせずに 0)
pub fn inet_readv(f: &FileRef, u: &InetRef, iov: &[(usize, usize)], nowait: bool) -> R {
    if iov.iter().all(|x| x.1 == 0) {
        return Ok(0);
    }
    let r = inet_recv(u, iov, 0, nowait || nonblock_of(f, 0))?;
    Ok(r.ret as i64)
}

/// write / writev / pwritev2 で AF_INET のソケットへ (TCP の EPIPE は SIGPIPE、RWF_NOSIGNAL なら送らない)
pub fn inet_writev(f: &FileRef, u: &InetRef, iov: &[(usize, usize)], nowait: bool, nosignal: bool) -> R {
    let total = iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    let flags = if nosignal { crate::inet::MSG_NOSIGNAL } else { 0 };
    crate::inet::send(u, crate::unix::Src::User(iov), total, None, &[], flags, nowait || nonblock_of(f, 0))
}

/// AF_INET の型と protocol (Linux の inet_create: 型ごとの表にあるか (なければ ESOCKTNOSUPPORT、protocol がちがえば
/// EPROTONOSUPPORT)、raw は CAP_NET_RAW、ping は net.ipv4.ping_group_range (既定の 1 0 はだれも入らない))
fn inet_check(t: u64, protocol: u64) -> Result<(), i64> {
    let p = protocol as u32 as i32;
    if !(0..263).contains(&p) {
        return Err(crate::why::inval());
    }
    match t {
        SOCK_STREAM if p == 0 || p == 6 => Ok(()),
        SOCK_DGRAM if p == 0 || p == 17 => Ok(()),
        SOCK_DGRAM if p == 1 => Err(-EACCES),
        SOCK_RAW if p != 0 => {
            if crate::cred::current().euid == 0 { Ok(()) } else { Err(-EPERM) }
        }
        SOCK_STREAM | SOCK_DGRAM | SOCK_RAW => Err(-EPROTONOSUPPORT),
        _ => Err(-ESOCKTNOSUPPORT),
    }
}

/// socket と socketpair の型のフラグと範囲 (Linux の __sys_socket と __sock_create: 知らないフラグは EINVAL、family は
/// EAFNOSUPPORT、型は EINVAL の順)。(family, 型, フラグ)
fn sock_args(domain: u64, typ: u64) -> Result<(u64, u64, u64), i64> {
    let typ = typ as u32 as u64;
    if typ & !0xf & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(crate::why::inval());
    }
    let d = domain as u32 as i32;
    if !(0..46).contains(&d) {
        return Err(-EAFNOSUPPORT);
    }
    if typ & 0xf >= 11 {
        return Err(crate::why::inval());
    }
    Ok((d as u64, typ & 0xf, typ & !0xf))
}

fn add_fd(s: Socket, cloexec: bool) -> R {
    let rc = Rc::new(RefCell::new(s));
    rc.borrow_mut().ino = Rc::as_ptr(&rc) as usize & 0xffffff;
    let f: FileRef = file::new(Kind::Socket(rc), 2);
    let fd = proc::current().files().add(f, cloexec, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

/// socketpair: AF_UNIX だけ (AF_INET は作れるものなら EOPNOTSUPP)
pub fn socketpair(domain: u64, typ: u64, protocol: u64, sv: usize) -> R {
    let (domain, t, flags) = sock_args(domain, typ)?;
    if domain == AF_INET {
        inet_check(t, protocol)?;
        return Err(-EOPNOTSUPP);
    }
    if domain != AF_UNIX {
        return Err(-EAFNOSUPPORT);
    }
    let a = crate::unix::create(t, protocol as u32 as u64)?;
    let b = crate::unix::create(t, protocol as u32 as u64)?;
    crate::unix::pair(&a, &b);
    let cloexec = flags & SOCK_CLOEXEC != 0;
    let files = proc::current().files();
    let fa = files.add(crate::unix::file_of(a, flags), cloexec, 0).ok_or(-EMFILE)?;
    let Some(fb) = files.add(crate::unix::file_of(b, flags), cloexec, 0) else {
        files.set(fa, None);
        return Err(-EMFILE);
    };
    let mut v = [0u8; 8];
    v[..4].copy_from_slice(&(fa as i32).to_le_bytes());
    v[4..].copy_from_slice(&(fb as i32).to_le_bytes());
    if proc::current().pt().copy_out(sv, &v).is_none() {
        let files = proc::current().files();
        files.set(fa, None);
        files.set(fb, None);
        return Err(-EFAULT);
    }
    Ok(0)
}

pub fn socket(domain: u64, typ: u64, protocol: u64) -> R {
    let (domain, t, flags) = sock_args(domain, typ)?;
    let cloexec = flags & SOCK_CLOEXEC != 0;
    if domain == AF_UNIX {
        let u = crate::unix::create(t, protocol as u32 as u64)?;
        let fd = proc::current().files().add(crate::unix::file_of(u, flags), cloexec, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    if domain != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let isolated = crate::ns::net().is_some();
    let protocol = protocol as u32 as u64;
    // ping はだれでも (Linux の ping_group_range を広げたもの)。raw は root だけ、ICMP だけ
    if (t == SOCK_DGRAM && protocol == IPPROTO_ICMP) || (t == SOCK_RAW && protocol == IPPROTO_ICMP && crate::cred::current().euid == 0) {
        n()?;
        let proto = if t == SOCK_RAW { Proto::Raw } else { Proto::Icmp };
        let mut s = Socket { proto, handle: None, local: None, peer: None, nonblock: flags & SOCK_NONBLOCK != 0, ino: 0, ttl: None, isolated };
        // raw は作ったときから受ける (traceroute は受ける口を poll で待つだけで、送らない)
        if s.proto == Proto::Raw && !isolated {
            s.bind_raw()?;
        }
        return add_fd(s, cloexec);
    }
    inet_check(t, protocol)?;
    if t == SOCK_RAW {
        return Err(-EPROTONOSUPPORT);
    }
    let u = crate::inet::create(t, protocol);
    let fd = proc::current().files().add(crate::inet::file_of(u, flags), cloexec, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

/// sockaddr_in のポート (landlock の TCP の bind / connect)
fn port_of(a: &[u8]) -> Option<u16> {
    (a.len() >= 4 && u16::from_le_bytes([a[0], a[1]]) as u64 == AF_INET).then(|| u16::from_be_bytes([a[2], a[3]]))
}

pub fn bind(fd: u64, addr: usize, len: usize) -> R {
    match sock(fd)? {
        Sock::Unix(_, u) => {
            let a = addr_in(addr, len)?;
            crate::unix::bind(&u, &a)
        }
        Sock::Inet(_, u) => {
            let a = addr_in(addr, len)?;
            if let (true, Some(p)) = (u.borrow().tcp, port_of(&a)) {
                crate::landlock::check_net(p, crate::landlock::BIND_TCP)?;
            }
            crate::inet::bind(&u, &a)
        }
        Sock::Smol(s) => {
            let ep = read_addr(addr, len)?;
            let mut s = s.borrow_mut();
            let a = if ep.addr.is_unspecified() { None } else { Some(ep.addr) };
            s.local = Some(IpListenEndpoint { addr: a, port: ep.port });
            Ok(0)
        }
    }
}

/// listen: backlog は (unsigned で) somaxconn まで
pub fn listen(fd: u64, backlog: u64) -> R {
    match sock(fd)? {
        Sock::Unix(_, u) => crate::unix::listen(&u, (backlog as u32).min(crate::unix::SOMAXCONN)),
        Sock::Inet(_, u) => crate::inet::listen(&u, backlog),
        Sock::Smol(_) => Err(-EOPNOTSUPP),
    }
}

/// accept4 (Linux の __sys_accept4_file: EBADF、知らないフラグの EINVAL、ENOTSOCK の順)。名前は相手 (つないできた口)
/// のもの。名前を返せなければ、受けた口は閉じる
pub fn accept(fd: u64, addr: usize, lenp: usize, flags: u64) -> R {
    fd_file(fd)?;
    let flags = flags as u32 as u64;
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(crate::why::inval());
    }
    match sock(fd)? {
        Sock::Unix(f, u) => {
            let n = crate::unix::accept(&f, &u)?;
            let name = crate::unix::name(&n, true).map_err(|_| -ECONNABORTED)?;
            let nf = crate::unix::file_of(n, flags);
            if addr != 0 {
                addr_out(addr, lenp, &name)?;
            }
            let fd = proc::current().files().add(nf, flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
            Ok(fd as i64)
        }
        Sock::Inet(f, u) => {
            let n = crate::inet::accept(&f, &u)?;
            let name = if addr != 0 { Some(crate::inet::name(&n, 2)) } else { None };
            let nf = crate::inet::file_of(n, flags);
            if let Some(name) = name {
                let name = name.map_err(|_| -ECONNABORTED)?;
                addr_out(addr, lenp, &name)?;
            }
            let fd = proc::current().files().add(nf, flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
            Ok(fd as i64)
        }
        Sock::Smol(_) => Err(-EOPNOTSUPP),
    }
}

pub fn connect(fd: u64, addr: usize, len: usize) -> R {
    match sock(fd)? {
        Sock::Unix(f, u) => {
            let a = addr_in(addr, len)?;
            crate::unix::connect(&f, &u, &a)
        }
        Sock::Inet(f, u) => {
            let a = addr_in(addr, len)?;
            if let (true, Some(p)) = (u.borrow().tcp, port_of(&a)) {
                crate::landlock::check_net(p, crate::landlock::CONNECT_TCP)?;
            }
            crate::inet::connect(&f, &u, &a)
        }
        Sock::Smol(s) => {
            let ep = read_addr(addr, len)?;
            let mut s = s.borrow_mut();
            if s.isolated {
                return Err(-ENETUNREACH);
            }
            s.peer = Some(ep);
            match s.proto {
                Proto::Icmp => s.bind_icmp().map(|_| 0),
                Proto::Raw => s.bind_raw().map(|_| 0),
            }
        }
    }
}

pub fn sendto(fd: u64, buf: usize, len: usize, flags: u64, addr: usize, alen: usize) -> R {
    let len = len.min(MAX_RW);
    match sock(fd)? {
        Sock::Unix(f, u) => {
            let to = if addr != 0 { Some(addr_in(addr, alen)?) } else { None };
            let iov = [(buf, len)];
            let nb = nonblock_of(&f, flags);
            crate::unix::send(&u, crate::unix::Src::User(&iov), len, to.as_deref(), &[], flags, nb)
        }
        Sock::Inet(f, u) => {
            let to = if addr != 0 { Some(addr_in(addr, alen)?) } else { None };
            let iov = [(buf, len)];
            let nb = nonblock_of(&f, flags);
            crate::inet::send(&u, crate::unix::Src::User(&iov), len, to.as_deref(), &[], flags, nb)
        }
        Sock::Smol(s) => {
            let to = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
            let mut data = uninit(len.min(64 * 1024));
            proc::current().pt().copy_in(&mut data, buf).ok_or(-EFAULT)?;
            let n = s.borrow_mut().send(&data, to, flags & MSG_DONTWAIT != 0)?;
            Ok(n as i64)
        }
    }
}

pub fn recvfrom(fd: u64, buf: usize, len: usize, flags: u64, addr: usize, lenp: usize) -> R {
    let iov = [(buf, len.min(MAX_RW))];
    match sock(fd)? {
        Sock::Unix(f, u) => {
            let r = unix_recv(&u, &iov, flags, nonblock_of(&f, flags))?;
            if addr != 0 {
                addr_out(addr, lenp, r.from.as_deref().unwrap_or(&[]))?;
            }
            Ok(r.ret as i64)
        }
        Sock::Inet(f, u) => {
            let r = inet_recv(&u, &iov, flags, nonblock_of(&f, flags))?;
            if addr != 0 {
                addr_out(addr, lenp, r.from.as_ref().map_or(&[][..], |a| &a[..]))?;
            }
            Ok(r.ret as i64)
        }
        Sock::Smol(s) => {
            let mut data = uninit(len.min(64 * 1024));
            let (k, from) = s.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0, flags & MSG_PEEK != 0)?;
            proc::current().pt().copy_out(buf, &data[..k]).ok_or(-EFAULT)?;
            if addr != 0 {
                addr_out(addr, lenp, &endpoint_bytes(from))?;
            }
            Ok(k as i64)
        }
    }
}

pub fn getsockname(fd: u64, addr: usize, lenp: usize) -> R {
    match sock(fd)? {
        Sock::Unix(_, u) => addr_out(addr, lenp, &crate::unix::name(&u, false)?)?,
        Sock::Inet(_, u) => addr_out(addr, lenp, &crate::inet::name(&u, 0)?)?,
        Sock::Smol(s) => {
            let s = s.borrow();
            let ep = s.local.map(|l| IpEndpoint::new(l.addr.unwrap_or(IpAddress::Ipv4(net::addr())), l.port));
            addr_out(addr, lenp, &endpoint_bytes(ep))?;
        }
    }
    Ok(0)
}

pub fn getpeername(fd: u64, addr: usize, lenp: usize) -> R {
    match sock(fd)? {
        Sock::Unix(_, u) => addr_out(addr, lenp, &crate::unix::name(&u, true)?)?,
        Sock::Inet(_, u) => addr_out(addr, lenp, &crate::inet::name(&u, 1)?)?,
        Sock::Smol(s) => {
            let peer = s.borrow().peer;
            if peer.is_none() {
                return Err(-ENOTCONN);
            }
            addr_out(addr, lenp, &endpoint_bytes(peer))?;
        }
    }
    Ok(0)
}

pub fn getsockopt(fd: u64, level: u64, opt: u64, val: usize, lenp: usize) -> R {
    const SOL_SOCKET: u64 = 1;
    const SO_TYPE: u64 = 3;
    let s = sock(fd)?;
    let pt = proc::current().pt();
    let mut l = [0u8; 4];
    pt.copy_in(&mut l, lenp).ok_or(-EFAULT)?;
    let len = i32::from_le_bytes(l);
    let v = match s {
        Sock::Unix(_, u) => crate::unix::getsockopt(&u, level as i32, opt as i32, len)?,
        Sock::Inet(_, u) => crate::inet::getsockopt(&u, level as i32, opt as i32, len)?,
        Sock::Smol(s) => {
            let v: i32 = match (level, opt) {
                (SOL_SOCKET, SO_TYPE) => if s.borrow().proto == Proto::Raw { 3 } else { 2 },
                (0, 2) => s.borrow().ttl.unwrap_or(64) as i32,
                _ => 0,
            };
            let mut b = v.to_le_bytes().to_vec();
            b.truncate(len.clamp(0, 4) as usize);
            b
        }
    };
    if !v.is_empty() {
        pt.copy_out(val, &v).ok_or(-EFAULT)?;
    }
    pt.copy_out(lenp, &(v.len() as u32).to_le_bytes()).ok_or(-EFAULT)?;
    Ok(0)
}

pub fn shutdown(fd: u64, how: u64) -> R {
    match sock(fd)? {
        Sock::Unix(_, u) => crate::unix::shutdown(&u, how as u32 as u64),
        Sock::Inet(_, u) => crate::inet::shutdown(&u, how),
        Sock::Smol(_) => Ok(0),
    }
}

/// struct msghdr: (name, namelen の場所, iov の (base, len) たち, controllen の場所, flags の場所)
struct MsgHdr {
    name: usize,
    namelen_at: usize,
    iov: Vec<(usize, usize)>,
    control: usize,
    controllen: usize,
    controllen_at: usize,
    flags_at: usize,
}

fn read_msghdr(va: usize) -> Result<MsgHdr, i64> {
    let pt = proc::current().pt();
    let mut b = [0u8; 56];
    pt.copy_in(&mut b, va).ok_or(-EFAULT)?;
    let u = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize;
    let (iov_at, iovlen) = (u(16), u(24));
    if iovlen > 1024 {
        return Err(crate::why::inval());
    }
    let mut iov = Vec::with_capacity(iovlen);
    for i in 0..iovlen {
        let mut e = [0u8; 16];
        pt.copy_in(&mut e, iov_at + i * 16).ok_or(-EFAULT)?;
        iov.push((u64::from_le_bytes(e[..8].try_into().unwrap()) as usize, u64::from_le_bytes(e[8..].try_into().unwrap()) as usize));
    }
    Ok(MsgHdr { name: u(0), namelen_at: va + 8, iov, control: u(32), controllen: u(40), controllen_at: va + 40, flags_at: va + 48 })
}

/// sendmsg の名前と付帯データ (Linux の ___sys_sendmsg: msghdr、名前 (長さ < 0 は EINVAL、128 まで)、iovec、付帯データ
/// (INT_MAX まで))
fn send_parts(m: &MsgHdr) -> Result<(Option<Vec<u8>>, Vec<u8>), i64> {
    let pt = proc::current().pt();
    let mut l = [0u8; 4];
    pt.copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
    let namelen = i32::from_le_bytes(l);
    if namelen < 0 {
        return Err(crate::why::inval());
    }
    let to = if m.name != 0 && namelen > 0 { Some(addr_in(m.name, (namelen as usize).min(128))?) } else { None };
    if m.controllen > i32::MAX as usize {
        return Err(-ENOBUFS);
    }
    let mut ctl = vec![0u8; if m.control != 0 { m.controllen } else { 0 }];
    if !ctl.is_empty() {
        pt.copy_in(&mut ctl, m.control).ok_or(-EFAULT)?;
    }
    Ok((to, ctl))
}

pub fn sendmsg(fd: u64, msg: usize, flags: u64) -> R {
    let s = sock(fd)?;
    let m = read_msghdr(msg)?;
    let total = m.iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    match s {
        Sock::Unix(f, u) => {
            let (to, ctl) = send_parts(&m)?;
            crate::unix::send(&u, crate::unix::Src::User(&m.iov), total, to.as_deref(), &ctl, flags, nonblock_of(&f, flags))
        }
        Sock::Inet(f, u) => {
            let (to, ctl) = send_parts(&m)?;
            crate::inet::send(&u, crate::unix::Src::User(&m.iov), total, to.as_deref(), &ctl, flags, nonblock_of(&f, flags))
        }
        Sock::Smol(s) => {
            let to = if m.name != 0 {
                let mut l = [0u8; 4];
                proc::current().pt().copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
                Some(read_addr(m.name, u32::from_le_bytes(l) as usize)?)
            } else {
                None
            };
            let mut data = Vec::new();
            for (base, len) in &m.iov {
                let start = data.len();
                data.resize(start + len, 0);
                proc::current().pt().copy_in(&mut data[start..], *base).ok_or(-EFAULT)?;
            }
            let n = s.borrow_mut().send(&data, to, flags & MSG_DONTWAIT != 0)?;
            Ok(n as i64)
        }
    }
}

/// recvmmsg / sendmmsg: struct mmsghdr (msghdr 56 バイト + msg_len) の並びを 1 つずつ。
/// 2 つ目からは待たない。1 つもできなければそのエラー。recvmmsg の timeout は見ない
pub fn mmsg(fd: u64, vec: usize, vlen: usize, flags: u64, send: bool) -> R {
    const MSG_WAITFORONE: u64 = 0x10000;
    let mut done = 0;
    for i in 0..vlen.min(1024) {
        let hdr = vec + i * 64;
        let mut f = flags & !MSG_WAITFORONE;
        if i > 0 {
            f |= MSG_DONTWAIT;
        }
        let r = if send { sendmsg(fd, hdr, f) } else { recvmsg(fd, hdr, f) };
        match r {
            Ok(n) => {
                proc::current().pt().copy_out(hdr + 56, &(n as u32).to_le_bytes()).ok_or(-14)?;
                done += 1;
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    Ok(done)
}

/// 付帯データを入れ物に (Linux の put_cmsg: 頭が入らなければ MSG_CTRUNC だけ、中身が入らなければ切って MSG_CTRUNC)。
/// (書くもの, 使った長さ, flags)
fn put_cmsgs(list: &[(i32, i32, Vec<u8>)], space: Option<usize>) -> (Vec<(usize, Vec<u8>)>, usize, u32) {
    let mut writes = Vec::new();
    let (mut used, mut flags) = (0usize, 0u32);
    for (level, typ, data) in list {
        let Some(room) = space.map(|s| s - used) else {
            flags |= MSG_CTRUNC;
            continue;
        };
        if room < 16 {
            flags |= MSG_CTRUNC;
            continue;
        }
        let full = 16 + data.len();
        let mut cmlen = full;
        if room < full {
            flags |= MSG_CTRUNC;
            cmlen = room;
        }
        let mut b = Vec::with_capacity(full);
        b.extend_from_slice(&(cmlen as u64).to_le_bytes());
        b.extend_from_slice(&level.to_le_bytes());
        b.extend_from_slice(&typ.to_le_bytes());
        b.extend_from_slice(data);
        b.truncate(cmlen);
        writes.push((used, b));
        used += full.div_ceil(8) * 8;
        used = used.min(space.unwrap_or(0));
    }
    (writes, used, flags)
}

pub fn recvmsg(fd: u64, msg: usize, flags: u64) -> R {
    let s = sock(fd)?;
    let m = read_msghdr(msg)?;
    let pt = proc::current().pt();
    let namelen = |m: &MsgHdr| -> Result<i32, i64> {
        let mut l = [0u8; 4];
        proc::current().pt().copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
        let n = i32::from_le_bytes(l);
        if n < 0 {
            return Err(crate::why::inval());
        }
        Ok(n)
    };
    match s {
        Sock::Unix(f, u) => {
            let namelen = namelen(&m)?;
            let mut r = unix_recv(&u, &m.iov, flags, nonblock_of(&f, flags))?;
            // 名前: 入れ物の長さが 0 なら、名前の長さも 0 (Linux は msg_name を NULL にして受ける)
            if m.name != 0 {
                let from = if namelen > 0 { r.from.clone().unwrap_or_default() } else { Vec::new() };
                addr_out(m.name, m.namelen_at, &from)?;
            }
            let space = (m.control != 0).then_some(m.controllen);
            let cloexec = flags & crate::unix::MSG_CMSG_CLOEXEC != 0;
            let out = crate::unix::scm_out(&mut r, crate::unix::passcred(&u), space, cloexec);
            for (off, b) in out.writes.iter().flatten() {
                pt.copy_out(m.control + off, b).ok_or(-EFAULT)?;
            }
            let mflags = r.flags | out.flags | (flags & crate::unix::MSG_CMSG_CLOEXEC) as u32;
            pt.copy_out(m.flags_at, &mflags.to_le_bytes()).ok_or(-EFAULT)?;
            pt.copy_out(m.controllen_at, &(out.used as u64).to_le_bytes()).ok_or(-EFAULT)?;
            Ok(r.ret as i64)
        }
        Sock::Inet(f, u) => {
            let namelen = namelen(&m)?;
            let r = inet_recv(&u, &m.iov, flags, nonblock_of(&f, flags))?;
            if m.name != 0 {
                let from: &[u8] = if namelen > 0 { r.from.as_ref().map_or(&[][..], |a| &a[..]) } else { &[] };
                addr_out(m.name, m.namelen_at, from)?;
            }
            let space = (m.control != 0).then_some(m.controllen);
            let (writes, used, cflags) = put_cmsgs(&r.cmsgs, space);
            for (off, b) in &writes {
                pt.copy_out(m.control + off, b).ok_or(-EFAULT)?;
            }
            let mflags = r.flags | cflags | (flags & MSG_CMSG_CLOEXEC) as u32;
            pt.copy_out(m.flags_at, &mflags.to_le_bytes()).ok_or(-EFAULT)?;
            pt.copy_out(m.controllen_at, &(used as u64).to_le_bytes()).ok_or(-EFAULT)?;
            Ok(r.ret as i64)
        }
        Sock::Smol(s) => {
            let total: usize = m.iov.iter().map(|(_, l)| l).sum();
            let mut data = uninit(total.min(64 * 1024));
            let (k, from) = s.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0, flags & MSG_PEEK != 0)?;
            copy_out_iov(&m.iov, &data[..k])?;
            if m.name != 0 {
                addr_out(m.name, m.namelen_at, &endpoint_bytes(from))?;
            }
            pt.copy_out(m.controllen_at, &0u64.to_le_bytes()).ok_or(-EFAULT)?;
            pt.copy_out(m.flags_at, &0u32.to_le_bytes()).ok_or(-EFAULT)?;
            Ok(k as i64)
        }
    }
}

/// /proc/net/tcp と /proc/net/udp (inet.rs)
pub fn proc_net(want_tcp: bool) -> alloc::string::String {
    crate::inet::proc_net(want_tcp)
}

pub fn setsockopt(fd: u64, level: u64, opt: u64, val: usize, len: usize) -> R {
    const IPPROTO_IP: u64 = 0;
    const IP_TTL: u64 = 2;
    let s = sock(fd)?;
    // Linux の do_sock_setsockopt: 長さ < 0 は EINVAL
    let len = len as u32 as i32;
    if len < 0 {
        return Err(crate::why::inval());
    }
    let mut b = vec![0u8; (len as usize).min(64)];
    match s {
        Sock::Unix(_, u) => {
            // Linux の sk_setsockopt: SOL_SOCKET でなければ EOPNOTSUPP、4 バイトより短ければ EINVAL、写してから
            if level as i32 != 1 {
                return Err(-EOPNOTSUPP);
            }
            if len < 4 {
                return Err(crate::why::inval());
            }
            b.truncate(16);
            proc::current().pt().copy_in(&mut b, val).ok_or(-EFAULT)?;
            crate::unix::setsockopt(&u, level as i32, opt as i32, &b)
        }
        Sock::Inet(_, u) => {
            if !b.is_empty() {
                proc::current().pt().copy_in(&mut b, val).ok_or(-EFAULT)?;
            }
            crate::inet::setsockopt(&u, level as i32, opt as i32, &b)
        }
        Sock::Smol(s) => {
            if (level, opt) == (IPPROTO_IP, IP_TTL) {
                let mut v = [0u8; 4];
                let n = (len as usize).min(4);
                proc::current().pt().copy_in(&mut v[..n], val).ok_or(-EFAULT)?;
                let v = i32::from_le_bytes(v);
                let ttl = match v {
                    -1 => None,
                    1..=255 => Some(v as u8),
                    _ => return Err(crate::why::inval()),
                };
                s.borrow_mut().set_ttl(ttl);
            }
            Ok(0)
        }
    }
}

/// /proc/net/sockstat (ss -s が読む): 使っているソケットの数
pub fn proc_sockstat() -> alloc::string::String {
    let c = crate::inet::counts();
    let raw = net::get().map_or(0, |n| n.sockets.iter().filter(|(_, s)| matches!(s, smoltcp::socket::Socket::Raw(_) | smoltcp::socket::Socket::Icmp(_))).count());
    // sockets: used は Linux の sock_inuse_get: どの family のソケットも (AF_UNIX も。TIME_WAIT の tw はのぞく)
    alloc::format!(
        "sockets: used {}\nTCP: inuse {} orphan {} tw {} alloc {} mem 0\nUDP: inuse {} mem 0\nUDPLITE: inuse 0\nRAW: inuse {}\nFRAG: inuse 0 memory 0\n",
        c.alloc + c.udp_all + raw + crate::unix::live(),
        c.inuse,
        c.orphan,
        c.tw,
        c.alloc,
        c.udp,
        raw
    )
}

/// /proc/net/snmp (ss -s と netstat -s が読む): TCP のつながっている数のほかは 0
pub fn proc_snmp() -> alloc::string::String {
    let estab = crate::inet::counts().estab;
    alloc::format!(
        "Ip: Forwarding DefaultTTL InReceives InHdrErrors InAddrErrors ForwDatagrams InUnknownProtos InDiscards InDelivers OutRequests OutDiscards OutNoRoutes ReasmTimeout ReasmReqds ReasmOKs ReasmFails FragOKs FragFails FragCreates OutTransmits\n\
Ip: 2 64 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n\
Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs OutRsts InCsumErrors\n\
Tcp: 1 200 120000 -1 0 0 0 0 {} 0 0 0 0 0 0\n\
Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors InCsumErrors IgnoredMulti MemErrors\n\
Udp: 0 0 0 0 0 0 0 0 0\n",
        estab
    )
}
