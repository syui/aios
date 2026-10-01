// ユーザー空間 (TTBR0, 4KiB granule, 39bit VA, L1-L3) とデマンドページング
//
// アドレス空間は「領域」(Vma: 範囲、PROT_*、無名かファイルか) の並びとページテーブルからなる。
// ページは最初に触れたときに作る (ページフォールト):
//   無名      0 で埋めたページ
//   ファイル  そのときにファイルから読む (ファイルの終わりより先は 0)
// fork はページを写さずに共有し、書けるページは両方で読み取り専用 + COW の印にする。
// 書こうとしたときに、まだ共有されていれば写し、自分だけなら書けるように戻す (コピーオンライト)。
// ページの共有の数は kalloc が持つ。
//
// MAP_SHARED のファイルは、(ファイルシステム, inode, ページ番号) ごとに 1 枚のページを共有する
// (SHARED_FILE)。最初は読み取り専用で写し、書かれたら「書いた」印をつけて書けるようにする。
// 書いたページは msync、munmap、プロセスの終わり、sync でファイルに書き戻す (ファイルの終わりの先は捨てる)。
// read()/write() とは別なので、写している間に write() で書いたものは写しには映らない。
//
// PTE のうち、ハードウェアが見ないビットを使う:
//   COW       (55) 書けるはずだが共有しているので読み取り専用にしてあるページ
//   PROTNONE  (56) PROT_NONE にしたので無効にしてあるが、中身は持っているページ (VALID は 0)
use crate::kalloc;
use crate::memlayout::{p2v, v2p, PGSIZE};
use crate::vfs::InodeRef;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{fence, AtomicUsize, Ordering};

const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1; // L1/L2 ではテーブル、L3 ではページ
const PTE_ATTR_NORMAL: u64 = 1 << 2; // MAIR attr1
const PTE_USER: u64 = 1 << 6; // AP[1]: EL0 から触れる
const PTE_RDONLY: u64 = 1 << 7; // AP[2]
const PTE_SH_INNER: u64 = 3 << 8;
const PTE_AF: u64 = 1 << 10;
const PTE_NG: u64 = 1 << 11;
const PTE_PXN: u64 = 1 << 53;
const PTE_UXN: u64 = 1 << 54;
const PTE_COW: u64 = 1 << 55;
const PTE_PROTNONE: u64 = 1 << 56;
const PTE_ADDR: u64 = 0x0000_ffff_ffff_f000;

pub const MAXVA: usize = 1 << 39;

pub const PROT_READ: u8 = 1;
pub const PROT_WRITE: u8 = 2;
pub const PROT_EXEC: u8 = 4;
pub const PROT_RW: u8 = PROT_READ | PROT_WRITE;

pub const fn pg_down(a: usize) -> usize {
    a & !(PGSIZE - 1)
}

pub const fn pg_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}

// ---- MAP_SHARED のファイルのページ ----

type FileKey = (usize, u64, usize);

struct Cached {
    page: *mut u8,
    dirty: bool,
    ino: InodeRef,
}

/// 表も 1 つ参照を持つ。だれも写さなくなったら (参照が表だけになったら) 書き戻して捨てる
static mut SHARED_FILE: BTreeMap<FileKey, Cached> = BTreeMap::new();

fn shared_file() -> &'static mut BTreeMap<FileKey, Cached> {
    unsafe { &mut *(&raw mut SHARED_FILE) }
}

fn file_key(ino: &InodeRef, off: usize) -> FileKey {
    let (fs, i) = ino.id();
    (fs, i, off / PGSIZE)
}

fn write_back(key: &FileKey, c: &Cached) {
    let size = c.ino.meta().size as usize;
    let off = key.2 * PGSIZE;
    if off < size {
        let n = (size - off).min(PGSIZE);
        let data = unsafe { core::slice::from_raw_parts(c.page, n) };
        if c.ino.write_at(off, data).is_err() {
            println!("vm: write back of a shared mapping failed");
        }
    }
}

/// 書いたページをファイルに書き戻す (sync)。印はそのまま (まだ書けるように写っているかもしれない)
pub fn sync_shared() {
    for (k, c) in shared_file().iter().filter(|(_, c)| c.dirty) {
        write_back(k, c);
    }
}

