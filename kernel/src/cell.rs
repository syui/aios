// static mut のかわりに、だれが守っているかを型に書く入れもの。
//   Bkl<T>    大きなロック (BKL) が守るもの。中を出すのは BKL を持っている CPU だけ
//   Once<T>   1 度だけ入れて、あとはどの CPU もロックなしで読むもの
//   Early<T>  起動のとき (ほかの CPU を起こす前) にだけ書き、あとはロックなしで読むもの
//   PerCpu<T> CPU ごとに 1 つずつ。自分の CPU の分だけを、割り込みを止めて触る
//   Raw<T>    番地をほかのもの (アロケータ、アセンブリ) に渡すだけのメモリ。Rust からは中を見ない
// ほかに、自分のロックを持つものは SpinLock<T>、数えるだけのものは atomic にする。
//
// static mut には Rust のコンパイラが何も言わない。BKL を外した道 (syscall::fast、vm の速いフォールト、
// 割り込み) がうっかり触っても気づけず、2 つの CPU が同時に BTreeMap を書けば、そこでもう壊れている。
// Bkl::get は呼んだ CPU が BKL を持っているかを見て、持っていなければ呼んだ場所 (ファイル:行) を 1 度だけ
// kmsg に出して数える (/proc/bkl の unlocked。test/vm-check.sh はそれがあれば落ちる)。機械は止めない
// (止めれば、いまは動いているものまで落ちる。直すまでのあいだ、見えるようにする)。
// CPU が 1 つしか動いていないあいだ (起動のはじめ、aios.smp=1) は、ほかに触る CPU がないので見ない。
// Linux はどのデータをどのロックが守るかをコメントに書き、lockdep は実行時にロックの順を見るだけ。
// aios は型に書くので、守り手のない道は動かせば分かる
use core::cell::UnsafeCell;
use core::panic::Location;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// 大きなロック (BKL) が守るもの
pub struct Bkl<T>(UnsafeCell<T>);

// 中身は BKL を持っている CPU だけが出す (get が見る)
unsafe impl<T> Sync for Bkl<T> {}

impl<T> Bkl<T> {
    pub const fn new(v: T) -> Self {
        Bkl(UnsafeCell::new(v))
    }

    /// 中身。BKL を持っていなければ、呼んだ場所を知らせる (unlocked)
    #[track_caller]
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        if !crate::smp::holding() && crate::smp::online() > 1 {
            unlocked(Location::caller(), "touched without the big lock");
        }
        unsafe { &mut *self.0.get() }
    }
}

/// 起動のとき (ほかの CPU を起こす前) にだけ書き、あとは読むだけのもの: DTB の写し、機器の番地、RAM の大きさなど。
/// 読むのにロックはいらない (もう変わらないので)。ほかの CPU が動きだしてから書けば、Bkl と同じく呼んだ場所を知らせる
pub struct Early<T>(UnsafeCell<T>);

// 書くのは CPU が 1 つのあいだだけ (get_mut が見る)。そのあとは &T を読むだけ
unsafe impl<T> Sync for Early<T> {}

impl<T> Early<T> {
    pub const fn new(v: T) -> Self {
        Early(UnsafeCell::new(v))
    }

    #[inline]
    pub fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }

    /// 書くとき (起動のときだけ)。ほかの CPU が動いていれば、呼んだ場所を知らせる
    #[track_caller]
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut T {
        if crate::smp::online() > 1 {
            unlocked(Location::caller(), "written after boot");
        }
        unsafe { &mut *self.0.get() }
    }

    #[track_caller]
    pub fn set(&self, v: T) {
        *self.get_mut() = v;
    }
}

impl<T: Copy> Early<T> {
    #[inline]
    pub fn val(&self) -> T {
        *self.get()
    }
}

/// CPU ごとに 1 つずつ持つもの (ヒープの置き場、まとめて作った乱数、いま走らせているスレッド)。触るのは自分の CPU の分
/// だけで、割り込みを止めたまま (カーネルはいつもそう: とちゅうでほかの CPU へ移らず、割り込みからも同時には触られない)。
/// 眠る (スケジューラへ戻る) のをまたいで持たないこと (起きたら別の CPU かも)。
/// 割り込みが止まっているかは、ここでは見ない: DAIF を読むのは QEMU (TCG) では重く (システムコール 1 回で 2 割)、カーネルが
/// 割り込みを開けるのはスケジューラが眠るところ (trap::intr_on) と、開いていたものを戻す SpinLock だけ。それを
/// test/kernel-json.py が見る (ほかで開けると落ちる)。debug のビルドでは、ここでも見る
pub struct PerCpu<T>(UnsafeCell<[T; crate::smp::MAXCPU]>);

// 中身はその CPU だけが、割り込みを止めて出す (割り込みを開けるところは kernel-json が見る)
unsafe impl<T> Sync for PerCpu<T> {}

impl<T> PerCpu<T> {
    pub const fn new(v: [T; crate::smp::MAXCPU]) -> Self {
        PerCpu(UnsafeCell::new(v))
    }

