// BSD ソケット (AF_INET の TCP と UDP)。中身は smoltcp のソケット
use crate::file::{self, FileRef, Kind};
use crate::unix::UnixRef;
use crate::net;
use crate::proc;
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{icmp, raw, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpVersion, Ipv4Address};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const EPIPE: i64 = 32;
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
const ECONNRESET: i64 = 104;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ECONNREFUSED: i64 = 111;
const EALREADY: i64 = 114;
const EINPROGRESS: i64 = 115;
const EADDRNOTAVAIL: i64 = 99;
const ENETUNREACH: i64 = 101;

const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_PEEK: u64 = 0x2;
/// 1 回に読み書きできる長さ (Linux の MAX_RW_COUNT)
const MAX_RW: usize = 0x7fff_f000;

const TCP_RX: usize = 64 * 1024;
const TCP_TX: usize = 32 * 1024;
const CONNECT_TIMEOUT_TICKS: u64 = 30 * crate::timer::HZ;

#[derive(PartialEq)]
enum Proto {
    Tcp,
    Udp,
    /// ICMP の echo (Linux の ping ソケット: SOCK_DGRAM + IPPROTO_ICMP。root でなくても ping できる)
    Icmp,
    /// raw の ICMP (SOCK_RAW + IPPROTO_ICMP、root だけ)。受けるのは IP ヘッダーからの ICMP ぜんぶ (traceroute が
    /// 途中のルーターの「時間切れ」を受ける)。送るものには IP ヘッダーをつける
    Raw,
}

pub struct Socket {
    proto: Proto,
    handle: Option<SocketHandle>,
    /// bind した場所 (TCP は listen するまで覚えておくだけ)
    local: Option<IpListenEndpoint>,
    /// UDP の connect 先
    peer: Option<IpEndpoint>,
    listening: bool,
    pub nonblock: bool,
    /// /proc/PID/fd の socket:[N] の N (add_fd で決まる)
    ino: usize,
    /// setsockopt IP_TTL (なければ 64)
    ttl: Option<u8>,
    /// 分けた NET (ns.rs) の中の UDP / ICMP: どこへも送れない
    isolated: bool,
}

/// smoltcp のソケット → (inode, 持ち主の uid)。/proc/net/tcp が読む (待っているプロセスはソケットを
/// 借りたまま眠っているので、ソケットからは読めない)
static mut OWNERS: alloc::collections::BTreeMap<SocketHandle, (usize, u32)> = alloc::collections::BTreeMap::new();

fn owners() -> &'static mut alloc::collections::BTreeMap<SocketHandle, (usize, u32)> {
    unsafe { &mut *(&raw mut OWNERS) }
}

fn own(h: SocketHandle, ino: usize) {
    owners().insert(h, (ino, crate::cred::current().uid));
}

pub type SockRef = Rc<RefCell<Socket>>;

static mut NEXT_PORT: u16 = 49152;

fn ephemeral() -> u16 {
    unsafe {
        let p = NEXT_PORT;
        NEXT_PORT = if p == 65535 { 49152 } else { p + 1 };
        p
    }
}

fn n() -> Result<&'static mut net::Net, i64> {
    net::get().ok_or(-ENETDOWN)
}

fn tcp_new() -> Result<SocketHandle, i64> {
    let s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_RX]), tcp::SocketBuffer::new(vec![0; TCP_TX]));
    Ok(n()?.sockets.add(s))
}

fn udp_new() -> Result<SocketHandle, i64> {
    let meta = || vec![udp::PacketMetadata::EMPTY; 16];
    let s = udp::Socket::new(udp::PacketBuffer::new(meta(), vec![0; 16 * 1024]), udp::PacketBuffer::new(meta(), vec![0; 16 * 1024]));
    Ok(n()?.sockets.add(s))
}

fn tcp(h: SocketHandle) -> &'static mut tcp::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<tcp::Socket>(h)
}

fn udp(h: SocketHandle) -> &'static mut udp::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<udp::Socket>(h)
}

fn raw(h: SocketHandle) -> &'static mut raw::Socket<'static> {
    net::get().unwrap().sockets.get_mut::<raw::Socket>(h)
}

