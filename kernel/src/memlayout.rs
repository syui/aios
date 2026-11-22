// 物理/仮想アドレスの配置
//
// カーネルは上位アドレス KBASE + 1GiB + 0x80000 にリンクしてあり、どこに置かれても
// (QEMU virt は RAM が 0x4000_0000 から、ラズパイは 0 から) そこが RAM の先頭 + 0x80000 に
// なるよう、boot.rs が写像を作る:
//   仮想 KBASE + 1GiB + (PA - RAM の先頭) = 物理 PA
// RAM の先頭の前の 1 GiB はデバイス。RAM は DTB でわかった大きさだけ通常のメモリとして写す
// (MAX_RAM まで。起動のときは MAX_RAM を写しておき、set_ram が RAM に合わせてなおす)。
// ラズパイ (RAM が 0 から) は RAM の 1 GiB の中の 0x3f00_0000.. も周辺機器、その後ろはデバイス。

pub const KBASE: usize = 0xffff_ff80_0000_0000;
const GIB: usize = 0x4000_0000;

pub const PGSIZE: usize = 4096;
/// 使う RAM の上限 (kalloc のページごとの参照の数の表もこの大きさ)
pub const MAX_RAM: usize = 16 * GIB;

unsafe extern "C" {
    /// boot.rs の TTBR1 の L1 (1 GiB ずつ) と、そのすぐ後ろの KPTI のトランポリンの表 (tramp_l1、tramp_l2、tramp_l3)
    static mut boot_l1_hi: [[u64; 512]; 4];
}

/// boot.rs と同じ、通常のメモリの 1 GiB ブロックの属性
const ATTR_NORMAL: u64 = (1 << 54) | (1 << 10) | (3 << 8) | (1 << 2) | 1;
/// KPTI のとき、カーネルの写像に足す nG (ASID 0 の TLB にだけ入り、ユーザー (ASID 1) からは TLB でも見えない)
const PTE_NG: u64 = 1 << 11;
static NG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn ng() -> u64 {
    NG.load(core::sync::atomic::Ordering::Relaxed)
}

unsafe extern "C" {
    /// boot.rs が書く: カーネルの置かれた場所 - 0x80000
    static boot_ram_base: u64;
}

/// RAM の先頭 (物理)
// [lockless]
pub fn ram_base() -> usize {
    unsafe { core::ptr::read_volatile(&raw const boot_ram_base) as usize }
}

/// 物理アドレスを仮想アドレスに (RAM とその前後 1 GiB のデバイス)
// [lockless]
pub fn p2v(pa: usize) -> usize {
    pa.wrapping_sub(ram_base()).wrapping_add(KBASE + GIB)
}

// [lockless]
pub fn v2p(va: usize) -> usize {
    va.wrapping_sub(KBASE + GIB).wrapping_add(ram_base())
}

/// RAM の終わり。DTB の /memory から決める (なければ 512 MiB)
static PHYSTOP: crate::cell::Early<usize> = crate::cell::Early::new(0);

// [lockless]
pub fn phystop() -> usize {
    match PHYSTOP.val() {
        0 => ram_base() + 512 * 1024 * 1024,
        p => p,
    }
}

/// 使える RAM の大きさ
// [lockless]
pub fn ram_size() -> usize {
    phystop() - ram_base()
}

pub fn set_ram(base: usize, size: usize) {
    if base != ram_base() || size < 64 * 1024 * 1024 {
        return;
    }
    if base == 0 {
        // ラズパイ: 写像してあるのは 1 GiB まで。0x3c00_0000 から上は GPU のメモリと周辺機器
        PHYSTOP.set((base + size.min(GIB)).min(0x3c00_0000));
        return;
    }
    // RAM のあるところだけを通常のメモリに (1 GiB ずつ。端数のある最後のブロックも写す)
    let size = size.min(MAX_RAM);
    let blocks = size.div_ceil(GIB);
    unsafe {
        let l1 = &mut (*(&raw mut boot_l1_hi))[0];
        for i in 0..MAX_RAM / GIB {
            l1[1 + i] = if i < blocks { (base + i * GIB) as u64 | ATTR_NORMAL | ng() } else { 0 };
        }
        core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb");
    }
    PHYSTOP.set(base + size);
}

/// boot.rs と同じ、装置の 1 GiB ブロックの属性 (MAIR の 0 番: Device-nGnRnE、実行しない)
const ATTR_DEVICE: u64 = (1 << 54) | (1 << 53) | (1 << 10) | 1;
/// 装置のための L1 の置き場: RAM の後ろに 1 つあけて、そこから (Linux の ioremap の vmalloc の場所にあたる)
const DEV_SLOT: usize = 2 + MAX_RAM / GIB;
const DEV_SLOTS: usize = 64;
/// TCR の IPS (40 ビット) で写せる物理アドレスの上限
const PA_LIMIT: usize = 1 << 40;

