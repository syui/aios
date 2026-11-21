// /proc: プロセスの様子を見せる小さなファイルシステム
//
//   /proc/self          -> 自分の PID
//   /proc/PID/stat      Linux と同じ並びの 1 行
//   /proc/PID/status    Name, State, Pid, Uid など
//   /proc/PID/cmdline   引数 (NUL で区切る。プロセスのメモリの argv の文字列から)
//   /proc/PID/environ   環境 (同じく。自分のものか root だけ)
//   /proc/PID/statm     メモリ (ページ): size resident shared text lib data dt
//   /proc/PID/smaps     maps の行ごとに、ページの数 (Rss、Pss、Shared / Private の Clean / Dirty、Swap など: vm.rs の smaps_text)
//   /proc/PID/limits    rlimit の表 (Linux と同じ形)
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
const EIO: i64 = 5;
const ERANGE: i64 = 34;
const CAP_SYS_TIME: u32 = 25;
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
    /// /proc/PID/task (スレッドの一覧)、task/TID、その中のスレッドごとのもの (TASK_FILES の番号: stat status comm
    /// syscall sched personality)。番号は本当のもの。ほかの名前はプロセスのもの (Linux の tid_base_stuff と同じ並び)
    TaskDir(u32),
    Task(u32, u32),
    TaskFile(u32, u32, u8),
    /// /proc/PID/limits (rlimit の表: Linux の proc_pid_limits)
    Limits(u32),
    /// /proc/PID/personality (%08x。ptrace の ATTACH ができる人だけ)、timerslack_ns (タイマーのゆとり)
    Personality(u32),
    TimerSlack(u32),
    /// /proc/PID/oom_score (選ばれやすさ 0..2000 の 2/3: oom::score)、oom_adj (昔の -17..15)、oom_score_adj (-1000..1000)
    OomScore(u32),
    OomAdj(u32),
    OomScoreAdj(u32),
    /// /proc/PID/auxv (exec で積んだもの)、syscall (いまのシステムコール)、sched (走った数と時間)、smaps_rollup、
    /// mountstats、timens_offsets、clear_refs
    Auxv(u32),
    Syscall(u32),
    Sched(u32),
    SmapsRollup(u32),
    Mountstats(u32),
    TimensOffsets(u32),
    ClearRefs(u32),
    /// /proc/net (self/net へのリンク: Linux と同じ)
    NetLink,
    /// /proc/PID/mem (ほかのプロセスのメモリを読み書き: 番地がファイルの位置)、pagemap (ページごとの 8 バイト)
    Mem(u32),
    Pagemap(u32),
    /// /proc/PID/map_files と、その中のリンク (ファイルを写した領域の はじめ-終わり)
    MapFilesDir(u32),
    MapFile(u32, usize, usize),
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
    SysvipcMsg,
    SysvipcSem,
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
/// task/TID の中のスレッドごとのもの (Node::TaskFile の番号)
const TASK_FILES: [&str; 6] = ["stat", "status", "comm", "syscall", "sched", "personality"];
/// task/TID の中のプロセスのもの (Linux の tid_base_stuff のうち、スレッドごとでないもの)
const TASK_SHARED: [&str; 30] = [
    "fd", "fdinfo", "ns", "net", "environ", "auxv", "limits", "cmdline", "statm", "maps", "mem", "cwd", "root", "exe", "mounts", "mountinfo", "clear_refs", "smaps",
    "smaps_rollup", "pagemap", "attr", "oom_score", "oom_adj", "oom_score_adj", "loginuid", "sessionid", "uid_map", "gid_map", "projid_map", "setgroups",
];

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
/// oom_adj を書いて Linux と同じく 1 度だけ kmsg に知らせた
static OOM_ADJ_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Linux の kstrtoint (基数 0: 0x は 16 進、0 で始まれば 8 進。+ と - をつけられる。i32 におさまらなければ None)
fn kstrtoint(s: &str) -> Option<i32> {
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()?
    } else if s.len() > 1 && s.starts_with('0') {
        u64::from_str_radix(&s[1..], 8).ok()?
    } else {
        s.parse::<u64>().ok()?
    };
    let v = if neg { -(v as i128) } else { v as i128 };
    i32::try_from(v).ok()
}

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
            Node::Limits(p) => (p as u64) << 16 | 0x60,
            Node::Personality(p) => (p as u64) << 16 | 0x61,
            Node::TimerSlack(p) => (p as u64) << 16 | 0x62,
            Node::OomScore(p) => (p as u64) << 16 | 0x63,
            Node::OomAdj(p) => (p as u64) << 16 | 0x64,
            Node::OomScoreAdj(p) => (p as u64) << 16 | 0x65,
            Node::Auxv(p) => (p as u64) << 16 | 0x66,
            Node::Syscall(p) => (p as u64) << 16 | 0x67,
            Node::Sched(p) => (p as u64) << 16 | 0x68,
            Node::SmapsRollup(p) => (p as u64) << 16 | 0x69,
            Node::Mountstats(p) => (p as u64) << 16 | 0x6a,
            Node::TimensOffsets(p) => (p as u64) << 16 | 0x6b,
            Node::ClearRefs(p) => (p as u64) << 16 | 0x6c,
            Node::NetLink => 58,
            Node::Mem(p) => (p as u64) << 16 | 0x6d,
            Node::Pagemap(p) => (p as u64) << 16 | 0x6e,
            Node::MapFilesDir(p) => (p as u64) << 16 | 0x6f,
            Node::MapFile(p, s, _) => 1 << 62 | (p as u64) << 32 | (s as u64 >> 12) & 0xffff_ffff,
            Node::SessionId(p) => (p as u64) << 16 | 28,
            Node::IdMap(p, k) => (p as u64) << 16 | (16 + k as u64),
            Node::TaskDir(p) => (p as u64) << 16 | 20,
            Node::Task(_, t) => (t as u64) << 16 | 21,
            Node::TaskFile(_, t, k) => (t as u64) << 16 | (0x80 + k as u64),
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
            Node::SysvipcMsg => 59,
            Node::SysvipcSem => 60,
        }
    }

    fn pid(&self) -> Option<u32> {
        match self.node {
            Node::Pid(p) | Node::Stat(p) | Node::Status(p) | Node::Cmdline(p) | Node::Environ(p) | Node::Statm(p) | Node::Comm(p) | Node::Cwd(p) | Node::RootLink(p) | Node::Exe(p) | Node::Maps(p) | Node::Smaps(p) | Node::Stack(p) | Node::FdDir(p) | Node::Fd(p, _) | Node::FdInfoDir(p) | Node::FdInfo(p, _) | Node::NsDir(p) | Node::NsLink(p, _) | Node::PidMounts(p) | Node::MountInfo(p) | Node::IdMap(p, _) | Node::TaskDir(p) | Node::Task(p, _) | Node::TaskFile(p, _, _) | Node::AttrDir(p, _) | Node::Attr(p, ..) | Node::LoginUid(p) | Node::SessionId(p) | Node::Limits(p) | Node::Personality(p) | Node::TimerSlack(p) | Node::OomScore(p) | Node::OomAdj(p) | Node::OomScoreAdj(p) | Node::Auxv(p) | Node::Syscall(p) | Node::Sched(p) | Node::SmapsRollup(p) | Node::Mountstats(p) | Node::TimensOffsets(p) | Node::ClearRefs(p) | Node::Mem(p) | Node::Pagemap(p) | Node::MapFilesDir(p) | Node::MapFile(p, ..) => Some(p),
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
                let p = leader(pid)?;
                let n = map_names(p);
                let pt = &p.mm().pt;
                let _h = pt.hold();
                pt.maps_text_named(n)
            }
            Node::Smaps(pid) => {
                let p = leader(pid)?;
                let n = map_names(p);
                let pt = &p.mm().pt;
                let _h = pt.hold();
                pt.smaps_text(n)
            }
            Node::Stack(pid) => {
                aa_ptrace(pid, false)?;
                stacks_text(leader(pid)?.tgid)
            }
            Node::Status(pid) => {
                let p = leader_any(pid)?;
                let ths = proc::threads_of(p.tgid);
                let threads = ths.len();
                // プロセスに来たもの (Linux の shared_pending) は、どのスレッドが持っていてもまとめて ShdPnd に
                let shared = ths.iter().fold(0, |a, t| a | t.sig_shared);
                let queued = ths.iter().map(|t| t.sig_pending.count_ones() + t.sig_shared.count_ones()).sum::<u32>();
                let sig = crate::signal::status_text(p, shared, queued);
                status_text(p, threads, &sig)
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
            // statm (Linux の task_statm: size resident shared text lib data dt。shared はファイルと共有メモリの RSS、text は
            // VmExe、data は VmData と VmStk、lib と dt は 0)
            Node::Statm(pid) => {
                let p = leader(pid)?;
                match &p.mm {
                    Some(m) => {
                        let mm = m.get();
                        let _h = mm.pt.hold();
                        let sm = mm.pt.status_mem(crate::exec::USER_STACK_TOP - 1, &mm.exe);
                        format!("{} {} {} {} 0 {} 0\n", mm.pt.vsize() / crate::memlayout::PGSIZE, sm[0] + sm[1] + sm[2], sm[1] + sm[2], sm[5], sm[3] + sm[4])
                    }
                    None => "0 0 0 0 0 0 0\n".to_string(),
                }
            }
            Node::StatAll => stat_all(),
            Node::Diskstats => crate::block::proc_diskstats(),
            Node::Buddyinfo => buddyinfo(),
            Node::SysvipcShm => crate::shm::proc_text(),
            Node::SysvipcMsg => crate::msg::proc_text(),
            Node::SysvipcSem => crate::sem::proc_text(),
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
            Node::Limits(pid) => limits_text(leader_any(pid)?),
            // lock_trace (ptrace の ATTACH: できなければ EPERM)
            Node::Personality(pid) => {
                aa_ptrace(pid, false)?;
                format!("{:08x}\n", leader(pid)?.personality)
            }
            // 自分 (代表スレッド) でなければ CAP_SYS_NICE (proc_pid_timerslack_ns_show)
            Node::TimerSlack(pid) => {
                let p = leader(pid)?;
                if p.pid != proc::current().pid && !crate::cred::current().capable(crate::cred::CAP_SYS_NICE) {
                    return Err(-EPERM);
                }
                format!("{}\n", p.timer_slack_ns)
            }
            Node::OomScore(pid) => format!("{}\n", crate::oom::score(leader_any(pid)?)),
            // oom_score_adj を -17..15 に (1000 は 15)
            Node::OomAdj(pid) => {
                let adj = proc::oom_adj(leader_any(pid)?).0 as i32;
                format!("{}\n", if adj == 1000 { 15 } else { adj * 17 / 1000 })
            }
            Node::OomScoreAdj(pid) => format!("{}\n", proc::oom_adj(leader_any(pid)?).0),
            // 開くときに mm_access (permission)。中身はバイトの並びなので read_at で (ここは来ない)
            Node::Auxv(_) => String::new(),
            Node::Syscall(pid) => {
                aa_ptrace(pid, false)?;
                syscall_text(leader(pid)?)
            }
            Node::Sched(pid) => sched_text(leader_any(pid)?),
            Node::SmapsRollup(pid) => {
                let pt = &leader(pid)?.mm().pt;
                let _h = pt.hold();
                pt.smaps_rollup_text()
            }
            // mounts の行の device、場所、種類を (Linux の show_vfsstat: nfs などの数はない)
            Node::Mountstats(pid) => crate::vfs::mounts_text_of(leader(pid)?.cred.ns.mnt.clone())
                .lines()
                .map(|l| {
                    let f: Vec<&str> = l.split(' ').collect();
                    format!("device {} mounted on {} with fstype {}\n", f.first().unwrap_or(&""), f.get(1).unwrap_or(&""), f.get(2).unwrap_or(&""))
                })
                .collect(),
            // 時間の namespace はない: init のもの (ずれ 0)
            Node::TimensOffsets(_) => format!("{:<10} {:>10} {:>9}\n{:<10} {:>10} {:>9}\n", "monotonic", 0, 0, "boottime", 0, 0),
            Node::ClearRefs(_) => return Err(crate::why::inval()),
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
                    2 => format!("{}\n", t.comm()),
                    3 => {
                        aa_ptrace(pid, false)?;
                        syscall_text(t)
                    }
                    4 => sched_text(t),
                    _ => {
                        aa_ptrace(pid, false)?;
                        format!("{:08x}\n", t.personality)
                    }
                }
            }
            Node::IdMap(pid, 2) => crate::ns::setgroups_text(&leader(pid)?.cred.ns.user).into(),
            Node::IdMap(pid, 3) => crate::ns::projid_text(&leader(pid)?.cred.ns.user),
            Node::Attr(pid, _, k) => crate::apparmor::attr_read(&leader(pid)?.cred, ATTRS[k as usize])?,
            Node::IdMap(pid, k) => crate::ns::map_text(&leader(pid)?.cred.ns.user, k == 1),
            _ => return Err(-EISDIR),
        })
    }
}