/// keys のページを書き戻し、もうだれも写していないものは捨てる
fn release_shared(keys: &[FileKey]) {
    for k in keys {
        let Some(c) = shared_file().get(k) else { continue };
        if c.dirty {
            write_back(k, c);
        }
        if kalloc::refs(c.page) <= 1 {
            let c = shared_file().remove(k).unwrap();
            kalloc::put(c.page);
        }
    }
}

/// 領域の中身のもと
#[derive(Clone)]
pub enum Backing {
    Anon,
    /// 領域の先頭がファイルの off。va が fend 以上のところは 0 (ELF の .bss の始まりなど)
    File { ino: InodeRef, off: usize, fend: usize },
}

#[derive(Clone)]
pub struct Vma {
    pub end: usize,
    pub prot: u8,
    /// MAP_SHARED (fork しても共有のまま)
    pub shared: bool,
    pub back: Backing,
}

/// ページフォールトの結果
pub enum FaultErr {
    /// 領域がない (SEGV_MAPERR)
    NoMap,
    /// 権限がない (SEGV_ACCERR)
    Access,
    NoMem,
}

/// ルートは kalloc したページ (カーネル仮想アドレス)
pub struct PageTable {
    root: *mut u64,
    vmas: BTreeMap<usize, Vma>,
    /// 大きなロックなしでユーザーのメモリに書いている CPU の数 (copy_out_nofault)。
    /// ページを手放す前に 0 になるのを待つ (quiesce)
    fast_users: AtomicUsize,
}

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * (3 - level))) & 0x1ff
}

fn table_at(e: u64) -> *mut u64 {
    p2v((e & PTE_ADDR) as usize) as *mut u64
}

/// ページを持っている PTE か (PROT_NONE で無効にしてあるものも)
fn has_page(e: u64) -> bool {
    e & PTE_VALID != 0 || e & PTE_PROTNONE != 0
}

fn page_of(e: u64) -> *mut u8 {
    table_at(e) as *mut u8
}

/// pa のページを prot で。cow なら書けるはずでも読み取り専用にして印をつける
fn make_pte(pa: u64, prot: u8, cow: bool) -> u64 {
    if prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 {
        return pa | PTE_PROTNONE;
    }
    let mut b = pa | PTE_VALID | PTE_TABLE | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF | PTE_NG | PTE_USER | PTE_PXN;
    if prot & PROT_WRITE == 0 || cow {
        b |= PTE_RDONLY;
    }
    if cow && prot & PROT_WRITE != 0 {
        b |= PTE_COW;
    }
    if prot & PROT_EXEC == 0 {
        b |= PTE_UXN;
    }
    b
}

/// 共有のぐあいを見て PTE を作りなおす (private で他と共有していれば COW)
fn remake_pte(e: u64, prot: u8, shared_vma: bool) -> u64 {
    let pa = e & PTE_ADDR;
    let cow = !shared_vma && kalloc::refs(page_of(e)) > 1;
    make_pte(pa, prot, cow)
}

// TLB はすべての CPU に (inner shareable) 消す: 同じアドレス空間のスレッドがほかの CPU で動いているかも
fn flush_va(va: usize) {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vaae1is, {}", "dsb ish", "isb", in(reg) (va >> 12) as u64);
    }
}

fn flush_all() {
    unsafe { core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb") };
}

/// 何も写していない L1 (スケジューラの中で TTBR0 に載せる)
#[repr(C, align(4096))]
struct Empty([u64; 512]);
static EMPTY_L1: Empty = Empty([0; 512]);

/// この CPU の TTBR0 からプロセスのページ表を外す
pub fn deactivate() {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            in(reg) v2p(&raw const EMPTY_L1 as usize),
        );
    }
}

impl PageTable {
    pub fn new() -> Option<Self> {
        Some(Self { root: kalloc::alloc()? as *mut u64, vmas: BTreeMap::new(), fast_users: AtomicUsize::new(0) })
    }

    /// TTBR0 に載せる物理アドレス
    pub fn root_pa(&self) -> usize {
        v2p(self.root as usize)
    }

    /// L3 の PTE へのポインタ。alloc なら途中のテーブルを作る
    fn walk(&self, va: usize, alloc: bool) -> Option<*mut u64> {
        if va >= MAXVA {
            return None;
        }
        let mut table = self.root;
        for level in 1..3 {
            let pte = unsafe { table.add(index(va, level)) };
            let e = unsafe { *pte };
            if e & PTE_VALID != 0 {
                table = table_at(e);
            } else {
                if !alloc {
                    return None;
                }
                let next = kalloc::alloc()? as *mut u64;
                // 0 にした中身が見えてから、ほかの CPU (copy_out_nofault) にたどらせる
                unsafe { core::arch::asm!("dmb ishst") };
                unsafe { *pte = v2p(next as usize) as u64 | PTE_VALID | PTE_TABLE };
                table = next;
            }
        }
        Some(unsafe { table.add(index(va, 3)) })
    }

