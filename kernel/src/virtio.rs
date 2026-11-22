// virtio の共通部分: デバイス探し、初期化、virtqueue。つなぎかたは virtio-mmio (version 2) と virtio-pci (modern:
// PCI の vendor の capability が指す BAR の中の共通の設定、知らせ、ISR、装置の設定。Linux の virtio_pci_modern.c)。
// 探すのは virtio-mmio の口が先、そのあと PCI (pci.rs が見つけて BAR を割り当てたもの)
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
    /// virtio-mmio の口の番地 (virtio-pci なら共通の設定の番地)
    pub base: usize,
    /// 割り込み (GIC の INTID。virtio-pci は INTx の線: 分けあうことがある)
    pub irq: u32,
    /// virtio-pci なら、その capability の場所
    pci: Option<Pci>,
}

/// virtio-pci の capability が指すところ (写した仮想番地)
#[derive(Clone, Copy)]
struct Pci {
    common: usize,
    notify: usize,
    mul: u32,
    isr: usize,
    device: usize,
}

/// virtio-pci の共通の設定 (struct virtio_pci_common_cfg)
const PCI_DFSELECT: usize = 0x00;
const PCI_DF: usize = 0x04;
const PCI_GFSELECT: usize = 0x08;
const PCI_GF: usize = 0x0c;
const PCI_STATUS: usize = 0x14;
const PCI_Q_SELECT: usize = 0x16;
const PCI_Q_SIZE: usize = 0x18;
const PCI_Q_MSIX: usize = 0x1a;
const PCI_Q_ENABLE: usize = 0x1c;
const PCI_Q_NOFF: usize = 0x1e;
const PCI_Q_DESC: usize = 0x20;
const PCI_Q_AVAIL: usize = 0x28;
const PCI_Q_USED: usize = 0x30;

impl Mmio {
    pub fn rd(&self, off: usize) -> u32 {
        crate::mmio::r32(self.base + off)
    }

    pub fn wr(&self, off: usize, v: u32) {
        crate::mmio::w32(self.base + off, v)
    }

    /// 装置の設定の領域のはじめ
    fn config(&self) -> usize {
        self.pci.map_or(self.base + CONFIG, |p| p.device)
    }

    /// 設定の領域の 1 バイト
    pub fn config_w8(&self, off: usize, v: u8) {
        crate::mmio::w8(self.config() + off, v)
    }

    pub fn config8(&self, off: usize) -> u8 {
        crate::mmio::r8(self.config() + off)
    }

    /// 設定の領域の 32 ビット
    pub fn config32(&self, off: usize) -> u32 {
        crate::mmio::r32(self.config() + off)
    }

    /// 設定の領域の 64 ビット (32 ビットずつ、下から)
    pub fn config64(&self, off: usize) -> u64 {
        let a = self.config() + off;
        crate::mmio::r32(a) as u64 | (crate::mmio::r32(a + 4) as u64) << 32
    }

    /// 割り込みの理由を読んで下げる (virtio-pci は ISR を読むと下がる)
    pub fn ack(&self) -> u32 {
        if let Some(p) = self.pci {
            return crate::mmio::r8(p.isr) as u32;
        }
        let isr = self.rd(INTERRUPT_STATUS);
        self.wr(INTERRUPT_ACK, isr);
        isr
    }

    fn set_status(&self, v: u32) {
        match self.pci {
            Some(p) => crate::mmio::w8(p.common + PCI_STATUS, v as u8),
            None => self.wr(STATUS, v),
        }
    }

    fn status(&self) -> u32 {
        match self.pci {
            Some(p) => crate::mmio::r8(p.common + PCI_STATUS) as u32,
            None => self.rd(STATUS),
        }
    }

    fn device_features(&self) -> u64 {
        let (sel, val) = match self.pci {
            Some(p) => (p.common + PCI_DFSELECT, p.common + PCI_DF),
            None => (self.base + DEVICE_FEATURES_SEL, self.base + DEVICE_FEATURES),
        };
        crate::mmio::w32(sel, 0);
        let lo = crate::mmio::r32(val) as u64;
        crate::mmio::w32(sel, 1);
        lo | (crate::mmio::r32(val) as u64) << 32
    }

    fn set_driver_features(&self, f: u64) {
        let (sel, val) = match self.pci {
            Some(p) => (p.common + PCI_GFSELECT, p.common + PCI_GF),
            None => (self.base + DRIVER_FEATURES_SEL, self.base + DRIVER_FEATURES),
        };
        crate::mmio::w32(sel, 0);
        crate::mmio::w32(val, f as u32);
        crate::mmio::w32(sel, 1);
        crate::mmio::w32(val, (f >> 32) as u32);
    }

