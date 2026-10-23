// static mut のかわりに、だれが守っているかを型に書く入れもの。
//   Bkl<T>  大きなロック (BKL) が守るもの。中を出すのは BKL を持っている CPU だけ
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
            unlocked(Location::caller());
        }
        unsafe { &mut *self.0.get() }
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
fn unlocked(at: &'static Location<'static>) {
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
                    println!("bkl: {}:{} touched without the big lock (cpu{})", at.file(), at.line(), crate::smp::id());
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