    /// [start, end) のページを持っている PTE ごとに f (途中のテーブルがないところは飛ばす)
    fn each_pte(&self, start: usize, end: usize, mut f: impl FnMut(usize, *mut u64)) {
        let mut va = pg_down(start);
        while va < end {
            match self.walk(va, false) {
                Some(pte) => {
                    if has_page(unsafe { *pte }) {
                        f(va, pte);
                    }
                    va += PGSIZE;
                }
                // L3 のテーブルがない: 次の 2 MiB へ
                None => va = (va | 0x1f_ffff) + 1,
            }
        }
    }

    // ---- 領域 ----

    /// va を含む領域 (先頭, 領域)
    pub fn find(&self, va: usize) -> Option<(usize, &Vma)> {
        self.vmas.range(..=va).next_back().filter(|(_, v)| va < v.end).map(|(&s, v)| (s, v))
    }

    /// addr で領域を 2 つに分ける (addr が領域の中なら)
    fn split(&mut self, addr: usize) {
        let Some((start, v)) = self.find(addr).map(|(s, v)| (s, v.clone())) else { return };
        if start == addr {
            return;
        }
        let mut hi = v.clone();
        if let Backing::File { off, .. } = &mut hi.back {
            *off += addr - start;
        }
        self.vmas.get_mut(&start).unwrap().end = addr;
        self.vmas.insert(addr, hi);
    }

    /// [start, end) にある MAP_SHARED のファイルのページの鍵
    fn shared_keys(&self, start: usize, end: usize) -> Vec<FileKey> {
        let mut keys = Vec::new();
        for (&s, v) in self.vmas.range(..end) {
            if v.end <= start || !v.shared {
                continue;
            }
            if let Backing::File { ino, off, .. } = &v.back {
                let mut va = s.max(start);
                while va < v.end.min(end) {
                    keys.push(file_key(ino, off + (va - s)));
                    va += PGSIZE;
                }
            }
        }
        keys
    }

    /// msync: 範囲の書いたページをファイルへ
    pub fn msync(&self, start: usize, end: usize) {
        for k in self.shared_keys(pg_down(start), pg_up(end)) {
            if let Some(c) = shared_file().get(&k).filter(|c| c.dirty) {
                write_back(&k, c);
            }
        }
    }

    /// [start, end) に領域を置く (前にあったものは外す)。中身は触れたときに作る
    pub fn map(&mut self, start: usize, end: usize, prot: u8, shared: bool, back: Backing) -> Option<()> {
        if start >= end || end > MAXVA {
            return None;
        }
        self.unmap(start, end);
        self.vmas.insert(start, Vma { end, prot, shared, back });
        // 共有の無名メモリは、fork のあとも同じページを指すように今作る
        if shared && matches!(self.vmas[&start].back, Backing::Anon) {
            let mut va = start;
            while va < end {
                self.fault_page(va, false, true).ok()?;
                va += PGSIZE;
            }
        }
        Some(())
    }

    /// [start, end) の領域とページを外す
    pub fn unmap(&mut self, start: usize, end: usize) {
        let (start, end) = (pg_down(start), pg_up(end));
        if start >= end {
            return;
        }
        self.split(start);
        self.split(end);
        let file_keys = self.shared_keys(start, end);
        let keys: Vec<usize> = self.vmas.range(start..end).map(|(&k, _)| k).collect();
        for k in keys {
            self.vmas.remove(&k);
        }
        // PTE を消して TLB を消してから、ページを返す (ほかの CPU がまだ使っているかもしれない)
        let mut pages = Vec::new();
        self.each_pte(start, end, |_, pte| unsafe {
            pages.push(page_of(*pte));
            *pte = 0;
        });
        flush_all();
        self.quiesce();
        for p in pages {
            kalloc::put(p);
        }
        release_shared(&file_keys);
    }

