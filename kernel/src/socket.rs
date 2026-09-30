// BSD ソケット (AF_INET の TCP と UDP)。中身は smoltcp のソケット
use crate::file::{self, FileRef, Kind};
use crate::net;
use crate::proc;
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const EPIPE: i64 = 32;
const ENOTSOCK: i64 = 88;
const EPROTONOSUPPORT: i64 = 93;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EADDRINUSE: i64 = 98;
const ENETDOWN: i64 = 100;
const ECONNRESET: i64 = 104;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ETIMEDOUT: i64 = 110;
const ECONNREFUSED: i64 = 111;
const EINPROGRESS: i64 = 115;

const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;
const MSG_DONTWAIT: u64 = 0x40;

const TCP_RX: usize = 64 * 1024;
const TCP_TX: usize = 32 * 1024;
const CONNECT_TIMEOUT_TICKS: u64 = 30 * crate::timer::HZ;

#[derive(PartialEq)]
enum Proto {
    Tcp,
    Udp,
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
        let Some(n) = net::get() else { return };
        match self.proto {
            Proto::Tcp => net::orphan(h),
            Proto::Udp => {
                n.sockets.remove(h);
            }
        }
    }
}

impl Socket {
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, i64> {
        self.recv(buf, false).map(|(n, _)| n)
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, i64> {
        self.send(buf, None, false)
    }

    fn recv(&mut self, buf: &mut [u8], dontwait: bool) -> Result<(usize, Option<IpEndpoint>), i64> {
        let nb = self.nonblock || dontwait;
        net::poll();
        match self.proto {
            Proto::Tcp => {
                let h = self.handle.ok_or(-ENOTCONN)?;
                loop {
                    let s = tcp(h);
                    if s.can_recv() {
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
                        let (k, meta) = s.recv_slice(buf).map_err(|_| -EINVAL)?;
                        return Ok((k, Some(meta.endpoint)));
                    }
                    wait(nb, 0)?;
                }
            }
        }
    }

    fn send(&mut self, buf: &[u8], to: Option<IpEndpoint>, dontwait: bool) -> Result<usize, i64> {
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
        }
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
        self.local = Some(ep);
        self.handle = Some(h);
        Ok(h)
    }

    /// (読める, 書ける, 閉じた)
    pub fn readiness(&self) -> (bool, bool, bool) {
        net::poll();
        let Some(h) = self.handle else { return (false, self.proto == Proto::Udp, false) };
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

fn sock_of(fd: u64) -> Result<SockRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-file::EBADF)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Socket(s) => Ok(s.clone()),
        _ => Err(-ENOTSOCK),
    }
}

fn add_fd(s: Socket, cloexec: bool) -> R {
    let f: FileRef = file::new(Kind::Socket(Rc::new(RefCell::new(s))), 2);
    let fd = proc::current().files().add(f, cloexec, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

pub fn socket(domain: u64, typ: u64, _proto: u64) -> R {
    if domain != AF_INET {
        return Err(-EAFNOSUPPORT);
    }
    let proto = match typ & 0xf {
        SOCK_STREAM => Proto::Tcp,
        SOCK_DGRAM => Proto::Udp,
        _ => return Err(-EPROTONOSUPPORT),
    };
    n()?;
    let s = Socket { proto, handle: None, local: None, peer: None, listening: false, nonblock: typ & SOCK_NONBLOCK != 0 };
    add_fd(s, typ & SOCK_CLOEXEC != 0)
}

pub fn bind(fd: u64, addr: usize, len: usize) -> R {
    let s = sock_of(fd)?;
    let ep = read_addr(addr, len)?;
    let mut s = s.borrow_mut();
    let a = if ep.addr.is_unspecified() { None } else { Some(ep.addr) };
    s.local = Some(IpListenEndpoint { addr: a, port: ep.port });
    if s.proto == Proto::Udp {
        s.bind_udp()?;
    }
    Ok(0)
}

pub fn listen(fd: u64) -> R {
    let s = sock_of(fd)?;
    let mut s = s.borrow_mut();
    if s.proto != Proto::Tcp {
        return Err(-EOPNOTSUPP);
    }
    let ep = s.local.ok_or(-EINVAL)?;
    if s.handle.is_none() {
        let h = tcp_new()?;
        tcp(h).listen(ep).map_err(|_| -EADDRINUSE)?;
        s.handle = Some(h);
    }
    s.listening = true;
    Ok(0)
}

pub fn accept(fd: u64, addr: usize, lenp: usize, flags: u64) -> R {
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
            let c = Socket { proto: Proto::Tcp, handle: Some(h), local: s.local, peer, listening: false, nonblock: flags & SOCK_NONBLOCK != 0 };
            write_addr(addr, lenp, peer)?;
            return add_fd(c, flags & SOCK_CLOEXEC != 0);
        }
        let nb = s.nonblock;
        wait(nb, 0)?;
    }
}

