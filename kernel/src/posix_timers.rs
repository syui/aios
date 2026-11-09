// setitimer・getitimer と POSIX タイマー (timer_create など) (Linux の CONFIG_POSIX_TIMERS: kernel/time/itimer.c、
// posix-timers.c、posix-cpu-timers.c)。タイマーそのもの (signal::PosixTimer) はプロセスの代表スレッドが持つ (Linux の
// signal_struct の posix_timers)。main.rs が syscall::register と timer::every でつなぐ
use crate::proc;
use crate::signal::{send_group, send_thread, PosixTimer, SigInfo, NSIG, SIGALRM, SI_KERNEL, SI_TIMER};
use alloc::vec::Vec;

type R = Result<i64, i64>;
const EFAULT: i64 = 14;
const EPERM: i64 = 1;

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[SETITIMER, GETITIMER, TIMER_CREATE, TIMER_SETTIME, TIMER_GETTIME, TIMER_GETOVERRUN, TIMER_DELETE]
};

pub fn sys(n: u64, a: &[u64; 6]) -> R {
    use crate::syscall::nr::*;
    match n {
        SETITIMER => setitimer(a[0], a[1] as usize, a[2] as usize),
        GETITIMER => getitimer(a[0], a[1] as usize),
        TIMER_CREATE => timer_create(a[0], a[1] as usize, a[2] as usize),
        TIMER_SETTIME => timer_settime(a[0] as i32, a[1], a[2] as usize, a[3] as usize),
        TIMER_GETTIME => timer_gettime(a[0] as i32, a[1] as usize),
        TIMER_GETOVERRUN => timer_getoverrun(a[0] as i32),
        TIMER_DELETE => timer_delete(a[0] as i32),
        _ => Err(-38),
    }
}

fn read_timespec(va: usize) -> Result<u64, i64> {
    let mut b = [0u8; 16];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(u64::from_le_bytes(b[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[8..].try_into().unwrap()))
}

fn ns_to_ticks(ns: u64) -> u64 {
    (ns * crate::timer::HZ).div_ceil(1_000_000_000)
}

fn ticks_to_ns(t: u64) -> u64 {
    t * 1_000_000_000 / crate::timer::HZ
}

fn timespec(ns: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    b
}

/// setitimer(ITIMER_REAL): 期限が来たら SIGALRM
fn setitimer(which: u64, new: usize, old: usize) -> R {
    if which != 0 {
        return Err(crate::why::inval()); // ITIMER_VIRTUAL/PROF はない
    }
    let leader = proc::current_leader();
    if old != 0 {
        getitimer(which, old)?;
    }
    if new != 0 {
        // struct itimerval { timeval interval; timeval value; } (usec)
        let mut b = [0u8; 32];
        proc::current().pt().copy_in(&mut b, new).ok_or(-EFAULT)?;
        let tv = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap()) * 1000;
        let (interval, value) = (ns_to_ticks(tv(0)), tv(16));
        leader.itimer = if value == 0 { (0, 0) } else { (crate::timer::ticks_after(value), interval) };
    }
    Ok(0)
}

fn getitimer(which: u64, cur: usize) -> R {
    if which != 0 {
        return Err(crate::why::inval());
    }
    let (deadline, interval) = proc::current_leader().itimer;
    let left = if deadline == 0 { 0 } else { ticks_to_ns(deadline.saturating_sub(crate::timer::ticks())) };
    let tv = |ns: u64| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        b
    };
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&tv(ticks_to_ns(interval)));
    b[16..].copy_from_slice(&tv(left));
    out(cur, &b)?;
    Ok(0)
}

fn timer_create(clock: u64, sev: usize, idp: usize) -> R {
    const SIGEV_SIGNAL: i32 = 0;
    const SIGEV_NONE: i32 = 1;
    const SIGEV_THREAD_ID: i32 = 4;
    const EOPNOTSUPP: i64 = 95;
    const CAP_WAKE_ALARM: u32 = 35;
    // 時計 (Linux の clockid_to_kclock): 知らないものは EINVAL、タイマのない時計 (MONOTONIC_RAW、REALTIME_COARSE、
    // MONOTONIC_COARSE、fd の時計) は EOPNOTSUPP。CPU の時計がいないもの、ALARM の時計は CAP_WAKE_ALARM がなければ
    // EPERM は、sigevent を見たあと (Linux の kc->timer_create)
    let clk = clock as i32;
    match clk {
        0..=3 | 7..=9 | 11 => {}
        4..=6 => return Err(-EOPNOTSUPP),
        c if c < 0 && c & 7 == 3 => return Err(-EOPNOTSUPP),
        c if c < 0 => {}
        _ => return Err(crate::why::inval()),
    }
    let (mut signo, mut notify, mut tid) = (SIGALRM, SIGEV_SIGNAL, 0u32);
    let mut value = 0u64;
    if sev != 0 {
        // struct sigevent { sigval value; int signo; int notify; int tid (union) }
        let mut b = [0u8; 20];
        proc::current().pt().copy_in(&mut b, sev).ok_or(-EFAULT)?;
        value = u64::from_le_bytes(b[0..8].try_into().unwrap());
        signo = i32::from_le_bytes(b[8..12].try_into().unwrap());
        notify = i32::from_le_bytes(b[12..16].try_into().unwrap());
        tid = u32::from_le_bytes(b[16..20].try_into().unwrap());
        if !matches!(notify, SIGEV_SIGNAL | SIGEV_NONE | SIGEV_THREAD_ID) || (notify != SIGEV_NONE && !(1..NSIG as i32).contains(&signo)) {
            return Err(crate::why::inval());
        }
    }
    let c = crate::timer::clock_of(clk, false)?;
    if matches!(clk, 8 | 9) && !crate::cred::current().capable(CAP_WAKE_ALARM) {
        return Err(-EPERM);
    }
    let cpu = matches!(c, crate::timer::Clock::Cpu { .. }).then_some(c);
    let leader = proc::current_leader();
    let id = (0..).find(|i| !leader.timers.iter().any(|t| t.id == *i)).unwrap();
    if value == 0 && sev == 0 {
        value = id as u64;
    }
    leader.timers.push(PosixTimer { id, realtime: c == crate::timer::Clock::Real, signo, notify, tid, value, deadline: 0, interval: 0, overrun: 0, cpu, cpu_deadline: 0, cpu_interval: 0 });
    out(idp, &id.to_le_bytes())?;
    Ok(0)
}

