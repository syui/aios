// 機器のレジスタ (MMIO) の読み書き
//
// いつも「アドレスのレジスタ 1 つだけ」の ldr / str 1 回にする (インラインアセンブリで)。
// read_volatile だと、コンパイラがアドレスを足しながら読む形 (ldr x0, [x1], #4) や
// ldp / stp を選ぶことがあり、Mac の Hypervisor.framework (HVF) や KVM はそういう命令の
// MMIO を真似できない (データアボートの ISV が 0 になり、QEMU が止まる)。
#![allow(dead_code)]
use core::arch::asm;

#[inline(always)]
pub fn r8(a: usize) -> u8 {
    let v: u32;
    unsafe { asm!("ldrb {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, preserves_flags)) };
    v as u8
}

#[inline(always)]
pub fn r16(a: usize) -> u16 {
    let v: u32;
    unsafe { asm!("ldrh {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, preserves_flags)) };
    v as u16
}

#[inline(always)]
pub fn r32(a: usize) -> u32 {
    let v: u32;
    unsafe { asm!("ldr {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, preserves_flags)) };
    v
}

#[inline(always)]
pub fn r64(a: usize) -> u64 {
    let v: u64;
    unsafe { asm!("ldr {v}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, preserves_flags)) };
    v
}

#[inline(always)]
pub fn w8(a: usize, v: u8) {
    unsafe { asm!("strb {v:w}, [{a}]", a = in(reg) a, v = in(reg) v as u32, options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn w32(a: usize, v: u32) {
    unsafe { asm!("str {v:w}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn w64(a: usize, v: u64) {
    unsafe { asm!("str {v}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack, preserves_flags)) };
}
