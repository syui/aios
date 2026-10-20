// 物理ページ (4KiB) アロケータ: buddy (Linux の mm/page_alloc.c と同じ考え)
//
// 空いているページは、2^k ページ (k = 0..=MAX_ORDER: 4 KiB から 4 MiB) のかたまりにして、k ごとのリストに積む。
// かたまりは大きさの境目から始まる (ページの番号が 2^k の倍数)。返されたかたまりは、となりの同じ大きさのかたまり
// (buddy: 番号の k ビット目だけちがうもの) も空いていれば 1 つにまとめなおす。だから RAM をひととおり使ったあとでも、
// 返ってきたページから続いたページが作れる (カーネルのヒープを足す: heap.rs の grow)。前はフリーリスト 1 本で、
// まだ一度も使っていない領域からしか続いたものを作れず、それを使いきるとヒープは足せなかった
//
// かたまりの印 (頭のしるし、リストの次と前、大きさ) はページの中ではなく、RAM のおしまいに置く表 (ページあたり 8 バイトと
// 1 ビット。Linux の mem_map のようなもの) に書く。空いているページには触らないので、起動のときに触るのは表の少しだけ
// (VM のホストは、触ったところにだけメモリを用意する。頭のページに書くと 4 MiB ごとに 1 ページ触ることになり、2 GiB の
// VM の起動が 8 秒おそくなった: ホストが 2 MiB ずつ用意するので)。頭のしるしだけ 0 で埋め、ほかはしるしのあるところだけ
// 読む。表の大きさは RAM に合わせる (bss に MAX_RAM の分を置くと、小さな機械でもむだになり、bss のならびも変わる)。
// 起動のときのヒープの区画 (heap::boot_extra) は、buddy を作る前にカーネルのすぐうしろから切り出す (Linux の memblock)
//
// 1 枚ずつ返ってきたものは、すぐにはまとめず HOT に少し置く (LIFO: すぐまた取るならキャッシュにのっていて、
// まとめて分けるのをくりかえさない。Linux の per-cpu のリストと同じ)。多くなったら古い半分をまとめにいく。
// 続いたものがほしいときは、まず HOT をみなまとめる
//
// ユーザーのページは複数のアドレス空間で共有されることがある (fork のコピーオンライト)
// ので、ページごとに参照の数を持つ。alloc で 1、get で +1、put で -1 し、0 になったら返す。
use crate::memlayout::{p2v, pg_round_up, phystop, v2p, ram_base, PGSIZE};
use crate::spinlock::SpinLock;
use core::ptr;
use core::sync::atomic::{AtomicU16, Ordering};

/// いちばん大きなかたまりは 2^MAX_ORDER ページ (4 MiB。Linux の MAX_PAGE_ORDER と同じ。/proc/buddyinfo の列も同じ数)
pub const MAX_ORDER: usize = 10;
/// HOT に置いておく 1 枚ずつのページの上限 (1 MiB)
const HOT_MAX: usize = 256;

/// RAM のページごとの参照の数 (RAM は memlayout::MAX_RAM まで)
const MAX_PAGES: usize = crate::memlayout::MAX_RAM / PGSIZE;
/// ページごとの、使っている人の数。大きなロックなしのページフォルト (vm.rs fast_fault) からも触るので atomic
static REFS: [AtomicU16; MAX_PAGES] = [const { AtomicU16::new(0) }; MAX_PAGES];

fn ref_slot(page: *mut u8) -> &'static AtomicU16 {
    &REFS[(v2p(page as usize) - ram_base()) / PGSIZE]
}

/// ページの番号 (RAM の先頭から)
fn idx_of(page: *mut u8) -> usize {
    (v2p(page as usize) - ram_base()) / PGSIZE
}

fn page_of(i: usize) -> *mut u8 {
    p2v(ram_base() + i * PGSIZE) as *mut u8
}

