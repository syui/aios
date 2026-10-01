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

/// RAM のページごとの参照の数 (RAM は 1 GiB まで)
const MAX_PAGES: usize = 1 << 18;
static mut REFS: [u16; MAX_PAGES] = [0; MAX_PAGES];

fn ref_slot(page: *mut u8) -> &'static mut u16 {
    let i = (v2p(page as usize) - ram_base()) / PGSIZE;
    unsafe { &mut (*(&raw mut REFS))[i] }
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
    *ref_slot(page) = 0;
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
    *ref_slot(page) = 1;
    Some(page)
}

/// 共有する人が増える
pub fn get(page: *mut u8) {
    let r = ref_slot(page);
    *r = r.saturating_add(1);
}

/// 共有をやめる。誰も使わなくなったら返す
pub fn put(page: *mut u8) {
    let r = ref_slot(page);
    if *r <= 1 {
        free(page);
    } else {
        *r -= 1;
    }
}

/// 何人で使っているか
pub fn refs(page: *mut u8) -> u16 {
    *ref_slot(page)
}

pub fn nfree() -> usize {
    let k = KMEM.lock();
    k.nlist + (k.end - k.fresh) / PGSIZE
}
