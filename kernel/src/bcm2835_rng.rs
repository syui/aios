// ラズパイの乱数の装置 (DTB の brcm,bcm2835-rng。QEMU の raspi3b にもあり、ホストの乱数を返す)。
// 起こしたときに 32 バイトを rand の池に入れ、あとは rand が 60 秒ごとに取りにくる (set_source)。
// 割り込みは使わず、たまった数 (STATUS の上 8 ビット) を見て読む (Linux の bcm2835-rng と同じ手順)
use crate::mmio::{r32, w32};
use core::sync::atomic::{AtomicUsize, Ordering};

const CTRL: usize = 0x0;
const STATUS: usize = 0x4;
const DATA: usize = 0x8;
const INT_MASK: usize = 0x10;
/// はじめに作られるものは乱雑さが少ないので、これだけ捨てさせる (Linux と同じ数)
const WARMUP: u32 = 0x40000;

/// レジスタの場所 (0 なら装置がない)
static BASE: AtomicUsize = AtomicUsize::new(0);

/// 見つかったら、32 バイトを rand へ入れて true
pub fn init() -> bool {
    let Some((pa, _)) = crate::dtb::reg_of("brcm,bcm2835-rng", 0) else { return false };
    let base = crate::memlayout::p2v(pa as usize);
    w32(base + INT_MASK, r32(base + INT_MASK) | 1);
    w32(base + STATUS, WARMUP);
    w32(base + CTRL, 1);
    BASE.store(base, Ordering::Relaxed);
    let mut b = [0u8; 32];
    let n = read(&mut b);
    crate::rand::seed("bcm2835-rng", &b[..n]);
    crate::crypto::wipe(&mut b);
    crate::rand::set_source(read);
    true
}

/// out に乱数を (4 バイトずつ)。できるのを 0.5 秒まで待つ。入れたバイト数
fn read(out: &mut [u8]) -> usize {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        return 0;
    }
    let t0 = crate::timer::uptime_ns();
    let mut n = 0;
    while n + 4 <= out.len() {
        if r32(base + STATUS) >> 24 == 0 {
            if crate::timer::uptime_ns().wrapping_sub(t0) > 500_000_000 {
                break;
            }
            core::hint::spin_loop();
            continue;
        }
        out[n..n + 4].copy_from_slice(&r32(base + DATA).to_le_bytes());
        n += 4;
    }
    n
}