/// リストのおしまい (表の次と前で)
const NONE: u32 = u32::MAX;
/// 表の前は 24 ビット (ページの番号は MAX_PAGES = 2^22 まで)。みな 1 ならおしまい
const PREV_MASK: u32 = 0xff_ffff;

struct Kmem {
    /// 大きさごとのリストの頭 (ページの番号)
    lists: [u32; MAX_ORDER + 1],
    counts: [usize; MAX_ORDER + 1],
    /// リストにある空きのページの数
    nbuddy: usize,
    /// 1 枚ずつ返ってきた、まだまとめていないページの番号 (古いものから)
    hot: [u32; HOT_MAX],
    nhot: usize,
    /// 配るページの番号の範囲 [lo, hi) (カーネルのうしろから表の前まで)
    lo: usize,
    hi: usize,
    /// 表 (ページの番号で引く): 空いているかたまりの頭か (1 ビット) と、その頭の、リストの次 (0..32 ビット)、前 (32..56)、
    /// 大きさ (56..64)。1 つにまとめるのは、1 回に触るところを少なくするため (TCG では触るページごとに TLB を使う)
    heads: *mut u64,
    meta: *mut u64,
}

unsafe impl Send for Kmem {}

static KMEM: SpinLock<Kmem> = SpinLock::new(Kmem {
    lists: [NONE; MAX_ORDER + 1],
    counts: [0; MAX_ORDER + 1],
    nbuddy: 0,
    hot: [0; HOT_MAX],
    nhot: 0,
    lo: 0,
    hi: 0,
    heads: ptr::null_mut(),
    meta: ptr::null_mut(),
});

impl Kmem {
    fn is_head(&self, i: usize) -> bool {
        unsafe { *self.heads.add(i / 64) & (1 << (i % 64)) != 0 }
    }

    fn meta_of(&self, i: usize) -> u64 {
        unsafe { *self.meta.add(i) }
    }

    fn next_of(&self, i: usize) -> u32 {
        self.meta_of(i) as u32
    }

    fn prev_of(&self, i: usize) -> u32 {
        match (self.meta_of(i) >> 32) as u32 & PREV_MASK {
            PREV_MASK => NONE,
            p => p,
        }
    }

    fn order_of(&self, i: usize) -> usize {
        (self.meta_of(i) >> 56) as usize
    }

    fn set_next(&mut self, i: usize, next: u32) {
        unsafe { *self.meta.add(i) = (*self.meta.add(i) & !0xffff_ffff) | next as u64 };
    }

    fn set_prev(&mut self, i: usize, prev: u32) {
        let p = (prev & PREV_MASK) as u64;
        unsafe { *self.meta.add(i) = (*self.meta.add(i) & !((PREV_MASK as u64) << 32)) | p << 32 };
    }

    /// i が 2^k の空いているかたまりの頭か
    fn free_block(&self, i: usize, k: usize) -> bool {
        i >= self.lo && i < self.hi && self.is_head(i) && self.order_of(i) == k
    }

    fn push(&mut self, i: usize, k: usize) {
        let next = self.lists[k];
        unsafe { *self.meta.add(i) = (k as u64) << 56 | (PREV_MASK as u64) << 32 | next as u64 };
        if next != NONE {
            self.set_prev(next as usize, i as u32);
        }
        self.lists[k] = i as u32;
        self.counts[k] += 1;
        unsafe { *self.heads.add(i / 64) |= 1 << (i % 64) };
        self.nbuddy += 1 << k;
    }

    fn unlink(&mut self, i: usize, k: usize) {
        let (next, prev) = (self.next_of(i), self.prev_of(i));
        if prev == NONE {
            self.lists[k] = next;
        } else {
            self.set_next(prev as usize, next);
        }
        if next != NONE {
            self.set_prev(next as usize, prev);
        }
        self.counts[k] -= 1;
        unsafe { *self.heads.add(i / 64) &= !(1 << (i % 64)) };
        self.nbuddy -= 1 << k;
    }

