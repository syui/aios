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

#[derive(Clone, Copy)]
pub enum Perm {
    /// 実行できる読み取り専用 (text)
    RX,
    /// 読み書きできる (data, stack)
    RW,
}

impl Perm {
    fn bits(self) -> u64 {
        let base = PTE_VALID | PTE_TABLE | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF | PTE_NG | PTE_USER | PTE_PXN;
        match self {
            Perm::RX => base | PTE_RDONLY,
            Perm::RW => base | PTE_UXN,
        }
    }
}

/// ルートは kalloc したページ (カーネル仮想アドレス)
pub struct PageTable {
    root: *mut u64,
}

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * (3 - level))) & 0x1ff
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
                table = p2v((e & PTE_ADDR) as usize) as *mut u64;
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

    /// va (ページ境界) に物理ページ pa を 1 枚写す
    pub fn map(&mut self, va: usize, pa: usize, perm: Perm) -> Option<()> {
        let pte = self.walk(va, true)?;
        unsafe {
            if *pte & PTE_VALID != 0 {
                panic!("vm: remap {:#x}", va);
            }
            *pte = pa as u64 | perm.bits();
        }
        Some(())
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
    pub fn copy_in(&self, dst: &mut [u8], mut src: usize) -> Option<()> {
        let mut done = 0;
        while done < dst.len() {
            let pa = self.user_pa(src)?;
            let n = (PGSIZE - (src & (PGSIZE - 1))).min(dst.len() - done);
            unsafe {
                core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[done..].as_mut_ptr(), n);
            }
            done += n;
            src += n;
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
