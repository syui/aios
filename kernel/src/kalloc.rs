// 物理ページ (4KiB) アロケータ
//
// 返されたページはフリーリストに積む。リストが空なら、まだ一度も
// 使っていない領域 [fresh, end) から切り出す。起動時に全ページへ
// 触らないので速い。
use crate::memlayout::{p2v, pg_round_up, phystop, PGSIZE};
use crate::spinlock::SpinLock;
use core::ptr;

struct Run {
    next: *mut Run,
}

struct Kmem {
    head: *mut Run,
    nlist: usize,
    fresh: usize,
    end: usize,
}

unsafe impl Send for Kmem {}

static KMEM: SpinLock<Kmem> =
    SpinLock::new(Kmem { head: ptr::null_mut(), nlist: 0, fresh: 0, end: 0 });

pub fn init() {
    unsafe extern "C" {
        static __kernel_end: u8;
    }
    let mut k = KMEM.lock();
    k.fresh = pg_round_up(&raw const __kernel_end as usize);
    k.end = p2v(phystop());
}

/// 仮想アドレス (KBASE 側) のページを返す
pub fn free(page: *mut u8) {
    let r = page as *mut Run;
    let mut k = KMEM.lock();
    unsafe { (*r).next = k.head };
    k.head = r;
    k.nlist += 1;
}

/// 0 埋めしたページを 1 枚。なければ None
pub fn alloc() -> Option<*mut u8> {
    let mut k = KMEM.lock();
    let page = if !k.head.is_null() {
        let r = k.head;
        k.head = unsafe { (*r).next };
        k.nlist -= 1;
        r as *mut u8
    } else if k.fresh < k.end {
        let p = k.fresh;
        k.fresh += PGSIZE;
        p as *mut u8
    } else {
        return None;
    };
    drop(k);
    unsafe { ptr::write_bytes(page, 0, PGSIZE) };
    Some(page)
}

pub fn nfree() -> usize {
    let k = KMEM.lock();
    k.nlist + (k.end - k.fresh) / PGSIZE
}
