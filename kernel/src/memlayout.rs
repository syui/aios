// 物理/仮想アドレスの配置 (qemu virt)

pub const KBASE: usize = 0xffff_ff80_0000_0000;

pub const UART0: usize = KBASE + 0x0900_0000;
pub const RTC: usize = KBASE + 0x0901_0000;
pub const GICD: usize = KBASE + 0x0800_0000;
pub const GICC: usize = KBASE + 0x0801_0000;

pub const PHYSBASE: usize = 0x4000_0000;
pub const PHYSTOP: usize = PHYSBASE + 512 * 1024 * 1024;

pub const PGSIZE: usize = 4096;

pub const fn p2v(pa: usize) -> usize {
    pa + KBASE
}

pub const fn v2p(va: usize) -> usize {
    va - KBASE
}

pub const fn pg_round_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}