/// maps の [heap]、[stack]、[vdso] の手がかり (カーネルのスレッドは なし)
fn map_names(p: &mut Proc) -> crate::vm::MapNames {
    if p.kthread || p.mm.is_none() {
        return crate::vm::MapNames::default();
    }
    let m = p.mm();
    let on = !crate::parts::off(crate::parts::Part::Vdso);
    let (vdso, vvar) = if on { (crate::vdso::VDSO_VA, crate::vdso::VVAR_VA) } else { (0, 0) };
    crate::vm::MapNames { start_brk: m.heap_start, brk: m.brk, start_stack: crate::exec::USER_STACK_TOP - 1, vdso, vvar }
}

/// map_files の名前 "はじめ-終わり" (16 進。0 でない数の頭の 0 はだめ: Linux の dname_to_vma_addr)
fn map_file_name(name: &str) -> Option<(usize, usize)> {
    let (a, b) = name.split_once('-')?;
    let ok = |x: &str| !x.is_empty() && !(x.len() > 1 && x.starts_with('0')) && x.bytes().all(|c| c.is_ascii_hexdigit());
    if !ok(a) || !ok(b) {
        return None;
    }
    Some((usize::from_str_radix(a, 16).ok()?, usize::from_str_radix(b, 16).ok()?))
}

