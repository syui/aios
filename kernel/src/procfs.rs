// /proc: プロセスの様子を見せる小さなファイルシステム
//
//   /proc/self          -> 自分の PID
//   /proc/PID/stat      Linux と同じ並びの 1 行
//   /proc/PID/status    Name, State, Pid, Uid など
//   /proc/PID/cmdline   引数 (NUL で区切る。プロセスのメモリの argv の文字列から)
//   /proc/PID/environ   環境 (同じく。自分のものか root だけ)
//   /proc/PID/statm     メモリ (ページ): size resident shared text lib data dt
//   /proc/PID/cwd       -> カレントディレクトリ
//   /proc/PID/fd/N      -> 開いているもの (ttyname はこれを読む)
//   /proc/mounts, /proc/uptime, /proc/meminfo, /proc/cmdline (カーネルのコマンドライン), /proc/cpuinfo
//   /proc/stat (CPU の時間、btime、processes ...), /proc/loadavg, /proc/version (ps や top、psutil が読む)
//   /proc/vmstat, /proc/diskstats (vmstat や iostat、psutil が読む)
//   /proc/net/tcp, udp  ソケットの一覧 (ss や netstat が読む)
//   /proc/net/dev       インターフェースごとの送り受けの数 (psutil や ifconfig が読む)
//   /proc/net/pnp       DHCP でもらった DNS (Linux の ip=dhcp と同じ形。/etc/resolv.conf はここへのリンク)
//   /proc/sys/...       カーネルの値 (sysctl.rs の表。root は書ける)
use crate::proc::{self, Proc, State};
use crate::vfs::*;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;

const EACCES: i64 = 13;
const PROC_SUPER_MAGIC: u64 = 0x9fa0;

#[derive(Clone, Copy, PartialEq)]
enum Node {
    Root,
    SelfLink,
    Mounts,
    Uptime,
    Meminfo,
    Swaps,
    Threads,
    Strace,
    Bkl,
    Kmsg,
    Sysstat,
    Modules,
    NetDir,
    Pnp,
    Route,
    NetTcp,
    NetUdp,
    NetSockstat,
    NetSnmp,
    /// /proc/net/dev (インターフェースごとの送り受けの数。psutil や ifconfig が読む)
    NetDev,
    /// まだ中身のない /proc/net のファイル (見出しだけ。netstat が読む): NET_STUBS の番号
    NetStub(u8),
    KernelCmdline,
    CpuInfo,
    StatAll,
    Loadavg,
    Version,
    Vmstat,
    Diskstats,
    /// /proc/sys の下のディレクトリ: sysctl の表の番号と、そのパスのはじめのいくつか
    SysDir(u16, u8),
    /// /proc/sys の下のファイル: sysctl の表の番号
    Sys(u16),
    Pid(u32),
    Stat(u32),
    Status(u32),
    Cmdline(u32),
    Environ(u32),
    Statm(u32),
    Comm(u32),
    Cwd(u32),
    RootLink(u32),
    Exe(u32),
    Maps(u32),
    Stack(u32),
    FdDir(u32),
    Fd(u32, usize),
}

pub struct ProcInode {
    fs: usize,
    node: Node,
}

pub fn new_root() -> InodeRef {
    Rc::new(ProcInode { fs: new_fs_id(), node: Node::Root })
}

