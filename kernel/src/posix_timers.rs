// setitimer・getitimer と POSIX タイマー (timer_create など) (Linux の CONFIG_POSIX_TIMERS: kernel/time/itimer.c、
// posix-timers.c、posix-cpu-timers.c)。タイマーそのもの (signal::PosixTimer) はプロセスの代表スレッドが持つ (Linux の
// signal_struct の posix_timers)。期限は起動からの ns (Linux の hrtimer: timer::hr_at で、その時刻にタイマを鳴らす)。
// CPU の時計のものは tick ごとに見る。main.rs が syscall::register と timer::every、timer::hr_hook でつなぐ
use crate::proc;
use crate::signal::{send_group, send_thread, PosixTimer, SigInfo, NSIG, SIGALRM, SI_KERNEL, SI_TIMER};
use alloc::vec::Vec;

type R = Result<i64, i64>;
const EFAULT: i64 = 14;
const EPERM: i64 = 1;

use crate::proc::put_user as out;

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
        let (interval, value) = (tv(0), tv(16));
        leader.itimer = if value == 0 { (0, 0, 0) } else { (crate::timer::uptime_ns().saturating_add(value).max(1), interval, 0) };
        crate::timer::hr_at(hr, leader.itimer.0, false);
    }
    Ok(0)
}

fn getitimer(which: u64, cur: usize) -> R {
    if which != 0 {
        return Err(crate::why::inval());
    }
    // SIGALRM が来ていて受けとられていないあいだは 0 (止まっている)。動いていれば、過ぎていても 1 us (Linux の
    // itimer_get_remtime)
    let (deadline, interval, _) = proc::current_leader().itimer;
    let left = if deadline == 0 { 0 } else { deadline.saturating_sub(crate::timer::uptime_ns()).max(1000) };
    let tv = |ns: u64| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        b[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
        b
    };
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&tv(interval));
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
    leader.timers.push(PosixTimer {
        id,
        realtime: c == crate::timer::Clock::Real,
        signo,
        notify,
        tid,
        value,
        deadline: 0,
        interval: 0,
        overrun: 0,
        queued: false,
        sig_seq: 0,
        cpu,
        cpu_deadline: 0,
        cpu_interval: 0,
    });
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
    // 来ていてまだ受けとられていないシグナルは、受けとるときに捨てる (Linux の it_signal_seq)
    (t.queued, t.sig_seq) = (false, 0);
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
    // 期限は起動からの ns (REALTIME の絶対時刻は、いまの差で直す。過ぎていればすぐ)
    let now = crate::timer::uptime_ns();
    t.interval = interval;
    t.deadline = if value == 0 {
        0
    } else if flags & TIMER_ABSTIME != 0 {
        let cur = if t.realtime { crate::timer::epoch_ns() } else { now };
        now.saturating_add(value.saturating_sub(cur)).max(1)
    } else {
        now.saturating_add(value)
    };
    t.overrun = 0;
    crate::timer::hr_at(hr, t.deadline, false);
    Ok(0)
}

