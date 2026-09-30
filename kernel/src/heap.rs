// カーネルヒープ: bss 上の固定領域を first-fit のフリーリストで切り分ける
use crate::spinlock::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

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
                    return (cur as *mut u8).add(HDR);
                }
                prev = &mut (*cur).next;
            }
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, p: *mut u8, _layout: Layout) {
        unsafe {
            let blk = p.sub(HDR) as *mut Free;
            let size = *(blk as *mut usize);
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
