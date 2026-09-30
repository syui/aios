// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::cred;
use crate::proc;
use crate::signal::{self, Restart};
use crate::socket;
use crate::sysfile;
use crate::timer;
use crate::trap::TrapFrame;
use crate::vm::{pg_up, Perm};
use alloc::string::String;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const E2BIG: i64 = 7;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const ENODATA: i64 = 61;
const EOPNOTSUPP: i64 = 95;
const ETIMEDOUT: i64 = 110;

mod nr {
    pub const SETXATTR: u64 = 5;
    pub const FSETXATTR: u64 = 7;
    pub const GETXATTR: u64 = 8;
    pub const FGETXATTR: u64 = 10;
    pub const LISTXATTR: u64 = 11;
    pub const FLISTXATTR: u64 = 13;
    pub const GETCWD: u64 = 17;
    pub const FLOCK: u64 = 32;
    pub const MKNODAT: u64 = 33;
    pub const MKDIRAT: u64 = 34;
    pub const UNLINKAT: u64 = 35;
    pub const SYMLINKAT: u64 = 36;
    pub const LINKAT: u64 = 37;
    pub const RENAMEAT: u64 = 38;
    pub const STATFS: u64 = 43;
    pub const FSTATFS: u64 = 44;
    pub const TRUNCATE: u64 = 45;
    pub const FTRUNCATE: u64 = 46;
    pub const DUP: u64 = 23;
    pub const DUP3: u64 = 24;
    pub const FCNTL: u64 = 25;
    pub const IOCTL: u64 = 29;
    pub const FACCESSAT: u64 = 48;
    pub const CHDIR: u64 = 49;
    pub const FCHDIR: u64 = 50;
    pub const FCHMOD: u64 = 52;
    pub const FCHMODAT: u64 = 53;
    pub const FCHOWNAT: u64 = 54;
    pub const FCHOWN: u64 = 55;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const PIPE2: u64 = 59;
    pub const GETDENTS64: u64 = 61;
    pub const LSEEK: u64 = 62;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const PREAD64: u64 = 67;
    pub const PWRITE64: u64 = 68;
    pub const SENDFILE: u64 = 71;
    pub const PPOLL: u64 = 73;
    pub const SPLICE: u64 = 76;
    pub const TEE: u64 = 77;
    pub const READLINKAT: u64 = 78;
    pub const NEWFSTATAT: u64 = 79;
    pub const FSTAT: u64 = 80;
    pub const SYNC: u64 = 81;
    pub const FSYNC: u64 = 82;
    pub const FDATASYNC: u64 = 83;
    pub const UTIMENSAT: u64 = 88;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const NANOSLEEP: u64 = 101;
    pub const GETITIMER: u64 = 102;
    pub const SETITIMER: u64 = 103;
    pub const TIMER_CREATE: u64 = 107;
    pub const TIMER_GETTIME: u64 = 108;
    pub const TIMER_GETOVERRUN: u64 = 109;
    pub const TIMER_SETTIME: u64 = 110;
    pub const TIMER_DELETE: u64 = 111;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const CLOCK_GETRES: u64 = 114;
    pub const CLOCK_NANOSLEEP: u64 = 115;
    pub const SCHED_GETAFFINITY: u64 = 123;
    pub const SCHED_YIELD: u64 = 124;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const RT_SIGSUSPEND: u64 = 133;
    pub const SIGALTSTACK: u64 = 132;
    pub const RT_SIGACTION: u64 = 134;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGPENDING: u64 = 136;
    pub const RT_SIGTIMEDWAIT: u64 = 137;
    pub const RT_SIGRETURN: u64 = 139;
    pub const REBOOT: u64 = 142;
    pub const SETREGID: u64 = 143;
    pub const SETGID: u64 = 144;
    pub const SETREUID: u64 = 145;
    pub const SETUID: u64 = 146;
    pub const SETRESUID: u64 = 147;
    pub const GETRESUID: u64 = 148;
    pub const SETRESGID: u64 = 149;
    pub const GETRESGID: u64 = 150;
    pub const SETFSUID: u64 = 151;
    pub const SETFSGID: u64 = 152;
    pub const GETGROUPS: u64 = 158;
    pub const SETGROUPS: u64 = 159;
    pub const SETPGID: u64 = 154;
    pub const PRCTL: u64 = 167;
    pub const GETPGID: u64 = 155;
    pub const GETSID: u64 = 156;
    pub const SETSID: u64 = 157;
    pub const UNAME: u64 = 160;
    pub const GETRLIMIT: u64 = 163;
    pub const UMASK: u64 = 166;
    pub const GETTIMEOFDAY: u64 = 169;
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETUID: u64 = 174;
    pub const GETEUID: u64 = 175;
    pub const GETGID: u64 = 176;
    pub const GETEGID: u64 = 177;
    pub const GETTID: u64 = 178;
    pub const SYSINFO: u64 = 179;
    pub const SOCKET: u64 = 198;
    pub const SOCKETPAIR: u64 = 199;
    pub const BIND: u64 = 200;
    pub const LISTEN: u64 = 201;
    pub const ACCEPT: u64 = 202;
    pub const CONNECT: u64 = 203;
    pub const GETSOCKNAME: u64 = 204;
    pub const GETPEERNAME: u64 = 205;
    pub const SENDTO: u64 = 206;
    pub const RECVFROM: u64 = 207;
    pub const SETSOCKOPT: u64 = 208;
    pub const GETSOCKOPT: u64 = 209;
    pub const SHUTDOWN: u64 = 210;
    pub const SENDMSG: u64 = 211;
    pub const RECVMSG: u64 = 212;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const MREMAP: u64 = 216;
    pub const CLONE: u64 = 220;
    pub const EXECVE: u64 = 221;
    pub const MMAP: u64 = 222;
    pub const FADVISE64: u64 = 223;
    pub const MPROTECT: u64 = 226;
    pub const MADVISE: u64 = 233;
    pub const ACCEPT4: u64 = 242;
    pub const WAIT4: u64 = 260;
    pub const PRLIMIT64: u64 = 261;
    pub const RENAMEAT2: u64 = 276;
    pub const COPY_FILE_RANGE: u64 = 285;
    pub const GETRANDOM: u64 = 278;
    pub const MEMBARRIER: u64 = 283;
    pub const RSEQ: u64 = 293;
    pub const CLOSE_RANGE: u64 = 436;
    pub const FACCESSAT2: u64 = 439;
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
    let r = match tf.x[8] {
        GETCWD => sysfile::getcwd(a[0] as usize, a[1] as usize),
        FLOCK => sysfile::flock(a[0], a[1]),
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
        FCHDIR => sysfile::fchdir(a[0]),
        FCHMOD => sysfile::fchmod(a[0], a[1]),
        FCHMODAT => sysfile::fchmodat(int(a[0]), a[1] as usize, a[2]),
        FCHOWN => sysfile::fchown(a[0], a[1], a[2]),
        FCHOWNAT => sysfile::fchownat(int(a[0]), a[1] as usize, a[2], a[3], a[4]),
        UTIMENSAT => sysfile::utimensat(int(a[0]), a[1] as usize, a[2] as usize, a[3]),
        PREAD64 => sysfile::pread(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        PWRITE64 => sysfile::pwrite(a[0], a[1] as usize, a[2] as usize, a[3] as i64),
        SYNC | FSYNC | FDATASYNC => Ok(0),
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
        PPOLL => sysfile::ppoll(a[0] as usize, a[1] as usize, a[2] as usize),
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
        SETSOCKOPT => Ok(0),
        GETSOCKOPT => socket::getsockopt(a[0], a[1], a[2], a[3] as usize, a[4] as usize),
        SHUTDOWN => socket::shutdown(a[0], a[1]),
        SENDMSG => socket::sendmsg(a[0], a[1] as usize, a[2]),
        RECVMSG => socket::recvmsg(a[0], a[1] as usize, a[2]),
        // xattr は持っていない
        LISTXATTR..=FLISTXATTR => Ok(0),
        GETXATTR..=FGETXATTR => Err(-ENODATA),
        SETXATTR..=FSETXATTR => Err(-EOPNOTSUPP),
        SPLICE => sysfile::splice(a[0], a[1] as usize, a[2], a[3] as usize, a[4] as usize, a[5]),
        TEE => sysfile::tee(a[0], a[1], a[2] as usize, a[3]),
        FADVISE64 => Ok(0),

        EXIT => proc::exit(a[0] as i32 & 0xff),
        EXIT_GROUP => proc::exit_group(a[0] as i32 & 0xff),
        CLONE => proc::clone(a[0], a[1] as usize, a[2] as usize, a[3], a[4] as usize).map(|t| t as i64),
        EXECVE => sys_execve(a[0] as usize, a[1] as usize, a[2] as usize),
        WAIT4 => sys_wait4(int(a[0]), a[1] as usize, a[2]),
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
        UMASK => Ok(0o022),
        SET_ROBUST_LIST | MEMBARRIER => Ok(0),
        RSEQ => Err(-ENOSYS),
        SIGALTSTACK => signal::sigaltstack(a[0] as usize, a[1] as usize),
        RT_SIGPROCMASK => signal::rt_sigprocmask(a[0], a[1] as usize, a[2] as usize),
        RT_SIGACTION => signal::rt_sigaction(a[0] as usize, a[1] as usize, a[2] as usize),
        PRCTL => sys_prctl(a[0], a[1] as usize),
        SCHED_YIELD => {
            proc::yield_now();
            Ok(0)
        }
        SCHED_GETAFFINITY => sys_sched_getaffinity(a[1] as usize, a[2] as usize),
        NANOSLEEP => sys_nanosleep(1, 0, a[0] as usize, a[1] as usize),
        CLOCK_NANOSLEEP => sys_nanosleep(a[0], a[1], a[2] as usize, a[3] as usize),
        CLOCK_GETTIME => sys_clock_gettime(a[0], a[1] as usize),
        CLOCK_GETRES => sys_clock_getres(a[1] as usize),
        GETTIMEOFDAY => sys_gettimeofday(a[0] as usize),
        UNAME => sys_uname(a[0] as usize),
        REBOOT => sys_reboot(a[0] as u32, a[1] as u32, a[2] as u32),
        SYSINFO => sys_sysinfo(a[0] as usize),
        GETRLIMIT => sys_prlimit(a[0], 0, a[1] as usize),
        PRLIMIT64 => sys_prlimit(a[1], a[2] as usize, a[3] as usize),

        BRK => Ok(sys_brk(a[0] as usize)),
        MMAP => sys_mmap(a[0] as usize, a[1] as usize, a[3], int(a[4])),
        MUNMAP => sys_munmap(a[0] as usize, a[1] as usize),
        MPROTECT | MADVISE => Ok(0),
        MREMAP => Err(-ENOMEM), // musl は自分で確保しなおす
        GETRANDOM => sys_getrandom(a[0] as usize, a[1] as usize),
        n => {
            println!("syscall: unknown {} (pid {})", n, proc::current().pid);
            Err(-ENOSYS)
        }
    };
    tf.x[0] = r.unwrap_or_else(|e| e) as u64;
    // SA_RESTART でやり直してよいもの (Linux で ERESTARTSYS を返すもの)
    let restartable = matches!(nr, READ | WRITE | READV | WRITEV | OPENAT | WAIT4 | FUTEX | ACCEPT | ACCEPT4 | RECVFROM | SENDTO | RECVMSG | SENDMSG | CONNECT);
    (r == Err(-EINTR_)).then_some(Restart { restartable })
}

const EINTR_: i64 = 4;

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

fn sys_uname(buf: usize) -> R {
    const FIELD: usize = 65;
    let fields: [&[u8]; 6] = [b"aios", b"aios", env!("CARGO_PKG_VERSION").as_bytes(), b"aios", b"aarch64", b""];
    let mut u = [0u8; FIELD * 6];
    for (i, f) in fields.iter().enumerate() {
        u[i * FIELD..i * FIELD + f.len()].copy_from_slice(f);
    }
    out(buf, &u)?;
    Ok(0)
}

fn sys_sysinfo(buf: usize) -> R {
    use crate::memlayout::{PGSIZE, PHYSBASE, PHYSTOP};
    let mut b = [0u8; 112];
    b[0..8].copy_from_slice(&(timer::uptime_ns() / 1_000_000_000).to_le_bytes());
    b[32..40].copy_from_slice(&((PHYSTOP - PHYSBASE) as u64).to_le_bytes());
    b[40..48].copy_from_slice(&((crate::kalloc::nfree() * PGSIZE) as u64).to_le_bytes());
    b[80..82].copy_from_slice(&(proc::nprocs() as u16).to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
    out(buf, &b)?;
    Ok(0)
}

fn sys_sched_getaffinity(len: usize, mask: usize) -> R {
    if len < 8 {
        return Err(-EINVAL);
    }
    out(mask, &1u64.to_le_bytes())?;
    Ok(8)
}

fn sys_prlimit(resource: u64, new: usize, old: usize) -> R {
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    const INF: u64 = u64::MAX;
    let _ = new;
    if old != 0 {
        let (cur, max) = match resource {
            RLIMIT_STACK => (8 * 1024 * 1024, INF),
            RLIMIT_NOFILE => (proc::NOFILE as u64, proc::NOFILE as u64),
            _ => (INF, INF),
        };
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
    if new > old {
        if m.pt.alloc_range(old, new, Perm::RW).is_none() {
            m.pt.unmap_range(old, new);
            return m.brk as i64;
        }
    } else if new < old {
        m.pt.unmap_range(new, old);
    }
    m.brk = addr;
    addr as i64
}

const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

fn sys_mmap(addr: usize, len: usize, flags: u64, fd: i64) -> R {
    if len == 0 {
        return Err(-EINVAL);
    }
    if flags & MAP_ANONYMOUS == 0 || fd != -1 {
        return Err(-ENODEV);
    }
    let m = proc::current().mm();
    let len = pg_up(len);
    let va = if flags & MAP_FIXED != 0 {
        if addr & 0xfff != 0 {
            return Err(-EINVAL);
        }
        m.pt.unmap_range(addr, addr + len);
        addr
    } else {
        let va = m.mmap_next;
        m.mmap_next += len;
        va
    };
    // TODO: prot を反映する。いまは常に RW
    if m.pt.alloc_range(va, va + len, Perm::RW).is_none() {
        m.pt.unmap_range(va, va + len);
        return Err(-ENOMEM);
    }
    Ok(va as i64)
}

fn sys_munmap(addr: usize, len: usize) -> R {
    if addr & 0xfff != 0 {
        return Err(-EINVAL);
    }
    proc::current().pt().unmap_range(addr, addr + pg_up(len));
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

fn sys_futex(uaddr: usize, op: u64, val: u32, timeout: usize) -> R {
    let p = proc::current();
    let chan = proc::futex_chan(p, uaddr);
    match op & 0x7f {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
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
            Ok(proc::wakeup(chan).min(val as usize) as i64)
        }
        _ => Err(-ENOSYS),
    }
}


const MAXARG: usize = 256;
const MAXSTR: usize = 32 * 1024;

/// NULL 終端のポインタ配列が指す文字列たち
fn copy_in_strv(mut va: usize) -> Result<Vec<Vec<u8>>, i64> {
    let mut v = Vec::new();
    if va == 0 {
        return Ok(v);
    }
    let pt = proc::current().pt();
    loop {
        let mut w = [0u8; 8];
        pt.copy_in(&mut w, va).ok_or(-EFAULT)?;
        let p = u64::from_le_bytes(w) as usize;
        if p == 0 {
            return Ok(v);
        }
        if v.len() >= MAXARG {
            return Err(-E2BIG);
        }
        v.push(pt.copy_in_str(p, MAXSTR).ok_or(-EFAULT)?);
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
    let (pid, xstatus) = proc::wait(pid, options)?;
    if status != 0 && pid != 0 {
        out(status, &xstatus.to_le_bytes())?;
    }
    Ok(pid as i64)
}

/// 電源を切る / 再起動する (PSCI を hvc で呼ぶ)
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
    println!("aios: {}", if fid == PSCI_SYSTEM_OFF { "power off" } else { "restart" });
    unsafe { core::arch::asm!("hvc #0", in("x0") fid, options(nostack)) };
    // PSCI が無ければ止まる
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// prctl: 名前の設定と取得 (PR_SET_NAME / PR_GET_NAME) のほかは何もしない
fn sys_prctl(op: u64, arg: usize) -> R {
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    let p = proc::current();
    match op {
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
