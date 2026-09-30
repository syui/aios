// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::proc;
use crate::sysfile;
use crate::trap::TrapFrame;
use crate::vm::{pg_up, Perm};
use alloc::string::String;
use alloc::vec::Vec;

const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;

const SYS_GETCWD: u64 = 17;
const SYS_DUP: u64 = 23;
const SYS_DUP3: u64 = 24;
const SYS_FCNTL: u64 = 25;
const SYS_IOCTL: u64 = 29;
const SYS_FACCESSAT: u64 = 48;
const SYS_CHDIR: u64 = 49;
const SYS_OPENAT: u64 = 56;
const SYS_CLOSE: u64 = 57;
const SYS_PIPE2: u64 = 59;
const SYS_GETDENTS64: u64 = 61;
const SYS_LSEEK: u64 = 62;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_READV: u64 = 65;
const SYS_WRITEV: u64 = 66;
const SYS_READLINKAT: u64 = 78;
const SYS_NEWFSTATAT: u64 = 79;
const SYS_FSTAT: u64 = 80;
const SYS_PPOLL: u64 = 73;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SET_TID_ADDRESS: u64 = 96;
const SYS_FUTEX: u64 = 98;
const SYS_SET_ROBUST_LIST: u64 = 99;
const SYS_NANOSLEEP: u64 = 101;
const SYS_CLOCK_NANOSLEEP: u64 = 115;
const SYS_CLOCK_GETTIME: u64 = 113;
const SYS_SCHED_YIELD: u64 = 124;
const SYS_SIGALTSTACK: u64 = 132;
const SYS_RT_SIGACTION: u64 = 134;
const SYS_RT_SIGPROCMASK: u64 = 135;
const SYS_KILL: u64 = 129;
const SYS_TGKILL: u64 = 131;
const SYS_SETPGID: u64 = 154;
const SYS_GETPGID: u64 = 155;
const SYS_SETSID: u64 = 157;
const SYS_UNAME: u64 = 160;
const SYS_UMASK: u64 = 166;
const SYS_GETPID: u64 = 172;
const SYS_GETPPID: u64 = 173;
const SYS_GETUID: u64 = 174;
const SYS_GETEUID: u64 = 175;
const SYS_GETGID: u64 = 176;
const SYS_GETEGID: u64 = 177;
const SYS_GETTID: u64 = 178;
const SYS_BRK: u64 = 214;
const SYS_CLONE: u64 = 220;
const SYS_EXECVE: u64 = 221;
const SYS_MUNMAP: u64 = 215;
const SYS_MMAP: u64 = 222;
const SYS_MPROTECT: u64 = 226;
const SYS_MADVISE: u64 = 233;
const SYS_WAIT4: u64 = 260;
const SYS_GETRANDOM: u64 = 278;
const SYS_FACCESSAT2: u64 = 439;