    /// 2^k のかたまり i を返す: buddy が空いていればまとめて、1 つ大きくしてくりかえす
    fn give(&mut self, mut i: usize, mut k: usize) {
        while k < MAX_ORDER {
            let b = i ^ (1 << k);
            if !self.free_block(b, k) {
                break;
            }
            self.unlink(b, k);
            i &= !(1 << k);
            k += 1;
        }
        self.push(i, k);
    }

    /// [s, e) のページを返す (境目にそろった、いちばん大きなかたまりに分けて)。うしろから返すので、リストの頭は
    /// 低いほう (起動のあとは、カーネルのすぐうしろから順に配る。前のアロケータと同じ)
    fn give_range(&mut self, s: usize, mut e: usize) {
        while e > s {
            let mut k = MAX_ORDER;
            while k > 0 && ((e - s) < (1 << k) || (e - (1 << k)) & ((1 << k) - 1) != 0) {
                k -= 1;
            }
            e -= 1 << k;
            self.give(e, k);
        }
    }

    /// 2^k のかたまりを 1 つ (大きなものしかなければ半分ずつに分けて、うしろの半分はリストへ)
    fn take(&mut self, k: usize) -> Option<usize> {
        let mut j = (k..=MAX_ORDER).find(|&j| self.counts[j] > 0)?;
        let i = self.lists[j] as usize;
        self.unlink(i, j);
        while j > k {
            j -= 1;
            self.push(i + (1 << j), j);
        }
        Some(i)
    }

    /// HOT の古いものから n 枚をまとめにいく
    fn drain(&mut self, n: usize) {
        let n = n.min(self.nhot);
        for h in 0..n {
            let i = self.hot[h] as usize;
            self.give(i, 0);
        }
        self.hot.copy_within(n..self.nhot, 0);
        self.nhot -= n;
    }

    /// 続いた n ページ (n が 2^MAX_ORDER をこえるときは、となりあった大きなかたまりを並べて)。頭の番号
    fn take_run(&mut self, n: usize) -> Option<usize> {
        let big = 1 << MAX_ORDER;
        if n <= big {
            let k = n.next_power_of_two().trailing_zeros() as usize;
            let i = self.take(k)?;
            self.give_range(i + n, i + (1 << k));
            return Some(i);
        }
        // 大きなかたまりのリストから、並びの頭 (前が空いた大きなかたまりでないもの) を探し、続く数を数える
        let blocks = n.div_ceil(big);
        let mut f = self.lists[MAX_ORDER];
        while f != NONE {
            let i = f as usize;
            f = self.next_of(i);
            if i >= big && self.free_block(i - big, MAX_ORDER) {
                continue;
            }
            let mut m = 1;
            while m < blocks && self.free_block(i + m * big, MAX_ORDER) {
                m += 1;
            }
            if m < blocks {
                continue;
            }
            for b in 0..blocks {
                self.unlink(i + b * big, MAX_ORDER);
            }
            self.give_range(i + n, i + blocks * big);
            return Some(i);
        }
        None
    }
}

/// 起動のとき (heap::init の前に)。early バイトをカーネルのすぐうしろから先に切り出して返し (起動のときのヒープの区画:
/// Linux の memblock のように buddy を作る前に。前のアロケータと同じ場所になる)、残りを buddy に積む
pub fn init(early: usize) -> Option<*mut u8> {
    unsafe extern "C" {
        static __kernel_end: u8;
    }
    let mut k = KMEM.lock();
    let mut lo = idx_of(pg_round_up(&raw const __kernel_end as usize) as *mut u8);
    let hi = ((phystop() - ram_base()) / PGSIZE).min(MAX_PAGES);
    // 表は RAM のおしまいに (ページの番号 0..hi で引く: 頭のしるしに 1 ビット、次と前と大きさに 8 バイト)
    let words = hi.div_ceil(64);
    let top = hi - (words * 8 + hi * 8).div_ceil(PGSIZE);
    let base = page_of(top);
    k.heads = base as *mut u64;
    unsafe { ptr::write_bytes(k.heads, 0, words) };
    k.meta = unsafe { base.add(words * 8) } as *mut u64;
    let n = pg_round_up(early) / PGSIZE;
    let got = if early > 0 && lo + n < top {
        lo += n;
        Some(page_of(lo - n))
    } else {
        None
    };
    k.lo = lo;
    k.hi = top;
    k.give_range(lo, top);
    got
}

