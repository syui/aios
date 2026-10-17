// カーネルヒープ: bss 上の固定領域 (と、メモリが多ければ足したもの) を切り分ける。空きの管理は TLSF (tlsf.rs:
// 取るのも返すのも O(1))。ここはロック、使っている量の数え、どこで取ったか (heap trace)
use crate::spinlock::SpinLock;
use crate::tlsf::{self, Tlsf};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

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

static HEAP: SpinLock<Tlsf> = SpinLock::new(Tlsf::new());

/// ヒープの大きさの上限 (メモリの多いマシンでも)
const MAX_TOTAL: usize = 512 * 1024 * 1024;

/// bss の 32 MiB に、メモリが多ければ (1/32 が 32 MiB をこえる分) まだ使っていないページの続いたかたまりを足す
/// (ページキャッシュの表、tmpfs、プロセスの地図などがメモリに比例してふえるので、決まった大きさでは
/// 大きなビルドで足りなくなった。2 GiB で 64 MiB、16 GiB で 512 MiB。kalloc::init のあとで)
pub fn init() {
    unsafe { HEAP.lock().add_region((&raw mut AREA) as *mut u8, HEAP_SIZE) };
    let want = (crate::memlayout::ram_size() / 32).clamp(HEAP_SIZE, MAX_TOTAL);
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

struct Kernel;

unsafe impl GlobalAlloc for Kernel {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN {
            return ptr::null_mut();
        }
        let need = round(layout.size().max(1)) + HDR;
        // わざと取れないことに (/proc/ai/ctl の fail ... heap。fail.rs): 取れなかったときと同じく数えて null
        if crate::fail::heap() {
            FAILS.fetch_add(1, Relaxed);
            return ptr::null_mut();
        }
        let mut h = HEAP.lock();
        unsafe {
            let b = h.alloc(need);
            if !b.is_null() {
                let size = tlsf::block_size(b);
                let used = USED.load(Relaxed) + size;
                USED.store(used, Relaxed);
                if used > PEAK.load(Relaxed) {
                    PEAK.store(used, Relaxed);
                }
                // ヘッダの 2 語目: どこで取ったか (調べているときだけ。ほかは 0)
                *(b as *mut usize).add(1) = if TRACE.load(Relaxed) { site_add(size) } else { 0 };
                return b.add(HDR);
            }
        }
        // 取れなかった: どれだけ使っていて、空きがどれだけ割れているか (このあと panic になることが多い。ヒープは使わずに書く)
        FAILS.fetch_add(1, Relaxed);
        let (n, big, _) = h.stats();
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
            let b = p.sub(HDR);
            // 大きさはロックの中で読む (ヘッダの PREV_FREE は、前のブロックを取ったり返したりするほかの CPU が変える)
            let mut h = HEAP.lock();
            let size = tlsf::block_size(b);
            USED.store(USED.load(Relaxed) - size, Relaxed);
            site_sub(*(b as *mut usize).add(1), size);
            h.free(b);
        }
    }
}

#[global_allocator]
static ALLOCATOR: Kernel = Kernel;
