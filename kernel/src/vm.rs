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
//   SWAP      (57) スワップへ追い出したページ (VALID は 0)。アドレスのところにスロットの番号 (swap.rs)
//
// スワップ: 自分だけの領域 (MAP_SHARED でない) の、ほかと共有していないページを追い出せる。
// 選ぶのは clock: AF (アクセスフラグ) を落としておき、次に見たときにまだ落ちていれば追い出す。
// AF が落ちたページに触れるとアクセスフラグのフォールトになり、fault_page が立てなおす。
use crate::kalloc;
use crate::swap;
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
const PTE_SWAP: u64 = 1 << 57;
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

/// MAP_SHARED で写しているファイルのページの数 (/proc/meminfo の Shmem)
pub fn shared_pages() -> usize {
    shared_file().len()
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
    /// カーネルが持っているページをそのまま見せる (/dev/fb0 のフレームバッファ)。off はバイト
    Pages { pages: alloc::rc::Rc<Vec<*mut u8>>, off: usize },
}

#[derive(Clone)]
pub struct Vma {
    pub end: usize,
    pub prot: u8,
    /// MAP_SHARED (fork しても共有のまま)
    pub shared: bool,
    pub back: Backing,
    /// 写したファイルのパス (/proc/PID/maps と、落ちたときの知らせ)
    pub name: Option<alloc::rc::Rc<str>>,
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
    /// 大きなロックなしでこの表を読み書きしている CPU の数 (copy_out_nofault、fast_fault)。
    /// ページを手放す前に 0 になるのを待つ (quiesce)
    fast_users: AtomicUsize,
    /// 大きなロックを持ってこの表 (領域か PTE) を変えている途中の数。0 でないあいだ fast_fault は
    /// ふつうの道 (大きなロック) へ回る。変える側は増やしてから quiesce する (Mutating)
    mutators: AtomicUsize,
    /// clock の針 (次にスワップへ追い出すページを探しはじめる va)
    clock: usize,
    /// 最近の map / protect / unmap (操作, 始め, 終わり, prot)。落ちたときに、そのアドレスにかかわったものを出す (調べもの用)
    hist: alloc::collections::VecDeque<(u8, usize, usize, u8)>,
}

/// hist に覚える数
const HIST: usize = 512;

/// 表を変えている途中 (PageTable::mutating)。落とすと終わり。
/// &mut self のメソッドの中で持つので、表そのものではなく数えるところだけを指す (表はそのあいだ動かない)
pub struct Mutating(*const AtomicUsize);

