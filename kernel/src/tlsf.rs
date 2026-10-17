// TLSF (two-level segregated fit): カーネルヒープ (heap.rs) の空きの管理。取るのも返すのも O(1)。
//   空きは大きさのクラスごとのリスト (1 KiB までは 16 バイトきざみ、それより上は 2 の累乗のあいだを 16 に分けたもの)
//   に入れ、空きのあるクラスはビットで覚える。取るときは、足りるクラスのうちいちばん小さいものの頭を取り、
//   余れば切って空きに戻す。返すときは、となりのブロックが空いていればつなげる (境界の印: ヘッダの FREE と
//   PREV_FREE、空きのブロックの終わりの語の大きさ)。前のものは、アドレスの順のリストを頭からなめていた
//   (取るのに 20、返すのに 40 ほど。fsbench の時間の 2 割)。
//   core のほかを使わないので、test/heap.py が開発のマシンでビルドして、でたらめに取り・返して確かめる (check)
//
// ブロック: [大きさ | FREE | PREV_FREE] [2 語目] ...   大きさはヘッダこみで 16 の倍数、MIN_BLOCK 以上
//   使っているもの: 2 語目は使う人のもの (heap.rs は取ったところの番号)。中身は 16 バイト目から
//   空いているもの: 2 語目と 3 語目がリストの次と前、終わりの語に大きさ (うしろのブロックが、空いた前をたどる)
//   となりあう空きはない (返すときにつなげる)。空きの前のブロックは、いつも使っているもの
// 区画 (add_region) の終わりの 16 バイトは、大きさ 0 の使っている印 (番兵)。うしろへはそこで止まる
use core::ptr;

/// ブロックの大きさのきざみ (中身もここにそろう)
pub const ALIGN: usize = 16;
/// いちばん小さいブロック (ヘッダ、次、前、終わりの大きさ)
pub const MIN_BLOCK: usize = 32;
const FREE: usize = 1;
const PREV_FREE: usize = 2;
const FLAGS: usize = ALIGN - 1;
/// これより小さいものは 16 バイトきざみのクラス
const SMALL: usize = 1024;
/// 2 の累乗のあいだを 1 << SL_LOG2 に分ける
const SL_LOG2: usize = 4;
const FL_MIN: usize = 10;
const FL_MAX: usize = 39;
const CLASSES: usize = SMALL / ALIGN + ((FL_MAX - FL_MIN + 1) << SL_LOG2);
const WORDS: usize = CLASSES.div_ceil(64);

pub struct Tlsf {
    /// 空きのあるクラス
    bits: [u64; WORDS],
    /// クラスごとの空きのリストの頭
    heads: [*mut u8; CLASSES],
    /// 足した区画 (check のため)
    /// 区画 (check が見る)。起動のときの 2 つと、足りなくなって足したもの (heap.rs の grow)
    regions: [(usize, usize); 64],
    nregions: usize,
}

unsafe impl Send for Tlsf {}

#[inline]
unsafe fn hdr(b: *mut u8) -> usize {
    unsafe { *(b as *const usize) }
}

#[inline]
unsafe fn set_hdr(b: *mut u8, v: usize) {
    unsafe { *(b as *mut usize) = v }
}

#[inline]
unsafe fn next(b: *mut u8) -> *mut u8 {
    unsafe { *(b.add(8) as *const *mut u8) }
}

#[inline]
unsafe fn set_next(b: *mut u8, v: *mut u8) {
    unsafe { *(b.add(8) as *mut *mut u8) = v }
}

#[inline]
unsafe fn prev(b: *mut u8) -> *mut u8 {
    unsafe { *(b.add(16) as *const *mut u8) }
}

#[inline]
unsafe fn set_prev(b: *mut u8, v: *mut u8) {
    unsafe { *(b.add(16) as *mut *mut u8) = v }
}

/// ブロック b の大きさ (ヘッダこみ)
#[inline]
pub unsafe fn block_size(b: *mut u8) -> usize {
    unsafe { hdr(b) & !FLAGS }
}

