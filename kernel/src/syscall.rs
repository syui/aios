// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::cell::Bkl;
use crate::cred;
use crate::proc;
use crate::signal::{self, Restart};
use crate::sysfile;
use crate::timer;
use crate::trap::TrapFrame;
use crate::vm::{pg_up, Backing, PROT_READ};
use crate::memlayout::PGSIZE;
use alloc::string::String;
use alloc::vec::Vec;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const ESRCH: i64 = 3;
const E2BIG: i64 = 7;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EBADF: i64 = 9;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;

// システムコールの番号 (nr::READ など) と名前 (name。/proc/bkl と strace で見せる)
macro_rules! nrs {
    ($($name:ident = $n:literal,)*) => {
        pub mod nr {
            $(pub const $name: u64 = $n;)*
        }
        /// 番号から名前 (大文字。知らない番号なら None)
        pub fn name(n: u64) -> Option<&'static str> {
            #[allow(unreachable_patterns)]
            match n {
                $($n => Some(stringify!($name)),)*
                _ => None,
            }
        }
        /// 名前 (大文字でも小文字でも) から番号 (fail.rs の ctl)
        pub fn number(name: &str) -> Option<u64> {
            $(if name.eq_ignore_ascii_case(stringify!($name)) { return Some($n); })*
            None
        }
    };
}

nrs! {
    SETXATTR = 5,
    LSETXATTR = 6,
    FSETXATTR = 7,
    GETXATTR = 8,
    LGETXATTR = 9,
    FGETXATTR = 10,
    LISTXATTR = 11,
    LLISTXATTR = 12,
    FLISTXATTR = 13,
    REMOVEXATTR = 14,
    LREMOVEXATTR = 15,
    FREMOVEXATTR = 16,
    SETXATTRAT = 463,
    GETXATTRAT = 464,
    LISTXATTRAT = 465,
    REMOVEXATTRAT = 466,
    OPEN_TREE_ATTR = 467,
    LISTNS = 470,
    FCHROOT = 472,
    PTRACE = 117,
    FILE_GETATTR = 468,
    FILE_SETATTR = 469,
    MSEAL = 462,
    PROCESS_MADVISE = 440,
    SETNS = 268,
    PROCESS_MRELEASE = 448,
    FUTEX_WAKE2 = 454,
    FUTEX_WAIT2 = 455,
    FUTEX_REQUEUE2 = 456,
    STATMOUNT = 457,
    LISTMOUNT = 458,
    GETCWD = 17,
    FLOCK = 32,
    MKNODAT = 33,
    SWAPON = 224,
    SWAPOFF = 225,
    MKDIRAT = 34,
    UNLINKAT = 35,
    SYMLINKAT = 36,
    LINKAT = 37,
    RENAMEAT = 38,
    STATFS = 43,
    FSTATFS = 44,
    TRUNCATE = 45,
    FTRUNCATE = 46,
    DUP = 23,
    DUP3 = 24,
    FCNTL = 25,
    IOCTL = 29,
    EVENTFD2 = 19,
    TIMERFD_CREATE = 85,
    TIMERFD_SETTIME = 86,
    TIMERFD_GETTIME = 87,
    INOTIFY_INIT1 = 26,
    INOTIFY_ADD_WATCH = 27,
    INOTIFY_RM_WATCH = 28,
    EPOLL_CREATE1 = 20,
    EPOLL_CTL = 21,
    EPOLL_PWAIT = 22,
    EPOLL_PWAIT2 = 441,
    FACCESSAT = 48,
    CHDIR = 49,
    FCHDIR = 50,
    CHROOT = 51,
    MOUNT = 40,
    UMOUNT2 = 39,
    PIVOT_ROOT = 41,
    NAME_TO_HANDLE_AT = 264,
    OPEN_TREE = 428,
    MOVE_MOUNT = 429,
    FSOPEN = 430,
    FSCONFIG = 431,
    FSMOUNT = 432,
    FSPICK = 433,
    MOUNT_SETATTR = 442,
    OPEN_BY_HANDLE_AT = 265,
    FCHMOD = 52,
    FCHMODAT = 53,
    FCHOWNAT = 54,
    FCHOWN = 55,
    OPENAT = 56,
    CLOSE = 57,
    PIPE2 = 59,
    GETDENTS64 = 61,
    LSEEK = 62,
    READ = 63,
    WRITE = 64,
    READV = 65,
    WRITEV = 66,
    PREAD64 = 67,
    PWRITE64 = 68,
    PREADV = 69,
    PWRITEV = 70,
    SENDFILE = 71,
    PSELECT6 = 72,
    PPOLL = 73,
    SIGNALFD4 = 74,
    VMSPLICE = 75,
    SPLICE = 76,
    TEE = 77,
    READLINKAT = 78,
    NEWFSTATAT = 79,
    FSTAT = 80,
    SYNC = 81,
    FSYNC = 82,
    FDATASYNC = 83,
    SYNC_FILE_RANGE = 84,
    SYNCFS = 267,
    UTIMENSAT = 88,
    EXIT = 93,
    EXIT_GROUP = 94,
    SET_TID_ADDRESS = 96,
    FUTEX = 98,
    SET_ROBUST_LIST = 99,
    GET_ROBUST_LIST = 100,
    FUTEX_WAITV = 449,
    NANOSLEEP = 101,
    GETITIMER = 102,
    SETITIMER = 103,
    TIMER_CREATE = 107,
    TIMER_GETTIME = 108,
    TIMER_GETOVERRUN = 109,
    TIMER_SETTIME = 110,
    TIMER_DELETE = 111,
    CLOCK_GETTIME = 113,
    CLOCK_GETRES = 114,
    CLOCK_NANOSLEEP = 115,
    SCHED_SETPARAM = 118,
    SCHED_SETSCHEDULER = 119,
    SCHED_GETSCHEDULER = 120,
    SCHED_GETPARAM = 121,
    SCHED_SETAFFINITY = 122,
    SCHED_GETAFFINITY = 123,
    SCHED_YIELD = 124,
    SCHED_GET_PRIORITY_MAX = 125,
    SCHED_GET_PRIORITY_MIN = 126,
    KILL = 129,
    TKILL = 130,
    TGKILL = 131,
    RT_SIGSUSPEND = 133,
    SIGALTSTACK = 132,
    RT_SIGACTION = 134,
    RT_SIGPROCMASK = 135,
    RT_SIGPENDING = 136,
    RT_SIGTIMEDWAIT = 137,
    RT_SIGQUEUEINFO = 138,
    RT_SIGRETURN = 139,
    REBOOT = 142,
    SETREGID = 143,
    SETGID = 144,
    SETREUID = 145,
    SETUID = 146,
    SETRESUID = 147,
    GETRESUID = 148,
    SETRESGID = 149,
    GETRESGID = 150,
    SETFSUID = 151,
    SETFSGID = 152,
    GETGROUPS = 158,
    SETGROUPS = 159,
    SETPGID = 154,
    PRCTL = 167,
    CAPGET = 90,
    CAPSET = 91,
    IO_URING_SETUP = 425,
    IO_URING_ENTER = 426,
    IO_URING_REGISTER = 427,
    GETPGID = 155,
    GETSID = 156,
    SETSID = 157,
    UNAME = 160,
    SETHOSTNAME = 161,
    SETDOMAINNAME = 162,
    GETCPU = 168,
    SCHED_RR_GET_INTERVAL = 127,
    RESTART_SYSCALL = 128,
    VHANGUP = 58,
    MLOCK = 228,
    MUNLOCK = 229,
    MLOCKALL = 230,
    MUNLOCKALL = 231,
    MLOCK2 = 284,
    UNSHARE = 97,
    GETRLIMIT = 163,
    SETRLIMIT = 164,
    PERSONALITY = 92,
    UMASK = 166,
    GETTIMEOFDAY = 169,
    GETPID = 172,
    GETPPID = 173,
    GETUID = 174,
    GETEUID = 175,
    GETGID = 176,
    GETEGID = 177,
    GETTID = 178,
    SYSINFO = 179,
    SOCKET = 198,
    SOCKETPAIR = 199,
    BIND = 200,
    LISTEN = 201,
    ACCEPT = 202,
    CONNECT = 203,
    GETSOCKNAME = 204,
    GETPEERNAME = 205,
    SENDTO = 206,
    RECVFROM = 207,
    SETSOCKOPT = 208,
    GETSOCKOPT = 209,
    SHUTDOWN = 210,
    SENDMSG = 211,
    RECVMSG = 212,
    RECVMMSG = 243,
    SENDMMSG = 269,
    IOPRIO_SET = 30,
    IOPRIO_GET = 31,
    BRK = 214,
    MUNMAP = 215,
    MREMAP = 216,
    CLONE = 220,
    CLONE3 = 435,
    EXECVE = 221,
    MMAP = 222,
    FADVISE64 = 223,
    READAHEAD = 213,
    FALLOCATE = 47,
    SETPRIORITY = 140,
    GETPRIORITY = 141,
    SCHED_SETATTR = 274,
    CLOCK_SETTIME = 112,
    SYSLOG = 116,
    SETTIMEOFDAY = 170,
    ADJTIMEX = 171,
    CLOCK_ADJTIME = 266,
    SCHED_GETATTR = 275,
    MPROTECT = 226,
    MSYNC = 227,
    MINCORE = 232,
    MADVISE = 233,
    REMAP_FILE_PAGES = 234,
    // NUMA (メモリをどのノードに置くか)。aios はノードが 1 つなので、Linux の NUMA なしと同じく何もせず成功
    MBIND = 235,
    GET_MEMPOLICY = 236,
    SET_MEMPOLICY = 237,
    RT_TGSIGQUEUEINFO = 240,
    PERF_EVENT_OPEN = 241,
    // System V の共有メモリ (shm.rs)
    MSGGET = 186,
    MSGCTL = 187,
    MSGRCV = 188,
    MSGSND = 189,
    SEMGET = 190,
    SEMCTL = 191,
    SEMTIMEDOP = 192,
    SEMOP = 193,
    SHMGET = 194,
    SHMCTL = 195,
    SHMAT = 196,
    SHMDT = 197,
    ACCEPT4 = 242,
    WAIT4 = 260,
    WAITID = 95,
    GETRUSAGE = 165,
    TIMES = 153,
    PIDFD_SEND_SIGNAL = 424,
    PIDFD_OPEN = 434,
    PIDFD_GETFD = 438,
    LANDLOCK_CREATE_RULESET = 444,
    SECCOMP = 277,
    LANDLOCK_ADD_RULE = 445,
    LANDLOCK_RESTRICT_SELF = 446,
    PRLIMIT64 = 261,
    INIT_MODULE = 105,
    DELETE_MODULE = 106,
    KCMP = 272,
    FINIT_MODULE = 273,
    RENAMEAT2 = 276,
    COPY_FILE_RANGE = 285,
    PREADV2 = 286,
    PWRITEV2 = 287,
    STATX = 291,
    GETRANDOM = 278,
    MEMFD_CREATE = 279,
    MEMBARRIER = 283,
    RSEQ = 293,
    EXECVEAT = 281,
    CLOSE_RANGE = 436,
    OPENAT2 = 437,
    FACCESSAT2 = 439,
    FCHMODAT2 = 452,
    LSM_GET_SELF_ATTR = 459,
    LSM_SET_SELF_ATTR = 460,
    LSM_LIST_MODULES = 461,
}

/// システムコールのアーキテクチャ (Linux の syscall_get_arch: seccomp_data の arch、SIGSYS の si_arch、audit の記録の arch=)
pub const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

/// 1 つめの引数が fd のシステムコールで、/proc/bkl がその fd の開いたものの種類 (file::CLASSES) ごとに分けて数えるもの
/// (read がパイプか、ソケットか、ファイルかで、ロックを分ける場所がちがう)
pub const FD_SYS: [u64; 15] = [
    nr::READ,
    nr::WRITE,
    nr::READV,
    nr::WRITEV,
    nr::PREAD64,
    nr::PWRITE64,
    nr::PREADV,
    nr::PWRITEV,
    nr::PREADV2,
    nr::PWRITEV2,
    nr::IOCTL,
    nr::SENDTO,
    nr::RECVFROM,
    nr::SENDMSG,
    nr::RECVMSG,
];

/// システムコールがどの extra のものか (切れるものだけ。parts.rs)
fn part_of(nr: u64) -> Option<crate::parts::Part> {
    use crate::parts::Part;
    use nr::*;
    Some(match nr {
        EPOLL_CREATE1 | EPOLL_CTL | EPOLL_PWAIT | EPOLL_PWAIT2 | EVENTFD2 => Part::Epoll,
        INOTIFY_INIT1 | INOTIFY_ADD_WATCH | INOTIFY_RM_WATCH => Part::Inotify,
        LANDLOCK_CREATE_RULESET | LANDLOCK_ADD_RULE | LANDLOCK_RESTRICT_SELF => Part::Landlock,
        INIT_MODULE | FINIT_MODULE | DELETE_MODULE => Part::Module,
        SECCOMP => Part::Seccomp,
        SWAPON | SWAPOFF => Part::Swap,
        TIMERFD_CREATE | TIMERFD_SETTIME | TIMERFD_GETTIME => Part::Timerfd,
        _ => return None,
    })
}

/// C の int 引数: 下位 32bit を符号拡張する
fn int(v: u64) -> i64 {
    v as i32 as i64
}

// ---- なくても動くところが持つシステムコール (Linux の COND_SYSCALL) ----
// epoll と eventfd、timerfd、inotify (init1 と rm_watch)、socket、module のシステムコールは、それぞれのファイルにあり、main.rs
// が起動のときに register で番号ごとに置く (Linux では入れずに作ると sys_ni_syscall で ENOSYS: ここも置いていなければ、
// ないシステムコールとして ENOSYS)。切ったところ (aios.off=) は、ここより前に part_of が ENOSYS にする

/// 番号ごとに呼ぶもの: (番号, 引数) から答え
pub type Handler = fn(u64, &[u64; 6]) -> Result<i64, i64>;
const NR_TABLE: usize = 512;
static HANDLERS: [core::sync::atomic::AtomicUsize; NR_TABLE] = [const { core::sync::atomic::AtomicUsize::new(0) }; NR_TABLE];

/// 番号 nrs のシステムコールを h で (起動のとき: main.rs)
pub fn register(nrs: &[u64], h: Handler) {
    for &n in nrs {
        HANDLERS[n as usize].store(h as usize, core::sync::atomic::Ordering::Release);
    }
}

/// 大きなロックなしの splice (fast から。None ならふつうの道へ)。main.rs が splice.rs の fast を置く
type FastHandler = fn(&proc::Proc, &[u64; 6]) -> Option<i64>;
static FAST_SPLICE: crate::cell::Early<Option<FastHandler>> = crate::cell::Early::new(None);

pub fn register_fast_splice(f: FastHandler) {
    FAST_SPLICE.set(Some(f));
}

#[inline]
fn handler(nr: u64) -> Option<Handler> {
    let h = HANDLERS.get(nr as usize)?.load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: register が Handler だけを置く
    (h != 0).then(|| unsafe { core::mem::transmute::<usize, Handler>(h) })
}

/// システムコールを動かす。待ちが割り込まれて EINTR になったら、その情報を返す
/// 開いたファイルの位置 (offset) を使うシステムコール (Linux の fdget_pos を使うもの: read、write、readv、writev、
/// lseek、getdents64、preadv2 と pwritev2 (位置 -1)、sendfile、splice、copy_file_range (位置なし)) の fd が、ふつうの
/// ファイルかディレクトリ (Linux の FMODE_ATOMIC_POS) なら、その位置の札 (file::pos_slow)。パイプやソケットでは
/// 取らない (長く眠ることがあるので)
fn pos_guard(nr: u64, a: &[u64; 6]) -> [Option<crate::file::PosSlow>; 2] {
    use nr::*;
    let fds = match nr {
        READ | WRITE | READV | WRITEV | LSEEK | GETDENTS64 | PREADV2 | PWRITEV2 => [Some(a[0]), None],
        SENDFILE => [Some(a[0]), Some(a[1])],
        SPLICE | COPY_FILE_RANGE => [Some(a[0]), Some(a[2])],
        _ => return [None, None],
    };
    let Some(files) = proc::current().files.as_ref() else { return [None, None] };
    fds.map(|fd| {
        let f = files.get().get(fd?)?;
        matches!(f.borrow().kind, crate::file::Kind::Inode(..)).then(|| crate::file::pos_slow(f))
    })
}