fn timer_of(id: i32) -> Result<&'static mut PosixTimer, i64> {
    proc::current_leader().timers.iter_mut().find(|t| t.id == id).ok_or_else(|| crate::why::inval())
}

fn timer_settime(id: i32, flags: u64, new: usize, old: usize) -> R {
    const TIMER_ABSTIME: u64 = 1;
    if old != 0 {
        timer_gettime(id, old)?;
    }
    let interval = read_timespec(new)?;
    let value = read_timespec(new + 16)?;
    let t = timer_of(id)?;
    // CPU の時計: その時計の ns で覚える (相手がもういなければ 0 から)
    if let Some(c) = t.cpu {
        let now = crate::timer::clock_ns(c).unwrap_or(0);
        t.cpu_interval = interval;
        t.cpu_deadline = if value == 0 {
            0
        } else if flags & TIMER_ABSTIME != 0 {
            value.max(1)
        } else {
            now.saturating_add(value)
        };
        t.overrun = 0;
        return Ok(0);
    }
    t.interval = ns_to_ticks(interval);
    t.deadline = if value == 0 {
        0
    } else if flags & TIMER_ABSTIME != 0 {
        let cur = if t.realtime { crate::timer::epoch_ns() } else { crate::timer::uptime_ns() };
        crate::timer::ticks_after(value.saturating_sub(cur).max(1))
    } else {
        crate::timer::ticks_after(value)
    };
    t.overrun = 0;
    Ok(0)
}

fn timer_gettime(id: i32, cur: usize) -> R {
    let t = *timer_of(id)?;
    let (left, interval) = match t.cpu {
        Some(c) => {
            let now = crate::timer::clock_ns(c).unwrap_or(0);
            (if t.cpu_deadline == 0 { 0 } else { t.cpu_deadline.saturating_sub(now).max(1) }, t.cpu_interval)
        }
        None => (if t.deadline == 0 { 0 } else { ticks_to_ns(t.deadline.saturating_sub(crate::timer::ticks())) }, ticks_to_ns(t.interval)),
    };
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&timespec(interval));
    b[16..].copy_from_slice(&timespec(left));
    out(cur, &b)?;
    Ok(0)
}

fn timer_getoverrun(id: i32) -> R {
    Ok(timer_of(id)?.overrun as i64)
}

fn timer_delete(id: i32) -> R {
    let l = proc::current_leader();
    let before = l.timers.len();
    l.timers.retain(|t| t.id != id);
    if l.timers.len() == before { Err(crate::why::inval()) } else { Ok(0) }
}

/// タイマ割り込みから (timer::every で tick ごと): 期限の来た itimer と POSIX タイマーを鳴らす
pub fn tick() {
    let now = crate::timer::ticks();
    for p in proc::all_leader_procs() {
        let tgid = p.tgid;
        let (deadline, interval) = p.itimer;
        if deadline != 0 && deadline <= now {
            p.itimer = if interval == 0 { (0, 0) } else { (now + interval, interval) };
            let _ = send_group(tgid, SIGALRM, SigInfo { code: SI_KERNEL, ..SigInfo::ZERO });
        }
        let mut fire: Vec<PosixTimer> = Vec::new();
        for t in p.timers.iter_mut() {
            // CPU の時計: その時計が期限をこえたら (もういない相手のものは鳴らない)
            if let Some(c) = t.cpu {
                if t.cpu_deadline == 0 {
                    continue;
                }
                let Ok(v) = crate::timer::clock_ns(c) else { continue };
                if v >= t.cpu_deadline {
                    fire.push(*t);
                    t.cpu_deadline = if t.cpu_interval == 0 {
                        0
                    } else {
                        let missed = (v - t.cpu_deadline) / t.cpu_interval;
                        t.overrun = missed.min(i32::MAX as u64) as i32;
                        t.cpu_deadline + (missed + 1) * t.cpu_interval
                    };
                }
                continue;
            }
            if t.deadline != 0 && t.deadline <= now {
                fire.push(*t);
                t.deadline = if t.interval == 0 { 0 } else { now + t.interval };
            }
        }
        for t in fire {
            let info = SigInfo { code: SI_TIMER, timerid: t.id, value: t.value, ..SigInfo::ZERO };
            match t.notify {
                4 => {
                    if let Some(th) = proc::find_thread(t.tid) {
                        send_thread(th, t.signo, info);
                    }
                }
                0 => {
                    let _ = send_group(tgid, t.signo, info);
                }
                _ => {}
            }
        }
    }
}
