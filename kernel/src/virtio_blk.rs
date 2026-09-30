// virtio-blk (virtio-mmio version 2)。qemu virt は PA 0x0a00_0000 から 0x200 おきに 32 個のスロットを持つ
//
// 1 CPU でカーネル内は割り込みを止めているので、要求を出したら終わるまで待つ (ポーリング)
use crate::kalloc;
use crate::memlayout::{v2p, KBASE};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const MMIO_BASE: usize = KBASE + 0x0a00_0000;
const MMIO_STRIDE: usize = 0x200;
const MMIO_SLOTS: usize = 32;

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
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG: usize = 0x100;

const STATUS_ACK: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

const DEVICE_BLOCK: u32 = 2;
/// VIRTIO_F_VERSION_1 (bit 32)
const F_VERSION_1: u32 = 1 << 0;

const QSIZE: usize = 8;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;

pub const SECTOR: usize = 512;

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QSIZE],
}

#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QSIZE],
}

#[repr(C)]
struct ReqHeader {
    typ: u32,
    reserved: u32,
    sector: u64,
}

struct Disk {
    base: usize,
    desc: *mut Desc,
    avail: *mut Avail,
    used: *mut Used,
    last_used: u16,
    capacity: u64,
}

static mut DISK: Option<Disk> = None;

fn reg(base: usize, off: usize) -> *mut u32 {
    (base + off) as *mut u32
}

fn rd(base: usize, off: usize) -> u32 {
    unsafe { read_volatile(reg(base, off)) }
}

fn wr(base: usize, off: usize, v: u32) {
    unsafe { write_volatile(reg(base, off), v) }
}

/// 見つかったら true
pub fn init() -> bool {
    for i in 0..MMIO_SLOTS {
        let base = MMIO_BASE + i * MMIO_STRIDE;
        if rd(base, MAGIC) != 0x7472_6976 || rd(base, DEVICE_ID) != DEVICE_BLOCK {
            continue;
        }
        if rd(base, VERSION) != 2 {
            println!("virtio-blk: legacy device (use -global virtio-mmio.force-legacy=false)");
            return false;
        }
        return setup(base);
    }
    false
}

fn setup(base: usize) -> bool {
    wr(base, STATUS, 0);
    let mut status = STATUS_ACK | STATUS_DRIVER;
    wr(base, STATUS, status);

    // VERSION_1 だけを使う
    wr(base, DEVICE_FEATURES_SEL, 1);
    if rd(base, DEVICE_FEATURES) & F_VERSION_1 == 0 {
        return false;
    }
    wr(base, DRIVER_FEATURES_SEL, 0);
    wr(base, DRIVER_FEATURES, 0);
    wr(base, DRIVER_FEATURES_SEL, 1);
    wr(base, DRIVER_FEATURES, F_VERSION_1);
    status |= STATUS_FEATURES_OK;
    wr(base, STATUS, status);
    if rd(base, STATUS) & STATUS_FEATURES_OK == 0 {
        println!("virtio-blk: features not accepted");
        return false;
    }

    wr(base, QUEUE_SEL, 0);
    if (rd(base, QUEUE_NUM_MAX) as usize) < QSIZE {
        return false;
    }
    wr(base, QUEUE_NUM, QSIZE as u32);
    let (Some(d), Some(a), Some(u)) = (kalloc::alloc(), kalloc::alloc(), kalloc::alloc()) else { return false };
    for (lo, hi, p) in [(QUEUE_DESC_LOW, QUEUE_DESC_HIGH, d), (QUEUE_DRIVER_LOW, QUEUE_DRIVER_HIGH, a), (QUEUE_DEVICE_LOW, QUEUE_DEVICE_HIGH, u)] {
        let pa = v2p(p as usize) as u64;
        wr(base, lo, pa as u32);
        wr(base, hi, (pa >> 32) as u32);
    }
    wr(base, QUEUE_READY, 1);
    status |= STATUS_DRIVER_OK;
    wr(base, STATUS, status);

    let capacity = unsafe { read_volatile((base + CONFIG) as *const u64) };
    unsafe {
        *(&raw mut DISK) = Some(Disk { base, desc: d as *mut Desc, avail: a as *mut Avail, used: u as *mut Used, last_used: 0, capacity });
    }
    println!("virtio-blk: {} MiB", capacity * SECTOR as u64 / (1024 * 1024));
    true
}

fn disk() -> &'static mut Disk {
    unsafe { (*(&raw mut DISK)).as_mut().expect("no disk") }
}

/// buf (カーネルのメモリ、SECTOR の倍数) と sector から読み書きする
fn rw(sector: u64, buf: *mut u8, len: usize, write: bool) -> Result<(), i64> {
    const EIO: i64 = 5;
    let d = disk();
    if len % SECTOR != 0 || sector + (len / SECTOR) as u64 > d.capacity {
        return Err(-EIO);
    }
    let hdr = ReqHeader { typ: if write { T_OUT } else { T_IN }, reserved: 0, sector };
    let mut status: u8 = 0xff;
    unsafe {
        let desc = core::slice::from_raw_parts_mut(d.desc, QSIZE);
        desc[0] = Desc { addr: v2p(&hdr as *const _ as usize) as u64, len: size_of::<ReqHeader>() as u32, flags: DESC_NEXT, next: 1 };
        desc[1] = Desc {
            addr: v2p(buf as usize) as u64,
            len: len as u32,
            flags: DESC_NEXT | if write { 0 } else { DESC_WRITE },
            next: 2,
        };
        desc[2] = Desc { addr: v2p(&mut status as *mut u8 as usize) as u64, len: 1, flags: DESC_WRITE, next: 0 };

        let avail = &mut *d.avail;
        let idx = read_volatile(&avail.idx);
        write_volatile(&mut avail.ring[idx as usize % QSIZE], 0);
        fence(Ordering::SeqCst);
        write_volatile(&mut avail.idx, idx.wrapping_add(1));
        fence(Ordering::SeqCst);
        wr(d.base, QUEUE_NOTIFY, 0);

        // 終わるまで待つ
        while read_volatile(&(*d.used).idx) == d.last_used {
            core::hint::spin_loop();
        }
        fence(Ordering::SeqCst);
        d.last_used = d.last_used.wrapping_add(1);
        let isr = rd(d.base, INTERRUPT_STATUS);
        wr(d.base, INTERRUPT_ACK, isr);
        if read_volatile(&status) != 0 {
            return Err(-EIO);
        }
    }
    Ok(())
}

pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    rw(sector, buf.as_mut_ptr(), buf.len(), false)
}

pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    rw(sector, buf.as_ptr() as *mut u8, buf.len(), true)
}
