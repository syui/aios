// 割り込みを止めて取るスピンロック。aios のカーネルはふつう割り込みを止めたまま走る (開けるのは、スケジューラが
// 割り込みを待つときだけ: proc.rs) ので、止めるのも戻すのも、開いていたときだけ。DAIF に書くと QEMU の TCG では
// そこで訳した命令のかたまりが終わるので重く (msr daif で 70〜130 ns)、取って放す 160〜270 ns の 8 割がこれだった
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    daif: u64,
}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> Guard<'_, T> {
        let daif = intr_save();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Guard { lock: self, daif }
    }

    /// 取れたら Some (取れなければ待たない。println のように、どこからでも呼ばれるところ用)
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        let daif = intr_save();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Some(Guard { lock: self, daif })
        } else {
            intr_restore(daif);
            None
        }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        intr_restore(self.daif);
    }
}

/// DAIF の I (IRQ を止めている印)
const DAIF_I: u64 = 1 << 7;

/// IRQ を止める (もう止まっていれば何もしない)。前の DAIF を返す
fn intr_save() -> u64 {
    let daif: u64;
    unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif) };
    if daif & DAIF_I == 0 {
        unsafe { core::arch::asm!("msr daifset, #2") };
    }
    daif
}

/// intr_save の前に戻す: 開いていたときだけ開ける (ロックの中で DAIF は変えないこと)
fn intr_restore(daif: u64) {
    if daif & DAIF_I == 0 {
        unsafe { core::arch::asm!("msr daifclr, #2") };
    }
}

/// lockbench (/proc/ai/ctl): ロックの部品ごとの 1 回の時間を kmsg に (どれも 10 万回の平均)。
/// 大きなロックを持って呼ぶ (それを放して取りなおすものも測る)
pub fn bench() {
    use core::hint::black_box;
    let n = 100_000u64;
    let cnt = || {
        let c: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
        c
    };
    let ns = |t: u64| t as f64 * 1e9 / crate::timer::freq() as f64 / n as f64;
    let l = SpinLock::new(0u64);
    let t0 = cnt();
    for _ in 0..n {
        let g = l.lock();
        black_box(&*g);
    }
    let lock = ns(cnt() - t0);
    let t0 = cnt();
    for i in 0..n {
        black_box(alloc::boxed::Box::new(i));
    }
    let heap = ns(cnt() - t0);
    let t0 = cnt();
    for _ in 0..n {
        black_box(crate::timer::uptime_ns());
    }
    let clock = ns(cnt() - t0);
    // 物理ページ (kalloc): 1 枚取ってすぐ返す (すぐまた取るもの)、1024 枚取ってから返す (大きなかたまりを分けて、
    // まとめなおす)。どちらも 1 枚あたり
    let t0 = cnt();
    for _ in 0..n {
        if let Some(p) = crate::kalloc::alloc_dirty() {
            crate::kalloc::free(black_box(p));
        }
    }
    let page = ns(cnt() - t0);
    let t0 = cnt();
    let mut got = 0;
    for _ in 0..n / 1024 {
        let mut head: *mut usize = core::ptr::null_mut();
        for _ in 0..1024 {
            let Some(p) = crate::kalloc::alloc_dirty() else { break };
            unsafe { *(p as *mut usize) = head as usize };
            head = p as *mut usize;
            got += 1;
        }
        while !head.is_null() {
            let next = unsafe { *head } as *mut usize;
            crate::kalloc::free(head as *mut u8);
            head = next;
        }
    }
    let pages = (cnt() - t0) as f64 * 1e9 / crate::timer::freq() as f64 / got.max(1) as f64;
    let bkl = crate::smp::bench_bkl();
    crate::println!(
        "lockbench: SpinLock lock+unlock {:.1} ns, heap alloc+free {:.1} ns, BKL unlock+lock {:.1} ns, uptime_ns {:.1} ns, page alloc+free {:.1} ns, page 1024 alloc+free {:.1} ns",
        lock, heap, bkl, clock, page, pages
    );
}