fn timer_gettime(id: i32, cur: usize) -> R {
    let t = *timer_of(id)?;
    // シグナルが来ているあいだのくり返しは、間の目盛りで先へ送って見せる (Linux の common_timer_get)
    let left = |d: u64, iv: u64, now: u64| {
        let d = if t.queued && iv != 0 && d != 0 && d <= now { forward(d, iv, now).0 } else { d };
        if d == 0 { 0 } else { d.saturating_sub(now).max(1) }
    };
    let (left, interval) = match t.cpu {
        Some(c) => (left(t.cpu_deadline, t.cpu_interval, crate::timer::clock_ns(c).unwrap_or(0)), t.cpu_interval),
        None => (left(t.deadline, t.interval, crate::timer::uptime_ns()), t.interval),
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

/// タイマーのシグナルの印 (SigInfo の addr: 0 でない。settime の前に来ていたものと見分ける)
static SIG_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// シグナルが受けとられた (か、なくなった) タイマーのくり返しを鳴らしなおす (Linux の common_hrtimer_rearm と
/// posix_cpu_timer_rearm)。とばした数を返す
fn rearm(t: &mut PosixTimer) -> u64 {
    t.queued = false;
    let (d, iv, now) = match t.cpu {
        Some(c) => match crate::timer::clock_ns(c) {
            Ok(v) => (&mut t.cpu_deadline, t.cpu_interval, v),
            Err(_) => return 0,
        },
        None => (&mut t.deadline, t.interval, crate::timer::uptime_ns()),
    };
    if iv == 0 || *d == 0 || *d > now {
        return 0;
    }
    let (n, missed) = forward(*d, iv, now);
    *d = n;
    if t.cpu.is_none() {
        crate::timer::hr_at(hr, n, false);
    }
    missed
}

/// シグナルを受けとるとき (signal::on_dequeue で置く。p は受けとるスレッド): POSIX タイマーのものは、タイマーがもう
/// ないか、来たあとに timer_settime されたものなら捨てる (false)。でなければくり返しを鳴らしなおし、とばした数を
/// si_overrun (status) と timer_getoverrun に (Linux の posixtimer_deliver_signal)。SIGALRM なら、止まっている itimer の
/// くり返しを鳴らしなおす (Linux の dequeue_signal)
pub fn dequeued(p: &mut proc::Proc, sig: i32, info: &mut SigInfo) -> bool {
    let l = if p.is_leader() {
        p
    } else {
        match proc::find_leader(p.tgid) {
            Some(l) => l,
            None => return info.code != SI_TIMER || info.addr == 0,
        }
    };
    let (d, iv, fired) = l.itimer;
    if sig == SIGALRM && d == 0 && iv != 0 && fired != 0 {
        let now = crate::timer::uptime_ns();
        let n = if fired <= now { forward(fired, iv, now).0 } else { fired };
        l.itimer = (n, iv, 0);
        crate::timer::hr_at(hr, n, false);
    }
    if info.code == SI_TIMER && info.addr != 0 {
        let Some(t) = l.timers.iter_mut().find(|t| t.id == info.timerid) else { return false };
        if !t.queued || t.sig_seq != info.addr {
            return false;
        }
        t.overrun = rearm(t).min(i32::MAX as u64) as i32;
        info.status = t.overrun;
        info.addr = 0;
    }
    true
}

/// くり返しのタイマの次の期限: deadline からの間隔の目盛りで、now より後のもの (Linux の hrtimer_forward)。とばした数も
fn forward(deadline: u64, interval: u64, now: u64) -> (u64, u64) {
    let missed = (now - deadline) / interval;
    (deadline.saturating_add((missed + 1).saturating_mul(interval)), missed)
}

/// tick ごと (timer::every、cpu0): CPU の時計のもの (と、念のためほかのもの)
pub fn tick() {
    let next = hr();
    crate::timer::hr_at(hr, next, true);
}

/// 期限の来た itimer と POSIX タイマーを鳴らす (timer::hr_run から、どの CPU でも)。のこりの壁と起動からの時計のいちばん
/// 近い期限 (0 は無し) を返す。シグナルはタイマーごとに 1 つ: 送ったら受けとられるまで止め (queued)、くり返しは受けとった
/// ときに鳴らしなおす (dequeued)
pub fn hr() -> u64 {
    let now = crate::timer::uptime_ns();
    let mut next = u64::MAX;
    for p in proc::all_leader_procs() {
        let tgid = p.tgid;
        let (deadline, interval, _) = p.itimer;
        if deadline != 0 && deadline <= now {
            // くり返しは SIGALRM を受けとったときに (それまで止まる。無視されて捨てられれば止まったまま: Linux と同じ)
            p.itimer = if interval == 0 { (0, 0, 0) } else { (0, interval, deadline) };
            let _ = send_group(tgid, SIGALRM, SigInfo { code: SI_KERNEL, ..SigInfo::ZERO });
        }
        if p.itimer.0 != 0 {
            next = next.min(p.itimer.0);
        }
        // プロセスに来ているシグナル (受けとられていないタイマーのものがあるときだけ見る)
        let shared = if p.timers.iter().any(|t| t.queued) { proc::threads_of(tgid).iter().fold(0, |a, th| a | th.sig_shared) } else { 0 };
        let mut fire: Vec<PosixTimer> = Vec::new();
        for t in p.timers.iter_mut() {
            if t.queued {
                // 来ていたシグナルが受けとられずになくなった (無視されて捨てられたなど) なら、受けとったものとして鳴らしなおす
                let b = 1u64 << (t.signo - 1);
                let pending = if t.notify == 4 { proc::find_thread(t.tid).is_some_and(|th| th.sig_pending & b != 0) } else { shared & b != 0 };
                if pending {
                    continue;
                }
                rearm(t);
            }
            // CPU の時計: その時計が期限をこえたら (もういない相手のものは鳴らない)。壁と起動からの時計: その時刻をこえたら
            let (d, iv, v) = match t.cpu {
                Some(c) => match crate::timer::clock_ns(c) {
                    Ok(v) => (&mut t.cpu_deadline, t.cpu_interval, v),
                    Err(_) => continue,
                },
                None => (&mut t.deadline, t.interval, now),
            };
            if *d != 0 && *d <= v {
                if t.notify == 1 {
                    // SIGEV_NONE: 送らないので、すぐ次へ
                    *d = if iv == 0 { 0 } else { forward(*d, iv, v).0 };
                } else {
                    // 1 回きりのものは止め、くり返しは鳴った時刻のまま受けとられるのを待つ
                    if iv == 0 {
                        *d = 0;
                    }
                    t.queued = true;
                    t.sig_seq = SIG_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    fire.push(*t);
                    continue;
                }
            }
            if t.cpu.is_none() && t.deadline != 0 {
                next = next.min(t.deadline);
            }
        }
        for t in fire {
            let info = SigInfo { code: SI_TIMER, timerid: t.id, value: t.value, addr: t.sig_seq, ..SigInfo::ZERO };
            match t.notify {
                4 => {
                    if let Some(th) = proc::find_thread(t.tid) {
                        send_thread(th, t.signo, info);
                    }
                }
                _ => {
                    let _ = send_group(tgid, t.signo, info);
                }
            }
        }
    }
    if next == u64::MAX { 0 } else { next }
}