pub fn connect(fd: u64, addr: usize, len: usize) -> R {
    let s = sock_of(fd)?;
    let ep = read_addr(addr, len)?;
    let mut s = s.borrow_mut();
    match s.proto {
        Proto::Udp => {
            s.peer = Some(ep);
            s.bind_udp()?;
            Ok(0)
        }
        Proto::Tcp => {
            if s.handle.is_some() {
                return Err(-EISCONN);
            }
            let h = tcp_new()?;
            let port = s.local.map(|l| l.port).filter(|&p| p != 0).unwrap_or_else(ephemeral);
            let net = n()?;
            if net.sockets.get_mut::<tcp::Socket>(h).connect(net.iface.context(), ep, port).is_err() {
                net.sockets.remove(h);
                return Err(-EINVAL);
            }
            s.handle = Some(h);
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
    let s = sock_of(fd)?;
    let to = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
    let mut data = vec![0u8; len.min(64 * 1024)];
    proc::current().pt().copy_in(&mut data, buf).ok_or(-EFAULT)?;
    let n = s.borrow_mut().send(&data, to, flags & MSG_DONTWAIT != 0)?;
    Ok(n as i64)
}

pub fn recvfrom(fd: u64, buf: usize, len: usize, flags: u64, addr: usize, lenp: usize) -> R {
    let s = sock_of(fd)?;
    let mut data = vec![0u8; len.min(64 * 1024)];
    let (k, from) = s.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0)?;
    proc::current().pt().copy_out(buf, &data[..k]).ok_or(-EFAULT)?;
    write_addr(addr, lenp, from)?;
    Ok(k as i64)
}

pub fn getsockname(fd: u64, addr: usize, lenp: usize) -> R {
    let s = sock_of(fd)?;
    let s = s.borrow();
    let ep = match (s.proto == Proto::Tcp, s.handle) {
        (true, Some(h)) => tcp(h).local_endpoint(),
        _ => s.local.map(|l| IpEndpoint::new(l.addr.unwrap_or(IpAddress::Ipv4(net::ADDR)), l.port)),
    };
    write_addr(addr, lenp, ep)?;
    Ok(0)
}

pub fn getpeername(fd: u64, addr: usize, lenp: usize) -> R {
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
    let s = sock_of(fd)?;
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
    Ok(MsgHdr { name: u(0), namelen_at: va + 8, iov, controllen_at: va + 40, flags_at: va + 48 })
}

pub fn sendmsg(fd: u64, msg: usize, flags: u64) -> R {
    let s = sock_of(fd)?;
    let m = read_msghdr(msg)?;
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

pub fn recvmsg(fd: u64, msg: usize, flags: u64) -> R {
    let s = sock_of(fd)?;
    let m = read_msghdr(msg)?;
    let total: usize = m.iov.iter().map(|(_, l)| l).sum();
    let mut data = vec![0u8; total.min(64 * 1024)];
    let (k, from) = s.borrow_mut().recv(&mut data, flags & MSG_DONTWAIT != 0)?;
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
    pt.copy_out(m.controllen_at, &0u64.to_le_bytes()).ok_or(-EFAULT)?;
    pt.copy_out(m.flags_at, &0u32.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(k as i64)
}
