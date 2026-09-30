// aarch64 Linux 互換のシステムコール
// 番号は x8、引数は x0..x5、戻り値は x0 (エラーは -errno)
use crate::proc;
use crate::trap::TrapFrame;
use crate::vm::{pg_up, Perm};

const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ENOSYS: i64 = 38;

const SYS_IOCTL: u64 = 29;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_WRITEV: u64 = 66;
const SYS_PPOLL: u64 = 73;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_SET_TID_ADDRESS: u64 = 96;
const SYS_FUTEX: u64 = 98;
const SYS_SET_ROBUST_LIST: u64 = 99;
const SYS_CLOCK_GETTIME: u64 = 113;
const SYS_SCHED_YIELD: u64 = 124;
const SYS_SIGALTSTACK: u64 = 132;
const SYS_RT_SIGACTION: u64 = 134;
const SYS_RT_SIGPROCMASK: u64 = 135;
const SYS_UNAME: u64 = 160;
const SYS_GETPID: u64 = 172;
const SYS_GETPPID: u64 = 173;
const SYS_GETUID: u64 = 174;
const SYS_GETEUID: u64 = 175;
const SYS_GETGID: u64 = 176;
const SYS_GETEGID: u64 = 177;
const SYS_GETTID: u64 = 178;
const SYS_BRK: u64 = 214;
const SYS_MUNMAP: u64 = 215;
const SYS_MMAP: u64 = 222;
const SYS_MPROTECT: u64 = 226;
const SYS_MADVISE: u64 = 233;
const SYS_GETRANDOM: u64 = 278;

pub fn dispatch(tf: &mut TrapFrame) {
    let a = tf.x;
    let ret = match tf.x[8] {
        SYS_IOCTL => -ENOTTY,
        SYS_READ => 0, // 入力はまだない (EOF)
        SYS_WRITE => sys_write(a[0], a[1] as usize, a[2] as usize),
        SYS_WRITEV => sys_writev(a[0], a[1] as usize, a[2] as usize),
        SYS_PPOLL => 0,
        SYS_EXIT | SYS_EXIT_GROUP => proc::exit(a[0] as i32),
        SYS_SET_TID_ADDRESS | SYS_GETPID | SYS_GETTID => proc::current().pid as i64,
        SYS_GETPPID => 0,
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_FUTEX | SYS_SET_ROBUST_LIST | SYS_SCHED_YIELD => 0,
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

fn sys_write(fd: u64, buf: usize, len: usize) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    let p = proc::current();
    let mut chunk = [0u8; 128];
    let mut done = 0;
    while done < len {
        let n = chunk.len().min(len - done);
        if p.pagetable.copy_in(&mut chunk[..n], buf + done).is_none() {
            return if done == 0 { -EFAULT } else { done as i64 };
        }
        let _g = crate::uart::LOCK.lock();
        for &c in &chunk[..n] {
            if c == b'\n' {
                crate::uart::putc(b'\r');
            }
            crate::uart::putc(c);
        }
        done += n;
    }
    done as i64
}

fn sys_writev(fd: u64, iov: usize, cnt: usize) -> i64 {
    let p = proc::current();
    let mut total = 0;
    for i in 0..cnt {
        let mut v = [0u8; 16];
        if p.pagetable.copy_in(&mut v, iov + i * 16).is_none() {
            return -EFAULT;
        }
        let base = u64::from_le_bytes(v[..8].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(v[8..].try_into().unwrap()) as usize;
        let n = sys_write(fd, base, len);
        if n < 0 {
            return if total == 0 { n } else { total };
        }
        total += n;
    }
    total
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
    match proc::current().pagetable.copy_out(ts, &b) {
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
    match proc::current().pagetable.copy_out(buf, &u) {
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
        if p.pagetable.alloc_range(old, new, Perm::RW).is_none() {
            p.pagetable.unmap_range(old, new);
            return p.brk as i64;
        }
    } else if new < old {
        p.pagetable.unmap_range(new, old);
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
        p.pagetable.unmap_range(addr, addr + len);
        addr
    } else {
        let va = p.mmap_next;
        p.mmap_next += len;
        va
    };
    // TODO: prot を反映する。いまは常に RW
    if p.pagetable.alloc_range(va, va + len, Perm::RW).is_none() {
        p.pagetable.unmap_range(va, va + len);
        return -ENOMEM;
    }
    va as i64
}

fn sys_munmap(addr: usize, len: usize) -> i64 {
    if addr & 0xfff != 0 {
        return -EINVAL;
    }
    proc::current().pagetable.unmap_range(addr, addr + pg_up(len));
    0
}

fn sys_getrandom(buf: usize, len: usize) -> i64 {
    let p = proc::current();
    let mut done = 0;
    while done < len {
        let b = crate::rand::bytes16();
        let n = b.len().min(len - done);
        if p.pagetable.copy_out(buf + done, &b[..n]).is_none() {
            return -EFAULT;
        }
        done += n;
    }
    len as i64
}