fn icmp(h: SocketHandle) -> &'static mut icmp::Socket<'static> {
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

/// 眠って待つ。nonblock なら EAGAIN、deadline を過ぎたら ETIMEDOUT
fn wait(nonblock: bool, deadline: u64) -> Result<(), i64> {
    if nonblock {
        return Err(-EAGAIN);
    }
    if deadline != 0 && crate::timer::ticks() >= deadline {
        return Err(-ETIMEDOUT);
    }
    proc::sleep_until(net::chan(), deadline)?;
    net::poll();
    Ok(())
}

impl Drop for Socket {
    fn drop(&mut self) {
        let Some(h) = self.handle.take() else { return };
        owners().remove(&h);
        let Some(n) = net::get() else { return };
        match self.proto {
            Proto::Tcp => net::orphan(h),
            Proto::Udp | Proto::Icmp | Proto::Raw => {
                n.sockets.remove(h);
            }
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
            Proto::Tcp => {
                let h = self.handle.ok_or(-ENOTCONN)?;
                loop {
                    let s = tcp(h);
                    if s.can_recv() {
                        if peek {
                            let k = s.peek_slice(buf).map_err(|_| -ECONNRESET)?;
                            return Ok((k, s.remote_endpoint()));
                        }
                        let k = s.recv_slice(buf).map_err(|_| -ECONNRESET)?;
                        net::poll(); // 窓が開いたことを知らせる
                        return Ok((k, s.remote_endpoint()));
                    }
                    if !s.may_recv() {
                        return Ok((0, None)); // 相手が閉じた
                    }
                    wait(nb, 0)?;
                }
            }
            Proto::Udp => {
                let h = self.bind_udp()?;
                loop {
                    let s = udp(h);
                    if s.can_recv() {
                        let (k, meta) = if peek { s.peek_slice(buf).map(|(k, m)| (k, *m)) } else { s.recv_slice(buf) }.map_err(|_| -EINVAL)?;
                        return Ok((k, Some(meta.endpoint)));
                    }
                    wait(nb, 0)?;
                }
            }
            // IP ヘッダーからぜんぶ。送り元は IP ヘッダーの中
            Proto::Raw => {
                let h = self.bind_raw()?;
                loop {
                    let s = raw(h);
                    if s.can_recv() {
                        let k = if peek { s.peek_slice(buf) } else { s.recv_slice(buf) }.map_err(|_| -EINVAL)?;
                        let from = (k >= 20).then(|| IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(buf[12], buf[13], buf[14], buf[15])), 0));
                        return Ok((k, from));
                    }
                    wait(nb, 0)?;
                }
            }
            // ICMP の echo reply (IP ヘッダーなし、ICMP ヘッダーから)
            Proto::Icmp => {
                let h = self.bind_icmp()?;
                loop {
                    let s = icmp(h);
                    if s.can_recv() {
                        let (k, from) = s.recv_slice(buf).map_err(|_| -EINVAL)?;
                        return Ok((k, Some(IpEndpoint::new(from, 0))));
                    }
                    wait(nb, 0)?;
                }
            }
        }
    }

    fn send(&mut self, buf: &[u8], to: Option<IpEndpoint>, dontwait: bool) -> Result<usize, i64> {
        if self.isolated {
            return Err(-ENETUNREACH);
        }
        let nb = self.nonblock || dontwait;
        match self.proto {
            Proto::Tcp => {
                let h = self.handle.ok_or(-ENOTCONN)?;
                let mut done = 0;
                while done < buf.len() {
                    let s = tcp(h);
                    if !s.may_send() {
                        return if done > 0 { Ok(done) } else { Err(-EPIPE) };
                    }
                    if s.can_send() {
                        done += s.send_slice(&buf[done..]).map_err(|_| -EPIPE)?;
                        net::poll();
                        continue;
                    }
                    if let Err(e) = wait(nb, 0) {
                        return if done > 0 { Ok(done) } else { Err(e) };
                    }
                }
                Ok(done)
            }
            Proto::Udp => {
                let to = to.or(self.peer).ok_or(-ENOTCONN)?;
                let h = self.bind_udp()?;
                udp(h).send_slice(buf, to).map_err(|_| -EAGAIN)?;
                net::poll();
                Ok(buf.len())
            }
            // ICMP のメッセージに IP ヘッダーをつけて送る (Linux の raw で IP_HDRINCL なしと同じ)
            Proto::Raw => {
                let to = to.or(self.peer).ok_or(-EDESTADDRREQ)?;
                let IpAddress::Ipv4(dst) = to.addr;
                let h = self.bind_raw()?;
                let total = 20 + buf.len();
                if total > 65535 {
                    return Err(-EINVAL);
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
                raw(h).send_slice(&pkt).map_err(|_| -EAGAIN)?;
                net::poll();
                Ok(buf.len())
            }
            // ICMP の echo request: Linux と同じく id をこのソケットの番号にして、チェックサムを計算しなおす
            Proto::Icmp => {
                let to = to.or(self.peer).ok_or(-EDESTADDRREQ)?;
                if buf.len() < 8 || buf[0] != 8 {
                    return Err(-EINVAL);
                }
                let h = self.bind_icmp()?;
                let id = self.local.map_or(0, |l| l.port);
                let mut pkt = buf.to_vec();
                pkt[4..6].copy_from_slice(&id.to_be_bytes());
                pkt[2..4].copy_from_slice(&[0, 0]);
                let sum = inet_checksum(&pkt);
                pkt[2..4].copy_from_slice(&sum.to_be_bytes());
                icmp(h).send_slice(&pkt, to.addr).map_err(|_| -EAGAIN)?;
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
        own(h, self.ino);
        Ok(h)
    }

    /// setsockopt IP_TTL: 送るものの TTL (bind していれば、すぐ smoltcp のソケットに)
    fn set_ttl(&mut self, ttl: Option<u8>) {
        self.ttl = ttl;
        if let Some(h) = self.handle {
            match self.proto {
                Proto::Udp => udp(h).set_hop_limit(ttl),
                Proto::Icmp => icmp(h).set_hop_limit(ttl),
                _ => {}
            }
        }
    }

    /// ICMP も送る前に自動で bind する (id は空いている番号)
    fn bind_icmp(&mut self) -> Result<SocketHandle, i64> {
        if let Some(h) = self.handle {
            return Ok(h);
        }
        let meta = || vec![icmp::PacketMetadata::EMPTY; 16];
        let mut sock = icmp::Socket::new(icmp::PacketBuffer::new(meta(), vec![0; 16 * 1024]), icmp::PacketBuffer::new(meta(), vec![0; 16 * 1024]));
        let id = self.local.map(|l| l.port).filter(|&p| p != 0).unwrap_or_else(ephemeral);
        sock.bind(icmp::Endpoint::Ident(id)).map_err(|_| -EADDRINUSE)?;
        sock.set_hop_limit(self.ttl);
        let h = n()?.sockets.add(sock);
        self.local = Some(IpListenEndpoint { addr: None, port: id });
        self.handle = Some(h);
        own(h, self.ino);
        Ok(h)
    }

    /// UDP は送る前に自動で bind する
    fn bind_udp(&mut self) -> Result<SocketHandle, i64> {
        if let Some(h) = self.handle {
            return Ok(h);
        }
        let h = udp_new()?;
        let ep = self.local.unwrap_or(IpListenEndpoint { addr: None, port: 0 });
        let ep = if ep.port == 0 { IpListenEndpoint { addr: ep.addr, port: ephemeral() } } else { ep };
        udp(h).bind(ep).map_err(|_| -EADDRINUSE)?;
        udp(h).set_hop_limit(self.ttl);
        self.local = Some(ep);
        self.handle = Some(h);
        own(h, self.ino);
        Ok(h)
    }

    /// (読める, 書ける, 閉じた)
    /// FIONREAD: いま読めるバイト数 (TCP は届いている分、UDP などは次のデータグラムの大きさ。Linux と同じ)
    pub fn available(&self) -> usize {
        net::poll();
        let Some(h) = self.handle else { return 0 };
        match self.proto {
            Proto::Tcp => tcp(h).recv_queue(),
            Proto::Udp => udp(h).peek().map_or(0, |(d, _)| d.len()),
            Proto::Icmp => usize::from(icmp(h).can_recv()),
            Proto::Raw => raw(h).peek().map_or(0, |d| d.len()),
        }
    }

    pub fn readiness(&self) -> (bool, bool, bool) {
        net::poll();
        // つながっていない TCP (connect の前、断られたあと) は Linux と同じく書ける + 閉じた (POLLOUT | POLLHUP)
        let Some(h) = self.handle else { return (false, true, self.proto == Proto::Tcp && !self.listening) };
        match self.proto {
            Proto::Tcp => {
                let s = tcp(h);
                if self.listening {
                    return (s.is_active() && s.state() != tcp::State::Listen, false, false);
                }
                let closed = !s.may_recv() && s.state() != tcp::State::SynSent;
                (s.can_recv() || closed, s.can_send(), matches!(s.state(), tcp::State::Closed))
            }
            Proto::Udp => (udp(h).can_recv(), true, false),
            Proto::Icmp => (icmp(h).can_recv(), true, false),
            Proto::Raw => (raw(h).can_recv(), true, false),
        }
    }
}

// ---- sockaddr_in ----

fn read_addr(va: usize, len: usize) -> Result<IpEndpoint, i64> {
    if len < 8 {
        return Err(-EINVAL);
    }
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    if u16::from_le_bytes([b[0], b[1]]) as u64 != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([b[2], b[3]]);
    Ok(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(b[4], b[5], b[6], b[7])), port))
}