pub fn dispatch(tf: &mut TrapFrame) -> Option<Restart> {
    use nr::*;
    // 引数は 6 つ (x0〜x5)。tf.x を 31 ぜんぶ写すと、それだけでシステムコールごとに 248 バイト
    let mut a = [tf.x[0], tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5]];
    let mut nr = tf.x[8];
    crate::debug::trace_sys_enter(nr, a[0]);
    let p = proc::current();
    p.orig_x0 = a[0];
    p.last_sys = (nr, a[0], a[1]);
    p.why.clear();
    crate::debug::count_sys(nr);
    // ヒープが予備にまで手をつけていれば、ここ (安全なところ) で空ける (oom.rs)
    crate::oom::check();
    // スレッドで分けた表から大きなロックなしで閉じたもの (file::fast_close_shared) を落とす (Linux の fput の task_work)
    crate::file::drop_closed();
    // seccomp: かかっていれば、フィルタの答えにしたがう (security::seccomp_permit_syscall。s は写して持つ: user notification
    // で眠るあいだに、ほかのスレッドの TSYNC で入れかわっても消えないように)。tracer が PTRACE_O_SUSPEND_SECCOMP で止めて
    // いれば見ない (CRIU が restore するあいだ)
    if let Some(s) = p.cred.seccomp.clone().filter(|_| !p.seccomp_suspended()) {
        let args = [a[0], a[1], a[2], a[3], a[4], a[5]];
        let mut permit = crate::security::seccomp_permit_syscall(&s, nr, &args, tf.elr);
        // SECCOMP_RET_TRACE (Linux の __seccomp_filter): PTRACE_O_TRACESECCOMP の tracer がいなければ ENOSYS。いれば
        // PTRACE_EVENT_SECCOMP で止まって見せ、tracer が番号を -1 にしたら呼ばない (x0 は tracer が書いたもの)。番号や
        // 引数を変えたかもしれないので、もう一度フィルタに (こんどの TRACE は通す)
        if let crate::security::Permit::Trace(data) = permit {
            let p = proc::current();
            if p.ptracer == 0 || p.pt_opts & signal::PTRACE_O_TRACESECCOMP == 0 {
                permit = crate::security::Permit::Errno(-ENOSYS);
            } else {
                p.pt_scno = None;
                signal::ptrace_event(signal::PTRACE_EVENT_SECCOMP, data as u64);
                proc::check_killed();
                if let Some(n) = proc::current().pt_scno.take() {
                    nr = n;
                }
                if nr as i32 == -1 {
                    crate::debug::trace_sys_exit(nr, tf.x[0] as i64);
                    return None;
                }
                a = [tf.x[0], tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5]];
                permit = match crate::security::seccomp_permit_syscall(&s, nr, &a, tf.elr) {
                    crate::security::Permit::Trace(_) => crate::security::Permit::Allow,
                    v => v,
                };
            }
        }
        match permit {
            crate::security::Permit::Allow => {}
            crate::security::Permit::Trace(_) => unreachable!(),
            crate::security::Permit::Errno(e) => {
                tf.x[0] = e as u64;
                crate::debug::trace_sys_exit(nr, e);
                crate::debug::strace(nr, &a, Err(e));
                crate::why::note(crate::why::Code::Seccomp, e, 0);
                crate::why::done(nr, e, &a);
                return None;
            }
            crate::security::Permit::Trap(errno) => {
                signal::force_sigsys(nr, tf.elr, errno);
                tf.x[0] = (-ENOSYS) as u64;
                crate::debug::trace_sys_exit(nr, -ENOSYS);
                return None;
            }
            crate::security::Permit::Kill => {
                println!("seccomp: pid {} killed at syscall {}", proc::current().pid, nr);
                proc::die(signal::SIGSYS);
            }
            // user notification: listener が答えた値で返す (CONTINUE ならそのままシステムコールを: Allow)
            crate::security::Permit::Return(v) => {
                tf.x[0] = v as u64;
                crate::debug::trace_sys_exit(nr, v);
                crate::debug::strace(nr, &a, if v < 0 && v > -4096 { Err(v) } else { Ok(v) });
                return None;
            }
            // listener を待つあいだにシグナルで割りこまれた: Linux の ERESTARTSYS (RESTART_BEFORE_RECV で受ける前なら
            // ERESTARTNOINTR)
            crate::security::Permit::Interrupted { always } => {
                tf.x[0] = (-EINTR_) as u64;
                crate::debug::trace_sys_exit(nr, -EINTR_);
                return Some(Restart { restartable: true, always });
            }
        }
    }
    // 切ったところ (aios.off=。parts.rs) のシステムコールは、それを入れずに作った Linux と同じ ENOSYS
    if let Some(p) = part_of(nr)
        && crate::parts::off(p)
    {
        tf.x[0] = (-ENOSYS) as u64;
        crate::debug::trace_sys_exit(nr, -ENOSYS);
        crate::debug::strace(nr, &a, Err(-ENOSYS));
        crate::why::note(crate::why::Code::PartOff, -ENOSYS, 0);
        crate::why::done(nr, -ENOSYS, &a);
        return None;
    }
    // わざとしくじらせる (/proc/ai/ctl の fail。fail.rs): 決まりに合えば、動かさずにその errno
    if let Some(e) = crate::debug::should_fail_syscall(nr) {
        tf.x[0] = (-e) as u64;
        crate::debug::trace_sys_exit(nr, -e);
        crate::debug::strace(nr, &a, Err(-e));
        crate::why::note(crate::why::Code::Injected, -e, 0);
        crate::why::done(nr, -e, &a);
        return None;
    }
    // 調べもの (prof on): ファイルのシステムコールは sysfile:NAME の区間に (中の vfs、extfs などの時間は引かれる)
    let _p = crate::debug::sys_span(nr);
    // 監査 (audit_syscall_entry: 入れもののあるスレッドだけ)
    if p.audit.is_some() {
        crate::security::audit_syscall_entry(nr, &a);
    }
    // 監査が名前を集めているなら、パスの名前と親やあるものを前に引く (audit.rs の audit_pre)
    let plan = if p.audit.is_some() && !crate::security::audit_dummy_context() { crate::security::audit_names_pre(nr, &a) } else { None };
    // 開いたファイルの位置を使うもの (Linux の fdget_pos): 終わるまで、大きなロックなしの read などに位置を使わせない
    let _pos = pos_guard(nr, &a);
    let r = match tf.x[8] {
        GETCWD => sysfile::getcwd(a[0] as usize, a[1] as usize),
        CLOSE_RANGE => sysfile::close_range(a[0], a[1], a[2]),
        DUP => sysfile::dup(a[0]),
        DUP3 => sysfile::dup3(a[0], a[1], a[2]),
        FCNTL => sysfile::fcntl(a[0], a[1], a[2]),
        // 要求番号は unsigned int (musl は int を符号拡張して渡してくる)
        IOCTL => sysfile::ioctl(a[0], a[1] & 0xffff_ffff, a[2] as usize),
        FACCESSAT => sysfile::faccessat(int(a[0]), a[1] as usize, a[2], 0),
        FACCESSAT2 => sysfile::faccessat(int(a[0]), a[1] as usize, a[2], a[3]),
        CHDIR => sysfile::chdir(a[0] as usize),
        OPENAT => sysfile::openat(int(a[0]), a[1] as usize, a[2], a[3]),
        OPENAT2 => sysfile::openat2(int(a[0]), a[1] as usize, a[2] as usize, a[3] as usize),
        MKNODAT => sysfile::mknodat(int(a[0]), a[1] as usize, a[2], a[3]),
        MKDIRAT => sysfile::mkdirat(int(a[0]), a[1] as usize, a[2]),
        UNLINKAT => sysfile::unlinkat(int(a[0]), a[1] as usize, a[2]),
        SYMLINKAT => sysfile::symlinkat(a[0] as usize, int(a[1]), a[2] as usize),
        LINKAT => sysfile::linkat(int(a[0]), a[1] as usize, int(a[2]), a[3] as usize, a[4]),
        RENAMEAT => sysfile::renameat(int(a[0]), a[1] as usize, int(a[2]), a[3] as usize, 0),
        RENAMEAT2 => sysfile::renameat(int(a[0]), a[1] as usize, int(a[2]), a[3] as usize, a[4]),
        STATFS => sysfile::statfs(a[0] as usize, a[1] as usize),
        FSTATFS => sysfile::fstatfs(a[0], a[1] as usize),
        TRUNCATE => sysfile::truncate(a[0] as usize, a[1] as i64),
        FTRUNCATE => sysfile::ftruncate(a[0], a[1] as i64),
        FALLOCATE => sysfile::fallocate(a[0], a[1], a[2] as i64, a[3] as i64),
        MEMFD_CREATE => sysfile::memfd_create(a[0] as usize, a[1]),
        FCHDIR => sysfile::fchdir(a[0]),
        CHROOT => sysfile::chroot(a[0] as usize),
        MOUNT => sysfile::mount(a[0] as usize, a[1] as usize, a[2] as usize, a[3], a[4] as usize),
        UMOUNT2 => sysfile::umount2(a[0] as usize, a[1] as u32 as u64),
        PIVOT_ROOT => sysfile::pivot_root(a[0] as usize, a[1] as usize),
        NAME_TO_HANDLE_AT => sysfile::name_to_handle_at(int(a[0]) as i32 as i64, a[1] as usize, a[2] as usize, a[3] as usize, a[4] as u32 as u64),
        OPEN_BY_HANDLE_AT => sysfile::open_by_handle_at(int(a[0]) as i32 as i64, a[1] as usize, a[2] as u32 as u64),
        FCHMOD => sysfile::fchmod(a[0], a[1]),
        FCHMODAT => sysfile::fchmodat(int(a[0]), a[1] as usize, a[2]),
        FCHMODAT2 => sysfile::fchmodat2(int(a[0]), a[1] as usize, a[2], a[3]),
        FCHOWN => sysfile::fchown(a[0], a[1], a[2]),
        FCHOWNAT => sysfile::fchownat(int(a[0]), a[1] as usize, a[2], a[3], a[4]),
        UTIMENSAT => sysfile::utimensat(int(a[0]), a[1] as usize, a[2] as usize, a[3]),
        PREAD64 => sysfile::pread(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PWRITE64 => sysfile::pwrite(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PREADV => sysfile::preadv(a[0], a[1] as usize, a[2] as usize, a[3] as i64, 0),
        PWRITEV => sysfile::pwritev(a[0], a[1] as usize, a[2] as usize, a[3] as i64, 0),
        // (fd, iov, vlen, pos_l, pos_h, flags): 64 ビットでは pos は pos_l (-1 ならいまの位置)
        PREADV2 => sysfile::preadv2(a[0], a[1] as usize, a[2] as usize, a[3] as i64, a[5] as u32),
        PWRITEV2 => sysfile::pwritev2(a[0], a[1] as usize, a[2] as usize, a[3] as i64, a[5] as u32),
        STATX => sysfile::statx(int(a[0]), a[1] as usize, a[2], a[3] as u32, a[4] as usize),
        SYNC => {
            crate::vfs::sync_all_wait();
            Ok(0)
        }
        FSYNC | FDATASYNC | SYNCFS => sysfile::fsync(a[0], nr == SYNCFS),
        SYNC_FILE_RANGE => sysfile::sync_file_range(a[0], a[1] as i64, a[2] as i64, a[3] as u32 as u64),
        CLOSE => sysfile::close(a[0]),
        PIPE2 => sysfile::pipe2(a[0] as usize, a[1]),
        GETDENTS64 => sysfile::getdents64(a[0], a[1] as usize, a[2] as usize),
        LSEEK => sysfile::lseek(a[0], a[1] as i64, a[2]),
        READ => sysfile::read(a[0], a[1] as usize, a[2] as usize),
        WRITE => sysfile::write(a[0], a[1] as usize, a[2] as usize),
        READV => sysfile::readv(a[0], a[1] as usize, a[2] as usize),
        WRITEV => sysfile::writev(a[0], a[1] as usize, a[2] as usize),
        READLINKAT => sysfile::readlinkat(int(a[0]), a[1] as usize, a[2] as usize, a[3] as usize),
        NEWFSTATAT => sysfile::newfstatat(int(a[0]), a[1] as usize, a[2] as usize, a[3]),
        FSTAT => sysfile::fstat(a[0], a[1] as usize),
        PPOLL => signal::wait_mask(a[3] as usize).and_then(|_| sysfile::ppoll(a[0] as usize, a[1] as usize, a[2] as usize)),
        PSELECT6 => signal::pselect_mask(a[5] as usize).and_then(signal::wait_mask).and_then(|_| sysfile::pselect6(a[0] as usize, a[1] as usize, a[2] as usize, a[3] as usize, a[4] as usize)),
        // I/O の優先度はない (どれも同じ)。聞かれたら IOPRIO_CLASS_NONE
        IOPRIO_SET => Ok(0),
        IOPRIO_GET => Ok(0),
        // 拡張属性 (ext4 の user.* と trusted.*。ほかのファイルシステムは EOPNOTSUPP)
        SETXATTR | LSETXATTR => sysfile::setxattr(None, sysfile::AT_FDCWD, a[0] as usize, if nr == LSETXATTR { sysfile::AT_SYMLINK_NOFOLLOW } else { 0 }, a[1] as usize, a[2] as usize, a[3] as usize, a[4]),
        FSETXATTR => sysfile::setxattr(Some(a[0]), 0, 0, 0, a[1] as usize, a[2] as usize, a[3] as usize, a[4]),
        GETXATTR | LGETXATTR => sysfile::getxattr(None, sysfile::AT_FDCWD, a[0] as usize, if nr == LGETXATTR { sysfile::AT_SYMLINK_NOFOLLOW } else { 0 }, a[1] as usize, a[2] as usize, a[3] as usize),
        FGETXATTR => sysfile::getxattr(Some(a[0]), 0, 0, 0, a[1] as usize, a[2] as usize, a[3] as usize),
        LISTXATTR | LLISTXATTR => sysfile::listxattr(None, sysfile::AT_FDCWD, a[0] as usize, if nr == LLISTXATTR { sysfile::AT_SYMLINK_NOFOLLOW } else { 0 }, a[1] as usize, a[2] as usize),
        FLISTXATTR => sysfile::listxattr(Some(a[0]), 0, 0, 0, a[1] as usize, a[2] as usize),
        REMOVEXATTR | LREMOVEXATTR => sysfile::removexattr(None, sysfile::AT_FDCWD, a[0] as usize, if nr == LREMOVEXATTR { sysfile::AT_SYMLINK_NOFOLLOW } else { 0 }, a[1] as usize),
        FREMOVEXATTR => sysfile::removexattr(Some(a[0]), 0, 0, 0, a[1] as usize),
        SETXATTRAT => sysfile::setxattrat(int(a[0]), a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5] as usize),
        GETXATTRAT => sysfile::getxattrat(int(a[0]), a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5] as usize),
        LISTXATTRAT => sysfile::listxattrat(int(a[0]), a[1] as usize, a[2], a[3] as usize, a[4] as usize),
        REMOVEXATTRAT => sysfile::removexattrat(int(a[0]), a[1] as usize, a[2], a[3] as usize),
        // 先読みのお願いは聞くだけ (ext4 の読み込みが自分で先読みする)
        FADVISE64 => sysfile::fadvise(a[0], a[1] as i64, a[2] as i64, a[3]),
        READAHEAD => Ok(0),
        // nice (返り値は Linux と同じく 20 - nice)
        GETPRIORITY => sys_getpriority(a[0] as i32, a[1] as i32),
        SETPRIORITY => sys_setpriority(a[0] as i32, a[1] as i32, a[2] as i32),

        EXIT => proc::exit(a[0] as i32 & 0xff),
        EXIT_GROUP => proc::exit_group(a[0] as i32 & 0xff),
        UNSHARE => proc::unshare(a[0]),
        CLONE => sys_clone(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize).map(|t| crate::ns::to_local(t).unwrap_or(t) as i64),
        CLONE3 => sys_clone3(a[0] as usize, a[1] as usize).map(|t| crate::ns::to_local(t).unwrap_or(t) as i64),
        EXECVE => sys_execve(a[0] as usize, a[1] as usize, a[2] as usize),
        EXECVEAT => sys_execveat(a[0] as i32 as i64, a[1] as usize, a[2] as usize, a[3] as usize, a[4]),
        WAIT4 => crate::ns::arg_pid(int(a[0])).map_err(|_| -10).and_then(|p| sys_wait4(p, a[1] as usize, a[2])),
        GETRUSAGE => sys_getrusage(int(a[0]), a[1] as usize),
        TIMES => sys_times(a[0] as usize),
        WAITID => sys_waitid(a[0], int(a[1]), a[2] as usize, a[3], a[4] as usize),
        PIDFD_OPEN => crate::ns::arg_pid(int(a[0])).and_then(|p| sys_pidfd_open(p, a[1])),
        SECCOMP => crate::security::sys_seccomp(a[0], a[1], a[2] as usize),
        PIDFD_SEND_SIGNAL => crate::pidfd::send_signal(int(a[0]), int(a[1]) as i32, a[2] as usize, a[3] as u32 as u64),
        PIDFD_GETFD => crate::pidfd::getfd(int(a[0]), int(a[1]), a[2] as u32 as u64),
        KILL => crate::ns::arg_pid(int(a[0])).and_then(|p| signal::kill(p, a[1] as i32)),
        // tkill / tgkill: 番号が 0 以下なら EINVAL (Linux)
        TKILL if int(a[0]) <= 0 => Err(crate::why::inval()),
        TGKILL if int(a[0]) <= 0 || int(a[1]) <= 0 => Err(crate::why::inval()),
        TKILL => crate::ns::arg_pid(int(a[0])).and_then(|t| signal::tgkill(0, t as u32, a[1] as i32)),
        TGKILL => crate::ns::arg_pid(int(a[0])).and_then(|g| Ok((g, crate::ns::arg_pid(int(a[1]))?))).and_then(|(g, t)| signal::tgkill(g as u32, t as u32, a[2] as i32)),
        RT_SIGSUSPEND => signal::rt_sigsuspend(a[0] as usize),
        RT_SIGPENDING => signal::rt_sigpending(a[0] as usize),
        RT_SIGTIMEDWAIT => signal::rt_sigtimedwait(a[0] as usize, a[1] as usize, a[2] as usize),
        // sigqueue (値つきのシグナル)。番号は送るほうの PID の namespace のもの
        RT_SIGQUEUEINFO => signal::rt_sigqueueinfo(0, int(a[0]), a[1] as i32, a[2] as usize),
        RT_TGSIGQUEUEINFO => signal::rt_sigqueueinfo(int(a[0]), int(a[1]), a[2] as i32, a[3] as usize),
        RT_SIGRETURN => signal::rt_sigreturn(tf),
        SET_TID_ADDRESS => {
            let p = proc::current();
            p.clear_tid = a[0] as usize;
            Ok(crate::ns::local_or_0(p.pid) as i64)
        }
        FUTEX => sys_futex(a[0] as usize, a[1], a[2] as u32, a[3] as usize, a[4] as usize, a[5] as u32),
        FUTEX_WAITV => crate::futex::waitv(a[0] as usize, a[1] as u32, a[2] as u32, a[3] as usize, a[4] as u32),
        FUTEX_WAKE2 => crate::futex::wake2(a[0] as usize, a[1], a[2] as i32, a[3] as u32),
        FUTEX_WAIT2 => crate::futex::wait2(a[0] as usize, a[1], a[2], a[3] as u32, a[4] as usize, a[5] as i32),
        FUTEX_REQUEUE2 => crate::futex::requeue2(a[0] as usize, a[1] as u32, a[2] as i32, a[3] as i32),
        MSEAL => sys_mseal(a[0] as usize, a[1] as usize, a[2]),
        REMAP_FILE_PAGES => sys_remap_file_pages(a[0] as usize, a[1] as usize, a[2], a[3] as usize, a[4]),
        PROCESS_MADVISE => sys_process_madvise(int(a[0]), a[1] as usize, a[2] as usize, a[3] as i32 as i64 as u64, a[4] as u32 as u64),
        PROCESS_MRELEASE => crate::pidfd::mrelease(int(a[0]), a[1] as u32 as u64),
        SETNS => crate::ns::setns(int(a[0]), a[1] as u32 as u64),
        LISTNS => crate::ns::listns(a[0] as usize, a[1] as usize, a[2] as usize, a[3] as u32 as u64),
        FCHROOT => sysfile::fchroot(int(a[0]), a[1] as u32 as u64),
        FILE_GETATTR => sysfile::file_getattr(int(a[0]), a[1] as usize, a[2] as usize, a[3] as usize, a[4] as u32 as u64),
        FILE_SETATTR => sysfile::file_setattr(int(a[0]), a[1] as usize, a[2] as usize, a[3] as usize, a[4] as u32 as u64),
        // PID の namespace の中なら、中の番号 (ns.rs)
        GETPID => Ok(crate::ns::local_or_0(proc::current().tgid) as i64),
        GETPGID => crate::ns::arg_pid(int(a[0])).and_then(|p| signal::getpgid(p as u32)).map(|g| crate::ns::local_or_0(g as u32) as i64),
        GETSID => crate::ns::arg_pid(int(a[0])).and_then(|p| signal::getsid(p as u32)).map(|g| crate::ns::local_or_0(g as u32) as i64),
        GETTID => Ok(crate::ns::local_or_0(proc::current().pid) as i64),
        GETPPID => Ok(crate::ns::local_or_0(proc::current().ppid) as i64),
        GETUID => Ok(crate::ns::show_uid(proc::current().cred.uid) as i64),
        GETEUID => Ok(crate::ns::show_uid(proc::current().cred.euid) as i64),
        GETGID => Ok(crate::ns::show_gid(proc::current().cred.gid) as i64),
        GETEGID => Ok(crate::ns::show_gid(proc::current().cred.egid) as i64),
        SETUID => cred::setuid(a[0]),
        SETGID => cred::setgid(a[0]),
        SETREUID => cred::setreuid(a[0], a[1]),
        SETREGID => cred::setregid(a[0], a[1]),
        SETRESUID => cred::setresuid(a[0], a[1], a[2]),
        SETRESGID => cred::setresgid(a[0], a[1], a[2]),
        GETRESUID => cred::getresuid(a[0] as usize, a[1] as usize, a[2] as usize),
        GETRESGID => cred::getresgid(a[0] as usize, a[1] as usize, a[2] as usize),
        SETFSUID => cred::setfsuid(a[0]),
        SETFSGID => cred::setfsgid(a[0]),
        GETGROUPS => cred::getgroups(a[0] as usize, a[1] as usize),
        SETGROUPS => cred::setgroups(a[0] as usize, a[1] as usize),
        // 負の pgid は、番号をしらべる前に EINVAL (Linux の setpgid)
        SETPGID if int(a[1]) < 0 => Err(crate::why::inval()),
        SETPGID => crate::ns::arg_pid(int(a[0])).and_then(|p| Ok((p, crate::ns::arg_pid(int(a[1]))?))).and_then(|(p, g)| signal::setpgid(p as u32, g as u32)),
        SETSID => signal::setsid().map(|s| crate::ns::local_or_0(s as u32) as i64),
        UMASK => {
            let f = crate::proc::current().files();
            let old = f.umask;
            f.umask = a[0] as u32 & 0o777;
            Ok(old as i64)
        }
        // robust futex の list の頭を覚える (終わるときと exec に futex::exit_robust_list がたどる)。長さは Linux と同じく
        // struct robust_list_head の 24 バイトだけ。ふだんは fast で答える
        SET_ROBUST_LIST if a[1] != 24 => Err(crate::why::inval()),
        SET_ROBUST_LIST => {
            crate::proc::current().robust_list = a[0] as usize;
            Ok(0)
        }
        GET_ROBUST_LIST => crate::futex::get_robust_list(a[0] as i32, a[1] as usize, a[2] as usize),
        MEMBARRIER => Ok(0),
        RSEQ => Err(-ENOSYS),
        SIGALTSTACK => signal::sigaltstack(a[0] as usize, a[1] as usize),
        RT_SIGPROCMASK => signal::rt_sigprocmask(a[0], a[1] as usize, a[2] as usize),
        RT_SIGACTION => signal::rt_sigaction(a[0] as usize, a[1] as usize, a[2] as usize),
        PRCTL => sys_prctl(a[0], a[1] as usize, a[2] as usize, a[3], a[4]),
        SCHED_YIELD => {
            proc::yield_voluntary();
            Ok(0)
        }
        SCHED_GETAFFINITY => sys_sched_getaffinity(int(a[0]), a[1] as usize, a[2] as usize),
        SCHED_SETAFFINITY => sys_sched_setaffinity(int(a[0]), a[1] as usize, a[2] as usize),
        SCHED_GETSCHEDULER if (a[0] as i32) < 0 => Err(crate::why::inval()),
        SCHED_GETSCHEDULER => sched_target(a[0] as i32).map(|t| t.policy as i64 | if t.reset_on_fork { SCHED_RESET_ON_FORK } else { 0 }),
        SCHED_GETPARAM if a[1] == 0 || (a[0] as i32) < 0 => Err(crate::why::inval()),
        SCHED_GETPARAM => sched_target(a[0] as i32).and_then(|t| out(a[1] as usize, &(t.rt_priority as i32).to_le_bytes())).map(|_| 0),
        SCHED_SETPARAM => do_sched_setscheduler(a[0] as i32, -1, a[1] as usize),
        SCHED_SETSCHEDULER if (a[1] as i32) < 0 => Err(crate::why::inval()),
        SCHED_SETSCHEDULER => do_sched_setscheduler(a[0] as i32, a[1] as i32, a[2] as usize),
        SCHED_SETATTR => sys_sched_setattr(a[0] as i32, a[1] as usize, a[2] as u32),
        SCHED_GETATTR => sys_sched_getattr(a[0] as i32, a[1] as usize, a[2] as u32, a[3] as u32),
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => match a[0] {
            1 | 2 => Ok(if nr == SCHED_GET_PRIORITY_MAX { 99 } else { 1 }),
            0 | 3 | 5 => Ok(0),
            _ => Err(crate::why::inval()),
        },
        NANOSLEEP => sys_nanosleep(1, 0, a[0] as usize, a[1] as usize),
        CLOCK_NANOSLEEP => sys_clock_nanosleep(a[0], a[1], a[2] as usize, a[3] as usize),
        CLOCK_GETTIME => sys_clock_gettime(a[0], a[1] as usize),
        CLOCK_GETRES => sys_clock_getres(a[0], a[1] as usize),
        GETTIMEOFDAY => sys_gettimeofday(a[0] as usize, a[1] as usize),
        SETTIMEOFDAY => sys_settimeofday(a[0] as usize, a[1] as usize),
        SYSLOG => crate::kmsg::syslog(a[0] as i32, a[1] as usize, a[2] as i32),
        CLOCK_SETTIME => sys_clock_settime(a[0] as i32, a[1] as usize),
        ADJTIMEX => sys_adjtimex(a[0] as usize),
        CLOCK_ADJTIME => sys_clock_adjtime(a[0] as i32, a[1] as usize),
        UNAME => sys_uname(a[0] as usize),
        SETHOSTNAME => sys_set_uts(a[0] as usize, a[1], false),
        SETDOMAINNAME => sys_set_uts(a[0] as usize, a[1], true),
        // CPU の番号と NUMA の節 (いつも 0)。渡されたところにだけ書く (Linux と同じ)
        GETCPU => (if a[0] != 0 { out(a[0] as usize, &(crate::smp::id() as u32).to_le_bytes()) } else { Ok(()) })
            .and_then(|_| if a[1] != 0 { out(a[1] as usize, &0u32.to_le_bytes()) } else { Ok(()) })
            .map(|_| 0),
        // SCHED_OTHER の時間の幅。Linux の CFS は、ほかに走るもののない列では 0 (get_rr_interval_fair)、そうでなくても
        // 1 jiffy に満たない slice は 0 になる。aios の線はみな SCHED_OTHER なので 0
        SCHED_RR_GET_INTERVAL => {
            if (a[0] as i32) < 0 {
                Err(crate::why::inval())
            } else if a[0] as i32 != 0 && crate::ns::arg_pid(a[0] as i32 as i64).ok().and_then(|g| proc::find_any(g as u32)).is_none() {
                Err(-3)
            } else {
                out(a[1] as usize, &timespec(0)).map(|_| 0)
            }
        }
        // やりなおすもの (Linux の restart_block) を持たない: do_no_restart_syscall と同じ EINTR。aios は眠りの
        // システムコールを、番号を変えずに同じ引数でやりなおす (signal.rs)
        RESTART_SYSCALL => Err(-4),
        MLOCK => sys_mlock(a[0] as usize, a[1] as usize, false),
        MLOCK2 if a[2] & !1 != 0 => Err(crate::why::inval()),
        MLOCK2 => sys_mlock(a[0] as usize, a[1] as usize, a[2] & 1 != 0),
        MUNLOCK => sys_munlock(a[0] as usize, a[1] as usize),
        MLOCKALL => sys_mlockall(a[0]),
        MUNLOCKALL => {
            let m = proc::current().mm();
            m.def_lock = 0;
            m.pt.set_locked_all(false, false);
            Ok(0)
        }
        // 制御端末を切る: CAP_SYS_TTY_CONFIG がなければ EPERM (Linux と同じ)。持っていても、aios はまだ端末を切らない
        VHANGUP => {
            if p.cred.capable(crate::cred::CAP_SYS_TTY_CONFIG) {
                Ok(0)
            } else {
                Err(-EPERM)
            }
        }
        REBOOT => sys_reboot(a[0] as u32, a[1] as u32, a[2] as u32),
        SYSINFO => sys_sysinfo(a[0] as usize),
        GETRLIMIT => sys_getrlimit(a[0] as u32, a[1] as usize),
        SETRLIMIT => sys_setrlimit(a[0] as u32, a[1] as usize),
        PERSONALITY => sys_personality(a[0] as u32),
        PRLIMIT64 => sys_prlimit(a[0] as i32, a[1] as u32, a[2] as usize, a[3] as usize),

        BRK => Ok(sys_brk(a[0] as usize)),
        MMAP => sys_mmap(a[0] as usize, a[1] as usize, a[2], a[3], int(a[4]), a[5] as usize),
        MUNMAP => sys_munmap(a[0] as usize, a[1] as usize),
        MPROTECT => sys_mprotect(a[0] as usize, a[1] as usize, a[2]),
        MADVISE => sys_madvise(a[0] as usize, a[1] as usize, a[2]),
        MINCORE => sys_mincore(a[0] as usize, a[1] as usize, a[2] as usize),
        MSYNC => sys_msync(a[0] as usize, a[1] as usize, a[2]),
        MREMAP => sys_mremap(a[0] as usize, a[1] as usize, a[2] as usize, a[3], a[4] as usize),
        GETRANDOM => sys_getrandom(a[0] as usize, a[1] as usize, a[2] as u32),
        CAPGET => sys_capget(a[0] as usize, a[1] as usize),
        CAPSET => Err(-crate::cred::EPERM),
        // OpenBLAS (numpy) などが呼ぶ。get_mempolicy は MPOL_DEFAULT (0) を返す
        MBIND | SET_MEMPOLICY => Ok(0),
        GET_MEMPOLICY => {
            if a[0] != 0 {
                let _ = proc::current().pt().copy_out(a[0] as usize, &0i32.to_le_bytes());
            }
            Ok(0)
        }
        // io_uring はない (libuv などは ENOSYS を見て、ふつうのシステムコールで動く)
        IO_URING_SETUP | IO_URING_ENTER | IO_URING_REGISTER => Err(-ENOSYS),
        // なくても動くところ (epoll と eventfd、timerfd、inotify、socket、module) が持つもの (register で置いたもの)
        n => match handler(n) {
            Some(h) => h(n, &a),
            None => {
                // 番号ごとに起動して最初の 1 回だけ (glibc の pthread_create の clone3 のように、毎回呼ぶものがある)
                if unknown_first(n) {
                    let p = proc::current();
                    println!("syscall: unknown {} by {} (pid {}; ENOSYS, shown once per number)", n, p.comm(), p.pid);
                }
                crate::why::note(crate::why::Code::NoSys, -ENOSYS, n);
                Err(-ENOSYS)
            }
        },
    };
    tf.x[0] = r.unwrap_or_else(|e| e) as u64;
    io_account(nr, r);
    crate::debug::trace_sys_exit(nr, r.unwrap_or_else(|e| e));
    if proc::current().audit.is_some() {
        if let Some(plan) = plan {
            crate::security::audit_names_post(plan, &r);
        }
        crate::security::audit_syscall_exit(r.unwrap_or_else(|e| e));
    }
    crate::debug::strace(nr, &a, r);
    // しくじったわけ (whylog.rs)。rseq は glibc が起動のたびに呼んで ENOSYS を見るだけなので入れない
    if let Err(e) = r
        && nr != RSEQ
    {
        crate::why::done(nr, e, &a);
    }
    // ソケットを閉じたなら、送られている途中の輪を片づける (unix.rs の gc)
    crate::file::release_deferred(false);
    // SA_RESTART でやり直してよいもの (Linux で ERESTARTSYS を返すもの)
    let (restartable, always) = match nr {
        FUTEX => crate::futex::restart_kind(a[1], a[3] as usize),
        // IOCTL: 端末の SIGTTOU (tty.rs の check_change) で止まって続けたら、やりなおす
        _ => (matches!(nr, READ | WRITE | READV | WRITEV | OPENAT | WAIT4 | WAITID | FUTEX_WAITV | ACCEPT | ACCEPT4 | RECVFROM | SENDTO | RECVMSG | SENDMSG | CONNECT | IOCTL), false),
    };
    (r == Err(-EINTR_)).then_some(Restart { restartable, always })
}

