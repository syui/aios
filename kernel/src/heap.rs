// カーネルヒープ: bss 上の固定領域を first-fit のフリーリストで切り分ける
use crate::spinlock::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// 使っているバイト (ヘッダこみ)、いちばん多かったとき、取れなかった回数 (/proc/ai/heap)
static USED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static FAILS: AtomicUsize = AtomicUsize::new(0);

/// 空きのかたまりの数と、いちばん大きなもの
fn free_blocks(h: &Heap) -> (usize, usize) {
    let (mut n, mut big) = (0, 0);
    let mut cur = h.head;
    while !cur.is_null() {
        unsafe {
            n += 1;
            big = big.max((*cur).size);
            cur = (*cur).next;
        }
    }
    (n, big)
}

// ---- どこで取ったか (調べもの用。/proc/ai/ctl に heap trace on) ----
// 取ったところの呼び出し (フレームポインタで 6 段) ごとに、いま持っているバイトと数を数える。
// どの場所のものかは、ブロックのヘッダの 2 語目 (ふだんは空き) に番号 + 1 を書いておき、返すときに引く

const SITES: usize = 512;
const DEPTH: usize = 6;

#[derive(Clone, Copy)]
struct Site {
    at: [usize; DEPTH],
    bytes: usize,
    count: usize,
}

static mut SITE: [Site; SITES] = [Site { at: [0; DEPTH], bytes: 0, count: 0 }; SITES];
pub static TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 呼び出し (alloc 自身の 2 段は飛ばす)
fn callers() -> [usize; DEPTH] {
    let mut at = [0; DEPTH];
    let mut fp: usize;
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp) };
    let mut i = 0;
    let mut skip = 2;
    while i < DEPTH && fp != 0 && fp % 16 == 0 && fp >= crate::memlayout::KBASE {
        let (next, lr) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        if lr == 0 {
            break;
        }
        if skip > 0 {
            skip -= 1;
        } else {
            at[i] = lr;
            i += 1;
        }
        if next <= fp {
            break;
        }
        fp = next;
    }
    at
}

/// HEAP のロックを持って呼ぶ。番号 + 1 (いっぱいなら 0)
fn site_add(bytes: usize) -> usize {
    let at = callers();
    let t = unsafe { &mut *(&raw mut SITE) };
    let mut free = None;
    for (i, s) in t.iter_mut().enumerate() {
        if s.count > 0 && s.at == at {
            s.bytes += bytes;
            s.count += 1;
            return i + 1;
        }
        if s.count == 0 && free.is_none() {
            free = Some(i);
        }
    }
    let Some(i) = free else { return 0 };
    t[i] = Site { at, bytes, count: 1 };
    i + 1
}

fn site_sub(id: usize, bytes: usize) {
    if id == 0 || id > SITES {
        return;
    }
    let s = unsafe { &mut (*(&raw mut SITE))[id - 1] };
    s.bytes = s.bytes.saturating_sub(bytes);
    s.count = s.count.saturating_sub(1);
}

/// heap trace on|off|reset
pub fn trace(on: bool) {
    let _h = HEAP.lock();
    TRACE.store(on, Relaxed);
}

/// /proc/ai/heap に足す: いま持っているバイトの多い場所 20 (1 行 1 つ)
fn sites_json(out: &mut alloc::string::String) {
    let mut top: [(usize, usize, [usize; DEPTH]); 20] = [(0, 0, [0; DEPTH]); 20];
    {
        let _h = HEAP.lock();
        for s in unsafe { (*(&raw const SITE)).iter() } {
            if s.count == 0 {
                continue;
            }
            if let Some(m) = (0..top.len()).min_by_key(|&k| top[k].0) {
                if s.bytes > top[m].0 {
                    top[m] = (s.bytes, s.count, s.at);
                }
            }
        }
    }
    top.sort_by(|a, b| b.0.cmp(&a.0));
    for (bytes, count, at) in top.iter().filter(|t| t.0 > 0) {
        let at: alloc::vec::Vec<alloc::string::String> = at.iter().filter(|&&a| a != 0).map(|a| alloc::format!("\"{:#x}\"", a)).collect();
        out.push_str(&alloc::format!("{{\"bytes\":{},\"count\":{},\"at\":[{}]}}\n", bytes, count, at.join(",")));
    }
}

/// ヒープの大きさ (起動のときに RAM に合わせて決まる) と、いま使っているバイト
/// (ほかが混みぐあいを見て減らすため: ページキャッシュの表など)
static TOTAL: AtomicUsize = AtomicUsize::new(HEAP_SIZE);
pub fn size() -> usize {
    TOTAL.load(Relaxed)
}
pub fn used() -> usize {
    USED.load(Relaxed)
}

/// /proc/ai/heap: 1 行の JSON
pub fn stats_json() -> alloc::string::String {
    let (n, big) = free_blocks(&HEAP.lock());
    let mut s = alloc::format!(
        "{{\"size\":{},\"used\":{},\"peak\":{},\"free_blocks\":{},\"largest_free\":{},\"fails\":{}}}\n",
        size(),
        USED.load(Relaxed),
        PEAK.load(Relaxed),
        n,
        big,
        FAILS.load(Relaxed)
    );
    if TRACE.load(Relaxed) {
        sites_json(&mut s);
    }
    s
}