fn write_addr(va: usize, lenp: usize, ep: Option<IpEndpoint>) -> Result<(), i64> {
    if va == 0 || lenp == 0 {
        return Ok(());
    }
    let mut b = [0u8; 16];
    b[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    if let Some(ep) = ep {
        b[2..4].copy_from_slice(&ep.port.to_be_bytes());
        let IpAddress::Ipv4(a) = ep.addr;
        b[4..8].copy_from_slice(&a.octets());
    }
    let pt = proc::current().pt();
    let mut l = [0u8; 4];
    pt.copy_in(&mut l, lenp).ok_or(-EFAULT)?;
    let n = (u32::from_le_bytes(l) as usize).min(16);
    pt.copy_out(va, &b[..n]).ok_or(-EFAULT)?;
    pt.copy_out(lenp, &16u32.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(())
}

// ---- システムコール ----

type R = Result<i64, i64>;

/// 0 で埋めない受け渡しの場所 (読んだ・写した分だけを使う)。64 KiB を毎回 0 で埋めると、IPC の
/// 小さなメッセージの recvmsg がそれだけで重い (Firefox が 1 秒に何百回も読む)
fn uninit(n: usize) -> alloc::vec::Vec<u8> {
    let mut v = alloc::vec::Vec::with_capacity(n);
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

fn sock_of(fd: u64) -> Result<SockRef, i64> {
    let f = fd_file(fd)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Socket(s) => Ok(s.clone()),
        _ => Err(-ENOTSOCK),
    }
}

/// AF_UNIX のソケット (unix.rs。分けた NET の中の TCP も) なら、その OpenFile と中身。fd がなければ EBADF
/// (ソケットでなければ None: ほかは sock_of で ENOTSOCK)
fn unix_sock(fd: u64) -> Result<Option<(FileRef, UnixRef)>, i64> {
    let f = fd_file(fd)?;
    let u = match &f.borrow().kind {
        Kind::Unix(u) => u.clone(),
        _ => return Ok(None),
    };
    Ok(Some((f, u)))
}

/// ユーザーの名前 (Linux の move_addr_to_kernel: 長さは 0 から sockaddr_storage (128) まで)
fn addr_in(va: usize, len: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    let len = len as u32 as i32;
    if !(0..=128).contains(&len) {
        return Err(-EINVAL);
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
        return Err(-EINVAL);
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

/// AF_UNIX から入れ物 (iovec) へ受ける。stream は、読めるものがあるのに入れ物にさわれなければ、読まずに EFAULT
/// (Linux の unix_stream_read_actor は写せなければその skb を残す)。dgram は受けてから写す (写せなければ、その
/// メッセージはなくなって EFAULT: Linux の __unix_dgram_recvmsg)
fn unix_recv(u: &UnixRef, iov: &[(usize, usize)], flags: u64, nonblock: bool) -> Result<crate::unix::Recv, i64> {
    let total = iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
    let stream = u.borrow().typ == crate::unix::SOCK_STREAM;
    if stream && flags & crate::unix::MSG_OOB == 0 && total > 0 && crate::unix::has_data(u) {
        let need = total.min(crate::unix::pending(u) as usize);
        if !writable(iov, need) {
            return Err(-EFAULT);
        }
    }
    let r = crate::unix::recv(u, total, flags, nonblock)?;
    let pt = proc::current().pt();
    let mut done = 0;
    for &(base, len) in iov {
        if done == r.data.len() {
            break;
        }
        let n = len.min(r.data.len() - done);
        pt.copy_out(base, &r.data[done..done + n]).ok_or(-EFAULT)?;
        done += n;
    }
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

/// AF_INET の型と protocol (Linux の inet_create: 型ごとの表にあるか (なければ ESOCKTNOSUPPORT、protocol がちがえば
/// EPROTONOSUPPORT)、raw は CAP_NET_RAW、ping は net.ipv4.ping_group_range (既定の 1 0 はだれも入らない))
fn inet_check(t: u64, protocol: u64) -> Result<(), i64> {
    let p = protocol as u32 as i32;
    if !(0..263).contains(&p) {
        return Err(-EINVAL);
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
        return Err(-EINVAL);
    }
    let d = domain as u32 as i32;
    if !(0..46).contains(&d) {
        return Err(-EAFNOSUPPORT);
    }
    if typ & 0xf >= 11 {
        return Err(-EINVAL);
    }
    Ok((d as u64, typ & 0xf, typ & !0xf))
}

fn add_fd(s: Socket, cloexec: bool) -> R {
    let rc = Rc::new(RefCell::new(s));
    {
        let mut b = rc.borrow_mut();
        b.ino = Rc::as_ptr(&rc) as usize & 0xffffff;
        if let Some(h) = b.handle {
            own(h, b.ino);
        }
    }
    let f: FileRef = file::new(Kind::Socket(rc), 2);
    let fd = proc::current().files().add(f, cloexec, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

/// socketpair: AF_UNIX だけ (AF_INET は作れるものなら EOPNOTSUPP)
pub fn socketpair(domain: u64, typ: u64, protocol: u64, sv: usize) -> R {
    const AF_UNIX: u64 = 1;
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
    const IPPROTO_ICMP: u64 = 1;
    const AF_UNIX: u64 = 1;
    let (domain, t, flags) = sock_args(domain, typ)?;
    let typ = t | flags;
    if domain == AF_UNIX {
        let u = crate::unix::create(t, protocol as u32 as u64)?;
        let fd = proc::current().files().add(crate::unix::file_of(u, flags), flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    if domain != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    // 分けた NET の中の TCP は、中どうしでだけつながる口 (unix.rs の inet)
    let isolated = crate::ns::net().is_some();
    if isolated && t == SOCK_STREAM {
        let f = crate::unix::file_of(crate::unix::inet_socket(), flags);
        let fd = proc::current().files().add(f, flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    let proto = match typ & 0xf {
        SOCK_STREAM => Proto::Tcp,
        SOCK_DGRAM if protocol == IPPROTO_ICMP => Proto::Icmp,
        // raw は ICMP だけ、root だけ (Linux の CAP_NET_RAW)
        SOCK_RAW if protocol == IPPROTO_ICMP => {
            if crate::cred::current().euid != 0 {
                return Err(-EPERM);
            }
            Proto::Raw
        }
        SOCK_DGRAM => Proto::Udp,
        _ => return Err(-EPROTONOSUPPORT),
    };
    n()?;
    let mut s = Socket { proto, handle: None, local: None, peer: None, listening: false, nonblock: typ & SOCK_NONBLOCK != 0, ino: 0, ttl: None, isolated };
    // raw は作ったときから受ける (traceroute は受ける口を poll で待つだけで、送らない)
    if s.proto == Proto::Raw && !isolated {
        s.bind_raw()?;
    }
    add_fd(s, typ & SOCK_CLOEXEC != 0)
}

/// 分けた NET の中: 127.x (か 0.0.0.0) でなければ、そのアドレスはない
fn loopback_only(ep: &IpEndpoint, err: i64) -> Result<(), i64> {
    let IpAddress::Ipv4(a) = ep.addr;
    if a.octets()[0] == 127 || ep.addr.is_unspecified() { Ok(()) } else { Err(err) }
}

pub fn bind(fd: u64, addr: usize, len: usize) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        if crate::unix::inet_port(&f).is_some() {
            let ep = read_addr(addr, len)?;
            loopback_only(&ep, -EADDRNOTAVAIL)?;
            crate::landlock::check_net(ep.port, crate::landlock::BIND_TCP)?;
            return crate::unix::inet_bind(&f, ep.port);
        }
        let a = addr_in(addr, len)?;
        return crate::unix::bind(&u, &a);
    }
    let s = sock_of(fd)?;
    let ep = read_addr(addr, len)?;
    let mut s = s.borrow_mut();
    if s.proto == Proto::Tcp {
        crate::landlock::check_net(ep.port, crate::landlock::BIND_TCP)?;
    }
    let a = if ep.addr.is_unspecified() { None } else { Some(ep.addr) };
    // port 0 は「空いているものを」(Linux と同じく bind のときに決め、getsockname で見える。Claude Code の
    // ログインの受け口など、port 0 で listen するものがある)
    let port = if ep.port == 0 && s.proto == Proto::Tcp { ephemeral() } else { ep.port };
    s.local = Some(IpListenEndpoint { addr: a, port });
    if s.proto == Proto::Udp && !s.isolated {
        s.bind_udp()?;
    }
    Ok(0)
}

/// listen: backlog は (unsigned で) somaxconn まで
pub fn listen(fd: u64, backlog: u64) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        if crate::unix::inet_port(&f).is_some() {
            return crate::unix::inet_listen(&f);
        }
        return crate::unix::listen(&u, (backlog as u32).min(crate::unix::SOMAXCONN));
    }
    let s = sock_of(fd)?;
    let mut s = s.borrow_mut();
    if s.proto != Proto::Tcp {
        return Err(-EOPNOTSUPP);
    }
    // bind していなければ、空いているポートで (Linux と同じ)
    let ep = *s.local.get_or_insert(IpListenEndpoint { addr: None, port: ephemeral() });
    if s.handle.is_none() {
        let h = tcp_new()?;
        tcp(h).listen(ep).map_err(|_| -EADDRINUSE)?;
        s.handle = Some(h);
        own(h, s.ino);
    }
    s.listening = true;
    Ok(0)
}

/// accept4 (Linux の __sys_accept4_file: EBADF、知らないフラグの EINVAL、ENOTSOCK の順)。名前は相手 (つないできた口)
/// のもの (分けた NET の中の TCP は 127.0.0.1:そのポート)。名前を返せなければ、受けた口は閉じる
pub fn accept(fd: u64, addr: usize, lenp: usize, flags: u64) -> R {
    fd_file(fd)?;
    let flags = flags as u32 as u64;
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(-EINVAL);
    }
    if let Some((f, u)) = unix_sock(fd)? {
        let n = crate::unix::accept(&f, &u)?;
        let name = crate::unix::name(&n, true).map_err(|_| -ECONNABORTED)?;
        let nf = crate::unix::file_of(n, flags);
        if addr != 0 {
            addr_out(addr, lenp, &name)?;
        }
        let fd = proc::current().files().add(nf, flags & SOCK_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    let s = sock_of(fd)?;
    let mut s = s.borrow_mut();
    if !s.listening {
        return Err(-EINVAL);
    }
    loop {
        net::poll();
        let h = s.handle.ok_or(-EINVAL)?;
        let t = tcp(h);
        if t.is_active() && t.state() != tcp::State::Listen {
            // つながったソケットを渡し、listen し直す
            let peer = t.remote_endpoint();
            let nh = tcp_new()?;
            tcp(nh).listen(s.local.unwrap()).map_err(|_| -EADDRINUSE)?;
            s.handle = Some(nh);
            own(nh, s.ino);
            let c = Socket { proto: Proto::Tcp, handle: Some(h), local: s.local, peer, listening: false, nonblock: flags & SOCK_NONBLOCK != 0, ino: 0, ttl: None, isolated: false };
            write_addr(addr, lenp, peer)?;
            return add_fd(c, flags & SOCK_CLOEXEC != 0);
        }
        let nb = s.nonblock;
        wait(nb, 0)?;
    }
}

pub fn connect(fd: u64, addr: usize, len: usize) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        if crate::unix::inet_port(&f).is_some() {
            let ep = read_addr(addr, len)?;
            loopback_only(&ep, -ENETUNREACH)?;
            crate::landlock::check_net(ep.port, crate::landlock::CONNECT_TCP)?;
            return crate::unix::inet_connect(&f, ep.port);
        }
        let a = addr_in(addr, len)?;
        return crate::unix::connect(&f, &u, &a);
    }
    let s = sock_of(fd)?;
    let ep = read_addr(addr, len)?;
    let mut s = s.borrow_mut();
    if s.proto == Proto::Tcp {
        crate::landlock::check_net(ep.port, crate::landlock::CONNECT_TCP)?;
    }
    if s.isolated {
        return Err(-ENETUNREACH);
    }
    match s.proto {
        Proto::Udp => {
            s.peer = Some(ep);
            s.bind_udp()?;
            Ok(0)
        }
        Proto::Icmp => {
            s.peer = Some(ep);
            s.bind_icmp()?;
            Ok(0)
        }
        Proto::Raw => {
            s.peer = Some(ep);
            s.bind_raw()?;
            Ok(0)
        }
        Proto::Tcp => {
            // もう connect したもの: 進みぐあいを答える (非同期 connect のあとに、終わったかをもう一度 connect で
            // たしかめるもの。NSPR の PR_ConnectContinue など)。つながる途中は EALREADY、断られたら ECONNREFUSED
            if let Some(h) = s.handle {
                return match tcp(h).state() {
                    tcp::State::SynSent | tcp::State::SynReceived => Err(-EALREADY),
                    tcp::State::Closed => {
                        s.handle = None;
                        owners().remove(&h);
                        n()?.sockets.remove(h);
                        Err(-ECONNREFUSED)
                    }
                    _ => Err(-EISCONN),
                };
            }
            let h = tcp_new()?;
            let port = s.local.map(|l| l.port).filter(|&p| p != 0).unwrap_or_else(ephemeral);
            let net = n()?;
            // 127.x へは 127.0.0.1 から (Linux と同じ。ほかは smoltcp が eth0 のアドレスを選ぶ)
            let local = match ep.addr {
                IpAddress::Ipv4(a) if a.octets()[0] == 127 => IpListenEndpoint { addr: Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1))), port },
                _ => IpListenEndpoint { addr: None, port },
            };
            if net.sockets.get_mut::<tcp::Socket>(h).connect(net.iface.context(), ep, local).is_err() {
                net.sockets.remove(h);
                return Err(-EINVAL);
            }
            s.handle = Some(h);
            own(h, s.ino);
            s.peer = Some(ep);
            net::poll();
            if s.nonblock {
                return Err(-EINPROGRESS);
            }
            let deadline = crate::timer::ticks() + CONNECT_TIMEOUT_TICKS;
            loop {
                match tcp(h).state() {
                    tcp::State::Established => return Ok(0),
                    tcp::State::Closed => {
                        s.handle = None;
                        owners().remove(&h);
                        n()?.sockets.remove(h);
                        return Err(-ECONNREFUSED);
                    }
                    _ => wait(false, deadline)?,
                }
            }
        }
    }
}

pub fn sendto(fd: u64, buf: usize, len: usize, flags: u64, addr: usize, alen: usize) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        let to = if addr != 0 { Some(addr_in(addr, alen)?) } else { None };
        let len = len.min(MAX_RW);
        let iov = [(buf, len)];
        let nb = nonblock_of(&f, flags);
        return crate::unix::send(&u, crate::unix::Src::User(&iov), len, to.as_deref(), &[], flags, nb);
    }
    let s = sock_of(fd)?;
    let to = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
    let mut data = uninit(len.min(64 * 1024));
    proc::current().pt().copy_in(&mut data, buf).ok_or(-EFAULT)?;
    let n = s.borrow_mut().send(&data, to, flags & MSG_DONTWAIT != 0)?;
    Ok(n as i64)
}

pub fn recvfrom(fd: u64, buf: usize, len: usize, flags: u64, addr: usize, lenp: usize) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        let iov = [(buf, len.min(MAX_RW))];
        let r = unix_recv(&u, &iov, flags, nonblock_of(&f, flags))?;
        if addr != 0 {
            addr_out(addr, lenp, r.from.as_deref().unwrap_or(&[]))?;
        }
        return Ok(r.ret as i64);
    }
    let s = sock_of(fd)?;
    let mut data = uninit(len.min(64 * 1024));
    let (k, from) = s.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0, flags & MSG_PEEK != 0)?;
    proc::current().pt().copy_out(buf, &data[..k]).ok_or(-EFAULT)?;
    write_addr(addr, lenp, from)?;
    Ok(k as i64)
}

