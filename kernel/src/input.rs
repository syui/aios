// virtio-input (キーボード、タブレット = 絶対座標のマウス) を Linux の evdev と同じ形で見せる
//   /dev/input/eventN (char 13, 64 + N)。読むと struct input_event (24 バイト) が並ぶ
//   ioctl: EVIOCGVERSION, EVIOCGID, EVIOCGNAME, EVIOCGBIT, EVIOCGABS
// 装置は 8 バイトのイベント (type, code, value) を eventq に書いてくる。割り込みで受けて、
// 時刻をつけて溜め、読む人と poll している人を起こす
use crate::memlayout::{v2p, PGSIZE};
use crate::proc;
use crate::virtio::{self, Mmio, Queue, DESC_WRITE};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

const DEVICE_INPUT: u32 = 18;
/// eventq の大きさ (イベント 1 つが 8 バイト)
const QSIZE: usize = 64;
/// 読まれずに溜めておくイベントの数
const KEEP: usize = 4096;
// virtio-input の config の select
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

pub struct Input {
    mmio: Mmio,
    q: Queue,
    bufs: *mut u8,
    pub name: String,
    events: VecDeque<[u8; 24]>,
}

static mut DEVS: Vec<Input> = Vec::new();

fn devs() -> &'static mut Vec<Input> {
    unsafe { &mut *(&raw mut DEVS) }
}

pub fn count() -> usize {
    devs().len()
}

/// 読む人が眠る channel (と poll の印)
pub fn chan(n: usize) -> usize {
    devs().as_ptr() as usize + n * core::mem::size_of::<Input>()
}

impl Input {
    /// config の select / subsel の答え (大きさと中身)
    fn config(&self, select: u8, subsel: u8) -> Vec<u8> {
        self.mmio.config_w8(0, select);
        self.mmio.config_w8(1, subsel);
        let n = self.mmio.config8(2) as usize;
        (0..n.min(128)).map(|i| self.mmio.config8(8 + i)).collect()
    }

    fn post(&mut self, i: usize) {
        *self.q.desc(i) = virtio::Desc { addr: v2p(self.bufs as usize + i * 8) as u64, len: 8, flags: DESC_WRITE, next: 0 };
        self.q.push(i as u16);
    }
}

pub fn init() {
    let mut n = 0;
    while let Some((mmio, _)) = virtio::probe_nth(DEVICE_INPUT, 0, n) {
        n += 1;
        let Some(q) = Queue::new(&mmio, 0, QSIZE) else { continue };
        let Some(bufs) = crate::kalloc::alloc() else { break };
        debug_assert!(QSIZE * 8 <= PGSIZE);
        virtio::ready(&mmio);
        let mut d = Input { mmio, q, bufs, name: String::new(), events: VecDeque::new() };
        d.name = String::from_utf8_lossy(&d.config(CFG_ID_NAME, 0)).trim_end_matches(['\0', ' ']).into();
        for i in 0..QSIZE {
            d.post(i);
        }
        d.q.notify(&d.mmio);
        crate::irq::enable(d.mmio.irq);
        println!("input: /dev/input/event{} {}", devs().len(), d.name);
        devs().push(d);
    }
}

/// 割り込みの番号が input のものなら受けて true
pub fn intr(irq: u32) -> bool {
    let Some(n) = devs().iter().position(|d| d.mmio.irq == irq) else { return false };
    let d = &mut devs()[n];
    d.mmio.ack();
    let ns = crate::timer::epoch_ns();
    let mut got = false;
    while let Some((id, _)) = d.q.pop_used() {
        let e = unsafe { core::slice::from_raw_parts(d.bufs.add(id as usize * 8), 8) };
        let mut ev = [0u8; 24];
        ev[0..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        ev[8..16].copy_from_slice(&((ns % 1_000_000_000) / 1000).to_le_bytes());
        ev[16..24].copy_from_slice(e);
        if d.events.len() >= KEEP {
            d.events.pop_front();
        }
        d.events.push_back(ev);
        d.post(id as usize);
        got = true;
    }
    d.q.notify(&d.mmio);
    if got {
        proc::wakeup(chan(n));
        proc::poll_wake(chan(n));
    }
    true
}

pub fn readable(n: usize) -> bool {
    devs().get(n).is_some_and(|d| !d.events.is_empty())
}

/// 溜まっているイベントを dst へ (24 バイトずつ)。なければ待つ (nonblock なら EAGAIN)
pub fn read(n: usize, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    const EAGAIN: i64 = 11;
    const EINVAL: i64 = 22;
    if dst.len() < 24 {
        return Err(-EINVAL);
    }
    loop {
        let d = devs().get_mut(n).ok_or(-19)?;
        if !d.events.is_empty() {
            let mut k = 0;
            while k + 24 <= dst.len() {
                let Some(ev) = d.events.pop_front() else { break };
                dst[k..k + 24].copy_from_slice(&ev);
                k += 24;
            }
            return Ok(k);
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        proc::sleep(chan(n))?;
    }
}

/// evdev の ioctl
pub fn ioctl(n: usize, req: u64, arg: usize) -> Result<i64, i64> {
    const ENOTTY: i64 = 25;
    const EFAULT: i64 = 14;
    let d = devs().get(n).ok_or(-19)?;
    let out = |b: &[u8]| proc::current().pt().copy_out(arg, b).ok_or(-EFAULT);
    let (dir, typ, nr, size) = ((req >> 30) & 3, (req >> 8) & 0xff, req & 0xff, ((req >> 16) & 0x3fff) as usize);
    if typ != b'E' as u64 || dir != 2 {
        return Err(-ENOTTY);
    }
    match nr {
        // EVIOCGVERSION
        0x01 => out(&0x010001u32.to_le_bytes()).map(|_| 0),
        // EVIOCGID: bustype, vendor, product, version (u16 4 つ)
        0x02 => {
            let mut id = d.config(CFG_ID_DEVIDS, 0);
            id.resize(8, 0);
            out(&id).map(|_| 0)
        }
        // EVIOCGNAME(len)
        0x06 => {
            let mut b = d.name.as_bytes().to_vec();
            b.push(0);
            b.truncate(size);
            out(&b).map(|_| b.len() as i64)
        }
        // EVIOCGBIT(ev, len): ev 0 はどの種類のイベントがあるか
        0x20..=0x3f => {
            let ev = (nr - 0x20) as u8;
            let mut bits = if ev == 0 {
                let mut b = alloc::vec![0u8; 4];
                for t in 1..32u8 {
                    if !d.config(CFG_EV_BITS, t).is_empty() {
                        b[t as usize / 8] |= 1 << (t % 8);
                    }
                }
                // EV_SYN はいつも
                b[0] |= 1;
                b
            } else {
                d.config(CFG_EV_BITS, ev)
            };
            bits.resize(size, 0);
            out(&bits).map(|_| size as i64)
        }
        // EVIOCGABS(axis): value, min, max, fuzz, flat, resolution
        0x40..=0x7f if size == 24 => {
            let a = d.config(CFG_ABS_INFO, (nr - 0x40) as u8);
            let u = |o: usize| a.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            let mut b = [0u8; 24];
            for (k, v) in [0, u(0), u(4), u(8), u(12), u(16)].iter().enumerate() {
                b[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            out(&b).map(|_| 0)
        }
        _ => Err(-ENOTTY),
    }
}
