// epoll と eventfd (tokio / mio がこれで待つ)
//
// epoll は登録された fd の OpenFile を弱い参照で持ち、待つたびに readiness を見て回る
// (poll と同じく poll_chan で起こされる)。EPOLLET は「前に知らせたときから新しく立った
// ビットがあるか、ファイルの世代 (書かれた回数など) が進んだとき」に知らせる。
// eventfd は書くたびに新しいイベントになる (mio の Waker はこれを当てにしている)。
use crate::file::{self, FileRef, Kind};
use crate::proc;
use alloc::collections::BTreeMap;
use alloc::rc::{Rc, Weak};
use core::cell::RefCell;

const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EEXIST: i64 = 17;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const ELOOP: i64 = 40;

const EPOLLIN: u32 = 0x001;
const EPOLLPRI: u32 = 0x002;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLRDNORM: u32 = 0x040;
const EPOLLWRNORM: u32 = 0x100;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// aarch64 の struct epoll_event (パックされない: u32 + 詰め物 + u64 = 16 バイト)
const EVENT_SIZE: usize = 16;

struct Entry {
    file: Weak<RefCell<file::OpenFile>>,
    events: u32,
    data: u64,
    /// EPOLLET 用: 前に知らせたビットと世代
    last: u32,
    last_gen: u64,
}

#[derive(Default)]
pub struct Epoll {
    /// fd 番号 → 登録
    entries: BTreeMap<i32, Entry>,
}

pub type EpollRef = Rc<RefCell<Epoll>>;

/// 今の状態を epoll のビットで
fn mask_of(f: &FileRef) -> u32 {
    let (r, w, hup) = f.borrow().readiness();
    let mut m = 0;
    if r {
        m |= EPOLLIN | EPOLLRDNORM;
    }
    if w {
        m |= EPOLLOUT | EPOLLWRNORM;
    }
    if hup {
        m |= EPOLLHUP | EPOLLRDHUP;
    }
    m
}

impl Epoll {
    /// 知らせることのある (fd, events, data) を集める。consume なら EPOLLET/ONESHOT の状態を進める
    fn collect(&mut self, max: usize, consume: bool) -> alloc::vec::Vec<(u32, u64)> {
        let mut out = alloc::vec::Vec::new();
        self.entries.retain(|_, e| e.file.strong_count() > 0);
        for e in self.entries.values_mut() {
            if out.len() >= max {
                break;
            }
            let Some(f) = e.file.upgrade() else { continue };
            let now = mask_of(&f);
            let now_gen = f.borrow().event_gen();
            let want = e.events | EPOLLERR | EPOLLHUP;
            let mut ready = now & want;
            if e.events & EPOLLET != 0 && now_gen == e.last_gen {
                ready &= !e.last;
            }
            if consume {
                e.last = now;
                e.last_gen = now_gen;
            }
            if e.events & !(EPOLLET | EPOLLONESHOT) == 0 {
                // ONESHOT で止められているもの
                continue;
            }
            if ready != 0 {
                out.push((ready, e.data));
                if consume && e.events & EPOLLONESHOT != 0 {
                    e.events &= EPOLLET | EPOLLONESHOT;
                }
            }
        }
        out
    }

    /// 調べるための様子: 登録ごとに fd、見張るビット、いまのビット、前に知らせたビットと世代
    pub fn debug(&self) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for (fd, e) in &self.entries {
            let Some(f) = e.file.upgrade() else { continue };
            let now = mask_of(&f);
            let g = f.borrow().event_gen();
            s.push_str(&alloc::format!(" [{} ev={:#x} now={:#x} last={:#x} gen={}/{}]", fd, e.events, now, e.last, g, e.last_gen));
        }
        s
    }

    pub fn readable(&mut self) -> bool {
        !self.collect(1, false).is_empty()
    }
}

fn epoll_of(fd: i64) -> Result<EpollRef, i64> {
    let f = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
    let b = f.borrow();
    match &b.kind {
        Kind::Epoll(e) => Ok(e.clone()),
        _ => Err(-EINVAL),
    }
}