pub fn getsockname(fd: u64, addr: usize, lenp: usize) -> R {
    if let Some((_, u)) = unix_sock(fd)? {
        addr_out(addr, lenp, &crate::unix::name(&u, false)?)?;
        return Ok(0);
    }
    let s = sock_of(fd)?;
    let s = s.borrow();
    let ep = match (s.proto == Proto::Tcp, s.handle) {
        (true, Some(h)) => tcp(h).local_endpoint(),
        _ => s.local.map(|l| IpEndpoint::new(l.addr.unwrap_or(IpAddress::Ipv4(net::addr())), l.port)),
    };
    write_addr(addr, lenp, ep)?;
    Ok(0)
}

pub fn getpeername(fd: u64, addr: usize, lenp: usize) -> R {
    if let Some((_, u)) = unix_sock(fd)? {
        addr_out(addr, lenp, &crate::unix::name(&u, true)?)?;
        return Ok(0);
    }
    let s = sock_of(fd)?;
    let s = s.borrow();
    let ep = match (s.proto == Proto::Tcp, s.handle) {
        (true, Some(h)) => tcp(h).remote_endpoint(),
        _ => s.peer,
    };
    if ep.is_none() {
        return Err(-ENOTCONN);
    }
    write_addr(addr, lenp, ep)?;
    Ok(0)
}

