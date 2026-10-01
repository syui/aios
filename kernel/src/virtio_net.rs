// virtio-net。受信用に 16 個のバッファを置いておき、送信は空いた記述子を使う
use crate::kalloc;
use crate::memlayout::{v2p, PGSIZE};
use crate::virtio::{self, Mmio, Queue, DESC_WRITE};
use alloc::vec::Vec;

const DEVICE_NET: u32 = 1;
const F_MAC: u64 = 1 << 5;

const RX: u32 = 0;
const TX: u32 = 1;
const QSIZE: usize = 16;
const BUF: usize = 2048;
/// struct virtio_net_hdr (VERSION_1 では num_buffers までの 12 バイト)
const HDR: usize = 12;

pub struct VirtioNet {
    pub mmio: Mmio,
    pub mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_bufs: Vec<*mut u8>,
    tx_bufs: Vec<*mut u8>,
    tx_free: Vec<u16>,
}

fn bufs(n: usize) -> Option<Vec<*mut u8>> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n.div_ceil(PGSIZE / BUF) {
        let p = kalloc::alloc()?;
        for k in 0..PGSIZE / BUF {
            if v.len() < n {
                v.push(unsafe { p.add(k * BUF) });
            }
        }
    }
    Some(v)
}

impl VirtioNet {
    pub fn probe() -> Option<VirtioNet> {
        let (mmio, feats) = virtio::probe(DEVICE_NET, F_MAC)?;
        let mut mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        if feats & F_MAC != 0 {
            for (i, m) in mac.iter_mut().enumerate() {
                *m = mmio.config8(i);
            }
        }
        let rx = Queue::new(&mmio, RX, QSIZE)?;
        let tx = Queue::new(&mmio, TX, QSIZE)?;
        let mut n = VirtioNet { mmio, mac, rx, tx, rx_bufs: bufs(QSIZE)?, tx_bufs: bufs(QSIZE)?, tx_free: (0..QSIZE as u16).collect() };
        virtio::ready(&n.mmio);
        for i in 0..QSIZE {
            n.post_rx(i as u16);
        }
        n.rx.notify(&n.mmio);
        Some(n)
    }

    fn post_rx(&mut self, i: u16) {
        let addr = v2p(self.rx_bufs[i as usize] as usize) as u64;
        *self.rx.desc(i as usize) = virtio::Desc { addr, len: BUF as u32, flags: DESC_WRITE, next: 0 };
        self.rx.push(i);
    }

    /// 届いたフレームを 1 つ
    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let (id, len) = self.rx.pop_used()?;
        let len = (len as usize).min(BUF);
        let buf = unsafe { core::slice::from_raw_parts(self.rx_bufs[id as usize], len) };
        let frame = buf.get(HDR..).unwrap_or(&[]).to_vec();
        self.post_rx(id);
        self.rx.notify(&self.mmio);
        Some(frame)
    }

    fn reclaim(&mut self) {
        while let Some((id, _)) = self.tx.pop_used() {
            self.tx_free.push(id);
        }
    }

    pub fn can_send(&mut self) -> bool {
        self.reclaim();
        !self.tx_free.is_empty()
    }

    /// len バイトのフレームを f で書いて送る
    pub fn send<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        self.reclaim();
        let id = self.tx_free.pop().expect("virtio-net: no tx buffer");
        let len = len.min(BUF - HDR);
        let buf = unsafe { core::slice::from_raw_parts_mut(self.tx_bufs[id as usize], HDR + len) };
        buf[..HDR].fill(0);
        let r = f(&mut buf[HDR..]);
        let addr = v2p(buf.as_ptr() as usize) as u64;
        *self.tx.desc(id as usize) = virtio::Desc { addr, len: (HDR + len) as u32, flags: 0, next: 0 };
        self.tx.push(id);
        self.tx.notify(&self.mmio);
        r
    }
}