const HEAP_SIZE: usize = 32 * 1024 * 1024;
const ALIGN: usize = 16;

#[repr(C, align(16))]
struct Area([u8; HEAP_SIZE]);

static mut AREA: Area = Area([0; HEAP_SIZE]);

/// 空きブロックの先頭に置くヘッダ (アドレス順に並べる)
struct Free {
    size: usize,
    next: *mut Free,
}

struct Heap {
    head: *mut Free,
}

unsafe impl Send for Heap {}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap { head: ptr::null_mut() });

/// ヒープの大きさの上限 (メモリの多いマシンでも)
const MAX_TOTAL: usize = 512 * 1024 * 1024;

/// bss の 32 MiB に、メモリが多ければ (1/32 が 32 MiB をこえる分) まだ使っていないページの続いたかたまりを足す
/// (ページキャッシュの表、tmpfs、プロセスの地図などがメモリに比例してふえるので、決まった大きさでは
/// 大きなビルドで足りなくなった。2 GiB で 64 MiB、16 GiB で 512 MiB。kalloc::init のあとで)
pub fn init() {
    let base = (&raw mut AREA) as *mut Free;
    unsafe { base.write(Free { size: HEAP_SIZE, next: ptr::null_mut() }) };
    HEAP.lock().head = base;
    let want = (crate::memlayout::ram_size() / 32).clamp(HEAP_SIZE, MAX_TOTAL);
    let extra = want - HEAP_SIZE;
    if extra == 0 {
        return;
    }
    let Some(p) = crate::kalloc::take_contiguous(extra) else { return };
    // フリーリストはアドレスの順: 足すものは bss (カーネルの中) より後ろにあるので、おしまいにつなぐ
    let blk = p as *mut Free;
    let h = HEAP.lock();
    unsafe {
        blk.write(Free { size: extra, next: ptr::null_mut() });
        let mut cur = h.head;
        while !(*cur).next.is_null() {
            cur = (*cur).next;
        }
        if (cur as usize) < blk as usize {
            (*cur).next = blk;
        } else {
            return;
        }
    }
    TOTAL.store(want, Relaxed);
}

fn round(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

/// 確保したブロックの直前に大きさを覚えておく
const HDR: usize = ALIGN;

struct Kernel;

unsafe impl GlobalAlloc for Kernel {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN {
            return ptr::null_mut();
        }
        let need = round(layout.size().max(1)) + HDR;
        let mut h = HEAP.lock();
        let mut prev: *mut *mut Free = &mut h.head;
        unsafe {
            while !(*prev).is_null() {
                let cur = *prev;
                let size = (*cur).size;
                if size >= need {
                    if size - need >= size_of::<Free>() + ALIGN {
                        let rest = (cur as *mut u8).add(need) as *mut Free;
                        rest.write(Free { size: size - need, next: (*cur).next });
                        *prev = rest;
                        *(cur as *mut usize) = need;
                    } else {
                        *prev = (*cur).next;
                        *(cur as *mut usize) = size;
                    }
                    let used = USED.fetch_add(*(cur as *mut usize), Relaxed) + *(cur as *mut usize);
                    PEAK.fetch_max(used, Relaxed);
                    // ヘッダの 2 語目: どこで取ったか (調べているときだけ。ほかは 0)
                    *(cur as *mut usize).add(1) = if TRACE.load(Relaxed) { site_add(*(cur as *mut usize)) } else { 0 };
                    return (cur as *mut u8).add(HDR);
                }
                prev = &mut (*cur).next;
            }
        }
        // 取れなかった: どれだけ使っていて、空きがどれだけ割れているか (このあと panic になることが多い。ヒープは使わずに書く)
        FAILS.fetch_add(1, Relaxed);
        let (n, big) = free_blocks(&h);
        crate::println!(
            "heap: no {} bytes (used {} of {}, peak {}, {} free blocks, the largest {})",
            need,
            USED.load(Relaxed),
            size(),
            PEAK.load(Relaxed),
            n,
            big
        );
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, p: *mut u8, _layout: Layout) {
        unsafe {
            let blk = p.sub(HDR) as *mut Free;
            let size = *(blk as *mut usize);
            USED.fetch_sub(size, Relaxed);
            let mut h = HEAP.lock();
            site_sub(*(blk as *mut usize).add(1), size);
            // アドレス順の位置を探して挿入し、前後と結合する
            let mut prev: *mut Free = ptr::null_mut();
            let mut cur = h.head;
            while !cur.is_null() && cur < blk {
                prev = cur;
                cur = (*cur).next;
            }
            blk.write(Free { size, next: cur });
            if !cur.is_null() && (blk as usize) + size == cur as usize {
                (*blk).size += (*cur).size;
                (*blk).next = (*cur).next;
            }
            if prev.is_null() {
                h.head = blk;
            } else if (prev as usize) + (*prev).size == blk as usize {
                (*prev).size += (*blk).size;
                (*prev).next = (*blk).next;
            } else {
                (*prev).next = blk;
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Kernel = Kernel;
