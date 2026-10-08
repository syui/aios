// seccomp のフィルタを作ってかける (aibox など)。aarch64 のシステムコールの番号
//   restrict(deny, kill): deny のものは EPERM で返し、kill のものは呼んだらプロセスを止める。ほかは許す。
//   ほかの arch から呼ばれたら止める (番号がちがうので)
//   先に prctl(PR_SET_NO_NEW_PRIVS) が要る (aibox は landlock のためにもうしている)
#![allow(dead_code)]

/// 砂場の中ではいらない、カーネルの深いところにさわるもの (ふつうのコマンドは使わない)
pub const DEFAULT_DENY: &[&str] = &[
    "ptrace", "process_vm_readv", "process_vm_writev", "perf_event_open", "bpf", "userfaultfd", "kexec_load", "kexec_file_load", "init_module", "finit_module",
    "delete_module", "reboot", "mount", "umount2", "pivot_root", "swapon", "swapoff", "acct", "keyctl", "add_key", "request_key", "settimeofday", "clock_settime",
    "adjtimex", "clock_adjtime", "sethostname", "setdomainname", "open_by_handle_at", "name_to_handle_at", "quotactl",
];

/// 名前と番号 (aarch64 の asm-generic)
const NAMES: &[(&str, u32)] = &[
    ("ioctl", 29), ("mknodat", 33), ("mkdirat", 34), ("unlinkat", 35), ("symlinkat", 36), ("linkat", 37), ("renameat", 38), ("umount2", 39), ("mount", 40),
    ("pivot_root", 41), ("chroot", 51), ("fchmod", 52), ("fchmodat", 53), ("fchownat", 54), ("fchown", 55), ("openat", 56), ("close", 57), ("pipe2", 59),
    ("quotactl", 60), ("read", 63), ("write", 64), ("sendfile", 71), ("acct", 89), ("capset", 91), ("personality", 92), ("exit", 93), ("exit_group", 94),
    ("unshare", 97), ("futex", 98), ("nanosleep", 101), ("kexec_load", 104), ("init_module", 105), ("delete_module", 106), ("clock_settime", 112),
    ("syslog", 116), ("ptrace", 117), ("sched_setscheduler", 119), ("kill", 129), ("tkill", 130), ("tgkill", 131), ("rt_sigreturn", 139),
    ("setpriority", 140), ("reboot", 142), ("setregid", 143), ("setgid", 144), ("setreuid", 145), ("setuid", 146), ("setresuid", 147), ("setresgid", 149),
    ("setfsuid", 151), ("setfsgid", 152), ("setpgid", 154), ("setsid", 157), ("setgroups", 159), ("uname", 160), ("sethostname", 161),
    ("setdomainname", 162), ("setrlimit", 164), ("umask", 166), ("prctl", 167), ("settimeofday", 170), ("adjtimex", 171), ("getpid", 172),
    ("getppid", 173), ("getuid", 174), ("geteuid", 175), ("socket", 198), ("socketpair", 199), ("bind", 200), ("listen", 201), ("accept", 202),
    ("connect", 203), ("sendto", 206), ("recvfrom", 207), ("add_key", 217), ("request_key", 218), ("keyctl", 219), ("clone", 220), ("execve", 221),
    ("mmap", 222), ("swapon", 224), ("swapoff", 225), ("mprotect", 226), ("perf_event_open", 241), ("accept4", 242), ("wait4", 260),
    ("prlimit64", 261), ("name_to_handle_at", 264), ("open_by_handle_at", 265), ("clock_adjtime", 266), ("setns", 268), ("process_vm_readv", 270),
    ("process_vm_writev", 271), ("finit_module", 273), ("seccomp", 277), ("getrandom", 278), ("memfd_create", 279), ("bpf", 280), ("execveat", 281),
    ("userfaultfd", 282), ("kexec_file_load", 294), ("io_uring_setup", 425), ("io_uring_enter", 426), ("io_uring_register", 427), ("clone3", 435),
    ("landlock_create_ruleset", 444),
];

/// 名前 (か数) からシステムコールの番号
pub fn number(name: &str) -> Option<u32> {
    name.parse().ok().or_else(|| NAMES.iter().find(|(n, _)| *n == name).map(|(_, v)| *v))
}

const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const EPERM: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const fn ld_abs(k: u32) -> Insn {
    Insn { code: 0x20, jt: 0, jf: 0, k }
}
const fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code: 0x15, jt, jf, k }
}
const fn ret(k: u32) -> Insn {
    Insn { code: 0x06, jt: 0, jf: 0, k }
}

/// フィルタ (BPF の命令の並び) を作る
fn program(deny: &[u32], kill: &[u32]) -> Vec<Insn> {
    let mut p = vec![ld_abs(4), jeq(AUDIT_ARCH_AARCH64, 1, 0), ret(RET_KILL_PROCESS), ld_abs(0)];
    for &n in kill {
        p.push(jeq(n, 0, 1));
        p.push(ret(RET_KILL_PROCESS));
    }
    for &n in deny {
        p.push(jeq(n, 0, 1));
        p.push(ret(RET_ERRNO | EPERM));
    }
    p.push(ret(RET_ALLOW));
    p
}

#[repr(C)]
struct Fprog {
    len: u16,
    filter: *const Insn,
}

/// フィルタをかける (外せない。子と exec したものにも引き継がれる)
pub fn restrict(deny: &[u32], kill: &[u32]) -> Result<(), String> {
    let p = program(deny, kill);
    let f = Fprog { len: p.len() as u16, filter: p.as_ptr() };
    // SECCOMP_SET_MODE_FILTER (1)
    let r = unsafe { libc::syscall(libc::SYS_seccomp, 1, 0, &f as *const Fprog) };
    if r != 0 {
        return Err(format!("seccomp: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}