    /// [start, end) のページを捨てる (領域は残す。次に触れたら作りなおす: MADV_DONTNEED)
    pub fn discard(&mut self, start: usize, end: usize) {
        let mut shared = Vec::new();
        for (&s, v) in self.vmas.range(..end) {
            if v.end > start && v.shared {
                shared.push((s.max(start), v.end.min(end)));
            }
        }
        let mut pages = Vec::new();
        self.each_pte(start, end, |va, pte| unsafe {
            // 共有のメモリは捨てない (ほかのプロセスが使っている)
            if !shared.iter().any(|&(s, e)| s <= va && va < e) {
                pages.push(page_of(*pte));
                *pte = 0;
            }
        });
        flush_all();
        self.quiesce();
        for p in pages {
            kalloc::put(p);
        }
    }

    /// mprotect。領域でないところが混じっていれば ENOMEM (Err)
    pub fn protect(&mut self, start: usize, end: usize, prot: u8) -> Result<(), ()> {
        let (start, end) = (pg_down(start), pg_up(end));
        // すき間がないか
        let mut va = start;
        while va < end {
            let (_, v) = self.find(va).ok_or(())?;
            va = v.end;
        }
        self.split(start);
        self.split(end);
        let mut shared_at = Vec::new();
        for (&s, v) in self.vmas.range_mut(start..end) {
            v.prot = prot;
            shared_at.push((s, v.end, v.shared));
        }
        for (s, e, shared) in shared_at {
            self.each_pte(s, e, |_, pte| unsafe { *pte = remake_pte(*pte, prot, shared) });
        }
        // 共有のファイルのページが書けるようになった: もう書いたものとして扱う
        if prot & PROT_WRITE != 0 {
            for k in self.shared_keys(start, end) {
                if let Some(c) = shared_file().get_mut(&k) {
                    c.dirty = true;
                }
            }
        }
        flush_all();
        self.quiesce();
        Ok(())
    }

    /// 領域の終わり (brk で伸ばす)。start から始まる領域を end まで伸ばすか縮める
    pub fn resize(&mut self, start: usize, old_end: usize, new_end: usize) -> Option<()> {
        if new_end > old_end {
            // 伸ばす先に別の領域があればだめ
            if self.vmas.range(old_end..new_end).next().is_some() {
                return None;
            }
            match self.vmas.get_mut(&start) {
                Some(v) if v.end == old_end => v.end = new_end,
                _ => {
                    self.vmas.insert(old_end, Vma { end: new_end, prot: PROT_RW, shared: false, back: Backing::Anon });
                }
            }
        } else if new_end < old_end {
            self.unmap(new_end, old_end);
        }
        Some(())
    }

    /// 次に mmap に渡せる、hint 以上で len のすき間
    pub fn free_area(&self, hint: usize, len: usize) -> usize {
        let mut va = hint;
        for (&s, v) in self.vmas.range(..) {
            if v.end <= va {
                continue;
            }
            if s >= va + len {
                break;
            }
            va = pg_up(v.end);
        }
        va
    }

    // ---- ページフォールト ----

    /// ユーザーが va で write / exec しようとして止まった
    pub fn fault(&mut self, va: usize, write: bool, exec: bool) -> Result<(), FaultErr> {
        let (_, v) = self.find(va).ok_or(FaultErr::NoMap)?;
        let ok = if write {
            v.prot & PROT_WRITE != 0
        } else if exec {
            v.prot & PROT_EXEC != 0
        } else {
            v.prot & (PROT_READ | PROT_WRITE | PROT_EXEC) != 0
        };
        if !ok {
            return Err(FaultErr::Access);
        }
        self.fault_page(va, write, false)
    }