const EINTR_: i64 = 4;

/// strace で見せるためのユーザーの文字列 (NUL まで、max バイトまで。読めなければ "?")
pub fn user_str(va: usize, max: usize) -> alloc::string::String {
    let pt = proc::current().pt();
    let mut b = Vec::new();
    for i in 0..max {
        let mut c = [0u8];
        if va == 0 || pt.copy_in(&mut c, va + i).is_none() {
            return if b.is_empty() { "?".into() } else { alloc::string::String::from_utf8_lossy(&b).into_owned() };
        }
        if c[0] == 0 {
            break;
        }
        b.push(c[0]);
    }
    alloc::string::String::from_utf8_lossy(&b).into_owned()
}

type R = Result<i64, i64>;

use crate::proc::put_user as out;

// [lockless]
fn timespec(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    b
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_REALTIME_COARSE: u64 = 5;

/// 大きなロックなしで済むシステムコール: 自分のことを読むだけのもの (smp.rs)。済ませたら true。
/// ユーザーのメモリに書くのは、もう写っていて書けるページだけ (なければ false でふつうの道へ)。
/// シグナルや kill はほかの CPU から割り込み (IPI) で来るので、ここでは見なくてよい。
/// 大きなロックで守られているもの (ファイルシステムの RefCell: ext4 の inode の覚えなど) は借りないこと: ほかの CPU が
/// ロックを持って書いていれば "already borrowed" で止まる (前の fast_splice。test/vm-check.sh の fast-smp)
// [lockless]
pub fn fast(tf: &mut TrapFrame) -> bool {
    use nr::*;
    // すべてのシステムコールがまずここを通るので、写すのは引数 (x0〜x5) と番号だけ
    let a = [tf.x[0], tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5]];
    let nr = tf.x[8];
    let p = proc::current();
    // seccomp がかかっていれば、フィルタを通すためにふつうの道へ。PID の namespace の中も (番号を読みかえる)
    // LSM のラベルのついたもの (AppArmor のプロファイル) も (決まりを見るのはふつうの道だけ)
    // 監査のシステムコールのルールがあれば、入れもののあるスレッドも (入口と出口でルールを見る)
    if p.has_cred_work(proc::CRED_SECCOMP | proc::CRED_PTRACE) || p.pid_ns.is_some() || p.cred.ns.user.is_some() || p.cred.label.is_some() || (p.audit.is_some() && crate::security::AUDIT_N_RULES.load(core::sync::atomic::Ordering::Relaxed) != 0) {
        return false;
    }
    let r = match nr {
        GETPID => p.tgid as i64,
        GETTID => p.pid as i64,
        GETPPID => p.ppid as i64,
        // 自分 (代表) のグループとセッション。Linux も大きなロックなしに読む (rcu_read_lock だけ)。
        // スレッドは代表を表から探すので、ふつうの道へ
        GETPGID | GETSID if a[0] == 0 && p.is_leader() => if nr == GETPGID { p.pgid as i64 } else { p.sid as i64 },
        // glibc はスレッドのはじめと fork した子でかならず呼ぶ (前は fork のたびに大きなロックを 7.5 µs 持っていた)。
        // 自分の Proc に頭を覚えるだけ (長さがちがえば EINVAL のわけを残すふつうの道へ)
        SET_ROBUST_LIST if a[1] == 24 => {
            p.robust_list = a[0] as usize;
            0
        }
        GETUID => p.cred.uid as i64,
        GETEUID => p.cred.euid as i64,
        GETGID => p.cred.gid as i64,
        GETEGID => p.cred.egid as i64,
        // 自分のスレッドの CPU の時計 (3) まで。プロセスやほかのものの CPU の時計、知らない時計はふつうの道へ
        CLOCK_GETTIME if matches!(a[0], 0 | 1 | 3..=7 | 11) => {
            let ns = match a[0] {
                CLOCK_REALTIME | CLOCK_REALTIME_COARSE | 11 => timer::epoch_ns(),
                3 => proc::cpu_ns(p),
                _ => timer::uptime_ns(),
            };
            if !p.pt().copy_out_nofault(a[1] as usize, &timespec(ns)) {
                return false;
            }
            0
        }
        // 自分のアドレス空間だけの futex (PRIVATE) で、眠らない・起こさないもの (futex.rs も見よ)。時間のあるもの、
        // そろっていないもの、bitset が 0 のもの、CLOCK_REALTIME の印のあるものは、答えのちがう (EINVAL など) ことが
        // あるので、ふつうの道へ
        FUTEX if a[1] & !(FUTEX_PRIVATE_FLAG | 0x7f) == 0 && a[1] & FUTEX_PRIVATE_FLAG != 0 && a[0] % 4 == 0 && a[0] < crate::vm::MAXVA as u64 => match a[1] & 0x7f {
            FUTEX_WAIT | FUTEX_WAIT_BITSET if a[3] == 0 && (a[1] & 0x7f == FUTEX_WAIT || a[5] as u32 != 0) => {
                // 値がもう val でなければ EAGAIN (Linux と同じ)。同じなら、ロックを取って眠る道へ
                let mut cur = [0u8; 4];
                if !p.pt().copy_in_nofault(a[0] as usize, &mut cur) || u32::from_le_bytes(cur) == a[2] as u32 {
                    return false;
                }
                -EAGAIN
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET if a[1] & 0x7f == FUTEX_WAKE || a[5] as u32 != 0 => {
                // 起こす側はユーザーが値を書いたあとに来る。眠る側は数を増やしてから値を読む (futex.rs) ので、
                // ここで 0 なら、眠っている (これから眠る) スレッドはいない: だれも起こさずに 0
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                if crate::futex::sleepers(p.mm_id(), a[0] as usize).load(core::sync::atomic::Ordering::SeqCst) != 0 {
                    return false;
                }
                0
            }
            _ => return false,
        },
        // パスの覚え (statc.rs) にある stat と statx
        READLINKAT => match crate::statc::lookup_readlink(p, a[0] as i32 as i64, a[1] as usize, a[3] as usize) {
            Some(r) => r,
            None => return false,
        },
        NEWFSTATAT => match crate::statc::lookup(p, a[0] as i32 as i64, a[1] as usize, a[2] as usize, a[3]) {
            Some(r) => r,
            None => return false,
        },
        STATX => match crate::statc::lookup_statx(p, a[0] as i32 as i64, a[1] as usize, a[2], a[3] as u32, a[4] as usize) {
            Some(r) => r,
            None => return false,
        },
        // 動かさずに伸ばす mremap (flags 0) で、伸ばせない (ENOMEM) か古い場所に領域がない (EFAULT) と表を読んで分かるもの
        // (Firefox が 1 ページの領域で何万回も)。伸ばせるもの、縮める・動かす・知らない形は、ふつうの道 (sys_mremap) へ
        MREMAP if a[3] == 0 && a[0] & 0xfff == 0 && a[1] != 0 && a[2] > a[1] && a[2] < crate::vm::MAXVA as u64 => {
            let (old, old_size, new_size) = (a[0] as usize, pg_up(a[1] as usize), pg_up(a[2] as usize));
            let (Some(old_end), Some(new_end)) = (old.checked_add(old_size), old.checked_add(new_size)) else { return false };
            if new_size <= old_size || new_end > crate::vm::MAXVA {
                return false;
            }
            match p.pt().fast_mremap_fail(old, old_end, new_end) {
                Some(r) => r,
                None => return false,
            }
        }
        // 自分だけの無名の mmap (MAP_FIXED でないもの) と、自分だけの無名とファイルの写し (MAP_PRIVATE。ファイルはまるごと)
        // の munmap (vm::PageTable::fast_map、fast_unmap)。スレッドで分けた表は、ロックなしの読みと大きなロックを持った
        // 同じ表のスレッドがいないときだけ、ひとりで変える (fast_map_threads、fast_unmap の threads: write_enter)。
        // ファイルの mmap、共有、MAP_FIXED、スワップへ出したページのあるところ、知らない印や prot は、ふつうの道
        // (sys_mmap、sys_munmap) へ
        // perf の MMAP の記録をとるものがあれば、ふつうの道で (記録を書く)
        MMAP if a[3] & !(MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK) == 0
            && crate::proc::PERF_MMAP.load(core::sync::atomic::Ordering::Relaxed) == 0
            && a[3] & (MAP_PRIVATE | MAP_ANONYMOUS) == MAP_PRIVATE | MAP_ANONYMOUS
            && a[2] & !7 == 0
            && a[5] % 4096 == 0
            && a[1] != 0
            && a[1] <= crate::vm::MAXVA as u64 =>
        {
            let Some(mm) = p.mm.as_ref() else { return false };
            let m = mm.get();
            // mlockall の MCL_FUTURE はふつうの道で (かけて写す)
            if m.def_lock != 0 {
                return false;
            }
            let len = pg_up(a[1] as usize);
            if mm.private() {
                let Some(va) = m.pt.fast_map(m.mmap_next, len, prot_bits(a[2])) else { return false };
                m.mmap_next = va + len;
                va as i64
            } else {
                let next = core::ptr::addr_of_mut!(m.mmap_next);
                let Some(va) = m.pt.fast_map_threads(next, len, prot_bits(a[2])) else { return false };
                va as i64
            }
        }
        // ふつうのファイルの写し (MAP_PRIVATE、MAP_FIXED でないもの): ext4 なら同じ inode の新しい InodeRef で
        // (vm::fast_mmap_file)。できなければふつうの道 (エラーもそこで)
        MMAP if a[3] & !(MAP_PRIVATE | MAP_NORESERVE) == 0
            && crate::proc::PERF_MMAP.load(core::sync::atomic::Ordering::Relaxed) == 0
            && a[3] & MAP_PRIVATE != 0
            && a[2] & !7 == 0
            && a[5] % 4096 == 0
            && a[1] != 0
            && a[1] <= crate::vm::MAXVA as u64
            && (a[4] as i32) >= 0 =>
        {
            if p.mm.as_ref().is_some_and(|m| m.get().def_lock != 0) {
                return false;
            }
            match crate::vm::fast_mmap_file(p, a[4] as i32 as u64, a[1] as usize, a[2], a[5] as usize) {
                Some(r) => r,
                None => return false,
            }
        }
        MUNMAP if a[0] & 0xfff == 0 && a[1] != 0 && a[0] < crate::vm::MAXVA as u64 && a[1] <= crate::vm::MAXVA as u64 => {
            let Some(m) = p.mm.as_ref() else { return false };
            match m.get().pt.fast_unmap(a[0] as usize, a[0] as usize + pg_up(a[1] as usize), !m.private()) {
                Some(()) => 0,
                None => return false,
            }
        }
        // fork (CLONE_VM のない clone): 自分だけの表なら、PTE の写しだけ先にここで作っておく (vm::PageTable::prefork)。
        // clone そのものは大きなロックの道で (proc::clone が使う)
        CLONE => {
            if a[0] & proc::CLONE_VM == 0 {
                p.prefork = p.mm.as_ref().filter(|m| m.private()).and_then(|m| m.get().pt.prefork());
            }
            return false;
        }
        LSEEK => match crate::file::fast_lseek(p, a[0], a[1] as i64, a[2]) {
            Some(r) => r,
            None => return false,
        },
        FCNTL => match crate::file::fast_fcntl(p, a[0], a[1], a[2]) {
            Some(r) => r,
            None => return false,
        },
        GETCWD => match crate::file::fast_getcwd(p, a[0] as usize, a[1] as usize) {
            Some(r) => r,
            None => return false,
        },
        CLOSE if crate::file::fast_close_badf(p, a[0]) => -EBADF,
        // 読むだけで開いた ext4 のファイルの close と、それをまた開く openat (file::Shelf)、ない (ENOENT) ものの openat
        CLOSE => match crate::file::fast_close(p, a[0]) {
            Some(r) => r,
            None => return false,
        },
        OPENAT => match crate::file::fast_open(p, a[0] as i32 as i64, a[1] as usize, a[2]).or_else(|| crate::fastdir::fast_create(p, a[0] as i32 as i64, a[1] as usize, a[2], a[3])) {
            Some(r) => r,
            None => return false,
        },
        // ext4 のふつうのファイルを作る・消す (fastdir.rs: ext4 の中は ext4 のロックで 1 つずつ)
        UNLINKAT => match crate::fastdir::fast_unlink(p, a[0] as i32 as i64, a[1] as usize, a[2]) {
            Some(r) => r,
            None => return false,
        },
        // umask は fd の表 (Linux の fs_struct と同じくスレッドで分ける) のもの。だれとも分けていなければ、変えるのも自分だけ
        UMASK => match p.files.as_ref().filter(|f| f.private()) {
            Some(f) => {
                let t = f.get();
                let old = t.umask;
                t.umask = a[0] as u32 & 0o777;
                old as i64
            }
            None => return false,
        },
        // ルートの ext4 のディレクトリを読む getdents64 (file::fast_getdents)
        GETDENTS64 => match crate::file::fast_getdents(p, a[0], a[1] as usize, a[2] as usize) {
            Some(r) => r,
            None => return false,
        },
        // ページキャッシュに書くだけの pwrite64 (vm::write_fast)
        PWRITE64 => match crate::file::fast_pwrite(p, a[0], a[1] as usize, a[2] as usize, a[3] as i64) {
            Some(r) => r,
            None => return false,
        },
        // ページキャッシュから読む pread64、ファイル → パイプ・パイプ → /dev/null の splice (眠らないもの)
        PREAD64 => match crate::file::fast_pread(p, a[0], a[1] as usize, a[2] as usize, a[3] as i64) {
            Some(r) => r,
            None => return false,
        },
        SPLICE => match FAST_SPLICE.val().and_then(|f| f(p, &a)) {
            Some(r) => r,
            None => return false,
        },
        // パイプの読み書きで、眠らず起こさずにすむもの (file::Pipe::fast_rw)
        READ | WRITE => match crate::file::Pipe::fast_rw(p, a[0], a[1] as usize, a[2] as usize, nr == WRITE)
            .or_else(|| crate::file::fast_rw_sock(p, a[0], a[1] as usize, a[2] as usize, nr == WRITE))
            .or_else(|| if nr == READ { crate::file::fast_read_file(p, a[0], a[1] as usize, a[2] as usize) } else { None })
            .or_else(|| if nr == WRITE { crate::file::fast_write_file(p, a[0], a[1] as usize, a[2] as usize) } else { None })
            .or_else(|| crate::file::fast_dev(p, a[0], a[1] as usize, a[2] as usize, nr == WRITE))
        {
            Some(r) => r,
            None => return false,
        },
        _ => return false,
    };
    tf.x[0] = r as u64;
    io_account(nr, if r < 0 && r > -4096 { Err(r) } else { Ok(r) });
    // /proc/sysstat を読みはじめてから、速い道で終わったものだけ数える
    crate::debug::count_fast(nr);
    crate::debug::trace_sys_fast(nr, a[0], r);
    true
}

/// 読み書きのシステムコールを /proc/PID/io に数える (Linux の vfs_read / vfs_write の add_rchar と inc_syscr、
/// do_sendfile と vfs_copy_file_range は両方)。中身の read / write の前にしくじったもの (fd がない・読めない EBADF、
/// 位置のないものへの pread の ESPIPE) は数えない
// [lockless]
fn io_account(nr: u64, r: Result<i64, i64>) {
    use nr::*;
    const ESPIPE: i64 = 29;
    if matches!(r, Err(e) if e == -EBADF || e == -ESPIPE) {
        return;
    }
    let n = r.map(|n| n as usize);
    match nr {
        READ | READV | PREAD64 | PREADV | PREADV2 => proc::io_read(n),
        WRITE | WRITEV | PWRITE64 | PWRITEV | PWRITEV2 => proc::io_write(n),
        SENDFILE | COPY_FILE_RANGE => {
            proc::io_read(n);
            proc::io_write(n);
        }
        _ => {}
    }
}

/// clock_gettime: 時計は timer::clock_of (知らないもの、いない CPU の時計は EINVAL)
fn sys_clock_gettime(clk: u64, ts: usize) -> R {
    let ns = timer::clock_ns(timer::clock_of(clk as i32, true)?)?;
    out(ts, &timespec(ns))?;
    Ok(0)
}

/// clock_getres: 壁と起動からの時計と CPU の時計 (SCHED) は 1 ns、COARSE は tick。ts は NULL でもよい
fn sys_clock_getres(clk: u64, ts: usize) -> R {
    let c = timer::clock_of(clk as i32, false)?;
    if ts != 0 {
        out(ts, &timespec(timer::clock_res(c, clk as i32)))?;
    }
    Ok(0)
}

fn sys_gettimeofday(tv: usize, tz: usize) -> R {
    if tv != 0 {
        let ns = timer::epoch_ns();
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        out(tv, &b)?;
    }
    if tz != 0 {
        let (w, d) = sys_tz();
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&w.to_le_bytes());
        b[4..].copy_from_slice(&d.to_le_bytes());
        out(tz, &b)?;
    }
    Ok(0)
}

