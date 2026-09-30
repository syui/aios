// ユーザー空間のページテーブル (TTBR0, 4KiB granule, 39bit VA, L1-L3)
use crate::kalloc;
use crate::memlayout::{p2v, v2p, PGSIZE};

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
const PTE_ADDR: u64 = 0x0000_ffff_ffff_f000;

pub const MAXVA: usize = 1 << 39;

/// ユーザーページの権限 (読みは常に可)
#[derive(Clone, Copy)]
pub struct Perm {
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const RW: Perm = Perm { write: true, exec: false };

    fn bits(self) -> u64 {
        let mut b = PTE_VALID | PTE_TABLE | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF | PTE_NG | PTE_USER | PTE_PXN;
        if !self.write {
            b |= PTE_RDONLY;
        }
        if !self.exec {
            b |= PTE_UXN;
        }
        b
    }

    fn from_bits(e: u64) -> Perm {
        Perm { write: e & PTE_RDONLY == 0, exec: e & PTE_UXN == 0 }
    }

    fn union(self, o: Perm) -> Perm {
        Perm { write: self.write || o.write, exec: self.exec || o.exec }
    }
}

pub const fn pg_down(a: usize) -> usize {
    a & !(PGSIZE - 1)
}

pub const fn pg_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}

/// ルートは kalloc したページ (カーネル仮想アドレス)
pub struct PageTable {
    root: *mut u64,
}

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * (3 - level))) & 0x1ff
}

fn table_at(e: u64) -> *mut u64 {
    p2v((e & PTE_ADDR) as usize) as *mut u64
}

impl PageTable {
    pub fn new() -> Option<Self> {
        Some(Self { root: kalloc::alloc()? as *mut u64 })
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
                unsafe { *pte = v2p(next as usize) as u64 | PTE_VALID | PTE_TABLE };
                table = next;
            }
        }
        Some(unsafe { table.add(index(va, 3)) })
    }

    /// [start, end) を 0 埋めページで埋める。すでにあるページは権限を足すだけ
    pub fn alloc_range(&mut self, start: usize, end: usize, perm: Perm) -> Option<()> {
        let mut va = pg_down(start);
        while va < end {
            let pte = self.walk(va, true)?;
            let e = unsafe { *pte };
            unsafe {
                *pte = if e & PTE_VALID != 0 {
                    (e & PTE_ADDR) | Perm::from_bits(e).union(perm).bits()
                } else {
                    v2p(kalloc::alloc()? as usize) as u64 | perm.bits()
                };
            }
            va += PGSIZE;
        }
        Some(())
    }

    /// [start, end) のページを外して返す
    pub fn unmap_range(&mut self, start: usize, end: usize) {
        let mut va = pg_down(start);
        while va < end {
            if let Some(pte) = self.walk(va, false) {
                let e = unsafe { *pte };
                if e & PTE_VALID != 0 {
                    kalloc::free(table_at(e) as *mut u8);
                    unsafe { *pte = 0 };
                }
            }
            va += PGSIZE;
        }
    }

    /// ユーザーが触れる va の物理アドレス
    fn user_pa(&self, va: usize) -> Option<usize> {
        let e = unsafe { *self.walk(va, false)? };
        if e & PTE_VALID == 0 || e & PTE_USER == 0 {
            return None;
        }
        Some((e & PTE_ADDR) as usize + (va & (PGSIZE - 1)))
    }

    /// ユーザー空間 src から dst へコピー
    pub fn copy_in(&self, dst: &mut [u8], src: usize) -> Option<()> {
        self.each_chunk(src, dst.len(), |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[off..].as_mut_ptr(), n);
        })
    }

    /// src をユーザー空間 dst へコピー (読み取り専用ページにも書ける)
    pub fn copy_out(&self, dst: usize, src: &[u8]) -> Option<()> {
        self.each_chunk(dst, src.len(), |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
        })
    }

    fn each_chunk(&self, mut va: usize, len: usize, mut f: impl FnMut(usize, usize, usize)) -> Option<()> {
        let mut done = 0;
        while done < len {
            let pa = self.user_pa(va)?;
            let n = (PGSIZE - (va & (PGSIZE - 1))).min(len - done);
            f(pa, done, n);
            done += n;
            va += n;
        }
        Some(())
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
}

impl Drop for PageTable {
    fn drop(&mut self) {
        fn free_level(table: *mut u64, level: usize) {
            for i in 0..512 {
                let e = unsafe { *table.add(i) };
                if e & PTE_VALID == 0 {
                    continue;
                }
                if level < 3 {
                    free_level(table_at(e), level + 1);
                } else {
                    kalloc::free(table_at(e) as *mut u8);
                }
            }
            kalloc::free(table as *mut u8);
        }
        free_level(self.root, 1);
    }
}