    /// va のページを用意する。write なら自分だけのものにして書けるように。
    /// force はカーネルが書く (exec で読み取り専用の領域に読み込む): 権限は変えずに自分だけのものに
    fn fault_page(&mut self, va: usize, write: bool, force: bool) -> Result<(), FaultErr> {
        let (start, v) = self.find(va).ok_or(FaultErr::NoMap)?;
        let (prot, shared, back) = (v.prot, v.shared, v.back.clone());
        let page_va = pg_down(va);
        let pte = self.walk(page_va, true).ok_or(FaultErr::NoMem)?;
        let e = unsafe { *pte };
        if let (true, Backing::File { ino, off, .. }) = (shared, &back) {
            // MAP_SHARED のファイル: みんなで 1 枚のページ。書くときに印をつける
            let key = file_key(ino, off + (page_va - start));
            let wr = write || force;
            let page = if has_page(e) {
                page_of(e)
            } else {
                let page = match shared_file().get(&key) {
                    Some(c) => c.page,
                    None => {
                        let page = kalloc::alloc().ok_or(FaultErr::NoMem)?;
                        let size = ino.meta().size as usize;
                        let foff = key.2 * PGSIZE;
                        if foff < size {
                            let buf = unsafe { core::slice::from_raw_parts_mut(page, (size - foff).min(PGSIZE)) };
                            if ino.read_at(foff, buf).is_err() {
                                kalloc::free(page);
                                return Err(FaultErr::NoMem);
                            }
                        }
                        shared_file().insert(key, Cached { page, dirty: false, ino: ino.clone() });
                        page
                    }
                };
                kalloc::get(page);
                page
            };
            let c = shared_file().get_mut(&key).ok_or(FaultErr::NoMem)?;
            if wr {
                c.dirty = true;
            }
            let p = if c.dirty { prot } else { prot & !PROT_WRITE };
            unsafe { *pte = make_pte(v2p(page as usize) as u64, p, false) };
            flush_va(page_va);
            return Ok(());
        }
        if !has_page(e) {
            let page = kalloc::alloc().ok_or(FaultErr::NoMem)?;
            if let Backing::File { ino, off, fend } = &back {
                let n = PGSIZE.min(fend.saturating_sub(page_va));
                if n > 0 {
                    let buf = unsafe { core::slice::from_raw_parts_mut(page, n) };
                    if ino.read_at(off + (page_va - start), buf).is_err() {
                        kalloc::free(page);
                        return Err(FaultErr::NoMem);
                    }
                }
            }
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        } else if write || force {
            let old = page_of(e);
            if !shared && kalloc::refs(old) > 1 {
                // 共有しているので写す
                let page = kalloc::alloc().ok_or(FaultErr::NoMem)?;
                unsafe { core::ptr::copy_nonoverlapping(old, page, PGSIZE) };
                unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
                flush_va(page_va);
                self.quiesce();
                kalloc::put(old);
            } else {
                unsafe { *pte = make_pte(e & PTE_ADDR, prot, false) };
            }
        } else {
            unsafe { *pte = remake_pte(e, prot, shared) };
        }
        flush_va(page_va);
        Ok(())
    }

    // ---- カーネルからユーザー空間へ ----

    /// ユーザーの va の物理アドレス。まだページがなければ作る (write なら書けるように)
    fn user_pa(&mut self, va: usize, write: bool, force: bool) -> Option<usize> {
        let ready = |e: u64| e & PTE_VALID != 0 && e & PTE_USER != 0 && (!write || e & PTE_RDONLY == 0 || (force && kalloc::refs(page_of(e)) == 1));
        let e = self.walk(va, false).map(|p| unsafe { *p }).unwrap_or(0);
        if !ready(e) {
            if force {
                self.fault_page(va, true, true).ok()?;
            } else {
                self.fault(va, write, false).ok()?;
            }
        }
        let e = unsafe { *self.walk(va, false)? };
        if e & PTE_VALID == 0 {
            return None;
        }
        Some((e & PTE_ADDR) as usize + (va & (PGSIZE - 1)))
    }