pub fn getsockopt(fd: u64, level: u64, opt: u64, val: usize, lenp: usize) -> R {
    const SOL_SOCKET: u64 = 1;
    const SO_ERROR: u64 = 4;
    const SO_TYPE: u64 = 3;
    if let Some((_, u)) = unix_sock(fd)? {
        let pt = proc::current().pt();
        let mut l = [0u8; 4];
        pt.copy_in(&mut l, lenp).ok_or(-EFAULT)?;
        let v = crate::unix::getsockopt(&u, level as i32, opt as i32, i32::from_le_bytes(l))?;
        if !v.is_empty() {
            pt.copy_out(val, &v).ok_or(-EFAULT)?;
        }
        pt.copy_out(lenp, &(v.len() as u32).to_le_bytes()).ok_or(-EFAULT)?;
        return Ok(0);
    }
    let s = sock_of(fd)?;
    const IPPROTO_IP: u64 = 0;
    const IP_OPTIONS: u64 = 4;
    if (level, opt) == (IPPROTO_IP, IP_OPTIONS) {
        // IP オプションはない (長さ 0)。4 バイトの 0 を返すと sshd は「オプションつき」として切る
        if lenp != 0 {
            proc::current().pt().copy_out(lenp, &0u32.to_le_bytes()).ok_or(-EFAULT)?;
        }
        return Ok(0);
    }
    let v: i32 = match (level, opt) {
        (SOL_SOCKET, SO_ERROR) => {
            // 非同期 connect の結果
            let s = s.borrow();
            match (s.proto == Proto::Tcp, s.handle) {
                (true, Some(h)) if tcp(h).state() == tcp::State::Closed => ECONNREFUSED as i32,
                _ => 0,
            }
        }
        (SOL_SOCKET, SO_TYPE) => if s.borrow().proto == Proto::Tcp { 1 } else { 2 },
        _ => 0,
    };
    if val != 0 {
        let pt = proc::current().pt();
        pt.copy_out(val, &v.to_le_bytes()).ok_or(-EFAULT)?;
        if lenp != 0 {
            pt.copy_out(lenp, &4u32.to_le_bytes()).ok_or(-EFAULT)?;
        }
    }
    Ok(0)
}