/// IPv6、raw、unix のソケットの一覧は見出しだけ (IPv6 と raw はない。netstat がないと言って止まる)
const NET_STUBS: [(&str, &str); 6] = [
    ("sockstat6", "TCP6: inuse 0\nUDP6: inuse 0\nUDPLITE6: inuse 0\nRAW6: inuse 0\nFRAG6: inuse 0 memory 0\n"),
    ("tcp6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"),
    ("udp6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    ("raw", "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    ("raw6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    ("unix", "Num       RefCount Protocol Flags    Type St Inode Path\n"),
];

fn leader(pid: u32) -> Result<&'static mut Proc, i64> {
    proc::find_leader(pid).filter(|p| p.state != State::Zombie).ok_or(-ENOENT)
}

impl ProcInode {
    fn child(&self, node: Node) -> InodeRef {
        Rc::new(ProcInode { fs: self.fs, node })
    }

    fn ino(&self) -> u64 {
        match self.node {
            Node::Root => 1,
            Node::SelfLink => 2,
            Node::Mounts => 3,
            Node::Uptime => 4,
            Node::Meminfo => 5,
            Node::NetDir => 6,
            Node::Pnp => 7,
            Node::KernelCmdline => 8,
            Node::CpuInfo => 9,
            Node::StatAll => 13,
            Node::Vmstat => 30,
            Node::Diskstats => 36,
            Node::Loadavg => 14,
            Node::Version => 15,
            Node::Swaps => 10,
            Node::Threads => 31,
            Node::Strace => 32,
            Node::Bkl => 34,
            Node::Kmsg => 35,
            Node::Sysstat => 33,
            Node::Modules => 12,
            Node::Route => 11,
            Node::NetTcp => 16,
            Node::NetUdp => 17,
            Node::NetSockstat => 37,
            Node::NetSnmp => 38,
            Node::NetDev => 39,
            Node::NetStub(i) => 18 + i as u64,
            Node::SysDir(i, d) => 0x100 + i as u64 * 8 + d as u64,
            Node::Sys(i) => 0x1000 + i as u64,
            Node::Pid(p) => (p as u64) << 16 | 1,
            Node::Stat(p) => (p as u64) << 16 | 2,
            Node::Status(p) => (p as u64) << 16 | 3,
            Node::Cmdline(p) => (p as u64) << 16 | 4,
            Node::Comm(p) => (p as u64) << 16 | 10,
            Node::Environ(p) => (p as u64) << 16 | 11,
            Node::Statm(p) => (p as u64) << 16 | 12,
            Node::Cwd(p) => (p as u64) << 16 | 5,
            Node::RootLink(p) => (p as u64) << 16 | 13,
            Node::Exe(p) => (p as u64) << 16 | 7,
            Node::Maps(p) => (p as u64) << 16 | 8,
            Node::Stack(p) => (p as u64) << 16 | 9,
            Node::FdDir(p) => (p as u64) << 16 | 6,
            Node::Fd(p, n) => (p as u64) << 16 | (0x100 + n as u64),
        }
    }

    fn pid(&self) -> Option<u32> {
        match self.node {
            Node::Pid(p) | Node::Stat(p) | Node::Status(p) | Node::Cmdline(p) | Node::Environ(p) | Node::Statm(p) | Node::Comm(p) | Node::Cwd(p) | Node::RootLink(p) | Node::Exe(p) | Node::Maps(p) | Node::Stack(p) | Node::FdDir(p) | Node::Fd(p, _) => Some(p),
            _ => None,
        }
    }

    /// ファイルの中身
    fn content(&self) -> Result<String, i64> {
        Ok(match self.node {
            Node::Mounts => {
                let etc = resolve("", "etc/mtab", true)?;
                let mut b = alloc::vec![0u8; etc.meta().size as usize];
                let n = etc.read_at(0, &mut b)?;
                String::from_utf8_lossy(&b[..n]).into_owned()
            }
            Node::KernelCmdline => format!("{}\n", crate::dtb::bootargs().unwrap_or("")),
            Node::CpuInfo => {
                let midr: u64;
                unsafe { core::arch::asm!("mrs {}, midr_el1", out(reg) midr) };
                let mut s = String::new();
                for i in 0..crate::smp::online() {
                    s += &format!(
                        "processor\t: {}\nBogoMIPS\t: {}.00\nFeatures\t: fp asimd\nCPU implementer\t: {:#04x}\nCPU architecture: 8\nCPU variant\t: {:#x}\nCPU part\t: {:#05x}\nCPU revision\t: {}\n\n",
                        i,
                        crate::timer::freq() * 2 / 1_000_000,
                        (midr >> 24) & 0xff,
                        (midr >> 20) & 0xf,
                        (midr >> 4) & 0xfff,
                        midr & 0xf
                    );
                }
                s
            }
            Node::Uptime => {
                let t = crate::timer::ticks();
                format!("{}.{:02} 0.00\n", t / 100, t % 100)
            }
            Node::Meminfo => {
                use crate::memlayout::{ram_size, PGSIZE};
                let total = ram_size() / 1024;
                let free = crate::kalloc::nfree() * PGSIZE / 1024;
                let (st, sf) = crate::swap::totals();
                format!(
                    // Buffers から下は、Linux の道具 (free、vmstat、top、psutil) が読む行。aios にない区分は 0
                    "MemTotal:     {:8} kB\nMemFree:      {:8} kB\nMemAvailable: {:8} kB\nBuffers:      {:8} kB\nCached:       {:8} kB\nSwapCached:   {:8} kB\nActive:       {:8} kB\nInactive:     {:8} kB\nShmem:        {:8} kB\nSlab:         {:8} kB\nSReclaimable: {:8} kB\nSwapTotal:    {:8} kB\nSwapFree:     {:8} kB\nDirty:        {:8} kB\n",
                    total,
                    free,
                    free,
                    0,
                    0,
                    0,
                    total - free,
                    0,
                    crate::vm::shared_pages() * PGSIZE / 1024,
                    0,
                    0,
                    st * PGSIZE / 1024,
                    sf * PGSIZE / 1024,
                    0
                )
            }
            Node::Swaps => crate::swap::proc_swaps(),
            Node::Threads => proc::threads_text(),
            Node::Strace => crate::syscall::strace_get(),
            Node::Bkl => crate::smp::stats(),
            Node::Kmsg => crate::kmsg::text(),
            Node::Sysstat => crate::syscall::sysstat(),
            Node::Modules => crate::module::proc_modules(),
            Node::Route => crate::netif::proc_route(),
            Node::NetDev => {
                let mut s = String::from("Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n");
                for (name, lo) in [("lo", true), ("eth0", false)] {
                    let c = crate::net::stats(lo);
                    s += &format!("{:>6}: {:>8} {:>7}    0    0    0     0          0         0 {:>8} {:>7}    0    0    0     0       0          0\n", name, c[0], c[1], c[2], c[3]);
                }
                s
            }
            Node::NetTcp => crate::socket::proc_net(true),
            Node::NetUdp => crate::socket::proc_net(false),
            Node::NetSockstat => crate::socket::proc_sockstat(),
            Node::NetSnmp => crate::socket::proc_snmp(),
            Node::NetStub(i) => NET_STUBS[i as usize].1.into(),
            Node::Sys(i) => crate::sysctl::TABLE[i as usize].read(),
            Node::Pnp => {
                // Linux と同じく、DHCP なら #PROTO: DHCP、手で決めたなら #MANUAL
                let mut s = String::from(if crate::net::is_dhcp() { "#PROTO: DHCP\n" } else { "#MANUAL\n" });
                if let Some(l) = crate::net::get().and_then(|n| n.lease.clone()) {
                    for d in &l.dns {
                        s.push_str(&format!("nameserver {}\n", d));
                    }
                    if let Some(r) = l.router {
                        s.push_str(&format!("bootserver {}\n", r));
                    }
                }
                s
            }
            Node::Stat(pid) => stat_line(leader(pid)?),
            Node::Maps(pid) => leader(pid)?.mm().pt.maps_text(),
            Node::Stack(pid) => proc::stacks_text(leader(pid)?.tgid),
            Node::Status(pid) => {
                let p = leader(pid)?;
                let c = &p.cred;
                let threads = proc::threads_of(p.tgid).len();
                let (vsize, rss) = mem_of(p);
                format!(
                    "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\nThreads:\t{}\nNoNewPrivs:\t{}\nLandlock:\t{}\n",
                    p.comm(),
                    state_name(p),
                    p.tgid,
                    p.pid,
                    p.ppid,
                    c.uid,
                    c.euid,
                    c.suid,
                    c.euid,
                    c.gid,
                    c.egid,
                    c.sgid,
                    c.egid,
                    vsize / 1024,
                    rss * 4,
                    threads,
                    c.no_new_privs as u8,
                    crate::landlock::layers(c)
                )
            }
            Node::Cmdline(pid) => {
                let p = leader(pid)?;
                let (a, e, _) = p.mm.as_ref().map_or((0, 0, 0), |m| m.get().args);
                match user_bytes(p, a, e) {
                    Some(b) if !b.is_empty() => String::from_utf8_lossy(&b).into_owned(),
                    // カーネルのスレッドや、読めないとき: Linux はカーネルのスレッドなら空。comm を出す
                    _ => format!("{}\0", p.comm()),
                }
            }
            Node::Environ(pid) => {
                let p = leader(pid)?;
                let me = crate::cred::current();
                if me.euid != 0 && me.euid != p.cred.uid {
                    return Err(-EACCES);
                }
                let (_, e, end) = p.mm.as_ref().map_or((0, 0, 0), |m| m.get().args);
                String::from_utf8_lossy(&user_bytes(p, e, end).unwrap_or_default()).into_owned()
            }
            Node::Statm(pid) => {
                let p = leader(pid)?;
                let (vsize, rss) = mem_of(p);
                format!("{} {} 0 0 0 {} 0\n", vsize / crate::memlayout::PGSIZE, rss, rss)
            }
            Node::StatAll => stat_all(),
            Node::Diskstats => crate::block::proc_diskstats(),
            Node::Vmstat => {
                use core::sync::atomic::Ordering::Relaxed;
                // ページ (4 KiB) の数と、Linux の vmstat が読む数え (ないものは 0)
                let free = crate::kalloc::nfree();
                let (mut rd, mut wr) = (0, 0);
                for p in crate::block::parts().iter().filter(|p| p.num != 0) {
                    let c = crate::block::io_stats(p.num);
                    rd += c[1];
                    wr += c[4];
                }
                format!(
                    "nr_free_pages {}\nnr_inactive_anon 0\nnr_active_anon 0\nnr_inactive_file 0\nnr_active_file 0\nnr_dirty 0\nnr_writeback 0\nnr_shmem {}\npgpgin {}\npgpgout {}\npswpin {}\npswpout {}\npgfault {}\npgmajfault {}\npgfree 0\n",
                    free,
                    crate::vm::shared_pages(),
                    rd / 2,
                    wr / 2,
                    crate::swap::PSWPIN.load(Relaxed),
                    crate::swap::PSWPOUT.load(Relaxed),
                    crate::smp::FAULTS.load(Relaxed),
                    crate::swap::PSWPIN.load(Relaxed)
                )
            }
            Node::Loadavg => {
                use core::sync::atomic::Ordering::Relaxed;
                let f = |i: usize| {
                    let v = proc::LOAD[i].load(Relaxed);
                    format!("{}.{:02}", v >> 11, ((v & 2047) * 100) >> 11)
                };
                let total = proc::nr_threads();
                format!("{} {} {} {}/{} {}\n", f(0), f(1), f(2), proc::nr_running(), total, proc::last_pid())
            }
            Node::Version => format!("aios version {} (rustc) #1 SMP\n", env!("AIOS_RELEASE")),
            Node::Comm(pid) => format!("{}\n", leader(pid)?.comm()),
            _ => return Err(-EISDIR),
        })
    }
}

/// プロセスのメモリの [start, end) (引数や環境の文字列。64 KiB まで)
fn user_bytes(p: &mut Proc, start: usize, end: usize) -> Option<Vec<u8>> {
    if start == 0 || end <= start {
        return None;
    }
    let mut b = alloc::vec![0u8; (end - start).min(64 << 10)];
    p.mm().pt.copy_in(&mut b, start)?;
    Some(b)
}

/// /proc/stat: CPU ごとの時間 (tick。user と idle だけ数える)、起動した時刻、作ったプロセスの数など
fn stat_all() -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let now = crate::timer::ticks();
    let n = crate::smp::online();
    let busy: Vec<u64> = (0..n).map(|c| proc::BUSY[c].load(Relaxed).min(now)).collect();
    let line = |name: &str, u: u64, idle: u64| format!("{} {} 0 0 {} 0 0 0 0 0 0\n", name, u, idle);
    let mut s = line("cpu", busy.iter().sum(), busy.iter().map(|b| now - b).sum());
    for (c, b) in busy.iter().enumerate() {
        s += &line(&format!("cpu{}", c), *b, now - b);
    }
    let boot = (crate::timer::epoch_ns() / 1_000_000_000).saturating_sub(now / crate::timer::HZ);
    let blocked = 0;
    s += &format!(
        "intr 0\nctxt 0\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked {}\n",
        boot,
        proc::last_pid(),
        proc::nr_running(),
        blocked
    );
    s
}

