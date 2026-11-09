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

const ENOENT: i64 = 2;
const E2BIG: i64 = 7;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EBADF: i64 = 9;
const ENOSYS: i64 = 38;
const ENODATA: i64 = 61;
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
    SPLICE = 76,
    TEE = 77,
    READLINKAT = 78,
    NEWFSTATAT = 79,
    FSTAT = 80,
    SYNC = 81,
    FSYNC = 82,
    FDATASYNC = 83,
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
    UNSHARE = 97,
    GETRLIMIT = 163,
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
    MPROTECT = 226,
    MSYNC = 227,
    MINCORE = 232,
    MADVISE = 233,
    // NUMA (メモリをどのノードに置くか)。aios はノードが 1 つなので、Linux の NUMA なしと同じく何もせず成功
    MBIND = 235,
    GET_MEMPOLICY = 236,
    SET_MEMPOLICY = 237,
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
pub fn dispatch(tf: &mut TrapFrame) -> Option<Restart> {
    use nr::*;
    // 引数は 6 つ (x0〜x5)。tf.x を 31 ぜんぶ写すと、それだけでシステムコールごとに 248 バイト
    let a = [tf.x[0], tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5]];
    let nr = tf.x[8];
    crate::debug::trace_sys_enter(nr, a[0]);
    let p = proc::current();
    p.orig_x0 = a[0];
    p.last_sys = (nr, a[0], a[1]);
    p.why.clear();
    crate::debug::count_sys(nr);
    // ヒープが予備にまで手をつけていれば、ここ (安全なところ) で空ける (oom.rs)
    crate::oom::check();
    // seccomp: かかっていれば、フィルタの答えにしたがう (security::seccomp_permit_syscall。s は写して持つ: user notification
    // で眠るあいだに、ほかのスレッドの TSYNC で入れかわっても消えないように)
    if let Some(s) = p.cred.seccomp.clone() {
        let args = [a[0], a[1], a[2], a[3], a[4], a[5]];
        match crate::security::seccomp_permit_syscall(&s, nr, &args, tf.elr) {
            crate::security::Permit::Allow => {}
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
        UMOUNT2 => sysfile::umount2(a[0] as usize, a[1]),
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
        // xattr は持っていない
        LISTXATTR | LLISTXATTR | FLISTXATTR => Ok(0),
        GETXATTR | LGETXATTR | FGETXATTR => Err(-ENODATA),
        SETXATTR | LSETXATTR | FSETXATTR => Err(-EOPNOTSUPP),
        // 先読みのお願いは聞くだけ (ext4 の読み込みが自分で先読みする)
        FADVISE64 => sysfile::fadvise(a[0], a[1] as i64, a[2] as i64, a[3]),
        READAHEAD => Ok(0),
        // 優先度 (nice) はまだ持たない: いつも 0 (システムコールの返り値は Linux と同じく 20 - nice)、変えるのは受けつけるだけ
        GETPRIORITY => Ok(20),
        SETPRIORITY => Ok(0),

        EXIT => proc::exit(a[0] as i32 & 0xff),
        EXIT_GROUP => proc::exit_group(a[0] as i32 & 0xff),
        UNSHARE => proc::unshare(a[0]),
        CLONE => proc::clone(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize).map(|t| crate::ns::to_local(t).unwrap_or(t) as i64),
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
        RT_SIGRETURN => signal::rt_sigreturn(tf),
        SET_TID_ADDRESS => {
            let p = proc::current();
            p.clear_tid = a[0] as usize;
            Ok(crate::ns::local_or_0(p.pid) as i64)
        }
        FUTEX => sys_futex(a[0] as usize, a[1], a[2] as u32, a[3] as usize, a[4] as usize, a[5] as u32),
        FUTEX_WAITV => crate::futex::waitv(a[0] as usize, a[1] as u32, a[2] as u32, a[3] as usize, a[4] as u32),
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
        SCHED_GETAFFINITY => sys_sched_getaffinity(a[1] as usize, a[2] as usize),
        // スケジューラはひとつ (SCHED_OTHER、優先度 0) だけ。CPU を選ぶこともしない (どれでも動く)
        SCHED_SETAFFINITY => Ok(0),
        SCHED_GETSCHEDULER => Ok(0),
        SCHED_GETPARAM => out(a[1] as usize, &0i32.to_le_bytes()).map(|_| 0),
        SCHED_SETPARAM => Ok(0),
        // SCHED_OTHER / BATCH / IDLE はそのまま (同じに扱う)。実時間 (FIFO / RR) はできない
        SCHED_SETSCHEDULER => match a[1] & 0xff {
            0 | 3 | 5 => Ok(0),
            1 | 2 => Err(-1),
            _ => Err(crate::why::inval()),
        },
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => match a[0] {
            1 | 2 => Ok(if nr == SCHED_GET_PRIORITY_MAX { 99 } else { 1 }),
            0 | 3 | 5 => Ok(0),
            _ => Err(crate::why::inval()),
        },
        NANOSLEEP => sys_nanosleep(1, 0, a[0] as usize, a[1] as usize),
        CLOCK_NANOSLEEP => sys_clock_nanosleep(a[0], a[1], a[2] as usize, a[3] as usize),
        CLOCK_GETTIME => sys_clock_gettime(a[0], a[1] as usize),
        CLOCK_GETRES => sys_clock_getres(a[0], a[1] as usize),
        GETTIMEOFDAY => sys_gettimeofday(a[0] as usize),
        UNAME => sys_uname(a[0] as usize),
        SETHOSTNAME => sys_sethostname(a[0] as usize, a[1] as usize),
        REBOOT => sys_reboot(a[0] as u32, a[1] as u32, a[2] as u32),
        SYSINFO => sys_sysinfo(a[0] as usize),
        GETRLIMIT => sys_prlimit(a[0], 0, a[1] as usize),
        PRLIMIT64 => sys_prlimit(a[1], a[2] as usize, a[3] as usize),

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
        _ => (matches!(nr, READ | WRITE | READV | WRITEV | OPENAT | WAIT4 | WAITID | FUTEX_WAITV | ACCEPT | ACCEPT4 | RECVFROM | SENDTO | RECVMSG | SENDMSG | CONNECT), false),
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
pub fn fast(tf: &mut TrapFrame) -> bool {
    use nr::*;
    // すべてのシステムコールがまずここを通るので、写すのは引数 (x0〜x5) と番号だけ
    let a = [tf.x[0], tf.x[1], tf.x[2], tf.x[3], tf.x[4], tf.x[5]];
    let nr = tf.x[8];
    let p = proc::current();
    // seccomp がかかっていれば、フィルタを通すためにふつうの道へ。PID の namespace の中も (番号を読みかえる)
    // LSM のラベルのついたもの (AppArmor のプロファイル) も (決まりを見るのはふつうの道だけ)
    // 監査のシステムコールのルールがあれば、入れもののあるスレッドも (入口と出口でルールを見る)
    if p.has_cred_work(proc::CRED_SECCOMP) || p.pid_ns.is_some() || p.cred.ns.user.is_some() || p.cred.label.is_some() || (p.audit.is_some() && crate::security::AUDIT_N_RULES.load(core::sync::atomic::Ordering::Relaxed) != 0) {
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
        // の munmap。スレッドで分けていない表だけ (vm::PageTable::fast_map、fast_unmap)。ファイルの mmap、共有、MAP_FIXED、
        // スワップへ出したページのあるところ、知らない印や prot は、ふつうの道 (sys_mmap、sys_munmap) へ
        MMAP if a[3] & !(MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK) == 0
            && a[3] & (MAP_PRIVATE | MAP_ANONYMOUS) == MAP_PRIVATE | MAP_ANONYMOUS
            && a[2] & !7 == 0
            && a[5] % 4096 == 0
            && a[1] != 0
            && a[1] <= crate::vm::MAXVA as u64 =>
        {
            let Some(m) = p.mm.as_ref().filter(|m| m.private()) else { return false };
            let m = m.get();
            let len = pg_up(a[1] as usize);
            let Some(va) = m.pt.fast_map(m.mmap_next, len, prot_bits(a[2])) else { return false };
            m.mmap_next = va + len;
            va as i64
        }
        MUNMAP if a[0] & 0xfff == 0 && a[1] != 0 && a[0] < crate::vm::MAXVA as u64 && a[1] <= crate::vm::MAXVA as u64 => {
            let Some(m) = p.mm.as_ref().filter(|m| m.private()) else { return false };
            match m.get().pt.fast_unmap(a[0] as usize, a[0] as usize + pg_up(a[1] as usize)) {
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
            .or_else(|| crate::file::fast_dev(p, a[0], a[1] as usize, a[2] as usize, nr == WRITE))
        {
            Some(r) => r,
            None => return false,
        },
        _ => return false,
    };
    tf.x[0] = r as u64;
    // /proc/sysstat を読みはじめてから、速い道で終わったものだけ数える
    crate::debug::count_fast(nr);
    crate::debug::trace_sys_fast(nr, a[0], r);
    true
}

/// clock_gettime: 時計は timer::clock_of (知らないもの、いない CPU の時計は EINVAL)
fn sys_clock_gettime(clk: u64, ts: usize) -> R {
    let ns = timer::clock_ns(timer::clock_of(clk as i32, true)?)?;
    out(ts, &timespec(ns))?;
    Ok(0)
}

/// clock_getres: 壁と起動からの時計は tick、CPU の時計 (SCHED) は 1 ns。ts は NULL でもよい
fn sys_clock_getres(clk: u64, ts: usize) -> R {
    let c = timer::clock_of(clk as i32, false)?;
    if ts != 0 {
        out(ts, &timespec(timer::clock_res(c)))?;
    }
    Ok(0)
}

fn sys_gettimeofday(tv: usize) -> R {
    if tv != 0 {
        let ns = timer::epoch_ns();
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        out(tv, &b)?;
    }
    Ok(0)
}

/// uname の nodename (sethostname で変わる。init が起動のときに /etc/hostname を入れる)

fn sys_sethostname(name: usize, len: usize) -> R {
    if len > 64 {
        return Err(crate::why::inval());
    }
    let mut b = [0u8; 64];
    proc::current().pt().copy_in(&mut b[..len], name).ok_or(-14)?;
    // root か、新しく作った UTS の中 (ns.rs)
    crate::ns::set_hostname(&b[..len], true)?;
    Ok(0)
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
    let fields: [&[u8]; 6] = [b"aios", &hb[..hl], env!("AIOS_RELEASE").as_bytes(), b"#1 aios", b"aarch64", b""];
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

fn sys_sched_getaffinity(len: usize, mask: usize) -> R {
    if len < 8 {
        return Err(crate::why::inval());
    }
    // 動いている CPU すべて
    let bits = (1u64 << crate::smp::online()) - 1;
    out(mask, &bits.to_le_bytes())?;
    Ok(8)
}

/// getrlimit / prlimit64 (自分のプロセスだけ)。変えられるのは RLIMIT_NOFILE のソフトの上限
fn sys_prlimit(resource: u64, new: usize, old: usize) -> R {
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    const INF: u64 = u64::MAX;
    const EPERM: i64 = 1;
    let files = proc::current().files();
    // 先に今の値 (old) を読んでから変える
    let (cur, max) = match resource {
        RLIMIT_STACK => (8 * 1024 * 1024, INF),
        RLIMIT_NOFILE => (files.nofile as u64, proc::NOFILE as u64),
        _ => (INF, INF),
    };
    if new != 0 && resource == RLIMIT_NOFILE {
        let mut b = [0u8; 16];
        proc::current().pt().copy_in(&mut b, new).ok_or(-EFAULT)?;
        let ncur = u64::from_le_bytes(b[..8].try_into().unwrap());
        let nmax = u64::from_le_bytes(b[8..].try_into().unwrap());
        if ncur > nmax {
            return Err(crate::why::inval());
        }
        // ハードの上限は上げられない (RLIM_INFINITY は上限そのものとみなす)
        if nmax != INF && nmax > proc::NOFILE as u64 && nmax > max {
            return Err(-EPERM);
        }
        files.nofile = ncur.min(proc::NOFILE as u64).max(3) as usize;
    }
    if old != 0 {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&cur.to_le_bytes());
        b[8..].copy_from_slice(&max.to_le_bytes());
        out(old, &b)?;
    }
    Ok(0)
}

fn sys_brk(addr: usize) -> i64 {
    let m = proc::current().mm();
    if addr < m.heap_start {
        return m.brk as i64;
    }
    let (old, new) = (pg_up(m.brk), pg_up(addr));
    if new != old && m.pt.resize(m.heap_start, old, new).is_none() {
        return m.brk as i64;
    }
    m.brk = addr;
    addr as i64
}

const MAP_SHARED: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_NORESERVE: u64 = 0x4000;
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
    let back = if flags & MAP_ANONYMOUS == 0 {
        let fr = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
        let f = fr.borrow();
        if f.flags & crate::file::O_PATH != 0 {
            return Err(-EBADF);
        }
        if len == 0 {
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
                // LSM (AppArmor): 実行して写すなら m (security_mmap_file)
                crate::security::mmap_file(p, prot, flags & MAP_SHARED == 0, ino.meta().uid)?;
                path = Some(p.clone());
                Backing::File { ino: ino.clone(), off, fend: usize::MAX, ver: crate::vm::file_ver(ino) }
            }
            // 装置やソケット (画面、音の再生の口、TCP のソケット: file.rs の mmap_dev。Linux の f_op->mmap)。ほかは ENODEV
            k => match crate::file::mmap_dev(k, &fr, off, len, prot) {
                Some(b) => b?,
                None => return Err(-ENODEV),
            },
        }
    } else {
        if len == 0 {
            return Err(crate::why::inval());
        }
        Backing::Anon
    };
    let m = proc::current().mm();
    let len = pg_up(len);
    let va = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if addr & 0xfff != 0 {
            return Err(crate::why::inval());
        }
        if flags & MAP_FIXED_NOREPLACE != 0 && m.pt.overlaps(addr, addr.saturating_add(len)) {
            return Err(-EEXIST);
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
    Ok(va as i64)
}

fn prot_bits(prot: u64) -> u8 {
    (prot & 7) as u8
}

fn sys_munmap(addr: usize, len: usize) -> R {
    if addr & 0xfff != 0 || len == 0 {
        return Err(crate::why::inval());
    }
    proc::current().pt().unmap(addr, addr + pg_up(len));
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
    if flags & MREMAP_FIXED == 0 {
        // その場で伸ばす (領域の終わりまで使っていて、その先が空いているとき)
        if v.end == old_end && m.pt.extend(vstart, old + new_size).is_some() {
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
    proc::current().pt().protect(addr, addr + pg_up(len), prot_bits(prot)).map(|_| 0).map_err(|_| -ENOMEM)
}

/// MADV_DONTNEED は無名のページを捨てる (次に触れると 0)。ほかは何もしない
fn sys_madvise(addr: usize, len: usize, advice: u64) -> R {
    const MADV_DONTNEED: u64 = 4;
    if addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    if advice == MADV_DONTNEED {
        proc::current().pt().discard(addr, addr + pg_up(len));
    }
    Ok(0)
}

/// mincore: ページがメモリにあるか。写してある領域はみな「ある」と答える (スワップに出ていても読めば戻る)。
/// 写していないところがあれば ENOMEM
fn sys_mincore(addr: usize, len: usize, vec: usize) -> R {
    const ENOMEM: i64 = 12;
    if addr & 0xfff != 0 {
        return Err(crate::why::inval());
    }
    let n = pg_up(len) / 4096;
    let m = proc::current().mm();
    if (0..n).any(|i| m.pt.find(addr + i * 4096).is_none()) {
        return Err(-ENOMEM);
    }
    out(vec, &alloc::vec![1u8; n])?;
    Ok(0)
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
    let until = timer::ticks_after(rel);
    while timer::ticks() < until {
        // chan 0 は誰も起こさないので、期限まで眠る
        if let Err(e) = proc::sleep_until(0, until) {
            if rem != 0 && flags & TIMER_ABSTIME == 0 {
                let left = (until - timer::ticks().min(until)) * 1_000_000_000 / timer::HZ;
                out(rem, &timespec(left))?;
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

/// clone3(args, size): struct clone_args (u64 の並び: flags pidfd child_tid parent_tid exit_signal stack stack_size tls
/// set_tid set_tid_size cgroup) を読んで clone と同じことを。glibc は pthread_create で先にこれを試す。
/// stack はいちばん下の番地で、子の sp は stack + stack_size。pidfd、set_tid、cgroup はないので EINVAL
fn sys_clone3(uargs: usize, size: usize) -> Result<u32, i64> {
    const EINVAL: i64 = 22;
    const E2BIG: i64 = 7;
    const CLONE_PIDFD: u64 = 0x1000;
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
    if flags & (CLONE_PIDFD | CLONE_INTO_CGROUP) != 0 || flags & 0xff != 0 || exit_signal > 64 {
        return Err(crate::why::inval());
    }
    if (stack == 0) != (stack_size == 0) {
        return Err(crate::why::inval());
    }
    let sp = if stack != 0 { stack + stack_size } else { 0 };
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

/// prctl: 名前 (PR_SET_NAME / PR_GET_NAME)、no_new_privs、seccomp、投機の止め方 (PR_GET/SET_SPECULATION_CTRL)、dumpable。
/// ほかは何もしない
fn sys_prctl(op: u64, arg: usize, arg3: usize, arg4: u64, arg5: u64) -> R {
    const PR_GET_SPECULATION_CTRL: u64 = 52;
    const PR_SET_SPECULATION_CTRL: u64 = 53;
    const PR_GET_SECCOMP: u64 = 21;
    const PR_SET_SECCOMP: u64 = 22;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    const PR_GET_DUMPABLE: u64 = 3;
    const PR_SET_DUMPABLE: u64 = 4;
    let p = proc::current();
    match op {
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
        _ => {}
    }
    Ok(0)
}