/// /proc/PID/syscall (Linux の proc_pid_syscall): 走れるもの (自分でない) は running。システムコールの中なら番号と引数 6 つ
/// (はじめのものは入ったときの x0) と sp と pc、ほかの入り方なら -1 と sp と pc
fn syscall_text(t: &mut Proc) -> String {
    let me = proc::current().pid == t.pid;
    if !me && matches!(t.state, State::Runnable | State::Running) {
        return "running\n".into();
    }
    let (nr, orig) = (t.sysno, t.orig_x0);
    let tf = t.tf_ref();
    if nr < 0 {
        format!("{} {:#x} {:#x}\n", nr, tf.sp_el0, tf.elr)
    } else {
        format!("{} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}\n", nr, orig, tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5], tf.sp_el0, tf.elr)
    }
}

/// /proc/PID/sched (Linux の proc_sched_show_task と同じ名前と形: 名前は 45 の左寄せ、数は 21、時間は ms の小数 6 けた)。
/// aios が数えていない平均 (se.avg.*) は 0、重みは nice 0 のもの、slice は Linux の既定 (0.7 ms × (1 + log2 CPU))
fn sched_text(p: &Proc) -> String {
    let mut s = format!("{} ({}, #threads: {})\n", p.comm(), crate::ns::local_or_0(p.pid), proc::threads_of(p.tgid).len());
    s += "-------------------------------------------------------------------\n";
    let ns = |n: &str, v: u64| format!("{:<45}:{:>14}.{:06}\n", n, v / 1_000_000, v % 1_000_000);
    let int = |n: &str, v: i64| format!("{:<45}:{:>21}\n", n, v);
    let run = proc::cpu_ns(p);
    let t0 = crate::timer::uptime_ns();
    let delta = crate::timer::uptime_ns() - t0;
    s += &ns("se.exec_start", if p.ran_at != 0 { p.ran_at } else { crate::timer::uptime_ns() });
    s += &ns("se.vruntime", run);
    s += &ns("se.sum_exec_runtime", run);
    s += &int("se.nr_migrations", 0);
    s += &int("nr_switches", (p.nvcsw + p.nivcsw) as i64);
    s += &int("nr_voluntary_switches", p.nvcsw as i64);
    s += &int("nr_involuntary_switches", p.nivcsw as i64);
    s += &int("se.load.weight", (proc::weight(p) as i64) << 10);
    for n in ["se.avg.load_sum", "se.avg.runnable_sum", "se.avg.util_sum", "se.avg.load_avg", "se.avg.runnable_avg", "se.avg.util_avg"] {
        s += &int(n, 0);
    }
    s += &int("se.avg.last_update_time", crate::timer::uptime_ns() as i64);
    s += &int("se.avg.util_est", 0);
    s += &int("policy", p.policy as i64);
    s += &int("prio", proc::kprio(p) as i64);
    s += &int("se.slice", proc::slice_ns(p) as i64);
    s += &int("clock-delta", delta as i64);
    s
}

