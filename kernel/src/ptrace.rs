// ptrace (Linux の kernel/ptrace.c と arch/arm64/kernel/ptrace.c)。止まるところ (シグナルの知らせ、PTRACE_SYSCALL の入口と
// 出口、PTRACE_INTERRUPT) と tracee が眠るところは core (signal::ptrace_stop、proc::ptrace_sleep) にあり、ここは tracer の
// たのみ (つなぐ・離す・続けさせる・メモリとレジスタを読み書き) だけ。main.rs が syscall::register でつなぐ
use crate::cred;
use crate::proc::{self, Proc};
use crate::signal::{self, SigInfo, SIGKILL, SIGSTOP};
use crate::trap::TrapFrame;

type R = Result<i64, i64>;
const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EIO: i64 = 5;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

const PTRACE_TRACEME: u64 = 0;
const PTRACE_PEEKTEXT: u64 = 1;
const PTRACE_PEEKDATA: u64 = 2;
const PTRACE_POKETEXT: u64 = 4;
const PTRACE_POKEDATA: u64 = 5;
const PTRACE_CONT: u64 = 7;
const PTRACE_KILL: u64 = 8;
const PTRACE_ATTACH: u64 = 16;
const PTRACE_DETACH: u64 = 17;
const PTRACE_SYSCALL: u64 = 24;
const PTRACE_SETOPTIONS: u64 = 0x4200;
const PTRACE_GETSIGINFO: u64 = 0x4202;
const PTRACE_GETREGSET: u64 = 0x4204;
const PTRACE_SETREGSET: u64 = 0x4205;
const PTRACE_SEIZE: u64 = 0x4206;
const PTRACE_INTERRUPT: u64 = 0x4207;
/// 知っている PTRACE_O_* (Linux の PTRACE_O_MASK)
const PTRACE_O_MASK: u64 = 0x0030_00ff;
/// GETREGSET の NT_PRSTATUS (struct user_pt_regs: x0〜x30、sp、pc、pstate の 272 バイト)
const NT_PRSTATUS: u64 = 1;
const PRSTATUS_SIZE: usize = 34 * 8;

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[PTRACE]
};

pub fn sys(n: u64, a: &[u64; 6]) -> R {
    match n {
        crate::syscall::nr::PTRACE => ptrace(a[0], a[1] as u32 as i32, a[2] as usize, a[3] as usize),
        _ => Err(-38),
    }
}

/// tracer がつなげてよいか (Linux の ptrace_may_access の PTRACE_MODE_ATTACH_REALCREDS): CAP_SYS_PTRACE か、uid と gid が
/// みな同じで dumpable なもの。LSM (landlock と AppArmor) も
fn may_attach(t: &Proc) -> bool {
    let (c, tc) = (cred::current(), &t.cred);
    let ids = c.uid == tc.uid && c.uid == tc.euid && c.uid == tc.suid && c.gid == tc.gid && c.gid == tc.egid && c.gid == tc.sgid;
    let cap = c.capable(cred::CAP_SYS_PTRACE);
    if !(ids && crate::pidfd::dumpable(t) == 1) && !cap {
        return false;
    }
    crate::landlock::check_ptrace(c.sandbox.as_ref(), tc.sandbox.as_ref()).is_ok() && crate::apparmor::may_ptrace(tc, false).is_ok()
}