pub fn shutdown(fd: u64, how: u64) -> R {
    const SHUT_RD: u64 = 0;
    if let Some((_, u)) = unix_sock(fd)? {
        return crate::unix::shutdown(&u, how as u32 as u64);
    }
    let s = sock_of(fd)?;
    let s = s.borrow();
    if let (Proto::Tcp, Some(h)) = (&s.proto, s.handle) {
        if how != SHUT_RD {
            tcp(h).close();
            net::poll();
        }
    }
    Ok(0)
}

/// struct msghdr: (name, namelen の場所, iov の (base, len) たち, controllen の場所, flags の場所)
struct MsgHdr {
    name: usize,
    namelen_at: usize,
    iov: alloc::vec::Vec<(usize, usize)>,
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
        return Err(-EINVAL);
    }
    let mut iov = alloc::vec::Vec::with_capacity(iovlen);
    for i in 0..iovlen {
        let mut e = [0u8; 16];
        pt.copy_in(&mut e, iov_at + i * 16).ok_or(-EFAULT)?;
        iov.push((u64::from_le_bytes(e[..8].try_into().unwrap()) as usize, u64::from_le_bytes(e[8..].try_into().unwrap()) as usize));
    }
    Ok(MsgHdr { name: u(0), namelen_at: va + 8, iov, control: u(32), controllen: u(40), controllen_at: va + 40, flags_at: va + 48 })
}

