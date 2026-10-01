// virtio-blk。1 CPU でカーネル内は割り込みを止めているので、要求を出したら終わるまで待つ (ポーリング)
use crate::memlayout::v2p;
use crate::virtio::{self, Mmio, Queue, DESC_NEXT, DESC_WRITE};

const DEVICE_BLOCK: u32 = 2;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;

pub const SECTOR: usize = 512;

#[repr(C)]
struct ReqHeader {
    typ: u32,
    reserved: u32,
    sector: u64,
}

struct Disk {
    mmio: Mmio,
    q: Queue,
    capacity: u64,
}

static mut DISK: Option<Disk> = None;

/// 見つかったら true
pub fn init() -> bool {
    let Some((mmio, _)) = virtio::probe(DEVICE_BLOCK, 0) else { return false };
    let Some(q) = Queue::new(&mmio, 0, 8) else { return false };
    virtio::ready(&mmio);
    let capacity: u64 = mmio.config(0);
    println!("virtio-blk: {} MiB", capacity * SECTOR as u64 / (1024 * 1024));
    unsafe { *(&raw mut DISK) = Some(Disk { mmio, q, capacity }) };
    true
}

/// ディスクのセクタ数
pub fn capacity() -> Option<u64> {
    unsafe { (*(&raw const DISK)).as_ref().map(|d| d.capacity) }
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
    *d.q.desc(0) = virtio::Desc { addr: v2p(&hdr as *const _ as usize) as u64, len: size_of::<ReqHeader>() as u32, flags: DESC_NEXT, next: 1 };
    *d.q.desc(1) = virtio::Desc {
        addr: v2p(buf as usize) as u64,
        len: len as u32,
        flags: DESC_NEXT | if write { 0 } else { DESC_WRITE },
        next: 2,
    };
    *d.q.desc(2) = virtio::Desc { addr: v2p(&mut status as *mut u8 as usize) as u64, len: 1, flags: DESC_WRITE, next: 0 };
    d.q.push(0);
    d.q.notify(&d.mmio);
    // 終わるまで待つ
    while d.q.pop_used().is_none() {
        core::hint::spin_loop();
    }
    d.mmio.ack();
    if unsafe { core::ptr::read_volatile(&status) } != 0 {
        return Err(-EIO);
    }
    Ok(())
}

pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    rw(sector, buf.as_mut_ptr(), buf.len(), false)
}

pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    rw(sector, buf.as_ptr() as *mut u8, buf.len(), true)
}