    /// ユーザー空間 src から dst へコピー
    pub fn copy_in(&mut self, dst: &mut [u8], src: usize) -> Option<()> {
        self.each_chunk(src, dst.len(), false, false, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[off..].as_mut_ptr(), n);
        })
    }

    /// src をユーザー空間 dst へコピー (書けない領域なら None)
    pub fn copy_out(&mut self, dst: usize, src: &[u8]) -> Option<()> {
        self.each_chunk(dst, src.len(), true, false, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
        })
    }

    /// 書けない領域にも書く (exec が ELF を読み込むとき)
    pub fn copy_out_force(&mut self, dst: usize, src: &[u8]) -> Option<()> {
        self.each_chunk(dst, src.len(), true, true, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
        })
    }

    fn each_chunk(&mut self, mut va: usize, len: usize, write: bool, force: bool, mut f: impl FnMut(usize, usize, usize)) -> Option<()> {
        let mut done = 0;
        while done < len {
            let pa = self.user_pa(va, write, force)?;
            let n = (PGSIZE - (va & (PGSIZE - 1))).min(len - done);
            f(pa, done, n);
            done += n;
            va += n;
        }
        Some(())
    }

    /// NUL 終端の文字列を max バイトまで読む
    pub fn copy_in_str(&mut self, mut va: usize, max: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let pa = self.user_pa(va, false, false)?;
            let n = PGSIZE - (va & (PGSIZE - 1));
            let page = unsafe { core::slice::from_raw_parts(p2v(pa) as *const u8, n) };
            if let Some(i) = page.iter().position(|&c| c == 0) {
                out.extend_from_slice(&page[..i]);
                return (out.len() <= max).then_some(out);
            }
            out.extend_from_slice(page);
            if out.len() > max {
                return None;
            }
            va += n;
        }
    }

    /// va に決まったページを写す (vDSO)。ページはカーネルも持ちつづける
    pub fn install(&mut self, va: usize, page: *mut u8, prot: u8) -> Option<()> {
        let pte = self.walk(va, true)?;
        kalloc::get(page);
        unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        Some(())
    }

    /// 大きなロックなしで、もう写っていて書けるページにだけ書く (clock_gettime などの速い道)。
    /// ページが写っていない・書けないなら false (ロックを取ってふつうの道で)
    pub fn copy_out_nofault(&self, dst: usize, src: &[u8]) -> bool {
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut pas = [(0usize, 0usize); 2];
        let mut n = 0;
        let mut ok = true;
        let (mut va, mut done) = (dst, 0);
        while done < src.len() {
            let e = self.walk(va, false).map_or(0, |p| unsafe { core::ptr::read_volatile(p) });
            let len = (PGSIZE - (va & (PGSIZE - 1))).min(src.len() - done);
            if e & PTE_VALID == 0 || e & PTE_USER == 0 || e & PTE_RDONLY != 0 || n == pas.len() {
                ok = false;
                break;
            }
            pas[n] = ((e & PTE_ADDR) as usize + (va & (PGSIZE - 1)), len);
            n += 1;
            done += len;
            va += len;
        }
        if ok {
            let mut off = 0;
            for &(pa, len) in &pas[..n] {
                unsafe { core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, len) };
                off += len;
            }
        }
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        ok
    }

    /// PTE を消した (TLB も消した) あと、ページを手放す前に: ロックなしで書いている CPU を待つ
    fn quiesce(&self) {
        fence(Ordering::SeqCst);
        while self.fast_users.load(Ordering::SeqCst) != 0 {
            core::hint::spin_loop();
        }
    }

    /// 同じ中身の新しいアドレス空間 (fork)。ページは写さずに共有し、書けるものは COW にする
    pub fn fork(&mut self) -> Option<PageTable> {
        let mut new = PageTable::new()?;
        new.vmas = self.vmas.clone();
        let areas: Vec<(usize, usize, u8, bool)> = self.vmas.iter().map(|(&s, v)| (s, v.end, v.prot, v.shared)).collect();
        for (s, e, prot, shared) in areas {
            let mut fail = false;
            self.each_pte(s, e, |va, pte| unsafe {
                if fail {
                    return;
                }
                kalloc::get(page_of(*pte));
                // 共有の領域はそのまま (書いた印はページの表にある)。private は COW に
                if !shared {
                    *pte = remake_pte(*pte, prot, false);
                }
                match new.walk(va, true) {
                    Some(np) => *np = *pte,
                    None => {
                        kalloc::put(page_of(*pte));
                        fail = true;
                    }
                }
            });
            if fail {
                flush_all();
                return None;
            }
        }
        flush_all();
        self.quiesce();
        Some(new)
    }

    pub fn activate(&self) {
        unsafe {
            core::arch::asm!(
                "msr ttbr0_el1, {}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                in(reg) self.root_pa(),
            );
        }
    }

    /// 領域の大きさの合計 (/proc の VmSize)
    pub fn vsize(&self) -> usize {
        self.vmas.iter().map(|(&s, v)| v.end - s).sum()
    }

    /// 持っているページの数 (/proc の VmRSS)
    pub fn resident(&self) -> usize {
        let mut n = 0;
        for (&s, v) in &self.vmas {
            self.each_pte(s, v.end, |_, _| n += 1);
        }
        n
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        let file_keys = self.shared_keys(0, MAXVA);
        fn free_level(table: *mut u64, level: usize) {
            for i in 0..512 {
                let e = unsafe { *table.add(i) };
                if level < 3 {
                    if e & PTE_VALID != 0 {
                        free_level(table_at(e), level + 1);
                    }
                } else if has_page(e) {
                    kalloc::put(page_of(e));
                }
            }
            kalloc::free(table as *mut u8);
        }
        free_level(self.root, 1);
        release_shared(&file_keys);
    }
}
