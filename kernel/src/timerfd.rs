// timerfd (timerfd_create / timerfd_settime / timerfd_gettime)。決めた時刻に読めるようになる fd。
// read で、前に読んでから何回期限が来たか (u64) が返る。interval があればくりかえす。
// 期限が来たかは、読むとき・poll のとき・タイマの割り込み (tick、cpu0 で 1/100 秒ごと) に見る。
// 時刻はみな起動からの ns (CLOCK_MONOTONIC) で覚える。CLOCK_REALTIME の絶対時刻は、決めたときに直す。
// CPU の時計 (CLOCK_PROCESS_CPUTIME_ID と CLOCK_THREAD_CPUTIME_ID: 作ったプロセスかスレッドのもの) は、その時計の ns で
// 覚えて tick ごとに見る: aios がえらんだちがい (timerfd-cputime)。Linux は EINVAL (timerfd_create(2) の BUGS:
// timer_create はできるのに)。シグナルなしで、epoll などで CPU を使いすぎたことを待てる
use crate::cell::Bkl;
use crate::file::{self, Kind};
use crate::proc;
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EBADF: i64 = 9;

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_BOOTTIME: u64 = 7;
const CLOCK_REALTIME_ALARM: u64 = 8;
const CLOCK_BOOTTIME_ALARM: u64 = 9;
const TFD_TIMER_ABSTIME: u64 = 1;
const TFD_CLOEXEC: u64 = 0o2000000;
const TFD_NONBLOCK: u64 = 0o4000;

pub struct TimerFd {
    realtime: bool,
    /// CPU の時計なら、その時計 (next と interval はその ns)
    cpu: Option<crate::timer::Clock>,
    /// 作ったときの時計と、さいごの settime の印 (TFD_TIMER_ABSTIME と TFD_TIMER_CANCEL_ON_SET。/proc/PID/fdinfo)
    clockid: u8,
    settime_flags: u8,
    /// 次の期限 (起動からの ns。0 なら止まっている)
    next: u64,
    interval: u64,
    /// まだ読まれていない期限の数
    ticks: u64,
    generation: u64,
}

pub type TimerRef = Rc<RefCell<TimerFd>>;

/// 動いているタイマ (tick で見る)
static ARMED: Bkl<Vec<Weak<RefCell<TimerFd>>>> = Bkl::new(Vec::new());

#[track_caller]
fn armed() -> &'static mut Vec<Weak<RefCell<TimerFd>>> {
    ARMED.get()
}

pub fn chan(t: &TimerRef) -> usize {
    Rc::as_ptr(t) as usize
}

fn now() -> u64 {
    crate::timer::uptime_ns()
}

/// そのタイマの時計のいま (CPU の時計の相手がもういなければ None: もう鳴らない)
fn now_of(t: &TimerFd) -> Option<u64> {
    match t.cpu {
        Some(c) => crate::timer::clock_ns(c).ok(),
        None => Some(now()),
    }
}

/// 期限が来ていたら ticks を増やして次の期限へ。増えたら true
fn update(t: &mut TimerFd) -> bool {
    let Some(n) = now_of(t) else { return false };
    if t.next == 0 || n < t.next {
        return false;
    }
    if t.interval > 0 {
        let k = 1 + (n - t.next) / t.interval;
        t.ticks += k;
        t.next += k * t.interval;
    } else {
        t.ticks += 1;
        t.next = 0;
    }
    t.generation += 1;
    true
}

fn wake(t: &TimerRef) {
    proc::wakeup(chan(t));
    proc::poll_wake(chan(t));
}

pub fn create(clock: u64, flags: u64) -> Result<i64, i64> {
    const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
    const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
    const CAP_WAKE_ALARM: u32 = 35;
    const EPERM: i64 = 1;
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM | CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID)
        || flags & !(TFD_CLOEXEC | TFD_NONBLOCK) != 0
    {
        return Err(crate::why::inval());
    }
    // ALARM の時計は CAP_WAKE_ALARM (Linux と同じ)
    if matches!(clock, CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM) && !crate::cred::current().capable(CAP_WAKE_ALARM) {
        return Err(-EPERM);
    }
    // aios がえらんだちがい (timerfd-cputime): CPU の時計 (作ったプロセスかスレッドのもの)
    let cpu = matches!(clock, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID).then(|| crate::timer::clock_of(clock as i32, false)).transpose()?;
    let t = Rc::new(RefCell::new(TimerFd { realtime: matches!(clock, CLOCK_REALTIME | CLOCK_REALTIME_ALARM), cpu, clockid: clock as u8, settime_flags: 0, next: 0, interval: 0, ticks: 0, generation: 0 }));
    let fl = file::O_RDWR | if flags & TFD_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 };
    let fd = proc::current().files().add(file::new(Kind::TimerFd(t), fl), flags & TFD_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn timer_of(fd: u64) -> Result<TimerRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let k = f.borrow();
    match &k.kind {
        Kind::TimerFd(t) => Ok(t.clone()),
        _ => Err(crate::why::inval()),
    }
}

/// struct timespec (秒, ns) の ns
fn ts_ns(b: &[u8]) -> Result<u64, i64> {
    let s = i64::from_le_bytes(b[0..8].try_into().unwrap());
    let ns = i64::from_le_bytes(b[8..16].try_into().unwrap());
    if s < 0 || !(0..1_000_000_000).contains(&ns) {
        return Err(crate::why::inval());
    }
    Ok((s as u64).saturating_mul(1_000_000_000).saturating_add(ns as u64))
}

