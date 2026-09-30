// GICv2 (qemu virt: distributor PA 0x0800_0000, cpu interface PA 0x0801_0000)
use crate::memlayout::{GICC, GICD};
use core::ptr::{read_volatile, write_volatile};

const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;

const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

pub const SPURIOUS: u32 = 1023;

fn reg(base: usize, off: usize) -> *mut u32 {
    (base + off) as *mut u32
}

pub fn init() {
    unsafe {
        write_volatile(reg(GICD, GICD_CTLR), 1);
        write_volatile(reg(GICC, GICC_PMR), 0xff);
        write_volatile(reg(GICC, GICC_CTLR), 1);
    }
}

pub fn enable(id: u32) {
    let id = id as usize;
    unsafe {
        let pri = (GICD + GICD_IPRIORITYR + id) as *mut u8;
        write_volatile(pri, 0);
        if id >= 32 {
            // SPI は cpu0 に届ける
            write_volatile((GICD + GICD_ITARGETSR + id) as *mut u8, 1);
        }
        write_volatile(reg(GICD, GICD_ISENABLER + (id / 32) * 4), 1 << (id % 32));
    }
}

pub fn claim() -> u32 {
    unsafe { read_volatile(reg(GICC, GICC_IAR)) & 0x3ff }
}

pub fn complete(id: u32) {
    unsafe { write_volatile(reg(GICC, GICC_EOIR), id) }
}