impl Drop for Mutating {
    fn drop(&mut self) {
        unsafe { (*self.0).fetch_sub(1, Ordering::SeqCst) };
    }
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

/// スワップへ追い出したページの PTE か
fn is_swap(e: u64) -> bool {
    e & PTE_VALID == 0 && e & PTE_SWAP != 0
}

fn swap_pte(slot: swap::Slot) -> u64 {
    (slot << 12) & PTE_ADDR | PTE_SWAP
}

fn slot_of(e: u64) -> swap::Slot {
    (e & PTE_ADDR) >> 12
}

/// カーネルが書いたところを命令として実行できるように: データキャッシュを PoU まで書き出し、
/// 命令キャッシュを (すべての CPU で) 消す。QEMU の TCG では要らないが、本物の CPU (HVF) では要る
pub fn sync_icache(kva: usize, len: usize) {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr) };
    let line = 4usize << ((ctr >> 16) & 0xf);
    let mut a = kva & !(line - 1);
    while a < kva + len {
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) a) };
        a += line;
    }
    unsafe { core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb") };
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

/// 消した (無効にした) ページの TLB を消す: 少なければ 1 つずつ、多ければ全部。
/// 全部消す (vmalle1is) はすべての CPU のすべてのプロセスの TLB を捨てるので、小さな munmap のたびにはしない
fn flush_pages(vas: &[usize]) {
    if vas.is_empty() {
        return;
    }
    if vas.len() <= 64 {
        unsafe { core::arch::asm!("dsb ishst") };
        for &va in vas {
            unsafe { core::arch::asm!("tlbi vaae1is, {}", in(reg) (va >> 12) as u64) };
        }
        unsafe { core::arch::asm!("dsb ish", "isb") };
    } else {
        flush_all();
    }
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
        Some(Self { root: kalloc::alloc()? as *mut u64, vmas: BTreeMap::new(), fast_users: AtomicUsize::new(0), mutators: AtomicUsize::new(0), clock: 0, hist: alloc::collections::VecDeque::new() })
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

    /// each_pte と同じだが、スワップへ追い出したページの PTE も
    fn each_entry(&self, start: usize, end: usize, mut f: impl FnMut(usize, *mut u64)) {
        let mut va = pg_down(start);
        while va < end {
            match self.walk(va, false) {
                Some(pte) => {
                    let e = unsafe { *pte };
                    if has_page(e) || is_swap(e) {
                        f(va, pte);
                    }
                    va += PGSIZE;
                }
                None => va = (va | 0x1f_ffff) + 1,
            }
        }
    }

    /// ユーザーのページを 1 枚。足りなければ、ほかのアドレス空間のページをスワップへ追い出して作る
    /// (自分のページ表は借りられているので触らない)
    fn alloc_page(&self) -> Option<*mut u8> {
        kalloc::alloc().or_else(|| {
            swap::reclaim(swap::BATCH, self.root as usize);
            kalloc::alloc()
        })
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
        match &mut hi.back {
            Backing::File { off, .. } | Backing::Pages { off, .. } => *off += addr - start,
            Backing::Anon => {}
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
        let _m = self.mutating();
        if start >= end || end > MAXVA {
            return None;
        }
        self.unmap_inner(start, end);
        self.note(b'm', start, end, prot);
        self.vmas.insert(start, Vma { end, prot, shared, back, name: None });
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
        let (s, e) = (pg_down(start), pg_up(end));
        if s < e {
            self.note(b'u', s, e, 0);
        }
        self.unmap_inner(start, end);
    }

    fn unmap_inner(&mut self, start: usize, end: usize) {
        let _m = self.mutating();
        let (start, end) = (pg_down(start), pg_up(end));
        if start >= end {
            return;
        }
        // 範囲に領域が 1 つもなければ (mmap が新しい場所に写すときはいつもそう)、することがない。
        // 領域は重ならないので、end より前で最後のものだけ見ればよい
        if !self.vmas.range(..end).next_back().is_some_and(|(_, v)| v.end > start) {
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
        let mut vas = Vec::new();
        self.each_entry(start, end, |va, pte| unsafe {
            if is_swap(*pte) {
                swap::free(slot_of(*pte));
            } else {
                pages.push(page_of(*pte));
                if *pte & PTE_VALID != 0 {
                    vas.push(va);
                }
            }
            *pte = 0;
        });
        flush_pages(&vas);
        if !vas.is_empty() {
            self.quiesce();
        }
        for p in pages {
            kalloc::put(p);
        }
        release_shared(&file_keys);
    }

    /// [start, end) のページを捨てる (領域は残す。次に触れたら作りなおす: MADV_DONTNEED)
    pub fn discard(&mut self, start: usize, end: usize) {
        let _m = self.mutating();
        let mut shared = Vec::new();
        for (&s, v) in self.vmas.range(..end) {
            if v.end > start && v.shared {
                shared.push((s.max(start), v.end.min(end)));
            }
        }
        let mut pages = Vec::new();
        let mut vas = Vec::new();
        self.each_entry(start, end, |va, pte| unsafe {
            // 共有のメモリは捨てない (ほかのプロセスが使っている)
            if !shared.iter().any(|&(s, e)| s <= va && va < e) {
                if is_swap(*pte) {
                    swap::free(slot_of(*pte));
                } else {
                    pages.push(page_of(*pte));
                    if *pte & PTE_VALID != 0 {
                        vas.push(va);
                    }
                }
                *pte = 0;
            }
        });
        flush_pages(&vas);
        if !vas.is_empty() {
            self.quiesce();
        }
        for p in pages {
            kalloc::put(p);
        }
    }

    /// mprotect。領域でないところが混じっていれば ENOMEM (Err)
    pub fn protect(&mut self, start: usize, end: usize, prot: u8) -> Result<(), ()> {
        let _m = self.mutating();
        let (start, end) = (pg_down(start), pg_up(end));
        self.note(b'p', start, end, prot);
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
        let _m = self.mutating();
        if new_end > old_end {
            // 伸ばす先に別の領域があればだめ
            if self.vmas.range(old_end..new_end).next().is_some() {
                return None;
            }
            match self.vmas.get_mut(&start) {
                Some(v) if v.end == old_end => v.end = new_end,
                _ => {
                    self.vmas.insert(old_end, Vma { end: new_end, prot: PROT_RW, shared: false, back: Backing::Anon, name: None });
                }
            }
        } else if new_end < old_end {
            self.unmap(new_end, old_end);
        }
        Some(())
    }

    /// start から始まる領域に名前 (写したファイルのパス) をつける
    pub fn set_name(&mut self, start: usize, name: &str) {
        let _m = self.mutating();
        if let Some(v) = self.vmas.get_mut(&start) {
            v.name = Some(alloc::rc::Rc::from(name));
        }
    }

    /// va のある領域の (名前, ファイルの中の場所)
    pub fn name_at(&self, va: usize) -> Option<(alloc::rc::Rc<str>, usize)> {
        let (s, v) = self.find(va)?;
        let off = match &v.back {
            Backing::File { off, .. } => *off,
            _ => 0,
        };
        Some((v.name.clone()?, off + (va - s)))
    }

    fn note(&mut self, op: u8, start: usize, end: usize, prot: u8) {
        if self.hist.len() >= HIST {
            self.hist.pop_front();
        }
        self.hist.push_back((op, start, end, prot));
    }

    /// va のページにかかわった最近の map / protect / unmap (古い順。落ちたときの知らせ)
    pub fn hist_text(&self, va: usize) -> alloc::vec::Vec<alloc::string::String> {
        let page = pg_down(va);
        let n = self.hist.len();
        self.hist
            .iter()
            .enumerate()
            .filter(|(_, (_, s, e, _))| *s <= page && page < *e)
            .map(|(i, &(op, s, e, prot))| {
                let p = |b: u8, c: char| if prot & b != 0 { c } else { '-' };
                let what = match op {
                    b'm' => "map",
                    b'p' => "protect",
                    _ => "unmap",
                };
                let pr = if op == b'u' { alloc::string::String::new() } else { alloc::format!(" {}{}{}", p(PROT_READ, 'r'), p(PROT_WRITE, 'w'), p(PROT_EXEC, 'x')) };
                alloc::format!("-{} {} [{:#x}-{:#x}){}", n - i, what, s, e, pr)
            })
            .collect()
    }

    /// va がどの領域か (落ちたときの知らせ): 中なら「[始め-終わり) 権限 名前」、外なら上と下の近い領域からの距離
    pub fn region_text(&self, va: usize) -> alloc::string::String {
        let desc = |s: usize, v: &Vma| {
            let p = |b: u8, c: char| if v.prot & b != 0 { c } else { '-' };
            alloc::format!(
                "[{:#x}-{:#x}) {}{}{}{} {}",
                s,
                v.end,
                p(PROT_READ, 'r'),
                p(PROT_WRITE, 'w'),
                p(PROT_EXEC, 'x'),
                if v.shared { 's' } else { 'p' },
                v.name.as_deref().unwrap_or("(anon)")
            )
        };
        if let Some((s, v)) = self.find(va) {
            let guard = if v.prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 { " (PROT_NONE: stack guard?)" } else { "" };
            return alloc::format!("inside {}{}", desc(s, v), guard);
        }
        let below = self.vmas.range(..=va).next_back().map(|(&s, v)| alloc::format!("{:#x} above the end of {}", va - v.end, desc(s, v)));
        let above = self.vmas.range(va..).next().map(|(&s, v)| alloc::format!("{:#x} below {}", s - va, desc(s, v)));
        match (below, above) {
            (Some(b), Some(a)) => alloc::format!("unmapped: {}; {}", b, a),
            (Some(x), None) | (None, Some(x)) => alloc::format!("unmapped: {}", x),
            (None, None) => "unmapped".into(),
        }
    }

    /// /proc/PID/maps (Linux と同じ形)
    pub fn maps_text(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for (&s, v) in self.vmas.iter() {
            let off = match &v.back {
                Backing::File { off, .. } => *off,
                _ => 0,
            };
            let r = if v.prot & PROT_READ != 0 { 'r' } else { '-' };
            let w = if v.prot & PROT_WRITE != 0 { 'w' } else { '-' };
            let x = if v.prot & PROT_EXEC != 0 { 'x' } else { '-' };
            let p = if v.shared { 's' } else { 'p' };
            out.push_str(&alloc::format!("{:08x}-{:08x} {}{}{}{} {:08x} 00:00 0 {}\n", s, v.end, r, w, x, p, off, v.name.as_deref().unwrap_or("")));
        }
        out
    }

    /// start から始まる領域の終わりを new_end まで伸ばす (その先が空いているときだけ。mremap)
    pub fn extend(&mut self, start: usize, new_end: usize) -> Option<()> {
        let _m = self.mutating();
        let old_end = self.vmas.get(&start)?.end;
        if new_end <= old_end || self.vmas.range(old_end..new_end).next().is_some() {
            return None;
        }
        self.vmas.get_mut(&start)?.end = new_end;
        Some(())
    }

    /// 次に mmap に渡せる、hint 以上で len のすき間
    pub fn free_area(&self, hint: usize, len: usize) -> usize {
        let mut va = hint;
        // hint より前の領域は、hint にかかりうる最後の 1 つ (領域は重ならない) から見ればよい
        let from = self.vmas.range(..=hint).next_back().map_or(hint, |(&s, _)| s);
        for (&s, v) in self.vmas.range(from..) {
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
        let _m = self.mutating();
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
                        let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
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
                        if prot & PROT_EXEC != 0 {
                            sync_icache(page as usize, PGSIZE);
                        }
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
        if let (false, Backing::Pages { pages, off }) = (has_page(e), &back) {
            // カーネルのページ (フレームバッファ) をそのまま。共有で、外すときに参照を返す
            let page = *pages.get((off + (page_va - start)) / PGSIZE).ok_or(FaultErr::NoMap)?;
            kalloc::get(page);
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
            flush_va(page_va);
            return Ok(());
        }
        if is_swap(e) {
            // スワップから読み戻す。スロットはほかのアドレス空間 (fork) とまだ共有しているかもしれない
            let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
            if swap::read(slot_of(e), page).is_err() {
                kalloc::free(page);
                return Err(FaultErr::NoMem);
            }
            swap::free(slot_of(e));
            if prot & PROT_EXEC != 0 {
                sync_icache(page as usize, PGSIZE);
            }
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        } else if !has_page(e) {
            let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
            if let Backing::File { ino, off, fend } = &back {
                let n = PGSIZE.min(fend.saturating_sub(page_va));
                if n > 0 {
                    let buf = unsafe { core::slice::from_raw_parts_mut(page, n) };
                    if ino.read_at(off + (page_va - start), buf).is_err() {
                        kalloc::free(page);
                        return Err(FaultErr::NoMem);
                    }
                }
                if prot & PROT_EXEC != 0 {
                    sync_icache(page as usize, PGSIZE);
                }
            }
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        } else if write || force {
            let old = page_of(e);
            if !shared && kalloc::refs(old) > 1 {
                // 共有しているので写す
                let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
                unsafe { core::ptr::copy_nonoverlapping(old, page, PGSIZE) };
                if prot & PROT_EXEC != 0 {
                    sync_icache(page as usize, PGSIZE);
                }
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

    /// 共有の領域 (MAP_SHARED) なら va の物理アドレス。プロセスをまたぐ futex は、同じページを
    /// 違うアドレスに写していても会えるように、これで待ち合わせる
    pub fn shared_pa(&mut self, va: usize) -> Option<usize> {
        if !self.find(va)?.1.shared {
            return None;
        }
        self.user_pa(va, false, false)
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
        // ELF のコードかもしれないので、命令キャッシュも合わせる
        self.each_chunk(dst, src.len(), true, true, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
            sync_icache(p2v(pa), n);
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
        let _m = self.mutating();
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

    /// 領域か PTE を変えはじめる (大きなロックを持って)。fast_fault を止めて、ロックなしで触っている CPU が
    /// いなくなるのを待つ。返ったものを持っているあいだが「変えている途中」
    fn mutating(&self) -> Mutating {
        self.mutators.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.quiesce();
        Mutating(&self.mutators)
    }

    /// 大きなロックなしのページフォルト。自分だけの (MAP_SHARED でない) 無名の領域で、
    ///   ページがまだない / 共有していて書く (コピーオンライト) / アクセスフラグが落ちている
    /// ものを、途中のテーブルがもうあるときだけ、PTE の compare-exchange で片づける。
    /// それ以外 (ファイル、スワップ、共有、テーブルがない、だれかが表を変えている途中) は false で、
    /// 呼んだほうが大きなロックを取ってふつうの道 (fault) へ。
    /// 同じ表のほかのスレッドと同時に走ってよい: PTE は compare-exchange で、負けたら false (やりなおし)
    pub fn fast_fault(&self, va: usize, write: bool) -> bool {
        if va >= MAXVA {
            return false;
        }
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let (ok, put) = if self.mutators.load(Ordering::SeqCst) == 0 { self.fast_fault_inner(va, write) } else { (false, None) };
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        if let Some(old) = put {
            // 写しとった元のページを手放す。ロックなしで元のページを読んでいる CPU がいなくなってから
            self.quiesce();
            kalloc::put(old);
        }
        ok
    }

    /// (片づいたか, あとで手放すページ)
    fn fast_fault_inner(&self, va: usize, write: bool) -> (bool, Option<*mut u8>) {
        use core::sync::atomic::AtomicU64;
        let Some((_, v)) = self.find(va) else { return (false, None) };
        if v.shared || !matches!(v.back, Backing::Anon) {
            return (false, None);
        }
        let prot = v.prot;
        if (write && prot & PROT_WRITE == 0) || prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 {
            return (false, None);
        }
        let page_va = pg_down(va);
        let Some(pte) = self.walk(page_va, false) else { return (false, None) };
        let slot = unsafe { AtomicU64::from_ptr(pte) };
        let e = slot.load(Ordering::Acquire);
        if is_swap(e) {
            return (false, None);
        }
        let (new, fresh, put) = if !has_page(e) {
            let Some(page) = kalloc::alloc() else { return (false, None) };
            (make_pte(v2p(page as usize) as u64, prot, false), Some(page), None)
        } else if write {
            if e & PTE_RDONLY == 0 && e & PTE_AF != 0 {
                // もう書ける (ほかの CPU が先に片づけた)。TLB が古いだけ
                flush_va(page_va);
                return (true, None);
            }
            let old = page_of(e);
            if kalloc::refs(old) > 1 {
                // 共有しているので写す
                let Some(page) = kalloc::alloc() else { return (false, None) };
                unsafe { core::ptr::copy_nonoverlapping(old, page, PGSIZE) };
                (make_pte(v2p(page as usize) as u64, prot, false), Some(page), Some(old))
            } else {
                (make_pte(e & PTE_ADDR, prot, false), None, None)
            }
        } else {
            // 読む: アクセスフラグが落ちているだけ
            (remake_pte(e, prot, false), None, None)
        };
        if slot.compare_exchange(e, new, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // ほかの CPU (同じ表のスレッド) が先に変えた。もらったページは返して、やりなおし
            if let Some(page) = fresh {
                kalloc::free(page);
            }
            return (false, None);
        }
        flush_va(page_va);
        (true, put)
    }

    /// 大きなロックなしで、もう写っているページからだけ読む (futex の速い道)。写っていなければ false
    pub fn copy_in_nofault(&self, src: usize, dst: &mut [u8]) -> bool {
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut ok = true;
        let (mut va, mut done) = (src, 0);
        while done < dst.len() {
            let e = self.walk(va, false).map_or(0, |p| unsafe { core::ptr::read_volatile(p) });
            let len = (PGSIZE - (va & (PGSIZE - 1))).min(dst.len() - done);
            if e & PTE_VALID == 0 || e & PTE_USER == 0 {
                ok = false;
                break;
            }
            let pa = (e & PTE_ADDR) as usize + (va & (PGSIZE - 1));
            unsafe { core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[done..].as_mut_ptr(), len) };
            done += len;
            va += len;
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
        let _m = self.mutating();
        let mut new = PageTable::new()?;
        new.vmas = self.vmas.clone();
        let areas: Vec<(usize, usize, u8, bool)> = self.vmas.iter().map(|(&s, v)| (s, v.end, v.prot, v.shared)).collect();
        for (s, e, prot, shared) in areas {
            let mut fail = false;
            self.each_entry(s, e, |va, pte| unsafe {
                if fail {
                    return;
                }
                let swapped = is_swap(*pte);
                if swapped {
                    // スワップのスロットも共有する (先に読み戻した方が自分のページを作る)
                    swap::dup(slot_of(*pte));
                } else {
                    kalloc::get(page_of(*pte));
                    // 共有の領域はそのまま (書いた印はページの表にある)。private は COW に
                    if !shared {
                        *pte = remake_pte(*pte, prot, false);
                    }
                }
                match new.walk(va, true) {
                    Some(np) => *np = *pte,
                    None => {
                        if swapped {
                            swap::free(slot_of(*pte));
                        } else {
                            kalloc::put(page_of(*pte));
                        }
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

    // ---- スワップ ----

    /// clock の針から、追い出せるページを want 枚までスワップへ書き出す。追い出した数を返す。
    /// 見たページの AF が立っていれば落とすだけ (次に回ってきたときまで触れられなければ追い出す)
    pub fn swap_out(&mut self, want: usize) -> usize {
        let _m = self.mutating();
        // 自分だけの領域を、針のところから一周
        let mut ranges = Vec::new();
        let hand = self.clock;
        for (&s, v) in &self.vmas {
            if !v.shared && v.end > hand {
                ranges.push((s.max(hand), v.end));
            }
        }
        for (&s, v) in &self.vmas {
            if !v.shared && s < hand {
                ranges.push((s, v.end.min(hand)));
            }
        }
        let mut victims = Vec::new();
        let mut aged = false;
        let mut next = 0;
        for (s, e) in ranges {
            self.each_pte(s, e, |va, pte| unsafe {
                if victims.len() >= want {
                    return;
                }
                let e = *pte;
                // PROT_NONE のものや、fork や vDSO で共有しているものは追い出さない
                if e & PTE_VALID == 0 || kalloc::refs(page_of(e)) != 1 {
                    return;
                }
                if e & PTE_AF != 0 {
                    *pte = e & !PTE_AF;
                    aged = true;
                } else {
                    victims.push((va, pte, e));
                }
                next = va + PGSIZE;
            });
            if victims.len() >= want {
                break;
            }
        }
        // 一周したら針は先頭へ
        self.clock = if victims.len() >= want { next } else { 0 };
        // PTE を外してから (ほかの CPU のスレッドがもう触れないようにして) 書き出す
        let mut out = Vec::new();
        for &(_, pte, e) in &victims {
            let Some(slot) = swap::alloc() else { break };
            unsafe { *pte = swap_pte(slot) };
            out.push((pte, e, slot));
        }
        if aged || !out.is_empty() {
            flush_all();
            self.quiesce();
        }
        let mut n = 0;
        for (pte, e, slot) in out {
            let page = page_of(e);
            if swap::write(slot, page).is_err() {
                unsafe { *pte = e };
                swap::free(slot);
                continue;
            }
            kalloc::put(page);
            n += 1;
        }
        n
    }

    /// スワップの区画 area に追い出したページを、ぜんぶ読み戻す (swapoff)。足りなければ Err
    pub fn swap_in_area(&mut self, area: usize) -> Result<(), ()> {
        let _m = self.mutating();
        let vmas: Vec<(usize, usize, u8)> = self.vmas.iter().map(|(&s, v)| (s, v.end, v.prot)).collect();
        let mut ok = true;
        for (s, e, prot) in vmas {
            self.each_entry(s, e, |_, pte| unsafe {
                let e = *pte;
                if !ok || !is_swap(e) || swap::area_of(slot_of(e)) != area {
                    return;
                }
                let Some(page) = kalloc::alloc() else {
                    ok = false;
                    return;
                };
                if swap::read(slot_of(e), page).is_err() {
                    kalloc::free(page);
                    ok = false;
                    return;
                }
                swap::free(slot_of(e));
                *pte = make_pte(v2p(page as usize) as u64, prot, false);
            });
        }
        if ok { Ok(()) } else { Err(()) }
    }

    /// ページ表のルート (スワップの回収で、自分を除くのに使う)
    pub fn id(&self) -> usize {
        self.root as usize
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
        let _m = self.mutating();
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
                } else if is_swap(e) {
                    swap::free(slot_of(e));
                }
            }
            kalloc::free(table as *mut u8);
        }
        free_level(self.root, 1);
        release_shared(&file_keys);
    }
}