fn state_char(p: &Proc) -> char {
    if p.stopped && p.state != State::Zombie {
        return 'T';
    }
    match p.state {
        State::Running | State::Runnable => 'R',
        State::Sleeping => 'S',
        State::Zombie => 'Z',
        State::Unused => 'X',
    }
}

fn state_name(p: &Proc) -> &'static str {
    match state_char(p) {
        'R' => "R (running)",
        'S' => "S (sleeping)",
        'Z' => "Z (zombie)",
        'T' => "T (stopped)",
        _ => "X (dead)",
    }
}

/// /proc/PID/stat (Linux の fs/proc/array.c と同じ 52 項目)
/// (VmSize バイト, RSS ページ)
fn mem_of(p: &Proc) -> (usize, usize) {
    match &p.mm {
        Some(m) => {
            let m = m.get();
            (m.pt.vsize(), m.pt.resident())
        }
        None => (0, 0),
    }
}

fn stat_line(p: &Proc) -> String {
    let (tty_nr, tpgid) = crate::tty::of_session(p.sid).map_or((0, -1), |(rdev, pg)| (rdev, pg as i64));
    let threads = proc::threads_of(p.tgid).len();
    let mut s = format!(
        "{} ({}) {} {} {} {} {} {} 0 0 0 0 0 {} 0 {} 0 20 0 {} 0 {} {} {}",
        p.tgid,
        p.comm(),
        state_char(p),
        p.ppid,
        p.pgid,
        p.sid,
        tty_nr,
        tpgid,
        proc::group_utime(p.tgid),
        p.cutime,
        threads,
        p.start,
        mem_of(p).0,
        mem_of(p).1
    );
    // 残り (rsslim から exit_code まで) は 0
    for _ in 25..=52 {
        s.push_str(" 0");
    }
    s.push('\n');
    s
}