/// 大きさ size の空きを入れるクラス
fn class_of(size: usize) -> usize {
    if size < SMALL {
        size / ALIGN
    } else {
        let fl = (usize::BITS - 1 - size.leading_zeros()) as usize;
        let sl = (size >> (fl - SL_LOG2)) & ((1 << SL_LOG2) - 1);
        SMALL / ALIGN + ((fl - FL_MIN) << SL_LOG2) + sl
    }
}

/// need バイトを探しはじめるクラス: ここから上のクラスの空きは、みな need 以上
fn class_for(need: usize) -> usize {
    if need < SMALL {
        need / ALIGN
    } else {
        let fl = (usize::BITS - 1 - need.leading_zeros()) as usize;
        class_of(need.saturating_add((1 << (fl - SL_LOG2)) - 1))
    }
}

impl Tlsf {
    pub const fn new() -> Self {
        Tlsf { bits: [0; WORDS], heads: [ptr::null_mut(); CLASSES], regions: [(0, 0); 64], nregions: 0 }
    }

    /// c から上で、空きのあるいちばん小さいクラス
    fn first_from(&self, c: usize) -> Option<usize> {
        let mut w = c / 64;
        if w >= WORDS {
            return None;
        }
        let mut bits = self.bits[w] & (!0u64 << (c % 64));
        loop {
            if bits != 0 {
                return Some(w * 64 + bits.trailing_zeros() as usize);
            }
            w += 1;
            if w == WORDS {
                return None;
            }
            bits = self.bits[w];
        }
    }

    unsafe fn push(&mut self, b: *mut u8, size: usize) {
        let c = class_of(size);
        let h = self.heads[c];
        unsafe {
            set_next(b, h);
            set_prev(b, ptr::null_mut());
            if !h.is_null() {
                set_prev(h, b);
            }
        }
        self.heads[c] = b;
        self.bits[c / 64] |= 1 << (c % 64);
    }

    unsafe fn unlink(&mut self, b: *mut u8, size: usize) {
        let c = class_of(size);
        unsafe {
            let (n, p) = (next(b), prev(b));
            if p.is_null() {
                self.heads[c] = n;
                if n.is_null() {
                    self.bits[c / 64] &= !(1 << (c % 64));
                }
            } else {
                set_next(p, n);
            }
            if !n.is_null() {
                set_prev(n, p);
            }
        }
    }

    /// b から size バイトを空きにして、リストに入れる (前のブロックは使っているもの)
    unsafe fn make_free(&mut self, b: *mut u8, size: usize) {
        unsafe {
            set_hdr(b, size | FREE);
            *(b.add(size - 8) as *mut usize) = size;
            self.push(b, size);
            let after = b.add(size);
            set_hdr(after, hdr(after) | PREV_FREE);
        }
    }

    /// [start, start + len) を空きとして足す。start は ALIGN にそろったところ、len は ALIGN の倍数で 64 以上
    /// (2^40 より小さい)。終わりの ALIGN バイトは番兵になる
    pub unsafe fn add_region(&mut self, start: *mut u8, len: usize) {
        let usable = len - ALIGN;
        unsafe {
            set_hdr(start.add(usable), 0);
            self.make_free(start, usable);
        }
        if self.nregions < self.regions.len() {
            self.regions[self.nregions] = (start as usize, len);
            self.nregions += 1;
        }
    }

    /// need バイト (ヘッダこみ、ALIGN の倍数、MIN_BLOCK 以上) 以上のブロック。なければ null。
    /// 2 語目は使う人のもの (ここでは書かない)
    pub unsafe fn alloc(&mut self, need: usize) -> *mut u8 {
        let b = match self.first_from(class_for(need)) {
            Some(c) => self.heads[c],
            // 上のクラスに空きがなくても、need と同じクラスに足りるものがあるかもしれない (1 KiB より大きなものだけ)
            None => self.fit_in(class_of(need), need),
        };
        if b.is_null() {
            return b;
        }
        unsafe {
            let size = block_size(b);
            self.unlink(b, size);
            if size - need >= MIN_BLOCK {
                // 切って、うしろを空きに戻す
                set_hdr(b, need);
                self.make_free(b.add(need), size - need);
            } else {
                set_hdr(b, size);
                let after = b.add(size);
                set_hdr(after, hdr(after) & !PREV_FREE);
            }
        }
        b
    }