fn put_ts(b: &mut [u8], ns: u64) {
    b[0..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b[8..16].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
}

/// struct itimerspec { it_interval, it_value } の今の値
fn current_spec(t: &mut TimerFd) -> [u8; 32] {
    update(t);
    let mut b = [0u8; 32];
    put_ts(&mut b[0..16], t.interval);
    if t.next != 0 {
        let n = now_of(t).unwrap_or(0);
        put_ts(&mut b[16..32], t.next.saturating_sub(n).max(1));
    }
    b
}

/// /proc/PID/fdinfo の行 (Linux の timerfd_show): 時計、まだ読まれていない期限の数、settime の印、のこりと間隔
pub fn fdinfo(t: &TimerRef) -> alloc::string::String {
    let b = current_spec(&mut t.borrow_mut());
    let w = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let tm = t.borrow();
    alloc::format!(
        "clockid: {}\nticks: {}\nsettime flags: 0{:o}\nit_value: ({}, {})\nit_interval: ({}, {})\n",
        tm.clockid,
        tm.ticks,
        tm.settime_flags,
        w(16),
        w(24),
        w(0),
        w(8)
    )
}

pub fn settime(fd: u64, flags: u64, new: usize, old: usize) -> Result<i64, i64> {
    let t = timer_of(fd)?;
    let pt = proc::current().pt();
    let mut b = [0u8; 32];
    pt.copy_in(&mut b, new).ok_or(-EFAULT)?;
    let (interval, value) = (ts_ns(&b[0..16])?, ts_ns(&b[16..32])?);
    let prev = current_spec(&mut t.borrow_mut());
    {
        let mut tm = t.borrow_mut();
        tm.ticks = 0;
        tm.interval = interval;
        tm.settime_flags = (flags & 3) as u8;
        tm.next = if value == 0 {
            0
        } else if flags & TFD_TIMER_ABSTIME != 0 {
            // 絶対時刻: REALTIME なら今の差で起動からの時刻に直す (過ぎていれば今)。CPU の時計はそのまま
            let base = if tm.realtime { crate::timer::epoch_ns().saturating_sub(now()) } else { 0 };
            value.saturating_sub(base).max(1)
        } else {
            now_of(&tm).unwrap_or(0).saturating_add(value)
        };
        tm.generation += 1;
    }
    if old != 0 {
        pt.copy_out(old, &prev).ok_or(-EFAULT)?;
    }
    if t.borrow().next != 0 {
        armed().push(Rc::downgrade(&t));
    }
    // もう過ぎていれば、すぐ知らせる
    if update(&mut t.borrow_mut()) {
        wake(&t);
    }
    Ok(0)
}

pub fn gettime(fd: u64, cur: usize) -> Result<i64, i64> {
    let t = timer_of(fd)?;
    let b = current_spec(&mut t.borrow_mut());
    proc::current().pt().copy_out(cur, &b).ok_or(-EFAULT)?;
    Ok(0)
}

pub fn read(t: &TimerRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if dst.len() < 8 {
        return Err(crate::why::inval());
    }
    loop {
        let deadline = {
            let mut tm = t.borrow_mut();
            update(&mut tm);
            if tm.ticks > 0 {
                dst[..8].copy_from_slice(&tm.ticks.to_le_bytes());
                tm.ticks = 0;
                return Ok(8);
            }
            tm.next
        };
        if nonblock {
            return Err(-EAGAIN);
        }
        // 期限 (tick) まで眠る。止まっているタイマなら settime を待つ。CPU の時計は壁の時刻では来ないので、tick が起こす
        let cpu = t.borrow().cpu.is_some();
        let d = if deadline == 0 || cpu { 0 } else { deadline.div_ceil(1_000_000_000 / crate::timer::HZ).max(1) };
        proc::sleep_until(chan(t), d)?;
    }
}

pub fn readiness(t: &TimerRef) -> (bool, bool, bool) {
    let mut tm = t.borrow_mut();
    update(&mut tm);
    (tm.ticks > 0, false, false)
}

pub fn generation(t: &TimerRef) -> u64 {
    t.borrow().generation
}

/// タイマの割り込みから (cpu0): 期限の来たものの待ち手を起こす。止まったもの・閉じたものは外す
pub fn tick() {
    let list = armed();
    if list.is_empty() {
        return;
    }
    let mut fired = Vec::new();
    list.retain(|w| {
        let Some(t) = w.upgrade() else { return false };
        let mut tm = t.borrow_mut();
        if update(&mut tm) {
            fired.push(t.clone());
        }
        tm.next != 0
    });
    for t in fired {
        wake(&t);
    }
}

/// システムコール (main.rs が syscall::register で置く。Linux の fs/timerfd.c)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[TIMERFD_CREATE, TIMERFD_SETTIME, TIMERFD_GETTIME]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        TIMERFD_CREATE => create(a[0], a[1]),
        TIMERFD_SETTIME => settime(a[0], a[1], a[2] as usize, a[3] as usize),
        TIMERFD_GETTIME => gettime(a[0], a[1] as usize),
        _ => Err(-38),
    }
}
