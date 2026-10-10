// virtio-blk。要求は 1 度に 1 つ (記述子 0..2)。終わるのを待つのは 2 通り:
//   ふつう: 大きなロックを持ったまま、終わるまで回る (ポーリング)
//   read_wait (眠ってよいと分かっているところ、ext4 の読むだけの道): 眠って、割り込みで起こしてもらう。
//     眠っているあいだ、大きなロックはほかの CPU が使える (キャッシュにないファイルを読むあいだ、
//     ほかのシステムコールが止まっていた)
// 眠っている人の要求が終わる前に、回る人 (眠れないところ) が来たら、その人が終わりを見とどけて起こす。
// 割り込みが来なくても止まらないように、タイマ (cpu0) でも終わりを見る
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
    /// 出したまま終わっていない要求があるか
    busy: bool,
    /// その要求の「終わった」印 (出した人のスタックの上。終わったら true にして起こす)
    done: *mut bool,
}

/// 眠って待つ人を起こす印 (要求が終わった、または空いた)
fn chan() -> usize {
    (&raw const DISK) as usize
}

static mut DISK: Option<Disk> = None;

/// 見つかったら true
pub fn init() -> bool {
    let Some((mmio, _)) = virtio::probe(DEVICE_BLOCK, 0) else { return false };
    let Some(q) = Queue::new(&mmio, 0, 8) else { return false };
    virtio::ready(&mmio);
    let capacity = mmio.config64(0);
    println!("virtio-blk: {} MiB", capacity * SECTOR as u64 / (1024 * 1024));
    crate::irq::enable(mmio.irq);
    unsafe { *(&raw mut DISK) = Some(Disk { mmio, q, capacity, busy: false, done: core::ptr::null_mut() }) };
    true
}

/// ディスクのセクタ数
pub fn capacity() -> Option<u64> {
    unsafe { (*(&raw const DISK)).as_ref().map(|d| d.capacity) }
}

fn disk() -> &'static mut Disk {
    unsafe { (*(&raw mut DISK)).as_mut().expect("no disk") }
}

/// 眠って待った読みの数 (/proc/bkl)
pub static SLEPT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 出ている要求が終わっていれば、印をつけて起こす (大きなロックを持って)。終わったものがあれば true
fn complete(d: &mut Disk) -> bool {
    if !d.busy || d.q.pop_used().is_none() {
        return false;
    }
    d.busy = false;
    if !d.done.is_null() {
        unsafe { *d.done = true };
        d.done = core::ptr::null_mut();
    }
    crate::proc::wakeup(chan());
    true
}

/// 割り込みの番号がこれなら受けて true
pub fn intr(irq: u32) -> bool {
    let Some(d) = (unsafe { (*(&raw mut DISK)).as_mut() }) else { return false };
    if d.mmio.irq != irq {
        return false;
    }
    d.mmio.ack();
    complete(d);
    true
}

/// タイマから: 割り込みが来なかったときのため、終わっているものを見とどける
pub fn poll() {
    if let Some(d) = unsafe { (*(&raw mut DISK)).as_mut() } {
        complete(d);
    }
}

/// buf (カーネルのメモリ、SECTOR の倍数) と sector から読み書きする。sleep なら眠って待つ
fn rw(sector: u64, buf: *mut u8, len: usize, write: bool, sleep: bool) -> Result<(), i64> {
    const EIO: i64 = 5;
    let d = disk();
    if len % SECTOR != 0 || sector + (len / SECTOR) as u64 > d.capacity {
        return Err(-EIO);
    }
    let t0 = crate::timer::uptime_ns();
    // ほかの人の要求が出ている: 眠れるなら空くまで眠る。眠れなければ、その終わりを見とどける (回って)
    while d.busy {
        if sleep {
            if crate::proc::sleep(chan()).is_err() {
                // シグナルでも、ディスクの読みはやめない (空くまで待つ)
            }
        } else if !complete(d) {
            core::hint::spin_loop();
        }
    }
    let hdr = ReqHeader { typ: if write { T_OUT } else { T_IN }, reserved: 0, sector };
    let mut status: u8 = 0xff;
    let mut done = false;
    *d.q.desc(0) = virtio::Desc { addr: v2p(&hdr as *const _ as usize) as u64, len: size_of::<ReqHeader>() as u32, flags: DESC_NEXT, next: 1 };
    *d.q.desc(1) = virtio::Desc {
        addr: v2p(buf as usize) as u64,
        len: len as u32,
        flags: DESC_NEXT | if write { 0 } else { DESC_WRITE },
        next: 2,
    };
    *d.q.desc(2) = virtio::Desc { addr: v2p(&mut status as *mut u8 as usize) as u64, len: 1, flags: DESC_WRITE, next: 0 };
    d.busy = true;
    d.done = &mut done;
    d.q.push(0);
    d.q.notify(&d.mmio);
    // 終わるまで待つ (done は complete がつける: 割り込み、タイマ、回っているほかの人、自分)
    while !unsafe { core::ptr::read_volatile(&done) } {
        if sleep {
            let _ = crate::proc::sleep(chan());
        } else if !complete(d) {
            core::hint::spin_loop();
        }
    }
    crate::smp::dev_wait(0, t0);
    if sleep {
        SLEPT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    if !sleep {
        d.mmio.ack();
    }
    if unsafe { core::ptr::read_volatile(&status) } != 0 {
        return Err(-EIO);
    }
    Ok(())
}

pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    rw(sector, buf.as_mut_ptr(), buf.len(), false, false)
}

/// 眠って待つ read (眠ってよいところだけ: 大きなロックはほかの CPU が使う。RefCell を借りたまま呼ばないこと)
pub fn read_wait(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    rw(sector, buf.as_mut_ptr(), buf.len(), false, true)
}

pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    rw(sector, buf.as_ptr() as *mut u8, buf.len(), true, false)
}
