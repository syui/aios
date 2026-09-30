// /proc: プロセスの様子を見せる小さなファイルシステム
//
//   /proc/self          -> 自分の PID
//   /proc/PID/stat      Linux と同じ並びの 1 行
//   /proc/PID/status    Name, State, Pid, Uid など
//   /proc/PID/cmdline   (いまは comm だけ)
//   /proc/PID/cwd       -> カレントディレクトリ
//   /proc/PID/fd/N      -> 開いているもの (ttyname はこれを読む)
//   /proc/mounts, /proc/uptime, /proc/meminfo
//   /proc/net/pnp       DHCP でもらった DNS (Linux の ip=dhcp と同じ形。/etc/resolv.conf はここへのリンク)
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
    NetDir,
    Pnp,
    Pid(u32),
    Stat(u32),
    Status(u32),
    Cmdline(u32),
    Cwd(u32),
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
            Node::Pid(p) => (p as u64) << 16 | 1,
            Node::Stat(p) => (p as u64) << 16 | 2,
            Node::Status(p) => (p as u64) << 16 | 3,
            Node::Cmdline(p) => (p as u64) << 16 | 4,
            Node::Cwd(p) => (p as u64) << 16 | 5,
            Node::FdDir(p) => (p as u64) << 16 | 6,
            Node::Fd(p, n) => (p as u64) << 16 | (0x100 + n as u64),
        }
    }

    fn pid(&self) -> Option<u32> {
        match self.node {
            Node::Pid(p) | Node::Stat(p) | Node::Status(p) | Node::Cmdline(p) | Node::Cwd(p) | Node::FdDir(p) | Node::Fd(p, _) => Some(p),
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
            Node::Uptime => {
                let t = crate::timer::ticks();
                format!("{}.{:02} 0.00\n", t / 100, t % 100)
            }
            Node::Meminfo => {
                use crate::memlayout::{PGSIZE, PHYSBASE, PHYSTOP};
                let total = (PHYSTOP - PHYSBASE) / 1024;
                let free = crate::kalloc::nfree() * PGSIZE / 1024;
                format!("MemTotal:     {:8} kB\nMemFree:      {:8} kB\nMemAvailable: {:8} kB\n", total, free, free)
            }
            Node::Pnp => {
                let mut s = String::from("#PROTO: DHCP\n");
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
            Node::Status(pid) => {
                let p = leader(pid)?;
                let c = &p.cred;
                let threads = proc::threads_of(p.tgid).len();
                format!(
                    "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nThreads:\t{}\n",
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
                    threads
                )
            }
            Node::Cmdline(pid) => {
                let mut s = leader(pid)?.comm().to_string();
                s.push('\0');
                s
            }
            _ => return Err(-EISDIR),
        })
    }
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
fn stat_line(p: &Proc) -> String {
    let (tty_nr, tpgid) = crate::tty::of_session(p.sid).map_or((0, -1), |(rdev, pg)| (rdev, pg as i64));
    let threads = proc::threads_of(p.tgid).len();
    let mut s = format!(
        "{} ({}) {} {} {} {} {} {} 0 0 0 0 0 {} 0 {} 0 20 0 {} 0 0 0 0",
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
        threads
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

impl Inode for ProcInode {
    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let (uid, gid) = self.pid().and_then(|p| leader(p).ok()).map_or((0, 0), |p| (p.cred.euid, p.cred.egid));
        let mode = match self.node {
            Node::Root | Node::Pid(_) | Node::NetDir => S_IFDIR | 0o555,
            Node::FdDir(_) => S_IFDIR | 0o500,
            Node::SelfLink | Node::Cwd(_) => S_IFLNK | 0o777,
            Node::Fd(..) => S_IFLNK | 0o700,
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

    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize, i64> {
        Err(-EACCES)
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        match self.node {
            Node::SelfLink => Ok(format!("{}", proc::current().tgid)),
            Node::Cwd(pid) => Ok(format!("/{}", leader(pid)?.files().cwd)),
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
            (Node::Root, "meminfo") => Node::Meminfo,
            (Node::Root, "net") => Node::NetDir,
            (Node::NetDir, "pnp") => Node::Pnp,
            (Node::Root, _) => Node::Pid(leader(num.ok_or(-ENOENT)?)?.tgid),
            (Node::Pid(p), "stat") => Node::Stat(p),
            (Node::Pid(p), "status") => Node::Status(p),
            (Node::Pid(p), "cmdline") => Node::Cmdline(p),
            (Node::Pid(p), "cwd") => Node::Cwd(p),
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
                add("meminfo".into(), Node::Meminfo);
                add("net".into(), Node::NetDir);
                for p in proc::all_leader_procs() {
                    add(format!("{}", p.tgid), Node::Pid(p.tgid));
                }
            }
            Node::NetDir => add("pnp".into(), Node::Pnp),
            Node::Pid(p) => {
                add("stat".into(), Node::Stat(p));
                add("status".into(), Node::Status(p));
                add("cmdline".into(), Node::Cmdline(p));
                add("cwd".into(), Node::Cwd(p));
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