    /// この CPU の分
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.ptr() }
    }

    /// この CPU の分の番地 (切りかえのアセンブリに渡すもの)
    #[inline]
    pub fn ptr(&self) -> *mut T {
        #[cfg(debug_assertions)]
        {
            let daif: u64;
            unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
            debug_assert!(daif & (1 << 7) != 0, "per-CPU data with IRQs open");
        }
        let cpu = crate::smp::id();
        assert!(cpu < crate::smp::MAXCPU);
        unsafe { (self.0.get() as *mut T).add(cpu) }
    }
}

/// 番地をほかのもの (アロケータ、アセンブリ、ページ表をたどる機械) に渡すだけのメモリ: ヒープの区画、カーネルの
/// スタック、プロセスの表。Rust の参照は作らないので (ptr で番地だけ)、中を守るのは渡した先のきまり
#[repr(transparent)]
pub struct Raw<T>(UnsafeCell<T>);

// 中身を読み書きするのは番地を渡した先 (そのきまりはそれぞれの場所に書く)
unsafe impl<T> Sync for Raw<T> {}

impl<T> Raw<T> {
    pub const fn new(v: T) -> Self {
        Raw(UnsafeCell::new(v))
    }

    #[inline]
    pub const fn ptr(&self) -> *mut T {
        self.0.get()
    }
}

/// 1 度だけ入れて、あとは読むだけのもの (マウントや起動のとき)。入ったあとは、どの CPU もロックなしで読める
pub struct Once<T> {
    /// 0 まだ、1 入れている途中、2 入った
    state: core::sync::atomic::AtomicU8,
    v: UnsafeCell<core::mem::MaybeUninit<T>>,
}

// 入れるのは 1 度だけ (state で 1 つの CPU に決める)。読むのは入ったあと (Release / Acquire) で、&T だけ
unsafe impl<T> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { state: core::sync::atomic::AtomicU8::new(0), v: UnsafeCell::new(core::mem::MaybeUninit::uninit()) }
    }

    /// 入っていれば中身
    #[inline]
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == 2 { Some(unsafe { (*self.v.get()).assume_init_ref() }) } else { None }
    }

    /// まだなら入れる (入れたら true。もう入っていれば何もしない)
    pub fn set(&self, v: T) -> bool {
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return false;
        }
        unsafe { (*self.v.get()).write(v) };
        self.state.store(2, Ordering::Release);
        true
    }
}

/// BKL を持たずに触った場所 (はじめの SITES こ) と、その回数
const SITES: usize = 32;
static SITE: [AtomicPtr<Location<'static>>; SITES] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SITES];
static HITS: [AtomicUsize; SITES] = [const { AtomicUsize::new(0) }; SITES];
/// 表に入らなかった回数
static MORE: AtomicUsize = AtomicUsize::new(0);

#[cold]
#[inline(never)]
fn unlocked(at: &'static Location<'static>, what: &str) {
    let p = at as *const Location<'static> as *mut Location<'static>;
    for (s, n) in SITE.iter().zip(&HITS) {
        let cur = s.load(Ordering::Acquire);
        if cur == p {
            n.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if cur.is_null() {
            match s.compare_exchange(core::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    n.fetch_add(1, Ordering::Relaxed);
                    println!("bkl: {}:{} {} (cpu{})", at.file(), at.line(), what, crate::smp::id());
                    return;
                }
                // ほかの CPU が同じところに入れた: それが同じ場所なら数える
                Err(other) if other == p => {
                    n.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {}
            }
        }
    }
    MORE.fetch_add(1, Ordering::Relaxed);
}

/// ctl の bkl selftest (test/vm-check.sh): BKL をいったん放して、ためしの Bkl を触る。その場所が数えられれば、見はりは
/// 効いている (kmsg に bkl: selftest ok / FAILED)。ためしの場所 (このファイル) は /proc/bkl の unlocked に出さない
pub fn selftest() {
    static T: Bkl<u64> = Bkl::new(0);
    if crate::smp::online() < 2 {
        println!("bkl: selftest skipped (1 cpu)");
        return;
    }
    crate::smp::unlock();
    // ここで BKL なしに触る (T はほかのだれも触らない)
    *T.get() += 1;
    crate::smp::lock();
    let ok = SITE.iter().zip(&HITS).any(|(s, n)| {
        let p = s.load(Ordering::Acquire);
        !p.is_null() && unsafe { &*p }.file().ends_with("cell.rs") && n.load(Ordering::Relaxed) > 0
    });
    println!("bkl: selftest {}", if ok { "ok" } else { "FAILED" });
}

/// /proc/bkl の終わりに: BKL を持たずに触った場所と回数 (なければ空。selftest のものは出さない)
pub fn unlocked_text() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for (site, n) in SITE.iter().zip(&HITS) {
        let p = site.load(Ordering::Acquire);
        if p.is_null() {
            break;
        }
        let at = unsafe { &*p };
        if at.file().ends_with("cell.rs") {
            continue;
        }
        let _ = writeln!(s, "unlocked {}:{} {}", at.file(), at.line(), n.load(Ordering::Relaxed));
    }
    let more = MORE.load(Ordering::Relaxed);
    if more > 0 {
        let _ = writeln!(s, "unlocked (more places) {}", more);
    }
    s
}