/// 続いた bytes (カーネルのヒープを足りなくなったときに足すため: heap.rs の grow。ページの参照の数は持たず、返さない)。返ってきたページもまとめてあるので、RAM をひととおり使ったあとでも、空きがそろっていれば取れる。
/// なければ None
pub fn take_contiguous(bytes: usize) -> Option<*mut u8> {
    let n = pg_round_up(bytes) / PGSIZE;
    let mut k = KMEM.lock();
    let nhot = k.nhot;
    k.drain(nhot);
    k.take_run(n).map(page_of)
}

/// 仮想アドレス (KBASE 側) のページを返す
pub fn free(page: *mut u8) {
    ref_slot(page).store(0, Ordering::Relaxed);
    let i = idx_of(page);
    let mut k = KMEM.lock();
    if k.nhot == HOT_MAX {
        k.drain(HOT_MAX / 2);
    }
    let n = k.nhot;
    k.hot[n] = i as u32;
    k.nhot += 1;
}

/// 0 埋めしたページを 1 枚。なければ None
pub fn alloc() -> Option<*mut u8> {
    let page = alloc_dirty()?;
    zero_page(page);
    Some(page)
}

/// ページを 0 にする。DC ZVA (1 命令で 64 バイトなど) が使えればそれで: QEMU の TCG ではホストの
/// memset 1 回になり、8 バイトずつ書くより何十倍も速い (本物の CPU でも速い。Linux の clear_page と同じ)
pub fn zero_page(page: *mut u8) {
    let dczid: u64;
    unsafe { core::arch::asm!("mrs {}, dczid_el0", out(reg) dczid) };
    // DZP (bit 4) が立っていれば使えない。ブロックの大きさは 4 << BS (bit 3:0) バイト
    if dczid & (1 << 4) == 0 {
        let bs = 4usize << (dczid & 0xf);
        let mut a = page as usize;
        let end = a + PGSIZE;
        while a < end {
            unsafe { core::arch::asm!("dc zva, {}", in(reg) a) };
            a += bs;
        }
    } else {
        unsafe { ptr::write_bytes(page, 0, PGSIZE) };
    }
}