// ---- 時計を合わせる (Linux の kernel/time/time.c、posix-timers.c、ntp.c) ----

const CAP_SYS_TIME: u32 = 25;

/// settimeofday の tz (Linux の sys_tz: 分 (西へ) と夏時間の種類)。はじめは 0 0
static SYS_TZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn sys_tz() -> (i32, i32) {
    let v = SYS_TZ.load(core::sync::atomic::Ordering::Relaxed);
    (v as u32 as i32, (v >> 32) as u32 as i32)
}

/// 時計を合わせる (Linux の do_sys_settimeofday64): 時刻がおかしければ EINVAL (秒が負、ns が 1 秒以上、遠すぎる先)、
/// CAP_SYS_TIME がなければ EPERM (tz だけでも)、tz の分が ±15 時間の外なら EINVAL
fn do_settime(ts: Option<(i64, i64)>, tz: Option<(i32, i32)>) -> R {
    const TIME_SETTOD_SEC_MAX: i64 = 9_223_372_036 - 946_080_000;
    if let Some((s, ns)) = ts {
        if s < 0 || !(0..1_000_000_000).contains(&ns) || s >= TIME_SETTOD_SEC_MAX {
            return Err(crate::why::inval());
        }
    }
    if !proc::current().cred.capable(CAP_SYS_TIME) {
        return Err(-EPERM);
    }
    if let Some((w, d)) = tz {
        if !(-15 * 60..=15 * 60).contains(&w) {
            return Err(crate::why::inval());
        }
        SYS_TZ.store(w as u32 as u64 | (d as u32 as u64) << 32, core::sync::atomic::Ordering::Relaxed);
    }
    if let Some((s, ns)) = ts {
        timer::set_realtime(s as u64 * 1_000_000_000 + ns as u64);
        ntp_clear();
    }
    Ok(0)
}

/// 時計を合わせたら NTP の状態を消す (Linux の ntp_clear: 合わせていないことに)
fn ntp_clear() {
    let n = NTP.get();
    n.status |= 0x40;
    n.maxerror = 16_000_000;
    n.esterror = 16_000_000;
    n.adjust = 0;
    n.offset = 0;
}

/// settimeofday (読めなければ EFAULT、µs が 0..1e6 の外か秒が負なら EINVAL)
fn sys_settimeofday(tv: usize, tz: usize) -> R {
    let pt = proc::current().pt();
    let ts = if tv != 0 {
        let mut b = [0u8; 16];
        pt.copy_in(&mut b, tv).ok_or(-EFAULT)?;
        let s = i64::from_le_bytes(b[..8].try_into().unwrap());
        let us = i64::from_le_bytes(b[8..].try_into().unwrap());
        if s < 0 || !(0..1_000_000).contains(&us) {
            return Err(crate::why::inval());
        }
        Some((s, us * 1000))
    } else {
        None
    };
    let z = if tz != 0 {
        let mut b = [0u8; 8];
        pt.copy_in(&mut b, tz).ok_or(-EFAULT)?;
        Some((i32::from_le_bytes(b[..4].try_into().unwrap()), i32::from_le_bytes(b[4..].try_into().unwrap())))
    } else {
        None
    };
    do_settime(ts, z)
}

/// clock_settime: 合わせられるのは CLOCK_REALTIME だけ (ほかの時計は EINVAL。CPU の時計は Linux と同じく、いなければ
/// EINVAL、いれば EPERM)。読めなければ EFAULT
fn sys_clock_settime(clk: i32, tp: usize) -> R {
    if clk > 0 {
        return Err(crate::why::inval());
    }
    let mut b = [0u8; 16];
    proc::current().pt().copy_in(&mut b, tp).ok_or(-EFAULT)?;
    if clk < 0 {
        return if clk & 7 == 3 || timer::clock_of(clk, false).is_err() { Err(crate::why::inval()) } else { Err(-EPERM) };
    }
    do_settime(Some((i64::from_le_bytes(b[..8].try_into().unwrap()), i64::from_le_bytes(b[8..].try_into().unwrap()))), None)
}

/// NTP の値 (Linux の ntp.c の time_status、time_maxerror、time_esterror、time_constant、time_freq、time_offset、
/// tick_usec、time_tai)。aios は覚えて答えるだけで、時計の進みは変えない (ADJ_SETOFFSET だけは時計を動かす)。
/// maxerror は Linux と同じく 1 秒ごとに 500 µs ずつ増え、16 秒で STA_UNSYNC にもどる
struct Ntp {
    status: i32,
    maxerror: i64,
    maxerror_at: u64,
    esterror: i64,
    constant: i64,
    freq: i64,
    offset: i64,
    tick: i64,
    tai: i32,
    adjust: i64,
}

static NTP: crate::cell::Bkl<Ntp> =
    crate::cell::Bkl::new(Ntp { status: 0x40, maxerror: 16_000_000, maxerror_at: 0, esterror: 16_000_000, constant: 2, freq: 0, offset: 0, tick: 10_000, tai: 0, adjust: 0 });

/// adjtimex の中身 (Linux の do_adjtimex): b は struct __kernel_timex (208 バイト)。答えは時計の状態 (TIME_OK 0、
/// TIME_ERROR 5: 合わせていない)
fn do_adjtimex(b: &mut [u8; 208]) -> R {
    const ADJ_OFFSET: u32 = 0x1;
    const ADJ_FREQUENCY: u32 = 0x2;
    const ADJ_MAXERROR: u32 = 0x4;
    const ADJ_ESTERROR: u32 = 0x8;
    const ADJ_STATUS: u32 = 0x10;
    const ADJ_TIMECONST: u32 = 0x20;
    const ADJ_TAI: u32 = 0x80;
    const ADJ_SETOFFSET: u32 = 0x100;
    const ADJ_MICRO: u32 = 0x1000;
    const ADJ_NANO: u32 = 0x2000;
    const ADJ_TICK: u32 = 0x4000;
    const ADJ_ADJTIME: u32 = 0x8000;
    const ADJ_OFFSET_READONLY: u32 = 0x2000;
    const STA_PLL: i32 = 0x1;
    const STA_UNSYNC: i32 = 0x40;
    const STA_CLOCKERR: i32 = 0x1000;
    const STA_NANO: i32 = 0x2000;
    const STA_RONLY: i32 = 0xff00;
    const NTP_PHASE_LIMIT: i64 = 16_000_000;
    const MAXFREQ_PPM: i64 = 32_768_000;
    let g64 = |b: &[u8; 208], o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let modes = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let cap = || proc::current().cred.capable(CAP_SYS_TIME);
    // たしかめ (Linux の timekeeping_validate_timex)
    if modes & ADJ_ADJTIME != 0 {
        if modes & ADJ_OFFSET == 0 {
            return Err(crate::why::inval());
        }
        if modes & ADJ_OFFSET_READONLY == 0 && !cap() {
            return Err(-EPERM);
        }
    } else {
        if modes != 0 && !cap() {
            return Err(-EPERM);
        }
        if modes & ADJ_TICK != 0 && !(9000..=11000).contains(&g64(b, 88)) {
            return Err(crate::why::inval());
        }
    }
    if modes & ADJ_SETOFFSET != 0 {
        if !cap() {
            return Err(-EPERM);
        }
        let frac = g64(b, 80);
        if frac < 0 || frac >= if modes & ADJ_NANO != 0 { 1_000_000_000 } else { 1_000_000 } {
            return Err(crate::why::inval());
        }
    }
    if modes & ADJ_FREQUENCY != 0 {
        let f = g64(b, 16);
        if f.checked_mul(65_536_000).is_none() {
            return Err(crate::why::inval());
        }
    }
    let n = NTP.get();
    // maxerror は 1 秒ごとに 500 µs 増える (Linux の second_overflow)
    let now_s = timer::uptime_ns() / 1_000_000_000;
    if n.maxerror_at == 0 {
        n.maxerror_at = now_s;
    }
    n.maxerror = (n.maxerror + 500 * (now_s - n.maxerror_at) as i64).min(NTP_PHASE_LIMIT);
    n.maxerror_at = now_s;
    if n.maxerror >= NTP_PHASE_LIMIT {
        n.status |= STA_UNSYNC;
    }
    if modes & ADJ_SETOFFSET != 0 {
        let (s, frac) = (g64(b, 72), g64(b, 80));
        let delta = s as i128 * 1_000_000_000 + if modes & ADJ_NANO != 0 { frac } else { frac * 1000 } as i128;
        let now = timer::epoch_ns() as i128 + delta;
        if now < 0 {
            return Err(crate::why::inval());
        }
        timer::set_realtime(now as u64);
        ntp_clear();
    }
    let offset_out;
    if modes & ADJ_ADJTIME != 0 {
        offset_out = n.adjust;
        if modes & ADJ_OFFSET_READONLY == 0 {
            n.adjust = g64(b, 8);
        }
    } else {
        if modes & ADJ_STATUS != 0 {
            let st = i32::from_le_bytes(b[40..44].try_into().unwrap());
            n.status = (n.status & STA_RONLY) | (st & !STA_RONLY);
        }
        if modes & ADJ_NANO != 0 {
            n.status |= STA_NANO;
        }
        if modes & ADJ_MICRO != 0 {
            n.status &= !STA_NANO;
        }
        if modes & ADJ_FREQUENCY != 0 {
            n.freq = g64(b, 16).clamp(-MAXFREQ_PPM, MAXFREQ_PPM);
        }
        if modes & ADJ_MAXERROR != 0 {
            n.maxerror = g64(b, 24).clamp(0, NTP_PHASE_LIMIT);
        }
        if modes & ADJ_ESTERROR != 0 {
            n.esterror = g64(b, 32).clamp(0, NTP_PHASE_LIMIT);
        }
        if modes & ADJ_TIMECONST != 0 {
            let mut c = g64(b, 48).clamp(0, 10);
            if n.status & STA_NANO == 0 {
                c += 4;
            }
            n.constant = c.clamp(0, 10);
        }
        if modes & ADJ_TAI != 0 {
            let c = g64(b, 48);
            if (0..=10_000_000).contains(&c) {
                n.tai = c as i32;
            }
        }
        if modes & ADJ_OFFSET != 0 && n.status & STA_PLL != 0 {
            n.offset = g64(b, 8);
        }
        if modes & ADJ_TICK != 0 {
            n.tick = g64(b, 88);
        }
        offset_out = if n.status & STA_PLL != 0 { n.offset } else { 0 };
    }
    let result = if n.status & (STA_UNSYNC | STA_CLOCKERR) != 0 { 5 } else { 0 };
    let now = timer::epoch_ns();
    let put = |b: &mut [u8; 208], o: usize, v: i64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    put(b, 8, offset_out);
    put(b, 16, n.freq);
    put(b, 24, n.maxerror);
    put(b, 32, n.esterror);
    b[40..44].copy_from_slice(&n.status.to_le_bytes());
    put(b, 48, n.constant);
    put(b, 56, 1);
    put(b, 64, MAXFREQ_PPM);
    put(b, 72, (now / 1_000_000_000) as i64);
    put(b, 80, if n.status & STA_NANO != 0 { (now % 1_000_000_000) as i64 } else { (now % 1_000_000_000 / 1000) as i64 });
    put(b, 88, n.tick);
    for o in [96, 104, 120, 128, 136, 144, 152] {
        put(b, o, 0);
    }
    b[112..116].copy_from_slice(&0i32.to_le_bytes());
    b[160..164].copy_from_slice(&n.tai.to_le_bytes());
    Ok(result)
}

/// adjtimex (読めなければ EFAULT。答えはエラーでも書きもどす: Linux と同じ)
fn sys_adjtimex(p: usize) -> R {
    let mut b = [0u8; 208];
    let pt = proc::current().pt();
    pt.copy_in(&mut b, p).ok_or(-EFAULT)?;
    let r = do_adjtimex(&mut b);
    out(p, &b)?;
    r
}

/// clock_adjtime: CLOCK_REALTIME は adjtimex と同じ。ほかのある時計は EOPNOTSUPP、知らない時計は EINVAL。書きもどすのは
/// うまくいったときだけ
fn sys_clock_adjtime(clk: i32, p: usize) -> R {
    let mut b = [0u8; 208];
    proc::current().pt().copy_in(&mut b, p).ok_or(-EFAULT)?;
    match clk {
        0 => {}
        1..=9 | 11 => return Err(-EOPNOTSUPP),
        c if c < 0 && c & 7 != 3 => return Err(-EOPNOTSUPP),
        _ => return Err(crate::why::inval()),
    }
    let r = do_adjtimex(&mut b)?;
    out(p, &b)?;
    Ok(r)
}

/// uname の nodename (sethostname で変わる。init が起動のときに /etc/hostname を入れる)

/// sethostname と setdomainname (Linux と同じ順: 許しがなければ EPERM、長さ (int) が 0 より小さいか 64 より大きければ
/// EINVAL、読めなければ EFAULT)。root か、新しく作った UTS の中 (ns.rs)
fn sys_set_uts(name: usize, len: u64, domain: bool) -> R {
    if !crate::ns::uts_may_set() {
        return Err(-EPERM);
    }
    let len = len as i32;
    if !(0..=64).contains(&len) {
        return Err(crate::why::inval());
    }
    let len = len as usize;
    let mut b = [0u8; 64];
    proc::current().pt().copy_in(&mut b[..len], name).ok_or(-14)?;
    crate::ns::set_uts_name(&b[..len], domain, false)?;
    Ok(0)
}

// ---- mlock (Linux の mm/mlock.c) ----

const RLIMIT_MEMLOCK: usize = 8;

/// mlock してよいか (Linux の can_do_mlock): RLIMIT_MEMLOCK が 0 でないか、CAP_IPC_LOCK
fn can_do_mlock(p: &proc::Proc) -> bool {
    proc::rlimit(p, RLIMIT_MEMLOCK)[0] != 0 || p.cred.capable(crate::cred::CAP_IPC_LOCK)
}

/// RLIMIT_MEMLOCK (ソフト) のページの数
fn memlock_pages(p: &proc::Proc) -> u64 {
    proc::rlimit(p, RLIMIT_MEMLOCK)[0] >> 12
}

/// これから bytes を mlock して写してよいか (Linux の mlock_future_ok: かけないもの、CAP_IPC_LOCK は いつもよい)
fn mlock_future_ok(p: &proc::Proc, locked: bool, bytes: usize) -> bool {
    if !locked {
        return true;
    }
    let pages = (bytes >> 12) as u64 + p.mm().pt.locked_pages() as u64;
    pages <= memlock_pages(p) || p.cred.capable(crate::cred::CAP_IPC_LOCK)
}

/// mlock と mlock2 (Linux の do_mlock): RLIMIT_MEMLOCK が 0 で CAP_IPC_LOCK もなければ EPERM。番地のページの端数は長さに
/// 足す (そろえてあふれれば 0)。かけたものと合わせて RLIMIT_MEMLOCK をこえれば ENOMEM (範囲のうちもうかけたところは
/// 数えなおさない)。終わりがあふれれば EINVAL、領域がなければ ENOMEM (すきまの前まではかける)。それからページを写す
/// (写せなければ ENOMEM: PROT_NONE など。かけたまま)
fn sys_mlock(start: usize, len: usize, onfault: bool) -> R {
    const ENOMEM: i64 = 12;
    let p = proc::current();
    if !can_do_mlock(p) {
        return Err(-EPERM);
    }
    let len = len.wrapping_add(start & 0xfff).wrapping_add(0xfff) & !0xfff;
    let start = start & !0xfff;
    let limit = memlock_pages(p);
    let pt = &mut p.mm().pt;
    let mut locked = (len >> 12) as u64 + pt.locked_pages() as u64;
    if locked > limit && !p.cred.capable(crate::cred::CAP_IPC_LOCK) {
        locked -= pt.locked_in(start, start.wrapping_add(len)) as u64;
    }
    if locked > limit && !p.cred.capable(crate::cred::CAP_IPC_LOCK) {
        return Err(-ENOMEM);
    }
    let end = start.checked_add(len).ok_or_else(crate::why::inval)?;
    if end == start {
        return Ok(0);
    }
    pt.set_locked(start, end, true, onfault)?;
    if !pt.populate_locked(start, end, false) {
        return Err(-ENOMEM);
    }
    Ok(0)
}

/// munlock (Linux と同じく許しは要らない。領域がなければ ENOMEM、すきまの前までは外す)
fn sys_munlock(start: usize, len: usize) -> R {
    let len = len.wrapping_add(start & 0xfff).wrapping_add(0xfff) & !0xfff;
    let start = start & !0xfff;
    let end = start.checked_add(len).ok_or_else(crate::why::inval)?;
    if end == start {
        return Ok(0);
    }
    proc::current().mm().pt.set_locked(start, end, false, false).map(|_| 0)
}

/// mlockall (Linux と同じ順: flags が 0 か、知らないものか、MCL_ONFAULT だけなら EINVAL、can_do_mlock でなければ
/// EPERM、MCL_CURRENT で VmSize が RLIMIT_MEMLOCK をこえれば ENOMEM)。MCL_FUTURE はこれから写すものにも (Mm::def_lock)。
/// MCL_CURRENT は今の領域みなにかけて写す (写せないものは飛ばす)
fn sys_mlockall(flags: u64) -> R {
    const ENOMEM: i64 = 12;
    const MCL_CURRENT: u64 = 1;
    const MCL_FUTURE: u64 = 2;
    const MCL_ONFAULT: u64 = 4;
    if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0 || flags == MCL_ONFAULT {
        return Err(crate::why::inval());
    }
    let p = proc::current();
    if !can_do_mlock(p) {
        return Err(-EPERM);
    }
    let m = p.mm();
    if flags & MCL_CURRENT != 0 && (m.pt.vsize() >> 12) as u64 > memlock_pages(p) && !p.cred.capable(crate::cred::CAP_IPC_LOCK) {
        return Err(-ENOMEM);
    }
    let onfault = flags & MCL_ONFAULT != 0;
    m.def_lock = if flags & MCL_FUTURE != 0 { 1 | if onfault { 2 } else { 0 } } else { 0 };
    if flags & MCL_CURRENT != 0 {
        m.pt.set_locked_all(true, onfault);
        m.pt.populate_locked(0, crate::vm::MAXVA, true);
    }
    Ok(0)
}

/// 写したばかりの [va, va + len) に mlock をかけて写す (MAP_LOCKED と MCL_FUTURE。Linux の mmap_region と
/// vm_mmap_pgoff の mm_populate: 写せないものは気にしない)。MAP_POPULATE も写す
fn lock_new(m: &mut proc::Mm, va: usize, len: usize, lock: u8, populate: bool) {
    if lock & 1 != 0 {
        let _ = m.pt.set_locked(va, va + len, true, lock & 2 != 0);
    }
    if lock & 3 == 1 || populate {
        m.pt.populate_locked(va, va + len, true);
    }
}

/// いまのドメイン名 (/proc/sys/kernel/domainname)
pub fn domainname() -> String {
    let (b, l) = crate::ns::domainname();
    String::from_utf8_lossy(&b[..l]).into_owned()
}

/// ドメイン名を変える (/proc/sys/kernel/domainname。root かは呼ぶほうで見る)
pub fn set_domainname(name: &[u8]) -> Result<(), i64> {
    crate::ns::set_uts_name(name, true, false)
}