    fn select_queue(&self, idx: u32) {
        match self.pci {
            Some(p) => crate::mmio::w16(p.common + PCI_Q_SELECT, idx as u16),
            None => self.wr(QUEUE_SEL, idx),
        }
    }

    fn queue_max(&self) -> u32 {
        match self.pci {
            Some(p) => crate::mmio::r16(p.common + PCI_Q_SIZE) as u32,
            None => self.rd(QUEUE_NUM_MAX),
        }
    }
}

/// device_id のデバイスを探して、wanted の機能 (VERSION_1 は自動で足す) で初期化する。
/// キューの設定は呼ぶ側が Queue::new で行い、最後に ready を呼ぶ
pub fn probe(device_id: u32, wanted: u64) -> Option<(Mmio, u64)> {
    probe_nth(device_id, wanted, 0)
}

/// virtio-mmio の口 (物理番地, GIC の INTID): DTB があればそこに書かれたもの、なければ qemu virt の決まった場所。番地の順
fn slots() -> alloc::vec::Vec<(usize, u32)> {
    let mut slots = alloc::vec::Vec::new();
    if crate::dtb::described() {
        crate::dtb::each_virtio(|base, spi| slots.push((base as usize, 32 + spi)));
        slots.sort();
    } else {
        slots.extend((0..MMIO_SLOTS).map(|i| (MMIO_BASE + i * MMIO_STRIDE, IRQ_BASE + i as u32)));
    }
    slots
}

/// Linux の virtio バスでのその装置の番号 (virtioN: rtnetlink の IFLA_PARENT_DEV_NAME): Linux は DTB の順 (番地の小さい
/// もの) に口を見て、装置のある (DEVICE_ID が 0 でない) ものにだけ番号をつける。base は Mmio の base
pub fn linux_index(base: usize) -> usize {
    slots()
        .into_iter()
        .filter(|&(pa, _)| p2v(pa) < base)
        .filter(|&(pa, _)| {
            let m = Mmio { base: p2v(pa), irq: 0, pci: None };
            m.rd(MAGIC) == 0x7472_6976 && m.rd(DEVICE_ID) != 0
        })
        .count()
}

