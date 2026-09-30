// 乱数もどき (xorshift, カウンタで種まき)。暗号用途には使えない
use core::sync::atomic::{AtomicU64, Ordering};

static STATE: AtomicU64 = AtomicU64::new(0);

pub fn next() -> u64 {
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        let c: u64;
        unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) c) };
        x = c | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    x
}

pub fn bytes16() -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&next().to_le_bytes());
    b[8..].copy_from_slice(&next().to_le_bytes());
    b
}
