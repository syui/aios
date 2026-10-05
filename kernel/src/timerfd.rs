// timerfd (timerfd_create / timerfd_settime / timerfd_gettime)。決めた時刻に読めるようになる fd。
// read で、前に読んでから何回期限が来たか (u64) が返る。interval があればくりかえす。
// 期限が来たかは、読むとき・poll のとき・タイマの割り込み (tick、cpu0 で 1/100 秒ごと) に見る。
// 時刻はみな起動からの ns (CLOCK_MONOTONIC) で覚える。CLOCK_REALTIME の絶対時刻は、決めたときに直す
use crate::file::{self, Kind};
use crate::proc;
use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

const EINVAL: i64 = 22;
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
    /// 次の期限 (起動からの ns。0 なら止まっている)
    next: u64,
    interval: u64,
    /// まだ読まれていない期限の数
    ticks: u64,
    generation: u64,
}

pub type TimerRef = Rc<RefCell<TimerFd>>;

/// 動いているタイマ (tick で見る)
static mut ARMED: Vec<Weak<RefCell<TimerFd>>> = Vec::new();

fn armed() -> &'static mut Vec<Weak<RefCell<TimerFd>>> {
    unsafe { &mut *(&raw mut ARMED) }
}

pub fn chan(t: &TimerRef) -> usize {
    Rc::as_ptr(t) as usize
}

fn now() -> u64 {
    crate::timer::uptime_ns()
}

/// 期限が来ていたら ticks を増やして次の期限へ。増えたら true
fn update(t: &mut TimerFd) -> bool {
    let n = now();
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
    if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM) || flags & !(TFD_CLOEXEC | TFD_NONBLOCK) != 0 {
        return Err(-EINVAL);
    }
    let t = Rc::new(RefCell::new(TimerFd { realtime: matches!(clock, CLOCK_REALTIME | CLOCK_REALTIME_ALARM), next: 0, interval: 0, ticks: 0, generation: 0 }));
    let fl = file::O_RDWR | if flags & TFD_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 };
    let fd = proc::current().files().add(file::new(Kind::TimerFd(t), fl), flags & TFD_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn timer_of(fd: u64) -> Result<TimerRef, i64> {
    let f = proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let k = f.borrow();
    match &k.kind {
        Kind::TimerFd(t) => Ok(t.clone()),
        _ => Err(-EINVAL),
    }
}

/// struct timespec (秒, ns) の ns
fn ts_ns(b: &[u8]) -> Result<u64, i64> {
    let s = i64::from_le_bytes(b[0..8].try_into().unwrap());
    let ns = i64::from_le_bytes(b[8..16].try_into().unwrap());
    if s < 0 || !(0..1_000_000_000).contains(&ns) {
        return Err(-EINVAL);
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
        put_ts(&mut b[16..32], t.next.saturating_sub(now()).max(1));
    }
    b
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
        tm.next = if value == 0 {
            0
        } else if flags & TFD_TIMER_ABSTIME != 0 {
            // 絶対時刻: REALTIME なら今の差で起動からの時刻に直す (過ぎていれば今)
            let base = if tm.realtime { crate::timer::epoch_ns().saturating_sub(now()) } else { 0 };
            value.saturating_sub(base).max(1)
        } else {
            now().saturating_add(value)
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
        return Err(-EINVAL);
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
        // 期限 (tick) まで眠る。止まっているタイマなら settime を待つ
        let d = if deadline == 0 { 0 } else { deadline.div_ceil(1_000_000_000 / crate::timer::HZ).max(1) };
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