pub fn dispatch(tf: &mut TrapFrame) {
    let a = tf.x;
    let file = |r: Result<i64, i64>| r.unwrap_or_else(|e| e);
    let ret = match tf.x[8] {
        SYS_GETCWD => file(sysfile::getcwd(a[0] as usize, a[1] as usize)),
        SYS_DUP => file(sysfile::dup(a[0])),
        SYS_DUP3 => file(sysfile::dup3(a[0], a[1], a[2])),
        SYS_FCNTL => file(sysfile::fcntl(a[0], a[1], a[2])),
        SYS_IOCTL => file(sysfile::ioctl(a[0], a[1], a[2] as usize)),
        SYS_FACCESSAT | SYS_FACCESSAT2 => file(sysfile::faccessat(a[0] as i64, a[1] as usize)),
        SYS_CHDIR => file(sysfile::chdir(a[0] as usize)),
        SYS_OPENAT => file(sysfile::openat(a[0] as i64, a[1] as usize, a[2])),
        SYS_CLOSE => file(sysfile::close(a[0])),
        SYS_PIPE2 => file(sysfile::pipe2(a[0] as usize, a[1])),
        SYS_GETDENTS64 => file(sysfile::getdents64(a[0], a[1] as usize, a[2] as usize)),
        SYS_LSEEK => file(sysfile::lseek(a[0], a[1] as i64, a[2])),
        SYS_READ => file(sysfile::read(a[0], a[1] as usize, a[2] as usize)),
        SYS_WRITE => file(sysfile::write(a[0], a[1] as usize, a[2] as usize)),
        SYS_READV => file(sysfile::readv(a[0], a[1] as usize, a[2] as usize)),
        SYS_WRITEV => file(sysfile::writev(a[0], a[1] as usize, a[2] as usize)),
        SYS_READLINKAT => file(sysfile::readlinkat(a[0] as i64, a[1] as usize, a[2] as usize, a[3] as usize)),
        SYS_NEWFSTATAT => file(sysfile::newfstatat(a[0] as i64, a[1] as usize, a[2] as usize, a[3])),
        SYS_FSTAT => file(sysfile::fstat(a[0], a[1] as usize)),
        SYS_UMASK => 0o022,
        SYS_SETPGID | SYS_SETSID => 0,
        SYS_GETPGID => proc::current().pid as i64,
        SYS_KILL | SYS_TGKILL => sys_kill(a[0] as i64, a[1] as i64, a[2] as i64, tf.x[8] == SYS_TGKILL),
        SYS_PPOLL => 0,
        SYS_EXIT | SYS_EXIT_GROUP => proc::exit(a[0] as i32 & 0xff),
        SYS_SET_TID_ADDRESS | SYS_GETPID | SYS_GETTID => proc::current().pid as i64,
        SYS_GETPPID => proc::current().ppid as i64,
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_FUTEX | SYS_SET_ROBUST_LIST => 0,
        SYS_SCHED_YIELD => {
            proc::yield_now();
            0
        }
        SYS_NANOSLEEP => sys_nanosleep(a[0] as usize),
        SYS_CLOCK_NANOSLEEP => sys_nanosleep(a[2] as usize),
        SYS_CLONE => sys_clone(a[0], a[1] as usize, a[3]),
        SYS_EXECVE => sys_execve(a[0] as usize, a[1] as usize, a[2] as usize),
        SYS_WAIT4 => sys_wait4(a[0] as i64, a[1] as usize, a[2]),
        SYS_SIGALTSTACK | SYS_RT_SIGACTION | SYS_RT_SIGPROCMASK => 0,
        SYS_MPROTECT | SYS_MADVISE => 0,
        SYS_CLOCK_GETTIME => sys_clock_gettime(a[1] as usize),
        SYS_UNAME => sys_uname(a[0] as usize),
        SYS_BRK => sys_brk(a[0] as usize),
        SYS_MMAP => sys_mmap(a[0] as usize, a[1] as usize, a[3], a[4] as i64),
        SYS_MUNMAP => sys_munmap(a[0] as usize, a[1] as usize),
        SYS_GETRANDOM => sys_getrandom(a[0] as usize, a[1] as usize),
        nr => {
            println!("syscall: unknown {} (pid {})", nr, proc::current().pid);
            -ENOSYS
        }
    };
    tf.x[0] = ret as u64;
}

fn sys_clock_gettime(ts: usize) -> i64 {
    let (cnt, frq): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt);
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frq);
    }
    let sec = cnt / frq;
    let nsec = (cnt % frq) * 1_000_000_000 / frq;
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&sec.to_le_bytes());
    b[8..].copy_from_slice(&nsec.to_le_bytes());
    match proc::current().pt().copy_out(ts, &b) {
        Some(()) => 0,
        None => -EFAULT,
    }
}

fn sys_uname(buf: usize) -> i64 {
    const FIELD: usize = 65;
    let fields: [&[u8]; 6] = [b"aios", b"aios", env!("CARGO_PKG_VERSION").as_bytes(), b"aios", b"aarch64", b""];
    let mut u = [0u8; FIELD * 6];
    for (i, f) in fields.iter().enumerate() {
        u[i * FIELD..i * FIELD + f.len()].copy_from_slice(f);
    }
    match proc::current().pt().copy_out(buf, &u) {
        Some(()) => 0,
        None => -EFAULT,
    }
}

fn sys_brk(addr: usize) -> i64 {
    let p = proc::current();
    if addr < p.heap_start {
        return p.brk as i64;
    }
    let (old, new) = (pg_up(p.brk), pg_up(addr));
    if new > old {
        if p.pt().alloc_range(old, new, Perm::RW).is_none() {
            p.pt().unmap_range(old, new);
            return p.brk as i64;
        }
    } else if new < old {
        p.pt().unmap_range(new, old);
    }
    p.brk = addr;
    addr as i64
}

