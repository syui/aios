// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::cred;
use crate::proc;
use crate::signal::{self, Restart};
use crate::socket;
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
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const ENODATA: i64 = 61;
const EOPNOTSUPP: i64 = 95;
const ETIMEDOUT: i64 = 110;

// システムコールの番号 (nr::READ など) と名前 (name。/proc/bkl と strace で見せる)
macro_rules! nrs {
    ($($name:ident = $n:literal,)*) => {
        mod nr {
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
    };
}

nrs! {
    SETXATTR = 5,
    FSETXATTR = 7,
    GETXATTR = 8,
    FGETXATTR = 10,
    LISTXATTR = 11,
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
    CLOSE_RANGE = 436,
    FACCESSAT2 = 439,
}

/// C の int 引数: 下位 32bit を符号拡張する
fn int(v: u64) -> i64 {
    v as i32 as i64
}

/// システムコールを動かす。待ちが割り込まれて EINTR になったら、その情報を返す
pub fn dispatch(tf: &mut TrapFrame) -> Option<Restart> {
    use nr::*;
    let a = tf.x;
    let nr = tf.x[8];
    proc::current().orig_x0 = tf.x[0];
    proc::current().last_sys = (nr, tf.x[0], tf.x[1]);
    count(nr);
    // seccomp: かかっていれば、フィルタの答えにしたがう
    if let Some(s) = proc::current().cred.seccomp.clone() {
        let args = [a[0], a[1], a[2], a[3], a[4], a[5]];
        match crate::seccomp::check(&s, nr, &args, tf.elr) {
            crate::seccomp::Verdict::Allow => {}
            crate::seccomp::Verdict::Errno(e) => {
                tf.x[0] = e as u64;
                strace(nr, &a, Err(e));
                return None;
            }
            crate::seccomp::Verdict::Trap(errno) => {
                signal::force_sigsys(nr, tf.elr, errno);
                tf.x[0] = (-ENOSYS) as u64;
                return None;
            }
            crate::seccomp::Verdict::Kill => {
                println!("seccomp: pid {} killed at syscall {}", proc::current().pid, nr);
                proc::die(signal::SIGSYS);
            }
        }
    }
    let r = match tf.x[8] {
        GETCWD => sysfile::getcwd(a[0] as usize, a[1] as usize),
        FLOCK => sysfile::flock(a[0], a[1]),
        CLOSE_RANGE => sysfile::close_range(a[0], a[1], a[2]),
        DUP => sysfile::dup(a[0]),
        DUP3 => sysfile::dup3(a[0], a[1], a[2]),
        FCNTL => sysfile::fcntl(a[0], a[1], a[2]),
        EVENTFD2 => crate::epoll::eventfd2(a[0], a[1]),
        TIMERFD_CREATE => crate::timerfd::create(a[0], a[1]),
        TIMERFD_SETTIME => crate::timerfd::settime(a[0], a[1], a[2] as usize, a[3] as usize),
        TIMERFD_GETTIME => crate::timerfd::gettime(a[0], a[1] as usize),
        INOTIFY_INIT1 => crate::inotify::init1(a[0]),
        INOTIFY_ADD_WATCH => sysfile::inotify_add_watch(a[0], a[1] as usize, a[2] as u32),
        INOTIFY_RM_WATCH => crate::inotify::rm_watch(a[0], a[1] as i32),
        EPOLL_CREATE1 => crate::epoll::create1(a[0]),
        EPOLL_CTL => crate::epoll::ctl(int(a[0]), a[1], int(a[2]), a[3] as usize),
        EPOLL_PWAIT => signal::wait_mask(a[4] as usize).and_then(|_| crate::epoll::pwait(int(a[0]), a[1] as usize, int(a[2]), crate::epoll::ms_to_ticks(int(a[3])))),
        EPOLL_PWAIT2 => signal::wait_mask(a[4] as usize).and_then(|_| crate::epoll::pwait2(int(a[0]), a[1] as usize, int(a[2]), a[3] as usize)),
        // 要求番号は unsigned int (musl は int を符号拡張して渡してくる)
        IOCTL => sysfile::ioctl(a[0], a[1] & 0xffff_ffff, a[2] as usize),
        FACCESSAT => sysfile::faccessat(int(a[0]), a[1] as usize, a[2], 0),
        FACCESSAT2 => sysfile::faccessat(int(a[0]), a[1] as usize, a[2], a[3]),
        CHDIR => sysfile::chdir(a[0] as usize),
        OPENAT => sysfile::openat(int(a[0]), a[1] as usize, a[2], a[3]),
        MKNODAT => sysfile::mknodat(int(a[0]), a[1] as usize, a[2], a[3]),
        SWAPON => sysfile::swapon(a[0] as usize),
        SWAPOFF => sysfile::swapoff(a[0] as usize),
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
        FCHMOD => sysfile::fchmod(a[0], a[1]),
        FCHMODAT => sysfile::fchmodat(int(a[0]), a[1] as usize, a[2]),
        FCHOWN => sysfile::fchown(a[0], a[1], a[2]),
        FCHOWNAT => sysfile::fchownat(int(a[0]), a[1] as usize, a[2], a[3], a[4]),
        UTIMENSAT => sysfile::utimensat(int(a[0]), a[1] as usize, a[2] as usize, a[3]),
        PREAD64 => sysfile::pread(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PWRITE64 => sysfile::pwrite(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PREADV | PREADV2 => sysfile::preadv(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PWRITEV | PWRITEV2 => sysfile::pwritev(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        STATX => sysfile::statx(int(a[0]), a[1] as usize, a[2], a[4] as usize),
        SYNC | FSYNC | FDATASYNC | SYNCFS => {
            crate::vfs::sync_all();
            Ok(0)
        }
        SENDFILE | COPY_FILE_RANGE => Err(-ENOSYS),
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
        SOCKET => socket::socket(a[0], a[1], a[2]),
        SOCKETPAIR => socket::socketpair(a[0], a[1], a[3] as usize),
        BIND => socket::bind(a[0], a[1] as usize, a[2] as usize),
        LISTEN => socket::listen(a[0]),
        ACCEPT => socket::accept(a[0], a[1] as usize, a[2] as usize, 0),
        ACCEPT4 => socket::accept(a[0], a[1] as usize, a[2] as usize, a[3]),
        CONNECT => socket::connect(a[0], a[1] as usize, a[2] as usize),
        GETSOCKNAME => socket::getsockname(a[0], a[1] as usize, a[2] as usize),
        GETPEERNAME => socket::getpeername(a[0], a[1] as usize, a[2] as usize),
        SENDTO => socket::sendto(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize, a[5] as usize),
        RECVFROM => socket::recvfrom(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize, a[5] as usize),
        SETSOCKOPT => socket::setsockopt(a[0], a[1], a[2], a[3] as usize, a[4] as usize),
        GETSOCKOPT => socket::getsockopt(a[0], a[1], a[2], a[3] as usize, a[4] as usize),
        SHUTDOWN => socket::shutdown(a[0], a[1]),
        SENDMSG => socket::sendmsg(a[0], a[1] as usize, a[2]),
        RECVMSG => socket::recvmsg(a[0], a[1] as usize, a[2]),
        RECVMMSG => socket::mmsg(a[0], a[1] as usize, a[2] as usize, a[3], false),
        SENDMMSG => socket::mmsg(a[0], a[1] as usize, a[2] as usize, a[3], true),
        // I/O の優先度はない (どれも同じ)。聞かれたら IOPRIO_CLASS_NONE
        IOPRIO_SET => Ok(0),
        IOPRIO_GET => Ok(0),
        // xattr は持っていない
        LISTXATTR..=FLISTXATTR => Ok(0),
        GETXATTR..=FGETXATTR => Err(-ENODATA),
        SETXATTR..=FSETXATTR => Err(-EOPNOTSUPP),
        SPLICE => sysfile::splice(a[0], a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5]),
        TEE => sysfile::tee(a[0], a[1], a[2] as usize, a[3]),
        // 先読みのお願いは聞くだけ (ext4 の読み込みが自分で先読みする)
        FADVISE64 | READAHEAD => Ok(0),
        // 優先度 (nice) はまだ持たない: いつも 0 (システムコールの返り値は Linux と同じく 20 - nice)、変えるのは受けつけるだけ
        GETPRIORITY => Ok(20),
        SETPRIORITY => Ok(0),

        EXIT => proc::exit(a[0] as i32 & 0xff),
        EXIT_GROUP => proc::exit_group(a[0] as i32 & 0xff),
        UNSHARE => proc::unshare(a[0]),
        CLONE => proc::clone(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize).map(|t| t as i64),
        EXECVE => sys_execve(a[0] as usize, a[1] as usize, a[2] as usize),
        WAIT4 => sys_wait4(int(a[0]), a[1] as usize, a[2]),
        GETRUSAGE => sys_getrusage(int(a[0]), a[1] as usize),
        TIMES => sys_times(a[0] as usize),
        WAITID => sys_waitid(a[0], int(a[1]), a[2] as usize, a[3], a[4] as usize),
        PIDFD_OPEN => sys_pidfd_open(int(a[0]), a[1]),
        LANDLOCK_CREATE_RULESET => crate::landlock::create_ruleset(a[0] as usize, a[1] as usize, a[2]),
        SECCOMP => crate::seccomp::sys_seccomp(a[0], a[1], a[2] as usize),
        LANDLOCK_ADD_RULE => crate::landlock::add_rule(a[0], a[1], a[2] as usize, a[3]),
        LANDLOCK_RESTRICT_SELF => crate::landlock::restrict_self(a[0], a[1]),
        PIDFD_SEND_SIGNAL => sys_pidfd_send_signal(int(a[0]), int(a[1]) as i32),
        KILL => signal::kill(int(a[0]), a[1] as i32),
        TKILL => signal::tgkill(0, a[0] as u32, a[1] as i32),
        TGKILL => signal::tgkill(a[0] as u32, a[1] as u32, a[2] as i32),
        RT_SIGSUSPEND => signal::rt_sigsuspend(a[0] as usize),
        RT_SIGPENDING => signal::rt_sigpending(a[0] as usize),
        RT_SIGTIMEDWAIT => signal::rt_sigtimedwait(a[0] as usize, a[1] as usize, a[2] as usize),
        RT_SIGRETURN => signal::rt_sigreturn(tf),
        SETITIMER => signal::setitimer(a[0], a[1] as usize, a[2] as usize),
        GETITIMER => signal::getitimer(a[0], a[1] as usize),
        TIMER_CREATE => signal::timer_create(a[0], a[1] as usize, a[2] as usize),
        TIMER_SETTIME => signal::timer_settime(a[0] as i32, a[1], a[2] as usize, a[3] as usize),
        TIMER_GETTIME => signal::timer_gettime(a[0] as i32, a[1] as usize),
        TIMER_GETOVERRUN => signal::timer_getoverrun(a[0] as i32),
        TIMER_DELETE => signal::timer_delete(a[0] as i32),
        SET_TID_ADDRESS => {
            let p = proc::current();
            p.clear_tid = a[0] as usize;
            Ok(p.pid as i64)
        }
        FUTEX => sys_futex(a[0] as usize, a[1], a[2] as u32, a[3] as usize),
        GETPID => Ok(proc::current().tgid as i64),
        GETPGID => signal::getpgid(a[0] as u32),
        GETSID => signal::getsid(a[0] as u32),
        GETTID => Ok(proc::current().pid as i64),
        GETPPID => Ok(proc::current().ppid as i64),
        GETUID => Ok(proc::current().cred.uid as i64),
        GETEUID => Ok(proc::current().cred.euid as i64),
        GETGID => Ok(proc::current().cred.gid as i64),
        GETEGID => Ok(proc::current().cred.egid as i64),
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
        SETPGID => signal::setpgid(a[0] as u32, a[1] as u32),
        SETSID => signal::setsid(),
        UMASK => {
            let f = crate::proc::current().files();
            let old = f.umask;
            f.umask = a[0] as u32 & 0o777;
            Ok(old as i64)
        }
        SET_ROBUST_LIST | MEMBARRIER => Ok(0),
        RSEQ => Err(-ENOSYS),
        SIGALTSTACK => signal::sigaltstack(a[0] as usize, a[1] as usize),
        RT_SIGPROCMASK => signal::rt_sigprocmask(a[0], a[1] as usize, a[2] as usize),
        RT_SIGACTION => signal::rt_sigaction(a[0] as usize, a[1] as usize, a[2] as usize),
        PRCTL => sys_prctl(a[0], a[1] as usize, a[2] as usize),
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
            _ => Err(-EINVAL),
        },
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => match a[0] {
            1 | 2 => Ok(if nr == SCHED_GET_PRIORITY_MAX { 99 } else { 1 }),
            0 | 3 | 5 => Ok(0),
            _ => Err(-EINVAL),
        },
        NANOSLEEP => sys_nanosleep(1, 0, a[0] as usize, a[1] as usize),
        CLOCK_NANOSLEEP => sys_nanosleep(a[0], a[1], a[2] as usize, a[3] as usize),
        CLOCK_GETTIME => sys_clock_gettime(a[0], a[1] as usize),
        CLOCK_GETRES => sys_clock_getres(a[1] as usize),
        GETTIMEOFDAY => sys_gettimeofday(a[0] as usize),
        UNAME => sys_uname(a[0] as usize),
        SETHOSTNAME => sys_sethostname(a[0] as usize, a[1] as usize),
        REBOOT => sys_reboot(a[0] as u32, a[1] as u32, a[2] as u32),
        INIT_MODULE => sys_init_module(a[0] as usize, a[1] as usize),
        FINIT_MODULE => sys_finit_module(a[0]),
        DELETE_MODULE => sys_delete_module(a[0] as usize),
        SYSINFO => sys_sysinfo(a[0] as usize),
        GETRLIMIT => sys_prlimit(a[0], 0, a[1] as usize),
        PRLIMIT64 => sys_prlimit(a[1], a[2] as usize, a[3] as usize),

        BRK => Ok(sys_brk(a[0] as usize)),
        MMAP => sys_mmap(a[0] as usize, a[1] as usize, a[2], a[3], int(a[4]), a[5] as usize),
        MUNMAP => sys_munmap(a[0] as usize, a[1] as usize),
        MPROTECT => sys_mprotect(a[0] as usize, a[1] as usize, a[2]),
        MADVISE => sys_madvise(a[0] as usize, a[1] as usize, a[2]),
        MINCORE => sys_mincore(a[0] as usize, a[1] as usize, a[2] as usize),
        MSYNC => sys_msync(a[0] as usize, a[1] as usize),
        MREMAP => sys_mremap(a[0] as usize, a[1] as usize, a[2] as usize, a[3], a[4] as usize),
        GETRANDOM => sys_getrandom(a[0] as usize, a[1] as usize),
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
        n => {
            println!("syscall: unknown {} (pid {})", n, proc::current().pid);
            Err(-ENOSYS)
        }
    };
    tf.x[0] = r.unwrap_or_else(|e| e) as u64;
    strace(nr, &a, r);
    // SA_RESTART でやり直してよいもの (Linux で ERESTARTSYS を返すもの)
    let restartable = matches!(nr, READ | WRITE | READV | WRITEV | OPENAT | WAIT4 | WAITID | FUTEX | ACCEPT | ACCEPT4 | RECVFROM | SENDTO | RECVMSG | SENDMSG | CONNECT);
    (r == Err(-EINTR_)).then_some(Restart { restartable })
}

const EINTR_: i64 = 4;

/// /proc/sysstat: スレッドごと (名前と番号) の、システムコールの回数 (空回りを探す調べもの用)。
/// 大きなロックの中で数える。fast の道 (getpid、clock_gettime など) は番号ごとに別に数える
static mut COUNTS: alloc::collections::BTreeMap<([u8; 16], u32, u64), u64> = alloc::collections::BTreeMap::new();
static FAST: [core::sync::atomic::AtomicU64; 512] = [const { core::sync::atomic::AtomicU64::new(0) }; 512];

fn count(nr: u64) {
    // スレッドの名前 (pthread_setname_np) ごと
    let p = proc::current();
    unsafe {
        *(*(&raw mut COUNTS)).entry((p.comm, p.pid, nr)).or_insert(0) += 1;
    }
}

/// 数えたものを多い順に (読むと 0 にもどる)
pub fn sysstat() -> alloc::string::String {
    use core::fmt::Write;
    use core::sync::atomic::Ordering;
    let m = unsafe { core::mem::take(&mut *(&raw mut COUNTS)) };
    let mut v: alloc::vec::Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    let mut s = alloc::string::String::new();
    for ((comm, tid, nr), n) in v.into_iter().take(40) {
        let len = comm.iter().position(|&c| c == 0).unwrap_or(16);
        let _ = writeln!(s, "{:>10} {:>4} {:>5} {}", n, nr, tid, core::str::from_utf8(&comm[..len]).unwrap_or("?"));
    }
    for (nr, c) in FAST.iter().enumerate() {
        let n = c.swap(0, Ordering::Relaxed);
        if n > 1000 {
            let _ = writeln!(s, "{:>10} {:>4} (fast)", n, nr);
        }
    }
    s
}

/// /proc/strace に書いた名前で始まるプロセスの、失敗したシステムコールを出す (調べもの用)。
/// 名前の頭に + をつけると、うまくいったものも出す。名前は , で区切っていくつも書ける
static mut STRACE: [u8; 16] = [0; 16];

pub fn strace_set(name: &[u8]) {
    let n = name.iter().position(|&c| c == b'\n' || c == 0).unwrap_or(name.len()).min(15);
    unsafe {
        let s = &mut *(&raw mut STRACE);
        s.fill(0);
        s[..n].copy_from_slice(&name[..n]);
    }
}

pub fn strace_get() -> alloc::string::String {
    let s = unsafe { &*(&raw const STRACE) };
    let n = s.iter().position(|&c| c == 0).unwrap_or(16);
    alloc::format!("{}\n", core::str::from_utf8(&s[..n]).unwrap_or(""))
}

/// いまのプロセスを strace で見ているか (見るなら、うまくいったものも出すか)
fn strace_on() -> Option<bool> {
    let s = unsafe { &*(&raw const STRACE) };
    let n = s.iter().position(|&c| c == 0).unwrap_or(16);
    if n == 0 {
        return None;
    }
    let all = s[0] == b'+';
    let name = if all { &s[1..n] } else { &s[..n] };
    let p = proc::current_leader();
    name.split(|&c| c == b',').any(|n| !n.is_empty() && p.comm.starts_with(n)).then_some(all)
}

fn strace(nr: u64, a: &[u64], r: R) {
    let Some(all) = strace_on() else { return };
    let e = r.unwrap_or_else(|e| e);
    // EAGAIN / EINTR / ETIMEDOUT はよくあるので出さない
    if !all && (r.is_ok() || matches!(-e, 11 | 4 | 110)) {
        return;
    }
    let p = proc::current_leader();
    let n = name(nr).map_or(alloc::format!("{}", nr), |n| n.to_ascii_lowercase());
    println!("strace [{} {}] t={} {}({:#x}, {:#x}, {:#x}) = {}", p.pid, proc::current().pid, crate::timer::ticks(), n, a[0], a[1], a[2], e);
}

type R = Result<i64, i64>;

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

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
/// シグナルや kill はほかの CPU から割り込み (IPI) で来るので、ここでは見なくてよい
pub fn fast(tf: &mut TrapFrame) -> bool {
    use nr::*;
    let a = tf.x;
    let p = proc::current();
    // seccomp がかかっていれば、フィルタを通すためにふつうの道へ
    if p.cred.seccomp.is_some() {
        return false;
    }
    if let Some(c) = FAST.get(a[8] as usize) {
        c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    let r = match a[8] {
        GETPID => p.tgid as i64,
        GETTID => p.pid as i64,
        GETPPID => p.ppid as i64,
        GETUID => p.cred.uid as i64,
        GETEUID => p.cred.euid as i64,
        GETGID => p.cred.gid as i64,
        GETEGID => p.cred.egid as i64,
        CLOCK_GETTIME => {
            let ns = match a[0] {
                CLOCK_REALTIME | CLOCK_REALTIME_COARSE => timer::epoch_ns(),
                _ => timer::uptime_ns(),
            };
            if !p.pt().copy_out_nofault(a[1] as usize, &timespec(ns)) {
                return false;
            }
            0
        }
        // 自分のアドレス空間だけの futex (PRIVATE) で、眠らない・起こさないもの (sys_futex も見よ)
        FUTEX if a[1] & FUTEX_PRIVATE_FLAG != 0 => match a[1] & 0x7f {
            FUTEX_WAIT | FUTEX_WAIT_BITSET => {
                // 値がもう val でなければ EAGAIN (Linux と同じ)。同じなら、ロックを取って眠る道へ
                let mut cur = [0u8; 4];
                if !p.pt().copy_in_nofault(a[0] as usize, &mut cur) || u32::from_le_bytes(cur) == a[2] as u32 {
                    return false;
                }
                -EAGAIN
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => {
                // 起こす側はユーザーが値を書いたあとに来る。眠る側は数を増やしてから値を読む (sys_futex) ので、
                // ここで 0 なら、眠っている (これから眠る) スレッドはいない: だれも起こさずに 0
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                if futex_sleepers(proc::futex_chan(p, a[0] as usize)).load(core::sync::atomic::Ordering::SeqCst) != 0 {
                    return false;
                }
                0
            }
            _ => return false,
        },
        _ => return false,
    };
    tf.x[0] = r as u64;
    true
}

fn sys_clock_gettime(clk: u64, ts: usize) -> R {
    let ns = match clk {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => timer::epoch_ns(),
        _ => timer::uptime_ns(),
    };
    out(ts, &timespec(ns))?;
    Ok(0)
}

fn sys_clock_getres(ts: usize) -> R {
    if ts != 0 {
        out(ts, &timespec(1))?;
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
        return Err(-EINVAL);
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
        return Err(-EINVAL);
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
    let (st, sf) = crate::swap::totals();
    b[56..64].copy_from_slice(&((st * PGSIZE) as u64).to_le_bytes());
    b[64..72].copy_from_slice(&((sf * PGSIZE) as u64).to_le_bytes());
    b[80..82].copy_from_slice(&(proc::nprocs() as u16).to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    out(buf, &b)?;
    Ok(0)
}

fn sys_sched_getaffinity(len: usize, mask: usize) -> R {
    if len < 8 {
        return Err(-EINVAL);
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
            return Err(-EINVAL);
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
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
const PROT_WRITE: u64 = 0x2;

/// mmap。無名のメモリとファイルの写し。ページは触れたときに作る (ファイルならそのときに読む)。
/// MAP_SHARED のファイルは写している人どうしでページを共有し、書いたものは msync などで書き戻す (vm.rs)
fn sys_mmap(addr: usize, len: usize, prot: u64, flags: u64, fd: i64, off: usize) -> R {
    const EACCES: i64 = 13;
    const EEXIST: i64 = 17;
    if len == 0 || off % 4096 != 0 {
        return Err(-EINVAL);
    }
    // ファイルを写すなら、その inode (と、/proc/PID/maps に出すパス)
    let mut path: Option<alloc::string::String> = None;
    let back = if flags & MAP_ANONYMOUS == 0 {
        let f = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
        let f = f.borrow();
        let mode = f.flags & crate::file::O_ACCMODE;
        // 読めること。共有で書くなら書けることも
        if mode == crate::file::O_WRONLY || (flags & MAP_SHARED != 0 && prot & PROT_WRITE != 0 && mode != crate::file::O_RDWR) {
            return Err(-EACCES);
        }
        match &f.kind {
            // fend はここではまだ「off から先のファイルの長さ」。場所が決まってから va にする
            crate::file::Kind::Inode(ino, p) if !ino.meta().is_dir() => {
                path = Some(p.clone());
                Backing::File { ino: ino.clone(), off, fend: (ino.meta().size as usize).saturating_sub(off) }
            }
            // 画面 (/dev/fb0): フレームバッファのページをそのまま
            crate::file::Kind::Fb => {
                let g = crate::gpu::get().ok_or(-ENODEV)?;
                if off + len > pg_up(g.size()) {
                    return Err(-EINVAL);
                }
                Backing::Pages { pages: g.pages.clone(), off }
            }
            // 音の再生の口: リングバッファのページ
            crate::file::Kind::SndPcm => Backing::Pages { pages: crate::sound::mmap_pages(off, len)?, off },
            _ => return Err(-ENODEV),
        }
    } else {
        Backing::Anon
    };
    let m = proc::current().mm();
    let len = pg_up(len);
    let va = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if addr & 0xfff != 0 {
            return Err(-EINVAL);
        }
        if flags & MAP_FIXED_NOREPLACE != 0 && (addr..addr + len).step_by(4096).any(|a| m.pt.find(a).is_some()) {
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
        Backing::File { ino, off, fend } => Backing::File { ino, off, fend: va.saturating_add(fend) },
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
    if addr & 0xfff != 0 {
        return Err(-EINVAL);
    }
    proc::current().pt().unmap(addr, addr + pg_up(len));
    Ok(0)
}

/// mremap: 領域の大きさを変える (縮める、その場で伸ばす、MREMAP_MAYMOVE なら別の場所へ写す)。
/// 古い場所に領域がなければ EFAULT (Firefox などは mremap でページがあるかを確かめる)
fn sys_mremap(old: usize, old_size: usize, new_size: usize, flags: u64, new_addr: usize) -> R {
    const EFAULT: i64 = 14;
    const MREMAP_MAYMOVE: u64 = 1;
    const MREMAP_FIXED: u64 = 2;
    const MREMAP_DONTUNMAP: u64 = 4;
    if old & 0xfff != 0 || new_size == 0 || flags & !7 != 0 || (flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0) {
        return Err(-EINVAL);
    }
    let (old_size, new_size) = (pg_up(old_size), pg_up(new_size));
    let m = proc::current().mm();
    // 古い場所は 1 つの領域の中でなければならない
    let Some((vstart, v)) = m.pt.find(old).map(|(s, v)| (s, v.clone())) else { return Err(-EFAULT) };
    let old_end = old.checked_add(old_size.max(PGSIZE)).ok_or(-EFAULT)?;
    if old_size == 0 || v.end < old_end {
        return Err(-EFAULT);
    }
    if flags & MREMAP_FIXED == 0 {
        if new_size == old_size {
            return Ok(old as i64);
        }
        if new_size < old_size {
            m.pt.unmap(old + new_size, old_end);
            return Ok(old as i64);
        }
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
            return Err(-EINVAL);
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

/// msync: MAP_SHARED のファイルで書いたページを書き戻す (MS_ASYNC でもすぐに)
fn sys_msync(addr: usize, len: usize) -> R {
    if addr & 0xfff != 0 {
        return Err(-EINVAL);
    }
    proc::current().pt().msync(addr, addr + len);
    Ok(0)
}

fn sys_mprotect(addr: usize, len: usize, prot: u64) -> R {
    if addr & 0xfff != 0 {
        return Err(-EINVAL);
    }
    proc::current().pt().protect(addr, addr + pg_up(len), prot_bits(prot)).map(|_| 0).map_err(|_| -ENOMEM)
}

/// MADV_DONTNEED は無名のページを捨てる (次に触れると 0)。ほかは何もしない
fn sys_madvise(addr: usize, len: usize, advice: u64) -> R {
    const MADV_DONTNEED: u64 = 4;
    if addr & 0xfff != 0 {
        return Err(-EINVAL);
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
        return Err(-EINVAL);
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
        return if data == 0 { Ok(0) } else { Err(-EINVAL) };
    }
    if data == 0 {
        return Ok(0);
    }
    // v1 は 1 組、v2 と v3 は 2 組の {effective, permitted, inheritable}
    let n = if ver == 0x1998_0330 { 1 } else { 2 };
    let all = if crate::cred::current().euid == 0 { u32::MAX } else { 0 };
    let mut out = alloc::vec::Vec::new();
    for _ in 0..n {
        out.extend_from_slice(&all.to_le_bytes());
        out.extend_from_slice(&all.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    pt.copy_out(data, &out).ok_or(-EFAULT)?;
    Ok(0)
}

fn sys_getrandom(buf: usize, len: usize) -> R {
    let mut done = 0;
    while done < len {
        let b = crate::rand::bytes16();
        let n = b.len().min(len - done);
        out(buf + done, &b[..n])?;
        done += n;
    }
    Ok(len as i64)
}

fn read_timespec(va: usize) -> Result<u64, i64> {
    let mut ts = [0u8; 16];
    proc::current().pt().copy_in(&mut ts, va).ok_or(-EFAULT)?;
    let sec = u64::from_le_bytes(ts[..8].try_into().unwrap());
    let nsec = u64::from_le_bytes(ts[8..].try_into().unwrap());
    Ok(sec.saturating_mul(1_000_000_000).saturating_add(nsec))
}

fn ns_to_ticks(ns: u64) -> u64 {
    (ns / 1000).saturating_mul(timer::HZ).div_ceil(1_000_000).max(1)
}

/// nanosleep / clock_nanosleep。シグナルで起こされたら残りを rem に書いて EINTR
fn sys_nanosleep(clock: u64, flags: u64, req: usize, rem: usize) -> R {
    const TIMER_ABSTIME: u64 = 1;
    let want = read_timespec(req)?;
    let now_ns = || if clock == CLOCK_REALTIME { timer::epoch_ns() } else { timer::uptime_ns() };
    let rel = if flags & TIMER_ABSTIME != 0 { want.saturating_sub(now_ns()) } else { want };
    let start = timer::ticks();
    let until = start + ns_to_ticks(rel);
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
static mut FUTEX_LOG: [(u32, u32, usize, u64, usize); 256] = [(0, 0, 0, 0, 0); 256];
static mut FUTEX_LOG_AT: usize = 0;

fn futex_log(tgid: u32, tid: u32, uaddr: usize, op: u64, n: usize) {
    unsafe {
        let i = FUTEX_LOG_AT % 256;
        (*(&raw mut FUTEX_LOG))[i] = (tgid, tid, uaddr, op, n);
        FUTEX_LOG_AT += 1;
    }
}

/// tgid の最近の futex の WAKE (古い順)
pub fn futex_wakes(tgid: u32) -> alloc::vec::Vec<(u32, usize, u64, usize)> {
    let mut v = alloc::vec::Vec::new();
    unsafe {
        let at = FUTEX_LOG_AT;
        for k in at.saturating_sub(256)..at {
            let (t, tid, a, op, n) = (*(&raw const FUTEX_LOG))[k % 256];
            if t == tgid {
                v.push((tid, a, op, n));
            }
        }
    }
    v
}

const FUTEX_PRIVATE_FLAG: u64 = 128;

/// futex で眠っている (眠ろうとしている) スレッドの数を、待ち合わせの場所 (chan) のハッシュごとに。
/// 起こす側が大きなロックなしに「ここにはだれもいない」と知るため (fast)。眠る側は増やしてから値を読む。
/// ハッシュがぶつかれば、起こす側がロックを取ってふつうに探すだけ
static FUTEX_SLEEPERS: [core::sync::atomic::AtomicUsize; 1024] = [const { core::sync::atomic::AtomicUsize::new(0) }; 1024];

fn futex_sleepers(chan: usize) -> &'static core::sync::atomic::AtomicUsize {
    &FUTEX_SLEEPERS[(chan.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54) as usize]
}

/// futex で眠ろうとしている数を 1 つ増やし、落とすと戻す (起こす側の速い道が見る)
struct FutexSleeper(&'static core::sync::atomic::AtomicUsize);

impl Drop for FutexSleeper {
    fn drop(&mut self) {
        self.0.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
    }
}

fn sys_futex(uaddr: usize, op: u64, val: u32, timeout: usize) -> R {
    let p = proc::current();
    // ふつうはアドレス空間とアドレスで待ち合わせる。PRIVATE でなく、共有の領域 (プロセスをまたぐ
    // pthread のミューテックスやセマフォ) なら、ページの物理アドレスで (どのプロセスからも同じ)
    let shared = if op & FUTEX_PRIVATE_FLAG == 0 { p.pt().shared_pa(uaddr) } else { None };
    let chan = match shared {
        Some(pa) => (pa << 1) ^ 1 ^ (1 << 63),
        None => proc::futex_chan(p, uaddr),
    };
    match op & 0x7f {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // 数を増やしてから値を読む: 起こす側 (fast) は値を書いてから数を読むので、
            // どちらかが必ず相手に気づく (値が変わったのを見て EAGAIN か、数を見てふつうに起こす)
            let sleepers = futex_sleepers(chan);
            sleepers.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let _sleeper = FutexSleeper(sleepers);
            let mut cur = [0u8; 4];
            p.pt().copy_in(&mut cur, uaddr).ok_or(-EFAULT)?;
            if u32::from_le_bytes(cur) != val {
                return Err(-EAGAIN);
            }
            let deadline = if timeout == 0 {
                0
            } else {
                let ns = read_timespec(timeout)?;
                // WAIT は相対時間、WAIT_BITSET は絶対時間
                let rel = if op & 0x7f == FUTEX_WAIT_BITSET {
                    let now = if op & 256 != 0 { timer::epoch_ns() } else { timer::uptime_ns() };
                    ns.saturating_sub(now)
                } else {
                    ns
                };
                timer::ticks() + ns_to_ticks(rel)
            };
            if proc::sleep_until(chan, deadline)? {
                Ok(0)
            } else {
                Err(-ETIMEDOUT)
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET | FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            // 数は数えずに全員起こす (起きすぎても待つ側が確かめなおす)
            let n = proc::wakeup(chan);
            futex_log(p.tgid, p.pid, uaddr, op, n);
            Ok(n.min(val as usize) as i64)
        }
        _ => Err(-ENOSYS),
    }
}


/// 引数 (と環境) の合計と 1 つの長さの上限 (Linux の ARG_MAX の既定と MAX_ARG_STRLEN)
const ARGS_TOTAL: usize = 2 * 1024 * 1024;
const MAXSTR: usize = 128 * 1024;

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

fn sys_wait4(pid: i64, status: usize, options: u64) -> R {
    let (pid, xstatus) = proc::wait(pid, options | proc::WEXITED)?;
    if status != 0 && pid != 0 {
        out(status, &xstatus.to_le_bytes())?;
    }
    Ok(pid as i64)
}

/// tick を timeval (秒, マイクロ秒) に
fn tick_timeval(t: u64) -> [u8; 16] {
    let us = t * (1_000_000 / timer::HZ);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(us / 1_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(us % 1_000_000).to_le_bytes());
    b
}

/// getrusage: 使った CPU 時間。カーネルの中の時間 (stime) は数えていないので 0
fn sys_getrusage(who: i64, buf: usize) -> R {
    const RUSAGE_SELF: i64 = 0;
    const RUSAGE_CHILDREN: i64 = -1;
    const RUSAGE_THREAD: i64 = 1;
    let me = proc::current();
    let t = match who {
        RUSAGE_SELF => proc::group_utime(me.tgid),
        RUSAGE_CHILDREN => proc::find_leader(me.tgid).map_or(0, |l| l.cutime),
        RUSAGE_THREAD => me.utime,
        _ => return Err(-EINVAL),
    };
    let mut b = [0u8; 144];
    b[..16].copy_from_slice(&tick_timeval(t));
    out(buf, &b)?;
    Ok(0)
}

/// times: struct tms (utime, stime, cutime, cstime。単位は tick = 1/100 秒)。起動からの tick を返す
fn sys_times(buf: usize) -> R {
    if buf != 0 {
        let me = proc::current();
        let c = proc::find_leader(me.tgid).map_or(0, |l| l.cutime);
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&proc::group_utime(me.tgid).to_le_bytes());
        b[16..24].copy_from_slice(&c.to_le_bytes());
        out(buf, &b)?;
    }
    Ok(timer::ticks() as i64)
}

/// pidfd の番号から pid
fn pidfd_pid(fd: i64) -> Result<u32, i64> {
    let f = proc::current().files().get(fd as u64).cloned().ok_or(-9)?;
    let b = f.borrow();
    match b.kind {
        crate::file::Kind::PidFd(pid) => Ok(pid),
        _ => Err(-EINVAL),
    }
}

/// pidfd_open(pid, flags): プロセスを指す fd (終わると読めるようになる)
fn sys_pidfd_open(pid: i64, flags: u64) -> R {
    const PIDFD_NONBLOCK: u64 = 0o4000;
    const ESRCH: i64 = 3;
    if pid <= 0 || flags & !PIDFD_NONBLOCK != 0 {
        return Err(-EINVAL);
    }
    if proc::find_leader(pid as u32).is_none() {
        return Err(-ESRCH);
    }
    let fl = crate::file::O_RDWR | if flags & PIDFD_NONBLOCK != 0 { crate::file::O_NONBLOCK } else { 0 };
    let f = crate::file::new(crate::file::Kind::PidFd(pid as u32), fl);
    // pidfd はいつも close-on-exec
    let fd = proc::current().files().add(f, true, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn sys_pidfd_send_signal(fd: i64, sig: i32) -> R {
    let pid = pidfd_pid(fd)?;
    if sig == 0 {
        return if proc::has_exited(pid) { Err(-3) } else { Ok(0) };
    }
    signal::send_group(pid, sig, signal::SigInfo::from(0)).map(|_| 0)
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
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        return Err(-EINVAL);
    }
    let pid = match idtype {
        P_ALL => -1,
        P_PID if id > 0 => id,
        P_PGID => if id == 0 { 0 } else { -id },
        P_PIDFD => pidfd_pid(id)? as i64,
        _ => return Err(-EINVAL),
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
/// init_module(image, len, params): 札 (module.rs) をメモリで
fn sys_init_module(image: usize, len: usize) -> R {
    let mut img = alloc::vec![0u8; len.min(4096)];
    proc::current().pt().copy_in(&mut img, image).ok_or(-EFAULT)?;
    crate::module::load(&img)
}

/// finit_module(fd, params, flags): 札のファイル (/usr/lib/modules/NAME.ko) を読む
fn sys_finit_module(fd: u64) -> R {
    let f = proc::current().files().get(fd).cloned().ok_or(-9)?;
    let mut img = alloc::vec![0u8; 4096];
    let n = f.borrow_mut().read(&mut img)?;
    img.truncate(n);
    crate::module::load(&img)
}

fn sys_delete_module(name: usize) -> R {
    let name = proc::current().pt().copy_in_str(name, 64).ok_or(-EFAULT)?;
    crate::module::unload(core::str::from_utf8(&name).map_err(|_| -EINVAL)?)
}

fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> R {
    if proc::current().cred.euid != 0 {
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
        return Err(-EINVAL);
    }
    let fid = match cmd {
        CMD_POWER_OFF | CMD_HALT => PSCI_SYSTEM_OFF,
        CMD_RESTART => PSCI_SYSTEM_RESET,
        _ => return Err(-EINVAL),
    };
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

/// prctl: 名前 (PR_SET_NAME / PR_GET_NAME)、no_new_privs、seccomp。ほかは何もしない
fn sys_prctl(op: u64, arg: usize, arg3: usize) -> R {
    const PR_GET_SECCOMP: u64 = 21;
    const PR_SET_SECCOMP: u64 = 22;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    let p = proc::current();
    match op {
        // 一度つけたら外せない (arg は 1 だけ)
        PR_SET_NO_NEW_PRIVS => {
            if arg != 1 {
                return Err(-22);
            }
            p.cred.no_new_privs = true;
        }
        PR_GET_NO_NEW_PRIVS => return Ok(p.cred.no_new_privs as i64),
        PR_GET_SECCOMP => return Ok(crate::seccomp::mode(&p.cred).0 as i64),
        PR_SET_SECCOMP => return crate::seccomp::prctl_set(arg as u64, arg3),
        PR_SET_NAME => {
            let mut b = [0u8; 16];
            p.pt().copy_in(&mut b[..15], arg).ok_or(-14)?;
            p.set_comm(&b);
        }
        PR_GET_NAME => {
            let c = p.comm;
            p.pt().copy_out(arg, &c).ok_or(-14)?;
        }
        _ => {}
    }
    Ok(0)
}