pub fn sendmsg(fd: u64, msg: usize, flags: u64) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        // Linux の ___sys_sendmsg: msghdr、名前 (長さ < 0 は EINVAL、128 まで)、iovec、付帯データ (INT_MAX まで) を写し、
        // 付帯データを読むのは unix の sendmsg のはじめ
        let m = read_msghdr(msg)?;
        let pt = proc::current().pt();
        let mut l = [0u8; 4];
        pt.copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
        let namelen = i32::from_le_bytes(l);
        if namelen < 0 {
            return Err(-EINVAL);
        }
        let to = if m.name != 0 && namelen > 0 { Some(addr_in(m.name, (namelen as usize).min(128))?) } else { None };
        if m.controllen > i32::MAX as usize {
            return Err(-ENOBUFS);
        }
        let mut ctl = vec![0u8; if m.control != 0 { m.controllen } else { 0 }];
        if !ctl.is_empty() {
            pt.copy_in(&mut ctl, m.control).ok_or(-EFAULT)?;
        }
        let total = m.iov.iter().fold(0usize, |a, x| a.saturating_add(x.1)).min(MAX_RW);
        return crate::unix::send(&u, crate::unix::Src::User(&m.iov), total, to.as_deref(), &ctl, flags, nonblock_of(&f, flags));
    }
    let m = read_msghdr(msg)?;
    let s = sock_of(fd)?;
    let to = if m.name != 0 {
        let mut l = [0u8; 4];
        proc::current().pt().copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
        Some(read_addr(m.name, u32::from_le_bytes(l) as usize)?)
    } else {
        None
    };
    let mut data = alloc::vec::Vec::new();
    for (base, len) in &m.iov {
        let start = data.len();
        data.resize(start + len, 0);
        proc::current().pt().copy_in(&mut data[start..], *base).ok_or(-EFAULT)?;
    }
    let n = s.borrow_mut().send(&data, to, flags & MSG_DONTWAIT != 0)?;
    Ok(n as i64)
}