pub fn create1(flags: u64) -> Result<i64, i64> {
    const EPOLL_CLOEXEC: u64 = 0o2000000;
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(-EINVAL);
    }
    let f = file::new(Kind::Epoll(Rc::new(RefCell::new(Epoll::default()))), file::O_RDWR);
    let fd = proc::current().files().add(f, flags & EPOLL_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

pub fn ctl(epfd: i64, op: u64, fd: i64, ev: usize) -> Result<i64, i64> {
    let ep = epoll_of(epfd)?;
    let target = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
    if matches!(target.borrow().kind, Kind::Epoll(ref e) if Rc::ptr_eq(e, &ep)) {
        return Err(-ELOOP);
    }
    let (events, data) = if op == EPOLL_CTL_DEL {
        (0, 0)
    } else {
        let mut b = [0u8; EVENT_SIZE];
        proc::current().pt().copy_in(&mut b, ev).ok_or(-EFAULT)?;
        (u32::from_le_bytes(b[0..4].try_into().unwrap()), u64::from_le_bytes(b[8..16].try_into().unwrap()))
    };
    let mut ep = ep.borrow_mut();
    let fd = fd as i32;
    // 同じ番号に別のファイルが入っていたら、古い登録は消えたものとみなす
    let stale = ep.entries.get(&fd).is_some_and(|e| !e.file.upgrade().is_some_and(|f| Rc::ptr_eq(&f, &target)));
    if stale {
        ep.entries.remove(&fd);
    }
    match op {
        EPOLL_CTL_ADD => {
            if ep.entries.contains_key(&fd) {
                return Err(-EEXIST);
            }
            file::mark_epolled(&target);
            ep.entries.insert(fd, Entry { file: Rc::downgrade(&target), events, data, last: 0, last_gen: target.borrow().event_gen().wrapping_sub(1) });
        }
        EPOLL_CTL_MOD => {
            let e = ep.entries.get_mut(&fd).ok_or(-ENOENT)?;
            e.events = events;
            e.data = data;
            e.last = 0;
            e.last_gen = target.borrow().event_gen().wrapping_sub(1);
        }
        EPOLL_CTL_DEL => {
            ep.entries.remove(&fd).ok_or(-ENOENT)?;
        }
        _ => return Err(-EINVAL),
    }
    let _ = EPOLLPRI;
    proc::wakeup(proc::poll_chan());
    Ok(0)
}

/// epoll_pwait (timeout はミリ秒、-1 で無限) / epoll_pwait2 (timespec)
pub fn pwait(epfd: i64, events: usize, max: i64, timeout_ticks: Option<u64>) -> Result<i64, i64> {
    if max <= 0 {
        return Err(-EINVAL);
    }
    let ep = epoll_of(epfd)?;
    let deadline = timeout_ticks.map(|t| crate::timer::ticks() + t);
    loop {
        let got = ep.borrow_mut().collect(max as usize, true);
        if !got.is_empty() {
            let mut buf = alloc::vec![0u8; got.len() * EVENT_SIZE];
            for (i, (ev, data)) in got.iter().enumerate() {
                buf[i * EVENT_SIZE..i * EVENT_SIZE + 4].copy_from_slice(&ev.to_le_bytes());
                buf[i * EVENT_SIZE + 8..i * EVENT_SIZE + 16].copy_from_slice(&data.to_le_bytes());
            }
            proc::current().pt().copy_out(events, &buf).ok_or(-EFAULT)?;
            return Ok(got.len() as i64);
        }
        match deadline {
            Some(d) if crate::timer::ticks() >= d => return Ok(0),
            Some(d) => {
                proc::sleep_until(proc::poll_chan(), d)?;
            }
            None => proc::sleep(proc::poll_chan())?,
        }
    }
}

/// epoll_pwait2: 待つ時間は timespec (NULL なら無限)
pub fn pwait2(epfd: i64, events: usize, max: i64, ts: usize) -> Result<i64, i64> {
    let t = if ts == 0 {
        None
    } else {
        let mut b = [0u8; 16];
        proc::current().pt().copy_in(&mut b, ts).ok_or(-EFAULT)?;
        let ns = u64::from_le_bytes(b[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(b[8..].try_into().unwrap());
        Some((ns * crate::timer::HZ).div_ceil(1_000_000_000))
    };
    pwait(epfd, events, max, t)
}

/// ミリ秒 (負なら無限) を tick に
pub fn ms_to_ticks(ms: i64) -> Option<u64> {
    (ms >= 0).then(|| (ms as u64 * crate::timer::HZ).div_ceil(1000))
}

// ---- eventfd ----

pub struct EventFd {
    count: u64,
    semaphore: bool,
    /// 書かれた回数
    generation: u64,
}

pub fn generation(e: &EventFdRef) -> u64 {
    e.borrow().generation
}

pub type EventFdRef = Rc<RefCell<EventFd>>;

pub fn eventfd2(initval: u64, flags: u64) -> Result<i64, i64> {
    const EFD_SEMAPHORE: u64 = 1;
    const EFD_CLOEXEC: u64 = 0o2000000;
    const EFD_NONBLOCK: u64 = 0o4000;
    if flags & !(EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK) != 0 {
        return Err(-EINVAL);
    }
    let e = Rc::new(RefCell::new(EventFd { count: initval & 0xffff_ffff, semaphore: flags & EFD_SEMAPHORE != 0, generation: 0 }));
    let fl = file::O_RDWR | if flags & EFD_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 };
    let fd = proc::current().files().add(file::new(Kind::EventFd(e), fl), flags & EFD_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

fn chan(e: &EventFdRef) -> usize {
    Rc::as_ptr(e) as usize
}

pub fn read(e: &EventFdRef, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if dst.len() < 8 {
        return Err(-EINVAL);
    }
    loop {
        {
            let mut ev = e.borrow_mut();
            if ev.count > 0 {
                let v = if ev.semaphore { 1 } else { ev.count };
                ev.count -= v;
                dst[..8].copy_from_slice(&v.to_le_bytes());
                proc::wakeup(chan(e));
                proc::wakeup(proc::poll_chan());
                return Ok(8);
            }
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        proc::sleep(chan(e))?;
    }
}

pub fn write(e: &EventFdRef, src: &[u8], nonblock: bool) -> Result<usize, i64> {
    if src.len() < 8 {
        return Err(-EINVAL);
    }
    let v = u64::from_le_bytes(src[..8].try_into().unwrap());
    if v == u64::MAX {
        return Err(-EINVAL);
    }
    loop {
        {
            let mut ev = e.borrow_mut();
            if ev.count.checked_add(v).is_some_and(|n| n < u64::MAX) {
                ev.count += v;
                ev.generation += 1;
                proc::wakeup(chan(e));
                proc::wakeup(proc::poll_chan());
                return Ok(8);
            }
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        proc::sleep(chan(e))?;
    }
}

pub fn readiness(e: &EventFdRef) -> (bool, bool, bool) {
    let ev = e.borrow();
    (ev.count > 0, ev.count < u64::MAX - 1, false)
}