/// 中身を 0 にしないページ (すぐに全部書きこむところ用: ディスクのブロックのキャッシュ、パイプの中身。
/// 前に使った人のデータが残っているので、書いていないところをユーザーに見せないこと)
pub fn alloc_dirty() -> Option<*mut u8> {
    // わざと取れないことに (/proc/ai/ctl の fail ... page。fail.rs)
    if crate::fail::page() {
        return None;
    }
    let mut k = KMEM.lock();
    let i = if k.nhot > 0 {
        k.nhot -= 1;
        k.hot[k.nhot] as usize
    } else {
        k.take(0)?
    };
    drop(k);
    let page = page_of(i);
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

/// page cycle (/proc/ai/ctl): 空いているページを (MIN_FREE_PAGES だけ残して) みな取ってから、取った順に返す。RAM を
/// ひととおり使ったあとの様子を作る (返ったページから続いたものが作れるか: heap の grow)。回したページの数
pub fn cycle() -> usize {
    let keep = crate::sysctl::MIN_FREE_PAGES.load(Ordering::Relaxed);
    let mut head: *mut usize = ptr::null_mut();
    let mut n = 0;
    while nfree() > keep {
        let Some(p) = alloc_dirty() else { break };
        unsafe { *(p as *mut usize) = head as usize };
        head = p as *mut usize;
        n += 1;
    }
    while !head.is_null() {
        let next = unsafe { *head } as *mut usize;
        put(head as *mut u8);
        head = next;
    }
    n
}

pub fn nfree() -> usize {
    let k = KMEM.lock();
    k.nbuddy + k.nhot
}

/// 大きさ (2^k ページ) ごとの空いているかたまりの数 (HOT のものは入れない: Linux の per-cpu のものと同じ) と HOT の枚数
pub fn orders() -> ([usize; MAX_ORDER + 1], usize) {
    let k = KMEM.lock();
    (k.counts, k.nhot)
}

/// /proc/buddyinfo (Linux と同じ形。ゾーンは 1 つ)
pub fn buddyinfo() -> alloc::string::String {
    let (c, _) = orders();
    let mut s = alloc::string::String::from("Node 0, zone   Normal ");
    for n in c {
        s.push_str(&alloc::format!("{:6} ", n));
    }
    s.push('\n');
    s
}

/// page check (/proc/ai/ctl): 空きの決まりを確かめて kmsg に (page: check ok / FAILED)。リストのかたまりが境目に
/// そろい、範囲の中で、重ならず、まとめ残しがなく (buddy が同じ大きさで空いていない)、数があい、中のページの参照の
/// 数が 0。HOT のページも。ロックを持ったまま、空きのページをみな見る
pub fn check() {
    let r = {
        let k = KMEM.lock();
        check_locked(&k)
    };
    match r {
        Ok(n) => crate::println!("page: check ok ({} free pages)", n),
        Err(e) => crate::println!("page: check FAILED: {}", e),
    }
}

fn check_locked(k: &Kmem) -> Result<usize, &'static str> {
    let mut pages = 0;
    let mut nheads = 0;
    for o in 0..=MAX_ORDER {
        let mut n = 0;
        let mut prev = NONE;
        let mut f = k.lists[o];
        while f != NONE {
            let i = f as usize;
            if i < k.lo || i + (1 << o) > k.hi {
                return Err("a free block is out of range");
            }
            if i & ((1 << o) - 1) != 0 {
                return Err("a free block is not aligned to its size");
            }
            if !k.is_head(i) || k.order_of(i) != o {
                return Err("a listed block has no head mark or a wrong order");
            }
            if k.prev_of(i) != prev {
                return Err("a list link back is wrong");
            }
            if (1..1 << o).any(|j| k.is_head(i + j)) {
                return Err("free blocks overlap");
            }
            if o < MAX_ORDER && k.free_block(i ^ (1 << o), o) {
                return Err("two free buddies are not merged");
            }
            if (0..1 << o).any(|j| REFS[i + j].load(Ordering::Relaxed) != 0) {
                return Err("a free page is still referenced");
            }
            n += 1;
            if n > k.counts[o] {
                return Err("a list is longer than its count (a loop?)");
            }
            prev = f;
            f = k.next_of(i);
        }
        if n != k.counts[o] {
            return Err("a list count is wrong");
        }
        pages += n << o;
        nheads += n;
    }
    if pages != k.nbuddy {
        return Err("the free page count is wrong");
    }
    let words = unsafe { core::slice::from_raw_parts(k.heads, k.hi.div_ceil(64)) };
    if words.iter().map(|w| w.count_ones() as usize).sum::<usize>() != nheads {
        return Err("a head mark is not on any list");
    }
    for (h, &i) in k.hot[..k.nhot].iter().enumerate() {
        let i = i as usize;
        if i < k.lo || i >= k.hi || k.is_head(i) || REFS[i].load(Ordering::Relaxed) != 0 {
            return Err("a hot page is out of range, listed, or referenced");
        }
        if k.hot[..h].contains(&(i as u32)) {
            return Err("a hot page is there twice");
        }
    }
    // HOT のページが、リストのかたまりの中にないか (頭だけでなく)
    for &i in &k.hot[..k.nhot] {
        let i = i as usize;
        for o in 1..=MAX_ORDER {
            let s = i & !((1 << o) - 1);
            if k.free_block(s, o) {
                return Err("a hot page is inside a free block");
            }
        }
    }
    Ok(pages + k.nhot)
}
