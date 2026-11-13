// /proc: プロセスの様子を見せる小さなファイルシステム
//
//   /proc/self          -> 自分の PID
//   /proc/PID/stat      Linux と同じ並びの 1 行
//   /proc/PID/status    Name, State, Pid, Uid など
//   /proc/PID/cmdline   引数 (NUL で区切る。プロセスのメモリの argv の文字列から)
//   /proc/PID/environ   環境 (同じく。自分のものか root だけ)
//   /proc/PID/statm     メモリ (ページ): size resident shared text lib data dt
//   /proc/PID/smaps     maps の行ごとに、ページの数 (Rss、Pss、Shared / Private の Clean / Dirty、Swap など: vm.rs の smaps_text)
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
    /// /proc/net/unix (AF_UNIX のソケット: unix.rs)
    NetUnix,
    /// /proc/net/dev (インターフェースごとの送り受けの数。psutil や ifconfig が読む)
    NetDev,
    /// /proc/net/nf_conntrack (conntrack のつながり: conntrack.rs。root だけ)
    NetConntrack,
    /// まだ中身のない /proc/net のファイル (見出しだけ。netstat が読む): NET_STUBS の番号
    NetStub(u8),
    KernelCmdline,
    CpuInfo,
    StatAll,
    Loadavg,
    Version,
    Vmstat,
    /// /proc/buddyinfo (物理ページの空きの、大きさごとのかたまりの数: kalloc.rs)
    Buddyinfo,
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
    Smaps(u32),
    Stack(u32),
    FdDir(u32),
    Fd(u32, usize),
    /// /proc/PID/fdinfo と fdinfo/N (pos、flags、mnt_id、ino と、ものごとの行: file::OpenFile::fdinfo)
    FdInfoDir(u32),
    FdInfo(u32, usize),
    /// /proc/PID/ns と、その中のリンク (NS_KINDS の番号)
    NsDir(u32),
    NsLink(u32, u8),
    /// /proc/PID/mounts (そのプロセスのマウントの namespace の表)
    PidMounts(u32),
    /// /proc/PID/mountinfo (番号と親、major:minor、根: vfs::mountinfo_text_of)
    MountInfo(u32),
    /// /proc/PID/uid_map、gid_map、setgroups (ユーザーの namespace の地図。0 uid、1 gid、2 setgroups)
    IdMap(u32, u8),
    /// /proc/PID/task (スレッドの一覧)、task/TID、その中の stat / status / comm (0 1 2)。番号は本当のもの
    TaskDir(u32),
    Task(u32, u32),
    TaskFile(u32, u32, u8),
    /// /proc/PID/loginuid と sessionid (ログインした人とそのセッション: audit.rs)
    LoginUid(u32),
    SessionId(u32),
    /// /proc/PID/attr (true なら attr/apparmor) と、その中の current exec prev (ATTRS の番号。apparmor.rs)
    AttrDir(u32, bool),
    Attr(u32, bool, u8),
    /// /proc/ai (aiproc.rs): ディレクトリ、threads、bkl、ctl、fd/ と fd/PID
    AiDir,
    AiThreads,
    AiBkl,
    AiHeap,
    AiProf,
    AiCtl,
    /// /proc/ai/kernel: kernel/kernel.json (ファイルごとの core と extra、quirk の一覧)
    AiKernel,
    AiMem,
    AiFs,
    AiTrace,
    AiParts,
    AiFail,
    AiFdDir,
    AiFd(u32),
    /// /proc/ai/why: しくじったシステムコールのわけ (whylog.rs)
    AiWhy,
    /// /proc/ai/fw (ファイアウォールの様子: nftables::ai_json)
    AiFw,
    /// /proc/ai/stack と stack/TID (スレッドの番号)
    AiStackDir,
    AiStack(u32),
    /// /proc/sysvipc と、その中の shm (System V の共有メモリの一覧: shm.rs。ipcs が読む)
    SysvipcDir,
    SysvipcShm,
}

pub struct ProcInode {
    fs: usize,
    node: Node,
}

pub fn new_root() -> InodeRef {
    Rc::new(ProcInode { fs: new_fs_id(), node: Node::Root })
}

/// /proc/PID/attr の中 (LSM の属性: AppArmor が使うもの)
const ATTRS: [&str; 3] = ["current", "exec", "prev"];

/// IPv6 と raw のもの: tcp6、udp6、sockstat6、if_inet6 は中身を作る (inet.rs の AF_INET6)。raw はまだ見出しだけ
/// (なければ netstat がないと言って止まる)
const NET_STUBS: [(&str, &str); 8] = [
    ("sockstat6", "TCP6: inuse 0\nUDP6: inuse 0\nUDPLITE6: inuse 0\nRAW6: inuse 0\nFRAG6: inuse 0 memory 0\n"),
    ("tcp6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"),
    ("udp6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    ("raw", "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    ("raw6", "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n"),
    // 口の IPv6 の番地 (addrconf の if6_seq_show: 番地、口の番号、prefix の長さ、scope、flags、名前) と IPv6 の道
    // (netlink.rs が作る)
    ("if_inet6", ""),
    ("ipv6_route", ""),
    // IPv4 の近所 (arp_seq_show: netlink.rs が作る)
    ("arp", ""),
];

/// プロセス pid のスレッド tid (終わったものはなし)
fn thread(pid: u32, tid: u32) -> Result<&'static mut Proc, i64> {
    proc::threads_of(pid).into_iter().find(|t| t.pid == tid).ok_or(-ENOENT)
}

fn leader(pid: u32) -> Result<&'static mut Proc, i64> {
    proc::find_leader(pid).filter(|p| p.state != State::Zombie).ok_or(-ENOENT)
}

/// kstrtou32 (10 進): 頭の + と、終わりの改行 1 つはよい。数字がない・ほかの文字は EINVAL、大きすぎれば ERANGE
fn kstrtou32(b: &[u8]) -> Result<u32, i64> {
    const ERANGE: i64 = 34;
    let b = b.strip_prefix(b"+").unwrap_or(b);
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut v: u64 = 0;
    let mut over = false;
    for &c in &b[..n] {
        match v.checked_mul(10).and_then(|x| x.checked_add((c - b'0') as u64)) {
            Some(x) => v = x,
            None => over = true,
        }
    }
    if over {
        return Err(-ERANGE);
    }
    let rest = &b[n..];
    let rest = rest.strip_prefix(b"\n").unwrap_or(rest);
    if n == 0 || !rest.is_empty() {
        return Err(crate::why::inval());
    }
    u32::try_from(v).map_err(|_| -ERANGE)
}