/// いまのホスト名 (/proc/sys/kernel/hostname)
pub fn hostname() -> String {
    let (hb, hl) = crate::ns::hostname();
    String::from_utf8_lossy(&hb[..hl]).into_owned()
}

/// ホスト名を変える (sethostname と /proc/sys/kernel/hostname。root かは呼ぶほうで見る)
pub fn set_hostname(name: &[u8]) -> Result<(), i64> {
    if name.len() > 64 {
        return Err(crate::why::inval());
    }
    crate::ns::set_hostname(name, false)
}

fn sys_uname(buf: usize) -> R {
    const FIELD: usize = 65;
    let (hb, hl) = crate::ns::hostname();
    let release = crate::ns::release();
    let (db, dl) = crate::ns::domainname();
    let fields: [&[u8]; 6] = [crate::ns::sysname().as_bytes(), &hb[..hl], release.as_bytes(), crate::ns::version().as_bytes(), b"aarch64", &db[..dl]];
    let mut u = [0u8; FIELD * 6];
    for (i, f) in fields.iter().enumerate() {
        u[i * FIELD..i * FIELD + f.len()].copy_from_slice(f);
    }
    out(buf, &u)?;
    Ok(0)
}

fn sys_sysinfo(buf: usize) -> R {
    use crate::memlayout::{ram_size, PGSIZE};
    let mut b = [0u8; 112];
    b[0..8].copy_from_slice(&(timer::uptime_ns() / 1_000_000_000).to_le_bytes());
    b[32..40].copy_from_slice(&(ram_size() as u64).to_le_bytes());
    b[40..48].copy_from_slice(&((crate::kalloc::nfree() * PGSIZE) as u64).to_le_bytes());
    let (st, sf) = crate::vm::swap_totals();
    b[56..64].copy_from_slice(&((st * PGSIZE) as u64).to_le_bytes());
    b[64..72].copy_from_slice(&((sf * PGSIZE) as u64).to_le_bytes());
    b[80..82].copy_from_slice(&(proc::nprocs() as u16).to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    out(buf, &b)?;
    Ok(0)
}

// ---- nice とスケジューラの決まり (Linux の kernel/sys.c の setpriority と kernel/sched/syscalls.c) ----

const RLIMIT_NICE: usize = 13;
const SCHED_RESET_ON_FORK: i64 = 0x4000_0000;

/// getpriority / setpriority の相手 (which: PRIO_PROCESS 0、PRIO_PGRP 1、PRIO_USER 2)。PRIO_PROCESS はスレッド
/// (who は TID、0 は自分)、PRIO_PGRP はグループのスレッドみな (0 は自分のグループ)、PRIO_USER は実 uid が who (0 は自分の)
/// で、いまの PID の namespace から見えるスレッドみな
fn prio_targets(which: i32, who: i32) -> Result<Vec<&'static mut proc::Proc>, i64> {
    let me = proc::current();
    Ok(match which {
        0 => {
            let t = if who == 0 { Some(me.pid) } else { crate::ns::arg_pid(who as i64).ok().filter(|&g| g > 0).map(|g| g as u32) };
            t.and_then(proc::find_thread).into_iter().collect()
        }
        1 => {
            let pg = if who == 0 { Some(me.pgid) } else { crate::ns::arg_pid(who as i64).ok().filter(|&g| g > 0).map(|g| g as u32) };
            match pg {
                Some(pg) => proc::live().filter(|p| p.state != proc::State::Unused && p.state != proc::State::Zombie && p.pgid == pg).collect(),
                None => Vec::new(),
            }
        }
        2 => {
            let uid = if who == 0 { Some(me.cred.uid) } else { crate::ns::take_uid(who as u32).ok() };
            match uid {
                Some(u) => proc::live()
                    .filter(|p| p.state != proc::State::Unused && p.state != proc::State::Zombie && !p.kthread && p.cred.uid == u && crate::ns::to_local(p.pid).is_some())
                    .collect(),
                None => Vec::new(),
            }
        }
        _ => return Err(crate::why::inval()),
    })
}

/// getpriority: 相手のうちいちばん高い (20 - nice の大きい) もの。いなければ ESRCH
fn sys_getpriority(which: i32, who: i32) -> R {
    prio_targets(which, who)?.iter().map(|p| 20 - p.nice as i64).max().ok_or(-3)
}

/// p の nice を nice まで上げてよいか (Linux の is_nice_reduction: 20 - nice が RLIMIT_NICE の中)
fn nice_reduction_ok(p: &proc::Proc, nice: i32) -> bool {
    (20 - nice) as u64 <= proc::rlimit(p, RLIMIT_NICE)[0]
}

/// setpriority (Linux の set_one_prio を相手ごとに): nice は -20..19 にまるめる。相手の実 uid か実効 uid が自分の実効
/// uid でなく CAP_SYS_NICE もなければ EPERM、nice を下げる (優先度を上げる) のは RLIMIT_NICE の中か CAP_SYS_NICE (でなければ
/// EACCES)。だれかに変えられれば 0、相手がいなければ ESRCH、だめなら最後のエラー
fn sys_setpriority(which: i32, who: i32, nice: i32) -> R {
    let nice = nice.clamp(-20, 19);
    let me = &proc::current().cred;
    let mut r = Err(-3);
    for p in prio_targets(which, who)? {
        if p.cred.uid != me.euid && p.cred.euid != me.euid && !me.capable(crate::cred::CAP_SYS_NICE) {
            r = Err(-EPERM);
            continue;
        }
        if nice < p.nice as i32 && !nice_reduction_ok(p, nice) && !me.capable(crate::cred::CAP_SYS_NICE) {
            r = Err(-13);
            continue;
        }
        if r == Err(-3) {
            r = Ok(0);
        }
        p.nice = nice as i8;
    }
    r
}

/// sched_* の相手 (Linux の find_process_by_pid: 0 は自分、ほかは TID)
fn sched_target(pid: i32) -> Result<&'static mut proc::Proc, i64> {
    if pid == 0 {
        return Ok(proc::current());
    }
    crate::ns::arg_pid(pid as i64).ok().filter(|&g| g > 0).and_then(|g| proc::find_thread(g as u32)).ok_or(-3)
}

/// struct sched_attr (Linux の SCHED_ATTR_SIZE_VER1: 56 バイト)。policy -1 は変えない (SETPARAM_POLICY)
struct SchedAttr {
    policy: i32,
    flags: u64,
    nice: i32,
    prio: u32,
    runtime: u64,
    deadline: u64,
    period: u64,
}

/// 決まりを変える (Linux の __sched_setscheduler と user_check_sched_setscheduler)
fn sched_set(p: &mut proc::Proc, a: &SchedAttr) -> R {
    const SCHED_FLAG_ALL: u64 = 0x7f;
    const SCHED_FLAG_SUGOV: u64 = 0x1000_0000;
    const SCHED_FLAG_UTIL_CLAMP: u64 = 0x60;
    let (policy, reset) = if a.policy < 0 {
        (p.policy as i32, p.reset_on_fork)
    } else {
        if !matches!(a.policy, 0 | 1 | 2 | 3 | 5 | 6) {
            return Err(crate::why::inval());
        }
        (a.policy, a.flags & 1 != 0)
    };
    if a.flags & !(SCHED_FLAG_ALL | SCHED_FLAG_SUGOV) != 0 || a.prio > 99 {
        return Err(crate::why::inval());
    }
    let rt = matches!(policy, 1 | 2);
    let dl_bad = policy == 6 && (a.deadline == 0 || a.runtime < 1024 || a.deadline < a.runtime || (a.period != 0 && a.period < a.deadline));
    if dl_bad || rt != (a.prio != 0) {
        return Err(crate::why::inval());
    }
    // 許し (なければ CAP_SYS_NICE)
    let me = &proc::current().cred;
    let fair = matches!(policy, 0 | 3);
    let mut need = false;
    if fair && a.nice < p.nice as i32 && !nice_reduction_ok(p, a.nice) {
        need = true;
    }
    if rt {
        let rtprio = proc::rlimit(p, 14)[0];
        if (policy != p.policy as i32 && rtprio == 0) || (a.prio > p.rt_priority as u32 && a.prio as u64 > rtprio) {
            need = true;
        }
    }
    if policy == 6 || (p.policy == 5 && policy != 5 && !nice_reduction_ok(p, p.nice as i32)) {
        need = true;
    }
    if me.euid != p.cred.euid && me.euid != p.cred.uid {
        need = true;
    }
    if p.reset_on_fork && !reset {
        need = true;
    }
    if need && !me.capable(crate::cred::CAP_SYS_NICE) {
        return Err(-EPERM);
    }
    if a.flags & SCHED_FLAG_SUGOV != 0 {
        return Err(crate::why::inval());
    }
    if a.flags & SCHED_FLAG_UTIL_CLAMP != 0 {
        return Err(-EOPNOTSUPP);
    }
    p.policy = policy as u8;
    p.reset_on_fork = reset;
    if fair {
        p.nice = a.nice as i8;
        p.slice_ns = if a.runtime != 0 { a.runtime.clamp(100_000, 100_000_000) } else { 0 };
    }
    p.rt_priority = a.prio as u8;
    Ok(0)
}

/// sched_setscheduler と sched_setparam (Linux の do_sched_setscheduler と _sched_setscheduler: param がないか pid が
/// 負なら EINVAL、読めなければ EFAULT、いなければ ESRCH。nice と slice はそのまま。policy の SCHED_RESET_ON_FORK の印)
fn do_sched_setscheduler(pid: i32, policy: i32, param: usize) -> R {
    if param == 0 || pid < 0 {
        return Err(crate::why::inval());
    }
    let mut b = [0u8; 4];
    proc::current().pt().copy_in(&mut b, param).ok_or(-EFAULT)?;
    let p = sched_target(pid)?;
    let mut flags = 0;
    let mut policy = policy;
    if policy >= 0 && policy as i64 & SCHED_RESET_ON_FORK != 0 {
        flags |= 1;
        policy &= !(SCHED_RESET_ON_FORK as i32);
    }
    let a = SchedAttr { policy, flags, nice: p.nice as i32, prio: i32::from_le_bytes(b) as u32, runtime: p.slice_ns, deadline: 0, period: 0 };
    sched_set(p, &a)
}

/// sched_setattr (Linux と同じ順: uattr がないか pid が負か flags があれば EINVAL、大きさが 48 より小さいか 4096 より
/// 大きいか、知らないところ (56 から先) が 0 でなければ 56 を書いて E2BIG、大きさ 0 は 48、nice は -20..19 にまるめる)
fn sys_sched_setattr(pid: i32, uattr: usize, flags: u32) -> R {
    const KEEP_POLICY: u64 = 0x08;
    const KEEP_PARAMS: u64 = 0x10;
    if uattr == 0 || pid < 0 || flags != 0 {
        return Err(crate::why::inval());
    }
    let pt = proc::current().pt();
    let mut sz = [0u8; 4];
    pt.copy_in(&mut sz, uattr).ok_or(-EFAULT)?;
    let mut size = u32::from_le_bytes(sz) as usize;
    if size == 0 {
        size = 48;
    }
    let too_big = |pt: &mut crate::vm::PageTable| {
        let _ = pt.copy_out(uattr, &56u32.to_le_bytes());
        Err(-7)
    };
    if !(48..=4096).contains(&size) {
        return too_big(pt);
    }
    let mut b = alloc::vec![0u8; size.max(56)];
    pt.copy_in(&mut b[..size], uattr).ok_or(-EFAULT)?;
    if b[56..size.max(56)].iter().any(|&x| x != 0) {
        return too_big(pt);
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut a = SchedAttr { policy: u32_at(4) as i32, flags: u64_at(8), nice: (u32_at(16) as i32).clamp(-20, 19), prio: u32_at(20), runtime: u64_at(24), deadline: u64_at(32), period: u64_at(40) };
    if a.flags & 0x60 != 0 && size < 56 {
        return Err(crate::why::inval());
    }
    if a.policy < 0 {
        return Err(crate::why::inval());
    }
    if a.flags & KEEP_POLICY != 0 {
        a.policy = -1;
    }
    let p = sched_target(pid)?;
    if a.flags & KEEP_PARAMS != 0 {
        a.nice = p.nice as i32;
        a.prio = p.rt_priority as u32;
        a.runtime = p.slice_ns;
    }
    sched_set(p, &a)
}

/// sched_getattr (Linux と同じ順: uattr がないか pid が負か、大きさが 48 より小さいか 4096 より大きいか、flags があれば
/// EINVAL。いなければ ESRCH。大きさ (56 と usize の小さいほう) を書いて、そこまで写す。SCHED_OTHER などは nice と
/// slice (sched_runtime)、FIFO / RR は優先度。uclamp は手本の Linux の設定にないので 0)
fn sys_sched_getattr(pid: i32, uattr: usize, usize_: u32, flags: u32) -> R {
    if uattr == 0 || pid < 0 || !(48..=4096).contains(&usize_) || flags != 0 {
        return Err(crate::why::inval());
    }
    let p = sched_target(pid)?;
    let size = (usize_ as usize).min(56);
    let mut b = [0u8; 56];
    b[0..4].copy_from_slice(&(size as u32).to_le_bytes());
    b[4..8].copy_from_slice(&(p.policy as u32).to_le_bytes());
    b[8..16].copy_from_slice(&(p.reset_on_fork as u64).to_le_bytes());
    match p.policy {
        1 | 2 => b[20..24].copy_from_slice(&(p.rt_priority as u32).to_le_bytes()),
        6 => {}
        _ => {
            b[16..20].copy_from_slice(&(p.nice as i32).to_le_bytes());
            b[24..32].copy_from_slice(&proc::slice_ns(p).to_le_bytes());
        }
    }
    out(uattr, &b[..size]).map(|_| 0)
}

/// 動いている CPU みな (ビットごと)
fn online_mask() -> u64 {
    let n = crate::smp::online();
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// sched_*affinity の相手のスレッド (0 は自分。いなければ ESRCH)
fn affinity_target(pid: i64) -> Result<&'static mut proc::Proc, i64> {
    if pid == 0 {
        return Ok(proc::current());
    }
    let g = crate::ns::to_global(u32::try_from(pid).map_err(|_| -ESRCH)?).ok_or(-ESRCH)?;
    proc::find_thread(g).ok_or(-ESRCH)
}

/// sched_getaffinity (Linux の順: len は CPU の数より多く 8 の倍数 (EINVAL)、いないスレッドは ESRCH。答えは走ってよい
/// CPU のうち動いているもの、大きさは cpumask の大きさ (8))
fn sys_sched_getaffinity(pid: i64, len: usize, mask: usize) -> R {
    if len * 8 < crate::smp::online() || len & 7 != 0 {
        return Err(crate::why::inval());
    }
    let t = affinity_target(pid)?;
    out(mask, &(t.affinity & online_mask()).to_le_bytes())?;
    Ok(8)
}

/// sched_setaffinity (Linux の順: いないスレッドは ESRCH、持ち主か CAP_SYS_NICE でなければ EPERM、len が 8 より短ければ
/// その分だけ読み、動いている CPU がひとつもなければ EINVAL)。自分がいまの CPU で走れなくなったら、ほかへゆずる
fn sys_sched_setaffinity(pid: i64, len: usize, mask: usize) -> R {
    let t = affinity_target(pid)?;
    let c = cred::current();
    if c.euid != t.cred.euid && c.euid != t.cred.uid && !c.capable(cred::CAP_SYS_NICE) {
        return Err(-EPERM);
    }
    let mut b = [0u8; 8];
    let n = len.min(8);
    proc::current().pt().copy_in(&mut b[..n], mask).ok_or(-EFAULT)?;
    let want = u64::from_le_bytes(b) & online_mask();
    if want == 0 {
        return Err(crate::why::inval());
    }
    t.affinity = want;
    if t.pid == proc::current().pid {
        if want >> crate::smp::id() & 1 == 0 {
            proc::yield_now();
        }
    } else {
        proc::affinity_changed(t);
    }
    Ok(0)
}

/// rlimit を読み、new があれば変える (Linux の do_prlimit): ない番号は EINVAL、ソフトがハードより上は EINVAL、NOFILE の
/// ハードが nr_open より上は EPERM、ハードを上げるのは CAP_SYS_RESOURCE (init の user namespace で) だけ。前の値を返す。
/// 守るのは NOFILE (fd の表) だけ。ほかは覚えて見せるだけ (FSIZE の SIGXFSZ、CPU の SIGXCPU、NPROC の fork などはまだ)
fn do_prlimit(t: &mut proc::Proc, resource: u32, new: Option<[u64; 2]>) -> Result<[u64; 2], i64> {
    let r = resource as usize;
    if r >= proc::RLIM_NLIMITS {
        return Err(crate::why::inval());
    }
    if let Some(v) = new {
        if v[0] > v[1] {
            return Err(crate::why::inval());
        }
        if r == proc::RLIMIT_NOFILE && v[1] > proc::NR_OPEN {
            return Err(-EPERM);
        }
    }
    let old = proc::rlimit(t, r);
    if let Some(v) = new {
        if v[1] > old[1] && !cred::current().capable(cred::CAP_SYS_RESOURCE) {
            return Err(-EPERM);
        }
        proc::set_rlimit(t, r, v);
    }
    Ok(old)
}

/// personality (arm64_personality): 0xffffffff は読むだけ。PER_LINUX32 は CPU に 32 ビットの EL0 がなければ EINVAL
/// (ID_AA64PFR0_EL1 の EL0 が 2: Linux の system_supports_32bit_el0。aios は 32 ビットのプログラムを動かさないが、印は
/// 覚える。uname は COMPAT のない Linux と同じく aarch64 のまま)。前の値を返す
fn sys_personality(per: u32) -> R {
    const PER_LINUX32: u32 = 0x0008;
    if per & 0xff == PER_LINUX32 {
        let pfr0: u64;
        unsafe { core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0) };
        if pfr0 & 0xf != 2 {
            return Err(crate::why::inval());
        }
    }
    let p = proc::current();
    let old = p.personality;
    if per != u32::MAX {
        p.personality = per;
    }
    Ok(old as i64)
}

fn rlim_bytes(v: [u64; 2]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&v[0].to_le_bytes());
    b[8..].copy_from_slice(&v[1].to_le_bytes());
    b
}

fn get_rlim(va: usize) -> Result<[u64; 2], i64> {
    let mut b = [0u8; 16];
    proc::get_user(va, &mut b)?;
    Ok([u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap())])
}

fn sys_getrlimit(resource: u32, old: usize) -> R {
    let v = do_prlimit(proc::current(), resource, None)?;
    out(old, &rlim_bytes(v))?;
    Ok(0)
}

fn sys_setrlimit(resource: u32, new: usize) -> R {
    let v = get_rlim(new)?;
    do_prlimit(proc::current(), resource, Some(v))?;
    Ok(0)
}

/// prlimit64 (Linux の順: new を読む (EFAULT)、pid (0 は自分。ESRCH)、check_prlimit_permission (自分のスレッドでなければ、
/// uid と euid と suid が呼んだ人の uid、gid と egid と sgid が gid か、CAP_SYS_RESOURCE。でなければ EPERM)、do_prlimit、
/// old を書く (EFAULT でも new は変わったまま))
fn sys_prlimit(pid: i32, resource: u32, new: usize, old: usize) -> R {
    let nv = if new != 0 { Some(get_rlim(new)?) } else { None };
    let me = proc::current();
    let t = if pid == 0 {
        me
    } else {
        let g = crate::ns::to_global(pid as u32).filter(|_| pid > 0).ok_or(-ESRCH)?;
        proc::find_thread(g).ok_or(-ESRCH)?
    };
    if t.pid != proc::current().pid {
        let (c, tc) = (cred::current(), &t.cred);
        let same = c.uid == tc.euid && c.uid == tc.suid && c.uid == tc.uid && c.gid == tc.egid && c.gid == tc.sgid && c.gid == tc.gid;
        if !same && !c.capable(cred::CAP_SYS_RESOURCE) {
            return Err(-EPERM);
        }
    }
    let v = do_prlimit(t, resource, nv)?;
    if old != 0 {
        out(old, &rlim_bytes(v))?;
    }
    Ok(0)
}

fn sys_brk(addr: usize) -> i64 {
    let m = proc::current().mm();
    // heap の始めより前と、ユーザーの番地の外 (ページにそろえるとあふれる -1 なども) は、いまの brk のまま (Linux は
    // そろえて 0 になったものを伸ばそうとして ENOMEM になり、もとの brk を返す)
    if addr < m.heap_start || addr > crate::vm::MAXVA {
        return m.brk as i64;
    }
    let (old, new) = (pg_up(m.brk), pg_up(addr));
    if new < old && m.pt.sealed_in(new, old) {
        return m.brk as i64;
    }
    // mlockall の MCL_FUTURE: 伸ばすところも RLIMIT_MEMLOCK の中で (Linux の check_brk_limits)、かけて写す
    if new > old && !mlock_future_ok(proc::current(), m.def_lock & 1 != 0, new - old) {
        return m.brk as i64;
    }
    if new != old && m.pt.resize(m.heap_start, old, new).is_none() {
        return m.brk as i64;
    }
    if new > old && m.def_lock != 0 {
        lock_new(m, old, new - old, m.def_lock, false);
    }
    m.brk = addr;
    addr as i64
}

const MAP_SHARED: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_DROPPABLE: u64 = 0x08;
const MAP_NORESERVE: u64 = 0x4000;
const MAP_LOCKED: u64 = 0x2000;
const MAP_POPULATE: u64 = 0x8000;
const MAP_NONBLOCK: u64 = 0x1_0000;
const MAP_STACK: u64 = 0x2_0000;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
const PROT_WRITE: u64 = 0x2;