const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

fn sys_mmap(addr: usize, len: usize, flags: u64, fd: i64) -> i64 {
    if len == 0 {
        return -EINVAL;
    }
    if flags & MAP_ANONYMOUS == 0 || fd != -1 {
        return -ENODEV;
    }
    let p = proc::current();
    let len = pg_up(len);
    let va = if flags & MAP_FIXED != 0 {
        if addr & 0xfff != 0 {
            return -EINVAL;
        }
        p.pt().unmap_range(addr, addr + len);
        addr
    } else {
        let va = p.mmap_next;
        p.mmap_next += len;
        va
    };
    // TODO: prot を反映する。いまは常に RW
    if p.pt().alloc_range(va, va + len, Perm::RW).is_none() {
        p.pt().unmap_range(va, va + len);
        return -ENOMEM;
    }
    va as i64
}

fn sys_munmap(addr: usize, len: usize) -> i64 {
    if addr & 0xfff != 0 {
        return -EINVAL;
    }
    proc::current().pt().unmap_range(addr, addr + pg_up(len));
    0
}

fn sys_getrandom(buf: usize, len: usize) -> i64 {
    let p = proc::current().pt();
    let mut done = 0;
    while done < len {
        let b = crate::rand::bytes16();
        let n = b.len().min(len - done);
        if p.copy_out(buf + done, &b[..n]).is_none() {
            return -EFAULT;
        }
        done += n;
    }
    len as i64
}

fn sys_nanosleep(req: usize) -> i64 {
    let mut ts = [0u8; 16];
    if proc::current().pt().copy_in(&mut ts, req).is_none() {
        return -EFAULT;
    }
    let sec = u64::from_le_bytes(ts[..8].try_into().unwrap());
    let nsec = u64::from_le_bytes(ts[8..].try_into().unwrap());
    let hz = crate::timer::HZ;
    let n = sec * hz + (nsec * hz).div_ceil(1_000_000_000);
    let until = crate::timer::ticks() + n;
    while crate::timer::ticks() < until {
        proc::sleep(crate::timer::chan());
    }
    0
}

fn sys_clone(flags: u64, stack: usize, tls: u64) -> i64 {
    match proc::clone(flags, stack, tls) {
        Ok(pid) => pid as i64,
        Err(e) => e,
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
    loop {
        let mut w = [0u8; 8];
        proc::current().pt().copy_in(&mut w, va).ok_or(-EFAULT)?;
        let p = u64::from_le_bytes(w) as usize;
        if p == 0 {
            return Ok(v);
        }
        if v.len() >= MAXARG {
            return Err(-7); // E2BIG
        }
        v.push(proc::current().pt().copy_in_str(p, MAXSTR).ok_or(-EFAULT)?);
        va += 8;
    }
}

fn sys_execve(path: usize, argv: usize, envp: usize) -> i64 {
    let r = (|| {
        let path = proc::current().pt().copy_in_str(path, 4096).ok_or(-EFAULT)?;
        let path = String::from_utf8(path).map_err(|_| -2i64)?;
        let argv = copy_in_strv(argv)?;
        let envp = copy_in_strv(envp)?;
        proc::execve(&path, &argv, &envp)
    })();
    match r {
        Ok(()) => 0,
        Err(e) => e,
    }
}

fn sys_wait4(pid: i64, status: usize, options: u64) -> i64 {
    match proc::wait(pid, options) {
        Ok((pid, xstatus)) => {
            if status != 0 && pid != 0 {
                let w = ((xstatus as u32 & 0xff) << 8).to_le_bytes();
                if proc::current().pt().copy_out(status, &w).is_none() {
                    return -EFAULT;
                }
            }
            pid as i64
        }
        Err(e) => e,
    }
}

/// シグナルはまだないので、自分宛ての致命的なもの (abort など) だけ終了として扱う
fn sys_kill(pid: i64, a1: i64, a2: i64, tgkill: bool) -> i64 {
    let (target, sig) = if tgkill { (a1, a2) } else { (pid, a1) };
    let me = proc::current().pid as i64;
    if sig != 0 && (target == me || target == 0) {
        proc::exit(128 + sig as i32);
    }
    0
}