/// /proc/PID/limits (Linux の proc_pid_limits と同じ形: 名前 25、ソフトとハード 20、単位 10 の左寄せ)
fn limits_text(p: &Proc) -> String {
    const NAMES: [(&str, &str); proc::RLIM_NLIMITS] = [
        ("Max cpu time", "seconds"),
        ("Max file size", "bytes"),
        ("Max data size", "bytes"),
        ("Max stack size", "bytes"),
        ("Max core file size", "bytes"),
        ("Max resident set", "bytes"),
        ("Max processes", "processes"),
        ("Max open files", "files"),
        ("Max locked memory", "bytes"),
        ("Max address space", "bytes"),
        ("Max file locks", "locks"),
        ("Max pending signals", "signals"),
        ("Max msgqueue size", "bytes"),
        ("Max nice priority", ""),
        ("Max realtime priority", ""),
        ("Max realtime timeout", "us"),
    ];
    let f = |x: u64| if x == proc::RLIM_INFINITY { "unlimited".to_string() } else { x.to_string() };
    let mut s = format!("{:<25} {:<20} {:<20} {:<10}\n", "Limit", "Soft Limit", "Hard Limit", "Units");
    for (i, (name, unit)) in NAMES.iter().enumerate() {
        let v = proc::rlimit(p, i);
        s += &format!("{:<25} {:<20} {:<20} ", name, f(v[0]), f(v[1]));
        if unit.is_empty() {
            s.push('\n');
        } else {
            s += &format!("{:<10}\n", unit);
        }
    }
    s
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
/// /proc/PID/status (Linux の proc_pid_status と同じ行と順: task_state、task_mem、task_sig、task_cap、seccomp、
/// 投機の手当て、CPU、切りかえの数)。Landlock (かかっている層の数) は aios が足したもの (aish が読む)。VmPeak と VmHWM は
/// 読んだときまでのいちばん大きなもの (Linux は領域を外すときにも覚える)。Cap* は aios の資格 (root がみな持つ) から
fn status_text(p: &Proc, threads: usize, sig: &str) -> String {
    let c = &p.cred;
    let umask = p.files.as_ref().map(|f| f.get().umask);
    // FDSize: fd の表の大きさ (Linux は 64 から、128 の 2 のべき倍に伸ばす)
    let nfds = p.files.as_ref().map_or(0, |f| f.get().fds.len());
    let mut fdsize = 64;
    while fdsize < nfds {
        fdsize = if fdsize == 64 { 128 } else { fdsize * 2 };
    }
    let groups: Vec<String> = c.groups.iter().map(|&g| format!("{}", crate::ns::show_gid(g))).collect();
    // NStgid などは、見ている人の PID の namespace から、そのプロセスの namespace まで
    let nested = p.pid_ns.is_some() && proc::current().pid_ns.is_none();
    let ns_ids = |g: u32| {
        let mut s = format!("{}", crate::ns::local_or_0(g));
        if nested && let Some(t) = proc::find_any(g) {
            s += &format!("\t{}", t.vpid);
        }
        s
    };
    let (vsize, rss_pages, locked, sm, peak, hwm) = match &p.mm {
        Some(m) => {
            let mm = m.get();
            let _h = mm.pt.hold();
            let stack = crate::exec::USER_STACK_TOP - 1;
            let sm = mm.pt.status_mem(stack, &mm.exe);
            let vsize = mm.pt.vsize();
            let rss = sm[0] + sm[1] + sm[2];
            mm.peak_vm = mm.peak_vm.max(vsize);
            mm.peak_rss = mm.peak_rss.max(rss);
            (vsize, rss, mm.pt.locked_pages(), sm, mm.peak_vm, mm.peak_rss)
        }
        None => (0, 0, 0, [0; 9], 0, 0),
    };
    let kb = |pages: usize| pages * 4;
    let caps: u64 = if c.euid == 0 { ((1u64 << 41) - 1) & crate::security::capget(c) } else { 0 };
    let bnd = ((1u64 << 41) - 1) & !p.capbset_dropped;
    let cpus = crate::smp::online().max(1);
    let mut s = String::new();
    s += &format!("Name:\t{}\n", p.comm());
    if let Some(u) = umask {
        s += &format!("Umask:\t{:04o}\n", u);
    }
    s += &format!("State:\t{}\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\n", state_name(p), crate::ns::local_or_0(p.tgid), crate::ns::local_or_0(p.pid), crate::ns::local_or_0(p.ppid));
    s += &format!(
        "Uid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\n",
        crate::ns::show_uid(c.uid),
        crate::ns::show_uid(c.euid),
        crate::ns::show_uid(c.suid),
        crate::ns::show_uid(c.fsuid),
        crate::ns::show_gid(c.gid),
        crate::ns::show_gid(c.egid),
        crate::ns::show_gid(c.sgid),
        crate::ns::show_gid(c.fsgid)
    );
    s += &format!("FDSize:\t{}\nGroups:\t{}\n", fdsize, groups.join(" "));
    s += &format!("NStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\nKthread:\t{}\n", ns_ids(p.tgid), ns_ids(p.pid), ns_ids(p.pgid), ns_ids(p.sid), p.kthread as u8);
    if p.mm.is_some() {
        for (k, v) in [
            ("VmPeak", peak / 1024),
            ("VmSize", vsize / 1024),
            ("VmLck", kb(locked)),
            ("VmPin", 0),
            ("VmHWM", kb(hwm)),
            ("VmRSS", kb(rss_pages)),
            ("RssAnon", kb(sm[0])),
            ("RssFile", kb(sm[1])),
            ("RssShmem", kb(sm[2])),
            ("VmData", kb(sm[3])),
            ("VmStk", kb(sm[4])),
            ("VmExe", kb(sm[5])),
            ("VmLib", kb(sm[6])),
            ("VmPTE", kb(sm[7])),
            ("VmSwap", kb(sm[8])),
        ] {
            s += &format!("{}:\t{:8} kB\n", k, v);
        }
        s += "CoreDumping:\t0\nTHP_enabled:\t0\nuntag_mask:\t0xffffffffffffff\n";
    }
    s += &format!("Threads:\t{}\n", threads);
    s += sig;
    s += &format!("CapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nCapAmb:\t{:016x}\n", 0, caps, caps, bnd, 0);
    s += &format!("NoNewPrivs:\t{}\nSeccomp:\t{}\nSeccomp_filters:\t{}\n", c.no_new_privs as u8, crate::seccomp::mode(c).0, crate::seccomp::mode(c).1);
    s += &format!("Landlock:\t{}\n", crate::landlock::layers(c));
    s += &spec_status(p.spec_ssb);
    let mask = if cpus >= 64 { u64::MAX } else { (1u64 << cpus) - 1 };
    let list = if cpus == 1 { "0".to_string() } else { format!("0-{}", cpus - 1) };
    s += &format!("Cpus_allowed:\t{:x}\nCpus_allowed_list:\t{}\n", mask, list);
    s += &format!("voluntary_ctxt_switches:\t{}\nnonvoluntary_ctxt_switches:\t{}\n", p.nvcsw, p.nivcsw);
    s
}

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
        "{} ({}) {} {} {} {} {} {} {} 0 0 0 0 {} {} {} {} {} {} {} 0 {} {} {}",
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
        proc::kprio(p) - 100,
        p.nice,
        threads,
        p.start,
        mem_of(p).0,
        mem_of(p).1
    );
    // 残り (rsslim から exit_code まで: Linux の do_task_stat)。コードやデータの場所、引数と環境の場所、exit_code は、
    // メモリを読んでよい人 (mm_access) にだけ。シグナルは 10 進 (Linux 2.0 のなごり)。exit_signal はプロセスなら SIGCHLD
    let permitted = mm_access(p.tgid).is_ok();
    let (code, args, start_brk) = match &p.mm {
        Some(m) if permitted => {
            let mm = m.get();
            let _h = mm.pt.hold();
            (mm.pt.exe_ranges(&mm.exe), mm.args, mm.heap_start)
        }
        _ => ((0, 0, 0, 0), (0, 0, 0), 0),
    };
    let (ign, cgt) = crate::signal::ign_cgt(p);
    let wchan = permitted && threads < 2 && !matches!(state_char(p), 'R');
    let sstack = if permitted && p.mm.is_some() { crate::exec::USER_STACK_TOP - 1 } else { 0 };
    for i in 25..=52 {
        let v: alloc::string::String = match i {
            25 => format!("{}", proc::rlimit(p, 5)[0]),
            26 => format!("{}", if permitted { code.0.max(1) } else { 0 }),
            27 => format!("{}", if permitted { code.1.max(1) } else { 0 }),
            28 => format!("{}", sstack),
            31 => format!("{}", p.sig_pending | p.sig_shared),
            32 => format!("{}", p.sig_mask),
            33 => format!("{}", ign),
            34 => format!("{}", cgt),
            35 => format!("{}", wchan as u8),
            38 => format!("{}", if p.pid != p.tgid { -1 } else { 17 }),
            39 => format!("{}", p.cpu),
            40 => format!("{}", p.rt_priority),
            41 => format!("{}", p.policy),
            45 => format!("{}", code.2),
            46 => format!("{}", code.3),
            47 => format!("{}", start_brk),
            48 => format!("{}", args.0),
            49 => format!("{}", args.1),
            50 => format!("{}", args.1),
            51 => format!("{}", args.2),
            _ => "0".to_string(),
        };
        s.push(' ');
        s.push_str(&v);
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
const NS_KINDS: [&str; 7] = ["ipc", "mnt", "net", "pid", "pid_for_children", "user", "uts"];

fn ns_id(c: &crate::cred::Cred, kind: &str) -> u64 {
    match kind {
        "mnt" => c.ns.mnt_id(),
        "user" => c.ns.user_id(),
        "net" => c.ns.net_id(),
        "pid_for_children" => c.ns.pid_children_id(),
        "ipc" => c.ns.ipc_id(),
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
            Node::Maps(pid) | Node::Smaps(pid) | Node::SmapsRollup(pid) | Node::Environ(pid) | Node::Auxv(pid) | Node::Pagemap(pid) => Some(c.check(m, want).and_then(|_| mm_access(pid))),
            // mem は ptrace の ATTACH ができる人だけ (proc_mem_open の mm_access: EACCES)
            Node::Mem(pid) => Some(c.check(m, want).and_then(|_| ptrace_may(pid, false).map_err(|_| -EACCES))),
            Node::FdInfo(pid, _) => Some(c.check(m, want).and_then(|_| aa_ptrace(pid, true))),
            // UTS ごとの sysctl (kernel/uname): その UTS で CAP_SYS_ADMIN を持てば持ち主のビットで (Linux の
            // net_ctl_permission と同じ考え。中に root の割りあてのないユーザーの namespace (aibox --image の 65532) でも、
            // sethostname できる人は書ける)
            Node::Sys(i) if crate::sysctl::TABLE[i as usize].uts_scoped() && crate::ns::uts_admin() => {
                Some(if want & !((m.mode >> 6) & 7) == 0 { Ok(()) } else { c.check(m, want) })
            }
            _ => None,
        }
    }

    /// map_files のリンクをたどるのは CAP_SYS_ADMIN だけ (proc_map_files_get_link: checkpoint_restore_ns_capable)
    fn may_follow(&self) -> Result<(), i64> {
        if matches!(self.node, Node::MapFile(..)) && !crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN) {
            return Err(-EPERM);
        }
        Ok(())
    }

    fn magic_link(&self) -> Option<(String, InodeRef)> {
        // map_files: 写しているファイルそのもの (消したものも)
        if let Node::MapFile(pid, s, e) = self.node {
            let l = leader(pid).ok()?;
            let pt = &l.mm().pt;
            let _h = pt.hold();
            return pt.file_maps().into_iter().find(|m| m.0 == s && m.1 == e).map(|m| (m.4, m.5));
        }
        // ns のリンク: nsfs の口 (setns で入れる。開いているあいだ namespace はなくならない)
        if let Node::NsLink(pid, k) = self.node {
            aa_ptrace(pid, true).ok()?;
            return Some(crate::ns::ns_file(NS_KINDS[k as usize], leader(pid).ok()?));
        }
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
            Node::SelfLink | Node::NetLink | Node::Cwd(_) | Node::RootLink(_) | Node::Exe(_) => S_IFLNK | 0o777,
            Node::Auxv(_) | Node::Syscall(_) | Node::Mountstats(_) | Node::Pagemap(_) => S_IFREG | 0o400,
            Node::Mem(_) => S_IFREG | 0o600,
            Node::MapFilesDir(_) => S_IFDIR | 0o500,
            // 写しの開き方しだい: 読めれば r、書ければ w (proc_map_files_instantiate)
            Node::MapFile(pid, s, e) => {
                let rw = leader(pid).ok().and_then(|l| {
                    let pt = &l.mm().pt;
                    let _h = pt.hold();
                    pt.file_maps().iter().find(|m| m.0 == s && m.1 == e).map(|m| (m.2, m.3))
                });
                let (r, w) = rw.unwrap_or((false, false));
                S_IFLNK | if r { 0o400 } else { 0 } | if w { 0o200 } else { 0 }
            }
            Node::ClearRefs(_) => S_IFREG | 0o200,
            Node::Sched(_) | Node::TimensOffsets(_) => S_IFREG | 0o644,
            // 開いた口しだい: 読めれば r-x、書ければ -wx (Linux の tid_fd_update_inode)
            Node::Fd(pid, n) => {
                let f = leader(pid).ok().and_then(|p| p.files().get_other(n as u64)).map(|(f, _)| f);
                // いま使っているもの (/proc/self/fd を getdents している、その fd など) は借りられない: 読む口として
                let (r, w) = f.map_or((false, false), |f| f.try_borrow().map_or((true, false), |f| (f.readable(), f.writable())));
                S_IFLNK | (if r { 0o500 } else { 0 }) | (if w { 0o300 } else { 0 })
            }
            Node::Strace | Node::Bkl | Node::IdMap(..) | Node::LoginUid(_) | Node::Comm(_) | Node::OomAdj(_) | Node::OomScoreAdj(_) => S_IFREG | 0o644,
            Node::TimerSlack(_) => S_IFREG | 0o666,
            Node::Personality(_) => S_IFREG | 0o400,
            Node::TaskFile(_, _, 2 | 4) => S_IFREG | 0o644,
            Node::TaskFile(_, _, 3 | 5) => S_IFREG | 0o400,
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
        // /proc/PID/mem (mem_rw): 位置は番地。読めたところまで。はじめから読めなければ EIO。カーネルのスレッドは 0
        if let Node::Mem(pid) = self.node {
            let p = leader(pid)?;
            let Some(mm) = p.mm.as_ref().filter(|_| !p.kthread) else { return Ok(0) };
            let pt = &mut mm.get().pt;
            let _h = pt.hold();
            let n = pt.read_remote(off, buf);
            return if n == 0 && !buf.is_empty() { Err(-EIO) } else { Ok(n) };
        }
        // /proc/PID/pagemap (pagemap_read): 位置と大きさは 8 の倍数 (でなければ EINVAL)。位置 / 8 がページの番号。番地の
        // 上の 8 ビットはタグとして外す (arm64 の untagged_addr_remote)。ユーザーの番地の終わりから先は読めない (0)
        if let Node::Pagemap(pid) = self.node {
            if off % 8 != 0 || buf.len() % 8 != 0 {
                return Err(crate::why::inval());
            }
            let p = leader(pid)?;
            let Some(mm) = p.mm.as_ref().filter(|_| !p.kthread) else { return Ok(0) };
            let pt = &mm.get().pt;
            let _h = pt.hold();
            let show = crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN);
            let vpn = off / 8;
            if vpn > usize::MAX >> 12 {
                return Ok(0);
            }
            let start = (vpn << 12) & ((1 << 56) - 1);
            let mut n = 0;
            while n < buf.len() {
                let va = start + n / 8 * crate::memlayout::PGSIZE;
                if va >= crate::vm::MAXVA {
                    break;
                }
                buf[n..n + 8].copy_from_slice(&pt.pagemap_entry(va, show).to_le_bytes());
                n += 8;
            }
            return Ok(n);
        }
        // /proc/PID/auxv: 型と値の 8 バイトずつ (AT_NULL まで: Linux の auxv_read)
        if let Node::Auxv(pid) = self.node {
            let p = leader(pid)?;
            let b: Vec<u8> = p.mm.as_ref().map_or(Vec::new(), |m| m.get().auxv.iter().flat_map(|v| v.to_le_bytes()).collect());
            if off >= b.len() {
                return Ok(0);
            }
            let n = buf.len().min(b.len() - off);
            buf[..n].copy_from_slice(&b[off..off + n]);
            return Ok(n);
        }
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
        // 0 バイト (writes_empty で来る): Linux の書き手が数として読むもの (oom_score_adj、oom_adj、timerslack_ns、loginuid:
        // EINVAL) と comm (空の名前に) だけ書き手へ。ほか (/proc/sys は proc_dointvec と同じく) 何もせず 0
        if b.is_empty() && !matches!(self.node, Node::OomAdj(_) | Node::OomScoreAdj(_) | Node::TimerSlack(_) | Node::LoginUid(_) | Node::Comm(_) | Node::TaskFile(_, _, 2) | Node::ClearRefs(_) | Node::TimensOffsets(_)) {
            return Ok(0);
        }
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
        // /proc/PID/oom_score_adj と oom_adj (oom_score_adj_write、oom_adj_write、__set_oom_adj): はじめの 12 バイトを
        // 前後の空白を除いて kstrtoint (0x と 0 の基数も)。-1000..1000 (oom_adj は -17..15 を 1000 / 17 倍、15 は 1000) の外は
        // EINVAL。下限 (oom_adj は今の値) より下げるのは CAP_SYS_RESOURCE だけ (EACCES)。それを持つ人が oom_score_adj を
        // 書くと下限もその値に
        if let Node::OomScoreAdj(pid) | Node::OomAdj(pid) = self.node {
            let legacy = matches!(self.node, Node::OomAdj(_));
            let s = core::str::from_utf8(&b[..b.len().min(12)]).map_err(|_| crate::why::inval())?.trim();
            let v = kstrtoint(s).ok_or_else(crate::why::inval)?;
            let adj = if legacy {
                if !(-17..=15).contains(&v) {
                    return Err(crate::why::inval());
                }
                if v == 15 { 1000 } else { v * 1000 / 17 }
            } else {
                if !(-1000..=1000).contains(&v) {
                    return Err(crate::why::inval());
                }
                v
            };
            let t = leader(pid)?;
            let (cur, min) = proc::oom_adj(t);
            let cap = crate::cred::current().capable(crate::cred::CAP_SYS_RESOURCE);
            if (adj as i16) < if legacy { cur } else { min } && !cap {
                return Err(-EACCES);
            }
            if legacy && !OOM_ADJ_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                let me = proc::current();
                crate::println!("{} ({}): /proc/{}/oom_adj is deprecated, please use /proc/{}/oom_score_adj instead.", me.comm(), me.pid, t.tgid, t.tgid);
            }
            proc::set_oom_adj(t, adj as i16, (!legacy && cap).then_some(adj as i16));
            return Ok(b.len());
        }
        // /proc/PID/mem: 書ける領域に書けたところまで (はじめから書けなければ EIO)
        if let Node::Mem(pid) = self.node {
            let p = leader(pid)?;
            let Some(mm) = p.mm.as_ref().filter(|_| !p.kthread) else { return Ok(0) };
            let pt = &mut mm.get().pt;
            let _h = pt.hold();
            let n = pt.write_remote(off, b);
            return if n == 0 { Err(-EIO) } else { Ok(n) };
        }
        // /proc/PID/clear_refs (clear_refs_write): 1〜5 (10 進、前後の空白はよい)。aios は参照の印 (AF) も最大の RSS も
        // 見せていないので、受けつけるだけ
        if let Node::ClearRefs(pid) = self.node {
            let s = core::str::from_utf8(&b[..b.len().min(12)]).map_err(|_| crate::why::inval())?.trim();
            let v: i32 = s.parse().map_err(|_| crate::why::inval())?;
            if !(1..=5).contains(&v) {
                return Err(crate::why::inval());
            }
            leader(pid)?;
            return Ok(b.len());
        }
        // /proc/PID/sched (proc_sched_set_task): 書くと数えなおす (aios は受けつけるだけ)
        if let Node::Sched(pid) | Node::TaskFile(pid, _, 4) = self.node {
            leader(pid)?;
            return Ok(b.len());
        }
        // /proc/PID/timens_offsets (timens_offsets_write、proc_timens_set_offset): "名前か番号 秒 ナノ秒" の行。読めなければ
        // EINVAL、CAP_SYS_TIME がなければ EPERM、時計の外は ERANGE。時間の namespace はないので、どれも init のもの
        // (もう決まっている: EACCES)
        if let Node::TimensOffsets(pid) = self.node {
            if off != 0 || b.len() >= 4096 {
                return Err(crate::why::inval());
            }
            let s = core::str::from_utf8(b).map_err(|_| crate::why::inval())?;
            let mut offs = Vec::new();
            for l in s.split('\n').filter(|l| !l.is_empty()).take(2) {
                let f: Vec<&str> = l.split_whitespace().collect();
                let (sec, ns) = match (f.get(1).and_then(|x| x.parse::<i64>().ok()), f.get(2).and_then(|x| x.parse::<u64>().ok())) {
                    (Some(a), Some(b)) if f.len() >= 3 && b < 1_000_000_000 => (a, b),
                    _ => return Err(crate::why::inval()),
                };
                if !matches!(f[0], "monotonic" | "1" | "boottime" | "7") {
                    return Err(crate::why::inval());
                }
                offs.push((sec, ns));
            }
            if offs.is_empty() {
                return Err(crate::why::inval());
            }
            leader(pid)?;
            if !crate::cred::current().capable(CAP_SYS_TIME) {
                return Err(-EPERM);
            }
            let now = (crate::timer::uptime_ns() / 1_000_000_000) as i64;
            const KTIME_SEC_MAX: i64 = i64::MAX / 1_000_000_000;
            for (sec, _) in offs {
                if !(-KTIME_SEC_MAX..=KTIME_SEC_MAX).contains(&sec) || !(0..=KTIME_SEC_MAX / 2).contains(&now.saturating_add(sec)) {
                    return Err(-ERANGE);
                }
            }
            return Err(-EACCES);
        }
        // /proc/PID/projid_map (ケーパビリティは要らない: ns::write_projid)
        if let Node::IdMap(pid, 3) = self.node {
            crate::ns::write_projid(&leader(pid)?.cred.ns.user, b)?;
            return Ok(b.len());
        }
        // /proc/PID/timerslack_ns (proc_pid_set_timer_slack_ns): 10 進。自分でなければ CAP_SYS_NICE。0 は fork したときの値
        if let Node::TimerSlack(pid) = self.node {
            let s = core::str::from_utf8(b).map_err(|_| crate::why::inval())?;
            let v = s.strip_suffix('\n').unwrap_or(s).parse::<u64>().map_err(|_| crate::why::inval())?;
            let t = leader(pid)?;
            if t.pid != proc::current().pid && !crate::cred::current().capable(crate::cred::CAP_SYS_NICE) {
                return Err(-EPERM);
            }
            t.timer_slack_ns = if v == 0 { t.default_slack_ns } else { v };
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

    fn writes_empty(&self) -> bool {
        true
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        // > /proc/strace (O_TRUNC) は受けつける
        let sys = matches!(self.node, Node::Sys(i) if crate::sysctl::TABLE[i as usize].may_write());
        if sys || matches!(self.node, Node::Strace | Node::Bkl) && crate::cred::current().euid == 0 || (self.node == Node::AiCtl && crate::aiproc::allowed()) {
            return Ok(());
        }
        // uid_map や attr/current など (書けるかは書くときに決める)
        if matches!(self.node, Node::IdMap(..) | Node::Attr(..) | Node::LoginUid(_) | Node::Comm(_) | Node::TaskFile(_, _, 2) | Node::OomAdj(_) | Node::OomScoreAdj(_) | Node::TimerSlack(_) | Node::ClearRefs(_) | Node::Sched(_) | Node::TaskFile(_, _, 4) | Node::TimensOffsets(_)) {
            return Ok(());
        }
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        // ほかのプロセスの exe cwd root fd ns を見るのは、AppArmor の ptrace の read も (proc_fd_access_allowed)
        if let Node::Cwd(pid) | Node::RootLink(pid) | Node::Exe(pid) | Node::Fd(pid, _) | Node::NsLink(pid, _) | Node::MapFile(pid, ..) = self.node {
            aa_ptrace(pid, true)?;
        }
        match self.node {
            Node::SelfLink => Ok(format!("{}", crate::ns::local_or_0(proc::current().tgid))),
            Node::NetLink => Ok("self/net".into()),
            Node::MapFile(pid, s, e) => {
                let l = leader(pid)?;
                let pt = &l.mm().pt;
                let _h = pt.hold();
                pt.file_maps().into_iter().find(|m| m.0 == s && m.1 == e).map(|m| m.4).ok_or(-ENOENT)
            }
            Node::Cwd(pid) => Ok(format!("/{}", leader(pid)?.files().cwd)),
            // chroot していれば、その場所 (pidof はこれが自分と同じものだけ探す)
            // failfs の根 (fchroot の FD_FAILFS_ROOT) は Linux の failfs_dname と同じ名前
            Node::RootLink(pid) => {
                let l = leader(pid)?;
                let root = &l.files().root;
                Ok(if root == crate::vfs::FAILFS { "failfs:/".into() } else { format!("/{}", root) })
            }
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
            (Node::Root, "net") => Node::NetLink,
            (Node::Root, "sysvipc") => Node::SysvipcDir,
            (Node::SysvipcDir, "shm") => Node::SysvipcShm,
            (Node::SysvipcDir, "msg") => Node::SysvipcMsg,
            (Node::SysvipcDir, "sem") => Node::SysvipcSem,
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
            (Node::Pid(p), "limits") => Node::Limits(p),
            (Node::Pid(p), "personality") => Node::Personality(p),
            (Node::Pid(p), "timerslack_ns") => Node::TimerSlack(p),
            (Node::Pid(p), "oom_score") => Node::OomScore(p),
            (Node::Pid(p), "oom_adj") => Node::OomAdj(p),
            (Node::Pid(p), "oom_score_adj") => Node::OomScoreAdj(p),
            (Node::Pid(p), "auxv") => Node::Auxv(p),
            (Node::Pid(p), "syscall") => Node::Syscall(p),
            (Node::Pid(p), "sched") => Node::Sched(p),
            (Node::Pid(p), "smaps_rollup") => Node::SmapsRollup(p),
            (Node::Pid(p), "mountstats") => Node::Mountstats(p),
            (Node::Pid(p), "timens_offsets") => Node::TimensOffsets(p),
            (Node::Pid(p), "clear_refs") => Node::ClearRefs(p),
            (Node::Pid(p), "projid_map") => Node::IdMap(p, 3),
            (Node::Pid(p), "mem") => Node::Mem(p),
            (Node::Pid(p), "pagemap") => Node::Pagemap(p),
            (Node::Pid(p), "map_files") => Node::MapFilesDir(p),
            // はじめ-終わり (16 進。頭に 0 はつけない: Linux の dname_to_vma_addr)、ちょうどその領域があるときだけ
            (Node::MapFilesDir(p), _) => {
                aa_ptrace(p, true)?;
                let (s, e) = map_file_name(name).ok_or(-ENOENT)?;
                let l = leader(p)?;
                let pt = &l.mm().pt;
                let _h = pt.hold();
                pt.file_maps().iter().find(|m| m.0 == s && m.1 == e).ok_or(-ENOENT)?;
                Node::MapFile(p, s, e)
            }
            (Node::Pid(p), "net") => {
                leader(p)?;
                Node::NetDir
            }
            (Node::Pid(p), "sessionid") => Node::SessionId(p),
            (Node::AttrDir(p, false), "apparmor") => Node::AttrDir(p, true),
            (Node::AttrDir(p, a), _) => Node::Attr(p, a, ATTRS.iter().position(|x| *x == name).ok_or(-ENOENT)? as u8),
            (Node::Pid(p), "task") => Node::TaskDir(p),
            (Node::TaskDir(p), _) => {
                let tid = crate::ns::to_global(num.ok_or(-ENOENT)?).ok_or(-ENOENT)?;
                thread(p, tid)?;
                Node::Task(p, tid)
            }
            (Node::Task(p, t), n) if TASK_FILES.contains(&n) => Node::TaskFile(p, t, TASK_FILES.iter().position(|x| *x == n).unwrap() as u8),
            // ほかはプロセスのもの (スレッドの中にないもの: task、map_files、mountstats、timens_offsets、timerslack_ns)
            (Node::Task(p, _), n) if TASK_SHARED.contains(&n) => return ProcInode { fs: self.fs, node: Node::Pid(p) }.lookup(n),
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
                add("net".into(), Node::NetLink);
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
            Node::SysvipcDir => {
                add("msg".into(), Node::SysvipcMsg);
                add("sem".into(), Node::SysvipcSem);
                add("shm".into(), Node::SysvipcShm);
            }
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
                add("limits".into(), Node::Limits(p));
                add("personality".into(), Node::Personality(p));
                add("timerslack_ns".into(), Node::TimerSlack(p));
                add("oom_score".into(), Node::OomScore(p));
                add("oom_adj".into(), Node::OomAdj(p));
                add("oom_score_adj".into(), Node::OomScoreAdj(p));
                add("auxv".into(), Node::Auxv(p));
                add("syscall".into(), Node::Syscall(p));
                add("sched".into(), Node::Sched(p));
                add("smaps_rollup".into(), Node::SmapsRollup(p));
                add("mountstats".into(), Node::Mountstats(p));
                add("timens_offsets".into(), Node::TimensOffsets(p));
                add("clear_refs".into(), Node::ClearRefs(p));
                add("projid_map".into(), Node::IdMap(p, 3));
                add("net".into(), Node::NetDir);
                add("mem".into(), Node::Mem(p));
                add("pagemap".into(), Node::Pagemap(p));
                add("map_files".into(), Node::MapFilesDir(p));
            }
            // ptrace で読める人だけ (proc_map_files_readdir: EACCES)
            Node::MapFilesDir(p) => {
                aa_ptrace(p, true)?;
                let l = leader(p)?;
                let pt = &l.mm().pt;
                let _h = pt.hold();
                for (s, e, ..) in pt.file_maps() {
                    add(format!("{:x}-{:x}", s, e), Node::MapFile(p, s, e));
                }
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
                for (i, n) in TASK_FILES.iter().enumerate() {
                    add((*n).into(), Node::TaskFile(p, t, i as u8));
                }
                let pd = ProcInode { fs: self.fs, node: Node::Pid(p) };
                for n in TASK_SHARED {
                    if let Ok(i) = pd.lookup(n)
                        && let Some(c) = i.as_any().downcast_ref::<ProcInode>()
                    {
                        add(n.into(), c.node);
                    }
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