/// mmap。無名のメモリとファイルの写し。ページは触れたときに作る (ファイルならそのときに読む)。
/// MAP_SHARED のファイルは写している人どうしでページを共有し、書いたものは msync などで書き戻す (vm.rs)
fn sys_mmap(addr: usize, len: usize, prot: u64, flags: u64, fd: i64, off: usize) -> R {
    const EACCES: i64 = 13;
    const EEXIST: i64 = 17;
    // Linux の順: 位置のそろい (sys_mmap)、fd (ksys_mmap_pgoff: O_PATH は EBADF)、長さ 0 (do_mmap)、許可、写せる種類
    if off % 4096 != 0 {
        return Err(crate::why::inval());
    }
    // ファイルを写すなら、その inode (と、/proc/PID/maps に出すパス)
    let mut path: Option<alloc::string::String> = None;
    let mut accessed: Option<(crate::vfs::InodeRef, u32)> = None;
    let mut dev_name: Option<alloc::string::String> = None;
    // 写しかたの型 (Linux の MAP_TYPE: 1 MAP_SHARED、2 MAP_PRIVATE、3 MAP_SHARED_VALIDATE (ファイルだけ)、8 MAP_DROPPABLE
    // (無名だけ: fork した子では 0、Linux はメモリが足りなければ捨てる。aios は捨てない))。ほかは EINVAL
    let kind = flags & 0xf;
    let back = if flags & MAP_ANONYMOUS == 0 {
        let fr = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
        let f = fr.borrow();
        if f.flags & crate::file::O_PATH != 0 {
            return Err(-EBADF);
        }
        if len == 0 || !matches!(kind, 1..=3) {
            return Err(crate::why::inval());
        }
        let mode = f.flags & crate::file::O_ACCMODE;
        // 共有で書くなら書けること。それから読めること (O_WRONLY と O_ACCMODE (3) は読めない)
        if flags & MAP_SHARED != 0 && prot & PROT_WRITE != 0 && mode != crate::file::O_RDWR && mode != crate::file::O_WRONLY {
            return Err(-EACCES);
        }
        if mode != crate::file::O_RDONLY && mode != crate::file::O_RDWR {
            return Err(-EACCES);
        }
        match &f.kind {
            // ふつうのファイルだけ (ディレクトリやパイプは ENODEV: Linux に mmap がない)。fend は MAX: ファイルの終わりは
            // 触れたときの大きさで見る (その先のページは SIGBUS。vm.rs)
            crate::file::Kind::Inode(ino, p) if ino.meta().mode & crate::vfs::S_IFMT == crate::vfs::S_IFREG => {
                // 足すだけのもの (chattr +a) を書ける口で共有に写すのは EACCES (Linux の do_mmap)
                if flags & MAP_SHARED != 0 && mode != crate::file::O_RDONLY && ino.append_only() {
                    return Err(-EACCES);
                }
                // LSM (AppArmor): 実行して写すなら m (security_mmap_file)
                crate::security::mmap_file(p, prot, flags & MAP_SHARED == 0, ino.meta().uid)?;
                // noexec のマウントのものは実行できるように写せない (Linux の do_mmap の path_noexec: EPERM)
                if prot & crate::vm::PROT_EXEC as u64 != 0 && crate::vfs::mount_flags(p) & crate::vfs::ATTR_NOEXEC != 0 {
                    return Err(-EPERM);
                }
                path = Some(p.clone());
                // 写したファイルの atime (Linux の generic_file_mmap と shmem_mmap の file_accessed)。開いたファイルを
                // 借りたまま書かない (ext4 は書き出しで眠ることがある) ので、あとで
                accessed = Some((ino.clone(), f.flags));
                Backing::File { ino: ino.clone(), off, fend: usize::MAX, ver: crate::vm::file_ver(ino) }
            }
            // 装置やソケット (画面、音の再生の口、TCP のソケット: file.rs の mmap_dev。Linux の f_op->mmap)。ほかは ENODEV
            k => {
                if let crate::file::Kind::Ops(o) = k
                    && o.mmap_shared_only()
                    && flags & MAP_SHARED == 0
                {
                    return Err(crate::why::inval());
                }
                // perf の MMAP の記録の名前 (Linux の d_path: anon_inode:[perf_event] など)
                if let crate::file::Kind::Ops(o) = k {
                    dev_name = Some(o.describe());
                }
                match crate::file::mmap_dev(k, &fr, off, len, prot) {
                    Some(b) => b?,
                    None => return Err(-ENODEV),
                }
            }
        }
    } else {
        if len == 0 || !matches!(kind, 1 | 2 | MAP_DROPPABLE) {
            return Err(crate::why::inval());
        }
        Backing::Anon
    };
    if let (Some((ino, fl)), Some(pp)) = (&accessed, &path) {
        crate::vfs::file_accessed(ino, pp, *fl);
    }
    let p = proc::current();
    let m = p.mm();
    let len = pg_up(len);
    // mlock をかけて写すか (MAP_LOCKED か mlockall の MCL_FUTURE。Linux の do_mmap: can_do_mlock でなければ EPERM、
    // RLIMIT_MEMLOCK をこえれば EAGAIN)
    let lock = if flags & MAP_LOCKED != 0 { 1 } else { 0 } | m.def_lock;
    if flags & MAP_LOCKED != 0 && !can_do_mlock(p) {
        return Err(-EPERM);
    }
    if !mlock_future_ok(p, lock & 1 != 0, len) {
        return Err(-11);
    }
    let va = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if addr & 0xfff != 0 {
            return Err(crate::why::inval());
        }
        if flags & MAP_FIXED_NOREPLACE != 0 && m.pt.overlaps(addr, addr.saturating_add(len)) {
            return Err(-EEXIST);
        }
        // 封じた領域の上には写せない
        if m.pt.sealed_in(addr, addr.saturating_add(len)) {
            return Err(-EPERM);
        }
        addr
    } else {
        let va = m.pt.free_area(m.mmap_next, len);
        m.mmap_next = va + len;
        va
    };
    let back = match back {
        // ファイルの終わりの va (その先は 0)
        Backing::File { ino, off, fend, ver } => Backing::File { ino, off, fend: va.saturating_add(fend), ver },
        b => b,
    };
    let shared = flags & MAP_SHARED != 0;
    m.pt.map(va, va + len, prot_bits(prot), shared, back).ok_or(-ENOMEM)?;
    if let Some(p) = path {
        m.pt.set_name(va, &p);
    }
    if kind == MAP_DROPPABLE {
        m.pt.set_wipe(va);
    }
    if lock != 0 || flags & (MAP_POPULATE | MAP_NONBLOCK) == MAP_POPULATE {
        lock_new(m, va, len, lock, flags & (MAP_POPULATE | MAP_NONBLOCK) == MAP_POPULATE);
    }
    // perf の PERF_RECORD_MMAP (Linux の mmap_region の perf_event_mmap)
    if proc::PERF_MMAP.load(core::sync::atomic::Ordering::Relaxed) != 0
        && let Some(h) = proc::perf()
    {
        let names = proc::map_names(proc::current());
        for r in proc::current().pt().map_recs(names, Some(va)) {
            h.mmap(&r, dev_name.as_deref());
        }
    }
    Ok(va as i64)
}

// [lockless]
fn prot_bits(prot: u64) -> u8 {
    (prot & 7) as u8
}

fn sys_munmap(addr: usize, len: usize) -> R {
    if addr & 0xfff != 0 || len == 0 {
        return Err(crate::why::inval());
    }
    let pt = proc::current().pt();
    // 封じた領域があれば、どこも外さずに EPERM (Linux の do_vmi_munmap)
    if pt.sealed_in(addr, addr.saturating_add(pg_up(len))) {
        return Err(-EPERM);
    }
    pt.unmap(addr, addr + pg_up(len));
    Ok(0)
}

/// mseal (Linux 6.10 の do_mseal: flags は 0 だけ、そろっていない番地と、ページにそろえるとあふれる長さは EINVAL、長さ 0 は
/// 0、すきまがあれば ENOMEM)
fn sys_mseal(addr: usize, len: usize, flags: u64) -> R {
    if flags != 0 || addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    let alen = len.wrapping_add(0xfff) & !0xfff;
    if len != 0 && alen == 0 {
        return Err(crate::why::inval());
    }
    let end = addr.checked_add(alen).ok_or_else(crate::why::inval)?;
    if end == addr {
        return Ok(0);
    }
    proc::current().pt().seal(addr, end).map(|_| 0)
}

/// remap_file_pages (Linux 4.0 からは mmap でまねるもの: mm/mmap.c。Linux の順): prot が 0 でなければ EINVAL、start と
/// size をページに落とし、size が 0 かあふれる、pgoff があふれるなら EINVAL、start の領域が MAP_SHARED のファイルでなければ
/// EINVAL、LSM (security_mmap_file)、範囲がすきまなく同じファイル・同じ印の領域 (前に写しなおして分かれたものも) で
/// なければ EINVAL、封じた領域は EPERM (do_mmap)。そこをファイルの pgoff から MAP_SHARED | MAP_FIXED | MAP_POPULATE で
/// 写しなおす (prot と mlock は領域のもの。MAP_NONBLOCK なら先に読まない。ほかの flags は見ない)
fn sys_remap_file_pages(start: usize, size: usize, prot: u64, pgoff: usize, flags: u64) -> R {
    if prot != 0 {
        return Err(crate::why::inval());
    }
    let (start, size) = (start & !0xfff, size & !0xfff);
    let end = match start.checked_add(size) {
        Some(e) if e > start => e,
        _ => return Err(crate::why::inval()),
    };
    if pgoff.checked_add(size >> 12).is_none() {
        return Err(crate::why::inval());
    }
    let p = proc::current();
    let m = p.mm();
    // 領域の印 (Linux の vm_flags: これがみな同じ領域だけをまたげる)
    let key = |v: &crate::vm::Vma| (v.prot, v.shared, v.locked, v.onfault, v.wipe, v.droppable, v.dontcopy, v.sealed);
    let (ino, name, vprot, locked, k0) = {
        let Some((_, v)) = m.pt.find(start) else { return Err(crate::why::inval()) };
        let Backing::File { ino, .. } = &v.back else { return Err(crate::why::inval()) };
        if !v.shared {
            return Err(crate::why::inval());
        }
        (ino.clone(), v.name.clone(), v.prot, v.locked, key(v))
    };
    if let Some(n) = &name {
        crate::security::mmap_file(n, vprot as u64, false, ino.meta().uid)?;
    }
    let mut cur = start;
    while cur < end {
        let Some((_, v)) = m.pt.find(cur) else { return Err(crate::why::inval()) };
        if !matches!(&v.back, Backing::File { ino: i, .. } if i.id() == ino.id()) || key(v) != k0 {
            return Err(crate::why::inval());
        }
        cur = v.end;
    }
    if m.pt.sealed_in(start, end) {
        return Err(-EPERM);
    }
    let back = Backing::File { ino: ino.clone(), off: pgoff * 4096, fend: usize::MAX, ver: crate::vm::file_ver(&ino) };
    m.pt.map(start, end, vprot, true, back).ok_or(-ENOMEM)?;
    if let Some(n) = name {
        m.pt.set_name(start, &n);
    }
    let populate = flags & MAP_NONBLOCK == 0;
    if locked || populate {
        lock_new(m, start, size, if locked { 1 } else { 0 }, populate);
    }
    Ok(0)
}

/// mremap: 領域の大きさを変える (縮める、その場で伸ばす、MREMAP_MAYMOVE なら別の場所へ写す)。
/// 古い場所に領域がなければ EFAULT (Firefox などは mremap でページがあるかを確かめる)。Linux 6.18 と同じ順に: 大きさ 0 の
/// 古い場所は、共有でなければ EINVAL。同じ大きさと縮めるのは、古い場所が領域をこえていても答える (縮めるなら、こえたところも
/// 外す)。こえていて EFAULT なのは伸ばすときと動かすとき
fn sys_mremap(old: usize, old_size: usize, new_size: usize, flags: u64, new_addr: usize) -> R {
    const EFAULT: i64 = 14;
    const MREMAP_MAYMOVE: u64 = 1;
    const MREMAP_FIXED: u64 = 2;
    const MREMAP_DONTUNMAP: u64 = 4;
    if old & 0xfff != 0 || new_size == 0 || flags & !7 != 0 || (flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0) {
        return Err(crate::why::inval());
    }
    let (old_size, new_size) = (pg_up(old_size), pg_up(new_size));
    let m = proc::current().mm();
    let Some((vstart, v)) = m.pt.find(old).map(|(s, v)| (s, v.clone())) else { return Err(-EFAULT) };
    // 封じた領域は動かせない・大きさを変えられない (Linux の mremap の can_modify_vma)。MREMAP_FIXED の行き先も
    if v.sealed || (flags & MREMAP_FIXED != 0 && m.pt.sealed_in(new_addr, new_addr.saturating_add(new_size))) {
        return Err(-EPERM);
    }
    // 大きさ 0 は、共有の領域を写すとき (Linux は MAP_PRIVATE のものを断る)
    if old_size == 0 && !v.shared {
        return Err(crate::why::inval());
    }
    let old_end = old.checked_add(old_size.max(PGSIZE)).ok_or(-EFAULT)?;
    if flags & MREMAP_FIXED == 0 {
        if new_size == old_size {
            return Ok(old as i64);
        }
        if new_size < old_size {
            m.pt.unmap(old + new_size, old_end);
            return Ok(old as i64);
        }
    }
    // 伸ばす・動かす古い場所は 1 つの領域の中でなければならない
    if old_size == 0 || v.end < old_end {
        return Err(-EFAULT);
    }
    // mlock した領域を伸ばすなら RLIMIT_MEMLOCK の中で (Linux の vma_to_resize: EAGAIN)
    if v.locked && new_size > old_size && !mlock_future_ok(proc::current(), true, new_size - old_size) {
        return Err(-11);
    }
    if flags & MREMAP_FIXED == 0 {
        // その場で伸ばす (領域の終わりまで使っていて、その先が空いているとき)
        if v.end == old_end && m.pt.extend(vstart, old + new_size).is_some() {
            if v.locked && !v.onfault {
                m.pt.populate_locked(old + old_size, old + new_size, true);
            }
            return Ok(old as i64);
        }
        if flags & MREMAP_MAYMOVE == 0 {
            return Err(-ENOMEM);
        }
    }
    // 別の場所へ: 新しい領域を作って中身を写す (共有の領域は写すと分かれてしまうので動かさない)
    if v.shared {
        return Err(-ENOMEM);
    }
    let dst = if flags & MREMAP_FIXED != 0 {
        if new_addr & 0xfff != 0 || (new_addr < old_end && old < new_addr + new_size) {
            return Err(crate::why::inval());
        }
        m.pt.unmap(new_addr, new_addr + new_size);
        new_addr
    } else {
        let va = m.pt.free_area(m.mmap_next, new_size);
        m.mmap_next = va + new_size;
        va
    };
    m.pt.map(dst, dst + new_size, v.prot, false, Backing::Anon).ok_or(-ENOMEM)?;
    if v.locked {
        let _ = m.pt.set_locked(dst, dst + new_size, true, v.onfault);
    }
    if v.prot & PROT_READ != 0 {
        let mut buf = alloc::vec![0u8; PGSIZE];
        for off in (0..old_size.min(new_size)).step_by(PGSIZE) {
            if m.pt.copy_in(&mut buf, old + off).is_none() {
                break;
            }
            m.pt.copy_out_force(dst + off, &buf).ok_or(-ENOMEM)?;
        }
    }
    if flags & MREMAP_DONTUNMAP == 0 {
        m.pt.unmap(old, old_end);
    }
    Ok(dst as i64)
}

/// msync: MAP_SHARED のファイルで書いたページを書き戻す (MS_ASYNC でもすぐに印をつけ、MS_SYNC ならディスクへ)。
/// Linux と同じく: 知らないフラグ、そろっていない addr、MS_ASYNC と MS_SYNC の両方は EINVAL。範囲に領域のないところが
/// あれば ENOMEM (あるところはしてから)
fn sys_msync(addr: usize, len: usize, flags: u64) -> R {
    const MS_ASYNC: u64 = 1;
    const MS_INVALIDATE: u64 = 2;
    const MS_SYNC: u64 = 4;
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 || addr & 0xfff != 0 || (flags & MS_ASYNC != 0 && flags & MS_SYNC != 0) {
        return Err(crate::why::inval());
    }
    let end = len.checked_next_multiple_of(4096).and_then(|l| addr.checked_add(l)).ok_or(-ENOMEM)?;
    if end == addr {
        return Ok(0);
    }
    proc::current().pt().msync(addr, end, flags & MS_SYNC != 0).map(|_| 0)
}

fn sys_mprotect(addr: usize, len: usize, prot: u64) -> R {
    if addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    if proc::current().pt().sealed_in(addr, addr.saturating_add(pg_up(len))) {
        return Err(-EPERM);
    }
    // 実行できなかったところを実行できるようにしたら perf の PERF_RECORD_MMAP (Linux の mprotect_fixup の perf_event_mmap)
    let rec = proc::PERF_MMAP.load(core::sync::atomic::Ordering::Relaxed) != 0
        && prot & crate::vm::PROT_EXEC as u64 != 0
        && proc::current().pt().map_recs(crate::vm::MapNames::default(), Some(addr)).first().is_some_and(|r| r.prot & crate::vm::PROT_EXEC == 0);
    proc::current().pt().protect(addr, addr + pg_up(len), prot_bits(prot)).map_err(|_| -ENOMEM)?;
    if rec && let Some(h) = proc::perf() {
        let names = proc::map_names(proc::current());
        for r in proc::current().pt().map_recs(names, Some(addr)) {
            h.mmap(&r, None);
        }
    }
    Ok(0)
}

/// madvise (Linux の do_madvise と同じ順: 知らない値・そろっていない番地・あふれる長さは EINVAL、長さ 0 は 0。写して
/// いないところがあれば、写したところにしてから ENOMEM)。
///   DONTNEED (と _LOCKED)  無名のページを捨てる (次に触れると 0)
///   FREE                    私有の無名だけ (ほかは EINVAL)。aios は捨てない (中身はそのまま: Linux もメモリが足りるうちは)
///   REMOVE                  ファイルのないものは EINVAL、共有で書けるファイルのものでなければ EACCES。穴をあけるのは
///                           まだできない (EOPNOTSUPP)
///   DONTFORK / DOFORK       fork した子に領域ごと写さない / 写す
///   WIPEONFORK / KEEPONFORK 私有の無名だけ (ほかは EINVAL)。子では 0 から。MAP_DROPPABLE のものは KEEPONFORK で外せない
///   POPULATE_READ / WRITE   ページを写しておく (読めない・書けない領域は EINVAL)
///   NORMAL RANDOM SEQUENTIAL WILLNEED DONTDUMP DODUMP COLD PAGEOUT  何もしない (DODUMP と COLD と PAGEOUT は、ページを
///                           持たない装置の写しなら EINVAL)
/// GUARD_INSTALL / REMOVE (Linux 6.13 の見張りのページ) はまだない: EINVAL (glibc は mprotect にする)。HUGEPAGE、MERGEABLE、
/// HWPOISON などは手本の Linux の設定にもないので EINVAL
fn sys_madvise(addr: usize, len: usize, advice: u64) -> R {
    madvise_in(proc::current().pt(), addr, len, advice)
}

/// process_madvise(pidfd, vec, vlen, advice, flags) (Linux の mm/madvise.c): pidfd の指すプロセスの iovec の範囲に madvise。
/// Linux と同じ順: flags が 0 でなければ EINVAL、iovec を写す (1024 をこえれば EINVAL、読めなければ EFAULT)、
/// pidfd (EBADF、ESRCH)、ptrace で読めるか (EACCES)。ほかのプロセスには、こわさない知らせ (WILLNEED、COLD、PAGEOUT、
/// COLLAPSE) だけ (でなければ EINVAL) で、CAP_SYS_NICE が要る (でなければ EPERM)。iovec を順に madvise して、知らせた
/// バイトの和を返す (途中でだめになれば、そこまでの和。1 つもできなければそのエラー)
fn sys_process_madvise(pidfd: i64, vec: usize, vlen: usize, advice: u64, flags: u64) -> R {
    const WILLNEED: u64 = 3;
    const COLD: u64 = 20;
    const PAGEOUT: u64 = 21;
    const COLLAPSE: u64 = 25;
    if flags != 0 {
        return Err(crate::why::inval());
    }
    let iov = crate::sysfile::iovecs(vec, vlen)?;
    crate::sysfile::iov_total(&iov)?;
    let t = crate::pidfd::mm_task(pidfd)?;
    let remote = t.tgid != proc::current().tgid;
    if remote && !matches!(advice, WILLNEED | COLD | PAGEOUT | COLLAPSE) {
        return Err(crate::why::inval());
    }
    if remote && !proc::current().cred.capable(crate::cred::CAP_SYS_NICE) {
        return Err(-EPERM);
    }
    let pt = t.pt();
    let mut done: usize = 0;
    for (addr, len) in iov {
        if let Err(e) = madvise_in(pt, addr, len, advice) {
            return if done != 0 { Ok(done as i64) } else { Err(e) };
        }
        done += len;
    }
    Ok(done as i64)
}