/// 装置の [pa, pa + len) を写して、その仮想アドレスを返す (Linux の ioremap)。RAM の前の 1 GiB (起動のときに
/// 写した窓) ならそのまま、外 (QEMU virt の上のほうの PCIe の ECAM 0x40_1000_0000 など) は L1 のあきに 1 GiB ずつ。
/// 同じ 1 GiB はいちど写したものを使いまわす。起動のとき (ほかの CPU の前) に呼ぶ
pub fn map_device(pa: usize, len: usize) -> Option<usize> {
    let end = pa.checked_add(len)?;
    let base = ram_base();
    if base >= GIB && pa >= base - GIB && end <= base {
        return Some(p2v(pa));
    }
    if base == 0 || len == 0 || end > PA_LIMIT {
        return None;
    }
    let first = pa & !(GIB - 1);
    let n = (end - first).div_ceil(GIB);
    unsafe {
        let l1 = &mut (*(&raw mut boot_l1_hi))[0];
        let want = |k: usize| (first + k * GIB) as u64 | ATTR_DEVICE | ng();
        // 写してあるものか、n こ続いたあき
        let slot = (DEV_SLOT..DEV_SLOT + DEV_SLOTS - n + 1)
            .find(|&s| (0..n).all(|k| l1[s + k] == want(k)))
            .or_else(|| (DEV_SLOT..DEV_SLOT + DEV_SLOTS - n + 1).find(|&s| (0..n).all(|k| l1[s + k] == 0)))?;
        if l1[slot] == 0 {
            for k in 0..n {
                l1[slot + k] = want(k);
            }
            core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb");
        }
        Some(KBASE + slot * GIB + (pa - first))
    }
}

/// KPTI (Linux の UNMAP_KERNEL_AT_EL0 と kpti_install_ng_mappings): カーネルの写像 (TTBR1 の L1 のブロックと、その下の
/// L2 のブロック) をみな nG にし、[va, va + len) (トランポリン: 16 KiB にそろえた 16 KiB まで) だけを写す表を作る。
/// トランポリンは同じ仮想アドレスに写す (カーネルの表ではブロックの中、こちらは 4 KiB のページ。どちらも nG で
/// ASID がちがうので TLB でぶつからない)。ほかの CPU を起こす前に呼ぶ
pub fn kpti_tables(va: usize, len: usize) {
    /// 4 KiB のページ: 通常のメモリ、EL1 だけが読める (AP 10)、ユーザーは実行できない、nG
    const ATTR_TRAMP: u64 = (1 << 54) | PTE_NG | (1 << 10) | (3 << 8) | (2 << 6) | (1 << 2) | 3;
    assert!(va % 0x4000 == 0 && len <= 0x4000, "kpti: trampoline layout");
    NG.store(PTE_NG, core::sync::atomic::Ordering::Relaxed);
    unsafe {
        let l1 = &mut (*(&raw mut boot_l1_hi))[0];
        for e in l1.iter_mut() {
            match *e & 3 {
                1 => *e |= PTE_NG,
                // ラズパイの 2 MiB ずつの表
                3 => {
                    let l2 = &mut *(p2v((*e & 0x0000_ffff_ffff_f000) as usize) as *mut [u64; 512]);
                    for b in l2.iter_mut().filter(|b| **b & 3 == 1) {
                        *b |= PTE_NG;
                    }
                }
                _ => {}
            }
        }
        // KPTI のときのユーザーのあいだの TTBR1 の表 (L1、L2、L3)
        let [_, t1, t2, t3] = &mut *(&raw mut boot_l1_hi);
        t1[(va - KBASE) >> 30] = v2p(t2.as_ptr() as usize) as u64 | 3;
        t2[(va >> 21) & 511] = v2p(t3.as_ptr() as usize) as u64 | 3;
        for k in 0..len.div_ceil(PGSIZE) {
            t3[((va >> 12) & 511) + k] = (v2p(va) + k * PGSIZE) as u64 | ATTR_TRAMP;
        }
        // nG にする前の (global の) カーネルの TLB を消す
        core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb");
    }
}

/// [pa, pa + len) が通常のメモリとして写像してある範囲か
pub fn is_mapped_ram(pa: usize, len: usize) -> bool {
    // RAM の大きさがまだわからなければ、起動のときに写した分
    let limit = if ram_base() == 0 { 0x3f00_0000 } else if PHYSTOP.val() == 0 { ram_base() + MAX_RAM } else { phystop() };
    pa >= ram_base() && pa.checked_add(len).is_some_and(|e| e <= limit)
}

pub const fn pg_round_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}
