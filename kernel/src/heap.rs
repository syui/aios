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

/// ヒープの大きさと、いま使っているバイト (ほかが混みぐあいを見て減らすため: ページキャッシュの表など)
pub const SIZE: usize = HEAP_SIZE;
pub fn used() -> usize {
    USED.load(Relaxed)
}

/// /proc/ai/heap: 1 行の JSON
pub fn stats_json() -> alloc::string::String {
    let (n, big) = free_blocks(&HEAP.lock());
    alloc::format!(
        "{{\"size\":{},\"used\":{},\"peak\":{},\"free_blocks\":{},\"largest_free\":{},\"fails\":{}}}\n",
        HEAP_SIZE,
        USED.load(Relaxed),
        PEAK.load(Relaxed),
        n,
        big,
        FAILS.load(Relaxed)
    )
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

pub fn init() {
    let base = (&raw mut AREA) as *mut Free;
    unsafe { base.write(Free { size: HEAP_SIZE, next: ptr::null_mut() }) };
    HEAP.lock().head = base;
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
            HEAP_SIZE,
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