/// recvmmsg / sendmmsg: struct mmsghdr (msghdr 56 バイト + msg_len) の並びを 1 つずつ。
/// 2 つ目からは待たない。1 つもできなければそのエラー。recvmmsg の timeout は見ない
pub fn mmsg(fd: u64, vec: usize, vlen: usize, flags: u64, send: bool) -> R {
    const MSG_DONTWAIT: u64 = 0x40;
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

pub fn recvmsg(fd: u64, msg: usize, flags: u64) -> R {
    if let Some((f, u)) = unix_sock(fd)? {
        let m = read_msghdr(msg)?;
        let pt = proc::current().pt();
        let mut l = [0u8; 4];
        pt.copy_in(&mut l, m.namelen_at).ok_or(-EFAULT)?;
        let namelen = i32::from_le_bytes(l);
        if namelen < 0 {
            return Err(-EINVAL);
        }
        let mut r = unix_recv(&u, &m.iov, flags, nonblock_of(&f, flags))?;
        // 名前: 入れ物の長さが 0 なら、名前の長さも 0 (Linux は msg_name を NULL にして受ける)
        if m.name != 0 {
            let from = if namelen > 0 { r.from.clone().unwrap_or_default() } else { alloc::vec::Vec::new() };
            addr_out(m.name, m.namelen_at, &from)?;
        }
        let space = (m.control != 0).then_some(m.controllen);
        let cloexec = flags & crate::unix::MSG_CMSG_CLOEXEC != 0;
        let out = crate::unix::scm_out(&mut r, crate::unix::passcred(&u), space, cloexec);
        for (off, b) in &out.writes {
            pt.copy_out(m.control + off, b).ok_or(-EFAULT)?;
        }
        let mflags = r.flags | out.flags | (flags & crate::unix::MSG_CMSG_CLOEXEC) as u32;
        pt.copy_out(m.flags_at, &mflags.to_le_bytes()).ok_or(-EFAULT)?;
        pt.copy_out(m.controllen_at, &(out.used as u64).to_le_bytes()).ok_or(-EFAULT)?;
        return Ok(r.ret as i64);
    }
    let m = read_msghdr(msg)?;
    let total: usize = m.iov.iter().map(|(_, l)| l).sum();
    let mut data = uninit(total.min(64 * 1024));
    let ctl: (usize, u32) = (0, 0);
    let (k, from) = sock_of(fd)?.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0, flags & MSG_PEEK != 0)?;
    let pt = proc::current().pt();
    let mut done = 0;
    for (base, len) in &m.iov {
        let n = (*len).min(k - done);
        pt.copy_out(*base, &data[done..done + n]).ok_or(-EFAULT)?;
        done += n;
        if done == k {
            break;
        }
    }
    if m.name != 0 {
        write_addr(m.name, m.namelen_at, from)?;
    }
    pt.copy_out(m.controllen_at, &(ctl.0 as u64).to_le_bytes()).ok_or(-EFAULT)?;
    pt.copy_out(m.flags_at, &ctl.1.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(k as i64)
}

/// /proc/net/tcp と /proc/net/udp (Linux と同じ形。ss と netstat が読む)。inode は /proc/PID/fd の socket:[N] と
/// 同じ番号 (ss -p がプロセスを見つける)。閉じたあと残っているもの (TIME_WAIT など) は inode 0
pub fn proc_net(want_tcp: bool) -> alloc::string::String {
    use alloc::format;
    let Some(n) = net::get() else { return alloc::string::String::new() };
    let hex = |a: IpAddress| match a {
        IpAddress::Ipv4(v) => format!("{:08X}", u32::from_le_bytes(v.octets())),
        #[allow(unreachable_patterns)]
        _ => "00000000".into(),
    };
    let mut s = alloc::string::String::from(if want_tcp {
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
    } else {
        "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"
    });
    let mut i = 0;
    let unspec = IpAddress::Ipv4(Ipv4Address::UNSPECIFIED);
    for (h, sock) in n.sockets.iter() {
        let (inode, uid) = owners().get(&h).copied().unwrap_or((0, 0));
        let line = match sock {
            smoltcp::socket::Socket::Tcp(t) if want_tcp => {
                let st = match t.state() {
                    tcp::State::Established => 1,
                    tcp::State::SynSent => 2,
                    tcp::State::SynReceived => 3,
                    tcp::State::FinWait1 => 4,
                    tcp::State::FinWait2 => 5,
                    tcp::State::TimeWait => 6,
                    // まだつないでいない・閉じたものも (Linux も CLOSE の 07 で出す)
                    tcp::State::Closed => 7,
                    tcp::State::CloseWait => 8,
                    tcp::State::LastAck => 9,
                    tcp::State::Listen => 10,
                    tcp::State::Closing => 11,
                };
                let (la, lp) = match t.local_endpoint() {
                    Some(e) => (e.addr, e.port),
                    None => (t.listen_endpoint().addr.unwrap_or(unspec), t.listen_endpoint().port),
                };
                let (ra, rp) = t.remote_endpoint().map_or((unspec, 0), |e| (e.addr, e.port));
                format!(
                    "{:4}: {}:{:04X} {}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5}        0 {} 1 0000000000000000 100 0 0 10 0\n",
                    i, hex(la), lp, hex(ra), rp, st, t.send_queue(), t.recv_queue(), uid, inode
                )
            }
            smoltcp::socket::Socket::Udp(u) if !want_tcp => {
                let e = u.endpoint();
                if e.port == 0 {
                    continue;
                }
                format!(
                    "{:5}: {}:{:04X} 00000000:0000 07 00000000:00000000 00:00000000 00000000 {:5}        0 {} 2 0000000000000000 0\n",
                    i, hex(e.addr.unwrap_or(unspec)), e.port, uid, inode
                )
            }
            _ => continue,
        };
        s.push_str(&line);
        i += 1;
    }
    s
}

/// setsockopt: IP_TTL だけ効かせる (ほかは受けて何もしない。いままでと同じ)
pub fn setsockopt(fd: u64, level: u64, opt: u64, val: usize, len: usize) -> R {
    const IPPROTO_IP: u64 = 0;
    const IP_TTL: u64 = 2;
    if let Some((_, u)) = unix_sock(fd)? {
        // Linux の do_sock_setsockopt と sk_setsockopt: 長さ < 0 は EINVAL、SOL_SOCKET でなければ EOPNOTSUPP、
        // 4 バイトより短ければ EINVAL、写してから
        let len = len as u32 as i32;
        if len < 0 {
            return Err(-EINVAL);
        }
        if level as i32 != 1 {
            return Err(-EOPNOTSUPP);
        }
        if len < 4 {
            return Err(-EINVAL);
        }
        let mut b = vec![0u8; (len as usize).min(16)];
        proc::current().pt().copy_in(&mut b, val).ok_or(-EFAULT)?;
        return crate::unix::setsockopt(&u, level as i32, opt as i32, &b);
    }
    let s = sock_of(fd)?;
    if (level, opt) == (IPPROTO_IP, IP_TTL) {
        let mut b = [0u8; 4];
        proc::current().pt().copy_in(&mut b[..len.min(4)], val).ok_or(-EFAULT)?;
        let v = i32::from_le_bytes(b);
        let ttl = match v {
            -1 => None,
            1..=255 => Some(v as u8),
            _ => return Err(-EINVAL),
        };
        s.borrow_mut().set_ttl(ttl);
    }
    Ok(0)
}

/// /proc/net/sockstat (ss -s が読む): 使っているソケットの数
pub fn proc_sockstat() -> alloc::string::String {
    let Some(n) = net::get() else { return alloc::string::String::new() };
    let (mut tcp, mut udp, mut raw) = (0, 0, 0);
    for (_, s) in n.sockets.iter() {
        match s {
            smoltcp::socket::Socket::Tcp(t) if t.state() != tcp::State::Closed => tcp += 1,
            smoltcp::socket::Socket::Udp(_) => udp += 1,
            smoltcp::socket::Socket::Raw(_) | smoltcp::socket::Socket::Icmp(_) => raw += 1,
            _ => {}
        }
    }
    alloc::format!(
        "sockets: used {}\nTCP: inuse {} orphan 0 tw 0 alloc {} mem 0\nUDP: inuse {} mem 0\nUDPLITE: inuse 0\nRAW: inuse {}\nFRAG: inuse 0 memory 0\n",
        tcp + udp + raw,
        tcp,
        tcp,
        udp,
        raw
    )
}

/// /proc/net/snmp (ss -s と netstat -s が読む): TCP のつながっている数のほかは 0
pub fn proc_snmp() -> alloc::string::String {
    let estab = net::get().map_or(0, |n| {
        n.sockets.iter().filter(|(_, s)| matches!(s, smoltcp::socket::Socket::Tcp(t) if t.state() == tcp::State::Established)).count()
    });
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