/// 自分が tracer で、(state なら) 止まっている tracee (Linux の ptrace_check_attach)。でなければ ESRCH
fn traced(t: &'static mut Proc, stopped: bool) -> Result<&'static mut Proc, i64> {
    if t.ptracer != proc::current().tgid || (stopped && (t.pt_status == 0 || t.pt_tf.is_null())) {
        return Err(-ESRCH);
    }
    Ok(t)
}

/// 続けさせる (CONT、SYSCALL、DETACH): シグナルは 0〜64 (でなければ EIO)
fn resume(t: &mut Proc, sig: usize, syscall: bool) -> R {
    if sig > 64 {
        return Err(-EIO);
    }
    t.pt_syscall = syscall;
    t.pt_resume = Some(sig as i32);
    proc::wakeup(proc::pt_chan(t));
    Ok(0)
}

fn attach(t: &'static mut Proc, seize: bool, addr: usize, data: usize) -> R {
    let me = proc::current();
    if seize && (addr != 0 || data as u64 & !PTRACE_O_MASK != 0) {
        return Err(if addr != 0 { -EIO } else { -EINVAL });
    }
    // カーネルのスレッド、自分のスレッドグループ、もうつながっているものは EPERM
    if t.kthread || t.tgid == me.tgid || t.ptracer != 0 || !may_attach(t) {
        return Err(-EPERM);
    }
    t.ptracer = me.tgid;
    t.pt_seized = seize;
    t.pt_opts = if seize { data as u32 } else { 0 };
    t.sync_cred_work();
    // ATTACH は SIGSTOP を送る (止まったら tracer に知らせる)。SEIZE は止めない
    if !seize {
        let info = SigInfo { code: 0x80, ..SigInfo::ZERO };
        signal::send_thread(t, SIGSTOP, info);
    }
    Ok(0)
}

pub fn ptrace(req: u64, pid: i32, addr: usize, data: usize) -> R {
    let me = proc::current();
    if req == PTRACE_TRACEME {
        // 親が tracer になる。もうつながっていれば EPERM
        if me.ptracer != 0 {
            return Err(-EPERM);
        }
        let parent = proc::find_leader(me.ppid).ok_or(-EPERM)?;
        me.ptracer = parent.tgid;
        me.pt_seized = false;
        me.sync_cred_work();
        return Ok(0);
    }
    let tid = if pid > 0 { crate::ns::to_global(pid as u32) } else { None };
    let t = tid.and_then(proc::find_thread).ok_or(-ESRCH)?;
    match req {
        PTRACE_ATTACH | PTRACE_SEIZE => return attach(t, req == PTRACE_SEIZE, addr, data),
        // KILL と INTERRUPT は止まっていなくても
        PTRACE_KILL => {
            let t = traced(t, false)?;
            signal::send_thread(t, SIGKILL, SigInfo::ZERO);
            return Ok(0);
        }
        PTRACE_INTERRUPT => {
            let t = traced(t, false)?;
            if !t.pt_seized {
                return Err(-EIO);
            }
            t.pt_interrupt = true;
            proc::interrupt(t);
            return Ok(0);
        }
        _ => {}
    }
    let t = traced(t, true)?;
    match req {
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let mut b = [0u8; 8];
            if remote(t, addr, &mut b, false) != 8 {
                return Err(-EIO);
            }
            proc::put_user(data, &b)?;
            Ok(0)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let mut b = (data as u64).to_le_bytes();
            if remote(t, addr, &mut b, true) != 8 {
                return Err(-EIO);
            }
            Ok(0)
        }
        PTRACE_CONT => resume(t, data, false),
        PTRACE_SYSCALL => resume(t, data, true),
        PTRACE_DETACH => {
            if data > 64 {
                return Err(-EIO);
            }
            t.ptracer = 0;
            t.pt_seized = false;
            t.pt_opts = 0;
            t.sync_cred_work();
            resume(t, data, false)
        }
        PTRACE_SETOPTIONS => {
            if data as u64 & !PTRACE_O_MASK != 0 {
                return Err(-EINVAL);
            }
            t.pt_opts = data as u32;
            Ok(0)
        }
        PTRACE_GETSIGINFO => {
            let (sig, info) = t.pt_info;
            proc::put_user(data, &signal::siginfo_of(sig, &info))?;
            Ok(0)
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => regset(t, req == PTRACE_SETREGSET, addr as u64, data),
        // PEEKUSR / POKEUSR (arm64 にはない) ほか、知らないもの
        _ => Err(-EIO),
    }
}

/// tracee のメモリを読む / 書く (Linux の ptrace_access_vm: /proc/PID/mem と同じ道)。読み書きできたバイト数
fn remote(t: &Proc, addr: usize, buf: &mut [u8], write: bool) -> usize {
    let Some(mm) = t.mm.as_ref().filter(|_| !t.kthread) else { return 0 };
    let pt = &mut mm.get().pt;
    let _h = pt.hold();
    if write { pt.write_remote(addr, buf) } else { pt.read_remote(addr, buf) }
}

/// GETREGSET / SETREGSET (iovec): NT_PRSTATUS だけ (ほかは EINVAL)。iov_len は写した長さに
fn regset(t: &mut Proc, set: bool, note: u64, iov: usize) -> R {
    if note != NT_PRSTATUS {
        return Err(-EINVAL);
    }
    let mut v = [0u8; 16];
    proc::current().pt().copy_in(&mut v, iov).ok_or(-EFAULT)?;
    let base = u64::from_le_bytes(v[..8].try_into().unwrap()) as usize;
    let len = (u64::from_le_bytes(v[8..].try_into().unwrap()) as usize).min(PRSTATUS_SIZE);
    // 止まっているあいだだけ (traced で確かめた)。tracee のカーネルのスタックの上のレジスタ
    let tf: &mut TrapFrame = unsafe { &mut *t.pt_tf };
    let mut b = [0u8; PRSTATUS_SIZE];
    for (i, r) in tf.x.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&r.to_le_bytes());
    }
    b[248..256].copy_from_slice(&tf.sp_el0.to_le_bytes());
    b[256..264].copy_from_slice(&tf.elr.to_le_bytes());
    b[264..272].copy_from_slice(&tf.spsr.to_le_bytes());
    if set {
        proc::current().pt().copy_in(&mut b[..len], base).ok_or(-EFAULT)?;
        let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        for i in 0..31 {
            tf.x[i] = w(i);
        }
        tf.sp_el0 = w(31);
        tf.elr = w(32);
        // pstate は EL0 の印だけ (NZCV など)。モードは変えさせない (Linux の valid_user_regs)
        tf.spsr = (tf.spsr & !0xf000_0000) | (w(33) & 0xf000_0000);
    } else {
        proc::put_user(base, &b[..len])?;
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&(len as u64).to_le_bytes());
    proc::put_user(iov + 8, &out)?;
    Ok(0)
}