/// madvise を表 pt に (自分のものと、process_madvise の相手のもの)
fn madvise_in(pt: &mut crate::vm::PageTable, addr: usize, len: usize, advice: u64) -> R {
    const ENOMEM: i64 = 12;
    const EACCES: i64 = 13;
    const DONTNEED: u64 = 4;
    const FREE: u64 = 8;
    const REMOVE: u64 = 9;
    const DONTFORK: u64 = 10;
    const DOFORK: u64 = 11;
    const DODUMP: u64 = 17;
    const WIPEONFORK: u64 = 18;
    const KEEPONFORK: u64 = 19;
    const COLD: u64 = 20;
    const PAGEOUT: u64 = 21;
    const POPULATE_READ: u64 = 22;
    const POPULATE_WRITE: u64 = 23;
    const DONTNEED_LOCKED: u64 = 24;
    if !matches!(advice, 0..=4 | 8..=11 | 16..=24) || addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    let alen = len.checked_add(0xfff).map(|l| l & !0xfff).ok_or_else(crate::why::inval)?;
    let end = addr.checked_add(alen).ok_or_else(crate::why::inval)?;
    if end == addr {
        return Ok(0);
    }
    // 捨てる知らせは、封じた読むだけの無名の領域には EPERM (Linux の can_madvise_modify)
    if matches!(advice, DONTNEED | DONTNEED_LOCKED | FREE | REMOVE | DONTFORK | WIPEONFORK) && pt.sealed_ro_anon_in(addr, end) {
        return Err(-EPERM);
    }
    let anon_private = |v: &crate::vm::Vma| matches!(v.back, Backing::Anon) && !v.shared;
    let gap = match advice {
        DONTNEED | DONTNEED_LOCKED => {
            let gap = pt.advise(addr, end, |_| Ok(()), |_| {})?;
            pt.discard(addr, end);
            gap
        }
        FREE => pt.advise(addr, end, |v| if anon_private(v) { Ok(()) } else { Err(crate::why::inval()) }, |_| {})?,
        REMOVE => pt.advise(
            addr,
            end,
            |v| match &v.back {
                Backing::File { .. } if v.shared && v.prot & crate::vm::PROT_WRITE != 0 => Err(-EOPNOTSUPP),
                Backing::File { .. } => Err(-EACCES),
                _ => Err(crate::why::inval()),
            },
            |_| {},
        )?,
        DONTFORK | DOFORK => pt.advise(addr, end, |_| Ok(()), |v| v.dontcopy = advice == DONTFORK)?,
        WIPEONFORK => pt.advise(addr, end, |v| if anon_private(v) { Ok(()) } else { Err(crate::why::inval()) }, |v| v.wipe = true)?,
        KEEPONFORK => pt.advise(addr, end, |v| if v.droppable { Err(crate::why::inval()) } else { Ok(()) }, |v| v.wipe = false)?,
        POPULATE_READ | POPULATE_WRITE => {
            let write = advice == POPULATE_WRITE;
            let want = if write { crate::vm::PROT_WRITE } else { crate::vm::PROT_READ };
            let gap = pt.advise(addr, end, |v| if v.prot & want != 0 { Ok(()) } else { Err(crate::why::inval()) }, |_| {})?;
            pt.populate(addr, end, write);
            gap
        }
        DODUMP | COLD | PAGEOUT => pt.advise(addr, end, |v| if matches!(v.back, Backing::Pages { .. }) { Err(crate::why::inval()) } else { Ok(()) }, |_| {})?,
        _ => pt.advise(addr, end, |_| Ok(()), |_| {})?,
    };
    if gap {
        return Err(-ENOMEM);
    }
    Ok(0)
}

/// mincore (Linux と同じ順: そろっていない番地は EINVAL、ユーザーの外は ENOMEM、長さ 0 は 0、vec がユーザーの外なら
/// EFAULT)。ページがあるかを 1 バイトずつ (vm::PageTable::mincore)。領域のないところに来たら、そこまでを書いて ENOMEM
fn sys_mincore(addr: usize, len: usize, vec: usize) -> R {
    const ENOMEM: i64 = 12;
    if addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    if addr.checked_add(len).is_none_or(|e| e > crate::vm::MAXVA) {
        return Err(-ENOMEM);
    }
    let n = len / 4096 + (len % 4096 != 0) as usize;
    if n == 0 {
        return Ok(0);
    }
    if vec.checked_add(n).is_none_or(|e| e > crate::vm::MAXVA) {
        return Err(-EFAULT);
    }
    let p = proc::current();
    let c = &p.cred;
    let (v, gap) = p.mm().pt.mincore(addr, n, |ino| {
        let m = ino.meta();
        m.uid == c.fsuid || c.may(&m, crate::cred::W, false)
    });
    if !v.is_empty() {
        out(vec, &v)?;
    }
    if gap { Err(-ENOMEM) } else { Ok(0) }
}