/// fd の向こう側の名前 (readlink /proc/PID/fd/N)
fn fd_target(pid: u32, n: usize) -> Result<String, i64> {
    let p = leader(pid)?;
    let f = p.files().get(n as u64).cloned().ok_or(-ENOENT)?;
    let f = f.borrow();
    Ok(f.describe())
}

/// /proc/PID/fd/N (と /dev/stdin のような、そこへのリンク) が指す、開いているもの (OpenFile)。
/// ファイルの名前があるもの (Kind::Inode) は magic_link でふつうに開くので、ここでは None
pub fn fd_link_file(cwd: &str, path: &str) -> Option<crate::file::FileRef> {
    let (mut dir, mut p) = (String::from(cwd), String::from(path));
    for _ in 0..8 {
        let (full, ino) = crate::vfs::lookup(&dir, &p, false).ok()?;
        if ino.meta().mode & crate::vfs::S_IFMT != crate::vfs::S_IFLNK {
            return None;
        }
        if let Some(pi) = ino.as_any().downcast_ref::<ProcInode>() {
            let Node::Fd(pid, n) = pi.node else { return None };
            let f = leader(pid).ok()?.files().get(n as u64).cloned()?;
            let named = matches!(f.borrow().kind, crate::file::Kind::Inode(..));
            return (!named).then_some(f);
        }
        // リンクの先は、リンクのあるディレクトリから
        p = ino.readlink().ok()?;
        dir = full.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
    }
    None
}