    /// クラス c のリストから need 以上のものを (なければ null)
    fn fit_in(&self, c: usize, need: usize) -> *mut u8 {
        if c >= CLASSES {
            return ptr::null_mut();
        }
        let mut b = self.heads[c];
        while !b.is_null() {
            if unsafe { block_size(b) } >= need {
                return b;
            }
            b = unsafe { next(b) };
        }
        ptr::null_mut()
    }

    /// alloc で取ったブロックを返す (となりの空きとつなげる)
    pub unsafe fn free(&mut self, b: *mut u8) {
        unsafe {
            let mut b = b;
            let mut size = block_size(b);
            let after = b.add(size);
            if hdr(after) & FREE != 0 {
                let s = block_size(after);
                self.unlink(after, s);
                size += s;
            }
            if hdr(b) & PREV_FREE != 0 {
                let ps = *(b.sub(8) as *const usize);
                let p = b.sub(ps);
                self.unlink(p, ps);
                b = p;
                size += ps;
            }
            self.make_free(b, size);
        }
    }

    /// (空きのブロックの数, いちばん大きな空き, 空きのバイト)。リストをみななめる (/proc/ai/heap のときだけ)
    pub fn stats(&self) -> (usize, usize, usize) {
        let (mut n, mut big, mut bytes) = (0, 0, 0);
        for &h in self.heads.iter() {
            let mut b = h;
            while !b.is_null() {
                let s = unsafe { block_size(b) };
                n += 1;
                big = big.max(s);
                bytes += s;
                b = unsafe { next(b) };
            }
        }
        (n, big, bytes)
    }

    /// 決まりが守られているか (test/heap.py)。区画を頭からたどって: 大きさ、PREV_FREE、となりあう空きがない、
    /// 空きの終わりの大きさ、番兵。リストとビット: 空きだけが正しいクラスに 1 度ずつ、前へのつながり
    pub unsafe fn check(&self) -> Result<(), &'static str> {
        let mut free_blocks = 0;
        for &(start, len) in &self.regions[..self.nregions] {
            let end = start + len - ALIGN;
            let mut b = start as *mut u8;
            let mut prev_free = false;
            loop {
                let h = unsafe { hdr(b) };
                if (h & PREV_FREE != 0) != prev_free {
                    return Err("PREV_FREE does not match the block before");
                }
                if b as usize == end {
                    // 大きさ 0 で使っている (PREV_FREE だけは立っていてよい)
                    if h & !PREV_FREE != 0 {
                        return Err("bad sentinel");
                    }
                    break;
                }
                let s = h & !FLAGS;
                if s < MIN_BLOCK || s % ALIGN != 0 || b as usize + s > end {
                    return Err("bad block size");
                }
                let free = h & FREE != 0;
                if free {
                    if prev_free {
                        return Err("two free blocks side by side");
                    }
                    if unsafe { *(b.add(s - 8) as *const usize) } != s {
                        return Err("bad footer");
                    }
                    free_blocks += 1;
                }
                prev_free = free;
                b = unsafe { b.add(s) };
            }
        }
        let mut listed = 0;
        for c in 0..CLASSES {
            let bit = self.bits[c / 64] & (1 << (c % 64)) != 0;
            if bit == self.heads[c].is_null() {
                return Err("class bit does not match its list");
            }
            let (mut b, mut p) = (self.heads[c], ptr::null_mut());
            while !b.is_null() {
                let h = unsafe { hdr(b) };
                if h & FREE == 0 {
                    return Err("block in use on a free list");
                }
                if class_of(h & !FLAGS) != c {
                    return Err("block in the wrong class");
                }
                if unsafe { prev(b) } != p {
                    return Err("bad prev link");
                }
                listed += 1;
                if listed > free_blocks {
                    return Err("more listed than free (a cycle or a lost block)");
                }
                p = b;
                b = unsafe { next(b) };
            }
        }
        if listed != free_blocks {
            return Err("free blocks missing from the lists");
        }
        Ok(())
    }
}
