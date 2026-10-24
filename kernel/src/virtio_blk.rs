// virtio-blk。要求は 1 度に 1 つ (記述子 0 が頭、そのあとに中身のかたまり (ページなど) を 1 つ以上、最後が答え)。
// キューは 256 (デバイスがそれより小さければ、入るだけ): 1 つの要求に 254 ページまで (デバイスの seg_max まで)、写さずに
// そのまま渡せる (Linux の scatter-gather と同じ。前はキューが 8 で、ext4 の書き出しは 1 MiB のバッファに写してから
// 書いた)。デバイスが seg_max を言わなければ、Linux と同じく 1 つずつ (写して書く)。
// 終わるのを待つのは 2 通り:
//   ふつう: 大きなロックを持ったまま、終わるまで回る (ポーリング)
//   read_wait / write_wait (眠ってよいと分かっているところ、ext4 の読むだけの道と眠って書く書き出し): 眠って、
//     割り込みで起こしてもらう。
//     眠っているあいだ、大きなロックはほかの CPU が使える (キャッシュにないファイルを読むあいだ、
//     ほかのシステムコールが止まっていた)
// 眠っている人の要求が終わる前に、回る人 (眠れないところ) が来たら、その人が終わりを見とどけて起こす。
// 割り込みが来なくても止まらないように、タイマ (cpu0) でも終わりを見る
use crate::cell::Bkl;
use crate::memlayout::v2p;
use crate::virtio::{self, Mmio, Queue, DESC_NEXT, DESC_WRITE};

const DEVICE_BLOCK: u32 = 2;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
/// 1 つの要求のかたまりの数の上限 (設定の 12 バイトめの seg_max) を言う
const F_SEG_MAX: u64 = 1 << 2;

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
    /// 1 つの要求に入れる中身のかたまりの数の上限 (キューの大きさ - 2 と、デバイスの seg_max の小さいほう)
    segs: usize,
    /// 出したまま終わっていない要求があるか
    busy: bool,
    /// その要求の「終わった」印 (出した人のスタックの上。終わったら true にして起こす)
    done: *mut bool,
}

/// 眠って待つ人を起こす印 (要求が終わった、または空いた)
fn chan() -> usize {
    &DISK as *const _ as usize
}

static DISK: Bkl<Option<Disk>> = Bkl::new(None);

/// 見つかったら true
pub fn init() -> bool {
    let Some((mmio, feats)) = virtio::probe(DEVICE_BLOCK, F_SEG_MAX) else { return false };
    let max = Queue::max(&mmio, 0);
    let Some(size) = [256, 128, 64, 32, 16, 8].into_iter().find(|&n| n <= max) else { return false };
    let Some(q) = Queue::new(&mmio, 0, size) else { return false };
    virtio::ready(&mmio);
    let capacity = mmio.config64(0);
    let seg_max = if feats & F_SEG_MAX != 0 { mmio.config32(12) as usize } else { 1 };
    let segs = seg_max.clamp(1, size - 2);
    println!("virtio-blk: {} MiB, {} segments a request", capacity * SECTOR as u64 / (1024 * 1024), segs);
    crate::irq::enable(mmio.irq);
    *DISK.get() = Some(Disk { mmio, q, capacity, segs, busy: false, done: core::ptr::null_mut() });
    true
}

/// ディスクのセクタ数
pub fn capacity() -> Option<u64> {
    DISK.get().as_ref().map(|d| d.capacity)
}

#[track_caller]
fn disk() -> &'static mut Disk {
    DISK.get().as_mut().expect("no disk")
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
    let Some(d) = DISK.get().as_mut() else { return false };
    if d.mmio.irq != irq {
        return false;
    }
    d.mmio.ack();
    complete(d);
    true
}

/// タイマから: 割り込みが来なかったときのため、終わっているものを見とどける
pub fn poll() {
    if let Some(d) = DISK.get().as_mut() {
        complete(d);
    }
}

/// 1 つの要求に入れられる中身のかたまりの数 (頭と答えの記述子をのぞく)
pub fn max_segs() -> usize {
    DISK.get().as_ref().map_or(1, |d| d.segs)
}

/// buf (カーネルのメモリ、SECTOR の倍数) と sector から読み書きする。sleep なら眠って待つ
fn rw(sector: u64, buf: *mut u8, len: usize, write: bool, sleep: bool) -> Result<(), i64> {
    rw_segs(sector, &[(buf, len)], write, sleep)
}

/// segs (カーネルのメモリのかたまり (はじめ, 長さ)。あわせて SECTOR の倍数、max_segs こまで) をつづけて、sector から
/// 読み書きする
fn rw_segs(sector: u64, segs: &[(*mut u8, usize)], write: bool, sleep: bool) -> Result<(), i64> {
    const EIO: i64 = 5;
    let d = disk();
    let len: usize = segs.iter().map(|s| s.1).sum();
    if segs.is_empty() || segs.len() > d.segs || len % SECTOR != 0 || sector + (len / SECTOR) as u64 > d.capacity {
        return Err(-EIO);
    }
    let t0 = crate::timer::uptime_ns();
    // ほかの人の要求が出ている: 眠れるなら空くまで眠る。眠れなければ、その終わりを見とどける (回って)。
    // 眠るのはシグナルで起きない眠り (sleep_nointr): シグナルで戻ると、大きなロックを持ったまま回り、終わりを受ける
    // 割り込みもタイマもロックを待って止まる
    while d.busy {
        if sleep {
            crate::proc::sleep_nointr(chan());
        } else if !complete(d) {
            core::hint::spin_loop();
        }
    }
    let hdr = ReqHeader { typ: if write { T_OUT } else { T_IN }, reserved: 0, sector };
    let mut status: u8 = 0xff;
    let mut done = false;
    *d.q.desc(0) = virtio::Desc { addr: v2p(&hdr as *const _ as usize) as u64, len: size_of::<ReqHeader>() as u32, flags: DESC_NEXT, next: 1 };
    for (i, &(buf, n)) in segs.iter().enumerate() {
        *d.q.desc(1 + i) = virtio::Desc {
            addr: v2p(buf as usize) as u64,
            len: n as u32,
            flags: DESC_NEXT | if write { 0 } else { DESC_WRITE },
            next: (2 + i) as u16,
        };
    }
    let k = 1 + segs.len();
    *d.q.desc(k) = virtio::Desc { addr: v2p(&mut status as *mut u8 as usize) as u64, len: 1, flags: DESC_WRITE, next: 0 };
    d.busy = true;
    d.done = &mut done;
    d.q.push(0);
    d.q.notify(&d.mmio);
    // 終わるまで待つ (done は complete がつける: 割り込み、タイマ、回っているほかの人、自分)
    while !unsafe { core::ptr::read_volatile(&done) } {
        if sleep {
            crate::proc::sleep_nointr(chan());
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

/// 眠って待つ write (ext4 の、眠って書く書き出し: fsync や sync。buf は書き終わるまで呼ぶ側のもの)
pub fn write_wait(sector: u64, buf: &[u8]) -> Result<(), i64> {
    rw(sector, buf.as_ptr() as *mut u8, buf.len(), true, true)
}

/// 眠って待つ write で、pages (each バイトずつ、max_segs こまで) をつづけて sector から (写さずに)
pub fn write_pages_wait(sector: u64, pages: &[*const u8], each: usize) -> Result<(), i64> {
    let segs: alloc::vec::Vec<(*mut u8, usize)> = pages.iter().map(|&p| (p as *mut u8, each)).collect();
    rw_segs(sector, &segs, true, true)
}
