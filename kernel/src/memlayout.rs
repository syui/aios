// 物理/仮想アドレスの配置
//
// カーネルは上位アドレス KBASE + 1GiB + 0x80000 にリンクしてあり、どこに置かれても
// (QEMU virt は RAM が 0x4000_0000 から、ラズパイは 0 から) そこが RAM の先頭 + 0x80000 に
// なるよう、boot.rs が写像を作る:
//   仮想 KBASE + 1GiB + (PA - RAM の先頭) = 物理 PA
// RAM の先頭から 1 GiB が通常のメモリ、その前後の 1 GiB ずつがデバイス
// (ラズパイでは RAM の 1 GiB の中の 0x3f00_0000.. も周辺機器なので、そこはデバイス)。

pub const KBASE: usize = 0xffff_ff80_0000_0000;
const GIB: usize = 0x4000_0000;

pub const PGSIZE: usize = 4096;

unsafe extern "C" {
    /// boot.rs が書く: カーネルの置かれた場所 - 0x80000
    static boot_ram_base: u64;
}

/// RAM の先頭 (物理)
pub fn ram_base() -> usize {
    unsafe { core::ptr::read_volatile(&raw const boot_ram_base) as usize }
}

/// 物理アドレスを仮想アドレスに (RAM とその前後 1 GiB のデバイス)
pub fn p2v(pa: usize) -> usize {
    pa.wrapping_sub(ram_base()).wrapping_add(KBASE + GIB)
}

pub fn v2p(va: usize) -> usize {
    va.wrapping_sub(KBASE + GIB).wrapping_add(ram_base())
}

/// RAM の終わり。DTB の /memory から決める (なければ 512 MiB)
static mut PHYSTOP: usize = 0;

pub fn phystop() -> usize {
    match unsafe { PHYSTOP } {
        0 => ram_base() + 512 * 1024 * 1024,
        p => p,
    }
}

/// 使える RAM の大きさ
pub fn ram_size() -> usize {
    phystop() - ram_base()
}

pub fn set_ram(base: usize, size: usize) {
    if base != ram_base() || size < 64 * 1024 * 1024 {
        return;
    }
    // 通常のメモリとして写像してあるのは 1 GiB まで。ラズパイでは 0x3c00_0000 から上は
    // GPU のメモリと周辺機器
    let mut top = base + size.min(GIB);
    if base == 0 {
        top = top.min(0x3c00_0000);
    }
    unsafe { PHYSTOP = top };
}

/// [pa, pa + len) が通常のメモリとして写像してある範囲か
pub fn is_mapped_ram(pa: usize, len: usize) -> bool {
    let limit = if ram_base() == 0 { 0x3f00_0000 } else { ram_base() + GIB };
    pa >= ram_base() && pa.checked_add(len).is_some_and(|e| e <= limit)
}

pub const fn pg_round_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}