/// leader と同じだが、終わってまだ回収されていないもの (ゾンビ) も: /proc/PID と stat、status、comm はある (Linux と同じ)
fn leader_any(pid: u32) -> Result<&'static mut Proc, i64> {
    proc::find_leader_any(pid).ok_or(-ENOENT)
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
            Node::Buddyinfo => 24,
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
            Node::NetUnix => 23,
            Node::NetDev => 39,
            Node::NetConntrack => 40,
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
            Node::Smaps(p) => (p as u64) << 16 | 29,
            Node::Stack(p) => (p as u64) << 16 | 9,
            Node::FdDir(p) => (p as u64) << 16 | 6,
            Node::Fd(p, n) => (p as u64) << 16 | (0x100 + n as u64),
            Node::FdInfoDir(p) => (p as u64) << 16 | 25,
            Node::FdInfo(p, n) => 1 << 63 | (p as u64) << 16 | (0x100 + n as u64),
            Node::NsDir(p) => (p as u64) << 16 | 14,
            Node::PidMounts(p) => (p as u64) << 16 | 15,
            Node::MountInfo(p) => (p as u64) << 16 | 26,
            Node::LoginUid(p) => (p as u64) << 16 | 27,
            Node::SessionId(p) => (p as u64) << 16 | 28,
            Node::IdMap(p, k) => (p as u64) << 16 | (16 + k as u64),
            Node::TaskDir(p) => (p as u64) << 16 | 20,
            Node::Task(_, t) => (t as u64) << 16 | 21,
            Node::TaskFile(_, t, k) => (t as u64) << 16 | (22 + k as u64),
            Node::AttrDir(p, a) => (p as u64) << 16 | (0x30 + a as u64),
            Node::Attr(p, a, k) => (p as u64) << 16 | (0x32 + 4 * a as u64 + k as u64),
            Node::NsLink(p, k) => (p as u64) << 16 | (0x40 + k as u64),
            Node::AiDir => 40,
            Node::AiThreads => 41,
            Node::AiBkl => 42,
            Node::AiHeap => 46,
            Node::AiProf => 47,
            Node::AiCtl => 43,
            Node::AiKernel => 48,
            Node::AiMem => 49,
            Node::AiFs => 50,
            Node::AiTrace => 51,
            Node::AiParts => 52,
            Node::AiFail => 53,
            Node::AiFdDir => 44,
            Node::AiFd(p) => (p as u64) << 16 | 30,
            Node::AiWhy => 54,
            Node::AiFw => 55,
            Node::AiStackDir => 45,
            Node::AiStack(t) => (t as u64) << 16 | 31,
            Node::SysvipcDir => 56,
            Node::SysvipcShm => 57,
        }
    }

    fn pid(&self) -> Option<u32> {
        match self.node {
            Node::Pid(p) | Node::Stat(p) | Node::Status(p) | Node::Cmdline(p) | Node::Environ(p) | Node::Statm(p) | Node::Comm(p) | Node::Cwd(p) | Node::RootLink(p) | Node::Exe(p) | Node::Maps(p) | Node::Smaps(p) | Node::Stack(p) | Node::FdDir(p) | Node::Fd(p, _) | Node::FdInfoDir(p) | Node::FdInfo(p, _) | Node::NsDir(p) | Node::NsLink(p, _) | Node::PidMounts(p) | Node::MountInfo(p) | Node::IdMap(p, _) | Node::TaskDir(p) | Node::Task(p, _) | Node::TaskFile(p, _, _) | Node::AttrDir(p, _) | Node::Attr(p, ..) | Node::LoginUid(p) | Node::SessionId(p) => Some(p),
            _ => None,
        }
    }

    /// ファイルの中身
    fn content(&self) -> Result<String, i64> {
        Ok(match self.node {
            Node::Mounts => crate::vfs::mounts_text(),
            Node::KernelCmdline => format!("{}\n", crate::dtb::bootargs().unwrap_or("")),
            Node::CpuInfo => {
                let midr: u64;
                unsafe { core::arch::asm!("mrs {}, midr_el1", out(reg) midr) };
                let mut s = String::new();
                let features = crate::exec::cpu_features();
                for i in 0..crate::smp::online() {
                    s += &format!(
                        "processor\t: {}\nBogoMIPS\t: {}.00\nFeatures\t: {}\nCPU implementer\t: {:#04x}\nCPU architecture: 8\nCPU variant\t: {:#x}\nCPU part\t: {:#05x}\nCPU revision\t: {}\n\n",
                        i,
                        crate::timer::freq() * 2 / 1_000_000,
                        features,
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
                    "MemTotal:     {:8} kB\nMemFree:      {:8} kB\nMemAvailable: {:8} kB\nBuffers:      {:8} kB\nCached:       {:8} kB\nSwapCached:   {:8} kB\nActive:       {:8} kB\nInactive:     {:8} kB\nShmem:        {:8} kB\nSlab:         {:8} kB\nSReclaimable: {:8} kB\nSwapTotal:    {:8} kB\nSwapFree:     {:8} kB\nDirty:        {:8} kB\nPageTables:   {:8} kB\n",
                    total,
                    free,
                    free + crate::vm::cached_pages() * PGSIZE / 1024,
                    0,
                    crate::vm::cached_pages() * PGSIZE / 1024,
                    0,
                    total - free,
                    0,
                    crate::vm::shared_pages() * PGSIZE / 1024,
                    0,
                    0,
                    st * PGSIZE / 1024,
                    sf * PGSIZE / 1024,
                    0,
                    crate::vm::table_pages() * PGSIZE / 1024
                )
            }
            Node::Swaps => crate::swap::proc_swaps(),
            Node::Threads => crate::sysrq::threads_text(),
            Node::AiThreads | Node::AiBkl | Node::AiHeap | Node::AiProf | Node::AiCtl | Node::AiMem | Node::AiFs | Node::AiTrace | Node::AiFail | Node::AiFw | Node::AiFd(_) | Node::AiStack(_) if !crate::aiproc::allowed() => return Err(-EACCES),
            Node::AiStack(t) => crate::aiproc::ai_stack(t).ok_or(-ENOENT)?,
            Node::AiThreads => crate::aiproc::ai_threads(),
            Node::AiBkl => crate::lockstat::json(),
            Node::AiHeap => crate::aiproc::heap_json(),
            Node::AiProf => crate::prof::json(),
            Node::AiCtl => crate::aiproc::CTL_HELP.into(),
            Node::AiKernel => include_str!("../kernel.json").into(),
            Node::AiMem => crate::aiproc::mem_json(),
            Node::AiFs => crate::aiproc::fs_json(),
            Node::AiTrace => crate::trace::json(),
            Node::AiParts => crate::parts::json(),
            Node::AiFail => crate::fail::json(),
            Node::AiFw => crate::nftables::ai_json(),
            Node::AiFd(p) => crate::aiproc::fds_json(p)?,
            Node::AiWhy => crate::whylog::text(if crate::aiproc::allowed() { None } else { Some(crate::cred::current().uid) }),
            Node::Strace => crate::trace::strace_get(),
            Node::Bkl => crate::lockstat::text(),
            Node::Kmsg => crate::kmsg::text(),
            Node::Sysstat => crate::trace::sysstat(),
            Node::Modules => crate::module::proc_modules(),
            Node::Route => crate::netif::proc_route(),
            Node::NetDev => {
                let mut s = String::from("Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n");
                // 分けた NET の中は lo だけ (数は 0 から: aios の lo の数は NET ごとにない)
                let netns = crate::ns::net().is_some();
                for (name, lo) in [("lo", true), ("eth0", false)] {
                    if netns && !lo {
                        continue;
                    }
                    let c = if netns { [0; 4] } else { crate::net::stats(lo) };
                    s += &format!("{:>6}: {:>8} {:>7}    0    0    0     0          0         0 {:>8} {:>7}    0    0    0     0       0          0\n", name, c[0], c[1], c[2], c[3]);
                }
                s
            }
            Node::NetConntrack => crate::conntrack::proc_text(),
            Node::NetTcp => crate::socket::proc_net(true),
            Node::NetUdp => crate::socket::proc_net(false),
            Node::NetSockstat => crate::socket::proc_sockstat(),
            Node::NetSnmp => crate::socket::proc_snmp(),
            Node::NetUnix => crate::unix::proc_net(),
            Node::NetStub(i) => match NET_STUBS[i as usize].0 {
                "tcp6" => crate::inet::proc_net6(true),
                "udp6" => crate::inet::proc_net6(false),
                "if_inet6" => crate::netlink::proc_if_inet6(),
                "ipv6_route" => crate::netlink::proc_ipv6_route(),
                "arp" => crate::netlink::proc_arp(),
                "sockstat6" => {
                    let c = crate::inet::counts();
                    alloc::format!("TCP6: inuse {}\nUDP6: inuse {}\nUDPLITE6: inuse 0\nRAW6: inuse 0\nFRAG6: inuse 0 memory 0\n", c.inuse6, c.udp6)
                }
                _ => NET_STUBS[i as usize].1.into(),
            },
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
            Node::Stat(pid) => stat_line(leader_any(pid)?, true),
            Node::Maps(pid) => {
                let pt = &leader(pid)?.mm().pt;
                let _h = pt.hold();
                pt.maps_text()
            }
            Node::Smaps(pid) => {
                let pt = &leader(pid)?.mm().pt;
                let _h = pt.hold();
                pt.smaps_text()
            }
            Node::Stack(pid) => {
                aa_ptrace(pid, false)?;
                stacks_text(leader(pid)?.tgid)
            }
            Node::Status(pid) => {
                let p = leader_any(pid)?;
                let c = &p.cred;
                let threads = proc::threads_of(p.tgid).len();
                let (vsize, rss) = mem_of(p);
                format!(
                    "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\nThreads:\t{}\nNoNewPrivs:\t{}\nLandlock:\t{}\nSeccomp:\t{}\nSeccomp_filters:\t{}\n{}",
                    p.comm(),
                    state_name(p),
                    crate::ns::local_or_0(p.tgid),
                    crate::ns::local_or_0(p.pid),
                    crate::ns::local_or_0(p.ppid),
                    crate::ns::show_uid(c.uid),
                    crate::ns::show_uid(c.euid),
                    crate::ns::show_uid(c.suid),
                    crate::ns::show_uid(c.fsuid),
                    crate::ns::show_gid(c.gid),
                    crate::ns::show_gid(c.egid),
                    crate::ns::show_gid(c.sgid),
                    crate::ns::show_gid(c.fsgid),
                    vsize / 1024,
                    rss * 4,
                    threads,
                    c.no_new_privs as u8,
                    crate::landlock::layers(c),
                    crate::seccomp::mode(c).0,
                    crate::seccomp::mode(c).1,
                    spec_status(p.spec_ssb)
                )
            }
            Node::Cmdline(pid) => {
                let p = leader(pid)?;
                let (a, e, _) = p.mm.as_ref().map_or((0, 0, 0), |m| m.get().args);
                match user_bytes(p, a, e) {
                    Some(b) if !b.is_empty() => String::from_utf8_lossy(&b).into_owned(),
                    // カーネルのスレッド (proc::kthread): Linux と同じく空 (ps はこれで [flush] と出す)
                    _ if p.kthread => String::new(),
                    // 読めないとき: comm を出す
                    _ => format!("{}\0", p.comm()),
                }
            }
            // 見るのは ptrace で READ できる人だけ (開くときに: permission)
            Node::FdInfo(pid, n) => {
                let p = leader(pid)?;
                let (f, cloexec) = p.files().get_other(n as u64).ok_or(-ENOENT)?;
                let s = f.borrow().fdinfo(cloexec);
                s
            }
            // 開くときに mm_access (permission)
            Node::Environ(pid) => {
                let p = leader(pid)?;
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
            Node::Buddyinfo => buddyinfo(),
            Node::SysvipcShm => crate::shm::proc_text(),
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
            Node::Version => match crate::ns::uname() {
                crate::ns::Uname::Aios => format!("aios version {} (rustc) #1 SMP\n", crate::ns::release()),
                crate::ns::Uname::Linux => format!("Linux version {} (rustc) {}\n", crate::ns::release(), crate::ns::version()),
            },
            Node::Comm(pid) => format!("{}\n", leader_any(pid)?.comm()),
            // 改行なし (Linux の proc_loginuid_read と proc_sessionid_read)
            Node::LoginUid(pid) => format!("{}", crate::ns::show_uid_raw(leader(pid)?.cred.loginuid)),
            Node::SessionId(pid) => format!("{}", leader(pid)?.cred.sessionid),
            Node::PidMounts(pid) => crate::vfs::mounts_text_of(leader(pid)?.cred.ns.mnt.clone()),
            // そのプロセスの namespace の表を、そのプロセスの chroot の根から
            Node::MountInfo(pid) => {
                let p = leader(pid)?;
                let root = p.files.as_ref().map(|f| f.get().root.trim_start_matches('/').to_string()).unwrap_or_default();
                crate::vfs::mountinfo_text_of(p.cred.ns.mnt.clone(), &root)
            }
            Node::TaskFile(pid, tid, k) => {
                let t = thread(pid, tid)?;
                match k {
                    // 1 つ目の欄はスレッドの番号
                    0 => {
                        let line = stat_line(t, false);
                        format!("{}{}", crate::ns::local_or_0(tid), &line[line.find(' ').unwrap_or(0)..])
                    }
                    1 => format!(
                        "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\n",
                        t.comm(),
                        state_name(t),
                        crate::ns::local_or_0(t.tgid),
                        crate::ns::local_or_0(tid),
                        crate::ns::local_or_0(t.ppid)
                    ),
                    _ => format!("{}\n", t.comm()),
                }
            }
            Node::IdMap(pid, 2) => crate::ns::setgroups_text(&leader(pid)?.cred.ns.user).into(),
            Node::Attr(pid, _, k) => crate::apparmor::attr_read(&leader(pid)?.cred, ATTRS[k as usize])?,
            Node::IdMap(pid, k) => crate::ns::map_text(&leader(pid)?.cred.ns.user, k == 1),
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
    let pt = &mut p.mm().pt;
    let _h = pt.hold();
    pt.copy_in(&mut b, start)?;
    Some(b)
}

/// /proc/stat: CPU ごとの時間 (USER_HZ の tick: user、system、idle。スレッドが走った時間と、そのうちカーネルにいた
/// 時間を ns で数えたもの: proc::RUN_NS と SYS_NS)、起動した時刻、作ったプロセスの数など
fn stat_all() -> String {
    let now = crate::timer::ticks();
    let tick = 1_000_000_000 / crate::timer::HZ;
    let n = crate::smp::online();
    // (user, system, idle)
    let cpus: Vec<(u64, u64, u64)> = (0..n)
        .map(|c| {
            let (run, sys) = proc::cpu_times(c);
            let run = (run / tick).min(now);
            let sys = (sys / tick).min(run);
            (run - sys, sys, now - run)
        })
        .collect();
    let line = |name: &str, (u, sy, idle): (u64, u64, u64)| format!("{} {} 0 {} {} 0 0 0 0 0 0\n", name, u, sy, idle);
    let sum = cpus.iter().fold((0, 0, 0), |a, c| (a.0 + c.0, a.1 + c.1, a.2 + c.2));
    let mut s = line("cpu", sum);
    for (c, t) in cpus.iter().enumerate() {
        s += &line(&format!("cpu{}", c), *t);
    }
    let boot = (crate::timer::epoch_ns() / 1_000_000_000).saturating_sub(now / crate::timer::HZ);
    // 割りこみ: みなの数と、番号 (GIC の INTID) ごとの数 (Linux は irq の番号ごと。最後に来た番号まで)
    let irqs: Vec<u64> = crate::irq::COUNTS.iter().map(|c| c.load(core::sync::atomic::Ordering::Relaxed)).collect();
    let last = irqs.iter().rposition(|&n| n != 0).map_or(0, |i| i + 1);
    let mut intr = format!("intr {}", irqs.iter().sum::<u64>());
    for n in &irqs[..last] {
        intr += &format!(" {}", n);
    }
    // ディスクを待って眠っているもの (ps の D: sleep_nointr)
    let blocked = proc::nr_blocked();
    s += &format!(
        "{}\nctxt {}\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked {}\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
        intr,
        crate::smp::SWITCHES.load(core::sync::atomic::Ordering::Relaxed),
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
        State::Sleeping if p.nointr => 'D',
        State::Sleeping => 'S',
        State::Zombie => 'Z',
        State::Unused => 'X',
    }
}

fn state_name(p: &Proc) -> &'static str {
    match state_char(p) {
        'R' => "R (running)",
        'S' => "S (sleeping)",
        'D' => "D (disk sleep)",
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
            let _h = m.pt.hold();
            (m.pt.vsize(), m.pt.resident())
        }
        None => (0, 0),
    }
}

/// /proc/PID/stat の 1 行 (whole ならスレッドグループの時間、でなければそのスレッドの: task/TID/stat)。utime と stime は
/// ユーザーとカーネルで走った時間、cutime と cstime は回収した子の (tick = 1/100 秒)
fn stat_line(p: &Proc, whole: bool) -> String {
    let (tty_nr, tpgid) = crate::tty::of_session(p.sid).map_or((0, -1), |(rdev, pg)| (rdev, pg as i64));
    let threads = proc::threads_of(p.tgid).len();
    let (u, sy) = proc::user_sys_ns(p, !whole);
    let (cu, cs) = crate::syscall::children_user_sys(p.tgid);
    let tick = crate::timer::TICK_NS;
    let mut s = format!(
        "{} ({}) {} {} {} {} {} {} {} 0 0 0 0 {} {} {} {} 20 0 {} 0 {} {} {}",
        crate::ns::local_or_0(p.tgid),
        p.comm(),
        state_char(p),
        crate::ns::local_or_0(p.ppid),
        crate::ns::local_or_0(p.pgid),
        crate::ns::local_or_0(p.sid),
        tty_nr,
        tpgid,
        // flags: カーネルのスレッドは PF_KTHREAD (ps や top がこれで見分ける)
        if p.kthread { 0x0020_0000 } else { 0 },
        u / tick,
        sy / tick,
        cu / tick,
        cs / tick,
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
    let (f, _) = p.files().get_other(n as u64).ok_or(-ENOENT)?;
    let f = f.borrow();
    Ok(f.describe())
}

/// /proc/PID/fd/N (と /dev/stdin のような、そこへのリンク) が指す、開いているもの (OpenFile)。
/// /proc/PID/ns のリンク (aios が分けられるもの。ほかははじめからあるものだけ)
const NS_KINDS: [&str; 6] = ["mnt", "net", "pid", "pid_for_children", "user", "uts"];

fn ns_id(c: &crate::cred::Cred, kind: &str) -> u64 {
    match kind {
        "mnt" => c.ns.mnt_id(),
        "user" => c.ns.user_id(),
        "net" => c.ns.net_id(),
        "pid_for_children" => c.ns.pid_children_id(),
        _ => c.ns.uts_id(),
    }
}

/// Linux の __ptrace_may_access (PTRACE_MODE_FSCREDS: proc_fd_access_allowed、mm_access、ns の口、fdinfo、lock_trace):
/// 自分のプロセスはよい。ほかは、自分の fs の id (fsuid / fsgid) が相手の uid・euid・suid と gid・egid・sgid に
/// みな同じで、相手が dumpable (1) か、でなければ CAP_SYS_PTRACE。それから landlock と AppArmor の ptrace の決まり
/// (read なら READ、でなければ ATTACH)。だめなら Err (呼ぶ側が EACCES か EPERM に)
fn ptrace_may(pid: u32, read: bool) -> Result<(), ()> {
    let p = leader(pid).map_err(|_| ())?;
    if p.tgid == proc::current().tgid {
        return Ok(());
    }
    let (c, tc) = (crate::cred::current(), &p.cred);
    let ids = c.fsuid == tc.uid && c.fsuid == tc.euid && c.fsuid == tc.suid && c.fsgid == tc.gid && c.fsgid == tc.egid && c.fsgid == tc.sgid;
    if !ids && !c.capable(crate::cred::CAP_SYS_PTRACE) {
        return Err(());
    }
    if crate::pidfd::dumpable(p) != 1 && !c.capable(crate::cred::CAP_SYS_PTRACE) {
        return Err(());
    }
    crate::landlock::check_ptrace(c.sandbox.as_ref(), tc.sandbox.as_ref()).map_err(|_| ())?;
    crate::apparmor::may_ptrace(tc, read).map_err(|_| ())
}

/// ptrace_may の答えを errno に: read は EACCES、ATTACH (lock_trace) は EPERM
fn aa_ptrace(pid: u32, read: bool) -> Result<(), i64> {
    leader(pid)?;
    ptrace_may(pid, read).map_err(|_| if read { -EACCES } else { -EPERM })
}

/// mm_access (environ と maps を開く: Linux の may_access_mm): 自分か、ptrace_may_access (READ) か、perfmon_capable
/// (CAP_PERFMON か CAP_SYS_ADMIN)。だめなら EACCES
fn mm_access(pid: u32) -> Result<(), i64> {
    use crate::cred::{CAP_PERFMON, CAP_SYS_ADMIN};
    leader(pid)?;
    if ptrace_may(pid, true).is_ok() {
        return Ok(());
    }
    let c = crate::cred::current();
    if c.capable(CAP_PERFMON) || c.capable(CAP_SYS_ADMIN) {
        return Ok(());
    }
    Err(-EACCES)
}

/// /dev/stdin や /proc/self/fd/N の名前のないもの (パイプ、ソケット、端末) は、開きなおさずにそのもの
/// [quirk tty-root-fdlink]。ファイルの名前があるもの (Kind::Inode) は magic_link でふつうに開くので、ここでは None
pub fn fd_link_file(cwd: &str, path: &str) -> Option<crate::file::FileRef> {
    let (mut dir, mut p) = (String::from(cwd), String::from(path));
    for _ in 0..8 {
        let (full, ino) = crate::vfs::lookup(&dir, &p, false).ok()?;
        if ino.meta().mode & crate::vfs::S_IFMT != crate::vfs::S_IFLNK {
            return None;
        }
        if let Some(pi) = ino.as_any().downcast_ref::<ProcInode>() {
            let Node::Fd(pid, n) = pi.node else { return None };
            aa_ptrace(pid, true).ok()?;
            let (f, _) = leader(pid).ok()?.files().get_other(n as u64)?;
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

    fn permission(&self, m: &Meta, want: u32) -> Option<Result<(), i64>> {
        let c = crate::cred::current();
        match self.node {
            // /proc/PID/fd: mode で入れなくても、自分のプロセスなら (Linux の proc_fd_permission: dumpable でなくなって
            // root のものになっても、自分の fd は見られる)
            Node::FdDir(pid) => Some(if leader(pid).is_ok_and(|p| p.tgid == proc::current().tgid) { Ok(()) } else { c.check(m, want) }),
            // /proc/PID/fdinfo: ptrace で見られる人だけ、それから mode で (Linux の proc_fdinfo_permission)
            Node::FdInfoDir(pid) => Some(aa_ptrace(pid, true).and_then(|_| c.check(m, want))),
            // 開くときに mode と、ptrace で読めるか (Linux は open で見る: proc_maps_open と environ_open の mm_access、
            // seq_fdinfo_open。開いたあとで相手が変わっても、読むのはそのまま)
            Node::Maps(pid) | Node::Smaps(pid) | Node::Environ(pid) => Some(c.check(m, want).and_then(|_| mm_access(pid))),
            Node::FdInfo(pid, _) => Some(c.check(m, want).and_then(|_| aa_ptrace(pid, true))),
            _ => None,
        }
    }

    fn magic_link(&self) -> Option<(String, InodeRef)> {
        let Node::Fd(pid, n) = self.node else { return None };
        // AppArmor で見られなければ、ふつうのリンクとして (readlink が EACCES にする)
        aa_ptrace(pid, true).ok()?;
        let (f, _) = leader(pid).ok()?.files().get_other(n as u64)?;
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
        let mode = match self.node {
            Node::Root | Node::Pid(_) | Node::NetDir | Node::SysvipcDir | Node::SysDir(..) | Node::AiDir | Node::AiFdDir | Node::AiStackDir => S_IFDIR | 0o555,
            Node::AiCtl => S_IFREG | 0o664,
            Node::Sys(i) if crate::sysctl::TABLE[i as usize].writable() => S_IFREG | 0o644,
            Node::FdDir(_) => S_IFDIR | 0o500,
            Node::FdInfoDir(_) => S_IFDIR | 0o555,
            Node::NsDir(_) => S_IFDIR | 0o511,
            Node::TaskDir(_) | Node::Task(..) | Node::AttrDir(..) => S_IFDIR | 0o555,
            // だれでも開けるが、書けるのは自分のものだけ (write_at)。prev は読むだけ
            Node::Attr(_, _, k) if ATTRS[k as usize] != "prev" => S_IFREG | 0o666,
            Node::NsLink(..) => S_IFLNK | 0o777,
            Node::SelfLink | Node::Cwd(_) | Node::RootLink(_) | Node::Exe(_) => S_IFLNK | 0o777,
            // 開いた口しだい: 読めれば r-x、書ければ -wx (Linux の tid_fd_update_inode)
            Node::Fd(pid, n) => {
                let f = leader(pid).ok().and_then(|p| p.files().get_other(n as u64)).map(|(f, _)| f);
                // いま使っているもの (/proc/self/fd を getdents している、その fd など) は借りられない: 読む口として
                let (r, w) = f.map_or((false, false), |f| f.try_borrow().map_or((true, false), |f| (f.readable(), f.writable())));
                S_IFLNK | (if r { 0o500 } else { 0 }) | (if w { 0o300 } else { 0 })
            }
            Node::Strace | Node::Bkl | Node::IdMap(..) | Node::LoginUid(_) | Node::Comm(_) => S_IFREG | 0o644,
            Node::TaskFile(_, _, 2) => S_IFREG | 0o644,
            // 持ち主だけが読む (Linux の S_IRUSR のもの)
            Node::Environ(_) | Node::Stack(_) => S_IFREG | 0o400,
            Node::NetConntrack => S_IFREG | 0o440,
            _ => S_IFREG | 0o444,
        };
        // 持ち主: そのプロセスの euid と egid。dumpable が 1 でなければ root のもの (dr-xr-xr-x のディレクトリはのぞく:
        // Linux の task_dump_owner。uid や gid を変えたプロセスの loginuid や environ などを、もとの人が開けないように)
        let (uid, gid) = match self.node {
            // root と wheel の人が書ける (aiproc::allowed。書くときにも確かめる)。aibox の中の aish (sudo が効かない) も -w で
            Node::AiCtl => (0, crate::aiproc::WHEEL),
            // 自分の UTS の namespace の中から見ると、その namespace の root のもの (Linux が net の sysctl を、netns の
            // ユーザーの namespace の root のものに見せるのと同じ: set_ownership)
            Node::Sys(i) if crate::sysctl::TABLE[i as usize].uts_scoped() => crate::ns::uts_owner(),
            _ => match self.pid().and_then(|p| leader(p).ok()) {
                Some(p) if mode == S_IFDIR | 0o555 || crate::pidfd::dumpable(p) == 1 => (p.cred.euid, p.cred.egid),
                _ => (0, 0),
            },
        };
        let now = crate::timer::epoch_ns();
        // /proc/PID/fd/N のリンクは大きさ 64 (Linux の proc_fd_instantiate。libapparmor の path_from_fd などは lstat の
        // 大きさから readlink を始めて、足りなければ倍にする: 0 だと 0 のまま回りつづける)
        let size = if matches!(self.node, Node::Fd(..)) { 64 } else { 0 };
        Meta { ino: self.ino(), mode, nlink: 1, uid, gid, size, rdev: 0, blocks: 0, mtime: now, ctime: now }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        // /proc/ai/trace は大きい (8192 の出来事): 読みはじめで 1 度作って、続きはそれを切って返す
        if self.node == Node::AiTrace {
            if !crate::aiproc::allowed() {
                return Err(-EACCES);
            }
            return Ok(crate::trace::read_at(off, buf));
        }
        // /proc/ai/why も大きい: 同じように
        if self.node == Node::AiWhy {
            return Ok(crate::whylog::read_at(off, buf, if crate::aiproc::allowed() { None } else { Some(crate::cred::current().uid) }));
        }
        let s = self.content()?;
        let b = s.as_bytes();
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, off: usize, b: &[u8]) -> Result<usize, i64> {
        // /proc/PID/comm と task/TID/comm (comm_write): 同じスレッドグループのものだけ (ほかは EINVAL)。はじめの 15 バイト
        // まで (改行もそのまま: Linux と同じ)
        if let Node::Comm(pid) | Node::TaskFile(pid, _, 2) = self.node {
            let t = match self.node {
                Node::TaskFile(_, tid, _) => thread(pid, tid)?,
                _ => leader(pid)?,
            };
            if t.tgid != proc::current().tgid {
                return Err(crate::why::inval());
            }
            t.set_comm(&b[..b.len().min(15)]);
            return Ok(b.len());
        }
        // /proc/PID/loginuid (proc_loginuid_write): 自分のものだけ、はじめから 1 度に。番号 (u32::MAX は外す) は、
        // 読むときの user namespace の地図で外の番号に
        if let Node::LoginUid(pid) = self.node {
            if leader(pid)?.pid != proc::current().pid {
                return Err(-EPERM);
            }
            if off != 0 {
                return Err(crate::why::inval());
            }
            let v = kstrtou32(&b[..b.len().min(34)])?;
            let uid = if v == u32::MAX { v } else { crate::ns::take_uid(v)? };
            crate::audit::set_loginuid(uid)?;
            return Ok(b.len());
        }
        // /proc/PID/attr/current と exec: AppArmor の change_profile (自分のものだけ: Linux も current でなければ EACCES)
        if let Node::Attr(pid, _, k) = self.node {
            if leader(pid)?.pid != proc::current().pid {
                return Err(-EACCES);
            }
            return crate::apparmor::attr_write(ATTRS[k as usize], b);
        }
        // /proc/PID/uid_map など: ユーザーの namespace の地図 (書けるかは ns.rs が決める)
        if let Node::IdMap(pid, k) = self.node {
            let u = leader(pid)?.cred.ns.user.clone();
            match k {
                2 => crate::ns::write_setgroups(&u, b)?,
                _ => crate::ns::write_map(&u, k == 1, b)?,
            }
            return Ok(b.len());
        }
        // /proc/sys/...: root が値を書く
        if let Node::Sys(i) = self.node
            && crate::sysctl::TABLE[i as usize].may_write()
        {
            crate::sysctl::TABLE[i as usize].write(b)?;
            return Ok(b.len());
        }
        // /proc/strace: root が名前を書くと、その名前のプロセスの失敗したシステムコールを出す (空で止める)
        if self.node == Node::Strace && crate::cred::current().euid == 0 {
            // "kick PID": その PID の futex で眠っているスレッドを起こす (調べもの用。futex::kick)
            if let Some(pid) = b.strip_prefix(b"kick ").and_then(|r| core::str::from_utf8(r).ok()).and_then(|r| r.trim().parse::<u32>().ok()) {
                let n = crate::futex::kick(pid);
                crate::println!("futex kick {}: woke {}", pid, n);
                return Ok(b.len());
            }
            if b.starts_with(b"pcmiss") {
                let on = !crate::vm::MISS_LOG.load(core::sync::atomic::Ordering::Relaxed);
                crate::vm::MISS_LOG.store(on, core::sync::atomic::Ordering::Relaxed);
                return Ok(b.len());
            }
            // "raw TID": そのスレッドのレジスタとスタックをシリアルに (aiproc::raw_dump)
            if let Some(tid) = b.strip_prefix(b"raw ").and_then(|r| core::str::from_utf8(r).ok()).and_then(|r| r.trim().parse::<u32>().ok()) {
                crate::aiproc::raw_dump(tid);
                return Ok(b.len());
            }
            crate::trace::strace_set(b);
            return Ok(b.len());
        }
        // /proc/ai/ctl: 調べもののスイッチ (root。aiproc::ctl)
        if self.node == Node::AiCtl && crate::aiproc::allowed() {
            crate::aiproc::ctl(b)?;
            return Ok(b.len());
        }
        // /proc/bkl: 書くと (root) 大きなロックの統計を 0 から数えなおす
        if self.node == Node::Bkl && crate::cred::current().euid == 0 {
            crate::lockstat::reset();
            return Ok(b.len());
        }
        Err(-EACCES)
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        // > /proc/strace (O_TRUNC) は受けつける
        let sys = matches!(self.node, Node::Sys(i) if crate::sysctl::TABLE[i as usize].may_write());
        if sys || matches!(self.node, Node::Strace | Node::Bkl) && crate::cred::current().euid == 0 || (self.node == Node::AiCtl && crate::aiproc::allowed()) {
            return Ok(());
        }
        // uid_map や attr/current など (書けるかは書くときに決める)
        if matches!(self.node, Node::IdMap(..) | Node::Attr(..) | Node::LoginUid(_) | Node::Comm(_) | Node::TaskFile(_, _, 2)) {
            return Ok(());
        }
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        // ほかのプロセスの exe cwd root fd ns を見るのは、AppArmor の ptrace の read も (proc_fd_access_allowed)
        if let Node::Cwd(pid) | Node::RootLink(pid) | Node::Exe(pid) | Node::Fd(pid, _) | Node::NsLink(pid, _) = self.node {
            aa_ptrace(pid, true)?;
        }
        match self.node {
            Node::SelfLink => Ok(format!("{}", crate::ns::local_or_0(proc::current().tgid))),
            Node::Cwd(pid) => Ok(format!("/{}", leader(pid)?.files().cwd)),
            // chroot していれば、その場所 (pidof はこれが自分と同じものだけ探す)
            Node::RootLink(pid) => Ok(format!("/{}", leader(pid)?.files().root)),
            Node::Exe(pid) => {
                let p = leader(pid)?;
                // カーネルのスレッドにはプログラムがない (Linux も ENOENT)
                p.mm.as_ref().filter(|_| !p.kthread).ok_or(-ENOENT)?;
                Ok(format!("/{}", p.mm().exe))
            }
            Node::Fd(pid, n) => fd_target(pid, n),
            Node::NsLink(pid, k) => {
                let l = leader(pid)?;
                let kind = NS_KINDS[k as usize];
                // pid はそのプロセスのもの (cred にはこれから作る子のものがある)。pid_for_children の名前も pid:[N]
                let id = if kind == "pid" { l.pid_ns.as_ref().map_or(crate::ns::INIT_PID, |n| n.id) } else { ns_id(&l.cred, kind) };
                Ok(format!("{}:[{}]", if kind == "pid_for_children" { "pid" } else { kind }, id))
            }
            _ => Err(crate::why::inval()),
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
            (Node::Root, "buddyinfo") => Node::Buddyinfo,
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
            (Node::Root, "sysvipc") => Node::SysvipcDir,
            (Node::SysvipcDir, "shm") => Node::SysvipcShm,
            (Node::Root, "ai") if !crate::parts::off(crate::parts::Part::Aiproc) => Node::AiDir,
            (Node::AiDir, "threads") => Node::AiThreads,
            (Node::AiDir, "bkl") => Node::AiBkl,
            (Node::AiDir, "heap") => Node::AiHeap,
            (Node::AiDir, "prof") => Node::AiProf,
            (Node::AiDir, "ctl") => Node::AiCtl,
            (Node::AiDir, "kernel") => Node::AiKernel,
            (Node::AiDir, "mem") => Node::AiMem,
            (Node::AiDir, "fs") => Node::AiFs,
            (Node::AiDir, "trace") => Node::AiTrace,
            (Node::AiDir, "parts") => Node::AiParts,
            (Node::AiDir, "fail") => Node::AiFail,
            (Node::AiDir, "fw") => Node::AiFw,
            (Node::AiDir, "fd") => Node::AiFdDir,
            (Node::AiDir, "why") => Node::AiWhy,
            (Node::AiDir, "stack") => Node::AiStackDir,
            (Node::AiStackDir, _) => {
                let t = num.ok_or(-ENOENT)?;
                if !crate::aiproc::all_tids().contains(&t) {
                    return Err(-ENOENT);
                }
                Node::AiStack(t)
            }
            (Node::AiFdDir, _) => Node::AiFd(leader(num.ok_or(-ENOENT)?)?.tgid),
            (Node::NetDir, "pnp") => Node::Pnp,
            (Node::NetDir, "route") => Node::Route,
            (Node::NetDir, "tcp") => Node::NetTcp,
            (Node::NetDir, "udp") => Node::NetUdp,
            (Node::NetDir, "sockstat") => Node::NetSockstat,
            (Node::NetDir, "snmp") => Node::NetSnmp,
            (Node::NetDir, "unix") => Node::NetUnix,
            (Node::NetDir, "dev") => Node::NetDev,
            (Node::NetDir, "nf_conntrack") => Node::NetConntrack,
            (Node::NetDir, n) if NET_STUBS.iter().any(|(s, _)| *s == n) => Node::NetStub(NET_STUBS.iter().position(|(s, _)| *s == n).unwrap() as u8),
            (Node::Root, "sys") => Node::SysDir(0, 0),
            (Node::SysDir(i, d), _) => match crate::sysctl::lookup(i as usize, d as usize, name).ok_or(-ENOENT)? {
                (j, true) => Node::Sys(j as u16),
                (j, false) => Node::SysDir(j as u16, d + 1),
            },
            // PID の namespace の中なら、中の番号 (ns.rs)
            (Node::Root, _) => Node::Pid(leader_any(crate::ns::to_global(num.ok_or(-ENOENT)?).ok_or(-ENOENT)?)?.tgid),
            (Node::Pid(p), "stat") => Node::Stat(p),
            (Node::Pid(p), "status") => Node::Status(p),
            (Node::Pid(p), "cmdline") => Node::Cmdline(p),
            (Node::Pid(p), "environ") => Node::Environ(p),
            (Node::Pid(p), "statm") => Node::Statm(p),
            (Node::Pid(p), "comm") => Node::Comm(p),
            (Node::Pid(p), "mounts") => Node::PidMounts(p),
            (Node::Pid(p), "mountinfo") => Node::MountInfo(p),
            (Node::Pid(p), "uid_map") => Node::IdMap(p, 0),
            (Node::Pid(p), "gid_map") => Node::IdMap(p, 1),
            (Node::Pid(p), "setgroups") => Node::IdMap(p, 2),
            (Node::Pid(p), "cwd") => Node::Cwd(p),
            (Node::Pid(p), "root") => Node::RootLink(p),
            (Node::Pid(p), "exe") => Node::Exe(p),
            (Node::Pid(p), "maps") => Node::Maps(p),
            (Node::Pid(p), "smaps") => Node::Smaps(p),
            (Node::Pid(p), "stack") => Node::Stack(p),
            (Node::Pid(p), "fd") => Node::FdDir(p),
            (Node::Pid(p), "fdinfo") => Node::FdInfoDir(p),
            (Node::Pid(p), "ns") => Node::NsDir(p),
            (Node::Pid(p), "attr") => Node::AttrDir(p, false),
            (Node::Pid(p), "loginuid") => Node::LoginUid(p),
            (Node::Pid(p), "sessionid") => Node::SessionId(p),
            (Node::AttrDir(p, false), "apparmor") => Node::AttrDir(p, true),
            (Node::AttrDir(p, a), _) => Node::Attr(p, a, ATTRS.iter().position(|x| *x == name).ok_or(-ENOENT)? as u8),
            (Node::Pid(p), "task") => Node::TaskDir(p),
            (Node::TaskDir(p), _) => {
                let tid = crate::ns::to_global(num.ok_or(-ENOENT)?).ok_or(-ENOENT)?;
                thread(p, tid)?;
                Node::Task(p, tid)
            }
            (Node::Task(p, t), "stat") => Node::TaskFile(p, t, 0),
            (Node::Task(p, t), "status") => Node::TaskFile(p, t, 1),
            (Node::Task(p, t), "comm") => Node::TaskFile(p, t, 2),
            (Node::NsDir(p), n) => Node::NsLink(p, NS_KINDS.iter().position(|k| *k == n).ok_or(-ENOENT)? as u8),
            (Node::FdDir(p), _) => {
                let n = num.ok_or(-ENOENT)? as usize;
                leader(p)?.files().get(n as u64).ok_or(-ENOENT)?;
                Node::Fd(p, n)
            }
            (Node::FdInfoDir(p), _) => {
                let n = num.ok_or(-ENOENT)? as usize;
                leader(p)?.files().get(n as u64).ok_or(-ENOENT)?;
                Node::FdInfo(p, n)
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
                add("buddyinfo".into(), Node::Buddyinfo);
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
                add("sysvipc".into(), Node::SysvipcDir);
                if !crate::parts::off(crate::parts::Part::Aiproc) {
                    add("ai".into(), Node::AiDir);
                }
                add("sys".into(), Node::SysDir(0, 0));
                for p in proc::all_leader_procs() {
                    if let Some(l) = crate::ns::to_local(p.tgid) {
                        add(format!("{}", l), Node::Pid(p.tgid));
                    }
                }
            }
            Node::AiDir => {
                add("threads".into(), Node::AiThreads);
                add("bkl".into(), Node::AiBkl);
                add("heap".into(), Node::AiHeap);
                add("prof".into(), Node::AiProf);
                add("ctl".into(), Node::AiCtl);
                add("kernel".into(), Node::AiKernel);
                add("mem".into(), Node::AiMem);
                add("fs".into(), Node::AiFs);
                add("trace".into(), Node::AiTrace);
                add("parts".into(), Node::AiParts);
                add("fail".into(), Node::AiFail);
                add("fw".into(), Node::AiFw);
                add("fd".into(), Node::AiFdDir);
                add("stack".into(), Node::AiStackDir);
                add("why".into(), Node::AiWhy);
            }
            Node::AiFdDir | Node::AiStackDir if !crate::aiproc::allowed() => return Err(-EACCES),
            Node::AiStackDir => {
                for t in crate::aiproc::all_tids() {
                    add(format!("{}", t), Node::AiStack(t));
                }
            }
            Node::AiFdDir => {
                for p in proc::all_leader_procs() {
                    add(format!("{}", p.tgid), Node::AiFd(p.tgid));
                }
            }
            Node::SysvipcDir => add("shm".into(), Node::SysvipcShm),
            Node::NetDir => {
                add("pnp".into(), Node::Pnp);
                add("route".into(), Node::Route);
                add("tcp".into(), Node::NetTcp);
                add("udp".into(), Node::NetUdp);
                add("sockstat".into(), Node::NetSockstat);
                add("snmp".into(), Node::NetSnmp);
                add("unix".into(), Node::NetUnix);
                add("dev".into(), Node::NetDev);
                add("nf_conntrack".into(), Node::NetConntrack);
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
                add("mounts".into(), Node::PidMounts(p));
                add("mountinfo".into(), Node::MountInfo(p));
                add("task".into(), Node::TaskDir(p));
                add("uid_map".into(), Node::IdMap(p, 0));
                add("gid_map".into(), Node::IdMap(p, 1));
                add("setgroups".into(), Node::IdMap(p, 2));
                add("cwd".into(), Node::Cwd(p));
                add("root".into(), Node::RootLink(p));
                add("exe".into(), Node::Exe(p));
                add("maps".into(), Node::Maps(p));
                add("smaps".into(), Node::Smaps(p));
                add("stack".into(), Node::Stack(p));
                add("fd".into(), Node::FdDir(p));
                add("fdinfo".into(), Node::FdInfoDir(p));
                add("ns".into(), Node::NsDir(p));
                add("attr".into(), Node::AttrDir(p, false));
                add("loginuid".into(), Node::LoginUid(p));
                add("sessionid".into(), Node::SessionId(p));
            }
            Node::AttrDir(p, a) => {
                for (k, n) in ATTRS.iter().enumerate() {
                    add((*n).into(), Node::Attr(p, a, k as u8));
                }
                if !a {
                    add("apparmor".into(), Node::AttrDir(p, true));
                }
            }
            Node::TaskDir(p) => {
                leader(p)?;
                for t in proc::threads_of(p).iter().map(|t| t.pid).collect::<Vec<_>>() {
                    add(format!("{}", crate::ns::local_or_0(t)), Node::Task(p, t));
                }
            }
            Node::Task(p, t) => {
                for (i, n) in ["stat", "status", "comm"].iter().enumerate() {
                    add((*n).into(), Node::TaskFile(p, t, i as u8));
                }
            }
            Node::NsDir(p) => {
                for (i, k) in NS_KINDS.iter().enumerate() {
                    add((*k).into(), Node::NsLink(p, i as u8));
                }
            }
            Node::FdDir(p) | Node::FdInfoDir(p) => {
                let fds: Vec<usize> = leader(p)?.files().fds.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i).collect();
                let info = matches!(self.node, Node::FdInfoDir(_));
                for n in fds {
                    add(format!("{}", n), if info { Node::FdInfo(p, n) } else { Node::Fd(p, n) });
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

/// /proc/threads: すべてのスレッドの pid、tgid、状態、待っているもの、最後のシステムコール (調べもの用)
/// /proc/PID/stack (調べもの用): プロセスのスレッドごとに、EL0 の pc と lr、フレームポインタをたどった戻り先、
/// それからスタック (sp から 32 KiB) の中で写したファイル (実行できるところ) を指す値を「ファイル+位置」で
fn stacks_text(tgid: u32) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    for p in proc::live() {
        if p.state == State::Zombie || p.tgid != tgid {
            continue;
        }
        let tf = p.tf_ref();
        let (pc, lr, fp, sp) = (tf.elr, tf.x[30], tf.x[29], tf.sp_el0);
        let n = p.comm.iter().position(|&c| c == 0).unwrap_or(16);
        let st = match p.state {
            State::Running => "run",
            State::Runnable => "ready",
            State::Sleeping => "sleep",
            _ => "other",
        };
        let _ = writeln!(out, "thread {} {} sys {} chan {:x} {} utime {}", p.pid, core::str::from_utf8(&p.comm[..n]).unwrap_or("?"), p.last_sys.0, p.chan, st, p.utime);
        let pt = p.pt();
        let _h = pt.hold();
        let name = |pt: &mut crate::vm::PageTable, va: u64| -> Option<alloc::string::String> {
            let (_, v) = pt.find(va as usize)?;
            if v.prot & 4 == 0 {
                return None;
            }
            let (f, off) = pt.name_at(va as usize)?;
            Some(alloc::format!("{}+{:#x}", f.rsplit('/').next().unwrap_or(&f), off))
        };
        // futex で待っているなら、そのアドレスと、待つときの値と、いまの値
        if p.last_sys.0 == 98 && p.state == State::Sleeping {
            let mut w = [0u8; 4];
            let now = pt.copy_in(&mut w, tf.x[0] as usize).map(|_| u32::from_le_bytes(w) as i64).unwrap_or(-1);
            let _ = writeln!(out, "  futex {:#x} op {} val {} now {}", tf.x[0], tf.x[1], tf.x[2] as u32, now);
        }
        for (what, va) in [("pc", pc), ("lr", lr)] {
            let _ = writeln!(out, "  {} {:#x} {}", what, va, name(pt, va).unwrap_or_default());
        }
        // フレームポインタ: [fp] = 前の fp、[fp + 8] = 戻り先
        let mut f = fp;
        for _ in 0..48 {
            if f == 0 || f & 7 != 0 {
                break;
            }
            let mut b = [0u8; 16];
            if pt.copy_in(&mut b, f as usize).is_none() {
                break;
            }
            let next = u64::from_le_bytes(b[..8].try_into().unwrap());
            let ret = u64::from_le_bytes(b[8..].try_into().unwrap());
            let _ = writeln!(out, "  fp {:#x} {}", ret, name(pt, ret).unwrap_or_default());
            if next <= f {
                break;
            }
            f = next;
        }
        // スタックの中の、コードを指す値
        let mut buf = alloc::vec![0u8; 32 * 1024];
        let mut got = 0;
        while got < buf.len() {
            let chunk = 4096 - ((sp as usize + got) & 4095);
            let chunk = chunk.min(buf.len() - got);
            if pt.copy_in(&mut buf[got..got + chunk], sp as usize + got).is_none() {
                break;
            }
            got += chunk;
        }
        // フレームの記録 ([前の fp, 戻り先]) をスタックの中から探してたどる (fp を使わない libc の中で
        // 眠っていても、その上の呼び出しの並びがわかる)。いちばん下から、4 つ以上つながるもの
        let word = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let (lo, hi) = (sp, sp + got as u64);
        let rec = |pt: &mut crate::vm::PageTable, off: usize| -> Option<(usize, u64)> {
            let next = word(off);
            let ret = word(off + 8);
            (next > lo + off as u64 && next < hi && next & 7 == 0 && name(pt, ret).is_some()).then(|| ((next - lo) as usize, ret))
        };
        let mut off = 0;
        while off + 16 <= got {
            let mut chain = alloc::vec::Vec::new();
            let mut o = off;
            while o + 16 <= got && chain.len() < 64 {
                let Some((next, ret)) = rec(pt, o) else { break };
                chain.push(ret);
                o = next;
            }
            if chain.len() >= 4 {
                for ret in chain {
                    let _ = writeln!(out, "  chain {:#x} {}", ret, name(pt, ret).unwrap_or_default());
                }
                break;
            }
            off += 8;
        }
        let mut shown = 0;
        for w in buf[..got].chunks_exact(8) {
            let va = u64::from_le_bytes(w.try_into().unwrap());
            if let Some(nm) = name(pt, va) {
                let _ = writeln!(out, "  scan {:#x} {}", va, nm);
                shown += 1;
                if shown >= 80 {
                    break;
                }
            }
        }
    }
    for (tid, a, op, n) in crate::syscall::futex_wakes(tgid) {
        let _ = writeln!(out, "wake by {} {:#x} op {} woke {}", tid, a, op, n);
    }
    // 開いている fd の様子 (ソケットに残っているバイト、epoll の見張り)
    if let Some(l) = proc::find_leader(tgid)
        && let Some(files) = l.files.as_ref()
    {
        let t = files.get();
        for n in 0..t.fds.len() {
            if let Some((f, _)) = t.get_other(n as u64) {
                let _ = writeln!(out, "fd {} {}", n, fd_state(&f.borrow()));
            }
        }
    }
    out
}


/// 調べるための fd の様子 (/proc/PID/stack の fd の一覧)
fn fd_state(f: &crate::file::OpenFile) -> String {
    let (r, w, h) = f.readiness();
    let rw = alloc::format!("r{}w{}h{}", r as u8, w as u8, h as u8);
    match &f.kind {
        crate::file::Kind::PipeRead(p) | crate::file::Kind::PipeWrite(p) | crate::file::Kind::PipeRw(p) => {
            let p = p.borrow();
            alloc::format!("{} {} len={} r={} w={}", f.describe(), rw, p.len(), p.readers, p.writers)
        }
        crate::file::Kind::Ops(o) => {
            let desc = f.describe();
            o.debug_state(&desc, &rw).unwrap_or_else(|| alloc::format!("{} {}", desc, rw))
        }
        _ => alloc::format!("{} {}", f.describe(), rw),
    }
}

/// /proc/buddyinfo (Linux と同じ形。ゾーンは 1 つ)
fn buddyinfo() -> alloc::string::String {
    let (c, _) = crate::kalloc::orders();
    let mut s = alloc::string::String::from("Node 0, zone   Normal ");
    for n in c {
        s.push_str(&alloc::format!("{:6} ", n));
    }
    s.push('\n');
    s
}

/// /proc/PID/status の Speculation_Store_Bypass と SpeculationIndirectBranch (fs/proc/array.c の task_seccomp)
fn spec_status(ssb: u8) -> String {
    let ssb_s = match crate::cpuvuln::prctl_get(crate::cpuvuln::PR_SPEC_STORE_BYPASS, ssb) {
        Ok(crate::cpuvuln::PR_SPEC_NOT_AFFECTED) => "not vulnerable",
        Ok(v) if v == crate::cpuvuln::PR_SPEC_PRCTL | crate::cpuvuln::PR_SPEC_FORCE_DISABLE as i64 => "thread force mitigated",
        Ok(v) if v == crate::cpuvuln::PR_SPEC_PRCTL | crate::cpuvuln::PR_SPEC_DISABLE as i64 => "thread mitigated",
        Ok(v) if v == crate::cpuvuln::PR_SPEC_PRCTL | crate::cpuvuln::PR_SPEC_ENABLE as i64 => "thread vulnerable",
        Ok(v) if v == crate::cpuvuln::PR_SPEC_DISABLE as i64 => "globally mitigated",
        _ => "vulnerable",
    };
    // arm64 は PR_SPEC_INDIRECT_BRANCH を ENODEV で答えるので unknown
    alloc::format!("Speculation_Store_Bypass:\t{}\nSpeculationIndirectBranch:\tunknown\n", ssb_s)
}