impl Inode for ProcInode {
    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino())
    }

    fn magic_link(&self) -> Option<(String, InodeRef)> {
        let Node::Fd(pid, n) = self.node else { return None };
        let f = leader(pid).ok()?.files().get(n as u64).cloned()?;
        let f = f.borrow();
        match &f.kind {
            crate::file::Kind::Inode(ino, path) => Some((path.clone(), ino.clone())),
            _ => None,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let (uid, gid) = self.pid().and_then(|p| leader(p).ok()).map_or((0, 0), |p| (p.cred.euid, p.cred.egid));
        let mode = match self.node {
            Node::Root | Node::Pid(_) | Node::NetDir | Node::SysDir(..) => S_IFDIR | 0o555,
            Node::Sys(i) if crate::sysctl::TABLE[i as usize].writable() => S_IFREG | 0o644,
            Node::FdDir(_) => S_IFDIR | 0o500,
            Node::SelfLink | Node::Cwd(_) | Node::RootLink(_) | Node::Exe(_) => S_IFLNK | 0o777,
            Node::Fd(..) => S_IFLNK | 0o700,
            Node::Strace | Node::Bkl => S_IFREG | 0o644,
            _ => S_IFREG | 0o444,
        };
        let now = crate::timer::epoch_ns();
        Meta { ino: self.ino(), mode, nlink: 1, uid, gid, size: 0, rdev: 0, blocks: 0, mtime: now, ctime: now }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let s = self.content()?;
        let b = s.as_bytes();
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, _: usize, b: &[u8]) -> Result<usize, i64> {
        // /proc/sys/...: root が値を書く
        if let Node::Sys(i) = self.node
            && crate::sysctl::TABLE[i as usize].writable()
            && crate::cred::current().euid == 0
        {
            crate::sysctl::TABLE[i as usize].write(b)?;
            return Ok(b.len());
        }
        // /proc/strace: root が名前を書くと、その名前のプロセスの失敗したシステムコールを出す (空で止める)
        if self.node == Node::Strace && crate::cred::current().euid == 0 {
            crate::syscall::strace_set(b);
            return Ok(b.len());
        }
        // /proc/bkl: 書くと (root) 大きなロックの統計を 0 から数えなおす
        if self.node == Node::Bkl && crate::cred::current().euid == 0 {
            crate::smp::stats_reset();
            return Ok(b.len());
        }
        Err(-EACCES)
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        // > /proc/strace (O_TRUNC) は受けつける
        let sys = matches!(self.node, Node::Sys(i) if crate::sysctl::TABLE[i as usize].writable());
        if (sys || matches!(self.node, Node::Strace | Node::Bkl)) && crate::cred::current().euid == 0 {
            return Ok(());
        }
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        match self.node {
            Node::SelfLink => Ok(format!("{}", proc::current().tgid)),
            Node::Cwd(pid) => Ok(format!("/{}", leader(pid)?.files().cwd)),
            // chroot していれば、その場所 (pidof はこれが自分と同じものだけ探す)
            Node::RootLink(pid) => Ok(format!("/{}", leader(pid)?.files().root)),
            Node::Exe(pid) => {
                let p = leader(pid)?;
                p.mm.as_ref().ok_or(-ENOENT)?;
                Ok(format!("/{}", p.mm().exe))
            }
            Node::Fd(pid, n) => fd_target(pid, n),
            _ => Err(-EINVAL),
        }
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let num = name.parse::<u32>().ok();
        let node = match (self.node, name) {
            (Node::Root, "self") => Node::SelfLink,
            (Node::Root, "mounts") => Node::Mounts,
            (Node::Root, "uptime") => Node::Uptime,
            (Node::Root, "stat") => Node::StatAll,
            (Node::Root, "loadavg") => Node::Loadavg,
            (Node::Root, "version") => Node::Version,
            (Node::Root, "vmstat") => Node::Vmstat,
            (Node::Root, "diskstats") => Node::Diskstats,
            (Node::Root, "cmdline") => Node::KernelCmdline,
            (Node::Root, "cpuinfo") => Node::CpuInfo,
            (Node::Root, "meminfo") => Node::Meminfo,
            (Node::Root, "swaps") => Node::Swaps,
            (Node::Root, "threads") => Node::Threads,
            (Node::Root, "strace") => Node::Strace,
            (Node::Root, "bkl") => Node::Bkl,
            (Node::Root, "kmsg") => Node::Kmsg,
            (Node::Root, "sysstat") => Node::Sysstat,
            (Node::Root, "modules") => Node::Modules,
            (Node::Root, "net") => Node::NetDir,
            (Node::NetDir, "pnp") => Node::Pnp,
            (Node::NetDir, "route") => Node::Route,
            (Node::NetDir, "tcp") => Node::NetTcp,
            (Node::NetDir, "udp") => Node::NetUdp,
            (Node::NetDir, "sockstat") => Node::NetSockstat,
            (Node::NetDir, "snmp") => Node::NetSnmp,
            (Node::NetDir, "dev") => Node::NetDev,
            (Node::NetDir, n) if NET_STUBS.iter().any(|(s, _)| *s == n) => Node::NetStub(NET_STUBS.iter().position(|(s, _)| *s == n).unwrap() as u8),
            (Node::Root, "sys") => Node::SysDir(0, 0),
            (Node::SysDir(i, d), _) => match crate::sysctl::lookup(i as usize, d as usize, name).ok_or(-ENOENT)? {
                (j, true) => Node::Sys(j as u16),
                (j, false) => Node::SysDir(j as u16, d + 1),
            },
            (Node::Root, _) => Node::Pid(leader(num.ok_or(-ENOENT)?)?.tgid),
            (Node::Pid(p), "stat") => Node::Stat(p),
            (Node::Pid(p), "status") => Node::Status(p),
            (Node::Pid(p), "cmdline") => Node::Cmdline(p),
            (Node::Pid(p), "environ") => Node::Environ(p),
            (Node::Pid(p), "statm") => Node::Statm(p),
            (Node::Pid(p), "comm") => Node::Comm(p),
            (Node::Pid(p), "cwd") => Node::Cwd(p),
            (Node::Pid(p), "root") => Node::RootLink(p),
            (Node::Pid(p), "exe") => Node::Exe(p),
            (Node::Pid(p), "maps") => Node::Maps(p),
            (Node::Pid(p), "stack") => Node::Stack(p),
            (Node::Pid(p), "fd") => Node::FdDir(p),
            (Node::FdDir(p), _) => {
                let n = num.ok_or(-ENOENT)? as usize;
                leader(p)?.files().get(n as u64).ok_or(-ENOENT)?;
                Node::Fd(p, n)
            }
            _ => return Err(-ENOENT),
        };
        Ok(self.child(node))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let mut v = Vec::new();
        let mut add = |name: String, node: Node| {
            let c = ProcInode { fs: self.fs, node };
            v.push(DirEntry { name, ino: c.ino(), mode: c.meta().mode });
        };
        match self.node {
            Node::Root => {
                add("self".into(), Node::SelfLink);
                add("mounts".into(), Node::Mounts);
                add("uptime".into(), Node::Uptime);
                add("stat".into(), Node::StatAll);
                add("loadavg".into(), Node::Loadavg);
                add("version".into(), Node::Version);
                add("vmstat".into(), Node::Vmstat);
                add("diskstats".into(), Node::Diskstats);
                add("cmdline".into(), Node::KernelCmdline);
                add("cpuinfo".into(), Node::CpuInfo);
                add("meminfo".into(), Node::Meminfo);
                add("swaps".into(), Node::Swaps);
                add("threads".into(), Node::Threads);
                add("strace".into(), Node::Strace);
                add("bkl".into(), Node::Bkl);
                add("kmsg".into(), Node::Kmsg);
                add("sysstat".into(), Node::Sysstat);
                add("modules".into(), Node::Modules);
                add("net".into(), Node::NetDir);
                add("sys".into(), Node::SysDir(0, 0));
                for p in proc::all_leader_procs() {
                    add(format!("{}", p.tgid), Node::Pid(p.tgid));
                }
            }
            Node::NetDir => {
                add("pnp".into(), Node::Pnp);
                add("route".into(), Node::Route);
                add("tcp".into(), Node::NetTcp);
                add("udp".into(), Node::NetUdp);
                add("sockstat".into(), Node::NetSockstat);
                add("snmp".into(), Node::NetSnmp);
                add("dev".into(), Node::NetDev);
                for (i, (n, _)) in NET_STUBS.iter().enumerate() {
                    add((*n).into(), Node::NetStub(i as u8));
                }
            }
            Node::SysDir(i, d) => {
                for (name, j, file) in crate::sysctl::list(i as usize, d as usize) {
                    add(name.into(), if file { Node::Sys(j as u16) } else { Node::SysDir(j as u16, d + 1) });
                }
            }
            Node::Pid(p) => {
                add("stat".into(), Node::Stat(p));
                add("status".into(), Node::Status(p));
                add("cmdline".into(), Node::Cmdline(p));
                add("environ".into(), Node::Environ(p));
                add("statm".into(), Node::Statm(p));
                add("comm".into(), Node::Comm(p));
                add("cwd".into(), Node::Cwd(p));
                add("root".into(), Node::RootLink(p));
                add("exe".into(), Node::Exe(p));
                add("maps".into(), Node::Maps(p));
                add("stack".into(), Node::Stack(p));
                add("fd".into(), Node::FdDir(p));
            }
            Node::FdDir(p) => {
                let fds: Vec<usize> = leader(p)?.files().fds.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i).collect();
                for n in fds {
                    add(format!("{}", n), Node::Fd(p, n));
                }
            }
            _ => return Err(-ENOTDIR),
        }
        Ok(v)
    }

    fn create(&self, _: &str, _: u32, _: NewNode) -> Result<InodeRef, i64> {
        Err(-EACCES)
    }

    fn link(&self, _: &str, _: &InodeRef) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn unlink(&self, _: &str, _: bool) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn rename(&self, _: &str, _: &InodeRef, _: &str) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn set_mode(&self, _: u32) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_owner(&self, _: Option<u32>, _: Option<u32>) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mtime(&self, _: u64) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn statfs(&self) -> [u8; 120] {
        crate::tmpfs::statfs_bytes(PROC_SUPER_MAGIC, 4096, 0, 0, 0, 0)
    }
}
