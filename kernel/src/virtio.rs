// virtio-mmio (version 2) の共通部分: デバイス探し、初期化、virtqueue
use crate::kalloc;
use crate::memlayout::{p2v, v2p, PGSIZE};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

/// qemu virt は PA 0x0a00_0000 から 0x200 おきに 32 個のスロット。割り込みは SPI 16 + スロット
/// DTB がないときの場所 (qemu virt)
const MMIO_BASE: usize = 0x0a00_0000;
const MMIO_STRIDE: usize = 0x200;
const MMIO_SLOTS: usize = 32;
const IRQ_BASE: u32 = 32 + 16;

const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
pub const CONFIG: usize = 0x100;

const STATUS_ACK: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

/// VIRTIO_F_VERSION_1 (bit 32)
const F_VERSION_1: u64 = 1 << 32;

pub const DESC_NEXT: u16 = 1;
pub const DESC_WRITE: u16 = 2;

pub struct Mmio {
    pub base: usize,
    pub irq: u32,
}

impl Mmio {
    pub fn rd(&self, off: usize) -> u32 {
        crate::mmio::r32(self.base + off)
    }

    pub fn wr(&self, off: usize, v: u32) {
        crate::mmio::w32(self.base + off, v)
    }

    /// 設定の領域の 1 バイト
    pub fn config_w8(&self, off: usize, v: u8) {
        crate::mmio::w8(self.base + CONFIG + off, v)
    }

    pub fn config8(&self, off: usize) -> u8 {
        crate::mmio::r8(self.base + CONFIG + off)
    }

    /// 設定の領域の 64 ビット (32 ビットずつ、下から)
    pub fn config64(&self, off: usize) -> u64 {
        let a = self.base + CONFIG + off;
        crate::mmio::r32(a) as u64 | (crate::mmio::r32(a + 4) as u64) << 32
    }

    /// 割り込みの理由を読んで下げる
    pub fn ack(&self) -> u32 {
        let isr = self.rd(INTERRUPT_STATUS);
        self.wr(INTERRUPT_ACK, isr);
        isr
    }
}

/// device_id のデバイスを探して、wanted の機能 (VERSION_1 は自動で足す) で初期化する。
/// キューの設定は呼ぶ側が Queue::new で行い、最後に ready を呼ぶ
pub fn probe(device_id: u32, wanted: u64) -> Option<(Mmio, u64)> {
    probe_nth(device_id, wanted, 0)
}

/// device_id の装置の nth 番目 (0 から)。キーボードとタブレットのように、同じ種類がいくつもあるとき
pub fn probe_nth(device_id: u32, wanted: u64, nth: usize) -> Option<(Mmio, u64)> {
    let mut seen = 0;
    // DTB があればそこに書かれたもの (base, GIC の INTID)、なければ qemu virt の決まった場所
    let mut slots = alloc::vec::Vec::new();
    if crate::dtb::present() {
        crate::dtb::each_virtio(|base, spi| slots.push((base as usize, 32 + spi)));
        slots.sort();
    } else {
        slots.extend((0..MMIO_SLOTS).map(|i| (MMIO_BASE + i * MMIO_STRIDE, IRQ_BASE + i as u32)));
    }
    for (pa, irq) in slots {
        let m = Mmio { base: p2v(pa), irq };
        if m.rd(MAGIC) != 0x7472_6976 || m.rd(DEVICE_ID) != device_id {
            continue;
        }
        seen += 1;
        if seen <= nth {
            continue;
        }
        if m.rd(VERSION) != 2 {
            println!("virtio: legacy device (use -global virtio-mmio.force-legacy=false)");
            return None;
        }
        m.wr(STATUS, 0);
        m.wr(STATUS, STATUS_ACK | STATUS_DRIVER);
        m.wr(DEVICE_FEATURES_SEL, 0);
        let lo = m.rd(DEVICE_FEATURES) as u64;
        m.wr(DEVICE_FEATURES_SEL, 1);
        let dev = lo | (m.rd(DEVICE_FEATURES) as u64) << 32;
        if dev & F_VERSION_1 == 0 {
            return None;
        }
        let feats = dev & (wanted | F_VERSION_1);
        m.wr(DRIVER_FEATURES_SEL, 0);
        m.wr(DRIVER_FEATURES, feats as u32);
        m.wr(DRIVER_FEATURES_SEL, 1);
        m.wr(DRIVER_FEATURES, (feats >> 32) as u32);
        m.wr(STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if m.rd(STATUS) & STATUS_FEATURES_OK == 0 {
            return None;
        }
        return Some((m, feats));
    }
    None
}

pub fn ready(m: &Mmio) {
    m.wr(STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
}

#[repr(C)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// 1 本の virtqueue。desc/avail/used はそれぞれ 1 ページ
pub struct Queue {
    idx: u32,
    pub size: usize,
    desc: *mut Desc,
    avail: *mut u16,
    used: *mut u16,
    last_used: u16,
}

impl Queue {
    pub fn new(m: &Mmio, idx: u32, size: usize) -> Option<Queue> {
        m.wr(QUEUE_SEL, idx);
        if (m.rd(QUEUE_NUM_MAX) as usize) < size || size * 16 > PGSIZE {
            return None;
        }
        m.wr(QUEUE_NUM, size as u32);
        let (d, a, u) = (kalloc::alloc()?, kalloc::alloc()?, kalloc::alloc()?);
        for (lo, p) in [(QUEUE_DESC_LOW, d), (QUEUE_DRIVER_LOW, a), (QUEUE_DEVICE_LOW, u)] {
            let pa = v2p(p as usize) as u64;
            m.wr(lo, pa as u32);
            m.wr(lo + 4, (pa >> 32) as u32);
        }
        m.wr(QUEUE_READY, 1);
        Some(Queue { idx, size, desc: d as *mut Desc, avail: a as *mut u16, used: u as *mut u16, last_used: 0 })
    }

    pub fn desc(&mut self, i: usize) -> &mut Desc {
        assert!(i < self.size);
        unsafe { &mut *self.desc.add(i) }
    }

    /// head の鎖をデバイスへ渡す (知らせるのは notify)
    pub fn push(&mut self, head: u16) {
        unsafe {
            let idx = read_volatile(self.avail.add(1));
            write_volatile(self.avail.add(2 + idx as usize % self.size), head);
            fence(Ordering::SeqCst);
            write_volatile(self.avail.add(1), idx.wrapping_add(1));
            fence(Ordering::SeqCst);
        }
    }

    pub fn notify(&self, m: &Mmio) {
        m.wr(QUEUE_NOTIFY, self.idx);
    }

    /// デバイスが使い終わったものを 1 つ: (head, 書かれた長さ)
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        unsafe {
            let used_idx = read_volatile(self.used.add(1));
            if used_idx == self.last_used {
                return None;
            }
            fence(Ordering::SeqCst);
            // used ring の要素は u32 id + u32 len (先頭 4 バイトの後ろ)
            let elem = (self.used as *mut u32).add(1 + 2 * (self.last_used as usize % self.size));
            let id = read_volatile(elem);
            let len = read_volatile(elem.add(1));
            self.last_used = self.last_used.wrapping_add(1);
            Some((id as u16, len))
        }
    }
}