/// capget(hdr, data): capability (root は全部、ほかはなし。aios は capability を分けない)。
/// 版が違えば、知っている版 (3) を hdr に書いて EINVAL (Linux と同じ。libcap はそれで版を知る)
fn sys_capget(hdr: usize, data: usize) -> R {
    const V3: u32 = 0x2008_0522;
    let pt = proc::current().pt();
    let mut h = [0u8; 8];
    pt.copy_in(&mut h, hdr).ok_or(-EFAULT)?;
    let ver = u32::from_le_bytes(h[..4].try_into().unwrap());
    if ver != V3 && ver != 0x1998_0330 && ver != 0x2007_1026 {
        pt.copy_out(hdr, &V3.to_le_bytes()).ok_or(-EFAULT)?;
        return if data == 0 { Ok(0) } else { Err(crate::why::inval()) };
    }
    if data == 0 {
        return Ok(0);
    }
    // v1 は 1 組、v2 と v3 は 2 組の {effective, permitted, inheritable}
    let n = if ver == 0x1998_0330 { 1 } else { 2 };
    // root はみな (CAP_LAST_CAP の 40 まで: 上の組は 0x1ff)。LSM のラベル (AppArmor のプロファイル) があれば、その LSM が
    // 許すものだけ (security_capget)
    let c = crate::cred::current();
    let all: u64 = if c.euid == 0 { ((1u64 << 41) - 1) & crate::security::capget(c) } else { 0 };
    let mut out = alloc::vec::Vec::new();
    for i in 0..n {
        let w = (all >> (32 * i)) as u32;
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    pt.copy_out(data, &out).ok_or(-EFAULT)?;
    Ok(0)
}

/// getrandom (rand の ChaCha20)。flags は GRND_NONBLOCK (1)、GRND_RANDOM (2)、GRND_INSECURE (4)。
/// rand がまだ ready でなければ GRND_NONBLOCK は EAGAIN、ほかは待たずに出す (rand.rs)。
/// Linux と同じく 1 度に 0x7ffff000 バイトまで、ページごとにシグナルを見て (来ていたらそこまでの数を)、
/// 書けないページに来たらそこまでの数を (はじめからなら EFAULT。写すのはページをまたがない分ずつ)
fn sys_getrandom(buf: usize, len: usize, flags: u32) -> R {
    const GRND_NONBLOCK: u32 = 1;
    const GRND_RANDOM: u32 = 2;
    const GRND_INSECURE: u32 = 4;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 || flags & (GRND_RANDOM | GRND_INSECURE) == GRND_RANDOM | GRND_INSECURE {
        return Err(crate::why::inval());
    }
    if flags & (GRND_NONBLOCK | GRND_INSECURE) == GRND_NONBLOCK && !crate::rand::ready() {
        return Err(-EAGAIN);
    }
    let len = len.min(0x7fff_f000);
    let mut s = crate::rand::stream();
    let mut b = [0u8; 256];
    let mut done = 0;
    while done < len {
        let page = PGSIZE - (buf + done) % PGSIZE;
        if done > 0 && page == PGSIZE {
            let p = proc::current();
            if p.killed || crate::signal::deliverable(p) {
                break;
            }
        }
        let n = b.len().min(len - done).min(page);
        s.fill(&mut b[..n]);
        if let Err(e) = out(buf + done, &b[..n]) {
            if done == 0 {
                crate::crypto::wipe(&mut b);
                return Err(e);
            }
            break;
        }
        done += n;
    }
    crate::crypto::wipe(&mut b);
    Ok(done as i64)
}

/// timespec を読んで、正しいか (秒が負でなく、ns が 10 億より下: Linux の timespec64_valid。でなければ EINVAL)
fn read_valid_timespec(va: usize) -> Result<u64, i64> {
    let mut ts = [0u8; 16];
    proc::current().pt().copy_in(&mut ts, va).ok_or(-EFAULT)?;
    let sec = i64::from_le_bytes(ts[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(ts[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(crate::why::inval());
    }
    Ok((sec as u64).saturating_mul(1_000_000_000).saturating_add(nsec as u64))
}

/// clock_nanosleep (Linux の posix-timers の clock_nanosleep): 知らない時計は EINVAL、眠れない時計 (THREAD_CPUTIME_ID、
/// MONOTONIC_RAW、COARSE、fd の時計) は EOPNOTSUPP、それから timespec (EFAULT、EINVAL)。CPU の時計は、その CPU 時間に
/// なるまで tick ごとに見て眠る (自分のスレッドの時計は EINVAL: 眠っているあいだは進まない)
fn sys_clock_nanosleep(clock: u64, flags: u64, req: usize, rem: usize) -> R {
    const TIMER_ABSTIME: u64 = 1;
    const EOPNOTSUPP: i64 = 95;
    let clk = clock as i32;
    match clk {
        0..=2 | 7..=9 | 11 => {}
        3..=6 => return Err(-EOPNOTSUPP),
        c if c < 0 && c & 7 == 3 => return Err(-EOPNOTSUPP),
        c if c < 0 => {}
        _ => return Err(crate::why::inval()),
    }
    if clk != 2 && clk >= 0 {
        read_valid_timespec(req)?;
        return sys_nanosleep(clock, flags, req, rem);
    }
    let want = read_valid_timespec(req)?;
    let c = timer::clock_of(clk, false)?;
    let me = proc::current();
    if let timer::Clock::Cpu { thread: true, pid, .. } = c {
        if pid == me.pid {
            return Err(crate::why::inval());
        }
    }
    let now = timer::clock_ns(c)?;
    let target = if flags & TIMER_ABSTIME != 0 { want } else { now.saturating_add(want) };
    loop {
        // 相手がいなくなったら起きる
        let Ok(v) = timer::clock_ns(c) else { return Ok(0) };
        if v >= target {
            return Ok(0);
        }
        if let Err(e) = proc::sleep_until(0, timer::ticks() + 1) {
            if rem != 0 && flags & TIMER_ABSTIME == 0 {
                out(rem, &timespec(target.saturating_sub(timer::clock_ns(c).unwrap_or(target))))?;
            }
            return Err(e);
        }
    }
}

/// nanosleep / clock_nanosleep (壁と起動からの時計)。シグナルで起こされたら残りを rem に書いて EINTR
fn sys_nanosleep(clock: u64, flags: u64, req: usize, rem: usize) -> R {
    const TIMER_ABSTIME: u64 = 1;
    let want = read_valid_timespec(req)?;
    let now_ns = || if matches!(clock as i32, 0 | 8 | 11) { timer::epoch_ns() } else { timer::uptime_ns() };
    let rel = if flags & TIMER_ABSTIME != 0 { want.saturating_sub(now_ns()) } else { want };
    // 期限は起動からの ns (tick の境めでなく、そのときに起きる: proc::sleep_until_ns)
    let until = timer::uptime_ns().saturating_add(rel).max(1);
    while timer::uptime_ns() < until {
        // chan 0 は誰も起こさないので、期限まで眠る
        if let Err(e) = proc::sleep_until_ns(0, until) {
            if rem != 0 && flags & TIMER_ABSTIME == 0 {
                out(rem, &timespec(until.saturating_sub(timer::uptime_ns())))?;
            }
            return Err(e);
        }
    }
    Ok(0)
}

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;

/// 調べるための記録: 最近の futex の WAKE (tgid, tid, アドレス, op, 起こした数)。/proc/PID/stack に出す
static FUTEX_LOG: Bkl<[(u32, u32, usize, u64, usize); 256]> = Bkl::new([(0, 0, 0, 0, 0); 256]);
static FUTEX_LOG_AT: Bkl<usize> = Bkl::new(0);

fn futex_log(tgid: u32, tid: u32, uaddr: usize, op: u64, n: usize) {
    let at = FUTEX_LOG_AT.get();
    FUTEX_LOG.get()[*at % 256] = (tgid, tid, uaddr, op, n);
    *at += 1;
}

/// tgid の最近の futex の WAKE (古い順)
pub fn futex_wakes(tgid: u32) -> alloc::vec::Vec<(u32, usize, u64, usize)> {
    let mut v = alloc::vec::Vec::new();
    let at = *FUTEX_LOG_AT.get();
    let log = FUTEX_LOG.get();
    for k in at.saturating_sub(256)..at {
        let (t, tid, a, op, n) = log[k % 256];
        if t == tgid {
            v.push((tid, a, op, n));
        }
    }
    v
}

const FUTEX_PRIVATE_FLAG: u64 = 128;

/// futex(2) は futex.rs。起こしたものは記録する (/proc/PID/stack)
fn sys_futex(uaddr: usize, op: u64, val: u32, timeout: usize, uaddr2: usize, val3: u32) -> R {
    let r = crate::futex::futex(uaddr, op, val, timeout, uaddr2, val3);
    if let Ok(n) = r {
        if matches!(op & 0x7f, FUTEX_WAKE | FUTEX_WAKE_BITSET | FUTEX_REQUEUE | FUTEX_CMP_REQUEUE | crate::futex::WAKE_OP) {
            let p = proc::current();
            futex_log(p.tgid, p.pid, uaddr, op, n as usize);
        }
    }
    r
}


/// 引数 (と環境) の合計と 1 つの長さの上限 (Linux の ARG_MAX の既定と MAX_ARG_STRLEN)
const ARGS_TOTAL: usize = 2 * 1024 * 1024;
const MAXSTR: usize = 128 * 1024;

/// 知らないシステムコールの番号を、はじめて見たか (見たものは覚える。大きなロックの中で呼ぶ)
fn unknown_first(n: u64) -> bool {
    static SEEN: crate::cell::Bkl<[u64; 8]> = crate::cell::Bkl::new([0; 8]);
    let Some(w) = SEEN.get().get_mut(n as usize / 64) else { return true };
    let bit = 1u64 << (n % 64);
    let first = *w & bit == 0;
    *w |= bit;
    first
}

const CLONE_PIDFD: u64 = 0x1000;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_DETACHED: u64 = 0x40_0000;

/// clone(flags, stack, parent_tid, tls, child_tid)。CLONE_PIDFD なら子を指す pidfd を parent_tid に書く (Linux の
/// kernel_clone: 同じ場所に書く CLONE_PARENT_SETTID と、CLONE_DETACHED とはいっしょにできない: EINVAL)。
/// Go の os/exec はこれで子を作って waitid(P_PIDFD) で待つ (前は pidfd を書かず、Go は pidfd を使わない道にしていた)
fn sys_clone(flags: u64, stack: usize, ptid: usize, tls: u64, ctid: usize) -> Result<u32, i64> {
    if flags & CLONE_PIDFD == 0 {
        return proc::clone(flags, stack, ptid, tls, ctid);
    }
    if flags & (CLONE_PARENT_SETTID | CLONE_DETACHED) != 0 {
        return Err(crate::why::inval());
    }
    clone_pidfd(flags, stack, ptid, tls, ctid, ptid)
}

/// clone して、子の pidfd (O_RDWR | O_CLOEXEC。CLONE_THREAD なら PIDFD_THREAD) の番号を at に書く。書けない場所なら
/// 子を作らずに EFAULT (Linux は子を走らせる前に put_user する)。fd は子が親の表を写したあとで足すので、子の表には
/// 入らない (CLONE_FILES なら同じ表なので見える。Linux と同じ)。vfork なら子が exec するか終わってから書く
fn clone_pidfd(flags: u64, stack: usize, ptid: usize, tls: u64, ctid: usize, at: usize) -> Result<u32, i64> {
    proc::current().pt().copy_out(at, &(-1i32).to_le_bytes()).ok_or(-EFAULT)?;
    let tid = proc::clone(flags & !CLONE_PIDFD, stack, ptid, tls, ctid)?;
    let fd = pidfd_install(tid, flags & 0x1_0000 != 0, false)?;
    proc::current().pt().copy_out(at, &fd.to_le_bytes()).ok_or(-EFAULT)?;
    Ok(tid)
}

/// clone3(args, size): struct clone_args (u64 の並び: flags pidfd child_tid parent_tid exit_signal stack stack_size tls
/// set_tid set_tid_size cgroup) を読んで clone と同じことを。glibc は pthread_create で先にこれを試す。
/// stack はいちばん下の番地で、子の sp は stack + stack_size。CLONE_PIDFD なら pidfd の場所に子の pidfd を書く
/// (parent_tid と同じ場所で CLONE_PARENT_SETTID もなら EINVAL)。set_tid と cgroup はないので EINVAL
fn sys_clone3(uargs: usize, size: usize) -> Result<u32, i64> {
    const EINVAL: i64 = 22;
    const E2BIG: i64 = 7;
    const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
    // 版 0 は 64 バイト (tls まで)。知らない後ろのものは 0 でなければならない (Linux と同じ)
    if size < 64 {
        return Err(crate::why::inval());
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = alloc::vec![0u8; size];
    proc::current().pt().copy_in(&mut b, uargs).ok_or(-EFAULT)?;
    let f = |i: usize| b.get(i * 8..i * 8 + 8).map_or(0, |w| u64::from_le_bytes(w.try_into().unwrap()));
    if b[64..].iter().any(|&x| x != 0) {
        // set_tid / set_tid_size / cgroup と、その先
        return Err(if size > 88 && b[88..].iter().any(|&x| x != 0) { -E2BIG } else { -EINVAL });
    }
    let (flags, exit_signal, stack, stack_size, tls) = (f(0), f(4), f(5) as usize, f(6) as usize, f(7));
    if flags & CLONE_INTO_CGROUP != 0 || flags & 0xff != 0 || exit_signal > 64 {
        return Err(crate::why::inval());
    }
    if flags & CLONE_PIDFD != 0 && (flags & CLONE_DETACHED != 0 || (flags & CLONE_PARENT_SETTID != 0 && f(1) == f(3))) {
        return Err(crate::why::inval());
    }
    if (stack == 0) != (stack_size == 0) {
        return Err(crate::why::inval());
    }
    let sp = if stack != 0 { stack + stack_size } else { 0 };
    if flags & CLONE_PIDFD != 0 {
        return clone_pidfd(flags | exit_signal, sp, f(3) as usize, tls, f(2) as usize, f(1) as usize);
    }
    proc::clone(flags | exit_signal, sp, f(3) as usize, tls, f(2) as usize)
}

/// NULL 終端のポインタ配列が指す文字列たち
fn copy_in_strv(mut va: usize) -> Result<Vec<Vec<u8>>, i64> {
    let mut v = Vec::new();
    if va == 0 {
        return Ok(v);
    }
    let pt = proc::current().pt();
    let mut total = 0;
    loop {
        let mut w = [0u8; 8];
        pt.copy_in(&mut w, va).ok_or(-EFAULT)?;
        let p = u64::from_le_bytes(w) as usize;
        if p == 0 {
            return Ok(v);
        }
        let s = pt.copy_in_str(p, MAXSTR).ok_or(-EFAULT)?;
        total += s.len() + 1 + 8;
        if total > ARGS_TOTAL {
            return Err(-E2BIG);
        }
        v.push(s);
        va += 8;
    }
}

fn sys_execve(path: usize, argv: usize, envp: usize) -> R {
    let path = proc::current().pt().copy_in_str(path, 4096).ok_or(-EFAULT)?;
    let path = String::from_utf8(path).map_err(|_| -ENOENT)?;
    let argv = copy_in_strv(argv)?;
    let envp = copy_in_strv(envp)?;
    proc::execve(&path, &argv, &envp)?;
    Ok(0)
}

/// execveat (Linux の do_execveat_common と alloc_bprm): dirfd からの名前、AT_EMPTY_PATH なら fd そのもの (O_PATH も)、
/// AT_SYMLINK_NOFOLLOW なら終わりのシンボリックリンクは ELOOP、AT_EXECVE_CHECK ならたしかめるだけ (0)。fd からの名前は
/// /dev/fd/N か /dev/fd/N/NAME になる (#! のインタプリタにわたす名前と AT_EXECFN。fd が O_CLOEXEC なら #! は ENOENT)
fn sys_execveat(dirfd: i64, path: usize, argv: usize, envp: usize, flags: u64) -> R {
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    const AT_EMPTY_PATH: u64 = 0x1000;
    const AT_EXECVE_CHECK: u64 = 0x10000;
    let path = proc::current().pt().copy_in_str(path, 4096).ok_or(-EFAULT)?;
    let path = String::from_utf8(path).map_err(|_| -ENOENT)?;
    // 名前を写すところ (getname_uflags): 空は AT_EMPTY_PATH のときだけ。flags のたしかめは開くところ (do_open_execat)
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(-ENOENT);
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_EXECVE_CHECK) != 0 {
        return Err(crate::why::inval());
    }
    let (exe, ino) = crate::sysfile::exec_open(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    let from_fd = dirfd != crate::sysfile::AT_FDCWD && !path.starts_with('/');
    let filename = match (from_fd, path.is_empty()) {
        (false, _) => path.clone(),
        (true, true) => alloc::format!("/dev/fd/{}", dirfd),
        (true, false) => alloc::format!("/dev/fd/{}/{}", dirfd, path),
    };
    let inaccessible = from_fd && proc::current().files().fds.get(dirfd as usize).and_then(|f| f.as_ref()).is_some_and(|f| f.cloexec);
    if flags & AT_EXECVE_CHECK != 0 {
        crate::exec::check(&exe, &ino)?;
        return Ok(0);
    }
    let argv = copy_in_strv(argv)?;
    let envp = copy_in_strv(envp)?;
    let t = crate::exec::Target { exe, ino, filename, inaccessible, comm_from_file: from_fd && path.is_empty() };
    proc::execve_target(t, &argv, &envp)?;
    Ok(0)
}

fn sys_wait4(pid: i64, status: usize, options: u64) -> R {
    // Linux の kernel_wait4: 知らない options は EINVAL (WNOHANG、WUNTRACED、WCONTINUED、__WNOTHREAD、__WCLONE、__WALL だけ)
    const ALLOWED: u64 = 1 | 2 | 8 | 0x2000_0000 | 0x4000_0000 | 0x8000_0000;
    if options & !ALLOWED != 0 {
        return Err(crate::why::inval());
    }
    let (pid, xstatus) = proc::wait(pid, (options & (1 | 2 | 8)) | proc::WEXITED)?;
    if status != 0 && pid != 0 {
        out(status, &xstatus.to_le_bytes())?;
    }
    Ok(pid as i64)
}

/// ns を timeval (秒, マイクロ秒) に
fn ns_timeval(ns: u64) -> [u8; 16] {
    let us = ns / 1000;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(us / 1_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(us % 1_000_000).to_le_bytes());
    b
}

/// 回収した子の (ユーザー, カーネル) の時間 (ns)
pub fn children_user_sys(tgid: u32) -> (u64, u64) {
    proc::find_leader_any(tgid).map_or((0, 0), |l| (l.child_cpu_ns - l.child_sys_ns.min(l.child_cpu_ns), l.child_sys_ns.min(l.child_cpu_ns)))
}

/// getrusage: 使った CPU 時間 (ru_utime はユーザー、ru_stime はカーネル: proc の cpu_ns と sys_ns)。ほかの欄はまだ 0
fn sys_getrusage(who: i64, buf: usize) -> R {
    const RUSAGE_SELF: i64 = 0;
    const RUSAGE_CHILDREN: i64 = -1;
    const RUSAGE_THREAD: i64 = 1;
    let me = proc::current();
    let (u, s) = match who {
        RUSAGE_SELF => proc::user_sys_ns(me, false),
        RUSAGE_CHILDREN => children_user_sys(me.tgid),
        RUSAGE_THREAD => proc::user_sys_ns(me, true),
        _ => return Err(crate::why::inval()),
    };
    let mut b = [0u8; 144];
    b[..16].copy_from_slice(&ns_timeval(u));
    b[16..32].copy_from_slice(&ns_timeval(s));
    out(buf, &b)?;
    Ok(0)
}

/// times: struct tms (utime, stime, cutime, cstime。単位は tick = 1/100 秒: USER_HZ)。起動からの tick を返す
fn sys_times(buf: usize) -> R {
    if buf != 0 {
        let me = proc::current();
        let (u, s) = proc::user_sys_ns(me, false);
        let (cu, cs) = children_user_sys(me.tgid);
        let mut b = [0u8; 32];
        for (i, v) in [u, s, cu, cs].into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&(v / timer::TICK_NS).to_le_bytes());
        }
        out(buf, &b)?;
    }
    Ok(timer::ticks() as i64)
}

/// pidfd の番号から (pid, PIDFD_THREAD のものか)
fn pidfd_of(fd: i64) -> Result<(u32, bool), i64> {
    let f = proc::current().files().get(fd as u64).cloned().ok_or(-9)?;
    let b = f.borrow();
    match b.kind {
        crate::file::Kind::PidFd(pid) => Ok((pid, b.flags & crate::file::PIDFD_THREAD != 0)),
        _ => Err(crate::why::inval()),
    }
}

fn pidfd_pid(fd: i64) -> Result<u32, i64> {
    pidfd_of(fd).map(|(pid, _)| pid)
}

/// pid (本当の番号) を指す pidfd を作って fd の表に入れる (いつも close-on-exec。Linux の pidfd_prepare と fd_install)。
/// thread は PIDFD_THREAD (そのスレッドを指す)。もう片づけられたものも作れる (Linux の PIDFD_STALE: SO_PEERPIDFD と
/// SCM_PIDFD。シグナルは ESRCH、poll はすぐ POLLIN | POLLHUP)
pub fn pidfd_install(pid: u32, thread: bool, nonblock: bool) -> Result<i32, i64> {
    let fl = crate::file::O_RDWR | if thread { crate::file::PIDFD_THREAD } else { 0 } | if nonblock { crate::file::O_NONBLOCK } else { 0 };
    let f = crate::file::new(crate::file::Kind::PidFd(pid), fl);
    crate::pidfd::opened(pid);
    let fd = proc::current().files().add(f, true, 0).ok_or(-24)?;
    Ok(fd as i32)
}

/// pidfd_open(pid, flags): プロセスを指す fd (終わると読めるようになる)。PIDFD_THREAD ならスレッドの番号でもよい
/// (そのスレッドを指す)。Linux 7.4 の順: 知らない印と 0 以下は EINVAL、いなければ ESRCH、代表でないスレッドを
/// PIDFD_THREAD なしで指せば ENOENT。終わってまだ片づけられていないもの (ゾンビ) も開ける
fn sys_pidfd_open(pid: i64, flags: u64) -> R {
    const PIDFD_NONBLOCK: u64 = 0o4000;
    const PIDFD_THREAD: u64 = crate::file::PIDFD_THREAD as u64;
    const ESRCH: i64 = 3;
    const ENOENT: i64 = 2;
    if pid <= 0 || flags & !(PIDFD_NONBLOCK | PIDFD_THREAD) != 0 {
        return Err(crate::why::inval());
    }
    let t = proc::find_any(pid as u32).ok_or(-ESRCH)?;
    if flags & PIDFD_THREAD == 0 && !t.is_leader() {
        return Err(-ENOENT);
    }
    pidfd_install(pid as u32, flags & PIDFD_THREAD != 0, flags & PIDFD_NONBLOCK != 0).map(|fd| fd as i64)
}

/// waitid(idtype, id, siginfo, options, rusage): 結果を siginfo で返す wait
fn sys_waitid(idtype: u64, id: i64, info: usize, options: u64, rusage: usize) -> R {
    const P_ALL: u64 = 0;
    const P_PID: u64 = 1;
    const P_PGID: u64 = 2;
    const P_PIDFD: u64 = 3;
    const WNOHANG: u64 = 1;
    const WSTOPPED: u64 = 2;
    const WEXITED: u64 = 4;
    const WCONTINUED: u64 = 8;
    // Linux の kernel_waitid の順: 知らない options、WEXITED / WSTOPPED / WCONTINUED のどれもない、idtype と id
    const ALLOWED: u64 = WNOHANG | WSTOPPED | WEXITED | WCONTINUED | proc::WNOWAIT | 0x2000_0000 | 0x4000_0000 | 0x8000_0000;
    if options & !ALLOWED != 0 {
        return Err(crate::why::inval());
    }
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        return Err(crate::why::inval());
    }
    let pid = match idtype {
        P_ALL => -1,
        P_PID if id > 0 => crate::ns::arg_pid(id).map_err(|_| -10)?,
        P_PGID if id < 0 => return Err(crate::why::inval()),
        P_PGID => if id == 0 { 0 } else { crate::ns::arg_pid(-id).map_err(|_| -10)? },
        P_PIDFD => pidfd_pid(id)? as i64,
        _ => return Err(crate::why::inval()),
    };
    // wait4 の WUNTRACED は WSTOPPED と同じ値
    let (cpid, st) = proc::wait(pid, options & (WNOHANG | WSTOPPED | WEXITED | WCONTINUED | proc::WNOWAIT))?;
    if rusage != 0 {
        out(rusage, &[0u8; 144])?;
    }
    if info != 0 {
        // siginfo_t: signo, errno, code, (詰め物), pid, uid, status
        let mut b = [0u8; 128];
        if cpid != 0 {
            let (code, status) = if st == 0xffff {
                (signal::CLD_CONTINUED, signal::SIGCONT)
            } else if st & 0xff == 0x7f {
                (signal::CLD_STOPPED, (st >> 8) & 0xff)
            } else if st & 0x7f == 0 {
                (signal::CLD_EXITED, (st >> 8) & 0xff)
            } else {
                (if st & 0x80 != 0 { 3 } else { signal::CLD_KILLED }, st & 0x7f)
            };
            let uid = proc::current().cred.uid;
            b[0..4].copy_from_slice(&signal::SIGCHLD.to_le_bytes());
            b[8..12].copy_from_slice(&code.to_le_bytes());
            b[16..20].copy_from_slice(&(cpid as i32).to_le_bytes());
            b[20..24].copy_from_slice(&uid.to_le_bytes());
            b[24..28].copy_from_slice(&status.to_le_bytes());
        }
        out(info, &b)?;
    }
    Ok(0)
}

/// 電源を切る / 再起動する (PSCI)
fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> R {
    if !proc::current().cred.capable(cred::CAP_SYS_BOOT) {
        return Err(-(cred::EPERM));
    }
    const MAGIC1: u32 = 0xfee1_dead;
    const MAGIC2: [u32; 4] = [672274793, 85072278, 369367448, 537993216];
    const CMD_RESTART: u32 = 0x0123_4567;
    const CMD_HALT: u32 = 0xcdef_0123;
    const CMD_POWER_OFF: u32 = 0x4321_fedc;
    const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
    const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;
    if magic1 != MAGIC1 || !MAGIC2.contains(&magic2) {
        return Err(crate::why::inval());
    }
    let fid = match cmd {
        CMD_POWER_OFF | CMD_HALT => PSCI_SYSTEM_OFF,
        CMD_RESTART => PSCI_SYSTEM_RESET,
        _ => return Err(crate::why::inval()),
    };
    // 眠って書くあいだは、ほかの CPU が動く (終わりかけのプロセスが、閉じたファイルや消したが使っていたもの (orphan) を
    // 片づける)。その変わったものも書くように、さいごにもう一度、大きなロックを持ったまま (そのあいだはほかは動かない)。
    // 変わったものがなければすぐ終わる
    crate::vfs::sync_all_wait();
    crate::vfs::sync_all();
    println!("aios: {}", if fid == PSCI_SYSTEM_OFF { "power off" } else { "restart" });
    // PSCI の呼び方は DTB の /psci の method (DTB がなければ qemu virt の hvc)。無ければ止まるだけ
    match crate::dtb::psci_method() {
        Some("hvc") => unsafe { core::arch::asm!("hvc #0", in("x0") fid, options(nostack)) },
        Some("smc") => unsafe { core::arch::asm!("smc #0", in("x0") fid, options(nostack)) },
        _ => println!("aios: no PSCI; halted"),
    }
    // PSCI が無ければ止まる
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// prctl: 名前 (PR_SET_NAME / PR_GET_NAME)、no_new_privs、seccomp、投機の止め方 (PR_GET/SET_SPECULATION_CTRL)、dumpable、
/// タイマーのゆとり (PR_SET/GET_TIMERSLACK)、PDEATHSIG、CHILD_SUBREAPER、KEEPCAPS と SECUREBITS、CAPBSET、CAP_AMBIENT、
/// TIMING、THP_DISABLE、TSC、MCE_KILL、IO_FLUSHER。知らないものは EINVAL (Linux と同じ)。
/// ほかは何もしない
fn sys_prctl(op: u64, arg: usize, arg3: usize, arg4: u64, arg5: u64) -> R {
    const PR_GET_SPECULATION_CTRL: u64 = 52;
    const PR_SET_SPECULATION_CTRL: u64 = 53;
    const PR_GET_SECCOMP: u64 = 21;
    const PR_TASK_PERF_EVENTS_DISABLE: u64 = 31;
    const PR_TASK_PERF_EVENTS_ENABLE: u64 = 32;
    const PR_SET_SECCOMP: u64 = 22;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    const PR_GET_DUMPABLE: u64 = 3;
    const PR_SET_DUMPABLE: u64 = 4;
    const PR_GET_TIMERSLACK: u64 = 30;
    const PR_SET_TIMERSLACK: u64 = 29;
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_PDEATHSIG: u64 = 2;
    const PR_GET_KEEPCAPS: u64 = 7;
    const PR_SET_KEEPCAPS: u64 = 8;
    const PR_GET_TIMING: u64 = 13;
    const PR_SET_TIMING: u64 = 14;
    const PR_CAPBSET_READ: u64 = 23;
    const PR_CAPBSET_DROP: u64 = 24;
    const PR_GET_TSC: u64 = 25;
    const PR_SET_TSC: u64 = 26;
    const PR_GET_SECUREBITS: u64 = 27;
    const PR_SET_SECUREBITS: u64 = 28;
    const PR_MCE_KILL: u64 = 33;
    const PR_MCE_KILL_GET: u64 = 34;
    const PR_SET_MM: u64 = 35;
    const PR_SET_CHILD_SUBREAPER: u64 = 36;
    const PR_GET_CHILD_SUBREAPER: u64 = 37;
    const PR_GET_TID_ADDRESS: u64 = 40;
    const PR_SET_THP_DISABLE: u64 = 41;
    const PR_GET_THP_DISABLE: u64 = 42;
    const PR_CAP_AMBIENT: u64 = 47;
    const PR_SET_IO_FLUSHER: u64 = 57;
    const PR_GET_IO_FLUSHER: u64 = 58;
    /// CAP_LAST_CAP (40: CAP_CHECKPOINT_RESTORE)
    const CAP_LAST: u64 = 40;
    const EPERM: i64 = 1;
    let put_int = |va: usize, v: i32| out(va, &v.to_le_bytes());
    let p = proc::current();
    match op {
        // 親が終わったときのシグナル (0 から 64。proc::reparent_kids が送る)
        PR_SET_PDEATHSIG => {
            if arg > 64 {
                return Err(crate::why::inval());
            }
            p.pdeath_signal = arg as u8;
        }
        PR_GET_PDEATHSIG => put_int(arg, p.pdeath_signal as i32)?,
        // set_tid_address と CLONE_CHILD_CLEARTID の番地 (Linux は CHECKPOINT_RESTORE のときだけ)
        PR_GET_TID_ADDRESS => out(arg, &(p.clear_tid as u64).to_le_bytes())?,
        // みなし子をひきとる (代表スレッドに。proc::reparent_kids が見る)
        PR_SET_CHILD_SUBREAPER => {
            let l = if p.pid != p.tgid { proc::find_leader_any(p.tgid) } else { None };
            match l {
                Some(l) => l.child_subreaper = arg != 0,
                None => p.child_subreaper = arg != 0,
            }
        }
        PR_GET_CHILD_SUBREAPER => {
            let l = if p.pid != p.tgid { proc::find_leader_any(p.tgid) } else { None };
            let v = l.map_or(p.child_subreaper, |l| l.child_subreaper);
            put_int(arg, v as i32)?;
        }
        // securebits の KEEP_CAPS (4)。LOCKED (5) なら変えられない
        PR_GET_KEEPCAPS => return Ok((p.securebits >> 4 & 1) as i64),
        PR_SET_KEEPCAPS => {
            if arg > 1 {
                return Err(crate::why::inval());
            }
            if p.securebits & 1 << 5 != 0 {
                return Err(-EPERM);
            }
            p.securebits = p.securebits & !(1 << 4) | (arg as u32) << 4;
        }
        PR_GET_SECUREBITS => return Ok(p.securebits as i64),
        // CAP_SETPCAP がなければ、知らないビット (0xfff の外)・ロックしたものを変える・ロックを外すのは EPERM (Linux の
        // cap_task_prctl)
        PR_SET_SECUREBITS => {
            let old = p.securebits as u64;
            let locks = old & 0xaaa;
            if (locks >> 1) & (old ^ arg as u64) != 0 || locks & !(arg as u64) != 0 || arg as u64 & !0xfff != 0 || !p.cred.capable(cred::CAP_SETPCAP) {
                return Err(-EPERM);
            }
            p.securebits = arg as u32;
        }
        PR_CAPBSET_READ => {
            if arg as u64 > CAP_LAST {
                return Err(crate::why::inval());
            }
            return Ok((p.capbset_dropped >> arg & 1 == 0) as i64);
        }
        PR_CAPBSET_DROP => {
            if !p.cred.capable(cred::CAP_SETPCAP) {
                return Err(-EPERM);
            }
            if arg as u64 > CAP_LAST {
                return Err(crate::why::inval());
            }
            p.capbset_dropped |= 1 << arg;
        }
        // ambient の capability はない: IS_SET は 0、RAISE は EPERM (継ぐ集合が空: Linux の root も同じ)
        PR_CAP_AMBIENT => {
            if arg == 4 {
                if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                    return Err(crate::why::inval());
                }
                return Ok(0);
            }
            if arg3 as u64 > CAP_LAST || arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            return match arg {
                1 | 3 => Ok(0),
                2 => Err(-EPERM),
                _ => Err(crate::why::inval()),
            };
        }
        PR_GET_TIMING => return Ok(0),
        PR_SET_TIMING => {
            if arg != 0 {
                return Err(crate::why::inval());
            }
        }
        // 大きなページは aios にないが、印は持ちものごとに覚える (fork と exec で受けつぐ)
        PR_SET_THP_DISABLE => {
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            p.mm().thp_disable = arg != 0;
        }
        PR_GET_THP_DISABLE => {
            if arg != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            return Ok(p.mm().thp_disable as i64);
        }
        // カウンタ (CNTVCT) はいつも読める (PR_TSC_ENABLE)。止める (PR_TSC_SIGSEGV) はまだ
        PR_GET_TSC => put_int(arg, 1)?,
        PR_SET_TSC => {
            if arg != 1 {
                return Err(crate::why::inval());
            }
        }
        PR_MCE_KILL => {
            if arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            p.mce_kill = match (arg, arg3) {
                (0, 0) => 0,
                (1, 0) => 1,
                (1, 1) => 2,
                (1, 2) => 0,
                _ => return Err(crate::why::inval()),
            };
        }
        PR_MCE_KILL_GET => {
            if arg != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            return Ok(match p.mce_kill {
                1 => 0,
                2 => 1,
                _ => 2,
            });
        }
        PR_SET_MM | PR_GET_IO_FLUSHER | PR_SET_IO_FLUSHER if !p.cred.capable(cred::CAP_SYS_RESOURCE) => return Err(-EPERM),
        PR_GET_IO_FLUSHER => return Ok(0),
        PR_SET_IO_FLUSHER => {
            if arg > 1 {
                return Err(crate::why::inval());
            }
        }
        // タイマーのゆとり: 0 なら fork したときの値にもどす (Linux の prctl。aios のタイマーは使わない)
        PR_SET_TIMERSLACK => p.timer_slack_ns = if arg == 0 { p.default_slack_ns } else { arg as u64 },
        PR_GET_TIMERSLACK => return Ok(p.timer_slack_ns as i64),
        // 持ちもの (mm) の dumpable (pidfd.rs: PIDFD_GET_INFO の coredump の印、pidfd_getfd の許し)。つけられるのは 0 と 1
        PR_GET_DUMPABLE => return Ok(p.mm().dumpable as i64),
        PR_SET_DUMPABLE => {
            if arg > 1 {
                return Err(crate::why::inval());
            }
            p.mm().dumpable = arg as u8;
        }
        // 一度つけたら外せない (arg は 1 だけ)
        PR_SET_NO_NEW_PRIVS => {
            if arg != 1 {
                return Err(-22);
            }
            p.cred.no_new_privs = true;
        }
        PR_GET_NO_NEW_PRIVS => return Ok(p.cred.no_new_privs as i64),
        PR_GET_SECCOMP => return Ok(crate::security::prctl_get_seccomp(&p.cred)),
        // 自分が開いた perf のものをみな止める / 動かす (perf.rs。なければ PERF_EVENTS のない Linux と同じ EINVAL)
        PR_TASK_PERF_EVENTS_DISABLE | PR_TASK_PERF_EVENTS_ENABLE => {
            return crate::proc::perf().map(|h| h.task_enable(op == PR_TASK_PERF_EVENTS_ENABLE)).ok_or_else(crate::why::inval);
        }
        // seccomp を切っていれば (aios.off=seccomp)、入れずに作った Linux と同じ EINVAL
        PR_SET_SECCOMP if crate::parts::off(crate::parts::Part::Seccomp) => return Err(crate::why::inval()),
        PR_SET_SECCOMP => return crate::security::prctl_set_seccomp(arg as u64, arg3),
        PR_SET_NAME => {
            let mut b = [0u8; 16];
            p.pt().copy_in(&mut b[..15], arg).ok_or(-14)?;
            p.set_comm(&b);
        }
        PR_GET_NAME => {
            let c = p.comm;
            p.pt().copy_out(arg, &c).ok_or(-14)?;
        }
        // 投機的なストアの追いこし (cpuvuln.rs。Linux の arch_prctl_spec_ctrl_get / set): このスレッドだけ
        PR_GET_SPECULATION_CTRL => {
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            return crate::cpuvuln::prctl_get(arg as u64, p.spec_ssb);
        }
        PR_SET_SPECULATION_CTRL => {
            if arg4 != 0 || arg5 != 0 {
                return Err(crate::why::inval());
            }
            p.spec_ssb = crate::cpuvuln::prctl_set(arg as u64, arg3 as u64, p.spec_ssb)?;
            let ssb = p.spec_ssb;
            crate::cpuvuln::task_spsr(&mut p.tf().spsr, ssb);
        }
        // 知らないもの (手本の Linux の設定にないもの: SET_MM の中身、TID_ADDRESS、SET_VMA、MEMORY_MERGE、SVE など) は EINVAL
        _ => return Err(crate::why::inval()),
    }
    Ok(0)
}