/// device_id の装置の nth 番目 (0 から)。キーボードとタブレットのように、同じ種類がいくつもあるとき
pub fn probe_nth(device_id: u32, wanted: u64, nth: usize) -> Option<(Mmio, u64)> {
    let mut seen = 0;
    let mut found = None;
    for (pa, irq) in slots() {
        let m = Mmio { base: p2v(pa), irq, pci: None };
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
        found = Some((m, alloc::format!("{:#x}", pa)));
        break;
    }
    if found.is_none() {
        // virtio-pci (pci.rs が見つけたもの。PCI の順)
        for (i, id) in crate::pci::virtio_devices() {
            if id != device_id {
                continue;
            }
            seen += 1;
            if seen <= nth {
                continue;
            }
            found = Some((pci_transport(i)?, crate::pci::devs()[i].name()));
            break;
        }
    }
    let (m, at) = found?;
    m.set_status(0);
    m.set_status(STATUS_ACK | STATUS_DRIVER);
    let dev = m.device_features();
    if dev & F_VERSION_1 == 0 {
        println!("virtio: device {} at {} has no VERSION_1", device_id, at);
        return None;
    }
    let feats = dev & (wanted | F_VERSION_1);
    m.set_driver_features(feats);
    m.set_status(STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
    if m.status() & STATUS_FEATURES_OK == 0 {
        println!("virtio: device {} at {} refused features {:#x}", device_id, at, feats);
        return None;
    }
    Some((m, feats))
}

/// virtio-pci の装置 i をつなぐ: vendor の capability (common 1、notify 2、ISR 3、device 4) の BAR の中を写し、
/// MEM の読み書きと bus master を入れ、INTx の線を引く (Linux の virtio_pci_modern_probe と vp_find_vqs_intx)
fn pci_transport(i: usize) -> Option<Mmio> {
    let (mut common, mut notify, mut mul, mut isr, mut device) = (0, 0, 0, 0, 0);
    for c in crate::pci::vendor_caps(i) {
        let Some(va) = crate::pci::bar_va(i, c.bar as usize, c.offset as u64, c.length as usize) else { continue };
        match c.kind {
            1 if common == 0 => common = va,
            2 if notify == 0 => {
                notify = va;
                mul = c.extra;
            }
            3 if isr == 0 => isr = va,
            4 if device == 0 => device = va,
            _ => {}
        }
    }
    if common == 0 || notify == 0 || isr == 0 {
        println!("virtio: {}: no modern virtio-pci capabilities", crate::pci::devs()[i].name());
        return None;
    }
    let Some(irq) = crate::pci::intx(i) else {
        println!("virtio: {}: no INTx route", crate::pci::devs()[i].name());
        return None;
    };
    crate::pci::enable(i);
    Some(Mmio { base: common, irq, pci: Some(Pci { common, notify, mul, isr, device }) })
}

/// 見えている virtio の装置 (「ID@番地」の並び。モジュールが装置を見つけられないときに出す)
pub fn list() -> alloc::string::String {
    let mut slots = alloc::vec::Vec::new();
    if crate::dtb::described() {
        crate::dtb::each_virtio(|base, _| slots.push(base as usize));
        slots.sort();
    } else {
        slots.extend((0..MMIO_SLOTS).map(|i| MMIO_BASE + i * MMIO_STRIDE));
    }
    let mut s = alloc::string::String::new();
    for pa in slots {
        let m = Mmio { base: p2v(pa), irq: 0, pci: None };
        if m.rd(MAGIC) == 0x7472_6976 && m.rd(DEVICE_ID) != 0 {
            s.push_str(&alloc::format!(" {}@{:#x}", m.rd(DEVICE_ID), pa));
        }
    }
    for (i, id) in crate::pci::virtio_devices() {
        s.push_str(&alloc::format!(" {}@{}", id, crate::pci::devs()[i].name()));
    }
    if s.is_empty() {
        s.push_str(" none");
    }
    s
}

pub fn ready(m: &Mmio) {
    m.set_status(STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
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
    /// virtio-pci の知らせる番地 (notify の BAR + queue_notify_off * 倍数。virtio-mmio は 0)
    notify_at: usize,
    pub size: usize,
    desc: *mut Desc,
    avail: *mut u16,
    used: *mut u16,
    last_used: u16,
}

impl Queue {
    /// キュー idx の入れ物の数の上限 (デバイスの言うもの)
    pub fn max(m: &Mmio, idx: u32) -> usize {
        m.select_queue(idx);
        m.queue_max() as usize
    }

    pub fn new(m: &Mmio, idx: u32, size: usize) -> Option<Queue> {
        m.select_queue(idx);
        if (m.queue_max() as usize) < size || size * 16 > PGSIZE {
            println!("virtio: queue {} has {} entries, {} wanted", idx, m.queue_max(), size);
            return None;
        }
        let (d, a, u) = (kalloc::alloc()?, kalloc::alloc()?, kalloc::alloc()?);
        let mut notify_at = 0;
        match m.pci {
            Some(p) => {
                crate::mmio::w16(p.common + PCI_Q_SIZE, size as u16);
                for (o, q) in [(PCI_Q_DESC, d), (PCI_Q_AVAIL, a), (PCI_Q_USED, u)] {
                    let pa = v2p(q as usize) as u64;
                    crate::mmio::w32(p.common + o, pa as u32);
                    crate::mmio::w32(p.common + o + 4, (pa >> 32) as u32);
                }
                // MSI-X は使わない (INTx)
                crate::mmio::w16(p.common + PCI_Q_MSIX, 0xffff);
                notify_at = p.notify + crate::mmio::r16(p.common + PCI_Q_NOFF) as usize * p.mul as usize;
                crate::mmio::w16(p.common + PCI_Q_ENABLE, 1);
            }
            None => {
                m.wr(QUEUE_NUM, size as u32);
                for (lo, q) in [(QUEUE_DESC_LOW, d), (QUEUE_DRIVER_LOW, a), (QUEUE_DEVICE_LOW, u)] {
                    let pa = v2p(q as usize) as u64;
                    m.wr(lo, pa as u32);
                    m.wr(lo + 4, (pa >> 32) as u32);
                }
                m.wr(QUEUE_READY, 1);
            }
        }
        Some(Queue { idx, notify_at, size, desc: d as *mut Desc, avail: a as *mut u16, used: u as *mut u16, last_used: 0 })
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
        if self.notify_at != 0 {
            crate::mmio::w16(self.notify_at, self.idx as u16);
        } else {
            m.wr(QUEUE_NOTIFY, self.idx);
        }
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
