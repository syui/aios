// カーネルヒープ: bss 上の固定領域 (と、メモリに合わせて足したもの) を切り分ける。空きの管理は TLSF (tlsf.rs:
// 取るのも返すのも O(1))。ここはロック、使っている量の数え、どこで取ったか (heap trace)、それから足りないとき
// (足す、捨てる、耐えるところには null、予備と oom.rs: 下の「足りないとき」)
use crate::spinlock::SpinLock;
use crate::tlsf::{self, Tlsf};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

/// 使っているバイト (ヘッダこみ)、いちばん多かったとき、取れなかった回数 (/proc/ai/heap)。USED と PEAK を変えるのは HEAP の
/// ロックを持っているときだけなので、読んで書くだけ (fetch_add のような読み書きの命令は TCG で重い)
static USED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static FAILS: AtomicUsize = AtomicUsize::new(0);

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

/// heap check: 空きの管理の決まり (つながり、印、リスト、ビット) を確かめて kmsg に。ブロックをみなたどるので、
/// ロックを持ったままになる (数万のブロックで数 ms)
pub fn check() {
    let r = unsafe { HEAP.lock().check() };
    match r {
        Ok(()) => crate::println!("heap: check ok"),
        Err(e) => crate::println!("heap: check FAILED: {}", e),
    }
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

/// (大きさ, 使っている, いちばん多かった, 空きのかたまりの数, 空きのいちばん大きなかたまり, 空きの全部, 取れなかった回数)。
/// どれもバイト (数のほかは)。/proc/ai/mem
pub fn stats() -> (usize, usize, usize, usize, usize, usize, usize) {
    let (n, big, free) = HEAP.lock().stats();
    (size(), USED.load(Relaxed), PEAK.load(Relaxed), n, big, free, FAILS.load(Relaxed))
}

/// /proc/ai/heap: 1 行の JSON
pub fn stats_json() -> alloc::string::String {
    let (n, big, _) = HEAP.lock().stats();
    let mut s = alloc::format!(
        "{{\"size\":{},\"used\":{},\"peak\":{},\"free_blocks\":{},\"largest_free\":{},\"fails\":{},\"grown\":{},\"reclaims\":{},\"reserve_used\":{},\"emergencies\":{},\"oom_kills\":{}}}\n",
        size(),
        USED.load(Relaxed),
        PEAK.load(Relaxed),
        n,
        big,
        FAILS.load(Relaxed),
        GROWN.load(Relaxed),
        RECLAIMS.load(Relaxed),
        RESERVE_USED.load(Relaxed),
        EMERGENCIES.load(Relaxed),
        crate::oom::kills()
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

static HEAP: SpinLock<Tlsf> = SpinLock::new(Tlsf::new());

// ---- 足りないとき ----
// ふつうの道 (HEAP の TLSF) で取れなければ、順に:
//   1. 足す (grow): まだだれも使っていない続いたページ (kalloc::take_contiguous) を区画として足す (GROW_CHUNK ずつ。
//      上限は RAM の 1/4: max_total)。たいていはこれで足りる。前は起動のときに決めた大きさ (RAM の 1/32) で、
//      それを使いきると panic だった
//   2. 捨てる (shrink): だれも写していないページキャッシュのページを捨てる (表の分がヒープに戻り、ページは kalloc へ)
//   3. それでもなければ、耐えるところ (heap::fallible の区間の中: read/write の入れ物など、取れなければ ENOMEM を
//      返せるところ) には null (Vec::try_reserve が Err になる)
//   4. 耐えないところ (Vec::push など。Rust の既定は null で panic) には予備 (RESERVE) から渡して EMERGENCY を立てる。
//      次のシステムコールの入り口で oom.rs が、いちばんメモリを持っているプロセスを終わらせて空ける (alloc の中では
//      殺せない: カーネルの表を変えている途中かもしれない)。予備もなければ null で、そこで panic する
// 遅い道の中で取ろうとしたもの (捨てるときなど) は CPU ごとの RECLAIMING で見分けて、遅い道をくりかえさない。
// /proc/ai/ctl の fail ... heap は 3 を、fail ... oom は 4 を、わざと起こす (fail.rs)

/// 足すときのひとかたまり (足りなければ、要る分まで半分ずつ小さくして)
const GROW_CHUNK: usize = 8 * 1024 * 1024;
/// 予備: 耐えないところへの最後の分。ここに手をつけたら oom.rs
const RESERVE_SIZE: usize = 2 * 1024 * 1024;

#[repr(C, align(16))]
struct ReserveArea([u8; RESERVE_SIZE]);

static mut RESERVE_AREA: ReserveArea = ReserveArea([0; RESERVE_SIZE]);
static RESERVE: SpinLock<Tlsf> = SpinLock::new(Tlsf::new());

/// 動かしたまま足したバイト、遅い道に来た回数、予備のいま使っているバイト、予備に手をつけた回数
static GROWN: AtomicUsize = AtomicUsize::new(0);
static RECLAIMS: AtomicUsize = AtomicUsize::new(0);
static RESERVE_USED: AtomicUsize = AtomicUsize::new(0);
static EMERGENCIES: AtomicUsize = AtomicUsize::new(0);
/// 予備に手をつけている (oom::check が次のシステムコールの入り口で見て、空いたら下ろす)
pub static EMERGENCY: AtomicBool = AtomicBool::new(false);
/// わざと (fail ... oom) 予備に手をつけた: ふつうの道に空きがあっても、oom::check は 1 度は殺す (その道を確かめるため)
pub static FORCED: AtomicBool = AtomicBool::new(false);
/// この CPU は遅い道の中 (その中で取ろうとしても、また遅い道へは行かない)
static RECLAIMING: [AtomicBool; crate::smp::MAXCPU] = [const { AtomicBool::new(false) }; crate::smp::MAXCPU];

/// ヒープの大きさの上限 (起動のときの分も、動かしたまま足す分も): RAM の 1/4
fn max_total() -> usize {
    (crate::memlayout::ram_size() / 4).max(HEAP_SIZE)
}

/// bss の 32 MiB に、メモリが多ければ (1/32 が 32 MiB をこえる分) まだ使っていないページの続いたかたまりを足す
/// (ページキャッシュの表、tmpfs、プロセスの地図などがメモリに比例してふえるので、決まった大きさでは
/// 大きなビルドで足りなくなった。2 GiB で 64 MiB、16 GiB で 512 MiB。足りなくなれば動かしたまま足す: grow)。
/// 予備 (RESERVE) もここで。kalloc::init のあとで
pub fn init() {
    unsafe {
        HEAP.lock().add_region((&raw mut AREA) as *mut u8, HEAP_SIZE);
        RESERVE.lock().add_region((&raw mut RESERVE_AREA) as *mut u8, RESERVE_SIZE);
    }
    let want = (crate::memlayout::ram_size() / 32).clamp(HEAP_SIZE, 512 * 1024 * 1024);
    let extra = want - HEAP_SIZE;
    if extra == 0 {
        return;
    }
    let Some(p) = crate::kalloc::take_contiguous(extra) else { return };
    unsafe { HEAP.lock().add_region(p as *mut u8, extra) };
    TOTAL.store(want, Relaxed);
}

fn round(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

/// 確保したブロックのヘッダ (1 語目は tlsf の大きさと印、2 語目は取ったところの番号)。中身はそのあと
const HDR: usize = tlsf::ALIGN;

/// HEAP から need バイト (ヘッダこみ) のブロックを 1 つ。中身の頭 (なければ null)
unsafe fn take(need: usize) -> *mut u8 {
    let mut h = HEAP.lock();
    unsafe {
        let b = h.alloc(need);
        if b.is_null() {
            return b;
        }
        let size = tlsf::block_size(b);
        let used = USED.load(Relaxed) + size;
        USED.store(used, Relaxed);
        if used > PEAK.load(Relaxed) {
            PEAK.store(used, Relaxed);
        }
        // ヘッダの 2 語目: どこで取ったか (調べているときだけ。ほかは 0)
        *(b as *mut usize).add(1) = if TRACE.load(Relaxed) { site_add(size) } else { 0 };
        b.add(HDR)
    }
}

/// 予備から (HEAP のロックも持つ: USED はその中でだけ変える)
unsafe fn take_reserve(need: usize) -> *mut u8 {
    let _h = HEAP.lock();
    let mut r = RESERVE.lock();
    unsafe {
        let b = r.alloc(need);
        if b.is_null() {
            return b;
        }
        let size = tlsf::block_size(b);
        let used = USED.load(Relaxed) + size;
        USED.store(used, Relaxed);
        if used > PEAK.load(Relaxed) {
            PEAK.store(used, Relaxed);
        }
        RESERVE_USED.store(RESERVE_USED.load(Relaxed) + size, Relaxed);
        *(b as *mut usize).add(1) = 0;
        b.add(HDR)
    }
}

fn in_reserve(b: *mut u8) -> bool {
    let start = (&raw const RESERVE_AREA) as usize;
    (start..start + RESERVE_SIZE).contains(&(b as usize))
}

/// 遅い道: 足すか捨てるかできたら true (そのあと take をやりなおす)
fn reclaim(need: usize) -> bool {
    let cpu = crate::smp::id();
    if RECLAIMING[cpu].swap(true, Relaxed) {
        return false;
    }
    RECLAIMS.fetch_add(1, Relaxed);
    let ok = grow(need) || crate::vm::shrink_cache_try(1024) > 0;
    RECLAIMING[cpu].store(false, Relaxed);
    ok
}

/// 区画を足す: GROW_CHUNK (足りなければ need の分まで半分ずつ) の続いたページ。上限 max_total まで
fn grow(need: usize) -> bool {
    let total = TOTAL.load(Relaxed);
    let cap = max_total();
    if total >= cap {
        return false;
    }
    let page = crate::memlayout::PGSIZE;
    // ブロックと、区画の番兵とヘッダの分
    let least = (need + 4 * ALIGN).div_ceil(page) * page;
    let mut chunk = GROW_CHUNK.max(least).min(cap - total);
    if chunk < least {
        return false;
    }
    loop {
        if let Some(p) = crate::kalloc::take_contiguous(chunk) {
            unsafe { HEAP.lock().add_region(p, chunk) };
            TOTAL.store(total + chunk, Relaxed);
            GROWN.fetch_add(chunk, Relaxed);
            crate::println!("heap: grew by {} KiB to {} MiB (for {} bytes; {} KiB in use)", chunk / 1024, (total + chunk) >> 20, need, USED.load(Relaxed) / 1024);
            return true;
        }
        if chunk <= least {
            return false;
        }
        chunk = (chunk / 2).max(least);
    }
}

/// 空きが戻ったか (oom::check が EMERGENCY を下ろすために): ふつうの道に、予備くらいの続いた空きがある
pub fn recovered() -> bool {
    let (_, big, _) = HEAP.lock().stats();
    big >= RESERVE_SIZE
}

/// (動かしたまま足したバイト, 遅い道に来た回数, 予備のいま使っているバイト, 予備に手をつけた回数)。/proc/ai/mem
pub fn stats_more() -> (usize, usize, usize, usize) {
    (GROWN.load(Relaxed), RECLAIMS.load(Relaxed), RESERVE_USED.load(Relaxed), EMERGENCIES.load(Relaxed))
}

/// heap hog N (/proc/ai/ctl): N MiB をわざと取って持っておく (足す・捨てる・ENOMEM の道を試すため。0 で返す)。取れた MiB
static HOG: SpinLock<alloc::vec::Vec<alloc::vec::Vec<u8>>> = SpinLock::new(alloc::vec::Vec::new());

pub fn hog(mib: usize) -> usize {
    let mut old = core::mem::take(&mut *HOG.lock());
    old.clear();
    let mut v = old;
    while v.len() < mib {
        // 1 MiB ずつ (耐えるところとして: 足して捨ててもなければ ENOMEM で止まる。予備は使わない)
        match try_vec(1 << 20, true) {
            Ok(b) => v.push(b),
            Err(_) => break,
        }
    }
    let got = v.len();
    *HOG.lock() = v;
    crate::println!("heap: hog {} MiB (used {} of {})", got, USED.load(Relaxed), size());
    got
}

// ---- 耐えるところ ----

/// 取れなくても耐える区間: この中で取れなければ null (Vec::try_reserve が Err) で、予備は使わない。区間は取るところ
/// だけに短く (その中のほかの alloc も null になる)。深さはスレッドごと (proc の fallible)
pub struct Fallible(());

pub fn fallible() -> Fallible {
    crate::proc::fallible_enter();
    Fallible(())
}

impl Drop for Fallible {
    fn drop(&mut self) {
        crate::proc::fallible_leave();
    }
}

/// n バイトの Vec。取れなければ Err(-ENOMEM) (panic しない)。zero なら 0 で埋め、でなければ中身は決まっていない
/// (すぐ全部書くところ用)
pub fn try_vec(n: usize, zero: bool) -> Result<alloc::vec::Vec<u8>, i64> {
    const ENOMEM: i64 = 12;
    let mut v = alloc::vec::Vec::new();
    {
        let _f = fallible();
        v.try_reserve_exact(n).map_err(|_| -ENOMEM)?;
    }
    if zero {
        v.resize(n, 0);
    } else {
        unsafe { v.set_len(n) };
    }
    Ok(v)
}

struct Kernel;

unsafe impl GlobalAlloc for Kernel {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN {
            return ptr::null_mut();
        }
        let need = round(layout.size().max(1)) + HDR;
        let fallible = crate::proc::in_fallible();
        // わざと取れなかったことに (/proc/ai/ctl の fail ... heap|oom。fail.rs): ふつうの道も遅い道もなしで下へ
        let struck = crate::fail::heap(fallible);
        if !struck {
            let p = unsafe { take(need) };
            if !p.is_null() {
                return p;
            }
            if reclaim(need) {
                let p = unsafe { take(need) };
                if !p.is_null() {
                    return p;
                }
            }
        }
        FAILS.fetch_add(1, Relaxed);
        if fallible {
            return ptr::null_mut();
        }
        let p = unsafe { take_reserve(need) };
        if !p.is_null() {
            if struck {
                FORCED.store(true, Relaxed);
            }
            if !EMERGENCY.swap(true, Relaxed) {
                EMERGENCIES.fetch_add(1, Relaxed);
                crate::println!(
                    "heap: out of memory for {} bytes (used {} of {}): using the reserve; the biggest process goes at the next syscall",
                    need,
                    USED.load(Relaxed),
                    size()
                );
            }
            return p;
        }
        // 予備もない: どれだけ使っていて、空きがどれだけ割れているか (このあと panic になる。ヒープは使わずに書く)
        let (n, big, _) = HEAP.lock().stats();
        crate::println!(
            "heap: no {} bytes, not even in the reserve (used {} of {}, peak {}, {} free blocks, the largest {})",
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
            let b = p.sub(HDR);
            // 大きさはロックの中で読む (ヘッダの PREV_FREE は、前のブロックを取ったり返したりするほかの CPU が変える)
            let mut h = HEAP.lock();
            let size = tlsf::block_size(b);
            USED.store(USED.load(Relaxed) - size, Relaxed);
            site_sub(*(b as *mut usize).add(1), size);
            if in_reserve(b) {
                RESERVE_USED.store(RESERVE_USED.load(Relaxed) - size, Relaxed);
                RESERVE.lock().free(b);
            } else {
                h.free(b);
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Kernel = Kernel;
