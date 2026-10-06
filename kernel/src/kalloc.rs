// 物理ページ (4KiB) アロケータ
//
// 返されたページはフリーリストに積む。リストが空なら、まだ一度も
// 使っていない領域 [fresh, end) から切り出す。起動時に全ページへ
// 触らないので速い。
//
// ユーザーのページは複数のアドレス空間で共有されることがある (fork のコピーオンライト)
// ので、ページごとに参照の数を持つ。alloc で 1、get で +1、put で -1 し、0 になったら返す。
use crate::memlayout::{p2v, pg_round_up, phystop, v2p, ram_base, PGSIZE};
use crate::spinlock::SpinLock;
use core::ptr;
use core::sync::atomic::{AtomicU16, Ordering};

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

/// RAM のページごとの参照の数 (RAM は memlayout::MAX_RAM まで)
const MAX_PAGES: usize = crate::memlayout::MAX_RAM / PGSIZE;
/// ページごとの、使っている人の数。大きなロックなしのページフォルト (vm.rs fast_fault) からも触るので atomic
static REFS: [AtomicU16; MAX_PAGES] = [const { AtomicU16::new(0) }; MAX_PAGES];

fn ref_slot(page: *mut u8) -> &'static AtomicU16 {
    &REFS[(v2p(page as usize) - ram_base()) / PGSIZE]
}

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
    ref_slot(page).store(0, Ordering::Relaxed);
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
    ref_slot(page).store(1, Ordering::Relaxed);
    Some(page)
}

/// 共有する人が増える
pub fn get(page: *mut u8) {
    ref_slot(page).fetch_add(1, Ordering::AcqRel);
}

/// 共有をやめる。誰も使わなくなったら返す
pub fn put(page: *mut u8) {
    // 最後の 1 人なら返す (fetch_sub の前の値が 1 以下)
    if ref_slot(page).fetch_sub(1, Ordering::AcqRel) <= 1 {
        free(page);
    }
}

/// 何人で使っているか
pub fn refs(page: *mut u8) -> u16 {
    ref_slot(page).load(Ordering::Acquire)
}

pub fn nfree() -> usize {
    let k = KMEM.lock();
    k.nlist + (k.end - k.fresh) / PGSIZE
}
