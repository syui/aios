// Wayland の通信 (wire protocol)。サーバー (aiwm) とクライアント (aiterm) で使う
//
// 1 つのメッセージは「オブジェクトの ID (u32)」「大きさ << 16 | 番号 (u32)」と引数。
// 引数は 4 バイトずつ: int / uint / fixed (24.8) / object / new_id はそのまま、
// string は長さ (NUL こみ) と中身 (4 にそろえる)、array は長さと中身。
// fd は中身に入らず、sendmsg の SCM_RIGHTS で別に送る (受けた順に使う)
#![allow(dead_code)]
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;

/// 1 回に送る fd の数の上限 (libwayland と同じ)
const MAX_FDS_OUT: usize = 28;

pub enum Arg<'a> {
    U(u32),
    I(i32),
    /// 24.8 の固定小数点
    F(f64),
    S(&'a str),
    A(&'a [u8]),
    /// object / new_id (0 は null)
    O(u32),
    Fd(RawFd),
}

pub struct Conn {
    pub fd: RawFd,
    rbuf: Vec<u8>,
    rfds: VecDeque<RawFd>,
    wbuf: Vec<u8>,
    wfds: Vec<RawFd>,
}

pub struct Msg {
    pub id: u32,
    pub op: u16,
    body: Vec<u8>,
    pos: usize,
}

impl Conn {
    pub fn new(fd: RawFd) -> Conn {
        Conn { fd, rbuf: Vec::new(), rfds: VecDeque::new(), wbuf: Vec::new(), wfds: Vec::new() }
    }

    /// $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY (既定 wayland-0) につなぐ
    pub fn connect() -> io::Result<Conn> {
        let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
        let name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        let path = if name.starts_with('/') { name } else { format!("{}/{}", dir, name) };
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (a, len) = sockaddr(&path)?;
        if unsafe { libc::connect(fd, &a as *const _ as *const libc::sockaddr, len) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(io::Error::other(format!("{}: {}", path, e)));
        }
        Ok(Conn::new(fd))
    }

    /// 来ているものを読む (待たない)。相手が閉じたら Ok(false)
    pub fn recv(&mut self) -> io::Result<bool> {
        loop {
            let mut buf = [0u8; 4096];
            let mut cbuf = [0u64; 32];
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr() as *mut _;
            msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
            let n = unsafe { libc::recvmsg(self.fd, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(libc::EAGAIN) => Ok(true),
                    Some(libc::EINTR) => continue,
                    _ => Err(e),
                };
            }
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                        let p = libc::CMSG_DATA(c) as *const RawFd;
                        for i in 0..k {
                            self.rfds.push_back(*p.add(i));
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            if n == 0 {
                return Ok(false);
            }
            self.rbuf.extend_from_slice(&buf[..n as usize]);
            if (n as usize) < buf.len() {
                return Ok(true);
            }
        }
    }

    /// 読んだものから、まるごと来ているメッセージを 1 つ
    pub fn next(&mut self) -> Option<Msg> {
        if self.rbuf.len() < 8 {
            return None;
        }
        let id = u32::from_le_bytes(self.rbuf[0..4].try_into().unwrap());
        let w = u32::from_le_bytes(self.rbuf[4..8].try_into().unwrap());
        let size = (w >> 16) as usize;
        if size < 8 || self.rbuf.len() < size {
            return None;
        }
        let body = self.rbuf[8..size].to_vec();
        self.rbuf.drain(..size);
        Some(Msg { id, op: (w & 0xffff) as u16, body, pos: 0 })
    }

    /// 受けた fd を 1 つ (メッセージの fd 引数の順)
    pub fn take_fd(&mut self) -> Option<RawFd> {
        self.rfds.pop_front()
    }

    pub fn send(&mut self, id: u32, op: u16, args: &[Arg]) {
        let start = self.wbuf.len();
        self.wbuf.extend_from_slice(&id.to_le_bytes());
        self.wbuf.extend_from_slice(&[0; 4]);
        for a in args {
            match a {
                Arg::U(v) | Arg::O(v) => self.wbuf.extend_from_slice(&v.to_le_bytes()),
                Arg::I(v) => self.wbuf.extend_from_slice(&v.to_le_bytes()),
                Arg::F(v) => self.wbuf.extend_from_slice(&((v * 256.0) as i32).to_le_bytes()),
                Arg::S(s) => {
                    self.wbuf.extend_from_slice(&(s.len() as u32 + 1).to_le_bytes());
                    self.wbuf.extend_from_slice(s.as_bytes());
                    self.wbuf.push(0);
                    pad(&mut self.wbuf);
                }
                Arg::A(b) => {
                    self.wbuf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    self.wbuf.extend_from_slice(b);
                    pad(&mut self.wbuf);
                }
                Arg::Fd(fd) => self.wfds.push(*fd),
            }
        }
        let size = (self.wbuf.len() - start) as u32;
        self.wbuf[start + 4..start + 8].copy_from_slice(&(size << 16 | op as u32).to_le_bytes());
    }

    /// たまったものを送る。送りきれなかった分は残す (相手が読まないときに止まらない)。相手が閉じたら Err
    pub fn flush(&mut self) -> io::Result<()> {
        while !self.wbuf.is_empty() || !self.wfds.is_empty() {
            let mut cbuf = [0u64; 16];
            let k = self.wfds.len().min(MAX_FDS_OUT);
            let mut iov = libc::iovec { iov_base: self.wbuf.as_mut_ptr() as *mut _, iov_len: self.wbuf.len() };
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if k > 0 {
                msg.msg_control = cbuf.as_mut_ptr() as *mut _;
                msg.msg_controllen = unsafe { libc::CMSG_SPACE(4 * k as u32) } as _;
                unsafe {
                    let c = libc::CMSG_FIRSTHDR(&msg);
                    (*c).cmsg_level = libc::SOL_SOCKET;
                    (*c).cmsg_type = libc::SCM_RIGHTS;
                    (*c).cmsg_len = libc::CMSG_LEN(4 * k as u32) as _;
                    std::ptr::copy_nonoverlapping(self.wfds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, k);
                }
            }
            let n = unsafe { libc::sendmsg(self.fd, &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(libc::EAGAIN) => Ok(()),
                    Some(libc::EINTR) => continue,
                    _ => Err(e),
                };
            }
            self.wfds.drain(..k);
            self.wbuf.drain(..n as usize);
        }
        Ok(())
    }

    pub fn pending_out(&self) -> bool {
        !self.wbuf.is_empty()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        for fd in self.rfds.drain(..) {
            unsafe { libc::close(fd) };
        }
        unsafe { libc::close(self.fd) };
    }
}

fn pad(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

impl Msg {
    pub fn uint(&mut self) -> u32 {
        let v = self.body.get(self.pos..self.pos + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        self.pos += 4;
        v
    }

    pub fn int(&mut self) -> i32 {
        self.uint() as i32
    }

    pub fn fixed(&mut self) -> f64 {
        self.int() as f64 / 256.0
    }

    pub fn string(&mut self) -> String {
        let n = self.uint() as usize;
        let end = (self.pos + n).min(self.body.len());
        let s = String::from_utf8_lossy(&self.body[self.pos..end]).trim_end_matches('\0').to_string();
        self.pos += n.div_ceil(4) * 4;
        s
    }

    pub fn array(&mut self) -> Vec<u8> {
        let n = self.uint() as usize;
        let end = (self.pos + n).min(self.body.len());
        let v = self.body[self.pos..end].to_vec();
        self.pos += n.div_ceil(4) * 4;
        v
    }
}

/// struct sockaddr_un と長さ
pub fn sockaddr(path: &str) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as _;
    if path.len() >= a.sun_path.len() {
        return Err(io::Error::other("socket path too long"));
    }
    for (i, b) in path.bytes().enumerate() {
        a.sun_path[i] = b as _;
    }
    Ok((a, (2 + path.len() + 1) as _))
}

/// 共有メモリ (memfd) を作って mmap する: (fd, ポインタ)
pub fn shm(size: usize) -> io::Result<(RawFd, *mut u8)> {
    let fd = unsafe { libc::memfd_create(c"aios-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::ftruncate(fd, size as _) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if p == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok((fd, p as *mut u8))
}

/// 今の時刻 (ミリ秒、wl_pointer / wl_keyboard の time)
pub fn now_ms() -> u32 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}
